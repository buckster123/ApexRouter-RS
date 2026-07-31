//! OWNER: unit S-01 (`server/src/{lib,state,shutdown}.rs` and this guard).
//!
//! **The mounting guard.** Three times now a control-plane module has shipped implemented,
//! unit-tested green and completely unreachable, because its one `.merge(…)` line in
//! `v1_routes()` was never written — Stage 4's `catalog` (all of `/v1/recipes*` and
//! `/v1/profiles*` dead) and Stage 5's `{vast, hf, providers, checks, compare}` (`/v1/checks`
//! and `/v1/vast/account` 404, `POST /v1/vast/instances` and `/v1/vast/offers/search` 405, so
//! `apexrouter doctor` and `apexrouter vast rent` could not run at all).
//!
//! A unit test cannot catch this. Every one of those modules had its own tests, and every one
//! of them passed: they build the module's own `axum::Router` in isolation and never once see
//! the composed application. `tests/openapi_routes.rs` compares two *documents* — source and
//! OpenAPI — which agreed with each other while the daemon served neither. **The only
//! question that catches it is "does the running daemon answer this path?", and this file is
//! the only place that asks it.**
//!
//! # What is asserted
//!
//! 1. [`every_api_module_router_is_reachable_from_the_booted_daemon`] — the inventory of
//!    `pub fn router()` under `src/api/` is **recovered from the source tree at test time**,
//!    never hand-maintained (a hand-maintained list has exactly the failure mode of the doc
//!    comments that caused this bug), and every path every module registers must be served by
//!    a real, booted, fully-mounted daemon.
//! 2. [`the_mounted_method_set_is_the_method_set_the_modules_register`] — for each of those
//!    paths, **axum's own report** of the composed method router's method set must contain
//!    every method the source registers. This catches the next variant: a module that is
//!    merged but whose methods a later overlapping merge silently dropped.
//! 3. [`every_documented_control_route_is_reachable`] — every path `ARCHITECTURE.md` §6.2
//!    documents (through `openapi/apexrouter-v1.yaml`, its machine form, which
//!    `tests/openapi_routes.rs` holds to §6) is either reachable, or is not implemented **at
//!    all** — not present in any `.route()` call anywhere. Documented-and-implemented-but-
//!    unreachable is precisely the defect, and it fails here.
//! 4. [`the_absent_route_signature_is_real`] — the guard proves its own negative. Paths that
//!    genuinely do not exist are classified `Unrouted` by the same discriminator the tests
//!    above use, so "everything passed" can never mean "the probe cannot tell the difference".
//!
//! # How reachability is decided, without running a single handler
//!
//! `axum::Router` in 0.8 exposes no way to enumerate what it holds — the whole public surface
//! is `has_routes() -> bool` — so the composed route table cannot be walked directly. It can,
//! however, be **interrogated**, and the two signals below are answers axum and S-02 give
//! about the composed application itself, not heuristics over its source:
//!
//! * **Presence** — one `PATCH` carrying a foreign `Origin`. S-02's scope table calls that a
//!   mutation, so §9.3's gate refuses it with `403` **before** `next.run(req)`. That gate is
//!   installed by `v1_routes().route_layer(require_auth)`, and by nothing else in the
//!   application: the embedded-UI catch-all (`assets::static_router`'s `/{*path}`) is merged
//!   *outside* the auth layer and is a `get()` route, so an unmounted path answers `405
//!   Allow: GET,HEAD` instead. `403` therefore means "this path is routed inside
//!   `v1_routes()`" and can mean nothing else — and no handler ever runs, which is what keeps
//!   this test hermetic while probing `/v1/vast/*`.
//! * **Methods** — one `TRACE`. The scope table calls `TRACE` a read, so it passes the gate;
//!   no route in the crate registers a `trace()` handler (asserted below), so it reaches the
//!   composed `MethodRouter` and is answered `405` — whose `Allow` header axum builds from
//!   exactly the methods that method router holds. Again no handler runs.
//!
//! # Hermeticity
//!
//! Nothing but `127.0.0.1` is contacted, and the daemon is booted into a `tempfile::TempDir`
//! with `[compat] read_legacy_state = false`, `[providers.together] base_url` and
//! `[vast] base_url` on a closed loopback port. Because neither probe reaches a handler, no
//! provider, Hugging Face or vast.ai call can be made from this file even in principle.

use apexrouter_core::config::Config;
use apexrouter_core::lockfile::DaemonLock;
use apexrouter_core::paths::Paths;
use apexrouter_server::{build_state, run, shutdown, Shutdown};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

/// The HTTP methods a `MethodRouter` can carry, in the spelling axum's constructors use.
const METHODS: [&str; 8] = [
    "get", "post", "put", "patch", "delete", "head", "options", "trace",
];

/// The one control route S-02 declares public (`auth::is_public`), so the presence probe's
/// `403` cannot apply to it: it is not behind the auth layer at all.
///
/// This is a carve-out of one path, not a route list, and it cannot rot silently —
/// [`the_public_carve_out_is_still_exactly_one_public_route`] proves each entry really is
/// outside the auth layer *and* really is served.
const PUBLIC_ROUTES: [&str; 1] = ["/health"];

/// The `Origin` the presence probe sends: an authority this daemon can never be.
const FOREIGN_ORIGIN: &str = "http://apexrouter-mount-guard.invalid";

/// What a path template's parameters are filled with. Never matches a literal sibling route.
const PARAM: &str = "apexrouter-mount-guard";

// =======================================================================================
// the inventory — recovered from the source tree, never written down
// =======================================================================================

/// One module's contribution to the control-plane route table.
#[derive(Debug)]
struct ApiModule {
    /// `vast`, `checks`, … — the `src/api/<name>.rs` stem, or `lib` for `lib.rs`.
    name: String,
    /// Path template -> the methods that file registers on it.
    routes: BTreeMap<String, BTreeSet<String>>,
}

/// The repository root, derived from this crate's manifest directory.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/<crate> always has two ancestors")
        .to_path_buf()
}

/// Read a file that must exist, with a message naming it when it does not.
fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// Every module that publishes a `pub fn router()`, plus `lib.rs`'s own `/health` and `/ws`.
///
/// The directory is *listed*, not enumerated from a constant: a new `src/api/foo.rs` joins
/// this inventory the moment it is written, which is the whole point.
fn inventory() -> Vec<ApiModule> {
    let root = repo_root();
    let api = root.join("crates/apexrouter-server/src/api");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&api)
        .unwrap_or_else(|e| panic!("cannot list {}: {e}", api.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .collect();
    files.push(root.join("crates/apexrouter-server/src/lib.rs"));
    files.sort();

    let mut out = Vec::new();
    for f in files {
        let src = read(&f);
        let stem = f
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_owned();
        // `mod.rs` publishes a `router()` that only aggregates its siblings; `lib.rs` has no
        // `router()` at all but does register `/health` and `/ws` directly. Both belong in
        // the inventory exactly when they register something of their own.
        let routes = routes_in_file(&src);
        if routes.is_empty() {
            continue;
        }
        assert!(
            stem == "lib" || stem == "mod" || src.contains("pub fn router()"),
            "{stem}.rs registers routes but publishes no `pub fn router()`, so nothing can \
             merge it"
        );
        out.push(ApiModule { name: stem, routes });
    }
    assert!(
        out.len() > 5,
        "the source scan found only {} route-registering files — the scanner is broken, and a \
         broken scanner passes every assertion in this file",
        out.len()
    );
    out
}

/// Every `(path, methods)` pair one file registers, ignoring anything after `#[cfg(test)]`.
///
/// Test modules build their own routers — `api/checks.rs` has a whole `serve_s07` helper that
/// merges all five S-07 routers — and those are emphatically not the product's route table.
/// Reading them would make this guard pass while the daemon served nothing.
fn routes_in_file(src: &str) -> BTreeMap<String, BTreeSet<String>> {
    let src = match src.find("#[cfg(test)]") {
        Some(i) => &src[..i],
        None => src,
    };

    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let bytes = src.as_bytes();
    let mut cursor = 0usize;

    while let Some(rel) = src[cursor..].find(".route(") {
        let open = cursor + rel + ".route(".len() - 1; // index of '('
        let Some(end) = matching_paren(bytes, open) else {
            break;
        };
        let call = &src[open + 1..end];
        if let Some(path) = first_string_literal(call) {
            out.entry(path).or_default().extend(methods_in(call));
        }
        cursor = end + 1;
    }
    out
}

/// Index of the `)` closing the `(` at `open`, skipping over string literals.
fn matching_paren(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_str = false;
    let mut i = open;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            match c {
                b'\\' => i += 1,
                b'"' => in_str = false,
                _ => {}
            }
        } else {
            match c {
                b'"' => in_str = true,
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

/// The first `"…"` literal in a `.route(…)` argument list — the path template.
fn first_string_literal(call: &str) -> Option<String> {
    let start = call.find('"')? + 1;
    let rest = &call[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_owned())
}

/// Which `MethodRouter` constructors appear in a `.route(…)` argument list.
///
/// Matches an identifier from [`METHODS`] followed immediately by `(`, with a non-identifier
/// character before it — so `post(post_reload)` yields `post` once and
/// `axum::routing::delete(destroy)` is found through its path qualifier.
fn methods_in(call: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for m in METHODS {
        let mut from = 0usize;
        while let Some(rel) = call[from..].find(m) {
            let at = from + rel;
            let after = at + m.len();
            let before_ok = at == 0
                || !call[..at]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_');
            if before_ok && call[after..].starts_with('(') {
                found.insert(m.to_owned());
                break;
            }
            from = after;
        }
    }
    found
}

/// A path template with its parameters filled in, so it can actually be requested.
///
/// `{*rest}` becomes two segments, because a catch-all that matched nothing would be a
/// different route. [`PARAM`] never collides with a literal sibling (`/v1/routes/default`,
/// `/v1/recipes/from-endpoint/{id}`), so the request lands on the template under test.
fn concrete(template: &str) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('}') else {
            break;
        };
        let name = &rest[open + 1..open + close];
        if name.starts_with('*') {
            out.push_str(PARAM);
            out.push('/');
        }
        out.push_str(PARAM);
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    out
}

// =======================================================================================
// the booted daemon
// =======================================================================================

/// `std::env` is process-global and `Paths::resolve` reads it, so every test that needs a
/// `Paths` rooted in a tempdir takes this first and holds it for the whole test.
static ENV: Mutex<()> = Mutex::new(());

/// The variables the fixture redirects, saved so the process is left as it was found.
///
/// The last five are credential variables, cleared rather than redirected: HERMETICITY says no
/// test may resolve a real credential, and the developer running `cargo test` is exactly the
/// person who has them exported.
const REDIRECTED: [&str; 13] = [
    "HOME",
    "APEXROUTER_HOME",
    "APEXROUTER_CONFIG",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "PROXY_PORT",
    "APEXROUTER_TOKEN",
    "VAST_API_KEY",
    "VASTAI_API_KEY",
    "HF_TOKEN",
    "HUGGING_FACE_HUB_TOKEN",
    "TOGETHER_API_KEY",
];

/// The subset of [`REDIRECTED`] that is cleared outright rather than pointed at the tempdir.
const CLEARED: [&str; 7] = [
    "PROXY_PORT",
    "APEXROUTER_TOKEN",
    "VAST_API_KEY",
    "VASTAI_API_KEY",
    "HF_TOKEN",
    "HUGGING_FACE_HUB_TOKEN",
    "TOGETHER_API_KEY",
];

/// A hermetic `$HOME`: `$STATE`, `$CACHE`, `config.toml` and every legacy path land inside one
/// tempdir, so nothing this suite does can touch `~/.vastai-gguf` or `~/.config`.
struct EnvFixture {
    _guard: MutexGuard<'static, ()>,
    saved: Vec<(&'static str, Option<OsString>)>,
    _dir: tempfile::TempDir,
    paths: Paths,
}

impl Drop for EnvFixture {
    fn drop(&mut self) {
        for (k, v) in &self.saved {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
}

impl EnvFixture {
    fn new() -> EnvFixture {
        let guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let saved: Vec<_> = REDIRECTED
            .iter()
            .map(|k| (*k, std::env::var_os(k)))
            .collect();
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::env::set_var("HOME", root);
        std::env::set_var("APEXROUTER_HOME", root.join("state"));
        std::env::set_var("APEXROUTER_CONFIG", root.join("config.toml"));
        std::env::set_var("XDG_STATE_HOME", root.join("xdg-state"));
        std::env::set_var("XDG_CACHE_HOME", root.join("xdg-cache"));
        std::env::set_var("XDG_CONFIG_HOME", root.join("xdg-config"));
        for k in CLEARED {
            std::env::remove_var(k);
        }
        let paths = Paths::resolve().expect("paths");
        paths.ensure_layout().expect("layout");
        EnvFixture {
            _guard: guard,
            saved,
            _dir: dir,
            paths,
        }
    }
}

/// A port nothing is listening on. Bound and released, which is the only portable way to ask
/// the kernel for one.
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind :0");
    let p = l.local_addr().expect("local_addr").port();
    drop(l);
    p
}

/// The real application, on ephemeral loopback ports, with every outbound base URL closed.
struct Daemon {
    /// `http://127.0.0.1:<control port>`.
    control: String,
    http: reqwest::Client,
    trigger: Shutdown,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
    /// Dropped last: restores `std::env` and removes the tempdir.
    _fx: EnvFixture,
}

impl Daemon {
    /// Boot `serve`'s own state and `run`'s own two listeners — not a router assembled here.
    ///
    /// Assembling a router in the test would reproduce the defect: the thing under test is
    /// `crate::api_router`'s composition, so the test must obtain it from the crate.
    async fn boot() -> Daemon {
        let fx = EnvFixture::new();

        let mut cfg = Config::default();
        cfg.server.proxy_bind = format!("127.0.0.1:{}", free_port());
        cfg.server.control_bind = format!("127.0.0.1:{}", free_port());
        cfg.server.drain_timeout_secs = 5;
        cfg.compat.mirror_usage_log = false;
        cfg.compat.read_legacy_state = false;
        // HERMETICITY: every outbound root points at a closed loopback port. Neither probe
        // reaches a handler, so nothing can dial out; this is the second lock on that door.
        cfg.vast.base_url = "http://127.0.0.1:1/api/v0".to_owned();
        for p in cfg.providers.values_mut() {
            p.base_url = "http://127.0.0.1:1/v1".to_owned();
        }
        let control = format!("http://{}", cfg.control_bind());

        let lock = DaemonLock::acquire(&fx.paths).expect("daemon lock");
        let state = build_state(fx.paths.clone(), cfg, lock)
            .await
            .expect("build_state");
        let (trigger, handle) = shutdown::channel();
        let mut task = tokio::spawn(run(state, handle));

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("client");

        // Poll `/health` until the listeners are up, and report a failed start rather than
        // timing out on it.
        let mut last = None;
        let mut up = false;
        for _ in 0..400 {
            match http.get(format!("{control}/health")).send().await {
                Ok(_) => {
                    up = true;
                    break;
                }
                Err(e) => last = Some(e),
            }
            if task.is_finished() {
                match (&mut task).await {
                    Ok(Ok(())) => panic!("the daemon exited cleanly before serving /health"),
                    Ok(Err(e)) => panic!("the daemon failed to start: {e:#}"),
                    Err(e) => panic!("the daemon task panicked: {e}"),
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(up, "the control listener never answered /health: {last:?}");

        Daemon {
            control,
            http,
            trigger,
            task,
            _fx: fx,
        }
    }

    /// Drain, so the tempdir is not removed from under a running daemon.
    async fn stop(self) {
        self.trigger.trigger();
        let _ = tokio::time::timeout(Duration::from_secs(10), self.task).await;
    }

    /// One request, with the headers the probe needs and nothing reqwest would add.
    async fn send(&self, method: reqwest::Method, path: &str, origin: Option<&str>) -> Probe {
        let mut rb = self.http.request(method, format!("{}{path}", self.control));
        if let Some(o) = origin {
            rb = rb.header(reqwest::header::ORIGIN, o);
        }
        let res = rb.send().await.expect("the control listener must answer");
        let status = res.status().as_u16();
        let allow = res
            .headers()
            .get(reqwest::header::ALLOW)
            .and_then(|v| v.to_str().ok())
            .map(|v| {
                v.split(',')
                    .map(|m| m.trim().to_ascii_uppercase())
                    .filter(|m| !m.is_empty())
                    .collect::<BTreeSet<String>>()
            });
        let body = res.text().await.unwrap_or_default();
        Probe {
            status,
            allow,
            body,
        }
    }

    /// Is `path` routed inside `v1_routes()`? See the module docs for why `403` is the answer.
    async fn reach(&self, path: &str) -> Reach {
        let p = self
            .send(reqwest::Method::PATCH, path, Some(FOREIGN_ORIGIN))
            .await;
        match p.status {
            403 => Reach::Mounted,
            // The embedded-UI catch-all is a `get()` route outside the auth layer, so an
            // unclaimed path is a method mismatch rather than a refusal.
            405 if p.allow.as_ref().is_some_and(|a| a == &get_head()) => Reach::Unrouted,
            _ => Reach::Ambiguous(p.status, p.body.chars().take(200).collect()),
        }
    }

    /// The composed application's method set at `path`, straight out of axum's `Allow` header.
    async fn allowed(&self, path: &str) -> Option<BTreeSet<String>> {
        let p = self.send(reqwest::Method::TRACE, path, None).await;
        (p.status == 405).then_some(p.allow).flatten()
    }
}

/// One probe response, reduced to the three things the discriminator reads.
struct Probe {
    status: u16,
    allow: Option<BTreeSet<String>>,
    body: String,
}

/// `{GET, HEAD}` — what axum reports for any `get()`-only route, the catch-all included.
fn get_head() -> BTreeSet<String> {
    ["GET", "HEAD"].into_iter().map(str::to_owned).collect()
}

/// What the booted daemon says about one path.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Reach {
    /// S-02's mutation gate refused it: the path carries the auth `route_layer`, which only
    /// `v1_routes()` installs. It is mounted.
    Mounted,
    /// The embedded-UI catch-all answered: nothing in the control plane claims this path.
    Unrouted,
    /// Neither. Reported verbatim, because a discriminator that guesses is worse than none.
    Ambiguous(u16, String),
}

/// The methods a source-level `get()` implies at runtime — axum answers HEAD from GET.
fn expand(methods: &BTreeSet<String>) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = methods.iter().map(|m| m.to_ascii_uppercase()).collect();
    if out.contains("GET") {
        out.insert("HEAD".to_owned());
    }
    out
}

// =======================================================================================
// 1. every module's router is reachable
// =======================================================================================

/// Every path every `api/*.rs` module registers is served by a booted, fully-mounted daemon.
///
/// **This is the test the S-07 mounting gap should have failed.** It fails the moment a
/// `.merge(api::<module>::router())` line is missing from `v1_routes()`, and its message is
/// the line to add.
#[tokio::test]
async fn every_api_module_router_is_reachable_from_the_booted_daemon() {
    let modules = inventory();
    let d = Daemon::boot().await;

    let mut unreachable: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut ambiguous = Vec::new();

    for m in &modules {
        for template in m.routes.keys() {
            if PUBLIC_ROUTES.contains(&template.as_str()) {
                continue;
            }
            let path = concrete(template);
            match d.reach(&path).await {
                Reach::Mounted => {}
                Reach::Unrouted => unreachable
                    .entry(m.name.clone())
                    .or_default()
                    .push(template.clone()),
                Reach::Ambiguous(status, body) => {
                    ambiguous.push(format!("{} {template} -> {status} {body:?}", m.name));
                }
            }
        }
    }

    d.stop().await;

    assert!(
        ambiguous.is_empty(),
        "the mounting probe could not classify these paths, so this guard is not guarding \
         anything until they are understood:\n{ambiguous:#?}"
    );
    let fix: Vec<String> = unreachable
        .keys()
        .map(|m| format!("    .merge(api::{m}::router())"))
        .collect();
    assert!(
        unreachable.is_empty(),
        "these api modules publish a `pub fn router()` whose routes the booted daemon does \
         NOT serve — they are implemented, unit-tested and unreachable:\n{unreachable:#?}\n\n\
         Fix: add these lines to `v1_routes()` in \
         crates/apexrouter-server/src/lib.rs:\n{}\n\n\
         (Write them as code. Written as a doc comment they compile, read as done, and serve \
         nothing — which is how this shipped three times.)",
        fix.join("\n")
    );
}

// =======================================================================================
// 2. the methods match, not just the paths
// =======================================================================================

/// axum's own `Allow` header at each path must contain every method the source registers.
///
/// A module can be merged and still lose methods — two `.route()` calls for one path in
/// different modules, a `merge` that overwrote a `MethodRouter`, a handler dropped in a
/// refactor. The path would still answer, so [`every_api_module_router_is_reachable_from_the_booted_daemon`]
/// would still pass, and `POST` would still 405.
#[tokio::test]
async fn the_mounted_method_set_is_the_method_set_the_modules_register() {
    let modules = inventory();
    let d = Daemon::boot().await;

    let mut wrong = Vec::new();
    for m in &modules {
        for (template, methods) in &m.routes {
            let want = expand(methods);
            assert!(
                !want.contains("TRACE"),
                "{}: {template} registers a `trace()` handler, which breaks the method probe \
                 — it would run the handler instead of being answered 405",
                m.name
            );
            let path = concrete(template);
            match d.allowed(&path).await {
                Some(live) => {
                    let missing: Vec<_> = want.difference(&live).cloned().collect();
                    if !missing.is_empty() {
                        wrong.push(format!(
                            "{}: {template} registers {want:?} but the booted daemon allows \
                             {live:?} (missing {missing:?})",
                            m.name
                        ));
                    }
                }
                None => wrong.push(format!(
                    "{}: {template} did not answer the method probe with a 405 + Allow header",
                    m.name
                )),
            }
        }
    }

    d.stop().await;
    assert!(
        wrong.is_empty(),
        "the composed router's method set disagrees with the modules that build it:\n{wrong:#?}"
    );
}

// =======================================================================================
// 3. documented (ARCHITECTURE §6.2) but absent
// =======================================================================================

/// Every control path the API document declares is either reachable, or is not implemented at
/// all.
///
/// The middle state — documented, implemented, unreachable — is the defect, and it is the one
/// this asserts away. `openapi/apexrouter-v1.yaml` is §6.2's machine form; `openapi_routes.rs`
/// is what holds the two together, so a path documented here is a path §6 promises.
///
/// The "not implemented at all" exemption is **derived**, never listed: a documented path with
/// no `.route()` anywhere in the source is genuinely unbuilt (`/metrics`, `POST /v1/migrate`),
/// and the moment somebody writes its handler it stops being exempt and must be mounted.
#[tokio::test]
async fn every_documented_control_route_is_reachable() {
    let root = repo_root();
    let doc = documented_control_paths(&read(&root.join("openapi/apexrouter-v1.yaml")));
    assert!(
        doc.len() > 20,
        "only {} control paths were parsed out of the OpenAPI document — the scanner is broken",
        doc.len()
    );

    let implemented: BTreeSet<String> = inventory()
        .iter()
        .flat_map(|m| m.routes.keys().cloned())
        .collect();

    let d = Daemon::boot().await;
    let mut dead = Vec::new();
    let mut ghosts = Vec::new();

    for (documented_path, axum_path) in &doc {
        if PUBLIC_ROUTES.contains(&axum_path.as_str()) {
            continue;
        }
        let built = implemented.contains(axum_path);
        let reach = d.reach(&concrete(axum_path)).await;
        match (built, &reach) {
            (true, Reach::Mounted) | (false, Reach::Unrouted) => {}
            (true, _) => dead.push(format!("{documented_path} (as {axum_path}) -> {reach:?}")),
            (false, _) => ghosts.push(format!(
                "{documented_path} (as {axum_path}) is served but no source file registers it \
                 -> {reach:?}"
            )),
        }
    }

    d.stop().await;

    assert!(
        dead.is_empty(),
        "ARCHITECTURE §6.2 documents these control routes and the source implements them, but \
         the booted daemon does not serve them:\n{dead:#?}\n\
         Each needs its module merged into `v1_routes()` in \
         crates/apexrouter-server/src/lib.rs."
    );
    assert!(
        ghosts.is_empty(),
        "these documented paths are answered by a daemon whose source registers no handler for \
         them — the route scan and the running application disagree, so this guard cannot be \
         trusted until that is explained:\n{ghosts:#?}"
    );
}

/// `documented path -> the path as axum spells it`, for operations on the control listener.
///
/// The document is written in a regular subset — two-space indentation, no anchors, no block
/// scalars inside `paths:` — so this needs no YAML dependency.
fn documented_control_paths(yaml: &str) -> BTreeMap<String, String> {
    let mut methods: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut listeners: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut axum_route: BTreeMap<String, String> = BTreeMap::new();

    let mut in_paths = false;
    let mut current: Option<String> = None;
    let mut in_method = false;

    for line in yaml.lines() {
        if line.trim_start().starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim_end();

        if indent == 0 {
            in_paths = trimmed.starts_with("paths:");
            current = None;
            in_method = false;
            continue;
        }
        if !in_paths {
            continue;
        }
        if indent == 2 && trimmed.starts_with("  /") && trimmed.ends_with(':') {
            current = Some(trimmed.trim().trim_end_matches(':').to_owned());
            in_method = false;
            continue;
        }
        let Some(path) = current.clone() else {
            continue;
        };
        if indent == 4 {
            let key = trimmed.trim().trim_end_matches(':');
            in_method = METHODS.contains(&key);
            if in_method {
                methods.entry(path).or_default().insert(key.to_owned());
            }
            continue;
        }
        if indent == 6 && in_method {
            let t = trimmed.trim();
            if let Some(rest) = t.strip_prefix("x-listener:") {
                listeners.entry(path).or_default().extend(
                    rest.trim()
                        .trim_start_matches('[')
                        .trim_end_matches(']')
                        .split(',')
                        .map(|s| s.trim().to_owned())
                        .filter(|s| !s.is_empty()),
                );
            } else if let Some(rest) = t.strip_prefix("x-axum-route:") {
                axum_route.insert(path, rest.trim().to_owned());
            }
        }
    }

    methods
        .into_keys()
        .filter(|p| listeners.get(p).is_some_and(|l| l.contains("control")))
        .map(|p| {
            let axum = axum_route.get(&p).cloned().unwrap_or_else(|| p.clone());
            (p, axum)
        })
        .collect()
}

// =======================================================================================
// 4. the guard proves its own negative
// =======================================================================================

/// Paths that genuinely do not exist are classified `Unrouted` — so a green run above means
/// "everything is mounted", never "the probe cannot tell".
///
/// Without this, unmounting every router in `v1_routes()` and re-running would be the only way
/// to know the guard works. This makes that experiment permanent: the absent-route signature is
/// re-derived from the running daemon on every test run, against paths shaped exactly like the
/// real ones — a bare `/v1` leaf, a nested leaf under a mounted prefix, and a namespace that
/// does not exist at all.
#[tokio::test]
async fn the_absent_route_signature_is_real() {
    let d = Daemon::boot().await;

    let absent = [
        "/v1/mount-guard-no-such-route",
        "/v1/checks/mount-guard-no-such-child",
        "/v1/vast/mount-guard-no-such-route",
        "/v1/no-such-namespace/at/all",
    ];
    let mut wrong = Vec::new();
    for p in absent {
        let r = d.reach(p).await;
        if r != Reach::Unrouted {
            wrong.push(format!("{p} -> {r:?}"));
        }
    }

    // And the positive control, on a route that has been mounted since Stage 3: the same
    // probe must be able to say "yes". A discriminator that answers `Unrouted` to everything
    // would pass the loop above and nothing else in this file.
    let mounted = d.reach("/v1/snapshot").await;

    d.stop().await;

    assert!(
        wrong.is_empty(),
        "these paths do not exist, but the mounting probe did not say so — the probe is broken \
         and every other test in this file is vacuous:\n{wrong:#?}"
    );
    assert_eq!(
        mounted,
        Reach::Mounted,
        "the mounting probe cannot recognise `/v1/snapshot`, which has been mounted since \
         Stage 3 — the probe is broken"
    );
}

/// The `PUBLIC_ROUTES` carve-out is exactly what it claims: served, and outside the auth layer.
///
/// The carve-out exists because §6.2's `/health` is public, so the `403` the presence probe
/// relies on cannot apply to it. That makes it the one place this guard could be widened into a
/// hole, so each entry is checked from both sides.
#[tokio::test]
async fn the_public_carve_out_is_still_exactly_one_public_route() {
    let d = Daemon::boot().await;

    for p in PUBLIC_ROUTES {
        let get = d.send(reqwest::Method::GET, p, None).await;
        assert_eq!(
            get.status, 200,
            "{p} is carved out as public but does not answer"
        );
        assert!(
            get.body.contains("\"product\""),
            "{p} answered 200 but not with the control-plane health body: {:?}",
            get.body
        );
        // Outside the auth layer: a foreign `Origin` on a mutation is not refused, because
        // there is no gate on this path to refuse it.
        let probe = d
            .send(reqwest::Method::PATCH, p, Some(FOREIGN_ORIGIN))
            .await;
        assert_ne!(
            probe.status, 403,
            "{p} IS behind the auth layer, so it needs no carve-out — remove it from \
             PUBLIC_ROUTES and let the guard cover it"
        );
    }

    d.stop().await;
}

// =======================================================================================
// the five routes S-07 shipped unreachable
// =======================================================================================

/// The specific defect, named: `/v1/checks`, `/v1/vast/*`, `/v1/hf/*`, `/v1/providers*` and
/// `/v1/compare` answer.
///
/// The general guard above would fail if any of these regressed, but a named test is what a
/// bisect lands on, and it states the acceptance criterion in the words the bug was reported
/// in: these were `404` and `405`.
///
/// `GET /v1/checks` is asserted end to end — it reads S-01's registry and performs no I/O at
/// all, so it is safe to actually run. The rest are asserted through the method set axum
/// reports, which is what `404`/`405` were: no handler is invoked, so no vast.ai, Hugging Face
/// or provider call can happen here.
#[tokio::test]
async fn the_s07_control_plane_is_no_longer_404_and_405() {
    let d = Daemon::boot().await;

    // GET /v1/checks — was 404. The registry, in registration order.
    let checks = d.send(reqwest::Method::GET, "/v1/checks", None).await;
    assert_eq!(
        checks.status, 200,
        "GET /v1/checks must serve the check registry, not {} {:?}",
        checks.status, checks.body
    );
    let ids: Vec<String> =
        serde_json::from_str(&checks.body).expect("GET /v1/checks must answer a JSON array of ids");
    assert!(
        !ids.is_empty(),
        "the check registry is empty, so `apexrouter doctor` has nothing to run"
    );

    // The four that were 405 because the path fell through to the UI catch-all's `get()`.
    let want: [(&str, &str); 6] = [
        ("/v1/vast/offers/search", "POST"),
        ("/v1/vast/instances", "POST"),
        ("/v1/compare", "POST"),
        ("/v1/smoke", "POST"),
        ("/v1/hf/downloads", "POST"),
        ("/v1/providers/apexrouter-mount-guard", "PUT"),
    ];
    for (path, method) in want {
        let allow = d
            .allowed(path)
            .await
            .unwrap_or_else(|| panic!("{path} did not answer the method probe"));
        assert!(
            allow.contains(method),
            "{method} {path} was 405 before S-07 was mounted; the daemon now allows {allow:?}"
        );
    }

    // And the two that were 404 outright.
    for path in ["/v1/vast/account", "/v1/vast/gpu-names", "/v1/providers"] {
        assert_eq!(
            d.reach(path).await,
            Reach::Mounted,
            "{path} was 404 before S-07 was mounted"
        );
    }

    d.stop().await;
}
