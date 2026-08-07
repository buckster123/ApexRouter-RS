# ApexRouter-RS — Architecture

> **Status: authoritative.** This document supersedes `docs/port/design-{router,lib,daemon}-first.md`,
> which are retained only as design history. Where this disagrees with them, this wins.
> Where this disagrees with `docs/port/00-machine-ground-truth.md`, `00b-brief-amendment-scale.md`,
> `00c-vast-api-verified.md` or `08-house-conventions.md`, **those** win and this is a bug.
>
> Companion: `docs/BUILD-PLAN.md` (the parallel-agent implementation plan). Between them these two
> files are the sole input for implementation agents.
>
> **Last cross-checked against the code on 2026-08-07** (full-repo audit packages a–c: proxy
> mutation gate mounted, money-path composition fixes, jobs/`register_started`/`retry_bucket`
> wiring, D10 boot-watchdog carve-out). Earlier **As built:** notes from 2026-07-31 (D-04 /
> MK1-CORE) remain. Where this disagrees with the code, **one of them is a bug** — say which.
> Operational companions: `docs/SLINT.md`, `docs/AGENTS.md`, `skills/apexrouter/SKILL.md`,
> `docs/audits/2026-08-07-full-repo-audit.md`.

---

## 0. What ApexRouter-RS is

ApexRouter-RS is a standalone Rust inference manager and local OpenAI-compatible endpoint proxy. It
holds a **routing table** — named aliases pointing at ordered chains of live OpenAI-compatible
backends — and serves that table on `http://127.0.0.1:8888/v1`, so every agent, SDK and script on
the machine has exactly one base URL and one model string that never change. Everything else it does
— discovering llama.cpp builds, GGUF weights and GPUs on the rig; solving what fits in VRAM;
spawning and supervising `llama-server`; renting a GPU on vast.ai and tunnelling it home; talking to
managed providers like together.ai; registering a LAN box — exists to **put rows in that table and
keep them honest**. It exposes that one control plane through five surfaces that share one protocol
crate: a REST + WebSocket API, a no-dependency embedded web UI, a native Slint app, a `clap` CLI,
and an MCP stdio server for local agents (ApexOS, Claude Code).

The one-line README promise: **point every agent at `http://127.0.0.1:8888/v1` and never change it
again.**

### 0.1 The five invariants

Break any of these and the product is broken. They are repeated in `CLAUDE.md`.

1. **One resolver.** There is exactly one `resolve()` and every surface calls it. LocalRouter had
   four implementations of "what is active" that disagreed (`07` D4); ApexRouter has one, and the
   answer is observable on every response as `X-ApexRouter-Route: <alias>|<reason>`.
2. **The request path never touches the filesystem.** It loads an `ArcSwap<RoutingTable>` and a
   per-backend `Arc<LiveBackend>`. No `stat()`, no TOML parse, no lock beyond one `Semaphore`.
3. **Persisted records hold facts, never status.** `pid`, `start_time_ticks`, `boot_id`, `port`,
   `argv`, `desired_state`. Liveness and health are *computed* on read. No `status: "running"`
   string ever goes to disk.
4. **Nothing is auto-destroyed that costs money, and nothing that costs money happens without a
   `SpendApproval`.** The ledger row is written before the billing call. A crash must never delete a
   paid box; a leak must be visible.
5. **State lives in one XDG state dir. Nothing is ever written into a repo directory.**

---

## 1. Process and deployment model

### 1.1 One process, two listeners

`apexrouter serve` is the daemon. It owns the routing table, the backend registry, every child
process (`llama-server`, `ssh`), the health prober, the file watcher, the ledger, the usage writer,
and both HTTP listeners. The CLI, the MCP server, the web UI and the Slint app are **clients**.

Two listeners in one process, because a single listener cannot satisfy both contracts:

| Listener | Default bind | Contents | Auth posture |
|---|---|---|---|
| **Proxy / data plane** | `127.0.0.1:8888` | `/v1/*`, the catch-all fallback, and the three byte-compatible legacy routes `/health`, `/providers`, `POST /switch` | open on loopback (`OPENAI_API_KEY=not-needed` keeps working); optional bearer; `POST /switch` is treated as a mutation and gets the mutation gate (§9.3) |
| **Control plane** | `127.0.0.1:2739` (`APEX` on a keypad) | `/v1/*` control REST, `/ws`, the embedded web UI, and `/metrics` (§4.5 — **Prometheus text written, not yet mounted**; control answers 404) | one configured bearer, loopback bypass keyed on `ConnectInfo` peer IP, mandatory `Origin`/`Sec-Fetch-Site` + `Host` validation on every mutation (§9.4); proxy `POST /switch` uses the same mutation gate (§9.3) |

Why not one socket: the proxy is a catch-all by contract (`05` §2 — everything that is not one of
five (path, method) pairs is proxied, including `POST /health`). A catch-all `any()` route and the
static-asset `get("/{*path}")` route **panic on `Router::merge` in axum 0.8** ("Overlapping method
route"), and a shared listener also permanently shadows llama.cpp's own `/health` for control
clients. Two listeners resolve both, let the two `/health` endpoints keep two different shapes
(legacy on the proxy, house shape on control), and make it possible to expose the proxy to the LAN
without exposing the control plane. The user only ever types `8888`; every client discovers the
control URL from the lock file's owner record or `$APEXROUTER_URL`.

```
                      ┌───────────────────────────────────────────────────────────────┐
  agents / SDKs       │                    apexrouter serve                            │
  curl / smoke.sh ───►│  :8888  proxy listener                                         │
  OPENAI_BASE_URL     │    ├─ path normalise (collapse duplicate /v1)                  │
                      │    ├─ RequestPeek {model, stream}                              │
                      │    ├─ resolve(model) ──► ArcSwap<RoutingTable>                 │
                      │    ├─ InFlightGuard (permit + gauge + RequestRecord, RAII)      │
                      │    ├─ PreFlight ──attempt──► Committed  (no retry past byte 1) │
                      │    └─ relay (bytes verbatim; SSE never re-framed)              │
                      │                    │                                            │
  web UI ────────────►│  :2739  control listener                                       │
  Slint  ────────────►│    ├─ /v1/... REST   ├─ /ws (snapshot-on-connect)              │
  CLI    ────────────►│    ├─ /metrics       └─ embedded ui-web (rust-embed)           │
  MCP    ────────────►│                                                                 │
                      │  ── shared, in-process ────────────────────────────────────────│
                      │  BackendRegistry (Arc<LiveBackend>: semaphore, breaker, health) │
                      │  Supervisor  ── setsid ──►  llama-server ×N   (survive restart) │
                      │  TunnelSupervisor ── ssh -L ──► vast.ai instance ×N             │
                      │  Ledger (instances.jsonl)  UsageWriter (usage.jsonl)            │
                      │  HealthProber  ConfigWatcher(notify+poll)  Broadcast<Event>     │
                      │  flock $STATE/apexrouterd.lock  (O_CLOEXEC, owner record)       │
                      └───────────────────────────────────────────────────────────────┘
                                │                              │
                    ~/.local/state/apexrouter/         https://console.vast.ai
                    (facts, ledger, usage, logs)       https://api.together.ai
                                                       https://huggingface.co
```

### 1.2 Ownership: `flock` with an owner record

`$STATE/apexrouterd.lock` is opened with `O_CLOEXEC` (Rust's `File` does this; there is an explicit
test asserting a spawned child does not inherit it) and held `LOCK_EX` for the daemon's lifetime.
The file contains the owner record:

```jsonc
{ "pid": 41233, "start_time_ticks": 8123441, "boot_id": "…", "version": "0.1.0",
  "proxy_url": "http://127.0.0.1:8888", "control_url": "http://127.0.0.1:2739",
  "started_at_unix": 1785412331 }
```

Properties, and the fixes that make them true:

- **"Is there an owner?" is one syscall** — `flock(LOCK_EX|LOCK_NB)` fails ⇒ an owner exists. The
  kernel drops it on process death, so there is no stale-owner class.
- **Only the daemon ever touches `apexrouterd.lock`.** Offline CLI readers do *not* take it; a
  concurrent `apexrouter status --watch` can never make `apexrouter serve` believe a daemon is
  running. Offline *readers* coordinate on `$STATE/state.lock` (`LOCK_SH`); offline *writers*
  (`migrate --apply`, `config init`) take `apexrouterd.lock` `LOCK_EX`, which proves no daemon is
  running.
- **Lock, then bind.** If `bind()` returns `EADDRINUSE` the daemon probes the port's `/health` and
  reports the foreign holder by name ("port 8888 is held by something answering
  `product=localrouter` — stop `endpoint_proxy.py` first"), then exits 1.

### 1.3 Startup order

1. Resolve `Paths`. Never `exit()` from a library.
2. `flock(apexrouterd.lock, LOCK_EX|LOCK_NB)`; on failure print the owner record and exit 1.
3. Load `Config` (a missing file is a working zero-config install). Write the owner record.
4. If `$STATE` is empty and legacy state exists → run `migrate` in `--auto` mode (import-only, never
   destructive) and emit a `Notice`.
5. Load `$STATE` facts: endpoints, routes, tunnels, catalog, ledger index.
6. **Reconcile before binding.** For each endpoint record: compute `Adoption` (§4.4). For each
   tunnel record: probe and re-adopt or mark down. Vast reconciliation is **not** on this path — it
   is a network call and the laptop is often offline; it runs in a background task and raises an
   `Alert` (§4.7).
7. Build the `BackendRegistry`, compile the `RoutingTable`, `ArcSwap::store`.
8. Bind both listeners. **The proxy answers `503 {"error":{"type":"starting"}}` from step 3 onward
   if bound early is ever needed; in mk1 it binds here and the pre-bind window is bounded by local
   `/proc` reads only.**
9. Start the health prober, the config watcher, the model/rig scanner, the Vast reconciler, the boot
   watchdogs, the tunnel supervisors.

### 1.4 Children outlive the manager

This is the single most important lifecycle decision, and it is grafted wholesale from the
library-first proposal.

`llama-server` is spawned with `setsid()` (its own session and process group), stdio consumed by an
owned `File` via `Stdio::from(file)` (so the parent's fd leak is not expressible), and its
`ProcIdentity` is written to `$STATE/endpoints/<id>.json` **before** the spawn function returns.
`ssh -N -L` tunnels are the same. Consequences:

- `systemctl --user restart apexrouterd` — or a crash, or `cargo install` — does not evict a model
  that took 90 seconds and 6 GB to load.
- The daemon `wait()`s on children it spawned in a background reaper task, so no zombies. Children
  it *adopted* were re-parented to init and are reaped there.
- `[supervisor] kill_children_on_exit = false` is the default and is honest about what it means.
- **Vast instances are never auto-destroyed on shutdown, ever, at any setting.** A crash must not
  delete a paid box.

### 1.5 Blocking work

Process spawn/wait, `/proc` reads, filesystem walks, GGUF header reads, `--help` probes, `toml_edit`
round-trips and log tailing all go through `tokio::task::spawn_blocking`. Nothing in a render path
or a route-resolution path does I/O.

### 1.6 Shutdown

`tokio::signal` sets a flag. Both listeners stop accepting. In-flight requests drain to
`drain_timeout_secs` (default 30). Ledger and usage appends complete inside awaited tasks. Tunnels
are left up or torn down per `[vast] tunnels_on_shutdown` (default `adopt`). `SIGHUP` reloads
config instead of exiting. The lock is released by process exit.

---

## 2. Crate graph

Acyclic, resolver 2, edition 2021, MSRV `1.75`, version `0.1.0`.

```
                    apexrouter-protocol        (serde only; no I/O, no tokio, no reqwest)
                       ▲   ▲   ▲   ▲
        ┌──────────────┘   │   │   └──────────────┐
        │                  │   │                  │
  apexrouter-core    apexrouter-client            │      (client: reqwest + tungstenite only)
        ▲   ▲                ▲                    │
        │   │                │                    │
        │   └── apexrouter-router                 │
        │              ▲                          │
        └── apexrouter-providers                  │
                       ▲                          │
                apexrouter-server                 │
                       ▲                          │
                apexrouter-cli ───────────────────┘
                                              apexrouter-slint ──► protocol, client
```

| Crate | Purpose | Depends on (internal) | Key third-party |
|---|---|---|---|
| `apexrouter-protocol` | Every wire and domain type. The single vocabulary every surface deserializes. `#[serde(rename_all="snake_case")]`, `#[serde(tag="type")]` on `Event`, `PartialEq` everywhere so the daemon suppresses no-op broadcasts, `#[serde(default)]` on additive `Vec` fields. | — | serde, serde_json, ulid |
| `apexrouter-core` | Paths, config, secrets, atomic store + locks, process identity, argv builder, discovery (builds/devices/models/GGUF), the fit solver, upstream probing, pricing, usage, ledger, catalog, migration, the `Check` registry. No axum, no provisioning. | protocol | tokio, reqwest, toml, toml_edit, dirs, notify, chrono, thiserror, anyhow, tracing, sysinfo, glob, sha2 |
| `apexrouter-router` | **The request path.** Routing table, `resolve()`, relay, SSE, retry/failover, breaker, limits, aggregated `/v1/models`, telemetry, legacy compat handlers, and the one Anthropic ingress translator (`anthropic/`). Knows nothing about Vast, HF or process spawning. | protocol, core | axum, tower, reqwest, arc-swap, bytes, futures-util |
| `apexrouter-providers` | How backends come to exist. Local `llama-server` supervisor, vast.ai REST + boot + tunnel, together.ai, HuggingFace, plain OpenAI-compatible node. Provider-specific `Check`s. | protocol, core | reqwest, async-trait, serde_json, tokio |
| `apexrouter-client` | `NodeClient` — the thin HTTP+WS client every non-server surface uses. ~300 lines. No business logic. | protocol | reqwest, tokio-tungstenite, futures-util |
| `apexrouter-server` | The axum application: proxy listener, control listener, `/ws`, auth, embedded assets, job runner. `pub use api_router()` so ApexOS-RS can mount the control plane. | protocol, core, router, providers | axum `["ws"]`, tower-http, rust-embed |
| `apexrouter-cli` | `[[bin]] apexrouter` (`default-run`). Every verb, plus `serve` (runs the server in-process) and `mcp` (the stdio JSON-RPC server, in `src/mcp/`). | protocol, core, router, providers, server, client | clap, tracing-subscriber, anyhow |
| `apexrouter-slint` | `[[bin]] apexrouter-ui`. GPL-3.0-only, `publish = false`, **out of `default-members` and out of the CI `-p` list** — CI never builds it, so its invariants are hand-checked (`docs/SLINT.md` §8.1). An edge client of the same HTTP API; no second business-logic path. | protocol, client | slint, slint-build, tokio, anyhow, serde_json, futures-util |

**Two binaries only.** `apexrouter` (headless, does everything including `serve` and `mcp`) and
`apexrouter-ui` (Slint). `~/Projects/.mcp.json` registers `target/release/apexrouter` with
`args: ["mcp"]`, so there is no third fat link on the release critical path.

**Cycle avoidance, stated explicitly because two of the three source proposals got this wrong:**
`core` must not depend on `providers`. Therefore (a) HTTP probing of an OpenAI-compatible upstream
(`/health`, `/props`, `/slots`, `/v1/models`, `/metrics`, `timings`) lives in
`core::upstream::probe`, not in a provider; (b) `core::pricing` is a *data* table fed by providers at
runtime, it never calls one; (c) `core::checks` defines `trait Check` and the concurrent runner,
while checks needing Vast/Together/SSH live in `providers::checks` and are registered by
`apexrouter-server` at startup; (d) `Secret`/`CredentialRef` live in `core`, and `protocol` never
carries key material (it carries `CredentialSource`, a description).

### 2.1 Dependency pins (house-consistent; every one already in `~/.cargo/registry`)

axum `0.8` `["ws"]` · tower `0.5` · tower-http `0.7` `["trace","fs"]` · **reqwest `0.12`**
`default-features = false` + `["json","rustls-tls","stream"]` · clap `4` `["derive","env"]` ·
clap_complete `4` · tokio `1` (explicit feature list, never `full`) · tracing `0.1` +
tracing-subscriber `0.3` `["env-filter"]` · serde `1` `["derive"]` · serde_json `1` · toml `0.8` ·
**toml_edit `0.22`** · dirs `6` · rust-embed `8` · notify `6` · **arc-swap `1`** · thiserror `2` ·
anyhow `1` · async-trait `0.1` · futures-util `0.3` · bytes `1` · chrono `0.4` `["serde"]` · ulid `1`
`["serde"]` · sysinfo `0.32` · glob `0.3` · sha2 `0.10` · rustix `0.38` `["fs","process"]` (flock,
setsid) · slint `1` + slint-build `1` · tokio-tungstenite `0.24` · tempfile `3` (dev) ·
wiremock `0.6` (dev).

Deliberate choices, each with a reason:

- **`arc-swap` is the one crate new to the garden.** The routing table is read on every proxied
  request and written rarely. `ArcSwap<RoutingTable>` makes that read a pointer load. It is used in
  exactly one place.
- **reqwest stays at 0.12**, not 0.13. `09` documents the trap (hf-hub 1.0 needs 0.13,
  reqwest-eventsource 0.6 needs 0.12). We use neither: HF is six hand-rolled calls, and SSE is
  relayed as bytes rather than parsed. 0.12 matches Prefrontal-RS and Imaginarium-RS.
- **No OpenAI SDK crate.** Typing the request schema would silently drop `top_k`,
  `repetition_penalty`, `timings_per_token` and llama.cpp reasoning fields. We relay bytes.
- **No `gguf` crate, no `russh`, no `mime_guess`, no colour crate, no `rusqlite`.** Everything a
  human might `cat` or a script might `tail` stays a file.
- `[profile.dev] debug = "line-tables-only"` — the dev box is at 92% disk.

---

## 3. Core domain types

These are the signatures implementers code against. They live in `apexrouter-protocol` unless marked
otherwise. `BUILD-PLAN.md` §3 quotes the complete file set verbatim; this section explains them.

### 3.1 Identifiers and honesty types

```rust
// ids.rs — validated newtypes; an empty or malformed id is not constructible.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BackendId(String);      // slug: ^[a-z0-9][a-z0-9._-]{0,63}$
pub struct Alias(String);          // the string a client puts in "model": same charset + '/' banned
pub struct BuildId(String);        // "build-vulkan"
pub struct RecipeId(String);
pub struct ProfileId(String);
pub struct InstanceId(u64);        // Vast contract id (the `new_contract` field)
pub struct RequestId(Ulid);
pub struct JobId(Ulid);
impl BackendId { pub fn parse(s: &str) -> Result<Self, IdError>; pub fn as_str(&self) -> &str; }

// money.rs — a guess must never render as a fact.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Money(pub i64);                        // integer micro-USD. No float dust.
impl Money { pub fn from_usd(f: f64) -> Self; pub fn as_usd(self) -> f64; }

#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CostEstimate {
    Metered     { usd: Money, source: PriceSource },
    Approximate { usd: Money, source: PriceSource, assumption: String },
    Unknown,
}
#[serde(rename_all = "snake_case")]
pub enum PriceSource { ProviderApi, VastOffer, ConfigTable, RecipeField, Derived }

#[serde(tag = "kind", rename_all = "snake_case", content = "n")]
pub enum TokenCount { Reported(u32), Estimated(u32) }
```

### 3.2 Rig — plural, per `00b`

Nothing in the data model is singular. `Gpu` is a list, `LlamaBuild` is a list, `DeviceSel` is a
list, `VramBudget` is a per-device vector.

```rust
pub struct RigSnapshot {
    pub gpus: Vec<Gpu>,
    pub builds: Vec<LlamaBuild>,
    pub ram_total_mb: u64, pub ram_free_mb: u64,
    pub swap_total_mb: u64, pub swap_used_mb: u64,
    pub cpu_threads: u32,
    pub scanned_at_unix: i64,
}

/// ONE ENUMERATION, NOT ONE PIECE OF SILICON. The same card is a different `Gpu` in every
/// backend that can reach it. Never add two `Gpu`s' VRAM together without checking
/// `physical_key` first — see §3.2.1.
pub struct Gpu {
    pub device: String,              // "Vulkan0", "CUDA1" — the exact -dev token
    pub index: u32,
    pub name: String,                // "AMD Radeon 840M Graphics (RADV KRACKAN1)"
    pub backend: Backend,
    pub vram_total_mb: u64,
    pub vram_free_mb: u64,           // as llama.cpp --list-devices reports it
    pub pci_bus_id: Option<String>,  // "0000:04:00.0" when sysfs could be aligned; physical id
    pub driver: Option<String>,
    pub is_software: bool,           // llvmpipe -> true; excluded from default selection
    pub seen_by_builds: Vec<BuildId>,
    pub held_by: Vec<BackendId>,     // endpoints currently using this device
    pub reserved_mb: u64,            // sum of fit estimates of endpoints in `held_by`
}
impl Gpu {
    /// The ONLY sanctioned "used" figure: `None` when free > total (GTT accounting), never
    /// an underflowed u64.
    pub fn vram_used_mb(&self) -> Option<u64>;
    pub fn reports_gtt_overcommit(&self) -> bool;
    pub fn physical_key(&self, ordinal: usize) -> String;   // "pci:…" or "name:…#n"
}

/// One piece of silicon, with every backend enumeration that reaches it. DERIVED, never
/// stored — `gpus` stays the raw per-backend truth, because that is what `-dev` takes.
/// VRAM is deliberately absent: the backends disagree about it on purpose, so read it
/// per backend from `views`.
pub struct PhysicalDevice { pub key: String, pub pci_bus_id: Option<String>, pub name: String,
                            pub is_software: bool, pub views: Vec<Gpu> }
impl PhysicalDevice {
    pub fn backends(&self) -> Vec<Backend>;
    pub fn device_tokens(&self) -> Vec<String>;
    pub fn view_for(&self, backend: &Backend) -> Option<&Gpu>;
    pub fn held_by(&self) -> Vec<BackendId>;
    pub fn seen_by_builds(&self) -> Vec<BuildId>;
}
impl RigSnapshot { pub fn physical_devices(&self) -> Vec<PhysicalDevice>; }
pub fn physical_devices(gpus: &[Gpu]) -> Vec<PhysicalDevice>;
pub fn normalise_device_name(name: &str) -> String;

#[serde(rename_all = "snake_case")]
pub enum Backend { Vulkan, Cuda, Rocm, Hip, Metal, Sycl, Cpu, Other(String) }

pub struct LlamaBuild {
    pub id: BuildId,                 // the build-dir name: "build-vulkan", "build-mtp"
    pub server_path: String,         // absolute path to llama-server
    pub label: String,
    pub build_info: Option<String>,  // "b9199 (39cf5d619)"
    pub backends: Vec<Backend>,      // from --list-devices, NEVER from grepping --help
    pub devices: Vec<String>,        // ["Vulkan0", "Vulkan1"]
    pub flags: FlagSupport,
    pub probed_at_unix: i64,
}

/// Feature detection, cached in $CACHE keyed by (path, mtime, size). Never a hardcoded whitelist:
/// b9199 already moved -fa to on|off|auto, made --jinja default-on and deprecated --webui.
pub struct FlagSupport { pub flags: BTreeSet<String>, pub jinja_default_on: bool,
                         pub fa_tristate: bool, pub has_fit: bool, pub has_router_mode: bool }
impl FlagSupport { pub fn has(&self, flag: &str) -> bool; }

pub struct LocalModel {
    pub id: String,                  // stable slug derived from the dir + base filename
    pub name: String,
    pub dir: String,
    pub shards: Vec<ModelShard>,     // -00001-of-000NN grouped into ONE logical model
    pub total_bytes: u64,
    pub mmproj: Vec<ModelShard>,     // vision projectors found alongside; empty = text-only
    pub quant: Option<String>,       // regex (UD-Q\d+[^.\s_-]*|Q\d+_K_[A-Z]+|Q\d+_\d+)
    pub gguf: Option<GgufMeta>,      // header read; None if unreadable
    pub discovered_at_unix: i64,
}
pub struct ModelShard { pub path: String, pub bytes: u64 }
pub struct GgufMeta { pub arch: String, pub n_layer: u32, pub n_head_kv: u32,
                      pub n_embd_head_k: u32, pub n_embd_head_v: u32, pub n_ctx_train: u32,
                      pub full_attn_layers: Option<u32>, pub n_expert: Option<u32> }
```

#### 3.2.1 Physical device identity — one card is one card

`gpus` is a list of **enumerations**. Two llama.cpp builds with different backends enumerate the
same silicon under different `-dev` tokens, with different names and different VRAM readings. On
the machine in `00-machine-ground-truth.md` the single Radeon 840M is `ROCm0` (11397 MiB, from
`~/llama.cpp/build`) and `Vulkan0` (20992 MiB, from `build-vulkan`). Both readings are true.
Neither may be added to the other, and `rig` must not present them as two GPUs.

Identity is established in two steps, strongest first:

1. **PCI bus id.** `core::discover::physical::scan_pci_gpus` reads `/sys/bus/pci/devices/*/class`
   for display controllers and `attach_pci_ids` aligns each backend's non-software enumeration
   with them, bucketed by the vendor inferred from the device name and matched **only when the
   counts agree exactly**. An ambiguous rig gets no bus ids rather than a guess.
2. **Documented name heuristic.** `name:<normalised name>#<ordinal within backend>`, where
   normalisation lowercases and drops parenthesised driver suffixes so
   `AMD Radeon 840M Graphics (RADV KRACKAN1)` and `AMD Radeon 840M Graphics` agree. The ordinal
   keeps four identical cards apart inside one backend while pairing card *n* of one backend with
   card *n* of another. It assumes both backends enumerate identical cards in the same order,
   which is exactly why rule 1 exists.

`RigSnapshot::physical_devices()` folds enumerations onto silicon with those rules. Consequences
that are load-bearing elsewhere: VRAM is **never** summed across backends (§3.3), a reservation is
attributed to the *card* rather than to the token (so an endpoint on `Vulkan0` also weighs on
`ROCm0`'s budget), and `apexrouter rig` prints one row per card with the set of backends that
reach it plus a second table of the per-backend readings.

**GTT accounting.** ROCm reports `free` (12821 MiB) greater than `total` (11397 MiB) on this box,
because a GTT-backed device allocates past its carve-out. `total - free` is therefore not a small
number, it is an underflowed `u64`. `Gpu::vram_used_mb() -> Option<u64>` is the only sanctioned way
to ask, and it returns `None` rather than a lie: the type cannot express the underflow.

### 3.3 Fit — one pure function replacing 54 hand-solved recipe strings

```rust
pub struct DeviceBudget { pub device: String, pub free_mb: u64, pub reserved_mb: u64 }
/// Budget is a VECTOR, never a scalar (00b consequence 1). `usable_mb` = free - reserved - margin.
/// It is also PER BACKEND: one llama-server process uses one backend, so `devices` are all on
/// `backend` and a budget is never a sum across backends.
pub struct VramBudget { pub devices: Vec<DeviceBudget>, pub margin_mb: u64,
                        pub host_ram_free_mb: u64,
                        pub backend: Option<Backend>,   // None = no device; never "all of them"
                        pub notes: Vec<String> }        // what was dropped, and why
impl VramBudget { pub fn total_usable_mb(&self) -> u64; pub fn largest_usable_mb(&self) -> u64; }

pub struct FitInput {
    pub weights_bytes: u64,          // local file bytes (shards summed) or HF paths-info
    pub gguf: GgufMeta,
    pub budget: VramBudget,          // LIVE, computed at plan time; never a 300 s cache
    pub want_ctx: Option<u32>,
    pub want_parallel: Option<u32>,
    pub want_kv: Option<KvType>,
    pub split: SplitPlan,
}
#[serde(rename_all = "snake_case")]
pub enum KvType { F32, F16, Bf16, Q8_0, Q4_0, Q4_1, Iq4Nl, Q5_0, Q5_1 }

pub struct FitPlan {
    pub ctx: u32,                    // TOTAL pool, shared across `parallel` slots (06: not per-slot)
    pub parallel: u32,
    pub kv_type: KvType,
    pub ngl: NglPlan,
    pub split: SplitPlan,
    pub weights_mb: u64, pub kv_mb: u64, pub compute_mb: u64, pub headroom_mb: i64,
    pub per_device_mb: Vec<(String, u64)>,
    pub verdict: FitVerdict,
    pub why: Vec<String>,            // rendered as tooltips next to every derived field
}
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum FitVerdict { Fits { headroom_mb: u64 }, Tight { headroom_mb: u64 },
                      NeedsOffload { layers_on_gpu: u32 }, WontFit { short_by_mb: u64 } }
#[serde(tag = "ngl", rename_all = "snake_case")]
pub enum NglPlan { Auto, All, Layers(u32) }   // Auto = emit nothing, let llama.cpp --fit decide
pub struct SplitPlan { pub devices: Vec<String>, pub mode: SplitMode,
                       pub main_gpu: Option<u32>, pub tensor_split: Option<Vec<f32>> }
#[serde(rename_all = "snake_case")]
pub enum SplitMode { None, Layer, Row, Tensor }

// core::fit
pub fn fit(input: &FitInput) -> FitPlan;   // pure, unit-tested, no I/O, no allocation surprises

/// Which backend's devices a budget may be spent on. One process, one backend.
pub enum BackendScope<'a> {
    Build(&'a BuildId),        // the build about to be exec'd — cannot disagree with the spawn
    Backend(&'a Backend),      // an already-resolved backend
    Auto,                      // first named device's backend, else choose_build's, else the
                               // first non-software GPU. NEVER a sum over all of them.
}
pub fn budget_from_rig(rig: &RigSnapshot, scope: BackendScope<'_>, devices: &[String],
                       margin_mb: u64, running: &[EndpointRecord]) -> VramBudget;
```

`kv_bytes = kv_layers × ctx × n_head_kv × (n_embd_head_k + n_embd_head_v) × bytes_per_elem(kv_type)`
where `kv_layers = full_attn_layers.unwrap_or(n_layer)` (hybrid-linear models like Qwen3.6 MoE carry
KV on only 10 of 41 layers). Compute buffer is estimated from batch size and **calibrated against
the archived run log** in `03`: Qwen3.5-9B Q4_K_M, ctx 32768, kv q8_0, Vulkan →
5956 MiB = 4861 model + 594 context + 501 compute. That fixture is a unit test.

`fit()` is exposed identically from `apexrouter fit`, `GET /v1/fit`, `apexrouter_fit` (MCP), the
Launch drawer's live headroom bar in both GUIs, and the Vast rent panel ("what fits on 2× 3090?").

**The budget is per backend.** `budget_from_rig` resolves `scope` to exactly one backend and
selects only that backend's devices; `devices` narrows within it and can never widen across it.
Anything dropped — a device of another backend, a name the rig does not have — lands in
`VramBudget::notes` and is folded into `FitPlan::why`, because a fallback is a visible value.
Reservations are attributed by `Gpu::physical_key`, so an endpoint holding a card through one
backend is subtracted from the same card's budget under another. Four cards on one backend still
sum: a genuine 4× H100 box budgets 4 × 81559 MiB, and the Vulkan enumerations of those same four
cards are excluded rather than added. `fit()` defends the pure `POST /v1/fit` path too: a
caller-supplied budget mixing device tokens of two backends is **not** added up — the largest
single-backend group wins and a `WARNING` line says so. Over-optimism is the direction that OOMs a
spawn, so an ambiguous budget resolves downwards.

### 3.4 Endpoint, Backend, Provider

An **Endpoint** is a thing ApexRouter knows how to start and stop. A **Backend** is a live upstream
in the routing table — OpenAI-compatible unless it declares otherwise. Every endpoint produces a
backend; not every backend has an endpoint (a LAN node or a managed provider has no lifecycle).

```rust
/// The wire dialect a listener accepts or an upstream speaks.
#[derive(Copy, Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol { OpenAi, Anthropic }
impl Default for Protocol { fn default() -> Self { Protocol::OpenAi } }

#[serde(rename_all = "snake_case")]
pub enum BackendKind { LocalLlama, LocalVllm, VastLlama, VastVllm, Managed, Node }

pub struct Backend {
    pub id: BackendId,
    pub kind: BackendKind,
    pub protocol: Protocol,          // #[serde(default)] — OpenAi unless the record says otherwise
    pub label: String,
    pub base_url: String,            // ALWAYS stored WITHOUT a trailing /v1  (§6.1)
    pub credential: CredentialSource,// a DESCRIPTION, never key material
    pub tags: Vec<String>,           // "local","tools","vision","cheap","gpu:vulkan","rented"
    pub models: Vec<UpstreamModel>,
    pub limits: BackendLimits,
    pub price: Option<PriceModel>,
    pub health: Health,
    pub provenance: Provenance,
    pub endpoint: Option<EndpointRef>,   // Some(..) when ApexRouter can start/stop it
    pub enabled: bool,
    pub devices: Vec<String>,        // as built: the `-dev` tokens this backend holds, so the
                                     // rig strip can say who owns a card without a join
    pub last_error: Option<String>,  // as built: the last probe/relay failure, rendered on the card
}
pub struct UpstreamModel { pub id: String, pub ctx: Option<u32>, pub vision: bool,
                           pub tools: bool }   // as built: tool-calling, from the probe
pub struct BackendLimits { pub max_concurrent: u32, pub queue_depth: u32,
                           pub ctx: Option<u32>, pub slots_total: Option<u32> }
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PriceModel { PerToken { input: Money, output: Money },  // per 1M tokens
                      PerHour { dph: Money }, Free }
impl PriceModel {
    /// Normalising PerHour into per-Mtok needs a throughput assumption. Return the assumption
    /// with the number so the UI can label it; NEVER bury a 100 tok/s constant like cost.py did.
    pub fn per_mtok(&self, tps_hint: Option<f32>) -> CostEstimate;
}
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Health {
    Unknown,
    Starting { phase: BootPhase, since_unix: i64, detail: Option<String> },
    Ready    { since_unix: i64, slots_busy: u32, slots_total: u32, tps_p50: Option<f32> },
    Degraded { reason: String, consecutive_failures: u32 },
    Down     { reason: String, retry_at_unix: i64 },
    Draining { in_flight: u32 },
}
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum BootPhase {
    Reserved, Provisioning, Pulling, Compiling,
    Downloading { pct: Option<f32>, mbps: Option<f32> },
    Loading { pct: Option<f32> }, Healthy, Failed { reason: String }, Destroyed,
}
#[serde(rename_all = "snake_case")]
pub enum Provenance { Discovered, Spawned, Rented, Manual, Adopted, Imported }
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CredentialSource { None, Env { var: String }, File { path: String },
                            Managed { store: String }, Instance }
```

**Protocol is a matrix cell, not a branch bolted onto a handler.** `Protocol` appears in exactly two
places on the request path: the ingress records which dialect the *client* spoke (§4.3), and the
resolved `Backend` declares which dialect the *upstream* speaks. What happens to the body is then
selected by the pair `(ingress, upstream)`. mk1 implements three of the four cells:

| ingress → upstream | mk1 behaviour |
|---|---|
| `OpenAi` → `OpenAi` | relay, byte-for-byte. Today's path, unchanged |
| `Anthropic` → `Anthropic` | **passthrough relay** — bytes verbatim, only the credential is swapped. No translation code runs |
| `Anthropic` → `OpenAi` | **translate** (§6.1; work unit R-10). This is the point of the surface: `ANTHROPIC_BASE_URL=http://127.0.0.1:8888` lets the Claude Code harness drive a local or rented model |
| `OpenAi` → `Anthropic` | **501** with an **OpenAI-shaped** error body (`{"error":{"type":"protocol_not_supported",…}}`). Permanently out of scope (§12) |

**OpenAI-compatible remains the canonical surface**, and the Anthropic side is a *translating ingress
only*, one direction. Two facts from the surrounding ecosystem fix that:
`ApexOS-RS/agentd/crates/gateway/src/compute.rs` sweeps the LAN probing `GET /v1/models` for the
OpenAI list shape and counts a candidate **only** when it answers in that shape, so being
byte-exact there is what makes ApexRouter adoptable as an ApexOS compute node at all — *necessary,
and, checked on 2026-07-31, not sufficient: that sweep probes only ports 11434, 8000, 1234 and
8080 (`OAI_PROBE_PORTS`), and 8888 is not among them.* Pasting `http://<host>:8888/v1` into the
Settings compute field works today because adoption re-verifies by the same shape check; automatic
discovery additionally needs the proxy bound to a probed port, or mDNS (mk2, §12). `docs/AGENTS.md`
carries both recipes. And `ApexOS-RS/agentd/crates/agent/src/provider.rs` is already a provider
trait over "Anthropic native, OpenAI-compat, OpenRouter" with `anthropic.rs` and `oai.rs`, whose
Anthropic calls go straight to `api.anthropic.com` with a real key. ApexOS does not want a
translating proxy. Nothing in the ecosystem ever needs OpenAI → Anthropic.

**Endpoint lifecycle types** (facts only; see invariant 3):

```rust
pub struct EndpointRef { pub id: BackendId, pub kind: BackendKind }

/// What gets persisted to $STATE/endpoints/<id>.json. NOTE: no `status` field.
pub struct EndpointRecord {
    pub id: BackendId,
    pub spec: EndpointSpec,
    pub desired: DesiredState,       // Running | Stopped — expectation IS state, and is a fact
    pub proc: Option<ProcFacts>,     // None for Managed/Node endpoints
    pub port: Option<u16>,
    pub log_path: Option<String>,
    pub started_at_unix: i64,
    pub fit: Option<FitPlan>,        // what we planned; used for VRAM reservation accounting
    pub adopted: bool,
    pub alias_bindings: Vec<Alias>,  // as built: which aliases point here, so `endpoint ls` and
                                     // the Slint/web cards can show it without recompiling routes
}
#[serde(rename_all = "snake_case")]
pub enum DesiredState { Running, Stopped }

pub struct ProcFacts { pub pid: u32, pub start_time_ticks: u64, pub boot_id: String,
                       pub exe: String, pub cmdline_sha256: String }

#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EndpointSpec {
    LocalLlama(LocalLlamaSpec),
    LocalVllm(LocalVllmSpec),
    Vast(VastSpec),
    Node(NodeSpec),
    Managed(ManagedSpec),
}

pub struct LocalLlamaSpec {
    pub build: BuildId,
    pub model_path: String,          // absolute, expanded — the raw ~ form is NEVER stored
    pub mmproj: Option<String>,
    pub alias_flag: String,          // -a value: the model id this server advertises
    pub host: String,                // default 127.0.0.1
    pub port: Option<u16>,           // None = allocate from [endpoints] port_range
    pub ctx: Option<u32>,            // None = leave --ctx-size UNSET so llama.cpp --fit works
    pub parallel: Option<u32>,
    pub kv_type: Option<KvType>,
    pub ngl: NglPlan,
    pub split: SplitPlan,            // devices + -sm + -mg + --tensor-split  (00b)
    pub mode: SamplingMode,
    pub flash_attn: Option<TriState>,
    pub api_key: Option<CredentialSource>,   // written via --api-key-file, never argv
    pub extra_args: Vec<String>,
}
#[serde(rename_all = "snake_case")]
pub enum SamplingMode { Thinking, Coding, Nonthinking, Raw }
#[serde(rename_all = "snake_case")]
pub enum TriState { On, Off, Auto }

pub struct LocalVllmSpec { pub bin: String, pub model_id: String, pub tp: Option<u32>,
                           pub ctx: Option<u32>, pub quantization: Option<String>,
                           pub kv_cache_dtype: Option<String>, pub enforce_eager: bool,
                           pub reasoning_parser: Option<String>, pub gpu_util: Option<f32>,
                           pub host: String, pub port: Option<u16>, pub extra_args: Vec<String> }
pub struct NodeSpec    { pub base_url: String, pub credential: CredentialSource,
                         pub label: String, pub declared_models: Vec<String>,
                         pub protocol: Protocol }          // default OpenAi
pub struct ManagedSpec { pub provider: String, pub base_url: String,
                         pub credential: CredentialSource, pub model_id: Option<String>,
                         pub protocol: Protocol }          // default OpenAi
pub struct VastSpec    { pub instance_id: InstanceId, pub runtime: ContainerRuntime,
                         pub launch: ContainerLaunch, pub tunnel: Option<TunnelSpec> }
```

**Adoption and liveness** (`core::proc`, not protocol — these hold `io::Error`):

```rust
pub enum Liveness { Alive, Zombie, Dead, Unknown(std::io::Error) }
pub enum Adoption {
    Adopted(ProcFacts),          // identity matches -> we own it, we may signal it
    Foreign { pid: u32, why: String },  // something else holds the port / pid. NEVER signalled.
    Vanished,                    // gone; desired==Running -> Failed, desired==Stopped -> tidy
    Ambiguous { pid: u32, why: String },
}
pub fn liveness(f: &ProcFacts) -> Liveness;      // parse /proc/<pid>/stat AFTER THE LAST ')'
pub fn adopt(rec: &EndpointRecord) -> Adoption;  // pid ∧ start_ticks ∧ boot_id ∧ exe ∧ cmdline
```

`boot_id` (`/proc/sys/kernel/random/boot_id`) is part of the identity because `start_time_ticks` is
measured since boot and is *not* comparable across a reboot. `exe` is compared advisory-only and the
`" (deleted)"` suffix is stripped, so rebuilding `build-vulkan` under a running server does not
un-adopt it. `Foreign` is never signalled: `POST /v1/endpoints/{id}/adopt` exists, requires
`/props` (or `/v1/models`) to match the spec's model path, and records `adopted: true`.

### 3.5 Routes — the product

```rust
pub struct ModelRoute {
    pub alias: Alias,
    pub targets: Vec<RouteTarget>,   // ordered
    pub strategy: Strategy,
    pub filter: RouteFilter,
    pub retry: RetryPolicy,
    pub is_default: bool,
    pub description: Option<String>,
}
pub struct RouteTarget { pub backend: BackendSelector, pub model: Option<String>, pub weight: u32 }
#[serde(tag = "sel", rename_all = "snake_case")]
pub enum BackendSelector { Id(BackendId), Tag(String), Glob(String) }   // "vast-*"

/// mk1 ships EXACTLY the strategies it implements. There is no reachable `todo!()` from config.
#[serde(rename_all = "snake_case")]
pub enum Strategy { FirstHealthy, RoundRobin, LeastBusy, Cheapest }

pub struct RouteFilter { pub require_tags: Vec<String>, pub exclude_tags: Vec<String>,
                         pub max_cost_per_mtok: Option<Money>, pub min_ctx: Option<u32>,
                         pub require_vision: bool, pub require_tools: bool }
pub struct RetryPolicy { pub attempts: u8, pub failover: bool, pub honor_retry_after: bool }

#[serde(rename_all = "snake_case")]
pub enum RouteReason { Alias, ExplicitPin, UpstreamIdMatch, ImplicitMulti,
                       DefaultFallback, LegacyModelName }
#[serde(rename_all = "snake_case")]
pub enum SwapMode { Hot, Sequential }   // chosen by fit(), overridable
```

### 3.6 Telemetry

```rust
pub struct RequestRecord {
    pub id: RequestId, pub started_unix: i64,
    pub alias: Option<Alias>, pub backend: Option<BackendId>,
    pub upstream_model: Option<String>, pub route_reason: RouteReason,
    pub ingress: Protocol,               // which dialect the CLIENT spoke; the upstream's is on
                                         // the Backend, so (ingress, upstream) names the cell
    pub method: String, pub path: String,
    pub status: u16, pub attempts: u8, pub streamed: bool, pub aborted: bool,
    pub ttft_ms: Option<u32>, pub total_ms: u32,
    pub prompt_tokens: Option<TokenCount>, pub completion_tokens: Option<TokenCount>,
    pub cached_tokens: Option<u32>,      // llama.cpp timings.cache_n
    pub tok_per_s: Option<f32>,          // timings.predicted_per_second
    pub cost: CostEstimate,
    pub error: Option<String>,
}

/// The on-disk usage row. LEGACY FIELD NAMES ARE PRESERVED EXACTLY so cost.py still parses it.
pub struct UsageRecord {
    pub timestamp: String,               // RFC3339 UTC going forward; legacy parsed leniently
    #[serde(default, skip_serializing_if = "Option::is_none")] pub epoch: Option<f64>,
    pub provider: String,                // "vast-gguf" stays on the wire
    pub model_id: String,
    pub prompt_tokens: u32, pub completion_tokens: u32, pub cost_usd: f64,
    // additive; old readers ignore these
    #[serde(default)] pub request_id: Option<String>,
    #[serde(default)] pub backend: Option<String>,
    #[serde(default)] pub alias: Option<String>,
    #[serde(default)] pub ttft_ms: Option<u32>,
    #[serde(default)] pub tok_per_s: Option<f32>,
    #[serde(default)] pub stream: Option<bool>,
    #[serde(default)] pub estimated: Option<bool>,
    #[serde(flatten)] pub extra: serde_json::Map<String, serde_json::Value>,
}
pub struct UsageSummary { pub window: String, pub by: Vec<UsageBucket>,
                          pub total_cost: CostEstimate, pub total_prompt: u64,
                          pub total_completion: u64, pub rows: u64 }
```

### 3.7 Recipes, profiles, and the container contract

`recipes.toml`'s 71 hand-written entries are replaced by **discovery + saved drafts**. A `Recipe` is
the *saved result* of a discovery session, with provenance so staleness is detectable. A
`SearchProfile` is a *query template* over the live Vast market, not a fixed tier.

```rust
pub struct Recipe {
    pub id: RecipeId, pub label: String, pub description: Option<String>,
    pub kind: RecipeKind,
    pub provenance: Provenance2,
    pub created_at_unix: i64, pub updated_at_unix: i64,
}
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecipeKind {
    Local(LocalLlamaSpec),
    LocalVllm(LocalVllmSpec),
    Vast { profile: ProfileId, launch: ContainerLaunch, fit: Option<FitPlan> },
    Managed(ManagedSpec),
}
pub struct Provenance2 { pub discovered_at_unix: i64, pub size_bytes: Option<u64>,
                         pub source: String, pub fit: Option<FitPlan> }
pub struct ValidationReport { pub ok: bool, pub issues: Vec<ValidationIssue> }
pub struct ValidationIssue { pub field: String, pub severity: Severity, pub message: String,
                             pub fix: Option<String> }

/// A Vast search profile. NEVER a hardcoded GPU enum: 00c proves gpu_name strings change.
pub struct SearchProfile {
    pub id: ProfileId, pub label: String,
    pub gpu_names: Vec<String>,          // ["RTX 3090"] — exact strings from the live vocabulary
    pub num_gpus_min: u32, pub num_gpus_max: u32,
    pub max_dph: Option<Money>, pub min_reliability: f32,
    pub min_inet_down: u32, pub min_disk_gb: u32,
    pub min_cuda: Option<f32>, pub geo: GeoFilter,
    pub image_type: ImageType,
    pub extra: serde_json::Map<String, serde_json::Value>,  // free-form query passthrough
}
#[serde(rename_all = "snake_case")]
pub enum GeoFilter { Any, EuNordic, Eu, Us, Codes(Vec<String>) }
#[serde(rename_all = "snake_case")]
pub enum ImageType { Prebuilt, Builder, Vllm }

/// THE CONTAINER CONTRACT. This is what makes a rented box actually serve tokens.
pub struct ContainerLaunch {
    pub runtime: ContainerRuntime,
    pub image: String,                   // resolved from [docker] by image_type
    pub image_type: ImageType,
    pub disk_gb: u32,
    pub env: BTreeMap<String, String>,   // the 25-var contract, built by core::argv
    pub onstart: String,                 // "bash /app/launch.sh > /var/log/launch.log 2>&1 &"
    pub host: String,                    // ALWAYS 127.0.0.1 — tunnel-only posture
    pub port: u16,                       // 8000
    pub expose_public: bool,             // false by default; true REQUIRES a minted api key
}
#[serde(rename_all = "snake_case")]
pub enum ContainerRuntime { LlamaCpp, Vllm }
pub struct ContainerEnvPreview { pub env: Vec<(String, String)>, pub onstart: String,
                                 pub image: String, pub warnings: Vec<String> }
```

The env map is produced by **one** builder (`core::argv`) that also produces the local argv, so the
`--top-k 20` divergence between `config.SAMPLING_PRESETS` and `launch.sh` cannot recur.
`launch.sh` is authoritative: all three presets include `--top-k 20`.

Variables emitted for `ContainerRuntime::LlamaCpp` (`04`, `06` external contract 1): `MODEL_REPO`,
`MODEL_QUANT`, `IMAGE_TYPE`, `CTX`, `KV_TYPE`, `MODE`, `N_GPU_LAYERS`, `PARALLEL`, `EXTRA_ARGS`,
`HF_TOKEN`, `MODELS_DIR`, `MMPROJ`, `PORT`, `HOST`, `LLAMA_CPP_REPO`, `LLAMA_CPP_REF`. For
`Vllm` (contract 2): `MODEL_ID`, `TP`, `CTX`, `QUANTIZATION`, `KV_CACHE_DTYPE`, `GPU_UTIL`,
`EXTRA_ARGS`, `HF_TOKEN`, `PORT`, `HOST`, `DTYPE`, `MAX_NUM_SEQS`, `TRUST_REMOTE`, `ENFORCE_EAGER`
(the literal string `"true"`/`"false"`), `CHUNKED_PREFILL`, `REASONING_PARSER`.

`known_forks` is a **read** config table, not a decoration:

```toml
[known_forks."deepseek-v4"]
match_repo = "deepseek-ai/DeepSeek-V4*"
llama_cpp_repo = "fairydreaming/llama.cpp"
llama_cpp_ref  = "deepseek-dsa"
```

`core::argv::plan_container()` looks the model repo up in `known_forks`; a hit **forces**
`image_type = Builder`, sets `LLAMA_CPP_REPO`/`LLAMA_CPP_REF`, and pushes a warning
`"custom fork → builder image → +12–18 min cold start"` into `ContainerEnvPreview.warnings`,
which every surface renders before the confirm.

`ENTRYPOINT` conflict resolution (`04` port note): the published images declare
`ENTRYPOINT ["/usr/bin/tini","-g","--","/app/launch.sh"]` *and* the onstart-cmd also runs it, which
would contend for port 8000. ApexRouter sets `onstart` to `bash /app/launch.sh …` and passes
`args` overriding the entrypoint to `sleep infinity`, so exactly one server starts. This is asserted
by a fixture test on the produced `PUT /asks/{id}/` body.

### 3.8 Vast

```rust
pub struct Offer {                    // ~28 named fields + everything else preserved
    pub id: u64, pub ask_contract_id: Option<u64>, pub machine_id: Option<u64>,
    pub gpu_name: String, pub num_gpus: u32,
    pub gpu_ram: u64, pub gpu_total_ram: u64,          // MiB; pooled is what fit() uses
    pub dph_total: f64, pub dph_base: Option<f64>,
    pub storage_cost: Option<f64>, pub inet_down_cost: Option<f64>, pub inet_up_cost: Option<f64>,
    pub cpu_ram: Option<u64>, pub cpu_cores_effective: Option<f64>, pub disk_space: Option<f64>,
    pub cuda_max_good: Option<f64>, pub driver_version: Option<String>,
    pub geolocation: Option<String>,                    // "Czechia, CZ" — parse, never string-equal
    pub inet_down: Option<f64>, pub inet_up: Option<f64>,
    pub reliability2: Option<f64>, pub direct_port_count: Option<u32>,
    pub static_ip: Option<bool>, pub rented: Option<bool>, pub rentable: Option<bool>,
    pub dlperf: Option<f64>, pub dlperf_per_dphtotal: Option<f64>,
    pub duration: Option<f64>, pub end_date: Option<f64>,
    #[serde(flatten)] pub extra: serde_json::Map<String, serde_json::Value>,
}
pub struct VastAccount { pub id: u64, pub credit: f64, pub balance: Option<f64>,
                         pub can_pay: Option<bool>, pub has_billing: Option<bool> }
                         // NOTE: no `api_key` field exists on this struct, by design.
pub struct VastInstance {
    pub id: InstanceId, pub actual_status: Option<String>, pub status_msg: Option<String>,
    pub ssh_host: Option<String>, pub ssh_port: Option<u16>, pub public_ipaddr: Option<String>,
    pub ports: serde_json::Value,     // docs say int[], the CLI reads a Docker map: tolerant accessor
    pub direct_port_start: Option<i32>, pub direct_port_end: Option<i32>,
    pub gpu_name: Option<String>, pub num_gpus: Option<u32>, pub gpu_util: Option<f64>,
    pub dph_total: Option<f64>, pub geolocation: Option<String>, pub label: Option<String>,
    pub start_date: Option<f64>, pub disk_util: Option<f64>, pub disk_space: Option<f64>,
    pub inet_down: Option<f64>,
    #[serde(flatten)] pub extra: serde_json::Map<String, serde_json::Value>,
}
impl VastInstance {
    pub fn phase(&self) -> BootPhase;
    /// exited | offline | unknown are TERMINAL — they never reach running.
    pub fn is_terminal(&self) -> bool;
    pub fn external_port(&self, internal: u16) -> Option<(String, u16)>;  // tolerant `ports` reader
}
pub struct LedgerRow {
    pub seq: u64, pub at_unix: i64, pub instance_id: Option<InstanceId>,
    pub state: LedgerState, pub offer_id: Option<u64>, pub profile: Option<ProfileId>,
    pub gpu: Option<String>, pub num_gpus: Option<u32>, pub dph: Option<f64>,
    pub approved_max_dph: Option<f64>, pub approval_source: Option<String>,
    pub destroyed_at_unix: Option<i64>, pub est_cost: CostEstimate, pub note: Option<String>,
}
#[serde(rename_all = "snake_case")]
pub enum LedgerState { Reserved, Confirmed, Running, DestroyRequested, Destroyed,
                       OrphanSuspect, Reconciled }
```

**`SpendApproval` — a value you cannot fabricate** (`core::money`, not protocol, so no surface can
build it with a struct literal):

```rust
#[non_exhaustive]
pub struct SpendApproval { max_usd_per_hour: Money, confirmed_at: i64, source: ApprovalSource }
pub enum ApprovalSource { Cli, WebUi, SlintUi, Mcp { human_cleared: bool }, Api }

impl SpendApproval {
    /// The ONLY constructor. Enforces the daemon-side hard ceiling, so an agent that
    /// fills in a big number still cannot exceed what the human configured.
    pub fn confirm(requested: Money, source: ApprovalSource, cfg: &VastConfig)
        -> Result<Self, ApprovalError>;
    pub fn max_usd_per_hour(&self) -> Money;
}
pub enum ApprovalError { AboveCeiling { requested: Money, ceiling: Money },
                         HumanConfirmationRequired { pending: JobId },
                         InsufficientCredit { credit: Money, needed: Money } }
```

There is no function signature in `providers::vast` that reaches a billing call without a
`SpendApproval` parameter. `require_human_confirm = true` makes an MCP-sourced approval return
`HumanConfirmationRequired { pending }`; the human clears it in either GUI or with
`apexrouter approvals grant <id>`.

### 3.9 Events (the WS protocol)

```rust
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Snapshot(Box<Snapshot>),
    BackendChanged  { backend: Box<Backend> },
    BackendRemoved  { id: BackendId },
    RouteTableChanged { routes: Vec<ModelRoute>, valid: bool, error: Option<String> },
    RigChanged      { rig: Box<RigSnapshot> },
    RequestStarted  { id: RequestId, alias: Option<Alias>, backend: Option<BackendId> },
    RequestFinished { record: Box<RequestRecord> },
    BootProgress    { backend: BackendId, phase: BootPhase, line: Option<String> },
    LogLine         { source: LogSource, line: String },
    VastFleetChanged{ instances: Vec<VastInstance>, credit: Option<f64> },
    UsageTick       { window: Box<UsageSummary> },     // coalesced to 1 Hz; boxed, as built —
                                                       // `Event` is cloned per subscriber
    JobChanged      { job: Box<JobRecord> },
    CheckResult     { result: CheckResult },
    Alert           { level: AlertLevel, message: String, action: Option<String>, id: String },
}
#[serde(rename_all = "snake_case")]
pub enum AlertLevel { Info, Warning, Serious, Critical }
#[serde(tag = "src", rename_all = "snake_case")]
pub enum LogSource { Endpoint { id: BackendId }, Instance { id: InstanceId }, Daemon }

pub struct Snapshot {
    pub product: String, pub version: String,
    pub served_by: ServedBy, pub as_of_unix: i64, pub stale: bool,
    pub proxy: ProxyStatus, pub backends: Vec<Backend>, pub routes: Vec<ModelRoute>,
    pub endpoints: Vec<EndpointRecord>, pub rig: RigSnapshot,
    pub instances: Vec<VastInstance>, pub tunnels: Vec<TunnelStatus>,
    pub providers: Vec<ProviderStatus>, pub recipes: Vec<Recipe>, pub profiles: Vec<SearchProfile>,
    pub totals: Totals, pub alerts: Vec<Alert>, pub jobs: Vec<JobRecord>,
}
#[serde(rename_all = "snake_case")]
pub enum ServedBy { Daemon, Offline }
/// As built: `control_url` so a client that only knows the proxy can find the control plane,
/// and `table_valid`/`table_error` so every surface can show the red banner of §4.1 without
/// asking a second endpoint.
pub struct ProxyStatus { pub base_url: String, pub control_url: String, pub uptime_secs: f64,
                         pub inflight: u32, pub req_per_min: f32, pub tok_per_s: f32,
                         pub default_alias: Alias,
                         pub table_valid: bool, pub table_error: Option<String> }
pub struct Totals { pub spend_24h: CostEstimate, pub spend_7d: CostEstimate,
                    pub tokens_24h: u64, pub vast_credit: Option<f64>,
                    pub burn_rate_usd_hr: Money, pub burn_down_hours: Option<f32> }
pub struct JobRecord { pub id: JobId, pub kind: String, pub state: JobState,
                       pub pct: Option<f32>, pub message: Option<String>,
                       pub started_unix: i64, pub finished_unix: Option<i64>,
                       pub result: Option<serde_json::Value>, pub error: Option<String> }
#[serde(rename_all = "snake_case")]
pub enum JobState { Pending, Running, Succeeded, Failed, Cancelled }
```

---

## 4. Runtime subsystems

### 4.1 The routing table and live backend state

The single most important structural fix relative to every source proposal: **the compiled table
holds `Arc<LiveBackend>` pointers, so rebuilding the table never resets live state.**

```rust
// apexrouter-router
pub struct LiveBackend {                      // one per backend, created once, mutated in place
    pub id: BackendId,
    pub meta: ArcSwap<Backend>,               // the serialisable description
    pub sem: Arc<Semaphore>,                  // sized from /props.total_slots or config
    pub breaker: Breaker,                     // atomics
    pub retry_bucket: TokenBucket,
    pub inflight: AtomicU32,
    pub accepting: AtomicBool,                // false while draining
    pub latency: LatencyEwma,
    pub model_index: ArcSwap<Vec<String>>,    // maintained by the prober; read by resolve() rule 3
}
pub struct BackendRegistry { inner: RwLock<HashMap<BackendId, Arc<LiveBackend>>> }
pub struct RoutingTable {
    by_alias: HashMap<Alias, CompiledRoute>,
    by_upstream_id: HashMap<String, SmallVec<[Arc<LiveBackend>; 2]>>,
    by_id: HashMap<BackendId, Arc<LiveBackend>>,
    default_alias: Alias,
    legacy_model_names: HashSet<String>,      // "", "x", "auto", "default"
    generation: u64,
}
pub struct RouterInner {
    table: ArcSwap<RoutingTable>,
    registry: BackendRegistry,
    http: reqwest::Client,                    // ONE pooled client, no_gzip/no_brotli/no_deflate
    inflight_bytes: Arc<Semaphore>,           // GLOBAL byte budget, not just a request count
    ring: Mutex<VecDeque<RequestRecord>>,
    events: broadcast::Sender<Event>,
    usage: UsageWriter,
    cfg: ArcSwap<Config>,
}
```

`TableBuilder::compile(&Config, &RouteFile, &BackendRegistry) -> Result<RoutingTable, CompileError>`
clones `Arc`s out of the registry. Reload = parse → compile → validate → `ArcSwap::store`. **A failed
compile keeps the running table**, raises an `Alert`, and shows red in both GUIs and in
`apexrouter status`. Watching is `notify` + 250 ms debounce + a 10 s poll fallback + `SIGHUP` +
`POST /v1/reload`. The watcher watches **`$CONFIG` and `$STATE/routes.json` only** — never a
directory containing endpoint logs, which children write to continuously.

### 4.2 Resolution — deterministic, observable, I/O-free

```rust
// As built, on `RoutingTable`. The unknown-model policy is a PARAMETER, not a field read off a
// config the table snapshotted: the table is rebuilt rarely and `[router] unknown_model` can be
// reloaded under it, so rule 6 must consult the live config on the request rather than a copy.
pub fn resolve(&self, model: Option<&str>, class: RequestClass, unknown: UnknownModelPolicy)
    -> Result<Plan, RouteError>;
pub enum UnknownModelPolicy { Reject, Fallback }
pub struct Plan { pub candidates: SmallVec<[Candidate; 4]>, pub reason: RouteReason,
                  pub alias: Option<Alias>, pub rewrite_model_to: Option<String>,
                  pub retry: RetryPolicy }
pub struct Candidate { pub backend: Arc<LiveBackend>, pub upstream_model: String }
```

`Plan::retry` is **the route's own `[retry]` block**, and it is on the `Plan` because that is the
only path from `routes.toml` to the attempt loop: rules 1, 5 and 6 copy it off the matched
`CompiledRoute`; rules 2, 3 and 4 name a backend rather than a route, so they carry
`RetryPolicy::default()` — which is also what a route declaring no `[retry]` block compiles to.
`attempts` and `failover` bound the loop in §4.3, and `honor_retry_after` is consumed inside
`attempt()`, where an upstream `Retry-After` is read. A `Plan` without this field is how a config key
gets parsed, validated and then silently ignored.

Order, and what each rule buys:

1. `model` matches an **alias** → that route. (`auto`, `coder`, `big`, `local`.) → `RouteReason::Alias`
2. `model` is `"<backend_id>/<upstream_model>"` → **explicit pin**, one candidate, no failover
   unless the route says so. → `ExplicitPin`
3. `model` exactly matches an **upstream model id** in `by_upstream_id` with exactly one enabled
   backend → route there. **This is what makes every existing client work unchanged.** →
   `UpstreamIdMatch`
4. Same id on several backends → implicit route using `[router] implicit_strategy`. →
   `ImplicitMulti`, plus a one-shot `Alert` naming the collision (a duplicate id appearing after a
   rental is exactly when routing silently changes under you).
5. `model` is in `legacy_model_names` (`""`, `"x"`, `"auto"`, `"default"`, absent) → `default_alias`.
   **This is why `smoke.sh`'s hardcoded `"model":"x"` keeps working.** → **`LegacyModelName`**
   (the wire token is `legacy_model_name`). `DefaultFallback` / `default_fallback` is **only**
   rule 6 under `[router] unknown_model = "fallback"`.
6. Anything else → behaviour from `[router] unknown_model`, **default `reject`**: `404`
   `model_not_found` listing known aliases. Set `= "fallback"` to get LocalRouter's old behaviour
   (`RouteReason::DefaultFallback`). Rejecting by default is deliberate: a fat-fingered
   `gpt-4o-mimi` must not silently bill a rented H100.

**As built, two classes never route on the model string.** `GET`/`HEAD` on `RequestClass::Models`
is answered by R-06 straight from the table, **before `resolve()` is called at all** — no upstream
hop, and the response carries `X-ApexRouter-Route: -|-` (no alias, no reason) precisely because
nothing was resolved. Inside `resolve()`, `Models` (other methods) and `Opaque` take the default
alias's **primary target only** (`candidates.truncate(1)`) with `RouteReason::LegacyModelName`,
because an arbitrary vendor path is not safely failoverable.

`resolve()` is **synchronous and does no I/O**. Rule 3 reads `model_index`, which the health prober
maintains; a cold index means rule 3 misses and rule 5/6 applies, which is documented and visible in
`X-ApexRouter-Route`. Every response carries `X-ApexRouter-Route: <alias-or-"-">|<reason>`.

Compile-time validation rejects: a dangling target, a duplicate alias, an alias that shadows a live
upstream id (unless `allow_shadow`), an unsatisfiable `require_tags`, and `Strategy::Cheapest` on a
route where no target has a price model *and* no `tps_hint` (it would be an invented number).

### 4.3 The request pipeline

```
inbound
 ├─ Host / Origin gate on mutations (§9.3)
 ├─ normalise path: collapse a repeated leading /v1 to one; log once per (UA, path) at debug
 ├─ loop guard: `Via` already contains "apexrouter" → 508 loop_detected
 ├─ classify: Models | Chat | Completion | Embedding | Rerank | Opaque
 ├─ record ingress: Protocol — Anthropic for /v1/messages, or when `anthropic-version` is
 │    present on /v1/models; OpenAi otherwise. Stored on the RequestRecord (§3.6)
 ├─ acquire global byte budget (max_inflight_bytes), then read body (max_body_bytes → 413)
 ├─ RequestPeek: a top-level-key scanner (NOT a full serde_json::Value parse) for
 │    {model, stream (strict bool), stream_options.include_usage}
 ├─ resolve(model, class) -> Plan
 └─ for candidate in plan, bounded by RetryPolicy.attempts AND a wall-clock deadline:
      ├─ (ingress, candidate.protocol) selects the matrix cell (§3.4): relay | translate | 501
      ├─ breaker.check()            → skip if Open (min_volume 5 before it can trip)
      ├─ InFlightGuard::acquire(backend).timeout(queue_timeout) → 503 + Retry-After if saturated
      ├─ outbound_headers(inbound, cred)  → CONSTRUCTED from an allowlist; inbound map never cloned
      ├─ body: Passthrough(bytes) when alias == upstream id, else Rewritten (only "model" changes)
      ├─ send with connect_timeout, then headers_timeout
      └─ classify: connect/DNS/TLS/pre-header timeout → Retryable + breaker.trip()
                   429 with Retry-After → Retryable on a DIFFERENT target
                   502/503/504/529 → Retryable
                   any other status → Terminal, relay verbatim
 ├─ FIRST BYTE ⇒ Committed. No retry past this point, ever.
 ├─ relay
 └─ InFlightGuard::drop → permit released, gauge decremented, RequestFinished emitted
```

Two invariants expressed as types rather than comments:

```rust
/// The retry loop consumes PreFlight values and can only exit by producing a Committed.
/// "Never retry after the first byte" is unrepresentable, not merely documented.
async fn attempt(p: PreFlight<'_>) -> Result<Committed, Retryable>;

/// Owns the OwnedSemaphorePermit, the inflight gauge decrement, the byte-budget permit,
/// and the RequestRecord. Its Drop emits RequestFinished{aborted:true} if `finish()` was
/// never called. A client Ctrl-C therefore CANNOT leak a permit or a zombie UI row.
pub struct InFlightGuard { /* … */ }
impl InFlightGuard { pub fn finish(self, rec: RequestRecord); }
impl Drop for InFlightGuard { /* … */ }
```

### 4.4 Streaming

- Byte-for-byte relay into `Body::from_stream`, **never re-framed** — a chunk boundary may split an
  SSE event and every OpenAI SDK buffers on `\n\n`.
- `Content-Type: text/event-stream` is forced **only** when upstream is 2xx *and* already says
  `text/event-stream`. A `400 {"error":…}` on a `stream:true` request reaches the client as JSON.
- **Never a total timeout on a stream.** `connect_timeout` (5 s) + `headers_timeout`
  (**600 s** default — for a non-streaming completion llama.cpp sends no headers until the body is
  ready, and 600 tokens at 4 tok/s is 150 s of generation before the first byte) + an
  **inter-chunk idle timeout** (300 s).
- Client disconnect drops `Committed`, which cancels the reqwest future and aborts upstream, so
  llama.cpp stops generating and frees its slot. Integration-tested by dropping mid-stream and
  asserting `/slots` frees within 1 s.
- **Mid-stream upstream death** has a defined client-visible behaviour: emit one synthetic
  `data: {"error":{"message":"upstream ended mid-stream","type":"upstream_unavailable"}}` frame
  followed by `data: [DONE]`, then close. Never a silent truncation. A **clean EOF with no
  `data: [DONE]` terminator is death too** — a socket closing politely mid-generation looks
  identical to a finished stream to every SDK, so it gets the same pair. The idle timeout gets its
  own, `"type":"upstream_timeout"`. There is exactly one implementation of these rules
  (`router::relay::stream`); the proxy handler calls it and holds no framing code of its own.
- A **tee** watches the tail for `usage` / `timings`. It is best-effort and never gates the relay;
  when the provider emits nothing the record degrades to `TokenCount::Estimated` /
  `CostEstimate::Approximate`.
- `[router] request_usage` defaults to **`"off"`**. Injecting `stream_options.include_usage` when
  the client did not ask changes what every streaming client receives; opting in is a choice, not a
  default, and when on we do **not** filter the extra chunk back out (that would break the
  byte-exactness claim in the other direction). Documented in `docs/API.md`.
- `X-Accel-Buffering: no`, `Cache-Control: no-cache`.
- **`X-Usage` is emitted on buffered responses only.** Response headers flush before the first SSE
  chunk and usage arrives in the last one; a streaming `X-Usage` would be absent or a lie. On
  streams we set `X-ApexRouter-Usage-Deferred: true` and the numbers land in `usage.jsonl`, the WS
  event and the live-request table. This is a stated, tested divergence from LocalRouter.

### 4.5 Headers, errors, limits, metrics

Outbound headers are **constructed** from an allowlist (`content-type`, `accept`,
`accept-encoding: identity`, `user-agent`, `x-request-id`, plus a configurable extra list) and the
backend's own credential. The inbound `HeaderMap` is never cloned, so a client's `Authorization`
cannot reach a third party — and a local `llama-server --api-key` becomes reachable through the
proxy for the first time. Added: `X-Request-Id`, `Via: 1.1 apexrouter`. Preserved for compat:
`X-Provider`, `X-Usage`. Added: `X-ApexRouter-Backend`, `X-ApexRouter-Route`,
`X-ApexRouter-Attempts`, `X-ApexRouter-Fallback`, and — whenever the ingress is not `open_ai` —
`X-ApexRouter-Protocol: <ingress>-><upstream>` (e.g. `anthropic->open_ai`), so which matrix cell ran
is observable exactly like `X-ApexRouter-Route`. The Anthropic ingress adds two inbound headers to
the never-forwarded set: `x-api-key` and `anthropic-version` are consumed by the proxy and never
reach an upstream.

Errors are OpenAI-shaped everywhere:
`{"error":{"message":…,"type":…,"code":…,"param":null}}`, with
`model_not_found`→404, `upstream_unavailable`→502, `upstream_timeout`→504,
`no_healthy_backend`→503, `server_overloaded`→503 + `Retry-After`, `request_too_large`→413,
`loop_detected`→508, `provider_not_configured`→503 (the 502-vs-503 distinction is load-bearing in
both house projects), `starting`→503.

Concurrency: one `Semaphore` per backend sized from `/props.total_slots` when available (the argv
builder **passes `--props`, `--metrics` and `--slots`** to every server ApexRouter launches, feature
detected — otherwise the sizing would read an endpoint we never enabled), else from `/slots` length,
else config's **`[router] max_inflight` as the per-backend permit default** (not a global count).
A **global `max_inflight_bytes`** budget caps resident body memory — a count cap alone would permit
64 × 32 MiB of RSS. Retry budget is a **per-backend token bucket** (`retry_budget_per_min`, spent
on failover retries after the first attempt), so a struggling backend cannot be amplified into a
storm. The breaker requires `min_volume` (5) observations before it can open, so a single 200 ms
blip on a 1 rps rig does not create a 30 s outage.

**As built, `/metrics` is written but not yet mounted.** `router::telemetry::Telemetry::prometheus
(&BackendRegistry, Option<&RigSnapshot>) -> String` produces the exposition below and is
unit-tested against it, but no `.route("/metrics", …)` line exists in
`server::lib::v1_routes()`, so the control listener currently `404`s it. That gap is *held open
deliberately rather than forgotten*: `crates/apexrouter-server/tests/openapi_routes.rs` carries it
on an explicit `PENDING` list naming the owing unit, and **the test fails the moment it is wired**
so the list cannot rot. (`POST /v1/migrate` (§6.2) sat in the same position until 2026-08-01, when
`api::migrate` and its `.merge(…)` line landed; `/metrics` is now that list's one entry.) Treat
`PENDING` in that test as the authority on what is still outstanding.

`GET /metrics` on the control listener (Prometheus text): `apexrouter_requests_total{alias,backend,
status}`, `apexrouter_ttft_seconds`, `apexrouter_tokens_total{kind}`,
`apexrouter_tokens_per_second`, `apexrouter_backend_up{backend}`, `apexrouter_inflight{backend}`,
`apexrouter_queue_depth`, `apexrouter_cost_usd_total{provider}`, `apexrouter_vram_free_mb{device}`.
llama.cpp's `/slots` is read internally and **never proxied outward** — it echoes prompts. A request
for `/slots` through the proxy returns `403 {"error":{"type":"redacted_endpoint"}}` with the reason.

### 4.6 The local supervisor

Every measured defect in `03` is closed by construction:

| Defect | Fix |
|---|---|
| build-vulkan trailing-colon RUNPATH picks up a sibling build's `.so` | child env always gets `LD_LIBRARY_PATH = dirname(binary)`; `cwd` is `$STATE` and is never load-bearing |
| backend detection greps `--help` and reports `cuda` on an AMD box | `llama-server --list-devices`, `llvmpipe` excluded; sibling `libggml-*.so` inspection as fallback |
| fixed candidate list misses `build-mtp` / `build-zaya1` | glob `build*/bin/llama-server` under every configured root and `$PATH`, label by build-dir name |
| binary picked by substring heuristic, silently choosing HIP | `BinaryChoice::{Exact, Fallback { got, wanted }, None }` — a fallback is a **visible** value the UI renders as a warning, never a silent substitution |
| log fd leaked per start | owned `File` consumed by `Stdio::from` |
| server dies with the terminal | `setsid()` |
| no port check | `TcpListener::bind` probe under a per-endpoint lock, **held until the child's health gate passes**, so two concurrent launches cannot both win port 8100. `LaunchError::PortInUse { port, held_by: Option<BackendId> }` names the holder |
| "60 second" health loop that can run 4 minutes and then orphans the child | a real wall-clock deadline (`health_deadline_ms`, default 600 000) that **resets on progress** (a 503 `{"status":"loading model"}`, or a recognised load line, means it is alive and working). On expiry: kill the child, remove the record, mark `Failed{reason}` with the log tail, clear any route. **The failure path is the stop path.** |
| no PID-reuse guard, `EPERM` crashes | `ProcIdentity` incl. `boot_id`; `Liveness::Unknown(io::Error)` must be matched |
| truncate-on-start destroys the crash log | rotation with **copytruncate** semantics (an adopted child holds an fd to that inode; renaming would send its output into a deleted file). Children we did not spawn are never rotated |
| no VRAM admission control | `fit()` runs at plan time against **live** free VRAM minus `reserved_mb` held by running endpoints. `LaunchError::InsufficientVram { need_mb, free_mb, held_by }` unless `--force` |
| local launch has no progress model | `Health::Starting { phase }` for local too, driven by log markers (`load_tensors`, `llama_context`, `main: server is listening`) plus the `/health` 503-vs-refused distinction, streamed as `BootProgress` |

Argv construction is `core::argv::plan_local(&LocalLlamaSpec, &LlamaBuild) -> ArgvPlan`, emitting a
flag **only if the probed `FlagSupport` has it**: `-m`, `--host`, `--port`, `-a`, `-dev`, `-sm`,
`-mg`, `--tensor-split`, `-c` *(omitted when `ctx` is `None`, so llama.cpp's own `--fit` can size
it)*, `-np`, `-ctk`, `-ctv`, `-ngl` *(omitted when `NglPlan::Auto`)*, `-fa on|off|auto`,
`--no-jinja` *(the meaningful flag; `--jinja` is default-on in b9199 and passing it is a no-op)*,
`--metrics`, `--props`, `--slots`, `--mmproj`, `--api-key-file`, the sampling preset, then
`extra_args`. Sampling presets (`launch.sh` authoritative — **all three include `--top-k 20`**):

| mode | flags |
|---|---|
| `thinking` | `--temp 1.0 --top-p 0.95 --top-k 20 --min-p 0.0 --presence-penalty 1.5` |
| `coding` | `--temp 0.6 --top-p 0.95 --top-k 20 --min-p 0.0 --presence-penalty 0.0` |
| `nonthinking` | `--temp 0.7 --top-p 0.80 --top-k 20 --min-p 0.0 --presence-penalty 1.5 --chat-template-kwargs {"enable_thinking":false}` |

Backend env: `GGML_VK_VISIBLE_DEVICES` / `HIP_VISIBLE_DEVICES` / `CUDA_VISIBLE_DEVICES` chosen from
the build's detected backend, and `-dev` carries the explicit device list regardless.

### 4.7 Swap — one verb, the mode chosen for you

```rust
pub async fn swap(alias: &Alias, to: &EndpointSpec, mode: Option<SwapMode>) -> Result<SwapReport>;
```

Default mode comes from `fit()`: if the new model fits **alongside** everything currently resident,
`SwapMode::Hot`; otherwise `SwapMode::Sequential`. `--mode hot|sequential` overrides.

**Hot** — start B on a free port; health-gate B (failure aborts with A untouched); one
`ArcSwap::store` repoints the alias; drain A (`accepting = false`, wait `inflight == 0` using the
router's **own** counter, not the upstream's `/slots`, which 501s on `--no-slots` builds); stop A.
In-flight requests keep the upstream captured at dispatch and finish against A;
`X-ApexRouter-Backend` tells the client which one served it.

**Sequential** — a **warm window** is opened on the alias *before A is touched*; A is drained and
stopped; B starts; arriving requests **park** on a `tokio::sync::Notify` behind a bounded queue
(`warm_queue_max`, default 32); the window is closed *after* the alias points at B, which wakes
everything parked to re-resolve onto B. The window is an RAII guard, so every failure path —
including a rollback and including a panic — releases the parked requests onto the restored A
instead of leaving them to wait out the timeout. Parked depth is broadcast so both GUIs show
"warming, N parked", and `SwapReport::parked` is the peak the queue reached.

`warm_timeout` is **not an independent number and not a total budget.**

* *Not independent*: it is the budget of the thing being waited on —
  `[supervisor] health_deadline_ms + [server] drain_timeout_secs`, floored at
  `[router] queue_timeout_ms`, because a sequential swap spends the drain before the start budget
  even begins. A 90 s park against a 180 s load is an arithmetic guarantee of failure.
* *Not a total budget*: it measures wall clock **since the last sign of life**, and is re-armed for
  as long as the launch is alive (`api/routes.rs::start_while_parked` →
  `WarmWindow::rearm`). This mirrors the health gate, whose deadline has always reset on observed
  progress — the gate and the park wait on the same event, so they share one liveness signal.
  Read as a stopwatch it was an outage with a delay on it: measured, a 3000 ms window against a
  12,038 ms swap `503`'d its 4 parked requests at 2977 ms and the alias then answered 74,550
  requests with `no_healthy_backend` over the remaining nine seconds. The re-armed window costs
  **zero 5xx** across the same swap. A bound on patience is kept for the case where progress
  genuinely stops, and that is what the deadline now measures.

On overflow the client gets an OpenAI-shaped `503 warm_queue_full` with `Retry-After` immediately,
rather than deepening a queue that is already the wrong answer; on expiry, `503 warm_timeout` with
`Retry-After`. A client that hangs up mid-park gives its slot back from `Drop`.

`[router] warm_queue_max` is a real configuration key on `core::config::RouterCfg` (default 32).
The bound is an argument at every level — `WarmRegistry::open(alias, timeout, max)` — and
`api/routes.rs::warm_queue_max` is the single function that reads it, so nothing else in the
daemon or the router knows the number. Measured against one 12 s sequential swap with twelve
concurrent clients: at `warm_queue_max = 4` the peak parked depth is 4 and the rest get
`503 warm_queue_full` immediately; at `16`, all twelve park and the swap costs **zero** 5xx.

Swap is a **daemon operation**. The CLI verb is an RPC to `POST /v1/routes/{alias}/swap`; it is not
a file write plus a hope that the watcher noticed. (This is the one place the library-first proposal
was internally contradictory, and it is resolved by ownership.)

### 4.8 Vast: money safety

- `SearchProfile` → one `OfferQuery` → **one** search path. The documented bug where "auto —
  cheapest" rented from a stricter candidate set than the browser displayed dies because there is
  one query builder. If a relaxation happens (geo dropped, reliability 0.99→0.97) it is returned in
  `OfferSearchResult.relaxations: Vec<String>` and every surface shows it as an explicit banner.
- `PUT /api/v0/search/asks/` with `{"q": …}` (verified in `00c`), `PUT /api/v0/asks/{offer_id}/`
  to rent — the instance id comes back as **`new_contract`**, not `id`.
- **Reserve before billing.** `ledger.reserve(&spec)` appends a `Reserved` row and returns a
  `PendingLaunch` guard; the create call happens; `pending.commit(instance_id)` appends `Confirmed`.
  `impl Drop for PendingLaunch` appends `OrphanSuspect` **synchronously** (the ledger has an
  explicit blocking append path precisely because `Drop` cannot `await`). A `SIGKILL` skips `Drop`
  entirely — which is why the `Reserved` row, written *before* the call, is the real protection.
- `BootPhase` machine driven by `PUT /api/v0/instances/request_logs/{id}/` → two-phase `result_url`
  poll (**no Bearer on the result fetch**; first-fetch 403/404 is normal; backoff to ~30 s), with a
  `max_boot_secs` watchdog that auto-destroys a wedged instance, and terminal-status awareness
  (`exited`/`offline`/`unknown` never recover).
- **Download-stall detection *and* recovery.** The 4 s `/proc/net/dev` eth0 RX delta over SSH;
  `< 1000 bytes` = stalled, `< 50 Mbps` = slow. Detection is a passive alert on the instance card
  with a **one-click Restart download** that pkills `launch.sh` + `hf download` and re-execs
  `launch.sh` with the env recovered from `/proc/<pid>/environ`, forcing `HOST=127.0.0.1`. The
  inventory marks this `[yes] genuinely valuable — keep it`, so mk1 keeps both halves.
- Startup `reconcile()` runs in the background (never blocking bind, never blocking startup when
  offline; an unreachable API leaves rows untouched and raises an `Info` alert, it never infers
  `destroyed`). Every ledger row without `destroyed_at` is queried; anything billing without a live
  record raises a `Critical` alert with a Destroy action.
- `destroy` **verifies before forgetting**: `DELETE`, then poll until the instance is gone or
  terminal, and only then append `Destroyed` with the accrued cost.
- Instances are **never** auto-destroyed on daemon shutdown.

### 4.9 SSH tunnels — supervised

`TunnelSupervisor` owns the `ssh` `Child` (never `pgrep`, which can kill an unrelated ssh), writes a
`$STATE/tunnels.json` record with `ProcFacts`, and re-adopts on restart. Flags are exactly:
`-N -L <local>:127.0.0.1:8000 -p <port> root@<host> -o ExitOnForwardFailure=yes -o
ServerAliveInterval=30 -o ServerAliveCountMax=3 -o StrictHostKeyChecking=accept-new -o
UserKnownHostsFile=$STATE/ssh/known_hosts -o ControlMaster=auto -o
ControlPath=$STATE/ssh/cm-<instance-id> -o ControlPersist=5m`. Dedicated `known_hosts` because Vast
recycles `sshN.vast.ai` hostnames; ControlMaster because it is a measured ~500 ms → RTT win for
agentic tool loops.

The two things the source proposals dropped and this does not:

1. **A reconnect supervisor.** `ExitOnForwardFailure` makes ssh *exit* on a dead link; it does not
   re-establish it. A laptop wifi blip must not leave a `$3.34/hr` box unreachable until a human
   notices. Bounded retry with exponential backoff (1 s → ×2 → cap 30 s, `max_restarts_per_hour`),
   with each attempt broadcast and a `Serious` alert after the budget is exhausted.
2. **Teardown does `ssh -O exit` and unlinks the ControlPath.** Killing the `-N -L` child leaves the
   ControlMaster alive for `ControlPersist` minutes against a destroyed box.

Local ports come from a pool starting at 8800 — multiple rentals is the normal case now.

### 4.10 The Check registry

One `trait Check` in `core::checks`, backing `doctor`, `diagnose` and the four native smoke probes.
Checks run **concurrently** with per-check timeouts and stream as each lands, so
`--only rate-limits` is instant instead of waiting through four sequential SSH probes.

The registry is reached three ways, and *`diagnose` is not a CLI verb*: `apexrouter doctor [--only]`
renders the whole registry once; `GET /v1/diagnose?only=` streams it as SSE, one event per check
plus a terminal `done`; and the `apexrouter_diagnose` MCP tool is what an agent calls. `doctor` and
`diagnose` are the same checks with different transports, not two registries.

```rust
#[async_trait]
pub trait Check: Send + Sync {
    fn id(&self) -> CheckId;                  // "creds.vast", "ports.8888", "builds.vulkan", …
    fn label(&self) -> &str;
    fn needs(&self) -> CheckNeeds;            // Local | Network | Daemon | Instance
    async fn run(&self, ctx: &CheckCtx) -> CheckResult;
}
pub struct CheckResult { pub id: CheckId, pub status: CheckStatus, pub ms: u32,
                         pub detail: String, pub fix: Option<String> }
#[serde(rename_all = "snake_case")]
pub enum CheckStatus { Pass, Warn, Fail, Skipped }
```

mk1 check ids: `creds.{vast,hf,together}`, `ports.{proxy,control}`, `builds.discovered`,
`builds.flags`, `devices.enumerated`, `models.discovered`, `state.writable`, `legacy.migration`,
`proxy.roundtrip`, `ssh.controlmaster`, `ssh.binary`, `vast.credit`, `vast.orphans`,
`together.ratelimits`, `net.stall`, `endpoint.orphans`. The four smoke probes are
`smoke.models`, `smoke.warmup`, `smoke.tools`, `smoke.throughput` — reimplementing `smoke.sh`
natively with pass/fail badges, TTFT and tok/s read from the `timings` object, and using **the
resolved route's model id** rather than `smoke.sh`'s hardcoded `"model":"x"` (which 400s on every
managed provider).

---

## 5. Config and state on disk

### 5.1 Paths

```
$APEXROUTER_CONFIG  →  $APEXROUTER_HOME/config.toml  →  $XDG_CONFIG_HOME/apexrouter/config.toml
$APEXROUTER_HOME    →  $XDG_STATE_HOME/apexrouter/        (state; default ~/.local/state/apexrouter)
                       $XDG_CACHE_HOME/apexrouter/        (HF metadata, --help probes, offer cache)
```

Global CLI flags (`--config`, `--home`) are pushed into the process env **before** `Config::load()`,
so env vars stay the single resolution mechanism (house rule).

### 5.2 `config.toml` — hand-edited, every field defaulted

```toml
[server]
proxy_bind        = "127.0.0.1:8888"     # PROXY_PORT env overrides the port and IS honoured
control_bind      = "127.0.0.1:2739"     # APEX on a phone keypad
token_env         = "APEXROUTER_TOKEN"   # required for ANY non-loopback bind
loopback_bypass   = true
ui_dir            = ""                   # "" = the embedded ui-web; a path = live-reload dev loop
drain_timeout_secs= 30
autostart         = true                 # CLI Mutate verbs may start the daemon
proxy_cors_origins= []                   # as built. PROXY listener only, non-mutating paths
                                         # only, and an explicit allowlist. Empty (the default)
                                         # emits no CORS header at all — a deliberate difference
                                         # from `endpoint_proxy.py`, which set
                                         # `Access-Control-Allow-Origin: *` on every response.
                                         # A single "*" entry opts back into that. `POST /switch`
                                         # is excluded whatever is listed, and the control plane
                                         # still has no CorsLayer (§9.3, §12).

[router]
default_alias        = "auto"
implicit_strategy    = "first_healthy"
unknown_model        = "reject"          # reject | fallback   (reject = a typo 404s, loudly)
max_inflight         = 64
max_inflight_bytes   = 536870912         # 512 MiB GLOBAL budget — a count cap alone is not one
max_body_bytes       = 33554432          # 32 MiB per request (aiohttp's silent 1 MiB was a bug)
connect_timeout_ms   = 5000
headers_timeout_ms   = 600000            # non-streaming: no headers until generation finishes
idle_timeout_ms      = 300000            # BETWEEN stream chunks; never a total timeout on a stream
queue_timeout_ms     = 30000
retry_budget_per_min = 30
breaker_min_volume   = 5
request_usage        = "off"             # off | passthrough (injects stream_options.include_usage)
capture_bodies       = false             # prompts are NEVER stored unless this is on
log_usage            = true
anthropic_ingress    = true              # serve POST /v1/messages on the proxy listener (§6.1)
anthropic_tools      = true              # translate tool_use/tool_result <-> tool_calls. ON by
                                         # default since 2026-07-31 (CHARTER amendment): Claude
                                         # Code sends 92 tool definitions on EVERY request, so off
                                         # meant the ingress did not work at all for the client it
                                         # exists to serve. Still best-effort (§12). Set to false
                                         # and a body carrying `tools` is REFUSED, loudly.

[supervisor]
health_deadline_ms   = 600000            # REAL deadline, reset on observed progress
health_interval_ms   = 3000
adopt_on_start       = true
kill_children_on_exit= false
restart              = "never"           # never | on-failure
max_restarts_per_hour= 5
log_rotate_mb        = 32

[endpoints]
model_roots  = ["~/models", "~/.cache/huggingface/hub"]
build_roots  = ["~/llama.cpp", "~/Projects/llama.cpp", "/usr/local/bin"]
ignore_globs = ["**/.cache/**"]
port_range   = [8100, 8199]
default_mode = "thinking"
vram_margin_mb = 1024
scan_interval_secs = 300                 # background rescan; plan-time queries are always LIVE

[providers.together]                      # `[providers.<id>]`: base_url + where the key lives
base_url     = "https://api.together.ai/v1"
api_key_env  = "TOGETHER_API_KEY"        # a key is NEVER a required plaintext field here
# api_key_file = "~/.config/together/key" # as built: the file form, step 3 of the chain (§9.2)

[providers.vast]
base_url       = "https://console.vast.ai/api/v0"
api_key_file   = "~/.config/vastai/vast_api_key"
poll_min_ms    = 5000                    # Vast publishes no rate limits; never poll faster
max_boot_secs  = 1800
tunnel_port_range = [8800, 8899]
tunnels_on_shutdown = "adopt"
max_usd_per_hour_ceiling = 4.00          # HARD daemon-side cap; SpendApproval cannot exceed it
require_human_confirm    = false         # true => MCP rentals need a human to grant the approval

[hf]
token_file  = "~/.cache/huggingface/token"
download_dir = "~/models"

[docker]                                  # genuine config: Andre publishes these artifacts
prebuilt = "ghcr.io/buckster123/vastai-gguf:prebuilt"
builder  = "ghcr.io/buckster123/vastai-gguf:builder"
vllm     = "ghcr.io/buckster123/vastai-gguf:vllm"

[known_forks."deepseek-v4"]
match_repo     = "deepseek-ai/DeepSeek-V4*"
llama_cpp_repo = "fairydreaming/llama.cpp"
llama_cpp_ref  = "deepseek-dsa"

[compat]
read_legacy_state   = true               # read ~/.vastai-gguf for usage/providers/instances
mirror_usage_log    = false              # DEFAULT OFF: opt in to appending every usage row to
                                         # ~/.vastai-gguf/usage.log — another tool's state file
active_endpoint_path= ""                 # "" = off. A path mirrors .active_endpoint for the old TUI
legacy_proxy_pidfile= false              # DEFAULT OFF: the old TUI's Proxy→stop SIGTERMs this pid
allow_switch_hosts  = ["api.together.ai", "127.0.0.1", "localhost"]
```

`config.example.toml` at the repo root is this file, fully commented, with optional sections
commented out. Runtime-only fields are `#[serde(skip)]`; `ConfigFile` is what `save()` writes and it
**has no field for a borrowed credential**.

### 5.3 `$STATE`

| Path | Shape | Notes |
|---|---|---|
| `apexrouterd.lock` | JSON owner record | `flock LOCK_EX`, `O_CLOEXEC`, daemon-only |
| `state.lock` | empty | `LOCK_SH`/`LOCK_EX` around offline read-modify-write |
| `routes.json` | `{ "schema_version":1, "routes":[ModelRoute], "default_alias":"auto" }` | the table |
| `endpoints/<id>.json` | `EndpointRecord` | facts only; atomic tmp+fsync+rename, 0600 |
| `backends.json` | `Vec<Backend>` minus credentials | registry for Node/Managed backends |
| `tunnels.json` | `Vec<TunnelRecord>` | pid + identity, so restart re-adopts |
| `catalog.toml` | recipes + search profiles | `toml_edit` round-trip: hand comments survive |
| `credentials.toml` | `0600` | only keys the user typed into a GUI; a *borrowed* key is never copied here |
| `ledger.jsonl` | append-only `LedgerRow` | "active" is a query, not a single-slot file |
| `usage.jsonl` | append-only `UsageRecord` | legacy field names preserved exactly |
| `jobs/<id>.json` | `JobRecord` | `?no_wait` long-running operations |
| `logs/<backend-id>.log[.1]` | text | copytruncate rotation; never truncate-on-start |
| `logs/apexrouterd.log` | text | daemon's own stderr when autostarted |
| `ssh/known_hosts`, `ssh/cm-<id>` | ssh | dedicated; Vast recycles hostnames |
| `approvals/<id>.json` | pending human confirmations | only when `require_human_confirm` |

Timestamps are real RFC 3339 UTC on write. On read, the legacy `%Y-%m-%dT%H:%M:%SZ`
local-time-with-a-lying-`Z` values parse leniently. All writes go through one helper that sets mode
`0600` at `OpenOptions` time and does tmp → `fsync(file)` → `rename` → `fsync(dir)`.

### 5.4 Migration and compatibility with `~/.vastai-gguf`

**Read, always** (when `[compat] read_legacy_state`):

| Legacy path | Used for |
|---|---|
| `~/.vastai-gguf/config.toml` | `[providers.*]` `base_url` + `api_key`, parsed with a **real TOML parser**; step 3 of the credential chain. **Never rewritten.** The existing `base_url` is used as-is (`api.together.xyz` must not be rewritten to `.ai`) |
| `~/.vastai-gguf/usage.log` | merged into every usage aggregate. `epoch` optional, unknown fields kept via `flatten`, **no row can ever fail a load** |
| `~/.vastai-gguf/local_instances/*.json` | shown as importable endpoints; **paths validated on load** — the saved instance points at a model that no longer exists, and that is normal |
| `~/.vastai-gguf/local_logs/` | offered in the logs view as historical |
| `~/.vastai-gguf/.pinned_provider` | imported once as a recipe (a live file pins `deepseek-ai/DeepSeek-V4-Pro`) |

**Read for migration** (`apexrouter migrate`): `<LocalRouter>/.active_endpoint` (**all four shapes**
via serde aliases for `activated_at`/`switched_at`, with/without `pid`), `.last_instance`,
`.instance_history`, `.hf_pin`, and `recipes.toml`. From `recipes.toml` we import: the `[docker]`
image map, the 7 `llama_cpp_repo`/`llama_cpp_ref` mappings as `known_forks` (genuinely
undiscoverable knowledge), the gpu tiers as `SearchProfile` seeds, the 3 `local` recipes as
`Recipe`s (re-validated against what actually exists on disk), and the 7 `together` recipes as
`Managed` recipes. The 54 `vast_gguf` rows are **not** imported — they are a frozen function
superseded by `fit()`, and `migrate --dry-run` says exactly that, per row.

Type traps handled on import: `max_price` is a quoted **string**; `enforce_eager` is the string
`"true"`/`"false"` (parse `true|1|yes` case-insensitively, everything else false); `provider` is
absent on 54 of 71 rows and defaults to `vast_gguf`; `ctx` is the **total** pool shared across
`parallel` slots; `vram_gb` is **per GPU** and must be multiplied by `num_gpus`.

**Write, optionally** (both default to the safe setting):

- `[compat] active_endpoint_path` — mirror the default alias into `.active_endpoint` in the legacy
  shape on every change, atomically. Off by default.
- `[compat] legacy_proxy_pidfile` — **off by default, and the reason is in the config comment**:
  LocalRouter's `_proxy_down()` reads `/tmp/vastai-gguf-proxy.pid` and SIGTERMs whatever it names.
  Turning it on hands the old TUI's "Proxy → stop" menu item a kill switch for the whole daemon.
  When on, the daemon's SIGTERM handler still performs a graceful drain and local children survive
  via `setsid`, but the routing table, tunnels and watchdogs go with it.
- `[compat] mirror_usage_log` — **off by default** (MK1-CORE ACCEPTANCE, finding B): append every
  usage row to `~/.vastai-gguf/usage.log` in the exact legacy field set, so the old LocalRouter
  TUI's usage view (`cost.py`) keeps working during a transition period. `~/.vastai-gguf` is
  *another tool's state directory*, so merely starting the daemon must never append to it — an
  acceptance run added 15 rows to the real file, which had to be restored. `apexrouter migrate`
  offers to switch it on, which is the case it exists for, so the capability stays discoverable.
  When on, the mirror is still opened only while that directory already exists. ApexRouter's own
  `$STATE/usage.jsonl` is unaffected and is written either way.

Provider id spelling is **one enum with serde aliases** for `vast-gguf` / `vast_gguf` /
`local-gguf` / `local` / `together` / `vllm`. **`vast-gguf` stays on the wire** in `/health`,
`/providers`, `/switch` and `usage.jsonl`.

Ports keep their defaults because they are baked into agent configs: proxy **8888**, local
llama-server **8100+**, Vast tunnel **8800+**.

---

## 6. HTTP API surface

Two listeners. Every route below is the contract both GUIs, the CLI and the MCP server code
against; `openapi/apexrouter-v1.yaml` is generated from these tables and checked in CI.

### 6.1 Proxy listener (`127.0.0.1:8888`) — the drop-in contract

| Method | Path | Behaviour |
|---|---|---|
| `POST` | `/v1/chat/completions` | routed; streaming or buffered |
| `POST` | `/v1/completions` | routed |
| `POST` | `/v1/embeddings` | routed (class `Embedding`; only embedding-capable backends) |
| `POST` | `/v1/rerank` | routed if the target supports it |
| `POST` | `/v1/messages` | **Anthropic ingress** (class `Chat`, `ingress = Anthropic`). Routed by the same `resolve()`. Upstream `Protocol::Anthropic` → relayed verbatim; `Protocol::OpenAi` → **translated** both ways, request and response, streaming and buffered. Requires `anthropic-version: 2023-06-01`; `x-api-key` is accepted for auth and never forwarded. `501` when `[router] anthropic_ingress = false` |
| `POST` | `/v1/messages/count_tokens` | `501` with an **Anthropic-shaped** error body. Not in mk1 (§12) |
| `GET` | `/v1/models` | **aggregated** across aliases + every enabled backend, served from the table. **The OpenAI list shape is the default and stays byte-exact** — ApexOS's LAN sweep depends on it. The Anthropic list shape is emitted **only** when the request carries an `anthropic-version` header; same rows, re-rendered — see "Anthropic ingress" below |
| `GET` | `/v1/models/{id}` | one entry; alias or `backend/model`. Same header rule |
| `GET`/`HEAD` | `/health` | `{"ok":true,"product":"apexrouter","version":"0.1.0","provider":"<active>","uptime":<f64>}` — a superset of the LocalRouter shape and the house shape. Always 200; never probes a backend |
| `GET`/`HEAD` | `/providers` | the **exact** LocalRouter JSON shape, plus additive `endpoints[]` and `routes[]`. Probes run **concurrently** with a 3 s cap (was ~8 s serial) and Together is detected from the **full credential chain**, not just `$TOGETHER_API_KEY` — the documented inconsistency, fixed |
| `POST` | `/switch` | the same request/response shapes, retargeting `default_alias`. Extended with `{"provider":"endpoint","id":…}` and `{"alias":…}`. **Three silent no-ops fixed**: `api_key` in a `together` body is now persisted as a `CredentialRef`; `local` now copies the instance's key; a malformed instance JSON returns a JSON 400, not an HTML 500. Gated by §9.3 |
| `GET` | `/slots` | **403 `redacted_endpoint`** — it echoes prompts and is never proxied outward |
| `*` | anything else | opaque passthrough to the resolved default alias's primary target, after `/v1` normalisation |

The catch-all is registered with `.fallback(any(proxy_handler))`, **not** as a `/{*path}` route, so
no `Router::merge` overlap can exist.

**`/v1` normalisation** — the highest-risk drop-in bug. Every `Backend.base_url` is stored
**without** `/v1`. Inbound paths get a repeated leading `/v1` collapsed to one. Both
`http://127.0.0.1:8888` and `http://127.0.0.1:8888/v1` work as client base URLs — mandatory,
because `smoke.sh` appends `/v1` to whatever you give it and the project's own `SKILL.md` tells
agents to use the form that 404s today. Non-OpenAI paths (`/props`, `/metrics`) forward raw. A
collapse is logged once per (User-Agent, path) at `debug` so a genuinely broken client stays
discoverable, and `diagnose` surfaces a "clients sending a doubled prefix" note.

**Aggregated `/v1/models`** — extras live under a single `apexrouter` key so strict clients ignore
them:

```jsonc
{"object":"list","data":[
  {"id":"auto","object":"model","created":1780000000,"owned_by":"apexrouter",
   "apexrouter":{"kind":"alias","strategy":"first_healthy","healthy":true,
                 "targets":["local-carnice","together:meta-llama/Llama-3.3-70B-Instruct-Turbo"]}},
  {"id":"local-carnice/Carnice-9b-Q6_K","object":"model","owned_by":"local-carnice",
   "apexrouter":{"kind":"backend_model","status":"ready","ctx":32768,"slots":"1/4",
                 "vision":false,"price":null,"tok_per_s_p50":4.1}}
]}
```

**Anthropic ingress — what is translated and what is relayed.** Only three things on this listener
are protocol-aware, and each is stated so nobody has to guess:

- **`POST /v1/messages` against an `OpenAi` backend is fully translated** — request body, response
  body, and the SSE event stream in both directions. This is real work with real edge cases and it
  is the whole of work unit **R-10**; the contract it must satisfy (system hoisting, required
  `max_tokens`, block arrays, tool shapes, `stop_reason`, usage field names, and the named-SSE-event
  state machine) is specified in `BUILD-PLAN.md` §4, Stage 5.
- **`POST /v1/messages` against an `Anthropic` backend is relayed, not translated** — same
  byte-for-byte relay as the OpenAI path, same never-re-framed SSE rule (§4.4). The only change is
  the credential: the client's `x-api-key` is dropped and the backend's own is constructed onto the
  outbound request (§4.5). No translation code is on this path at all.
- **`GET /v1/models` is re-rendered, never translated.** The rows come from the same routing table
  and the same aggregation function, and `GET`/`HEAD` on this path is answered **before**
  `resolve()` runs (§4.2), which is why it carries `X-ApexRouter-Route: -|-`. Without an
  `anthropic-version` header the OpenAI list shape is returned unchanged — that default is
  load-bearing, because ApexOS's compute sweep verifies a node by exactly that shape (§3.4).
  With the header, the same rows are emitted as
  `{"data":[{"type":"model","id":…,"display_name":…,"created_at":…}],"has_more":false,
  "first_id":…,"last_id":…}`; the `apexrouter` extras key is carried through untouched.

Everything else on the proxy listener is protocol-agnostic: `resolve()`, the breaker, the limits,
the retry rules and the telemetry do not know which dialect came in beyond recording it.

### 6.2 Control listener (`127.0.0.1:2739`)

All under `/v1/` (house convention; the proxy's `/v1` lives on a different socket so there is no
collision). Every response body is a protocol type. Every mutation is `Origin`/`Host`-gated.

```
GET    /health                                 {ok, product, version, uptime}  (public)
GET    /v1/snapshot                                                   -> Snapshot
GET    /ws                                     WebSocket, Event stream
GET    /metrics                                Prometheus text     [NOT MOUNTED YET — §4.5]
POST   /v1/reload                              reparse config + routes, keep old table on failure
POST   /v1/shutdown                            graceful (admin scope)

--- rig, discovery, fit -------------------------------------------------------
GET    /v1/rig                                                        -> RigSnapshot
POST   /v1/rig/rescan                          ?builds=&models=       -> RigSnapshot
GET    /v1/models/local                        discovered GGUFs       -> Vec<LocalModel>
GET    /v1/fit?model=&ctx=&parallel=&kv=&devices=&build=&split_mode=&tensor_split=&main_gpu=
                                                                      -> FitPlan
POST   /v1/fit                                 body = FitInput        -> FitPlan
POST   /v1/fit/input                           body = FitQuery        -> FitInput
                                               as built: resolve a model name + flags into the
                                               FitInput (GGUF header + LIVE budget) that a GUI
                                               then edits locally, so a ctx slider re-solves
                                               through POST /v1/fit without re-reading the rig

--- backends and routes -------------------------------------------------------
GET    /v1/backends                                                   -> Vec<Backend>
POST   /v1/backends                            register a URL (NodeSpec)   -> Backend
GET    /v1/backends/{id}                                              -> Backend
PATCH  /v1/backends/{id}                       tags, label, limits, enabled
DELETE /v1/backends/{id}
POST   /v1/backends/{id}/probe|enable|disable|drain
GET    /v1/backends/{id}/logs?tail=200&follow=1                       text | SSE
GET    /v1/routes                                                     -> Vec<ModelRoute>
PUT    /v1/routes                              whole table (atomic)
GET    /v1/routes/{alias}                      PUT /v1/routes/{alias}     DELETE /v1/routes/{alias}
POST   /v1/routes/validate                     body = Vec<ModelRoute>  -> ValidationReport
POST   /v1/routes/{alias}/test                 20-token probe          -> SmokeProbe
POST   /v1/routes/{alias}/swap                 {to: EndpointSpec|BackendId, mode?}  -> SwapReport
POST   /v1/routes/default                      {alias}

--- endpoints (lifecycle) -----------------------------------------------------
GET    /v1/endpoints                                                  -> Vec<EndpointRecord>
POST   /v1/endpoints                           body = EndpointSpec (+ ?no_wait, ?alias, ?force)
GET    /v1/endpoints/{id}                      DELETE /v1/endpoints/{id}   (stop + forget)
POST   /v1/endpoints/{id}/stop|restart|adopt
GET    /v1/endpoints/{id}/argv                                        -> ArgvPreview

--- recipes and search profiles  (== "dynamic recipe building in the GUI") -----
GET    /v1/recipes                             POST /v1/recipes        -> Recipe
GET    /v1/recipes/{id}                        PUT /v1/recipes/{id}    DELETE /v1/recipes/{id}
POST   /v1/recipes/{id}/validate                                      -> ValidationReport
POST   /v1/recipes/{id}/instantiate            ?alias=&no_wait=        -> EndpointRecord | JobRecord
POST   /v1/recipes/from-endpoint/{id}          "save this running thing as a recipe"
GET    /v1/profiles                            POST /v1/profiles       -> SearchProfile
GET    /v1/profiles/{id}                       PUT /v1/profiles/{id}   DELETE /v1/profiles/{id}

--- providers and credentials -------------------------------------------------
GET    /v1/providers          -> [{id, base_url, credential:{source,present}, models_cached, last_error}]
PUT    /v1/providers/{id}     {base_url?, api_key?|api_key_env?|api_key_file?}  (key -> credentials.toml 0600)
POST   /v1/providers/{id}/test                 connection + optional completion  -> Vec<CheckResult>
GET    /v1/providers/{id}/models               live catalogue, grouped by org    -> Vec<UpstreamModel>

--- vast ----------------------------------------------------------------------
GET    /v1/vast/account                        credit, balance, can_pay (never api_key)
GET    /v1/vast/gpu-names                      LIVE vocabulary for the dropdown
POST   /v1/vast/offers/search                  body = OfferQuery | {profile}  -> OfferSearchResult
GET    /v1/vast/instances                      DELETE /v1/vast/instances/{id}?confirm=true
POST   /v1/vast/instances                      {profile|offer_id, launch, confirm, max_usd_per_hour}
                                               -> 409 without approval; else JobRecord (?no_wait)
GET    /v1/vast/instances/{id}/log?follow=1    SSE
POST   /v1/vast/instances/{id}/restart-download    the stall recovery
POST   /v1/vast/instances/{id}/tunnel          DELETE …/tunnel
GET    /v1/vast/instances/{id}/diagnose        the four SSH probes + RX sample -> Vec<CheckResult>
GET    /v1/tunnels                                                    -> Vec<TunnelStatus>
GET    /v1/approvals    POST /v1/approvals/{id}/grant|deny

--- huggingface ---------------------------------------------------------------
GET    /v1/hf/search?q=&limit=                                        -> Vec<HfModel>
GET    /v1/hf/models/{*repo}/files                                    -> Vec<HfFile> (paths-info)
POST   /v1/hf/downloads     {repo, files[]|quant, dest?}  ?no_wait    -> JobRecord
GET    /v1/hf/downloads     DELETE /v1/hf/downloads/{job}

--- observability -------------------------------------------------------------
GET    /v1/requests?limit=&alias=&backend=&since=                     -> Vec<RequestRecord>
GET    /v1/requests/{id}    POST /v1/requests/{id}/cancel
GET    /v1/usage?since=24h&by=provider|model|backend|alias|day        -> UsageSummary
POST   /v1/compare          {aliases[], prompt, max_tokens}  ?no_wait -> JobRecord  (batch compare)
POST   /v1/smoke            {alias|base_url}                          SSE, one event per probe
GET    /v1/diagnose?only=                                             SSE, one event per check
GET    /v1/checks                                                     -> the registry
GET    /v1/jobs   GET /v1/jobs/{id}   POST /v1/jobs/{id}/cancel
POST   /v1/migrate          {dry_run, skip[]}  -> MigrationPlan | MigrationReport
                                               [skip strikes plan rows first, with
                                                core::migrate::strike's matching; a pattern
                                                that matches no row is a 400]
```

**As built, one path above is not registered** — `/metrics`. It is on the `PENDING` list in
`crates/apexrouter-server/tests/openapi_routes.rs`, which fails the build if a documented path is
neither registered nor listed there **and** fails if a listed one has since been wired. That test, not this table, is the authority on what the daemon serves today.
Everything else here is registered and is proved reachable in the composed application by
`crates/apexrouter-server/tests/mounted_routes.rs` — the guard that exists because three separate
API modules once shipped implemented, tested and unreachable.

**`?no_wait=true`** is the house pattern: return a `JobRecord` immediately and have the spawned task
flip the row to `Failed` on **every** error path including a `JoinError` from a panic, so nothing
sits `Pending` forever.

**`GET /ws`**: subscribe to the broadcast **before** sending the snapshot; re-send a full snapshot on
`RecvError::Lagged`; `tokio::select!` also drains `socket.recv()` to notice a close.
`RequestStarted`/`RequestFinished` are only serialised when at least one subscriber exists, and
`UsageTick` is coalesced to 1 Hz — a router at 50 rps must not drown its own dashboard.

---

## 7. CLI surface

`clap` derive, noun-grouped, house verb vocabulary. `--json` is **per subcommand, never global**, and
prints `serde_json::to_string_pretty` of the protocol type and **nothing else** on stdout. No colour
crate, no emoji. `fn main() -> anyhow::Result<()>`; failures via `?`/`bail!` → anyhow prints
`Error: …` to stderr and exits 1. **Tracing always to stderr**, because `mcp` shares the binary and
owns stdout.

**Daemon resolution** (the answer to "daemon-first is annoying"), declared per command:

```rust
enum Need { Pure, ReadState, Mutate }
```

| Class | Commands | Daemon down → |
|---|---|---|
| `Pure` | `version`, `config path/show`, `fit`, `completions` | runs; no daemon involved |
| `ReadState` | `status`, `rig`, `models ls`, `endpoint ls`, `route ls`, `recipe ls`, `profile ls`, `usage`, `doctor`, `vast ls` (cached) | serves from `$STATE` under `LOCK_SH`; output tagged `served_by: "offline"`, `stale: true` for poller-derived fields |
| `Mutate` | everything else | **autostart** the daemon (default), poll `/health` for 5 s, proceed; `--no-autostart` turns this into a plain error |

`served_by`, `as_of_unix` and `stale` are on **every** `--json` envelope so a script can tell where
its answer came from without parsing prose. Human output prints one dim line
`(offline — apexrouterd is not running)` before the table. `--json` failures print
`{"error":{"kind":"…","message":"…"}}` on stdout and exit 1 — `kind` is the machine-readable
discriminator, satisfying `07` §2.2 without inventing exit codes.

```
apexrouter                                   # bare = status
apexrouter status [--json] [--watch] [--interval SECS]
apexrouter serve [--proxy-bind A] [--control-bind A] [--foreground] [--detach] [--stop] [--no-ui]
                 [--allow-remote --token-env VAR]
apexrouter open                              # ensure daemon, xdg-open the web UI
apexrouter url [--json]                      # prints http://127.0.0.1:8888/v1 and nothing else
apexrouter env                               # OPENAI_BASE_URL, OPENAI_API_KEY, ANTHROPIC_BASE_URL
apexrouter version
apexrouter completions <bash|zsh|fish|elvish|powershell>

apexrouter rig [--json] [--rescan]           # GPUs (free/total, who holds them), builds, RAM, swap
apexrouter models ls [--json] | show <id> [--json]
apexrouter fit <model> [--devices Vulkan0,Vulkan1] [--ctx N] [--parallel N] [--kv q8_0]
                       [--split-mode layer] [--tensor-split 3,1] [--main-gpu 0] [--batch N] [--json]

apexrouter up <model|recipe> [--alias A] [--yes] …            # the one-command happy path
apexrouter endpoint ls [--json] | show <id> | logs <id> [-f] [-n 200] | argv <id>
apexrouter endpoint start <model|recipe> [--alias A] [--build B] [--devices D,D] [--port N]
      [--ctx N] [--parallel N] [--kv q8_0] [--ngl auto|all|N] [--split-mode m] [--tensor-split r]
      [--main-gpu N] [--mode thinking|coding|nonthinking] [--mmproj PATH] [--no-wait] [--force]
apexrouter endpoint stop <id> [--all] | restart <id> [--ctx N] [--parallel N] … | adopt <id> | rm <id>
apexrouter endpoint vllm start --model-id M [--tp N] [--ctx N] …          # local vLLM
apexrouter swap <alias> --to <model|recipe|backend-id> [--mode hot|sequential]

apexrouter route ls [--json] | show <alias> | rm <alias> | default <alias> | test <alias>
apexrouter route set <alias> --target <backend[:model]|tag:T[:model]|glob:P[:model]>...
      [--strategy first-healthy|round-robin|least-busy|cheapest]   # kebab at the CLI; the wire
      [--failover | --no-failover] [--retries N]                   # enum stays snake_case
      [--require-tag T] [--max-cost F] [--min-ctx N] [--json]
apexrouter switch <together|local <name>|vast-gguf|endpoint <id>|alias <a>>   # muscle memory
apexrouter backend ls | show <id> | add <url> [--label L] [--tag T] [--key-env VAR]
                    | enable|disable|drain|probe|rm <id>

apexrouter recipe ls [--json] | show <id> | new --from-endpoint <id> [--edit] | edit <id>
                   | rm <id> | validate <id> | run <id> [--alias A]
      # --from-endpoint is REQUIRED: a recipe is a snapshot of something that ran, never invented
apexrouter profile ls [--json] | show <id> | new | edit <id> | rm <id>

apexrouter provider ls [--json] | show <id> | set <id> [--base-url U] [--key-env VAR]
                     [--key-file P] [--key-stdin] | test <id> | models <id> [--org O] [--json]

apexrouter vast account [--json]
apexrouter vast offers [--profile P] [--gpu "RTX 3090"] [--num-gpus 2] [--geo EU] [--max-price F]
                       [--json]
apexrouter vast gpu-names [--json]
apexrouter vast rent <offer-id|--auto> --profile P (--model-repo R --quant Q | --model-id M)
                     --max-hourly F --yes [--no-wait] [--dry-run]
apexrouter vast ls [--orphans] [--json] | watch <id> | log <id> [-f] | diagnose <id>
                   | restart-download <id> | destroy <id> [--all] --yes
apexrouter tunnel up <instance-id> | down [<id>] | status [--json]
apexrouter approvals ls | grant <id> | deny <id>

apexrouter hf search <query> [--json] | files <repo> [--json]
                   | get <repo> [--quant Q] [--file F] [--no-wait]

apexrouter usage [--since 24h|7d|all] [--by provider|model|backend|alias|day] [--json]
apexrouter compare --alias A --alias B --prompt P [--max-tokens N] [--json]
apexrouter smoke [--alias A | --base-url URL] [--model M] [--json]
apexrouter doctor [--only <check>] [--json]
apexrouter migrate [--dry-run] [--apply] [--skip PATTERN]… [--from ~/.vastai-gguf]
                   [--localrouter PATH] [--json]
      # --dry-run is the DEFAULT. Without --apply, `migrate` only ever prints.
      # --skip strikes rows out of the plan (a category name, or a `from` substring);
      # a pattern that matches nothing is an error, and the strike shows on the dry run.
apexrouter config init [--force] | show [--json] | path | edit | validate [--json]
apexrouter update [--no-pull]
      # git pull --ff-only on the checkout $STATE/install.conf records, then hand over to
      # its install.sh --yes, which rebuilds, reinstalls and re-verifies with the recorded
      # choices. Installs not made by install.sh: `git pull && cargo build --release`.
apexrouter token create [--scope read|write|admin] | ls | revoke <id>
apexrouter mcp [--proxy URL]
      # NOT a clap subcommand: `mcp` is intercepted in main() before Cli::parse(), so it does
      # not appear in `apexrouter --help` and its own --help prints to STDERR. Deliberate —
      # one clap diagnostic on stdout is a protocol violation an MCP client reports as a crash.
```

Blocking semantics are stated per verb in `docs/API.md`: `endpoint start` and `swap` return when the
endpoint is `Ready` (or the deadline expires) unless `--no-wait`; `vast rent` returns when the
instance is `Confirmed` and streams `BootPhase` unless `--no-wait`. `route set` prints the
before → after target diff. `apexrouter up` resolves its positional in a **documented** order:
exact recipe id → exact model id → unique case-insensitive prefix of a model id → path on disk; an
ambiguous prefix errors with the candidates listed.

**There is no TUI.** The brief asks for two GUIs; a third interactive front-end is a third place to
get the state model wrong. `status --watch` covers terminal-staring over the same API.

---

## 8. MCP tool surface

Hand-rolled newline-delimited JSON-RPC 2.0 over stdio in `apexrouter-cli/src/mcp/`, copying
`Prefrontal-RS/prefrontal-cli/src/mcp.rs` in shape. Hard rules: **compact one-line JSON**
(`to_string`, never `to_string_pretty`), **all logging to stderr**, exit promptly on stdin EOF,
never write a non-MCP byte to stdout. `initialize` **echoes the client's requested
`protocolVersion`** back (falling back to `"2024-11-05"`), which is instant compatibility with every
legacy revision. Tool failures are results with `isError: true` and helpful text; JSON-RPC error
codes (`-32601`, `-32700`) are reserved for protocol breakage.

Dual-era hedge for the 2026-07-28 revision (~30 lines, per `09`): also answer `server/discover`
advertising `supportedVersions`, accept-and-ignore per-request `_meta`, and emit
`resultType: "complete"`. Streamable-HTTP is **not** implemented, but dispatch is
transport-agnostic — `fn dispatch(method: &str, params: Value) -> Result<Value, RpcError>` — so an
axum route is a day's work when ApexOS-RV nodes need it over the network.

`#[async_trait] trait McpBackend`: `LocalBackend` answers `Pure`/`ReadState` tools directly from
`apexrouter-core` even when the daemon is down and returns a helpful `isError` result for mutations
("run `apexrouter serve`"); `ProxyBackend` forwards to `$APEXROUTER_URL` with `$APEXROUTER_TOKEN`.
Selected by `--proxy URL` / `-p` / env, parsed by hand so `clap` stays out of the MCP module.

All names are prefixed `apexrouter_` (three MCP servers share `~/Projects/.mcp.json`). Descriptions
are long and operational: an agent should get from `apexrouter_status` to a working
`OPENAI_BASE_URL` without reading a doc.

| Tool | Input schema (summary) | Purpose |
|---|---|---|
| `apexrouter_status` | `{}` | Router health, proxy URL, every alias and where it points, backend health, rig summary, in-flight, 24 h spend, Vast credit. The "what is my inference situation" call. |
| `apexrouter_models` | `{}` | The aggregated model list with alias, backend, ctx, vision, price, live tok/s. **The call an agent makes before choosing a `model` string.** |
| `apexrouter_rig` | `{}` | GPUs with free/total VRAM and who holds them, llama.cpp builds and their backends, RAM/swap. |
| `apexrouter_fit` | `{model, ctx?, parallel?, kv?, devices?[]}` | Will this fit, at what ctx/parallel, with the arithmetic in `why[]`. Pure, instant, no side effects. |
| `apexrouter_up` | `{model, alias?, ctx?, parallel?, devices?[], wait?}` | **The one-call happy path**: pick a build, fit, spawn, health-gate, bind an alias, return the base URL and model string to use. |
| `apexrouter_endpoint_start` | `{spec: EndpointSpec, alias?, no_wait?, force?}` | Full control when `up` is too opinionated. |
| `apexrouter_endpoint_stop` | `{id\|alias, mode?: drain\|now}` | |
| `apexrouter_swap` | `{alias, to, mode?}` | Atomic model swap behind a stable alias. One call, not four. |
| `apexrouter_logs` | `{id, tail?}` | Endpoint or instance log tail. **The call an agent makes when a start failed.** |
| `apexrouter_backend_set` | `{id, enabled?, drain?, tags?[]}` | Quarantine a degraded backend without flipping the whole route. |
| `apexrouter_route_set` | `{alias, targets[], strategy?, failover?, default?}` | Point an alias; effective on the next request, no restart. |
| `apexrouter_recipe_list` / `_save` / `_run` | `{}` / `{recipe}` / `{id, alias?}` | Author and launch saved plans — the agent half of "build recipes from the GUI". |
| `apexrouter_usage` | `{since?, by?}` | Tokens, cost and tok/s by window and grouping, metered-vs-estimated marked. |
| `apexrouter_smoke` | `{alias?, base_url?}` | Four named probes with pass/fail, TTFT and tok/s — "is the endpoint I'm about to use actually working". |
| `apexrouter_diagnose` | `{only?}` | The check registry, optionally one check. |
| `apexrouter_hf_search` / `_hf_files` / `_hf_get` | `{q}` / `{repo}` / `{repo, quant?, no_wait?}` | Find, size and **download** weights. Sizes come from `paths-info`, authoritative. |
| `apexrouter_vast_offers` | `{profile?, gpu?, num_gpus?, geo?, max_price?}` | Read-only live market search. Free and safe. |
| `apexrouter_vast_rent` | `{profile\|offer_id, launch, confirm: true, max_usd_per_hour}` | **Spends money.** Without `confirm` and `max_usd_per_hour` it returns `isError: true` **carrying the full cost preview and current credit** — a refusal that doubles as a dry run showing the bill. Subject to the daemon-side ceiling and, if configured, a human approval. |
| `apexrouter_vast_destroy` | `{id, confirm: true}` | Tear down; verifies before forgetting; returns accrued cost. |
| `apexrouter_compare` | `{aliases[], prompt, max_tokens?}` | Run one prompt across N aliases in parallel; latency, tok/s, cost, first 200 chars each. |

**As built:** 24 tools, exactly this list, **sorted by name** in `tools/list` (the 2026-07-28
revision asks for it so a client can cache the list and an LLM can hit its prompt cache), each
carrying a `title` alongside `name`/`description`/`inputSchema`. A test asserts the inventory
against this table, so a tool cannot be added here in prose and missed in code. Optional
arguments the table above compresses: `_up` takes `wait`; `_hf_search` and `_vast_offers` take
`limit`; `_vast_rent` **requires** `launch` and also accepts `auto_tunnel` and `bind_alias`;
`_endpoint_stop` accepts `id` **or** `alias`. `initialize` also returns an `instructions` string
that already names the base URL and the `model` string, so a harness that surfaces instructions
gets the operational fact without a tool call. Registration snippets per harness live in
`docs/AGENTS.md`; the agent-facing operating manual is `skills/apexrouter/SKILL.md`.

---

## 9. Security posture

### 9.1 Bind addresses

Both listeners default to loopback. A **non-loopback bind refuses to start** unless auth is
configured (`bail!` with the fix in the message). Serving uses
`app.into_make_service_with_connect_info::<SocketAddr>()`, and the loopback bypass requires **both**
an explicit opt-in flag and a genuinely loopback peer IP read from `ConnectInfo` — absent
connect-info **fails closed**, never open.

### 9.2 Secrets

`Secret<String>` prints `***` in `Debug`/`Display`; the sole accessor is `expose()`. The resolution
chain is one function with one order: **explicit config value → ApexRouter config/`credentials.toml`
→ conventional third-party path (`~/.config/vastai/vast_api_key`, `~/.cache/huggingface/token`,
`~/.vastai-gguf/config.toml`) → the env var named by `api_key_env`.** It returns
`(Secret<String>, CredentialSource)`.

Enforced structurally, not by discipline:

- A **borrowed** credential is never copied into our config. `ConfigFile` has no field for it. Only
  a key the user *typed into a GUI or `--key-stdin`* is written, and it goes to `credentials.toml`
  at `0600`, never `config.toml`.
- No credential reaches an argv. `llama-server` gets `--api-key-file` (a `0600` file in `$STATE`) or
  `LLAMA_ARG_API_KEY`, never `--api-key <secret>` — that would put it in `/proc/*/cmdline`.
- `HF_TOKEN` goes in the Vast `env` **map**, never in the `--onstart-cmd` string, which Vast
  persists and echoes back in `show instance`.
- `GET /v1/providers` returns `{source: "env:TOGETHER_API_KEY", present: true}` — the source, never
  the value. `VastAccount` has no `api_key` field even though the API echoes one.
- `TraceLayer`'s span records **method and path only**, never the query string, because `?token=` is
  an accepted presentation.
- Four live credentials exist on this box and none may ever be logged:
  `~/.config/vastai/vast_api_key`, `~/.cache/huggingface/token`, `$TOGETHER_API_KEY` (also in
  `~/.vastai-gguf/config.toml`), and any per-instance llama-server key we mint.

### 9.3 The mutation gate — CSRF and DNS rebinding

A loopback control plane is **not** a trust boundary. A cross-origin `fetch` to
`POST http://127.0.0.1:8888/switch` with `Content-Type: text/plain` is a CORS *simple request*: no
preflight, the request is delivered, and the attacker never needs to read the response. So every
mutating request on **either** listener passes `require_mutation_origin()`:

1. `Host` must be in the bind allowlist (`127.0.0.1:PORT`, `localhost:PORT`, or a configured name).
   This closes DNS rebinding, which otherwise makes an attacker page same-origin.
2. If `Origin` is present it must be same-origin with the listener; if `Sec-Fetch-Site` is present
   it must be `same-origin` or `none`.
3. Otherwise a bearer token with `write` scope is required.

Non-browser clients (CLI, Slint, `curl`) send no `Origin` and no `Sec-Fetch-Site`, so they pass
rule 2 unchanged. There is still **no `CorsLayer`** on the authenticated API; the embedded UI is
same-origin, and a cross-origin deploy adds an explicit allowlist, never `allow_origin(Any)`.

`POST /switch` additionally validates any supplied `base_url` against
`[compat] allow_switch_hosts`. Unauthenticated `/switch` with an arbitrary URL plus an injected
Together key is a **credential-exfiltration primitive**, not merely SSRF.

### 9.4 Tokens and scopes

Bearer accepted three ways: `Authorization: Bearer <t>`, `X-ApexRouter-Token: <t>`, `?token=<t>`.
Scopes `read|write|admin` derived from (path, method); `/v1/tokens*` and `/v1/shutdown` are always
`admin`, and — deliberately — the **data plane is `Read` even though it is `POST`**
(`/v1/chat/completions`, `/v1/completions`, `/v1/embeddings`, `/v1/rerank`, `/v1/messages`):
inference mutates nothing this server owns, and a read-scoped agent that could not run a
completion would be a scope system nobody uses. A doubled `/v1` is collapsed before
classification, so `/v1/v1/chat/completions` classifies identically.

**As built, there is one bearer, not a token store.** `required_scope()` exists and is tested, but
the *subject* side of it does not: the daemon accepts exactly one token — the value of the
environment variable named by `[server] token_env`, defaulting to `APEXROUTER_TOKEN` — and grants
it every scope. `apexrouter token create` mints 256 bits from `/dev/urandom`, hex-encodes them,
prints the token **once** and prints the two lines that put it to work; it stores nothing, so
`token ls` reports *where the daemon looks* and whether it finds a value, and `token revoke`
explains how to take one out of service (rotate the variable, restart) rather than deleting a row.
The command says so in its own output rather than implying a store that is not there. A per-token
hashed store, and therefore genuine per-token scopes, is `/v1/tokens*` — reserved in the scope
table, not implemented in mk1. **The security posture that is real today is loopback + the
mutation gate (§9.3); the bearer exists so a non-loopback bind is possible at all** (§9.1 refuses
one without it), not as a multi-tenant authorisation system.

### 9.5 Rented-endpoint exposure

Default posture is **tunnel-only**: `HOST=127.0.0.1` is forced at create time *and* on every
stall-restart (`launch_vllm.sh`'s own default is `0.0.0.0`). `expose_public = true` is an explicit
opt-in and **requires** a freshly minted per-instance `llama-server` API key delivered via
`--api-key-file`, because a Vast direct port is plaintext HTTP on a shared public IP.

---

## 10. Web UI

`ui-web/{index.html, app.js, style.css}` — three files, no npm, no CDN, no framework, no build step,
embedded with `rust-embed` pointed straight at `../../ui-web` (no `dist/`), with `[server] ui_dir`
as a live-reload escape hatch. `"use strict"`, module-level state, `el(tag,cls,text)` + `$(id)`
helpers, `container.replaceChildren()` re-render, `textContent` everywhere and `innerHTML` never.
Inline SVG emoji favicon as a `data:` URI. `:root` token block with `color-scheme: dark` and a
`@media (prefers-color-scheme: light)` override; **status colours reserved for health, never
identity**; badges pair icon + label, never colour alone; system sans for body text with monospace
strictly for code and logs. Every element that toggles `hidden` and declares its own `display` gets
a `[hidden]{display:none}` guard.

WebSocket first with a REST first-paint fallback; `connectWS()` sets a connection dot, dispatches on
the Rust `Event` tags, and on close schedules a reconnect with exponential backoff 1 s → ×2 → cap
15 s. `setInterval(render, 60_000)` keeps relative timestamps honest. Search inputs are debounced
250 ms with a monotonic `seq` guard dropping stale responses. A `503` latches a
feature-unavailable flag. Every `fetch` is wrapped in try/catch with the connection dot as the
single failure reporter.

| Panel | Contents | Interactions |
|---|---|---|
| **Router bar** (always visible) | `http://127.0.0.1:8888/v1` with a **copy button** and `OPENAI_API_KEY=not-needed` beside it; connection dot; in-flight; req/min; aggregate tok/s; 24 h spend; **an alias dropdown that re-points `default` in one click** | copy; rebind default |
| **Rig strip** (always visible) | one bar per device: name, backend, free/total VRAM, who holds it; RAM + swap | click a device to filter Backends |
| **Routes** | one row per alias: alias · target chips (reorder with ↑↓ and drag) · strategy · health roll-up · p50 TTFT · p50 tok/s · $/Mtok. A red banner when the on-disk table failed to compile, naming the parse error and stating the previous table is still serving | open the editor drawer; **Test** (20-token probe with inline TTFT/tok/s); Save is a `PUT`, hot, no restart |
| **Backends** | uniform card grid: label, kind badge, model(s), health dot, `slots 1/4`, queue depth, p50/p95, `$/hr` or `$/Mtok`, device(s), last error, uptime. Replaces LocalRouter's Local-status + Instances + Providers menus with one card | probe · drain · disable · **Bind to alias…** · logs (follow mode, filter box) · stop · destroy |
| **Launch** (non-modal drawer, 3 tabs, one `EndpointSpec`, summary visible **while** you edit) | *Local*: discovered models (shards grouped, real sizes, vision badge) × builds × device checkboxes; split-mode/main-gpu/tensor-split; ctx/parallel/kv sliders re-running `fit()` live with a stacked weights/KV/compute/headroom bar and the `why[]` strings as tooltips; mode preset; alias. *vLLM*: model id, TP, ctx, quantization, kv dtype. *Rent*: profile → live sortable/filterable/re-queryable offer table → HF search with authoritative sizes → fit against pooled VRAM → **cost panel: $/hr, estimated total, current credit, burn-down hours** → confirm. The drawer then becomes the live `BootPhase` view with the log stream, elapsed timer and a Destroy button — there is no separate "Watch boot" | **Start & bind** · **Save as recipe** · confirm |
| **Fleet & cost** | rented instances with uptime, accrued cost, total hourly burn, credit remaining, burn-down; orphan-suspects flagged loudly with one-click reconcile; download-stall banner with **Restart download**; tunnel toggle; per-instance diagnose | destroy · tunnel · restart download · reconcile |
| **Catalog** | Recipes (with staleness flags: model file gone, build removed, offer unrentable) and Search profiles, both fully editable — this is "dynamic recipe building in the GUI". Local models; HF search with per-file sizes and a **Download** button with progress | new/edit/duplicate/delete/validate/run a recipe; new/edit a profile; download a quant |
| **Providers** | configured providers with credential **source** (never the value), a masked key field, base URL, live model catalogue grouped by org with per-row **Activate** and **Save as recipe** | set key · Test (connection + completion) · activate a model |
| **Live requests** | streaming table off the WS: time, alias → backend, model, status, TTFT, tok/s, tokens, cost, attempts, route reason. Prompts are **not** captured unless `capture_bodies` is on, and the toggle says so | click for detail; cancel |
| **Usage** | tokens/day and $/day stacked by provider (hand-rolled inline SVG, no CDN), tok/s by backend, metered-vs-approximate badges | change the window and grouping |
| **Doctor** | the check registry as rows with pass/fail badges, timings and a fix line each, individually runnable; the four smoke probes | run all · run one · smoke |

---

## 11. Slint app

`apexrouter-slint`, `[[bin]] apexrouter-ui`, GPL-3.0-only, `publish = false`, out of
`default-members` and out of the CI `-p` list, so `cargo build`/`clippy`/`test` never link the
slint ecosystem or need `libfontconfig1-dev`. `build.rs` is one
`slint_build::compile("src/ui/appwindow.slint")` line.

**Thread model.** Never `#[tokio::main]`. `fn main() -> anyhow::Result<()>` builds
`tokio::runtime::Builder::new_multi_thread().enable_all().build()?`, keeps it alive for the app
lifetime, and ends with `ui.run()?`. Slint owns the main thread. Each callback is wired in a braced
block capturing `ui.as_weak()` + `rt.handle().clone()`; properties are read on the UI thread, work
is `handle.spawn()`ed, and results come back via `weak.upgrade_in_event_loop(move |ui| …)`. All
fallible async work goes in one inner `async { … anyhow::Ok(v) }.await` so a single `match` handles
every failure. A single background task holds the `NodeClient` WS subscription and pushes `Event`s
into the event loop, so the app renders the same `Snapshot` as the web UI with zero polling.

**It is an edge client of the same HTTP API. There is no second business-logic path.** It links
`apexrouter-protocol` and `apexrouter-client` only.

**Layout, as built.** `src/main.rs` (one `wire_*` fn per screen) + `src/api.rs` (`Bridge`, `Store`,
and the pure `protocol → *Row` mappers) + `src/ui/appwindow.slint` root + `state.slint`
(`export global State` — the whole Rust↔UI contract in one global, because threading properties
through eight write-capable screens would be most of the file and all of the bugs) +
`types.slint` (20 row structs) + `palette.slint` +
`components/{card,badge,meter,table,logview,drawer,widgets}.slint` +
`screens/{dashboard,routes,backends,launch,fleet,catalog,providers,doctor}.slint`.
`export global Palette` matches the web tokens exactly (`#0d0d0d` page, `#1a1a19` surface,
`#2c2c2a` hairline, `#ffffff` ink, `#c3c2b7` ink-2, `#898781` muted, `#3987e5` accent, `#0ca30c`
good, `#fab219` warn, `#ec835a` serious, `#d03b3b` critical). **Every component reads from
`Palette`; no colour literal exists outside it** (a few `border-radius` px literals do — radius is
shape, not identity). Models are `ModelRc::new(VecModel::from(rows))`, except the route editor's
target list which is mutated in place because reordering is the operation; kebab-case Slint names
map to snake_case Rust (`base-url` → `get_base_url`/`set_base_url`). Crate features:
`default = ["winit"]`, with a commented `linuxkms` line for a compositor-less ApexOS node.

**mk1 screens — full parity with the web UI on everything that matters, including money.** The brief
says "a nice GUI, in TWO forms", not one GUI and one dashboard.

| Screen | mk1 contents |
|---|---|
| **Dashboard** | Router bar with the copyable base URL and an alias dropdown; rig strip with per-device VRAM meters; aggregate stats; backend cards with health dots; live request ticker |
| **Routes** | Alias list + editor pane: target list with ↑/↓ reorder, strategy combo, failover toggle, filter fields, **Test**. Write access — this is the product |
| **Backends** | List + per-card actions (probe/drain/disable/**bind to alias**/stop/destroy) + a `Flickable` log pane with follow mode and a filter field |
| **Launch** | All three tabs. Local: model list, build combo, device checkboxes, split-mode/main-gpu/tensor-split, ctx/parallel/kv steppers with the live fit readout and headroom bar. vLLM: the same shape. **Rent: profile combo, live offer table, HF repo/quant fields, the cost panel with credit and burn-down, and the confirm** |
| **Fleet** | Rented instances with uptime, accrued cost, burn rate, credit, stall banner with Restart download, tunnel toggle, and a **Destroy** button that is always visible |
| **Catalog** | Recipes and profiles: list, create, edit, duplicate, delete, validate, run. "Save this launch as a recipe" |
| **Providers** | Masked key entry, source display, Test, live model catalogue with Activate |
| **Usage / Doctor** | Totals with metered/approximate badges and a simple per-day bar row; the check registry with badges and per-check run; the four smoke probes |

Honest deferrals, stated in `docs/SLINT.md` as a table: drag-to-reorder (Slint uses ↑/↓ buttons),
the stacked SVG charts (Slint uses a bar row), and the HF *search* browser (Slint takes repo + quant
as fields and offers "open the web UI" for browsing). Nothing that spends, destroys, launches,
routes or authors is missing.

**As built, the eight screens are not one-for-one with the web UI's eight tabs**, and
`docs/SLINT.md` carries the full port map. The differences worth knowing here: the app gains a
**Dashboard** (the web keeps those stats in its always-visible router bar) which also hosts the
**live-request ticker** the web gives a tab of its own; **Launch is a screen rather than a
drawer** (a drawer over a dense desktop window buys nothing — the *boot* view is still a drawer);
and **Usage and Doctor share one screen**. `docs/SLINT.md` also records the headless invocation —
`env -u WAYLAND_DISPLAY WINIT_UNIX_BACKEND=x11` under `Xvfb`, because winit otherwise prefers
Wayland and silently opens on the real desktop where an X11 capture sees nothing.

---

## 12. Explicitly NOT in mk1

Each with the reason, so a future reader knows it was a decision and not an oversight.

> **Amended.** Anthropic `/v1/messages` translation *was* on this list, on the reasoning that
> "Claude Code reaches ApexRouter through MCP for control, not for inference". That reasoning was
> wrong: inference is exactly the point — pointing `ANTHROPIC_BASE_URL` at ApexRouter is what lets
> the Claude Code harness drive a local or rented model. Anthropic ingress is **in** mk1 as a bonus
> last feature (§3.4, §6.1, work unit R-10). What genuinely stays out is narrower, and is the first
> three bullets below.

- **OpenAI → Anthropic translation. Permanently out, not deferred.** The matrix cell returns `501`
  with an OpenAI-shaped body. ApexOS-RS already speaks Anthropic natively
  (`agentd/crates/agent/src/anthropic.rs`) and calls `api.anthropic.com` directly with a real key;
  nothing in the ecosystem wants a proxy that fakes an Anthropic upstream out of an OpenAI one.
  Building it would mean maintaining a second, unexercised translator.
- **Perfect Anthropic tool-use translation.** `[router] anthropic_tools` is **on by default** (since
  2026-07-31 — see the CHARTER amendments log) and is *allowed to be imperfect*, which is the honest
  statement rather than a promise we would quietly break. `input_schema`/`parameters` and
  `tool_use`/`tool_calls` map cleanly; parallel tool calls, `tool_choice` variants, and a
  `tool_result` whose content is a block array rather than a string do not map cleanly in every
  case. It is on because the alternative to imperfect tool translation is not "no tools" but "the
  feature does not work at all": real Claude Code sends **92 tool definitions on every request**.
  Set **explicitly to `false`** and a `/v1/messages` body carrying `tools` is still **refused with a
  clear error naming the key** — never silently stripped and answered wrongly, which is the failure
  mode that actually costs an agent an hour.
- **`thinking` blocks, and `/v1/messages/count_tokens`.** There is no OpenAI-side equivalent of an
  Anthropic `thinking` content block, so mk1 neither synthesises one on the way out nor accepts one
  on the way in. The closest thing that exists is llama.cpp b9199's `--reasoning-format`, which can
  emit `reasoning_content` on a chat completion; mk1 records that it exists and does **not** map it
  onto `thinking` — inventing a signature or a token count for a block the upstream never produced
  would violate the honesty rule that `CostEstimate` and `TokenCount` exist to enforce.
  `count_tokens` is `501` for the same reason: the only honest answer needs a tokenizer we do not
  have, and a fabricated count is worse than an error the client can fall back from.
- **`Strategy::Mirror`, `Fastest` and sticky sessions.** They are not in the mk1 enum at all, so no
  config value can reach an unimplemented arm. Batch comparison ships instead as
  `POST /v1/compare` / `apexrouter compare`, which is what the feature was actually for.
- **llama.cpp router mode** (`--models-dir`, `POST /models/load`, `--sleep-idle-seconds`). b9199
  already has it and it overlaps our supervision job. mk1 keeps direct single-model supervision
  because it matches the state model and the failure modes we understand. Filed in
  `docs/CHARTER.md` as the mk2 simplification, along with idle-unload.
- **MCP streamable-HTTP transport.** The 2026-07-28 revision's header/body mirroring with base64
  sentinel encoding is real work with no user today, and the deprecated HTTP+SSE transport will
  never be implemented. Dispatch stays transport-agnostic so an axum route is a day's work.
- **sqlite.** Everything a human might `cat` or a script might `tail` stays a file. If usage
  aggregation ever needs SQL, copy Imaginarium's `Mutex<Connection>` + `migrate()` + terminal-guard
  pattern verbatim.
- **A TUI.** Declined outright, not deferred (§7).
- **CORS.** No browser client is cross-origin; the embedded UI is same-origin. The mutation gate
  (§9.3) is the defence, and it is stronger than a CORS policy.
- **Vast *bidding* (interruptible instances), volumes, and multi-region orchestration.** On-demand
  `type: "ask"` only.
- **GPU-mesh scheduling across LAN nodes.** A LAN node is a `Node` backend today, which is the
  cheap 90%; automatic discovery (mDNS) and capacity-aware placement are mk2. Nothing here
  architects it out.
- **Automatic model *conversion*/quantisation.** We find, size, download and serve GGUFs; we do not
  run `llama-quantize`.
- **Windows and macOS.** Linux only in mk1: the process model uses `/proc`, `flock`, `setsid` and
  `boot_id`. `Backend::Metal` exists in the enum so the data model does not have to change later.

---

## 13. Rationale, in one page

LocalRouter's proxy is 417 lines that read a JSON file per request and forward bytes to whatever it
says; `05` §14 lists sixteen things a serious router needs and marks all sixteen absent. Fourteen
TUI menus, 71 recipes and 19 GPU tiers exist to set one string in that one file. Inverting it — the
routing table is the primary data structure, and everything else is a way to add a row to it — makes
most of LocalRouter's structure evaporate rather than get ported.

**Model aliasing is the feature that makes the whole thing worth running.** Today, switching
backends silently breaks every client because the `model` string goes upstream verbatim. With
aliases, an agent hardcodes `OPENAI_BASE_URL=http://127.0.0.1:8888/v1` and `model: "auto"` once and
never touches either again while the thing behind it changes from an iGPU to a rented 2×H100 and
back. That is the product promise, and it is one `HashMap` lookup plus one JSON key rewrite.

**Children outlive the manager.** A supervising daemon that owns its children couples the lifetime
of the expensive thing (a 30 GB model that took three minutes to load) to the lifetime of the cheap
one (a manager you want to restart freely). `setsid` + facts-on-disk + identity-verified adoption
inverts that, and it is why `apexrouter serve --stop`, a crash, and a `cargo install` are all safe
to do while a model is hot.

**Facts on disk, status computed.** LocalRouter had four implementations of "what is active" that
disagreed. The cure is not "add a fifth authority"; it is to persist only what cannot be recomputed
(pid, start ticks, boot id, port, argv, desired state) and derive everything else on read. A
`status: "running"` string is a lie the moment someone types `kill`.

**Money safety earns its complexity budget** because the failure mode — a GPU billing overnight with
no local record — has already happened in this codebase, and the account has $7.73 of credit against
$3.34/hr boxes. A reservation row written before the billing call, an append-only ledger where
"active" is a query, a `SpendApproval` no code path can fabricate, a daemon-side hard ceiling, and
startup reconciliation turn that from a possibility into an alert.

**One argv builder, one fit solver, one resolver, one credential chain, one check registry.** Every
duplicated table in LocalRouter (docker images in three places, GPU pricing in four, sampling
presets in two that disagree, "what is active" in four) became a bug. The port's job is not to
translate them into Rust; it is to have one of each.

And everything else follows the garden's existing shape rather than inventing: one serde-only
protocol crate every surface shares, hand-rolled MCP over stdio echoing the client's protocol
version, a three-file no-build web UI, a Slint app that is an edge client, a CLI with `--json` on
everything, XDG state with nothing written into the repo, and credentials named rather than stored.
The point of following it is that Andre can open this repo six months from now and already know
where everything is.
