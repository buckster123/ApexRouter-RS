//! OWNER: unit R-04 (router/src/{attempt,breaker,limits}.rs). Do not edit outside that unit.
//!
//! The attempt state machine.
//!
//! **The retry loop consumes `PreFlight` values and can only exit by producing a
//! `Committed`.** "Never retry after the first byte" is unrepresentable, not merely
//! documented — there is no code path that calls [`attempt`] twice on the same `PreFlight`,
//! and ownership is what enforces it. `PreFlight` is not `Clone`, `attempt` takes it by
//! value, and the `InFlightGuard` it carries moves into the `Committed`.
//!
//! What one call does, in order (`ARCHITECTURE.md` §4.3):
//!
//! 1. refuse immediately if the wall-clock deadline has already passed;
//! 2. send, bounded by `headers_timeout_ms` **and** the remaining deadline, whichever is
//!    shorter — the connect timeout lives on the shared `reqwest::Client`;
//! 3. classify: connect/DNS/TLS and a pre-header timeout are [`Retryable`] and **trip** the
//!    breaker (a refused port is evidence, not a blip); `429`, `502`, `503`, `504` and `529`
//!    are [`Retryable::Status`]; **every other status is terminal and relays verbatim**;
//! 4. on a response worth relaying, stamp TTFT, arm the guard and hand back a [`Committed`].
//!
//! The breaker is recorded here, once per attempt, so no caller has to remember to. The
//! retry *budget* ([`crate::limits::TokenBucket`]) is the caller's: only it knows whether
//! this is a first try or a retry.

use crate::breaker::Breaker;
use crate::limits::InFlightGuard;
use crate::relay::body::BodyPlan;
use crate::resolve::Candidate;
use apexrouter_core::config::RouterCfg;
use apexrouter_protocol::RetryPolicy;
use bytes::Bytes;
use reqwest::header::{HeaderMap, RETRY_AFTER};
use reqwest::{Client, Method, Response};
use std::time::{Duration, Instant};

/// Everything one upstream attempt needs. Consumed by [`attempt`].
///
/// Built by the handler (R-08) once per candidate: a retry builds a **new** `PreFlight`
/// against the next candidate, which is why the type is neither `Clone` nor reusable.
pub struct PreFlight<'a> {
    /// The target, carrying the live backend and the model id to send upstream.
    pub candidate: &'a Candidate,
    /// The one pooled client. Holds `connect_timeout` and the no-compression settings.
    pub http: &'a Client,
    /// The inbound method, unchanged.
    pub method: Method,
    /// The absolute upstream URL, already joined against `Backend::base_url`.
    pub url: String,
    /// Headers **constructed** from the allowlist by `relay::headers::outbound_headers` —
    /// never the inbound map.
    pub headers: HeaderMap,
    /// Passthrough bytes, or bytes whose `model` value was rewritten.
    pub body: BodyPlan,
    /// The wall-clock deadline for the whole request, across every attempt.
    pub deadline: Instant,
    /// Timeouts and budgets.
    pub cfg: &'a RouterCfg,
    /// The **route's own** `[retry]` block, carried here by `Plan::retry` (R-02). `attempts`
    /// and `failover` bound the caller's loop; `honor_retry_after` is consumed here, because
    /// this is where an upstream `Retry-After` is read.
    pub retry: RetryPolicy,
    /// The permit, the byte budget, the gauge and the partial record. Moves into the
    /// [`Committed`] on success; released when this attempt fails.
    pub guard: InFlightGuard,
}

/// An upstream response whose first byte has arrived. **Past this point there is no retry.**
///
/// Owns the `InFlightGuard`, so dropping it releases the permit, decrements the gauge and —
/// if `finish()` never ran — emits `RequestFinished { aborted: true }`.
pub struct Committed {
    /// The upstream response, headers read, body not yet consumed.
    pub response: Response,
    /// The guard, still armed. `take()` it to hand to the relay and call `finish()` when
    /// the last byte lands; drop it and the client is recorded as having gone away.
    pub guard: Option<InFlightGuard>,
}

impl std::fmt::Debug for Committed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Committed")
            .field("status", &self.response.status().as_u16())
            .field("guard_held", &self.guard.is_some())
            .finish()
    }
}

/// A failure that may be retried, on this or a different target.
#[derive(Debug)]
pub enum Retryable {
    /// Connect, DNS or TLS failure. Trips the breaker.
    Connect(String),
    /// Timed out before headers.
    Timeout,
    /// An upstream status worth another try: 429 (on a **different** target), 502, 503, 504,
    /// 529. Any other status is terminal and relayed verbatim.
    Status {
        /// The status.
        code: u16,
        /// A parsed `Retry-After`, when the upstream sent one.
        retry_after: Option<Duration>,
    },
}

/// Is this status worth another target?
///
/// `429` is retryable only on a **different** backend, which is the caller's decision; this
/// says nothing about which candidate comes next.
fn is_retryable_status(code: u16) -> bool {
    matches!(code, 429 | 502 | 503 | 504 | 529)
}

/// Parse `Retry-After`: delta-seconds, or an HTTP-date, or nothing.
///
/// A date in the past yields `Duration::ZERO` rather than an error — an upstream saying
/// "you may retry now" is not a parse failure.
fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let raw = headers.get(RETRY_AFTER)?.to_str().ok()?.trim().to_owned();
    if let Ok(secs) = raw.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let when = chrono::DateTime::parse_from_rfc2822(&raw).ok()?;
    let delta = when.timestamp_millis() - chrono::Utc::now().timestamp_millis();
    Some(Duration::from_millis(delta.max(0) as u64))
}

/// One upstream attempt.
pub async fn attempt(p: PreFlight<'_>) -> std::result::Result<Committed, Retryable> {
    let PreFlight {
        candidate,
        http,
        method,
        url,
        headers,
        body,
        deadline,
        cfg,
        retry,
        mut guard,
    } = p;
    let breaker = &candidate.backend.breaker;

    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        // The deadline is a property of the request, not of this backend: the breaker hears
        // nothing about it.
        return Err(Retryable::Timeout);
    }
    let cap = Duration::from_millis(cfg.headers_timeout_ms).min(remaining);

    let url = match reqwest::Url::parse(&url) {
        Ok(u) => u,
        Err(e) => {
            return Err(fail(
                breaker,
                Retryable::Connect(format!("bad url {url}: {e}")),
            ))
        }
    };
    let bytes: Bytes = match body {
        BodyPlan::Passthrough(b) | BodyPlan::Rewritten(b) => b,
    };
    let request = http
        .request(method, url)
        .headers(headers)
        .body(bytes)
        .build();
    let request = match request {
        Ok(r) => r,
        Err(e) => return Err(fail(breaker, Retryable::Connect(e.to_string()))),
    };

    let response = match tokio::time::timeout(cap, http.execute(request)).await {
        // No headers before the cap: retryable, and the breaker hears about it.
        Err(_elapsed) => return Err(fail(breaker, Retryable::Timeout)),
        Ok(Err(e)) => {
            let r = if e.is_timeout() {
                Retryable::Timeout
            } else {
                Retryable::Connect(e.to_string())
            };
            return Err(fail(breaker, r));
        }
        Ok(Ok(r)) => r,
    };

    let code = response.status().as_u16();
    if is_retryable_status(code) {
        // `honor_retry_after = false` means the route does not want an upstream dictating
        // when we come back, so the header is not even read.
        let retry_after = if retry.honor_retry_after {
            parse_retry_after(response.headers())
        } else {
            None
        };
        breaker.record(false);
        // An upstream that says when to come back is telling us it is saturated, not
        // flaky. Honour it exactly, without waiting for a quorum.
        if let Some(d) = retry_after {
            breaker.trip(Some(d));
        }
        return Err(Retryable::Status { code, retry_after });
    }

    // Any other status is the upstream answering, which is a working upstream even when the
    // answer is a 400.
    breaker.record(true);
    guard.mark_first_byte();
    guard.arm();
    Ok(Committed {
        response,
        guard: Some(guard),
    })
}

/// Record a hard failure and hand the error straight back.
///
/// Connect/DNS/TLS failures and pre-header timeouts trip the breaker outright
/// (`ARCHITECTURE.md` §4.3); a dead port is conclusive, so it does not wait for the
/// `min_volume` quorum that `record()` enforces for statistical failures.
fn fail(breaker: &Breaker, r: Retryable) -> Retryable {
    breaker.record(false);
    breaker.trip(None);
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::breaker::BreakerDecision;
    use crate::limits::{test_backend, test_record};
    use apexrouter_protocol::Event;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use tokio::sync::{broadcast, Semaphore};
    use wiremock::matchers::{method as m, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A guard on a one-permit backend, wired to `tx`.
    async fn guard_for(
        b: &Arc<crate::registry::LiveBackend>,
        tx: &broadcast::Sender<Event>,
    ) -> InFlightGuard {
        let global = Arc::new(Semaphore::new(4096));
        let mut g = InFlightGuard::acquire(b, 16, &global, Duration::from_secs(1))
            .await
            .expect("admitted");
        g.events = Some(tx.clone());
        g.record = Some(test_record());
        g
    }

    fn preflight<'a>(
        candidate: &'a Candidate,
        http: &'a Client,
        cfg: &'a RouterCfg,
        url: String,
        guard: InFlightGuard,
    ) -> PreFlight<'a> {
        PreFlight {
            candidate,
            http,
            method: Method::POST,
            url,
            headers: HeaderMap::new(),
            body: BodyPlan::Passthrough(Bytes::from_static(b"{\"model\":\"x\"}")),
            deadline: Instant::now() + Duration::from_secs(30),
            cfg,
            retry: RetryPolicy::default(),
            guard,
        }
    }

    fn candidate_for(b: &Arc<crate::registry::LiveBackend>) -> Candidate {
        Candidate {
            backend: Arc::clone(b),
            upstream_model: "carnice-9b".to_owned(),
        }
    }

    /// **The disconnect-leak regression test.** Dropping a `Committed` mid-stream must
    /// return every permit and broadcast exactly one `RequestFinished { aborted: true }`.
    #[tokio::test]
    async fn dropping_a_committed_mid_stream_releases_the_permit_and_aborts_once() {
        let server = MockServer::start().await;
        Mock::given(m("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string("data: {\"choices\":[]}\n\n"),
            )
            .mount(&server)
            .await;

        let b = test_backend("live", 1);
        let cand = candidate_for(&b);
        let http = Client::new();
        let cfg = RouterCfg::default();
        let (tx, mut rx) = broadcast::channel(8);

        let guard = guard_for(&b, &tx).await;
        assert_eq!(b.sem.available_permits(), 0, "the permit is held");

        let committed = attempt(preflight(
            &cand,
            &http,
            &cfg,
            format!("{}/v1/chat/completions", server.uri()),
            guard,
        ))
        .await
        .expect("200 commits");

        // The client hangs up: the whole Committed, response body and all, is dropped.
        drop(committed);

        assert_eq!(b.sem.available_permits(), 1, "the permit came back");
        assert_eq!(b.inflight.load(Ordering::Acquire), 0, "the gauge came back");
        match rx.try_recv().expect("one RequestFinished") {
            Event::RequestFinished { record } => {
                assert!(record.aborted, "the client vanished, so it aborted");
                assert!(record.ttft_ms.is_some(), "TTFT was stamped at commit");
            }
            other => panic!("unexpected event {other:?}"),
        }
        assert!(
            rx.try_recv().is_err(),
            "exactly one RequestFinished, never two"
        );
    }

    #[tokio::test]
    async fn a_committed_request_that_finishes_is_not_aborted() {
        let server = MockServer::start().await;
        Mock::given(m("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;

        let b = test_backend("live", 1);
        let cand = candidate_for(&b);
        let http = Client::new();
        let cfg = RouterCfg::default();
        let (tx, mut rx) = broadcast::channel(8);

        let guard = guard_for(&b, &tx).await;
        let mut committed = attempt(preflight(
            &cand,
            &http,
            &cfg,
            format!("{}/v1/chat/completions", server.uri()),
            guard,
        ))
        .await
        .expect("200 commits");
        let g = committed.guard.take().expect("the guard is there");
        drop(committed);
        g.finish(test_record());

        match rx.try_recv().expect("one RequestFinished") {
            Event::RequestFinished { record } => assert!(!record.aborted),
            other => panic!("unexpected event {other:?}"),
        }
        assert!(rx.try_recv().is_err());
        assert_eq!(b.sem.available_permits(), 1);
        assert_eq!(b.breaker.check(), BreakerDecision::Allow);
    }

    #[tokio::test]
    async fn a_terminal_status_is_relayed_rather_than_retried() {
        let server = MockServer::start().await;
        Mock::given(m("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_string("{\"error\":{}}"))
            .mount(&server)
            .await;

        let b = test_backend("live", 1);
        let cand = candidate_for(&b);
        let http = Client::new();
        let cfg = RouterCfg::default();
        let (tx, _rx) = broadcast::channel(8);

        let guard = guard_for(&b, &tx).await;
        let committed = attempt(preflight(
            &cand,
            &http,
            &cfg,
            format!("{}/v1/chat/completions", server.uri()),
            guard,
        ))
        .await
        .expect("a 400 is an answer, not a failure");
        assert_eq!(committed.response.status().as_u16(), 400);
        assert_eq!(
            b.breaker.check(),
            BreakerDecision::Allow,
            "a 400 is the upstream working"
        );
    }

    #[tokio::test]
    async fn a_502_is_retryable_and_leaks_nothing() {
        let server = MockServer::start().await;
        Mock::given(m("POST"))
            .respond_with(ResponseTemplate::new(502))
            .mount(&server)
            .await;

        let b = test_backend("live", 1);
        let cand = candidate_for(&b);
        let http = Client::new();
        let cfg = RouterCfg::default();
        let (tx, mut rx) = broadcast::channel(8);

        let guard = guard_for(&b, &tx).await;
        let err = attempt(preflight(
            &cand,
            &http,
            &cfg,
            format!("{}/v1/chat/completions", server.uri()),
            guard,
        ))
        .await
        .expect_err("502 is retryable");
        match err {
            Retryable::Status { code, retry_after } => {
                assert_eq!(code, 502);
                assert!(retry_after.is_none());
            }
            other => panic!("expected Status, got {other:?}"),
        }
        assert_eq!(b.sem.available_permits(), 1, "the failed attempt released");
        assert!(
            rx.try_recv().is_err(),
            "a retryable failure is not a finished request"
        );
        assert_eq!(
            b.breaker.check(),
            BreakerDecision::Allow,
            "one 502 is below min_volume"
        );
    }

    #[tokio::test]
    async fn retry_after_is_parsed_and_opens_the_breaker_for_exactly_that_long() {
        let server = MockServer::start().await;
        Mock::given(m("POST"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "120"))
            .mount(&server)
            .await;

        let b = test_backend("live", 1);
        let cand = candidate_for(&b);
        let http = Client::new();
        let cfg = RouterCfg::default();
        let (tx, _rx) = broadcast::channel(8);

        let guard = guard_for(&b, &tx).await;
        let err = attempt(preflight(
            &cand,
            &http,
            &cfg,
            format!("{}/v1/chat/completions", server.uri()),
            guard,
        ))
        .await
        .expect_err("429 is retryable on a different target");
        match err {
            Retryable::Status { code, retry_after } => {
                assert_eq!(code, 429);
                assert_eq!(retry_after, Some(Duration::from_secs(120)));
            }
            other => panic!("expected Status, got {other:?}"),
        }
        match b.breaker.check() {
            BreakerDecision::Deny { retry_at_unix } => {
                let now = chrono::Utc::now().timestamp();
                assert!(retry_at_unix >= now + 118 && retry_at_unix <= now + 121);
            }
            other => panic!("Retry-After must open the breaker, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn honor_retry_after_false_leaves_the_breaker_cool_down_alone() {
        // The third knob in a route's `[retry]` block. With it off, the upstream does not
        // get to dictate a two-minute cool-down on a backend the operator pinned.
        let server = MockServer::start().await;
        Mock::given(m("POST"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "120"))
            .mount(&server)
            .await;

        let b = test_backend("live", 1);
        let cand = candidate_for(&b);
        let http = Client::new();
        let cfg = RouterCfg::default();
        let (tx, _rx) = broadcast::channel(8);

        let guard = guard_for(&b, &tx).await;
        let mut p = preflight(
            &cand,
            &http,
            &cfg,
            format!("{}/v1/chat/completions", server.uri()),
            guard,
        );
        p.retry = RetryPolicy {
            honor_retry_after: false,
            ..RetryPolicy::default()
        };
        let err = attempt(p).await.expect_err("429 is still retryable");
        match err {
            Retryable::Status { code, retry_after } => {
                assert_eq!(code, 429);
                assert_eq!(retry_after, None, "the header was not to be read");
            }
            other => panic!("expected Status, got {other:?}"),
        }
        assert_eq!(
            b.breaker.check(),
            BreakerDecision::Allow,
            "one 429 is below min_volume, and no Retry-After was honoured"
        );
    }

    #[tokio::test]
    async fn a_refused_connection_trips_the_breaker_immediately() {
        // Bind and drop, so the port is certainly nobody's.
        let dead = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = dead.local_addr().expect("addr").port();
        drop(dead);

        let b = test_backend("live", 1);
        let cand = candidate_for(&b);
        let http = Client::new();
        let cfg = RouterCfg::default();
        let (tx, _rx) = broadcast::channel(8);

        let guard = guard_for(&b, &tx).await;
        let err = attempt(preflight(
            &cand,
            &http,
            &cfg,
            format!("http://127.0.0.1:{port}/v1/chat/completions"),
            guard,
        ))
        .await
        .expect_err("nothing is listening");
        assert!(matches!(err, Retryable::Connect(_)), "got {err:?}");
        assert!(
            matches!(b.breaker.check(), BreakerDecision::Deny { .. }),
            "a refused port is conclusive, not a blip"
        );
        assert_eq!(b.sem.available_permits(), 1);
    }

    #[tokio::test]
    async fn an_expired_deadline_never_reaches_the_network() {
        let b = test_backend("live", 1);
        let cand = candidate_for(&b);
        let http = Client::new();
        let cfg = RouterCfg::default();
        let (tx, _rx) = broadcast::channel(8);

        let guard = guard_for(&b, &tx).await;
        let mut p = preflight(
            &cand,
            &http,
            &cfg,
            "http://127.0.0.1:1/v1/chat/completions".to_owned(),
            guard,
        );
        p.deadline = Instant::now() - Duration::from_millis(1);
        let err = attempt(p).await.expect_err("the deadline passed");
        assert!(matches!(err, Retryable::Timeout), "got {err:?}");
        assert_eq!(
            b.breaker.check(),
            BreakerDecision::Allow,
            "a client deadline says nothing about the backend"
        );
    }

    #[tokio::test]
    async fn a_pre_header_timeout_is_retryable() {
        let server = MockServer::start().await;
        Mock::given(m("POST"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(500)))
            .mount(&server)
            .await;

        let b = test_backend("live", 1);
        let cand = candidate_for(&b);
        let http = Client::new();
        let cfg = RouterCfg {
            headers_timeout_ms: 50,
            ..RouterCfg::default()
        };
        let (tx, _rx) = broadcast::channel(8);

        let guard = guard_for(&b, &tx).await;
        let err = attempt(preflight(
            &cand,
            &http,
            &cfg,
            format!("{}/v1/chat/completions", server.uri()),
            guard,
        ))
        .await
        .expect_err("no headers within 50 ms");
        assert!(matches!(err, Retryable::Timeout), "got {err:?}");
        assert_eq!(b.sem.available_permits(), 1);
    }

    #[test]
    fn retry_after_accepts_an_http_date() {
        let mut h = HeaderMap::new();
        let when = chrono::Utc::now() + chrono::Duration::seconds(60);
        h.insert(
            RETRY_AFTER,
            when.to_rfc2822().parse().expect("valid header value"),
        );
        let d = parse_retry_after(&h).expect("parsed");
        assert!(d.as_secs() >= 55 && d.as_secs() <= 61, "got {d:?}");
    }

    #[test]
    fn a_retry_after_in_the_past_is_zero_not_an_error() {
        let mut h = HeaderMap::new();
        let when = chrono::Utc::now() - chrono::Duration::seconds(60);
        h.insert(
            RETRY_AFTER,
            when.to_rfc2822().parse().expect("valid header value"),
        );
        assert_eq!(parse_retry_after(&h), Some(Duration::ZERO));
    }

    #[test]
    fn only_the_documented_statuses_are_retryable() {
        for code in [429, 502, 503, 504, 529] {
            assert!(is_retryable_status(code), "{code} should be retryable");
        }
        for code in [
            200, 201, 204, 301, 400, 401, 403, 404, 409, 413, 422, 500, 501,
        ] {
            assert!(!is_retryable_status(code), "{code} must relay verbatim");
        }
    }
}
