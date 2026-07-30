//! OWNER: unit R-08 (router/src/lib.rs, router/src/handler.rs). Do not edit outside that
//! unit.
//!
//! The proxy listener's wiring, and the `(ingress, upstream)` matrix dispatch.
//!
//! The catch-all is registered with `.fallback(any(proxy_handler))` — **never** as a
//! `/{*path}` route — because a catch-all `any()` route and the static-asset
//! `get("/{*path}")` route panic on `Router::merge` in axum 0.8 ("Overlapping method
//! route"). There is an explicit `merge_does_not_panic` test.
//!
//! The matrix (`ARCHITECTURE.md` §3.4), all four cells owned here:
//!
//! | ingress → upstream | behaviour |
//! |---|---|
//! | `OpenAi` → `OpenAi` | relay, byte-for-byte |
//! | `Anthropic` → `Anthropic` | passthrough relay; only the credential is swapped |
//! | `Anthropic` → `OpenAi` | call into [`crate::anthropic`] (unit R-10) |
//! | `OpenAi` → `Anthropic` | **501** with an **OpenAI-shaped** body. Permanently out of scope |
//!
//! The pipeline is `ARCHITECTURE.md` §4.3, in order: loop guard, `/v1` normalisation,
//! ingress, classification, body under the byte budget, peek, `resolve()`, then a retry loop
//! bounded by `RetryPolicy::attempts` **and** a wall-clock deadline. The first upstream byte
//! commits the request; there is no retry past that point.

use crate::attempt::{attempt, Committed, PreFlight, Retryable};
use crate::breaker::BreakerDecision;
use crate::errors::{map_status, openai_error};
use crate::limits::{InFlightGuard, LimitError};
use crate::models::{aggregate_models, one_model};
use crate::relay::{
    normalize_path, outbound_headers, peek, plan_body, response_headers, RequestPeek,
};
use crate::resolve::{RequestClass, UnknownModelPolicy};
use crate::{Router, COLLAPSE_LOG_CAPACITY, RING_CAPACITY};

use apexrouter_core::secret::Secret;
use apexrouter_core::upstream::{join_v1, parse_timings, parse_usage, Timings, UsageFields};
use apexrouter_protocol::{
    Alias, BackendId, CostEstimate, CredentialSource, Event, Protocol, RequestId, RequestRecord,
    RetryPolicy, RouteReason, TokenCount, UsageRecord,
};
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// The `Via` token this proxy stamps, and refuses to see twice.
const VIA_TOKEN: &str = "apexrouter";
/// How much of a stream's tail is kept for the usage tee. Bounded — a stream is not.
const TEE_TAIL_BYTES: usize = 64 * 1024;
/// The one synthetic frame emitted when an upstream dies mid-stream, plus `[DONE]`.
const MID_STREAM_DEATH: &[u8] = concat!(
    "data: {\"error\":{\"message\":\"upstream ended mid-stream\",",
    "\"type\":\"upstream_unavailable\"}}\n\n",
    "data: [DONE]\n\n"
)
.as_bytes();

/// The axum `Router` for the PROXY listener.
///
/// Only the three legacy compat routes (R-09) are registered explicitly. Everything the
/// drop-in contract in `ARCHITECTURE.md` §6.1 lists — `/v1/chat/completions`, `/v1/models`,
/// `/v1/messages`, `/slots` and the opaque catch-all — is served by [`proxy_handler`]
/// through the fallback, because a static route cannot express `/v1` normalisation: a client
/// that sends `/v1/v1/models` must be answered, and `Router::route` would 404 it before any
/// handler ran.
///
/// Each legacy `MethodRouter` carries its **own** `.fallback(proxy_handler)`. LocalRouter
/// intercepted exactly five `(path, method)` pairs and proxied everything else, `POST /health`
/// included (`docs/port/05-proxy.md` §2, §15 item 6); in axum 0.8 an unmatched method on a
/// matched path hits the `MethodRouter`'s own 405 and never reaches `Router::fallback`, so
/// without these three lines `POST /health` would 405 instead of being forwarded.
pub fn proxy_router(r: Router) -> axum::Router {
    axum::Router::new()
        .route(
            "/health",
            get(crate::compat::legacy_health)
                .head(crate::compat::legacy_health)
                .fallback(proxy_handler),
        )
        .route(
            "/providers",
            get(crate::compat::legacy_providers)
                .head(crate::compat::legacy_providers)
                .fallback(proxy_handler),
        )
        .route(
            "/switch",
            post(crate::compat::legacy_switch).fallback(proxy_handler),
        )
        .fallback(any(proxy_handler))
        .with_state(r)
}

/// The catch-all handler: normalise, classify, peek, resolve, dispatch, relay.
///
/// Records `RequestRecord::ingress`, and emits `X-ApexRouter-Protocol: <ingress>-><upstream>`
/// whenever the ingress is not `open_ai`, so which matrix cell ran is observable exactly
/// like `X-ApexRouter-Route`.
pub async fn proxy_handler(State(r): State<Router>, req: axum::extract::Request) -> Response {
    let started = Instant::now();
    let id = RequestId::new();
    let cfg = r.cfg.load_full();

    let (parts, body) = req.into_parts();
    let method = parts.method;
    let headers = parts.headers;
    let raw_path = parts.uri.path().to_owned();
    let query = parts.uri.query().map(str::to_owned);

    // ---- loop guard, before anything can cost money -------------------------------------
    if via_loops(&headers) {
        return stamp(
            error_response(
                Protocol::OpenAi,
                StatusCode::LOOP_DETECTED,
                "loop_detected",
                "this request already carries a Via: apexrouter token",
            ),
            Stamp::pre(&id, Protocol::OpenAi),
        );
    }

    // ---- /v1 normalisation ----------------------------------------------------------------
    let (path, collapsed) = normalize_path(&raw_path);
    if collapsed {
        note_collapse(&r, &headers, &path).await;
    }

    // ---- ingress ---------------------------------------------------------------------------
    let ingress = detect_ingress(&path, &headers);

    // ---- everything answered without an upstream hop -----------------------------------------
    if is_slots(&path) {
        return stamp(
            error_response(
                ingress,
                StatusCode::FORBIDDEN,
                "redacted_endpoint",
                "/slots echoes prompts and is never proxied outward",
            ),
            Stamp::pre(&id, ingress),
        );
    }

    if path == "/v1/messages/count_tokens" {
        return stamp(
            error_response(
                Protocol::Anthropic,
                StatusCode::NOT_IMPLEMENTED,
                "not_implemented",
                "/v1/messages/count_tokens is not implemented in mk1",
            ),
            Stamp::pre(&id, ingress),
        );
    }

    let class = classify(&path);

    if class == RequestClass::Models && (method == Method::GET || method == Method::HEAD) {
        return stamp(
            models_response(&r, &path, ingress),
            Stamp::pre(&id, ingress),
        );
    }

    if path == "/v1/messages" && !cfg.router.anthropic_ingress {
        return stamp(
            error_response(
                Protocol::Anthropic,
                StatusCode::NOT_IMPLEMENTED,
                "not_implemented",
                "[router] anthropic_ingress is off",
            ),
            Stamp::pre(&id, ingress),
        );
    }

    // ---- body, under the per-request cap ------------------------------------------------------
    let max_body = cfg.router.max_body_bytes as usize;
    let bytes = match axum::body::to_bytes(body, max_body).await {
        Ok(b) => b,
        Err(_) => {
            return stamp(
                error_response(
                    ingress,
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "request_too_large",
                    &format!("request body exceeds the {max_body} byte limit"),
                ),
                Stamp::pre(&id, ingress),
            )
        }
    };

    // ---- peek, then resolve ---------------------------------------------------------------------
    let pk: RequestPeek = peek(&bytes);
    let unknown = unknown_policy(&cfg.router.unknown_model);
    let plan = {
        let table = r.table();
        table.resolve(pk.model.as_deref(), class, unknown)
    };
    let plan = match plan {
        Ok(p) => p,
        Err(e) => {
            let (status, kind) = map_status(&e);
            return stamp(
                error_response(ingress, status, kind, &e.to_string()),
                Stamp::pre(&id, ingress),
            );
        }
    };

    let mut draft = RecordDraft {
        id,
        started,
        started_unix: chrono::Utc::now().timestamp(),
        alias: plan.alias.clone(),
        backend: None,
        upstream_model: None,
        route_reason: plan.reason,
        ingress,
        method: method.to_string(),
        path: path.clone(),
        attempts: 0,
        fallback: false,
    };

    // ---- the retry loop ---------------------------------------------------------------------------
    let policy = RetryPolicy::default();
    // `headers_timeout_ms` is what ONE attempt gets to produce response headers, and it is
    // long by design: a non-streaming completion on a 100B model sends none until generation
    // finishes. The wall-clock budget therefore has to span every attempt the policy allows.
    // Setting it to a single `headers_timeout_ms` would mean the first wedged candidate eats
    // the whole budget and `Instant::now() >= deadline` breaks the loop before the healthy
    // backend beside it is ever tried — a 504 next to an idle GPU.
    let deadline = started
        + Duration::from_millis(cfg.router.headers_timeout_ms)
            .saturating_mul(u32::from(policy.attempts.max(1)));
    let queue_timeout = Duration::from_millis(cfg.router.queue_timeout_ms);
    let mut last: Option<Retryable> = None;

    for (index, cand) in plan.candidates.iter().enumerate() {
        if draft.attempts >= policy.attempts || Instant::now() >= deadline {
            break;
        }
        if draft.attempts > 0 && !policy.failover {
            break;
        }
        let meta = cand.backend.meta.load_full();
        if !meta.enabled || !cand.backend.accepting.load(Ordering::Relaxed) {
            continue;
        }

        // ---- the (ingress, upstream) matrix cell, ARCHITECTURE §3.4 --------------------------
        match (ingress, meta.protocol) {
            (Protocol::OpenAi, Protocol::Anthropic) => {
                // Permanently out of scope (§12). The body is **OpenAI**-shaped, because the
                // client is an OpenAI SDK and will parse it as one.
                return stamp(
                    openai_error(
                        StatusCode::NOT_IMPLEMENTED,
                        "protocol_not_supported",
                        "open_ai -> anthropic translation is out of scope",
                    ),
                    draft.stamp_cell(&meta.id, meta.protocol),
                );
            }
            (Protocol::Anthropic, Protocol::OpenAi) => {
                // R-10 (Stage 5) fills this cell in. The documented Stage 3 behaviour is a
                // 501 with an **Anthropic**-shaped body — the client is an Anthropic SDK.
                return stamp(
                    error_response(
                        Protocol::Anthropic,
                        StatusCode::NOT_IMPLEMENTED,
                        "not_implemented",
                        "anthropic -> open_ai translation is not built yet (unit R-10)",
                    ),
                    draft.stamp_cell(&meta.id, meta.protocol),
                );
            }
            // OpenAi -> OpenAi and Anthropic -> Anthropic are both the byte relay below.
            _ => {}
        }

        if let BreakerDecision::Deny { .. } = cand.backend.breaker.check() {
            continue;
        }

        let mut guard =
            match InFlightGuard::acquire(&cand.backend, pk.bytes, &r.inflight_bytes, queue_timeout)
                .await
            {
                Ok(g) => g,
                Err(LimitError::NotAccepting) => continue,
                Err(e @ (LimitError::QueueTimeout { .. } | LimitError::ShuttingDown)) => {
                    let mut resp = error_response(
                        ingress,
                        StatusCode::SERVICE_UNAVAILABLE,
                        "server_overloaded",
                        &e.to_string(),
                    );
                    set(resp.headers_mut(), "retry-after", "1");
                    return stamp(resp, draft.stamp_cell(&meta.id, meta.protocol));
                }
                Err(e @ LimitError::TooLarge { .. }) => {
                    return stamp(
                        error_response(
                            ingress,
                            StatusCode::PAYLOAD_TOO_LARGE,
                            "request_too_large",
                            &e.to_string(),
                        ),
                        draft.stamp_cell(&meta.id, meta.protocol),
                    );
                }
            };

        // The guard is what emits `RequestFinished` — on `finish()`, or as
        // `{ aborted: true }` from its `Drop` when the client vanished. Handing it the
        // broadcast here is what makes a client Ctrl-C visible instead of a zombie UI row.
        guard.events = Some(r.events.clone());

        draft.attempts = draft.attempts.saturating_add(1);
        draft.fallback = index > 0;
        draft.backend = Some(meta.id.clone());
        draft.upstream_model = Some(cand.upstream_model.clone());

        if r.events.receiver_count() > 0 {
            let _ = r.events.send(Event::RequestStarted {
                id,
                alias: draft.alias.clone(),
                backend: draft.backend.clone(),
            });
        }

        // A body that is not a JSON object has no `model` to rewrite — a multipart
        // `/v1/audio/transcriptions` upload, an empty `POST /health`, a llama.cpp-native
        // `POST /tokenize`. LocalRouter forwarded those verbatim and let the upstream judge
        // them, and `05-proxy.md` §15 item 11 ("pass upstream status codes and bodies
        // through unchanged") means we must not invent a 400 the upstream would not have
        // sent. So a failed rewrite degrades to a byte-verbatim relay, never to an error.
        let body_plan = plan_body(&bytes, plan.rewrite_model_to.as_deref()).unwrap_or_else(|_| {
            tracing::debug!(
                path = %path,
                "body is not a JSON object; relaying verbatim without a model rewrite"
            );
            crate::relay::BodyPlan::Passthrough(bytes.clone())
        });

        // Outbound headers are CONSTRUCTED: the inbound map is never cloned, so a client's
        // `Authorization` — or an Anthropic client's `x-api-key` — cannot reach a third party.
        let cred = credential_for(&meta.credential).await;
        let mut out_headers = outbound_headers(&headers, cred.as_ref(), &[]);
        if !out_headers.contains_key("x-request-id") {
            set(&mut out_headers, "x-request-id", &id.to_string());
        }

        // `attempt` CONSUMES this `PreFlight`, and a retry has to build a new one against
        // the next candidate. "Never retry after the first byte" is therefore unrepresentable
        // here rather than merely documented — and the breaker bookkeeping lives inside
        // `attempt`, so this loop never double-counts an outcome.
        let pre = PreFlight {
            candidate: cand,
            http: &r.http,
            method: method.clone(),
            url: upstream_url(&meta.base_url, &path, query.as_deref()),
            headers: out_headers,
            body: body_plan,
            deadline,
            cfg: &cfg.router,
            guard,
        };

        match attempt(pre).await {
            Ok(Committed { response, guard }) => {
                let Some(guard) = guard else {
                    return stamp(
                        error_response(
                            ingress,
                            StatusCode::BAD_GATEWAY,
                            "upstream_unavailable",
                            "the committed upstream response arrived without its in-flight guard",
                        ),
                        draft.stamp_cell(&meta.id, meta.protocol),
                    );
                };
                let protocol = meta.protocol;
                return relay(
                    r.clone(),
                    response,
                    draft,
                    guard,
                    pk.stream,
                    cfg.router.log_usage,
                    protocol,
                )
                .await;
            }
            Err(why) => {
                // The guard moved into the `PreFlight` and was dropped there: its permits are
                // back, and because it was never armed no `aborted` record was emitted for an
                // attempt the client never saw.
                last = Some(why);
                continue;
            }
        }
    }

    // ---- nothing worked -----------------------------------------------------------------------------
    let (status, kind, msg) = match &last {
        Some(Retryable::Timeout) => (
            StatusCode::GATEWAY_TIMEOUT,
            "upstream_timeout",
            "no upstream produced response headers in time".to_owned(),
        ),
        Some(Retryable::Connect(e)) => (StatusCode::BAD_GATEWAY, "upstream_unavailable", e.clone()),
        Some(Retryable::Status { code, .. }) => (
            StatusCode::BAD_GATEWAY,
            "upstream_unavailable",
            format!("every upstream attempt failed; the last returned {code}"),
        ),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "no_healthy_backend",
            "every candidate was skipped: breaker open, draining or disabled".to_owned(),
        ),
    };
    stamp(
        error_response(ingress, status, kind, &msg),
        draft.stamp_failed(),
    )
}

// ==========================================================================================
// the pipeline steps, in the order §4.3 lists them
// ==========================================================================================

/// `Via` already names us, so this request has been round-tripped through the proxy.
fn via_loops(headers: &HeaderMap) -> bool {
    headers
        .get_all("via")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| v.to_ascii_lowercase().contains(VIA_TOKEN))
}

/// Log a collapsed doubled `/v1` once per `(user-agent, path)`, at debug.
///
/// A genuinely broken client must stay discoverable; a busy one must not flood the log.
async fn note_collapse(r: &Router, headers: &HeaderMap, path: &str) {
    let ua = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_owned();
    let key = (ua, path.to_owned());
    let mut seen = r.collapse_seen.lock().await;
    if seen.len() >= COLLAPSE_LOG_CAPACITY || !seen.insert(key.clone()) {
        return;
    }
    tracing::debug!(
        user_agent = %key.0,
        path = %key.1,
        "collapsed a doubled leading /v1 — this client is sending its base URL twice"
    );
}

/// Which dialect the **client** spoke.
///
/// `Anthropic` for `/v1/messages` and its sub-paths, or when an `anthropic-version` header is
/// present on `/v1/models`. `OpenAi` otherwise — and `GET /v1/models` without that header
/// stays byte-exact OpenAI, because ApexOS's LAN compute sweep identifies a node by exactly
/// that shape.
fn detect_ingress(path: &str, headers: &HeaderMap) -> Protocol {
    if path == "/v1/messages" || path.starts_with("/v1/messages/") {
        return Protocol::Anthropic;
    }
    if (path == "/v1/models" || path.starts_with("/v1/models/"))
        && headers.contains_key("anthropic-version")
    {
        return Protocol::Anthropic;
    }
    Protocol::OpenAi
}

/// llama.cpp's `/slots` echoes prompts and is never proxied outward.
fn is_slots(path: &str) -> bool {
    path == "/slots" || path == "/v1/slots" || path.starts_with("/slots/")
}

/// Which class of request this is. Drives which backends are eligible.
fn classify(path: &str) -> RequestClass {
    match path {
        p if p == "/v1/models" || p.starts_with("/v1/models/") => RequestClass::Models,
        "/v1/chat/completions" | "/v1/messages" => RequestClass::Chat,
        "/v1/completions" => RequestClass::Completion,
        "/v1/embeddings" => RequestClass::Embedding,
        "/v1/rerank" | "/v1/reranking" => RequestClass::Rerank,
        _ => RequestClass::Opaque,
    }
}

/// The `{id}` of `/v1/models/{id}`, when there is one.
fn model_id_of(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/v1/models/")?;
    if rest.is_empty() {
        None
    } else {
        Some(rest)
    }
}

/// The aggregated model list, served **from the table with no upstream hop**.
fn models_response(r: &Router, path: &str, ingress: Protocol) -> Response {
    let table = r.table();
    let body = match model_id_of(path) {
        Some(one) => match one_model(&table, one) {
            Some(v) => v,
            None => {
                return error_response(
                    ingress,
                    StatusCode::NOT_FOUND,
                    "model_not_found",
                    &format!("no model or alias named {one}"),
                )
            }
        },
        None => aggregate_models(&table),
    };
    let body = if ingress == Protocol::Anthropic {
        anthropic_model_view(body)
    } else {
        body
    };
    axum::Json(body).into_response()
}

/// `[router] unknown_model`, defaulting to the documented `reject`.
fn unknown_policy(configured: &str) -> UnknownModelPolicy {
    if configured.eq_ignore_ascii_case("fallback") {
        UnknownModelPolicy::Fallback
    } else {
        UnknownModelPolicy::Reject
    }
}

/// The upstream URL: the backend's base (stored without `/v1`), the normalised inbound path
/// and the original query string.
fn upstream_url(base_url: &str, path: &str, query: Option<&str>) -> String {
    let joined = join_v1(base_url, path);
    match query {
        Some(q) if !q.is_empty() => format!("{joined}?{q}"),
        _ => joined,
    }
}

/// The backend's own credential, resolved from its *description*.
///
/// `Managed`/`Instance` sources need `Paths` and the credential store, which the request path
/// deliberately does not hold; they resolve to `None` here and are supplied by the supervisor
/// when it builds the backend.
async fn credential_for(src: &CredentialSource) -> Option<Secret<String>> {
    let raw = match src {
        CredentialSource::Env { var } => std::env::var(var).ok()?,
        CredentialSource::File { path } => tokio::fs::read_to_string(path).await.ok()?,
        CredentialSource::None | CredentialSource::Managed { .. } | CredentialSource::Instance => {
            return None
        }
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(Secret::new(trimmed.to_owned()))
    }
}

// ==========================================================================================
// relay
// ==========================================================================================

/// Everything known about a request before its outcome.
struct RecordDraft {
    id: RequestId,
    started: Instant,
    started_unix: i64,
    alias: Option<Alias>,
    backend: Option<BackendId>,
    upstream_model: Option<String>,
    route_reason: RouteReason,
    ingress: Protocol,
    method: String,
    path: String,
    attempts: u8,
    fallback: bool,
}

impl RecordDraft {
    /// Seal the record with its outcome.
    fn seal(
        &self,
        status: u16,
        streamed: bool,
        ttft_ms: Option<u32>,
        usage: Option<UsageFields>,
        timings: Option<Timings>,
        error: Option<String>,
    ) -> RequestRecord {
        RequestRecord {
            id: self.id,
            started_unix: self.started_unix,
            alias: self.alias.clone(),
            backend: self.backend.clone(),
            upstream_model: self.upstream_model.clone(),
            route_reason: self.route_reason,
            ingress: self.ingress,
            method: self.method.clone(),
            path: self.path.clone(),
            status,
            attempts: self.attempts,
            streamed,
            aborted: false,
            ttft_ms,
            total_ms: millis(self.started.elapsed()),
            prompt_tokens: usage.map(|u| TokenCount::Reported(u.prompt_tokens)),
            completion_tokens: usage.map(|u| TokenCount::Reported(u.completion_tokens)),
            cached_tokens: usage
                .and_then(|u| u.cached_tokens)
                .or_else(|| timings.map(|t| t.cache_n)),
            tok_per_s: timings.map(|t| t.predicted_per_second),
            cost: CostEstimate::Unknown,
            error,
        }
    }

    /// Header set for a matrix cell that answered without an upstream hop.
    fn stamp_cell<'a>(&'a self, backend: &'a BackendId, upstream: Protocol) -> Stamp<'a> {
        Stamp {
            id: &self.id,
            alias: self.alias.as_ref(),
            reason: Some(self.route_reason),
            backend: Some(backend),
            attempts: self.attempts,
            fallback: self.fallback,
            ingress: self.ingress,
            upstream: Some(upstream),
        }
    }

    /// Header set for a request that exhausted every candidate.
    fn stamp_failed(&self) -> Stamp<'_> {
        Stamp {
            id: &self.id,
            alias: self.alias.as_ref(),
            reason: Some(self.route_reason),
            backend: self.backend.as_ref(),
            attempts: self.attempts,
            fallback: self.fallback,
            ingress: self.ingress,
            upstream: None,
        }
    }
}

/// Relay a committed upstream response. **Past this point there is no retry.**
#[allow(clippy::too_many_arguments)]
async fn relay(
    r: Router,
    up: reqwest::Response,
    draft: RecordDraft,
    mut guard: InFlightGuard,
    client_asked_to_stream: bool,
    log_usage: bool,
    upstream: Protocol,
) -> Response {
    guard.mark_first_byte();
    let ttft_ms = Some(millis(draft.started.elapsed()));

    let status = up.status();
    let says_sse = up
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim_start().starts_with("text/event-stream"))
        .unwrap_or(false);
    // `text/event-stream` is forced ONLY when upstream is 2xx *and* already says so: a
    // `400 {"error":…}` on a `stream: true` request must reach the client as JSON.
    let streaming = client_asked_to_stream && status.is_success() && says_sse;

    let mut head = response_headers(up.headers());
    let idle = Duration::from_millis(r.cfg.load().router.idle_timeout_ms);

    if streaming {
        set(&mut head, "content-type", "text/event-stream");
        set(&mut head, "cache-control", "no-cache");
        set(&mut head, "x-accel-buffering", "no");
        // Response headers flush before the first chunk and usage arrives in the last one,
        // so a streaming `X-Usage` would be absent or a lie.
        set(&mut head, "x-apexrouter-usage-deferred", "true");

        let stamp_set = Stamp {
            id: &draft.id,
            alias: draft.alias.as_ref(),
            reason: Some(draft.route_reason),
            backend: draft.backend.as_ref(),
            attempts: draft.attempts,
            fallback: draft.fallback,
            ingress: draft.ingress,
            upstream: Some(upstream),
        };
        apply(&mut head, stamp_set);

        let relay = StreamRelay {
            inner: Some(Box::pin(up.bytes_stream())),
            idle,
            router: r.clone(),
            draft,
            guard: Some(guard),
            status: status.as_u16(),
            ttft_ms,
            tail: Vec::new(),
            log_usage,
            done: false,
        };
        let body = Body::from_stream(futures_util::stream::unfold(relay, step));
        let mut resp = Response::new(body);
        *resp.status_mut() = status;
        *resp.headers_mut() = head;
        return resp;
    }

    // ---- buffered ---------------------------------------------------------------------------
    let body = match tokio::time::timeout(idle, up.bytes()).await {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => {
            let msg = e.to_string();
            finalize(
                &r,
                &draft,
                Some(guard),
                Outcome {
                    status: StatusCode::BAD_GATEWAY.as_u16(),
                    streamed: false,
                    ttft_ms,
                    usage: None,
                    timings: None,
                    error: Some(msg.clone()),
                },
                log_usage,
            )
            .await;
            return stamp(
                error_response(
                    draft.ingress,
                    StatusCode::BAD_GATEWAY,
                    "upstream_unavailable",
                    &msg,
                ),
                draft.stamp_failed(),
            );
        }
        Err(_) => {
            finalize(
                &r,
                &draft,
                Some(guard),
                Outcome {
                    status: StatusCode::GATEWAY_TIMEOUT.as_u16(),
                    streamed: false,
                    ttft_ms,
                    usage: None,
                    timings: None,
                    error: Some("upstream idle timeout".to_owned()),
                },
                log_usage,
            )
            .await;
            return stamp(
                error_response(
                    draft.ingress,
                    StatusCode::GATEWAY_TIMEOUT,
                    "upstream_timeout",
                    "upstream stopped sending before the body was complete",
                ),
                draft.stamp_failed(),
            );
        }
    };

    let parsed: Option<serde_json::Value> = serde_json::from_slice(&body).ok();
    let usage = parsed.as_ref().and_then(parse_usage);
    let timings = parsed.as_ref().and_then(parse_timings);
    // `X-Usage` is emitted on buffered responses ONLY. On streams the numbers land in
    // `usage.jsonl`, the WS event and the live-request table instead.
    //
    // The wire format is LocalRouter's, verbatim — `"{prompt}+{completion}"`, e.g.
    // `"131072+500"` (`docs/port/05-proxy.md` §5, §15 item 9). ARCHITECTURE §4.5 lists this
    // header under "preserved for compat", so its spelling is part of the drop-in contract
    // and not ours to improve.
    if let Some(u) = usage {
        set(
            &mut head,
            "x-usage",
            &format!("{}+{}", u.prompt_tokens, u.completion_tokens),
        );
    }

    finalize(
        &r,
        &draft,
        Some(guard),
        Outcome {
            status: status.as_u16(),
            streamed: false,
            ttft_ms,
            usage,
            timings,
            error: None,
        },
        log_usage,
    )
    .await;

    let mut resp = Response::new(Body::from(body));
    *resp.status_mut() = status;
    *resp.headers_mut() = head;
    stamp(
        resp,
        Stamp {
            id: &draft.id,
            alias: draft.alias.as_ref(),
            reason: Some(draft.route_reason),
            backend: draft.backend.as_ref(),
            attempts: draft.attempts,
            fallback: draft.fallback,
            ingress: draft.ingress,
            upstream: Some(upstream),
        },
    )
}

/// The state the SSE relay carries between chunks.
///
/// It owns the `InFlightGuard`, so a client disconnect drops this struct, cancels the
/// `reqwest` future, aborts the upstream — freeing llama.cpp's slot — and leaves exactly one
/// `RequestFinished { aborted: true }` to the guard's `Drop`.
struct StreamRelay {
    inner: Option<Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>>,
    idle: Duration,
    router: Router,
    draft: RecordDraft,
    guard: Option<InFlightGuard>,
    status: u16,
    ttft_ms: Option<u32>,
    /// Bounded tail, for the best-effort usage tee. Never delays a byte.
    tail: Vec<u8>,
    log_usage: bool,
    done: bool,
}

impl StreamRelay {
    /// Feed the tee. Bounded, so an hour-long stream costs 64 KiB, not an hour of bytes.
    fn feed(&mut self, chunk: &[u8]) {
        self.tail.extend_from_slice(chunk);
        if self.tail.len() > TEE_TAIL_BYTES {
            let cut = self.tail.len() - TEE_TAIL_BYTES;
            self.tail.drain(..cut);
        }
    }
}

/// One step of the SSE relay. Bytes go through **verbatim** — never re-framed, because a
/// chunk boundary may split an event and every OpenAI SDK buffers on `\n\n`.
async fn step(mut st: StreamRelay) -> Option<(Result<Bytes, std::io::Error>, StreamRelay)> {
    if st.done {
        return None;
    }
    let next = {
        let inner = st.inner.as_mut()?;
        // There is NEVER a total timeout on a stream — only this inter-chunk idle gap.
        tokio::time::timeout(st.idle, inner.next()).await
    };
    match next {
        Err(_) => {
            st.done = true;
            finish_stream(&mut st, Some("stream idle timeout".to_owned())).await;
            None
        }
        Ok(None) => {
            st.done = true;
            finish_stream(&mut st, None).await;
            None
        }
        Ok(Some(Ok(chunk))) => {
            st.feed(&chunk);
            Some((Ok(chunk), st))
        }
        // Mid-stream upstream death has a defined client-visible behaviour: exactly one
        // synthetic error frame plus `[DONE]`. Never a silent truncation.
        Ok(Some(Err(e))) => {
            st.done = true;
            st.inner = None;
            finish_stream(&mut st, Some(e.to_string())).await;
            Some((Ok(Bytes::from_static(MID_STREAM_DEATH)), st))
        }
    }
}

/// Seal the record for a finished stream.
async fn finish_stream(st: &mut StreamRelay, error: Option<String>) {
    let (usage, timings) = scan_tail(&st.tail);
    let guard = st.guard.take();
    finalize(
        &st.router,
        &st.draft,
        guard,
        Outcome {
            status: st.status,
            streamed: true,
            ttft_ms: st.ttft_ms,
            usage,
            timings,
            error,
        },
        st.log_usage,
    )
    .await;
}

/// Best-effort: the last `data:` frame that carried a `usage` or `timings` object.
///
/// A malformed tail is not an error — the record simply degrades, which is what
/// `TokenCount::Estimated` exists to say.
fn scan_tail(tail: &[u8]) -> (Option<UsageFields>, Option<Timings>) {
    let text = String::from_utf8_lossy(tail);
    let mut usage = None;
    let mut timings = None;
    for line in text.lines().rev() {
        let payload = line.strip_prefix("data:").unwrap_or(line).trim();
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) else {
            continue;
        };
        if usage.is_none() {
            usage = parse_usage(&v);
        }
        if timings.is_none() {
            timings = parse_timings(&v);
        }
        if usage.is_some() && timings.is_some() {
            break;
        }
    }
    (usage, timings)
}

/// What a finished request turned out to be.
struct Outcome {
    status: u16,
    streamed: bool,
    ttft_ms: Option<u32>,
    usage: Option<UsageFields>,
    timings: Option<Timings>,
    error: Option<String>,
}

/// Release the guard, broadcast, ring-buffer and append the usage row — exactly once.
async fn finalize(
    r: &Router,
    draft: &RecordDraft,
    guard: Option<InFlightGuard>,
    outcome: Outcome,
    log_usage: bool,
) {
    let rec = draft.seal(
        outcome.status,
        outcome.streamed,
        outcome.ttft_ms,
        outcome.usage,
        outcome.timings,
        outcome.error,
    );
    // Exactly one `RequestFinished` per request: the guard broadcasts it, and its `Drop`
    // broadcasts the `aborted: true` variant when `finish()` never ran. The handler only
    // emits directly if there is no guard to do it — which cannot happen on a served
    // request, and is here so a future call site cannot silently lose a record.
    match guard {
        Some(g) => g.finish(rec.clone()),
        None => {
            // Only serialised when somebody is listening: a router at 50 rps must not drown
            // its own dashboard.
            if r.events.receiver_count() > 0 {
                let _ = r.events.send(Event::RequestFinished {
                    record: Box::new(rec.clone()),
                });
            }
        }
    }
    {
        let mut ring = r.ring.lock().await;
        while ring.len() >= RING_CAPACITY {
            ring.pop_front();
        }
        ring.push_back(rec.clone());
    }
    if log_usage {
        if let Err(e) = r.usage.append(&usage_row(&rec)) {
            tracing::warn!(error = %e, "usage row not appended");
        }
    }
}

/// The legacy-shaped usage row for a finished request.
fn usage_row(rec: &RequestRecord) -> UsageRecord {
    UsageRecord {
        timestamp: chrono::Utc::now().to_rfc3339(),
        epoch: None,
        provider: rec
            .backend
            .as_ref()
            .map(BackendId::to_string)
            .unwrap_or_else(|| "unknown".to_owned()),
        model_id: rec.upstream_model.clone().unwrap_or_default(),
        prompt_tokens: rec.prompt_tokens.map(TokenCount::value).unwrap_or(0),
        completion_tokens: rec.completion_tokens.map(TokenCount::value).unwrap_or(0),
        cost_usd: 0.0,
        request_id: Some(rec.id.to_string()),
        backend: rec.backend.as_ref().map(BackendId::to_string),
        alias: rec.alias.as_ref().map(Alias::to_string),
        ttft_ms: rec.ttft_ms,
        tok_per_s: rec.tok_per_s,
        stream: Some(rec.streamed),
        estimated: Some(rec.prompt_tokens.is_none()),
        extra: serde_json::Map::new(),
    }
}

/// Milliseconds, saturating rather than wrapping.
fn millis(d: Duration) -> u32 {
    d.as_millis().min(u128::from(u32::MAX)) as u32
}

// ==========================================================================================
// responses
// ==========================================================================================

/// The observability header set every response carries, error or not.
struct Stamp<'a> {
    id: &'a RequestId,
    alias: Option<&'a Alias>,
    reason: Option<RouteReason>,
    backend: Option<&'a BackendId>,
    attempts: u8,
    fallback: bool,
    ingress: Protocol,
    upstream: Option<Protocol>,
}

impl<'a> Stamp<'a> {
    /// The set for a response produced before `resolve()` ran: no alias, no reason, no
    /// backend, zero attempts.
    fn pre(id: &'a RequestId, ingress: Protocol) -> Stamp<'a> {
        Stamp {
            id,
            alias: None,
            reason: None,
            backend: None,
            attempts: 0,
            fallback: false,
            ingress,
            upstream: None,
        }
    }
}

/// Apply a [`Stamp`] to a response.
fn stamp(mut resp: Response, s: Stamp<'_>) -> Response {
    apply(resp.headers_mut(), s);
    resp
}

/// Apply a [`Stamp`] to a header map.
fn apply(h: &mut HeaderMap, s: Stamp<'_>) {
    set(h, "x-request-id", &s.id.to_string());
    set(h, "via", "1.1 apexrouter");
    set(h, "x-apexrouter-route", &route_value(s.alias, s.reason));
    set(h, "x-apexrouter-attempts", &s.attempts.to_string());
    if s.fallback {
        set(h, "x-apexrouter-fallback", "true");
    }
    if let Some(b) = s.backend {
        set(h, "x-apexrouter-backend", b.as_str());
        set(h, "x-provider", b.as_str());
    }
    if s.ingress != Protocol::OpenAi {
        let up = s.upstream.unwrap_or(s.ingress);
        set(
            h,
            "x-apexrouter-protocol",
            &format!("{}->{}", protocol_token(s.ingress), protocol_token(up)),
        );
    }
}

/// The `X-ApexRouter-Route` value, `<alias-or-"-">|<reason>`.
fn route_value(alias: Option<&Alias>, reason: Option<RouteReason>) -> String {
    format!(
        "{}|{}",
        alias.map(Alias::as_str).unwrap_or("-"),
        reason.as_ref().map(RouteReason::as_str).unwrap_or("-")
    )
}

/// The wire token for a protocol, matching the `snake_case` serde rename.
fn protocol_token(p: Protocol) -> &'static str {
    match p {
        Protocol::OpenAi => "open_ai",
        Protocol::Anthropic => "anthropic",
    }
}

/// Set a header, dropping it silently when the value is not header-safe.
fn set(h: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        h.insert(HeaderName::from_static(name), v);
    }
}

/// An error body in the dialect the **client** speaks.
///
/// The OpenAI shape comes from R-06. The Anthropic shape is built here for Stage 3; R-10
/// publishes `anthropic::anthropic_error` in Stage 5 and this delegates to it then.
fn error_response(ingress: Protocol, status: StatusCode, kind: &str, msg: &str) -> Response {
    match ingress {
        Protocol::OpenAi => openai_error(status, kind, msg),
        Protocol::Anthropic => {
            let body = serde_json::json!({
                "type": "error",
                "error": { "type": kind, "message": msg }
            });
            (status, axum::Json(body)).into_response()
        }
    }
}

/// Re-render an OpenAI model list as the Anthropic one. **Never a translation** — the rows
/// come from the same aggregation function, and the `apexrouter` extras key is carried
/// through untouched.
fn anthropic_model_view(v: serde_json::Value) -> serde_json::Value {
    let rows: Vec<serde_json::Value> = v
        .get("data")
        .and_then(serde_json::Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    let id = row.get("id").and_then(|i| i.as_str()).unwrap_or_default();
                    let created = row.get("created").and_then(serde_json::Value::as_i64);
                    let mut out = serde_json::json!({
                        "type": "model",
                        "id": id,
                        "display_name": id,
                        "created_at": created,
                    });
                    if let (Some(extras), Some(obj)) = (row.get("apexrouter"), out.as_object_mut())
                    {
                        obj.insert("apexrouter".to_owned(), extras.clone());
                    }
                    out
                })
                .collect()
        })
        .unwrap_or_default();
    let first = rows.first().and_then(|r| r.get("id")).cloned();
    let last = rows.last().and_then(|r| r.get("id")).cloned();
    serde_json::json!({
        "data": rows,
        "has_more": false,
        "first_id": first,
        "last_id": last,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_core::config::Config;
    use apexrouter_core::paths::Paths;
    use apexrouter_core::usage::UsageWriter;
    use apexrouter_protocol::{
        Backend, BackendKind, BackendLimits, BackendSelector, Health, ModelRoute, Provenance,
        RouteFile, RouteFilter, RouteTarget, Strategy, UpstreamModel,
    };
    use axum::body::to_bytes as body_to_bytes;
    use axum::extract::Request;
    use std::sync::{Arc, OnceLock};
    use tower::ServiceExt;
    use wiremock::matchers::{method as m_method, path as m_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// One tempdir for the whole test binary, exported as `$APEXROUTER_HOME` before any
    /// `Paths::resolve()` runs.
    fn paths() -> &'static Paths {
        static P: OnceLock<Paths> = OnceLock::new();
        P.get_or_init(|| {
            // Leaked on purpose: the directory must outlive every test in the binary.
            let dir = tempfile::TempDir::new().expect("tempdir");
            let root = dir.keep();
            std::env::set_var("APEXROUTER_HOME", &root);
            let p = Paths::resolve().expect("paths");
            p.ensure_layout().expect("layout");
            p
        })
    }

    fn test_config() -> Config {
        let mut cfg = Config::default();
        cfg.compat.mirror_usage_log = false;
        cfg.router.log_usage = false;
        // `GET /providers` probes Together at `[providers.together] base_url`, using the key
        // `core::secret::resolve_provider` finds — which on a developer's machine really is
        // `$TOGETHER_API_KEY`. Pointing the probe at a closed loopback port is what keeps
        // `cargo test` free of live, authenticated calls to a paid API. Do not remove: this
        // is the "no network, no credentials, no money" half of the Stage 3 gate, and it is
        // enforced by `the_test_suite_never_reaches_a_paid_endpoint`.
        cfg.providers.insert(
            "together".to_owned(),
            apexrouter_core::config::ProviderCfg {
                base_url: "http://127.0.0.1:1/v1".to_owned(),
                api_key_env: None,
                api_key_file: None,
            },
        );
        cfg
    }

    fn router_of(cfg: Config, backends: Vec<Backend>, routes: Vec<ModelRoute>) -> Router {
        let cfg = Arc::new(cfg);
        let (tx, _rx) = tokio::sync::broadcast::channel(64);
        let usage = UsageWriter::open(paths(), &cfg.compat).expect("usage writer");
        let r = crate::RouterInner::new(cfg.clone(), tx, usage);
        for b in backends {
            r.registry().upsert(b, &cfg.router);
        }
        let file = RouteFile {
            schema_version: 1,
            default_alias: Alias::parse("auto").expect("alias"),
            routes,
        };
        let table = crate::TableBuilder::compile(&cfg, &file, r.registry()).expect("compile");
        r.store_table(table);
        r
    }

    fn router_with(backends: Vec<Backend>, routes: Vec<ModelRoute>) -> Router {
        router_of(test_config(), backends, routes)
    }

    fn backend(id: &str, base_url: &str) -> Backend {
        Backend {
            id: BackendId::parse(id).expect("id"),
            kind: BackendKind::Node,
            protocol: Protocol::OpenAi,
            label: id.to_owned(),
            base_url: base_url.to_owned(),
            credential: CredentialSource::None,
            tags: vec![],
            models: vec![UpstreamModel {
                id: "carnice".to_owned(),
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

    fn route(alias: &str, targets: Vec<&str>) -> ModelRoute {
        ModelRoute {
            alias: Alias::parse(alias).expect("alias"),
            targets: targets
                .into_iter()
                .map(|t| RouteTarget {
                    backend: BackendSelector::Id(BackendId::parse(t).expect("id")),
                    model: Some("carnice".to_owned()),
                    weight: 1,
                })
                .collect(),
            strategy: Strategy::FirstHealthy,
            filter: RouteFilter::default(),
            retry: RetryPolicy::default(),
            is_default: true,
            description: None,
        }
    }

    async fn call(app: &axum::Router, req: Request<Body>) -> Response {
        app.clone().oneshot(req).await.expect("infallible")
    }

    fn post_chat(body: &str) -> Request<Body> {
        Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(body.to_owned()))
            .expect("request")
    }

    fn anthropic_post(body: &str) -> Request<Body> {
        Request::builder()
            .method(Method::POST)
            .uri("/v1/messages")
            .header("content-type", "application/json")
            .header("anthropic-version", "2023-06-01")
            .header("x-api-key", "sk-client-secret")
            .body(Body::from(body.to_owned()))
            .expect("request")
    }

    fn header<'a>(resp: &'a Response, name: &str) -> Option<&'a str> {
        resp.headers().get(name).and_then(|v| v.to_str().ok())
    }

    // ---- wiring ----------------------------------------------------------------------------

    #[tokio::test]
    async fn merge_does_not_panic() {
        // The whole reason the catch-all is `.fallback(any(..))` and not a `/{*path}` route:
        // the server merges this router with one that owns `get("/{*path}")`, and two
        // `/{*path}` routes are an "Overlapping method route" panic in axum 0.8.
        let r = router_with(vec![], vec![]);
        let assets = axum::Router::new()
            .route("/{*path}", get(|| async { "asset" }))
            .route("/ui", get(|| async { "ui" }));
        let merged = proxy_router(r).merge(assets);
        let resp = call(
            &merged,
            Request::builder()
                .uri("/ui")
                .body(Body::empty())
                .expect("req"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn every_response_carries_the_route_header() {
        let r = router_with(vec![], vec![]);
        let app = proxy_router(r);
        for uri in ["/v1/models", "/slots", "/v1/messages/count_tokens"] {
            let resp = call(
                &app,
                Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .expect("req"),
            )
            .await;
            assert!(
                resp.headers().contains_key("x-apexrouter-route"),
                "{uri} had no X-ApexRouter-Route"
            );
            assert!(resp.headers().contains_key("x-request-id"), "{uri}");
        }
    }

    #[tokio::test]
    async fn via_loop_is_508() {
        let r = router_with(vec![], vec![]);
        let app = proxy_router(r);
        let req = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .header("via", "1.1 apexrouter")
            .body(Body::from("{}"))
            .expect("req");
        let resp = call(&app, req).await;
        assert_eq!(resp.status(), StatusCode::LOOP_DETECTED);
        assert!(resp.headers().contains_key("x-apexrouter-route"));
        let body = body_to_bytes(resp.into_body(), 64 * 1024)
            .await
            .expect("body");
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["error"]["type"], "loop_detected");
    }

    #[tokio::test]
    async fn slots_is_never_proxied_outward() {
        let r = router_with(vec![], vec![]);
        let app = proxy_router(r);
        let resp = call(
            &app,
            Request::builder()
                .uri("/slots")
                .body(Body::empty())
                .expect("req"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = body_to_bytes(resp.into_body(), 64 * 1024)
            .await
            .expect("body");
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["error"]["type"], "redacted_endpoint");
    }

    #[tokio::test]
    async fn an_oversized_body_is_413() {
        let mut cfg = test_config();
        cfg.router.max_body_bytes = 64;
        let r = router_of(cfg, vec![], vec![]);
        let app = proxy_router(r);
        let big = format!("{{\"model\":\"auto\",\"pad\":\"{}\"}}", "x".repeat(4096));
        let resp = call(&app, post_chat(&big)).await;
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    // ---- the pipeline, against wiremock ---------------------------------------------------------

    #[tokio::test]
    async fn routes_and_relays_verbatim() {
        const BODY: &[u8] = br#"{"id":"a","usage":{"prompt_tokens":7,"completion_tokens":3}}"#;
        let up = MockServer::start().await;
        Mock::given(m_method("POST"))
            .and(m_path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(BODY.to_vec(), "application/json"),
            )
            .mount(&up)
            .await;

        let r = router_with(
            vec![backend("up-a", &up.uri())],
            vec![route("auto", vec!["up-a"])],
        );
        let app = proxy_router(r);
        let resp = call(&app, post_chat(r#"{"model":"auto"}"#)).await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(header(&resp, "x-apexrouter-backend"), Some("up-a"));
        assert_eq!(header(&resp, "x-provider"), Some("up-a"));
        assert!(header(&resp, "x-apexrouter-route").is_some());
        assert!(header(&resp, "x-usage").is_some());
        let body = body_to_bytes(resp.into_body(), 64 * 1024)
            .await
            .expect("body");
        assert_eq!(body.as_ref(), BODY);
    }

    #[tokio::test]
    async fn one_started_and_one_finished_event_per_served_request() {
        // The guard owns `RequestFinished`, the handler owns `RequestStarted`. A double
        // emission here would show up as two rows in every live-request table.
        let up = MockServer::start().await;
        Mock::given(m_method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(b"{}".to_vec(), "application/json"),
            )
            .mount(&up)
            .await;
        let r = router_with(
            vec![backend("up-a", &up.uri())],
            vec![route("auto", vec!["up-a"])],
        );
        let mut rx = r.events.subscribe();
        let app = proxy_router(r);
        let resp = call(&app, post_chat(r#"{"model":"auto"}"#)).await;
        let _ = body_to_bytes(resp.into_body(), 64 * 1024)
            .await
            .expect("body");

        let (mut started, mut finished) = (0, 0);
        while let Ok(ev) = rx.try_recv() {
            match ev {
                Event::RequestStarted { .. } => started += 1,
                Event::RequestFinished { record } => {
                    assert!(!record.aborted);
                    assert_eq!(record.status, 200);
                    finished += 1;
                }
                _ => {}
            }
        }
        assert_eq!((started, finished), (1, 1));
    }

    #[tokio::test]
    async fn a_502_is_retried_onto_the_next_candidate() {
        let bad = MockServer::start().await;
        Mock::given(m_method("POST"))
            .respond_with(ResponseTemplate::new(502))
            .mount(&bad)
            .await;
        let good = MockServer::start().await;
        Mock::given(m_method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(b"{}".to_vec(), "application/json"),
            )
            .mount(&good)
            .await;

        let r = router_with(
            vec![
                backend("up-bad", &bad.uri()),
                backend("up-good", &good.uri()),
            ],
            vec![route("auto", vec!["up-bad", "up-good"])],
        );
        let app = proxy_router(r);
        let resp = call(&app, post_chat(r#"{"model":"auto"}"#)).await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(header(&resp, "x-apexrouter-attempts"), Some("2"));
        assert_eq!(header(&resp, "x-apexrouter-fallback"), Some("true"));
        assert_eq!(header(&resp, "x-apexrouter-backend"), Some("up-good"));
    }

    #[tokio::test]
    async fn a_terminal_status_is_relayed_and_never_retried() {
        // A 400 is the upstream's answer, not a failure to route: relay it verbatim, once.
        let up = MockServer::start().await;
        Mock::given(m_method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_raw(
                br#"{"error":{"message":"bad"}}"#.to_vec(),
                "application/json",
            ))
            .expect(1)
            .mount(&up)
            .await;

        let r = router_with(
            vec![backend("up-a", &up.uri())],
            vec![route("auto", vec!["up-a"])],
        );
        let app = proxy_router(r);
        let resp = call(&app, post_chat(r#"{"model":"auto"}"#)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(header(&resp, "x-apexrouter-attempts"), Some("1"));
    }

    #[tokio::test]
    async fn a_committed_stream_is_never_retried() {
        // The upstream commits (200 + text/event-stream). Whatever happens next, there must
        // be exactly one upstream attempt — `.expect(1)` on the mock is the assertion.
        let up = MockServer::start().await;
        Mock::given(m_method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(
                        b"data: {\"choices\":[]}\n\ndata: [DONE]\n\n".to_vec(),
                        "text/event-stream",
                    ),
            )
            .expect(1)
            .mount(&up)
            .await;

        let r = router_with(
            vec![backend("up-a", &up.uri())],
            vec![route("auto", vec!["up-a"])],
        );
        let app = proxy_router(r);
        let resp = call(&app, post_chat(r#"{"model":"auto","stream":true}"#)).await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(header(&resp, "content-type"), Some("text/event-stream"));
        assert_eq!(header(&resp, "x-apexrouter-usage-deferred"), Some("true"));
        assert_eq!(header(&resp, "cache-control"), Some("no-cache"));
        assert_eq!(header(&resp, "x-accel-buffering"), Some("no"));
        assert!(
            header(&resp, "x-usage").is_none(),
            "X-Usage is buffered-only"
        );
        let body = body_to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .expect("body");
        assert_eq!(body.as_ref(), b"data: {\"choices\":[]}\n\ndata: [DONE]\n\n");
    }

    #[tokio::test]
    async fn a_json_400_on_a_stream_request_stays_json() {
        // `text/event-stream` is forced ONLY when upstream is 2xx and already says so.
        let up = MockServer::start().await;
        Mock::given(m_method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_raw(
                br#"{"error":{"message":"no"}}"#.to_vec(),
                "application/json",
            ))
            .mount(&up)
            .await;
        let r = router_with(
            vec![backend("up-a", &up.uri())],
            vec![route("auto", vec!["up-a"])],
        );
        let app = proxy_router(r);
        let resp = call(&app, post_chat(r#"{"model":"auto","stream":true}"#)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(header(&resp, "content-type")
            .map(|c| c.contains("json"))
            .unwrap_or(false));
    }

    #[tokio::test]
    async fn an_unreachable_upstream_is_502_not_503() {
        // The 502-vs-503 distinction is load-bearing in both house projects.
        let r = router_with(
            vec![backend("up-dead", "http://127.0.0.1:1")],
            vec![route("auto", vec!["up-dead"])],
        );
        let app = proxy_router(r);
        let resp = call(&app, post_chat(r#"{"model":"auto"}"#)).await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    // ---- the (ingress, upstream) matrix -----------------------------------------------------------

    #[tokio::test]
    async fn openai_into_an_anthropic_backend_is_501_openai_shaped() {
        let up = MockServer::start().await;
        let mut b = backend("up-anth", &up.uri());
        b.protocol = Protocol::Anthropic;
        let r = router_with(vec![b], vec![route("auto", vec!["up-anth"])]);
        let app = proxy_router(r);
        let resp = call(&app, post_chat(r#"{"model":"auto"}"#)).await;

        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
        // The ingress is open_ai, so no X-ApexRouter-Protocol header and an OpenAI body.
        assert!(header(&resp, "x-apexrouter-protocol").is_none());
        let body = body_to_bytes(resp.into_body(), 64 * 1024)
            .await
            .expect("body");
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["error"]["type"], "protocol_not_supported");
    }

    #[tokio::test]
    async fn anthropic_into_an_anthropic_backend_is_relayed() {
        let up = MockServer::start().await;
        Mock::given(m_method("POST"))
            .and(m_path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                br#"{"type":"message","content":[]}"#.to_vec(),
                "application/json",
            ))
            .mount(&up)
            .await;
        let mut b = backend("up-anth", &up.uri());
        b.protocol = Protocol::Anthropic;
        let r = router_with(vec![b], vec![route("auto", vec!["up-anth"])]);
        let app = proxy_router(r);

        let resp = call(&app, anthropic_post(r#"{"model":"auto","max_tokens":8}"#)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            header(&resp, "x-apexrouter-protocol"),
            Some("anthropic->anthropic")
        );
    }

    #[tokio::test]
    async fn anthropic_into_an_openai_backend_is_501_anthropic_shaped_until_r10() {
        let up = MockServer::start().await;
        let r = router_with(
            vec![backend("up-a", &up.uri())],
            vec![route("auto", vec!["up-a"])],
        );
        let app = proxy_router(r);
        let resp = call(&app, anthropic_post(r#"{"model":"auto","max_tokens":8}"#)).await;

        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
        assert_eq!(
            header(&resp, "x-apexrouter-protocol"),
            Some("anthropic->open_ai")
        );
        let body = body_to_bytes(resp.into_body(), 64 * 1024)
            .await
            .expect("body");
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["type"], "error");
    }

    #[tokio::test]
    async fn anthropic_ingress_can_be_switched_off() {
        let mut cfg = test_config();
        cfg.router.anthropic_ingress = false;
        let r = router_of(cfg, vec![], vec![]);
        let app = proxy_router(r);
        let resp = call(&app, anthropic_post(r#"{"model":"auto","max_tokens":8}"#)).await;
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    }

    // ---- pure units -------------------------------------------------------------------------------

    #[test]
    fn ingress_detection_never_fires_on_a_plain_models_sweep() {
        let empty = HeaderMap::new();
        assert_eq!(detect_ingress("/v1/models", &empty), Protocol::OpenAi);
        assert_eq!(
            detect_ingress("/v1/chat/completions", &empty),
            Protocol::OpenAi
        );
        assert_eq!(detect_ingress("/v1/messages", &empty), Protocol::Anthropic);
        let mut h = HeaderMap::new();
        h.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        assert_eq!(detect_ingress("/v1/models", &h), Protocol::Anthropic);
    }

    #[test]
    fn classification_matches_the_surface_table() {
        assert_eq!(classify("/v1/models"), RequestClass::Models);
        assert_eq!(classify("/v1/models/auto"), RequestClass::Models);
        assert_eq!(classify("/v1/chat/completions"), RequestClass::Chat);
        assert_eq!(classify("/v1/messages"), RequestClass::Chat);
        assert_eq!(classify("/v1/completions"), RequestClass::Completion);
        assert_eq!(classify("/v1/embeddings"), RequestClass::Embedding);
        assert_eq!(classify("/v1/rerank"), RequestClass::Rerank);
        assert_eq!(classify("/props"), RequestClass::Opaque);
    }

    #[test]
    fn upstream_url_keeps_the_query_and_never_doubles_v1() {
        assert_eq!(
            upstream_url("http://h:8100", "/v1/chat/completions", None),
            "http://h:8100/v1/chat/completions"
        );
        assert_eq!(
            upstream_url("http://h:8100/v1", "/v1/models", Some("a=1")),
            "http://h:8100/v1/models?a=1"
        );
        assert_eq!(
            upstream_url("http://h:8100", "/props", None),
            "http://h:8100/props"
        );
    }

    #[test]
    fn via_loop_detection_is_case_insensitive() {
        let mut h = HeaderMap::new();
        h.insert("via", HeaderValue::from_static("1.1 ApexRouter"));
        assert!(via_loops(&h));
        let mut h = HeaderMap::new();
        h.insert("via", HeaderValue::from_static("1.1 squid"));
        assert!(!via_loops(&h));
    }

    #[test]
    fn route_value_renders_the_documented_pair() {
        let a = Alias::parse("auto").expect("alias");
        assert_eq!(
            route_value(Some(&a), Some(RouteReason::UpstreamIdMatch)),
            "auto|upstream_id_match"
        );
        assert_eq!(route_value(None, None), "-|-");
    }

    #[test]
    fn the_mid_stream_death_frame_is_one_error_then_done() {
        let s = String::from_utf8_lossy(MID_STREAM_DEATH);
        assert_eq!(s.matches("data: ").count(), 2);
        assert!(s.contains("\"type\":\"upstream_unavailable\""));
        assert!(s.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn the_tee_reads_usage_off_the_tail_and_shrugs_at_garbage() {
        let tail = b"data: {\"choices\":[]}\n\ndata: {\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\ndata: [DONE]\n\n";
        let (usage, _) = scan_tail(tail);
        assert_eq!(usage.map(|u| u.prompt_tokens), Some(5));
        let (usage, timings) = scan_tail(b"data: not json at all\n\n");
        assert!(usage.is_none() && timings.is_none());
    }

    #[test]
    fn the_anthropic_model_view_re_renders_the_same_rows() {
        let openai = serde_json::json!({
            "object": "list",
            "data": [{"id":"auto","object":"model","created":1,"apexrouter":{"kind":"alias"}}]
        });
        let v = anthropic_model_view(openai);
        assert_eq!(v["data"][0]["type"], "model");
        assert_eq!(v["data"][0]["id"], "auto");
        assert_eq!(v["data"][0]["apexrouter"]["kind"], "alias");
        assert_eq!(v["has_more"], false);
        assert_eq!(v["first_id"], "auto");
    }

    #[test]
    fn unknown_model_policy_defaults_to_reject() {
        assert_eq!(unknown_policy("reject"), UnknownModelPolicy::Reject);
        assert_eq!(unknown_policy("Fallback"), UnknownModelPolicy::Fallback);
        assert_eq!(unknown_policy("nonsense"), UnknownModelPolicy::Reject);
    }

    // =========================================================================================
    // Stage 3 gate: the drop-in contract (`docs/port/05-proxy.md` §15, ARCHITECTURE §6.1).
    // These are end-to-end through `proxy_router` against a wiremock upstream — no network
    // beyond loopback, no llama.cpp, no credentials, no money.
    // =========================================================================================

    /// A `POST` whose path the upstream mock only answers at `/v1/...`, sent three ways.
    fn post_to(uri: &str, body: &str) -> Request<Body> {
        Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_owned()))
            .expect("request")
    }

    #[tokio::test]
    async fn the_test_suite_never_reaches_a_paid_endpoint() {
        // The Stage 3 gate is "against a fake upstream — no network, no llama.cpp, no
        // credentials, no money". Every URL any handler test can dial has to be loopback.
        // `GET /providers` is the one handler that reads a real credential chain and probes
        // a remote host, so its configured target is asserted here rather than trusted.
        let cfg = test_config();
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

    #[tokio::test]
    async fn only_the_five_legacy_pairs_are_intercepted_everything_else_is_proxied() {
        // `docs/port/05-proxy.md` §2 "Routing subtlety that must be preserved" and §15
        // item 6. LocalRouter intercepted exactly `GET|HEAD /health`, `GET|HEAD /providers`
        // and `POST /switch`; every other (path, method) pair — `POST /health`,
        // `DELETE /providers`, `GET /switch` — went to the upstream. In axum 0.8 an
        // unmatched method on a matched path hits the MethodRouter's own 405, not
        // `Router::fallback`, so this has to be arranged deliberately.
        let up = MockServer::start().await;
        Mock::given(m_method("POST"))
            .and(m_path("/health"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(br#"{"up":1}"#.to_vec(), "application/json"),
            )
            .mount(&up)
            .await;
        Mock::given(m_method("DELETE"))
            .and(m_path("/providers"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&up)
            .await;
        Mock::given(m_method("GET"))
            .and(m_path("/switch"))
            .respond_with(ResponseTemplate::new(405))
            .mount(&up)
            .await;

        let r = router_with(
            vec![backend("up-a", &up.uri())],
            vec![route("auto", vec!["up-a"])],
        );
        let app = proxy_router(r);

        // Intercepted: served locally, never from the upstream mock.
        for (m, p) in [
            (Method::GET, "/health"),
            (Method::HEAD, "/health"),
            (Method::GET, "/providers"),
            (Method::HEAD, "/providers"),
        ] {
            let resp = call(
                &app,
                Request::builder()
                    .method(m.clone())
                    .uri(p)
                    .body(Body::empty())
                    .expect("req"),
            )
            .await;
            assert_eq!(resp.status(), StatusCode::OK, "{m} {p}");
        }

        // Proxied: the upstream's own answer, verbatim.
        for (m, p, want) in [
            (Method::POST, "/health", StatusCode::OK),
            (Method::DELETE, "/providers", StatusCode::NO_CONTENT),
            (Method::GET, "/switch", StatusCode::METHOD_NOT_ALLOWED),
        ] {
            let resp = call(
                &app,
                Request::builder()
                    .method(m.clone())
                    .uri(p)
                    .body(Body::empty())
                    .expect("req"),
            )
            .await;
            assert_eq!(resp.status(), want, "{m} {p} was not proxied");
            assert_eq!(
                header(&resp, "x-apexrouter-backend"),
                Some("up-a"),
                "{m} {p} never reached an upstream"
            );
        }
    }

    #[tokio::test]
    async fn both_client_base_urls_work_and_neither_doubles_v1() {
        // `docs/port/05-proxy.md` §15 item 8 — "the single highest-risk drop-in
        // incompatibility". A client configured with `http://127.0.0.1:8888` sends
        // `/chat/completions`; one configured with `http://127.0.0.1:8888/v1` sends
        // `/v1/chat/completions`; `smoke.sh` against the latter sends `/v1/v1/...`.
        // The upstream here answers ONLY `/v1/chat/completions` — exactly like Together,
        // and like any llama-server reached through a `/v1`-only path.
        let up = MockServer::start().await;
        Mock::given(m_method("POST"))
            .and(m_path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(b"{}".to_vec(), "application/json"),
            )
            .expect(3)
            .mount(&up)
            .await;

        let r = router_with(
            vec![backend("up-a", &up.uri())],
            vec![route("auto", vec!["up-a"])],
        );
        let app = proxy_router(r);

        for uri in [
            "/chat/completions",       // base_url = http://127.0.0.1:8888
            "/v1/chat/completions",    // base_url = http://127.0.0.1:8888/v1
            "/v1/v1/chat/completions", // smoke.sh against …:8888/v1
        ] {
            let resp = call(&app, post_to(uri, r#"{"model":"auto"}"#)).await;
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "{uri} did not reach upstream"
            );
            assert_eq!(
                header(&resp, "x-apexrouter-backend"),
                Some("up-a"),
                "{uri} was not routed"
            );
        }
    }

    #[tokio::test]
    async fn a_bare_base_url_is_classified_and_aliased_exactly_like_the_v1_form() {
        // Not just reachability: `/chat/completions` must be class `Chat` and go through
        // the same `resolve()` as `/v1/chat/completions`, or a bare-base client silently
        // loses model aliasing and every filter that depends on the class.
        assert_eq!(
            classify(&normalize_path("/chat/completions").0),
            classify("/v1/chat/completions")
        );
        assert_eq!(classify(&normalize_path("/models").0), RequestClass::Models);
        assert_eq!(
            classify(&normalize_path("/embeddings").0),
            RequestClass::Embedding
        );
        assert_eq!(
            detect_ingress(&normalize_path("/messages").0, &HeaderMap::new()),
            Protocol::Anthropic
        );
    }

    #[tokio::test]
    async fn a_bare_base_url_still_gets_the_aggregated_model_list() {
        let r = router_with(
            vec![backend("up-a", "http://127.0.0.1:1")],
            vec![route("auto", vec!["up-a"])],
        );
        let app = proxy_router(r);
        for uri in ["/models", "/v1/models", "/v1/v1/models"] {
            let resp = call(
                &app,
                Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .expect("req"),
            )
            .await;
            assert_eq!(resp.status(), StatusCode::OK, "{uri}");
            let body = body_to_bytes(resp.into_body(), 256 * 1024)
                .await
                .expect("body");
            let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
            assert_eq!(v["object"], "list", "{uri} was not served from the table");
        }
    }

    #[tokio::test]
    async fn the_clients_model_string_is_rewritten_to_the_upstream_id() {
        // `05-proxy.md` §14 item 4 — the papercut that broke provider swapping in
        // LocalRouter. The client says `"auto"`; the upstream must be asked for `"carnice"`,
        // and everything else in the document must survive byte-identically.
        let up = MockServer::start().await;
        Mock::given(m_method("POST"))
            .and(m_path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(b"{}".to_vec(), "application/json"),
            )
            .mount(&up)
            .await;

        let r = router_with(
            vec![backend("up-a", &up.uri())],
            vec![route("auto", vec!["up-a"])],
        );
        let app = proxy_router(r);
        let sent = r#"{"model":"auto","temperature":0.10,"top_p":1e-7,"messages":[{"role":"user","content":"héllo"}]}"#;
        let resp = call(&app, post_chat(sent)).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let seen = up.received_requests().await.expect("recording enabled");
        assert_eq!(seen.len(), 1);
        let raw = String::from_utf8(seen[0].body.clone()).expect("utf8");
        assert_eq!(
            raw,
            r#"{"model":"carnice","temperature":0.10,"top_p":1e-7,"messages":[{"role":"user","content":"héllo"}]}"#,
            "only the model value may change"
        );
    }

    #[tokio::test]
    async fn a_passthrough_body_reaches_the_upstream_byte_identically() {
        // When the client already spells the upstream id there is no rewrite at all.
        let up = MockServer::start().await;
        Mock::given(m_method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(b"{}".to_vec(), "application/json"),
            )
            .mount(&up)
            .await;
        let r = router_with(
            vec![backend("up-a", &up.uri())],
            vec![route("auto", vec!["up-a"])],
        );
        let app = proxy_router(r);
        let sent = r#"{"model":"carnice","tools":[{"a":"é"}]}"#;
        let resp = call(&app, post_chat(sent)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let seen = up.received_requests().await.expect("recording");
        assert_eq!(String::from_utf8(seen[0].body.clone()).expect("utf8"), sent);
    }

    #[tokio::test]
    async fn x_usage_keeps_the_legacy_plus_form_and_x_provider_names_the_backend() {
        // `05-proxy.md` §5 / §15 item 9: `X-Usage: "{prompt}+{completion}"`, e.g. "131072+500".
        // Anything else silently breaks every existing reader.
        let up = MockServer::start().await;
        Mock::given(m_method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                br#"{"usage":{"prompt_tokens":131072,"completion_tokens":500}}"#.to_vec(),
                "application/json",
            ))
            .mount(&up)
            .await;
        let r = router_with(
            vec![backend("up-a", &up.uri())],
            vec![route("auto", vec!["up-a"])],
        );
        let app = proxy_router(r);
        let resp = call(&app, post_chat(r#"{"model":"auto"}"#)).await;
        assert_eq!(header(&resp, "x-provider"), Some("up-a"));
        assert_eq!(header(&resp, "x-usage"), Some("131072+500"));
    }

    #[tokio::test]
    async fn the_model_list_unions_every_enabled_backend_not_just_the_aliased_ones() {
        // ARCHITECTURE §6.1: "aggregated across aliases + every enabled backend".
        let mut lonely = backend("up-lonely", "http://127.0.0.1:1");
        lonely.models = vec![UpstreamModel {
            id: "solo".to_owned(),
            ctx: Some(8192),
            vision: false,
            tools: true,
        }];
        let r = router_with(
            vec![backend("up-a", "http://127.0.0.1:1"), lonely],
            vec![route("auto", vec!["up-a"])],
        );
        let app = proxy_router(r);
        let resp = call(
            &app,
            Request::builder()
                .uri("/v1/models")
                .body(Body::empty())
                .expect("req"),
        )
        .await;
        let body = body_to_bytes(resp.into_body(), 256 * 1024)
            .await
            .expect("body");
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        let ids: Vec<&str> = v["data"]
            .as_array()
            .expect("data")
            .iter()
            .filter_map(|r| r["id"].as_str())
            .collect();
        assert!(ids.contains(&"auto"), "alias row missing: {ids:?}");
        assert!(
            ids.contains(&"up-a/carnice"),
            "aliased backend model missing: {ids:?}"
        );
        assert!(
            ids.contains(&"up-lonely/solo"),
            "an enabled backend behind no alias must still be listed: {ids:?}"
        );
        let alias_row = v["data"]
            .as_array()
            .expect("data")
            .iter()
            .find(|r| r["id"] == "auto")
            .expect("alias row");
        assert_eq!(
            alias_row["apexrouter"]["strategy"], "first_healthy",
            "the route's strategy is part of the documented §6.1 shape"
        );
    }

    #[tokio::test]
    async fn a_table_swap_mid_stream_does_not_break_the_in_flight_response() {
        // §4.7's whole point: recompiling the table (a `/switch`, a prober update, a SIGHUP)
        // must not disturb a response that already committed. The relay holds `Arc`s, so the
        // old table can go away underneath it.
        let up = MockServer::start().await;
        const FRAMES: &[u8] = b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\ndata: {\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2}}\n\ndata: [DONE]\n\n";
        Mock::given(m_method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(FRAMES.to_vec(), "text/event-stream")
                    .set_delay(Duration::from_millis(30)),
            )
            .mount(&up)
            .await;

        let cfg = Arc::new(test_config());
        let (tx, _rx) = tokio::sync::broadcast::channel(64);
        let usage = UsageWriter::open(paths(), &cfg.compat).expect("usage writer");
        let r = crate::RouterInner::new(cfg.clone(), tx, usage);
        r.registry().upsert(backend("up-a", &up.uri()), &cfg.router);
        let file = RouteFile {
            schema_version: 1,
            default_alias: Alias::parse("auto").expect("alias"),
            routes: vec![route("auto", vec!["up-a"])],
        };
        let first = crate::TableBuilder::compile(&cfg, &file, r.registry()).expect("compile");
        let first_gen = first.generation();
        r.store_table(first);

        let app = proxy_router(r.clone());
        let resp = call(&app, post_chat(r#"{"model":"auto","stream":true}"#)).await;
        assert_eq!(resp.status(), StatusCode::OK);

        // The response has committed; swap the table out from under it, twice, including
        // one that removes the backend the stream is talking to.
        let second = crate::TableBuilder::compile(&cfg, &file, r.registry()).expect("recompile");
        assert!(second.generation() > first_gen, "generation must advance");
        r.store_table(second);
        r.registry().remove(&BackendId::parse("up-a").expect("id"));
        let empty = RouteFile {
            schema_version: 1,
            default_alias: Alias::parse("auto").expect("alias"),
            routes: vec![],
        };
        let third = crate::TableBuilder::compile(&cfg, &empty, r.registry()).expect("empty");
        r.store_table(third);

        let body = body_to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .expect("body");
        assert_eq!(
            body.as_ref(),
            FRAMES,
            "a table swap must not corrupt or truncate a committed stream"
        );
    }

    #[tokio::test]
    async fn a_backend_that_hangs_before_headers_fails_over_instead_of_burning_the_budget() {
        // The nastiest real failure on a big rig: a llama-server that accepted the socket
        // and then wedged mid-load. It sends no headers, so it is only detectable by
        // `headers_timeout_ms`. If the wall-clock deadline for ALL attempts equals the
        // per-attempt header timeout, attempt 1 consumes the whole budget and the healthy
        // second backend is never tried — the client gets a 504 next to an idle GPU.
        let hung = MockServer::start().await;
        Mock::given(m_method("POST"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
            .mount(&hung)
            .await;
        let good = MockServer::start().await;
        Mock::given(m_method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(b"{}".to_vec(), "application/json"),
            )
            .mount(&good)
            .await;

        let mut cfg = test_config();
        cfg.router.headers_timeout_ms = 300;
        let r = router_of(
            cfg,
            vec![
                backend("up-hung", &hung.uri()),
                backend("up-good", &good.uri()),
            ],
            vec![route("auto", vec!["up-hung", "up-good"])],
        );
        let app = proxy_router(r);
        let resp = call(&app, post_chat(r#"{"model":"auto"}"#)).await;

        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "a pre-header timeout must fail over, not 504"
        );
        assert_eq!(header(&resp, "x-apexrouter-backend"), Some("up-good"));
        assert_eq!(header(&resp, "x-apexrouter-attempts"), Some("2"));
    }

    #[tokio::test]
    async fn a_body_that_is_not_json_is_relayed_verbatim_instead_of_400ed() {
        // A multipart upload, a llama.cpp-native `POST /tokenize`, an empty `POST /health`:
        // none has a top-level `model` to rewrite. LocalRouter forwarded them and let the
        // upstream judge; inventing a 400 here would break §15 item 11.
        let up = MockServer::start().await;
        Mock::given(m_method("POST"))
            .and(m_path("/tokenize"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(br#"{"tokens":[1]}"#.to_vec(), "application/json"),
            )
            .mount(&up)
            .await;
        let r = router_with(
            vec![backend("up-a", &up.uri())],
            vec![route("auto", vec!["up-a"])],
        );
        let app = proxy_router(r);
        let raw = b"--x\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\nnot json\r\n--x--";
        let resp = call(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri("/tokenize")
                .header("content-type", "multipart/form-data; boundary=x")
                .body(Body::from(raw.to_vec()))
                .expect("req"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let seen = up.received_requests().await.expect("recording");
        assert_eq!(seen[0].body, raw, "a non-JSON body must not be touched");
    }

    // ---- the SSE relay, against an upstream whose chunk boundaries we control ------------------

    /// A hand-rolled HTTP/1.1 upstream that writes an SSE body in exactly these pieces, with
    /// `gap` between them.
    ///
    /// wiremock can express neither chunk boundaries nor inter-chunk gaps, and "relays
    /// without added buffering" is only observable if the test owns both.
    async fn sse_upstream(pieces: Vec<Vec<u8>>, gap: Duration) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            // Drain the request head; the body length does not matter to this fixture.
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf).await;
            let head = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                         Transfer-Encoding: chunked\r\n\r\n";
            if sock.write_all(head).await.is_err() {
                return;
            }
            let _ = sock.flush().await;
            for p in pieces {
                if sock
                    .write_all(format!("{:x}\r\n", p.len()).as_bytes())
                    .await
                    .is_err()
                {
                    return;
                }
                if sock.write_all(&p).await.is_err() || sock.write_all(b"\r\n").await.is_err() {
                    return;
                }
                let _ = sock.flush().await;
                tokio::time::sleep(gap).await;
            }
            let _ = sock.write_all(b"0\r\n\r\n").await;
            let _ = sock.flush().await;
        });
        format!("http://{addr}")
    }

    /// Every `data:` frame of a real llama.cpp b9199 `stream:true` completion, plus the
    /// `include_usage` tail and `[DONE]`.
    fn llama_capture() -> Vec<u8> {
        let mut s = String::new();
        s.push_str("data: {\"choices\":[{\"finish_reason\":null,\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}],\"created\":1780000000,\"id\":\"chatcmpl-1\",\"model\":\"Carnice-9b-Q6_K\",\"object\":\"chat.completion.chunk\"}\n\n");
        for tok in ["Hello", " there", ",", " world", "!"] {
            s.push_str(&format!(
                "data: {{\"choices\":[{{\"finish_reason\":null,\"index\":0,\"delta\":{{\"content\":\"{tok}\"}}}}],\"created\":1780000000,\"id\":\"chatcmpl-1\",\"model\":\"Carnice-9b-Q6_K\",\"object\":\"chat.completion.chunk\"}}\n\n"
            ));
        }
        s.push_str("data: {\"choices\":[{\"finish_reason\":\"stop\",\"index\":0,\"delta\":{}}],\"created\":1780000000,\"id\":\"chatcmpl-1\",\"model\":\"Carnice-9b-Q6_K\",\"object\":\"chat.completion.chunk\",\"usage\":{\"completion_tokens\":5,\"prompt_tokens\":11,\"total_tokens\":16},\"timings\":{\"predicted_per_second\":4.1,\"prompt_per_second\":90.2}}\n\n");
        s.push_str("data: [DONE]\n\n");
        s.into_bytes()
    }

    #[tokio::test]
    async fn a_capture_relays_byte_identically_whatever_the_chunk_boundaries_are() {
        // The relay must never re-frame: chunk boundaries deliberately fall INSIDE `data:`
        // events, and the client must still receive the exact original byte sequence.
        let capture = llama_capture();
        for size in [1usize, 3, 7, 33, 64, 4096] {
            let pieces: Vec<Vec<u8>> = capture.chunks(size).map(<[u8]>::to_vec).collect();
            let base = sse_upstream(pieces, Duration::from_millis(0)).await;
            let r = router_with(
                vec![backend("up-a", &base)],
                vec![route("auto", vec!["up-a"])],
            );
            let app = proxy_router(r);
            let resp = call(&app, post_chat(r#"{"model":"auto","stream":true}"#)).await;
            assert_eq!(resp.status(), StatusCode::OK, "chunk size {size}");
            assert_eq!(header(&resp, "content-type"), Some("text/event-stream"));
            let body = body_to_bytes(resp.into_body(), 4 * 1024 * 1024)
                .await
                .expect("body");
            assert_eq!(
                body.as_ref(),
                capture.as_slice(),
                "chunk size {size} was re-framed"
            );
        }
    }

    #[tokio::test]
    async fn the_relay_forwards_each_chunk_as_it_arrives_and_never_fills_a_buffer() {
        // aiohttp's `iter_chunked(4096)` returned as soon as ≥1 byte was available, so there
        // was no artificial TTFT delay (`05-proxy.md` §6). If the Rust relay waited to fill a
        // buffer — or buffered the whole stream — the first frame would not arrive until the
        // last one had been generated. Six frames, 120 ms apart: the first must land in a
        // small fraction of the total.
        const GAP: Duration = Duration::from_millis(120);
        let frames: Vec<Vec<u8>> = (0..6)
            .map(|i| format!("data: {{\"i\":{i}}}\n\n").into_bytes())
            .collect();
        let base = sse_upstream(frames.clone(), GAP).await;
        let r = router_with(
            vec![backend("up-a", &base)],
            vec![route("auto", vec!["up-a"])],
        );
        let app = proxy_router(r);

        let began = Instant::now();
        let resp = call(&app, post_chat(r#"{"model":"auto","stream":true}"#)).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let mut stream = resp.into_body().into_data_stream();
        let mut first_at = None;
        let mut seen = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.expect("chunk");
            if first_at.is_none() {
                first_at = Some(began.elapsed());
            }
            seen.extend_from_slice(&chunk);
        }
        let total = began.elapsed();
        let first_at = first_at.expect("at least one chunk");

        let whole: Vec<u8> = frames.concat();
        assert_eq!(seen, whole, "bytes were altered");
        assert!(
            total >= GAP * 4,
            "the fixture did not actually stream over time: {total:?}"
        );
        assert!(
            first_at < total / 2,
            "the first chunk arrived at {first_at:?} of {total:?} — the relay is buffering"
        );
    }

    #[tokio::test]
    async fn usage_is_teed_off_the_stream_without_touching_the_bytes() {
        // The tee reads `usage` and `timings` out of the tail and reports them on the
        // `RequestFinished` event; the client's byte stream is unaffected.
        let capture = llama_capture();
        let pieces: Vec<Vec<u8>> = capture.chunks(37).map(<[u8]>::to_vec).collect();
        let base = sse_upstream(pieces, Duration::from_millis(0)).await;
        let r = router_with(
            vec![backend("up-a", &base)],
            vec![route("auto", vec!["up-a"])],
        );
        let mut rx = r.events.subscribe();
        let app = proxy_router(r);
        let resp = call(&app, post_chat(r#"{"model":"auto","stream":true}"#)).await;
        let body = body_to_bytes(resp.into_body(), 4 * 1024 * 1024)
            .await
            .expect("body");
        assert_eq!(body.as_ref(), capture.as_slice());

        let mut record = None;
        while let Ok(ev) = rx.try_recv() {
            if let Event::RequestFinished { record: rec } = ev {
                record = Some(rec);
            }
        }
        let rec = record.expect("a RequestFinished for the completed stream");
        assert!(rec.streamed, "the record must know this was a stream");
        assert!(!rec.aborted, "a completed stream is not an abort");
        assert_eq!(
            rec.prompt_tokens,
            Some(TokenCount::Reported(11)),
            "prompt usage came off the tail, not an estimate"
        );
        assert_eq!(rec.completion_tokens, Some(TokenCount::Reported(5)));
        assert_eq!(
            rec.tok_per_s,
            Some(4.1),
            "tok/s is READ from timings.predicted_per_second, never stopwatched"
        );
    }

    #[tokio::test]
    async fn the_breaker_opens_after_repeated_failures_and_stops_dispatching() {
        // End-to-end companion to `breaker::tests`: once the breaker is open the request path
        // stops reaching the sick upstream at all, and says 503 (never dispatched) rather
        // than 502 (dispatched and failed).
        let sick = MockServer::start().await;
        Mock::given(m_method("POST"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&sick)
            .await;
        let r = router_with(
            vec![backend("up-sick", &sick.uri())],
            vec![route("auto", vec!["up-sick"])],
        );
        let be = r
            .registry()
            .get(&BackendId::parse("up-sick").expect("id"))
            .expect("backend");
        let app = proxy_router(r);

        let mut last = StatusCode::OK;
        for _ in 0..12 {
            last = call(&app, post_chat(r#"{"model":"auto"}"#)).await.status();
        }
        assert!(
            matches!(be.breaker.check(), BreakerDecision::Deny { .. }),
            "the breaker never opened under a steady 503"
        );
        assert_eq!(
            last,
            StatusCode::SERVICE_UNAVAILABLE,
            "an open breaker means we never dispatched, so 503 not 502"
        );

        // …and it recovers. Expire the cool-down, and the next check hands out the single
        // half-open probe; a good probe closes the breaker and traffic flows again.
        be.breaker.trip(Some(Duration::ZERO));
        assert!(matches!(
            be.breaker.check(),
            BreakerDecision::AllowProbe | BreakerDecision::Allow
        ));
        be.breaker.record(true);
        assert!(
            matches!(be.breaker.check(), BreakerDecision::Allow),
            "a successful probe must close the breaker"
        );
    }
}
