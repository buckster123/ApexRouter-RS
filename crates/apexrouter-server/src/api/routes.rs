//! OWNER: unit S-03 (server/src/api/{mod,snapshot,backends,routes,endpoints}.rs). Do not edit outside that unit.
//!
//! The `/v1/routes*` set, including `validate`, `test`, `swap` and `default`. `PUT /v1/routes` is atomic, and a compile failure leaves the previous table serving.
//!
//! # Atomicity, concretely
//!
//! `PUT /v1/routes` is compile → persist → `ArcSwap::store`, in that order, inside
//! [`super::apply_routes`]. A table that does not compile is refused **before** either side
//! effect, so:
//!
//! * the previous table is still the table serving — a request that was streaming when the
//!   PUT arrived finishes against the backend it captured at dispatch, and a request that
//!   arrives afterwards still resolves through the old aliases;
//! * `$STATE/routes.json` still describes what is running, so a reload reproduces reality
//!   rather than resurrecting the rejected draft.
//!
//! # Swap
//!
//! `POST /v1/routes/{alias}/swap` is the one verb, and the mode is chosen **for** you from
//! `fit()`: if the new model fits alongside everything currently resident it is
//! [`SwapMode::Hot`], otherwise [`SwapMode::Sequential`]. `ARCHITECTURE.md` §4.7.

use super::{apply_routes, ApiError, ApiResult};
use crate::state::AppState;
use apexrouter_core::error::Error as CoreError;
use apexrouter_core::upstream;
use apexrouter_protocol::{
    Alias, BackendId, BackendSelector, EndpointSpec, FitVerdict, ModelRoute, RetryPolicy,
    RouteFile, RouteFilter, RouteTarget, SmokeProbe, Strategy, SwapMode, SwapReport,
    ValidationReport,
};
use apexrouter_providers::{DownMode, Provisioner};
use apexrouter_router::{RequestClass, UnknownModelPolicy};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How many tokens `POST /v1/routes/{alias}/test` asks for. §6.2 says twenty.
const PROBE_TOKENS: u32 = 20;
/// How long the 20-token probe gets.
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);
/// How often a drain re-reads the in-flight counter.
const DRAIN_POLL: Duration = Duration::from_millis(100);

/// The `/v1/routes*` set.
///
/// `validate` and `default` are static segments and therefore win over `{alias}` in axum's
/// matcher, which is why an alias may not contain a `/` and why those two names are safe.
pub fn router() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route("/v1/routes", get(list).put(put_table))
        .route("/v1/routes/validate", post(validate))
        .route("/v1/routes/default", post(set_default))
        .route("/v1/routes/{alias}", get(one).put(put_one).delete(remove))
        .route("/v1/routes/{alias}/test", post(test))
        .route("/v1/routes/{alias}/swap", post(swap))
}

/// `POST /v1/routes/default` body.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SetDefault {
    /// The alias unknown and legacy model names fall through to.
    pub alias: Alias,
}

/// What `POST /v1/routes/{alias}/swap` is being pointed at.
///
/// Untagged, because §6.2 writes the body as `{to: EndpointSpec | BackendId}`: a bare string
/// is a backend that already exists, an object is something to start first.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum SwapTarget {
    /// A backend that is already registered.
    Backend(BackendId),
    /// Something to start, then swap to.
    Spec(Box<EndpointSpec>),
}

/// `POST /v1/routes/{alias}/swap` body.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SwapRequest {
    /// Where the alias should point when this is over.
    pub to: SwapTarget,
    /// Override the mode `fit()` would have chosen.
    #[serde(default)]
    pub mode: Option<SwapMode>,
}

/// `GET /v1/routes` — the table on disk, which is the table serving unless a compile failed.
pub async fn list(State(s): State<Arc<AppState>>) -> ApiResult<Vec<ModelRoute>> {
    Ok(Json(s.store.load_routes().map_err(ApiError::from)?.routes))
}

/// `PUT /v1/routes` — replace the whole table, atomically.
///
/// `200` with the routes now in effect, or `400` with the [`ValidationReport`] and **no
/// change at all**. The report is the body rather than an `ErrorEnvelope` because it is
/// strictly more informative: it names the field, the severity and the fix.
pub async fn put_table(
    State(s): State<Arc<AppState>>,
    Json(routes): Json<Vec<ModelRoute>>,
) -> Response {
    let default_alias = match s.store.load_routes() {
        Ok(f) => f.default_alias,
        Err(e) => return ApiError::from(e).into_response(),
    };
    let file = RouteFile {
        schema_version: 1,
        default_alias: routes
            .iter()
            .find(|r| r.is_default)
            .map(|r| r.alias.clone())
            .unwrap_or(default_alias),
        routes,
    };
    match apply_routes(&s, &file) {
        Ok(applied) => (StatusCode::OK, Json(applied)).into_response(),
        Err(report) => (StatusCode::BAD_REQUEST, Json(report)).into_response(),
    }
}

/// `GET /v1/routes/{alias}`.
pub async fn one(
    State(s): State<Arc<AppState>>,
    Path(alias): Path<String>,
) -> ApiResult<ModelRoute> {
    let alias = parse_alias(&alias)?;
    s.store
        .load_routes()
        .map_err(ApiError::from)?
        .routes
        .into_iter()
        .find(|r| r.alias == alias)
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("route {alias}")))
}

/// `PUT /v1/routes/{alias}` — insert or replace one route.
///
/// The path segment wins over the body's `alias`, so a copy-pasted body cannot silently
/// create a route under a different name than the URL says.
pub async fn put_one(
    State(s): State<Arc<AppState>>,
    Path(alias): Path<String>,
    Json(mut route): Json<ModelRoute>,
) -> Response {
    let alias = match parse_alias(&alias) {
        Ok(a) => a,
        Err(e) => return e.into_response(),
    };
    route.alias = alias.clone();
    let mut file = match s.store.load_routes() {
        Ok(f) => f,
        Err(e) => return ApiError::from(e).into_response(),
    };
    match file.routes.iter_mut().find(|r| r.alias == alias) {
        Some(slot) => *slot = route.clone(),
        None => file.routes.push(route.clone()),
    }
    if route.is_default {
        file.default_alias = alias;
    }
    match apply_routes(&s, &file) {
        Ok(_) => (StatusCode::OK, Json(route)).into_response(),
        Err(report) => (StatusCode::BAD_REQUEST, Json(report)).into_response(),
    }
}

/// `DELETE /v1/routes/{alias}`.
///
/// Refuses to delete the default alias: `resolve()` rule 5 sends every legacy model name
/// there, so a table with no default is a table where `smoke.sh` stops working.
pub async fn remove(State(s): State<Arc<AppState>>, Path(alias): Path<String>) -> Response {
    let alias = match parse_alias(&alias) {
        Ok(a) => a,
        Err(e) => return e.into_response(),
    };
    let mut file = match s.store.load_routes() {
        Ok(f) => f,
        Err(e) => return ApiError::from(e).into_response(),
    };
    if file.default_alias == alias && file.routes.len() > 1 {
        return ApiError::conflict(format!(
            "{alias} is the default alias; point the default somewhere else first with \
             POST /v1/routes/default"
        ))
        .into_response();
    }
    let before = file.routes.len();
    file.routes.retain(|r| r.alias != alias);
    if file.routes.len() == before {
        return ApiError::not_found(format!("route {alias}")).into_response();
    }
    match apply_routes(&s, &file) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(report) => (StatusCode::BAD_REQUEST, Json(report)).into_response(),
    }
}

/// `POST /v1/routes/validate` — compile a candidate table without arming it.
///
/// Always `200`: the answer is the report, and "this would not compile" is a successful
/// answer to "would this compile?".
pub async fn validate(
    State(s): State<Arc<AppState>>,
    Json(routes): Json<Vec<ModelRoute>>,
) -> ApiResult<ValidationReport> {
    // `Store::load_routes` answers with the shipped default when the file is absent, so the
    // `None` arm is only reachable if `$STATE` itself is unreadable — which is a `500`, not
    // a hardcoded fallback alias.
    let default_alias = match routes
        .iter()
        .find(|r| r.is_default)
        .map(|r| r.alias.clone())
        .or_else(|| s.store.load_routes().ok().map(|f| f.default_alias))
    {
        Some(a) => a,
        None => return Err(ApiError::internal("$STATE/routes.json could not be read")),
    };
    let file = RouteFile {
        schema_version: 1,
        default_alias,
        routes,
    };
    let cfg = s.cfg.load_full();
    Ok(Json(
        match apexrouter_router::TableBuilder::compile(&cfg, &file, s.router.registry()) {
            Ok(_) => ValidationReport {
                ok: true,
                issues: vec![],
            },
            Err(report) => report,
        },
    ))
}

/// `POST /v1/routes/default` — retarget the fallback alias.
pub async fn set_default(State(s): State<Arc<AppState>>, Json(body): Json<SetDefault>) -> Response {
    let mut file = match s.store.load_routes() {
        Ok(f) => f,
        Err(e) => return ApiError::from(e).into_response(),
    };
    file.default_alias = body.alias.clone();
    for r in &mut file.routes {
        r.is_default = r.alias == body.alias;
    }
    match apply_routes(&s, &file) {
        Ok(applied) => (StatusCode::OK, Json(applied)).into_response(),
        Err(report) => (StatusCode::BAD_REQUEST, Json(report)).into_response(),
    }
}

/// `POST /v1/routes/{alias}/test` — a real 20-token completion against whatever the alias
/// resolves to right now.
///
/// Goes to the resolved backend directly rather than back through the proxy listener: the
/// point is to test the *route*, and a hop through our own socket would also be testing the
/// listener, the auth layer and the loopback stack.
///
/// `tok_per_s` is **read** from llama.cpp's `timings.predicted_per_second` when the upstream
/// reports it, and derived from the response's own token count otherwise. It is never
/// stopwatched.
pub async fn test(
    State(s): State<Arc<AppState>>,
    Path(alias): Path<String>,
) -> ApiResult<SmokeProbe> {
    let alias = parse_alias(&alias)?;
    let (base_url, model) = {
        let table = s.router.table();
        let plan = table
            .resolve(
                Some(alias.as_str()),
                RequestClass::Chat,
                UnknownModelPolicy::Reject,
            )
            .map_err(|e| ApiError::new(StatusCode::NOT_FOUND, "no_route", e.to_string()))?;
        let cand = plan.candidates.first().ok_or_else(|| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "no_healthy",
                format!("nothing behind {alias} is routable"),
            )
        })?;
        (
            cand.backend.meta.load().base_url.clone(),
            cand.upstream_model.clone(),
        )
    };

    let started = Instant::now();
    let url = upstream::join_v1(&base_url, "/v1/chat/completions");
    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "ping"}],
        "max_tokens": PROBE_TOKENS,
        "stream": false,
    });
    let sent = super::http()
        .post(&url)
        .timeout(PROBE_TIMEOUT)
        .json(&body)
        .send()
        .await;

    let ms = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
    let probe = match sent {
        Err(e) => SmokeProbe {
            name: format!("route:{alias}"),
            ok: false,
            ms,
            detail: format!("{model} at {base_url}: {e}"),
            ttft_ms: None,
            tok_per_s: None,
            tokens: None,
        },
        Ok(res) => {
            let status = res.status();
            let json: serde_json::Value = res.json().await.unwrap_or(serde_json::Value::Null);
            let timings = upstream::parse_timings(&json);
            let tokens = upstream::parse_usage(&json)
                .map(|u| u.completion_tokens)
                .or_else(|| timings.map(|t| t.predicted_n));
            SmokeProbe {
                name: format!("route:{alias}"),
                ok: status.is_success(),
                ms,
                detail: if status.is_success() {
                    format!("{model} at {base_url}")
                } else {
                    format!("{model} at {base_url}: HTTP {status}")
                },
                // Non-streamed: there is no first token to time, and inventing one from the
                // total would be a number nobody could act on.
                ttft_ms: None,
                tok_per_s: timings
                    .map(|t| t.predicted_per_second)
                    .filter(|v| v.is_finite() && *v > 0.0)
                    .or_else(|| {
                        tokens
                            .filter(|_| ms > 0)
                            .map(|n| n as f32 * 1000.0 / ms as f32)
                    }),
                tokens,
            }
        }
    };
    Ok(Json(probe))
}

/// `POST /v1/routes/{alias}/swap` — one verb, the mode chosen for you.
///
/// **Hot**: start B (a health-gate failure aborts with A untouched), repoint the alias with
/// one `ArcSwap::store`, drain A on the router's own in-flight counter, stop A.
///
/// **Sequential**: drain and stop A first, then start B, then repoint — because B does not
/// fit alongside A.
///
/// `SwapReport::parked` is `0` in mk1: the warm queue of `ARCHITECTURE.md` §4.7 needs a
/// parking primitive on the router's request path, and `RouterInner` publishes none. During
/// a sequential swap the alias resolves to nothing and the proxy answers its usual
/// `no_healthy`, which is visible rather than silent. Reported in `signature_problems`.
pub async fn swap(
    State(s): State<Arc<AppState>>,
    Path(alias): Path<String>,
    Json(req): Json<SwapRequest>,
) -> ApiResult<SwapReport> {
    let alias = parse_alias(&alias)?;
    let started = Instant::now();
    let from = current_target(&s, &alias);

    // What are we swapping to, and would it fit alongside what is resident?
    let (mode, action) = match &req.to {
        SwapTarget::Backend(id) => {
            if s.router.registry().get(id).is_none() {
                return Err(ApiError::not_found(format!("backend {id}")));
            }
            // Nothing to start, so nothing can fail to fit.
            (
                req.mode.unwrap_or(SwapMode::Hot),
                Next::Existing(id.clone()),
            )
        }
        SwapTarget::Spec(spec) => {
            let plan = s.supervisor.plan(spec).await.map_err(ApiError::from)?;
            let chosen = req.mode.unwrap_or_else(|| mode_from_fit(&plan));
            (chosen, Next::Start(Box::new(plan)))
        }
    };

    let mut drained_ms = 0u32;

    // Sequential: A goes away first, because B cannot fit next to it.
    if mode == SwapMode::Sequential {
        if let Some(old) = from.clone() {
            drained_ms = drain_and_stop(&s, &old).await?;
        }
    }

    let to = match action {
        Next::Existing(id) => id,
        Next::Start(plan) => {
            let backend = s.supervisor.up(*plan, None).await.map_err(ApiError::from)?;
            let id = backend.id.clone();
            super::register_backend(&s, backend);
            id
        }
    };

    // One store: the alias now points at B.
    super::bind_alias(&s, &alias, &to)
        .map_err(|report| ApiError::bad_request("invalid_routes", super::render_issues(&report)))?;

    // Hot: A keeps serving what it captured, then goes.
    if mode == SwapMode::Hot {
        if let Some(old) = from.clone().filter(|o| o != &to) {
            drained_ms = drain_and_stop(&s, &old).await?;
        }
    }

    Ok(Json(SwapReport {
        alias,
        mode,
        from,
        to,
        parked: 0,
        drained_ms,
        total_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
    }))
}

// ----------------------------------------------------------------------------------------
// internals
// ----------------------------------------------------------------------------------------

/// Parse a path segment into an `Alias`, distinguishing "you sent nonsense" (`400`) from
/// "there is no such route" (`404`).
fn parse_alias(alias: &str) -> Result<Alias, ApiError> {
    Alias::parse(alias)
        .map_err(|e| ApiError::bad_request("bad_id", e.to_string()).with_param("alias"))
}

/// Which backend an alias points at right now, when it points at exactly one.
fn current_target(state: &Arc<AppState>, alias: &Alias) -> Option<BackendId> {
    let file = state.store.load_routes().ok()?;
    let route = file.routes.iter().find(|r| &r.alias == alias)?;
    route.targets.iter().find_map(|t| match &t.backend {
        BackendSelector::Id(id) => Some(id.clone()),
        // A tag or a glob names a set, not a thing to swap away from. Reporting `None` is
        // honest: there is no single "from" to drain.
        BackendSelector::Tag(_) | BackendSelector::Glob(_) => None,
    })
}

/// What the swap has to do to produce the "to" backend.
enum Next {
    /// It already exists.
    Existing(BackendId),
    /// It has to be started first. Boxed: a `LaunchPlan` carries a whole `ArgvPreview`.
    Start(Box<apexrouter_providers::LaunchPlan>),
}

/// `Fits`/`Tight` means B can live next to A. Anything else means it cannot.
///
/// No fit at all means nothing of ours occupies VRAM — a node or a managed provider — so a
/// hot swap is free.
fn mode_from_fit(plan: &apexrouter_providers::LaunchPlan) -> SwapMode {
    match plan.fit.as_ref().map(|f| &f.verdict) {
        None | Some(FitVerdict::Fits { .. } | FitVerdict::Tight { .. }) => SwapMode::Hot,
        Some(FitVerdict::NeedsOffload { .. } | FitVerdict::WontFit { .. }) => SwapMode::Sequential,
    }
}

/// Stop accepting, wait for the **router's own** counter to reach zero, then take it down.
///
/// The router's counter and not `/slots`: llama.cpp answers `501` there on `--no-slots`
/// builds, and a drain that reads zero because the endpoint does not exist is a drain that
/// kills live requests.
///
/// A backend with no endpoint (a LAN node, a managed provider) is drained but never stopped —
/// there is no process of ours to stop.
async fn drain_and_stop(state: &Arc<AppState>, id: &BackendId) -> Result<u32, ApiError> {
    let started = Instant::now();
    let live = match state.router.registry().get(id) {
        Some(l) => l,
        None => return Ok(0),
    };
    live.accepting.store(false, Ordering::Release);

    let cfg = state.cfg.load_full();
    let deadline = Duration::from_secs(cfg.server.drain_timeout_secs.max(1));
    while live.inflight.load(Ordering::Acquire) > 0 && started.elapsed() < deadline {
        tokio::time::sleep(DRAIN_POLL).await;
    }
    let drained_ms = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);

    let has_endpoint = live.meta.load().endpoint.is_some();
    if has_endpoint {
        match state.supervisor.down(id, DownMode::Now).await {
            Ok(()) => {}
            // A record that vanished under us is the outcome we wanted anyway.
            Err(CoreError::NotFound(_)) => {}
            Err(e) => return Err(ApiError::from(e)),
        }
        state.router.registry().remove(id);
        let _ = super::recompile(state);
    }
    Ok(drained_ms)
}

/// A one-target route, for the tests and for anything that needs the simplest possible
/// `ModelRoute`.
///
/// Public because S-06's `apexrouter route add` builds exactly this shape and there should
/// be one definition of "the simplest route that can exist".
pub fn simple_route(alias: &Alias, backend: &BackendId, is_default: bool) -> ModelRoute {
    ModelRoute {
        alias: alias.clone(),
        targets: vec![RouteTarget {
            backend: BackendSelector::Id(backend.clone()),
            model: None,
            weight: 1,
        }],
        strategy: Strategy::FirstHealthy,
        filter: RouteFilter::default(),
        retry: RetryPolicy::default(),
        is_default,
        description: None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;
    use futures_util::StreamExt;
    use wiremock::matchers::{method as m_method, path as m_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Register two node backends and point `auto` at the first.
    async fn two_nodes(state: &Arc<AppState>, a_url: &str, b_url: &str) {
        super::super::register_backend(state, node_backend("a", a_url, "m"));
        super::super::register_backend(state, node_backend("b", b_url, "m2"));
        let alias = Alias::parse("auto").expect("alias");
        super::super::bind_alias(state, &alias, &BackendId::parse("a").expect("id")).expect("bind");
    }

    #[tokio::test]
    async fn the_route_set_returns_the_documented_protocol_types() {
        let state = app(test_config());
        two_nodes(&state, "http://127.0.0.1:9", "http://127.0.0.1:9").await;
        let base = serve_api(Arc::clone(&state)).await;
        let http = reqwest::Client::new();

        let all: Vec<ModelRoute> = http
            .get(format!("{base}/v1/routes"))
            .send()
            .await
            .expect("get")
            .json()
            .await
            .expect("Vec<ModelRoute>");
        assert_eq!(all.len(), 1);

        let one: ModelRoute = http
            .get(format!("{base}/v1/routes/auto"))
            .send()
            .await
            .expect("get")
            .json()
            .await
            .expect("ModelRoute");
        assert_eq!(one.alias.as_str(), "auto");

        let report: ValidationReport = http
            .post(format!("{base}/v1/routes/validate"))
            .json(&all)
            .send()
            .await
            .expect("post")
            .json()
            .await
            .expect("ValidationReport");
        assert!(report.ok);

        let applied: Vec<ModelRoute> = http
            .post(format!("{base}/v1/routes/default"))
            .json(&serde_json::json!({"alias": "auto"}))
            .send()
            .await
            .expect("post")
            .json()
            .await
            .expect("Vec<ModelRoute>");
        assert!(applied[0].is_default);
    }

    #[tokio::test]
    async fn validate_names_the_field_and_the_fix_for_a_dangling_target() {
        let state = app(test_config());
        let base = serve_api(Arc::clone(&state)).await;
        let bad = vec![simple_route(
            &Alias::parse("auto").expect("alias"),
            &BackendId::parse("ghost").expect("id"),
            true,
        )];
        let report: ValidationReport = reqwest::Client::new()
            .post(format!("{base}/v1/routes/validate"))
            .json(&bad)
            .send()
            .await
            .expect("post")
            .json()
            .await
            .expect("ValidationReport");
        assert!(!report.ok);
        assert!(report.issues[0].field.starts_with("routes[0]"));
        assert!(report.issues[0].fix.is_some());
    }

    /// The acceptance line, literally: a compile failure leaves the previous table serving,
    /// asserted **while a request is streaming through it**.
    #[tokio::test]
    async fn put_routes_is_atomic_while_a_request_is_streaming() {
        // An upstream that dribbles an SSE body out over ~600 ms, so the PUT lands mid-stream.
        let upstream = MockServer::start().await;
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"b\"}}]}\n\n\
                   data: [DONE]\n\n";
        Mock::given(m_method("POST"))
            .and(m_path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(400))
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .mount(&upstream)
            .await;

        let state = app(test_config());
        two_nodes(&state, &upstream.uri(), "http://127.0.0.1:9").await;
        let good_generation = state.router.table().generation();

        // The proxy listener, so the streaming request is a real request through the real
        // request path — not a simulation of one.
        let proxy = apexrouter_router::proxy_router(Arc::clone(&state.router));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let proxy_addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, proxy).await;
        });
        let control = serve_api(Arc::clone(&state)).await;

        let streaming = tokio::spawn(async move {
            let res = reqwest::Client::new()
                .post(format!("http://{proxy_addr}/v1/chat/completions"))
                .json(&serde_json::json!({
                    "model": "auto",
                    "messages": [{"role": "user", "content": "hi"}],
                    "stream": true
                }))
                .send()
                .await
                .expect("send");
            assert_eq!(res.status(), 200);
            let mut body = res.bytes_stream();
            let mut seen = Vec::new();
            while let Some(chunk) = body.next().await {
                seen.extend_from_slice(&chunk.expect("chunk"));
            }
            String::from_utf8_lossy(&seen).into_owned()
        });

        // While that is in flight, PUT a table that cannot compile.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let bad = vec![simple_route(
            &Alias::parse("auto").expect("alias"),
            &BackendId::parse("ghost").expect("id"),
            true,
        )];
        let res = reqwest::Client::new()
            .put(format!("{control}/v1/routes"))
            .json(&bad)
            .send()
            .await
            .expect("put");
        assert_eq!(res.status(), 400);
        let report: ValidationReport = res.json().await.expect("ValidationReport");
        assert!(!report.ok);

        // The previous table never moved…
        assert_eq!(
            state.router.table().generation(),
            good_generation,
            "a refused PUT must not swap the table"
        );
        // …and neither did the file.
        let on_disk = state.store.load_routes().expect("load");
        assert_eq!(on_disk.routes[0].targets.len(), 1);
        assert_eq!(
            on_disk.routes[0].targets[0].backend,
            BackendSelector::Id(BackendId::parse("a").expect("id"))
        );

        // The streaming request finishes against the backend it captured at dispatch.
        let seen = streaming.await.expect("join");
        assert!(seen.contains("[DONE]"), "stream completed: {seen}");

        // And a request that arrives after the refusal still routes.
        let res = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/v1/chat/completions"))
            .json(&serde_json::json!({"model": "auto", "messages": []}))
            .send()
            .await
            .expect("send");
        assert_eq!(res.status(), 200, "the old table is still serving");
    }

    /// A good PUT does swap, and the new table is the one that serves.
    #[tokio::test]
    async fn a_valid_put_replaces_the_whole_table() {
        let state = app(test_config());
        two_nodes(&state, "http://127.0.0.1:9", "http://127.0.0.1:9").await;
        let base = serve_api(Arc::clone(&state)).await;
        let before = state.router.table().generation();

        let next = vec![
            simple_route(
                &Alias::parse("auto").expect("alias"),
                &BackendId::parse("b").expect("id"),
                true,
            ),
            simple_route(
                &Alias::parse("coder").expect("alias"),
                &BackendId::parse("a").expect("id"),
                false,
            ),
        ];
        let applied: Vec<ModelRoute> = reqwest::Client::new()
            .put(format!("{base}/v1/routes"))
            .json(&next)
            .send()
            .await
            .expect("put")
            .json()
            .await
            .expect("Vec<ModelRoute>");
        assert_eq!(applied.len(), 2);
        assert!(state.router.table().generation() > before);
        assert_eq!(state.store.load_routes().expect("load").routes.len(), 2);
    }

    #[tokio::test]
    async fn swap_to_a_registered_backend_is_hot_and_reports_it() {
        let state = app(test_config());
        two_nodes(&state, "http://127.0.0.1:9", "http://127.0.0.1:9").await;
        let base = serve_api(Arc::clone(&state)).await;

        let report: SwapReport = reqwest::Client::new()
            .post(format!("{base}/v1/routes/auto/swap"))
            .json(&serde_json::json!({"to": "b"}))
            .send()
            .await
            .expect("swap")
            .json()
            .await
            .expect("SwapReport");

        assert_eq!(report.alias.as_str(), "auto");
        assert_eq!(report.mode, SwapMode::Hot);
        assert_eq!(
            report.from.as_ref().map(BackendId::to_string),
            Some("a".to_owned())
        );
        assert_eq!(report.to.as_str(), "b");
        assert_eq!(report.parked, 0);

        let on_disk = state.store.load_routes().expect("load");
        assert_eq!(
            on_disk.routes[0].targets[0].backend,
            BackendSelector::Id(BackendId::parse("b").expect("id")),
            "the alias really moved"
        );
    }

    #[tokio::test]
    async fn a_sequential_swap_can_be_forced_and_is_reported_as_such() {
        let state = app(test_config());
        two_nodes(&state, "http://127.0.0.1:9", "http://127.0.0.1:9").await;
        let base = serve_api(Arc::clone(&state)).await;

        let report: SwapReport = reqwest::Client::new()
            .post(format!("{base}/v1/routes/auto/swap"))
            .json(&serde_json::json!({"to": "b", "mode": "sequential"}))
            .send()
            .await
            .expect("swap")
            .json()
            .await
            .expect("SwapReport");
        assert_eq!(report.mode, SwapMode::Sequential);
        assert_eq!(report.to.as_str(), "b");
    }

    #[tokio::test]
    async fn swapping_to_a_backend_that_does_not_exist_is_a_404() {
        let state = app(test_config());
        two_nodes(&state, "http://127.0.0.1:9", "http://127.0.0.1:9").await;
        let base = serve_api(Arc::clone(&state)).await;
        let res = reqwest::Client::new()
            .post(format!("{base}/v1/routes/auto/swap"))
            .json(&serde_json::json!({"to": "ghost"}))
            .send()
            .await
            .expect("swap");
        assert_eq!(res.status(), 404);
    }

    #[tokio::test]
    async fn route_test_runs_a_twenty_token_probe_and_reads_the_upstream_timings() {
        let upstream = MockServer::start().await;
        Mock::given(m_method("POST"))
            .and(m_path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"role": "assistant", "content": "pong"}}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7},
                "timings": {
                    "prompt_n": 3, "prompt_ms": 10.0,
                    "predicted_n": 4, "predicted_ms": 100.0,
                    "predicted_per_second": 40.0, "cache_n": 0
                }
            })))
            .mount(&upstream)
            .await;

        let state = app(test_config());
        two_nodes(&state, &upstream.uri(), "http://127.0.0.1:9").await;
        let base = serve_api(Arc::clone(&state)).await;

        let probe: SmokeProbe = reqwest::Client::new()
            .post(format!("{base}/v1/routes/auto/test"))
            .send()
            .await
            .expect("test")
            .json()
            .await
            .expect("SmokeProbe");
        assert!(probe.ok, "{probe:?}");
        assert_eq!(probe.name, "route:auto");
        assert_eq!(probe.tokens, Some(4));
        assert_eq!(probe.tok_per_s, Some(40.0), "read, not stopwatched");
    }

    #[tokio::test]
    async fn deleting_the_default_alias_is_refused_while_another_route_exists() {
        let state = app(test_config());
        two_nodes(&state, "http://127.0.0.1:9", "http://127.0.0.1:9").await;
        let base = serve_api(Arc::clone(&state)).await;
        let http = reqwest::Client::new();
        http.put(format!("{base}/v1/routes/coder"))
            .json(&simple_route(
                &Alias::parse("coder").expect("alias"),
                &BackendId::parse("b").expect("id"),
                false,
            ))
            .send()
            .await
            .expect("put");
        let res = http
            .delete(format!("{base}/v1/routes/auto"))
            .send()
            .await
            .expect("delete");
        assert_eq!(res.status(), 409);
    }
}
