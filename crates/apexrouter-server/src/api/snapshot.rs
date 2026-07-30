//! OWNER: unit S-03 (server/src/api/{mod,snapshot,backends,routes,endpoints}.rs). Do not edit outside that unit.
//!
//! `GET /v1/snapshot`, `POST /v1/reload`, `POST /v1/shutdown`.
//!
//! [`build`] is the one place a [`Snapshot`] is assembled, and it is `pub` because S-05's
//! `/ws` handler must send exactly the same picture on connect and after a
//! `RecvError::Lagged`. Two assemblers would eventually disagree, and a dashboard that
//! disagrees with the REST answer is worse than one that is merely stale.
//!
//! Nothing in here probes an upstream. A snapshot is a **read of state we already hold**:
//! the registry, the store, the supervisor's cached rig, and the usage log. `GET
//! /v1/snapshot` on a laptop with the network unplugged is instant and cannot fail.

use super::{emit, now_unix, table_error, ApiError, ApiResult};
use crate::state::AppState;
use apexrouter_core::config::Config;
use apexrouter_core::secret;
use apexrouter_core::usage::{self, GroupBy};
use apexrouter_protocol::{
    Alert, AlertLevel, CredentialSource, Event, Money, ProviderId, ProviderStatus, ProxyStatus,
    ServedBy, Snapshot, Totals, UsageRecord, ValidationReport, PRODUCT, VERSION,
};
use axum::extract::State;
use axum::routing::{get, post};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// One day, in seconds.
const DAY: i64 = 86_400;
/// The window `req_per_min` and `tok_per_s` are measured over.
const RATE_WINDOW: i64 = 60;

/// `/v1/snapshot`, `/v1/reload`, `/v1/shutdown`.
pub fn router() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route("/v1/snapshot", get(get_snapshot))
        .route("/v1/reload", post(post_reload))
        .route("/v1/shutdown", post(post_shutdown))
}

/// `GET /v1/snapshot` — everything every surface renders, in one round trip.
pub async fn get_snapshot(State(s): State<Arc<AppState>>) -> ApiResult<Snapshot> {
    Ok(Json(build(&s).await))
}

/// `POST /v1/reload` — reparse `config.toml` and `routes.json`.
///
/// **A failed compile keeps the running table.** The report comes back with `ok: false` and
/// the issues; the daemon is still serving whatever it was serving, which is exactly what
/// `ARCHITECTURE.md` §4.1 asks for. A config file that does not *parse* is a `400`, because
/// unlike a bad route table there is nothing partial to accept.
pub async fn post_reload(State(s): State<Arc<AppState>>) -> ApiResult<ValidationReport> {
    reload(&s).map(Json)
}

/// The acknowledgement `POST /v1/shutdown` returns before the process starts draining.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShutdownAck {
    /// Always true; the refusal path is an `ErrorEnvelope`.
    pub ok: bool,
    /// How long in-flight requests get, from `[server] drain_timeout_secs`.
    pub drain_timeout_secs: u64,
    /// What is *not* going to happen, spelled out because it is the surprising part.
    pub message: String,
}

/// `POST /v1/shutdown` — graceful, admin scope (enforced by S-02's `required_scope`).
///
/// Returns **before** the drain, because the response has to survive the listener it is
/// riding on being closed. The child `llama-server` processes are deliberately untouched:
/// they are `setsid`, `kill_children_on_exit` defaults to false, and "`endpoint ls` after a
/// restart still says `Ready`" is a Stage 4 acceptance line.
pub async fn post_shutdown(State(s): State<Arc<AppState>>) -> ApiResult<ShutdownAck> {
    let cfg = s.cfg.load_full();
    super::request_shutdown();
    emit(
        &s,
        Event::Alert {
            level: AlertLevel::Info,
            message: "shutdown requested through the control plane".to_owned(),
            action: None,
            id: "daemon.shutdown".to_owned(),
        },
    );
    Ok(Json(ShutdownAck {
        ok: true,
        drain_timeout_secs: cfg.server.drain_timeout_secs,
        message: "draining; llama-server children are left running".to_owned(),
    }))
}

/// Reparse config and routes and re-arm the table. Shared with `SIGHUP` (S-01) and the
/// config watcher (S-05), so there is exactly one reload path.
///
/// # Errors
/// [`ApiError`] `400 invalid_config` when `config.toml` exists but does not parse. A routing
/// table that does not compile is **not** an error: it comes back as a report with
/// `ok: false`, and the previous table keeps serving.
pub fn reload(state: &Arc<AppState>) -> Result<ValidationReport, ApiError> {
    let path = state.paths.config_file();
    let cfg = Config::load_from(Some(&path), None).map_err(|e| {
        ApiError::bad_request(
            "invalid_config",
            format!("{} did not parse: {e}", path.display()),
        )
    })?;
    state.cfg.store(Arc::new(cfg.clone()));
    state.supervisor.set_config(cfg);

    match super::recompile(state) {
        Ok(()) => Ok(ValidationReport {
            ok: true,
            issues: vec![],
        }),
        Err(report) => Ok(report),
    }
}

/// Assemble the whole picture. Never fails: an unreadable part is an empty part.
///
/// The only `await` is the supervisor's rig accessor, which serves a 60 s cache; a cold
/// cache means one `--list-devices` per discovered build, which is also what `GET /v1/rig`
/// costs and is why both go through the same cache.
pub async fn build(state: &Arc<AppState>) -> Snapshot {
    let cfg = state.cfg.load_full();
    let table = state.router.table();
    let rows = usage::read_all(&state.paths, &cfg.compat).unwrap_or_default();
    let now = now_unix();
    let (req_per_min, tok_per_s) = rate(&rows, now);
    let catalog = apexrouter_core::catalog::load(&state.paths).unwrap_or_default();
    let err = table_error();

    Snapshot {
        product: PRODUCT.to_owned(),
        version: VERSION.to_owned(),
        served_by: ServedBy::Daemon,
        as_of_unix: now,
        stale: false,
        proxy: ProxyStatus {
            base_url: format!("http://{}", cfg.proxy_bind()),
            control_url: format!("http://{}", cfg.control_bind()),
            uptime_secs: state.started_at.elapsed().as_secs_f64(),
            inflight: inflight(state),
            req_per_min,
            tok_per_s,
            default_alias: table.default_alias().clone(),
            table_valid: err.is_none(),
            table_error: err,
        },
        backends: state.router.registry().snapshot(),
        routes: state
            .store
            .load_routes()
            .map(|f| f.routes)
            .unwrap_or_default(),
        endpoints: state.store.list_endpoints().unwrap_or_default(),
        rig: state.supervisor.rig().await.unwrap_or_default(),
        // Stage 5 fills this in: the rented fleet and the approvals queue.
        instances: vec![],
        tunnels: state.store.load_tunnels().unwrap_or_default(),
        providers: providers(&cfg, state),
        recipes: catalog.recipes,
        profiles: catalog.profiles,
        totals: totals(&rows, now),
        alerts: alerts(state),
        jobs: state.jobs.all(),
    }
}

/// Requests in flight, summed over the registry's **own** counters.
///
/// Not `/slots`: llama.cpp answers `501` there on `--no-slots` builds, and a number that
/// silently becomes zero is worse than no number.
fn inflight(state: &Arc<AppState>) -> u32 {
    state
        .router
        .registry()
        .all()
        .iter()
        .map(|b| b.inflight.load(Ordering::Relaxed))
        .fold(0u32, u32::saturating_add)
}

/// `(requests per minute, tokens per second)` over the last [`RATE_WINDOW`] seconds.
///
/// Derived from the usage log rather than from a counter, so the number survives a daemon
/// restart and agrees with `apexrouter usage`. With `[router] log_usage = false` both are
/// zero, which is honest: nothing is being recorded.
fn rate(rows: &[UsageRecord], now: i64) -> (f32, f32) {
    let window = usage::aggregate(rows, Some(now - RATE_WINDOW), GroupBy::Provider);
    let per_min = window.rows as f32 * (60.0 / RATE_WINDOW as f32);
    let tps = window.total_completion as f32 / RATE_WINDOW as f32;
    (per_min, tps)
}

/// Spend and tokens, folded with `CostEstimate::add` inside `aggregate`, so one estimated
/// row visibly demotes the total instead of the total quietly claiming to be metered.
fn totals(rows: &[UsageRecord], now: i64) -> Totals {
    let d1 = usage::aggregate(rows, Some(now - DAY), GroupBy::Provider);
    let d7 = usage::aggregate(rows, Some(now - 7 * DAY), GroupBy::Provider);
    Totals {
        spend_24h: d1.total_cost,
        spend_7d: d7.total_cost,
        tokens_24h: d1.total_prompt.saturating_add(d1.total_completion),
        // Stage 5: credit and burn come from the vast ledger.
        vast_credit: None,
        burn_rate_usd_hr: Money::ZERO,
        burn_down_hours: None,
    }
}

/// Managed providers, **without probing them**.
///
/// `credential_present` walks the full chain (`credentials.toml` → `api_key_file` → the
/// legacy `~/.vastai-gguf/config.toml` → the env var), which is the documented
/// inconsistency `ARCHITECTURE.md` §6.1 fixes for `/providers`; doing it here as well means
/// the dashboard and the legacy endpoint cannot disagree. No socket is opened.
fn providers(cfg: &Config, state: &Arc<AppState>) -> Vec<ProviderStatus> {
    let registry = state.router.registry().snapshot();
    let mut out = Vec::with_capacity(cfg.providers.len());
    for (id, pcfg) in &cfg.providers {
        let pid = match ProviderId::parse(id) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let resolved = secret::resolve_provider(cfg, &state.paths, &pid)
            .ok()
            .flatten();
        let models_cached: u32 = registry
            .iter()
            .filter(|b| b.id.as_str() == pid.as_str())
            .map(|b| u32::try_from(b.models.len()).unwrap_or(u32::MAX))
            .sum();
        out.push(ProviderStatus {
            id: pid,
            base_url: pcfg.base_url.clone(),
            credential: resolved
                .as_ref()
                .map_or(CredentialSource::None, |r| r.source.clone()),
            credential_present: resolved.is_some(),
            models_cached,
            last_ok_unix: None,
            last_error: None,
            // Rate-limit headers are read by the provider clients (S-07/P-06), not by a
            // snapshot that opens no sockets.
            rate_limit: None,
        });
    }
    out
}

/// Standing alerts derived from state we already hold.
///
/// mk1 has no alert store — alerts are broadcast as they happen — so the snapshot
/// reconstructs the ones that are *standing conditions* rather than events: the routing
/// table on disk does not compile, and a backend is carrying a last error.
fn alerts(state: &Arc<AppState>) -> Vec<Alert> {
    let now = now_unix();
    let mut out = Vec::new();
    if let Some(err) = table_error() {
        out.push(Alert {
            id: "routes.compile".to_owned(),
            level: AlertLevel::Serious,
            message: format!("the routing table on disk does not compile: {err}"),
            action: Some("apexrouter route validate".to_owned()),
            at_unix: now,
        });
    }
    for b in state.router.registry().snapshot() {
        if let Some(err) = b.last_error.as_deref() {
            out.push(Alert {
                id: format!("backend.error.{}", b.id),
                level: AlertLevel::Warning,
                message: format!("{}: {err}", b.id),
                action: Some(format!("apexrouter backend probe {}", b.id)),
                at_unix: now,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;
    use apexrouter_protocol::{
        Alias, BackendId, BackendSelector, ModelRoute, RouteFile, RouteTarget, Strategy,
    };

    #[tokio::test]
    async fn snapshot_is_a_complete_protocol_type_on_a_bare_machine() {
        let state = app(test_config());
        let base = serve_api(Arc::clone(&state)).await;
        let body = reqwest::get(format!("{base}/v1/snapshot"))
            .await
            .expect("get")
            .text()
            .await
            .expect("text");
        let snap: Snapshot = serde_json::from_str(&body).expect("Snapshot deserialises");
        assert_eq!(snap.product, PRODUCT);
        assert_eq!(snap.version, VERSION);
        assert_eq!(snap.served_by, ServedBy::Daemon);
        assert!(!snap.stale);
        assert_eq!(snap.proxy.default_alias.as_str(), "auto");
        assert!(snap.proxy.base_url.starts_with("http://"));
        assert!(snap.backends.is_empty());
        assert!(snap.endpoints.is_empty());
    }

    #[tokio::test]
    async fn a_registered_backend_shows_up_in_the_snapshot() {
        let state = app(test_config());
        super::super::register_backend(&state, node_backend("n1", "http://127.0.0.1:9", "m"));
        let snap = build(&state).await;
        assert_eq!(snap.backends.len(), 1);
        assert_eq!(snap.backends[0].id.as_str(), "n1");
    }

    #[tokio::test]
    async fn reload_keeps_the_old_table_when_the_new_one_does_not_compile() {
        let state = app(test_config());
        let cfg = state.cfg.load_full();
        state
            .router
            .registry()
            .upsert(node_backend("a", "http://127.0.0.1:9", "m"), &cfg.router);
        let alias = Alias::parse("auto").expect("alias");
        super::super::bind_alias(&state, &alias, &BackendId::parse("a").expect("id"))
            .expect("bind");
        let good_generation = state.router.table().generation();

        // A table naming a backend that does not exist, straight to disk, the way a hand
        // edit or a botched migration would write one.
        let bad = RouteFile {
            schema_version: 1,
            default_alias: alias.clone(),
            routes: vec![ModelRoute {
                alias: alias.clone(),
                targets: vec![RouteTarget {
                    backend: BackendSelector::Id(BackendId::parse("ghost").expect("id")),
                    model: None,
                    weight: 1,
                }],
                strategy: Strategy::FirstHealthy,
                filter: Default::default(),
                retry: Default::default(),
                is_default: true,
                description: None,
            }],
        };
        state.store.save_routes(&bad).expect("save");

        let report = reload(&state).expect("reload does not fail on a bad table");
        assert!(!report.ok, "the report must say what is wrong");
        assert!(!report.issues.is_empty());
        assert_eq!(
            state.router.table().generation(),
            good_generation,
            "the previous table is still serving"
        );
    }

    #[tokio::test]
    async fn shutdown_returns_before_it_drains() {
        let state = app(test_config());
        let ack = post_shutdown(State(Arc::clone(&state))).await.expect("ack");
        assert!(ack.0.ok);
        assert!(super::super::shutdown_requested());
    }
}
