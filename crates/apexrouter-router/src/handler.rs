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

use crate::anthropic::{
    check_version_header, request_to_openai, response_to_anthropic, translate_error, upstream_path,
    AnthropicCfg, SseTranslator,
};
use crate::attempt::{attempt, Committed, PreFlight, Retryable};
use crate::breaker::BreakerDecision;
use crate::errors::{map_status, openai_error};
use crate::limits::{InFlightGuard, LimitError};
use crate::models::{aggregate_models, one_model};
use crate::registry::Parked;
use crate::relay::stream::is_event_stream;
use crate::relay::{
    normalize_path, outbound_headers, peek, plan_body, response_headers, sse_response, RequestPeek,
    StreamOutcome,
};
use crate::resolve::{RequestClass, RouteError, UnknownModelPolicy};
use crate::{Router, COLLAPSE_LOG_CAPACITY, RING_CAPACITY};

use apexrouter_core::upstream::{join_v1, parse_timings, parse_usage, Timings, UsageFields};
use apexrouter_protocol::{
    AlertLevel, Alias, BackendId, CostEstimate, Event, Protocol, RequestId, RequestRecord,
    RouteReason, TokenCount, UsageRecord,
};
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use bytes::Bytes;
use futures_util::StreamExt;
use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

/// The `Via` token this proxy stamps, and refuses to see twice.
const VIA_TOKEN: &str = "apexrouter";

/// How much of an untranslatable upstream error body is quoted back to the client.
const UPSTREAM_MSG_CHARS: usize = 512;

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
        note_collapse(&r, &headers, &path);
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
    let started_unix = chrono::Utc::now().timestamp();

    // **The warm queue, `ARCHITECTURE.md` §4.7.** A *Sequential* swap stops A before B
    // exists, and on a box where two 7 GB models cannot coexist that is the common mode, not
    // the rare one. Everything arriving in that window used to get a `503` for the whole of
    // the replacement's model load. It now parks instead, on `RouterInner::warm()`.
    //
    // Everything a park needs is borrowed rather than cloned, and the only thing the happy
    // path pays is one relaxed atomic load inside `WarmRegistry::any_open` — and only after a
    // dispatch has *already* failed.
    let park = ParkCtx {
        id,
        started,
        started_unix,
        ingress,
        method: &method,
        path: &path,
    };
    // **At most one park per request.** When the window closes the alias points somewhere
    // new, so one re-resolve is the whole answer; a second park would let a swap that failed
    // hold a client for two `warm_timeout`s and still answer `503`.
    let mut parked_once = false;
    // `(depth, waited_ms)` for `X-ApexRouter-Warm`, so a request that was served *through* a
    // swap says so. `None` on every request that never parked, which is nearly all of them.
    let mut warmed: Option<(u32, u32)> = None;

    // A parked request re-enters here: while it waited the alias was re-pointed, so the plan
    // it could not be served under is not the plan it should be served under.
    'dispatch: loop {
        let plan = {
            let table = r.table();
            table.resolve(pk.model.as_deref(), class, unknown)
        };
        let plan = match plan {
            Ok(p) => p,
            Err(e) => {
                // The alias resolved and nothing behind it is dispatchable. During a sequential
                // swap that is not a failure — it is the gap §4.7 says to park across.
                if let RouteError::NoHealthy { alias } = &e {
                    match park_on(
                        &r,
                        alias,
                        &park,
                        park_reason(pk.model.as_deref(), alias),
                        &mut parked_once,
                    )
                    .await
                    {
                        Parking::Retry { depth, waited_ms } => {
                            warmed = Some((depth, waited_ms));
                            continue 'dispatch;
                        }
                        Parking::Refused(resp) => {
                            let why = park_reason(pk.model.as_deref(), alias);
                            return stamp(resp, park.stamp(alias, why));
                        }
                        Parking::NotWarming => {}
                    }
                }
                let (status, kind) = map_status(&e);
                // Alias-known failures carry the alias (and Alias reason) so
                // `X-ApexRouter-Route` is not `-|-` when we already know which route failed.
                let mark = match &e {
                    RouteError::NoHealthy { alias } | RouteError::FilteredOut { alias, .. } => {
                        Stamp {
                            id: &id,
                            alias: Some(alias),
                            reason: Some(RouteReason::Alias),
                            backend: None,
                            attempts: 0,
                            fallback: false,
                            ingress,
                            upstream: None,
                        }
                    }
                    RouteError::NoRoute { .. } => Stamp::pre(&id, ingress),
                };
                return stamp(error_response(ingress, status, kind, &e.to_string()), mark);
            }
        };

        // Rule 4: an exact upstream id on several backends. resolve() deliberately does not
        // emit the one-shot Alert (it is sync and owns no bus); the handler does, once per
        // dispatch, so operators see the collision without grepping response headers.
        if matches!(plan.reason, RouteReason::ImplicitMulti) && r.events.receiver_count() > 0 {
            let model = pk.model.as_deref().unwrap_or("");
            let _ = r.events.send(Event::Alert {
                level: AlertLevel::Info,
                message: format!(
                    "model `{model}` matches more than one backend; using \
                     [router] implicit_strategy — pin with `<backend_id>/{model}` or an alias"
                ),
                action: Some(format!(
                    "apexrouter route set <alias> --target <backend_id>/{model}"
                )),
                id: format!("route.implicit_multi.{model}"),
            });
        }

        let mut draft = RecordDraft {
            id,
            started,
            started_unix,
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
        // **The route's own `[retry]` block**, carried here by `Plan::retry` (R-02). A route that
        // declared none was compiled with `RetryPolicy::default()`, so the config-wide default is
        // what a per-route override overrides — the key is never silently ignored.
        let policy = plan.retry;
        // `headers_timeout_ms` is what ONE attempt gets to produce response headers, and it is
        // long by design: a non-streaming completion on a 100B model sends none until generation
        // finishes. The wall-clock budget therefore has to span every attempt the policy allows.
        // Setting it to a single `headers_timeout_ms` would mean the first wedged candidate eats
        // the whole budget and `Instant::now() >= deadline` breaks the loop before the healthy
        // backend beside it is ever tried — a 504 next to an idle GPU.
        //
        // Measured from **this** pass rather than from the request's arrival, because a request
        // that parked behind a sequential swap has already spent minutes doing nothing. Charging
        // the park to the upstream's header budget would mean every parked request woke up and
        // immediately answered `504` — the parking primitive would have made things worse.
        let deadline = Instant::now()
            + Duration::from_millis(cfg.router.headers_timeout_ms)
                .saturating_mul(u32::from(policy.attempts.max(1)));
        let queue_timeout = Duration::from_millis(cfg.router.queue_timeout_ms);
        let mut last: Option<Retryable> = None;
        // R-10's rewritten request body, translated at most once. The translation depends only on
        // the client's bytes and on `[router] anthropic_tools`, never on which candidate is being
        // tried, so a failover reuses it instead of paying for it again.
        let mut translated: Option<Bytes> = None;

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
            let cell = match (ingress, meta.protocol) {
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
                // The one translating cell (R-10). The upstream model id travels with it because
                // it is what `SseTranslator` echoes in `message_start`.
                //
                // Gated on `/v1/messages` because that is the only body R-10 translates and the
                // only path it rewrites — `upstream_path` says so in its own doc. Without the
                // gate, an `anthropic-version` header on some other path would hand
                // `request_to_openai` a body that is not a `MessagesRequest` and turn a relayable
                // request into a `400`. Anything else this ingress can name stays a byte relay and
                // lets the upstream judge it, which is `05-proxy.md` §15 item 11.
                (Protocol::Anthropic, Protocol::OpenAi) if path == "/v1/messages" => {
                    Cell::Translate(cand.upstream_model.clone())
                }
                // OpenAi -> OpenAi and Anthropic -> Anthropic are both the byte relay below.
                _ => Cell::Relay,
            };

            // R-10's request contract, in the order its module doc states it — and **before** the
            // breaker, the permit and any upstream hop, because every failure it can produce is a
            // `400` the client caused: a missing `max_tokens`, a `thinking` block, or `tools` with
            // `[router] anthropic_tools = false`. None of those may cost a slot, a token or a
            // dollar, and none of them may be answered by silently dropping what was asked for.
            if matches!(cell, Cell::Translate(_)) && translated.is_none() {
                if let Some(res) = check_version_header(&headers) {
                    return stamp(res, draft.stamp_cell(&meta.id, meta.protocol));
                }
                let acfg = AnthropicCfg {
                    tools: cfg.router.anthropic_tools,
                };
                match request_to_openai(&bytes, &acfg) {
                    Ok(v) => translated = Some(Bytes::from(v)),
                    Err(e) => {
                        return stamp(
                            translate_error(&e),
                            draft.stamp_cell(&meta.id, meta.protocol),
                        )
                    }
                }
            }

            if let BreakerDecision::Deny { .. } = cand.backend.breaker.check() {
                continue;
            }

            // Per-backend retry budget: a flapping backend must not absorb unlimited
            // failover retries. First attempt is free; retries spend a token.
            if draft.attempts > 0 && !cand.backend.retry_bucket.try_take() {
                tracing::debug!(
                    backend = %meta.id,
                    "retry budget exhausted; skipping candidate"
                );
                continue;
            }

            let mut guard = match InFlightGuard::acquire(
                &cand.backend,
                pk.bytes,
                &r.inflight_bytes,
                queue_timeout,
            )
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

            // …and the record that `Drop` broadcasts. `InFlightGuard::drop` returns silently
            // when `record` is `None`, so leaving it unset made the whole abort path a no-op:
            // a client that hung up produced no event, no ring row and no usage line, which is
            // precisely the zombie row `ARCHITECTURE.md` §4.3 says cannot happen. `relay` seals
            // a real record on every outcome it sees, so this only ever ships when the request
            // task itself is cancelled mid-flight.
            guard.record = Some(draft.seal(Outcome::abandoned()));

            if r.events.receiver_count() > 0 {
                let _ = r.events.send(Event::RequestStarted {
                    id,
                    alias: draft.alias.clone(),
                    backend: draft.backend.clone(),
                });
            }

            // What actually goes on the wire. The translating cell sends R-10's rewritten body to
            // `/v1/chat/completions`; every other cell sends the client's own bytes to the path it
            // asked for. `resolve()`'s model rewrite is applied to whichever body that is, so an
            // alias still becomes the upstream's own model id on the Anthropic path too.
            //
            // The translating cell also sends **no query string**. Claude Code asks for
            // `POST /v1/messages?beta=true`; relaying that verbatim would hand a strict OpenAI
            // upstream `/v1/chat/completions?beta=true`, a parameter that means nothing on the
            // endpoint it is now attached to. llama.cpp ignores it, but it is an Anthropic-side
            // concern and does not survive the rewrite. Every other cell is a byte relay and keeps
            // the client's query untouched.
            let (out_bytes, out_path, out_query) = match &cell {
                Cell::Translate(_) => (
                    translated.as_ref().unwrap_or(&bytes),
                    upstream_path(&path),
                    None,
                ),
                Cell::Relay => (&bytes, path.as_str(), query.as_deref()),
            };

            // A body that is not a JSON object has no `model` to rewrite — a multipart
            // `/v1/audio/transcriptions` upload, an empty `POST /health`, a llama.cpp-native
            // `POST /tokenize`. LocalRouter forwarded those verbatim and let the upstream judge
            // them, and `05-proxy.md` §15 item 11 ("pass upstream status codes and bodies
            // through unchanged") means we must not invent a 400 the upstream would not have
            // sent. So a failed rewrite degrades to a byte-verbatim relay, never to an error.
            let body_plan =
                plan_body(out_bytes, plan.rewrite_model_to.as_deref()).unwrap_or_else(|_| {
                    tracing::debug!(
                        path = %path,
                        "body is not a JSON object; relaying verbatim without a model rewrite"
                    );
                    crate::relay::BodyPlan::Passthrough(out_bytes.clone())
                });

            // Outbound headers are CONSTRUCTED: the inbound map is never cloned, so a client's
            // `Authorization` — or an Anthropic client's `x-api-key` — cannot reach a third party.
            // The credential was materialised at upsert onto `LiveBackend` — no FS/env on the
            // request path (invariant 2).
            let cred_slot = cand.backend.credential.load_full();
            let mut out_headers = outbound_headers(&headers, cred_slot.as_ref().as_ref(), &[]);
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
                url: upstream_url(&meta.base_url, out_path, out_query),
                headers: out_headers,
                body: body_plan,
                deadline,
                cfg: &cfg.router,
                retry: policy,
                guard,
            };

            match attempt(pre).await {
                Ok(committed) => {
                    let protocol = meta.protocol;
                    let mut answered = relay(
                        r.clone(),
                        committed,
                        draft,
                        pk.stream,
                        cfg.router.log_usage,
                        protocol,
                        cell,
                    )
                    .await;
                    mark_warm(&mut answered, warmed);
                    return answered;
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
        // `last == None` means every candidate was **skipped** — nothing was even attempted. On a
        // warming alias that is the sequential-swap gap arriving a few nanoseconds later than
        // `resolve()`'s own health read: the drain flipped `accepting` between the two loads.
        // Rare, but "zero 5xx" is the acceptance bar, and this is the only hole left in it.
        if last.is_none() {
            if let Some(alias) = plan.alias.clone() {
                match park_on(&r, &alias, &park, plan.reason, &mut parked_once).await {
                    Parking::Retry { depth, waited_ms } => {
                        warmed = Some((depth, waited_ms));
                        continue 'dispatch;
                    }
                    Parking::Refused(resp) => return stamp(resp, draft.stamp_failed()),
                    Parking::NotWarming => {}
                }
            }
        }

        let (status, kind, msg) = match &last {
            Some(Retryable::Timeout) => (
                StatusCode::GATEWAY_TIMEOUT,
                "upstream_timeout",
                "no upstream produced response headers in time".to_owned(),
            ),
            Some(Retryable::Connect(e)) => {
                (StatusCode::BAD_GATEWAY, "upstream_unavailable", e.clone())
            }
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
        return stamp(
            error_response(ingress, status, kind, &msg),
            draft.stamp_failed(),
        );
    }
}

// ==========================================================================================
// the warm queue — ARCHITECTURE.md §4.7
// ==========================================================================================

/// What parking behind a sequential swap decided.
enum Parking {
    /// The alias is armed again. Re-resolve and dispatch — it points somewhere new.
    Retry {
        /// The queue depth this request saw, for `X-ApexRouter-Warm`.
        depth: u32,
        /// How long it waited.
        waited_ms: u32,
    },
    /// The client gets this `503` instead. It already carries `Retry-After`.
    Refused(Response),
    /// Nothing is warming on this alias, or this request has already parked once. The
    /// caller's own failure stands, unchanged.
    NotWarming,
}

/// The scalars a parked request needs to leave an honest record behind.
///
/// Borrowed from the handler's own locals rather than cloned: a request that never parks —
/// which is nearly all of them — must not pay two `String` clones for a queue it never
/// touched.
struct ParkCtx<'a> {
    id: RequestId,
    started: Instant,
    started_unix: i64,
    ingress: Protocol,
    method: &'a Method,
    path: &'a str,
}

impl ParkCtx<'_> {
    /// The header set for a `503` raised while parked: the alias (and why we picked it)
    /// are known; no backend is.
    fn stamp<'b>(&'b self, alias: &'b Alias, reason: RouteReason) -> Stamp<'b> {
        Stamp {
            id: &self.id,
            alias: Some(alias),
            reason: Some(reason),
            backend: None,
            attempts: 0,
            fallback: false,
            ingress: self.ingress,
            upstream: None,
        }
    }

    /// The record a client that hangs up **while parked** leaves behind.
    ///
    /// `ARCHITECTURE.md` §4.3 is unconditional: a request the client abandoned produces a
    /// `RequestFinished { aborted: true }`. A parked request holds no permit and no
    /// `InFlightGuard` — it never reached a backend — so without this it would be the one
    /// abandonment in the product that vanished silently.
    fn abandoned(&self, alias: &Alias, reason: RouteReason) -> RequestRecord {
        RequestRecord {
            id: self.id,
            started_unix: self.started_unix,
            alias: Some(alias.clone()),
            backend: None,
            upstream_model: None,
            route_reason: reason,
            ingress: self.ingress,
            method: self.method.to_string(),
            path: self.path.to_owned(),
            // nginx's "client closed request", exactly as `Outcome::abandoned` uses it.
            status: 499,
            attempts: 0,
            streamed: false,
            aborted: true,
            ttft_ms: None,
            // Filled in by `ParkedAbort::drop`, which is the only thing that knows how long
            // the client actually waited before it gave up.
            total_ms: 0,
            prompt_tokens: None,
            completion_tokens: None,
            cached_tokens: None,
            tok_per_s: None,
            cost: CostEstimate::Unknown,
            error: Some("client disconnected while parked behind a swap".to_owned()),
        }
    }
}

/// Broadcasts `RequestFinished { aborted: true }` if this future is dropped while parked.
///
/// The same shape as [`crate::limits::InFlightGuard`]'s abort path and for the same reason: a
/// client Ctrl-C must not leave a zombie row in either GUI. `disarm` is how a park that ended
/// on its own terms says nothing happened.
struct ParkedAbort {
    events: broadcast::Sender<Event>,
    started: Instant,
    record: Option<RequestRecord>,
}

impl ParkedAbort {
    /// The park ended normally; there is nothing to report.
    fn disarm(mut self) {
        self.record = None;
    }
}

impl Drop for ParkedAbort {
    fn drop(&mut self) {
        let Some(mut rec) = self.record.take() else {
            return;
        };
        if self.events.receiver_count() == 0 {
            return;
        }
        rec.total_ms = millis(self.started.elapsed());
        let _ = self.events.send(Event::RequestFinished {
            record: Box::new(rec),
        });
    }
}

/// Which rule sent this request at `alias`, for the record a park may have to seal.
///
/// The resolver's error does not carry it, and inventing `Alias` for a client that named
/// nothing would be a small lie in a row an operator reads. The client either spelled the
/// alias out — rule 1 — or landed there through the default, which is rule 5.
fn park_reason(model: Option<&str>, alias: &Alias) -> RouteReason {
    match model {
        Some(m) if m == alias.as_str() => RouteReason::Alias,
        _ => RouteReason::LegacyModelName,
    }
}

/// Park this request behind a sequential swap, per `ARCHITECTURE.md` §4.7.
///
/// Returns [`Parking::NotWarming`] — instantly, and having touched nothing — unless a swap
/// has an open [`crate::WarmWindow`] on this exact alias. Parking is a promise that something
/// is coming back, and a promise nobody is keeping is just a slower `503`.
///
/// The three exits §4.7 names:
///
/// * the window closes → [`Parking::Retry`], and the caller re-resolves against the
///   replacement;
/// * `warm_queue_max` is already parked → `503` + `Retry-After`, **immediately**, because
///   deepening a queue that is already the wrong answer only moves the failure later;
/// * `warm_timeout` expires → `503` + `Retry-After`.
///
/// And the fourth exit, which is the client's: dropping this future decrements the queue
/// depth and emits the `aborted` record, both from `Drop`.
async fn park_on(
    r: &Router,
    alias: &Alias,
    ctx: &ParkCtx<'_>,
    reason: RouteReason,
    once: &mut bool,
) -> Parking {
    if *once {
        return Parking::NotWarming;
    }
    let Some(slot) = r.warm().parking_for(alias) else {
        return Parking::NotWarming;
    };
    *once = true;

    // The row both GUIs show while this request waits. Re-sent on the dispatch that follows,
    // which is a Map upsert on the other end, not a duplicate.
    if r.events.receiver_count() > 0 {
        let _ = r.events.send(Event::RequestStarted {
            id: ctx.id,
            alias: Some(alias.clone()),
            backend: None,
        });
    }

    let abort = ParkedAbort {
        events: r.events.clone(),
        started: ctx.started,
        record: Some(ctx.abandoned(alias, reason)),
    };
    let outcome = slot.park().await;
    abort.disarm();

    match outcome {
        Parked::Rearmed { waited_ms, depth } => {
            tracing::debug!(alias = %alias, waited_ms, depth, "parked request released by a swap");
            Parking::Retry { depth, waited_ms }
        }
        Parked::Overflow {
            depth,
            max,
            retry_after_secs,
        } => Parking::Refused(warm_refusal(
            ctx.ingress,
            "warm_queue_full",
            &format!(
                "{alias} is warming and its parking queue is full ({depth} of {max} parked); \
                 retry in {retry_after_secs}s"
            ),
            retry_after_secs,
        )),
        Parked::TimedOut {
            waited_ms,
            retry_after_secs,
        } => Parking::Refused(warm_refusal(
            ctx.ingress,
            "warm_timeout",
            &format!(
                "{alias} was still warming after {waited_ms} ms, which is the whole budget the \
                 replacement had to start in; retry in {retry_after_secs}s"
            ),
            retry_after_secs,
        )),
    }
}

/// A `503` in the dialect the client speaks, carrying `Retry-After`.
fn warm_refusal(ingress: Protocol, kind: &str, msg: &str, retry_after_secs: u32) -> Response {
    let mut resp = error_response(ingress, StatusCode::SERVICE_UNAVAILABLE, kind, msg);
    set(
        resp.headers_mut(),
        "retry-after",
        &retry_after_secs.to_string(),
    );
    resp
}

/// Mark a response that was served *through* a swap, so the park is observable rather than
/// merely invisible. Absent on every request that did not park.
fn mark_warm(resp: &mut Response, warmed: Option<(u32, u32)>) {
    if let Some((depth, waited_ms)) = warmed {
        set(
            resp.headers_mut(),
            "x-apexrouter-warm",
            &format!("parked={depth},waited_ms={waited_ms}"),
        );
    }
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
/// Synchronous: the set is touched for a few nanoseconds with nothing awaited inside, which
/// is exactly the case a `std::sync::Mutex` is for — and it lets the same bookkeeping run
/// from a `Drop` on the relay's finish path.
fn note_collapse(r: &Router, headers: &HeaderMap, path: &str) {
    let ua = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_owned();
    let key = (ua, path.to_owned());
    // A poisoned lock means some other request panicked mid-update; the set is a debug-log
    // de-duplicator, so carrying on with it is strictly better than propagating the panic.
    let mut seen = r.collapse_seen.lock().unwrap_or_else(|e| e.into_inner());
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

// ==========================================================================================
// relay
// ==========================================================================================

/// Which `(ingress, upstream)` cell of `ARCHITECTURE.md` §3.4 a candidate is, and therefore
/// what the response side owes the client.
///
/// The two cells that answer without an upstream hop are not here: they `return` from the
/// dispatch itself.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Cell {
    /// `OpenAi → OpenAi` and `Anthropic → Anthropic`: the byte relay, verbatim both ways.
    Relay,
    /// `Anthropic → OpenAi`: R-10's translating cell. Carries the upstream model id, which is
    /// what [`SseTranslator`] echoes in `message_start` when a chunk names none.
    Translate(String),
}

/// An upstream reply the `Anthropic → OpenAi` cell cannot hand over as it stands.
struct Refused {
    /// What the client is told.
    status: StatusCode,
    /// The Anthropic `error.type` token.
    kind: &'static str,
    /// Why, in one sentence.
    msg: String,
}

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
    fn seal(&self, o: Outcome) -> RequestRecord {
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
            status: o.status,
            attempts: self.attempts,
            streamed: o.streamed,
            aborted: o.aborted,
            ttft_ms: o.ttft_ms,
            total_ms: millis(self.started.elapsed()),
            prompt_tokens: o.prompt_tokens,
            completion_tokens: o.completion_tokens,
            cached_tokens: o.cached_tokens,
            tok_per_s: o.tok_per_s,
            cost: CostEstimate::Unknown,
            error: o.error,
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
///
/// One decision, two shapes: a stream goes to [`sse_response`] — R-05, the crate's only SSE
/// implementation, which owns the frame rules, the idle timeout and the usage tee — and
/// everything else is buffered here, because `X-Usage` can only carry real numbers on a body
/// that has fully arrived.
///
/// `cell` decides what happens to the bytes on their way out, and nothing else: the retry
/// chain, the breaker, the limits, the tee, the telemetry and the `InFlightGuard` are the same
/// code on every cell, so the Anthropic ingress cannot be a bypass that quietly loses them.
async fn relay(
    r: Router,
    mut c: Committed,
    draft: RecordDraft,
    client_asked_to_stream: bool,
    log_usage: bool,
    upstream: Protocol,
    cell: Cell,
) -> Response {
    let Some(g) = c.guard.as_mut() else {
        return stamp(
            error_response(
                draft.ingress,
                StatusCode::BAD_GATEWAY,
                "upstream_unavailable",
                "the committed upstream response arrived without its in-flight guard",
            ),
            draft.stamp_failed(),
        );
    };
    g.mark_first_byte();
    let ttft_ms = Some(millis(draft.started.elapsed()));

    // `text/event-stream` is forced ONLY when upstream is 2xx *and* already says so: a
    // `400 {"error":…}` on a `stream: true` request must reach the client as JSON. That rule
    // is R-05's `is_event_stream`, asked here rather than restated, so this branch and the
    // relay's own header decoration can never disagree about what a stream is.
    let status = c.response.status();
    let streaming = client_asked_to_stream && is_event_stream(status, c.response.headers());

    // The stamp is computed here because on the streaming path `draft` moves into the
    // relay's finish callback. `insert`, never `extend`: an upstream `Via` has to be
    // replaced by ours rather than appended to.
    let mut marks = HeaderMap::new();
    apply(
        &mut marks,
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
    );

    if streaming {
        // **The one SSE relay** (R-05, `relay::stream`). It builds the response headers,
        // owns the guard, and calls back exactly once from its `Drop` — on a clean end, an
        // upstream death, an idle gap or a client disconnect alike, with `aborted` saying
        // which. Nothing about framing, timeouts or the usage tee lives in this file.
        let cfg = r.cfg.load_full();
        let code = status.as_u16();
        let router = r.clone();
        let mut resp = sse_response(
            c,
            &cfg.router,
            Box::new(move |guard, out| {
                finalize(
                    &router,
                    &draft,
                    guard,
                    Outcome::streamed(code, ttft_ms, &out),
                    log_usage,
                );
            }),
        );
        // The streaming half of the one translating cell, wrapped **around** R-05 rather than
        // spliced into it: the relay above still owns the guard, the idle timeout, the tee and
        // the single `Drop` that seals the record, and the Anthropic frames are rebuilt from
        // the OpenAI bytes on their way past.
        if let Cell::Translate(model) = cell {
            let inner = std::mem::replace(resp.body_mut(), Body::empty());
            *resp.body_mut() = anthropic_stream(inner, model);
        }
        stamp_with(&mut resp, &marks);
        return resp;
    }

    let Committed {
        response: up,
        guard,
    } = c;
    let mut head = response_headers(up.headers());
    let idle = Duration::from_millis(r.cfg.load().router.idle_timeout_ms);

    // ---- buffered ---------------------------------------------------------------------------
    let body = match tokio::time::timeout(idle, up.bytes()).await {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => {
            let msg = e.to_string();
            finalize(
                &r,
                &draft,
                guard,
                Outcome::buffered(
                    StatusCode::BAD_GATEWAY.as_u16(),
                    ttft_ms,
                    None,
                    None,
                    Some(msg.clone()),
                ),
                log_usage,
            );
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
                guard,
                Outcome::buffered(
                    StatusCode::GATEWAY_TIMEOUT.as_u16(),
                    ttft_ms,
                    None,
                    None,
                    Some("upstream idle timeout".to_owned()),
                ),
                log_usage,
            );
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

    // ---- the buffered half of the one translating cell (R-10) --------------------------------
    // Before `finalize`, so the ring row, the WebSocket event and the usage line carry the
    // status the client actually received. The token counts were already read off the
    // upstream's own body above, so the telemetry is upstream's numbers on either branch.
    let (status, body) = match &cell {
        Cell::Relay => (status, body),
        Cell::Translate(_) => match anthropic_buffered(status, &body) {
            Ok(b) => (status, b),
            Err(refused) => {
                finalize(
                    &r,
                    &draft,
                    guard,
                    Outcome::buffered(
                        refused.status.as_u16(),
                        ttft_ms,
                        usage,
                        timings,
                        Some(refused.msg.clone()),
                    ),
                    log_usage,
                );
                let mut resp = error_response(
                    Protocol::Anthropic,
                    refused.status,
                    refused.kind,
                    &refused.msg,
                );
                stamp_with(&mut resp, &marks);
                return resp;
            }
        },
    };

    finalize(
        &r,
        &draft,
        guard,
        Outcome::buffered(status.as_u16(), ttft_ms, usage, timings, None),
        log_usage,
    );

    let mut resp = Response::new(Body::from(body));
    *resp.status_mut() = status;
    *resp.headers_mut() = head;
    stamp_with(&mut resp, &marks);
    resp
}

// ==========================================================================================
// the response half of the `Anthropic -> OpenAi` cell
// ==========================================================================================

/// A buffered OpenAI reply, as the Anthropic Messages API would have written it.
///
/// A 2xx goes through R-10's [`response_to_anthropic`]. Anything else is an upstream error
/// written in the OpenAI dialect, and an Anthropic SDK cannot read one — so it is re-rendered
/// in the Anthropic shape carrying the upstream's own status and message, rather than handed
/// over as a body the client will fail to parse and report as "unknown error". A 2xx that will
/// not translate is a `502`: the request did reach a model, and the failure is this proxy's.
fn anthropic_buffered(status: StatusCode, body: &Bytes) -> Result<Bytes, Refused> {
    if !status.is_success() {
        return Err(Refused {
            status,
            kind: anthropic_error_kind(status),
            msg: upstream_message(body),
        });
    }
    match response_to_anthropic(body) {
        Ok(v) => Ok(Bytes::from(v)),
        Err(e) => Err(Refused {
            status: StatusCode::BAD_GATEWAY,
            kind: "api_error",
            msg: format!("the upstream reply could not be translated to the Messages API: {e}"),
        }),
    }
}

/// The Anthropic `error.type` token for an upstream status.
///
/// An Anthropic SDK turns these into distinct exception classes and retries on only some of
/// them, so an upstream `429` has to arrive as `rate_limit_error` rather than as a generic
/// `api_error` that a harness will give up on.
fn anthropic_error_kind(status: StatusCode) -> &'static str {
    match status.as_u16() {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        529 => "overloaded_error",
        _ => "api_error",
    }
}

/// The most useful sentence in an upstream error body, whatever dialect it is in.
///
/// `error.message` is where both OpenAI and llama.cpp put it. A body that is neither is quoted
/// verbatim up to [`UPSTREAM_MSG_CHARS`], because a truncated real message beats a generic one.
fn upstream_message(body: &Bytes) -> String {
    let parsed: Option<serde_json::Value> = serde_json::from_slice(body).ok();
    let found = parsed
        .as_ref()
        .and_then(|v| {
            v.pointer("/error/message")
                .or_else(|| v.pointer("/message"))
        })
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    found.unwrap_or_else(|| {
        String::from_utf8_lossy(body)
            .chars()
            .take(UPSTREAM_MSG_CHARS)
            .collect()
    })
}

/// One OpenAI SSE stream, re-framed as the Anthropic named-event stream.
///
/// Wraps the body R-05 produced instead of reaching into it. R-05 still owns the
/// [`InFlightGuard`], the idle timeout, the usage tee and the one `Drop` that seals the
/// record; dropping this wrapper drops that body, so a client who hangs up mid-answer still
/// produces exactly one `aborted` record and still frees the llama.cpp slot.
///
/// `model` is what [`SseTranslator`] echoes in `message_start` when an upstream chunk names
/// none.
fn anthropic_stream(inner: Body, model: String) -> Body {
    /// The pump's state: the upstream bytes, the state machine, and frames not yet handed out.
    struct St {
        inner: axum::body::BodyDataStream,
        tr: Option<SseTranslator>,
        out: VecDeque<Bytes>,
    }
    let st = St {
        inner: inner.into_data_stream(),
        tr: Some(SseTranslator::new(model)),
        out: VecDeque::new(),
    };
    Body::from_stream(futures_util::stream::unfold(st, |mut s| async move {
        loop {
            if let Some(frame) = s.out.pop_front() {
                return Some((Ok::<Bytes, std::convert::Infallible>(frame), s));
            }
            // `tr` is taken exactly once, by whichever end arrives first; once it is gone the
            // closing frames have been queued and drained, and the response is over.
            s.tr.as_ref()?;
            match s.inner.next().await {
                Some(Ok(chunk)) => {
                    if let Some(tr) = s.tr.as_mut() {
                        let frames = tr.feed(&chunk);
                        s.out.extend(frames);
                    }
                }
                // R-05 has already turned an upstream death or an idle gap into its synthetic
                // frame pair, which the translator reads as an `error` event followed by a
                // clean close. An error this far out is the body itself failing, and the
                // honest answer to it and to a bare EOF alike is to close every open block.
                Some(Err(_)) | None => {
                    if let Some(tr) = s.tr.take() {
                        s.out.extend(tr.finish());
                    }
                }
            }
        }
    }))
}

/// What a finished request turned out to be.
///
/// The token counts are already `TokenCount`s rather than raw `UsageFields`, because the two
/// paths degrade differently: a buffered body either reported usage or did not, while a
/// stream can estimate from its frame count. Both spellings are constructed once, here.
struct Outcome {
    status: u16,
    streamed: bool,
    aborted: bool,
    ttft_ms: Option<u32>,
    prompt_tokens: Option<TokenCount>,
    completion_tokens: Option<TokenCount>,
    cached_tokens: Option<u32>,
    tok_per_s: Option<f32>,
    error: Option<String>,
}

impl Outcome {
    /// A buffered response, whose whole body was parsed for `usage` and `timings`.
    fn buffered(
        status: u16,
        ttft_ms: Option<u32>,
        usage: Option<UsageFields>,
        timings: Option<Timings>,
        error: Option<String>,
    ) -> Outcome {
        Outcome {
            status,
            streamed: false,
            aborted: false,
            ttft_ms,
            prompt_tokens: usage.map(|u| TokenCount::Reported(u.prompt_tokens)),
            completion_tokens: usage.map(|u| TokenCount::Reported(u.completion_tokens)),
            cached_tokens: usage
                .and_then(|u| u.cached_tokens)
                .or_else(|| timings.map(|t| t.cache_n)),
            tok_per_s: timings.map(|t| t.predicted_per_second),
            error,
        }
    }

    /// A request the client walked away from before anything could seal a real outcome.
    ///
    /// Armed onto the `InFlightGuard` so its `Drop` has something to broadcast. `499` is
    /// nginx's "client closed request", which is exactly what happened; `Drop` fills in
    /// `total_ms` and `ttft_ms` itself.
    fn abandoned() -> Outcome {
        Outcome {
            status: 499,
            streamed: false,
            aborted: true,
            ttft_ms: None,
            prompt_tokens: None,
            completion_tokens: None,
            cached_tokens: None,
            tok_per_s: None,
            error: Some("client disconnected".to_owned()),
        }
    }

    /// A streamed response, exactly as the one SSE relay reported it.
    ///
    /// `aborted` comes from the relay because the relay is the only thing that knows whether
    /// the client or the upstream went first, and the completion count degrades to
    /// `TokenCount::Estimated` rather than to `None` when the tail carried no usage.
    fn streamed(status: u16, ttft_ms: Option<u32>, out: &StreamOutcome) -> Outcome {
        Outcome {
            status,
            streamed: out.streamed,
            aborted: out.aborted,
            ttft_ms,
            prompt_tokens: out.prompt_tokens(),
            completion_tokens: Some(out.completion_tokens()),
            cached_tokens: out.cached_tokens(),
            tok_per_s: out.tok_per_s(),
            error: out.error().map(str::to_owned),
        }
    }
}

/// Release the guard, broadcast, ring-buffer and append the usage row — exactly once.
///
/// **Synchronous on purpose.** The streaming path calls this from the relay's `Drop`, which
/// is what makes a client that hung up mid-stream produce exactly one record — marked
/// `aborted`, carrying whatever the tee had already read — instead of vanishing. Nothing
/// here awaits: the ring is a `std::sync::Mutex` held for two pushes, and the usage writer
/// appends a line.
fn finalize(
    r: &Router,
    draft: &RecordDraft,
    guard: Option<InFlightGuard>,
    outcome: Outcome,
    log_usage: bool,
) {
    let rec = draft.seal(outcome);
    // Exactly one `RequestFinished` per request: the guard broadcasts the record it is
    // handed — `aborted` and all — and its `Drop` only invents one when `finish()` never
    // ran at all. The handler emits directly if there is no guard to do it, which cannot
    // happen on a served request and is here so a future call site cannot lose a record.
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
        // A poisoned ring means another request panicked while pushing. The ring is a
        // display buffer; carrying on with it beats turning that into a second panic.
        let mut ring = r.ring.lock().unwrap_or_else(|e| e.into_inner());
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

/// Apply an already-rendered stamp to a response.
///
/// The relay renders its stamp before handing the draft to the SSE relay's finish callback,
/// so this is what puts it on afterwards. `insert` rather than `extend`, so an upstream
/// header of the same name — a `Via` from a proxy in front of llama.cpp — is replaced by
/// ours rather than appended to.
fn stamp_with(resp: &mut Response, marks: &HeaderMap) {
    let h = resp.headers_mut();
    for (name, value) in marks {
        h.insert(name.clone(), value.clone());
    }
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
/// The OpenAI shape comes from R-06, the Anthropic shape from R-10. Neither is spelled out
/// here, so an error this file invents and an error the ingress translator invents cannot
/// drift apart.
fn error_response(ingress: Protocol, status: StatusCode, kind: &str, msg: &str) -> Response {
    match ingress {
        Protocol::OpenAi => openai_error(status, kind, msg),
        Protocol::Anthropic => crate::anthropic::anthropic_error(status, kind, msg),
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
        Backend, BackendKind, BackendLimits, BackendSelector, CredentialSource, Health, ModelRoute,
        Provenance, RetryPolicy, RouteFile, RouteFilter, RouteTarget, Strategy, UpstreamModel,
    };
    use axum::body::to_bytes as body_to_bytes;
    use axum::extract::Request;
    use futures_util::StreamExt;
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

    /// A route carrying the shipped default `[retry]` — i.e. what `routes.toml` compiles to
    /// when it declares no `[routes.retry]` block at all.
    fn route(alias: &str, targets: Vec<&str>) -> ModelRoute {
        route_with_retry(alias, targets, RetryPolicy::default())
    }

    /// A route with an explicit `[retry]` block, as `routes.toml` writes one.
    fn route_with_retry(alias: &str, targets: Vec<&str>, retry: RetryPolicy) -> ModelRoute {
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
            retry,
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

    // ---- `routes.toml`'s `[retry]` block, which used to be silently ignored -----------------

    /// A pair of upstreams: the first always 502s, the second always answers 200. How many of
    /// them get touched is entirely a function of the route's `[retry]` block.
    async fn bad_then_good() -> (MockServer, MockServer) {
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
        (bad, good)
    }

    #[tokio::test]
    async fn a_per_route_attempts_of_one_stops_where_the_default_would_have_failed_over() {
        // `[routes.retry] attempts = 1` — the `coder` route in `routes.example.toml`.
        // Same table, same upstreams, same request as the default-policy test below; the
        // only difference is the config value, and it has to change the outcome.
        let (bad, good) = bad_then_good().await;
        let r = router_with(
            vec![
                backend("up-bad", &bad.uri()),
                backend("up-good", &good.uri()),
            ],
            vec![route_with_retry(
                "auto",
                vec!["up-bad", "up-good"],
                RetryPolicy {
                    attempts: 1,
                    failover: true,
                    honor_retry_after: true,
                },
            )],
        );
        let app = proxy_router(r);
        let resp = call(&app, post_chat(r#"{"model":"auto"}"#)).await;

        assert_eq!(
            resp.status(),
            StatusCode::BAD_GATEWAY,
            "attempts = 1 must not reach the second candidate"
        );
        assert_eq!(header(&resp, "x-apexrouter-attempts"), Some("1"));
        assert_eq!(
            good.received_requests().await.expect("recording").len(),
            0,
            "the healthy backend was never supposed to be dialled"
        );
    }

    #[tokio::test]
    async fn a_route_that_declares_no_retry_block_gets_the_shipped_default() {
        // The same table with `RetryPolicy::default()` — what a `routes.toml` route with no
        // `[routes.retry]` compiles to. Two attempts, failover on, so it succeeds.
        let (bad, good) = bad_then_good().await;
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
        assert_eq!(header(&resp, "x-apexrouter-backend"), Some("up-good"));
        assert_eq!(
            good.received_requests().await.expect("recording").len(),
            1,
            "the default policy fails over exactly once"
        );
    }

    #[tokio::test]
    async fn a_per_route_failover_of_false_never_leaves_the_first_backend() {
        // `attempts = 3` with `failover = false`: three tries are allowed, but none of them
        // may go to a *different* backend, so the chain stops at one.
        let (bad, good) = bad_then_good().await;
        let r = router_with(
            vec![
                backend("up-bad", &bad.uri()),
                backend("up-good", &good.uri()),
            ],
            vec![route_with_retry(
                "auto",
                vec!["up-bad", "up-good"],
                RetryPolicy {
                    attempts: 3,
                    failover: false,
                    honor_retry_after: true,
                },
            )],
        );
        let app = proxy_router(r);
        let resp = call(&app, post_chat(r#"{"model":"auto"}"#)).await;

        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(header(&resp, "x-apexrouter-attempts"), Some("1"));
        assert_eq!(
            good.received_requests().await.expect("recording").len(),
            0,
            "failover = false must not dial a second backend"
        );
    }

    #[tokio::test]
    async fn a_per_route_attempts_of_three_walks_three_candidates() {
        // The `big` route in `routes.example.toml`. The default policy would have stopped
        // after two, leaving the third — healthy — backend untouched.
        let (bad, good) = bad_then_good().await;
        let worse = MockServer::start().await;
        Mock::given(m_method("POST"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&worse)
            .await;

        let r = router_with(
            vec![
                backend("up-bad", &bad.uri()),
                backend("up-worse", &worse.uri()),
                backend("up-good", &good.uri()),
            ],
            vec![route_with_retry(
                "auto",
                vec!["up-bad", "up-worse", "up-good"],
                RetryPolicy {
                    attempts: 3,
                    failover: true,
                    honor_retry_after: true,
                },
            )],
        );
        let app = proxy_router(r);
        let resp = call(&app, post_chat(r#"{"model":"auto"}"#)).await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(header(&resp, "x-apexrouter-attempts"), Some("3"));
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

    // ---- the one translating cell: Anthropic -> OpenAi (R-10, wired here) ---------------------

    /// A buffered OpenAI `ChatCompletion`, in llama.cpp's own spelling.
    const OPENAI_REPLY: &[u8] =
        br#"{"id":"chatcmpl-9","object":"chat.completion","model":"carnice",
      "choices":[{"index":0,"message":{"role":"assistant","content":"Hi there."},
      "finish_reason":"stop"}],
      "usage":{"prompt_tokens":11,"completion_tokens":3,"total_tokens":14}}"#;

    /// The smallest body the Messages API accepts.
    const MINIMAL: &str =
        r#"{"model":"auto","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}"#;

    async fn json_of(resp: Response) -> serde_json::Value {
        let body = body_to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .expect("body");
        serde_json::from_slice(&body).expect("json")
    }

    fn openai_upstream_at(up: &MockServer) -> Router {
        router_with(
            vec![backend("up-a", &up.uri())],
            vec![route("auto", vec!["up-a"])],
        )
    }

    #[tokio::test]
    async fn anthropic_into_an_openai_backend_is_translated_both_ways() {
        let up = MockServer::start().await;
        Mock::given(m_method("POST"))
            .and(m_path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(OPENAI_REPLY.to_vec(), "application/json"),
            )
            .expect(1)
            .mount(&up)
            .await;
        let app = proxy_router(openai_upstream_at(&up));

        let resp = call(
            &app,
            anthropic_post(
                r#"{"model":"auto","max_tokens":64,"system":"Be terse.",
                    "messages":[{"role":"user","content":[{"type":"text","text":"Hello"}]}]}"#,
            ),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            header(&resp, "x-apexrouter-protocol"),
            Some("anthropic->open_ai")
        );
        assert_eq!(header(&resp, "x-apexrouter-backend"), Some("up-a"));
        assert_eq!(header(&resp, "x-usage"), Some("11+3"));

        // The response the client sees is an Anthropic `Message`, not a ChatCompletion.
        let v = json_of(resp).await;
        assert_eq!(v["type"], "message");
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["id"], "msg_chatcmpl-9");
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(v["content"][0]["text"], "Hi there.");
        assert_eq!(v["stop_reason"], "end_turn");
        assert_eq!(v["usage"]["input_tokens"], 11);
        assert_eq!(v["usage"]["output_tokens"], 3);
        assert!(v["usage"].get("prompt_tokens").is_none());

        // …and what reached the upstream is a ChatCompletion on the rewritten path, with the
        // top-level `system` hoisted into a system message and the client's credential gone.
        let seen = up.received_requests().await.expect("recording");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].url.path(), "/v1/chat/completions");
        assert!(seen[0].headers.get("x-api-key").is_none());
        assert!(seen[0].headers.get("anthropic-version").is_none());
        let sent: serde_json::Value = serde_json::from_slice(&seen[0].body).expect("json");
        assert_eq!(
            sent["model"], "carnice",
            "resolve()'s model rewrite still runs"
        );
        assert_eq!(sent["max_tokens"], 64);
        assert_eq!(sent["messages"][0]["role"], "system");
        assert_eq!(sent["messages"][0]["content"], "Be terse.");
        assert_eq!(sent["messages"][1]["role"], "user");
        assert_eq!(sent["messages"][1]["content"], "Hello");
    }

    /// Claude Code sends `POST /v1/messages?beta=true`. The path is rewritten, so the query
    /// has to go with it: `/v1/chat/completions?beta=true` is a parameter attached to an
    /// endpoint that never defined it. llama.cpp ignores it; a strict OpenAI upstream need
    /// not. The byte-relay cells keep the client's query — `upstream_url` still carries one
    /// when it is given one, which its own test asserts.
    #[tokio::test]
    async fn the_translating_cell_drops_the_query_string() {
        let up = MockServer::start().await;
        Mock::given(m_method("POST"))
            .and(m_path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(OPENAI_REPLY.to_vec(), "application/json"),
            )
            .expect(1)
            .mount(&up)
            .await;
        let app = proxy_router(openai_upstream_at(&up));

        let req = Request::builder()
            .method(Method::POST)
            .uri("/v1/messages?beta=true")
            .header("content-type", "application/json")
            .header("anthropic-version", "2023-06-01")
            .body(Body::from(
                r#"{"model":"auto","max_tokens":64,
                    "messages":[{"role":"user","content":"Hello"}]}"#
                    .to_owned(),
            ))
            .expect("request");
        let resp = call(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let seen = up.received_requests().await.expect("recording");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].url.path(), "/v1/chat/completions");
        assert_eq!(
            seen[0].url.query(),
            None,
            "?beta=true is an Anthropic-side concern and means nothing on chat/completions"
        );
    }

    #[tokio::test]
    async fn a_missing_max_tokens_is_a_400_and_costs_no_upstream_hop() {
        // Required in Anthropic, optional in OpenAI — so it can only be caught here, and it
        // must never be defaulted silently.
        let up = MockServer::start().await;
        Mock::given(m_method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&up)
            .await;
        let app = proxy_router(openai_upstream_at(&up));

        let resp = call(
            &app,
            anthropic_post(r#"{"model":"auto","messages":[{"role":"user","content":"hi"}]}"#),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            header(&resp, "x-apexrouter-protocol"),
            Some("anthropic->open_ai")
        );
        let v = json_of(resp).await;
        assert_eq!(v["type"], "error");
        assert_eq!(v["error"]["type"], "invalid_request_error");
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("max_tokens"),
            "{v}"
        );
    }

    #[tokio::test]
    async fn tools_with_the_flag_off_are_refused_loudly_and_never_stripped() {
        // The failure mode this prevents: an agent asks for tools, the proxy quietly drops
        // them, and the model answers in prose as if it had none.
        //
        // `anthropic_tools` now defaults to **true** (CHARTER 2026-07-31), so "off" has to
        // be said out loud here. The refusal itself is unchanged and is still the whole
        // point: an operator who turns translation off gets a `400` naming the key, never
        // silence.
        let up = MockServer::start().await;
        Mock::given(m_method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&up)
            .await;
        let mut cfg = test_config();
        cfg.router.anthropic_tools = false;
        let app = proxy_router(router_of(
            cfg,
            vec![backend("up-a", &up.uri())],
            vec![route("auto", vec!["up-a"])],
        ));

        let resp = call(
            &app,
            anthropic_post(
                r#"{"model":"auto","max_tokens":64,
                    "messages":[{"role":"user","content":"weather?"}],
                    "tools":[{"name":"get_weather","description":"w",
                              "input_schema":{"type":"object","properties":{}}}]}"#,
            ),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let v = json_of(resp).await;
        assert_eq!(v["error"]["type"], "invalid_request_error");
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("anthropic_tools"),
            "the message must name the config key: {v}"
        );
    }

    #[tokio::test]
    async fn a_stock_config_translates_tools_because_claude_code_always_sends_them() {
        // CHARTER 2026-07-31: `anthropic_tools` defaults to **true**. Real Claude Code sends
        // 92 tool definitions on every request, so an off-by-default flag made this endpoint
        // a `400` on request one for the one client the Anthropic ingress exists to serve.
        // The guard is the *default* config — no knob touched anywhere in this test.
        let up = MockServer::start().await;
        Mock::given(m_method("POST"))
            .and(m_path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(OPENAI_REPLY.to_vec(), "application/json"),
            )
            .expect(1)
            .mount(&up)
            .await;
        let app = proxy_router(openai_upstream_at(&up));

        let resp = call(
            &app,
            anthropic_post(
                r#"{"model":"auto","max_tokens":64,
                    "messages":[{"role":"user","content":"weather?"}],
                    "tools":[{"name":"get_weather","description":"w",
                              "input_schema":{"type":"object",
                                              "properties":{"city":{"type":"string"}}}}]}"#,
            ),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK, "a stock config must not 400");

        // …and the tool actually crossed the boundary in OpenAI spelling, rather than being
        // accepted and quietly dropped — which is the other way this could "pass".
        let seen = up.received_requests().await.expect("recording");
        assert_eq!(seen.len(), 1);
        let sent: serde_json::Value = serde_json::from_slice(&seen[0].body).expect("json");
        assert_eq!(sent["tools"][0]["type"], "function");
        assert_eq!(sent["tools"][0]["function"]["name"], "get_weather");
        assert_eq!(
            sent["tools"][0]["function"]["parameters"]["properties"]["city"]["type"], "string",
            "input_schema becomes parameters, whole: {sent}"
        );
    }

    #[tokio::test]
    async fn a_missing_or_unknown_anthropic_version_is_a_400_before_anything_else() {
        let up = MockServer::start().await;
        Mock::given(m_method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&up)
            .await;
        let app = proxy_router(openai_upstream_at(&up));

        for version in [None, Some("1999-01-01")] {
            let mut b = Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .header("content-type", "application/json");
            if let Some(v) = version {
                b = b.header("anthropic-version", v);
            }
            let req = b
                .body(Body::from(r#"{"model":"auto","max_tokens":8}"#.to_owned()))
                .expect("request");
            let resp = call(&app, req).await;
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{version:?}");
            let v = json_of(resp).await;
            assert_eq!(v["type"], "error");
            assert!(
                v["error"]["message"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("anthropic-version"),
                "{v}"
            );
        }
    }

    #[tokio::test]
    async fn a_streamed_message_becomes_the_six_named_events_in_order() {
        const FRAMES: &[u8] = concat!(
            "data: {\"id\":\"chatcmpl-7\",\"model\":\"carnice\",\"choices\":",
            "[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hi\"}}]}\n\n",
            "data: {\"id\":\"chatcmpl-7\",\"choices\":",
            "[{\"index\":0,\"delta\":{\"content\":\" there\"}}]}\n\n",
            "data: {\"id\":\"chatcmpl-7\",\"choices\":",
            "[{\"index\":0,\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
            "data: {\"id\":\"chatcmpl-7\",\"choices\":[],",
            "\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n"
        )
        .as_bytes();

        let up = MockServer::start().await;
        Mock::given(m_method("POST"))
            .and(m_path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(FRAMES.to_vec(), "text/event-stream"),
            )
            .expect(1)
            .mount(&up)
            .await;
        let app = proxy_router(openai_upstream_at(&up));

        let resp = call(
            &app,
            anthropic_post(
                r#"{"model":"auto","max_tokens":8,"stream":true,
                    "messages":[{"role":"user","content":"hi"}]}"#,
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(header(&resp, "content-type"), Some("text/event-stream"));
        assert_eq!(
            header(&resp, "x-apexrouter-protocol"),
            Some("anthropic->open_ai")
        );

        let body = body_to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .expect("body");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        let names: Vec<&str> = text
            .lines()
            .filter_map(|l| l.strip_prefix("event: "))
            .collect();
        assert_eq!(
            names,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ],
            "{text}"
        );
        assert!(
            !text.contains("data: [DONE]"),
            "an Anthropic stream has no [DONE]: {text}"
        );

        // Indices, the mapped stop reason and the final usage.
        let payloads: Vec<serde_json::Value> = text
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter_map(|d| serde_json::from_str(d).ok())
            .collect();
        for p in &payloads {
            if let Some(i) = p.get("index") {
                assert_eq!(i, 0, "one text block means index 0 throughout");
            }
        }
        let delta = payloads
            .iter()
            .find(|p| p["type"] == "message_delta")
            .expect("message_delta");
        assert_eq!(delta["delta"]["stop_reason"], "max_tokens");
        assert_eq!(delta["usage"]["output_tokens"], 2);
    }

    #[tokio::test]
    async fn an_upstream_error_reaches_an_anthropic_client_in_its_own_shape() {
        // An OpenAI error body handed to an Anthropic SDK reads as "unknown error". The
        // status and the upstream's own sentence both survive the re-render.
        let up = MockServer::start().await;
        Mock::given(m_method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_raw(
                br#"{"error":{"message":"context window exceeded","type":"invalid_request_error"}}"#
                    .to_vec(),
                "application/json",
            ))
            .mount(&up)
            .await;
        let app = proxy_router(openai_upstream_at(&up));

        let resp = call(&app, anthropic_post(MINIMAL)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let v = json_of(resp).await;
        assert_eq!(v["type"], "error");
        assert_eq!(v["error"]["type"], "invalid_request_error");
        assert_eq!(v["error"]["message"], "context window exceeded");
    }

    #[tokio::test]
    async fn the_translating_cell_keeps_the_retry_chain_and_the_record() {
        // The Anthropic cell must not be a bypass: failover, the attempt count and the
        // `RequestFinished` record are the same code as the OpenAI path.
        let dead = MockServer::start().await;
        Mock::given(m_method("POST"))
            .respond_with(
                ResponseTemplate::new(502).set_body_raw(b"{}".to_vec(), "application/json"),
            )
            .mount(&dead)
            .await;
        let good = MockServer::start().await;
        Mock::given(m_method("POST"))
            .and(m_path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(OPENAI_REPLY.to_vec(), "application/json"),
            )
            .expect(1)
            .mount(&good)
            .await;

        let r = router_with(
            vec![
                backend("up-dead", &dead.uri()),
                backend("up-good", &good.uri()),
            ],
            vec![route("auto", vec!["up-dead", "up-good"])],
        );
        let mut rx = r.events.subscribe();
        let app = proxy_router(r);

        let resp = call(&app, anthropic_post(MINIMAL)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(header(&resp, "x-apexrouter-backend"), Some("up-good"));
        assert_eq!(header(&resp, "x-apexrouter-attempts"), Some("2"));
        assert_eq!(header(&resp, "x-apexrouter-fallback"), Some("true"));
        let v = json_of(resp).await;
        assert_eq!(v["type"], "message");

        let mut finished = 0;
        while let Ok(ev) = rx.try_recv() {
            if let Event::RequestFinished { record } = ev {
                assert_eq!(record.ingress, Protocol::Anthropic);
                assert_eq!(record.prompt_tokens.map(TokenCount::value), Some(11));
                finished += 1;
            }
        }
        assert_eq!(
            finished, 1,
            "exactly one sealed record on the Anthropic path"
        );
    }

    #[tokio::test]
    async fn the_translating_cell_is_only_ever_reached_for_v1_messages() {
        // R-10 translates one body shape and rewrites one path. An `anthropic-version` header
        // on anything else must stay a byte relay, not become a `400` from handing
        // `request_to_openai` a body that was never a MessagesRequest.
        let up = MockServer::start().await;
        Mock::given(m_method("POST"))
            .and(m_path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                br#"{"object":"list","data":[]}"#.to_vec(),
                "application/json",
            ))
            .expect(1)
            .mount(&up)
            .await;
        let app = proxy_router(openai_upstream_at(&up));

        let req = Request::builder()
            .method(Method::POST)
            .uri("/v1/embeddings")
            .header("content-type", "application/json")
            .header("anthropic-version", "2023-06-01")
            .body(Body::from(r#"{"model":"auto","input":"hi"}"#.to_owned()))
            .expect("request");
        let resp = call(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let seen = up.received_requests().await.expect("recording");
        assert_eq!(
            seen[0].url.path(),
            "/v1/embeddings",
            "path is not rewritten"
        );
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

    // The synthetic mid-stream frame and the usage tee used to be duplicated here, against a
    // second copy of the relay. There is now one relay (`relay::stream`) and one set of unit
    // tests for those rules, in that file; what this file tests is the wiring and the
    // end-to-end behaviour through `proxy_router`, below.

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
        //
        // The `[DONE]` terminator is part of the fixture because the relay treats an SSE
        // stream that stops without one as a truncation and appends a synthetic frame —
        // see `a_stream_that_stops_without_done_is_never_silently_truncated`.
        const GAP: Duration = Duration::from_millis(120);
        let mut frames: Vec<Vec<u8>> = (0..6)
            .map(|i| format!("data: {{\"i\":{i}}}\n\n").into_bytes())
            .collect();
        frames.push(b"data: [DONE]\n\n".to_vec());
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
    async fn a_stream_that_stops_without_done_is_never_silently_truncated() {
        // ARCHITECTURE §4.4: mid-stream upstream death has a defined client-visible
        // behaviour — exactly one synthetic frame plus `[DONE]`, never a silent truncation.
        // `relay::stream` unit-tests the rule; this proves the shipping path is wired to it,
        // which the handler's own former copy of the relay never did.
        let capture = llama_capture();
        let truncated = capture[..capture.len() - b"data: [DONE]\n\n".len()].to_vec();
        let base = sse_upstream(vec![truncated.clone()], Duration::from_millis(0)).await;
        let r = router_with(
            vec![backend("up-a", &base)],
            vec![route("auto", vec!["up-a"])],
        );
        let mut rx = r.events.subscribe();
        let app = proxy_router(r);
        let resp = call(&app, post_chat(r#"{"model":"auto","stream":true}"#)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_to_bytes(resp.into_body(), 4 * 1024 * 1024)
            .await
            .expect("body");

        let text = String::from_utf8_lossy(&body);
        assert!(
            text.starts_with(&String::from_utf8_lossy(&truncated).to_string()),
            "the prefix must still relay verbatim"
        );
        assert_eq!(
            text.matches("upstream ended mid-stream").count(),
            1,
            "exactly one synthetic frame"
        );
        assert!(text.ends_with("data: [DONE]\n\n"));

        let mut record = None;
        while let Ok(ev) = rx.try_recv() {
            if let Event::RequestFinished { record: rec } = ev {
                record = Some(rec);
            }
        }
        let rec = record.expect("a RequestFinished for the truncated stream");
        assert_eq!(rec.error.as_deref(), Some("upstream ended mid-stream"));
        assert!(!rec.aborted, "the upstream went first, not the client");
    }

    #[tokio::test]
    async fn a_client_that_hangs_up_mid_stream_settles_exactly_one_aborted_record() {
        // The end-to-end half of the disconnect-leak regression test (`attempt::tests` owns
        // the guard-level half). The relay reports its outcome from `Drop`, so an abandoned
        // stream is sealed by the same code path as a completed one — same permit release,
        // same ring row, and `aborted: true` because the upstream was still open. Before the
        // two relays were merged this path reached no telemetry at all beyond the guard's
        // own partial record.
        let frames: Vec<Vec<u8>> = (0..8)
            .map(|i| format!("data: {{\"i\":{i}}}\n\n").into_bytes())
            .collect();
        let base = sse_upstream(frames, Duration::from_millis(40)).await;
        let r = router_with(
            vec![backend("up-a", &base)],
            vec![route("auto", vec!["up-a"])],
        );
        let be = r
            .registry()
            .get(&BackendId::parse("up-a").expect("id"))
            .expect("backend");
        let permits_before = be.sem.available_permits();
        let mut rx = r.events.subscribe();
        let app = proxy_router(r);

        let resp = call(&app, post_chat(r#"{"model":"auto","stream":true}"#)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        {
            let mut stream = resp.into_body().into_data_stream();
            let first = stream.next().await.expect("one chunk").expect("chunk");
            assert!(!first.is_empty());
            // The client Ctrl-Cs here: axum drops the body, which drops the relay.
        }

        let mut finished = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let Event::RequestFinished { record } = ev {
                finished.push(record);
            }
        }
        assert_eq!(
            finished.len(),
            1,
            "exactly one record, never two and never none"
        );
        assert!(
            finished[0].aborted,
            "the upstream was still open, so the client left first"
        );
        assert!(finished[0].streamed);
        assert_eq!(
            be.inflight.load(Ordering::Acquire),
            0,
            "the in-flight gauge came back"
        );
        assert_eq!(
            be.sem.available_permits(),
            permits_before,
            "the permit came back"
        );
    }

    #[tokio::test]
    async fn a_client_that_hangs_up_waiting_for_a_buffered_body_is_recorded_too() {
        // The buffered twin of the test above, and the one that proves `guard.record` is
        // armed: the request task is cancelled while `relay` is awaiting the upstream body,
        // so nothing in `relay` runs at all and the guard's `Drop` is the only thing left to
        // speak. It used to have a `None` record and therefore said nothing — a GPU
        // generating for a client that had already gone, invisible in every surface.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf).await;
            // Headers, then a body that never completes.
            let head = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Transfer-Encoding: chunked\r\n\r\n1\r\n{\r\n";
            let _ = sock.write_all(head).await;
            let _ = sock.flush().await;
            futures_util::future::pending::<()>().await;
        });

        let r = router_with(
            vec![backend("up-a", &format!("http://{addr}"))],
            vec![route("auto", vec!["up-a"])],
        );
        let be = r
            .registry()
            .get(&BackendId::parse("up-a").expect("id"))
            .expect("backend");
        let permits_before = be.sem.available_permits();
        let mut rx = r.events.subscribe();
        let app = proxy_router(r);

        let handle =
            tokio::spawn(async move { call(&app, post_chat(r#"{"model":"auto"}"#)).await });
        tokio::time::sleep(Duration::from_millis(150)).await;
        handle.abort();
        let _ = handle.await;

        let mut finished = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let Event::RequestFinished { record } = ev {
                finished.push(record);
            }
        }
        assert_eq!(
            finished.len(),
            1,
            "a cancelled request still gets one record"
        );
        assert!(finished[0].aborted);
        assert_eq!(finished[0].status, 499, "nginx's client-closed-request");
        assert_eq!(
            finished[0].backend.as_ref().map(BackendId::as_str),
            Some("up-a"),
            "the record names the backend that was working on it"
        );
        assert_eq!(be.inflight.load(Ordering::Acquire), 0);
        assert_eq!(be.sem.available_permits(), permits_before);
    }

    #[tokio::test]
    async fn a_stream_with_no_reported_usage_degrades_to_estimated_never_to_nothing() {
        // §4.4: "when the provider emits nothing the record degrades to
        // `TokenCount::Estimated`". llama.cpp without `stream_options.include_usage` is
        // exactly that case, and it is the common one.
        let mut s = String::new();
        for tok in ["Hel", "lo", "!"] {
            s.push_str(&format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{tok}\"}}}}]}}\n\n"
            ));
        }
        s.push_str("data: [DONE]\n\n");
        let base = sse_upstream(vec![s.into_bytes()], Duration::from_millis(0)).await;
        let r = router_with(
            vec![backend("up-a", &base)],
            vec![route("auto", vec!["up-a"])],
        );
        let mut rx = r.events.subscribe();
        let app = proxy_router(r);
        let resp = call(&app, post_chat(r#"{"model":"auto","stream":true}"#)).await;
        let _ = body_to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .expect("body");

        let mut record = None;
        while let Ok(ev) = rx.try_recv() {
            if let Event::RequestFinished { record: rec } = ev {
                record = Some(rec);
            }
        }
        let rec = record.expect("a RequestFinished");
        assert!(rec.prompt_tokens.is_none(), "a prompt cannot be estimated");
        assert!(
            matches!(rec.completion_tokens, Some(TokenCount::Estimated(n)) if n > 0),
            "got {:?} — a stream with no usage degrades, it does not report a silent zero",
            rec.completion_tokens
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

    // ---- the warm queue, ARCHITECTURE.md §4.7 ---------------------------------------------------

    /// The drain a sequential swap performs, without a swap: `accepting = false` is exactly
    /// what makes `resolve()` answer `NoHealthy` and the old code answer `503`.
    fn drain(r: &Router, id: &str) {
        r.registry()
            .get(&BackendId::parse(id).expect("id"))
            .expect("live")
            .accepting
            .store(false, Ordering::Release);
    }

    /// The re-arm a sequential swap performs when B is up and the alias is re-pointed.
    fn rearm(r: &Router, id: &str) {
        r.registry()
            .get(&BackendId::parse(id).expect("id"))
            .expect("live")
            .accepting
            .store(true, Ordering::Release);
    }

    #[tokio::test]
    async fn a_request_that_arrives_mid_swap_parks_instead_of_answering_503() {
        // THE defect, at the level of the request path: with the alias's only backend drained
        // and no warm window, this request is a `503`. With one, it waits and is served.
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
        let auto = Alias::parse("auto").expect("alias");

        // Without a window: the measured failure.
        drain(&r, "up-a");
        let bare = call(&proxy_router(r.clone()), post_chat(r#"{"model":"auto"}"#)).await;
        assert_eq!(
            bare.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "the pre-fix behaviour, so the fix below is measured against something"
        );

        // With one: the swap opens it before it drains, and closes it once the alias can
        // serve again.
        let window = r.warm().open(&auto, Duration::from_secs(10), 32);
        let swap = tokio::spawn({
            let r = r.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(120)).await;
                rearm(&r, "up-a");
                window.close()
            }
        });

        let resp = call(&proxy_router(r.clone()), post_chat(r#"{"model":"auto"}"#)).await;
        let peak = swap.await.expect("swap task");

        assert_eq!(resp.status(), StatusCode::OK, "parked, then served");
        assert_eq!(header(&resp, "x-apexrouter-backend"), Some("up-a"));
        let warm = header(&resp, "x-apexrouter-warm").expect("the park is observable");
        assert!(warm.starts_with("parked=1,waited_ms="), "{warm}");
        assert_eq!(peak, 1, "and it is the number SwapReport::parked carries");
    }

    #[tokio::test]
    async fn a_full_warm_queue_answers_503_with_a_retry_after() {
        let r = router_with(
            vec![backend("up-a", "http://127.0.0.1:1")],
            vec![route("auto", vec!["up-a"])],
        );
        let auto = Alias::parse("auto").expect("alias");
        drain(&r, "up-a");
        let window = r.warm().open(&auto, Duration::from_secs(10), 1);

        // One request occupies the whole one-deep queue.
        let app = proxy_router(r.clone());
        let held = tokio::spawn({
            let app = app.clone();
            async move { call(&app, post_chat(r#"{"model":"auto"}"#)).await.status() }
        });
        let slot = r.warm().parking_for(&auto).expect("open");
        while slot.parked() < 1 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        // The next one overflows, and is refused at once rather than deepening the queue.
        let resp = call(&app, post_chat(r#"{"model":"auto"}"#)).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let retry_after = header(&resp, "retry-after").expect("Retry-After is not optional");
        assert!(
            retry_after.parse::<u32>().map(|n| n >= 1).unwrap_or(false),
            "Retry-After must be a usable number of seconds: {retry_after}"
        );
        // Alias is known; reason is the rule that put us on `auto` (legacy_model_name or
        // alias depending on the model string — here `model: auto` is rule 1).
        assert_eq!(header(&resp, "x-apexrouter-route"), Some("auto|alias"));
        let body = body_to_bytes(resp.into_body(), 64 * 1024)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("OpenAI-shaped");
        assert_eq!(json["error"]["code"], "warm_queue_full", "{json}");

        window.close();
        let _ = held.await;
    }

    #[tokio::test]
    async fn a_warm_window_that_expires_answers_503_with_a_retry_after() {
        let r = router_with(
            vec![backend("up-a", "http://127.0.0.1:1")],
            vec![route("auto", vec!["up-a"])],
        );
        let auto = Alias::parse("auto").expect("alias");
        drain(&r, "up-a");
        // A window narrower than the launch it was opened for: §4.7's "arithmetic guarantee
        // of failure", which the caller's `warm_timeout` exists to prevent and which the
        // request path still has to answer honestly when it happens.
        let _window = r.warm().open(&auto, Duration::from_millis(80), 32);

        let resp = call(&proxy_router(r.clone()), post_chat(r#"{"model":"auto"}"#)).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(header(&resp, "retry-after").is_some());
        let body = body_to_bytes(resp.into_body(), 64 * 1024)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("OpenAI-shaped");
        assert_eq!(json["error"]["code"], "warm_timeout", "{json}");
    }

    #[tokio::test]
    async fn a_client_that_gives_up_while_parked_still_produces_an_aborted_record() {
        // `ARCHITECTURE.md` §4.3 is unconditional, and a parked request holds no
        // `InFlightGuard` to carry it — so without `ParkedAbort` this is the one abandonment
        // in the product that disappears.
        let r = router_with(
            vec![backend("up-a", "http://127.0.0.1:1")],
            vec![route("auto", vec!["up-a"])],
        );
        let auto = Alias::parse("auto").expect("alias");
        drain(&r, "up-a");
        let _window = r.warm().open(&auto, Duration::from_secs(10), 32);
        let mut rx = r.events.subscribe();

        let app = proxy_router(r.clone());
        let client =
            tokio::spawn(async move { call(&app, post_chat(r#"{"model":"auto"}"#)).await });
        let slot = r.warm().parking_for(&auto).expect("open");
        while slot.parked() < 1 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        // Ctrl-C: the handler future is dropped where it stands.
        client.abort();
        let _ = client.await;
        while slot.parked() > 0 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        let mut aborted = None;
        while let Ok(ev) = rx.try_recv() {
            if let Event::RequestFinished { record } = ev {
                aborted = Some(record);
            }
        }
        let rec = aborted.expect("a parked client that vanished must still land in the ring");
        assert!(rec.aborted, "{rec:?}");
        assert_eq!(rec.status, 499);
        assert_eq!(rec.alias.as_ref().map(Alias::as_str), Some("auto"));
        assert_eq!(rec.backend, None, "it never reached one");
        assert!(rec.total_ms > 0, "the wait is recorded, not zeroed");
        assert_eq!(slot.parked(), 0, "and its place in the queue came back");
    }

    #[tokio::test]
    async fn a_request_parks_at_most_once() {
        // A swap that failed, and a second swap that starts straight after it, must not hold
        // one client across both `warm_timeout`s and then answer `503` anyway.
        let r = router_with(
            vec![backend("up-a", "http://127.0.0.1:1")],
            vec![route("auto", vec!["up-a"])],
        );
        let auto = Alias::parse("auto").expect("alias");
        drain(&r, "up-a");
        let first = r.warm().open(&auto, Duration::from_secs(10), 32);

        let app = proxy_router(r.clone());
        let pending = tokio::spawn({
            let app = app.clone();
            async move { call(&app, post_chat(r#"{"model":"auto"}"#)).await.status() }
        });
        let slot = r.warm().parking_for(&auto).expect("open");
        while slot.parked() < 1 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        // The rollback that could not restart A: the window closes with the backend still
        // drained, and a second swap opens its own window in the same breath. A request
        // willing to park twice would vanish into this one.
        first.close();
        let _second = r.warm().open(&auto, Duration::from_secs(10), 32);

        let status = pending.await.expect("request task");
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "one park, then the honest failure"
        );
        assert_eq!(slot.parked(), 0, "and it is not still sitting in the queue");
    }
}
