//! OWNER: unit S-05 (server/src/{ws,assets,prober,watcher}.rs). Do not edit outside that
//! unit.
//!
//! The health prober. Sizes each backend's semaphore from `/props.total_slots`, falling back
//! to `/slots` length, falling back to config — which is why the argv builder passes
//! `--props`, `--metrics` and `--slots` to every server we launch, feature-detected.
//! Otherwise the sizing would read an endpoint we never enabled.
//!
//! It requires N consecutive failures before `Degraded`, and it maintains the
//! `model_index` that `resolve()` rule 3 reads.
//!
//! # Why the failure threshold exists
//!
//! `Health::Ready` is the **only** routable state (`Health::is_routable`), so a prober that
//! demoted on the first missed probe would take a whole backend out of rotation for one
//! dropped packet, and put it back three seconds later — flapping a route on and off while
//! the breaker, which is the thing actually designed to react per request, has not even
//! reached its `min_volume`. [`FAILURES_BEFORE_DEGRADED`] consecutive failures is the point
//! at which "it went away" is a better explanation than "one probe was unlucky".
//!
//! # Why the model index is followed by a recompile
//!
//! `resolve()` rule 3 reads `RoutingTable::by_upstream_id`, which `TableBuilder::compile`
//! builds out of each `LiveBackend::model_index`. Calling `set_models` alone therefore
//! changes nothing a request can observe until the next compile, so a changed index asks for
//! one. This is the path by which `curl -d '{"model":"Carnice-9b-Q6_K"}'` starts working
//! seconds after a server finishes loading, with nobody editing a route.

use crate::state::AppState;
use apexrouter_core::config::{Config, RouterCfg};
use apexrouter_core::secret::{resolve_credential, Secret};
use apexrouter_core::upstream::{self, UpstreamProbe};
use apexrouter_protocol::{
    Backend, BackendId, BootPhase, CredentialSource, Event, Health, UpstreamModel,
};
use apexrouter_router::LiveBackend;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// Consecutive failed probes before a backend that *was* answering is demoted.
pub const FAILURES_BEFORE_DEGRADED: u32 = 3;

/// Consecutive failed probes before it is called `Down` rather than `Degraded`.
pub const FAILURES_BEFORE_DOWN: u32 = 6;

/// Floor on the probe interval, so a hand-edited `health_interval_ms = 1` cannot turn the
/// prober into a load generator against the very server it is watching.
const MIN_INTERVAL: Duration = Duration::from_millis(250);

/// Ceiling on how long one probe may take. Longer than this and the answer is stale before
/// it lands; `probe()` treats it as the budget for the whole round trip.
const MAX_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Floor on the same.
const MIN_PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// Run for the daemon's lifetime, probing every enabled backend on an interval.
pub async fn health_prober(state: Arc<AppState>) {
    let http = client();
    let mut failures: HashMap<BackendId, u32> = HashMap::new();
    loop {
        let interval = probe_interval(&state.cfg.load());
        probe_round(&state, &http, &mut failures).await;
        tokio::time::sleep(interval).await;
    }
}

/// The prober's own HTTP client.
///
/// Separate from the router's, which is deliberately `no_gzip`/`no_brotli`/`no_deflate`
/// because it relays bytes verbatim; nothing here relays anything, it parses small JSON
/// documents. Pooled, because unlike the supervisor's boot gate this talks to servers that
/// are already up and will be probed again in three seconds.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(MIN_PROBE_TIMEOUT)
        .pool_max_idle_per_host(2)
        .build()
        .unwrap_or_default()
}

/// `[supervisor] health_interval_ms`, floored.
fn probe_interval(cfg: &Config) -> Duration {
    Duration::from_millis(cfg.supervisor.health_interval_ms).max(MIN_INTERVAL)
}

/// How long one probe gets: the interval, clamped, so a probe never outlives its round.
fn probe_timeout(cfg: &Config) -> Duration {
    probe_interval(cfg).clamp(MIN_PROBE_TIMEOUT, MAX_PROBE_TIMEOUT)
}

/// One pass over every enabled backend. Public to the crate so a test can drive it without
/// waiting on a timer, and so `POST /v1/backends/{id}/probe` could share it later.
pub(crate) async fn probe_round(
    state: &Arc<AppState>,
    http: &reqwest::Client,
    failures: &mut HashMap<BackendId, u32>,
) {
    let cfg = state.cfg.load_full();
    let timeout = probe_timeout(&cfg);
    let live = state.router.registry().all();

    // Credentials are resolved up front, synchronously: the chain reads files and the
    // environment, and doing it inside the concurrent futures would put blocking reads on
    // the runtime for no benefit — there are single digits of backends.
    let mut work = Vec::with_capacity(live.len());
    for backend in &live {
        let meta = backend.meta.load_full();
        if !meta.enabled {
            continue;
        }
        let cred = credential_for(&meta);
        let base = meta.base_url.clone();
        work.push(async move {
            let probe = upstream::probe(http, &base, cred.as_ref(), timeout).await;
            (Arc::clone(backend), probe)
        });
    }

    let results = futures_util::future::join_all(work).await;

    // Forget the counters of backends that are no longer registered, so a long-lived daemon
    // that starts and stops endpoints all day does not grow this map without bound.
    let known: std::collections::HashSet<BackendId> = live.iter().map(|b| b.id.clone()).collect();
    failures.retain(|id, _| known.contains(id));

    let mut models_changed = false;
    for (backend, probe) in results {
        models_changed |= apply(state, &cfg.router, &backend, &probe, failures);
    }

    if models_changed {
        // Rule 3 lives in the compiled table, not in `model_index` directly.
        if let Err(report) = crate::api::recompile(state) {
            tracing::warn!(
                issues = %crate::api::render_issues(&report),
                "the model index changed but the routing table did not recompile"
            );
        }
    }
}

/// Fold one probe into one backend. Returns true when the model index changed.
fn apply(
    state: &Arc<AppState>,
    cfg: &RouterCfg,
    backend: &Arc<LiveBackend>,
    probe: &UpstreamProbe,
    failures: &mut HashMap<BackendId, u32>,
) -> bool {
    let previous = backend.meta.load_full();
    let answered = probe.healthy || probe.loading;
    let consecutive = {
        let slot = failures.entry(backend.id.clone()).or_insert(0);
        *slot = if answered { 0 } else { slot.saturating_add(1) };
        *slot
    };

    // ---- concurrency ---------------------------------------------------------------------
    // `/props.total_slots` → `/slots` length (both already resolved by `probe()`) → what
    // config asked for → the global cap. Never zero: a zero-permit backend stalls forever.
    let slots_total = probe
        .slots_total
        .filter(|n| *n > 0)
        .or(previous.limits.slots_total.filter(|n| *n > 0))
        .or(Some(previous.limits.max_concurrent).filter(|n| *n > 0))
        .unwrap_or_else(|| cfg.max_inflight.max(1));
    if probe.healthy {
        backend.resize_semaphore(slots_total);
    }

    // ---- the model index -----------------------------------------------------------------
    let mut models_changed = false;
    if probe.healthy && !probe.models.is_empty() {
        let ids: Vec<String> = probe.models.iter().map(|m| m.id.clone()).collect();
        if backend.model_index.load().as_slice() != ids.as_slice() {
            backend.set_models(ids);
            models_changed = true;
        }
    }

    // ---- the description ------------------------------------------------------------------
    let mut next = Backend::clone(&previous);
    next.health = next_health(&previous.health, probe, backend, consecutive, slots_total);
    if probe.healthy {
        next.limits.slots_total = Some(slots_total);
        next.limits.max_concurrent = slots_total;
        if let Some(ctx) = probe.ctx {
            next.limits.ctx = Some(ctx);
        }
        if !probe.models.is_empty() {
            next.models = merge_models(&previous.models, &probe.models);
        }
    }
    next.last_error = probe.error.clone().or(match &next.health {
        Health::Degraded { reason, .. } | Health::Down { reason, .. } => Some(reason.clone()),
        _ => None,
    });

    if next != *previous {
        backend.update_meta(next.clone());
        let _ = state.tx.send(Event::BackendChanged {
            backend: Box::new(next),
        });
    }
    models_changed
}

/// The state machine, written out so every transition is visible in one place.
fn next_health(
    previous: &Health,
    probe: &UpstreamProbe,
    backend: &Arc<LiveBackend>,
    consecutive: u32,
    slots_total: u32,
) -> Health {
    let now = chrono::Utc::now().timestamp();

    // Draining wins over everything: the operator asked for this backend to stop taking
    // work, and a successful probe must not quietly put it back in rotation.
    if !backend.accepting.load(Ordering::Relaxed) {
        return Health::Draining {
            in_flight: backend.inflight.load(Ordering::Relaxed),
        };
    }

    if probe.healthy {
        return Health::Ready {
            // Uptime is measured from when it *became* ready, so a healthy backend's
            // "up for 4 h" does not reset every three seconds.
            since_unix: match previous {
                Health::Ready { since_unix, .. } => *since_unix,
                _ => now,
            },
            slots_busy: probe.slots_busy.unwrap_or(0),
            slots_total,
            tps_p50: match previous {
                Health::Ready { tps_p50, .. } => *tps_p50,
                _ => None,
            },
        };
    }

    if probe.loading {
        return Health::Starting {
            phase: BootPhase::Loading { pct: None },
            since_unix: match previous {
                Health::Starting { since_unix, .. } => *since_unix,
                _ => now,
            },
            detail: probe.error.clone().or(Some("loading model".to_owned())),
        };
    }

    let reason = probe
        .error
        .clone()
        .or_else(|| probe.status.map(|s| format!("/health -> {s}")))
        .unwrap_or_else(|| "no answer".to_owned());

    if consecutive >= FAILURES_BEFORE_DOWN {
        return Health::Down {
            reason,
            // The breaker owns per-request admission; this is only what a UI counts down.
            retry_at_unix: now,
        };
    }
    if consecutive >= FAILURES_BEFORE_DEGRADED {
        return Health::Degraded {
            reason,
            consecutive_failures: consecutive,
        };
    }
    // Below the threshold nothing changes. One unlucky probe is not an outage, and
    // `Health::Ready` is the only routable state — demoting here would flap the route.
    match previous {
        // Except from `Unknown`: a backend that has never answered has nothing to preserve,
        // and reporting it as "not probed yet" forever would hide a dead upstream.
        Health::Unknown => Health::Starting {
            phase: BootPhase::Loading { pct: None },
            since_unix: now,
            detail: Some(reason),
        },
        other => other.clone(),
    }
}

/// Keep what the probe learned, without losing what we already knew.
///
/// The probe is authoritative about *which* models exist. It is not always authoritative
/// about their context length — Together reports it, llama.cpp reports `null` until the
/// model finishes loading — so a previously known `ctx` survives a probe that omits one.
fn merge_models(previous: &[UpstreamModel], probed: &[UpstreamModel]) -> Vec<UpstreamModel> {
    probed
        .iter()
        .map(|m| {
            let old = previous.iter().find(|p| p.id == m.id);
            UpstreamModel {
                id: m.id.clone(),
                ctx: m.ctx.or_else(|| old.and_then(|o| o.ctx)),
                vision: m.vision || old.is_some_and(|o| o.vision),
                tools: m.tools || old.is_some_and(|o| o.tools),
            }
        })
        .collect()
}

/// The credential this backend probes with, if any.
///
/// Only the two *described* sources are followed here. `Managed` and `Instance` name a store
/// this module has no handle on — `credentials.toml` is keyed by provider id, and a minted
/// per-instance key belongs to the endpoint record — so they are left to the provider
/// surfaces in Stage 5 rather than guessed at. A local `llama-server` is `None`, which is
/// the whole of mk1-core.
fn credential_for(b: &Backend) -> Option<Secret<String>> {
    let resolved = match &b.credential {
        CredentialSource::Env { var } => resolve_credential(None, None, &[], Some(var)),
        CredentialSource::File { path } => {
            resolve_credential(None, Some(Path::new(path)), &[], None)
        }
        CredentialSource::None | CredentialSource::Managed { .. } | CredentialSource::Instance => {
            return None
        }
    };
    match resolved {
        Ok(found) => found.map(|r| r.secret),
        Err(e) => {
            // Never the value, only that it could not be read.
            tracing::debug!(backend = %b.id, error = %e, "probe credential unavailable");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ws::testkit::{harness, Harness};
    use apexrouter_protocol::{BackendKind, BackendLimits, Protocol, Provenance};
    use serde_json::json;
    use wiremock::matchers::{method, path as p};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn backend(id: &str, base_url: &str) -> Backend {
        Backend {
            id: BackendId::parse(id).expect("id"),
            kind: BackendKind::LocalLlama,
            protocol: Protocol::OpenAi,
            label: id.to_owned(),
            base_url: base_url.to_owned(),
            credential: CredentialSource::None,
            tags: vec!["local".to_owned()],
            models: vec![],
            limits: BackendLimits {
                max_concurrent: 2,
                queue_depth: 8,
                ctx: None,
                slots_total: None,
            },
            price: None,
            health: Health::Unknown,
            provenance: Provenance::Spawned,
            endpoint: None,
            enabled: true,
            devices: vec![],
            last_error: None,
        }
    }

    fn register(h: &Harness, b: Backend) -> Arc<LiveBackend> {
        let cfg = h.state.cfg.load_full();
        h.state.router.registry().upsert(b, &cfg.router)
    }

    async fn round(h: &Harness, failures: &mut HashMap<BackendId, u32>) {
        let http = reqwest::Client::new();
        probe_round(&h.state, &http, failures).await;
    }

    /// `/health` 200, `/v1/models`, `/props` and `/slots` — a healthy llama.cpp.
    async fn healthy_server(total_slots: Option<u32>, slots: Option<usize>) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(p("/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "ok"})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(p("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object": "list",
                "data": [{"id": "Carnice-9b-Q6_K", "meta": {"n_ctx_train": 262144}}]
            })))
            .mount(&server)
            .await;
        match total_slots {
            Some(n) => {
                Mock::given(method("GET"))
                    .and(p("/props"))
                    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                        "total_slots": n,
                        "default_generation_settings": {"n_ctx": 32768}
                    })))
                    .mount(&server)
                    .await;
            }
            None => {
                // What a build without `--props` answers.
                Mock::given(method("GET"))
                    .and(p("/props"))
                    .respond_with(ResponseTemplate::new(404))
                    .mount(&server)
                    .await;
            }
        }
        match slots {
            Some(n) => {
                let body: Vec<serde_json::Value> = (0..n)
                    .map(|i| json!({"id": i, "is_processing": false}))
                    .collect();
                Mock::given(method("GET"))
                    .and(p("/slots"))
                    .respond_with(ResponseTemplate::new(200).set_body_json(body))
                    .mount(&server)
                    .await;
            }
            None => {
                // What `--no-slots` answers. Not an error.
                Mock::given(method("GET"))
                    .and(p("/slots"))
                    .respond_with(ResponseTemplate::new(501))
                    .mount(&server)
                    .await;
            }
        }
        server
    }

    #[tokio::test]
    async fn the_semaphore_is_sized_from_props_total_slots() {
        let h = harness();
        let server = healthy_server(Some(4), Some(1)).await;
        let live = register(&h, backend("local-a", &server.uri()));
        assert_eq!(live.sem.available_permits(), 2, "config, before any probe");

        let mut failures = HashMap::new();
        round(&h, &mut failures).await;

        assert_eq!(
            live.sem.available_permits(),
            4,
            "/props.total_slots wins over both /slots and config"
        );
        assert!(matches!(
            live.meta.load().health,
            Health::Ready { slots_total: 4, .. }
        ));
        assert_eq!(live.meta.load().limits.ctx, Some(32768));
    }

    #[tokio::test]
    async fn the_semaphore_falls_back_to_the_slots_array_then_to_config() {
        let h = harness();
        // No `/props`, but `/slots` answers with three slots.
        let with_slots = healthy_server(None, Some(3)).await;
        let a = register(&h, backend("local-a", &with_slots.uri()));
        // Neither endpoint enabled: `max_concurrent = 2` from config is all there is.
        let bare = healthy_server(None, None).await;
        let b = register(&h, backend("local-b", &bare.uri()));

        let mut failures = HashMap::new();
        round(&h, &mut failures).await;

        assert_eq!(a.sem.available_permits(), 3, "/slots length");
        assert_eq!(b.sem.available_permits(), 2, "config's max_concurrent");
    }

    #[tokio::test]
    async fn the_model_index_is_maintained_and_the_table_recompiles() {
        let h = harness();
        let server = healthy_server(Some(1), Some(1)).await;
        let live = register(&h, backend("local-a", &server.uri()));
        assert!(live.model_index.load().is_empty(), "cold index");

        let mut failures = HashMap::new();
        round(&h, &mut failures).await;

        assert_eq!(live.model_index.load().as_slice(), ["Carnice-9b-Q6_K"]);
        // Rule 3 is served out of the compiled table, so the index change must reach it.
        let table = h.state.router.table();
        assert!(
            table
                .resolve(
                    Some("Carnice-9b-Q6_K"),
                    apexrouter_router::RequestClass::Chat,
                    apexrouter_router::UnknownModelPolicy::Reject,
                )
                .is_ok(),
            "the model id should route after the prober learned it"
        );
    }

    #[tokio::test]
    async fn n_consecutive_failures_are_required_before_degraded() {
        let h = harness();
        // A closed loopback port: connection refused, every time, no network.
        let live = register(&h, backend("local-dead", "http://127.0.0.1:1"));
        let mut failures = HashMap::new();

        for i in 1..FAILURES_BEFORE_DEGRADED {
            round(&h, &mut failures).await;
            assert!(
                !matches!(live.meta.load().health, Health::Degraded { .. }),
                "demoted after only {i} failure(s)"
            );
        }
        round(&h, &mut failures).await;
        match &live.meta.load().health {
            Health::Degraded {
                consecutive_failures,
                ..
            } => assert_eq!(*consecutive_failures, FAILURES_BEFORE_DEGRADED),
            other => panic!("expected Degraded after {FAILURES_BEFORE_DEGRADED}, got {other:?}"),
        }

        for _ in FAILURES_BEFORE_DEGRADED..FAILURES_BEFORE_DOWN {
            round(&h, &mut failures).await;
        }
        assert!(
            matches!(live.meta.load().health, Health::Down { .. }),
            "expected Down after {FAILURES_BEFORE_DOWN}"
        );
    }

    #[tokio::test]
    async fn a_healthy_probe_clears_the_failure_count() {
        let h = harness();
        let server = healthy_server(Some(2), Some(2)).await;
        let live = register(&h, backend("local-a", &server.uri()));
        let mut failures = HashMap::new();
        failures.insert(live.id.clone(), FAILURES_BEFORE_DEGRADED);

        round(&h, &mut failures).await;

        assert_eq!(failures.get(&live.id).copied(), Some(0));
        assert!(matches!(live.meta.load().health, Health::Ready { .. }));
    }

    #[tokio::test]
    async fn ready_since_is_not_reset_by_a_later_healthy_probe() {
        let h = harness();
        let server = healthy_server(Some(1), Some(1)).await;
        let live = register(&h, backend("local-a", &server.uri()));
        let mut failures = HashMap::new();

        round(&h, &mut failures).await;
        let first = match &live.meta.load().health {
            Health::Ready { since_unix, .. } => *since_unix,
            other => panic!("expected Ready, got {other:?}"),
        };
        tokio::time::sleep(Duration::from_millis(5)).await;
        round(&h, &mut failures).await;
        match &live.meta.load().health {
            Health::Ready { since_unix, .. } => assert_eq!(*since_unix, first),
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_draining_backend_is_never_promoted_back_to_ready() {
        let h = harness();
        let server = healthy_server(Some(2), Some(2)).await;
        let live = register(&h, backend("local-a", &server.uri()));
        live.accepting.store(false, Ordering::SeqCst);
        live.inflight.store(3, Ordering::SeqCst);

        let mut failures = HashMap::new();
        round(&h, &mut failures).await;

        match &live.meta.load().health {
            Health::Draining { in_flight } => assert_eq!(*in_flight, 3),
            other => panic!("a draining backend must stay draining, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_disabled_backend_is_not_probed_at_all() {
        let h = harness();
        let server = MockServer::start().await;
        // No mocks mounted: any request would be a 404 and would show up as an error.
        let mut b = backend("local-off", &server.uri());
        b.enabled = false;
        let live = register(&h, b);

        let mut failures = HashMap::new();
        round(&h, &mut failures).await;

        assert_eq!(live.meta.load().health, Health::Unknown);
        assert!(server
            .received_requests()
            .await
            .is_some_and(|r| r.is_empty()));
    }

    /// A backend that is still loading its model is `Starting`, not `Degraded` — the
    /// distinction llama.cpp's 503 `{"status":"loading model"}` exists to make.
    #[tokio::test]
    async fn a_loading_upstream_is_starting_not_failing() {
        let h = harness();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(p("/health"))
            .respond_with(
                ResponseTemplate::new(503).set_body_json(json!({"status": "loading model"})),
            )
            .mount(&server)
            .await;
        let live = register(&h, backend("local-boot", &server.uri()));

        let mut failures = HashMap::new();
        for _ in 0..FAILURES_BEFORE_DOWN + 2 {
            round(&h, &mut failures).await;
        }

        assert!(
            matches!(live.meta.load().health, Health::Starting { .. }),
            "a loading model must never be counted as a failure: {:?}",
            live.meta.load().health
        );
        assert_eq!(failures.get(&live.id).copied(), Some(0));
    }

    #[tokio::test]
    async fn a_change_is_broadcast_once_and_a_no_op_is_not_broadcast_at_all() {
        let h = harness();
        let server = healthy_server(Some(2), Some(2)).await;
        register(&h, backend("local-a", &server.uri()));
        let mut rx = h.subscribe();

        let mut failures = HashMap::new();
        round(&h, &mut failures).await;
        assert!(
            matches!(rx.try_recv(), Ok(Event::BackendChanged { .. })),
            "the first probe changes Unknown -> Ready"
        );
        // Drain the recompile's RouteTableChanged, if the index change triggered one.
        while let Ok(ev) = rx.try_recv() {
            assert!(
                !matches!(ev, Event::BackendChanged { .. }),
                "one probe, one BackendChanged"
            );
        }

        round(&h, &mut failures).await;
        let mut saw_backend_changed = false;
        while let Ok(ev) = rx.try_recv() {
            saw_backend_changed |= matches!(ev, Event::BackendChanged { .. });
        }
        assert!(
            !saw_backend_changed,
            "an unchanged backend must not be re-broadcast — PartialEq is there to suppress it"
        );
    }

    #[tokio::test]
    async fn the_failure_map_forgets_backends_that_went_away() {
        let h = harness();
        let live = register(&h, backend("local-dead", "http://127.0.0.1:1"));
        let mut failures = HashMap::new();
        round(&h, &mut failures).await;
        assert!(failures.contains_key(&live.id));

        h.state.router.registry().remove(&live.id);
        round(&h, &mut failures).await;
        assert!(
            failures.is_empty(),
            "a removed backend must not keep a counter alive forever"
        );
    }

    #[test]
    fn the_probe_interval_and_timeout_are_clamped() {
        let mut cfg = crate::ws::testkit::test_config();
        cfg.supervisor.health_interval_ms = 1;
        assert_eq!(probe_interval(&cfg), MIN_INTERVAL);
        assert_eq!(probe_timeout(&cfg), MIN_PROBE_TIMEOUT);

        cfg.supervisor.health_interval_ms = 600_000;
        assert_eq!(probe_interval(&cfg), Duration::from_secs(600));
        assert_eq!(probe_timeout(&cfg), MAX_PROBE_TIMEOUT);
    }

    #[test]
    fn only_described_credential_sources_are_followed() {
        let mut b = backend("x", "http://127.0.0.1:1");
        b.credential = CredentialSource::None;
        assert!(credential_for(&b).is_none());
        b.credential = CredentialSource::Instance;
        assert!(credential_for(&b).is_none());
        b.credential = CredentialSource::Managed {
            store: "credentials.toml".to_owned(),
        };
        assert!(credential_for(&b).is_none());

        let dir = tempfile::TempDir::new().expect("tempdir");
        let key = dir.path().join("k");
        std::fs::write(&key, "sk-from-a-file\n").expect("write");
        b.credential = CredentialSource::File {
            path: key.display().to_string(),
        };
        let found = credential_for(&b).expect("a key");
        assert_eq!(found.expose(), "sk-from-a-file");
    }

    /// Hermeticity, asserted rather than trusted: this module dials `Backend.base_url`, and
    /// every URL any test here can produce is a `wiremock` server or a closed loopback port.
    #[tokio::test]
    async fn the_prober_tests_never_leave_the_loopback_interface() {
        let server = healthy_server(Some(1), Some(1)).await;
        for url in [server.uri(), "http://127.0.0.1:1".to_owned()] {
            assert!(
                url.starts_with("http://127.0.0.1:") || url.starts_with("http://localhost:"),
                "a probe would leave the machine: {url}"
            );
        }
        let cfg = crate::ws::testkit::test_config();
        let together = cfg
            .providers
            .get("together")
            .map(|p| p.base_url.clone())
            .unwrap_or_default();
        assert!(
            together.starts_with("http://127.0.0.1:"),
            "the Together probe would leave the machine: {together}"
        );
    }
}
