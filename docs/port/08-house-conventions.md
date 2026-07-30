# House conventions — extracted from Andre's shipping Rust projects

Read-only survey of **Imaginarium-RS** (closest template), **Prefrontal-RS** (daemon + protocol crate +
no-dep web UI + Slint UI) and **ApexOS-RS/ui-slint** (the heavyweight Slint idioms). Everything below is
lifted from code that exists and builds, not invented. Where the two reference projects disagree, both
variants are shown and a pick for ApexRouter-RS is stated.

Sources (all read in full unless noted):

```
/home/andre/Projects/Imaginarium-RS/Cargo.toml
/home/andre/Projects/Imaginarium-RS/crates/imaginarium-core/src/{lib,config,paths,error,types,jobs,client}.rs
/home/andre/Projects/Imaginarium-RS/crates/imaginarium-mcp/src/{lib,main,transport,dispatch,tools,backend}.rs
/home/andre/Projects/Imaginarium-RS/crates/imaginarium-server/src/{lib,routes,static_files,auth}.rs
/home/andre/Projects/Imaginarium-RS/crates/imaginarium-cli/src/main.rs
/home/andre/Projects/Imaginarium-RS/crates/imaginarium-slint/{Cargo.toml,build.rs,src/main.rs,src/api.rs,ui/app.slint}
/home/andre/Projects/Imaginarium-RS/{README.md,SECURITY.md,BACKLOG.md,.gitignore,rustfmt.toml,.github/workflows/ci.yml,docs/*}
/home/andre/Projects/Prefrontal-RS/Cargo.toml
/home/andre/Projects/Prefrontal-RS/prefrontal-protocol/src/lib.rs
/home/andre/Projects/Prefrontal-RS/prefrontald/src/main.rs
/home/andre/Projects/Prefrontal-RS/prefrontal-cli/src/{main,mcp}.rs
/home/andre/Projects/Prefrontal-RS/prefrontal-core/src/{lib,config}.rs
/home/andre/Projects/Prefrontal-RS/prefrontal-client/src/lib.rs
/home/andre/Projects/Prefrontal-RS/ui-web/{index.html,app.js,style.css}
/home/andre/Projects/Prefrontal-RS/ui-slint/{Cargo.toml,build.rs,src/main.rs,ui/main.slint}
/home/andre/Projects/Prefrontal-RS/{README.md,CLAUDE.md,config.example.toml,docs/API.md,skills/prefrontal/SKILL.md}
/home/andre/Projects/ApexOS-RS/ui-slint/{Cargo.toml,build.rs,README.md,src/main.rs(head),src/ui/palette.slint,src/ui/appwindow.slint(head)}
```

---

## 1. Workspace layout + Cargo.toml conventions

### Two layout shapes in the garden

| | Imaginarium-RS | Prefrontal-RS |
|---|---|---|
| Members | `crates/<product>-<role>` | flat top-level `<product>-<role>` dirs |
| Web UI | `ui-web/` (Vue + Vite, `dist/` committed) | `ui-web/` (3 files, no build) |
| Slint UI | `crates/imaginarium-slint` | `ui-slint/` |
| Naming | `imaginarium-{core,cli,mcp,server,slint}` | `prefrontal-{protocol,core,cli,client}`, `prefrontald`, `ui-slint` |

**Pick for ApexRouter-RS: the Imaginarium `crates/` shape** — it is the stated template and it keeps
`ui-web/` and `assets/` out of the member list. Keep Prefrontal's *protocol crate* idea (see §4).

Proposed member set:

```
ApexRouter-RS/
├── Cargo.toml                  # workspace root
├── rustfmt.toml                # "# rustfmt defaults"
├── README.md  CLAUDE.md  LICENSE-MIT  LICENSE-APACHE  .gitignore
├── config.example.toml
├── assets/banner.png           # Imaginarium-generated
├── docs/{CHARTER,API,ARCHITECTURE,AGENTS,SLINT}.md   docs/port/NN-*.md
├── skills/apexrouter/SKILL.md
├── ui-web/{index.html,app.js,style.css}
└── crates/
    ├── apexrouter-protocol/    # wire types, serde only  (Prefrontal's trick)
    ├── apexrouter-core/        # config, paths, error, providers, llama supervisor
    ├── apexrouter-server/      # axum: OpenAI-compatible proxy + control REST + WS + embedded UI
    ├── apexrouter-mcp/         # stdio JSON-RPC, lib + bin
    ├── apexrouter-cli/         # `apexrouter` binary (default-run), owns `mcp` + `serve` subcommands
    └── apexrouter-slint/       # GPL-3.0-only native app, NOT in default-members
```

### Root manifest (Imaginarium, verbatim structure)

```toml
[workspace]
resolver = "2"
members = [
    "crates/imaginarium-core",
    "crates/imaginarium-cli",
    "crates/imaginarium-mcp",
    "crates/imaginarium-server",
    "crates/imaginarium-slint", # GPL app — not in default-members
]
default-members = [
    "crates/imaginarium-core",
    "crates/imaginarium-cli",
]

[workspace.package]
version = "0.1.0"
edition = "2021"
license = "MIT OR Apache-2.0"
repository = "https://github.com/buckster123/Imaginarium-RS"
authors = ["Andre <buckster123>"]
rust-version = "1.75"

[workspace.dependencies]
imaginarium-core = { path = "crates/imaginarium-core", version = "0.1.0" }
imaginarium-mcp = { path = "crates/imaginarium-mcp", version = "0.1.0" }
imaginarium-server = { path = "crates/imaginarium-server", version = "0.1.0" }
# ... externals below, see §2

[profile.release]
lto = "thin"
codegen-units = 1
strip = true
```

Rules extracted:

- `resolver = "2"`, `edition = "2021"`, `version = "0.1.0"` on day one and it stays there through v0.1.
- **MSRV is declared**: `rust-version = "1.75"`. Box has `rustc 1.97.0`, so 1.75 is a deliberate floor.
- **Licence**: `MIT OR Apache-2.0` for Imaginarium (dual `LICENSE-MIT` + `LICENSE-APACHE` files);
  plain `MIT` for Prefrontal (single `LICENSE`). Either is house-legal. **Take Imaginarium's dual**
  for ApexRouter since it is the closer template and dual is friendlier to downstream ApexOS reuse.
- **The Slint crate is a member but not a default-member**, carries `license = "GPL-3.0-only"` and
  `publish = false`, and the README states the caveat in plain English. `cargo build`/`clippy`/`test`
  at the root therefore never touch Slint (and never need `libfontconfig1-dev`).
- Internal path deps are listed in `[workspace.dependencies]` **with both `path` and `version`** so
  the crates stay publishable.
- `[profile.release]` is exactly those three keys. No `panic = "abort"`, no opt-level fiddling.
- `rustfmt.toml` exists containing only `# rustfmt defaults` — the file's presence makes
  `cargo fmt --all -- --check` an explicit, non-accidental gate.

### Member manifest

```toml
[package]
name = "imaginarium-core"
version.workspace = true
edition = "2021"                    # Imaginarium spells it; Prefrontal uses edition.workspace = true
license.workspace = true
repository.workspace = true
authors.workspace = true
rust-version.workspace = true
description = "Core client, models, jobs, and library for Imaginarium-RS (xAI Imagine gateway)"

[dependencies]
anyhow.workspace = true             # bare `.workspace = true` form, one per line, no version here
thiserror.workspace = true
tokio = { workspace = true, features = ["rt-multi-thread", "macros", "fs", "time", "io-util", "sync"] }

[dev-dependencies]
tokio = { workspace = true, features = ["test-util", "macros", "rt-multi-thread"] }
tempfile = "3"
```

- **Every crate has a real one-line `description`.** No exceptions in either project.
- Binary name ≠ crate name. Declared explicitly, and the CLI crate sets `default-run`:

```toml
# imaginarium-cli
default-run = "imaginarium"
[[bin]]
name = "imaginarium"
path = "src/main.rs"

# imaginarium-mcp — both a lib and a bin
[lib]
name = "imaginarium_mcp"
path = "src/lib.rs"
[[bin]]
name = "imaginarium-mcp"
path = "src/main.rs"
```

  Existing binary names in the garden: `imaginarium`, `imaginarium-mcp`, `imaginarium-app`,
  `prefrontal`, `prefrontald`, `prefrontal-ui`, `apexos-rs-ui`.
  → ApexRouter: `apexrouter` (CLI), `apexrouter-mcp`, `apexrouterd` **or** `apexrouter serve`
  (Imaginarium folds the server into the CLI; Prefrontal ships a separate daemon binary — folding is
  simpler and matches the template).
- Crate-level `//!` doc comment on every `lib.rs` and `main.rs`, one sentence, em-dash style:
  `//! Imaginarium core — xAI Imagine client, capability matrix, config, jobs, local library.`
- `lib.rs` is a module list + re-exports + consts, nothing else:

```rust
pub mod client; pub mod config; pub mod error; /* … */
pub use config::Config;
pub use error::{Error, Result};
pub use types::*;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const DEFAULT_BIND: &str = "127.0.0.1:8791";
pub const PRODUCT: &str = "Imaginarium-RS";
```

### CI + lint policy

`.github/workflows/ci.yml` (Imaginarium), single `build` job on ubuntu-latest:

```yaml
- uses: dtolnay/rust-toolchain@stable
  with: { components: rustfmt, clippy }
- uses: Swatinem/rust-cache@v2
- run: cargo fmt --all -- --check
- run: cargo clippy -p imaginarium-core -p imaginarium-cli -- -D warnings
- run: cargo test -p imaginarium-core -p imaginarium-cli
- run: cargo build -p imaginarium-cli
```

Clippy is `-D warnings` and **explicitly scoped to headless crates** (never the Slint one).
Prefrontal's CLAUDE.md calls it a **"clippy-zero policy"**:
`cargo build --workspace && cargo clippy --workspace`.

`.gitignore` (both):

```
/target
**/*.rs.bk
.idea/
.vscode/
*.swp
.DS_Store
ui-web/node_modules/
# ui-web/dist is committed so rust-embed works without npm at cargo build time
.env
.env.*
```

---

## 2. Dependencies actually in use

Exact versions/features, so ApexRouter picks the same ones and shares the lockfile shape.

| Crate | Version + features | Where |
|---|---|---|
| `anyhow` | `1` | everywhere (applications + fallible glue) |
| `thiserror` | `2` | core error enums only |
| `serde` | `1`, `["derive"]` | everywhere |
| `serde_json` | `1` | everywhere |
| `toml` | `0.8` (Imaginarium) / `1.1.4` (prefrontal-core) | config read/write — **version drift exists; pick `0.8`** |
| `clap` | `4`, `["derive", "env"]` | CLI. Prefrontal omits `env`; Imaginarium uses it |
| `tokio` | `1`, `["rt-multi-thread","macros","fs","time","io-util","io-std","sync"]` (Imaginarium) / `["full"]` (Prefrontal) | prefer the explicit feature list |
| `reqwest` | `0.12`, `default-features = false`, `["json","rustls-tls","stream","multipart"]` | **rustls always, never native-tls/OpenSSL** |
| `axum` | `0.8` (`["ws"]` when websockets are used) | server |
| `tower` | `0.5` | server |
| `tower-http` | `0.6` `["cors","trace","fs"]` (Imaginarium) / `0.7` `["fs"]` (Prefrontal) | |
| `rust-embed` | `8` (`#[derive(Embed)]`) | embedding the built web UI |
| `rusqlite` | `0.32`, `["bundled"]` | Imaginarium job/token store only |
| `directories` | `5` (`ProjectDirs`) | Imaginarium paths |
| `dirs` | `6.0.0` (`config_dir`, `home_dir`) | Prefrontal paths |
| `tracing` | `0.1` | everywhere |
| `tracing-subscriber` | `0.3`, `["env-filter"]` | binaries only |
| `chrono` | `0.4`, `["serde"]` | timestamps (RFC3339 on the wire) |
| `ulid` | `1`, `["serde"]` | id generation (`JobId`, `AssetId`) |
| `url` | `2`, `["serde"]` | |
| `base64` | `0.22` · `bytes` `1` · `sha2` `0.10` · `hex` `0.4` | |
| `futures-util` | `0.3` | stream/sink glue |
| `async-trait` | `0.1` | the `Backend` trait (local vs proxy) |
| `tokio-tungstenite` | `0.24` | WS **clients** (SDK, Slint UI) |
| `slint` | `1` + `slint-build` `1` (build-dep) | native UI only |
| `image` | `0.25`, `default-features = false`, `["png","jpeg","webp"]` | Slint preview decode |
| `tempfile` | `3` | dev-dependencies |
| `notify` `8.2` · `comrak` `0.54` · `ammonia` `4.1.4` · `gix` `0.86` · `tantivy` `0.26.1` · `regex` `1.13.1` | Prefrontal-specific; listed for the "pure Rust, no C linking" precedent |

Slint backend features by project:

```toml
slint = { version = "1", default-features = true, features = ["backend-winit"] }              # Imaginarium
slint = "1"                                                                                    # Prefrontal (defaults)
slint = { version = "1", features = ["backend-linuxkms-noseat", "backend-winit"] }             # ApexOS (Pi kiosk)
```

**Storage decision, rusqlite vs files.** Both patterns are in the house:

- Imaginarium: `rusqlite 0.32 bundled`, `Mutex<Connection>` (Connection is `!Sync`), `CREATE TABLE IF
  NOT EXISTS` in a `migrate()` on open, the full object stored as `result_json TEXT` alongside a few
  indexed scalar columns, upserts guarded by a `WHERE NOT (terminal → non-terminal)` clause so a late
  in-flight write can't reset a completed job. Unit-tested with `tempfile::tempdir()`.
- Prefrontal: **no database at all** — TOML config, filesystem walks, a tantivy index that is
  explicitly "a cache, never truth" (schema mismatch wipes and rebuilds).

→ ApexRouter port note: LocalRouter's state is JSON/JSONL files (`usage.log`, `local_instances/*.json`).
Keep files for anything a human might `cat` or a shell script might tail; reach for `rusqlite` only if
usage aggregation genuinely needs SQL. If sqlite is added, copy the `Mutex<Connection>` + `migrate()` +
terminal-status-guard pattern wholesale.

---

## 3. MCP server pattern

**Hand-rolled JSON-RPC over stdio. No SDK, in either project.** Prefrontal's `cli/mcp.rs` states why:

> Hand-rolled on purpose: the surface is seven tools and four methods, and the daemon isn't required —
> every tool works from a direct scan, so agents get answers even when nothing else is running.

Two implementations, sync and async:

| | Prefrontal (`prefrontal-cli/src/mcp.rs`, 281 ln) | Imaginarium (`crates/imaginarium-mcp/`, 5 files) |
|---|---|---|
| Runtime | sync `std::io::stdin().lock().lines()` | tokio, `StdioTransport` over `BufReader<Stdin>` |
| Frame cap | none | `MAX_FRAME_BYTES = 16 * 1024 * 1024`, `Frame::Oversized` drains to newline |
| Backends | one (direct scan) | `dyn Backend` — `LocalBackend` or `ProxyBackend` via `--proxy` |
| Tool errors | MCP result with `isError: true` | JSON-RPC error `-32000` |

**Protocol version string: `"2024-11-05"`** in both. Prefrontal echoes the client's requested version
back (falling back to the const), which is the more tolerant behaviour:

```rust
const PROTOCOL_VERSION: &str = "2024-11-05";

"initialize" => {
    let requested = params.get("protocolVersion").and_then(|v| v.as_str())
        .unwrap_or(PROTOCOL_VERSION);
    Ok(json!({
        "protocolVersion": requested,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "prefrontal", "version": env!("CARGO_PKG_VERSION") }
    }))
}
"ping" => Ok(json!({})),
"tools/list" => Ok(json!({ "tools": tool_definitions() })),
"tools/call" => { /* … */ }
_ => Err((-32601, format!("method not found: {method}"))),
```

Error codes in use: `-32601` method not found, `-32700` parse error, `-32000` tool failure
(Imaginarium only). `type RpcError = (i64, String);`

Loop shape (Prefrontal — the simpler one, recommended):

```rust
for line in stdin.lock().lines() {
    let line = line?;
    if line.trim().is_empty() { continue }
    let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
    let Some(id) = msg.get("id").cloned() else { continue };   // notification → no reply
    let response = match server.dispatch(method, &params) {
        Ok(result)          => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => json!({ "jsonrpc":"2.0","id":id,"error":{"code":code,"message":message} }),
    };
    writeln!(stdout, "{response}")?;
    stdout.flush()?;
}
```

Imaginarium additionally suppresses anything whose `method` starts with `"notifications/"` and handles
`initialize` **before** entering the loop, so a malformed first frame gets a `parse_error()` and a clean
exit.

Tool declaration — a tiny local helper, schemas written as `json!` literals, never derived:

```rust
fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({ "name": name, "description": description, "inputSchema": input_schema })
}

// Prefrontal's variant folds `required` in:
let obj = |props: Value, required: &[&str]| json!({ "type":"object","properties":props,"required":required });
```

**Tool result envelope:**

```rust
// Prefrontal — tool-level failures are results, not protocol errors
match outcome {
    Ok(text) => json!({ "content": [{ "type":"text","text": text }], "isError": false }),
    Err(msg) => json!({ "content": [{ "type":"text","text": msg }], "isError": true  }),
}
// Imaginarium — success only; failures become a -32000 JSON-RPC error
Ok(v) => json!({ "content": [{ "type": "text", "text": v.to_string() }] })
```

→ **Use Prefrontal's `isError` convention for ApexRouter.** It is the documented, reasoned one
("JSON-RPC errors are reserved for protocol breakage"), and it lets a model read the failure text.

Other extracted rules:

- **stdout is JSON-RPC only; every log line goes to stderr.** Stated as a comment in three places.
  `imaginarium-mcp/src/main.rs`: `//! Logs → stderr. stdout is JSON-RPC only.`
- Tool payloads are `serde_json::to_string_pretty(&protocol_type)` — the same structs the REST surface
  serialises. Never a bespoke text format.
- **Tool naming.** Imaginarium prefixes every tool (`imaginarium_image_generate`), Prefrontal does not
  (`list_projects`). Since all three MCP servers land in the same `~/Projects/.mcp.json` registry,
  → **prefix: `apexrouter_*`.**
- Descriptions are long and operational — they teach the model the workflow, defaults and gotchas
  ("Defaults wait until done; set no_wait=true then use job_status/job_wait"), not just the shape.
- The MCP server is exposed **twice**: as its own binary and as a `<cli> mcp` subcommand delegating to
  the same `run()`.
- **Proxy mode** (very relevant to ApexRouter's local-rig/remote-node split):

```rust
pub async fn run(proxy_url: Option<String>) -> Result<()> {
    let proxy_url = proxy_url.or_else(|| std::env::var("IMAGINARIUM_URL").ok())
        .filter(|s| !s.trim().is_empty());
    let backend: Arc<dyn Backend> = if let Some(url) = proxy_url {
        let token = std::env::var("IMAGINARIUM_TOKEN")
            .context("IMAGINARIUM_TOKEN required when using proxy/URL mode")?;
        Arc::new(ProxyBackend::new(&url, &token))
    } else {
        Arc::new(LocalBackend::new(Config::load().map_err(|e| anyhow::anyhow!(e))?)?)
    };
    /* … */
}
```

  `#[async_trait] pub trait Backend: Send + Sync { async fn models(&self) -> Result<Value>; … }`
  with the expensive client behind a `OnceLock` so it initialises lazily.
- Unit tests live in the MCP crate and assert the wire shape:
  `assert_eq!(resp["result"]["protocolVersion"], "2024-11-05")`, `assert!(names.len() >= 10)`.
- **Operational trap (Prefrontal CLAUDE.md):** `~/Projects/.mcp.json` points at
  `target/release/<bin>`. After changing the MCP surface, `cargo build --release` or agents run a
  stale binary.

---

## 4. HTTP / REST surface pattern

### Router composition

```rust
let app = Router::new()
    .merge(routes::public_router())        // /health — no auth
    .merge(api_router(state.clone()))      // /v1/* — auth layer applied inside
    .merge(static_files::static_router())  // / and /{*path} — embedded UI, registered LAST
    .layer(DefaultBodyLimit::max(MAX_BODY))
    .layer(trace);

pub fn api_router(state: AppState) -> Router {
    Router::new()
        .route("/v1/models", get(models))
        .route("/v1/jobs/{id}", get(jobs_get))          // axum 0.8: {id}, NOT :id
        .route("/v1/jobs/{id}/wait", post(jobs_wait))
        .route("/v1/tokens", get(tokens_list).post(tokens_create))
        .route("/v1/tokens/{id}", delete(tokens_revoke))
        .route_layer(axum::middleware::from_fn_with_state(state.clone(), require_auth))
        .with_state(state)
}
```

- **axum 0.8 path syntax is `/{param}` and `/{*path}`** — filed as a sharp edge in Prefrontal's
  CLAUDE.md. `:param` is axum 0.7 and will not compile.
- `route_layer` (not `layer`) for auth, so unmatched paths fall through to the static handler
  unauthenticated instead of 401-ing.
- `api_router` is `pub use`d from the crate root so ApexOS can mount it inside another service.

### State

```rust
#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub client: Arc<ImagineClient>,
    pub library: Arc<Library>,
    pub tokens: Arc<Mutex<TokenStore>>,      // tokio::sync::Mutex
    pub allow_localhost_no_auth: bool,
}
```

Prefrontal's variant is `State<Arc<AppState>>` with interior `RwLock<Vec<Project>>` +
`broadcast::Sender<Event>`. Use `Arc<AppState>` when the state has mutable collections; use the
`#[derive(Clone)]`-of-`Arc`-fields form when it's all read-only handles.

### Error → response mapping (two idioms, both house-legal)

```rust
// A) Imaginarium — handler returns `Response`, one helper, JSON envelope
fn err_response(status: StatusCode, msg: impl ToString) -> Response {
    (status, Json(json!({ "ok": false, "error": msg.to_string() }))).into_response()
}

// B) Prefrontal — handler returns Result<Json<T>, ApiError>, plain-text body
type ApiError = (StatusCode, String);
async fn search_handler(/* … */) -> Result<Json<Vec<SearchHit>>, ApiError> {
    let Some(search) = state.search.clone() else {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "search index unavailable".into()));
    };
    /* … */
}
```

Status-code vocabulary observed: `400` bad input / unparsable selector, `401` missing-or-invalid token,
`403` insufficient scope, `404` unknown id or unknown project, `413` payload too large, `500` internal
(db open, join error), `502 BAD_GATEWAY` **upstream provider failure**, `503` optional feature disabled.

→ ApexRouter: use **(B)** for the control-plane REST, and **(A)**'s `{"ok":false,"error":…}` envelope
for the OpenAI-compatible proxy routes (clients expect JSON bodies on errors). `502` for
"the upstream/llama-server failed", `503` for "feature/provider not configured" — that distinction is
already load-bearing in both projects.

### Health

Public, unauthenticated, and the shape the Slint client and every smoke test depend on:

```rust
pub async fn health() -> impl IntoResponse {
    axum::Json(serde_json::json!({ "ok": true, "product": PRODUCT, "version": VERSION }))
}
```

### Auth

```rust
pub async fn require_auth(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let required = required_scope(req.uri().path(), req.method().as_str());
    let presented = extract_presented_token(
        req.headers().get(AUTHORIZATION).and_then(|v| v.to_str().ok()),
        req.headers().get("x-imaginarium-token").and_then(|v| v.to_str().ok()),
        req.uri().query(),
    );
    if let Some(token) = presented { /* verify → scope check → insert RequestAuth extension */ }

    // Localhost bypass: explicit opt-in flag AND a genuinely-loopback PEER.
    if state.allow_localhost_no_auth {
        let peer_is_loopback = req.extensions().get::<ConnectInfo<SocketAddr>>()
            .map(|ci| ci.0.ip().is_loopback()).unwrap_or(false);   // absent ⇒ fails closed
        if peer_is_loopback { /* … */ return next.run(req).await; }
    }
    (StatusCode::UNAUTHORIZED, "invalid or missing token").into_response()
}
```

Non-negotiables encoded here:

- Three token presentations accepted: `Authorization: Bearer <t>`, `X-<Product>-Token: <t>`, `?token=<t>`.
- Scopes `read | write | admin`, derived from `(path, method)`; `/v1/tokens*` is always `admin`.
- The loopback bypass keys on the **real peer IP via `ConnectInfo`**, never the configured bind string,
  and fails closed when connect-info is missing (i.e. when embedded without
  `into_make_service_with_connect_info`). Serve with:
  `axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())`.
- **A non-loopback bind refuses to start without auth**: `bail!("refusing non-loopback bind {} without
  auth — set IMAGINARIUM_TOKEN or run `imaginarium token create` first")`.
- Tokens stored hashed; plaintext shown once at mint; hashes never serialised (the list handler
  hand-projects `id/label/scope/created_at`).

### CORS, tracing, body limits

- **No `CorsLayer` at all** on the authenticated API, with a comment explaining why (SPA is
  same-origin; CLI/Slint/ApexOS ignore CORS). "A cross-origin browser deployment should add an
  explicit origin allowlist here, never `allow_origin(Any)`."
- Trace span records **method + path only** — never the query string, because it can carry `?token=`:

```rust
let trace = TraceLayer::new_for_http().make_span_with(|req: &Request<Body>| {
    tracing::info_span!("request", method = %req.method(), path = %req.uri().path())
});
```

- `const MAX_BODY: usize = 64 * 1024 * 1024;` + `DefaultBodyLimit::max(MAX_BODY)` — axum's 2 MB
  default is too small for data-URL payloads.
- Blocking work always goes through `tokio::task::spawn_blocking` (ffmpeg, sqlite, git, fs walks).
- **Long jobs:** `?no_wait=true` returns a pending row immediately; the spawned task must flip the row
  to `failed` on *every* error path (including `JoinError` from a panic) — "a craft job must never sit
  pending forever."

### Serving the web UI — two options, both in use

```rust
// A) rust-embed — Imaginarium. Ships one binary; dist/ is committed.
#[derive(rust_embed::Embed)]
#[folder = "../../ui-web/dist"]
struct Assets;

pub fn static_router() -> Router {
    Router::new().route("/", get(static_handler)).route("/{*path}", get(static_handler))
}
async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    if path.starts_with("v1/") || path == "health" { return StatusCode::NOT_FOUND.into_response() }
    if let Some(c) = Assets::get(path) { /* mime_guess(path) + body */ }
    if let Some(i) = Assets::get("index.html") { /* SPA fallback */ }
    (StatusCode::NOT_FOUND, "UI not embedded — run: cd ui-web && npm ci && npm run build").into_response()
}
```

`mime_guess` is a hand-written 14-arm `match path.rsplit('.').next()` — no `mime_guess` crate.

```rust
// B) ServeDir — Prefrontal. No build step, UI reloads without recompiling.
.fallback_service(tower_http::services::ServeDir::new(&cfg.server.ui_dir))
```

→ ApexRouter: **rust-embed** for the shipped binary (Andre's UI is no-dep, so `#[folder = "../../ui-web"]`
points straight at the sources — no `dist/`, no npm, no vite). Keep an `ui_dir` config escape hatch if
a live-reload dev loop is wanted.

### WebSocket (Prefrontal — the reference implementation)

```rust
.route("/ws", get(ws_upgrade))

async fn ws_session(mut socket: WebSocket, state: Arc<AppState>) {
    // Subscribe BEFORE snapshotting so no delta can fall between the two.
    let mut rx = state.tx.subscribe();
    let snapshot = Event::Snapshot { projects: state.projects.read().await.clone() };
    if send_event(&mut socket, &snapshot).await.is_err() { return }
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(ev) => if send_event(&mut socket, &ev).await.is_err() { break },
                Err(broadcast::error::RecvError::Lagged(_)) => { /* re-send full snapshot */ }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            msg = socket.recv() => match msg { Some(Ok(_)) => {}, _ => break }, // drain to notice close
        }
    }
}
```

**WS contract (binding, per CLAUDE.md): "snapshot on connect covers all gaps — clients need zero replay
logic. Keep it that way."** Channel is `broadcast::channel(64)`. Frames are the protocol `Event` enum
with `#[serde(tag = "type", rename_all = "snake_case")]`.

### The protocol crate

`prefrontal-protocol` is `serde`-only (one dependency) and holds every wire type. The doc comment is
the rule:

> Frontends deserialize into the SAME types the daemon serializes from — no hand-rolled string matching
> (same trick as apexos-protocol).

Conventions inside it: `#[serde(rename_all = "snake_case")]` on enums; `#[serde(tag = "flag", …)]` for
tagged unions carrying data (`DirtyPile { count: u32 }`); `#[derive(PartialEq)]` on the big struct so
the daemon can suppress no-op broadcasts; a big variant boxed (`ProjectChanged { project: Box<Project> }`)
with a comment saying serde sees straight through it; `#[serde(default)]` on additive `Vec` fields.
**Take this crate wholesale for ApexRouter** — the Slint UI, the web UI, the CLI, the SDK and the MCP
server all need the same `Endpoint`/`Provider`/`Gpu`/`UsageRecord` shapes.

---

## 5. The no-dependency web UI pattern (Prefrontal `ui-web/`)

Three files, ~1100 lines total, **no build step, no npm, no CDN, no framework**:

```
ui-web/index.html    94 ln
ui-web/app.js       601 ln
ui-web/style.css    421 ln
```

(Imaginarium's `ui-web` is Vue 3 + Vite with `dist/` committed — the exception. For ApexRouter the
no-dep form is the target; it embeds directly with rust-embed and has no toolchain.)

### index.html

- `<!doctype html>`, `lang="en"`, viewport meta, `<link rel="stylesheet" href="style.css">`.
- **Inline SVG emoji favicon, no file on disk:**
  `<link rel="icon" href="data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100'><text y='.9em' font-size='90'>[one emoji]</text></svg>">`
- Static skeleton of empty containers with ids; JS injects children. Comments mark the holes:
  `<!-- stat tiles injected -->`.
- `<details>` elements as collapsible drawers (health, timeline) with a custom `summary` and a chevron
  span; `<div class="overlay" hidden>` + `<div class="panel" role="dialog" aria-modal="true">` for the
  modal. `aria-label` on every landmark section.
- `<script src="app.js"></script>` last — **plain script, not `type="module"`**.

### app.js

```js
// Prefrontal-RS ui-web — snapshot over WS with REST fallback, client-side filter.
// Types mirror prefrontal-protocol; frontends never invent their own shapes.
"use strict";

const ACTIVITY_ORDER = ["active", "warm", "cold", "parked", "archived"];
const ACTIVITY_DOT   = { active: "var(--act-active)", /* … */ };
// severity classes map to the reserved status palette; icon + label, never color alone
const FLAG_VIEW = { no_git: { label: "no git", icon: "✖", cls: "critical" }, /* … */ };

let projects = [];          // module-level mutable state, no store abstraction
let wsRetryMs = 1000;

const $  = (id) => document.getElementById(id);
function el(tag, cls, text) {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (text !== undefined) n.textContent = text;
  return n;
}
```

- Rendering is `container.replaceChildren()` then append `el(...)` nodes. **`innerHTML` is used exactly
  once**, for server-sanitised markdown, with the reason inline:
  `$("doc-view").innerHTML = doc.html; // comrak output, raw HTML escaped server-side`.
- **Live refresh**: WebSocket first, REST fallback for first paint.

```js
function connectWS() {
  const ws = new WebSocket(`ws://${location.host}/ws`);
  ws.onopen    = () => { setConn(true); wsRetryMs = 1000; };
  ws.onmessage = (m) => handleEvent(JSON.parse(m.data));
  ws.onclose   = () => { setConn(false); scheduleReconnect(); };  // fresh snapshot on reconnect
  ws.onerror   = () => ws.close();
}
function scheduleReconnect() {
  if (wsRetryTimer) return;
  wsRetryTimer = setTimeout(connectWS, wsRetryMs);
  wsRetryMs = Math.min(wsRetryMs * 2, 15_000);       // 1s → ×2 → cap 15s
}
setInterval(render, 60_000);   // keeps "Nd ago" honest while the tab sits open
```

- Event handling mirrors the Rust enum tags exactly: `snapshot` / `project_changed` / `project_removed`.
- **Debounce + stale-response guard** on search: 250 ms `setTimeout`, monotonic `searchSeq`, and
  `if (!res.ok || seq !== searchSeq) return;`.
- **Feature-off latch**: a `503` sets `cortexAvailable = false` so the UI stops asking for the session.
- Every `fetch` is wrapped in `try/catch` with a quiet fallback; the connection dot is the single place
  that reports "daemon gone".
- Boot is a plain `async function boot() { … }` called at the bottom of the file.

### style.css

- One `:root` token block, `color-scheme: dark`, then a `@media (prefers-color-scheme: light)` block
  that redefines the *same* variables. Dark is the default.

```css
:root {
  color-scheme: dark;
  --page: #0d0d0d;  --surface: #1a1a19;  --ink: #ffffff;  --ink-2: #c3c2b7;
  --muted: #898781; --hairline: #2c2c2a; --border: rgba(255,255,255,0.10);
  /* activity — vivid means alive, gray means resting (label always present) */
  --act-active: #3987e5; --act-warm: #86b6ef; --act-cold: #898781; --act-parked: #5b5a55;
  /* status (fixed, never themed) */
  --st-warning: #fab219; --st-serious: #ec835a; --st-critical: #d03b3b; --st-good: #0ca30c;
  /* categorical slots — fixed assignment per entity */
  --lang-rust: #d95926; --lang-node: #199e70; --lang-python: #3987e5; --lang-godot: #9085e9;
}
* { box-sizing: border-box; margin: 0; }
body {
  background: var(--page); color: var(--ink);
  font-family: system-ui, -apple-system, "Segoe UI", sans-serif;
  font-size: 14px; line-height: 1.45;
  padding: 0 clamp(12px, 3vw, 40px) 40px; max-width: 1400px; margin-inline: auto;
}
```

Design rules stated in the file header and CLAUDE.md, treat as binding:

- **Status colours are reserved for health, never for identity.**
- **Badges always pair icon + label, never colour alone.**
- **Body text is system sans (~1.5–1.6 line-height); monospace strictly inside code blocks.** This is
  an accessibility decision (owner's eyes), not taste.
- Layout primitives: stat tiles `grid-template-columns: repeat(auto-fit, minmax(150px, 1fr))`;
  card grid `repeat(auto-fill, minmax(290px, 1fr))`; 8 px radii, 1 px `var(--border)`, `gap: 10px`.
- `@media (max-width: 700px)` collapses the panel to a column. That is the only breakpoint.
- **Sharp edge (CLAUDE.md):** *"an author `display:` beats the `hidden` attribute — every element that
  toggles `hidden` and declares its own display needs a `[hidden]{display:none}` guard. Render-test UI
  changes; curl is not enough."*

---

## 6. The Slint app pattern

### Crate + build

```toml
[package]
name = "imaginarium-slint"
license = "GPL-3.0-only"          # overrides workspace licence
publish = false
description = "Native Slint UI for Imaginarium-RS — winit desktop (linuxkms later)"

[[bin]] name = "imaginarium-app"  path = "src/main.rs"

[features] default = ["winit"]  winit = []
# linuxkms = []                 # optional Pi path

[dependencies]
slint = { version = "1", default-features = true, features = ["backend-winit"] }
tokio.workspace = true  reqwest.workspace = true  anyhow.workspace = true
image = { version = "0.25", default-features = false, features = ["png","jpeg","webp"] }

[build-dependencies] slint-build = "1"
```

```rust
// build.rs — one line, always
fn main() { slint_build::compile("ui/app.slint").expect("slint compile"); }
```

`.slint` file layout scales in two steps:

- Small app (Imaginarium, Prefrontal): a single `ui/app.slint` / `ui/main.slint` (282 / 323 lines) with
  the structs and the window in one file.
- Big app (ApexOS): `src/ui/appwindow.slint` as root + `src/ui/{palette,types,personas}.slint` +
  `src/ui/components/*.slint` (28 components). `build.rs` compiles only the root; imports pull the rest.

### Thread model — the single most important rule

ApexOS states it at the top of `main.rs`:

```rust
// Thread model:
//   main thread — Slint event loop (never use #[tokio::main])
//   tokio pool  — WebSocket I/O + HTTP polling
//
// Cross-thread bridge:
//   slint::invoke_from_event_loop() queues closures to the Slint thread.
```

```rust
slint::include_modules!();

fn main() -> anyhow::Result<()> {                    // NOT #[tokio::main]
    tracing_subscriber::fmt().with_env_filter(/* … */).init();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let ui = AppWindow::new()?;
    // … wire callbacks …
    let _rt = Arc::new(rt);                          // keep the runtime alive for the app lifetime
    ui.run()?;
    Ok(())
}
```

Callback wiring is a braced block per callback, capturing a `Weak` + a runtime `Handle`:

```rust
{
    let ui_weak = ui.as_weak();
    let handle  = rt.handle().clone();
    ui.on_generate_clicked(move || {
        let ui = ui_weak.unwrap();
        let base   = ui.get_base_url().to_string();     // read props on the UI thread
        let prompt = ui.get_prompt().to_string();
        if prompt.trim().is_empty() { ui.set_status_line("Prompt required".into()); return }
        ui.set_busy(true);
        let ui_weak = ui.as_weak();
        handle.spawn(async move {
            // one inner async block so a single `match` handles every failure
            let outcome = async {
                let client = NodeClient::new(&base, &token)?;
                let job = client.image_generate(&prompt, /* … */).await?;
                anyhow::Ok((job_id, status, path, jobs))
            }.await;
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_busy(false);
                    match outcome { Ok(v) => { /* set props */ } Err(e) => ui.set_status_line(format!("…: {e}").into()) }
                }
            }).ok();
        });
    });
}
```

Two bridge forms; both are used, `upgrade_in_event_loop` is tidier:

```rust
slint::invoke_from_event_loop(move || { if let Some(ui) = ui_weak.upgrade() { … } }).ok();  // Imaginarium
let _ = weak.upgrade_in_event_loop(move |ui| ui.set_connected(connected));                  // Prefrontal / ApexOS
```

Models:

```rust
ui.set_jobs(ModelRc::new(VecModel::from(rows)));                             // Imaginarium
ui.set_projects(ModelRc::from(std::rc::Rc::new(VecModel::from(projects))));  // Prefrontal
// ApexOS keeps Rc<VecModel<T>> in thread_local! slots for in-place row mutation:
thread_local! { static MESSAGES: RefCell<Option<Rc<slint::VecModel<MessageItem>>>> = const { RefCell::new(None) }; }
```

Naming bridge: Slint kebab-case ⇄ Rust snake_case. `in-out property <string> base-url;` →
`ui.get_base_url()` / `ui.set_base_url(x.into())`. `callback connect-clicked();` → `ui.on_connect_clicked(…)`.
Shared structs are `export struct JobRow { id: string, mode: string, … }` in the `.slint` file and
become Rust structs of `SharedString` fields.

### Styling / theme

Small apps hardcode hex that **matches the web palette exactly** (`#0d0d0d` page, `#1a1a19` surface,
`#2c2c2a` hairline, `#ffffff` ink, `#c3c2b7` ink-2, `#898781` muted, `#3987e5` accent, `#0ca30c` good,
`#fab219` warn, `#ec835a` serious, `#d03b3b` critical) — Prefrontal's `.slint` header literally says
"same palette as ui-web, system default font".

Big apps use a `Palette` global — the scalable form, worth copying if ApexRouter's UI grows:

```slint
export enum Theme { ApexOS, Aurum, MacOS, Gnome, Windows, Jarvis }
export global Palette {
    in-out property <Theme> theme: Theme.ApexOS;
    out property <color> bg:
        theme == Theme.ApexOS ? #0d0f18 :
        theme == Theme.Aurum  ? #0d0800 : /* … */ #000a14;
    out property <length> radius: theme == Theme.ApexOS ? 3px : /* … */ 1px;
}
// Hot-switch from Rust:  ui.global::<Palette>().set_theme(Theme::Jarvis);
```

Header comment: *"Single source of truth for all visual tokens. Every component reads from here;
nothing hardcodes a color or radius directly."*

### Slint idioms in use

- Spacer: `Rectangle { horizontal-stretch: 1; background: transparent; }`
- Hover: `touch := TouchArea { clicked => { root.x(); } }` + `background: touch.has-hover ? #232322 : transparent;`
- Conditional subtree: `if root.has-preview: Image { … }` / `if root.open-project == "" : VerticalLayout { … }`
- `overflow: elide` on every text that can overflow; `wrap: word-wrap` on prose.
- `pure function status-color() -> color { … }` for derived colours.
- `Flickable` needs an explicit viewport: `viewport-height: doc-text.preferred-height + 40px;`
- `VerticalBox`/`HorizontalBox` (padded) from `std-widgets` vs `VerticalLayout`/`HorizontalLayout` (raw).
- Window preamble: `title`, `background`, `preferred-width/height`, `min-width/height`,
  `default-font-size: 14px`.
- Env prefill of connection settings straight into properties:
  `if let Ok(url) = std::env::var("IMAGINARIUM_URL") { ui.set_base_url(url.into()) }`

### The native app is an edge client, never a second logic path

`docs/SLINT.md`: *"Native UI is an edge client of the same LAN API as the Vue SPA. No second business
logic path."* It ships a ~200-line `src/api.rs` with a `NodeClient { http: reqwest::Client, base, token }`,
a private `fn auth(&self, req: RequestBuilder) -> RequestBuilder` that adds the bearer header, a
300 s timeout, and `res.status()` + `res.text()` checked manually before `serde_json::from_str`.

Build requirement: `libfontconfig1-dev` (ApexOS ui-slint README).

---

## 7. CLI conventions

- Binary = product short name. `clap` derive throughout:

```rust
#[derive(Parser, Debug)]
#[command(name = "imaginarium", version = VERSION,
          about = "Imaginarium-RS — local-first xAI Imagine studio gateway (CLI / MCP / API)")]
struct Cli {
    #[arg(long, global = true, env = "IMAGINARIUM_CONFIG")]  config: Option<PathBuf>,
    #[arg(long, global = true, env = "IMAGINARIUM_HOME")]    data_home: Option<PathBuf>,
    #[arg(long, global = true, default_value = "info")]      log_level: String,
    #[command(subcommand)] command: Commands,
}
```

- **Subcommand grammar.** Imaginarium groups by noun with nested enums
  (`image gen|edit`, `video gen|i2v|ref|edit|extend|status|wait`, `jobs ls|get`,
  `token create|ls|revoke`, `config init|show|path`, `library path`, `mcp`, `serve`, `version`).
  Prefrontal is flat verbs (`status|health|timeline|find|mcp|recall|cortex-sync`).
  → ApexRouter has enough surface for the noun form: `endpoint`, `model`, `provider`, `serve`, `mcp`,
  `config`, `usage`.
- Verb vocabulary is fixed: **`ls`** list · **`get`** one · **`path`** print a path · **`init`/`show`**
  config · **`create`/`revoke`** tokens · **`status`/`wait`** jobs.
- Short flags are rare and deliberate: only `-p/--prompt`. Renamed long flags for ergonomics:
  `#[arg(long = "ar")] aspect_ratio`, `#[arg(long = "res")] resolution`, `#[arg(long = "image")] images: Vec<String>`.
- **`--json` is per-subcommand, never global.** Every data-producing command has it, and it emits
  `serde_json::to_string_pretty(&protocol_type)` and nothing else on stdout.
- Human output is `key=value` lines and space-padded tables with an uppercase header:

```rust
println!("{:<28} {:<8} {:<12} {:<24} {:>5} {:>8}  FLAGS", "PROJECT","STATE","LANGS","BRANCH","DIRTY","TOUCHED");
println!("job_id={} status={} model={}", result.job_id, result.status.as_str(), result.model);
println!("  [{i}] local={} upstream={}", a.local_path.as_deref().unwrap_or("-"), /* … */);
```

  Missing values render as `-`. Empty states get a friendly parenthetical: `(no jobs yet)`,
  `(no minted tokens — set IMAGINARIUM_TOKEN or create one)`, `all clear — nothing rotting`.
- **No colour crate anywhere in the garden.** Plain text; emphasis comes from unicode marks
  (`⎇`, `≈`, `·`, `→`, `└`, `✖`). Emoji appear in web/Slint/README, never in CLI table output.
- **Exit codes:** `fn main() -> anyhow::Result<()>`; failures are `?` / `bail!("job not found: {id}")`,
  anyhow prints `Error: …` to stderr and the process exits 1. No `std::process::exit`, no custom codes.
- Tracing to **stderr**, `try_init()` not `init()`, with the reason in a comment:

```rust
fn init_tracing(level: &str) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("imaginarium={level},imaginarium_core={level}")));
    // Log to stderr: the `mcp` subcommand owns stdout for the JSON-RPC stream, and
    // any log line on stdout corrupts it. stderr is correct for every subcommand.
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_target(false)
        .with_writer(std::io::stderr).try_init();
}
```

- Global path flags are pushed **into the environment** before `Config::load()`, so env vars stay the
  one resolution mechanism:

```rust
fn load_cfg(cli: &Cli) -> Result<Config> {
    if let Some(home) = &cli.data_home { std::env::set_var("IMAGINARIUM_HOME", home); }
    if let Some(cfg)  = &cli.config    { std::env::set_var("IMAGINARIUM_CONFIG", cfg); }
    Config::load().map_err(Into::into)
}
```

- A `version` subcommand exists alongside `--version` and prints product + default bind:
  `println!("{PRODUCT} v{VERSION}"); println!("bin=imaginarium  default_bind={DEFAULT_BIND}");`
- The MCP subcommand delegates with a comment: `// Logs must stay on stderr; this takes over stdio for JSON-RPC.`

---

## 8. Config / paths conventions

### Resolution order

```rust
// Imaginarium — XDG via directories::ProjectDirs, with env overrides
const QUALIFIER: &str = "";  const ORGANIZATION: &str = "Imaginarium";  const APPLICATION: &str = "imaginarium";

pub fn data_home() -> Result<PathBuf> {
    if let Ok(v) = std::env::var("IMAGINARIUM_HOME") { /* if non-empty → use */ }
    ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION)
        .map(|d| d.data_dir().to_path_buf())
        .ok_or_else(|| Error::config("could not resolve data directory"))
}
pub fn config_path() -> Result<PathBuf> {
    // $IMAGINARIUM_CONFIG → $IMAGINARIUM_HOME/config.toml → ProjectDirs.config_dir()/config.toml
}
pub fn ensure_layout(home: &Path) -> Result<()> {
    for sub in ["library", "cache", "tokens"] { std::fs::create_dir_all(home.join(sub))?; }
    Ok(())
}
pub fn db_path(home: &Path)     -> PathBuf { home.join("imaginarium.db") }
pub fn library_root(home: &Path)-> PathBuf { home.join("library") }
pub fn job_dir(home: &Path, job_id: &str, when: DateTime<Utc>) -> PathBuf {  // library/YYYY/MM/DD/<id>/
    library_root(home).join(when.format("%Y").to_string()).join(/* … */).join(job_id)
}
```

```rust
// Prefrontal — simpler: dirs::config_dir()/prefrontal/config.toml, missing file = defaults
pub fn path() -> Option<PathBuf> { dirs::config_dir().map(|d| d.join("prefrontal").join("config.toml")) }
pub fn load() -> Result<Self> {
    match Self::path() {
        Some(p) if p.exists() => toml::from_str(&std::fs::read_to_string(&p)?)
            .with_context(|| format!("parsing {}", p.display())),
        _ => Ok(Self::default()),
    }
}
pub fn expand_tilde(path: &str) -> PathBuf { /* "~/" → dirs::home_dir() */ }
```

Resulting locations:

| What | Imaginarium | Prefrontal |
|---|---|---|
| Config | `$IMAGINARIUM_CONFIG` → `$IMAGINARIUM_HOME/config.toml` → `~/.config/imaginarium/config.toml` | `~/.config/prefrontal/config.toml` |
| Data/state | `$IMAGINARIUM_HOME` → `~/.local/share/imaginarium/` | `~/.local/share/prefrontal/index` |
| Bind | `127.0.0.1:8791` | `127.0.0.1:7320` ("PFC" on a phone keypad) |

Port numbers get a mnemonic and the joke lives in a comment. Pick one for ApexRouter and comment it.

### TOML shape

Top-level tables, one struct per section, **every field defaults so a missing file is a fully working
zero-config setup**. Two default styles:

```rust
// Imaginarium — per-field default fns + explicit impl Default
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)] pub upstream: UpstreamConfig,
    #[serde(default)] pub server:   ServerConfig,
    #[serde(skip)]    pub data_home:   PathBuf,     // runtime-only, never serialised
    #[serde(skip)]    pub config_path: PathBuf,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_bind")] pub bind: String,
    #[serde(default)] pub allow_localhost_no_auth: bool,
    #[serde(default = "default_node_token_env")] pub node_token_env: String,
}
fn default_bind() -> String { DEFAULT_BIND.into() }
impl Default for ServerConfig { fn default() -> Self { Self { bind: default_bind(), /* … */ } } }

// Prefrontal — container-level #[serde(default)] + one impl Default
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Thresholds { pub active_days: u32, pub warm_days: u32, pub cold_days: u32, pub dirty_pile: u32 }
impl Default for Thresholds { fn default() -> Self { Self { active_days: 7, warm_days: 30, /* … */ } } }
```

Prefrontal's `#[serde(default)]`-on-the-container form is less boilerplate; use it unless a field needs
a distinct fn.

Writing config back excludes runtime fields via a separate struct:

```rust
pub fn serializable(&self) -> ConfigFile { ConfigFile { upstream: …, server: …, /* no skip fields */ } }
pub fn save(&self) -> Result<()> { std::fs::write(&self.config_path, toml::to_string_pretty(&self.serializable())?) }
pub fn init_file() -> Result<PathBuf> { /* write defaults if missing, return path; idempotent */ }
```

API surface: `Config::load()`, `Config::load_from(&path, data_home)` (testable), `Config::init_file()`,
`Config::save()`, plus derived accessors (`base_url()`, `library_dir()`, `db_path()`).

### Secrets

**No secret is ever a required plaintext config field.** The config names the *env var* instead:

```rust
#[serde(default = "default_api_key_env")] pub api_key_env: String,     // "XAI_API_KEY"
#[serde(default, skip_serializing_if = "Option::is_none")] pub api_key: Option<String>,  // "discouraged"

pub fn resolve_api_key(&self) -> Result<String> {
    if let Some(k) = self.upstream.api_key.as_ref().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        return Ok(k);
    }
    match std::env::var(&self.upstream.api_key_env) {
        Ok(v) if !v.trim().is_empty() => Ok(v.trim().to_string()),
        _ => Err(Error::MissingCredential(format!("set {env_name} or upstream.api_key in config"))),
    }
}
```

Plus a node/mesh token from env (`server.node_token_env`, default `IMAGINARIUM_TOKEN`) — explicitly an
"ApexOS `AGENTD_TOKEN` analogue". `skip_serializing_if = "Option::is_none"` keeps unset secrets out of
the written file.

→ ApexRouter port note: `00-machine-ground-truth.md` extends the chain with a **conventional
third-party path** step (`~/.config/vastai/vast_api_key`, `~/.cache/huggingface/token`). Order:
explicit config value → ApexRouter config file → conventional third-party path → env var. Never log,
echo, or copy a key into the new config file if it was sourced elsewhere.

### Other rules

- **`config.example.toml` at the repo root**, fully commented, showing the defaults, with commented-out
  optional sections. The README carries a `What | Where` table pointing at it.
- **Central config only; no per-project dotfiles, ever** (Prefrontal charter D5).
- **Never write state into the repo directory** — called out in `00-machine-ground-truth.md` as
  LocalRouter's design flaw (`.active_endpoint`, `.last_instance`, `.hf_pin` in the repo dir).
- **Validate paths on load.** Ground truth: LocalRouter's saved instance references a model that no
  longer exists; "stale state files are the normal case, not an edge case."

---

## 9. Docs conventions

### README

Centred header block, then a fixed section order.

```html
<div align="center">

<img src="assets/banner.png" alt="Prefrontal-RS" width="100%">

# Prefrontal-RS

### Executive function as a service — a fully local, live dashboard over all your projects.

*Where was I? What's rotting? Didn't I already write this?*
*One daemon, one browser tab, zero cloud.*

[![rust](https://img.shields.io/badge/100%25-Rust-orange?style=for-the-badge&logo=rust)](https://www.rust-lang.org/)
[![local](https://img.shields.io/badge/127.0.0.1-only-22c55e?style=for-the-badge)]()
[![license](https://img.shields.io/badge/license-MIT-blue?style=for-the-badge)](LICENSE)
[![mcp](https://img.shields.io/badge/agents-MCP_·_7_tools-8b5cf6?style=for-the-badge)](docs/API.md#mcp)

</div>

---
```

Section order (both projects agree):

1. Pitch / **Why** — problem in the user's voice, 3 bullets of answers.
2. **How it works** — ASCII box diagram of the process topology.
3. **Quick start** — copy-pasteable shell, clone → `cargo run --release -p …` → the URL.
4. Per-surface subsections: **The CLI** (a shell block of real invocations), **Agents (MCP)**
   (`claude mcp add … -- /path/to/bin mcp` + the tool list + the skill install line),
   **Extending (Rust SDK)** (a rust block).
5. **Surfaces** table (`Surface | For | How`) — Imaginarium.
6. **Repository layout** table (`Crate / dir | What`).
7. **Security posture** — 3 bullets + link to `SECURITY.md`.
8. **Configuration** table (`What | Where`).
9. **License** — including the plain-English Slint/GPL caveat.
10. Centred `<sub>` footer with ecosystem links.

Also in use: `> [!NOTE]` GitHub callout for the one operational fact that must not be missed
("Bring your own key…"); `<details>` around the long endpoint list; centred `<img>` screenshots with
`<br><sub>caption</sub>`.

**Banner is house standard**: generated by Imaginarium-RS, stored at `assets/banner.{png,jpg}`, and
**credited in the footer** (by model, sometimes by job id):
`<sub>banner generated by <a href="…/Imaginarium-RS">Imaginarium-RS</a></sub>`.

Sub-crate READMEs are short and fixed-shape (ApexOS `ui-slint/README.md`, 14 lines): `# <dir>`,
a `>` blockquote one-liner, one paragraph, then bullets **Key files:** / **Depends on:** / **Lift via:**,
then `Part of [X](…) — see PATTERNS.md`.

### CLAUDE.md

`# CLAUDE.md — <Project> maintainer's brief`, then exactly these sections:

1. Opening paragraph: what you're working on, when it was built, and a pointer to the binding
   decisions log ("**the decisions log (D1–D9 + dated entries) is binding**; amend it with a dated
   entry when a decision changes, never silently").
2. **Architecture in one breath** — one dense paragraph.
3. **Invariants — break these and you've broken the product** — bulleted, imperative, each with the
   reason. (Localhost only. Path inputs are hostile. Commits are pathspec-scoped. The index is a cache,
   never truth. Central config only. Rendered HTML is sanitized. Pure Rust, no C linking. Typography.)
4. **Where things live / how they work** — one bullet per module with the non-obvious detail.
5. **Sharp edges met and filed down (don't rediscover these)** — the axum-0.8 `{param}` trap, the
   comrak `render.r#unsafe` raw identifier, the CSS `[hidden]` trap, the stale-MCP-binary trap.
6. **Workflow** — a copy-pasteable build / run / smoke block, plus the verification style and the
   house commit voice ("story-telling subject lines").
7. **Roadmap seeds** — pointers to `notes/`, not a plan.

### docs/

| File | Contents |
|---|---|
| `docs/CHARTER.md` | binding decisions log `D1…Dn` + dated amendments; open questions at the bottom |
| `docs/API.md` | surface table (`Surface \| Transport \| For`), then REST / WS / CLI / MCP reference with `jsonc` examples carrying inline `// enum \| values` comments |
| `docs/ARCHITECTURE.md` | topology |
| `docs/SLINT.md` | native-UI plan: architecture box, thread model, **port map table** (web → Slint, per feature), **MVP slices table** (6.0…6.4) |
| `docs/AGENTS.md` | MCP registration snippets per harness (Hermes YAML, Claude Code JSON), local vs proxy env |
| `docs/LICENSING.md` | the dual/GPL split spelled out |
| `SECURITY.md` (root) | audit date, scope, method, headline trust-model statement, findings with `[confirmed]`/`[swarm]` provenance tags |
| `BACKLOG.md` (root) | audit findings collapsed into suggested PRs, checkbox list with file:line refs |
| `openapi/<product>-v1.yaml` | machine-readable API spec |
| `notes/*.md` | unscheduled ideas (this is where `write_doc` files land) |
| `docs/port/NN-topic.md` | already the convention in this repo — keep the two-digit prefix |

### skills/

`skills/<name>/SKILL.md`, installed with `cp -r skills/<name> ~/.claude/skills/`:

```markdown
---
name: prefrontal
description: Query and update the user's project garden via Prefrontal-RS — USE BEFORE writing any
  function/tool/code that might already exist…, when asked "where was I", … Works via MCP tools, CLI, or REST.
---

# Prefrontal — the project-garden brain

## Pick your surface (in order of preference)
1. **MCP tools** (if `mcp__prefrontal__*` are loaded): …
2. **CLI** (daemon not required): ```sh … ```
3. **REST** — daemon at `http://127.0.0.1:7320` (check: `curl -sf …`)

## Knowledge you need
- **Projects are addressed by directory name** — …

## Patterns
- "Do we already have X?" → `search` X, check symbol hits first…
```

Front-matter `description` is trigger-heavy (it is what routes the skill). Body is: surface preference
order → domain knowledge the model can't infer → concrete task→tool patterns.

---

## Quick checklist for ApexRouter-RS

- [ ] `crates/apexrouter-{protocol,core,server,mcp,cli,slint}`; slint out of `default-members`, GPL-3.0-only.
- [ ] `[workspace.package]` with `rust-version = "1.75"`, `MIT OR Apache-2.0`, buckster123 repo URL.
- [ ] `[profile.release] lto="thin" codegen-units=1 strip=true`; `rustfmt.toml` present.
- [ ] reqwest `rustls-tls`, `default-features = false`. Never native-tls.
- [ ] axum 0.8 `{param}` paths; `route_layer` auth; `ConnectInfo` loopback bypass; refuse non-loopback bind without a token; no CORS layer; trace span without the query string.
- [ ] `/health` public, `{"ok":true,"product":…,"version":…}`.
- [ ] Hand-rolled MCP over stdio, `"2024-11-05"`, tools prefixed `apexrouter_*`, failures as `isError: true`, logs to stderr only.
- [ ] `ui-web/{index.html,app.js,style.css}` — no npm, dark-first CSS variables, WS + REST fallback, exponential reconnect.
- [ ] Slint: no `#[tokio::main]`, `Weak` + `upgrade_in_event_loop`, edge client of the same HTTP API.
- [ ] CLI: clap derive, noun subcommands, per-command `--json`, no colour, anyhow exit-1, tracing to stderr.
- [ ] Config: XDG + `APEXROUTER_HOME`/`APEXROUTER_CONFIG`, all fields defaulted, secrets via `*_env` names, `config.example.toml` at root, never write state into the repo dir.
- [ ] README with Imaginarium-generated banner + credit; CLAUDE.md maintainer's brief; `docs/CHARTER.md` binding log; `skills/apexrouter/SKILL.md`.
