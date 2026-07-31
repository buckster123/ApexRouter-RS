//! OWNER: unit S-07 (server/src/api/{vast,hf,providers,checks,compare}.rs). Do not edit outside that unit.
//!
//! `GET /v1/checks`, `GET /v1/diagnose` and `POST /v1/smoke` — the last two stream SSE, one event per check or probe.
//!
//! # This module runs checks; it does not write any
//!
//! Every diagnostic in mk1 is a `Check` in a registry (`ARCHITECTURE.md` §4.10): the local
//! ones come from `core::checks::local_checks()` and the provider ones — `creds.*`,
//! `vast.credit`, `ssh.*`, `net.stall`, `together.ratelimits` and the four `smoke.*` probes
//! — are registered by the daemon at startup from `apexrouter_providers::checks`. So this
//! module is a *runner*: it builds the [`CheckCtx`], streams results as they land, and
//! never contains a diagnostic of its own. Adding a check must never mean editing an API
//! module.
//!
//! # The streaming contract
//!
//! `GET /v1/diagnose` and `POST /v1/smoke` both emit **one named SSE event per result**
//! (`check` and `probe` respectively, each carrying a `CheckResult` as JSON), in
//! *completion* order, followed by exactly one `done` event carrying the whole set in
//! *registration* order. A client that only wants the summary can wait for `done`; a client
//! that wants a progressively filling table renders every `check`. The registry's own
//! per-check timeout and panic guard mean the stream always terminates.
//!
//! # How a smoke probe learns what to smoke
//!
//! `CheckCtx::ext` is the documented injection point, and the four smoke probes need a
//! target that only the control plane can resolve (an alias goes through the live routing
//! table). The contract, so `providers/src/smoke.rs` and this file cannot drift, is three
//! keys holding types both crates already share:
//!
//! | key | type | meaning |
//! |---|---|---|
//! | `smoke.base_url` | `Arc<String>` | the resolved upstream, **without** `/v1` |
//! | `smoke.model` | `Arc<String>` | the **resolved route's** model id — never `"x"` |
//! | `smoke.api_key` | `Arc<Secret<String>>` | present only when the target needs one |
//! | `smoke.alias` | `Arc<String>` | present only when the request named an alias |

use super::{ApiError, ApiResult};
use crate::state::AppState;
use apexrouter_core::checks::CheckCtx;
use apexrouter_core::secret::Secret;
use apexrouter_protocol::{CheckId, CheckResult, CheckStatus, InstanceId, ProviderId};
use apexrouter_router::{RequestClass, UnknownModelPolicy};
use axum::extract::{Query, State};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::mpsc;

/// How many results may queue before a slow SSE client applies backpressure to the check
/// that is trying to report. Deep enough that a whole registry fits without blocking.
const STREAM_DEPTH: usize = 64;

/// The `/v1/checks`, `/v1/diagnose` and `/v1/smoke` routes.
pub fn router() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route("/v1/checks", get(list))
        .route("/v1/diagnose", get(diagnose))
        .route("/v1/smoke", post(smoke))
}

/// `?only=` — an exact id, a namespace, or any fragment. Separators are ignored on both
/// sides, so `--only rate-limits` finds `together.ratelimits`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct OnlyQuery {
    /// Which checks to run. Absent runs them all.
    #[serde(default)]
    pub only: Option<String>,
}

/// `POST /v1/smoke` — what to smoke.
///
/// Exactly one of `alias` and `base_url` is required. An alias is resolved through the live
/// routing table, which is the whole point: the probe then uses **the resolved route's
/// model id** rather than `smoke.sh`'s hardcoded `"model":"x"`, which 400s on every managed
/// provider.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SmokeRequest {
    /// Smoke whatever this alias resolves to right now.
    #[serde(default)]
    pub alias: Option<String>,
    /// Smoke a URL directly. Stored and used **without** a trailing `/v1`.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Override the model id the probes ask for.
    #[serde(default)]
    pub model: Option<String>,
    /// Name a configured provider whose credential the probes should use, when smoking a
    /// managed base URL directly.
    #[serde(default)]
    pub provider: Option<String>,
}

/// `GET /v1/checks` — every registered check id, in registration (display) order.
pub async fn list(State(s): State<Arc<AppState>>) -> ApiResult<Vec<CheckId>> {
    Ok(Json(s.checks.ids()))
}

/// `GET /v1/diagnose?only=` — the whole registry, streamed as SSE, one `check` event each.
pub async fn diagnose(
    State(s): State<Arc<AppState>>,
    Query(q): Query<OnlyQuery>,
) -> Sse<impl futures_util::Stream<Item = Result<SseEvent, Infallible>>> {
    let ctx = check_ctx(&s, None, HashMap::new()).await;
    stream_registry(&s, ctx, q.only, "check")
}

/// `POST /v1/smoke` — the four native probes against one alias or URL, streamed as SSE, one
/// `probe` event each.
///
/// The probes themselves are P-08's `smoke.*` checks; this resolves the target, injects it
/// through [`CheckCtx::ext`] and runs the registry filtered to `smoke`.
pub async fn smoke(
    State(s): State<Arc<AppState>>,
    Json(req): Json<SmokeRequest>,
) -> Result<Sse<impl futures_util::Stream<Item = Result<SseEvent, Infallible>>>, ApiError> {
    let target = resolve_target(&s, &req)?;

    let mut ext: HashMap<String, Arc<dyn std::any::Any + Send + Sync>> = HashMap::new();
    ext.insert("smoke.base_url".to_owned(), Arc::new(target.base_url));
    ext.insert("smoke.model".to_owned(), Arc::new(target.model));
    if let Some(alias) = target.alias {
        ext.insert("smoke.alias".to_owned(), Arc::new(alias));
    }
    if let Some(key) = target.credential {
        ext.insert("smoke.api_key".to_owned(), Arc::new(key));
    }

    let ctx = check_ctx(&s, None, ext).await;
    Ok(stream_registry(&s, ctx, Some("smoke".to_owned()), "probe"))
}

// ----------------------------------------------------------------------------------------
// shared with the rest of S-07
// ----------------------------------------------------------------------------------------

/// Everything a check may read, assembled from the live daemon.
///
/// The rig is taken from S-04's cache rather than rescanned: a diagnose that spent four
/// seconds enumerating Vulkan devices before running `creds.vast` would be a diagnose
/// nobody runs.
pub async fn check_ctx(
    s: &Arc<AppState>,
    instance: Option<InstanceId>,
    ext: HashMap<String, Arc<dyn std::any::Any + Send + Sync>>,
) -> CheckCtx {
    let cfg = s.cfg();
    let rig = crate::api::rig::rig_snapshot(s, false)
        .await
        .ok()
        .map(Arc::new);
    CheckCtx {
        paths: s.paths.clone(),
        cfg: Arc::clone(&cfg),
        http: super::http().clone(),
        rig,
        // A check that `needs()` a daemon is running inside one right now, by construction.
        proxy_url: Some(format!("http://{}", cfg.proxy_bind())),
        instance,
        ext,
    }
}

/// Run the registry to completion and return every result, in registration order.
///
/// The blocking sibling of [`stream_registry`], for the routes that answer with a `Vec`
/// rather than a stream — `GET /v1/vast/instances/{id}/diagnose` is the one that matters.
pub async fn run_checks(s: &Arc<AppState>, ctx: &CheckCtx, only: Option<&str>) -> Vec<CheckResult> {
    // The registry requires a sender; a dropped receiver is documented as normal, so this
    // is the "no stream attached" case rather than a channel nobody drains.
    let (tx, rx) = mpsc::channel(1);
    drop(rx);
    s.checks.run(ctx, only, tx).await
}

/// The S-07 control routes, bound to an ephemeral loopback port. Returns the base URL.
///
/// `mod.rs`'s `serve_api` deliberately merges only S-03's own routers, and S-01 owns the
/// one file where the rest are assembled. This is the same thing for the five routers this
/// unit publishes, so every module here can be tested over real HTTP — which is the only
/// way to test an SSE frame or a `409` body. Nothing but `127.0.0.1` is ever contacted.
#[cfg(test)]
pub(crate) async fn serve_s07(state: Arc<AppState>) -> String {
    let app = axum::Router::new()
        .merge(router())
        .merge(super::vast::router())
        .merge(super::hf::router())
        .merge(super::providers::router())
        .merge(super::compare::router())
        .merge(super::jobs::router())
        .with_state(state);
    let listener = tokio::net::TcpListener::bind::<std::net::SocketAddr>(
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

/// Split an SSE body into `(event, data)` pairs. Shared by the SSE tests in this unit.
#[cfg(test)]
pub(crate) fn sse_frames(body: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for frame in body.split("\n\n") {
        let mut name = String::new();
        let mut data = String::new();
        for line in frame.lines() {
            if let Some(rest) = line.strip_prefix("event:") {
                name = rest.trim().to_owned();
            } else if let Some(rest) = line.strip_prefix("data:") {
                data.push_str(rest.trim());
            }
        }
        if !name.is_empty() {
            out.push((name, data));
        }
    }
    out
}

// ----------------------------------------------------------------------------------------
// internals
// ----------------------------------------------------------------------------------------

/// What `POST /v1/smoke` resolved its request to.
struct SmokeTarget {
    base_url: String,
    model: String,
    alias: Option<String>,
    credential: Option<Secret<String>>,
}

/// Turn `{alias|base_url}` into a concrete upstream, a model id and a credential.
fn resolve_target(s: &Arc<AppState>, req: &SmokeRequest) -> Result<SmokeTarget, ApiError> {
    match (req.alias.as_deref(), req.base_url.as_deref()) {
        (Some(alias), _) => {
            let parsed = apexrouter_protocol::Alias::parse(alias)
                .map_err(|e| ApiError::bad_request("bad_id", e.to_string()).with_param("alias"))?;
            let table = s.router.table();
            let plan = table
                .resolve(
                    Some(parsed.as_str()),
                    RequestClass::Chat,
                    UnknownModelPolicy::Reject,
                )
                .map_err(|e| {
                    ApiError::new(axum::http::StatusCode::NOT_FOUND, "no_route", e.to_string())
                        .with_param("alias")
                })?;
            let cand = plan.candidates.first().ok_or_else(|| {
                ApiError::new(
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    "no_healthy",
                    format!("nothing behind {parsed} is routable"),
                )
            })?;
            let meta = cand.backend.meta.load();
            Ok(SmokeTarget {
                base_url: meta.base_url.clone(),
                model: req
                    .model
                    .clone()
                    .unwrap_or_else(|| cand.upstream_model.clone()),
                alias: Some(parsed.to_string()),
                credential: credential_for(s, &meta.id, req.provider.as_deref()),
            })
        }
        (None, Some(url)) => {
            let url = url.trim();
            if url.is_empty() {
                return Err(
                    ApiError::bad_request("invalid", "base_url must not be empty")
                        .with_param("base_url"),
                );
            }
            Ok(SmokeTarget {
                base_url: strip_v1(url),
                model: req.model.clone().unwrap_or_default(),
                alias: None,
                credential: req
                    .provider
                    .as_deref()
                    .and_then(|p| ProviderId::parse(p).ok())
                    .and_then(|p| super::providers::credential(s, &p)),
            })
        }
        (None, None) => Err(ApiError::bad_request(
            "invalid",
            "one of `alias` or `base_url` is required",
        )
        .with_param("alias")),
    }
}

/// The credential a smoke probe should present to a backend.
///
/// A managed backend's id **is** its provider id, so the credential chain
/// (`credentials.toml` → key file → legacy config → env) resolves without the caller having
/// to name the provider; `?provider=` is the override for a bare URL.
fn credential_for(
    s: &Arc<AppState>,
    backend: &apexrouter_protocol::BackendId,
    provider: Option<&str>,
) -> Option<Secret<String>> {
    let named = provider
        .and_then(|p| ProviderId::parse(p).ok())
        .or_else(|| ProviderId::parse(backend.as_str()).ok())?;
    super::providers::credential(s, &named)
}

/// Strip a trailing `/v1` (and any trailing slash) — the base-URL invariant.
fn strip_v1(url: &str) -> String {
    let mut s = url.trim().trim_end_matches('/');
    while let Some(stripped) = s.strip_suffix("/v1") {
        s = stripped.trim_end_matches('/');
    }
    s.to_owned()
}

/// Run the registry in the background and stream each result as a named SSE event, then one
/// `done` event carrying the whole set.
fn stream_registry(
    s: &Arc<AppState>,
    ctx: CheckCtx,
    only: Option<String>,
    event_name: &'static str,
) -> Sse<impl futures_util::Stream<Item = Result<SseEvent, Infallible>>> {
    let registry = Arc::clone(&s.checks);
    let selected_none = only
        .as_deref()
        .is_some_and(|pattern| !registry.ids().iter().any(|id| matches_loosely(id, pattern)));

    let (tx, rx) = mpsc::channel(STREAM_DEPTH);
    let task = tokio::spawn(async move { registry.run(&ctx, only.as_deref(), tx).await });

    Sse::new(results_stream(rx, task, event_name, selected_none)).keep_alive(KeepAlive::default())
}

/// The stream behind both SSE routes: every result, then exactly one `done`.
fn results_stream(
    rx: mpsc::Receiver<CheckResult>,
    task: tokio::task::JoinHandle<Vec<CheckResult>>,
    event_name: &'static str,
    selected_none: bool,
) -> impl futures_util::Stream<Item = Result<SseEvent, Infallible>> {
    /// Where the stream is. `Live` still has results to forward; `End` is terminal.
    enum St {
        Live(
            mpsc::Receiver<CheckResult>,
            tokio::task::JoinHandle<Vec<CheckResult>>,
        ),
        End,
    }

    futures_util::stream::unfold(St::Live(rx, task), move |st| async move {
        match st {
            St::Live(mut rx, task) => match rx.recv().await {
                Some(result) => Some((sse(event_name, &result), St::Live(rx, task))),
                None => {
                    let mut all = task.await.unwrap_or_default();
                    // Nothing matched: say so as a result rather than closing an empty
                    // stream, which a UI cannot tell apart from a dropped connection.
                    if all.is_empty() && selected_none {
                        all.push(nothing_selected(event_name));
                    }
                    Some((sse("done", &all), St::End))
                }
            },
            St::End => None,
        }
    })
}

/// One SSE frame: a named event whose data is the JSON of `v`.
fn sse<T: Serialize>(name: &str, v: &T) -> Result<SseEvent, Infallible> {
    let data = serde_json::to_string(v)
        .unwrap_or_else(|e| format!("{{\"error\":\"a result could not be serialised: {e}\"}}"));
    Ok(SseEvent::default().event(name).data(data))
}

/// The honest answer when a filter selected nothing.
fn nothing_selected(event_name: &str) -> CheckResult {
    let (id, detail, fix) = if event_name == "probe" {
        (
            "smoke.registry",
            "no smoke probes are registered in this daemon",
            "the four `smoke.*` probes are registered at startup; `GET /v1/checks` lists what is",
        )
    } else {
        (
            "checks.registry",
            "no check matched that filter",
            "`GET /v1/checks` lists every registered id",
        )
    };
    CheckResult {
        id: CheckId::from(id),
        label: "check selection".to_owned(),
        status: CheckStatus::Skipped,
        ms: 0,
        detail: detail.to_owned(),
        fix: Some(fix.to_owned()),
    }
}

/// The registry's own `--only` matching, mirrored so the stream can tell "nothing matched"
/// from "everything passed" without running anything.
fn matches_loosely(id: &CheckId, pattern: &str) -> bool {
    let norm = |s: &str| -> String {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect()
    };
    let pattern = norm(pattern);
    pattern.is_empty() || norm(id.as_str()).contains(&pattern)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::testkit::{app, node_backend, test_config};
    use apexrouter_core::checks::{Check, CheckNeeds, Registry};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A check that always passes and counts how often it ran.
    ///
    /// The trait is written with `#[async_trait]` in `core`; this crate does not link
    /// `async-trait`, so the one method is written in its desugared form rather than
    /// growing a dependency for a test double.
    struct Counting {
        id: CheckId,
        runs: Arc<AtomicUsize>,
    }

    impl Check for Counting {
        fn id(&self) -> CheckId {
            self.id.clone()
        }

        fn label(&self) -> &str {
            "a test check"
        }

        fn needs(&self) -> CheckNeeds {
            CheckNeeds::Local
        }

        fn run<'s, 'c, 'f>(
            &'s self,
            _ctx: &'c CheckCtx,
        ) -> Pin<Box<dyn Future<Output = CheckResult> + Send + 'f>>
        where
            's: 'f,
            'c: 'f,
            Self: 'f,
        {
            let id = self.id.clone();
            let runs = Arc::clone(&self.runs);
            Box::pin(async move {
                runs.fetch_add(1, Ordering::SeqCst);
                CheckResult {
                    id,
                    label: "a test check".to_owned(),
                    status: CheckStatus::Pass,
                    ms: 0,
                    detail: "fine".to_owned(),
                    fix: None,
                }
            })
        }
    }

    fn registry_with(ids: &[&str], runs: &Arc<AtomicUsize>) -> Arc<Registry> {
        let mut r = Registry::new();
        for id in ids {
            r.register(Arc::new(Counting {
                id: CheckId::from(*id),
                runs: Arc::clone(runs),
            }));
        }
        Arc::new(r)
    }

    /// `testkit::app` hands back a sole-owner `Arc`, so the registry can be swapped for a
    /// deterministic one before anything clones the state.
    fn with_registry(state: &mut Arc<AppState>, ids: &[&str], runs: &Arc<AtomicUsize>) {
        Arc::get_mut(state)
            .expect("the test is the sole owner before the server starts")
            .checks = registry_with(ids, runs);
    }

    #[tokio::test]
    async fn the_registry_is_listed_in_registration_order() {
        let runs = Arc::new(AtomicUsize::new(0));
        let mut state = app(test_config());
        with_registry(&mut state, &["creds.vast", "ports.proxy"], &runs);

        let Json(ids) = list(State(Arc::clone(&state))).await.expect("list");
        assert_eq!(
            ids.iter().map(CheckId::as_str).collect::<Vec<_>>(),
            vec!["creds.vast", "ports.proxy"],
            "registration order is display order"
        );
    }

    /// THE acceptance sentence for this route: one event per check, then a summary.
    #[tokio::test]
    async fn diagnose_streams_one_event_per_check_then_done() {
        let runs = Arc::new(AtomicUsize::new(0));
        let mut state = app(test_config());
        with_registry(&mut state, &["a.one", "a.two", "b.three"], &runs);
        let base = serve_s07(Arc::clone(&state)).await;

        let body = reqwest::Client::new()
            .get(format!("{base}/v1/diagnose"))
            .send()
            .await
            .expect("get")
            .text()
            .await
            .expect("body");

        let frames = sse_frames(&body);
        assert_eq!(
            frames.iter().filter(|(n, _)| n == "check").count(),
            3,
            "one event per check: {frames:?}"
        );
        assert_eq!(runs.load(Ordering::SeqCst), 3);
        assert_eq!(frames.last().map(|(n, _)| n.as_str()), Some("done"));

        let all: Vec<CheckResult> =
            serde_json::from_str(&frames[frames.len() - 1].1).expect("done carries the set");
        assert_eq!(
            all.iter().map(|r| r.id.to_string()).collect::<Vec<_>>(),
            vec!["a.one", "a.two", "b.three"],
            "the summary is in registration order"
        );
    }

    #[tokio::test]
    async fn only_filters_loosely_and_an_empty_selection_still_terminates() {
        let runs = Arc::new(AtomicUsize::new(0));
        let mut state = app(test_config());
        with_registry(&mut state, &["together.ratelimits", "creds.vast"], &runs);
        let base = serve_s07(Arc::clone(&state)).await;
        let http = reqwest::Client::new();

        let body = http
            .get(format!("{base}/v1/diagnose?only=rate-limits"))
            .send()
            .await
            .expect("get")
            .text()
            .await
            .expect("body");
        let frames = sse_frames(&body);
        assert_eq!(
            frames.iter().filter(|(n, _)| n == "check").count(),
            1,
            "`rate-limits` finds `together.ratelimits`: {frames:?}"
        );

        let body = http
            .get(format!("{base}/v1/diagnose?only=nothing-matches-this"))
            .send()
            .await
            .expect("get")
            .text()
            .await
            .expect("body");
        let frames = sse_frames(&body);
        assert_eq!(frames.last().map(|(n, _)| n.as_str()), Some("done"));
        let all: Vec<CheckResult> = serde_json::from_str(&frames[frames.len() - 1].1).expect("de");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].status, CheckStatus::Skipped, "said so, not silent");
    }

    /// `POST /v1/smoke` resolves the alias through the live table and streams one event per
    /// probe. The registry here holds `smoke.*` stand-ins, because P-08 owns the real ones.
    #[tokio::test]
    async fn smoke_resolves_an_alias_and_streams_one_event_per_probe() {
        let runs = Arc::new(AtomicUsize::new(0));
        let mut state = app(test_config());
        with_registry(
            &mut state,
            &["smoke.models", "smoke.warmup", "creds.vast"],
            &runs,
        );
        let cfg = state.cfg();
        state.router.registry().upsert(
            node_backend("local-carnice", "http://127.0.0.1:9", "Carnice-9b"),
            &cfg.router,
        );
        let alias = apexrouter_protocol::Alias::parse("auto").expect("alias");
        let id = apexrouter_protocol::BackendId::parse("local-carnice").expect("id");
        crate::api::bind_alias(&state, &alias, &id).expect("bind");

        let base = serve_s07(Arc::clone(&state)).await;
        let body = reqwest::Client::new()
            .post(format!("{base}/v1/smoke"))
            .json(&serde_json::json!({"alias": "auto"}))
            .send()
            .await
            .expect("post")
            .text()
            .await
            .expect("body");

        let frames = sse_frames(&body);
        assert_eq!(
            frames.iter().filter(|(n, _)| n == "probe").count(),
            2,
            "only the smoke.* probes ran: {frames:?}"
        );
        assert_eq!(frames.last().map(|(n, _)| n.as_str()), Some("done"));
    }

    #[tokio::test]
    async fn smoke_without_a_target_is_a_400() {
        let state = app(test_config());
        let base = serve_s07(Arc::clone(&state)).await;
        let res = reqwest::Client::new()
            .post(format!("{base}/v1/smoke"))
            .json(&serde_json::json!({}))
            .send()
            .await
            .expect("post");
        assert_eq!(res.status(), 400);
        let body: apexrouter_protocol::ErrorEnvelope = res.json().await.expect("envelope");
        assert_eq!(body.error.param.as_deref(), Some("alias"));
    }

    #[tokio::test]
    async fn a_smoke_target_carries_the_resolved_model_id_not_a_placeholder() {
        let state = app(test_config());
        let cfg = state.cfg();
        state.router.registry().upsert(
            node_backend("local-carnice", "http://127.0.0.1:9", "Carnice-9b-Q6_K"),
            &cfg.router,
        );
        let alias = apexrouter_protocol::Alias::parse("auto").expect("alias");
        let id = apexrouter_protocol::BackendId::parse("local-carnice").expect("id");
        crate::api::bind_alias(&state, &alias, &id).expect("bind");

        let target = resolve_target(
            &state,
            &SmokeRequest {
                alias: Some("auto".to_owned()),
                ..SmokeRequest::default()
            },
        )
        .expect("resolved");
        assert_eq!(target.model, "Carnice-9b-Q6_K", "never the hardcoded `x`");
        assert_eq!(target.base_url, "http://127.0.0.1:9");
        assert_eq!(target.alias.as_deref(), Some("auto"));
    }

    #[test]
    fn loose_matching_ignores_separators() {
        assert!(matches_loosely(
            &CheckId::from("together.ratelimits"),
            "rate-limits"
        ));
        assert!(matches_loosely(&CheckId::from("creds.vast"), "creds"));
        assert!(!matches_loosely(&CheckId::from("creds.vast"), "ports"));
    }
}
