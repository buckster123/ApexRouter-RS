# Design proposal — daemon-first ApexRouter-RS

**Lens: operational correctness and one source of truth.**
**Author: design pass, 2026-07-30. Status: proposal, not yet a charter decision.**

> One process owns the state, the sockets, and the children. Everything else — Slint app, web UI,
> CLI, MCP — is an edge client over one documented API. There is no second business-logic path and
> no second writer.

---

## 0. The thesis in one page

LocalRouter's defects are not typos. They are the predictable consequences of **having no owner**.
Five programs (the TUI, `endpoint_proxy.py`, `vast_up.sh`, `vast_tunnel.sh`, `vast_down.sh`) all
read and write the same handful of files, and each one re-derives the answer to "what is the active
endpoint?" independently. That single missing decision produces, mechanically:

| LocalRouter symptom | Root cause | Spec ref |
|---|---|---|
| `.active_endpoint` has **four** schemas | four writers, no owner | 02 §"Endpoint activation", 05 §7 |
| torn read mid-`write_text()` silently reroutes traffic to `127.0.0.1:8800` | no atomic write, no owner | 05 §7 |
| `resolve_target()` vs `get_active_endpoint()` disagree on the fallback | two resolvers | 07 D4 |
| two independent `config.toml` parsers, one section-blind | proxy can't import the package | 07 B2, E1 |
| `is_local_running()` **unlinks the pidfile as a read side effect** | predicate that mutates shared state | 03, 02 |
| health-check timeout leaves a live orphan `llama-server` + stale pidfile + live `.active_endpoint` | nobody is responsible for the child | 03 §"health-check poll loop" |
| `.last_instance` is one slot; launching B orphans a billing A | file used as a variable | 07 A2 |
| the proxy never logs usage; the TUI logs only its own test calls | the component that sees traffic isn't the component that owns the ledger | 05 §10 |
| `pgrep -n ssh` can kill an unrelated ssh | PID discovered instead of held | 07 G |

Every one of those is closed by the same move: **`apexrouterd` is the only process that mutates
router state, the only process that holds a `Child`, and the only process that answers "what is
active".** It holds an exclusive `flock` for its lifetime, so "is there an owner right now?" is a
question with a hard, cheap, race-free answer that every other surface can ask.

The three questions this lens has to answer honestly are answered in §4 (daemon down + CLI), §5
(children across daemon restarts) and §6 (serving through a backend swap). Those three sections are
the load-bearing part of this document.

---

## 1. Process model

Exactly **three** kinds of process exist.

```
                       ┌──────────────────────────── apexrouterd ────────────────────────────┐
                       │  one flock on $STATE/apexrouterd.lock  →  exactly one owner          │
                       │                                                                      │
  Slint app ──HTTP/WS──┤  control plane :2739   ── axum, bearer + loopback bypass             │
  web UI    ──HTTP/WS──┤       api_router() (pub use — ApexOS can mount it)                   │
  CLI       ──HTTP─────┤       /ws  snapshot-on-connect broadcast                             │
  MCP       ──HTTP─────┤                                                                      │
                       │  data  plane :8888     ── the OpenAI-compatible proxy (catch-all)     │
  OpenAI clients ──────┤       ArcSwap<RouteTable>, streaming relay, usage tee                 │
                       │                                                                      │
                       │  supervisor  ── owns every Child; reconciles on start                 │
                       │  pollers     ── health, rig, vast fleet, together limits              │
                       │  store       ── the ONLY writer of $STATE/**                          │
                       └───────┬─────────────────────────────┬────────────────────────────────┘
                               │ setsid, own process group   │
                       llama-server (N of them)        ssh -N -L (N tunnels)
```

### 1.1 Two listeners, one process — deliberately

The proxy's contract is a catch-all: `*` on `/{*path}`. LocalRouter proved what happens when a
control route shares that listener — llama.cpp's own `/health` is permanently shadowed and
`POST /health` is silently proxied (05 §2). So the control plane gets its **own** socket:

| Plane | Bind | Why |
|---|---|---|
| data | `127.0.0.1:8888` (`PROXY_PORT` env honoured) | frozen by every agent config and shell alias on this box (07 §2.1) |
| control | `127.0.0.1:2739` (`2739 = APEX` on a phone keypad, house mnemonic style) | no catch-all, so routes are stable forever |

Both live in one process because they must share one `RouteTable`, one `reqwest::Client` pool, one
usage writer and one supervisor. Splitting them would recreate the exact two-owner problem this
design exists to kill.

### 1.2 The lock is the design

`$STATE/apexrouterd.lock` is opened `O_CREAT|O_RDWR`, `flock(LOCK_EX|LOCK_NB)` for the whole
process lifetime, and contains a JSON identity record:

```jsonc
{"pid": 44219, "start_time_ticks": 918347, "control_url": "http://127.0.0.1:2739",
 "proxy_url": "http://127.0.0.1:8888", "started_at": "2026-07-30T14:02:11Z", "version": "0.1.0"}
```

Three properties fall out for free, and they are exactly the properties LocalRouter lacked:

1. **A second daemon cannot start.** `bail!("apexrouterd already running (pid {pid}) — 'apexrouter status'")`.
2. **Any process can ask "is there an owner?" in one syscall** — try `flock(LOCK_SH|LOCK_NB)`;
   failure means alive. This is a *real* answer, unlike a pidfile, because the kernel drops the lock
   on process death — no stale-pidfile class exists at all.
3. **Offline mutation is provably safe**, because a CLI that holds the exclusive lock knows no
   daemon is running (§4).

Note the contrast with LocalRouter, where `/tmp/vastai-gguf-proxy.pid` was written *before* the
listener bound, so an `EADDRINUSE` left the TUI reporting "running" for a corpse (05 §1).

### 1.3 Startup / shutdown

**Startup**, in order, each step failing loudly:

1. Resolve `Paths` (§7). `create_dir_all` everything.
2. Take the lock. If held → print the owner and exit 1.
3. Load `Config` into `Arc<Config>`. Missing file = full defaults (house rule); a *malformed* file
   is a hard error naming the key.
4. `store.load()` — endpoints, routes, catalog, ledger tail. Validate every path read from state;
   stale state is the normal case (00 ground truth: the saved instance points at a deleted model).
5. **`supervisor.reconcile()`** — §5. This runs *before* any listener binds, so the first request
   after a restart already sees the truth.
6. Bind the control listener. Bind the proxy listener. A non-loopback bind with no token configured
   `bail!`s with the fix in the message (house rule).
7. Write the lock record's `control_url`/`proxy_url`, write the legacy compat pidfiles
   (`/tmp/vastai-gguf-proxy.pid`) if `compat.legacy_pidfiles = true`.
8. Start pollers. Broadcast the first `Event::Snapshot`.

**Shutdown** (`SIGTERM`/`SIGINT`, via `tokio::signal`) sets a shutdown flag rather than exiting:

1. Stop accepting new control requests; proxy starts returning `503` **only for routes with no
   healthy upstream** — it keeps relaying otherwise.
2. `axum::serve(..).with_graceful_shutdown(..)` drains in-flight proxy requests, deadline
   `shutdown.drain_secs` (default 30).
3. Flush the usage writer and the ledger. Any `PendingLaunch` guard still alive writes its
   `orphan-suspect` record (§8.2).
4. **Children are NOT killed.** `llama-server` and the ssh tunnels keep running and are re-adopted
   on the next start (§5). `--kill-children-on-exit` exists for dev loops.
5. Release the lock (implicit on exit), remove the compat pidfiles.

### 1.4 What is *not* a process

No TUI. The brief asks for two GUIs (Slint + embedded web); a third interactive front-end would be a
third place to get the state model wrong. 07's ratatui suggestion is **declined** — the CLI is
non-interactive and scriptable, and `apexrouter status --watch` covers "I want to stare at it in a
terminal" by polling the same API the GUIs use.

No shell scripts on the primary path. `vastai`, `jq`, `curl`, `bash` all disappear. `ssh` survives
(00 ground truth: OpenSSH 10.2p1 present; there is no REST substitute for a port forward).

---

## 2. Crates

House layout: `crates/<product>-<role>`, resolver 2, edition 2021, MSRV 1.75, `version 0.1.0`,
`MIT OR Apache-2.0`, `repository = github.com/buckster123/ApexRouter-RS`, `authors = ["Andre <buckster123>"]`.
Internal deps carry path **and** version; members use `dep.workspace = true`.
`[profile.release] = { lto = "thin", codegen-units = 1, strip = true }`. `rustfmt.toml` exists with
only a comment. Clippy-zero policy, `-p` scoped to headless crates.

```
ApexRouter-RS/
├── Cargo.toml                      members + default-members (slint excluded)
├── rustfmt.toml
├── config.example.toml
├── README.md  CLAUDE.md  SECURITY.md  BACKLOG.md
├── assets/banner.png               Imaginarium-generated, credited in the footer
├── docs/{CHARTER,API,ARCHITECTURE,SLINT,AGENTS,LICENSING}.md, docs/port/NN-*.md
├── openapi/apexrouter-v1.yaml
├── skills/apexrouter/SKILL.md
├── ui-web/{index.html,app.js,style.css}      no npm, no CDN, no dist/
└── crates/
    ├── apexrouter-protocol   serde-only wire types (serde, serde_json, chrono)
    ├── apexrouter-core       config, paths, store, ledger, supervisor, discovery, fit, pricing
    ├── apexrouter-providers  Provider trait + vast / together / hf / llamacpp / ssh clients
    ├── apexrouter-proxy      the data plane: route table, relay, SSE, usage tee
    ├── apexrouterd           the daemon binary (control API + WS + proxy + supervisor)
    ├── apexrouter-client     thin HTTP client (NodeClient) — CLI, MCP and Slint all use it
    ├── apexrouter-cli        clap CLI, bin `apexrouter`, default-run; `apexrouter mcp`
    ├── apexrouter-mcp        MCP dispatch lib + bin `apexrouter-mcp`
    └── apexrouter-slint      GPL-3.0-only, publish = false, NOT in default-members
```

`default-members = [protocol, core, providers, proxy, apexrouterd, client, cli, mcp]`. Root
`cargo build` never links Slint and never needs `libfontconfig1-dev`.

### 2.1 apexrouter-protocol — the contract crate

Serde only. **Every** surface deserializes the same types the daemon serializes; no frontend ever
string-matches (house rule, and it is what makes "one source of truth" observable rather than
aspirational).

```
lib.rs      re-exports, PRODUCT, VERSION, DEFAULT_CONTROL_BIND, DEFAULT_PROXY_BIND
endpoint.rs EndpointId, EndpointKind, EndpointSpec, Endpoint, EndpointStatus, ProcFacts, DeviceSel
route.rs    RouteKey, UpstreamRef, Route, RouteTable, SwapMode
rig.rs      Backend, Gpu, LlamaBuild, LocalModel, ModelShardSet, RigSnapshot
plan.rs     FitInput, FitPlan, VramBudget, ArgvPreview, ContainerEnvPreview
vast.rs     SearchProfile, OfferQuery, Offer, VastAccount, VastInstance, BootPhase, RentRequest
usage.rs    Money, TokenCount, CostEstimate, UsageRecord, UsageSummary, PriceSource
catalog.rs  Recipe, RecipeKind, RecipeDraft, Provenance, ValidationReport
check.rs    CheckId, CheckStatus, CheckResult
event.rs    Event (tag="type", snake_case), Snapshot, JobId, JobState
```

Types that carry the operational lens:

```rust
/// Persisted records store FACTS. Status is computed (07 C9) and never serialised into state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum EndpointStatus {
    Stopped,
    Starting { phase: BootPhase, since_unix: i64 },
    Ready    { since_unix: i64, slots_busy: u32, slots_total: u32, tps: Option<f32> },
    Degraded { reason: String },      // process alive, health failing
    Draining { in_flight: u32 },      // no new traffic; waiting to stop
    Failed   { reason: String, exit_code: Option<i32> },
}

/// Money is integer micro-USD. Floats never accumulate cost (07 A5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Money(pub i64);

/// A guess must never render as a fact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CostEstimate {
    Metered    { usd: Money, source: PriceSource },
    Approximate{ usd: Money, source: PriceSource, assumption: String },
    Unknown,
}

/// Reported vs estimated token counts are different types (07 A6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", content = "n")]
pub enum TokenCount { Reported(u32), Estimated(u32) }
```

`Event` is the WS protocol, `#[serde(tag = "type", rename_all = "snake_case")]`, `PartialEq` so the
daemon suppresses no-op broadcasts:

```
snapshot | endpoint_changed | endpoint_removed | route_changed | rig_changed
| vast_fleet_changed | boot_progress | log_line | usage_recorded | check_result
| job_changed | credit_changed | notice
```

`Box` the `snapshot` variant (it is the oversized one). `#[serde(default)]` on additive `Vec` fields.

### 2.2 apexrouter-core

```
paths.rs      Paths (XDG + APEXROUTER_HOME/APEXROUTER_CONFIG), legacy paths, ensure_layout()
config.rs     Config / ConfigFile, load/load_from/init_file/save, serializable()
secret.rs     Secret<String> (Debug/Display = "***", only accessor expose()), CredentialRef, resolver
store.rs      Store — THE only writer of $STATE. atomic_write (tmp+fsync+rename), 0600 helper
lockfile.rs   DaemonLock, DaemonProbe { Owned(record) | Free }
ledger.rs     append-only ledger.jsonl, PendingLaunch Drop guard, reconcile queries
usage.rs      UsageWriter (O_APPEND single write), legacy usage.log mirror, windowed aggregation
pricing.rs    PriceTable (config + live refresh), Money arithmetic
proc.rs       ProcIdentity{pid,start_time_ticks,exe,cmdline_hash}, Liveness, spawn_supervised()
supervisor.rs Supervisor: reconcile(), start(), stop(), adopt(), health poller, log rotation
discover/     local.rs (builds, devices, models), gguf.rs (header metadata for fit)
argv.rs       ArgvPlan — ONE builder for local argv AND container env; FlagSupport feature cache
fit.rs        fit(FitInput) -> FitPlan  — the pure VRAM solver
catalog.rs    Recipe CRUD, TryFrom<RecipeDraft>, toml_edit round-trip persistence
migrate.rs    legacy import from ~/.vastai-gguf + LocalRouter repo dir + recipes.toml
checks.rs     Check registry (doctor / diagnose / smoke share it), concurrent runner
error.rs      thiserror Error, pub type Result<T>
```

### 2.3 apexrouter-providers

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> ProviderId;
    async fn list_models(&self) -> Result<Vec<ModelInfo>>;
    async fn health(&self, ep: &Endpoint) -> Result<HealthReport>;
    fn price(&self, model: &str) -> CostEstimate;
    fn credential(&self) -> Option<&Secret<String>>;
}
```

```
http.rs      one ClientBuilder: rustls only, no_gzip/no_brotli/no_deflate (transparent relay, 07 D1),
             pool + keep-alive, retry_with_jitter(), Retry-After / x-ratelimit-reset awareness
vast/query.rs  OfferQuery -> serde_json::Value builder (PUT /search/asks/, verified 00c)
vast/mod.rs    account, offers, rent, instances, destroy — typed, with #[serde(flatten)] extra
vast/logs.rs   two-phase result_url poll (no Bearer on the result fetch)
together.rs    BARE-ARRAY /v1/models deserializer, chat relay, 429 handling
hf.rs          search (filter=gguf), paths-info sizing (authoritative), token discovery,
               gated-vs-missing classification, shard grouping
llamacpp.rs    /health, /v1/models, /props, /slots (never proxied outward), /metrics, timings parse
ssh.rs         TunnelSupervisor — spawns ssh, HOLDS the Child (kills pgrep forever, 07 G)
```

### 2.4 apexrouter-proxy

```
lib.rs        ProxyState { table: ArcSwap<RouteTable>, http: reqwest::Client, usage: UsageWriter,
                           inflight: InFlightRegistry, cfg: Arc<Config> }
table.rs      RouteTable::resolve(model: Option<&str>) -> Option<Arc<Upstream>>; health gating;
              drain bookkeeping; alias map
relay.rs      outbound_headers(inbound, cred) -> HeaderMap  (CONSTRUCTED, never copied — 07 B1)
              body relay both directions; connect/header/idle timeouts; loop guard
sse.rs        byte-for-byte SSE relay + optional tee for usage; never re-frames
compat.rs     legacy /health, /providers, /switch; the /v1 path normaliser
models.rs     aggregated GET /v1/models across all ready routes
usage_tee.rs  usage + timings extraction (buffered and streamed)
errors.rs     OpenAI-shaped {"error":{message,type,code,param}} + status mapping
```

### 2.5 apexrouterd / -client / -cli / -mcp / -slint

```
apexrouterd/src: main.rs, api/{mod,endpoints,routes,discover,catalog,vast,tunnels,providers,
                 usage,checks,jobs}.rs, ws.rs, assets.rs, shutdown.rs
apexrouter-client/src: lib.rs — NodeClient{ http, base, token }, private auth(RequestBuilder),
                 300 s timeout, manual status/text check before serde_json::from_str
apexrouter-cli/src: main.rs, daemon.rs (probe / autostart / offline gate), cmd/*.rs, render.rs
apexrouter-mcp/src: lib.rs (transport-agnostic dispatch(method, params)), tools.rs, backend.rs
                 (LocalBackend vs ProxyBackend), bin/apexrouter-mcp.rs
apexrouter-slint: build.rs (one slint_build::compile line), src/{main,api}.rs,
                 src/ui/appwindow.slint + palette.slint + components/*.slint
```

---

## 3. Configuration and state — one root, one writer

### 3.1 Paths

```
$APEXROUTER_CONFIG  ->  $APEXROUTER_HOME/config.toml  ->  $XDG_CONFIG_HOME/apexrouter/config.toml
$APEXROUTER_HOME    ->  $XDG_STATE_HOME/apexrouter/            (state)
                        $XDG_CACHE_HOME/apexrouter/            (HF metadata cache, help-output cache)
```

```
$STATE/
├── apexrouterd.lock              flock + owner record          (§1.2)
├── endpoints/<id>.json           facts only; atomic writes
├── routes.json                   the route table (facts)
├── catalog.toml                  saved recipes (toml_edit round-trip)
├── ledger.jsonl                  append-only: reservations, rentals, destroys, launches
├── usage.jsonl                   append-only: one row per completion (proxy-sourced)
├── jobs/<job-id>.json            long-running job records
├── logs/<endpoint-id>.log[.1..]  rotated, never truncated on start (03: truncation destroys the crash log)
├── ssh/known_hosts               dedicated — Vast recycles sshN.vast.ai hostnames
└── ssh/cm-<instance-id>          per-instance ControlPath (07 G: a shared path collides)
```

**Nothing is ever written into the repo directory.** LocalRouter's `.active_endpoint` /
`.last_instance` / `.hf_pin` in its own checkout is called out as a design flaw in 00 and 08; it is
not repeated.

### 3.2 config.toml

Top-level `[section]` tables, one struct per section, every field defaulted so a missing file is a
working zero-config install. Runtime-only fields are `#[serde(skip)]`; a separate `ConfigFile` is
what `save()` writes.

```toml
[server]
control_bind = "127.0.0.1:2739"
proxy_bind   = "127.0.0.1:8888"     # PROXY_PORT env still honoured (07 §2.1)
token_env    = "APEXROUTER_TOKEN"   # required for any non-loopback bind
loopback_bypass = true
ui_dir       = ""                   # escape hatch; empty = use the embedded ui-web

[proxy]
connect_timeout_ms   = 5000
headers_timeout_ms   = 120000       # a cold prompt-eval on a big model exceeds 30 s
idle_timeout_ms      = 120000       # BETWEEN stream chunks — never a total timeout on a stream (05 §8)
max_body_bytes       = 67108864     # 64 MiB, house default (aiohttp's silent 1 MiB cap was a bug)
wait_for_backend_ms  = 0            # >0 parks a request while a backend finishes starting
drain_timeout_ms     = 60000
strip_client_auth    = true

[supervisor]
health_deadline_ms   = 600000       # REAL deadline; on expiry the child is killed, not orphaned
health_interval_ms   = 3000
adopt_on_start       = true
kill_children_on_exit= false

[discovery]
model_roots  = ["~/models", "~/.cache/huggingface/hub"]
build_roots  = ["~/llama.cpp", "~/Projects/llama.cpp"]   # globbed as build*/bin/llama-server
ignore_globs = ["**/.cache/**", "**/*mmproj*", "**/*vocab*"]

[providers.together]
base_url    = "https://api.together.ai/v1"
api_key_env = "TOGETHER_API_KEY"    # NEVER a required plaintext field (house rule)

[providers.vast]
base_url     = "https://console.vast.ai/api/v0"
api_key_path = "~/.config/vastai/vast_api_key"
poll_min_ms  = 5000                 # Vast publishes no rate limits; never poll faster
max_boot_secs= 1800                 # watchdog: auto-destroy a wedged instance
require_confirm_above_usd_hr = 0.0  # 0.0 = every rental needs explicit confirmation

[compat]
legacy_pidfiles   = true            # /tmp/vastai-gguf-{proxy,tunnel}.pid
legacy_usage_log  = true            # mirror to ~/.vastai-gguf/usage.log
legacy_active_endpoint = ""         # path to LocalRouter's .active_endpoint; "" = off
```

### 3.3 Credentials

`Secret<String>`: `Debug`/`Display` print `***`, sole accessor `expose()`. Resolution chain, per 00
and 08:

```
explicit config value  →  ApexRouter config file  →  conventional third-party path
                          (~/.config/vastai/vast_api_key, ~/.cache/huggingface/token)
                       →  environment variable
```

Rules that are enforced structurally, not by discipline:
- A borrowed credential is **never** copied into `config.toml`. `ConfigFile` has no field for it.
- No credential ever reaches an argv. `reqwest` puts headers in-process; `llama-server` gets
  `--api-key-file` (a 0600 file in `$STATE`) or `LLAMA_ARG_API_KEY`, never `--api-key <secret>`
  (09: keeps it out of `/proc/*/cmdline`).
- Every config/state write goes through one helper that sets mode `0600` at `OpenOptions` time.
- `GET /v1/providers` returns `{id, base_url, credential: {source: "env:TOGETHER_API_KEY", present: true}}`
  — the source, never the value.

### 3.4 Backwards compatibility with `~/.vastai-gguf`

**Read** (always): `config.toml` `[providers.*]` via a real TOML parser; `usage.log` JSONL with
`epoch` optional and permissive `#[serde(default)]` + ignore-unknown so no row can fail;
`local_instances/<name>.json`; `.pinned_provider`; and, from the LocalRouter checkout,
`.active_endpoint` (all four shapes, via serde aliases for `activated_at`/`switched_at`),
`.last_instance`, `.instance_history`, `.hf_pin`, `recipes.toml`.

Legacy timestamps are `%Y-%m-%dT%H:%M:%SZ` in **local** time with a lying `Z` (02, 03). Parse
leniently; emit real RFC 3339 UTC going forward.

**Write** (opt-out, on by default while `~/.vastai-gguf` exists):
- mirror every usage row into `~/.vastai-gguf/usage.log` in the exact legacy field set,
- mirror the active route into `<compat.legacy_active_endpoint>` atomically (tmp+rename), so the
  Python TUI keeps working during migration,
- write `/tmp/vastai-gguf-proxy.pid` and `/tmp/vastai-gguf-tunnel.pid`.

`apexrouter migrate --dry-run` prints the import plan (recipes → catalog, provider config →
credential *references*, instances → ledger, usage → merged); `--apply` performs it. Recipes are
imported **read-only**: ApexRouter never writes back to `recipes.toml`. `.hf_pin` and
`.pinned_provider` are read once during migration and then dropped — they exist only because TUI
screens could not pass data, and row-level actions replace them.

---

## 4. The daemon isn't running and the user types a CLI command

This is where a daemon-first design usually gets annoying. The rule set below makes it not annoying,
without ever creating a second writer.

### 4.1 Resolution, in order

```rust
enum Serving { Daemon(NodeClient), Offline(OfflineStore), None(anyhow::Error) }

fn resolve(cmd: &Cmd, cfg: &Config) -> Serving
```

1. **Probe.** `flock(LOCK_SH|LOCK_NB)` on `$STATE/apexrouterd.lock`. Locked → an owner exists; read
   its record for the control URL. This is a syscall, not an HTTP round-trip, and it cannot be
   fooled by a stale pidfile.
2. **Confirm.** `GET {control_url}/health`, 300 ms timeout. Owner alive but not answering →
   `Degraded`: report it and exit 1 with `apexrouter serve --foreground` in the message. Never
   proceed as if offline while another process holds the lock.
3. **No owner** → branch on the command's declared `Need`.

```rust
#[derive(Clone, Copy)]
enum Need {
    Pure,       // no state at all: fit, discover, version, config path
    ReadState,  // may read $STATE directly under a SHARED lock
    Mutate,     // must go through the daemon
}
```

### 4.2 The policy table

| Command | `Need` | Daemon down → |
|---|---|---|
| `version`, `config path/show`, `fit`, `discover local\|hf\|vast\|together` | `Pure` | runs, no daemon involved |
| `status`, `endpoints ls/get`, `route ls`, `models ls`, `usage`, `recipes ls/show`, `vast ls` (cached), `doctor` | `ReadState` | serves from `$STATE` under a **shared** flock; every output is tagged `served_by: "offline"` and carries `"stale": true` for anything derived from a poller |
| `up`, `down`, `endpoints start/stop/restart/rm`, `route set/alias`, `recipes new/edit/rm`, `vast rent/destroy`, `tunnel up/down`, `switch`, `migrate --apply`, `smoke`, `diagnose` | `Mutate` | **autostart** (default) → §4.3 |
| `serve` | — | becomes the daemon |

Human output prints a single dim line `(offline — apexrouterd is not running)` before the table.
`--json` never prints anything but the protocol type, so the flag lives *inside* it:

```jsonc
{ "served_by": "offline", "as_of_unix": 1785412331, "stale": true, "endpoints": [ … ] }
```

`served_by` is on **every** `--json` envelope, daemon or not. A script can tell where its answer came
from without parsing prose.

### 4.3 Autostart

`Mutate` with no owner, and `cli.autostart = true` (default):

1. `Command::new(current_exe_dir/"apexrouterd")` — resolved next to the CLI binary first, then
   `$PATH`; `.process_group(0)`, stdin `null`, stdout/stderr → `$STATE/logs/apexrouterd.log`.
   **stdout is never inherited** — that matters because `apexrouter mcp` shares this code path and
   owns stdout for JSON-RPC.
2. Poll `GET /health` every 50 ms up to 5 s. Success → proceed and print
   `(started apexrouterd, pid N)` to **stderr**.
3. Failure → exit 1 with the tail of `apexrouterd.log` and the fix.
4. `--no-autostart` / `cli.autostart = false` turns this into a plain error.

Autostart is safe precisely because of the lock: if two CLI invocations race, one wins the `flock`
and the other's spawned daemon exits immediately with "already running"; the loser's poll then
succeeds against the winner. No coordination code needed.

### 4.4 The one offline mutation

`apexrouter migrate --apply` and `apexrouter config init` may run offline. They take the
**exclusive** lock first. Holding it proves no daemon is running, so there is no race — and if a
daemon *is* running, they fail with "stop apexrouterd first" rather than racing it. That is the
whole ownership story in three lines of code, and it is the thing LocalRouter never had.

### 4.5 MCP

`apexrouter mcp` follows the same table. Read tools work offline against `$STATE`. Write tools
autostart. `LocalBackend` (does the work in-process, holds the credential) vs `ProxyBackend`
(forwards to a remote node with a bearer token) selected by `--proxy URL` / `-p` / `APEXROUTER_URL`
+ `APEXROUTER_TOKEN` — the house `Backend` trait pattern, which is also the LAN-node story.

---

## 5. Supervising `llama-server` across daemon restarts

### 5.1 Spawn

`core::proc::spawn_supervised(plan: &ArgvPlan) -> Result<SupervisedChild>`:

- argv vector, **never** `sh -c`. A CI grep bans `"sh", "-c"` in the workspace (07 directive 1).
- `pre_exec` → `setsid()` (or `.process_group(0)`): the child is a session leader, so Ctrl-C in the
  terminal that started the daemon does not kill a model that took 90 s to load, and the child
  survives daemon exit.
- `LD_LIBRARY_PATH = dirname(binary)` explicitly. **This is the RUNPATH trap** from 03: the
  `build-vulkan` binary has a trailing-colon RUNPATH (= cwd on the search path), so exec'ing it from
  a sibling build's `bin/` dies with `undefined symbol: _Z23common_init_from_paramsR13common_params`.
  LocalRouter dodges it by accident via `cwd=ROOT`. We do it deterministically and set `cwd` to `/`.
- Backend env: `GGML_VK_VISIBLE_DEVICES` / `HIP_VISIBLE_DEVICES` / `CUDA_VISIBLE_DEVICES` derived
  from the selected `DeviceSel`, plus `-dev Vulkan0,...` on the argv. Never a hardcoded device.
- Log file opened once, `Stdio::from(file)` **consumes** it (RAII kills the fd leak of 07 C8).
  Rotate at `supervisor.log_max_bytes`; never truncate on start.
- Port pre-check: bind-probe before spawn → `LaunchError::PortInUse { port, held_by: Option<EndpointId> }`
  naming the endpoint from the store (07 C10).

Immediately after spawn, before anything else can fail, the store writes:

```jsonc
// $STATE/endpoints/<id>.json — FACTS ONLY
{
  "schema_version": 1,
  "id": "local-carnice-9b",
  "kind": "local_llama_cpp",
  "spec":  { "binary": "/home/andre/llama.cpp/build-vulkan/bin/llama-server",
             "model_path": "/home/andre/models/carnice-9b/Carnice-9b-Q6_K.gguf",
             "argv": ["--model", "…", "-c", "32768", "…"],
             "env": {"GGML_VK_VISIBLE_DEVICES": "0"},
             "host": "127.0.0.1", "port": 8100, "devices": ["Vulkan0"] },
  "proc":  { "pid": 44310, "start_time_ticks": 918402,
             "exe": "/home/andre/llama.cpp/build-vulkan/bin/llama-server",
             "cmdline_sha256": "9f2a…" },
  "started_at": "2026-07-30T14:03:02Z",
  "log_path": "/home/andre/.local/state/apexrouter/logs/local-carnice-9b.log"
}
```

There is **no `status` field**. Status is computed (§5.3).

### 5.2 Reconcile — the restart story

`Supervisor::reconcile()` runs before any listener binds. For each `endpoints/<id>.json`:

```rust
enum Adoption {
    Adopted   { proc: ProcIdentity },      // /proc/<pid> exists, start_time matches, exe matches
    Foreign   { pid: u32 },                // port answers but identity does not match ours
    Vanished,                              // no process, port silent
    Ambiguous { reason: String },          // EPERM etc — needs the operator
}
```

The identity check is `(pid, /proc/<pid>/stat field 22 start_time, /proc/<pid>/exe)`. Matching all
three makes PID reuse *detectable*, not merely survivable (07 C7). `Liveness` is a three-valued
enum — `Alive | Dead | Unknown(io::Error)` — so `EPERM` (the process exists but is another user's)
is a case the caller must handle, not an exception that crashes a menu render.

| Outcome | Action |
|---|---|
| `Adopted` | resume health polling, re-arm the route, emit `endpoint_changed`. **No restart, no reload.** A 30 GB model stays hot across a daemon upgrade. |
| `Foreign` | mark `Degraded{reason:"port held by a foreign process"}`, do not route to it, surface it in `doctor`. Never SIGTERM something we did not start. |
| `Vanished` | append `endpoint_exited` to the ledger with the log tail, keep the log file, set `Failed` if it was expected to be up, clear the route. |
| `Ambiguous` | `Degraded`, and `doctor` prints the exact `kill`/`ps` command. |

The daemon **does** keep a `tokio::process::Child` for children it spawned in this lifetime, purely
to `wait()` and reap them (a `setsid` child is still our child until we exit). `kill_on_drop` stays
false. On daemon exit the children reparent to init and the next reconcile re-adopts them. That is
the correct trade for an inference server and it is the opposite of LocalRouter, where the server
shares the TUI's process group and dies with the terminal (03).

### 5.3 Computed status, health, and no-orphan guarantees

A background poller per endpoint, interval `supervisor.health_interval_ms`:

```
identity ok?  →  GET /health (llama.cpp: 200 {"status":"ok"} / 503 "Loading model")
              →  GET /props  (real model path, effective n_ctx, total_slots, build_info)
              →  GET /slots  (busy/total; NEVER proxied outward — it echoes prompts)
              →  timings from the last completion  →  tok/s
```

Startup uses a **real total deadline** (`health_deadline_ms`, default 600 s) with the ×3
`BootPhase` breakdown, not 60 iterations of `sleep(1) + connect_timeout(3)` that can silently run
four minutes (03). On deadline expiry or early exit, the supervisor **kills the child, removes the
record, marks `Failed{reason}` with the log tail, and clears any route pointing at it.** LocalRouter
leaves a live orphan plus a stale pidfile plus a live `.active_endpoint` on this exact path; that
class of bug is closed by making one component responsible for the whole transaction.

`stop()`: mark `Draining` → wait for in-flight to hit 0 or `drain_timeout_ms` → verify identity →
`SIGTERM` → poll 10×500 ms → `SIGKILL` → `wait()` (no zombies) → append to the ledger. Identity is
re-verified immediately before the signal, so a reused PID is never signalled.

---

## 6. Serving through a backend swap

### 6.1 The route table

```rust
pub struct Upstream {
    pub endpoint: EndpointId,
    pub provider: ProviderId,          // "local" | "vast-gguf" | "together" | "vllm" | "remote"
    pub base: Url,                     // stored WITHOUT the /v1 suffix — see §6.4
    pub cred: Option<CredentialRef>,
    pub model_rewrite: Option<String>, // what to put in the body's "model"
    pub health: HealthState,
    pub inflight: Arc<AtomicU32>,
    pub accepting: AtomicBool,         // false = draining
}

pub struct RouteTable {
    pub default: Vec<Arc<Upstream>>,          // ordered: first healthy wins
    pub aliases: HashMap<String, Vec<Arc<Upstream>>>,
}
```

Held as `ArcSwap<RouteTable>`. Per request the proxy does **one atomic pointer load**. Compare with
LocalRouter, which does `stat()` + `read()` + `json.loads()` (+ sometimes a whole `config.toml`
line-scan) **on every single proxied request** (05 §3) — and which can read a truncated file
mid-write and silently reroute all traffic to `127.0.0.1:8800`.

### 6.2 Swap semantics

`PUT /v1/routes/default {"endpoint_id": "...", "mode": "drain" | "now" | "start_then_swap"}`

**`now`** — build a new `RouteTable`, `table.store(Arc::new(new))`. In-flight requests already hold
`Arc<Upstream>` and finish against the old backend, returning the old `X-Provider`. New requests use
the new one. This is exactly LocalRouter's observable behaviour, minus the torn-read race, minus the
per-request file parse.

**`drain`** (default) — the operationally correct version:

```
1. old.accepting = false                      new requests stop selecting it, instantly
2. table.store(new)                           new default is live; zero-request gap
3. await old.inflight == 0, or drain_timeout_ms
4. only THEN is the old endpoint eligible to be stopped
```

The proxy is never unavailable during a swap. A 4 000-token generation in flight is not truncated.

**`start_then_swap`** — the feature LocalRouter cannot express at all, and the reason a daemon is
worth building:

```
1. start endpoint B on a free port          (proxy keeps serving A the whole time)
2. wait for B: Ready                        (BootPhase streamed over WS to both GUIs)
3. atomic table.store: default -> [B, A]    (A demoted to fallback, still healthy)
4. drain A
5. stop A, table.store: default -> [B]
```

Zero-downtime model swap behind a stable `127.0.0.1:8888`. `apexrouter up <model> --swap` is one
command.

### 6.3 When nothing is healthy

Ordered fallback: `default` is a `Vec`, and `resolve()` returns the first upstream that is
`accepting && health.is_ok()`. That gives free failover from a rented GPU to a local model when the
tunnel drops — health-gated routing, which LocalRouter computes in `/providers` and then never
consumes (05 §14 item 2).

If no upstream is healthy:
- `wait_for_backend_ms == 0` (default) → `503` with an OpenAI-shaped body naming the reason
  (`no_backend`, `all_backends_unhealthy`, `starting`).
- `> 0` → the request parks on a `tokio::sync::Notify` fed by the health poller, up to the deadline,
  then serves or 503s. This absorbs a cold start rather than failing the first prompt after a swap.

### 6.4 Drop-in compatibility (non-negotiable)

The `/v1` doubling bug (05 §4.1) is the highest-risk drop-in incompatibility, because SKILL.md tells
agents to use `OPENAI_BASE_URL=http://localhost:8888/v1` — the path that is broken today. Fix by
storing upstream bases **without** `/v1` and canonicalising the inbound path to exactly one `/v1`
prefix. `http://localhost:8888` and `http://localhost:8888/v1` both work. `smoke.sh` (which appends
`/v1` to whatever you give it) passes against either.

Preserved byte-for-byte:
- `GET /health` → `{"ok":true,"provider":"<id>","uptime":<float>}`, always 200, never probes.
- `GET /providers` → the exact §2 shape of 05, plus additive fields. The Together probe now falls
  back to `config.toml` (fixing the inconsistency where a config-only key reports `available:false`
  while requests succeed), and the two probes run **concurrently** instead of 8 s serially.
- `POST /switch` → same bodies, same responses, `together` | `vast-gguf` | `local`. Now implemented
  as a route change. **Hardened**: an arbitrary `base_url` is no longer honoured — it must match a
  configured provider's origin, else `400`. That kills the credential-exfiltration primitive (05
  §11) without breaking any documented call. The `api_key` field, which today is silently ignored,
  is now honoured and stored as a credential reference.
- `X-Provider` and `X-Usage: "{prompt}+{completion}"` response headers, including on streams (which
  LocalRouter never emits).
- Every other (path, method) pair proxied, including `POST /health`.

Relay correctness, all fixes over the Python:
- Outbound headers are **constructed from an allowlist**, never copied (07 B1). Unit test:
  `authorization` / `proxy-authorization` / `cookie` never appear unless a `Credential` supplied them.
- `no_gzip/no_brotli/no_deflate` on the client → bytes relayed untouched, `Content-Encoding` stays
  honest (07 D1). Multi-valued response headers preserved (`Set-Cookie` no longer collapses).
- One process-wide `reqwest::Client`. LocalRouter builds a fresh `ClientSession` — and therefore a
  fresh TLS handshake to Together — **per request** (05 §13).
- SSE: relay whatever arrives, immediately, no re-framing, no fill-the-buffer delay.
  `Content-Type: text/event-stream` forced **only** when upstream is 2xx *and* actually SSE;
  otherwise fall through to the buffered path, so a `400 {"error":…}` on a `stream:true` request no
  longer arrives mislabelled and chokes the SDK parser (05 §6).
- Timeouts split into connect / headers / **inter-chunk idle**. Never a total timeout on a stream
  (that is what half-writes a response and then raises inside an already-prepared response today).
- Loop guard: `Via: apexrouter/0.1` + `X-ApexRouter-Hop`; seeing our own hop → `508`. Kills the
  `/switch {"port":8888}` self-recursion footgun.
- Errors are OpenAI-shaped: connect refused → 502, upstream timeout → 504, no backend → 503,
  oversized → 413, loop → 508.
- **Usage is logged from the request path** — where the real traffic is. Non-streaming parses
  `usage` from the buffered body; streaming tees and parses the terminal chunk when
  `stream_options.include_usage` is set, else records `TokenCount::Estimated`. Rows are appended
  `O_APPEND` in a single `write` so concurrent writers cannot interleave, in the legacy field set,
  and mirrored to `~/.vastai-gguf/usage.log`.
- **Model aliasing** — the #1 papercut in 05 §14. `aliases` maps a client-visible model name to an
  upstream plus a `model_rewrite`, so a client that says `"model": "x"` keeps working when the route
  moves from llama.cpp (ignores the name) to Together (requires a real id).
- **Aggregated `GET /v1/models`** — a union over all ready routes with aliases included, instead of
  a passthrough whose contents change under the client's feet.

CORS: today `Access-Control-Allow-Origin: *` is set on proxied responses with no preflight handler,
so browsers do not actually work. mk1 ships an explicit origin allowlist (empty by default) with a
real `OPTIONS` handler and `Access-Control-Expose-Headers: X-Provider, X-Usage`. Never
`allow_origin(Any)` (house rule).

---

## 7. Control-plane API

Auth: bearer in three presentations (`Authorization: Bearer`, `X-ApexRouter-Token`, `?token=`),
scopes `read|write|admin` derived from (path, method). Loopback bypass requires **both** an explicit
opt-in and a genuinely loopback peer IP from `ConnectInfo<SocketAddr>` — absent connect-info fails
closed. Served with `into_make_service_with_connect_info::<SocketAddr>()`. A non-loopback bind with
no token configured refuses to start. No `CorsLayer` on the authenticated API. The trace span records
method + path only, never the query string (it can carry `?token=`). axum 0.8 `{param}` syntax.
`DefaultBodyLimit::max(64 MiB)`.

```
GET    /health                               public: {ok, product, version}
GET    /ws                                   Event stream; snapshot on connect; Lagged -> resnapshot
GET    /v1/status                            Snapshot (rig + endpoints + routes + fleet + usage head)

GET    /v1/endpoints                         [Endpoint]
POST   /v1/endpoints            [?no_wait]   EndpointSpec -> Endpoint (create + start)
GET    /v1/endpoints/{id}
DELETE /v1/endpoints/{id}                    stop + forget
POST   /v1/endpoints/{id}/start|stop|restart {mode: drain|now}
GET    /v1/endpoints/{id}/logs?tail=&follow= text/plain; follow -> chunked
POST   /v1/endpoints/{id}/adopt              take over a Foreign/Ambiguous process explicitly

GET    /v1/routes
PUT    /v1/routes/default                    {endpoint_id, mode: drain|now|start_then_swap}
PUT    /v1/routes/aliases/{alias}            {endpoint_id, model_rewrite?}
DELETE /v1/routes/aliases/{alias}

GET    /v1/rig                               RigSnapshot: GPUs, builds, RAM, disk, backends
GET    /v1/discover/local                    {builds, devices, models(+shard groups, sizes)}
GET    /v1/discover/hf?q=&limit=             HF model search (filter=gguf)
GET    /v1/discover/hf/{owner}/{repo}/files  siblings + authoritative sizes via paths-info
POST   /v1/discover/vast/offers              OfferQuery -> [Offer]  (PUT /search/asks/ upstream)
GET    /v1/discover/together/models          [ModelInfo] with pricing

POST   /v1/fit                               FitInput -> FitPlan (+ ArgvPreview, ContainerEnvPreview)

GET    /v1/catalog/recipes                   [Recipe]
POST   /v1/catalog/recipes                   RecipeDraft -> Recipe (TryFrom validates)
GET    /v1/catalog/recipes/{id}
PUT    /v1/catalog/recipes/{id}
DELETE /v1/catalog/recipes/{id}
POST   /v1/catalog/recipes/{id}/validate     ValidationReport (incl. staleness: model gone, tier gone)

GET    /v1/vast/account                      credit, balance, can_pay, burn-down estimate
GET    /v1/vast/instances                    fleet + total $/hr + accrued
POST   /v1/vast/instances                    RentRequest{offer_id, spec, confirm:{max_usd_hr, token}}
DELETE /v1/vast/instances/{id}
GET    /v1/vast/instances/{id}/logs?tail=    two-phase result_url, resolved server-side
POST   /v1/vast/reconcile                    reconcile the ledger against the live fleet (orphans)

GET    /v1/tunnels
POST   /v1/tunnels                           {instance_id, local_port, remote_port}
DELETE /v1/tunnels/{id}

GET    /v1/providers                         base_url + credential SOURCE (never the value)
PUT    /v1/providers/{id}                    {base_url?, api_key_env?, api_key?}  (write-only key)
POST   /v1/providers/{id}/test               connection + optional completion probe

GET    /v1/usage?since=&until=&group_by=     UsageSummary
GET    /v1/usage/records?since=&limit=       [UsageRecord]

POST   /v1/checks/run                        {only:[CheckId]} -> JobId; results stream over /ws
POST   /v1/smoke                             {target} -> JobId; 4 named probes with pass/fail + timings
GET    /v1/jobs/{id}                         JobState

GET    /v1/metrics                           Prometheus text (proxy counters, per-route latency/TTFT)
GET    /                 GET /{*path}        embedded ui-web (refuses to shadow /v1 and /health)
```

Long-running operations use the house `?no_wait` pattern: return the pending record immediately;
the spawned task flips it to `failed` on **every** error path including `JoinError` from a panic, so
nothing sits pending forever.

Error mapping (house idiom B for the control plane: `type ApiError = (StatusCode, String)`):
400 bad input · 401 missing/invalid token · 403 scope · 404 unknown id · 409 conflicting state
(e.g. port held) · 413 too large · 500 internal · 502 upstream provider failed · 503 feature
disabled or no backend.

---

## 8. Subsystems

### 8.1 Local llama.cpp — discovery, argv, fit

**Discovery** (`discover/local.rs`) fixes three measured bugs from 03:

- Builds: glob `{build_root}/build*/bin/llama-server` (finds `build-mtp` and `build-zaya1`, which
  the fixed candidate list misses), plus `$PATH`. Label with the **build directory name**, not the
  basename.
- Backends: **never grep `--help`.** The installed binary's 635-line help contains zero occurrences
  of `vulkan`/`cuda`/`hip`/`rocm`, and `--gpu` matches `--gpu-layers`, so LocalRouter reports
  `backends == ["cuda"]` on a machine with no NVIDIA hardware. Use `llama-server --list-devices`
  (`Vulkan0: AMD Radeon 840M Graphics (RADV KRACKAN1) (20992 MiB, 19518 MiB free)`), falling back to
  inspecting sibling `libggml-{vulkan,hip,cpu}.so`. **Exclude `llvmpipe`.** Enumerate a `Vec<Gpu>`
  per build — never a scalar `backend` (00b: N GPUs, N builds, N backends).
- Models: recurse `discovery.model_roots`, **follow symlinks** (Python 3.14's `rglob` does not, which
  hides `resources/models/ternary-bonsai-27b`), skip `.cache`, match `mmproj`/`vocab` as filename
  tokens rather than substrings anywhere in the path, and **group multi-shard GGUFs** into a
  `ModelShardSet` with a total size. Sort smallest-first by default; the largest-first order is
  backwards on any machine where fit matters.

**Argv** (`argv.rs`) is **one** builder producing both the local argv and the container env, so the
Python/bash divergence (Python omits `--top-k 20`; bash has it in all three presets) cannot recur.
`launch.sh` is authoritative: the unified presets are

```
thinking     --temp 1.0 --top-p 0.95 --top-k 20 --min-p 0.0 --presence-penalty 1.5
coding       --temp 0.6 --top-p 0.95 --top-k 20 --min-p 0.0 --presence-penalty 0.0
nonthinking  --temp 0.7 --top-p 0.80 --top-k 20 --min-p 0.0 --presence-penalty 1.5
             --chat-template-kwargs {"enable_thinking":false}
```

Flags are **feature-detected** against `llama-server --help`, cached in `$CACHE` keyed by the
binary's mtime+size. b9199 reality (00): `--jinja` is already default-on (emitting it is a no-op),
`-fa` takes `on|off|auto`, `-np` defaults to `-1`, `--slots` is on, `--metrics`/`--props` are off,
`-ctk/-ctv` accept `iq4_nl`. New: when the recipe omits `ctx`/`ngl`, leave them **unset** so
llama.cpp's own `--fit` auto-sizing can work — impossible today because the builder always emits
explicit values. Always pass `-a/--alias <endpoint id>` and an explicit `-dev`.

**Fit** (`fit.rs`) is the highest-value unbuilt feature in the whole plan (07 Part 3 phase 4.3). It
is a pure function with unit tests, reachable from CLI, MCP, REST and both GUIs:

```rust
pub fn fit(input: FitInput) -> FitPlan;
// weights_bytes  <- GGUF file size (local) or HF paths-info (remote)
// kv_bytes       <- GGUF header metadata: n_layer, n_head_kv, n_embd_head, hybrid-layer count
// budget         <- sum of selected devices' free VRAM, minus fit_target margin
// -> { max_ctx, max_parallel, kv_type, n_gpu_layers, headroom_bytes, verdict, why: Vec<String> }
```

That one function replaces the 54 hand-solved `description` strings in `recipes.toml` ("Q4 (21.2
GiB) + hybrid KV (8.2 GiB) = ~29.4 GiB. Fits.") and works for a model published tomorrow. `why` is
rendered as the tooltip next to every derived field in the draft editor, so the operator can see the
arithmetic and override it.

### 8.2 Vast.ai — money safety first

Everything is REST over `reqwest`+`rustls`; the `vastai` CLI is never invoked (00: it is broken on
this box). The verified shapes from 00c win over desk research: `PUT /api/v0/search/asks/` with a
single `q` object, `{"offers":[…]}` back; `PUT /api/v0/asks/{offer_id}/` to rent, instance id comes
back as **`new_contract`**; `GET /api/v0/instances/` returns `{"instances_found":N,"instances":[…]}`.
Offers deserialize into ~25 named fields plus `#[serde(flatten)] extra: Map<String, Value>` so the
GUI can show an all-fields inspector and no upstream addition breaks parsing.

The `.last_instance` single slot is replaced by **`ledger.jsonl`**, append-only:

```jsonc
{"ts":"…","event":"rental_reserved","reservation":"01J…","offer_id":43731729,"dph_total":0.305,"spec":{…}}
{"ts":"…","event":"rental_confirmed","reservation":"01J…","instance_id":28714412}
{"ts":"…","event":"rental_destroy_requested","instance_id":28714412}
{"ts":"…","event":"rental_destroyed","instance_id":28714412,"accrued":{"usd_micro":915000}}
```

"Active" is a **query** (`WHERE destroyed_at IS NULL`), not a file that can hold one thing.

The create→record window is closed by a guard type:

```rust
let pending = ledger.reserve(&spec, &approval)?;   // WRITTEN TO DISK BEFORE the API call
let id = vast.rent(offer_id, &spec).await?;
pending.commit(id)?;                                // ledger: rental_confirmed
// impl Drop for PendingLaunch: if not committed, append rental_orphan_suspect
```

`SpendApproval { max_usd_per_hour, confirmed_at, surface }` must be constructed to reach `rent()` —
there is no code path to a billing call without one. Every surface constructs it explicitly:
`--yes` on the CLI, a modal with credit + burn-down in the GUIs, an explicit
`confirm_spend_usd_per_hour` argument with no default in MCP. At $7.73 credit, a 2×H100 at $3.34/hr
is 2.3 hours (00c) — the confirm dialog shows exactly that number.

On daemon start, `vast::reconcile()` diffs the ledger against `GET /instances/` and reports
orphan-suspects and unrecorded live instances as a `notice` event plus a `doctor` finding.

Boot is a state machine, not a log tail:
`BootPhase { Provisioning, Pulling, Compiling, Downloading, Loading, Healthy, Failed(reason) }`,
driven by the instance logs API and `actual_status`. `exited|offline|unknown` are **terminal** —
they never reach `running`, and a poll loop that does not know that bills forever. A
`max_boot_secs` watchdog auto-destroys (with a loud notice) rather than polling forever. Polling is
never faster than `poll_min_ms` (Vast publishes no rate limits and sends no `Retry-After`).

Offer search unifies the two divergent implementations. LocalRouter's browser searches at
`reliability>0.97 / inet_down>300` and then hands "auto — cheapest" to `vast_up.sh`, which searches
at `>0.99 / >500` — so you rent an offer you never saw, and the widened fallback silently drops your
geo constraint. mk1 has **one** search, and any relaxation is shown as an explicit "widened: geo
dropped, reliability 0.99→0.97" banner on the result set.

Search profiles are config, not 19 hardcoded tiers:

```toml
[[vast.profiles]]
id = "3090-multi"; label = "2–4× RTX 3090"
gpu_names = ["RTX 3090"]; num_gpus_min = 2; num_gpus_max = 4
min_reliability = 0.98; min_inet_down = 300; min_disk_gb = 120; image_type = "prebuilt"
[[vast.profiles]]
id = "h100-1x2"; label = "1–2× H100 SXM"
gpu_names = ["H100 SXM", "H100 NVL"]; num_gpus_min = 1; num_gpus_max = 2
min_reliability = 0.98; min_inet_down = 500; min_disk_gb = 200; image_type = "builder"
```

`gpu_name` is a **string**, never an enum (00c: the vocabulary changes with the market), and the GUI
populates its dropdown from the distinct values in a broad live search plus a free-form field.
3090 profiles carry Ampere caveats as data: no FP8, so `kv_cache_dtype=fp8` is rejected;
`cuda_vers>=12.8` is surfaced as an editable field rather than a hardcoded floor.

The container env contract (`MODEL_REPO`, `MODEL_QUANT`, `CTX`, `KV_TYPE`, `MODE`, `PARALLEL`,
`HOST`, `IMAGE_TYPE`, `MMPROJ`, `HF_TOKEN`, `LLAMA_CPP_REPO`, `LLAMA_CPP_REF` …) is reproduced
exactly by `argv.rs`'s container path, because the published GHCR images are unchanged. `HOST` is
forced to `127.0.0.1` at create **and** on every stall-restart (`launch_vllm.sh`'s own default is
`0.0.0.0`, so the tunnel-only posture depends on that override). `HF_TOKEN` moves from the
`--onstart-cmd` string (which Vast persists and echoes back) into the env map.

Tunnels: `TunnelSupervisor` spawns `ssh -N -L <local>:127.0.0.1:<remote> -p <port> root@<host>` and
**holds the `Child`** — `pgrep -n ssh` is gone, and with it the race that kills an unrelated ssh.
Mandatory flags: `-o ExitOnForwardFailure=yes` (otherwise ssh stays alive with a dead forward),
`ServerAliveInterval=30 -o ServerAliveCountMax=3`, `-o UserKnownHostsFile=$STATE/ssh/known_hosts -o
StrictHostKeyChecking=accept-new` (Vast recycles `sshN.vast.ai` names), and a per-instance
`ControlPath`+`ControlPersist=5m` — the documented ~500 ms → RTT win for agentic tool loops, which
`doctor` checks for and reports.

Stall detection survives as a **passive check** on the instance card with one-click restart, not a
30-second blocking diagnostic: the 4 s `eth0` RX delta, `<1000 bytes` = STALLED, `<50 Mbps` = slow.

The `keep-your-fork` knowledge is preserved as a small `[[known_forks]]` config table
(`fairydreaming/llama.cpp @ deepseek-dsa` for DeepSeek-V4) with the rule "a fork implies
`image_type = builder`, +12–18 min cold start" enforced at validation time. That is undiscoverable
knowledge; the 71-recipe catalogue around it is not.

### 8.3 Together AI and remote OpenAI endpoints

Two endpoints only: `GET {base}/models` (a **bare array** — Together, unlike llama.cpp, has no
`{"object":"list","data":[…]}` envelope, so two deserializers) and `POST {base}/chat/completions`.
Pricing rides on each model object; the unit is undocumented, so it is stored raw with the
assumption recorded and rendered as `CostEstimate::Approximate`. `finish_reason` is a `String`,
never an enum (Together emits `eos`). On 429, read `x-ratelimit-reset`; `x-ratelimit-remaining` is
not reliably sent, so no budget display is built on it. The three hardcoded price tables and the
hardcoded popular-model list all die; the catalogue comes from the API.

`EndpointKind::RemoteOpenAi { base_url, credential }` covers "a llama-server on another box on the
LAN" with zero extra machinery — which is the 00b multi-node hook, without shipping discovery in mk1.

### 8.4 Checks: doctor, diagnose, smoke — one registry

```rust
pub trait Check { fn id(&self) -> CheckId; fn scope(&self) -> Scope;
                  async fn run(&self, ctx: &CheckCtx) -> CheckResult; }
```

Run selected checks **concurrently** with per-check timeouts, streaming `CheckResult` over WS as
each lands. `apexrouter diagnose --only rate-limits` becomes trivial; today you wait through four
sequential SSH probes to see a rate-limit header.

Checks in mk1: `creds.*` (vast/hf/together present + valid), `ports.*` (8888/2739/endpoint ports
free or ours), `builds.*` (found, devices enumerable, RUNPATH sanity), `models.*` (paths in state
still exist), `ssh.controlmaster` (the `~/.ssh/config` block), `disk.state`, `proxy.roundtrip`,
`endpoint.health`, `vast.credit`, `vast.orphans`, `together.ratelimits`, `net.stall` (remote RX
sample), `legacy.migration_pending`.

The four `smoke.sh` probes become named native steps with pass/fail badges and timings:
`models_list`, `warmup_completion` (80 tok, TTFT), `tool_calling` (`get_weather`, `tool_choice:auto`),
`throughput` (200 tok, tok/s from the `timings` object rather than a stopwatch). `smoke.sh`'s
hardcoded `"model":"x"` bug — which 400s on every managed provider — is fixed by using the resolved
route's model id. Local endpoints get diagnostics too; today they get an early return.

---

## 9. CLI

clap derive. Global args with env fallbacks (`--config` env `APEXROUTER_CONFIG`, `--home` env
`APEXROUTER_HOME`, `--url` env `APEXROUTER_URL`, `--token` env `APEXROUTER_TOKEN`, `--log-level`).
Global path flags are pushed into the process env **before** `Config::load()`, so env stays the
single resolution mechanism. `--json` is **per-subcommand**, never global, and prints
`serde_json::to_string_pretty` of the protocol type and nothing else on stdout. No colour crate. No
emoji. `fn main() -> anyhow::Result<()>`; `bail!` → anyhow prints `Error: …` to stderr and exits 1;
no `std::process::exit`. Tracing always to stderr (mandatory: `apexrouter mcp` shares the binary and
owns stdout).

```
apexrouter serve [--foreground] [--bind] [--proxy-bind]
apexrouter status [--json] [--watch]
apexrouter doctor [--json] [--fix]                     preconditions + one-line fixes
apexrouter version

apexrouter up <model|recipe|path> [--port] [--device] [--ctx] [--parallel] [--kv]
                                  [--build] [--alias] [--swap] [--json] [--yes]
apexrouter down <endpoint> [--now]
apexrouter endpoints ls|get <id>|start <id>|stop <id>|restart <id>|rm <id>|logs <id> [-f] [--tail N]
apexrouter route ls | set <endpoint> [--drain|--now|--start-then-swap] | alias set <name> <endpoint>
                | alias rm <name> | clear
apexrouter switch <provider> [--model]                 legacy-compatible alias of `route set`
apexrouter models ls [--json]                          the aggregated /v1/models view

apexrouter rig [--json]                                GPUs, builds, RAM, disk
apexrouter discover local|hf <query>|vast|together [--json]
apexrouter fit <model> [--device …] [--ctx] [--json]

apexrouter recipes ls|show <id>|new|edit <id>|rm <id>|validate [<id>]|import [--from <path>]
apexrouter vast account|offers [--profile]|rent <offer-id> --recipe <id> --yes|ls|logs <id>
                 |destroy <id> --yes|watch <id>|reconcile
apexrouter tunnel ls|up <instance>|down <id>|status

apexrouter usage [--since 24h] [--group-by provider|model|endpoint] [--json]
apexrouter smoke [--target <url|endpoint>] [--json]
apexrouter diagnose [--only <check>…] [--json]
apexrouter config init|show|path
apexrouter migrate [--dry-run|--apply]
apexrouter mcp [--proxy URL]
```

Human output: `key = value` lines and space-padded tables with an uppercase header row; missing
values render `-`; empty states get a friendly parenthetical. Every money-spending command requires
`--yes` or an interactive confirm. Exit codes are meaningful (0 ok, 1 error; `doctor` and `smoke`
exit 2 when a check fails so CI can gate on them).

---

## 10. MCP

Hand-rolled newline-delimited JSON-RPC 2.0 over stdio, copying `Prefrontal-RS/prefrontal-cli/src/mcp.rs`
(282 lines, no SDK, works with Claude Code today). Echo back the client's requested
`protocolVersion` rather than asserting one; tool failures are results with `isError: true`;
JSON-RPC error codes (`-32601`, `-32700`) are reserved for protocol breakage. One compact JSON
message per line (`to_string`, never `to_string_pretty`); **nothing but MCP messages on stdout**;
exit on stdin EOF. Dual-era hedge for the 2026-07-28 revision: keep the legacy handshake, add
`server/discover` advertising `supportedVersions`, accept-and-ignore per-request `_meta`, emit
`resultType: "complete"`. Streamable-HTTP is deferred; dispatch stays transport-agnostic
(`fn dispatch(method, params) -> Result<Value, RpcError>`) so an axum route is a day's work when
ApexOS-RS/RV nodes need it.

Names are prefixed `apexrouter_` — all MCP servers share `~/Projects/.mcp.json` and unprefixed names
collide. Descriptions are long and operational.

| Tool | Need | Notes |
|---|---|---|
| `apexrouter_status` | read | full snapshot: routes, endpoints, rig, fleet, credit |
| `apexrouter_models_list` | read | the aggregated model list an agent should target |
| `apexrouter_endpoints_list` | read | |
| `apexrouter_discover_local` | read | builds, devices, models with sizes |
| `apexrouter_discover_hf` | read | GGUF search + file sizes |
| `apexrouter_fit` | read | "will this fit?" — pure |
| `apexrouter_usage` | read | windowed cost/token summary |
| `apexrouter_logs` | read | tail an endpoint's log |
| `apexrouter_endpoint_start` | write | autostarts the daemon; returns when Ready or on deadline |
| `apexrouter_endpoint_stop` | write | `mode: drain\|now` |
| `apexrouter_route_set` | write | `mode: drain\|now\|start_then_swap` |
| `apexrouter_smoke` | write | the four probes against a target |
| `apexrouter_diagnose` | write | `only: [CheckId]` |
| `apexrouter_vast_offers` | read | live offer search |
| `apexrouter_vast_rent` | **money** | requires `confirm_spend_usd_per_hour` (no default) + `offer_id` |
| `apexrouter_vast_destroy` | **money** | requires `instance_id`; verifies before forgetting |

`.mcp.json` registration points at `target/release/apexrouter-mcp` — rebuild after surface changes
or agents run a stale binary.

---

## 11. GUI

Both GUIs are **edge clients of the same control API**. No second business-logic path — the Slint
crate never links `apexrouter-core`, only `apexrouter-protocol` + `apexrouter-client`, which also
keeps the GPL boundary clean.

### 11.1 Shared model

WebSocket first, REST fallback for first paint. `snapshot` on connect covers all gaps, so clients
need **zero replay logic**. Reconnect with exponential backoff 1 s → ×2 → cap 15 s. A single
connection dot is the one failure reporter. `setInterval(render, 60_000)` keeps relative timestamps
honest. Nothing in a draw path performs I/O — rendering takes a `&Snapshot` and nothing else, which
is the structural cure for LocalRouter's 21-second blocking status panel (07 F1).

### 11.2 Screens

| Screen | What the operator sees / does | Web | Slint mk1 |
|---|---|---|---|
| **Router** (home) | The hero card: default route → endpoint → provider → model → health dot → live tok/s, in-flight, req/min, error rate. Route switcher over ready endpoints with a drain/now/start-then-swap toggle. Copy-paste `OPENAI_BASE_URL=http://127.0.0.1:8888/v1`. Proxy uptime. | ✅ | ✅ |
| **Endpoints** | Scaling list (never a fixed-slot layout): id, kind, model, port, devices, status badge, VRAM, slots busy/total, tok/s, uptime. Per-row start/stop/restart/logs/make-default. Adopt button for `Foreign`. | ✅ | ✅ |
| **Rig** | GPUs enumerated (index, name, backend, total/free VRAM, driver). llama.cpp builds with detected devices and a "usable/broken" verdict. RAM, swap, disk. "What fits here" panel driven by `fit()`. | ✅ | ✅ |
| **Discover** | Three tabs. *Local*: scanned models with sizes, shard groups, fit verdict. *HuggingFace*: search, file list with authoritative sizes, gated badge, fit verdict per quant. *Vast*: live sortable/filterable offer table with credit + burn-down header. Every row has **Build endpoint from this**. | ✅ | Local tab only |
| **Draft** | The "dynamic recipe building" the brief asks for. Left: fields derived by `fit()` (ctx, parallel, kv type, ngl, devices, mode) each with an override and a `why` tooltip showing the arithmetic. Right: **live preview of the exact argv / container env** that will be produced. Buttons: Launch now · Save as recipe · Both. | ✅ | Local-launch subset |
| **Vast** | Credit + burn-down. Fleet table with $/hr, accrued, geo, uptime, `BootPhase`, and a **Destroy button that is always visible**. Boot view: phase timeline + streaming log + elapsed. Stall banner with one-click restart. Orphan reconciliation notices. | ✅ | read-only |
| **Usage** | Window selector (1h/24h/7d/all), per-provider and per-model table, sparkline, grand total, `Metered` vs `Approximate` badges so a guess never renders as a fact. | ✅ | read-only |
| **Logs** | Follow-mode tail per endpoint with a filter box. Not a static 2000-char dump. | ✅ | ✅ |
| **Checks** | Check registry with checkboxes, run concurrently, results stream in with pass/fail badges + timings. Smoke test presented as four named probes with TTFT and tok/s. | ✅ | deferred |
| **Settings** | Providers (base URL + credential **source**, never the value), ports, autostart, compat toggles, discovery roots, Vast profiles. | ✅ | deferred |

The main-menu redesign from the inventory (Serve / Connect / Inspect / Catalog) maps onto:
Router+Endpoints (Serve) · Vast+Discover (Connect) · Rig+Usage+Logs+Checks (Inspect) · Draft+Recipes
(Catalog). There is no "Exit" item, no `press_enter()` gate, no screen-clearing, and no `← Back` that
aborts a whole wizard — the Draft screen is a persistent form, so there is nothing to abort.

### 11.3 Web UI

`ui-web/{index.html,app.js,style.css}` — no npm, no CDN, no framework, no build step. Plain
`<script src="app.js">` (not a module), `"use strict"`, module-level mutable state,
`el(tag,cls,text)` + `$(id)` helpers, `container.replaceChildren()` re-render, `textContent`
everywhere. Inline SVG emoji favicon as a `data:` URI. Static HTML skeleton of empty ids with
`<!-- injected -->` comments; `<details>` drawers; a fixed `.overlay/.panel` modal with
`role="dialog"` for the two confirmations that matter (rent, destroy).

CSS: `:root` token block with `color-scheme: dark`, then `@media (prefers-color-scheme: light)`
redefining the same variables. `--page/--surface/--ink/--ink-2/--muted/--hairline/--border`; fixed
never-themed `--st-good/warning/serious/critical` reserved **for health only, never identity**;
badges always pair icon + label, never colour alone. System sans body text at 14px/1.45; monospace
strictly for code, argv previews and log panes. Stat tiles `auto-fit minmax(150px,1fr)`, cards
`auto-fill minmax(290px,1fr)`, 8px radii, one 700px breakpoint. `[hidden]{display:none}` guards on
every element that toggles `hidden` and declares its own `display` (filed house sharp edge). UI
changes are render-tested — curl is not enough.

Embedded with `rust-embed` pointing straight at `../../ui-web` (no `dist/`), with `server.ui_dir` as
a live-reload escape hatch.

### 11.4 Slint

`build.rs` is one `slint_build::compile("src/ui/appwindow.slint")` line. **Never `#[tokio::main]`** —
Slint owns the main thread; a manually built multi-thread runtime is kept alive for the app lifetime
and does all I/O; every UI touch crosses back via `Weak` + `invoke_from_event_loop` /
`upgrade_in_event_loop`. Callbacks are wired in braced blocks capturing `ui.as_weak()` +
`rt.handle().clone()`; all fallible async work goes in one inner `async { … anyhow::Ok(v) }.await`
so a single `match` handles every failure. Models via `ModelRc::new(VecModel::from(rows))`.
`export global Palette` + `export enum Theme` with the exact hex values matching the web tokens
(`#0d0d0d` page, `#1a1a19` surface, `#2c2c2a` hairline, `#ffffff` ink, `#c3c2b7` ink-2, `#898781`
muted, `#3987e5` accent, `#0ca30c` good, `#fab219` warn, `#ec835a` serious, `#d03b3b` critical) —
every component reads from the palette; nothing hardcodes a colour or radius. Crate is
`GPL-3.0-only`, `publish = false`, out of `default-members` and out of the CI `-p` lists, with the
caveat stated plainly in the README License section. `~200-line src/api.rs` `NodeClient` with a
private `auth()` builder wrapper, 300 s timeout, manual status/text checks before parsing.

---

## 12. mk1 scope

The end-to-end acceptance test, on this laptop, with the only real model on it:

```
apexrouter up ~/models/carnice-9b/Carnice-9b-Q6_K.gguf --yes
curl -s http://127.0.0.1:8888/v1/models | head
curl -s http://127.0.0.1:8888/v1/chat/completions -d '{"model":"x","messages":[…],"stream":true}'
apexrouter status --json | jq .routes
systemctl --user restart apexrouterd     # or kill -TERM; the model stays hot
apexrouter status                         # endpoint re-adopted, route intact, no reload
apexrouter smoke                          # 4 probes green, tok/s from the timings object
# both GUIs show the endpoint, its VRAM, its tok/s, and can stop/start it
```

### Ships in mk1
1. Workspace, protocol crate, core (paths/config/Secret/atomic store/lockfile/ledger/usage).
2. `apexrouterd`: lock, reconcile-before-bind, control API + WS, proxy, graceful drain.
3. Local llama.cpp end to end: glob discovery, `--list-devices` backends (llvmpipe excluded),
   recursive+symlink model scan with shard grouping, feature-detected argv, `setsid` +
   `LD_LIBRARY_PATH` spawn, real health deadline with kill-on-failure, log rotation, identity-checked
   stop, adoption across daemon restart, port pre-check.
4. Proxy: `ArcSwap` table, both `/v1` forms, byte-compatible `/health` `/providers` `/switch`
   (SSRF-hardened), streaming relay with split timeouts, OpenAI-shaped errors, one client,
   constructed outbound headers, usage tee → `usage.jsonl` + legacy mirror, `X-Provider`/`X-Usage`,
   **model aliasing**, **aggregated `/v1/models`**, ordered fallback, drain + `start_then_swap`,
   loop guard, explicit CORS allowlist.
5. `fit()` with GGUF header parsing + unit tests; `ArgvPreview` and `ContainerEnvPreview`.
6. Catalog: typed recipes in `catalog.toml` via `toml_edit`, `TryFrom<RecipeDraft>` as the only
   constructor, duplicate-id error at load, staleness validation, **read-only import** of
   `recipes.toml`.
7. Vast: verified REST client, offer search with one unified threshold set + explicit widening
   banner, profiles as config (2–4× 3090, 1–2× H100), ledger + `PendingLaunch` guard +
   `SpendApproval`, `BootPhase` watcher with `max_boot_secs` auto-destroy, startup reconciliation,
   `ssh` tunnel supervision holding the `Child`, stall check with one-click restart.
8. Together + `RemoteOpenAi`: live model catalogue with pricing, credential-by-reference,
   429 handling, connection + completion probes.
9. CLI: the full verb table in §9, `--json` everywhere with `served_by`, offline/autostart policy.
10. MCP: 16 tools, dual-era handshake, `isError` convention, Local/Proxy backends.
11. Web UI: Router, Endpoints, Rig, Discover (Local + HF + Vast), Draft, Vast, Usage, Logs, Checks,
    Settings.
12. Slint: Router, Endpoints, Rig, Logs, local Discover + Launch; Vast/Usage read-only.
13. Checks registry: doctor + diagnose + the four native smoke probes, run concurrently.
14. `apexrouter migrate` from `~/.vastai-gguf` + the LocalRouter checkout + `recipes.toml`.
15. Docs: README with banner, `CLAUDE.md`, `docs/{CHARTER,API,ARCHITECTURE,SLINT,AGENTS}.md`,
    `config.example.toml`, `openapi/apexrouter-v1.yaml`, `skills/apexrouter/SKILL.md`, CI
    (fmt/clippy `-D warnings`/test/build, Slint excluded).

### Deliberately deferred past mk1 (each named because the inventory marks it `yes` or `redesign`)
- **Local vLLM launch.** `EndpointKind::LocalVllm` exists in the type system and validates; there is
  no spawner. Vast-side vLLM *is* shipped (same create call, different image + env).
- **Writing back to `recipes.toml`.** Import is one-way. The legacy file is never rewritten, so the
  comment-destruction problem cannot recur; `toml_edit` is used for `catalog.toml`.
- **Batch compare.** The primitive lands in mk1 (multi-route addressing via aliases + a parallel
  client), the fan-out UI does not. Post-mk1, it is a screen, not an engine.
- **Local HF downloads.** mk1 discovers and sizes; it does not download weights to `~/models`.
  Container-side download is unchanged (`hf download` inside `launch.sh`).
- **llama.cpp router mode** (`--models-dir`, `POST /models/load`). b9199 already has it and it
  overlaps our supervision job. mk1 keeps direct single-model supervision (it matches the existing
  state model and the failure modes we understand); router mode is the documented mk2 simplification.
- **Anthropic `/v1/messages` translation.** mk1 relays it transparently (works iff the backend
  speaks it). Translation is an open question in `docs/CHARTER.md`.
- **MCP streamable-HTTP** and the deprecated HTTP+SSE transport. Never the latter.
- **LAN node discovery.** `RemoteOpenAi` lets you add a node by URL today; mDNS/registry is mk2.
- **Weighted load balancing, circuit breakers, sticky sessions.** mk1 ships ordered first-healthy
  fallback only.
- **Request-time budget enforcement.** Usage and cost are recorded and displayed; nothing blocks a
  request on spend yet.
- **SQLite.** Files + JSONL for everything a human might `cat` or a script might `tail`. Add
  `rusqlite` only when usage aggregation genuinely needs SQL, and then copy the house
  `Mutex<Connection>` + `migrate()` + terminal-guard pattern verbatim.
- **`keyring` crate.** 0600 files in mk1; keyring behind a feature flag later.
- **A TUI.** Declined outright, not deferred (§1.4).

---

## 13. Risks

1. **Scope.** Eight crates plus two GUIs is a lot for an mk1. Mitigation: the acceptance test in §12
   only exercises local llama.cpp + proxy + both GUIs; Vast can be stubbed to read-only for the
   first tag without changing any type.
2. **Money.** Vast bills from creation for storage and from `running` for GPU, and `$7.73` of credit
   is 2.3 hours of 2×H100. The `PendingLaunch` guard, the ledger, the reconciliation pass, the
   watchdog and the mandatory `SpendApproval` are all load-bearing. Test the Drop-guard path with a
   deliberately killed process.
3. **Drop-in regression.** The `/v1` doubling normalisation is the single highest-risk compatibility
   change. `smoke.sh` must pass against both `http://127.0.0.1:8888` and `.../v1` in CI as a fixture
   test before the first tag.
4. **`/proc`-based process identity is Linux-only.** Fine for ApexOS and this box; `ProcIdentity`
   needs a `sysinfo`-backed fallback (weaker: pid + start-time only) if macOS ever matters. State it
   in the charter rather than pretending it is portable.
5. **Adoption vs foreign processes.** Re-adopting by (pid, start_time, exe) is sound, but a
   `llama-server` started by hand on port 8100 is `Foreign` and will *not* be routed to until the
   operator presses Adopt. That is the correct conservative behaviour and it will surprise someone.
6. **Autostart races and surprise daemons.** A `--json` script that accidentally triggers autostart
   now has a background process it did not ask for. Mitigations: `served_by` on every envelope,
   `--no-autostart`, and autostart is never triggered by a `Pure` or `ReadState` command.
7. **llama.cpp router mode overlap.** If upstream's `--models-dir` supervision becomes the obvious
   answer, part of `supervisor.rs` becomes redundant. Deliberate, documented, revisit at mk2.
8. **MCP spec churn.** The 2026-07-28 revision is two days old and nothing speaks it. The dual-era
   hedge is cheap but it is a hedge; if the ecosystem moves, `apexrouter-mcp` needs a real
   `server/discover` implementation and a `resultType` audit.
9. **Vast wire-format landmines.** `new_contract` not `id`; `env` encoding Docker flags as map
   *keys*; `runtype` as `"ssh_direc ssh_proxy"` (missing `t`); `ports` as a Docker map in practice
   but `int[]` per the docs. Every one of these needs a tolerant accessor and a recorded fixture.
10. **Slint build deps.** `libfontconfig1-dev` is required and CI never builds that crate; a broken
    Slint app can therefore land green. Add a manual `cargo build -p apexrouter-slint` gate before
    tagging.
11. **`arc_swap` is a new dependency for the garden.** If a fifth crate is unwelcome, `tokio::sync::watch<Arc<RouteTable>>`
    gives the same atomicity with `borrow()` per request — slightly more expensive, zero new deps.
    Decide in the charter.
12. **The compat mirrors are two writers again, briefly.** While `compat.legacy_active_endpoint` is
    on, the Python TUI can still write that file behind us. Documented as migration-only, atomic on
    our side, and default-off once `migrate --apply` has run.

---

## 14. Rationale

A router is a *stateful* product wearing a stateless costume. The prompt in flight, the model that
took ninety seconds to load, the GPU that started billing four minutes ago, the SSH tunnel, the token
ledger — all of it is live state with real consequences, and LocalRouter tried to keep it in a
handful of files that five programs edited by hand. Every serious bug in the inventory is a
consequence of that, not of Python.

So the first architectural decision is not a crate list, it is: **name an owner.** `apexrouterd`
holds an exclusive lock for its lifetime, and that single fact converts a family of race conditions
into non-questions. There is one writer of state, so `.active_endpoint` cannot have four schemas.
There is one resolver, so the proxy and the CLI cannot disagree about what is active. There is one
holder of every `Child`, so a health-check timeout cannot leave a running orphan behind — the
component that started the process is the component responsible for cleaning it up, and it is still
alive to do so.

The lock also answers the objection that a daemon is inconvenient. Because "is there an owner?" is
one syscall, the CLI can be honest instead of hopeful: read commands serve from disk and stamp
`served_by: "offline"`; write commands autostart the daemon; the one genuinely offline mutation takes
the exclusive lock first and therefore *cannot* be racing anyone. No heuristics, no lock files that
outlive their process, no "probably fine".

Children outliving the daemon is the second decision, and it is the one that separates an inference
manager from a process babysitter. A model server is expensive to start and cheap to re-attach.
`setsid` plus a persisted `(pid, start_time, exe)` identity means you can upgrade `apexrouterd`
without evicting a 30 GB model, and PID reuse is *detected* rather than merely survived. Status is
computed from liveness plus health, never read from a `status: "running"` string that went stale the
moment someone typed `kill`.

The third decision is that the routing table is memory, not a file. LocalRouter re-parses JSON — and
sometimes a whole TOML file — on every proxied request, and can read that file mid-truncation and
silently reroute your traffic. One `ArcSwap` pointer load replaces all of it, and the swap becomes
expressive rather than merely fast: drain before you stop, start the replacement before you retire
the incumbent, fall back to a healthy peer when the tunnel drops. `start_then_swap` is a genuine
product feature that the file-as-a-variable design cannot even describe.

Everything else follows from "one API, many thin clients". The fit solver is a pure function, so it
is identical in the CLI, the MCP tool, the web draft editor and the Slint app. The Slint app links
only the protocol and client crates, which keeps the GPL boundary clean *and* structurally prevents
a second business-logic path. The 71-recipe catalogue collapses into discovery plus that one
function, because a table of hand-solved VRAM arithmetic is a frozen function pretending to be data —
and it is already wrong about two of its three local models.
