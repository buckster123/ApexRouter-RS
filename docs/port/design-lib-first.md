# ApexRouter-RS — design proposal: **library-first, one fat binary, no daemon**

> Design lens: **simplicity and zero-ceremony UX.** A library crate plus a single binary that
> can be a one-shot CLI, a headless server, an MCP server, or a GUI backend — with no mandatory
> background daemon. State lives on disk, mutation is serialised by advisory locks, and the
> proxy is started on demand by whichever surface needs it.
>
> Companion documents: `00-machine-ground-truth.md` (authority for the smoke test),
> `00b-brief-amendment-scale.md` (authority for "the laptop is not the target"),
> `00c-vast-api-verified.md` (authority for the Vast wire format), `08-house-conventions.md`
> (authority for everything stylistic).

---

## 0. The thesis in one page

LocalRouter's real failure was never Python. It was that **"what is active" had four
implementations that disagreed** (`providers.get_active_endpoint`, `endpoint_proxy.resolve_target`,
the TUI's `show_status`, and `proxy_status_detail`), each with its own parser, its own fallback,
and its own idea of liveness. The obvious cure is "put one daemon in charge". That cure is wrong
here, and this document argues the alternative.

**The alternative:** one library (`apexrouter-core`) owns every fact and every mutation. One state
directory holds the facts. One advisory-lock discipline serialises mutation. Liveness is
*computed*, never stored. Every surface — CLI, MCP, HTTP API, web UI, Slint app — is a thin
projection of that library. The listening socket is the only long-lived process, it is started on
demand, and it owns *nothing* except a cache, a broadcast channel and a request counter.

Concretely:

- `apexrouter local start carnice` from a fresh checkout: discovers the model, discovers the
  Vulkan build, computes a fit, spawns `llama-server` **detached**, waits for `/health`, registers
  the alias, starts the proxy on `:8888` if it isn't up, prints the base URL. One command. No
  install step, no unit file, no "did you start the daemon first".
- `apexrouter status` with nothing at all running: reads the state dir, probes liveness, prints
  the truth. Works for agents over MCP the same way.
- `apexrouter serve --stop` kills the socket. **It does not kill your models.** The 6 GB you just
  spent three minutes loading survives a GUI restart, a manager upgrade, and a crash.

That last property is the whole argument. A supervising daemon couples the lifetime of the
expensive thing to the lifetime of the cheap thing. Inverting that is both simpler and better.

---

## 1. Crates

Five workspace members; two binaries. Crate count is a compile-time concern — the simplicity claim
in this proposal is about **processes and ownership**, not about how many `Cargo.toml` files exist.
The split below is the minimum that satisfies the GPL boundary (Slint), the ApexOS mount point
(`api_router`), and the house rule that frontends deserialize the same enums the server serializes.

```
ApexRouter-RS/
├── Cargo.toml                    # workspace, resolver 2, default-members excludes ui-slint
├── rustfmt.toml                  # comment only, per house
├── config.example.toml
├── assets/banner.png             # Imaginarium-generated, credited in the README footer
├── crates/
│   ├── apexrouter-protocol/      # serde-only wire types. No I/O, no tokio, no reqwest.
│   ├── apexrouter-core/          # THE library. All logic, all I/O adapters, all state.
│   ├── apexrouter-server/        # axum: proxy routes + /api control plane + WS + embedded UI
│   └── apexrouter-cli/           # [[bin]] apexrouter — clap CLI + `serve` + `mcp`. default-run.
├── ui-web/                       # index.html, app.js, style.css — no npm, no CDN, no build step
└── ui-slint/                     # [[bin]] apexrouter-gui — GPL-3.0-only, publish=false
```

| Crate | Purpose | Key modules | Depends on |
|---|---|---|---|
| `apexrouter-protocol` | Wire/domain types shared by daemon, CLI, web UI, Slint, agents. `#[serde(rename_all="snake_case")]`, `#[serde(tag="type")]` on `Event`, `PartialEq` everywhere so no-op broadcasts can be suppressed. | `endpoint`, `rig`, `route`, `rental`, `usage`, `fit`, `event` | serde, serde_json |
| `apexrouter-core` | Config, paths, credentials, store+locking, discovery, GGUF, fit solver, llama argv, process supervision, providers, Vast REST, HF, SSH tunnel, usage/pricing, smoke, doctor, migration. | see §2 | protocol, tokio, reqwest(rustls), toml, toml_edit, serde, thiserror, anyhow, tracing, notify, fs2, chrono, sysinfo, async-trait, futures-util, eventsource-stream |
| `apexrouter-server` | The HTTP surface. `pub fn api_router(state) -> Router` is exported so ApexOS-RS can mount the control plane inside its own node. | `lib` (serve/AppState), `proxy`, `api`, `ws`, `auth`, `static_files` | core, protocol, axum 0.8, tower, tower-http, rust-embed |
| `apexrouter-cli` | The fat binary: every CLI verb, `serve`, `mcp`, `open`. | `main`, `cmd/*`, `mcp`, `render`, `ensure` | core, server, protocol, clap 4 |
| `ui-slint` | Native app; an **edge client of the same HTTP API** — no second business-logic path. Links `apexrouter-protocol` only. | `main`, `api` (~200-line `NodeClient`) | protocol, slint 1, tokio, tokio-tungstenite, reqwest, anyhow |

Workspace deps pinned to the house set: axum 0.8, tower 0.5, tower-http 0.7, `reqwest = { version
= "0.12", default-features = false, features = ["json","rustls-tls","stream"] }`, clap 4
derive+env, tokio 1 with an explicit feature list, tracing + tracing-subscriber env-filter to
**stderr**, serde/serde_json, toml 0.8 + toml_edit, dirs 6, rust-embed 8, chrono 0.4, thiserror 2,
anyhow 1, async-trait 0.1, futures-util 0.3, tokio-tungstenite 0.30, slint 1 + slint-build 1.
`[profile.release] lto="thin", codegen-units=1, strip=true`. No `hf-hub` (its reqwest ^0.13
requirement collides with the rest of the tree — we make six HF calls by hand). No rusqlite:
everything a human might `cat` or a script might `tail` stays a file (§4).

CI: `cargo fmt --all -- --check`, `cargo clippy -p apexrouter-protocol -p apexrouter-core -p
apexrouter-server -p apexrouter-cli -- -D warnings`, `cargo test`, `cargo build`. `ui-slint` is
never in the `-p` list, so CI never needs `libfontconfig1-dev`.

### 1.1 `apexrouter-core` module map

```
lib.rs            PRODUCT, VERSION, DEFAULT_BIND, DEFAULT_LOCAL_PORT, TUNNEL_PORT; mod list; re-exports
error.rs          thiserror Error + `pub type Result<T>`; From for serde_json/toml/reqwest/io
secret.rs         Secret<String>: Debug/Display print `***`; only accessor is `.expose()`
paths.rs          Paths { config, state, cache, logs, legacy_vastai, localrouter_dir } — resolved once
config.rs         Config (all fields defaulted) + ConfigFile for writing + `creds::resolve()`
store.rs          Store: flock'd read-modify-write over state.json; `watch()`; `snapshot()`
ledger.rs         instances.jsonl append-only + PendingLaunch Drop guard (billing safety)
usage.rs          usage.jsonl append + aggregate; reads the legacy log too; Money (micro-USD)
pricing.rs        PriceTable, Estimate::{Metered, Approximate}
exec.rs           argv-only Command wrapper; timeout is a required parameter; stdout/stderr split
proc.rs           Liveness{Alive,Dead,Unknown}, ProcIdentity{pid,start_ticks}, spawn_detached, port_free
discover/builds   glob `build*/bin/llama-server`, `--list-devices`, `--version`, flag feature-detect
discover/models   recursive GGUF scan, shard grouping, symlink follow, `.cache` skip
discover/gguf     minimal GGUF header reader (n_layer, n_head_kv, n_embd_head_k/v, n_ctx_train)
discover/rig      Rig assembly: GPUs (from --list-devices), RAM (sysinfo), builds
fit.rs            `fit(FitInput) -> Fit` — the "what fits here?" solver. Pure. Unit-tested.
llama/args.rs     ONE argv builder serving the local spawn AND the container env contract
llama/api.rs      /health /props /v1/models /slots /metrics + timings extraction
endpoint.rs       Endpoint lifecycle: start, stop, restart, swap, logs, status
providers/        Provider trait (list_models, chat, health, price) + together, openai_compat
vast/api.rs       REST client (reqwest+rustls) — offers, instances, create, destroy, logs, exec
vast/offers.rs    profile -> query object; geo/price/CUDA filters; live gpu_name vocabulary
vast/rent.rs      two-phase rental with the ledger guard; BootPhase state machine
vast/tunnel.rs    ssh -L supervision (owned Child, per-instance ControlPath, accept-new hostkeys)
vast/onstart.rs   the launch.sh / launch_vllm.sh env contract (25 vars), produced from a LaunchSpec
hf.rs             model search, paths-info sizing, token discovery, gated-repo classification
profiles.rs       saved recipes: profiles.toml via toml_edit (comment-preserving, atomic)
route.rs          RouteTable, alias resolution, `resolve(model) -> Upstream`
smoke.rs          four native probes with pass/fail badges, TTFT, tok/s
doctor.rs         Check registry, run concurrently with per-check timeouts
migrate.rs        import from ~/.vastai-gguf and a LocalRouter checkout
```

---

## 2. Process model

### 2.1 What processes exist

| Process | Started by | Lifetime | Owns |
|---|---|---|---|
| `apexrouter <verb>` (one-shot) | user / agent | milliseconds–seconds | nothing; reads state, mutates under lock, exits |
| `apexrouter serve` | on demand (§2.3) or `apexrouter serve` | until stopped | the listening socket, an in-memory cache of state, the WS broadcast channel, the request ring buffer, the health poller |
| `apexrouter mcp` | an agent harness via stdio | the agent session | nothing; same as one-shot, per tool call |
| `llama-server` (N of them) | `apexrouter local start` | **until explicitly stopped** — outlives every ApexRouter process | its own port and its model |
| `ssh -N -L …` (one per rented instance) | `apexrouter tunnel up` | until torn down or the instance dies | a local forwarded port |
| `apexrouter-gui` (Slint) | user | until closed | a window; talks HTTP/WS to `serve` |

**Nobody supervises `llama-server`.** It is spawned with `setsid` into its own session, stdio
redirected to `logs/endpoints/<id>.log`, and its identity `{pid, start_ticks, port, argv_hash,
model, build}` written to the store *before* the parent returns. The parent then exits or moves
on; the child is reparented to pid 1 and reaped by init. There are no zombies, no leaked file
descriptors (`File` is RAII; `Stdio::from(file)` consumes it), and no `Child` handle that a
crashing manager could drop on the floor. This deletes three of the four spawn-related defects in
`03-local-endpoint.md` by construction.

### 2.2 Who owns state

**The state directory owns state.** `apexrouter-core::Store` is the only code that reads or writes
it, and there is exactly one deserialization site per file.

```
$XDG_STATE_HOME/apexrouter/          (default ~/.local/state/apexrouter)
├── state.json          versioned single document (endpoints, routes, providers, tunnels)
├── state.lock          flock(LOCK_EX) around every read-modify-write of state.json
├── serve.lock          flock(LOCK_EX) held for the lifetime of a `serve` process
├── instances.jsonl     append-only Vast ledger
├── usage.jsonl         append-only completions log (legacy-compatible schema)
├── profiles.toml       saved recipes, toml_edit round-tripped
├── endpoints/<id>.lock per-endpoint start/stop mutex (makes check-then-act atomic)
├── logs/serve.log
├── logs/endpoints/<id>.log[.1]
└── cache/{rig.json, offers.json, hf/*.json}
```

Mutation is always:

```rust
store.update(|s: &mut State| {                  // flock LOCK_EX on state.lock
    s.endpoints.push(ep);                       // read → mutate → serialise
    Ok(())
})?;                                            // tmp file + fs::rename, then unlock
```

Held for microseconds. Atomic on rename, so the torn-read race that silently rerouted LocalRouter
traffic to `127.0.0.1:8800` (`05-proxy.md` §7) cannot occur. Readers that hit a partial file (they
can't, but belt-and-braces) retry once before erroring — they never fall back to a different
route.

**`state.json` stores facts, never derived status.** `EndpointRecord` holds `pid`, `start_ticks`,
`port`, `model`, `build`, `argv`, `started_at_unix`. It does **not** hold `status: "running"`.
`EndpointState` is computed on read:

```rust
fn liveness(rec) -> Liveness {
    if !proc_exists(rec.pid)                    { return Dead }
    if start_ticks(rec.pid) != rec.start_ticks  { return Dead }   // PID reuse detected
    match cmdline_contains(rec.pid, "llama-server") { true => Alive, false => Dead }
}
```
`Liveness::Unknown(io::Error)` exists for EPERM — callers must match it, so the three uncaught
`PermissionError` sites in the Python die with the type. Health (`GET /health`, falling back to
`/v1/models`) is layered on top with a 3-second TTL cache. This is directive #10 from
`07-known-issues` implemented as the only way to ask the question.

### 2.3 Startup: the proxy on demand

Every surface that needs a listening socket calls the same function:

```rust
// apexrouter-cli/src/ensure.rs
pub async fn ensure_serve(cfg: &Config) -> Result<ServeHandle>
```

1. `GET http://{bind}/health` with a 300 ms timeout.
   - Answers with `product == "apexrouter"` → **reuse it**. Done.
   - Answers with something else (an old Python `endpoint_proxy.py`, another app) → hard error
     naming the port holder (looked up via `/proc/*/fd`), plus the one-line fix.
   - No answer → continue.
2. `flock(serve.lock, LOCK_EX|LOCK_NB)`.
   - Acquired → we are the starter; release it and spawn (the child re-takes it).
   - Busy → someone is mid-startup; poll `/health` for up to 5 s, then error.
3. `spawn_detached(current_exe(), ["serve", "--managed"])` with `setsid`, stdio → `logs/serve.log`.
4. Poll `/health` up to 10 s. Return the base URL.

`serve` itself holds `serve.lock` for its whole life, which *is* the single-instance guarantee —
no PID file needed for correctness. (A `/tmp/vastai-gguf-proxy.pid` file is still written and
removed, purely so the surviving Python TUI's `os.kill(pid,0)` liveness check keeps working during
migration — drop-in checklist item 2.)

`serve` shuts down on SIGINT/SIGTERM by (a) stopping the accept loop, (b) awaiting in-flight
requests up to `drain_timeout` (default 20 s), (c) releasing the lock. **It never touches
`llama-server` children.**

### 2.4 Consistency across concurrent surfaces, without a daemon

Three mechanisms, in order of importance:

1. **One store, one lock.** Every mutation from every surface is a `Store::update` under
   `state.lock`. Two CLI invocations, the Slint app and the web UI cannot lose each other's
   writes. This is the entire concurrency story for correctness.
2. **The CLI never RPCs `serve`.** A one-shot mutates the store directly. This is deliberate: it
   means `apexrouter local stop x` behaves *identically* whether or not a server is running, so
   there is no second code path to keep in sync and no "daemon is down" failure mode. The HTTP
   handlers call the same `apexrouter_core` functions the CLI calls.
3. **`serve` watches the store.** `notify` on the state dir, 100 ms debounce → reload → `ArcSwap`
   the routes → broadcast `Event::Snapshot`. A CLI-driven change is visible in both GUIs within
   ~150 ms. The WS contract is the house one, taken wholesale: **subscribe before sending the
   snapshot; re-send a full snapshot on `RecvError::Lagged`; snapshot-on-connect covers all gaps
   so clients need zero replay logic.**

Staleness that remains: a one-shot CLI reading while another is mid-write. It can't see a torn
file (rename is atomic) and the window is microseconds. `serve`'s cache is at most 150 ms behind.
MCP caches the snapshot 5 s per process, invalidated by any mutating tool call in the same process
(Prefrontal's pattern, tightened).

### 2.5 Process supervision, honestly

| Concern | Answer |
|---|---|
| Crash detection | `serve`, when running, polls each endpoint's health every `health_interval` (10 s) and flips it to `Failed{reason}` + broadcasts. When `serve` is not running, the next `status`/`snapshot` read computes it. Detection is never *missed*, only *delayed*, and the delay is bounded by the next read. |
| Auto-restart | Opt-in: `[endpoints] restart = "never" \| "on-failure"` with exponential backoff and `max_restarts_per_hour`. Only `serve` performs restarts — documented plainly: **no server, no auto-restart.** For a laptop and for a rig alike, that is the honest contract; nobody is surprised by a model resurrecting itself. |
| Startup failure | `start` runs to a **real deadline** (`start_timeout_secs`, default 180) with a single wall clock, not 60 × (1 s sleep + 3 s connect) ≈ 4 minutes. On timeout or early exit: SIGTERM the child, wait, SIGKILL, remove the record, mark `Failed{reason}` with the last 40 log lines. **The Python's permanent orphan-plus-stale-active-endpoint on timeout cannot happen** — the failure path is the same code as `stop`. |
| Stop | SIGTERM → poll liveness 5 s → SIGKILL → poll 2 s. PID identity is verified (`start_ticks`) *before* any signal, so a reused PID is never killed. |
| Port conflicts | `port_free()` bind-probe **plus** a store lookup, before spawn, under `endpoints/<id>.lock`. Error is typed: `LaunchError::PortInUse { port, held_by: Option<EndpointId> }` — it names the endpoint holding it. Ports are allocated from `default_port_range` (8100–8199) when unspecified. |
| Reaping | `setsid` + parent exit → init reaps. No zombies, ever. |
| The RUNPATH trap | `build-vulkan/bin/llama-server` has a trailing-colon RUNPATH (= cwd on the library search path). The child env **always** gets `LD_LIBRARY_PATH=<dirname(binary)>` prepended, and cwd is set to the state dir. Deterministic; never relies on the caller's cwd. |

### 2.6 Swap-while-serving

The hard case, and the one that justifies the alias layer.

Clients address a **stable alias** (`default`, or `coder`, or `big`). Aliases resolve through an
`ArcSwap<RouteTable>` in `serve`. Endpoints come and go underneath.

**Hot swap (`apexrouter local swap coder --to <model>`), when memory allows:**

1. Start endpoint B on a *new* port. A keeps serving throughout.
2. Wait for B's `/health`. If it fails, abort — **A was never touched**, nothing degraded.
3. `routes.swap(alias → B)` — one `ArcSwap::store`, atomic, sub-microsecond. New requests go to B.
4. Drain A: poll `GET /slots` until `all(!is_processing)` or `drain_timeout` (60 s).
5. Stop A.

In-flight requests keep the upstream they captured at dispatch and finish against A. That matches
the Python's actual behaviour, which was correct by accident; here it is correct on purpose and
the response carries `X-ApexRouter-Endpoint: <id>` so a client can tell.

**Sequential swap (`--sequential`), when both models will not fit:**

1. Alias enters `Warming` state in the route table.
2. Stop A (drain first).
3. Start B.
4. **Requests that arrive during the window are parked**, not failed: they wait on a
   `tokio::sync::Notify` behind a bounded queue (`warm_queue_max`, default 32) up to
   `warm_timeout` (default 90 s). On success they proceed to B; on timeout or overflow they get a
   proper OpenAI-shaped `503` with `Retry-After`.

Parking beats 502-ing. An agent loop that hits a swap sees a slow request, not a broken one. This
is a ~40-line feature and it is the single most user-visible improvement over LocalRouter, where
switching backends mid-session simply broke every client (`05-proxy.md` §14 item 4).

**Provider switch** (`local → together`) is the degenerate case: no process involved, just an
`ArcSwap` store after a `Store::update`. Latency of the switch is one file write plus one notify
debounce.

---

## 3. API surface

### 3.1 HTTP — one listener, two namespaces

`serve` binds `127.0.0.1:8888` by default (`PROXY_PORT` env honoured, unlike LocalRouter where
`config.PROXY_PORT` and the proxy could disagree). Two namespaces on the one socket:

- **`/v1/*` plus the three legacy paths — the OpenAI-compatible proxy.** Byte-compatible with
  LocalRouter so existing clients don't notice a swap.
- **`/api/*` — the ApexRouter control plane.** *Deliberate deviation from the Imaginarium
  convention of `/v1/*` for control*: `/v1` is spoken for by the drop-in contract. `/api` follows
  the Prefrontal precedent.

#### Proxy namespace (drop-in compatible)

| Method | Path | Behaviour |
|---|---|---|
| `GET` `HEAD` | `/health` | `{"ok":true,"product":"apexrouter","version":"0.1.0","provider":"local","uptime":123.4}` — the union of the house health shape and the legacy one. Always 200. |
| `GET` `HEAD` | `/providers` | Legacy shape exactly (`active`, `target`, `providers{...}`, `local_instances[]`) **plus** `endpoints[]` and `routes[]`. Probes run **concurrently** with a 3 s cap (was ~8 s serial), and Together is probed from the full credential chain, not just `$TOGETHER_API_KEY` (fixes the documented inconsistency). |
| `POST` | `/switch` | Legacy bodies work verbatim: `{"provider":"together"\|"local"\|"vast-gguf", …}`. Extended with `{"provider":"endpoint","id":"…"}` and `{"alias":"…"}`. **Security fix:** an arbitrary `base_url` is only accepted with auth or `--allow-adhoc-targets`; otherwise the target must be a known provider/endpoint. Kills the credential-exfiltration primitive in `05-proxy.md` §11. |
| `GET` | `/v1/models` | **Aggregated**, not passed through: the union of route aliases and every reachable endpoint's models. `owned_by` names the endpoint. (Fixes §14 item 5.) |
| `*` | everything else | Forwarded to the resolved upstream. |

Routing rules that matter:

- **`/v1` doubling is fixed.** Upstream bases are stored *without* `/v1`. Incoming paths get
  leading `/v1` segments stripped, then exactly one `/v1` re-added for known OpenAI segments
  (`chat/completions`, `completions`, `embeddings`, `models`, `responses`, `rerank`, `audio/*`,
  `images/*`). Non-OpenAI paths (`/slots`, `/props`, `/metrics`) forward raw. **Both**
  `http://127.0.0.1:8888` and `http://127.0.0.1:8888/v1` work as client base URLs — the highest-
  risk drop-in incompatibility, closed.
- **Model-name routing.** The request body's `"model"` is matched against the alias table. Unknown
  or absent → the default route. If the target needs a different model id (Together), the body's
  `model` is **rewritten** to the alias's `model_id`. This is `05-proxy.md`'s #1 papercut ("model
  aliasing: Highest priority") and it is what makes provider switching invisible to clients.
- **Headers are constructed, never copied.** `fn outbound_headers(inbound: &HeaderMap, cred:
  Option<&Credential>) -> HeaderMap` builds from an allowlist. The client's `Authorization` never
  reaches an upstream unless the upstream's own credential put it there. Unit-tested.
- **One `reqwest::Client`** in `AppState` with keep-alive pooling, `no_gzip()/no_brotli()/
  no_deflate()` so bytes relay untouched and `Content-Encoding` never lies. (Was: a fresh
  `ClientSession` + full TLS handshake **per request**.)
- **Timeouts split**: `connect` 5 s, `headers` 120 s (a cold prompt-eval on a weak iGPU exceeds
  30 s), `idle` between stream chunks 300 s. **Never a total timeout on a stream** — that was the
  half-written-response bug.
- **Streaming** relays bytes as they arrive (`Body::from_stream`), never re-frames SSE, and only
  forces `Content-Type: text/event-stream` when the upstream status is 2xx *and* the upstream
  content type already says so. A `400 {"error":…}` on a `stream:true` request comes back as
  JSON, not as a mislabelled SSE body.
- **Usage is logged from the request path.** Non-streaming: parse `usage` + `timings` from the
  buffered JSON. Streaming: tee the byte stream and parse the terminal chunk. `X-Usage:
  "{prompt}+{completion}"` and `X-Provider` are preserved; `X-ApexRouter-Endpoint`,
  `X-ApexRouter-Request-Id` and `X-ApexRouter-Hop` (loop guard → `508`) are added.
- **Errors are OpenAI-shaped**: `{"error":{"message","type","code","param"}}`. Connect refused →
  `502`, upstream timeout → `504`, no route → `503`, oversized → `413`, loop → `508`.
- **Body limit** 64 MB (was a silent 1 MiB aiohttp default that 413'd long chat histories with an
  HTML page).

#### Control plane `/api/*`

```
GET    /api/health                     house shape: {ok, product, version}
GET    /api/snapshot                   the whole Snapshot (what the GUIs render)
GET    /api/ws                         Event stream (snapshot-on-connect)

GET    /api/rig                        GPUs, RAM, llama.cpp builds, backends
POST   /api/rig/rescan

GET    /api/models                     discovered local GGUFs (grouped shards, sizes)
GET    /api/models/{id}/fit?ctx=&parallel=&kv=&device=      the fit solver, live

GET    /api/endpoints
POST   /api/endpoints                  start (body = LaunchSpec); ?no_wait=true supported
GET    /api/endpoints/{id}
DELETE /api/endpoints/{id}             stop + forget
POST   /api/endpoints/{id}/stop|restart
GET    /api/endpoints/{id}/logs?tail=200&follow=1           SSE when follow
POST   /api/endpoints/swap             {alias, to: LaunchSpec, sequential?}

GET    /api/routes
PUT    /api/routes/{alias}             {target}
DELETE /api/routes/{alias}
POST   /api/routes/default             {alias}

GET    /api/profiles                   saved recipes
POST   /api/profiles                   save (usually "save this launch")
PUT    /api/profiles/{id}
DELETE /api/profiles/{id}
POST   /api/profiles/{id}/validate     re-validate: model gone? build gone? offer unrentable?

GET    /api/providers                  configured API providers + credential SOURCE (never the key)
PUT    /api/providers/{id}
POST   /api/providers/{id}/test        connection + optional completion probe

GET    /api/vast/account               credit, balance, can_pay  (never api_key)
GET    /api/vast/offers?profile=&geo=&max_dph=&num_gpus=
GET    /api/vast/gpu-names             live vocabulary for the dropdown
GET    /api/vast/instances
POST   /api/vast/rent                  {offer_id|auto, profile, launch, max_usd_per_hour, confirm}
DELETE /api/vast/instances/{id}
GET    /api/vast/instances/{id}/logs
POST   /api/vast/tunnel                {instance_id, local_port}
DELETE /api/vast/tunnel/{instance_id}

GET    /api/hf/models?q=               HF search (filter=gguf)
GET    /api/hf/models/{repo}/files     siblings + authoritative sizes via paths-info

POST   /api/smoke                      {base_url|alias} -> four probes with timings
GET    /api/doctor?only=               checks, concurrent, per-check timeout
GET    /api/usage?since=24h&by=provider|model|day
GET    /api/metrics                    ApexRouter's own Prometheus (NOT /metrics — that passes through)
POST   /api/shutdown                   graceful stop (admin scope)
```

Auth follows the house pattern exactly: bearer accepted as `Authorization: Bearer`,
`X-ApexRouter-Token`, or `?token=`; scopes read|write|admin derived from (path, method); loopback
bypass requires **both** an opt-in flag and a genuinely-loopback peer from
`ConnectInfo<SocketAddr>` (absent connect-info fails closed); a non-loopback bind **refuses to
start** without configured auth. Default posture: loopback + no token, so
`OPENAI_API_KEY=not-needed` keeps working. No `CorsLayer` on `/api` (same-origin UI); the proxy
namespace answers `OPTIONS` properly with an explicit origin allowlist instead of LocalRouter's
useless blanket `*` with no preflight handler. `TraceLayer` records method + path only — never the
query string, which can carry `?token=`.

### 3.2 CLI

`fn main() -> anyhow::Result<()>`; failures via `?`/`bail!` → anyhow prints `Error: …` to stderr,
exit 1. No colour crate, no emoji, no `std::process::exit`. Tracing to **stderr** always, because
`mcp` shares the binary and owns stdout.

```
apexrouter                              # bare invocation = status. Zero ceremony.
apexrouter status [--json] [--watch]
apexrouter serve [--bind ADDR] [--detach] [--stop] [--no-ui] [--allow-remote --token T]
apexrouter open                         # ensure serve, xdg-open the web UI
apexrouter gui                          # exec apexrouter-gui if installed, else `open`

apexrouter rig [--json]                                     # GPUs, RAM, builds, backends
apexrouter models ls [--json] | show <id> | fit <id> [--ctx N] [--parallel N] [--kv T]

apexrouter local start <model|profile> [--alias A] [--port N] [--ctx N] [--parallel N]
                                        [--kv q8_0] [--device Vulkan0] [--build ID]
                                        [--mode thinking|coding|nonthinking] [--no-wait]
apexrouter local stop <id> | --all
apexrouter local restart <id>
apexrouter local logs <id> [-f] [-n 200]
apexrouter local swap <alias> --to <model|profile> [--sequential]

apexrouter route ls | set <alias> <endpoint-id> | rm <alias> | default <alias>
apexrouter switch <local <name>|together [--model M]|vast-gguf|endpoint <id>>   # muscle memory

apexrouter profile ls | show <id> | save <id> --from <endpoint-id> | rm <id> | edit <id>

apexrouter vast account
apexrouter vast offers [--profile rtx3090] [--geo EU] [--max-price 1.20] [--json]
apexrouter vast rent <offer-id|--auto> --profile P --model REPO --quant Q
                     [--max-price X] [--geo G] --yes
apexrouter vast ls [--json] | watch <id> | logs <id> [-f] | destroy <id>|--all --yes
apexrouter tunnel up <instance-id> | down [<id>] | status

apexrouter hf search <query> [--json] | files <repo> [--json]

apexrouter smoke [--alias A | --base-url URL] [--json]
apexrouter usage [--since 24h|7d|all] [--by provider|model|day] [--json]
apexrouter doctor [--only <check>] [--json]
apexrouter config path | show | init | edit
apexrouter migrate [--dry-run]
apexrouter mcp
apexrouter version
```

House rules applied: `--json` is **per subcommand**, never global, and prints
`serde_json::to_string_pretty` of the protocol type and nothing else on stdout. Human output is
space-padded tables with an uppercase header row, `-` for missing values, and a friendly
parenthetical for empty states. Verb vocabulary is the house one (`ls / show / start / stop /
status / init / path`). Every money-spending command requires `--yes` or an interactive confirm
that shows `$/hr`, estimated total, and **current Vast credit**.

### 3.3 MCP

Hand-rolled newline-delimited JSON-RPC 2.0 over stdio, copied from
`Prefrontal-RS/prefrontal-cli/src/mcp.rs`: **echo the client's requested `protocolVersion` back**
(instant compatibility with every legacy revision, falling back to `"2024-11-05"`), tool failures
are results with `isError: true` and helpful text, JSON-RPC error codes reserved for protocol
breakage. Compact one-line JSON, all logging to stderr, exit on stdin EOF. Dual-era hedge: also
answer `server/discover` advertising `supportedVersions`, accept-and-ignore per-request `_meta`,
and emit `resultType: "complete"` — about 30 lines that buy compatibility with the 2026-07-28
revision without implementing streamable-HTTP (explicitly out of scope). Dispatch is
transport-agnostic (`fn dispatch(method, params) -> Result<Value, RpcError>`) so an axum route is
a day's work when ApexOS-RV nodes need it over the network.

All tools prefixed `apexrouter_*` (the three MCP servers share `~/Projects/.mcp.json`):

| Tool | Does | Notes |
|---|---|---|
| `apexrouter_status` | Full snapshot: endpoints, routes, proxy URL, rig summary, rentals, spend | the "where am I" tool |
| `apexrouter_rig` | GPUs, VRAM free/total, llama.cpp builds + their backends | |
| `apexrouter_models` | Discovered local models with sizes; `fit` estimate per model | |
| `apexrouter_endpoint_start` | Start a local model. Args: `model`, `alias?`, `ctx?`, `parallel?`, `device?`, `wait?` | returns the base URL to point a client at |
| `apexrouter_endpoint_stop` | Stop by id or alias | |
| `apexrouter_endpoint_logs` | Tail N lines | |
| `apexrouter_route_set` | Point an alias at an endpoint/provider; optionally make it default | |
| `apexrouter_switch` | Legacy `/switch` semantics for agents that already know them | |
| `apexrouter_smoke` | Run the four probes; returns pass/fail + TTFT + tok/s | |
| `apexrouter_usage` | Tokens + cost by provider/model/day over a window | |
| `apexrouter_doctor` | Run the check registry; returns failures with fixes | |
| `apexrouter_vast_offers` | Live offer search by profile | read-only, free |
| `apexrouter_vast_rent` | **Spends money.** Requires `confirm: true` **and** `max_usd_per_hour`. Without them it returns `isError:true` carrying the full cost preview and current credit — a dry run that shows the bill. | |
| `apexrouter_vast_destroy` | Destroy an instance; verifies before forgetting the id | |
| `apexrouter_hf_files` | GGUF files + exact sizes for a repo (feeds the fit solver) | |

Tool descriptions are long and operational — an agent should be able to go from
`apexrouter_status` to a working `OPENAI_BASE_URL` without reading a doc.

---

## 4. Data model

### 4.1 Core types (`apexrouter-protocol`)

Plural everywhere, per `00b-brief-amendment-scale.md`: N GPUs, N builds, N concurrent endpoints, N
backends. Nothing named "the endpoint".

```rust
pub struct Snapshot {
    pub endpoints: Vec<Endpoint>,
    pub routes: Vec<Route>,
    pub rig: Rig,
    pub proxy: ProxyStatus,
    pub rentals: Vec<Rental>,
    pub providers: Vec<ProviderStatus>,
    pub totals: Totals,          // tokens + spend today / 7d / all
    pub as_of_unix: i64,         // the UI shows staleness, never pretends
}

#[derive(PartialEq)]  pub struct Endpoint {
    pub id: EndpointId,          // newtype; slug, validated charset
    pub alias: Vec<String>,      // aliases pointing here
    pub kind: EndpointKind,
    pub model: ModelRef,
    pub base_url: String,        // WITHOUT /v1
    pub state: EndpointState,    // computed, never stored
    pub health: Health,
    pub port: Option<u16>,
    pub devices: Vec<String>,    // e.g. ["Vulkan0"]
    pub started_at_unix: i64,
    pub stats: Option<EndpointStats>,   // slots busy/total, tok/s, ctx used, requests
    pub provenance: Provenance,         // discovered_at, size_bytes, fit_estimate
}

#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EndpointKind {
    LocalLlama { pid: u32, build: BuildId, log: String },
    Remote     { label: String },                 // any OpenAI-compatible URL (LAN node, tunnel)
    Vast       { instance_id: u64, tunnel: Option<TunnelInfo> },
    Api        { provider: ProviderId },          // together, etc.
}

#[serde(tag = "state", rename_all = "snake_case")]
pub enum EndpointState { Starting, Warming, Ready, Degraded, Stopping, Stopped, Failed { reason: String } }

pub struct Rig { pub gpus: Vec<Gpu>, pub ram_total_mb: u64, pub ram_free_mb: u64,
                 pub swap_used_mb: u64, pub builds: Vec<Build> }
pub struct Gpu { pub index: u32, pub name: String, pub backend: Backend,
                 pub vram_total_mb: u64, pub vram_free_mb: u64, pub driver: Option<String> }
#[serde(rename_all = "snake_case")]
pub enum Backend { Vulkan, Cuda, Rocm, Metal, Cpu, Other(String) }
pub struct Build { pub id: BuildId, pub path: String, pub label: String,     // "build-vulkan"
                   pub backends: Vec<Backend>, pub build_info: Option<String>,
                   pub devices: Vec<String>, pub flags: FlagSupport }        // feature-detected

pub struct Route { pub alias: String, pub target: RouteTarget, pub is_default: bool,
                   pub model_id: Option<String> }   // rewrite the body's "model" to this
#[serde(tag = "target", rename_all = "snake_case")]
pub enum RouteTarget { Endpoint { id: EndpointId }, Provider { provider: ProviderId, model_id: String } }

pub struct Rental { pub instance_id: u64, pub gpu_name: String, pub num_gpus: u32,
                    pub dph_total: f64, pub geolocation: String,
                    pub created_at_unix: i64, pub destroyed_at_unix: Option<i64>,
                    pub phase: BootPhase, pub est_cost_usd: f64, pub ssh: Option<Ssh>,
                    pub download: Option<DownloadHealth> }   // stall detector output
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum BootPhase { Reserved, Provisioning, Pulling, Compiling, Downloading, Loading,
                     Healthy, Failed { reason: String }, Destroyed }

pub struct Offer { /* ~25 named fields */ #[serde(flatten)] pub extra: Map<String, Value> }

pub struct Fit { pub ctx: u32, pub parallel: u32, pub kv_type: KvType, pub ngl: NglPlan,
                 pub weights_mb: u64, pub kv_mb: u64, pub compute_mb: u64,
                 pub headroom_mb: i64, pub verdict: FitVerdict }
pub enum FitVerdict { Fits { headroom_mb: u64 }, Tight { headroom_mb: u64 },
                      NeedsOffload { layers_on_gpu: u32 }, WontFit { short_by_mb: u64 } }

#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event { Snapshot(Box<Snapshot>), EndpointChanged { endpoint: Box<Endpoint> },
                 EndpointRemoved { id: EndpointId }, RouteChanged { routes: Vec<Route> },
                 RentalChanged { rental: Box<Rental> }, Log { endpoint: EndpointId, line: String },
                 Request(RequestRecord), Notice { level: Level, text: String } }
```

Money is a `Money` newtype over integer micro-USD so float dust cannot accumulate, and every
estimate is tagged `Estimate::Metered` vs `Estimate::Approximate` so the UI can never present a
guess as a fact. `Secret<String>` prints `***` in `Debug`/`Display`; its only accessor is
`.expose()`. `ProviderStatus` reports the credential **source** (`"~/.config/vastai/vast_api_key"`),
never the key.

Legacy spellings are handled by serde aliases on one enum, not by normalising the data:
`#[serde(alias = "vast_gguf", alias = "vast-gguf", alias = "local-gguf")]`. The wire keeps
`vast-gguf`.

### 4.2 The fit solver — the highest-value new feature

`06-recipe-schema.md` is right that the 54 hand-written vast recipes are a *frozen function*: their
`description` strings literally contain the VRAM arithmetic that produced them. One pure function
replaces all of them and works for models published tomorrow.

```rust
pub struct FitInput {
    pub weights_bytes: u64,           // from HF paths-info, or the local file (shards summed)
    pub n_layer: u32,
    pub n_head_kv: u32,
    pub n_embd_head_k: u32,
    pub n_embd_head_v: u32,
    pub n_ctx_train: u32,
    pub full_attn_layers: Option<u32>,   // hybrid-linear models: only these carry KV
    pub budget_bytes: u64,               // Σ free VRAM over the selected devices, minus margin
    pub want_ctx: Option<u32>,
    pub want_parallel: Option<u32>,
}
pub fn fit(i: &FitInput) -> Fit;
```

KV bytes = `kv_layers × ctx × n_head_kv × (n_embd_head_k + n_embd_head_v) × bytes_per_elem(kv_type)`.
Compute buffer is estimated from batch size and calibrated against the archived run log (Qwen3.5-9B
Q4_K_M, ctx 32768, kv q8_0, Vulkan → 5956 MiB device: 4861 model + 594 context + 501 compute).
Callable from `apexrouter models fit`, the launch screen (live, as you drag ctx), the Vast rent
screen ("what fits on 2×3090?"), and MCP. Pure, unit-tested against the real GGUFs on the box.

### 4.3 Config file shape

`$APEXROUTER_CONFIG` → `$APEXROUTER_HOME/config.toml` → `~/.config/apexrouter/config.toml`.
**Every field defaults**, so a missing file is a fully working zero-config setup. Runtime-only
fields are `#[serde(skip)]`; a separate `ConfigFile` is what gets written. `config.example.toml`
sits at the repo root, fully commented.

```toml
[server]
bind = "127.0.0.1:8888"                 # PROXY_PORT env overrides the port, and is honoured
allow_localhost_no_auth = true
token_env = "APEXROUTER_TOKEN"
ui_dir = ""                             # empty = the assets embedded in the binary

[proxy]
connect_timeout_secs = 5
headers_timeout_secs = 120
idle_timeout_secs    = 300              # between stream chunks; never a total stream timeout
warm_timeout_secs    = 90               # how long a request parks during a sequential swap
warm_queue_max       = 32
drain_timeout_secs   = 60
max_body_mb          = 64
request_log_size     = 200              # in-memory ring buffer

[endpoints]
model_roots  = ["~/models", "~/.cache/huggingface/hub"]
build_roots  = ["~/llama.cpp", "~/Projects/llama.cpp", "/usr/local/bin"]
port_range   = [8100, 8199]
restart      = "never"                  # never | on-failure
health_interval_secs = 10
start_timeout_secs   = 180
default_mode = "thinking"

[providers.together]
base_url    = "https://api.together.ai/v1"
api_key_env = "TOGETHER_API_KEY"        # named, never stored inline

[vast]
api_key_env  = "VAST_API_KEY"
api_key_file = "~/.config/vastai/vast_api_key"
tunnel_local_port = 8800
poll_interval_secs = 5                  # never faster: Vast publishes no rate limits
max_boot_minutes   = 25                 # watchdog: destroy a wedged instance rather than bill forever

[vast.profiles.rtx3090]                 # the fixed tiers Andre asked for, as QUERY TEMPLATES
label = "RTX 3090 ×2–4"
gpu_name = ["RTX 3090"]                 # live vocabulary; never a hardcoded enum
num_gpus_min = 2
num_gpus_max = 4
max_dph = 1.20
min_reliability = 0.98
min_inet_down = 300
min_disk_gb = 80
image_type = "prebuilt"

[vast.profiles.h100]
label = "H100 ×1–2"
gpu_name = ["H100 SXM", "H100 NVL", "H100 PCIE"]
num_gpus_min = 1
num_gpus_max = 2
max_dph = 4.00
min_disk_gb = 150
image_type = "builder"

[hf]
token_file = "~/.cache/huggingface/token"

[docker]                                # genuine config: Andre publishes these
prebuilt = "ghcr.io/buckster123/vastai-gguf:prebuilt"
builder  = "ghcr.io/buckster123/vastai-gguf:builder"
vllm     = "ghcr.io/buckster123/vastai-gguf:vllm"

[compat]
localrouter_dir = ""                    # set to mirror .active_endpoint for the Python TUI
read_legacy_state = true                # read ~/.vastai-gguf for usage/providers/instances
```

**Credential resolution — one function, one order** (fixing the file-first/env-first split that
had the TUI and the proxy disagreeing):

1. explicit `api_key` in our config (documented as discouraged, `skip_serializing_if`)
2. `api_key_file` in our config
3. conventional third-party path — `~/.config/vastai/vast_api_key`, `~/.cache/huggingface/token`,
   `~/.vastai-gguf/config.toml [providers.together]`
4. the env var named by `api_key_env`

Returns `(Secret<String>, CredSource)`. A borrowed credential is **never copied into our config
file**, never logged, never echoed, never placed in an argv. For rented instances exposed on a
public port, `llama-server` gets a freshly minted per-instance key via `--api-key-file`, not
`--api-key`, so it stays out of `/proc/*/cmdline`.

### 4.4 State persistence

`state.json` is a single versioned document:

```jsonc
{
  "schema_version": 1,
  "endpoints": [ { "id": "carnice-9b", "kind": "local_llama", "pid": 41233,
                   "start_ticks": 8123441, "port": 8100, "build": "build-vulkan",
                   "model_path": "/home/andre/models/carnice-9b/Carnice-9b-Q6_K.gguf",
                   "argv_hash": "…", "devices": ["Vulkan0"], "started_at_unix": 1785000000,
                   "provenance": { "discovered_at_unix": …, "size_bytes": …, "fit": {…} } } ],
  "routes":   [ { "alias": "default", "target": {"target":"endpoint","id":"carnice-9b"},
                  "is_default": true } ],
  "tunnels":  [ … ],
  "providers":[ { "id": "together", "last_ok_unix": …, "last_error": null } ]
}
```

Note what is **absent**: no `status` string, no `active_endpoint` singleton. "Active" is
`routes.default`. Liveness is computed.

Append-only, human-tailable, never rewritten:

- `usage.jsonl` — **the legacy field names are preserved exactly** so
  `cost.py::get_session_costs()` still parses it: `{timestamp, epoch?, provider, model_id,
  prompt_tokens, completion_tokens, cost_usd}`. New optional fields are additive and ignored by
  the old reader: `alias`, `endpoint_id`, `ttft_ms`, `tok_per_s`, `stream`, `estimate` (metered vs
  approximate). Timestamps are **real UTC RFC 3339** going forward; legacy local-time-with-a-lying-Z
  values parse leniently. Rotated at `usage_rotate_mb`.
- `instances.jsonl` — the Vast ledger that replaces the single-slot `.last_instance`:
  `{instance_id, state: reserved|confirmed|running|destroyed|orphan_suspect, offer_id, gpu, dph,
  created_at, destroyed_at, est_cost_usd}`. "Active" is a query, not a file that holds one thing.

**Billing safety** is the ledger's reason to exist. `PendingLaunch` is a guard: `ledger.reserve
(&spec)?` writes a `reserved` row **before** the create call; `pending.commit(id)?` promotes it;
`impl Drop for PendingLaunch` writes `orphan_suspect` if the guard dies uncommitted (Ctrl-C in the
sub-second window, a panic, a crash). At every startup, `reconcile()` diffs the ledger against
`GET /api/v0/instances/` and surfaces anything billing that we don't have a live record for. A
`tokio::signal` handler sets a shutdown flag rather than killing mid-critical-section.

### 4.5 Backwards compatibility with `~/.vastai-gguf`

Read, always (when `[compat] read_legacy_state`):

| Legacy path | Used for |
|---|---|
| `~/.vastai-gguf/config.toml` | `[providers.together]` base_url + api_key — read with a **real TOML parser**, step 3 of the credential chain. Never rewritten. |
| `~/.vastai-gguf/usage.log` | merged into every `usage` aggregate (`epoch` optional, unknown fields ignored, no row ever fails a load) |
| `~/.vastai-gguf/local_instances/*.json` | imported as endpoints by `apexrouter migrate`; **paths validated on load** — the saved instance points at a model that no longer exists, and that is the normal case, not an edge case |
| `~/.vastai-gguf/local_logs/` | offered in the logs view as historical |
| `~/.vastai-gguf/.pinned_provider` | imported as a route/profile once (a live file pins `deepseek-ai/DeepSeek-V4-Pro`) |
| `<LocalRouter>/.active_endpoint` | all **four** legacy shapes deserialize into one enum via serde aliases (`activated_at` \| `switched_at`, with/without `pid`) |
| `<LocalRouter>/.last_instance`, `.instance_history`, `.hf_pin` | seeded into the ledger / profiles; trailing newlines always trimmed |
| `recipes.toml` | `apexrouter migrate` imports the 7 `llama_cpp_repo`/`llama_cpp_ref` fork mappings as a `known_forks` table (undiscoverable knowledge), the docker image map, and the tier price hints. The other 64 recipes are **not** imported — they are a stale catalogue superseded by discovery, and `migrate --dry-run` says so per row. |

Write, for interop during migration (opt-in, one config line):

- `/tmp/vastai-gguf-proxy.pid` while serving — so the Python TUI's proxy status keeps working.
- `<localrouter_dir>/.active_endpoint` mirrored on every route change, in the legacy shape.

Nothing is ever written into the ApexRouter repo directory. Ever.

---

## 5. GUI plan

Both GUIs render the **same `Snapshot`** deserialized from the **same protocol crate**, over the
**same HTTP+WS API**. The Slint app is an edge client: it has no business logic, links only
`apexrouter-protocol`, and ships a ~200-line `api.rs` `NodeClient { http, base, token }` with a
private `auth()` wrapper. There is no second code path, and the GPL boundary stays clean.

Both start the same way: `ensure_serve()` (§2.3). The web UI can only exist if `serve` runs; the
Slint app spawns it if it isn't up. Neither GUI ever touches the state files directly.

### 5.1 Web UI (`ui-web/` — three files, no npm, no CDN, no build step)

`index.html` is a static skeleton of empty ids with `<!-- injected -->` comments; `app.js` is
`"use strict"` with module-level state, `el(tag,cls,text)` + `$(id)` helpers,
`container.replaceChildren()` re-render, `textContent` everywhere; `style.css` is a `:root` token
block with `color-scheme: dark` and a `@media (prefers-color-scheme: light)` override. Status
colours are reserved for health and never for identity; badges pair icon + label, never colour
alone; body text is system sans with monospace strictly for code and log panes. WS first with a
REST first-paint fallback, exponential reconnect backoff 1 s → ×2 → cap 15 s, a single connection
dot as the failure reporter, `setInterval(render, 60_000)` to keep relative timestamps honest.
Every element that toggles `hidden` and declares its own `display` gets a `[hidden]{display:none}`
guard.

Served by `rust-embed` pointing straight at `../../ui-web` — no `dist/`, no vite. `ui_dir` in
config is the live-reload escape hatch.

| Screen | What the operator sees | What they do |
|---|---|---|
| **Rig** (home) | Stat tiles: endpoints up, GPUs, VRAM free/total per device, RAM+swap, requests/min, spend today, Vast credit. The **proxy card**: `http://127.0.0.1:8888/v1`, a copy button, `OPENAI_API_KEY=not-needed`, uptime, active route. Endpoint cards with state badge, model, port, tok/s, slots busy/total, ctx used. | copy the base URL; stop/restart an endpoint; jump into logs |
| **Launch** | Discovered models (grouped shards, total size, "fits / tight / needs offload / won't fit" badge computed live). Build picker with detected backends. Device checkboxes. ctx / parallel / kv sliders that re-run the fit solver on every change, showing weights + KV + compute + headroom as a stacked bar. | Start. "Save as profile". "Start and make default". |
| **Endpoints** | Table + detail pane: live log tail (SSE), slots, per-request timings, the exact argv used. | stop, restart, **swap model** (hot or sequential), rename alias |
| **Routes** | Alias table: alias → target, default marker, model rewrite. A "test" button running a 5-token completion. | add/point/remove aliases, set default |
| **Cloud** | Vast credit + burn-down banner. Profile chips (3090 ×2–4, H100 ×1–2, + custom). Live offer table, sortable, with $/hr, reliability, pooled VRAM, Mbps, CUDA, geo — and a **`gpu_name` dropdown populated from a live query**, not a constant. Rentals list with `BootPhase`, elapsed timer, log stream, download-stall alert with a one-click restart, tunnel toggle, and a big Destroy. | rent (behind a confirm showing $/hr + est. total + credit), tunnel up/down, watch boot, destroy |
| **Catalog** | Local models; HF search (`filter=gguf`) with exact per-file sizes from `paths-info`; saved profiles with staleness flags ("model file gone", "build removed"). | pin a quant, create a profile, launch from a repo |
| **Usage** | Tokens and spend over time; by provider / model / day; metered-vs-estimated marked. | change the window |
| **Doctor** | Check list with pass/fail badges, timings, and a fix line each. Smoke test with four named probes, TTFT and tok/s. | run all, run one |

### 5.2 Slint app (`ui-slint/` — `apexrouter-gui`)

Never `#[tokio::main]`. `fn main() -> anyhow::Result<()>` builds a multi-thread runtime manually,
keeps it alive for the app's lifetime, and ends with `ui.run()?`. Slint owns the main thread;
every UI touch crosses back via `Weak` + `invoke_from_event_loop` / `upgrade_in_event_loop`.
`build.rs` is one `slint_build::compile("ui/app.slint")` line. Palette hardcoded to match the web
tokens exactly (`#0d0d0d` page, `#1a1a19` surface, `#2c2c2a` hairline, `#ffffff` ink, `#c3c2b7`
ink-2, `#898781` muted, `#3987e5` accent, `#0ca30c` good, `#fab219` warn, `#ec835a` serious,
`#d03b3b` critical).

mk1 Slint scope — deliberately a subset, stated honestly:

| Screen | mk1 | Notes |
|---|---|---|
| Rig | ✅ full | endpoint cards, GPU meters, proxy card with a copy button |
| Endpoints | ✅ start / stop / restart / log tail | the swap flow is web-only in mk1 |
| Launch | ✅ model list + fit badge + Start | slider-driven live fit is web-only in mk1 |
| Routes | ✅ set default, point alias | |
| Cloud | 🔎 read-only + Destroy | renting stays in the web UI in mk1 (money + a wide form) |
| Usage | ✅ read-only totals | charts are web-only |
| Doctor / Smoke | ✅ run + badges | |

The port map (web screen → Slint screen) goes in `docs/SLINT.md` as a table, per house convention.

---

## 6. mk1 scope

Ordered slices. **The cut line for "mk1 ships" is after slice 11.** Slices 12–14 are mk1.1 and are
listed so the shape is visible; everything past them is explicitly deferred at the bottom.

1. **Workspace skeleton + `apexrouter-protocol`.** All wire types, `Event`, serde aliases for
   every legacy provider spelling. `cargo fmt`/`clippy -D warnings` green from commit one.
2. **`core`: paths, config, secrets, credential chain, `Store` with flock + atomic
   read-modify-write, `Liveness`, `exec` (argv-only, mandatory timeouts), `spawn_detached`.**
   Verified by `apexrouter config show` and `apexrouter status` on an empty machine.
3. **Discovery.** `build*/bin/llama-server` glob (finds `build-mtp` and `build-zaya1`, which
   LocalRouter's fixed candidate list misses), backend detection via `--list-devices` (**never**
   grepping `--help`, which contains zero occurrences of vulkan/cuda/hip/rocm on this box) with a
   sibling-`.so` fallback, `llvmpipe` excluded, `--help` flag feature-detection, recursive GGUF
   scan with shard grouping and symlink follow, GGUF header reader, `Rig` assembly.
   Gate: `apexrouter rig` and `apexrouter models ls` are correct on the real machine.
4. **Fit solver.** Pure, unit-tested against `Carnice-9b-Q6_K.gguf` and calibrated to the archived
   5956 MiB run log. `apexrouter models fit`.
5. **Local endpoint lifecycle.** One argv builder (with `--top-k 20` restored, `--jinja` dropped
   because it is default-on in b9199, `-fa on|off|auto`, `LD_LIBRARY_PATH` set for the RUNPATH
   trap, `-dev` device list, `--alias`), port pre-check, detached spawn, real-deadline health gate
   with full cleanup on failure, verified-identity stop, log capture with rotation, computed
   status. Gate: **start/stop `Carnice-9b-Q6_K.gguf` on `build-vulkan` ten times with no orphans,
   no zombies, no stale state.**
6. **Routes + aliases.** `RouteTable`, default route, model-id rewrite. CLI `route`/`switch`.
7. **The proxy.** axum, `/v1` normalisation accepting both client base URLs, alias routing,
   constructed outbound headers, one pooled client, split timeouts, byte-relay streaming, OpenAI-
   shaped errors, usage logging from the request path (streaming and not), `X-Provider`/`X-Usage`
   preserved, loop guard, legacy `/health` + `/providers` + `/switch` byte-compatible, aggregated
   `/v1/models`. Gate: **`smoke.sh http://127.0.0.1:8888` passes unmodified**, and so does
   `OPENAI_BASE_URL=http://localhost:8888/v1`.
8. **`serve` + control plane + WS + `ensure_serve`.** `/api/*`, snapshot-on-connect, the state
   watcher, the health poller, the request ring buffer, auth + loopback gate, graceful drain.
9. **Swap.** Hot swap and sequential swap with the warming queue. Gate: a client loop of
   completions survives `apexrouter local swap default --to <other model>` with zero errors (hot)
   and zero errors but visible latency (sequential).
10. **The CLI, complete for local.** `status`, `rig`, `models`, `local *`, `route`, `switch`,
    `serve`, `usage`, `smoke`, `doctor`, `config`, `migrate`, `open`. `--json` everywhere.
11. **Both GUIs.** `ui-web` three files with the seven screens above (Cloud read-only at this
    point); `ui-slint` with its mk1 subset. Gate: **the model from slice 5 is visible, startable
    and stoppable in both**, and the proxy base URL is one click to copy in both.

    — *mk1 ships here.* —

12. **MCP.** All 15 tools, dual-era handshake, registered in `~/Projects/.mcp.json` pointing at
    `target/release/apexrouter`.
13. **Vast, read + rent + destroy.** REST client (`PUT /api/v0/search/asks/` with the verified `q`
    object, `PUT /asks/{id}/`, `GET /instances/`, `DELETE /instances/{id}/`), the four profiles as
    query templates, live `gpu_name` vocabulary, the ledger with the `PendingLaunch` drop guard and
    startup reconciliation, credit + burn-down in every confirm, `BootPhase` watcher with the
    `max_boot_minutes` auto-destroy watchdog, SSH tunnel with an owned `Child`, per-instance
    `ControlPath`, `ExitOnForwardFailure=yes`, `ServerAliveInterval=30`, and a project-scoped
    `UserKnownHostsFile` with `accept-new`. The rented endpoint appears as a normal `Endpoint`, so
    routes, swap, usage and both GUIs work on it with zero new code.
14. **HF + profiles + Together.** HF search + `paths-info` sizing feeding the fit solver, gated-repo
    classification, `profiles.toml` via `toml_edit` (comment-preserving, atomic), the `Provider`
    trait with Together (models, pricing from the model objects, chat passthrough, 429 handling via
    `x-ratelimit-reset`), the `known_forks` table.

**Deliberately deferred past mk1 — stated, not forgotten:**

- **vLLM launch.** The env contract (`launch_vllm.sh`, 16 vars) is produced by `vast/onstart.rs`
  and is in the type system from day one, but no GUI flow ships in mk1 and it is untested. The
  Python wizard's separate vLLM branch is *not* reproduced — it becomes a facet of one launch form.
- **Deep SSH diagnostics** (the four remote probes) and **download-stall recovery**. The 4-second
  eth0 RX sample and the `<1000 bytes = STALLED` threshold are valuable and are kept in the design
  (`DownloadHealth` on `Rental`, a passive alert on the instance card with one-click restart), but
  they land with slice 13+ hardening, not mk1.
- **Builder-image fork compile UI.** `known_forks` + "fork implies `image_type=builder` implies
  +12–18 min cold start" is data in mk1; the GUI for authoring new forks is later.
- **Batch provider comparison.** Redesigned as a real concurrent side-by-side over the alias table,
  including local endpoints (which the Python excluded entirely) — but not in mk1.
- **Together model browser** with family drill-down. mk1 has provider config, connection test,
  live `/models`, and passthrough; the browse-and-pin UI is later.
- **Full recipe/tier/docker CRUD editor.** mk1 has "save this launch as a profile", profile
  validation and profile delete — which is the useful 80%. The depth-6 nested editor is not
  reproduced at all; it was a terminal artefact.
- **llama.cpp router mode** (`--models-dir`, `POST /models/load`) — noted as the mk2 simplification
  of our own supervision job, deliberately not adopted in mk1 because direct supervision matches
  the state model and the failure modes we understand.
- **Anthropic `/v1/messages` translation.** Passed through untouched; flagged as an open question.
- **MCP streamable-HTTP transport.** Dispatch stays transport-agnostic; the axum route is a day's
  work when ApexOS-RV needs it.
- **LAN node discovery.** The `Remote` endpoint kind exists and works if you type a URL; automatic
  discovery is not architected out, just not shipped.

### 6.1 Capability coverage

Every `must_port = yes` and `redesign` item from the inventory, mapped. Slice numbers refer to §6.

| Area | Capability | Where it lands | Slice |
|---|---|---|---|
| tui | live status panel → async snapshot | `Snapshot` + WS + GUI Rig screen | 8, 11 |
| tui | main menu → Serve/Connect/Inspect/Catalog | GUI screens; CLI noun groups | 10, 11 |
| tui | vast launch wizard → discovery flow | Cloud screen: profile → offers → model → fit → confirm | 13 |
| tui | vLLM variant | a facet of one launch form; env contract in `vast/onstart.rs` | deferred |
| tui | offer browser | live sortable table, REST | 13 |
| tui | Together activation | `Provider` trait + Routes screen | 14 |
| tui | local launch wizard | Launch screen + `local start` | 5, 10, 11 |
| tui | llama argv construction | `llama/args.rs` (one builder, both targets) | 5 |
| tui | local status / logs / stop | Endpoints screen + `local logs -f` (SSE) | 5, 11 |
| tui | local hardware & model discovery | `discover/*`, fixed (glob builds, `--list-devices`) | 3 |
| tui | provider configuration | `/api/providers`, real serializer, 0600, key by reference | 14 |
| tui | Together model catalog | live `/models`; browse UI deferred | 14 |
| tui | HF browser + quant pin | `hf.rs` + Catalog screen; exact filename, not a substring | 14 |
| tui | batch compare | deferred (redesigned as concurrent, includes local) | deferred |
| tui | boot watcher | `BootPhase` + auto-tunnel + `max_boot_minutes` watchdog | 13 |
| tui | deep diagnostics | `doctor.rs` registry, concurrent, `--only`; SSH probes later | 10 / deferred |
| tui | stall detection + recovery | `DownloadHealth` + passive alert + one-click restart | deferred |
| tui | usage & cost tracking | `usage.jsonl` (legacy-compatible) + `pricing.rs` + Usage screen | 7, 10 |
| tui | vast fleet list | Cloud rentals table with per-row actions + hourly burn total | 13 |
| tui | recipe / tier / docker CRUD | `profiles.toml` via toml_edit; "save as profile" | 14 |
| tui | editor save / reload / dirty | atomic tmp+rename, comment-preserving, no global dirty bool | 14 |
| tui | SSH tunnel management | a toggle + live indicator on the rental card; owned `Child` | 13 |
| tui | unified local proxy | slices 7–8 — the product | 7 |
| tui | smoke test | `smoke.rs`, four native probes, badges + TTFT + tok/s | 7, 10 |
| tui | destroy instance | verify-before-forget; shows GPU, geo, uptime, accrued cost | 13 |
| core | path/constant registry | `paths.rs`, XDG + legacy read paths | 2 |
| core | recipes.toml loader | `migrate.rs` (import), not a runtime dependency; no `exit(1)` | 14 |
| core | recipe CRUD + validation | `profiles.rs`, `TryFrom<Draft>` is the only constructor | 14 |
| core | provider config load/save | one parser, one writer, 0600, unknown keys preserved | 14 |
| core | Together connection + completion test | `providers/together.rs`, deduplicated, with backoff | 14 |
| core | endpoint activation / resolution | `route.rs` — ONE resolver for proxy, CLI and GUI | 6, 7 |
| core | cost estimation | `pricing.rs`; recipe/API prices authoritative, table as fallback | 7, 14 |
| core | usage logging + aggregation | `usage.rs`, real windows, rotation, logged from the proxy | 7 |
| core | rate-limit probe | Together headers + a real token bucket + 429 backoff | 14 |
| core | HF browser / quant + provider pinning | `hf.rs` + profiles; legacy pins imported | 14 |
| core | shell/subprocess helpers | `exec.rs` — argv only, no `sh -c` anywhere, CI grep | 2 |
| core | PID liveness | `proc.rs` — pure predicate, `Unknown` variant, start-ticks | 2 |
| core | local instance metadata | `state.json` facts + validated paths on load | 2, 5 |
| core | HF token + formatting utils | `hf.rs`, `render.rs` | 3, 14 |
| core | sampling presets + wizard maps | one table (launch.sh authoritative, `--top-k 20` restored) | 5 |
| local | binary / backend / model discovery | slice 3 (all three defects fixed) | 3 |
| local | binary selection | `BinaryChoice::{Exact, Fallback{got,wanted}, None}` — no silent substitution | 5 |
| local | spawn / health poll / stop / list | slice 5 (setsid, no fd leak, real deadline, verified kill) | 5 |
| local | log capture and viewing | rotation on start (crash evidence survives), char-safe tails | 5 |
| vast | offer search (both paths) | **unified** — one search, one threshold set, geo never silently dropped | 13 |
| vast | gpu filter / geo / price / CUDA gating | `vast/offers.rs`, profiles as query templates | 13 |
| vast | create + response parsing + billing guards | `vast/rent.rs`, `new_contract`, ledger, drop guard | 13 |
| vast | container provisioning env (llama.cpp + vLLM) | `vast/onstart.rs`, exact 25-var contract | 13 |
| vast | builder compile / HF download / weight discovery | preserved in the env contract; images unchanged | 13 |
| vast | tunnel up / status / down / logs | `vast/tunnel.rs` | 13 |
| vast | instance status / listing / reattach | `vast/api.rs` + rentals table | 13 |
| vast | cold-start estimate | shown in the rent confirm | 13 |
| recipes | catalogue / tiers → discovery + profiles | slices 3, 4, 13, 14 | |
| recipes | docker registry, mmproj, geo, forks | config `[docker]`, auto-detected mmproj, `known_forks` | 13, 14 |
| recipes | VRAM fit reasoning | `fit.rs` — one function replacing 54 hand-solved rows | 4 |
| ext | Vast REST (search/create/list/status/destroy/logs/exec/auth/429) | `vast/api.rs` | 13 |
| ext | Vast port mapping | tolerant `serde_json::Value` accessor (docs and CLI disagree) | 13 |
| ext | Vast exposure posture | SSH tunnel default; public port opt-in + mandatory `--api-key-file` | 13 |
| ext | Together models/pricing/chat/429 | `providers/together.rs` (bare-array deserializer, `eos` as String) | 14 |
| ext | HF search / sizes / gated / token | `hf.rs` (`paths-info` authoritative) | 14 |
| ext | llama-server launch / health / props / slots / metrics / timings | `llama/args.rs`, `llama/api.rs` | 5, 7 |
| ext | llama-server router mode | evaluated, deferred to mk2 with a written reason | deferred |
| ext | MCP stdio + dual-era | `cli/mcp.rs` | 12 |
| ext | SSE relay | byte-for-byte relay; parsed tee only for usage | 7 |
| ext | SSH tunnel supervision | `vast/tunnel.rs` | 13 |
| conv | all house conventions | §1, §3, §5 throughout | all |
| conv | storage: files not sqlite | §4.4 — decided: files, `cat`-able and `tail`-able | 2 |
| conv | secret handling | `secret.rs` + the credential chain | 2 |

Dropped, per the inventory: the pin-file handoff (replaced by direct row actions),
`press_enter`/`← Back`/`console.clear`, the `MODEL_GPU` preset case table, the hardcoded Together
catalogue (three copies), `prebuilt_legacy`, and the Python packaging entry point.

---

## 7. Risks

1. **No daemon means no auto-restart when nothing is running.** A model that dies at 03:00 with no
   `serve` up stays dead until the next read. Mitigation: honest documentation, `restart =
   "on-failure"` when `serve` runs, and a `doctor` check that names dead endpoints. If this ever
   hurts, the fix is a systemd user unit for `serve` — an *addition*, not a redesign.
2. **`ensure_serve` racing itself.** Two surfaces starting simultaneously. Mitigated by
   `flock(serve.lock, LOCK_NB)` plus a health poll, but the window is real and needs a test that
   spawns eight `apexrouter local start` calls at once.
3. **`state.lock` contention under a hot loop.** `Store::update` is microseconds, but a `--watch`
   loop plus a busy `serve` plus a GUI could serialise. Mitigation: reads never take the exclusive
   lock (atomic rename means a lock-free read is always consistent); only writes do.
4. **The sequential-swap warming queue is a new failure mode.** Parked requests hold connections;
   a client with its own 30 s timeout gives up before `warm_timeout`. Mitigation: bounded queue,
   `Retry-After`, and the warming state broadcast so both GUIs show "warming, N parked".
5. **Detached children survive a manager bug.** If the store loses an endpoint record, a
   `llama-server` holding 6 GB becomes invisible. Mitigation: `doctor` scans for `llama-server`
   processes on our port range that we have no record of and offers to adopt or kill them.
6. **Vast billing.** The single most expensive failure class. Mitigated by the reserve-before-call
   ledger, the `Drop` guard, startup reconciliation, `max_boot_minutes` auto-destroy, and
   verify-before-forget on destroy — but with $7.73 of credit and a $3.34/hr 2×H100, one missed
   orphan is a real loss. This deserves the first integration test that costs money.
7. **`vast_up.sh`'s `--onstart-cmd` semantics.** Replacing the shell script with REST still has to
   reproduce the exact env blob or the published images will not boot; the images also declare an
   `ENTRYPOINT` that already runs `launch.sh`, so two servers can contend for port 8000. Must be
   resolved explicitly (drop one) before the first paid launch.
8. **GGUF header parsing for the fit solver.** Hand-rolling a header reader risks wrong KV maths on
   exotic architectures, and a wrong "fits" verdict costs a failed launch or an OOM. Mitigation:
   the verdict is advisory, `Tight` is loud, and llama.cpp's own `--fit` (default ON in b9199) is
   left to do the real work whenever the user did not pin `--ctx-size`/`-ngl`.
9. **llama.cpp flag drift.** b9199 already changed `-fa`, `--jinja`, `--webui` and `-np` defaults
   relative to what LocalRouter emits. Mitigation: feature-detect from `--help` per build and store
   `FlagSupport` on `Build`; never hardcode a whitelist.
10. **`serve` handling both the proxy and the control plane on one socket.** A pathological upload
    or a saturated model could starve `/api`. Mitigation: `/api` handlers never await upstream
    inference; blocking work goes through `spawn_blocking`; the body limit is explicit.
11. **Two GUIs is two maintenance surfaces.** Mitigated by making Slint a strict subset and an edge
    client with no logic — but every protocol change still touches three renderers.
12. **MCP revision churn.** The 2026-07-28 revision is two days old and nothing speaks it. The
    dual-era hedge could be wrong in detail. Mitigation: echo-the-requested-version keeps legacy
    clients working regardless, which is the only case with a real user today.
13. **Migration half-states.** Users will run the Python TUI and ApexRouter in the same week.
    Mitigation: `[compat] localrouter_dir` mirroring, the legacy PID file, and read-only legacy
    parsing — but the two can still disagree about what is active for one refresh interval.

---

## 8. Rationale

**Why this shape.**

The 22 GiB laptop in front of us is running 5.5 GiB into swap before ApexRouter starts. Every
resident process is KV cache we didn't get. A supervising daemon idling at 40 MB is not expensive
in isolation; it is expensive because it gets swapped out and then has to be paged back in at
exactly the moment you ask it something, which is the worst possible latency profile for a status
query. And on the *target* rigs — the ones `00b` tells us to design for — a daemon buys nothing
either, because the expensive processes were always going to be the inference servers.

The deeper argument is about ownership. LocalRouter's original sin was four answers to "what is
active". The instinctive fix is to appoint an owner: a daemon that holds the truth in memory. But
that only moves the problem, because now every surface needs the daemon to be alive to get an
answer, and the daemon needs a second liveness question layered on top of the one that actually
matters. Putting the truth on disk under one lock, with liveness computed rather than stored, gives
the same single-source-of-truth guarantee with strictly fewer things that can be wrong. Prefrontal
already proved the shape works: its CLI and MCP server answer correctly with the daemon down.

Coupling matters more than component count. A daemon that spawns `llama-server` makes the model's
lifetime a child of the manager's lifetime — restart the manager to pick up a config change and
you drop a warm 27B that took three minutes to load. Detached children plus a state file invert
that: the manager is disposable, the models are durable, and `apexrouter serve --stop` is a safe
thing to type. That inversion is worth more than any supervision feature we'd get back.

Zero-ceremony falls out of the same choice. There is no install step, no unit file, no daemon to
start, and no "is it running?" failure mode. `apexrouter local start carnice` does discovery, fit,
spawn, health-gate, alias registration and proxy start in one command from a fresh checkout — and
`apexrouter status` tells the truth with nothing running at all. Agents get the same guarantee over
MCP with no prerequisite process, which is exactly what ApexOS and Claude Code need.

The costs are real and I am naming them rather than hiding them: no auto-restart when nothing is
listening, no in-memory request history across `serve` restarts, and an `ensure_serve` race that
needs a proper test. All three are acceptable, and the first is one systemd unit away from being
solved additively if it ever bites.

Everything else here is the house style applied without deviation, plus two opinionated
redesigns the specs argue for and LocalRouter never got: **model aliasing** (so switching backends
is invisible to clients, closing the #1 drop-in papercut) and **the fit solver** (one pure function
replacing 54 hand-solved recipe rows, and the single highest-value unbuilt feature in the original
author's own plan).
