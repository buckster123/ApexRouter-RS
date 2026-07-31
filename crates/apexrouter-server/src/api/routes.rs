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
//!
//! ## The one rule the whole verb obeys
//!
//! **A swap never commits to a target it has not proven can serve.** The hot path always had
//! this — a child that fails its health gate aborts the swap with the alias untouched — and
//! the other two paths did not, which produced the same 503 storm in two costumes:
//!
//! * swapping to a **registered but dead** backend reported success, bound the dead one and
//!   drained the live one. Now [`prove_can_serve`] probes the target *before* anything moves,
//!   and a target that cannot answer is a `409` naming the command that fixes it;
//! * a **sequential** swap whose replacement failed to start left the old backend stopped for
//!   ever. Now [`Restore`] is captured before the old backend is touched and applied on every
//!   failure path, so the alias ends the call pointing at something that answers.
//!
//! Two consequences worth stating out loud:
//!
//! * `mode` only means something when something has to be **started**. Pointing an alias at a
//!   backend that is already running allocates nothing, so there is nothing to sequence and
//!   the swap is reported as [`SwapMode::Hot`] whatever was asked for. Honouring "sequential"
//!   there would mean a deliberate gap in service for no benefit.
//! * a sequential swap that *does* start something still has an unavoidable gap: the whole
//!   point is that the replacement cannot fit until the old one is gone. `SwapReport::parked`
//!   is where that gap is closed, and it needs a parking primitive on the request path that
//!   `RouterInner` does not publish. Until it does, the gap is bounded by the launch and ends
//!   without operator action — which is the difference between this and what shipped.

use super::{apply_routes, ApiError, ApiResult};
use crate::state::AppState;
use apexrouter_core::error::Error as CoreError;
use apexrouter_core::upstream;
use apexrouter_protocol::{
    AlertLevel, Alias, BackendId, BackendSelector, EndpointSpec, Event, FitVerdict, Health,
    ModelRoute, RetryPolicy, RouteFile, RouteFilter, RouteTarget, SmokeProbe, Strategy, SwapMode,
    SwapReport, ValidationReport,
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
/// **Hot**: prove B can serve — start it and pass its health gate, or probe it when it is
/// already running — then repoint the alias with one `ArcSwap::store`, drain A on the
/// router's own in-flight counter, and stop A. A failure at the proving step aborts with A
/// untouched.
///
/// **Sequential**: drain and stop A first, then start B, then repoint — because B does not
/// fit alongside A. If B does not come up, **A is put back and the alias re-points at it**
/// before the failure is returned. The old behaviour left A stopped for ever, which is one
/// `503` per request until somebody guessed the remedy.
///
/// `SwapReport::parked` is `0` in mk1 — see this module's header for why, and for what
/// closing the sequential gap would take.
pub async fn swap(
    State(s): State<Arc<AppState>>,
    Path(alias): Path<String>,
    Json(req): Json<SwapRequest>,
) -> ApiResult<SwapReport> {
    let alias = parse_alias(&alias)?;
    let started = Instant::now();
    let from = current_target(&s, &alias);

    // ---- what are we swapping to? -------------------------------------------------------
    // NOTHING has moved yet and nothing may, until this block has produced a target that is
    // known to be able to answer.
    let (mode, action) = match &req.to {
        SwapTarget::Backend(id) => {
            // THE RULE. A target that cannot answer is not a target. Refusing here costs an
            // operator one `409`; not refusing cost 7492 requests a `503`.
            prove_can_serve(&s, &alias, id).await?;
            // Nothing is started, so nothing can fail to fit and `mode` describes nothing.
            (SwapMode::Hot, Next::Existing(id.clone()))
        }
        SwapTarget::Spec(spec) => match already_serving(&s, spec).await {
            // A healthy endpoint already serves these weights. Starting a second copy is how
            // a 24 GB box OOMs; the alias just moves.
            Some(id) => (SwapMode::Hot, Next::Existing(id)),
            None => {
                let plan = s.supervisor.plan(spec).await.map_err(ApiError::from)?;
                let chosen = req.mode.unwrap_or_else(|| mode_from_fit(&plan));
                (chosen, Next::Start(Box::new(plan)))
            }
        },
    };

    // Already there. Re-binding is idempotent; draining the backend we were just asked to
    // point at is the bug this function exists to stop.
    if let (Next::Existing(id), Some(current)) = (&action, from.as_ref()) {
        if id == current {
            super::bind_alias(&s, &alias, id).map_err(|report| {
                ApiError::bad_request("invalid_routes", super::render_issues(&report))
            })?;
            let to = id.clone();
            return Ok(Json(SwapReport {
                alias,
                mode,
                from,
                to,
                parked: 0,
                drained_ms: 0,
                total_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
            }));
        }
    }

    let mut drained_ms = 0u32;
    // How to put A back, captured **before** A is touched. `None` means there is nothing to
    // undo — the hot path, where A is still serving.
    let mut undo: Option<Restore> = None;

    // Sequential: A goes away first, because B cannot fit next to it.
    if mode == SwapMode::Sequential {
        if let Some(old) = from.clone() {
            undo = Restore::capture(&s, &old);
            drained_ms = drain_and_stop(&s, &old).await?;
        }
    }

    let to = match action {
        Next::Existing(id) => id,
        Next::Start(plan) => match s.supervisor.up(*plan, None).await {
            Ok(backend) => {
                let id = backend.id.clone();
                // `register_started`, not `register_backend`: this is a new process, and the
                // drain flag left behind by whatever last stopped this id is not its.
                super::register_started(&s, backend);
                id
            }
            Err(e) => return Err(rolled_back(&s, &alias, undo, ApiError::from(e)).await),
        },
    };

    // One store: the alias now points at B.
    if let Err(report) = super::bind_alias(&s, &alias, &to) {
        let why = ApiError::bad_request("invalid_routes", super::render_issues(&report));
        return Err(rolled_back(&s, &alias, undo, why).await);
    }

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
///
/// The row **stays in the registry**, marked `Down`, exactly as `POST /v1/endpoints/{id}/stop`
/// leaves it. Removing it made every alias still naming it a *dangling target*, which fails
/// the compile, which leaves the previous table armed — so the operator got a standing "the
/// routing table does not compile" alert on top of the outage, and the stale table went on
/// pointing at a `LiveBackend` whose process was gone. A `Down` row compiles, resolves to a
/// clean `no_healthy_backend`, and is what a restart re-registers over.
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
    }
    // Whatever it was, it is not serving now, and the row must say so rather than keep the
    // `Ready` the prober last wrote. `register_backend` recompiles and broadcasts.
    let mut meta = live.meta.load_full().as_ref().clone();
    meta.health = if has_endpoint {
        Health::Down {
            reason: format!("stopped by a swap away from {id}"),
            retry_at_unix: super::now_unix(),
        }
    } else {
        Health::Draining {
            in_flight: live.inflight.load(Ordering::Relaxed),
        }
    };
    super::register_backend(state, meta);
    Ok(drained_ms)
}

/// Prove a **registered** backend can actually serve, before anything is drained or
/// re-pointed.
///
/// This is FIX-5 in one function. The hot path already had the property — a child that fails
/// its health gate aborts the swap — and it is the same rule here: the alias does not move
/// until the destination has answered.
///
/// A probe rather than the stored `Health`, because invariant 3 says health is computed on
/// read. The row that said `ready` while the proxy answered `503` was a stored value nobody
/// re-checked; consulting it here would have re-derived the same wrong answer. The probe's
/// findings are folded back into the record, so what the operator sees in `apexrouter backend
/// show` after a refusal is the evidence the refusal was based on.
///
/// A backend that is `enabled` but merely **drained** is re-armed rather than refused:
/// pointing an alias at something is an explicit instruction to serve from it, and leaving it
/// out of rotation would be the very sin this function exists to prevent.
///
/// # Errors
/// `404` for an id the registry does not know; `409 backend_not_ready` — naming the command
/// that fixes it — for one that cannot answer.
async fn prove_can_serve(
    state: &Arc<AppState>,
    alias: &Alias,
    id: &BackendId,
) -> Result<(), ApiError> {
    let live = state
        .router
        .registry()
        .get(id)
        .ok_or_else(|| ApiError::not_found(format!("backend {id}")))?;

    if !live.meta.load().enabled {
        let remedy = format!("apexrouter backend enable {id}");
        note_remedy(
            state,
            id,
            &format!("disabled; re-enable it with `{remedy}`"),
        );
        return Err(not_ready(
            alias,
            id,
            "it is disabled, so every request through the alias would answer 503",
            &remedy,
        ));
    }

    let probed = super::backends::probe_into_record(state, id.as_str()).await?;
    match &probed.health {
        Health::Ready { .. } => {
            // Drained but healthy: the swap is the instruction to put it back in rotation.
            live.accepting.store(true, Ordering::Release);
            let _ = super::recompile(state);
            Ok(())
        }
        Health::Starting { .. } => Err(not_ready(
            alias,
            id,
            "it is still loading a model",
            &format!("wait for it, then: apexrouter swap {alias} --to {id}"),
        )),
        other => {
            let why = match other {
                Health::Down { reason, .. } | Health::Degraded { reason, .. } => {
                    format!("it did not answer: {reason}")
                }
                _ => "it did not answer a health probe".to_owned(),
            };
            Err(not_ready(
                alias,
                id,
                &why,
                &format!("apexrouter backend probe {id}   # then start or fix it"),
            ))
        }
    }
}

/// A refusal that names the reason **and** the command that fixes it.
///
/// `409`, not `503`: nothing moved, and the machine is simply not in a state where this can
/// happen yet. The remedy is spelled out because the failure this replaces was a silent
/// storm of `503`s whose fix (`apexrouter backend enable <id>`) appeared nowhere at all.
fn not_ready(alias: &Alias, id: &BackendId, why: &str, remedy: &str) -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "backend_not_ready",
        format!(
            "not pointing {alias} at {id}: {why}. Nothing was changed — {alias} still serves \
             from wherever it did. Fix it with: {remedy}"
        ),
    )
    .with_param("to")
}

/// Hang a one-line remedy off the backend itself.
///
/// `Backend::last_error` is rendered by `apexrouter backend show` and lifted into a standing
/// alert by the snapshot, so this is how the fix reaches somebody who is looking at the
/// backend rather than at the response to the swap they ran ten minutes ago.
fn note_remedy(state: &Arc<AppState>, id: &BackendId, note: &str) {
    if let Some(live) = state.router.registry().get(id) {
        let mut meta = live.meta.load_full().as_ref().clone();
        meta.last_error = Some(note.to_owned());
        super::register_backend(state, meta);
    }
}

/// What it would take to put the old backend back, captured **before** it is taken down.
enum Restore {
    /// Nothing of ours runs it — a LAN node, a managed provider. It was only drained, so
    /// putting it back is flipping the flag and re-probing.
    Rearm(BackendId),
    /// A child we stopped. Putting it back means running the same spec again, which is what
    /// `POST /v1/endpoints/{id}/restart` does.
    Respawn {
        /// The id it ran under; `allocate_id` reuses it for an identical spec.
        id: BackendId,
        /// Exactly what it was running. Boxed — an `EndpointSpec` is large.
        spec: Box<EndpointSpec>,
    },
}

impl Restore {
    /// Capture the undo for `id`, or `None` when there is nothing to undo.
    fn capture(state: &Arc<AppState>, id: &BackendId) -> Option<Restore> {
        let live = state.router.registry().get(id)?;
        if live.meta.load().endpoint.is_none() {
            return Some(Restore::Rearm(id.clone()));
        }
        let rec = state
            .store
            .list_endpoints()
            .ok()?
            .into_iter()
            .find(|r| &r.id == id)?;
        Some(Restore::Respawn {
            id: id.clone(),
            spec: Box::new(rec.spec),
        })
    }

    /// Which backend this would put back, for a message written before it is applied.
    fn id(&self) -> &BackendId {
        match self {
            Restore::Rearm(id) | Restore::Respawn { id, .. } => id,
        }
    }

    /// Put it back and re-point `alias` at it.
    ///
    /// # Errors
    /// A rendered sentence, because the caller is already building a failure message and a
    /// second error type would only be flattened into a string one line later.
    async fn apply(self, state: &Arc<AppState>, alias: &Alias) -> Result<BackendId, String> {
        let id = match self {
            Restore::Rearm(id) => {
                if let Some(live) = state.router.registry().get(&id) {
                    live.accepting.store(true, Ordering::Release);
                }
                // Health is computed on read, so restore the *fact* and re-derive the status.
                let _ = super::backends::probe_into_record(state, id.as_str()).await;
                id
            }
            Restore::Respawn { id, spec } => {
                let plan = state
                    .supervisor
                    .plan(&spec)
                    .await
                    .map_err(|e| format!("{id} could not be planned again: {e}"))?;
                let backend = state
                    .supervisor
                    .up(plan, None)
                    .await
                    .map_err(|e| format!("{id} could not be started again: {e}"))?;
                let back = backend.id.clone();
                // A restore that left `accepting = false` would put the process back and
                // still serve nothing — the outage would survive its own fix.
                super::register_started(state, backend);
                back
            }
        };
        super::bind_alias(state, alias, &id).map_err(|report| {
            format!(
                "{id} is back up, but {alias} could not be re-pointed at it: {}",
                super::render_issues(&report)
            )
        })?;
        Ok(id)
    }
}

/// Undo a half-finished swap and turn the original failure into one that says so.
///
/// The status and kind of `why` are preserved — an `insufficient_vram` refusal is still a
/// `409 insufficient_vram` — and only the message grows, because the caller already branches
/// on `error.kind` and a swap that failed for want of VRAM has not become a different
/// failure by being rolled back.
async fn rolled_back(
    state: &Arc<AppState>,
    alias: &Alias,
    undo: Option<Restore>,
    mut why: ApiError,
) -> ApiError {
    let tail = match undo {
        // The hot path never took anything down.
        None => format!("Nothing was taken down; {alias} still serves from wherever it did."),
        Some(r) => {
            let was = r.id().clone();
            match r.apply(state, alias).await {
                Ok(id) => format!("{id} has been brought back and {alias} points at it again."),
                Err(e) => {
                    let remedy = format!("apexrouter endpoint restart {was}");
                    note_remedy(
                        state,
                        &was,
                        &format!("a swap stopped it and could not restart it: {e}. `{remedy}`"),
                    );
                    super::emit(
                        state,
                        Event::Alert {
                            level: AlertLevel::Serious,
                            message: format!(
                                "{alias} has nothing to serve from: the swap failed and {was} \
                                 could not be brought back ({e})"
                            ),
                            action: Some(remedy.clone()),
                            id: format!("swap.stranded.{alias}"),
                        },
                    );
                    format!(
                        "{was} was stopped for the swap and could NOT be brought back ({e}), so \
                         {alias} is serving nothing. Recover with: {remedy}"
                    )
                }
            }
        }
    };
    why.body.message = format!("swap of {alias} failed: {} — {tail}", why.body.message);
    why
}

/// A registered backend that already serves what this spec would start.
///
/// `apexrouter swap <alias> --to <model>` resolves to an `EndpointSpec`, and starting a second
/// copy of a model that is already resident is how a 24 GB box OOMs — and the OOM takes the
/// *first* copy with it, so the alias ends up broken either way. Identity is the **weights**,
/// not the id: two specs naming the same file are the same model however they were spelled.
///
/// A candidate still has to prove itself through [`super::backends::probe_into_record`]
/// before it is used; "already serving" means healthy, not merely recorded.
///
/// This deliberately ignores the rest of the spec — a different context, a different
/// `--parallel`. `swap` is a **routing** verb, and "point this alias at this model" is
/// answered by the copy that is already resident. Wanting the same weights up a second time
/// with different settings is a lifecycle decision, and `apexrouter endpoint start` is where
/// it is made.
async fn already_serving(state: &Arc<AppState>, spec: &EndpointSpec) -> Option<BackendId> {
    let want = model_identity(spec)?;
    let advertised = advertised_ids(spec);

    let mut candidates: Vec<BackendId> = Vec::new();
    if let Ok(records) = state.store.list_endpoints() {
        for rec in records {
            if model_identity(&rec.spec).as_deref() == Some(want.as_str()) {
                candidates.push(rec.id);
            }
        }
    }
    // A backend that already advertises the model id this spec would serve counts too: a LAN
    // node serving the same weights is still "already serving", and no record of ours names
    // its file.
    for live in state.router.registry().all() {
        let meta = live.meta.load();
        let serves = meta.models.iter().any(|m| advertised.contains(&m.id))
            || live
                .model_index
                .load()
                .iter()
                .any(|m| advertised.contains(m));
        if serves && !candidates.contains(&live.id) {
            candidates.push(live.id.clone());
        }
    }

    for id in candidates {
        match state.router.registry().get(&id) {
            Some(live) if live.meta.load().enabled => {}
            _ => continue,
        }
        if let Ok(b) = super::backends::probe_into_record(state, id.as_str()).await {
            if matches!(b.health, Health::Ready { .. }) {
                if let Some(live) = state.router.registry().get(&id) {
                    live.accepting.store(true, Ordering::Release);
                }
                tracing::info!(%id, model = %want, "already served; re-pointing instead of starting a second copy");
                return Some(id);
            }
        }
    }
    None
}

/// What makes two specs "the same model": the weights, not the id, the port or the flags.
///
/// Only the local kinds have an answer. A `Node` or a `Managed` spec occupies no VRAM here,
/// so "already resident" is not a question about it, and a `Vast` spec is refused by the CLI
/// long before it reaches this function.
fn model_identity(spec: &EndpointSpec) -> Option<String> {
    match spec {
        EndpointSpec::LocalLlama(s) => Some(s.model_path.clone()),
        EndpointSpec::LocalVllm(s) => Some(s.model_id.clone()),
        EndpointSpec::Vast(_) | EndpointSpec::Node(_) | EndpointSpec::Managed(_) => None,
    }
}

/// The model ids a backend running this spec would advertise on `/v1/models`.
///
/// llama.cpp answers with whatever `-a/--alias` said, and with the weights' file stem when it
/// said nothing — so both are matched.
fn advertised_ids(spec: &EndpointSpec) -> Vec<String> {
    match spec {
        EndpointSpec::LocalLlama(s) => {
            let mut v = Vec::with_capacity(2);
            if !s.alias_flag.trim().is_empty() {
                v.push(s.alias_flag.clone());
            }
            let stem = file_stem(&s.model_path);
            if !v.contains(&stem) {
                v.push(stem);
            }
            v
        }
        EndpointSpec::LocalVllm(s) => vec![s.model_id.clone()],
        EndpointSpec::Vast(_) | EndpointSpec::Node(_) | EndpointSpec::Managed(_) => Vec::new(),
    }
}

/// `"/models/carnice-9b/Carnice-9b-Q6_K.gguf"` → `"Carnice-9b-Q6_K"`.
fn file_stem(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.')
        .map(|(stem, _)| stem.to_owned())
        .unwrap_or_else(|| name.to_owned())
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
    use apexrouter_tests_support::Stub;
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

    /// Two stub upstreams that really answer, with `auto` on the first.
    ///
    /// A swap target now has to **prove** it can serve, so a swap test cannot point at
    /// `127.0.0.1:9` and call the result a swap — that closed port is the whole of MK1's
    /// fourth failing shape.
    async fn two_stubs(state: &Arc<AppState>) -> (Stub, Stub) {
        let a = Stub::start();
        let b = Stub::start();
        super::super::register_backend(state, a.backend("a"));
        super::super::register_backend(state, b.backend("b"));
        let alias = Alias::parse("auto").expect("alias");
        super::super::bind_alias(state, &alias, &BackendId::parse("a").expect("id")).expect("bind");
        (a, b)
    }

    #[tokio::test]
    async fn swap_to_a_registered_backend_is_hot_and_reports_it() {
        let state = app(test_config());
        let (_a, _b) = two_stubs(&state).await;
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

    /// `mode` describes when the replacement is **started** relative to the old one being
    /// freed. Pointing an alias at a backend that is already running starts nothing, so there
    /// is no sequencing constraint to honour and the report says `hot`.
    ///
    /// This test used to assert the opposite. Honouring "sequential" here means stopping the
    /// only working backend before re-pointing the alias — a deliberate gap in service, for a
    /// swap that needed no VRAM freed at all, against a zero-5xx acceptance criterion.
    #[tokio::test]
    async fn forcing_sequential_onto_a_running_target_is_reported_as_the_hot_swap_it_is() {
        let state = app(test_config());
        let (_a, _b) = two_stubs(&state).await;
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
        assert_eq!(report.mode, SwapMode::Hot, "nothing had to be started");
        assert_eq!(report.to.as_str(), "b");
    }

    /// MK1's fourth failing shape, at unit scale: the target is registered and dead.
    #[tokio::test]
    async fn swapping_to_a_target_that_cannot_answer_refuses_and_leaves_the_alias_alone() {
        let state = app(test_config());
        let a = Stub::start();
        super::super::register_backend(&state, a.backend("a"));
        // Registered, enabled, `Unknown` — and pointing at a closed port.
        super::super::register_backend(&state, node_backend("dead", "http://127.0.0.1:9", "m"));
        let alias = Alias::parse("auto").expect("alias");
        super::super::bind_alias(&state, &alias, &BackendId::parse("a").expect("id"))
            .expect("bind");
        let base = serve_api(Arc::clone(&state)).await;

        let res = reqwest::Client::new()
            .post(format!("{base}/v1/routes/auto/swap"))
            .json(&serde_json::json!({"to": "dead"}))
            .send()
            .await
            .expect("swap");
        assert_eq!(res.status(), 409);
        let body: apexrouter_protocol::ErrorEnvelope = res.json().await.expect("ErrorEnvelope");
        assert_eq!(body.error.kind, "backend_not_ready");
        assert!(
            body.error.message.contains("apexrouter backend"),
            "the refusal must name a command that helps: {}",
            body.error.message
        );

        let on_disk = state.store.load_routes().expect("load");
        assert_eq!(
            on_disk.routes[0].targets[0].backend,
            BackendSelector::Id(BackendId::parse("a").expect("id")),
            "the live backend keeps the alias"
        );
        let live = state
            .router
            .registry()
            .get(&BackendId::parse("a").expect("id"))
            .expect("live");
        assert!(
            live.accepting.load(Ordering::Relaxed),
            "the only working backend must not have been drained for a dead one"
        );
    }

    /// A disabled target names the one recovery MK1 found surfaced nowhere.
    #[tokio::test]
    async fn swapping_to_a_disabled_target_names_backend_enable() {
        let state = app(test_config());
        let (_a, b) = two_stubs(&state).await;
        let mut meta = b.backend("b");
        meta.enabled = false;
        super::super::register_backend(&state, meta);
        let base = serve_api(Arc::clone(&state)).await;

        let res = reqwest::Client::new()
            .post(format!("{base}/v1/routes/auto/swap"))
            .json(&serde_json::json!({"to": "b"}))
            .send()
            .await
            .expect("swap");
        assert_eq!(res.status(), 409);
        let body: apexrouter_protocol::ErrorEnvelope = res.json().await.expect("ErrorEnvelope");
        assert!(
            body.error.message.contains("apexrouter backend enable b"),
            "{}",
            body.error.message
        );

        // …and on the backend itself, where `backend show` and the snapshot's alerts read it.
        let shown = state
            .router
            .registry()
            .get(&BackendId::parse("b").expect("id"))
            .expect("live")
            .meta
            .load_full()
            .as_ref()
            .clone();
        assert!(
            shown
                .last_error
                .as_deref()
                .unwrap_or_default()
                .contains("apexrouter backend enable b"),
            "{:?}",
            shown.last_error
        );
    }

    /// Swapping to where the alias already points must not drain the thing it points at.
    #[tokio::test]
    async fn swapping_to_where_the_alias_already_points_is_a_no_op_not_a_self_drain() {
        let state = app(test_config());
        let (_a, _b) = two_stubs(&state).await;
        let base = serve_api(Arc::clone(&state)).await;

        let report: SwapReport = reqwest::Client::new()
            .post(format!("{base}/v1/routes/auto/swap"))
            .json(&serde_json::json!({"to": "a"}))
            .send()
            .await
            .expect("swap")
            .json()
            .await
            .expect("SwapReport");
        assert_eq!(report.to.as_str(), "a");
        assert_eq!(report.drained_ms, 0);
        let live = state
            .router
            .registry()
            .get(&BackendId::parse("a").expect("id"))
            .expect("live");
        assert!(
            live.accepting.load(Ordering::Relaxed),
            "it must still be serving"
        );
    }

    /// A drained-but-healthy backend is re-armed by being swapped to: pointing an alias at
    /// something is an explicit instruction to serve from it.
    #[tokio::test]
    async fn swapping_to_a_drained_but_healthy_backend_puts_it_back_in_rotation() {
        let state = app(test_config());
        let (_a, _b) = two_stubs(&state).await;
        let b_id = BackendId::parse("b").expect("id");
        state
            .router
            .registry()
            .get(&b_id)
            .expect("live")
            .accepting
            .store(false, Ordering::Release);
        let base = serve_api(Arc::clone(&state)).await;

        let res = reqwest::Client::new()
            .post(format!("{base}/v1/routes/auto/swap"))
            .json(&serde_json::json!({"to": "b"}))
            .send()
            .await
            .expect("swap");
        assert_eq!(
            res.status(),
            200,
            "{}",
            res.text().await.unwrap_or_default()
        );
        assert!(
            state
                .router
                .registry()
                .get(&b_id)
                .expect("live")
                .accepting
                .load(Ordering::Relaxed),
            "an alias pointed at a drained backend that answers must re-arm it, not leave the \
             alias pointing at something that will never be dispatched to"
        );
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
