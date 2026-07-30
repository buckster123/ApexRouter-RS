//! OWNER: unit S-03 (server/src/api/{mod,snapshot,backends,routes,endpoints}.rs). Do not edit outside that unit.
//!
//! The `/v1/backends*` set: list, register a URL, patch, delete, probe, enable, disable, drain, logs.
//!
//! A **backend** is a live upstream in the routing table. Some backends have an endpoint —
//! something ApexRouter can start and stop — and some do not: a LAN node or a managed
//! provider has no lifecycle. That distinction is enforced here rather than documented:
//! `DELETE /v1/backends/{id}` refuses anything carrying an `EndpointRef` and names the verb
//! that would actually work, because deleting the routing-table entry of a running child
//! would leave a 6 GB process nobody can reach and nobody can stop.
//!
//! Every mutation ends in [`super::recompile`], because a selector that matched nothing a
//! moment ago may match now.

use super::{persist_manual_backends, recompile, register_backend, ApiError, ApiResult};
use crate::state::AppState;
use apexrouter_core::secret::Secret;
use apexrouter_core::upstream;
use apexrouter_protocol::{
    Backend, BackendId, BackendKind, BackendLimits, BootPhase, CredentialSource, EndpointSpec,
    Event, Health, NodeSpec, Provenance, UpstreamModel,
};
use apexrouter_providers::Provisioner;
use axum::extract::{Path, Query, State};
use axum::response::sse::{Event as SseEvent, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// How long a manual probe gets before it is a finding rather than a wait.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// How often a `?follow=1` log tail looks for new bytes.
const FOLLOW_INTERVAL: Duration = Duration::from_millis(250);
/// Default `?tail=`.
const DEFAULT_TAIL: usize = 200;

/// The `/v1/backends*` set.
pub fn router() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route("/v1/backends", get(list).post(register))
        .route(
            "/v1/backends/{id}",
            get(one).patch(patch).delete(remove_backend),
        )
        .route("/v1/backends/{id}/probe", post(probe))
        .route("/v1/backends/{id}/enable", post(enable))
        .route("/v1/backends/{id}/disable", post(disable))
        .route("/v1/backends/{id}/drain", post(drain))
        .route("/v1/backends/{id}/logs", get(logs))
}

/// The subset of a backend a human may edit. Everything absent is left alone.
///
/// `base_url`, `kind` and `provenance` are deliberately **not** patchable: a backend whose
/// URL changed is a different backend, and silently repointing one would move every alias
/// that selects it without anything in the table appearing to change.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct BackendPatch {
    /// Replace the tag set.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// Rename it, for the UI.
    #[serde(default)]
    pub label: Option<String>,
    /// Concurrency, queue depth, context, slots.
    #[serde(default)]
    pub limits: Option<BackendLimits>,
    /// In or out of the table.
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// `?tail=&follow=`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct LogQuery {
    /// How many lines. Defaults to 200.
    #[serde(default)]
    pub tail: Option<usize>,
    /// `1` streams new lines as SSE instead of returning text.
    #[serde(default)]
    pub follow: Option<u8>,
}

/// `GET /v1/backends` — every live backend, from the registry rather than from disk.
pub async fn list(State(s): State<Arc<AppState>>) -> ApiResult<Vec<Backend>> {
    Ok(Json(s.router.registry().snapshot()))
}

/// `GET /v1/backends/{id}` — one backend.
pub async fn one(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult<Backend> {
    Ok(Json(lookup(&s, &id)?))
}

/// `POST /v1/backends` — register a plain OpenAI-compatible URL as a backend.
///
/// The id comes from `EndpointSpec::suggested_id()` (`node-<host>`), which is guaranteed to
/// be a valid `BackendId`, and is made unique against the registry rather than overwriting:
/// two boxes on the same host with different ports are two backends.
///
/// The registration is **not** probed. A node that is asleep is still a node, and a
/// registration that fails because the box is off would be a worse answer than a `Down`
/// row the prober will pick up. `POST /v1/backends/{id}/probe` is the explicit verb.
pub async fn register(
    State(s): State<Arc<AppState>>,
    Json(spec): Json<NodeSpec>,
) -> ApiResult<Backend> {
    if spec.base_url.trim().is_empty() {
        return Err(ApiError::bad_request("invalid", "base_url is required").with_param("base_url"));
    }
    let id = unique_id(&s, &EndpointSpec::Node(spec.clone()).suggested_id())?;
    let label = if spec.label.trim().is_empty() {
        id.to_string()
    } else {
        spec.label.clone()
    };
    let backend = Backend {
        id: id.clone(),
        kind: BackendKind::Node,
        protocol: spec.protocol,
        label,
        // INVARIANT: stored WITHOUT a trailing `/v1`. `join_v1` puts it back on exactly
        // once, which is the whole of the `/v1/v1/...` fix.
        base_url: strip_v1(&spec.base_url),
        credential: spec.credential.clone(),
        tags: vec!["node".to_owned()],
        models: spec
            .declared_models
            .iter()
            .map(|m| UpstreamModel {
                id: m.clone(),
                ctx: None,
                vision: false,
                tools: false,
            })
            .collect(),
        limits: BackendLimits::default(),
        price: None,
        health: Health::Unknown,
        provenance: Provenance::Manual,
        endpoint: None,
        enabled: true,
        devices: vec![],
        last_error: None,
    };
    register_backend(&s, backend.clone());
    persist_manual_backends(&s)?;
    Ok(Json(backend))
}

/// `PATCH /v1/backends/{id}` — tags, label, limits, enabled.
pub async fn patch(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(p): Json<BackendPatch>,
) -> ApiResult<Backend> {
    let mut b = lookup(&s, &id)?;
    if let Some(tags) = p.tags {
        b.tags = tags;
    }
    if let Some(label) = p.label {
        b.label = label;
    }
    if let Some(limits) = p.limits {
        b.limits = limits;
    }
    if let Some(enabled) = p.enabled {
        b.enabled = enabled;
        if let Some(live) = s.router.registry().get(&b.id) {
            live.accepting.store(enabled, Ordering::Release);
        }
    }
    register_backend(&s, b.clone());
    persist_manual_backends(&s)?;
    Ok(Json(b))
}

/// `DELETE /v1/backends/{id}` — deregister a backend that has no lifecycle.
///
/// Refuses anything with an `EndpointRef`: that is a process, and the verb for a process is
/// `DELETE /v1/endpoints/{id}`, which stops it first.
pub async fn remove_backend(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let b = lookup(&s, &id)?;
    if b.endpoint.is_some() {
        return Err(ApiError::conflict(format!(
            "{id} is backed by an endpoint; use DELETE /v1/endpoints/{id}, which stops the \
             process before it forgets it"
        )));
    }
    s.router.registry().remove(&b.id);
    let _ = recompile(&s);
    persist_manual_backends(&s)?;
    super::emit(&s, Event::BackendRemoved { id: b.id });
    Ok(axum::http::StatusCode::NO_CONTENT.into_response())
}

/// `POST /v1/backends/{id}/probe` — ask the upstream what it is, now.
///
/// The probe's findings are folded into the record: health, the model list, the slot count
/// and the advertised context. `slots_total` matters beyond display — R-01 sizes the
/// backend's permit pool from it, so a probe is also how a `--parallel 4` server stops being
/// treated as `--parallel 1`.
pub async fn probe(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult<Backend> {
    let mut b = lookup(&s, &id)?;
    let cred = credential_for(&b.credential).await;
    let found = upstream::probe(super::http(), &b.base_url, cred.as_ref(), PROBE_TIMEOUT).await;

    let now = super::now_unix();
    b.last_error = found.error.clone();
    if !found.models.is_empty() {
        b.models = found.models.clone();
    }
    if let Some(total) = found.slots_total {
        b.limits.slots_total = Some(total);
    }
    if let Some(ctx) = found.ctx {
        b.limits.ctx = Some(ctx);
    }
    b.health = if found.healthy {
        Health::Ready {
            since_unix: now,
            slots_busy: found.slots_busy.unwrap_or(0),
            slots_total: found.slots_total.unwrap_or(0),
            tps_p50: None,
        }
    } else if found.loading {
        Health::Starting {
            phase: BootPhase::Loading { pct: None },
            since_unix: now,
            detail: Some("upstream reports it is loading a model".to_owned()),
        }
    } else {
        Health::Down {
            reason: found
                .error
                .clone()
                .unwrap_or_else(|| "unreachable".to_owned()),
            retry_at_unix: now + 30,
        }
    };

    if let Some(live) = s.router.registry().get(&b.id) {
        live.set_models(b.models.iter().map(|m| m.id.clone()).collect());
    }
    register_backend(&s, b.clone());
    Ok(Json(b))
}

/// `POST /v1/backends/{id}/enable`.
pub async fn enable(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult<Backend> {
    set_enabled(&s, &id, true).map(Json)
}

/// `POST /v1/backends/{id}/disable` — out of the table entirely.
pub async fn disable(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult<Backend> {
    set_enabled(&s, &id, false).map(Json)
}

/// `POST /v1/backends/{id}/drain` — stop accepting, keep serving what is in flight.
///
/// The difference from `disable` is that the backend stays `enabled`, so it stays compiled
/// into the table and `resolve()` can still see it; `accepting = false` is what makes the
/// attempt loop skip it. In-flight requests keep the upstream they captured at dispatch and
/// finish against it.
///
/// `in_flight` is the **router's own** counter, not the upstream's `/slots`, which `501`s on
/// `--no-slots` builds.
pub async fn drain(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult<Backend> {
    let parsed = parse_id(&id)?;
    let live = s
        .router
        .registry()
        .get(&parsed)
        .ok_or_else(|| ApiError::not_found(format!("backend {id}")))?;
    live.accepting.store(false, Ordering::Release);
    let mut b = live.meta.load_full().as_ref().clone();
    b.health = Health::Draining {
        in_flight: live.inflight.load(Ordering::Relaxed),
    };
    register_backend(&s, b.clone());
    persist_manual_backends(&s)?;
    Ok(Json(b))
}

/// `GET /v1/backends/{id}/logs?tail=200&follow=1` — text, or SSE when following.
///
/// Only endpoint-backed backends have a log: a LAN node's log lives on the LAN node.
pub async fn logs(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<LogQuery>,
) -> Result<Response, ApiError> {
    let backend_id = parse_id(&id)?;
    let record = s
        .store
        .list_endpoints()
        .map_err(ApiError::from)?
        .into_iter()
        .find(|r| r.id == backend_id)
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "{id} has no endpoint record, so it has no local log; its log lives wherever it \
                 is running"
            ))
        })?;

    let tail = q.tail.unwrap_or(DEFAULT_TAIL);
    if q.follow.unwrap_or(0) == 0 {
        let lines = s
            .supervisor
            .logs(&backend_id, tail)
            .await
            .map_err(ApiError::from)?;
        return Ok((
            [("content-type", "text/plain; charset=utf-8")],
            lines.join("\n"),
        )
            .into_response());
    }

    let path = record
        .log_path
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| s.paths.log_file(&backend_id));
    Ok(Sse::new(follow_file(path)).into_response())
}

// ----------------------------------------------------------------------------------------
// internals
// ----------------------------------------------------------------------------------------

/// Parse a path segment into a `BackendId`, refusing an invalid one with a `400` rather
/// than letting it 404 as "not found" — the two are different problems.
fn parse_id(id: &str) -> Result<BackendId, ApiError> {
    BackendId::parse(id)
        .map_err(|e| ApiError::bad_request("bad_id", e.to_string()).with_param("id"))
}

/// One backend out of the live registry.
fn lookup(state: &Arc<AppState>, id: &str) -> Result<Backend, ApiError> {
    let parsed = parse_id(id)?;
    state
        .router
        .registry()
        .get(&parsed)
        .map(|l| l.meta.load_full().as_ref().clone())
        .ok_or_else(|| ApiError::not_found(format!("backend {id}")))
}

/// Flip `enabled` — both on the record and on the live `accepting` flag — and recompile.
fn set_enabled(state: &Arc<AppState>, id: &str, enabled: bool) -> Result<Backend, ApiError> {
    let mut b = lookup(state, id)?;
    b.enabled = enabled;
    if let Some(live) = state.router.registry().get(&b.id) {
        live.accepting.store(enabled, Ordering::Release);
    }
    register_backend(state, b.clone());
    persist_manual_backends(state)?;
    Ok(b)
}

/// `node-<host>`, made unique against the registry.
fn unique_id(state: &Arc<AppState>, suggested: &str) -> Result<BackendId, ApiError> {
    let taken: Vec<String> = state
        .router
        .registry()
        .snapshot()
        .into_iter()
        .map(|b| b.id.to_string())
        .collect();
    let base = if suggested.is_empty() {
        "node"
    } else {
        suggested
    };
    if !taken.iter().any(|t| t == base) {
        return parse_id(base);
    }
    for n in 2..1000u32 {
        let candidate = format!("{base}-{n}");
        if !taken.iter().any(|t| t == &candidate) {
            return parse_id(&candidate);
        }
    }
    Err(ApiError::conflict(format!(
        "a thousand backends are already called {base}-N"
    )))
}

/// Strip a trailing `/v1` (and any trailing slash), the invariant every `Backend.base_url`
/// carries. `join_v1` puts exactly one back on.
fn strip_v1(url: &str) -> String {
    let mut s = url.trim().trim_end_matches('/');
    while let Some(stripped) = s.strip_suffix("/v1") {
        s = stripped.trim_end_matches('/');
    }
    s.to_owned()
}

/// Turn a `CredentialSource` **description** back into a usable key, for a probe.
///
/// `Managed` and `Instance` return `None` here: the first belongs to S-07's provider
/// plumbing and the second to the vast provisioner, and guessing at either would be a
/// credential this module invented.
async fn credential_for(src: &CredentialSource) -> Option<Secret<String>> {
    let raw = match src {
        CredentialSource::None => return None,
        CredentialSource::Env { var } => std::env::var(var).ok()?,
        CredentialSource::File { path } => tokio::fs::read_to_string(path).await.ok()?,
        CredentialSource::Managed { .. } | CredentialSource::Instance => return None,
    };
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| Secret::new(trimmed.to_owned()))
}

/// Tail a file forever, one SSE event per batch of new lines.
///
/// Polling rather than `notify`: a log written by a child at a few lines a second does not
/// justify an inotify watch per follower, and S-05's watcher deliberately refuses to watch
/// the directory these files live in.
fn follow_file(
    path: std::path::PathBuf,
) -> impl futures_util::Stream<Item = Result<SseEvent, Infallible>> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    futures_util::stream::unfold((path, 0u64), |(path, mut pos)| async move {
        loop {
            let mut buf = String::new();
            if let Ok(mut f) = tokio::fs::File::open(&path).await {
                let len = f.metadata().await.map(|m| m.len()).unwrap_or(0);
                // A rotation (copytruncate) shrinks the file; start over rather than seek
                // past the end and go silent forever.
                if len < pos {
                    pos = 0;
                }
                if len > pos && f.seek(std::io::SeekFrom::Start(pos)).await.is_ok() {
                    let mut bytes = Vec::new();
                    if f.read_to_end(&mut bytes).await.is_ok() {
                        pos += bytes.len() as u64;
                        buf = String::from_utf8_lossy(&bytes).into_owned();
                    }
                }
            }
            let trimmed = buf.trim_end_matches('\n');
            if !trimmed.is_empty() {
                return Some((Ok(SseEvent::default().data(trimmed)), (path, pos)));
            }
            tokio::time::sleep(FOLLOW_INTERVAL).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn register_list_patch_delete_round_trip_through_http() {
        let state = app(test_config());
        let base = serve_api(Arc::clone(&state)).await;
        let http = reqwest::Client::new();

        // register — the documented protocol type comes back
        let res = http
            .post(format!("{base}/v1/backends"))
            .json(&serde_json::json!({
                "base_url": "http://127.0.0.1:8100/v1",
                "credential": {"kind": "none"},
                "label": "a lan node",
                "declared_models": ["carnice"],
            }))
            .send()
            .await
            .expect("post");
        assert_eq!(res.status(), 200);
        let created: Backend = res.json().await.expect("Backend");
        assert_eq!(created.id.as_str(), "node-127.0.0.1");
        assert_eq!(
            created.base_url, "http://127.0.0.1:8100",
            "the /v1 invariant is enforced at the door"
        );

        // list
        let all: Vec<Backend> = http
            .get(format!("{base}/v1/backends"))
            .send()
            .await
            .expect("get")
            .json()
            .await
            .expect("Vec<Backend>");
        assert_eq!(all.len(), 1);

        // patch
        let patched: Backend = http
            .patch(format!("{base}/v1/backends/{}", created.id))
            .json(&serde_json::json!({"tags": ["cheap"], "enabled": false}))
            .send()
            .await
            .expect("patch")
            .json()
            .await
            .expect("Backend");
        assert_eq!(patched.tags, vec!["cheap".to_owned()]);
        assert!(!patched.enabled);

        // delete
        let res = http
            .delete(format!("{base}/v1/backends/{}", created.id))
            .send()
            .await
            .expect("delete");
        assert_eq!(res.status(), 204);
        let all: Vec<Backend> = http
            .get(format!("{base}/v1/backends"))
            .send()
            .await
            .expect("get")
            .json()
            .await
            .expect("Vec<Backend>");
        assert!(all.is_empty());
    }

    #[tokio::test]
    async fn a_missing_backend_is_a_typed_error_envelope() {
        let state = app(test_config());
        let base = serve_api(Arc::clone(&state)).await;
        let res = reqwest::get(format!("{base}/v1/backends/ghost"))
            .await
            .expect("get");
        assert_eq!(res.status(), 404);
        let body: apexrouter_protocol::ErrorEnvelope = res.json().await.expect("ErrorEnvelope");
        assert_eq!(body.error.kind, "not_found");
    }

    #[tokio::test]
    async fn probe_folds_slots_and_models_into_the_record() {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&upstream)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [{"id": "carnice", "object": "model"}]
            })))
            .mount(&upstream)
            .await;
        Mock::given(method("GET"))
            .and(path("/props"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "total_slots": 4,
                "default_generation_settings": {"n_ctx": 32768}
            })))
            .mount(&upstream)
            .await;
        Mock::given(method("GET"))
            .and(path("/slots"))
            .respond_with(ResponseTemplate::new(501))
            .mount(&upstream)
            .await;

        let state = app(test_config());
        super::super::register_backend(&state, node_backend("n1", &upstream.uri(), "stale"));
        let base = serve_api(Arc::clone(&state)).await;

        let b: Backend = reqwest::Client::new()
            .post(format!("{base}/v1/backends/n1/probe"))
            .send()
            .await
            .expect("probe")
            .json()
            .await
            .expect("Backend");
        assert!(matches!(b.health, Health::Ready { .. }), "{:?}", b.health);
        assert_eq!(b.limits.slots_total, Some(4));
        assert!(b.models.iter().any(|m| m.id == "carnice"));
    }

    #[tokio::test]
    async fn draining_a_backend_stops_it_accepting_without_disabling_it() {
        let state = app(test_config());
        super::super::register_backend(&state, node_backend("n1", "http://127.0.0.1:9", "m"));
        let base = serve_api(Arc::clone(&state)).await;
        let b: Backend = reqwest::Client::new()
            .post(format!("{base}/v1/backends/n1/drain"))
            .send()
            .await
            .expect("drain")
            .json()
            .await
            .expect("Backend");
        assert!(b.enabled, "draining keeps it in the table");
        assert!(matches!(b.health, Health::Draining { .. }));
        let live = state
            .router
            .registry()
            .get(&BackendId::parse("n1").expect("id"))
            .expect("live");
        assert!(!live.accepting.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn a_backend_with_an_endpoint_cannot_be_deleted_here() {
        let state = app(test_config());
        let mut b = node_backend("local-carnice", "http://127.0.0.1:8100", "carnice");
        b.endpoint = Some(apexrouter_protocol::EndpointRef {
            id: b.id.clone(),
            kind: BackendKind::LocalLlama,
        });
        super::super::register_backend(&state, b);
        let base = serve_api(Arc::clone(&state)).await;
        let res = reqwest::Client::new()
            .delete(format!("{base}/v1/backends/local-carnice"))
            .send()
            .await
            .expect("delete");
        assert_eq!(res.status(), 409);
        let body: apexrouter_protocol::ErrorEnvelope = res.json().await.expect("ErrorEnvelope");
        assert!(
            body.error.message.contains("/v1/endpoints/"),
            "the refusal names the verb that works: {}",
            body.error.message
        );
    }
}
