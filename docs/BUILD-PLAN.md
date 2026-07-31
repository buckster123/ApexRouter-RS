# ApexRouter-RS — mk1 Build Plan (parallel agents)

> Companion to `docs/ARCHITECTURE.md`. That document is **what** to build; this one is **who builds
> which file, against which signature, in which order**. Between them they are the sole input for
> implementation agents.
>
> **The file-ownership rule is absolute: no two agents ever write the same file.** Every work unit
> below lists the exact files it owns. If you need something in a file you do not own, it is either
> already stubbed with the signature you need (Stage 0 guarantees this) or the plan is wrong — say
> so, do not edit another agent's file.

---

## 0. How this plan works

```
Stage 0  ── 1 agent, serial ──  workspace + Cargo.toml ×9 + the ENTIRE protocol crate
                                + a compiling stub for every module in every crate
            GATE: cargo check --workspace --all-targets   (Slint excluded from default-members)

Stage 1  ── 8 agents, parallel ──  core foundations: paths, config, secret, store, lock,
                                   proc, exec, error, ledger
            GATE: cargo test -p apexrouter-core; `apexrouter config show` on a bare machine

Stage 2  ── 9 agents, parallel ──  core capabilities: discovery, gguf, fit, argv, upstream probe,
                                   usage, pricing, catalog, migrate, checks
            GATE: cargo test -p apexrouter-core; `apexrouter rig` correct on the real machine

Stage 3  ── 10 agents, parallel ── router crate (the request path) + the local supervisor
            GATE: cargo test -p apexrouter-router (fake upstream, no network, no llama.cpp)

Stage 4  ── 6 agents, parallel ──  server (both listeners, ws, auth, assets) + CLI core verbs
            GATE: ***MK1-CORE ACCEPTANCE*** — §7.1 end-to-end on this laptop

Stage 5  ── 14 agents, parallel ── vast, hf, together, ssh, web UI, Slint, MCP, CLI remainder,
                                   compare/smoke/diagnose, jobs, Anthropic ingress, docs
            GATE: per-unit acceptance + `cargo clippy -D warnings` on every headless crate

Stage 6  ── 5 agents, parallel ──  migration, README/banner, CHARTER/API/SLINT/AGENTS, SKILL.md,
                                   openapi, the full acceptance run
            GATE: ***MK1 ACCEPTANCE*** — §7.2
```

Rules every agent follows:

1. **Read `docs/ARCHITECTURE.md` first.** It is normative. This plan does not repeat its reasoning.
2. **Never change a signature that Stage 0 published.** If a signature is wrong, stop and report it;
   changing it breaks other agents silently.
3. `cargo fmt` before finishing. `cargo clippy -p <your crate> -- -D warnings` must be clean.
4. Every `pub fn` gets a doc comment; every crate gets a `//!` crate doc.
5. **No `sh -c` anywhere.** `core::exec` takes an argv vector. There is a CI grep that fails the
   build on `"sh", "-c"` and on `.arg("-c")`.
6. **No `unwrap()`/`expect()` outside tests and `main()`.** `thiserror` in libraries, `anyhow` in
   binaries.
7. **Nothing logs to stdout except MCP JSON-RPC and `--json` output.** `tracing` goes to stderr.
8. **Nothing is ever written into the repo directory.** All state under `Paths::state()`.

---

## 1. The exact Cargo.toml files

### 1.1 `/Cargo.toml` (workspace root)

```toml
[workspace]
resolver = "2"
members = [
    "crates/apexrouter-protocol",
    "crates/apexrouter-core",
    "crates/apexrouter-router",
    "crates/apexrouter-providers",
    "crates/apexrouter-client",
    "crates/apexrouter-server",
    "crates/apexrouter-cli",
    "crates/apexrouter-slint",   # GPL app — deliberately NOT in default-members
]
default-members = [
    "crates/apexrouter-protocol",
    "crates/apexrouter-core",
    "crates/apexrouter-router",
    "crates/apexrouter-providers",
    "crates/apexrouter-client",
    "crates/apexrouter-server",
    "crates/apexrouter-cli",
]

[workspace.package]
version      = "0.1.0"
edition      = "2021"
rust-version = "1.75"
license      = "MIT OR Apache-2.0"
repository   = "https://github.com/buckster123/ApexRouter-RS"
authors      = ["Andre <buckster123>"]

[workspace.dependencies]
apexrouter-protocol  = { path = "crates/apexrouter-protocol",  version = "0.1.0" }
apexrouter-core      = { path = "crates/apexrouter-core",      version = "0.1.0" }
apexrouter-router    = { path = "crates/apexrouter-router",    version = "0.1.0" }
apexrouter-providers = { path = "crates/apexrouter-providers", version = "0.1.0" }
apexrouter-client    = { path = "crates/apexrouter-client",    version = "0.1.0" }
apexrouter-server    = { path = "crates/apexrouter-server",    version = "0.1.0" }

anyhow             = "1"
thiserror          = "2"
serde              = { version = "1", features = ["derive"] }
serde_json         = { version = "1", features = ["preserve_order"] }
toml               = "0.8"
toml_edit          = "0.22"
clap               = { version = "4", features = ["derive", "env"] }
clap_complete      = "4"
tokio              = { version = "1", features = ["rt-multi-thread", "macros", "fs", "time", "io-util", "io-std", "sync", "process", "signal", "net"] }
reqwest            = { version = "0.12", default-features = false, features = ["json", "rustls-tls", "stream"] }
axum               = { version = "0.8", features = ["ws"] }
tower              = "0.5"
tower-http         = { version = "0.7.0", features = ["trace", "fs"] }
tracing            = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
arc-swap           = "1"
bytes              = "1"
futures-util       = "0.3"
async-trait        = "0.1"
chrono             = { version = "0.4", features = ["serde"] }
ulid               = { version = "1", features = ["serde"] }
dirs               = "6"
notify             = "6"
rust-embed         = "8"
sysinfo            = "0.32"
glob               = "0.3"
sha2               = "0.10"
rustix             = { version = "0.38", features = ["fs", "process", "thread"] }
tokio-tungstenite  = "0.24"
slint              = "1"
slint-build        = "1"
tempfile           = "3"
wiremock           = "0.6"

[profile.release]
lto           = "thin"
codegen-units = 1
strip         = true

[profile.dev]
debug = "line-tables-only"     # the dev box is at 92% disk
```

### 1.2 `crates/apexrouter-protocol/Cargo.toml`

```toml
[package]
name = "apexrouter-protocol"
description = "ApexRouter-RS wire and domain types — shared by daemon, CLI, MCP, web UI and Slint"
version.workspace      = true
edition.workspace      = true
rust-version.workspace = true
license.workspace      = true
repository.workspace   = true
authors.workspace      = true

[dependencies]
serde      = { workspace = true }
serde_json = { workspace = true }
ulid       = { workspace = true }

[dev-dependencies]
serde_json = { workspace = true }
```

### 1.3 `crates/apexrouter-core/Cargo.toml`

```toml
[package]
name = "apexrouter-core"
description = "ApexRouter-RS core — config, state, discovery, fit solver, argv builder, usage, ledger"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
authors.workspace = true

[dependencies]
apexrouter-protocol = { workspace = true }
serde        = { workspace = true }
serde_json   = { workspace = true }
toml         = { workspace = true }
toml_edit    = { workspace = true }
tokio        = { workspace = true }
reqwest      = { workspace = true }
tracing      = { workspace = true }
thiserror    = { workspace = true }
anyhow       = { workspace = true }
chrono       = { workspace = true }
dirs         = { workspace = true }
notify       = { workspace = true }
sysinfo      = { workspace = true }
glob         = { workspace = true }
sha2         = { workspace = true }
rustix       = { workspace = true }
async-trait  = { workspace = true }
futures-util = { workspace = true }
ulid         = { workspace = true }

[dev-dependencies]
tempfile  = { workspace = true }
wiremock  = { workspace = true }
tokio     = { workspace = true, features = ["test-util"] }
```

### 1.4 `crates/apexrouter-router/Cargo.toml`

```toml
[package]
name = "apexrouter-router"
description = "ApexRouter-RS request path — routing table, resolver, relay, retries, telemetry"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
authors.workspace = true

[dependencies]
apexrouter-protocol = { workspace = true }
apexrouter-core     = { workspace = true }
axum         = { workspace = true }
tower         = { workspace = true }
tower-http    = { workspace = true }
reqwest       = { workspace = true }
tokio         = { workspace = true }
arc-swap      = { workspace = true }
bytes         = { workspace = true }
futures-util  = { workspace = true }
serde         = { workspace = true }
serde_json    = { workspace = true }
tracing       = { workspace = true }
thiserror     = { workspace = true }
chrono        = { workspace = true }
ulid          = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
wiremock = { workspace = true }
tokio    = { workspace = true, features = ["test-util"] }
```

### 1.5 `crates/apexrouter-providers/Cargo.toml`

```toml
[package]
name = "apexrouter-providers"
description = "ApexRouter-RS provisioners — local llama.cpp/vLLM supervisor, vast.ai, together.ai, HuggingFace, ssh"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
authors.workspace = true

[dependencies]
apexrouter-protocol = { workspace = true }
apexrouter-core     = { workspace = true }
reqwest      = { workspace = true }
tokio        = { workspace = true }
serde        = { workspace = true }
serde_json   = { workspace = true }
async-trait  = { workspace = true }
futures-util = { workspace = true }
tracing      = { workspace = true }
thiserror    = { workspace = true }
anyhow       = { workspace = true }
chrono       = { workspace = true }
rustix       = { workspace = true }
glob         = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
wiremock = { workspace = true }
tokio    = { workspace = true, features = ["test-util"] }
```

### 1.6 `crates/apexrouter-client/Cargo.toml`

```toml
[package]
name = "apexrouter-client"
description = "ApexRouter-RS thin HTTP+WS client — used by the CLI, the MCP server and the Slint app"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
authors.workspace = true

[dependencies]
apexrouter-protocol = { workspace = true }
reqwest           = { workspace = true }
tokio             = { workspace = true }
tokio-tungstenite = { workspace = true }
futures-util      = { workspace = true }
serde             = { workspace = true }
serde_json        = { workspace = true }
thiserror         = { workspace = true }
```

### 1.7 `crates/apexrouter-server/Cargo.toml`

```toml
[package]
name = "apexrouter-server"
description = "ApexRouter-RS axum application — proxy listener, control plane, WebSocket, embedded web UI"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
authors.workspace = true

[dependencies]
apexrouter-protocol  = { workspace = true }
apexrouter-core      = { workspace = true }
apexrouter-router    = { workspace = true }
apexrouter-providers = { workspace = true }
axum         = { workspace = true }
tower        = { workspace = true }
tower-http   = { workspace = true }
tokio        = { workspace = true }
reqwest      = { workspace = true }
arc-swap     = { workspace = true }
bytes        = { workspace = true }
futures-util = { workspace = true }
serde        = { workspace = true }
serde_json   = { workspace = true }
tracing      = { workspace = true }
anyhow       = { workspace = true }
thiserror    = { workspace = true }
chrono       = { workspace = true }
ulid         = { workspace = true }
notify       = { workspace = true }
rust-embed   = { workspace = true }
sha2         = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
wiremock = { workspace = true }
```

### 1.8 `crates/apexrouter-cli/Cargo.toml`

```toml
[package]
name = "apexrouter-cli"
description = "apexrouter — the CLI, the daemon entrypoint and the MCP stdio server"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
authors.workspace = true
default-run = "apexrouter"

[[bin]]
name = "apexrouter"
path = "src/main.rs"

[dependencies]
apexrouter-protocol  = { workspace = true }
apexrouter-core      = { workspace = true }
apexrouter-router    = { workspace = true }
apexrouter-providers = { workspace = true }
apexrouter-server    = { workspace = true }
apexrouter-client    = { workspace = true }
clap               = { workspace = true }
clap_complete      = { workspace = true }
tokio              = { workspace = true }
serde              = { workspace = true }
serde_json         = { workspace = true }
tracing            = { workspace = true }
tracing-subscriber = { workspace = true }
anyhow             = { workspace = true }
chrono             = { workspace = true }
async-trait        = { workspace = true }
futures-util       = { workspace = true }
rustix             = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
```

### 1.9 `crates/apexrouter-slint/Cargo.toml`

```toml
[package]
name = "apexrouter-slint"
description = "ApexRouter-RS native UI — Slint edge client of the same HTTP API"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
repository.workspace = true
authors.workspace = true
license = "GPL-3.0-only"     # deliberate: Slint's GPL option. NOT the workspace license.
publish = false

[[bin]]
name = "apexrouter-ui"
path = "src/main.rs"

[features]
default = ["winit"]
winit = ["slint/backend-winit"]
# linuxkms = ["slint/backend-linuxkms-noseat"]   # for pure-ApexOS KMS/DRM setups

[dependencies]
apexrouter-protocol = { workspace = true }
apexrouter-client   = { workspace = true }
slint        = { workspace = true }
tokio        = { workspace = true }
anyhow       = { workspace = true }
serde_json   = { workspace = true }
futures-util = { workspace = true }

[build-dependencies]
slint-build = { workspace = true }
```

### 1.10 `rustfmt.toml`

```toml
# House default. Intentionally empty — we use rustfmt's defaults, and this file
# exists so `cargo fmt --all -- --check` is unambiguous in CI.
```

### 1.11 `.github/workflows/ci.yml`

```yaml
name: ci
on: [push, pull_request]
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with: { components: rustfmt, clippy }
      - uses: Swatinem/rust-cache@v2
      - run: cargo fmt --all -- --check
      - name: no shell-outs
        run: |
          ! grep -rn --include=*.rs -e '"sh", *"-c"' -e '\.arg("-c")' crates/ \
            || (echo "shell invocation found — use core::exec with an argv vector" && exit 1)
      - run: cargo clippy -p apexrouter-protocol -p apexrouter-core -p apexrouter-router
                          -p apexrouter-providers -p apexrouter-client -p apexrouter-server
                          -p apexrouter-cli -- -D warnings
      - run: cargo test --workspace --exclude apexrouter-slint
      - run: cargo build --release
```

`apexrouter-slint` is never in the clippy `-p` list and never in `cargo test`, so CI never needs
`libfontconfig1-dev` and never links the slint ecosystem.

### 1.12 `.gitignore`

```
/target
**/*.rs.bk
.env*
.idea/
.vscode/
/Cargo.lock          # NOT ignored — this is a binary workspace. (line kept as a reminder: DELETE IT)
```

Correction to apply verbatim: `Cargo.lock` **is committed** (binary workspace). The `.gitignore`
contains `/target`, `**/*.rs.bk`, `.env*`, `.idea/`, `.vscode/` and nothing else.

---

## 2. Stage 0 — the skeleton (ONE agent, serial, blocks everything)

**Owner: agent `S0`.** Nobody else may start until this stage's gate is green.

### S0 deliverables

1. Every file in §1 (nine `Cargo.toml`, `rustfmt.toml`, CI, `.gitignore`).
2. `config.example.toml` — a verbatim copy of `ARCHITECTURE.md` §5.2 with every line commented
   explaining the default, and optional sections commented out.
3. `routes.example.toml`.
4. **The complete `apexrouter-protocol` crate** — every type in §3 of this document, compiling, with
   `#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]` and the documented serde attributes.
   Round-trip tests for every tagged enum.
5. **A compiling stub for every module listed in §4**, with the *exact* public signatures given
   there, bodies `todo!("<unit-id>")`, and `#![allow(unused)]` at each crate root (removed at the
   end of Stage 4). Each stub file starts with a comment naming the work unit that owns it:
   `//! OWNER: unit C-04 (core/fit.rs). Do not edit outside that unit.`
6. `crates/*/src/lib.rs` with `//!` crate docs, the `mod` list, `pub use` re-exports, and the
   constants: `PRODUCT = "apexrouter"`, `VERSION = env!("CARGO_PKG_VERSION")`,
   `DEFAULT_PROXY_BIND = "127.0.0.1:8888"`, `DEFAULT_CONTROL_BIND = "127.0.0.1:2739"`,
   `DEFAULT_LOCAL_PORT_RANGE = (8100, 8199)`, `DEFAULT_TUNNEL_PORT_RANGE = (8800, 8899)`.
7. `ui-web/{index.html,app.js,style.css}` as three empty-but-valid placeholder files, so
   `rust-embed`'s `#[folder = "../../ui-web"]` compiles.
8. `crates/apexrouter-slint/build.rs` (one `slint_build::compile("src/ui/appwindow.slint")` line)
   and a minimal `src/ui/appwindow.slint` exporting an empty `AppWindow`.

### S0 gate

```
cargo fmt --all -- --check
cargo check --workspace --all-targets
cargo test -p apexrouter-protocol
cargo check -p apexrouter-slint          # separately; needs libfontconfig1-dev locally
```

All four green. `cargo check --workspace` must not need `libfontconfig1-dev` because Slint is out of
`default-members` (a bare `cargo check` uses default-members; `--workspace` includes Slint, so the
CI job uses the explicit `-p` list instead — run `cargo check -p …` for the seven headless crates).

---

## 3. The shared-types contract (quoted in full)

This is `crates/apexrouter-protocol/src/**`. **Every implementer codes against exactly these
signatures.** Stage 0 writes it; nobody else edits it. Attributes elided for brevity in the listing
below are always: `#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]` on structs and enums,
`#[serde(rename_all = "snake_case")]` on every enum, `#[serde(default)]` on every additive `Vec`
and `Option` field, and `#[serde(deny_unknown_fields)]` **nowhere** (we must survive additive
changes).

### 3.1 `src/lib.rs`

```rust
//! ApexRouter-RS wire and domain types. Serde only: no I/O, no tokio, no reqwest.
//! Every surface (daemon, CLI, MCP, web UI, Slint) deserializes the same types the daemon
//! serializes. No frontend ever string-matches a status.

pub mod ids;      pub mod money;    pub mod rig;      pub mod fit;
pub mod backend;  pub mod endpoint; pub mod route;    pub mod telemetry;
pub mod catalog;  pub mod vast;     pub mod provider; pub mod check;
pub mod hf;       pub mod event;

pub use ids::*;   pub use money::*;   pub use rig::*;      pub use fit::*;
pub use backend::*; pub use endpoint::*; pub use route::*;  pub use telemetry::*;
pub use catalog::*; pub use vast::*;   pub use provider::*; pub use check::*;
pub use hf::*;     pub use event::*;

pub const PRODUCT: &str = "apexrouter";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const DEFAULT_PROXY_BIND:   &str = "127.0.0.1:8888";
pub const DEFAULT_CONTROL_BIND: &str = "127.0.0.1:2739";
pub const DEFAULT_LOCAL_PORT_RANGE:  (u16, u16) = (8100, 8199);
pub const DEFAULT_TUNNEL_PORT_RANGE: (u16, u16) = (8800, 8899);
/// Model names that ALWAYS fall through to the default alias, so smoke.sh's hardcoded
/// `"model":"x"` and an absent model field keep working regardless of `unknown_model`.
pub const LEGACY_MODEL_NAMES: &[&str] = &["", "x", "auto", "default"];
```

### 3.2 `src/ids.rs`

```rust
macro_rules! slug_id { ($name:ident, $what:expr) => { /* newtype + parse + as_str + Display */ } }

pub struct BackendId(String);   // ^[a-z0-9][a-z0-9._-]{0,63}$
pub struct Alias(String);       // same charset, '/' explicitly banned (it means an explicit pin)
pub struct BuildId(String);
pub struct RecipeId(String);
pub struct ProfileId(String);
pub struct ProviderId(String);
#[derive(Copy)] pub struct InstanceId(pub u64);
pub struct RequestId(pub ulid::Ulid);
pub struct JobId(pub ulid::Ulid);

#[derive(thiserror::Error)] // NOTE: protocol has no thiserror dep — hand-write Display + Error
pub enum IdError { Empty, TooLong { got: usize }, BadChar { at: usize, ch: char } }

impl BackendId { pub fn parse(s: &str) -> Result<Self, IdError>; pub fn as_str(&self) -> &str; }
// identical inherent impls on Alias, BuildId, RecipeId, ProfileId, ProviderId.
// All implement: Clone, Debug, Display, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize,
// Deserialize (deserialize goes through parse and REJECTS invalid ids).
```

### 3.3 `src/money.rs`

```rust
#[derive(Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Money(pub i64);                     // integer micro-USD
impl Money {
    pub const ZERO: Money = Money(0);
    pub fn from_usd(usd: f64) -> Money;        // rounds to nearest micro
    pub fn as_usd(self) -> f64;
    pub fn saturating_add(self, o: Money) -> Money;
    pub fn mul_f64(self, k: f64) -> Money;
}
impl std::fmt::Display for Money { /* "$3.34" / "$0.000305" adaptive */ }

#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CostEstimate {
    Metered     { usd: Money, source: PriceSource },
    Approximate { usd: Money, source: PriceSource, assumption: String },
    Unknown,
}
impl CostEstimate {
    pub fn usd(&self) -> Option<Money>;
    pub fn is_guess(&self) -> bool;
    pub fn add(self, other: CostEstimate) -> CostEstimate; // Metered+Metered=Metered, else weakest
}
pub enum PriceSource { ProviderApi, VastOffer, ConfigTable, RecipeField, Derived }

#[serde(tag = "kind", rename_all = "snake_case", content = "n")]
#[derive(Copy, Eq)]
pub enum TokenCount { Reported(u32), Estimated(u32) }
impl TokenCount { pub fn value(self) -> u32; pub fn is_reported(self) -> bool; }
```

### 3.4 `src/rig.rs`

```rust
pub struct RigSnapshot { pub gpus: Vec<Gpu>, pub builds: Vec<LlamaBuild>,
    pub ram_total_mb: u64, pub ram_free_mb: u64, pub swap_total_mb: u64, pub swap_used_mb: u64,
    pub cpu_threads: u32, pub scanned_at_unix: i64 }

/// ONE ENUMERATION, NOT ONE PIECE OF SILICON. Two backends see the same card as two `Gpu`s.
pub struct Gpu { pub device: String, pub index: u32, pub name: String, pub backend: Backend,
    pub vram_total_mb: u64, pub vram_free_mb: u64, pub pci_bus_id: Option<String>,
    pub driver: Option<String>,
    pub is_software: bool, pub seen_by_builds: Vec<BuildId>,
    pub held_by: Vec<BackendId>, pub reserved_mb: u64 }
impl Gpu {
    pub fn vram_used_mb(&self) -> Option<u64>;   // None when free > total (GTT). NEVER subtract.
    pub fn reports_gtt_overcommit(&self) -> bool;
    pub fn physical_key(&self, ordinal: usize) -> String;
}

/// One piece of silicon; every backend enumeration that reaches it. Derived, never stored.
pub struct PhysicalDevice { pub key: String, pub pci_bus_id: Option<String>, pub name: String,
    pub is_software: bool, pub views: Vec<Gpu> }
impl PhysicalDevice {
    pub fn backends(&self) -> Vec<Backend>;
    pub fn device_tokens(&self) -> Vec<String>;
    pub fn view_for(&self, backend: &Backend) -> Option<&Gpu>;  // VRAM is per backend, on purpose
    pub fn held_by(&self) -> Vec<BackendId>;
    pub fn seen_by_builds(&self) -> Vec<BuildId>;
}
impl RigSnapshot { pub fn physical_devices(&self) -> Vec<PhysicalDevice>; }
pub fn physical_devices(gpus: &[Gpu]) -> Vec<PhysicalDevice>;
pub fn normalise_device_name(name: &str) -> String;

pub enum Backend { Vulkan, Cuda, Rocm, Hip, Metal, Sycl, Cpu, Other(String) }

pub struct LlamaBuild { pub id: BuildId, pub server_path: String, pub label: String,
    pub build_info: Option<String>, pub backends: Vec<Backend>, pub devices: Vec<String>,
    pub flags: FlagSupport, pub probed_at_unix: i64 }

pub struct FlagSupport { pub flags: std::collections::BTreeSet<String>,
    pub jinja_default_on: bool, pub fa_tristate: bool, pub has_fit: bool,
    pub has_router_mode: bool, pub help_lines: u32 }
impl FlagSupport { pub fn has(&self, flag: &str) -> bool; }

pub struct LocalModel { pub id: String, pub name: String, pub dir: String,
    pub shards: Vec<ModelShard>, pub total_bytes: u64, pub mmproj: Vec<ModelShard>,
    pub quant: Option<String>, pub gguf: Option<GgufMeta>, pub discovered_at_unix: i64 }
pub struct ModelShard { pub path: String, pub bytes: u64 }
impl LocalModel { pub fn primary_path(&self) -> Option<&str>; pub fn is_vision(&self) -> bool; }

pub struct GgufMeta { pub arch: String, pub n_layer: u32, pub n_head_kv: u32,
    pub n_embd_head_k: u32, pub n_embd_head_v: u32, pub n_ctx_train: u32,
    pub full_attn_layers: Option<u32>, pub n_expert: Option<u32>, pub quant_desc: Option<String> }

pub struct BinaryChoiceInfo { pub chosen: BuildId, pub exact: bool,
    pub wanted: Option<Backend>, pub got: Option<Backend> }
```

### 3.5 `src/fit.rs`

```rust
pub struct DeviceBudget { pub device: String, pub free_mb: u64, pub reserved_mb: u64 }
/// PER BACKEND. One llama-server process uses one backend, so `devices` are all on `backend`
/// and this is never a sum across backends.
pub struct VramBudget { pub devices: Vec<DeviceBudget>, pub margin_mb: u64,
                        pub host_ram_free_mb: u64,
                        pub backend: Option<Backend>, pub notes: Vec<String> }
impl VramBudget {
    pub fn total_usable_mb(&self) -> u64;   // Σ (free - reserved) - margin, saturating
    pub fn largest_usable_mb(&self) -> u64;
    pub fn device_names(&self) -> Vec<String>;
}
pub struct FitInput { pub weights_bytes: u64, pub gguf: GgufMeta, pub budget: VramBudget,
    pub want_ctx: Option<u32>, pub want_parallel: Option<u32>, pub want_kv: Option<KvType>,
    pub split: SplitPlan, pub batch: Option<u32> }
pub enum KvType { F32, F16, Bf16, Q8_0, Q4_0, Q4_1, Iq4Nl, Q5_0, Q5_1 }
impl KvType { pub fn bytes_per_elem(self) -> f32; pub fn as_flag(self) -> &'static str; }

pub struct FitPlan { pub ctx: u32, pub parallel: u32, pub kv_type: KvType, pub ngl: NglPlan,
    pub split: SplitPlan, pub weights_mb: u64, pub kv_mb: u64, pub compute_mb: u64,
    pub headroom_mb: i64, pub per_device_mb: Vec<(String, u64)>, pub verdict: FitVerdict,
    pub why: Vec<String> }
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum FitVerdict { Fits { headroom_mb: u64 }, Tight { headroom_mb: u64 },
                      NeedsOffload { layers_on_gpu: u32 }, WontFit { short_by_mb: u64 } }
#[serde(tag = "ngl", rename_all = "snake_case")]
pub enum NglPlan { Auto, All, Layers(u32) }
pub struct SplitPlan { pub devices: Vec<String>, pub mode: SplitMode,
                       pub main_gpu: Option<u32>, pub tensor_split: Option<Vec<f32>> }
pub enum SplitMode { None, Layer, Row, Tensor }
impl Default for SplitPlan { /* devices: [], mode: Layer, main_gpu: None, tensor_split: None */ }
```

### 3.6 `src/backend.rs`

```rust
/// The wire dialect a listener accepts or an upstream speaks. ARCHITECTURE §3.4 is the matrix.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol { OpenAi, Anthropic }
impl Default for Protocol { fn default() -> Self { Protocol::OpenAi } }
impl Protocol { pub fn as_str(&self) -> &'static str; }   // "open_ai" | "anthropic"

pub enum BackendKind { LocalLlama, LocalVllm, VastLlama, VastVllm, Managed, Node }

pub struct Backend { pub id: BackendId, pub kind: BackendKind,
    #[serde(default)] pub protocol: Protocol,             // OpenAi unless the record says otherwise
    pub label: String,
    pub base_url: String,                 // INVARIANT: never ends in /v1
    pub credential: CredentialSource, pub tags: Vec<String>, pub models: Vec<UpstreamModel>,
    pub limits: BackendLimits, pub price: Option<PriceModel>, pub health: Health,
    pub provenance: Provenance, pub endpoint: Option<EndpointRef>, pub enabled: bool,
    pub devices: Vec<String>, pub last_error: Option<String> }

pub struct UpstreamModel { pub id: String, pub ctx: Option<u32>, pub vision: bool,
                           pub tools: bool }
pub struct BackendLimits { pub max_concurrent: u32, pub queue_depth: u32,
                           pub ctx: Option<u32>, pub slots_total: Option<u32> }
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PriceModel { PerToken { input: Money, output: Money }, PerHour { dph: Money }, Free }
impl PriceModel { pub fn per_mtok(&self, tps_hint: Option<f32>) -> CostEstimate; }

#[serde(tag = "state", rename_all = "snake_case")]
pub enum Health {
    Unknown,
    Starting { phase: BootPhase, since_unix: i64, detail: Option<String> },
    Ready    { since_unix: i64, slots_busy: u32, slots_total: u32, tps_p50: Option<f32> },
    Degraded { reason: String, consecutive_failures: u32 },
    Down     { reason: String, retry_at_unix: i64 },
    Draining { in_flight: u32 },
}
impl Health { pub fn is_routable(&self) -> bool; }   // Ready only

#[serde(tag = "phase", rename_all = "snake_case")]
pub enum BootPhase { Reserved, Provisioning, Pulling, Compiling,
    Downloading { pct: Option<f32>, mbps: Option<f32> }, Loading { pct: Option<f32> },
    Healthy, Failed { reason: String }, Destroyed }
impl BootPhase { pub fn is_terminal(&self) -> bool; }

pub enum Provenance { Discovered, Spawned, Rented, Manual, Adopted, Imported }

#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CredentialSource { None, Env { var: String }, File { path: String },
                            Managed { store: String }, Instance }
```

### 3.7 `src/endpoint.rs`

```rust
pub struct EndpointRef { pub id: BackendId, pub kind: BackendKind }

/// Persisted at $STATE/endpoints/<id>.json. NOTE: there is deliberately NO `status` field.
pub struct EndpointRecord { pub id: BackendId, pub spec: EndpointSpec, pub desired: DesiredState,
    pub proc: Option<ProcFacts>, pub port: Option<u16>, pub log_path: Option<String>,
    pub started_at_unix: i64, pub fit: Option<FitPlan>, pub adopted: bool,
    pub alias_bindings: Vec<Alias> }

pub enum DesiredState { Running, Stopped }
pub struct ProcFacts { pub pid: u32, pub start_time_ticks: u64, pub boot_id: String,
                       pub exe: String, pub cmdline_sha256: String }

#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EndpointSpec { LocalLlama(LocalLlamaSpec), LocalVllm(LocalVllmSpec),
                        Vast(VastSpec), Node(NodeSpec), Managed(ManagedSpec) }
impl EndpointSpec { pub fn kind(&self) -> BackendKind; pub fn suggested_id(&self) -> String; }

pub struct LocalLlamaSpec { pub build: BuildId, pub model_path: String,
    pub mmproj: Option<String>, pub alias_flag: String, pub host: String, pub port: Option<u16>,
    pub ctx: Option<u32>, pub parallel: Option<u32>, pub kv_type: Option<KvType>,
    pub ngl: NglPlan, pub split: SplitPlan, pub mode: SamplingMode,
    pub flash_attn: Option<TriState>, pub api_key: Option<CredentialSource>,
    pub extra_args: Vec<String> }
pub enum SamplingMode { Thinking, Coding, Nonthinking, Raw }
pub enum TriState { On, Off, Auto }

pub struct LocalVllmSpec { pub bin: String, pub model_id: String, pub tp: Option<u32>,
    pub ctx: Option<u32>, pub quantization: Option<String>, pub kv_cache_dtype: Option<String>,
    pub enforce_eager: bool, pub reasoning_parser: Option<String>, pub gpu_util: Option<f32>,
    pub max_num_seqs: Option<u32>, pub trust_remote: bool, pub chunked_prefill: bool,
    pub host: String, pub port: Option<u16>, pub devices: Vec<String>,
    pub extra_args: Vec<String> }

pub struct NodeSpec { pub base_url: String, pub credential: CredentialSource,
                      pub label: String, pub declared_models: Vec<String>,
                      #[serde(default)] pub protocol: Protocol }
pub struct ManagedSpec { pub provider: ProviderId, pub base_url: String,
                         pub credential: CredentialSource, pub model_id: Option<String>,
                         #[serde(default)] pub protocol: Protocol }
pub struct VastSpec { pub instance_id: InstanceId, pub runtime: ContainerRuntime,
                      pub launch: ContainerLaunch, pub tunnel: Option<TunnelSpec> }

pub struct TunnelSpec { pub instance_id: InstanceId, pub local_port: u16,
                        pub remote_port: u16, pub ssh_host: String, pub ssh_port: u16 }
pub struct TunnelStatus { pub spec: TunnelSpec, pub up: bool, pub proc: Option<ProcFacts>,
    pub since_unix: Option<i64>, pub restarts: u32, pub last_error: Option<String> }

pub struct ArgvPreview { pub program: String, pub args: Vec<String>,
    pub env: Vec<(String, String)>, pub cwd: String, pub warnings: Vec<String> }
pub struct SwapReport { pub alias: Alias, pub mode: SwapMode, pub from: Option<BackendId>,
    pub to: BackendId, pub parked: u32, pub drained_ms: u32, pub total_ms: u32 }
```

### 3.8 `src/route.rs`

```rust
pub struct ModelRoute { pub alias: Alias, pub targets: Vec<RouteTarget>, pub strategy: Strategy,
    pub filter: RouteFilter, pub retry: RetryPolicy, pub is_default: bool,
    pub description: Option<String> }
pub struct RouteTarget { pub backend: BackendSelector, pub model: Option<String>, pub weight: u32 }
#[serde(tag = "sel", rename_all = "snake_case")]
pub enum BackendSelector { Id(BackendId), Tag(String), Glob(String) }
/// mk1 ships EXACTLY the strategies it implements — no config value can reach a todo!().
pub enum Strategy { FirstHealthy, RoundRobin, LeastBusy, Cheapest }
pub struct RouteFilter { pub require_tags: Vec<String>, pub exclude_tags: Vec<String>,
    pub max_cost_per_mtok: Option<Money>, pub min_ctx: Option<u32>,
    pub require_vision: bool, pub require_tools: bool }
pub struct RetryPolicy { pub attempts: u8, pub failover: bool, pub honor_retry_after: bool }
impl Default for RetryPolicy { /* attempts: 2, failover: true, honor_retry_after: true */ }

pub enum RouteReason { Alias, ExplicitPin, UpstreamIdMatch, ImplicitMulti,
                       DefaultFallback, LegacyModelName }
impl RouteReason { pub fn as_str(&self) -> &'static str; }   // for the X-ApexRouter-Route header
pub enum SwapMode { Hot, Sequential }

pub struct RouteFile { pub schema_version: u32, pub default_alias: Alias,
                       pub routes: Vec<ModelRoute> }
pub struct ValidationReport { pub ok: bool, pub issues: Vec<ValidationIssue> }
pub struct ValidationIssue { pub field: String, pub severity: Severity,
                             pub message: String, pub fix: Option<String> }
pub enum Severity { Info, Warning, Error }
```

### 3.9 `src/telemetry.rs`

```rust
pub struct RequestRecord { pub id: RequestId, pub started_unix: i64, pub alias: Option<Alias>,
    pub backend: Option<BackendId>, pub upstream_model: Option<String>,
    pub route_reason: RouteReason,
    #[serde(default)] pub ingress: Protocol,   // the dialect the CLIENT spoke; the upstream's lives
                                               // on the Backend, so the pair names the matrix cell
    pub method: String, pub path: String, pub status: u16,
    pub attempts: u8, pub streamed: bool, pub aborted: bool, pub ttft_ms: Option<u32>,
    pub total_ms: u32, pub prompt_tokens: Option<TokenCount>,
    pub completion_tokens: Option<TokenCount>, pub cached_tokens: Option<u32>,
    pub tok_per_s: Option<f32>, pub cost: CostEstimate, pub error: Option<String> }

pub struct UsageRecord { pub timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub epoch: Option<f64>,
    pub provider: String, pub model_id: String, pub prompt_tokens: u32,
    pub completion_tokens: u32, pub cost_usd: f64,
    #[serde(default)] pub request_id: Option<String>,
    #[serde(default)] pub backend: Option<String>,
    #[serde(default)] pub alias: Option<String>,
    #[serde(default)] pub ttft_ms: Option<u32>,
    #[serde(default)] pub tok_per_s: Option<f32>,
    #[serde(default)] pub stream: Option<bool>,
    #[serde(default)] pub estimated: Option<bool>,
    #[serde(flatten)] pub extra: serde_json::Map<String, serde_json::Value> }

pub struct UsageSummary { pub window: String, pub by: Vec<UsageBucket>,
    pub total_cost: CostEstimate, pub total_prompt: u64, pub total_completion: u64, pub rows: u64 }
pub struct UsageBucket { pub key: String, pub cost: CostEstimate, pub prompt_tokens: u64,
    pub completion_tokens: u64, pub requests: u64, pub tok_per_s_p50: Option<f32> }

pub struct SmokeProbe { pub name: String, pub ok: bool, pub ms: u32, pub detail: String,
    pub ttft_ms: Option<u32>, pub tok_per_s: Option<f32>, pub tokens: Option<u32> }
pub struct CompareRow { pub alias: Alias, pub backend: Option<BackendId>, pub model: String,
    pub ok: bool, pub ms: u32, pub ttft_ms: Option<u32>, pub tok_per_s: Option<f32>,
    pub prompt_tokens: Option<TokenCount>, pub completion_tokens: Option<TokenCount>,
    pub cost: CostEstimate, pub preview: String, pub error: Option<String> }
```

### 3.10 `src/catalog.rs`

```rust
pub struct Recipe { pub id: RecipeId, pub label: String, pub description: Option<String>,
    pub kind: RecipeKind, pub provenance: Provenance2,
    pub created_at_unix: i64, pub updated_at_unix: i64 }
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecipeKind { Local(LocalLlamaSpec), LocalVllm(LocalVllmSpec),
    Vast { profile: ProfileId, launch: ContainerLaunch, fit: Option<FitPlan> },
    Managed(ManagedSpec) }
pub struct Provenance2 { pub discovered_at_unix: i64, pub size_bytes: Option<u64>,
                         pub source: String, pub fit: Option<FitPlan> }

pub struct SearchProfile { pub id: ProfileId, pub label: String, pub gpu_names: Vec<String>,
    pub num_gpus_min: u32, pub num_gpus_max: u32, pub max_dph: Option<Money>,
    pub min_reliability: f32, pub min_inet_down: u32, pub min_disk_gb: u32,
    pub min_cuda: Option<f32>, pub geo: GeoFilter, pub image_type: ImageType,
    #[serde(default)] pub extra: serde_json::Map<String, serde_json::Value> }
pub enum GeoFilter { Any, EuNordic, Eu, Us, Codes(Vec<String>) }
impl GeoFilter { pub fn codes(&self) -> Vec<&str>; pub fn matches(&self, geolocation: &str) -> bool; }
pub enum ImageType { Prebuilt, Builder, Vllm }

pub struct ContainerLaunch { pub runtime: ContainerRuntime, pub image: String,
    pub image_type: ImageType, pub disk_gb: u32,
    pub env: std::collections::BTreeMap<String, String>, pub onstart: String,
    pub host: String, pub port: u16, pub expose_public: bool }
pub enum ContainerRuntime { LlamaCpp, Vllm }
pub struct ContainerEnvPreview { pub env: Vec<(String, String)>, pub onstart: String,
    pub image: String, pub args_override: Vec<String>, pub warnings: Vec<String> }

pub struct MigrationPlan { pub items: Vec<MigrationItem>, pub source_paths: Vec<String> }
pub struct MigrationItem { pub what: String, pub from: String, pub action: MigrationAction,
                           pub detail: String }
pub enum MigrationAction { Import, Skip, Warn }
pub struct MigrationReport { pub imported: u32, pub skipped: u32, pub warnings: Vec<String> }
```

### 3.11 `src/vast.rs`

```rust
pub struct Offer { /* exactly the 28 fields listed in ARCHITECTURE.md §3.8 */
    #[serde(flatten)] pub extra: serde_json::Map<String, serde_json::Value> }
impl Offer { pub fn pooled_vram_mb(&self) -> u64; pub fn geo_code(&self) -> Option<&str>; }

pub struct OfferQuery { pub gpu_names: Vec<String>, pub num_gpus_min: u32, pub num_gpus_max: u32,
    pub max_dph: Option<f64>, pub min_reliability: Option<f64>, pub min_inet_down: Option<f64>,
    pub min_disk_gb: Option<u32>, pub min_cuda: Option<f64>, pub geo: GeoFilter,
    pub verified: Option<bool>, pub limit: u32, pub order: Vec<(String, String)>,
    #[serde(default)] pub extra: serde_json::Map<String, serde_json::Value> }
pub struct OfferSearchResult { pub offers: Vec<Offer>, pub relaxations: Vec<String>,
                               pub queried_at_unix: i64, pub gpu_name_vocabulary: Vec<String> }

pub struct VastAccount { pub id: u64, pub credit: f64, pub balance: Option<f64>,
                         pub can_pay: Option<bool>, pub has_billing: Option<bool> }
                         // deliberately NO api_key field: the API echoes one, we never model it.

pub struct VastInstance { /* exactly the fields listed in ARCHITECTURE.md §3.8 */
    #[serde(flatten)] pub extra: serde_json::Map<String, serde_json::Value> }
impl VastInstance {
    pub fn phase(&self) -> BootPhase;
    pub fn is_terminal(&self) -> bool;                       // exited | offline | unknown
    pub fn external_port(&self, internal: u16) -> Option<(String, u16)>;  // tolerant `ports` reader
    pub fn uptime_secs(&self) -> Option<f64>;                // computed from start_date
}

pub struct LedgerRow { pub seq: u64, pub at_unix: i64, pub instance_id: Option<InstanceId>,
    pub state: LedgerState, pub offer_id: Option<u64>, pub profile: Option<ProfileId>,
    pub gpu: Option<String>, pub num_gpus: Option<u32>, pub dph: Option<f64>,
    pub approved_max_dph: Option<f64>, pub approval_source: Option<String>,
    pub destroyed_at_unix: Option<i64>, pub est_cost: CostEstimate, pub note: Option<String> }
pub enum LedgerState { Reserved, Confirmed, Running, DestroyRequested, Destroyed,
                       OrphanSuspect, Reconciled }

pub struct RentRequest { pub profile: Option<ProfileId>, pub offer_id: Option<u64>,
    pub launch: ContainerLaunch, pub confirm: bool, pub max_usd_per_hour: f64,
    pub auto_tunnel: bool, pub bind_alias: Option<Alias> }
pub struct ApprovalRequest { pub id: JobId, pub what: String, pub max_usd_per_hour: f64,
    pub est_total_usd: f64, pub credit: Option<f64>, pub requested_at_unix: i64,
    pub source: String }
pub struct DownloadHealth { pub sampled_at_unix: i64, pub rx_bytes_4s: u64,
                            pub mbps: f32, pub verdict: StallVerdict }
pub enum StallVerdict { Active, Slow, Stalled }
```

### 3.12 `src/provider.rs`, `src/check.rs`, `src/hf.rs`

```rust
// provider.rs
pub struct ProviderStatus { pub id: ProviderId, pub base_url: String,
    pub credential: CredentialSource, pub credential_present: bool,
    pub models_cached: u32, pub last_ok_unix: Option<i64>, pub last_error: Option<String>,
    pub rate_limit: Option<RateLimitInfo> }
pub struct RateLimitInfo { pub limit: Option<u64>, pub remaining: Option<u64>,
                           pub reset_unix: Option<i64> }

// check.rs
pub struct CheckId(pub String);          // "creds.vast", "ports.proxy", "smoke.throughput"
pub struct CheckResult { pub id: CheckId, pub label: String, pub status: CheckStatus,
    pub ms: u32, pub detail: String, pub fix: Option<String> }
pub enum CheckStatus { Pass, Warn, Fail, Skipped }

// hf.rs
pub struct HfModel { pub id: String, pub author: Option<String>, pub downloads: Option<u64>,
    pub likes: Option<u64>, pub gated: bool, pub last_modified: Option<String>,
    pub tags: Vec<String> }
pub struct HfFile { pub rfilename: String, pub size: Option<u64>, pub quant: Option<String>,
    pub is_mmproj: bool, pub shard_of: Option<(u32, u32)> }
pub struct HfFileGroup { pub label: String, pub quant: Option<String>, pub total_bytes: u64,
    pub files: Vec<HfFile>, pub mmproj: Vec<HfFile> }
pub struct DownloadProgress { pub job: JobId, pub repo: String, pub file: String,
    pub bytes_done: u64, pub bytes_total: Option<u64>, pub mbps: f32 }
```

### 3.13 `src/event.rs`

```rust
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Snapshot(Box<Snapshot>),
    BackendChanged { backend: Box<Backend> },
    BackendRemoved { id: BackendId },
    RouteTableChanged { routes: Vec<ModelRoute>, valid: bool, error: Option<String> },
    RigChanged { rig: Box<RigSnapshot> },
    RequestStarted { id: RequestId, alias: Option<Alias>, backend: Option<BackendId> },
    RequestFinished { record: Box<RequestRecord> },
    BootProgress { backend: BackendId, phase: BootPhase, line: Option<String> },
    LogLine { source: LogSource, line: String },
    VastFleetChanged { instances: Vec<VastInstance>, credit: Option<f64> },
    UsageTick { window: Box<UsageSummary> },
    JobChanged { job: Box<JobRecord> },
    CheckResult { result: CheckResult },
    Alert { level: AlertLevel, message: String, action: Option<String>, id: String },
}
pub enum AlertLevel { Info, Warning, Serious, Critical }
#[serde(tag = "src", rename_all = "snake_case")]
pub enum LogSource { Endpoint { id: BackendId }, Instance { id: InstanceId }, Daemon }
pub struct Alert { pub id: String, pub level: AlertLevel, pub message: String,
                   pub action: Option<String>, pub at_unix: i64 }

pub struct Snapshot { pub product: String, pub version: String, pub served_by: ServedBy,
    pub as_of_unix: i64, pub stale: bool, pub proxy: ProxyStatus, pub backends: Vec<Backend>,
    pub routes: Vec<ModelRoute>, pub endpoints: Vec<EndpointRecord>, pub rig: RigSnapshot,
    pub instances: Vec<VastInstance>, pub tunnels: Vec<TunnelStatus>,
    pub providers: Vec<ProviderStatus>, pub recipes: Vec<Recipe>,
    pub profiles: Vec<SearchProfile>, pub totals: Totals, pub alerts: Vec<Alert>,
    pub jobs: Vec<JobRecord> }
pub enum ServedBy { Daemon, Offline }
pub struct ProxyStatus { pub base_url: String, pub control_url: String, pub uptime_secs: f64,
    pub inflight: u32, pub req_per_min: f32, pub tok_per_s: f32, pub default_alias: Alias,
    pub table_valid: bool, pub table_error: Option<String> }
pub struct Totals { pub spend_24h: CostEstimate, pub spend_7d: CostEstimate,
    pub tokens_24h: u64, pub vast_credit: Option<f64>, pub burn_rate_usd_hr: Money,
    pub burn_down_hours: Option<f32> }
pub struct JobRecord { pub id: JobId, pub kind: String, pub state: JobState,
    pub pct: Option<f32>, pub message: Option<String>, pub started_unix: i64,
    pub finished_unix: Option<i64>, pub result: Option<serde_json::Value>,
    pub error: Option<String> }
pub enum JobState { Pending, Running, Succeeded, Failed, Cancelled }

/// The envelope EVERY `--json` CLI output and every control-plane GET is wrapped in.
pub struct Envelope<T> { pub served_by: ServedBy, pub as_of_unix: i64, pub stale: bool,
                         #[serde(flatten)] pub data: T }
pub struct ErrorEnvelope { pub error: ErrorBody }
pub struct ErrorBody { pub kind: String, pub message: String,
                       pub param: Option<String>, pub code: Option<String> }
```

---

## 4. Work units

Format: **`unit-id` — files owned (exclusive) — signatures — acceptance — stage.**
Every unit's files are disjoint from every other unit's in the same stage *and* across stages.

### Stage 1 — core foundations (8 agents, parallel)

**C-01 · paths + error** — owns `core/src/paths.rs`, `core/src/error.rs`
```rust
pub struct Paths { /* … */ }
impl Paths {
    pub fn resolve() -> Result<Paths>;              // $APEXROUTER_CONFIG -> $APEXROUTER_HOME -> XDG
    pub fn config_file(&self) -> PathBuf;   pub fn state(&self) -> &Path;
    pub fn cache(&self) -> &Path;           pub fn routes_file(&self) -> PathBuf;
    pub fn backends_file(&self) -> PathBuf; pub fn tunnels_file(&self) -> PathBuf;
    pub fn catalog_file(&self) -> PathBuf;  pub fn credentials_file(&self) -> PathBuf;
    pub fn ledger(&self) -> PathBuf;        pub fn usage_log(&self) -> PathBuf;
    pub fn endpoints_dir(&self) -> PathBuf; pub fn endpoint_file(&self, id: &BackendId) -> PathBuf;
    pub fn jobs_dir(&self) -> PathBuf;      pub fn approvals_dir(&self) -> PathBuf;
    pub fn logs_dir(&self) -> PathBuf;      pub fn log_file(&self, id: &BackendId) -> PathBuf;
    pub fn ssh_dir(&self) -> PathBuf;       pub fn known_hosts(&self) -> PathBuf;
    pub fn control_path(&self, id: InstanceId) -> PathBuf;
    pub fn daemon_lock(&self) -> PathBuf;   pub fn state_lock(&self) -> PathBuf;
    pub fn legacy(&self) -> &LegacyPaths;
    pub fn ensure_layout(&self) -> Result<()>;
}
pub struct LegacyPaths { pub vastai_gguf: PathBuf, pub localrouter_dir: Option<PathBuf>,
    pub vast_key: PathBuf, pub hf_token: PathBuf }
#[derive(thiserror::Error)] pub enum Error { /* Io, Toml, Json, Reqwest, NotFound, Invalid,
    MissingCredential, PortInUse, InsufficientVram, Conflict, … */ }
pub type Result<T> = std::result::Result<T, Error>;
```
*Acceptance*: unit tests cover all three config-resolution branches and both state branches with
`$HOME` redirected to a `tempdir`. `ensure_layout()` is idempotent and creates every dir at `0700`.
Nothing resolves to a path inside the repo.

**C-02 · config** — owns `core/src/config.rs`, and the repo-root `config.example.toml`
```rust
pub struct Config { /* one struct per [section] in ARCHITECTURE §5.2; EVERY field defaulted */ }
pub struct ServerCfg; pub struct RouterCfg; pub struct SupervisorCfg; pub struct EndpointsCfg;
pub struct ProviderCfg; pub struct VastCfg; pub struct HfCfg; pub struct DockerCfg;
pub struct KnownFork; pub struct CompatCfg;
impl Config {
    pub fn load() -> Result<Config>;
    pub fn load_from(path: Option<&Path>, home: Option<&Path>) -> Result<Config>;
    pub fn init_file(paths: &Paths, force: bool) -> Result<PathBuf>;
    pub fn save(&self, paths: &Paths) -> Result<()>;     // writes ConfigFile, 0600, toml_edit
    pub fn serializable(&self) -> ConfigFile;
    pub fn image_for(&self, t: ImageType) -> String;
    pub fn known_fork_for(&self, repo: &str) -> Option<&KnownFork>;
    pub fn proxy_bind(&self) -> SocketAddr;   // honours $PROXY_PORT
    pub fn control_bind(&self) -> SocketAddr;
}
```
*Acceptance*: a missing file yields a fully working `Config` (test asserts every default against
§5.2 literally). `$PROXY_PORT=9999` changes `proxy_bind()`. `save()` round-trips through `toml_edit`
preserving a hand-written comment. `ConfigFile` has **no** field capable of holding a borrowed
credential (compile-time: a test that tries to set one must not compile — express as a doc test).

**C-03 · secret + credential chain** — owns `core/src/secret.rs`
```rust
pub struct Secret<T>(T);
impl Secret<String> { pub fn new(s: String) -> Self; pub fn expose(&self) -> &str; }
// Debug and Display BOTH print "***". A test asserts format!("{:?}", s) contains no key material.
pub enum CredentialRef { Env(String), File(PathBuf), Inline(Secret<String>), None }
pub struct ResolvedCredential { pub secret: Secret<String>, pub source: CredentialSource }
pub fn resolve_credential(cfg_inline: Option<&Secret<String>>, cfg_file: Option<&Path>,
    conventional: &[&Path], env_var: Option<&str>) -> Result<Option<ResolvedCredential>>;
pub fn resolve_vast(cfg: &Config, paths: &Paths) -> Result<Option<ResolvedCredential>>;
pub fn resolve_hf(cfg: &Config, paths: &Paths) -> Result<Option<ResolvedCredential>>;
pub fn resolve_provider(cfg: &Config, paths: &Paths, id: &ProviderId)
    -> Result<Option<ResolvedCredential>>;
/// Only ever called with a key the USER typed. Writes $STATE/credentials.toml at 0600.
pub fn store_user_credential(paths: &Paths, id: &ProviderId, key: Secret<String>) -> Result<()>;
```
*Acceptance*: the order is exactly explicit → config file → conventional path → env var, proven by a
four-case test with a tempdir. `~/.vastai-gguf/config.toml` is parsed with `toml`, not string
matching, and its `base_url` is returned unmodified. Every read is `.trim()`ed. A borrowed key never
reaches `store_user_credential`.

**C-04 · store + atomic writes + state lock** — owns `core/src/store.rs`
```rust
pub struct Store { paths: Paths }
impl Store {
    pub fn new(paths: Paths) -> Store;
    pub fn write_atomic(&self, path: &Path, bytes: &[u8], mode: u32) -> Result<()>;
      // tmp in the SAME dir -> write -> fsync(file) -> rename -> fsync(dir); mode at OpenOptions
    pub fn read_json<T: DeserializeOwned>(&self, path: &Path) -> Result<Option<T>>;
    pub fn write_json<T: Serialize>(&self, path: &Path, v: &T) -> Result<()>;
    pub fn with_state_lock_shared<R>(&self, f: impl FnOnce() -> Result<R>) -> Result<R>;
    pub fn with_state_lock_exclusive<R>(&self, f: impl FnOnce() -> Result<R>) -> Result<R>;
    pub fn list_endpoints(&self) -> Result<Vec<EndpointRecord>>;
    pub fn put_endpoint(&self, r: &EndpointRecord) -> Result<()>;
    pub fn remove_endpoint(&self, id: &BackendId) -> Result<()>;
    pub fn load_routes(&self) -> Result<RouteFile>;      pub fn save_routes(&self, f: &RouteFile) -> Result<()>;
    pub fn load_backends(&self) -> Result<Vec<Backend>>; pub fn save_backends(&self, b: &[Backend]) -> Result<()>;
    pub fn load_tunnels(&self) -> Result<Vec<TunnelStatus>>; pub fn save_tunnels(&self, t: &[TunnelStatus]) -> Result<()>;
}
```
*Acceptance*: a concurrent-write test (16 threads × 100 updates) leaves a parseable file with no
lost update. A reader during a rename never sees a partial file. Every written file is `0600`.

**C-05 · daemon lock + owner record** — owns `core/src/lockfile.rs`
```rust
pub struct DaemonLock { file: File }                     // held for the process lifetime
pub struct OwnerRecord { pub pid: u32, pub start_time_ticks: u64, pub boot_id: String,
    pub version: String, pub proxy_url: String, pub control_url: String,
    pub started_at_unix: i64 }
pub enum DaemonProbe { Owned(OwnerRecord), Free }
impl DaemonLock {
    /// LOCK_EX|LOCK_NB. The File is O_CLOEXEC so a spawned llama-server can NEVER inherit it.
    pub fn acquire(paths: &Paths) -> Result<DaemonLock>;
    pub fn write_owner(&mut self, rec: &OwnerRecord) -> Result<()>;
}
pub fn probe(paths: &Paths) -> Result<DaemonProbe>;      // try LOCK_EX|LOCK_NB, then read the record
/// Offline mutations (migrate --apply, config init) take this and thereby PROVE no daemon runs.
pub fn acquire_offline_exclusive(paths: &Paths) -> Result<DaemonLock>;
```
*Acceptance*: **an integration test spawns a child process, kills the parent that holds the lock,
and asserts a fresh `acquire()` succeeds** — this is the CLOEXEC regression test. A second
`acquire()` while held returns `Owned(record)`. Only the daemon path ever touches this file.

**C-06 · proc identity + liveness + spawn** — owns `core/src/proc.rs`
```rust
pub enum Liveness { Alive, Zombie, Dead, Unknown(std::io::Error) }
pub enum Adoption { Adopted(ProcFacts), Foreign { pid: u32, why: String }, Vanished,
                    Ambiguous { pid: u32, why: String } }
pub fn boot_id() -> Result<String>;                      // /proc/sys/kernel/random/boot_id
pub fn start_time_ticks(pid: u32) -> Result<u64>;        // /proc/<pid>/stat field 22,
                                                         // parsed AFTER THE LAST ')' — comm may
                                                         // contain spaces and parentheses
pub fn cmdline(pid: u32) -> Result<Vec<String>>;
pub fn exe_path(pid: u32) -> Result<String>;             // strips a trailing " (deleted)"
pub fn liveness(f: &ProcFacts) -> Liveness;
pub fn identify(pid: u32, argv: &[String], exe: &str) -> Result<ProcFacts>;
pub fn adopt(rec: &EndpointRecord) -> Adoption;
pub fn port_free(port: u16) -> bool;                     // bind-probe on 127.0.0.1
pub fn alloc_port(range: (u16, u16), taken: &[u16]) -> Option<u16>;
pub struct SpawnRequest<'a> { pub program: &'a Path, pub args: &'a [String],
    pub env: &'a [(String, String)], pub cwd: &'a Path, pub log: &'a Path, pub setsid: bool }
pub struct SpawnedChild { pub pid: u32, pub facts: ProcFacts }
/// setsid + Stdio::from(owned File) + O_APPEND. Never truncates the log.
pub fn spawn_detached(req: SpawnRequest<'_>) -> Result<SpawnedChild>;
pub fn signal_verified(f: &ProcFacts, sig: Signal) -> Result<()>;   // re-verifies identity FIRST
pub fn stop_graceful(f: &ProcFacts, term_wait: Duration, kill_wait: Duration) -> Result<()>;
```
*Acceptance*: a test spawns `/bin/sleep 60` with a `comm` containing a paren (via a copied binary
named `sl e)ep`) and asserts `start_time_ticks` parses correctly. `signal_verified` on a
deliberately-mismatched `ProcFacts` returns an error and sends **no** signal. `EPERM` yields
`Unknown`, never a panic. `spawn_detached` output shows the child's parent becomes pid 1.

**C-07 · exec wrapper** — owns `core/src/exec.rs`
```rust
pub struct Output { pub status: i32, pub stdout: String, pub stderr: String, pub timed_out: bool }
/// argv vector only. There is NO variant that takes a shell string. Timeout is REQUIRED.
pub async fn run(program: &Path, args: &[&str], timeout: Duration) -> Result<Output>;
pub async fn run_env(program: &Path, args: &[&str], env: &[(&str, &str)], timeout: Duration)
    -> Result<Output>;
pub async fn ssh(host: &str, port: u16, opts: &SshOpts, remote_argv: &[&str], timeout: Duration)
    -> Result<Output>;
pub struct SshOpts { pub known_hosts: PathBuf, pub control_path: Option<PathBuf>,
    pub connect_timeout: u32, pub extra: Vec<String> }
```
*Acceptance*: stdout and stderr are separate in every path (no `2>&1` exists in the API). A timeout
returns `timed_out: true`, never `rc 124`. `ssh()` always emits
`-o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=<known_hosts>`.

**C-08 · ledger + spend approval** — owns `core/src/ledger.rs`, `core/src/money.rs`
```rust
pub struct Ledger { path: PathBuf }
impl Ledger {
    pub fn open(paths: &Paths) -> Result<Ledger>;
    /// SYNCHRONOUS (Drop cannot await). O_APPEND, one write() per row.
    pub fn append(&self, row: &LedgerRow) -> Result<u64>;
    pub fn rows(&self) -> Result<Vec<LedgerRow>>;
    pub fn active(&self) -> Result<Vec<LedgerRow>>;     // "active" is a QUERY, not a file
    pub fn reserve(&self, req: &RentRequest, approval: &SpendApproval) -> Result<PendingLaunch>;
}
pub struct PendingLaunch { /* ledger, seq, committed: bool */ }
impl PendingLaunch { pub fn commit(self, id: InstanceId) -> Result<()>; }
impl Drop for PendingLaunch { /* if !committed -> append OrphanSuspect synchronously */ }

#[non_exhaustive]
pub struct SpendApproval { max_usd_per_hour: Money, confirmed_at: i64, source: ApprovalSource }
pub enum ApprovalSource { Cli, WebUi, SlintUi, Mcp { human_cleared: bool }, Api }
pub enum ApprovalError { AboveCeiling { requested: Money, ceiling: Money },
    HumanConfirmationRequired { pending: JobId }, InsufficientCredit { credit: Money, needed: Money } }
impl SpendApproval {
    /// The ONLY constructor. Enforces cfg.max_usd_per_hour_ceiling and require_human_confirm.
    pub fn confirm(requested: Money, source: ApprovalSource, cfg: &VastCfg, credit: Option<f64>)
        -> std::result::Result<SpendApproval, ApprovalError>;
    pub fn max_usd_per_hour(&self) -> Money;
}
```
*Acceptance*: the fields are private and there is **no** `pub` struct-literal path — a doc test
asserting `SpendApproval { .. }` fails to compile. `PendingLaunch` dropped without `commit` appends
`OrphanSuspect` (test with `std::mem::drop`). A ceiling of `$4.00` rejects a `$9.00` request.
`ApprovalSource::Mcp { human_cleared: false }` with `require_human_confirm` returns
`HumanConfirmationRequired`.

**Stage 1 gate**: `cargo test -p apexrouter-core` green; `cargo clippy -p apexrouter-core -D warnings`
clean.

---

### Stage 2 — core capabilities (9 agents, parallel)

**C-09 · build + device discovery** — owns `core/src/discover/builds.rs`, `core/src/discover/mod.rs`
```rust
pub async fn discover_builds(cfg: &EndpointsCfg, cache: &Path) -> Result<Vec<LlamaBuild>>;
pub async fn probe_devices(server: &Path) -> Result<Vec<Gpu>>;    // llama-server --list-devices
pub async fn probe_flags(server: &Path, cache: &Path) -> Result<FlagSupport>;  // --help, cached
pub fn choose_build(builds: &[LlamaBuild], want: Option<Backend>) -> Option<BinaryChoiceInfo>;
```
*Acceptance*: on the real machine it finds **five** builds including `build-mtp` and `build-zaya1`
(glob `build*/bin/llama-server`), labels them by build-dir name, reports `Backend::Vulkan` for
`build-vulkan` **via `--list-devices`, never by grepping `--help`** (a test asserts the help text
contains zero occurrences of "vulkan"/"cuda"/"hip"/"rocm"), and excludes `llvmpipe` from
`Gpu` unless `is_software` is explicitly requested. `choose_build` returns `exact: false` with
`wanted`/`got` populated rather than silently substituting. `probe_flags` cache key is
`(path, mtime, size)`.

**C-10 · model discovery + GGUF header** — owns `core/src/discover/models.rs`,
`core/src/discover/gguf.rs`
```rust
pub async fn discover_models(cfg: &EndpointsCfg) -> Result<Vec<LocalModel>>;
pub fn read_gguf_meta(path: &Path) -> Result<GgufMeta>;    // header only; bounded read
```
*Acceptance*: on the real machine it finds `~/models/carnice-9b/Carnice-9b-Q6_K.gguf` (recursing
into per-model subdirectories), ignores `.cache`, follows symlinks, groups `-00001-of-000NN` shards
into one `LocalModel` with a summed `total_bytes`, pairs `mmproj-*.gguf` into `LocalModel.mmproj`,
matches `mmproj`/`vocab` as **filename tokens, not path substrings** (a directory named `vocab-x`
must not hide its contents), sorts **smallest first**, and tolerates a models dir with nothing in it.
`read_gguf_meta` extracts `n_layer`, `n_head_kv`, `n_embd_head_k/v`, `n_ctx_train` from the real
Carnice file, handles typed KV/array/string values, and never reads more than 8 MiB.

**C-11 · the fit solver** — owns `core/src/fit.rs`
```rust
pub fn fit(input: &FitInput) -> FitPlan;                              // pure, no I/O
pub enum BackendScope<'a> { Build(&'a BuildId), Backend(&'a Backend), Auto }
pub fn budget_from_rig(rig: &RigSnapshot, scope: BackendScope<'_>, devices: &[String],
                       margin_mb: u64,
                       running: &[EndpointRecord]) -> VramBudget;     // subtracts reservations
```
*Acceptance*: unit tests calibrate against the archived run log in `docs/port/03` — Qwen3.5-9B
Q4_K_M, ctx 32768, kv q8_0, Vulkan → 5956 MiB total = 4861 weights + 594 KV + 501 compute, within
±10%. Hybrid models use `full_attn_layers` when present. `ctx` is the **total** pool shared across
`parallel`. A multi-device budget sums correctly and `per_device_mb` splits by `tensor_split` when
given, else evenly by `SplitMode::Layer`. `budget_from_rig` subtracts `fit.weights+kv+compute` of
every `EndpointRecord` whose `desired == Running`. `why[]` is non-empty and human-readable.
**A budget is per backend and is NEVER a sum across backends** (MK1-CORE acceptance finding A):
`scope` resolves to exactly one backend, `devices` narrows within it and can never widen across it,
and everything dropped lands in `VramBudget::notes` → `FitPlan::why`. A synthetic two-backend
one-device rig must not sum; a synthetic 4-card CUDA rig must. Reservations are attributed by
`Gpu::physical_key`, so a card held through one backend is subtracted from its budget under
another. `fit()` refuses to add up a caller-supplied budget whose tokens mix backends: the largest
single-backend group wins, loudly.

**C-11b · physical device identity** — owns `core/src/discover/physical.rs`
```rust
pub struct PciGpu { pub bus_id: String, pub vendor_id: u16, pub device_id: u16,
                    pub vendor: GpuVendor }
pub enum GpuVendor { Amd, Nvidia, Intel, Unknown }
pub fn scan_pci_gpus() -> Vec<PciGpu>;                  // /sys/bus/pci/devices, class 0x03xxxx
pub fn scan_pci_gpus_in(root: &Path) -> Vec<PciGpu>;    // same, against a synthetic sysfs
pub fn attach_pci_ids(gpus: &mut [Gpu], pci: &[PciGpu]);   // pure; called by probe_devices
```
*Acceptance*: on the real machine both `ROCm0` and `Vulkan0` come back with
`pci_bus_id == Some("0000:04:00.0")` and `RigSnapshot::physical_devices()` returns **one** device
with two backends. Alignment is per backend, bucketed by the vendor inferred from the device name,
and happens **only** when the counts agree exactly — an ambiguous rig gets `None` and falls back to
the documented name heuristic rather than to a guess. `llvmpipe` never receives a bus id.

**C-12 · the argv/env builder (ONE builder, both targets)** — owns `core/src/argv.rs`
```rust
pub fn plan_local(spec: &LocalLlamaSpec, build: &LlamaBuild, key_file: Option<&Path>)
    -> Result<ArgvPreview>;
pub fn plan_local_vllm(spec: &LocalVllmSpec) -> Result<ArgvPreview>;
pub fn plan_container(launch_in: &ContainerLaunchInput, cfg: &Config)
    -> Result<(ContainerLaunch, ContainerEnvPreview)>;
pub struct ContainerLaunchInput { pub runtime: ContainerRuntime, pub image_type: Option<ImageType>,
    pub model_repo: Option<String>, pub model_quant: Option<String>, pub model_id: Option<String>,
    pub ctx: Option<u32>, pub parallel: Option<u32>, pub kv_type: Option<KvType>,
    pub mode: SamplingMode, pub mmproj: Option<String>, pub disk_gb: u32,
    pub tp: Option<u32>, pub quantization: Option<String>, pub kv_cache_dtype: Option<String>,
    pub enforce_eager: bool, pub reasoning_parser: Option<String>, pub expose_public: bool,
    pub hf_token: Option<Secret<String>> }
pub fn sampling_flags(mode: SamplingMode) -> &'static [&'static str];
pub fn backend_env(backend: Backend, devices: &[String]) -> Vec<(String, String)>;
```
*Acceptance*: all three sampling presets include `--top-k 20` (`launch.sh` is authoritative), with
`nonthinking` also emitting `--chat-template-kwargs {"enable_thinking":false}`. A flag is emitted
**only** if `build.flags.has(flag)`. `--jinja` is never emitted when `jinja_default_on`;
`--no-jinja` is the meaningful flag. `-fa` is emitted as `on|off|auto`. `--ctx-size` and `-ngl` are
**omitted** when `ctx: None` / `NglPlan::Auto`, so llama.cpp's own `--fit` can size. `--props`,
`--metrics` and `--slots` are emitted when supported. `-dev`, `-sm`, `-mg`, `--tensor-split` are all
emitted from `SplitPlan`. `LD_LIBRARY_PATH = dirname(server_path)` is always in the env (the
build-vulkan trailing-colon RUNPATH trap). No credential is ever in `args`; `--api-key-file` only.
`plan_container` emits exactly the 16 llama.cpp vars or the 16 vLLM vars listed in
`ARCHITECTURE.md` §3.7, forces `HOST=127.0.0.1` unless `expose_public`, puts `HF_TOKEN` in the env
map and **never** in `onstart`, resolves the image from `[docker]` by `image_type`, applies
`known_forks` (forcing `Builder` and pushing the "+12–18 min cold start" warning), and sets
`args_override` so the image's own `ENTRYPOINT` cannot start a second server.

**C-13 · upstream probing** — owns `core/src/upstream.rs`
```rust
pub struct UpstreamProbe { pub healthy: bool, pub loading: bool, pub status: Option<u16>,
    pub models: Vec<UpstreamModel>, pub slots_busy: Option<u32>, pub slots_total: Option<u32>,
    pub ctx: Option<u32>, pub build_info: Option<String>, pub model_path: Option<String>,
    pub ms: u32, pub error: Option<String> }
pub async fn probe(http: &reqwest::Client, base_url: &str, cred: Option<&Secret<String>>,
                   timeout: Duration) -> UpstreamProbe;
pub fn parse_timings(v: &serde_json::Value) -> Option<Timings>;
pub struct Timings { pub cache_n: u32, pub prompt_n: u32, pub prompt_ms: f32,
    pub predicted_n: u32, pub predicted_ms: f32, pub predicted_per_second: f32 }
pub fn parse_usage(v: &serde_json::Value) -> Option<UsageFields>;
pub struct UsageFields { pub prompt_tokens: u32, pub completion_tokens: u32,
                         pub cached_tokens: Option<u32> }
pub fn join_v1(base_url: &str, segment: &str) -> String;   // base never ends in /v1
```
*Acceptance*: `/health` 200 → healthy; `/health` 503 `{"status":"loading model"}` → `loading: true`
(the readiness gate must distinguish this from connection-refused). `/v1/models` deserialises
**both** `{"object":"list","data":[…]}` (llama.cpp) and a **bare array** (Together) — two
deserialisers, one function. `/props` populates `ctx`, `build_info`, `model_path`; a 404 is not an
error. `/slots` populates slot counts; a **501** is not an error. `parse_timings` must **not** share
a struct with `/slots?action=save`'s different `timings` object. Tested with `wiremock`.

**C-14 · usage + pricing** — owns `core/src/usage.rs`, `core/src/pricing.rs`
```rust
pub struct UsageWriter { /* O_APPEND file + optional legacy mirror */ }
impl UsageWriter {
    pub fn open(paths: &Paths, cfg: &CompatCfg) -> Result<UsageWriter>;
    pub fn append(&self, rec: &UsageRecord) -> Result<()>;      // ONE write() per row
    pub fn rotate_if_needed(&self, max_mb: u64) -> Result<()>;
}
pub fn read_all(paths: &Paths, cfg: &CompatCfg) -> Result<Vec<UsageRecord>>;  // new + legacy
pub fn aggregate(rows: &[UsageRecord], since: Option<i64>, by: GroupBy) -> UsageSummary;
pub enum GroupBy { Provider, Model, Backend, Alias, Day }
pub fn parse_lenient_timestamp(s: &str) -> Option<i64>;   // handles local-time-with-a-lying-Z

pub struct PriceTable { /* … */ }
impl PriceTable {
    pub fn set_provider_models(&mut self, id: &ProviderId, models: &[(String, PriceModel)]);
    pub fn set_instance_dph(&mut self, id: InstanceId, dph: f64);
    pub fn estimate(&self, provider: &str, model: &str, prompt: TokenCount, completion: TokenCount,
                    tps_hint: Option<f32>) -> CostEstimate;
}
```
*Acceptance*: the on-disk row is byte-identical in field names to the legacy schema, verified by
deserialising the **real** `~/.vastai-gguf/usage.log` (including the legacy `epoch` field and any
unknown key) with **zero failed rows**. New fields are additive. Timestamps written are RFC 3339
UTC. Aggregation windows are real (`24h`, `7d`, `all`). A `PerHour` price yields
`CostEstimate::Approximate` with the throughput assumption **in the string**, never a bare number.

**C-15 · catalog (recipes + profiles) with `toml_edit`** — owns `core/src/catalog.rs`
```rust
pub struct Catalog { pub recipes: Vec<Recipe>, pub profiles: Vec<SearchProfile> }
pub fn load(paths: &Paths) -> Result<Catalog>;
pub fn save(paths: &Paths, c: &Catalog) -> Result<()>;   // toml_edit round-trip + atomic
pub fn upsert_recipe(paths: &Paths, r: Recipe) -> Result<Recipe>;
pub fn remove_recipe(paths: &Paths, id: &RecipeId) -> Result<()>;
pub fn upsert_profile(paths: &Paths, p: SearchProfile) -> Result<SearchProfile>;
pub fn remove_profile(paths: &Paths, id: &ProfileId) -> Result<()>;
pub fn validate_recipe(r: &Recipe, rig: &RigSnapshot, models: &[LocalModel]) -> ValidationReport;
pub fn default_profiles() -> Vec<SearchProfile>;   // 3090×2-4, 3090×4 (perf), H100×1, H100×2
pub fn recipe_from_endpoint(rec: &EndpointRecord, label: &str) -> Recipe;
```
*Acceptance*: a hand-written comment in `catalog.toml` survives a GUI edit (`toml_edit`), proven by
a round-trip test. Ids are **generated**, never typed by the user, and are unique.
`validate_recipe` reports staleness (`model file gone`, `build removed`, `profile deleted`) as
`Warning`, not `Error`, with a `fix` string. `default_profiles()` uses the **exact** `gpu_name`
strings from `00c` (`"RTX 3090"`, `"H100 SXM"`, `"H100 NVL"`, `"H100 PCIE"`) and
`num_gpus_min/max`, never one profile per GPU count.

**C-16 · migration** — owns `core/src/migrate.rs`
```rust
pub fn plan(paths: &Paths, cfg: &Config) -> Result<MigrationPlan>;
pub fn apply(paths: &Paths, cfg: &Config, plan: &MigrationPlan) -> Result<MigrationReport>;
pub fn read_legacy_active_endpoint(path: &Path) -> Result<Option<LegacyActiveEndpoint>>;
pub struct LegacyActiveEndpoint { /* all FOUR shapes via serde aliases activated_at|switched_at */ }
pub fn read_legacy_instances(dir: &Path) -> Result<Vec<LegacyLocalInstance>>;
pub fn import_recipes_toml(path: &Path) -> Result<(Vec<Recipe>, Vec<SearchProfile>,
                                                   Vec<KnownFork>, DockerCfg, Vec<String>)>;
```
*Acceptance*: all four `.active_endpoint` shapes deserialise. `.last_instance` /
`.instance_history` / `.hf_pin` / `.pinned_provider` all parse, **with trailing newlines trimmed**.
`recipes.toml` import handles `max_price` as a quoted **string**, `enforce_eager` as the string
`"true"`/`"false"`, absent `provider` defaulting to `vast_gguf`, and `vram_gb` being **per GPU**.
The 54 `vast_gguf` recipes are reported as `MigrationAction::Skip` with the reason, per row. The 7
`llama_cpp_repo`/`ref` mappings become `KnownFork`s. `--dry-run` writes nothing at all — proven by
comparing a directory hash before and after.

**C-17 · check registry** — owns `core/src/checks.rs`
```rust
#[async_trait] pub trait Check: Send + Sync {
    fn id(&self) -> CheckId; fn label(&self) -> &str; fn needs(&self) -> CheckNeeds;
    async fn run(&self, ctx: &CheckCtx) -> CheckResult;
}
pub enum CheckNeeds { Local, Network, Daemon, Instance }
pub struct CheckCtx { pub paths: Paths, pub cfg: Arc<Config>, pub http: reqwest::Client,
    pub rig: Option<Arc<RigSnapshot>>, pub proxy_url: Option<String>,
    pub instance: Option<InstanceId>, pub ext: HashMap<String, Arc<dyn std::any::Any + Send + Sync>> }
pub struct Registry { checks: Vec<Arc<dyn Check>> }
impl Registry {
    pub fn new() -> Registry;  pub fn register(&mut self, c: Arc<dyn Check>);
    pub fn ids(&self) -> Vec<CheckId>;
    /// Concurrent, per-check timeout, results streamed through `tx` as each lands.
    pub async fn run(&self, ctx: &CheckCtx, only: Option<&str>,
                     tx: tokio::sync::mpsc::Sender<CheckResult>) -> Vec<CheckResult>;
}
pub fn local_checks() -> Vec<Arc<dyn Check>>;   // creds.*, ports.*, builds.*, devices.*,
                                                // models.*, state.writable, legacy.migration
```
*Acceptance*: checks run concurrently (a test with three 200 ms checks finishes in < 400 ms).
`only` filters. A check that panics yields `CheckStatus::Fail`, never poisons the run. `ext` is how
`providers::checks` injects clients without `core` depending on `providers`.

**Stage 2 gate**: `cargo test -p apexrouter-core`; and on the real machine
`apexrouter rig --json | jq '.builds|length'` ≥ 5 and
`apexrouter models ls --json | jq '.[0].name'` = `Carnice-9b-Q6_K`.

---

### Stage 3 — the router and the local supervisor (10 agents, parallel)

**R-01 · table + registry + compile** — owns `router/src/table.rs`, `router/src/registry.rs`
```rust
pub struct LiveBackend { /* as in ARCHITECTURE §4.1 */ }
impl LiveBackend {
    pub fn new(b: Backend, cfg: &RouterCfg) -> Arc<LiveBackend>;
    pub fn update_meta(&self, b: Backend);
    pub fn resize_semaphore(&self, permits: u32);
    pub fn set_models(&self, m: Vec<String>);
}
pub struct BackendRegistry { /* RwLock<HashMap<BackendId, Arc<LiveBackend>>> */ }
impl BackendRegistry {
    pub fn new() -> Self;
    pub fn upsert(&self, b: Backend, cfg: &RouterCfg) -> Arc<LiveBackend>;  // REUSES live state
    pub fn remove(&self, id: &BackendId) -> Option<Arc<LiveBackend>>;
    pub fn get(&self, id: &BackendId) -> Option<Arc<LiveBackend>>;
    pub fn all(&self) -> Vec<Arc<LiveBackend>>;
    pub fn snapshot(&self) -> Vec<Backend>;
}
pub struct RoutingTable { /* as in ARCHITECTURE §4.1 */ }
pub struct TableBuilder;
impl TableBuilder {
    pub fn compile(cfg: &Config, routes: &RouteFile, reg: &BackendRegistry)
        -> std::result::Result<RoutingTable, ValidationReport>;
}
```
*Acceptance*: **`upsert` on an existing id preserves the `Semaphore`, breaker state, EWMA and
in-flight count** — the regression test starts 3 in-flight requests, recompiles the table, and
asserts the permit count is unchanged. Compile rejects: dangling target, duplicate alias, alias
shadowing a live upstream id without `allow_shadow`, unsatisfiable `require_tags`, and
`Strategy::Cheapest` where no target has a price. A failed compile returns a `ValidationReport` and
leaves the caller free to keep the old table.

**R-02 · resolve + policy** — owns `router/src/resolve.rs`, `router/src/policy.rs`
```rust
pub struct Plan { pub candidates: SmallVecLike<Candidate>, pub reason: RouteReason,
                  pub alias: Option<Alias>, pub rewrite_model_to: Option<String>,
                  pub retry: RetryPolicy }
pub struct Candidate { pub backend: Arc<LiveBackend>, pub upstream_model: String }
pub enum RequestClass { Models, Chat, Completion, Embedding, Rerank, Opaque }
pub enum RouteError { NoRoute { known: Vec<String> }, NoHealthy { alias: Alias },
                      FilteredOut { alias: Alias, why: String } }
impl RoutingTable {
    /// SYNCHRONOUS. NO I/O. Six rules, in the order documented in ARCHITECTURE §4.2.
    pub fn resolve(&self, model: Option<&str>, class: RequestClass,
                   unknown: UnknownModelPolicy) -> Result<Plan, RouteError>;
}
pub enum UnknownModelPolicy { Reject, Fallback }
pub fn order_candidates(strategy: Strategy, cands: &mut Vec<Candidate>);
```
*Acceptance*: a table test proves each of the six rules in order, including `"x"` → default
(`LegacyModelName`) and an unknown name → `NoRoute` under `Reject`. `resolve` takes no `&mut`,
performs no I/O, and does not allocate on the alias hit path (bench-asserted `< 200 ns`).
`Cheapest` orders by `per_mtok`, with `Unknown` prices last. **`Plan::retry` carries the matched
route's own `[retry]` block** (`ARCHITECTURE.md` §4.2) — a test proves a non-default route's policy
reaches the plan, that the *matched* route's policy is used rather than the default route's, and
that rules 2/3/4, which name a backend and not a route, carry `RetryPolicy::default()`.

**R-03 · headers + body plan** — owns `router/src/relay/headers.rs`, `router/src/relay/body.rs`
```rust
pub fn outbound_headers(inbound: &HeaderMap, cred: Option<&Secret<String>>,
                        extra_allow: &[String]) -> HeaderMap;   // CONSTRUCTED, never cloned
pub fn response_headers(upstream: &HeaderMap) -> HeaderMap;     // drops hop-by-hop, keeps multi-valued
pub enum BodyPlan { Passthrough(Bytes), Rewritten(Bytes) }
pub fn plan_body(original: &Bytes, rewrite_model_to: Option<&str>) -> Result<BodyPlan>;
pub struct RequestPeek { pub model: Option<String>, pub stream: bool,
                         pub include_usage: bool, pub bytes: usize }
/// Top-level key scanner. Does NOT build a full serde_json::Value.
pub fn peek(body: &[u8]) -> RequestPeek;
pub fn normalize_path(path: &str) -> (String, bool);   // (normalized, collapsed_a_duplicate_v1)
```
*Acceptance*: a unit test asserts `authorization`, `proxy-authorization`, `cookie`, `host`,
`content-length`, `connection`, `transfer-encoding` and `te` are **never** present in the output of
`outbound_headers` unless the backend's own credential put an `authorization` there.
`plan_body(Passthrough)` returns the original `Bytes` with zero copies. `Rewritten` changes **only**
the `model` value — proven by a tool-calling fixture round-trip asserting every other key, including
float formatting inside `tools[]`, is byte-identical apart from `model`. `normalize_path("/v1/v1/chat/completions")`
→ `("/v1/chat/completions", true)`. `peek` on a 4 MiB body allocates < 4 KiB.

**R-04 · attempt state machine + breaker + limits** — owns `router/src/attempt.rs`,
`router/src/breaker.rs`, `router/src/limits.rs`
```rust
pub struct PreFlight<'a> { /* candidate, body plan, headers, deadline, cfg, retry */ }
pub struct Committed { pub response: reqwest::Response, pub guard: Option<InFlightGuard> }
pub enum Retryable { Connect(String), Timeout, Status { code: u16, retry_after: Option<Duration> } }
/// The retry loop consumes PreFlight values and can only exit by producing a Committed.
pub async fn attempt(p: PreFlight<'_>) -> std::result::Result<Committed, Retryable>;
pub struct Breaker { /* atomics: state, opened_at, failures, successes, volume */ }
impl Breaker { pub fn check(&self) -> BreakerDecision; pub fn record(&self, ok: bool);
               pub fn trip(&self, retry_after: Option<Duration>); }
pub enum BreakerDecision { Allow, AllowProbe, Deny { retry_at_unix: i64 } }
pub struct InFlightGuard { /* OwnedSemaphorePermit + byte permit + gauge + partial record */ }
impl InFlightGuard {
    pub async fn acquire(b: &Arc<LiveBackend>, bytes: usize, global: &Arc<Semaphore>,
                         queue_timeout: Duration) -> Result<InFlightGuard, LimitError>;
    pub fn mark_first_byte(&mut self);
    pub fn finish(self, rec: RequestRecord);
}
impl Drop for InFlightGuard { /* release permits, decrement gauge,
                                 emit RequestFinished{aborted:true} if finish() never ran */ }
pub struct TokenBucket { /* per-backend retry budget */ }
```
*Acceptance*: a test drops a `Committed` mid-stream and asserts the permit count returns to full and
exactly one `RequestFinished { aborted: true }` was broadcast — **the disconnect-leak regression
test**. The breaker requires `min_volume` (5) observations before opening; half-open admits exactly
one probe; `Retry-After` is honoured **when `PreFlight::retry.honor_retry_after` says to**, and is
not even read when it does not. There is no code path that calls `attempt` twice on the same
`PreFlight` (enforced by ownership). `Committed`'s fields are public because R-05 destructures it:
the relay takes the response and the guard, and there is exactly one relay.

**R-05 · SSE relay + usage tee** — owns `router/src/relay/stream.rs`
```rust
/// The relay hands the guard back, plus everything it learned, exactly once — from its Drop,
/// so a client disconnect settles the record instead of losing it. `StreamOutcome::aborted`
/// says whether the client or the upstream went first.
pub type FinishFn = Box<dyn FnOnce(Option<InFlightGuard>, StreamOutcome) + Send>;
pub fn sse_response(c: Committed, cfg: &RouterCfg, on_end: FinishFn) -> axum::response::Response;
pub struct StreamOutcome { /* bytes, newlines, usage, timings, aborted, ended_mid_stream,
                              idle_timeout, total_ms, streamed — all pub */ }
impl StreamOutcome {
    pub fn prompt_tokens(&self) -> Option<TokenCount>;   // reported or None; never estimated
    pub fn completion_tokens(&self) -> TokenCount;       // degrades to Estimated, never to zero
    pub fn cached_tokens(&self) -> Option<u32>;
    pub fn tok_per_s(&self) -> Option<f32>;              // READ from timings, never stopwatched
    pub fn error(&self) -> Option<&'static str>;
}
pub struct UsageTee { /* rolling tail buffer, bounded */ }
impl UsageTee { pub fn feed(&mut self, chunk: &[u8]); pub fn finish(self) -> Option<(UsageFields, Option<Timings>)>; }
```
**This is the crate's only SSE relay.** R-08 calls it and owns no framing code of its own; a second
copy of these rules is the bug this signature exists to prevent. `sse_response` builds the response
headers (hop-by-hop dropped, streaming-only added) and R-08 stamps the observability headers onto
what it returns.

*Acceptance*: bytes are relayed **verbatim** — a fixture replay of a real llama.cpp SSE capture and
a real Together SSE capture produces byte-identical output. `Content-Type: text/event-stream` is
forced **only** when upstream is 2xx and already says so; a `400` JSON body on `stream:true` comes
back as JSON. An idle gap longer than `idle_timeout_ms` aborts; there is **no total timeout**. When
the upstream ends mid-stream — **including a clean EOF with no `data: [DONE]` terminator, which is a
truncation** — exactly one synthetic
`data: {"error":{"message":"upstream ended mid-stream","type":"upstream_unavailable"}}` frame plus
`data: [DONE]` is emitted. The tee never delays a byte (test: measured chunk-arrival→chunk-emit
latency < 1 ms) and a malformed tail degrades to `TokenCount::Estimated`.

**R-06 · errors + models aggregation** — owns `router/src/errors.rs`, `router/src/models.rs`
```rust
pub fn openai_error(status: StatusCode, kind: &str, msg: &str) -> axum::response::Response;
pub fn map_status(e: &RouteError) -> (StatusCode, &'static str);
pub fn aggregate_models(t: &RoutingTable) -> serde_json::Value;   // ARCHITECTURE §6.1 shape
pub fn one_model(t: &RoutingTable, id: &str) -> Option<serde_json::Value>;
```
*Acceptance*: every error body matches `{"error":{"message","type","code","param"}}`. Status mapping
matches §4.5 exactly, including the 502-vs-503 distinction. `aggregate_models` puts all extras under
one `apexrouter` key and is served with no upstream hop.

**R-07 · telemetry + metrics** — owns `router/src/telemetry.rs`
```rust
pub struct Telemetry { ring: Mutex<VecDeque<RequestRecord>>, tx: broadcast::Sender<Event>, /* … */ }
impl Telemetry {
    pub fn record(&self, r: RequestRecord);
    pub fn recent(&self, limit: usize, alias: Option<&Alias>, backend: Option<&BackendId>)
        -> Vec<RequestRecord>;
    pub fn prometheus(&self, reg: &BackendRegistry, rig: Option<&RigSnapshot>) -> String;
    pub fn tick(&self) -> Option<UsageSummary>;   // coalesced to 1 Hz
}
```
*Acceptance*: `RequestStarted`/`RequestFinished` are only serialised when
`tx.receiver_count() > 0`. `UsageTick` fires at most 1 Hz. The Prometheus body parses with a
standard exposition-format validator and contains every metric named in §4.5.

**R-08 · the proxy handler (wiring)** — owns `router/src/lib.rs`, `router/src/handler.rs`
```rust
pub struct RouterInner { /* as in ARCHITECTURE §4.1 */ }
pub type Router = Arc<RouterInner>;
impl RouterInner {
    pub fn new(cfg: Arc<Config>, tx: broadcast::Sender<Event>, usage: UsageWriter) -> Router;
    pub fn store_table(&self, t: RoutingTable);
    pub fn table(&self) -> arc_swap::Guard<Arc<RoutingTable>>;
    pub fn registry(&self) -> &BackendRegistry;
}
/// The axum Router for the PROXY listener. Registered with `.fallback(any(proxy_handler))`
/// — NEVER a `/{*path}` route, so no merge overlap can exist.
pub fn proxy_router(r: Router) -> axum::Router;
pub async fn proxy_handler(State(r): State<Router>, req: Request) -> Response;
```
*Acceptance*: `proxy_router()` merged with any other router does **not** panic (explicit test).
The full pipeline of §4.3 is exercised against a `wiremock` upstream: routing, retry on 502,
no-retry-after-first-byte, 413 on an oversized body, 508 on a `Via` loop, `X-ApexRouter-Route` on
every response. The retry loop is bounded by **`Plan::retry`** (R-02), not by
`RetryPolicy::default()` — proven by a test where a per-route `attempts = 1` stops one candidate
short of where the default policy succeeds, and one where a route carrying the default still fails
over. Streaming is delegated to **R-05's `sse_response`**; this file holds no frame rules, no idle
timeout and no tee of its own, and a raw-TCP fixture (chunk sizes 1/3/7/33/64/4096, a timing proof
that the first chunk is not buffered, and a usage tee round trip) runs end-to-end through
`proxy_router` against it. The handler also **owns the `(ingress, upstream)` matrix dispatch** of
`ARCHITECTURE.md` §3.4: it records `RequestRecord.ingress`, relays for `OpenAi→OpenAi` and
`Anthropic→Anthropic`, returns `501` with an **OpenAI-shaped** body for `OpenAi→Anthropic`, and for
`Anthropic→OpenAi` calls into `router/src/anthropic/` — stubbed by Stage 0, filled in by **R-10**
(Stage 5) against the signatures published there. In Stage 3 the stub returns `501`; the dispatch,
the `X-ApexRouter-Protocol` header and the `ingress` field are R-08's and are tested here.

**R-09 · legacy compat handlers** — owns `router/src/compat.rs`
```rust
pub async fn legacy_health(State(r): State<Router>) -> Json<Value>;
pub async fn legacy_providers(State(r): State<Router>) -> Json<Value>;
pub async fn legacy_switch(State(r): State<Router>, headers: HeaderMap, body: Bytes) -> Response;
pub fn mirror_active_endpoint(cfg: &CompatCfg, route: &ModelRoute, reg: &BackendRegistry)
    -> Result<()>;
```
*Acceptance*: `/health` returns a superset of `{"ok","provider","uptime"}` plus `product`/`version`.
`/providers` returns the **exact** legacy key structure (`active`, `target`, `providers{}`,
`local_instances[]`) plus additive `endpoints[]`/`routes[]`, and probes run **concurrently** with a
3 s cap. `/switch` accepts every documented legacy body and now **persists** the `together`
`api_key` (as a `CredentialRef`) and copies a local instance's key — the two documented silent
no-ops. A malformed instance JSON returns a JSON `400`, not an HTML 500. `base_url` is validated
against `allow_switch_hosts`. Golden-file tests compare against captures of the Python responses.

**P-01 · the local supervisor** — owns `providers/src/local/mod.rs`,
`providers/src/local/supervisor.rs`, `providers/src/local/adopt.rs`
```rust
#[async_trait] pub trait Provisioner: Send + Sync {
    fn kind(&self) -> BackendKind;
    async fn plan(&self, draft: &EndpointSpec) -> Result<LaunchPlan>;
    async fn up(&self, plan: LaunchPlan, approval: Option<SpendApproval>) -> Result<Backend>;
    async fn down(&self, id: &BackendId, mode: DownMode) -> Result<()>;
    async fn logs(&self, id: &BackendId, tail: usize) -> Result<Vec<String>>;
    async fn reconcile(&self) -> Result<Vec<Backend>>;
}
pub enum DownMode { Drain, Now, Forget }
pub struct LaunchPlan { pub spec: EndpointSpec, pub argv: ArgvPreview, pub fit: Option<FitPlan>,
    pub cost: CostEstimate, pub warnings: Vec<String>, pub port: u16 }
pub struct LocalProvisioner { /* Paths, Config, Store, tx, rig cache */ }
impl LocalProvisioner { pub fn new(...) -> Self; }
pub enum LaunchError { PortInUse { port: u16, held_by: Option<BackendId> },
    InsufficientVram { need_mb: u64, free_mb: u64, held_by: Vec<BackendId> },
    BinaryMissing(String), ModelMissing(String), HealthTimeout { log_tail: Vec<String> },
    ExitedEarly { code: Option<i32>, log_tail: Vec<String> } }
```
*Acceptance* (this is the highest-value acceptance in the plan): **start and stop
`Carnice-9b-Q6_K.gguf` on `build-vulkan` ten times in a loop with no orphaned process, no zombie,
no leaked fd and no stale state file.** Plus: `LD_LIBRARY_PATH` is set (a negative test runs the
binary with the wrong cwd and asserts we still start); the port bind-probe holds its reservation
under a per-endpoint lock until the health gate passes, so two concurrent launches cannot both take
8100; the health gate has a real wall-clock deadline that **resets on observed progress** and, on
expiry, **kills the child, removes the record, marks `Failed` with the log tail, and clears the
route** — asserted by a test that launches against a deliberately-missing model; `spawn_detached`
gives the child pid 1 as its parent; `adopt()` re-adopts across a simulated daemon restart and
returns `Foreign` (never signalling) when the identity mismatches; `InsufficientVram` fires when a
second launch would exceed the budget unless `force`.

**Stage 3 gate**: `cargo test -p apexrouter-router` (entirely against `wiremock` — no network, no
llama.cpp, no credentials, no money) and `cargo test -p apexrouter-providers --lib`.

---

### Stage 4 — the server and the core CLI (6 agents) → **MK1-CORE ACCEPTANCE**

**S-01 · server skeleton, state, both listeners, shutdown** — owns `server/src/lib.rs`,
`server/src/state.rs`, `server/src/shutdown.rs`
```rust
pub struct AppState { pub paths: Paths, pub cfg: ArcSwap<Config>, pub store: Store,
    pub router: apexrouter_router::Router, pub tx: broadcast::Sender<Event>,
    pub supervisor: Arc<LocalProvisioner>, pub jobs: JobRegistry, pub checks: Arc<Registry>,
    pub started_at: Instant, pub lock: Arc<Mutex<DaemonLock>>, /* provider slots filled in Stage 5 */ }
pub async fn serve(paths: Paths, cfg: Config, lock: DaemonLock) -> anyhow::Result<()>;
pub fn api_router(state: Arc<AppState>) -> axum::Router;   // pub use: ApexOS can mount this
pub async fn reconcile_on_start(state: &Arc<AppState>) -> anyhow::Result<()>;
```
*Acceptance*: both listeners bind; `EADDRINUSE` on either produces the "held by X" message from
§1.2 and exits 1; reconciliation runs **before** the table is armed; `SIGHUP` reloads config without
exiting; shutdown drains in-flight to the deadline and **never** signals a `llama-server` child.

**S-02 · auth + the mutation gate** — owns `server/src/auth.rs`
```rust
pub async fn require_auth(State(s): State<Arc<AppState>>, ci: ConnectInfo<SocketAddr>,
                          req: Request, next: Next) -> Response;
pub fn extract_presented_token(h: &HeaderMap, uri: &Uri) -> Option<String>;
pub fn required_scope(path: &str, method: &Method) -> Scope;
pub enum Scope { Read, Write, Admin }
/// CSRF + DNS-rebinding defence. Applied to EVERY mutation on BOTH listeners.
pub fn require_mutation_origin(h: &HeaderMap, bind: &SocketAddr, cfg: &ServerCfg)
    -> Result<(), Response>;
```
*Acceptance*: absent `ConnectInfo` **fails closed**. A non-loopback bind without a configured token
refuses to start. A request with `Host: evil.com` is rejected (rebinding). A request with
`Origin: http://evil.com` is rejected. A request with **no** `Origin` and no `Sec-Fetch-Site`
(curl, the CLI, Slint) passes. `?token=` works but the `TraceLayer` span records method+path only —
asserted by a test capturing the span fields.

**S-03 · control-plane API modules (part A)** — owns `server/src/api/mod.rs`,
`server/src/api/snapshot.rs`, `server/src/api/backends.rs`, `server/src/api/routes.rs`,
`server/src/api/endpoints.rs`
*Implements*: `/v1/snapshot`, `/v1/reload`, `/v1/shutdown`, the `/v1/backends*` set, the
`/v1/routes*` set including `validate`, `test`, `swap`, `default`, and the `/v1/endpoints*` set
including `argv` and `adopt`, exactly as tabulated in `ARCHITECTURE.md` §6.2.
*Acceptance*: every route returns the documented protocol type; `PUT /v1/routes` is atomic and a
compile failure leaves the previous table serving (asserted while streaming a request);
`POST /v1/routes/{alias}/swap` performs the mode selection of §4.7 and returns a `SwapReport`.

**S-04 · control-plane API modules (part B)** — owns `server/src/api/rig.rs`,
`server/src/api/fit.rs`, `server/src/api/catalog.rs`, `server/src/api/usage.rs`,
`server/src/api/requests.rs`, `server/src/api/jobs.rs`, `server/src/jobs.rs`
*Implements*: `/v1/rig*`, `/v1/models/local`, `/v1/fit` (GET and POST), the `/v1/recipes*` and
`/v1/profiles*` CRUD sets, `/v1/usage`, `/v1/requests*`, `/v1/jobs*`, and the `JobRegistry` behind
`?no_wait`.
*Acceptance*: `JobRegistry` flips a job to `Failed` on **every** error path including a `JoinError`
from a panicking task — asserted by a test that panics inside a job. Nothing sits `Pending` forever.

**S-05 · WebSocket + embedded assets + health prober + config watcher** — owns
`server/src/ws.rs`, `server/src/assets.rs`, `server/src/prober.rs`, `server/src/watcher.rs`
```rust
pub async fn ws_handler(ws: WebSocketUpgrade, State(s): State<Arc<AppState>>) -> Response;
#[derive(rust_embed::Embed)] #[folder = "../../ui-web"] pub struct Assets;
pub fn static_router() -> axum::Router;       // "/" + "/{*path}", refuses to shadow /v1 or /health
pub fn mime_for(path: &str) -> &'static str;  // hand-written 14-arm match, no mime_guess
pub async fn health_prober(state: Arc<AppState>);
pub async fn config_watcher(state: Arc<AppState>);
```
*Acceptance*: **subscribe to the broadcast BEFORE sending the snapshot**; a full snapshot is
re-sent on `RecvError::Lagged`; `tokio::select!` also drains `socket.recv()`. The watcher watches
`$CONFIG` and `$STATE/routes.json` only — **never** a directory containing endpoint logs (a test
writes 1000 log lines and asserts zero reloads). Debounce 250 ms + a 10 s poll fallback. The prober
sizes each backend's semaphore from `/props.total_slots`, falling back to `/slots`, falling back to
config, and requires N consecutive failures before `Degraded`.

**S-06 · CLI skeleton + core verbs + daemon resolution** — owns `cli/src/main.rs`,
`cli/src/cli.rs`, `cli/src/daemon.rs`, `cli/src/render.rs`, `cli/src/cmd/mod.rs`,
`cli/src/cmd/{status,serve,config,rig,models,fit,endpoint,route,switch,url,version,completions}.rs`
```rust
pub enum Need { Pure, ReadState, Mutate }
pub enum Serving { Daemon(NodeClient), Offline(Store), None(anyhow::Error) }
pub async fn resolve_serving(need: Need, cfg: &Config, paths: &Paths, autostart: bool)
    -> anyhow::Result<Serving>;
pub fn print_json<T: Serialize>(served_by: ServedBy, as_of: i64, stale: bool, v: &T)
    -> anyhow::Result<()>;
pub fn print_table(headers: &[&str], rows: Vec<Vec<String>>);   // space-padded, "-" for missing
pub fn print_error_json(kind: &str, msg: &str) -> anyhow::Result<()>;
```
*Acceptance*: `apexrouter status` works on a machine where nothing is running and prints
`served_by: "offline"`. `apexrouter config show --json` prints **only** the JSON envelope on stdout.
`tracing` goes to stderr in every subcommand. `Mutate` autostarts and two racing autostarts
converge on one daemon. `--no-autostart` errors cleanly.

### 4.1 **MK1-CORE ACCEPTANCE** (the Stage 4 gate)

Run on Andre's laptop, in this order, with nothing else running:

```bash
cargo build --release
export PATH="$PWD/target/release:$PATH"

apexrouter doctor                       # every local check Pass or Warn, none Fail
apexrouter rig                          # >=5 builds; Vulkan0 with real VRAM; llvmpipe excluded
apexrouter models ls                    # Carnice-9b-Q6_K, ~6.9 GB, one row (shards grouped)
apexrouter fit Carnice-9b-Q6_K --ctx 32768
                                        # a verdict + a `why` list, not a crash

apexrouter up Carnice-9b-Q6_K --alias auto
                                        # spawns build-vulkan/bin/llama-server, health-gates,
                                        # binds `auto`, prints http://127.0.0.1:8888/v1

curl -s http://127.0.0.1:8888/health | jq       # ok:true, product:"apexrouter"
curl -s http://127.0.0.1:8888/v1/models | jq '.data[].id'      # "auto" + "<id>/Carnice-9b-Q6_K"
curl -s http://localhost:8888/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"auto","messages":[{"role":"user","content":"hi"}],"max_tokens":20}' -D- | head -20
                                        # 200; X-ApexRouter-Route: auto|alias ; X-Usage present
curl -s http://localhost:8888/v1/chat/completions \
  -d '{"model":"x","messages":[{"role":"user","content":"hi"}],"stream":true}' --no-buffer
                                        # streams; model "x" falls through to the default alias

bash <path-to-LocalRouter>/smoke.sh http://127.0.0.1:8888     # all four sections pass
bash <path-to-LocalRouter>/smoke.sh http://127.0.0.1:8888/v1  # ALSO passes (the /v1 collapse)

apexrouter usage --since 24h            # the requests above appear with real tok/s
apexrouter endpoint ls                  # one Ready endpoint, correct port, correct model

systemctl --user restart apexrouterd  ||  (apexrouter serve --stop && apexrouter serve --detach)
apexrouter endpoint ls                  # STILL Ready — the child survived and was re-adopted
curl -s http://127.0.0.1:8888/v1/chat/completions -d '{"model":"auto",…}'   # still works

apexrouter endpoint stop <id>           # clean: no orphan, no zombie, no stale record
pgrep -a llama-server                   # empty
```

Everything above must pass before Stage 5 begins. It exercises the whole spine — discovery, fit,
supervision, adoption, routing, streaming, telemetry, drop-in compatibility — with no network, no
credentials and no money.

---

### Stage 5 — providers, GUIs, MCP, remaining CLI (14 agents, parallel)

**P-02 · vast REST client** — owns `providers/src/vast/api.rs`, `providers/src/vast/query.rs`
```rust
pub struct VastApiHttp { http: reqwest::Client, base: String, cred: Secret<String> }
#[async_trait] pub trait VastApi: Send + Sync {
    async fn account(&self) -> Result<VastAccount>;
    async fn search(&self, q: &OfferQuery) -> Result<OfferSearchResult>;
    async fn create(&self, offer_id: u64, launch: &ContainerLaunch, label: &str)
        -> Result<InstanceId>;
    async fn instances(&self) -> Result<Vec<VastInstance>>;
    async fn instance(&self, id: InstanceId) -> Result<Option<VastInstance>>;
    async fn destroy(&self, id: InstanceId) -> Result<()>;
    async fn logs(&self, id: InstanceId, tail: u32) -> Result<Vec<String>>;
    async fn exec(&self, id: InstanceId, cmd: &str) -> Result<String>;
}
pub struct FixtureVast { /* replays recorded JSON — lets the money path be tested for free */ }
pub fn build_query(q: &OfferQuery) -> serde_json::Value;   // the {"q": …} body, verified in 00c
```
*Acceptance*: `PUT /api/v0/search/asks/` with the exact body shape from `00c` §"Offer search".
The create response's instance id is read from **`new_contract`**, not `id`. Offers keep unknown
fields via `flatten`. Logs use the **two-phase `result_url`** poll with **no Bearer on the result
fetch**, treating a first-fetch 403/404 as normal and backing off to ~30 s. 429 handling is
exponential backoff with jitter capped at 30 s (Vast publishes no headers). `GET /users/current/`
is live-tested against the real key (it is free) and the parsed struct has no `api_key` field.
Everything else is tested against `FixtureVast`.

**P-03 · offers, profiles, geo** — owns `providers/src/vast/offers.rs`
```rust
pub fn profile_to_query(p: &SearchProfile, overrides: &QueryOverrides) -> OfferQuery;
pub async fn search_unified(api: &dyn VastApi, p: &SearchProfile, o: &QueryOverrides)
    -> Result<OfferSearchResult>;
pub async fn gpu_name_vocabulary(api: &dyn VastApi) -> Result<Vec<String>>;
```
*Acceptance*: **one** search path — "auto cheapest" and the browser use the same query, so the
documented "rents an offer you never saw" bug is impossible. Any relaxation appends a string to
`relaxations` (e.g. `"widened: geo dropped, reliability 0.99 -> 0.97"`) that every surface renders.
Geo is a client-side match on the **tail** of `geolocation`. `gpu_name_vocabulary` comes from a live
broad search, never a constant.

**P-04 · rent + boot watchdog + stall detection/recovery** — owns `providers/src/vast/rent.rs`,
`providers/src/vast/boot.rs`, `providers/src/vast/stall.rs`
```rust
pub struct VastProvisioner { /* impl Provisioner */ }
pub async fn rent(api: &dyn VastApi, ledger: &Ledger, req: &RentRequest, approval: SpendApproval,
                  tx: &broadcast::Sender<Event>) -> Result<InstanceId>;
pub async fn watch_boot(api: &dyn VastApi, id: InstanceId, max_secs: u64,
                        tx: &broadcast::Sender<Event>) -> Result<BootPhase>;
pub async fn sample_download(ssh: &SshOpts, host: &str, port: u16) -> Result<DownloadHealth>;
pub async fn restart_download(ssh: &SshOpts, host: &str, port: u16) -> Result<()>;
```
*Acceptance*: the reservation row is appended **before** the create call (test: a `FixtureVast` that
panics inside `create` still leaves a `Reserved` row). `watch_boot` polls no faster than
`poll_min_ms`, treats `exited|offline|unknown` as terminal, and auto-destroys at `max_boot_secs`.
`sample_download` implements the 4 s `/proc/net/dev` eth0 RX delta with the `<1000 bytes` / `<50
Mbps` thresholds. `restart_download` recovers env from `/proc/<pid>/environ`, **forces
`HOST=127.0.0.1`**, and re-execs with `>>` (append), never `>`.

**P-05 · ssh tunnel supervisor** — owns `providers/src/ssh.rs`
```rust
pub struct TunnelSupervisor { /* owns the Child; persists TunnelStatus */ }
impl TunnelSupervisor {
    pub async fn up(&self, spec: TunnelSpec) -> Result<TunnelStatus>;
    pub async fn down(&self, id: InstanceId) -> Result<()>;   // kill child + `ssh -O exit` + unlink
    pub async fn adopt_all(&self) -> Result<Vec<TunnelStatus>>;
    pub async fn supervise(self: Arc<Self>);   // bounded-retry reconnect loop
}
```
*Acceptance*: the exact flag set of `ARCHITECTURE.md` §4.9. The `Child` pid is owned — `pgrep`
appears nowhere. Teardown runs `ssh -O exit` **and** unlinks the ControlPath (test asserts the
socket file is gone). A killed tunnel is reconnected with exponential backoff up to
`max_restarts_per_hour`, then raises a `Serious` alert. Records persist so a daemon restart
re-adopts rather than colliding on the local port.

**P-06 · together provider** — owns `providers/src/together.rs`
*Acceptance*: `GET /v1/models` deserialises the **bare array** (not the `{"data":[]}` envelope) and
reads `pricing` off each model object; the pricing **unit assumption is recorded in the
`CostEstimate::Approximate.assumption` string**, never silently applied. `finish_reason` is always a
`String` (Together emits `eos`). A 429 reads `x-ratelimit-reset`; `x-ratelimit-remaining` is not
relied upon. The base URL comes from config/legacy and `api.together.xyz` is never rewritten.

**P-07 · huggingface** — owns `providers/src/hf.rs`
```rust
pub async fn search(&self, q: &str, limit: u32) -> Result<Vec<HfModel>>;
pub async fn files(&self, repo: &str) -> Result<Vec<HfFileGroup>>;   // paths-info, authoritative
pub async fn download(&self, repo: &str, files: &[String], dest: &Path,
                      tx: mpsc::Sender<DownloadProgress>) -> Result<Vec<PathBuf>>;
```
*Acceptance*: search uses `GET /api/models?filter=gguf&search=`, following an RFC 5988
`Link: rel=next` when present. Sizes come from `POST /api/models/{ns}/{repo}/paths-info/{rev}` (the
authoritative call), not from `siblings` without sizes. Gated repos are classified on
(status, header-if-present, body) with an **anonymous retry** to distinguish a bad token, and always
surface the request-access URL — never "not found". The quant regex is
`(UD-Q\d+[^.\s_-]*|Q\d+_K_[A-Z]+|Q\d+_\d+)`. Shards are grouped with a summed size.
`download` resumes, streams progress, writes to `~/models/<repo-basename>/`, and verifies size on
completion. **This closes the discovery→launch dead-end: an HF row can become a local endpoint
without leaving the app.**

**P-08 · provider checks + native smoke + compare** — owns `providers/src/checks.rs`,
`providers/src/smoke.rs`, `providers/src/compare.rs`
*Acceptance*: the four smoke probes reimplement `smoke.sh` natively — models list, an 80-token
warm-up, a `get_weather` tool-calling probe with `tool_choice: auto`, and a 200-token throughput run
— reporting TTFT and tok/s **read from the `timings` object**, not a stopwatch, and using **the
resolved route's model id** rather than the hardcoded `"model":"x"` that 400s on managed providers.
`compare` runs N aliases **in parallel** (LocalRouter ran them serially) and reports real
`prompt_tokens` from the response (LocalRouter fabricated `word_count*1.3` while discarding the real
number). The deep-diagnostic checks implement the four SSH probes plus the RX sample.

**S-07 · control-plane API modules (part C: vast, hf, providers, checks)** — owns
`server/src/api/vast.rs`, `server/src/api/hf.rs`, `server/src/api/providers.rs`,
`server/src/api/checks.rs`, `server/src/api/compare.rs`
*Acceptance*: every remaining route in §6.2. `POST /v1/vast/instances` returns **409** without
`{confirm:true, max_usd_per_hour}`, and the 409 body carries the cost preview and current credit.
`PUT /v1/providers/{id}` with `api_key` writes `credentials.toml` at `0600` and **never**
`config.toml`. `/v1/smoke` and `/v1/diagnose` stream SSE, one event per probe/check.

**U-01 · web UI** — owns `ui-web/index.html`, `ui-web/app.js`, `ui-web/style.css`
*Acceptance*: three files, **no npm, no CDN, no framework, no build step**. Every panel in
`ARCHITECTURE.md` §10 renders. WS first with a REST first-paint fallback and 1 s→×2→15 s reconnect
backoff. `textContent` everywhere, `innerHTML` nowhere. `[hidden]{display:none}` guards on every
element that toggles `hidden` and declares its own `display`. Dark-first CSS variables with a
`prefers-color-scheme: light` override; status colours reserved for health; badges pair icon+label.
**Render-tested in a browser, not with curl** — a screenshot of each panel is attached to the PR.
The Router bar's copy button and the alias dropdown work. The Launch drawer's fit bar updates live
as sliders move. The Catalog panel can create, edit and delete a recipe **and** a search profile.

**U-02 · Slint app** — owns `crates/apexrouter-slint/src/main.rs`,
`crates/apexrouter-slint/src/api.rs`, `crates/apexrouter-slint/src/ui/**`
*Acceptance*: `fn main() -> anyhow::Result<()>` builds a multi-thread tokio runtime manually, keeps
it alive, and ends with `ui.run()?` — **`#[tokio::main]` appears nowhere** (CI greps for it in this
crate). Every UI touch crosses back via `upgrade_in_event_loop`. All eight screens of
`ARCHITECTURE.md` §11 exist and are **write-capable, including Rent, Destroy, Catalog and
Providers**. `export global Palette`; a grep asserts no colour literal outside `palette.slint`.
It links `apexrouter-protocol` and `apexrouter-client` only — a `cargo tree` assertion in CI proves
it does not link `apexrouter-core`.

**M-01 · MCP server** — owns `cli/src/mcp/mod.rs`, `cli/src/mcp/tools.rs`,
`cli/src/mcp/backend.rs`, `cli/src/cmd/mcp.rs`
```rust
pub async fn run_stdio(backend: Arc<dyn McpBackend>) -> anyhow::Result<()>;
pub async fn dispatch(b: &dyn McpBackend, method: &str, params: Value)
    -> std::result::Result<Value, RpcError>;   // transport-agnostic
#[async_trait] pub trait McpBackend: Send + Sync { /* one method per tool */ }
pub struct LocalBackend;  pub struct ProxyBackend;
```
*Acceptance*: every tool in `ARCHITECTURE.md` §8, prefixed `apexrouter_`, with long operational
descriptions. `initialize` **echoes the client's requested `protocolVersion`**. Tool failures are
results with `isError: true`; JSON-RPC error codes only for protocol breakage. **One compact JSON
message per line** (`to_string`, never pretty), **nothing but MCP on stdout**, exit on stdin EOF.
`server/discover` answers with `supportedVersions`; `_meta` is accepted and ignored;
`resultType: "complete"` is emitted. `LocalBackend` answers read-only tools with the daemon down and
returns a helpful `isError` for mutations. `apexrouter_vast_rent` without `confirm` returns
`isError` **carrying the cost preview and credit**. Verified live by adding
`{"command":"…/target/release/apexrouter","args":["mcp"]}` to `~/Projects/.mcp.json` and calling
`apexrouter_status` from Claude Code.

**S-08 · CLI remainder** — owns `cli/src/cmd/{vast,tunnel,hf,provider,recipe,profile,usage,smoke,
doctor,compare,backend,swap,up,approvals,token,open,env,migrate}.rs`
*Acceptance*: every verb in `ARCHITECTURE.md` §7 with `--json` printing only the protocol type.
`apexrouter up` resolves its positional in the documented order and errors with candidates on an
ambiguous prefix. Money verbs require `--yes` and print `$/hr`, estimated total and **current
credit** before acting. `apexrouter completions bash|zsh|fish` emits working completions.

**R-10 · Anthropic ingress translation** — owns `router/src/anthropic/mod.rs`,
`router/src/anthropic/translate.rs`, `router/src/anthropic/sse.rs`

The bonus last mk1 feature: `ANTHROPIC_BASE_URL=http://127.0.0.1:8888` makes the Claude Code harness
drive a local or rented model. Read `ARCHITECTURE.md` §3.4 (the `(ingress, upstream)` matrix) and
§6.1 (the routes) first — **this unit implements exactly one matrix cell, `Anthropic → OpenAi`.**
The `Anthropic → Anthropic` cell is the existing byte relay and needs no code here; `OpenAi →
Anthropic` is a `501` and stays one.

**You do not own `router/src/handler.rs`.** R-08 wired the matrix dispatch in Stage 3 against the
signatures below and Stage 0 stubbed `router/src/anthropic/`. If a signature here is genuinely
wrong, stop and report it — do not edit R-08's file.

```rust
// mod.rs — the surface R-08's handler calls. Nothing else in the crate knows this module exists.
pub fn is_anthropic_ingress(path: &str, headers: &HeaderMap) -> bool;
/// `/v1/messages` -> `/v1/chat/completions`. The ONLY path rewrite this unit performs.
pub fn upstream_path(ingress_path: &str) -> &'static str;
/// Anthropic-shaped error body, for failures that occur before/instead of an upstream hop.
/// Shape: {"type":"error","error":{"type":…,"message":…}}  — NOT the OpenAI shape.
pub fn anthropic_error(status: StatusCode, kind: &str, msg: &str) -> axum::response::Response;

// translate.rs — pure, synchronous, no I/O, unit-tested against fixtures.
pub struct AnthropicCfg { pub tools: bool }          // from [router] anthropic_tools
pub enum TranslateError {
    MissingMaxTokens,                                 // required on the Anthropic side
    ToolsDisabled,                                    // `tools` present while cfg.tools == false
    UnsupportedBlock { kind: String },                // e.g. "thinking"
    Malformed { at: String, why: String },
}
/// Anthropic MessagesRequest -> OpenAI ChatCompletionRequest. `model` is left EXACTLY as the
/// client sent it: resolve() owns model naming, this unit never invents an alias.
pub fn request_to_openai(body: &[u8], cfg: &AnthropicCfg) -> Result<Vec<u8>, TranslateError>;
/// Buffered OpenAI ChatCompletion -> Anthropic Message. `id` is passed through prefixed `msg_`.
pub fn response_to_anthropic(body: &[u8]) -> Result<Vec<u8>, TranslateError>;
pub fn map_stop_reason_to_anthropic(finish_reason: &str) -> &'static str;
pub fn map_stop_reason_to_openai(stop_reason: &str) -> &'static str;

// sse.rs — the state machine. This is the unit's main risk.
pub struct SseTranslator { /* block index, open/close state, accumulated usage, message id */ }
impl SseTranslator {
    pub fn new(model: String) -> Self;
    /// Feed one OpenAI SSE frame; get back zero or more Anthropic frames, already framed
    /// `event: <name>\ndata: <json>\n\n`. `data: [DONE]` yields the closing frames.
    pub fn feed(&mut self, frame: &[u8]) -> Vec<Bytes>;
    /// Upstream ended without [DONE]: close every open block honestly, then message_stop.
    pub fn finish(self) -> Vec<Bytes>;
}
```

**The translation contract, precisely** (each line is a fixture test):

| Anthropic | OpenAI | Rule |
|---|---|---|
| top-level `system` (string or block array) | a `{"role":"system"}` message | **hoist/lower.** Prepended as the first message; a block array is joined on `\n\n`. Absent ⇒ no system message is invented |
| `max_tokens` — **REQUIRED** | `max_tokens` — optional | Missing ⇒ `TranslateError::MissingMaxTokens` ⇒ `400` Anthropic-shaped `invalid_request_error`. Never defaulted silently |
| `content`: typed block array (`text`, `image`, `tool_use`, `tool_result`) | `content`: a plain string, or the OpenAI parts array | A single `text` block lowers to a plain string (the common case, and what keeps llama.cpp happy); multiple blocks lower to the parts array |
| `tools[].input_schema` | `tools[].function.parameters` | Rename only; the JSON Schema itself is copied byte-identically |
| `tool_use` block | an assistant message's `tool_calls[]` | `id`→`id`, `name`→`function.name`, `input` (object) → `function.arguments` (**a JSON string**) |
| `tool_result` block in a `user` message | a `{"role":"tool","tool_call_id":…}` message | One `tool_result` becomes one `tool` message; they are hoisted out of the user turn in order |
| `stop_reason: end_turn` | `finish_reason: stop` | both directions |
| `stop_reason: max_tokens` | `finish_reason: length` | both directions |
| `stop_reason: tool_use` | `finish_reason: tool_calls` | both directions |
| `usage.input_tokens` / `output_tokens` | `usage.prompt_tokens` / `completion_tokens` | rename only; never recomputed, never estimated |
| `thinking` block | — | no equivalent. Rejected as `UnsupportedBlock` (§12). llama.cpp b9199 has `--reasoning-format` and can emit `reasoning_content`, which is the closest thing that exists — mk1 records that and does **not** map it |

**Required inbound headers.** `anthropic-version: 2023-06-01` must be present (a missing or
unrecognised value is a `400` Anthropic-shaped error naming the header). `x-api-key` is the
Anthropic-side auth header and is accepted wherever a bearer would be; both it and
`anthropic-version` are consumed here and **never** appear on the outbound request — R-03's
constructed-allowlist rule already guarantees that, and a test asserts it.

**Streaming is the hard part.** Anthropic emits *named* SSE events —
`message_start`, `content_block_start`, `content_block_delta`, `content_block_stop`, `message_delta`,
`message_stop` — carrying explicit content-block **indices** and a final `usage` on `message_delta`.
OpenAI emits one delta shape repeatedly and terminates with `data: [DONE]`. Rebuilding that state
machine correctly — opening a block on the first delta of a kind, keeping the index monotonic,
closing every opened block exactly once, emitting `message_delta` with `stop_reason` **and** the
final usage before `message_stop` — is where this unit will break if it breaks.

**Stage the work inside the unit, in this order:**
1. **Non-streaming text.** `request_to_openai` + `response_to_anthropic` round-trip.
2. **Streaming text.** `SseTranslator` for `text` blocks only.
3. **Tool use, behind `[router] anthropic_tools`.** With the flag **off**, a body carrying `tools`
   returns `TranslateError::ToolsDisabled` as a `400` Anthropic-shaped error naming the config key —
   **a clear error, never a silent wrong answer with the tools quietly dropped.**

*Acceptance*:
- A `wiremock` OpenAI upstream + a real `/v1/messages` request body captured from Claude Code:
  buffered round-trip produces a valid Anthropic `Message` with `role:"assistant"`,
  `content:[{"type":"text",…}]`, a mapped `stop_reason`, and `usage` in Anthropic field names.
- **The SSE state-machine test is the gate.** Replaying a recorded llama.cpp OpenAI SSE capture
  through `SseTranslator` produces a frame sequence that: starts with exactly one `message_start`;
  opens and closes each content block exactly once with indices `0..n` and no gaps; never emits a
  `content_block_delta` for a closed or unopened index; ends with `message_delta` carrying both
  `stop_reason` and the final `usage`, then exactly one `message_stop`. A property test feeding
  arbitrary chunk splits of the same capture (including splits **inside** a `data:` line) yields an
  identical frame sequence — chunk boundaries must not be observable.
- Upstream dies mid-stream ⇒ every open block is closed, then `message_stop`. Never a truncated
  block, never a dangling index.
- `max_tokens` absent ⇒ `400`, and the body is **Anthropic-shaped**
  (`{"type":"error","error":{"type":"invalid_request_error",…}}`), because the client is an
  Anthropic SDK and will parse it as one. Symmetrically, the `OpenAi → Anthropic` `501` carries an
  **OpenAI-shaped** body.
- `tools` present with `anthropic_tools = false` ⇒ `400` naming the config key. Zero upstream hops.
- With `anthropic_tools = true`, a `get_weather` fixture survives the full loop:
  `tool_use` → `tool_calls` → upstream → `tool_result` → `role:"tool"` → final answer.
- A `thinking` block in a request ⇒ `UnsupportedBlock`, not a panic and not a silent drop.
- Ingress detection never fires on an OpenAI request: `is_anthropic_ingress` is false for every path
  and header combination in the existing R-08 test suite, and
  **`GET /v1/models` without an `anthropic-version` header returns the byte-identical OpenAI list
  shape it returns today** — a regression test asserts this, because ApexOS's LAN compute sweep
  identifies a node by that shape.
- Clippy clean; `translate.rs` does no I/O and is not `async`.

**D-01 · docs (part A)** — owns `docs/API.md`, `docs/ROUTING.md`, `openapi/apexrouter-v1.yaml`
*Acceptance*: every route in §6 documented with a jsonc example carrying inline enum comments. The
OpenAPI file validates and is checked in CI against the route table (a test enumerates axum's routes
and diffs).

**Stage 5 gate**: `cargo clippy -p …seven headless crates… -- -D warnings` clean;
`cargo test --workspace --exclude apexrouter-slint` green; each unit's own acceptance met.

---

### Stage 6 — integration, migration and docs (5 agents)

**I-01 · migration end-to-end** — owns `crates/apexrouter-cli/tests/migrate_e2e.rs`,
`docs/MIGRATION.md`
*Acceptance*: `apexrouter migrate --dry-run` against the **real** `~/.vastai-gguf` and the real
LocalRouter checkout prints a plan with per-row reasons and writes nothing (directory hash
unchanged). `--apply` in a redirected `$APEXROUTER_HOME` imports providers as credential
*references* (the real Together key is **not** copied), merges `usage.log` with zero failed rows,
seeds the ledger from `.last_instance`/`.instance_history`, and imports `known_forks` + the docker
map + tier→profile seeds from `recipes.toml`.

**D-02 · README + banner** — owns `README.md`, `assets/banner.png`
*Acceptance*: house structure exactly — centred `<div align="center">` header with the
Imaginarium-generated banner at `width=100%`, H1, bold tagline, italic sub-lines, shields.io badges,
`</div>`, `---`; then Why → How it works (ASCII box diagram) → Quick start (copy-paste shell) →
CLI → Agents (MCP) → Surfaces table → Repository layout table → Security posture → Configuration
(What | Where) table → License **including the plain-English Slint/GPL caveat** → centred `<sub>`
ecosystem footer crediting the banner model. A `> [!NOTE]` callout carries the one must-not-miss
operational fact (both `http://127.0.0.1:8888` and `.../v1` work as base URLs).

**D-03 · CLAUDE.md + CHARTER** — owns `CLAUDE.md`, `docs/CHARTER.md`, `docs/LICENSING.md`
*Acceptance*: `CLAUDE.md` follows the house shape — architecture in one breath; **Invariants** (the
five from `ARCHITECTURE.md` §0.1); Where things live; **Sharp edges met and filed down** (the
build-vulkan RUNPATH trap, axum 0.8 `{param}` syntax, `--help` backend detection being measured
broken, `new_contract` not `id`, `/proc/pid/stat` field 22 after the last `)`, the `X-Usage` stream
divergence, the legacy pidfile kill switch); Workflow; Roadmap seeds. `docs/CHARTER.md` opens with a
binding numbered decisions log D1..Dn (two listeners; `/api` vs `/v1`; children outlive the manager;
`unknown_model = reject`; legacy pidfile off by default; `X-Usage` buffered-only; compat routes
removed at 1.0) with dated amendments.

**D-04 · SLINT + AGENTS + ARCHITECTURE cross-check + SKILL** — owns `docs/SLINT.md`,
`docs/AGENTS.md`, `skills/apexrouter/SKILL.md`
*Acceptance*: `docs/SLINT.md` carries the architecture box, the thread model, a **web→Slint port map
table**, and the honest deferral table. `docs/AGENTS.md` gives copy-paste MCP registration snippets
per harness (Claude Code, ApexOS) for both local and `--proxy` modes.
`skills/apexrouter/SKILL.md` has trigger-heavy YAML front-matter then: Pick your surface
(MCP → CLI → REST) / Knowledge you need / Patterns — and it **must state the correct base URL
forms**, because LocalRouter's own SKILL.md told agents to use the one that 404s.

**I-02 · the full acceptance run** — owns `scripts/acceptance.sh`, `docs/ACCEPTANCE.md`
*Acceptance*: the script runs §7.2 unattended except for the two interactive confirmations, and
prints a pass/fail table.

---

## 5. File-ownership index

Quick check that no file is owned twice. If you are about to write a file not on this list, stop.

| Owner | Files |
|---|---|
| S0 | all `Cargo.toml`, `rustfmt.toml`, `.gitignore`, `.github/workflows/ci.yml`, `routes.example.toml`, all of `crates/apexrouter-protocol/src/**`, every `crates/*/src/lib.rs`, every stub file, `ui-web/*` placeholders, `crates/apexrouter-slint/build.rs` |
| C-01 | `core/src/paths.rs`, `core/src/error.rs` |
| C-02 | `core/src/config.rs`, `config.example.toml` |
| C-03 | `core/src/secret.rs` |
| C-04 | `core/src/store.rs` |
| C-05 | `core/src/lockfile.rs` |
| C-06 | `core/src/proc.rs` |
| C-07 | `core/src/exec.rs` |
| C-08 | `core/src/ledger.rs`, `core/src/money.rs` |
| C-09 | `core/src/discover/builds.rs`, `core/src/discover/mod.rs` |
| C-10 | `core/src/discover/models.rs`, `core/src/discover/gguf.rs` |
| C-11 | `core/src/fit.rs` |
| C-11b | `core/src/discover/physical.rs` |
| C-12 | `core/src/argv.rs` |
| C-13 | `core/src/upstream.rs` |
| C-14 | `core/src/usage.rs`, `core/src/pricing.rs` |
| C-15 | `core/src/catalog.rs` |
| C-16 | `core/src/migrate.rs` |
| C-17 | `core/src/checks.rs` |
| R-01 | `router/src/table.rs`, `router/src/registry.rs` |
| R-02 | `router/src/resolve.rs`, `router/src/policy.rs` |
| R-03 | `router/src/relay/headers.rs`, `router/src/relay/body.rs`, `router/src/relay/mod.rs` |
| R-04 | `router/src/attempt.rs`, `router/src/breaker.rs`, `router/src/limits.rs` |
| R-05 | `router/src/relay/stream.rs` |
| R-06 | `router/src/errors.rs`, `router/src/models.rs` |
| R-07 | `router/src/telemetry.rs` |
| R-08 | `router/src/lib.rs`, `router/src/handler.rs` |
| R-09 | `router/src/compat.rs` |
| R-10 (Stage 5) | `router/src/anthropic/{mod,translate,sse}.rs` |
| P-01 | `providers/src/local/**` |
| P-02 | `providers/src/vast/api.rs`, `providers/src/vast/query.rs`, `providers/src/vast/mod.rs` |
| P-03 | `providers/src/vast/offers.rs` |
| P-04 | `providers/src/vast/{rent,boot,stall}.rs` |
| P-05 | `providers/src/ssh.rs` |
| P-06 | `providers/src/together.rs` |
| P-07 | `providers/src/hf.rs` |
| P-08 | `providers/src/{checks,smoke,compare}.rs` |
| S-01 | `server/src/{lib,state,shutdown}.rs` |
| S-02 | `server/src/auth.rs` |
| S-03 | `server/src/api/{mod,snapshot,backends,routes,endpoints}.rs` |
| S-04 | `server/src/api/{rig,fit,catalog,usage,requests,jobs}.rs`, `server/src/jobs.rs` |
| S-05 | `server/src/{ws,assets,prober,watcher}.rs` |
| S-06 | `cli/src/{main,cli,daemon,render}.rs`, `cli/src/cmd/{mod,status,serve,config,rig,models,fit,endpoint,route,switch,url,version,completions}.rs` |
| S-07 | `server/src/api/{vast,hf,providers,checks,compare}.rs` |
| S-08 | `cli/src/cmd/{vast,tunnel,hf,provider,recipe,profile,usage,smoke,doctor,compare,backend,swap,up,approvals,token,open,env,migrate}.rs` |
| CL-01 (Stage 4, with S-06) | `client/src/lib.rs`, `client/src/ws.rs` |
| U-01 | `ui-web/{index.html,app.js,style.css}` |
| U-02 | `crates/apexrouter-slint/src/**` (except `build.rs`) |
| M-01 | `cli/src/mcp/**`, `cli/src/cmd/mcp.rs` |
| D-01 | `docs/API.md`, `docs/ROUTING.md`, `openapi/apexrouter-v1.yaml` |
| D-02 | `README.md`, `assets/banner.png` |
| D-03 | `CLAUDE.md`, `docs/CHARTER.md`, `docs/LICENSING.md` |
| D-04 | `docs/SLINT.md`, `docs/AGENTS.md`, `skills/apexrouter/SKILL.md` |
| I-01 | `crates/apexrouter-cli/tests/migrate_e2e.rs`, `docs/MIGRATION.md` |
| I-02 | `scripts/acceptance.sh`, `docs/ACCEPTANCE.md` |

**CL-01 · `apexrouter-client`** (belongs to Stage 4, runs alongside S-06):
```rust
pub struct NodeClient { http: reqwest::Client, base: String, token: Option<String> }
impl NodeClient {
    pub fn new(base: impl Into<String>, token: Option<String>) -> Self;   // 300 s timeout
    fn auth(&self, rb: RequestBuilder) -> RequestBuilder;                  // private
    pub async fn health(&self) -> Result<Value>;
    pub async fn snapshot(&self) -> Result<Snapshot>;
    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T>;
    pub async fn post<B: Serialize, T: DeserializeOwned>(&self, path: &str, b: &B) -> Result<T>;
    pub async fn put<B: Serialize, T: DeserializeOwned>(&self, path: &str, b: &B) -> Result<T>;
    pub async fn delete(&self, path: &str) -> Result<()>;
    pub async fn subscribe(&self) -> Result<impl Stream<Item = Result<Event>>>;  // /ws
    pub async fn sse(&self, path: &str) -> Result<impl Stream<Item = Result<Event>>>;
}
```
*Acceptance*: manual status/text check **before** `serde_json::from_str`, so a 500 HTML page yields
a useful error rather than a parse failure. No business logic. `subscribe()` reconnects with
backoff and re-emits the snapshot on reconnect.

---

## 6. Build and verification strategy

### 6.1 What must pass at each stage

| Stage | Command | Must be green |
|---|---|---|
| 0 | `cargo fmt --all -- --check` · `cargo check -p <7 headless crates>` · `cargo test -p apexrouter-protocol` · `cargo check -p apexrouter-slint` | all |
| 1 | `cargo test -p apexrouter-core` · `cargo clippy -p apexrouter-core -- -D warnings` | all |
| 2 | as stage 1, plus the on-machine `rig`/`models` assertions | all |
| 3 | `cargo test -p apexrouter-router -p apexrouter-providers` · clippy on both | all; **no network, no llama.cpp, no credentials** |
| 4 | full CI job **plus §4.1 MK1-CORE ACCEPTANCE on the laptop** | all |
| 5 | full CI job; each unit's acceptance; web UI screenshots attached | all |
| 6 | full CI job **plus §7.2 MK1 ACCEPTANCE** | all |

The CI job at every stage is exactly `.github/workflows/ci.yml` §1.11: fmt, the no-shell-out grep,
clippy `-D warnings` on the seven headless crates, `cargo test --workspace --exclude
apexrouter-slint`, `cargo build --release`.

### 6.2 Test taxonomy

- **Unit** — pure functions: `fit`, `argv`, `resolve`, `normalize_path`, `outbound_headers`,
  `plan_body`, `aggregate`, `GeoFilter::matches`, `Money`. No I/O, no tokio where avoidable.
- **Fixture** — recorded real payloads under `crates/*/tests/fixtures/`: llama.cpp `/props`,
  `/slots`, `/v1/models`, a streaming SSE capture; Together `/v1/models` (bare array) and an SSE
  capture; Vast `search/asks`, `instances`, `users/current`; HF `paths-info`; the real
  `usage.log`; all four `.active_endpoint` shapes; `recipes.toml`.
- **Integration (`wiremock`)** — the whole router pipeline against a fake upstream: retry, no-retry-
  after-first-byte, breaker, 413, 508, SSE relay, disconnect abort.
- **On-machine** — marked `#[ignore]` and run explicitly with `--ignored`: discovery, spawn/stop,
  adoption, the RUNPATH negative test, the ten-times start/stop loop.
- **Money** — **`FixtureVast` only.** The single live Vast call in the test suite is
  `GET /users/current/`, which is free. No test ever creates an instance.

### 6.3 Regression tests that exist because a judge found the bug

Each of these is a named test that must exist, because each corresponds to a fatal flaw found in one
of the three source proposals:

| Test | Guards against |
|---|---|
| `router::table::live_state_survives_recompile` | rebuilding the table resetting semaphores/breakers mid-flight |
| `router::attempt::disconnect_releases_permit` | permit + telemetry leak on client disconnect |
| `router::handler::merge_does_not_panic` | the axum `/{*path}` overlap panic |
| `core::lockfile::child_does_not_inherit_lock` | flock inheritance making the daemon permanently unstartable |
| `core::proc::stat_field22_with_parens_in_comm` | naive whitespace splitting of `/proc/pid/stat` |
| `core::proc::signal_verified_refuses_mismatch` | SIGTERM to a reused PID |
| `providers::local::health_timeout_leaves_no_orphan` | the Python's guaranteed orphan on every timeout |
| `providers::local::two_launches_cannot_share_a_port` | the TOCTOU port race |
| `providers::local::insufficient_vram_is_refused` | launching a second model that OOMs the first |
| `server::watcher::log_writes_do_not_trigger_reload` | a recursive state-dir watch firing 10×/s |
| `server::auth::rebinding_host_is_rejected` | DNS-rebinding against the loopback control plane |
| `core::ledger::drop_without_commit_writes_orphan_suspect` | a billing leak in the Ctrl-C window |
| `core::money::spend_approval_has_no_literal_constructor` (doc test) | fabricating an approval |
| `router::relay::rewrite_moves_only_the_model_key` | body rewriting perturbing tool payloads |
| `router::compat::smoke_sh_model_x_still_routes` | breaking `smoke.sh` |
| `router::relay::v1_collapse_both_base_urls` | the `/v1/v1/...` 404 |
| `core::usage::real_legacy_log_has_zero_failed_rows` | a strict deserializer rejecting real data |

### 6.4 Local dev loop

```bash
# fast: never touches Slint, never needs libfontconfig1-dev
cargo check -p apexrouter-protocol -p apexrouter-core -p apexrouter-router \
            -p apexrouter-providers -p apexrouter-client -p apexrouter-server -p apexrouter-cli
cargo test  -p apexrouter-core -p apexrouter-router
cargo run   -p apexrouter-cli -- serve --foreground        # ui_dir="" uses the embedded UI
# live-reload the web UI without rebuilding:
APEXROUTER_UI_DIR=$PWD/ui-web cargo run -p apexrouter-cli -- serve --foreground
# the Slint app, separately:
cargo run -p apexrouter-slint --bin apexrouter-ui
```

Disk note: `/` is at 92%. Use `cargo clean -p <crate>` rather than a full clean, and keep
`[profile.dev] debug = "line-tables-only"`.

---

## 7. Acceptance

### 7.1 MK1-CORE — reproduced in §4.1 above. It is the Stage 4 gate and must pass before Stage 5.

### 7.2 MK1 — the full end-to-end run

Everything in §4.1, still passing, **plus**:

**A. Both GUIs.**
```bash
apexrouter open                       # web UI at http://127.0.0.1:2739
cargo run -p apexrouter-slint --bin apexrouter-ui &
```
- Both show the running endpoint with its model, port, device, slots and a **real tok/s** after the
  §4.1 requests.
- Both show the rig strip with `Vulkan0`, its free/total VRAM, and the endpoint listed in `held_by`.
- Both show the request just made in the live-request view with alias, backend, TTFT, tok/s, tokens.
- In **each** GUI independently: open the Routes panel, re-point `auto` at a second endpoint, and
  confirm the next `curl` goes there — **with no restart**.
- In **each** GUI independently: open the Catalog panel, save the running endpoint as a recipe,
  edit its ctx, and launch it. Then create a new search profile for `RTX 3090 ×2` and see it appear
  in the Rent tab's profile list.

**B. CLI.**
```bash
apexrouter route set auto --target <second-endpoint>      # prints the before -> after diff
apexrouter swap auto --to Carnice-9b-Q6_K                  # mode chosen by fit(); zero 5xx during
apexrouter smoke --alias auto                              # four probes, all pass, TTFT + tok/s
apexrouter usage --since 24h --by alias --json | jq
apexrouter doctor --json | jq '.[] | select(.status=="fail")'   # empty
apexrouter recipe ls && apexrouter profile ls
apexrouter completions bash > /dev/null
```
A client loop (`while true; do curl … ; done`) running across the `swap` records **zero** connection
errors and zero 5xx.

**C. MCP.** With `~/Projects/.mcp.json` pointing at `target/release/apexrouter` `["mcp"]`, from
Claude Code:
- `apexrouter_status` returns the proxy URL, the aliases and the rig.
- `apexrouter_models` lists `auto` and the backend model.
- `apexrouter_up` with `{"model":"Carnice-9b-Q6_K","alias":"agent"}` starts and binds in one call
  and returns a base URL the agent can use immediately.
- `apexrouter_logs` returns the endpoint tail after a deliberately-failed start.
- `apexrouter_swap` re-points `auto`.
- `apexrouter_vast_rent` **without** `confirm` returns `isError: true` containing the cost preview
  and the live credit figure — and creates nothing.

**D. Money path, without spending.**
```bash
apexrouter vast account                 # live: prints the real credit (~$7.73)
apexrouter vast gpu-names               # live: the real vocabulary, incl. "RTX 3090", "H100 SXM"
apexrouter vast offers --profile rtx3090 --json | jq '.offers[0] | {id,gpu_name,num_gpus,dph_total}'
                                        # live: a real 2x3090 around $0.30/hr
apexrouter vast rent --auto --profile rtx3090 --model-repo X --quant Q --max-hourly 0.40 --dry-run
                                        # prints the offer, the $/hr, the estimated total, the
                                        # credit, the burn-down, the full container env and the
                                        # onstart string — and creates NOTHING
```
Plus the fixture-driven test suite covering reserve→create→commit, the boot watchdog, the stall
detector, tunnel up/down, and destroy-with-verification.

**E. Drop-in compatibility, one more time.**
```bash
bash smoke.sh http://127.0.0.1:8888        # unmodified, passes
bash smoke.sh http://127.0.0.1:8888/v1     # unmodified, ALSO passes
curl -s localhost:8888/providers | jq      # the exact legacy key structure
curl -s -XPOST localhost:8888/switch -d '{"provider":"together"}' | jq   # {"status":"ok",…}
curl -s localhost:8888/slots               # 403 redacted_endpoint (it echoes prompts)
```

**F. Migration.**
```bash
apexrouter migrate --dry-run               # a per-row plan over the REAL ~/.vastai-gguf
```
Reports every legacy artefact, marks the 54 `vast_gguf` recipes `Skip` with the reason, and writes
nothing.

**G. Hygiene.**
```bash
pgrep -a llama-server                      # exactly the endpoints ApexRouter reports
ls ~/.local/state/apexrouter               # all state here; nothing in the repo
git status --porcelain                     # clean: no state file was written into the checkout
grep -rn 'unwrap()' crates/*/src | grep -v tests | wc -l    # 0 outside tests and main()
```

**H. Anthropic ingress (the bonus surface, R-10).**
```bash
curl -s localhost:8888/v1/messages -H 'anthropic-version: 2023-06-01' \
  -H 'x-api-key: not-needed' -H 'content-type: application/json' \
  -d '{"model":"auto","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}' | jq
# -> {"type":"message","role":"assistant","content":[{"type":"text",...}],
#     "stop_reason":"end_turn","usage":{"input_tokens":N,"output_tokens":M}}

curl -s localhost:8888/v1/models | jq '.data[0]'          # OpenAI shape, unchanged (ApexOS sweep)
curl -s localhost:8888/v1/models -H 'anthropic-version: 2023-06-01' | jq '.data[0].type'  # "model"
```
- The same request with `"stream":true` yields the named-event sequence: `message_start`,
  `content_block_start`, N × `content_block_delta`, `content_block_stop`, `message_delta` (with
  `stop_reason` **and** final `usage`), `message_stop`.
- The same request **without** `max_tokens` returns `400` with an **Anthropic-shaped** error body.
- With `[router] anthropic_tools = false` (the default), a body carrying `tools` returns `400`
  naming the config key — and the upstream is never contacted.
- **The real test:** `ANTHROPIC_BASE_URL=http://127.0.0.1:8888 ANTHROPIC_API_KEY=not-needed claude`
  starts a Claude Code session that completes a turn against the local model.

When A–H pass, mk1 is done.
