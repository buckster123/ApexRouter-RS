//! OWNER: unit S-03 (server/src/api/{mod,snapshot,backends,routes,endpoints}.rs). Do not
//! edit outside that unit — `{rig,fit,catalog,usage,requests,jobs}.rs` are S-04 and
//! `{vast,hf,providers,checks,compare}.rs` are S-07.
//!
//! The control-plane REST surface, all under `/v1/` (the proxy's `/v1` lives on a different
//! socket, so there is no collision). **Every response body is a protocol type** and every
//! mutation is `Origin`/`Host`-gated.
//!
//! `?no_wait=true` is the house pattern: return a `JobRecord` immediately and have the
//! spawned task flip the row to `Failed` on **every** error path, including a `JoinError`
//! from a panic, so nothing sits `Pending` forever.
//!
//! # What lives here rather than in a handler
//!
//! Four things are shared by every module in this directory, so they are published from
//! `mod.rs`:
//!
//! * [`ApiError`] — the one failure shape. Every refusal on the control plane is an
//!   [`apexrouter_protocol::ErrorEnvelope`], never an axum `String`, so the CLI's `--json`
//!   mode and both GUIs branch on `error.kind` instead of matching prose.
//! * [`apply_routes`] and [`recompile`] — **the only two ways a `RoutingTable` is ever
//!   swapped in.** Compile happens *before* the file is persisted and before `store_table`,
//!   which is what makes `PUT /v1/routes` atomic: a table that does not compile never
//!   reaches disk and never displaces the one that is serving.
//! * [`register_backend`] — registry upsert, recompile, broadcast, in that order.
//! * [`request_shutdown`] — `POST /v1/shutdown` has to reach S-01's signal loop, and
//!   `AppState` (S-01's file) publishes no field for it. A process-global `Notify` is the
//!   smallest thing that does not require editing another unit's file.

pub mod backends;
pub mod catalog;
pub mod checks;
pub mod compare;
pub mod endpoints;
pub mod fit;
pub mod hf;
pub mod jobs;
pub mod migrate;
pub mod providers;
pub mod requests;
pub mod rig;
pub mod routes;
pub mod snapshot;
pub mod usage;
pub mod vast;

use crate::state::AppState;
use apexrouter_core::error::Error as CoreError;
use apexrouter_protocol::{
    Alias, Backend, BackendId, BackendSelector, ErrorBody, ErrorEnvelope, Event, ModelRoute,
    RetryPolicy, RouteFile, RouteFilter, RouteTarget, Severity, Strategy, ValidationIssue,
    ValidationReport,
};
use apexrouter_router::TableBuilder;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;
use tokio::sync::Notify;

/// The control-plane routes unit S-03 owns, ready to be `.merge`d by S-01's `api_router`.
///
/// This is **not** the aggregate of everything in this directory, and never was. Each of
/// S-04's modules (`/v1/rig*`, `/v1/fit`, `/v1/usage`, `/v1/requests*`, `/v1/jobs*`) and
/// S-07's (`/v1/vast*`, `/v1/hf*`, `/v1/providers*`, `/v1/checks`, `/v1/compare`) publishes
/// its own `pub fn router()` and is merged **once, by `crate::v1_routes`** — the single
/// assembly point, in the single file only S-01 may edit.
///
/// A module that is not merged there is compiled, unit-tested and unreachable; that shipped
/// twice. `tests/mounted_routes.rs` now enumerates this directory and asks the booted
/// application which paths it actually serves, so a missing merge fails the build rather
/// than the operator.
pub fn router() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .merge(snapshot::router())
        .merge(backends::router())
        .merge(routes::router())
        .merge(endpoints::router())
}

// ----------------------------------------------------------------------------------------
// errors
// ----------------------------------------------------------------------------------------

/// One control-plane refusal: an HTTP status plus the `ErrorEnvelope` body.
///
/// `kind` is the machine-readable discriminator (`"not_found"`, `"port_in_use"`,
/// `"invalid_routes"`, …), which is what lets the CLI satisfy `ARCHITECTURE.md` §7's
/// "`{"error":{"kind":…}}` on stdout, exit 1" without inventing exit codes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiError {
    /// The status to answer with.
    pub status: StatusCode,
    /// The body.
    pub body: ErrorBody,
}

impl ApiError {
    /// A refusal with an explicit status and kind.
    pub fn new(status: StatusCode, kind: &str, message: impl Into<String>) -> ApiError {
        ApiError {
            status,
            body: ErrorBody {
                kind: kind.to_owned(),
                message: message.into(),
                param: None,
                code: None,
            },
        }
    }

    /// `404 not_found`.
    pub fn not_found(message: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    /// `400`, with the caller's own kind.
    pub fn bad_request(kind: &str, message: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::BAD_REQUEST, kind, message)
    }

    /// `409 conflict`.
    pub fn conflict(message: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::CONFLICT, "conflict", message)
    }

    /// `500 internal`.
    pub fn internal(message: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
    }

    /// `501 not_implemented`, for a documented route whose backing unit has not landed.
    pub fn not_implemented(message: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::NOT_IMPLEMENTED, "not_implemented", message)
    }

    /// Name the offending parameter, so a form can highlight one field.
    #[must_use]
    pub fn with_param(mut self, param: &str) -> ApiError {
        self.body.param = Some(param.to_owned());
        self
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(ErrorEnvelope { error: self.body })).into_response()
    }
}

impl From<CoreError> for ApiError {
    /// Map the library error onto a status **and a stable kind**.
    ///
    /// The mapping is by what the operator can do about it, not by which syscall failed: a
    /// port that is taken and VRAM that is short are both `409`, because both mean "the
    /// machine is not in a state where this can happen yet", and both name the holder.
    fn from(e: CoreError) -> ApiError {
        let (status, kind) = match &e {
            CoreError::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
            CoreError::Invalid { .. } => (StatusCode::BAD_REQUEST, "invalid"),
            CoreError::Id(_) => (StatusCode::BAD_REQUEST, "bad_id"),
            CoreError::MissingCredential(_) => (StatusCode::BAD_REQUEST, "missing_credential"),
            CoreError::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
            CoreError::PortInUse { .. } => (StatusCode::CONFLICT, "port_in_use"),
            CoreError::InsufficientVram { .. } => (StatusCode::CONFLICT, "insufficient_vram"),
            CoreError::Timeout { .. } => (StatusCode::GATEWAY_TIMEOUT, "timeout"),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        };
        ApiError::new(status, kind, e.to_string())
    }
}

/// The shape every control-plane handler returns.
pub type ApiResult<T> = std::result::Result<Json<T>, ApiError>;

// ----------------------------------------------------------------------------------------
// shared bits and pieces
// ----------------------------------------------------------------------------------------

/// Unix seconds, now.
pub fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The control plane's own outbound HTTP client.
///
/// One pooled client for probes and 20-token smoke tests. Separate from the router's, which
/// is deliberately `no_gzip`/`no_brotli`/`no_deflate` because it relays bytes verbatim —
/// nothing here relays anything, it parses JSON.
pub fn http() -> &'static reqwest::Client {
    static HTTP: OnceLock<reqwest::Client> = OnceLock::new();
    HTTP.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .build()
            .unwrap_or_default()
    })
}

/// Broadcast an event, ignoring "nobody is listening".
pub fn emit(state: &Arc<AppState>, ev: Event) {
    let _ = state.tx.send(ev);
}

/// Render a [`ValidationReport`] as one line, for an `Alert` or a log.
pub fn render_issues(report: &ValidationReport) -> String {
    if report.issues.is_empty() {
        return "the routing table did not compile".to_owned();
    }
    report
        .issues
        .iter()
        .map(|i| format!("{}: {}", i.field, i.message))
        .collect::<Vec<_>>()
        .join("; ")
}

/// A one-issue [`ValidationReport`], for the failures that are not the compiler's.
pub fn single_issue(field: &str, message: impl Into<String>, fix: &str) -> ValidationReport {
    ValidationReport {
        ok: false,
        issues: vec![ValidationIssue {
            field: field.to_owned(),
            severity: Severity::Error,
            message: message.into(),
            fix: Some(fix.to_owned()),
        }],
    }
}

/// The last compile failure, or `None` when the table on disk is the table serving.
pub fn table_error() -> Option<String> {
    table_error_slot()
        .read()
        .map(|g| g.clone())
        .unwrap_or_default()
}

/// Where [`table_error`] reads from.
fn table_error_slot() -> &'static RwLock<Option<String>> {
    static SLOT: OnceLock<RwLock<Option<String>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(None))
}

/// Record (or clear) the last compile failure.
fn set_table_error(v: Option<String>) {
    if let Ok(mut slot) = table_error_slot().write() {
        *slot = v;
    }
}

// ----------------------------------------------------------------------------------------
// the routing table — the only two ways it is ever swapped
// ----------------------------------------------------------------------------------------

/// Compile `file`, persist it, and arm it. **In that order.**
///
/// This is what makes `PUT /v1/routes` atomic. A table that does not compile is refused
/// before `save_routes` runs and before `store_table` runs, so the previous table keeps
/// serving *and* the file on disk still describes it — a reload after a failed PUT reproduces
/// what is running rather than resurrecting the rejected draft.
///
/// # Errors
/// The compiler's [`ValidationReport`], or a one-issue report when the write failed.
pub fn apply_routes(
    state: &Arc<AppState>,
    file: &RouteFile,
) -> std::result::Result<Vec<ModelRoute>, ValidationReport> {
    let cfg = state.cfg.load_full();
    let table = TableBuilder::compile(&cfg, file, state.router.registry()).map_err(|report| {
        let msg = render_issues(&report);
        set_table_error(Some(msg.clone()));
        emit(
            state,
            Event::RouteTableChanged {
                routes: state
                    .store
                    .load_routes()
                    .map(|f| f.routes)
                    .unwrap_or_default(),
                valid: false,
                error: Some(msg),
            },
        );
        report
    })?;

    if let Err(e) = state.store.save_routes(file) {
        // The table compiled but could not be persisted. Do NOT arm it: a table that is
        // serving but is not on disk vanishes on the next reload, which is the worst of
        // both outcomes.
        let msg = e.to_string();
        set_table_error(Some(msg.clone()));
        return Err(single_issue(
            "routes",
            format!("the table compiled but could not be written: {msg}"),
            "check permissions on $STATE/routes.json",
        ));
    }

    state.router.store_table(table);
    set_table_error(None);
    emit(
        state,
        Event::RouteTableChanged {
            routes: file.routes.clone(),
            valid: true,
            error: None,
        },
    );
    Ok(file.routes.clone())
}

/// Recompile the table **already on disk** against the current registry.
///
/// Called whenever the set of backends changes — a registration, a probe, an enable, a
/// launch — because a selector that matched nothing a second ago may match now, and a
/// backend that just went away must stop being a candidate. Nothing is written.
///
/// # Errors
/// The compiler's report. The running table is left alone, which is the point.
pub fn recompile(state: &Arc<AppState>) -> std::result::Result<(), ValidationReport> {
    let file = state.store.load_routes().map_err(|e| {
        single_issue(
            "routes",
            format!("$STATE/routes.json could not be read: {e}"),
            "fix or delete the file; an absent file is a working zero-config install",
        )
    })?;
    let cfg = state.cfg.load_full();
    match TableBuilder::compile(&cfg, &file, state.router.registry()) {
        Ok(table) => {
            state.router.store_table(table);
            set_table_error(None);
            emit(
                state,
                Event::RouteTableChanged {
                    routes: file.routes,
                    valid: true,
                    error: None,
                },
            );
            Ok(())
        }
        Err(report) => {
            let msg = render_issues(&report);
            set_table_error(Some(msg.clone()));
            emit(
                state,
                Event::RouteTableChanged {
                    routes: file.routes,
                    valid: false,
                    error: Some(msg),
                },
            );
            Err(report)
        }
    }
}

/// Put a backend into the live registry, recompile, and tell everybody.
///
/// A failed recompile is logged and broadcast, never propagated: the backend is real and
/// already registered, and refusing to acknowledge it because an unrelated alias is
/// misconfigured would hide the thing the operator just started.
pub fn register_backend(state: &Arc<AppState>, b: Backend) {
    let cfg = state.cfg.load_full();
    state.router.registry().upsert(b.clone(), &cfg.router);
    if let Err(report) = recompile(state) {
        tracing::warn!(
            issues = %render_issues(&report),
            "backend registered, but the routing table did not recompile"
        );
    }
    emit(
        state,
        Event::BackendChanged {
            backend: Box::new(b),
        },
    );
}

/// Put a **freshly started** backend into the registry, armed.
///
/// The difference from [`register_backend`] is one flag, and it is load-bearing.
/// `BackendRegistry::upsert` deliberately preserves live state across a re-registration — the
/// permit pool, the breaker, the drain flag — because a table recompile must be invisible to
/// the request path. A *new process* under an old id is the one case where that is wrong: the
/// `accepting = false` left behind by the drain that stopped the previous process belongs to
/// a corpse, and inheriting it gives you a backend that is `Ready`, in the table, and never
/// dispatched to. That is the same invisible-outage shape FIX-5 exists to close, so a start
/// says so explicitly instead of hoping nothing drained the id earlier.
pub fn register_started(state: &Arc<AppState>, b: Backend) {
    let enabled = b.enabled;
    let id = b.id.clone();
    register_backend(state, b);
    if let Some(live) = state.router.registry().get(&id) {
        live.accepting.store(enabled, Ordering::Release);
    }
}

/// Persist the backends that have no lifecycle — nodes and managed providers.
///
/// Endpoint-backed backends are **not** written here: `$STATE/endpoints/<id>.json` already
/// owns them, and a second copy would be a second truth.
///
/// # Errors
/// Anything the atomic write can fail with.
pub fn persist_manual_backends(state: &Arc<AppState>) -> Result<(), ApiError> {
    let all: Vec<Backend> = state
        .router
        .registry()
        .snapshot()
        .into_iter()
        .filter(|b| b.endpoint.is_none())
        .collect();
    state.store.save_backends(&all).map_err(ApiError::from)
}

/// Point `alias` at exactly one backend, replacing whatever it pointed at.
///
/// The route that results is the simplest one that can exist — one target, `FirstHealthy`,
/// no filter, the default retry policy — because this is the "`apexrouter up --alias auto`"
/// path, and a route the user did not write should not carry policy the user did not choose.
/// An alias that already has a hand-written route keeps that route's strategy, filter and
/// retry block; only its targets are replaced.
///
/// # Errors
/// The compiler's report, unchanged, so the caller can render exactly what was wrong.
pub fn bind_alias(
    state: &Arc<AppState>,
    alias: &Alias,
    backend: &BackendId,
) -> std::result::Result<Vec<ModelRoute>, ValidationReport> {
    let mut file = state.store.load_routes().map_err(|e| {
        single_issue(
            "routes",
            format!("$STATE/routes.json could not be read: {e}"),
            "fix or delete the file",
        )
    })?;
    let target = RouteTarget {
        backend: BackendSelector::Id(backend.clone()),
        model: None,
        weight: 1,
    };
    let is_default = alias == &file.default_alias;
    match file.routes.iter_mut().find(|r| &r.alias == alias) {
        Some(existing) => existing.targets = vec![target],
        None => file.routes.push(ModelRoute {
            alias: alias.clone(),
            targets: vec![target],
            strategy: Strategy::FirstHealthy,
            filter: RouteFilter::default(),
            retry: RetryPolicy::default(),
            is_default,
            description: Some("bound by `apexrouter up --alias`".to_owned()),
        }),
    }
    apply_routes(state, &file)
}

// ----------------------------------------------------------------------------------------
// shutdown
// ----------------------------------------------------------------------------------------

/// Whether [`request_shutdown`] has been called.
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// The notifier S-01's signal loop selects on alongside `SIGINT`/`SIGTERM`.
///
/// Process-global rather than a field on `AppState`, because `AppState` is S-01's file and
/// the plan publishes no shutdown channel. One daemon per process, so a global is exactly as
/// scoped as the thing it names.
pub fn shutdown_notify() -> Arc<Notify> {
    static N: OnceLock<Arc<Notify>> = OnceLock::new();
    Arc::clone(N.get_or_init(|| Arc::new(Notify::new())))
}

/// Ask the process to drain and exit. Idempotent.
pub fn request_shutdown() {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
    shutdown_notify().notify_waiters();
}

/// Whether a graceful shutdown has been asked for.
pub fn shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::SeqCst)
}

// ----------------------------------------------------------------------------------------
// test scaffolding, shared by every module in this directory
// ----------------------------------------------------------------------------------------

/// A whole `AppState` on a private tempdir, plus the helpers every S-03 test needs.
///
/// Lives inside `mod.rs` rather than in a `testkit.rs`, because §5 of `BUILD-PLAN.md` gives
/// S-03 five files by name and a sixth would be a file nobody owns.
#[cfg(test)]
pub(crate) mod testkit {
    use super::*;
    use apexrouter_core::checks::Registry;
    use apexrouter_core::config::{Config, ProviderCfg};
    use apexrouter_core::lockfile::DaemonLock;
    use apexrouter_core::paths::Paths;
    use apexrouter_core::store::Store;
    use apexrouter_core::usage::UsageWriter;
    use apexrouter_protocol::{
        BackendKind, BackendLimits, CredentialSource, Health, Protocol, Provenance, UpstreamModel,
    };
    use apexrouter_providers::local::LocalProvisioner;
    use arc_swap::ArcSwap;
    use std::net::SocketAddr;
    use std::sync::Mutex as StdMutex;
    use std::time::Instant;
    use tokio::sync::{broadcast, Mutex};

    /// `Paths::resolve()` reads the process environment, which is global. One mutex around
    /// "set `$APEXROUTER_HOME`, resolve, ensure the layout" makes a per-test state root safe
    /// under `cargo test`'s thread pool; the resolved `Paths` is a value, so nothing later
    /// re-reads the variable.
    fn env_lock() -> &'static StdMutex<()> {
        static L: OnceLock<StdMutex<()>> = OnceLock::new();
        L.get_or_init(|| StdMutex::new(()))
    }

    /// A fresh, private state root. Leaked on purpose: it must outlive the test's `AppState`.
    pub(crate) fn fresh_paths() -> Paths {
        let guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::TempDir::new().expect("tempdir");
        let root = dir.keep();
        std::env::set_var("APEXROUTER_HOME", &root);
        let p = Paths::resolve().expect("paths");
        p.ensure_layout().expect("layout");
        drop(guard);
        p
    }

    /// The **hermetic** config every test in this crate uses.
    ///
    /// `[providers.together] base_url` points at a closed loopback port, exactly as the
    /// router crate's `test_config()` does: `GET /providers` and the provider probes resolve
    /// the real credential chain, which on a developer's machine finds `$TOGETHER_API_KEY`.
    /// No test may connect anywhere but `127.0.0.x`.
    pub(crate) fn test_config() -> Config {
        let mut cfg = Config::default();
        cfg.compat.mirror_usage_log = false;
        cfg.compat.read_legacy_state = false;
        cfg.router.log_usage = false;
        // Discovery roots are emptied so a rig scan is a no-op: no test in this unit needs
        // the machine's real builds or weights, and a test whose result depends on what is
        // installed under `~/llama.cpp` is not a test.
        cfg.endpoints.build_roots = vec![];
        cfg.endpoints.model_roots = vec![];
        cfg.providers.insert(
            "together".to_owned(),
            ProviderCfg {
                base_url: "http://127.0.0.1:1/v1".to_owned(),
                api_key_env: None,
                api_key_file: None,
            },
        );
        cfg
    }

    /// A whole daemon's worth of state on a private tempdir, minus the listeners.
    pub(crate) fn app(cfg: Config) -> Arc<AppState> {
        let paths = fresh_paths();
        let cfg = Arc::new(cfg);
        let (tx, _rx) = broadcast::channel(256);
        let usage = UsageWriter::open(&paths, &cfg.compat).expect("usage writer");
        let router = apexrouter_router::RouterInner::new(Arc::clone(&cfg), tx.clone(), usage);
        let supervisor = Arc::new(LocalProvisioner::new(
            paths.clone(),
            (*cfg).clone(),
            tx.clone(),
        ));
        let lock = DaemonLock::acquire(&paths).expect("daemon lock");
        let telemetry = Arc::new(apexrouter_router::Telemetry::new(tx.clone(), 1_000));
        Arc::new(AppState {
            store: Store::new(paths.clone()),
            paths,
            cfg: ArcSwap::new(cfg),
            router,
            tx,
            supervisor,
            jobs: crate::jobs::JobRegistry::new(),
            checks: Arc::new(Registry::new()),
            started_at: Instant::now(),
            lock: Arc::new(Mutex::new(lock)),
            fleet: std::sync::RwLock::new(crate::state::FleetCache::default()),
            telemetry,
        })
    }

    /// The S-03 control plane, bound to an ephemeral loopback port. Returns the base URL.
    ///
    /// A real listener rather than `tower::ServiceExt::oneshot`, because the acceptance this
    /// unit is on is "works against a real llama-server", and because `tower`'s `util`
    /// feature is not enabled for this crate. Nothing but `127.0.0.1` is ever contacted.
    pub(crate) async fn serve_api(state: Arc<AppState>) -> String {
        let app = router().with_state(state);
        let listener = tokio::net::TcpListener::bind::<SocketAddr>(
            "127.0.0.1:0".parse().expect("loopback addr"),
        )
        .await
        .expect("bind");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    /// A `Ready` node backend pointing at `base_url`, advertising one model.
    pub(crate) fn node_backend(id: &str, base_url: &str, model: &str) -> Backend {
        Backend {
            id: BackendId::parse(id).expect("id"),
            kind: BackendKind::Node,
            protocol: Protocol::OpenAi,
            label: id.to_owned(),
            base_url: base_url.to_owned(),
            credential: CredentialSource::None,
            tags: vec!["test".to_owned()],
            models: vec![UpstreamModel {
                id: model.to_owned(),
                ctx: Some(4096),
                vision: false,
                tools: true,
            }],
            limits: BackendLimits {
                max_concurrent: 4,
                queue_depth: 8,
                ctx: Some(4096),
                slots_total: Some(4),
            },
            price: None,
            health: Health::Ready {
                since_unix: 0,
                slots_busy: 0,
                slots_total: 4,
                tps_p50: None,
            },
            provenance: Provenance::Manual,
            endpoint: None,
            enabled: true,
            devices: vec![],
            last_error: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::*;
    use super::*;

    #[tokio::test]
    async fn an_api_error_serialises_as_an_error_envelope() {
        let e = ApiError::not_found("endpoint local-carnice").with_param("id");
        assert_eq!(e.status, StatusCode::NOT_FOUND);
        let s = serde_json::to_string(&ErrorEnvelope {
            error: e.body.clone(),
        })
        .expect("ser");
        assert!(s.contains("\"kind\":\"not_found\""), "{s}");
        assert!(s.contains("\"param\":\"id\""), "{s}");
    }

    #[tokio::test]
    async fn core_errors_map_to_actionable_statuses() {
        let cases: Vec<(CoreError, StatusCode, &str)> = vec![
            (
                CoreError::NotFound("x".into()),
                StatusCode::NOT_FOUND,
                "not_found",
            ),
            (
                CoreError::Conflict("x".into()),
                StatusCode::CONFLICT,
                "conflict",
            ),
            (
                CoreError::PortInUse {
                    port: 8100,
                    held_by: None,
                },
                StatusCode::CONFLICT,
                "port_in_use",
            ),
            (
                CoreError::InsufficientVram {
                    need_mb: 1,
                    free_mb: 0,
                },
                StatusCode::CONFLICT,
                "insufficient_vram",
            ),
            (
                CoreError::Invalid {
                    what: "a".into(),
                    why: "b".into(),
                },
                StatusCode::BAD_REQUEST,
                "invalid",
            ),
        ];
        for (e, status, kind) in cases {
            let mapped = ApiError::from(e);
            assert_eq!(mapped.status, status);
            assert_eq!(mapped.body.kind, kind);
        }
    }

    /// The whole point of `bind_alias`: `apexrouter up --alias auto` must leave a table in
    /// which `auto` resolves to the backend that was just started.
    #[tokio::test]
    async fn bind_alias_creates_then_repoints_a_route() {
        let state = app(test_config());
        let cfg = state.cfg.load_full();
        state
            .router
            .registry()
            .upsert(node_backend("a", "http://127.0.0.1:9", "m"), &cfg.router);
        state
            .router
            .registry()
            .upsert(node_backend("b", "http://127.0.0.1:9", "n"), &cfg.router);

        let alias = Alias::parse("auto").expect("alias");
        let a = BackendId::parse("a").expect("id");
        let b = BackendId::parse("b").expect("id");

        let routes = bind_alias(&state, &alias, &a).expect("bind a");
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].targets[0].backend, BackendSelector::Id(a));
        assert!(routes[0].is_default, "auto is the default alias");

        let routes = bind_alias(&state, &alias, &b).expect("bind b");
        assert_eq!(routes.len(), 1, "repointed, not appended");
        assert_eq!(routes[0].targets[0].backend, BackendSelector::Id(b));

        // and it survives a round trip through disk
        let on_disk = state.store.load_routes().expect("load");
        assert_eq!(on_disk.routes.len(), 1);
    }

    #[tokio::test]
    async fn a_table_that_does_not_compile_is_never_persisted() {
        let state = app(test_config());
        let file = RouteFile {
            schema_version: 1,
            default_alias: Alias::parse("auto").expect("alias"),
            routes: vec![ModelRoute {
                alias: Alias::parse("auto").expect("alias"),
                targets: vec![RouteTarget {
                    backend: BackendSelector::Id(BackendId::parse("nope").expect("id")),
                    model: None,
                    weight: 1,
                }],
                strategy: Strategy::FirstHealthy,
                filter: RouteFilter::default(),
                retry: RetryPolicy::default(),
                is_default: true,
                description: None,
            }],
        };
        let report = apply_routes(&state, &file).expect_err("dangling target must be refused");
        assert!(!report.ok);
        assert!(
            state.store.load_routes().expect("load").routes.is_empty(),
            "a refused table must not reach disk"
        );
    }

    #[tokio::test]
    async fn shutdown_is_idempotent_and_observable() {
        let n = shutdown_notify();
        let waiter = tokio::spawn(async move { n.notified().await });
        // Give the waiter a moment to register before notifying.
        tokio::time::sleep(Duration::from_millis(50)).await;
        request_shutdown();
        request_shutdown();
        assert!(shutdown_requested());
        tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("notified")
            .expect("join");
    }
}
