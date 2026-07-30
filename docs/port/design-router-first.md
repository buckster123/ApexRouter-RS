# ApexRouter-RS — architecture proposal: **the router is the product**

> Design lens: the most valuable thing ApexRouter can be is a genuinely good OpenAI-compatible
> router. Named endpoints, model aliases, aggregated `/v1/models`, routing rules, retries and
> failover, streaming passthrough, per-request telemetry, concurrency limits. Lifecycle
> management (local spawn, Vast spin-up) exists to *populate the routing table*, and is designed
> around the request path rather than the other way round.
>
> One sentence for the README: **point every agent at `http://127.0.0.1:8888/v1` and never change
> it again.**

Status: proposal. Companion to `docs/port/01`–`09`. Where this disagrees with the port specs it is
a deliberate redesign and says so.

---

## 0. The thesis, and what falls out of it

LocalRouter's proxy is 417 lines that read a JSON file per request and forward bytes to whatever it
says. Spec `05-proxy.md` §14 lists sixteen things a serious router needs and marks all sixteen
absent. That table *is* the product backlog, and everything else in LocalRouter — 14 TUI menus, 71
recipes, 19 GPU tiers — exists to eventually set one string in one file.

Invert it. The routing table is the primary data structure. A `Backend` is a live
OpenAI-compatible upstream; a `ModelRoute` is an alias plus a target chain plus a policy;
everything ApexRouter does — discovering a GGUF, spawning `llama-server`, renting a 3090, saving a
Together key — is a way to *add a Backend and bind an alias to it*. That framing collapses the
whole system:

| LocalRouter concept | ApexRouter concept |
|---|---|
| `.active_endpoint` (one global target) | a routing table with N aliases → N backends |
| `POST /switch` | `PUT /api/v1/routes/{alias}` (and `/switch` kept as a legacy shim) |
| 71 recipes in `recipes.toml` | discovery + a saved `LaunchPreset` that *produces a backend* |
| 19 GPU tiers | 5 `SearchProfile`s (query templates over the live Vast market) |
| "Pin" files between TUI screens | there are no screens to pass data between; there is one table |
| Batch compare | fan-out across aliases (one alias, N targets, `mirror` strategy) — mk2 |
| Watch boot | a `BootPhase` state machine feeding the same WS event stream as everything else |

Consequences that shape the crates:

1. **The request path never touches the filesystem.** It reads an `ArcSwap<RoutingTable>` and
   nothing else. All config parsing happens on reload, off the hot path.
2. **One resolver.** Spec `07` D4 flags that `resolve_target()` and `get_active_endpoint()` answer
   the same question two different ways with two different config parsers and two different
   fallbacks. In ApexRouter there is exactly one `resolve()` and every surface calls it.
3. **Model aliasing is the headline feature, not a nice-to-have.** `05` §14 item 4 calls it the
   #1 drop-in papercut: switching provider today silently breaks every client because the `model`
   string goes upstream verbatim. Aliases fix it permanently and are what let an agent hardcode
   one base URL *and* one model name forever.
4. **Lifecycle is a `Provisioner` that returns a `BackendId`.** Local spawn, Vast rental and
   "register this LAN URL" are the same shape. Slint and the web UI drive one API for all three.

---

## 1. Workspace layout

Cargo workspace, resolver 2, edition 2021, `rust-version = "1.75"`, version `0.1.0`,
`license = "MIT OR Apache-2.0"`, `repository = "https://github.com/buckster123/ApexRouter-RS"`,
`authors = ["Andre <buckster123>"]`. `[profile.release]` = `lto = "thin"`, `codegen-units = 1`,
`strip = true`. `rustfmt.toml` present containing only a comment. Internal deps carry both `path`
and `version`; members use `dep.workspace = true`.

```
ApexRouter-RS/
├── Cargo.toml                 # members + default-members (slint excluded)
├── rustfmt.toml
├── config.example.toml
├── routes.example.toml
├── assets/banner.png          # Imaginarium-generated, credited in the README footer
├── ui-web/{index.html,app.js,style.css}     # no npm, no CDN, no build step
├── openapi/apexrouter-v1.yaml
├── docs/{CHARTER,API,ARCHITECTURE,SLINT,AGENTS,ROUTING,LICENSING}.md + docs/port/*
├── skills/apexrouter/SKILL.md
└── crates/
    ├── apexrouter-protocol    # serde-only wire types — every surface shares these
    ├── apexrouter-core        # config, paths, secrets, state, pricing, usage, discovery, fit
    ├── apexrouter-router      # THE REQUEST PATH — table, policy, relay, retries, telemetry
    ├── apexrouter-providers   # local llama.cpp, vast.ai, together, hf, plain LAN node
    ├── apexrouter-server      # axum app: /v1 data plane + /api/v1 control plane + /ws + UI
    ├── apexrouter-cli         # clap CLI, bin `apexrouter` (default-run), `serve`, `mcp`
    ├── apexrouter-mcp         # stdio JSON-RPC MCP server, bin `apexrouter-mcp`
    └── apexrouter-slint       # GPL-3.0-only native app, bin `apexrouter-ui`, publish = false
```

`default-members` = everything except `apexrouter-slint`. CI runs
`cargo clippy -p apexrouter-{protocol,core,router,providers,server,cli,mcp} -- -D warnings`, so the
root build never needs `libfontconfig1-dev`.

**Dependency pins** (house-consistent): axum 0.8 `["ws"]`, tower 0.5, tower-http 0.7
`["trace","fs"]`, **reqwest 0.12** `default-features = false` + `["json","rustls-tls","stream"]`,
clap 4 `["derive","env"]`, tokio 1 with an explicit feature list, tracing + tracing-subscriber
`["env-filter"]`, serde/serde_json, toml 0.8, **toml_edit 0.22** (comment-preserving writes),
directories 5, rust-embed 8, notify 6, arc-swap 1, thiserror 2, anyhow 1, async-trait 0.1,
futures-util 0.3, chrono 0.4, ulid 1, slint 1 + slint-build 1, tokio-tungstenite 0.24,
tempfile 3 (dev).

Two deviations, both deliberate:

- **`arc-swap`** is new to the garden. The routing table is read on every single proxied request
  and written rarely; `ArcSwap<RoutingTable>` makes reads a pointer load with no lock. This is the
  one place in the workspace where that matters.
- **reqwest stays at 0.12**, not 0.13. `09-external-apis.md` flags the crate trap (hf-hub 1.0
  needs 0.13, reqwest-eventsource 0.6 needs 0.12). We use neither: HF is ~6 hand-rolled calls, and
  SSE is relayed as bytes rather than parsed. 0.12 matches Prefrontal-RS and Imaginarium-RS.

No `mime_guess`, no colour crate, no OpenAI SDK crate (typing the schema would silently drop
provider extensions like `top_k`, `repetition_penalty`, `timings_per_token`).

---

## 2. Crate by crate

### 2.1 `apexrouter-protocol` — the shared vocabulary

Serde only, zero I/O deps. Every surface (daemon, CLI, MCP, web UI via JSON, Slint) deserializes
the *same* enums the daemon serializes. `#[serde(rename_all = "snake_case")]`, `#[serde(tag =
"type")]` on `Event`, `#[derive(PartialEq)]` everywhere so the daemon can suppress no-op
broadcasts, `#[serde(default)]` on additive `Vec` fields.

```rust
// ids.rs — validated newtypes; an empty id is not constructible
pub struct BackendId(String);   // slug: [a-z0-9][a-z0-9._-]*
pub struct Alias(String);       // the string a client puts in "model"
pub struct InstanceId(u64);     // Vast contract id
pub struct RequestId(Ulid);

// backend.rs
pub enum BackendKind { LocalLlama, VastLlama, VastVllm, Together, Node }
pub struct Backend {
    pub id: BackendId,
    pub kind: BackendKind,
    pub label: String,
    pub base_url: String,          // ALWAYS without a trailing /v1 — see §4.1
    pub credential: CredentialRef, // never key material
    pub tags: Vec<String>,         // "local","tools","vision","cheap","fast","gpu:vulkan"
    pub models: Vec<UpstreamModel>,
    pub limits: BackendLimits,     // max_concurrent, queue_depth, ctx
    pub price: Option<PriceModel>, // PerToken { in, out } | PerHour { dph } | Free
    pub health: Health,
    pub provenance: Provenance,    // Discovered | Spawned | Rented | Manual | Imported
    pub enabled: bool,
}
pub enum Health { Unknown, Starting { phase: BootPhase }, Ready { since_unix, slots_free },
                  Degraded { reason: String }, Down { reason: String, retry_at_unix: i64 },
                  Draining }
pub enum BootPhase { Provisioning, Pulling, Compiling, Downloading { pct: Option<f32> },
                     Loading, Healthy, Failed { reason: String } }

// route.rs — the product
pub struct ModelRoute {
    pub alias: Alias,
    pub targets: Vec<RouteTarget>,     // ordered
    pub strategy: Strategy,
    pub filter: RouteFilter,
    pub retry: RetryPolicy,
    pub sticky: Option<StickyKey>,
    pub description: Option<String>,
}
pub struct RouteTarget { pub backend: BackendSelector, pub model: Option<String>, pub weight: u32 }
pub enum BackendSelector { Id(BackendId), Tag(String), Glob(String) }  // "vast-*"
pub enum Strategy { FirstHealthy, Cheapest, Fastest, LeastBusy, RoundRobin, Mirror }
pub struct RouteFilter { pub require_tags: Vec<String>, pub exclude_tags: Vec<String>,
                         pub max_cost_per_mtok: Option<f64>, pub min_ctx: Option<u32> }
pub struct RetryPolicy { pub attempts: u8, pub failover: bool, pub honor_retry_after: bool }

// telemetry.rs
pub struct Money(i64);                 // micro-USD; no float dust
pub enum Estimate<T> { Metered(T), Approximate(T) }
pub enum TokenCount { Reported(u32), Estimated(u32) }
pub struct RequestRecord {
    pub id: RequestId, pub started_unix: i64, pub alias: Option<Alias>,
    pub backend: Option<BackendId>, pub upstream_model: Option<String>,
    pub route_reason: RouteReason,      // Alias | ExplicitPin | UpstreamIdMatch | DefaultFallback
    pub status: u16, pub attempts: u8, pub streamed: bool,
    pub ttft_ms: Option<u32>, pub total_ms: u32,
    pub prompt_tokens: Option<TokenCount>, pub completion_tokens: Option<TokenCount>,
    pub tok_per_s: Option<f32>,          // llama.cpp timings.predicted_per_second
    pub cost: Option<Estimate<Money>>, pub error: Option<String>,
}

// event.rs — the WS protocol
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Snapshot(Box<Snapshot>),            // boxed: the one oversized variant
    BackendChanged { backend: Backend },
    BackendRemoved { id: BackendId },
    RouteTableChanged { routes: Vec<ModelRoute>, valid: bool, error: Option<String> },
    RequestStarted { id: RequestId, alias: Option<Alias>, backend: Option<BackendId> },
    RequestFinished { record: RequestRecord },
    InstanceChanged { instance: VastInstance },
    BootPhaseChanged { id: InstanceId, phase: BootPhase, line: Option<String> },
    LogLine { source: LogSource, line: String },
    UsageTick { window: UsageWindow },   // coalesced to 1 Hz
    Alert { level: AlertLevel, message: String, action: Option<String> },
}
```

Also here: `Snapshot`, `UsageRecord` (the legacy-compatible JSONL row), `Offer`, `VastInstance`,
`SearchProfile`, `LaunchDraft`/`LaunchPreset`, `LocalInstance`, `LlamaBuild`, `Device`, `GgufModel`,
`FitEstimate`, `CheckResult`.

### 2.2 `apexrouter-core` — config, state, discovery, arithmetic

No HTTP server, no axum. Modules:

- **`paths.rs`** — one `Paths` struct resolved once at startup. `$APEXROUTER_CONFIG` →
  `$APEXROUTER_HOME/config.toml` → XDG config dir; state at `$APEXROUTER_HOME` → XDG state dir.
  Named accessors: `config_file()`, `routes_file()`, `backends_file()`, `ledger()`, `usage_log()`,
  `logs_dir()`, `known_hosts()`, `lock_file()`, plus `legacy` sub-struct pointing at
  `~/.vastai-gguf/*` and the LocalRouter checkout. **Nothing takes a relative path; nothing is ever
  written into a repo directory.**
- **`config.rs`** — `Config` with per-section structs, every field defaulted (missing file = a
  working zero-config setup), runtime-only fields `#[serde(skip)]`, a separate `ConfigFile` for
  writing. `load()`, `load_from()`, `init_file()`, `save()`.
- **`secret.rs`** — `Secret<String>` with redacting `Debug`/`Display` and a single `expose()`.
  `CredentialRef { Env(String) | File(PathBuf) | Inline(Secret<String>) | None }` and
  `resolve(&self) -> Result<Option<Secret<String>>>` implementing the house chain: explicit config
  value → ApexRouter config → conventional third-party path (`~/.config/vastai/vast_api_key`,
  `~/.cache/huggingface/token`, `~/.vastai-gguf/config.toml`) → env var. **A borrowed credential is
  never copied into our config file** — we persist the `CredentialRef`, not the key.
- **`store.rs`** — atomic writes (tmp + `fs::rename` in the same dir, `0600` set at `OpenOptions`
  time), `flock` on the state dir, `Ledger` (append-only JSONL with `O_APPEND` single-`write`
  semantics), `PendingLaunch` two-phase-commit guard with a `Drop` impl that writes an
  `orphan-suspect` row.
- **`migrate.rs`** — reads all four `.active_endpoint` shapes (serde aliases for
  `activated_at`/`switched_at`), `.last_instance`, `.instance_history`, `.hf_pin`,
  `.pinned_provider`, `local_instances/*.json`, `~/.vastai-gguf/config.toml`, and optionally
  `recipes.toml`, and emits backends + routes + `LaunchPreset`s. Every imported path is validated;
  stale is the normal case, not an error.
- **`pricing.rs`** — `PriceTable` from (a) live provider `/v1/models` pricing, (b) live Vast
  `dph_total`, (c) a `pricing.toml` fallback. Every number carries `Estimate::{Metered,
  Approximate}` so the UI can never present a guess as a fact.
- **`usage.rs`** — append + aggregate with real time windows (`--since 24h`), rotation at a size
  cap, permissive deserialize (optional legacy `epoch`, unknown fields ignored, never fail a row).
  Reads both the new state-dir log and `~/.vastai-gguf/usage.log`.
- **`discovery/`** — `builds.rs` globs `~/llama.cpp/build*/bin/llama-server` plus configured roots
  and `$PATH`, labels by build-dir name, and probes each with `--list-devices` (never by grepping
  `--help` for "vulkan" — measured broken) plus a `--help` scrape into a `FlagSet` for
  feature-detection. `devices.rs` enumerates `Vulkan0/CUDA0/…` per build and excludes `llvmpipe`.
  `models.rs` walks configured model dirs recursively, skips `.cache`/`mmproj`/`vocab` by filename
  token (not substring), groups `-00001-of-000NN` shards into one logical model with a summed size,
  and reads the GGUF header for `n_layer`/`n_head_kv`/`n_embd_head`/`n_ctx_train`.
- **`fit.rs`** — `fn fit(model: &GgufModel, budget: &VramBudget, want: &FitRequest) ->
  FitEstimate`. Pure, unit-tested, replaces the 54 hand-solved recipe `description` strings. Powers
  the launch UI's ctx/parallel/kv suggestions, `apexrouter fit`, and the `apexrouter_fit` MCP tool.
- **`proc.rs`** — `exec::run(program, argv: &[&OsStr], timeout)` returning
  `Result<Output, ExecError>` with **stdout and stderr as separate fields and no `2>&1` codepath in
  the API**. `ProcessHandle { pid, start_time_ticks }` and `Liveness { Alive, Dead,
  Unknown(io::Error) }` so `EPERM` is a value, not a panic. A CI grep bans `"sh", "-c"`.
- **`error.rs`** — thiserror `Error` + `pub type Result<T>`.

### 2.3 `apexrouter-router` — the heart

This crate is the product. It owns the request path and knows nothing about how backends came to
exist.

```
lib.rs        Router = Arc<RouterInner>; pub fn data_plane() -> axum::Router<RouterState>
table.rs      RoutingTable + TableBuilder::compile(&Config, &Registry) -> Result<RoutingTable>
resolve.rs    resolve(model: &str, class: RequestClass) -> Result<Plan, RouteError>
policy.rs     Strategy impls, score(), sticky hashing
registry.rs   BackendRegistry: add/update/remove/enable/drain; owns LiveBackend state
health.rs     background prober; /health + /props (llama.cpp), /v1/models (managed)
upstream.rs   the shared reqwest::Client(s); Upstream::send(&Candidate, OutboundRequest)
relay/headers.rs   outbound_headers(inbound, cred) -> HeaderMap  (CONSTRUCTED, never cloned)
relay/body.rs      BodyPlan::{Passthrough(Bytes), Rewritten(Bytes)}; RequestPeek
relay/stream.rs    SSE byte relay + usage/timings tee + idle timeout + abort-on-disconnect
attempt.rs    PreFlight -> Committed state machine; the retry/failover loop
breaker.rs    per-backend circuit breaker (Closed / Open{until} / HalfOpen)
limits.rs     per-backend Semaphore, global inflight cap, queue timeout, Retry-After
models.rs     /v1/models aggregation + per-backend model cache with TTL
errors.rs     OpenAI-shaped error envelope + status mapping
telemetry.rs  RequestRecord assembly, ring buffer, broadcast, usage-log write, /metrics
compat.rs     legacy /health, /providers, POST /switch, .active_endpoint bridge, /v1 collapse
```

`RouterInner` holds `ArcSwap<RoutingTable>`, the `reqwest::Client`s, the semaphores, the ring
buffer, the `broadcast::Sender<Event>`. The table is rebuilt (never mutated) on config reload or
registry change, validated, then `store`d — a bad edit can never take the router down.

### 2.4 `apexrouter-providers` — how backends come to exist

```rust
#[async_trait]
pub trait Provisioner: Send + Sync {
    fn kind(&self) -> BackendKind;
    async fn plan(&self, draft: &LaunchDraft) -> Result<LaunchPlan>;   // cost + fit + warnings
    async fn up(&self, plan: LaunchPlan, approval: SpendApproval) -> Result<Backend>;
    async fn down(&self, id: &BackendId, mode: DownMode) -> Result<()>;
    async fn logs(&self, id: &BackendId, tail: usize) -> Result<LogStream>;
    async fn reconcile(&self) -> Result<Vec<Backend>>;                 // startup adoption
}
```

`SpendApproval { max_hourly_usd, confirmed_at, source }` is a value you cannot fabricate implicitly
— it must be threaded from an explicit confirmation in whatever surface asked. There is no path to
a billing call without one (kills `07` A4).

- **`local/`** — `args.rs` is **one** argv builder serving both the local spawn and the container
  `launch.sh` env contract, with the sampling presets unified (`--top-k 20` included; `launch.sh`
  is authoritative). Flags are emitted only if present in the probed `FlagSet`; `--jinja` is
  skipped on builds where it is already default-on; `-fa` is emitted as `on|off|auto`; `--ctx-size`
  and `-ngl` are **left unset** when the draft doesn't specify them so llama.cpp's `--fit` can do
  its job. `supervisor.rs` spawns with `LD_LIBRARY_PATH=<dirname(binary)>` (the trailing-colon
  RUNPATH trap), `setsid`, an owned log `File` (RAII, no fd leak), a **port bind-probe first**, a
  real total deadline on the health gate, and **cleanup on failure** — kill the child, remove the
  pidfile, mark the backend `Down`, never leave an orphan. `reconcile()` adopts children whose
  `{pid, start_time_ticks}` still match.
- **`vast/`** — `api.rs` speaks REST over reqwest+rustls (`PUT /search/asks/`, `PUT /asks/{id}/`,
  `GET /instances/`, `DELETE /instances/{id}/`, `PUT /instances/request_logs/{id}/` with the
  two-phase `result_url` poll, `GET /users/current/` for credit). Offers deserialize into ~25 typed
  fields plus `#[serde(flatten)] extra: Map<String, Value>` so nothing is lost. `offers.rs` holds
  the five `SearchProfile`s and one unified search path — the "auto cheapest" bug (browser and
  `vast_up.sh` searching different candidate sets) dies because there is only one query builder.
  `launch.rs` does reserve → create → commit through `PendingLaunch`. `tunnel.rs` owns the `ssh`
  child (`Child::id()`, never `pgrep`), passes `-o ExitOnForwardFailure=yes -o
  ServerAliveInterval=30 -o ServerAliveCountMax=3`, uses a per-instance `ControlPath` and a
  dedicated `UserKnownHostsFile` with `StrictHostKeyChecking=accept-new`, and allocates the local
  port from a pool starting at 8800. `boot.rs` drives the `BootPhase` machine off the REST log
  stream with a `max_boot_secs` watchdog that auto-destroys.
- **`together.rs`** — live `/v1/models` (a **bare array**, unlike llama.cpp's `{"object":"list"}`
  envelope — two deserializers, deliberately), pricing straight off each model object, 429 handling
  reading `x-ratelimit-reset`, `finish_reason` always `String` (Together emits `eos`).
- **`hf.rs`** — `GET /api/models?filter=gguf&search=` for search, `POST
  /api/models/{ns}/{repo}/paths-info/{rev}` for authoritative sizes, gated-repo classification on
  (status, header-if-present, body) with an anonymous retry to distinguish a bad token, and the
  quant regex `(UD-Q\d+[^.\s_-]*|Q\d+_K_[A-Z]+|Q\d+_\d+)`.
- **`node.rs`** — "here is an OpenAI-compatible URL, treat it as a backend". This is how a locally
  run vLLM, a LAN box, or an ApexOS node joins the table in mk1 with zero extra code.

### 2.5 `apexrouter-server`, `-cli`, `-mcp`, `-slint`

`apexrouter-server` mounts three routers on one port and `pub use`s `api_router()` so ApexOS can
embed it. Auth is Imaginarium's gate verbatim (bearer in three presentations, `read|write|admin`
scopes, loopback bypass keyed on `ConnectInfo` peer IP and failing closed, refuse a non-loopback
bind without auth). `static_files.rs` is rust-embed pointed straight at `../../ui-web` — no
`dist/`, no npm.

`apexrouter-cli` provides `bin apexrouter` (default-run) and hosts `serve` (runs the server
in-process) and `mcp`. `apexrouter-server` also exposes a thin `bin apexrouterd` for systemd users.

`apexrouter-mcp` is the hand-rolled stdio server, with the `Backend` trait split (§7).

`apexrouter-slint` is GPL-3.0-only, `publish = false`, `[[bin]] name = "apexrouter-ui"`, `build.rs`
is one `slint_build::compile` line, and it is an **edge client of the same HTTP API** — a ~250-line
`src/api.rs` `NodeClient` and no second business-logic path.

---

## 3. Process model

**One process owns everything.** `apexrouter serve` is the daemon: it holds the routing table, the
backend registry, every child process (`llama-server`, `ssh` tunnels), the health prober, the file
watcher, the HTTP server and the WS broadcast. The CLI, the MCP server and both GUIs are clients.

- **Single instance** enforced by `flock` on `$STATE/apexrouter.lock` containing
  `{pid, start_time_ticks, bind}`. A second `serve` prints the holder's PID and bind and exits 1.
- **Startup order**: load config (never `exit()` from a lib) → open state + lock → migrate if
  `routes.toml` is absent and legacy state exists → **reconcile** (adopt live local children whose
  pid+start-time match; query Vast for instances in the ledger without `destroyed_at`; raise an
  `Alert` for every orphan-suspect) → build the registry → compile the table → bind → start the
  prober, the watcher, the UI.
- **Child ownership**: local servers get `setsid` so a daemon restart doesn't kill them, and are
  re-adopted on the next start. `[server] on_shutdown = "adopt" | "stop"` (default `adopt`).
  **Vast instances are never auto-destroyed on shutdown** — a crash must not delete a paid box, and
  a leak must be *visible* rather than silently cleaned. `apexrouter vast ls --orphans` and a
  startup alert do that.
- **Blocking work** (process spawn/wait, filesystem walks, GGUF header reads, `--help` probes) goes
  through `tokio::task::spawn_blocking`. Nothing in a render or route-resolution path does I/O.
- **Shutdown**: `tokio::signal` sets a shutdown flag; the listener stops accepting; in-flight
  requests drain with a deadline (`drain_timeout_secs`, default 30); tunnels close; ledger writes
  complete inside an awaited task so the critical section can't be interrupted; the PID files are
  removed. `SIGHUP` reloads config instead of exiting.
- **PID-file compat**: the daemon writes `/tmp/vastai-gguf-proxy.pid` (and removes it on exit) so
  LocalRouter's `menu_proxy()` liveness check keeps working during the migration window.
- **Locking**: routing config and state writes take the state-dir lock; the request path takes no
  locks at all beyond `ArcSwap::load()` and a per-backend `Semaphore`.

---

## 4. The request path, in detail

### 4.1 Path normalization — the highest-risk drop-in bug

Spec `05` §4.1: LocalRouter's `base_url` always ends in `/v1` and the client path is appended
whole, so `http://localhost:8888/v1` as a client base URL produces `/v1/v1/chat/completions` and a
404 — while the project's own `SKILL.md` tells agents to use exactly that.

ApexRouter stores every `Backend.base_url` **without** `/v1`, and normalizes inbound paths by
collapsing a repeated leading `/v1` to one. Both `http://127.0.0.1:8888` and
`http://127.0.0.1:8888/v1` work, which is mandatory because `smoke.sh` appends `/v1` to whatever
you give it. A collapsed path is logged once per (User-Agent, path) pair at `debug` so a genuinely
malformed client is still discoverable.

### 4.2 The pipeline

```
inbound request
 └─ normalize path (collapse duplicate /v1)
 └─ auth gate (loopback bypass, or bearer with write scope for mutations)
 └─ Via loop guard: if `Via` already contains "apexrouter" → 508 (kills the self-loop footgun)
 └─ classify: Models | Chat | Completion | Embedding | Rerank | Opaque
 └─ read body (cap `max_body_bytes`, default 256 MiB, OpenAI-shaped 413 — not aiohttp's silent 1 MiB)
 └─ RequestPeek: {model, stream (strict bool), stream_options.include_usage}
 └─ resolve(model, class) -> Plan { candidates: Vec<Candidate>, reason: RouteReason }
 └─ for candidate in plan (bounded by retry budget AND a wall-clock deadline):
      ├─ breaker.check(backend)?                  → skip if Open
      ├─ limits.acquire(backend).timeout(queue_timeout)?  → 503 + Retry-After if saturated
      ├─ outbound_headers(inbound, credential)    → CONSTRUCTED from an allowlist
      ├─ body: Passthrough(bytes) if alias == upstream id, else Rewritten (only "model" changed)
      ├─ send with connect_timeout + headers_timeout
      └─ classify outcome:
           connect refused / DNS / TLS / pre-header timeout → Retryable, breaker.trip()
           429 with Retry-After                              → Retryable on a *different* target
           502/503/504/529                                   → Retryable
           other 4xx/5xx                                      → Terminal, relay verbatim
 └─ FIRST BYTE ⇒ Committed. No retries past this point, ever.
 └─ relay response
 └─ telemetry: RequestRecord → ring buffer + broadcast + usage log
```

The "no retry after first byte" invariant is a type, not a comment:

```rust
async fn attempt(p: PreFlight<'_>) -> Result<Committed, Retryable>;
// Committed owns the upstream response stream; the retry loop consumes PreFlight
// values and can only exit by producing a Committed. There is no way to loop on one.
```

### 4.3 Model aliasing and resolution order

`resolve()` is deterministic and observable — every response carries
`X-ApexRouter-Route: <alias>|<reason>`:

1. `model` matches an **alias** in the table → that route. (`auto`, `coder`, `big`, `local`, …)
2. `model` is `"<backend_id>/<upstream_model>"` → **explicit pin**, one candidate, no failover
   unless the route says so.
3. `model` exactly matches an **upstream model id** known to exactly one enabled backend → route
   there. *This is what makes every existing client work unchanged.*
4. Same id on several backends → an implicit route using `[router] implicit_strategy`
   (default `FirstHealthy` in registration order).
5. Otherwise → `[router] default_alias` (default `auto`), with `X-ApexRouter-Fallback: true`.
   **This is why `smoke.sh`'s hardcoded `"model":"x"` keeps working.**
6. No default and nothing matched → `404` OpenAI error `model_not_found`, message listing the
   known aliases.

Body rewriting only happens in cases 1/2/4-with-a-different-upstream-id. When the alias equals the
upstream id the original bytes are relayed untouched, so provider extensions and exact float
formatting survive. When rewriting, only the `model` value changes; everything else is re-emitted
from the parsed `Value` (documented as the one place where byte-exactness is not preserved, with a
tool-calling round-trip test to prove nothing else moves).

### 4.4 `/v1/models` aggregation

Served from the table with no upstream hop unless a backend's model cache is stale. Aliases first,
then per-backend models as `"<backend_id>/<model>"`. Extras live under a single `apexrouter` key so
strict clients ignore them.

```jsonc
{"object":"list","data":[
  {"id":"auto","object":"model","created":1780000000,"owned_by":"apexrouter",
   "apexrouter":{"kind":"alias","strategy":"first_healthy","healthy":true,
                 "targets":["local-carnice","together:…-70B-Instruct-Turbo"]}},
  {"id":"local-carnice/Carnice-9b-Q6_K","object":"model","owned_by":"local-carnice",
   "apexrouter":{"kind":"backend_model","status":"ready","ctx":32768,"slots":"1/4",
                 "price":null,"tok_per_s_p50":4.1}}
]}
```

### 4.5 Streaming

Byte-for-byte relay into `Body::from_stream`, never re-framed — chunk boundaries may split an SSE
event and every OpenAI SDK buffers on `\n\n`. Fixes carried from spec `05` §6:

- SSE headers are forced **only** when upstream is 2xx *and* `Content-Type: text/event-stream`;
  otherwise the buffered path runs, so a `400 {"error":…}` reaches the client as JSON with the
  right content type instead of choking the SDK parser.
- **Never a total timeout on a stream.** `connect_timeout` (5 s) + `headers_timeout` (120 s, a cold
  prompt-eval is slow) + an **idle timeout between chunks** (120 s).
- Client disconnect **explicitly aborts** the upstream request. Today llama.cpp keeps generating
  into a dead socket and holds a slot; ApexRouter drops the `Committed` handle, which cancels the
  reqwest future.
- A tee watches the tail of the stream for the `usage` / `timings` objects (set
  `stream_options.include_usage` upstream when the client didn't and `[router]
  request_usage = true`), so streaming requests are metered too. If the provider never emits one
  (Together's behaviour is undocumented), the record degrades to
  `TokenCount::Estimated` / `Estimate::Approximate` — it never blocks or fails the proxy.
- `X-Accel-Buffering: no` is set, and `Cache-Control: no-cache`.

### 4.6 Headers, errors, limits

Outbound headers are **constructed** from an allowlist plus the backend's credential — the inbound
`HeaderMap` is never cloned wholesale (`07` B1). The client's `Authorization` never reaches a
third party; the backend's own credential is injected, which finally makes a local `llama-server
--api-key` reachable through the proxy. Added: `X-Request-Id`, `Via: 1.1 apexrouter`. Preserved for
compat: `X-Provider`, `X-Usage: "{prompt}+{completion}"`. Added: `X-ApexRouter-Backend`,
`X-ApexRouter-Route`, `X-ApexRouter-Attempts`.

Response relay uses `RelayMode::Passthrough` by default with reqwest's automatic decompression
disabled, so `Content-Encoding`/`Content-Length` stay truthful and multi-valued headers (e.g.
`Set-Cookie`) survive.

Errors are OpenAI-shaped everywhere:
`{"error":{"message":…,"type":…,"code":…,"param":null}}` with
`upstream_unavailable`→502, `upstream_timeout`→504, `no_healthy_backend`→503,
`model_not_found`→404, `server_overloaded`→503 + `Retry-After`, `request_too_large`→413,
`loop_detected`→508, `provider_not_configured`→503 (distinct from 502 upstream-failed — the
distinction is load-bearing in both house projects).

Concurrency: one `Semaphore` per backend sized from `/props.total_slots` (or config), a global
`max_inflight`, a queue with `queue_timeout_ms`. The retry budget is a **token bucket per backend**,
not per request, so a struggling backend cannot be amplified into a retry storm.

`/metrics` (Prometheus text): `apexrouter_requests_total{alias,backend,status}`,
`apexrouter_ttft_seconds`, `apexrouter_tokens_total{kind}`, `apexrouter_tokens_per_second`,
`apexrouter_backend_up{backend}`, `apexrouter_inflight{backend}`, `apexrouter_queue_depth`,
`apexrouter_cost_usd_total{provider}`. llama.cpp's `/slots` is read internally for slot counts and
**never proxied outward** — it echoes prompts.

### 4.7 Hot reload

`routes.toml` and `config.toml` are watched with `notify` (250 ms debounce) plus a 10 s poll
fallback for filesystems where inotify misses events. On change: parse → `TableBuilder::compile` →
validate (no dangling target, no duplicate alias, no alias shadowing a live upstream id without
`allow_shadow`, every `require_tags` satisfiable) → `ArcSwap::store` → broadcast
`RouteTableChanged`. **A failed compile keeps the running table**, raises an `Alert`, and shows red
in both GUIs and in `apexrouter status`. Writes from the API/GUI go through `toml_edit` so hand
comments survive, then tmp+rename.

---

## 5. API surface

Both planes live on `127.0.0.1:8888`. `/v1/*` belongs to OpenAI compatibility, so the control plane
is `/api/v1/*` — a deliberate deviation from Imaginarium's `/v1/*` control API, documented in
`docs/CHARTER.md`.

### 5.1 Data plane (the router)

| Method | Path | Behaviour |
|---|---|---|
| `POST` | `/v1/chat/completions` | routed, streaming or buffered |
| `POST` | `/v1/completions` | routed |
| `POST` | `/v1/embeddings` | routed (class `Embedding`; only embedding-capable backends) |
| `GET` | `/v1/models` | aggregated across all enabled backends + aliases |
| `GET` | `/v1/models/{id}` | one entry, alias or `backend/model` |
| `POST` | `/v1/rerank` | routed if the target supports it |
| `*` | `/v1/{*path}` | opaque passthrough to the resolved alias's primary target |
| `*` | `/{*path}` | same, after `/v1` normalization |

Legacy compat, byte-identical to `05` §15:

| Method | Path | Behaviour |
|---|---|---|
| `GET` | `/health` | `{"ok":true,"product":"apexrouter","version":"0.1.0","provider":"<active>","uptime":<f64>}` — a superset of both the LocalRouter shape and the house `/health` contract |
| `GET` | `/providers` | exact LocalRouter JSON shape, but probes run **concurrently** and Together is detected from the config file too (the documented inconsistency, fixed) |
| `POST` | `/switch` | same request/response shapes; retargets the `default_alias` route, and mirrors to `.active_endpoint` when `[compat] active_endpoint_path` is set. `api_key` is now actually persisted as a `CredentialRef`, and `local` copies the instance's key — both were silent no-ops |

`/switch` gains a `base_url` allowlist (`[compat] allow_switch_hosts`) because unauthenticated
`/switch` with an arbitrary URL plus an injected Together key is a credential-exfiltration
primitive, not just SSRF (`05` §11).

### 5.2 Control plane

```
GET    /api/v1/snapshot
GET    /api/v1/backends                 POST /api/v1/backends            (register a URL)
GET    /api/v1/backends/{id}            PATCH /api/v1/backends/{id}      DELETE …
POST   /api/v1/backends/{id}/{probe|enable|disable|drain}
GET    /api/v1/routes                   PUT  /api/v1/routes              (whole table)
GET    /api/v1/routes/{alias}           PUT  /api/v1/routes/{alias}      DELETE …
POST   /api/v1/routes/validate          POST /api/v1/routes/{alias}/test
POST   /api/v1/reload
GET    /api/v1/requests?limit=&alias=&backend=      GET /api/v1/requests/{id}
POST   /api/v1/requests/{id}/cancel
GET    /api/v1/usage?since=&group_by=provider|model|backend|alias
GET    /api/v1/local/discovery          POST /api/v1/local/instances
DELETE /api/v1/local/instances/{name}   GET  /api/v1/local/instances/{name}/logs?tail=&follow=  (SSE)
GET    /api/v1/vast/{profiles,credit,instances}     POST /api/v1/vast/offers/search
POST   /api/v1/vast/instances           DELETE /api/v1/vast/instances/{id}
GET    /api/v1/vast/instances/{id}/log?follow=      (SSE)
POST   /api/v1/vast/instances/{id}/tunnel           DELETE …/tunnel
GET    /api/v1/hf/search?q=             GET /api/v1/hf/models/{*repo}/files
GET    /api/v1/providers                PUT /api/v1/providers/{id}
GET    /api/v1/fit?model=&vram_gb=&ctx=
POST   /api/v1/smoke                    (SSE: one event per named probe)
GET    /api/v1/diagnose?only=           (SSE: one event per check)
GET    /metrics                         GET /ws
```

`POST /api/v1/vast/instances` requires `{"confirm": true, "max_hourly_usd": <f>}` in the body and
returns `409` without it — the `SpendApproval` boundary. It responds immediately with a pending
record (`?no_wait` house pattern) and the spawned task flips the row to `failed` on **every** error
path including a `JoinError` from a panic.

`GET /ws` — subscribe to the broadcast **before** sending the snapshot, re-send a full snapshot on
`RecvError::Lagged`, `tokio::select!` also drains `socket.recv()` to notice a close.
`RequestStarted`/`RequestFinished` are only serialized when there is at least one subscriber, and
`UsageTick` is coalesced to 1 Hz — a router at 50 rps must not drown its own dashboard.

### 5.3 CLI

`clap` derive, noun-grouped, house verb vocabulary (`ls`/`get`/`show`/`path`/`init`/`create`/
`status`), `--json` per-subcommand emitting `serde_json::to_string_pretty` of the protocol type and
nothing else on stdout, no colour crate, `anyhow`-driven exit 1, tracing to **stderr** (mandatory —
`mcp` shares the binary and owns stdout).

```
apexrouter serve [--bind ADDR] [--no-ui] [--allow-remote --token-env VAR]
apexrouter status [--json]                     # router, aliases, backends, inflight, 24h cost
apexrouter models [--json]                     # the aggregated /v1/models, human table
apexrouter route ls | show <alias> | set <alias> --target <backend[:model]>... [--strategy S]
                 [--failover] [--require-tag T] [--max-cost F] | rm <alias> | test <alias>
apexrouter backend ls | show <id> | add <url> [--kind node] [--tag T] [--key-env VAR]
                   | enable|disable|drain|probe|rm <id>
apexrouter local discover [--json]
apexrouter local start <model-id|path> [--alias A] [--build B] [--device D] [--ctx N] [--np N]
                                       [--kv q8_0] [--mode thinking|coding|nonthinking]
apexrouter local stop <name> | ls | logs <name> [-f]
apexrouter vast profiles | offers [--profile P] [--geo G] [--max-price F] [--json]
apexrouter vast launch --profile P --model-repo R --quant Q [--yes] [--max-hourly F]
apexrouter vast ls [--orphans] | destroy <id> [--yes] | log <id> [-f]
apexrouter tunnel up <id> | down <id> | status
apexrouter provider ls | set together --base-url U --key-env VAR | test <id>
apexrouter hf search <query> | files <repo>
apexrouter fit <model> [--vram-gb N] [--ctx N]
apexrouter usage [--since 24h] [--by provider|model|backend|alias] [--json]
apexrouter smoke [--base-url URL] [--model auto]
apexrouter diagnose [--only <check>] [--json]
apexrouter migrate [--from ~/.vastai-gguf] [--dry-run]
apexrouter config init | show | path      apexrouter token create|ls|revoke
apexrouter mcp [--proxy URL]              apexrouter version
```

There is **no TUI**. The brief asks for two GUIs; the CLI is the terminal surface, and it is
scriptable in a way LocalRouter's questionary loop never was (spec `07` §2.2: "an agent today
cannot start a model, cannot tear one down, cannot list recipes").

### 5.4 MCP tools

Hand-rolled newline-delimited JSON-RPC over stdio, copying `Prefrontal-RS/prefrontal-cli/src/mcp.rs`
verbatim in shape: protocol version `"2024-11-05"` **echoed back from the client's request**, tool
failures as results with `isError: true` (JSON-RPC errors reserved for protocol breakage), compact
one-line JSON, all logging to stderr, exit on stdin EOF. Dual-era hooks per `09`: accept-and-ignore
`_meta`, answer `server/discover` advertising `supportedVersions`, emit `resultType: "complete"`.
Streamable-HTTP is deferred, but dispatch is transport-agnostic
(`fn dispatch(method, params) -> Result<Value, RpcError>`) so an axum route is a day's work when
ApexOS nodes need it.

All names prefixed `apexrouter_` (three MCP servers share `~/Projects/.mcp.json`). Descriptions are
long and operational.

| Tool | Purpose |
|---|---|
| `apexrouter_status` | router health, every alias and where it currently points, backend health, inflight, 24 h spend. The "what is my inference situation" call. |
| `apexrouter_models` | the aggregated model list with alias, backend, ctx, price, live tok/s. **The call an agent makes before choosing a `model` string.** |
| `apexrouter_route_set` | point an alias at a backend/model, set strategy/failover. Takes effect on the next request; no restart. |
| `apexrouter_local_discover` | builds, devices (with free VRAM), models (with sizes and fit estimates) on this machine. |
| `apexrouter_local_start` / `apexrouter_local_stop` | spawn/stop a llama-server and bind an alias. Returns the alias to use immediately. |
| `apexrouter_fit` | "will this model fit, and at what ctx/parallel" — pure, instant, no side effects. |
| `apexrouter_usage` | cost + tokens + tok/s by window and grouping. |
| `apexrouter_smoke` | four named probes against an alias; the "is the endpoint I'm about to use actually working" call. |
| `apexrouter_vast_offers` | read-only live market search. Safe. |
| `apexrouter_vast_launch` | **spends money.** Requires `confirm: true` and `max_hourly_usd`; refuses otherwise with the current credit in the error text. |
| `apexrouter_vast_destroy` | tear down, with accrued cost in the result. |
| `apexrouter_diagnose` | run the check registry, optionally one check. |

`#[async_trait] trait Backend`: `LocalBackend` answers read-only tools (discovery, fit, usage,
config) directly from `apexrouter-core` even when the daemon is down, and returns a helpful
`isError` result for mutations telling the agent to run `apexrouter serve`. `ProxyBackend` forwards
everything to `$APEXROUTER_URL` with `$APEXROUTER_TOKEN`. Selected by `--proxy URL` / `-p` /
env, parsed by hand so clap stays out of the MCP crate.

---

## 6. Data model, config, state, compatibility

### 6.1 `config.toml` (hand-edited, XDG config dir)

```toml
[server]
bind = "127.0.0.1:8888"
allow_localhost_no_auth = true
node_token_env = "APEXROUTER_TOKEN"
ui_dir = ""                       # empty = the embedded ui-web
drain_timeout_secs = 30
on_shutdown = "adopt"             # adopt | stop  (local children; Vast is never auto-destroyed)

[router]
default_alias      = "auto"       # what an unknown model name falls back to
implicit_strategy  = "first_healthy"
max_inflight       = 64
connect_timeout_ms = 5000
headers_timeout_ms = 120000
idle_timeout_ms    = 120000
queue_timeout_ms   = 30000
max_body_bytes     = 268435456
retry_budget_per_min = 30
request_usage      = true         # inject stream_options.include_usage when absent
capture_bodies     = false        # prompts are NEVER stored unless this is on
log_usage          = true

[discovery]
model_dirs   = ["~/models"]
build_globs  = ["~/llama.cpp/build*/bin/llama-server"]
scan_interval_secs = 300

[providers.together]
base_url    = "https://api.together.ai/v1"
api_key_env = "TOGETHER_API_KEY"          # or api_key_file = "~/.vastai-gguf/config.toml"

[providers.vast]
api_key_file = "~/.config/vastai/vast_api_key"
max_boot_secs = 1800                       # watchdog: auto-destroy a wedged instance

[compat]
active_endpoint_path = "~/Projects/Inference/tools/LocalRouter/.active_endpoint"  # optional mirror
proxy_pid_file       = "/tmp/vastai-gguf-proxy.pid"
allow_switch_hosts   = ["api.together.ai", "127.0.0.1", "localhost"]
```

### 6.2 `routes.toml` — the product's core config

```toml
# The alias an agent hardcodes forever.
[[route]]
alias    = "auto"
strategy = "first_healthy"
targets  = ["local-carnice", "together:meta-llama/Llama-3.3-70B-Instruct-Turbo"]
failover = true
retries  = 2

[[route]]
alias    = "coder"
strategy = "cheapest"
targets  = ["local-*", "vast-*"]
require_tags = ["tools"]
max_cost_per_mtok = 1.0

[[route]]
alias    = "big"
targets  = ["vast-3090x2", "together:deepseek-ai/DeepSeek-V3"]
```

Written by the GUI/CLI through `toml_edit`, so hand comments and ordering survive — the exact
failure mode `tomli_w` inflicted on `recipes.toml`.

### 6.3 State (daemon-owned, XDG state dir)

| File | Shape | Notes |
|---|---|---|
| `backends.json` | `Vec<Backend>` minus secrets | registry; atomic tmp+rename |
| `instances.jsonl` | append-only ledger | `{id, provider, spec, created_at, confirmed_at, destroyed_at, last_status, est_cost_usd}` — replaces the single-slot `.last_instance` (`07` A2). "Active" is a query, not a file. |
| `usage.log` | JSONL | legacy field names preserved exactly, plus optional `request_id`/`backend`/`alias`/`ttft_ms`/`tok_per_s`/`estimated` |
| `presets.toml` | `Vec<LaunchPreset>` | what `recipes.toml` becomes: saved *results* of a discovery session, each with `provenance {discovered_at, size_bytes, fit_estimate}` so staleness is detectable |
| `logs/<backend>.log` | text | rotated, never truncate-on-start (that destroyed the previous crash log) |
| `apexrouter.lock` | JSON | flock holder |
| `known_hosts` | ssh | dedicated, because Vast recycles `sshN.vast.ai` hostnames |

Timestamps are real RFC 3339 UTC on write. On read, the legacy `%Y-%m-%dT%H:%M:%SZ` local-time-with-
a-lying-`Z` values are parsed leniently.

### 6.4 Backwards compatibility with `~/.vastai-gguf`

Read, always: `config.toml` `[providers.together]` (with a *real* TOML parser — we store the
`CredentialRef`, never a copy of the key), `.pinned_provider`, `usage.log` (including the legacy
`epoch` field), `local_instances/<name>.json` (paths validated on load; stale is normal),
`local_logs/`. Read for migration: `.active_endpoint` (all four shapes via serde aliases),
`.last_instance`, `.instance_history`, `.hf_pin`, `recipes.toml` (→ `presets.toml` +
`SearchProfile`s + a `known_forks` table preserving the `fairydreaming/llama.cpp @ deepseek-dsa`
knowledge, which is genuinely undiscoverable).

Written, optionally: `.active_endpoint` mirrored on every `default_alias` change when
`[compat] active_endpoint_path` is set, and `/tmp/vastai-gguf-proxy.pid`. Both exist purely so the
Python TUI keeps working during the migration window; `docs/CHARTER.md` records that they are
removed at 1.0.

Provider id spelling: one enum with serde aliases for `vast-gguf` / `vast_gguf` / `local-gguf` /
`local` / `together` / `vllm`. **`vast-gguf` stays on the wire** in `/health`, `/providers`,
`/switch` and `usage.log`.

Ports keep their defaults because they are baked into agent configs: proxy **8888**, local
llama-server **8100+**, Vast tunnel **8800+** (allocated from a pool, since multiple instances is
now the normal case). `PROXY_PORT` is honoured as an override.

---

## 7. GUI plan — two surfaces, one backend

Both GUIs are clients of `/api/v1/*` + `/ws`. Neither contains business logic. The web UI is three
files (`index.html`, `app.js`, `style.css`), no npm, no CDN, no framework, no build step, embedded
via `rust-embed` pointed straight at `../../ui-web`, with a `ui_dir` config escape hatch for a
live-reload dev loop. Dark-first CSS custom properties with a `prefers-color-scheme: light`
override; status colours reserved for health, never identity; badges pair icon + label;
`textContent` everywhere; `[hidden]{display:none}` guards on every element that declares its own
`display`. WS first with a REST fallback for first paint, exponential-backoff reconnect
(1 s → ×2 → cap 15 s), `setInterval(render, 60_000)` to keep relative timestamps honest.

### 7.1 Web UI screens

**Router bar** (always visible, the top of the page). The base URL
`http://127.0.0.1:8888/v1` with a copy button and `OPENAI_API_KEY=not-needed` beside it — the
first thing a new user sees is the thing they paste into their agent. Then: connection dot, inflight
count, requests/min, aggregate tok/s, 24 h spend.

**Routes** — the primary panel, because routing is the product. One row per alias: alias · target
chain (chips, drag to reorder) · strategy · health roll-up · p50 TTFT · p50 tok/s · $/Mtok.
Clicking a row opens the route editor drawer: target picker populated from live backends, strategy
select, filter fields, failover/retries toggles, and a **Test** button that runs a 20-token
completion through the alias and reports TTFT and tok/s inline. Save is a `PUT` — hot, no restart.
A red banner appears if the on-disk table failed to compile, showing the parse error and the fact
that the *previous* table is still serving.

**Backends** — card grid: label, kind badge, model(s), health dot, `slots 1/4`, queue depth,
p50/p95, `$/hr` or `$/Mtok`, last error, uptime. Per-card actions: probe, drain, disable, logs,
stop/destroy. This replaces LocalRouter's "Local status", "Instances" and "Providers" menus with one
uniform card, exactly as `01`'s redesign notes ask.

**Live requests** — a streaming table off the WS: time, alias → backend, model, status, TTFT,
tok/s, tokens, cost, attempts. Click for detail. Prompt bodies are **not** captured unless
`capture_bodies` is on, and the toggle says so in the UI.

**Launch drawer** — one non-modal drawer with two tabs over one `LaunchDraft` type, and a summary
that is visible *while* you edit rather than only at the end of an irreversible paid action:
- *Local*: discovered models (grouped shards, real sizes) × builds × devices (with free VRAM), the
  fit solver proposing ctx/parallel/kv with a headroom bar, mode preset, alias to bind. One button:
  **Start & bind**.
- *Rent*: profile → live offer table (sortable, filterable, re-queryable in place, showing $/hr,
  reliability, pooled VRAM, down Mbps, CUDA, geo, disk) → HF model search with authoritative sizes
  → fit → **cost panel showing $/hr, estimated total, and current Vast credit** → confirm. Then the
  drawer turns into a live `BootPhase` progress view with the log stream, an elapsed timer, and a
  destroy button — no separate "Watch boot" menu item.

**Fleet & cost** — rented instances with uptime, accrued cost, total hourly burn, credit remaining
and a burn-down estimate; orphan-suspects flagged loudly with a one-click reconcile. Usage charts:
tokens/day and $/day stacked by provider, plus a tok/s-by-backend view.

**Diagnostics drawer** — the check registry as rows with pass/fail badges and timings, each
runnable individually (no more waiting through SSH probes to see rate limits).

### 7.2 Slint app (`apexrouter-ui`)

GPL-3.0-only, out of `default-members`, an edge client of the same API. Never `#[tokio::main]`:
`fn main() -> anyhow::Result<()>` builds a multi-thread runtime, keeps it alive for the app
lifetime, and ends with `ui.run()?`; every UI touch crosses back via `Weak` +
`invoke_from_event_loop` / `upgrade_in_event_loop`. `export global Palette` matching the web
tokens exactly (`#0d0d0d` page, `#1a1a19` surface, `#2c2c2a` hairline, `#3987e5` accent, `#0ca30c`
good, `#fab219` warn, `#d03b3b` critical); nothing hardcodes a colour outside it. Layouts:
`src/ui/appwindow.slint` root + `palette.slint`, `types.slint`, `components/*.slint`.

mk1 screens:

1. **Dashboard** — router bar (copyable base URL), aggregate stats, backend list with health dots,
   a live request ticker.
2. **Routes** — the alias list with an editor pane (target combo from live backends, strategy
   combo, failover toggle, Test button). Write access, because this is the product.
3. **Backends** — list + actions + a `Flickable` log pane with follow mode.
4. **Local launch** — discovered models list, device combo, ctx/parallel steppers with the fit
   readout, alias field, Start.

Deferred to mk2 in Slint: the Vast rent flow (money actions live in the web UI and CLI first, where
the confirmation UX is richer), usage charts, diagnostics.

---

## 8. mk1 scope

Ordered. Everything below ships; the deferral list at the end is explicit.

1. **Workspace + protocol + core skeleton.** Crates, CI, `rustfmt.toml`, `config.example.toml`,
   `routes.example.toml`. `Paths`, `Config`, `Secret`/`CredentialRef`, atomic `Store`, `Ledger`,
   `error.rs`. CLI: `config init|show|path`, `version`.
2. **The router core, headless and unit-tested against a fake upstream.** `RoutingTable`,
   `TableBuilder::compile` + validation, `resolve()` with all six resolution rules, `outbound_headers`
   allowlist, `BodyPlan` passthrough-vs-rewrite, buffered relay, SSE relay with tee + idle timeout +
   abort-on-disconnect, `PreFlight → Committed` retry/failover, circuit breaker, per-backend
   semaphores + queue timeout, OpenAI error envelope, `RequestRecord` + ring buffer + broadcast +
   usage-log write, `/metrics`.
3. **The data plane on 8888.** `/v1/*` routes, `/v1/models` aggregation, `/v1` path collapse, the
   three legacy routes byte-compatible, `X-Provider`/`X-Usage` preserved, PID-file compat.
   *Milestone: `smoke.sh http://127.0.0.1:8888` passes against a manually registered backend.*
4. **Registry + health prober + hot reload.** `backends.json`, `routes.toml`, `notify` + poll
   fallback + `SIGHUP` + `POST /api/v1/reload`, `ArcSwap` swap, bad-config-keeps-old-table.
5. **Local provider.** Discovery (build glob + `--list-devices` + `--help` `FlagSet`; recursive
   model scan with shard grouping and GGUF headers), the single argv builder with unified sampling
   presets, supervisor with `LD_LIBRARY_PATH` + `setsid` + port bind-probe + deadline health gate +
   **cleanup on failure**, startup reconciliation, log tail/follow. `fit.rs` with tests.
6. **Together provider + credential resolution.** Live models + pricing, connection/completion
   probes, 429 handling with `x-ratelimit-reset`, token bucket.
7. **Control plane + WS + auth + web UI.** All `/api/v1` routes above except the Vast subtree's
   advanced bits; the seven web-UI panels; the token gate with loopback bypass.
8. **Vast provider — the money path.** REST client (search/create/instances/destroy/logs/credit),
   five `SearchProfile`s (2×/3×/4× RTX 3090, 1×/2× H100 — expressed as query *templates* over
   `gpu_name` + `num_gpus`, plus a free-form query box so a new card works on day one),
   `PendingLaunch` two-phase commit, `BootPhase` machine off the REST log stream with the
   `max_boot_secs` auto-destroy watchdog, ssh tunnel with owned PID and per-instance ControlPath,
   auto-registration of the booted instance as a backend + alias binding, destroy with accrued cost,
   credit display, startup orphan reconciliation, and a **download-stall alert** derived from log
   progress.
9. **HF provider** — search + `paths-info` sizing + gated classification, feeding the rent tab.
10. **Slint app** — the four mk1 screens.
11. **CLI** — every verb in §5.3 except `vast log -f` niceties; `--json` on all of them.
12. **MCP** — the twelve tools in §5.4, `Local`/`Proxy` backends, registered in `~/Projects/.mcp.json`.
13. **Migration** — `apexrouter migrate` importing `~/.vastai-gguf` + `.active_endpoint` +
    `recipes.toml`, with `--dry-run`.
14. **Native smoke + docs.** The four `smoke.sh` probes reimplemented as named steps with pass/fail
    badges, TTFT and tok/s. README with the Imaginarium banner, `CLAUDE.md` maintainer's brief,
    `docs/{CHARTER,API,ARCHITECTURE,ROUTING,SLINT,AGENTS}.md`, `skills/apexrouter/SKILL.md`.
15. **The acceptance test.** `apexrouter local start carnice-9b --alias auto` spawns
    `~/llama.cpp/build-vulkan/bin/llama-server` on `Carnice-9b-Q6_K.gguf`, registers it, binds
    `auto`; `curl http://127.0.0.1:8888/v1/chat/completions -d '{"model":"auto",...}'` streams a
    reply; the request appears in both GUIs' live-request view with a real tok/s; `apexrouter usage`
    shows the tokens; `apexrouter route set auto --target together:…` re-points it with no restart
    and the next request goes to Together.

**Explicitly deferred past mk1** (each with the reason):

- **Anthropic `/v1/messages` translation.** `/v1/messages` is passed through to any backend that
  speaks it, but no OpenAI↔Anthropic body translation. It is a large surface (tool blocks, system
  prompts, content parts) and Claude Code reaches ApexRouter through MCP for control, not for
  inference. mk2.
- **vLLM *launch*** (local or Vast). The vLLM env contract is preserved in `presets.toml` and a
  running vLLM server registers as a `Node` backend in one command, so the *router* loses nothing.
  The provisioning wizard is mk2.
- **Batch/compare across providers.** Becomes trivial once `Strategy::Mirror` exists;
  `apexrouter compare` is mk2.
- **Deep SSH diagnostics** (the four remote probes) and **stall *recovery*** (pkill + re-exec
  `launch.sh` from `/proc/<pid>/environ`). mk1 detects and alerts; the remote surgery is mk2.
- **HF download manager.** mk1 lists and sizes; downloading is llama.cpp's `-hf` or the container's
  `hf download`.
- **Recipe/tier/docker CRUD editor as such.** Replaced by discovery + `LaunchPreset`s + search
  profiles; `recipes.toml` is imported read-only. The GUI's "authoring" is "save this draft".
- **llama.cpp router mode** (`--models-dir`, `POST /models/load`). b9199 already has it and it
  overlaps our supervision job; mk1 keeps direct single-model supervision because it matches the
  existing state files and the failure modes we understand. Noted as the mk2 simplification.
- **MCP streamable-HTTP transport**, **sqlite storage**, **Vast in the Slint app**, **CORS**
  (no browser client exists; the embedded UI is same-origin). All mk2, all cheap once needed.

---

## 9. Risks

Listed with mitigations, because "risk" without a mitigation is just anxiety.

1. **Body rewriting perturbs payloads.** Re-serializing to change `model` changes key order and
   float formatting. *Mitigation*: rewrite only when the alias differs from the upstream id;
   byte-passthrough otherwise; a tool-calling round-trip test asserting nothing but `model` moves.
2. **SSE tee correctness.** Getting usage out of a stream without breaking framing or adding
   latency is the fiddliest code in the crate. *Mitigation*: the tee never gates the relay — bytes
   go out first, the parse copy is best-effort and lossy by design; a fixture test replays real
   llama.cpp and Together SSE captures.
3. **Client-disconnect abort.** If it doesn't work, llama.cpp keeps generating and holds a slot,
   which on a small rig means the router wedges itself. *Mitigation*: an integration test that
   drops the client mid-stream and asserts `/slots` frees within a second.
4. **Retry amplification.** Retries + a flapping breaker can turn a struggling backend into a dead
   one. *Mitigation*: retry budget is a per-backend token bucket, breaker half-open admits exactly
   one probe, and `Retry-After` is honoured.
5. **Vast billing leak.** The `PendingLaunch` guard closes the Ctrl-C window but not a hard kill
   between the API call and the response. *Mitigation*: the reservation row is written *before* the
   call; startup reconciliation queries Vast for every ledger row without `destroyed_at` and alerts
   on anything it did not expect. Credit and burn rate are always on screen.
6. **Two writers to `.active_endpoint`** while the Python TUI is still installed. *Mitigation*:
   atomic tmp+rename on both sides and a mtime watch; `docs/CHARTER.md` says plainly: run one or
   the other, not both.
7. **Alias/model-id shadowing.** An alias named the same as a real upstream model silently changes
   what a client gets. *Mitigation*: compile-time validation rejects it unless `allow_shadow` is
   set, and `X-ApexRouter-Route` always reports the reason.
8. **`/v1` collapse hides real bugs.** *Mitigation*: log once per (UA, path); surface a "clients
   sending a doubled prefix" note in diagnostics.
9. **Full JSON parse per POST** costs allocations at 128 K-token contexts. *Mitigation*: measured,
   not assumed; if it shows up, replace `RequestPeek` with a streaming scanner that stops after the
   top-level `model`/`stream` keys.
10. **`notify` misses events** on some filesystems. *Mitigation*: 10 s poll fallback plus explicit
    `POST /reload` and `SIGHUP`.
11. **Slint build friction** (`libfontconfig1-dev`, GPL boundary). *Mitigation*: out of
    `default-members` and out of the CI `-p` list; the licence caveat is stated plainly in the
    README.
12. **Scope creep from the Vast subtree.** It is the single biggest chunk of mk1 and the least
    load-bearing for the lens. *Mitigation*: it is item 8 of 15 — if the schedule slips, the router,
    the local provider and both GUIs still constitute a real, useful product, and Vast can ship as
    mk1.1.
13. **Credential leakage into logs.** *Mitigation*: `Secret<String>` with redacting `Debug`, no
    credential ever in argv (`--api-key-file` + `LLAMA_ARG_*` for llama-server), the `TraceLayer`
    span records method + path only because `?token=` exists, and Vast's `/users/current/` response
    echoes `api_key` — the struct simply has no field for it.

---

## 10. Rationale

The proxy is the only part of LocalRouter that everything else exists to serve, and it is the part
that got the least design. Fourteen menus, 71 recipes and 19 GPU tiers all funnel into writing one
string into one file that a 417-line forwarder re-reads on every request. Building the router
properly and treating provisioning as "things that add rows to the routing table" makes most of
LocalRouter's structure evaporate rather than get ported.

Model aliasing is the feature that makes the whole thing worth running. Today, switching backends
silently breaks every client because the `model` string is passed upstream verbatim; with aliases,
an agent hardcodes `OPENAI_BASE_URL=http://127.0.0.1:8888/v1` and `model: "auto"` once and never
touches either again while the thing behind it changes from a laptop iGPU to a rented 2×H100 and
back. That is the actual product promise, and it is one `HashMap` lookup plus one JSON key rewrite.

Putting the request path in its own crate with no knowledge of Vast, HuggingFace or process
supervision means it can be tested exhaustively against a fake upstream, and it keeps the hot path
honest: an `ArcSwap` load, a lookup, a semaphore, a `reqwest` send. No file reads, no TOML parsing,
no `stat()` per request — all things the Python version does today.

The lifecycle work is designed backwards from the table. "Launch" is not a wizard that ends in a
shell script; it is a `Provisioner` returning a `Backend`, which is why local spawn, Vast rental and
"register this LAN URL" share one API, one drawer in the web UI, one CLI noun and one MCP shape.
That symmetry is also what makes the scale amendment cheap: N backends, N GPUs, N builds and remote
LAN nodes are not special cases, they are just more rows.

Money safety earns its complexity budget because the failure mode is a GPU billing overnight with
no local record — which has already happened once in this codebase. An append-only ledger, a
two-phase-commit guard, a `SpendApproval` value you cannot fabricate, and startup reconciliation
turn that from a possibility into an alert.

Everything else follows the garden's existing shape rather than inventing: one serde-only protocol
crate every surface shares, hand-rolled MCP over stdio echoing the client's protocol version, a
three-file no-build web UI, a Slint app that is an edge client with no second business-logic path, a
CLI-first surface with `--json` on everything, XDG state with nothing written into the repo, and
credentials named by env var rather than stored. The point of following it is that Andre can open
this repo six months from now and already know where everything is.
