//! OWNER: unit R-04 (router/src/{attempt,breaker,limits}.rs). Do not edit outside that unit.
//!
//! Concurrency, the global byte budget, and the RAII guard that makes a client disconnect
//! harmless.
//!
//! A count cap alone permits 64 × 32 MiB of resident bodies, so there is a **global
//! `max_inflight_bytes`** budget as well as a count. The retry budget is a **per-backend
//! token bucket**, so a struggling backend cannot be amplified into a storm.
//!
//! # How the guard is wired (for R-08)
//!
//! ```text
//! let mut g = InFlightGuard::acquire(&backend, body.len(), &inflight_bytes, qt).await?;
//! g.events = Some(tx.clone());          // where RequestFinished goes
//! g.record = Some(partial_record);      // what Drop broadcasts if the client vanishes
//! // … PreFlight { guard: g, … } -> attempt() -> Committed …
//! // success:  committed.guard.take().finish(final_record)
//! // client disconnect: everything is simply dropped
//! ```
//!
//! A guard is **armed** by [`crate::attempt::attempt`] at the moment it produces a
//! `Committed`. Only an armed guard broadcasts `RequestFinished { aborted: true }` from its
//! `Drop`, so a guard released while the retry loop moves to the next candidate does not
//! invent a finished request — and a committed request that the client abandons emits
//! exactly one.

use crate::breaker::now_unix_ms;
use crate::registry::LiveBackend;
use apexrouter_protocol::{Event, RequestRecord};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Default retry budget, in tokens per minute, matching `RouterCfg::retry_budget_per_min`.
const DEFAULT_RETRY_BUDGET_PER_MIN: u32 = 30;

/// Why a request could not be admitted.
#[derive(Debug, thiserror::Error)]
pub enum LimitError {
    /// No permit became available before `queue_timeout_ms`. Becomes a `503` with a
    /// `Retry-After`.
    #[error("queue timeout after {ms} ms")]
    QueueTimeout {
        /// The deadline that expired.
        ms: u64,
    },
    /// The backend is draining and is no longer accepting.
    #[error("backend is draining")]
    NotAccepting,
    /// The body exceeds `max_body_bytes`, or would exceed the global byte budget. Becomes
    /// a `413`.
    #[error("request body is {bytes} bytes, the limit is {max}")]
    TooLarge {
        /// The body size.
        bytes: usize,
        /// The configured limit.
        max: usize,
    },
    /// The daemon is shutting down.
    #[error("shutting down")]
    ShuttingDown,
}

/// Owns the `OwnedSemaphorePermit`, the byte-budget permit, the in-flight gauge and the
/// partially built `RequestRecord`.
///
/// **Its `Drop` emits `RequestFinished { aborted: true }` if `finish()` was never called**,
/// so a client Ctrl-C cannot leak a permit or leave a zombie row in either GUI.
pub struct InFlightGuard {
    /// Whose gauge to decrement, and whose permit pool this came from.
    backend: Arc<LiveBackend>,
    /// The per-backend concurrency permit. Released on drop.
    _permit: OwnedSemaphorePermit,
    /// The slice of the global `max_inflight_bytes` budget this body holds.
    _bytes: OwnedSemaphorePermit,
    /// When the request was admitted, for `total_ms`.
    started: Instant,
    /// When the first upstream byte arrived, for `ttft_ms`.
    first_byte: Option<Instant>,
    /// True once an attempt committed. Only an armed guard can broadcast an abort.
    armed: bool,
    /// True once [`InFlightGuard::finish`] ran; suppresses the abort in `Drop`.
    finished: bool,
    /// Where `RequestFinished` goes. `None` in tests and on surfaces with no bus.
    pub events: Option<broadcast::Sender<Event>>,
    /// The record as far as it is known. `Drop` completes it and marks it aborted.
    pub record: Option<RequestRecord>,
}

impl std::fmt::Debug for InFlightGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InFlightGuard")
            .field("backend", &self.backend.id)
            .field("armed", &self.armed)
            .field("finished", &self.finished)
            .field("first_byte", &self.first_byte.is_some())
            .finish()
    }
}

impl InFlightGuard {
    /// Acquire everything, or fail with the reason.
    ///
    /// Takes the **global byte budget first**, then the per-backend permit, both inside one
    /// `queue_timeout`: a body that cannot fit in memory must not also sit on a backend
    /// slot while it waits.
    pub async fn acquire(
        b: &Arc<LiveBackend>,
        bytes: usize,
        global: &Arc<tokio::sync::Semaphore>,
        queue_timeout: Duration,
    ) -> Result<InFlightGuard, LimitError> {
        if !b.accepting.load(Ordering::Acquire) {
            return Err(LimitError::NotAccepting);
        }
        // The byte budget is a Semaphore, so one body can never claim more than a
        // Semaphore can express. Anything larger is a 413 by construction.
        let want = u32::try_from(bytes).map_err(|_| LimitError::TooLarge {
            bytes,
            max: u32::MAX as usize,
        })?;

        let sem = Arc::clone(&b.sem);
        let global = Arc::clone(global);
        let acquired = tokio::time::timeout(queue_timeout, async move {
            let bytes_permit = global.acquire_many_owned(want).await?;
            let permit = sem.acquire_owned().await?;
            Ok::<_, tokio::sync::AcquireError>((bytes_permit, permit))
        })
        .await;

        let (bytes_permit, permit) = match acquired {
            Err(_elapsed) => {
                return Err(LimitError::QueueTimeout {
                    ms: queue_timeout.as_millis() as u64,
                })
            }
            Ok(Err(_closed)) => return Err(LimitError::ShuttingDown),
            Ok(Ok(pair)) => pair,
        };

        // Recheck: a drain may have started while we queued.
        if !b.accepting.load(Ordering::Acquire) {
            return Err(LimitError::NotAccepting);
        }
        b.inflight.fetch_add(1, Ordering::AcqRel);
        Ok(InFlightGuard {
            backend: Arc::clone(b),
            _permit: permit,
            _bytes: bytes_permit,
            started: Instant::now(),
            first_byte: None,
            armed: false,
            finished: false,
            events: None,
            record: None,
        })
    }

    /// Stamp TTFT. Called exactly once, at the first upstream byte.
    pub fn mark_first_byte(&mut self) {
        if self.first_byte.is_none() {
            self.first_byte = Some(Instant::now());
        }
    }

    /// Complete the record and release everything. Consuming `self` is what makes the
    /// aborted-vs-finished distinction unforgeable.
    pub fn finish(mut self, rec: RequestRecord) {
        self.finished = true;
        self.record = None;
        self.broadcast(rec);
        // `self` drops here: permits released, gauge decremented, no abort.
    }

    /// Mark the request as committed to an upstream. From here a drop means the client
    /// went away, which is exactly what `Drop` reports.
    pub(crate) fn arm(&mut self) {
        self.armed = true;
    }

    /// Milliseconds since the request was admitted.
    fn elapsed_ms(&self) -> u32 {
        u32::try_from(self.started.elapsed().as_millis()).unwrap_or(u32::MAX)
    }

    /// Milliseconds from admission to the first upstream byte, when there was one.
    fn ttft_ms(&self) -> Option<u32> {
        self.first_byte
            .map(|t| u32::try_from(t.duration_since(self.started).as_millis()).unwrap_or(u32::MAX))
    }

    /// Send one `RequestFinished`, but only when somebody is listening — serialising a
    /// record nobody reads is pure cost on the request path.
    fn broadcast(&self, rec: RequestRecord) {
        if let Some(tx) = &self.events {
            if tx.receiver_count() > 0 {
                let _ = tx.send(Event::RequestFinished {
                    record: Box::new(rec),
                });
            }
        }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        // The gauge is ours whatever happened. Saturating, so a double decrement can never
        // wrap a u32 into four billion in-flight requests.
        let _ = self
            .backend
            .inflight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                Some(n.saturating_sub(1))
            });

        if self.finished || !self.armed {
            return;
        }
        let Some(mut rec) = self.record.take() else {
            return;
        };
        rec.aborted = true;
        rec.total_ms = self.elapsed_ms();
        if rec.ttft_ms.is_none() {
            rec.ttft_ms = self.ttft_ms();
        }
        self.broadcast(rec);
        // Permits are released by their own Drop, after this returns.
    }
}

/// A per-backend retry budget.
///
/// A leaky bucket in milli-tokens: `per_min` tokens accrue every 60 s, capped at one
/// minute's worth. A backend that is failing therefore costs at most `per_min` extra
/// requests per minute, however hard clients hammer it.
#[derive(Debug)]
pub struct TokenBucket {
    /// Tokens available, ×1000, so refill is exact at millisecond resolution.
    milli_tokens: AtomicI64,
    /// Unix ms of the last refill.
    last_ms: AtomicI64,
    /// Refill rate, and also the burst capacity.
    per_min: AtomicI64,
}

impl Default for TokenBucket {
    fn default() -> Self {
        TokenBucket::new(DEFAULT_RETRY_BUDGET_PER_MIN)
    }
}

impl TokenBucket {
    /// A full bucket refilling at `per_min` tokens a minute.
    pub fn new(per_min: u32) -> TokenBucket {
        let per_min = i64::from(per_min);
        TokenBucket {
            milli_tokens: AtomicI64::new(per_min * 1000),
            last_ms: AtomicI64::new(now_unix_ms()),
            per_min: AtomicI64::new(per_min),
        }
    }

    /// Spend one retry, or report that the budget is exhausted.
    pub fn try_take(&self) -> bool {
        let per_min = self.per_min.load(Ordering::Acquire);
        if per_min <= 0 {
            return false;
        }
        let cap = per_min.saturating_mul(1000);
        let now = now_unix_ms();
        let mut current = self.milli_tokens.load(Ordering::Acquire);
        loop {
            let last = self.last_ms.load(Ordering::Acquire);
            let elapsed = (now - last).max(0);
            let refill = elapsed.saturating_mul(per_min) / 60;
            let available = current.saturating_add(refill).min(cap);
            if available < 1000 {
                return false;
            }
            match self.milli_tokens.compare_exchange(
                current,
                available - 1000,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.last_ms.store(now, Ordering::Release);
                    return true;
                }
                Err(observed) => current = observed,
            }
        }
    }
}

/// A `LiveBackend` with `permits` slots, for R-04's tests.
#[cfg(test)]
pub(crate) fn test_backend(id: &str, permits: usize) -> Arc<LiveBackend> {
    use apexrouter_core::config::RouterCfg;
    use apexrouter_protocol::{
        Backend, BackendId, BackendKind, BackendLimits, CredentialSource, Health, Protocol,
        Provenance,
    };

    let bid = BackendId::parse(id).expect("valid test backend id");
    let meta = Backend {
        id: bid,
        kind: BackendKind::Node,
        protocol: Protocol::OpenAi,
        label: id.to_owned(),
        base_url: "http://127.0.0.1:1".to_owned(),
        credential: CredentialSource::None,
        tags: Vec::new(),
        models: Vec::new(),
        limits: BackendLimits {
            max_concurrent: permits as u32,
            queue_depth: 8,
            ctx: None,
            slots_total: None,
        },
        price: None,
        health: Health::Unknown,
        provenance: Provenance::Manual,
        endpoint: None,
        enabled: true,
        devices: Vec::new(),
        last_error: None,
    };
    LiveBackend::new(meta, &RouterCfg::default())
}

/// A `RequestRecord` with nothing interesting in it, for the guard tests.
#[cfg(test)]
pub(crate) fn test_record() -> RequestRecord {
    use apexrouter_protocol::{CostEstimate, Protocol, RequestId, RouteReason};
    RequestRecord {
        id: RequestId::new(),
        started_unix: now_unix_ms() / 1000,
        alias: None,
        backend: None,
        upstream_model: None,
        route_reason: RouteReason::LegacyModelName,
        ingress: Protocol::OpenAi,
        method: "POST".to_owned(),
        path: "/v1/chat/completions".to_owned(),
        status: 200,
        attempts: 1,
        streamed: true,
        aborted: false,
        ttft_ms: None,
        total_ms: 0,
        prompt_tokens: None,
        completion_tokens: None,
        cached_tokens: None,
        tok_per_s: None,
        cost: CostEstimate::Unknown,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn global(bytes: usize) -> Arc<Semaphore> {
        Arc::new(Semaphore::new(bytes))
    }

    #[tokio::test]
    async fn acquire_takes_a_permit_a_byte_slice_and_the_gauge() {
        let b = test_backend("b", 2);
        let g = global(1024);
        let guard = InFlightGuard::acquire(&b, 100, &g, Duration::from_secs(1))
            .await
            .expect("admitted");
        assert_eq!(b.sem.available_permits(), 1);
        assert_eq!(g.available_permits(), 924);
        assert_eq!(b.inflight.load(Ordering::Acquire), 1);
        drop(guard);
        assert_eq!(b.sem.available_permits(), 2);
        assert_eq!(g.available_permits(), 1024);
        assert_eq!(b.inflight.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn a_draining_backend_is_refused() {
        let b = test_backend("b", 1);
        b.accepting.store(false, Ordering::Release);
        let err = InFlightGuard::acquire(&b, 0, &global(64), Duration::from_secs(1))
            .await
            .expect_err("draining");
        assert!(matches!(err, LimitError::NotAccepting));
    }

    #[tokio::test]
    async fn a_saturated_backend_times_out_rather_than_queueing_forever() {
        let b = test_backend("b", 1);
        let g = global(1024);
        let _held = InFlightGuard::acquire(&b, 1, &g, Duration::from_secs(1))
            .await
            .expect("first is admitted");
        let err = InFlightGuard::acquire(&b, 1, &g, Duration::from_millis(50))
            .await
            .expect_err("second must not wait forever");
        match err {
            LimitError::QueueTimeout { ms } => assert_eq!(ms, 50),
            other => panic!("expected QueueTimeout, got {other:?}"),
        }
        // The failed acquisition leaked nothing.
        assert_eq!(g.available_permits(), 1023);
        assert_eq!(b.inflight.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn a_body_larger_than_the_budget_can_express_is_too_large() {
        let b = test_backend("b", 1);
        let huge = u32::MAX as usize + 1;
        let err = InFlightGuard::acquire(&b, huge, &global(1024), Duration::from_secs(1))
            .await
            .expect_err("too large");
        match err {
            LimitError::TooLarge { bytes, .. } => assert_eq!(bytes, huge),
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_closed_budget_reports_shutting_down() {
        let b = test_backend("b", 1);
        let g = global(1024);
        g.close();
        let err = InFlightGuard::acquire(&b, 8, &g, Duration::from_secs(1))
            .await
            .expect_err("closed");
        assert!(matches!(err, LimitError::ShuttingDown));
    }

    #[tokio::test]
    async fn finish_broadcasts_exactly_one_non_aborted_record() {
        let b = test_backend("b", 1);
        let (tx, mut rx) = broadcast::channel(8);
        let mut guard = InFlightGuard::acquire(&b, 8, &global(64), Duration::from_secs(1))
            .await
            .expect("admitted");
        guard.events = Some(tx);
        guard.record = Some(test_record());
        guard.arm();
        guard.finish(test_record());

        match rx.try_recv().expect("one event") {
            Event::RequestFinished { record } => assert!(!record.aborted),
            other => panic!("unexpected event {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "exactly one event");
        assert_eq!(b.sem.available_permits(), 1);
    }

    #[tokio::test]
    async fn an_unarmed_guard_never_invents_a_finished_request() {
        // This is the retry path: the attempt failed, the guard is released, and the loop
        // moves to the next candidate. No request finished.
        let b = test_backend("b", 1);
        let (tx, mut rx) = broadcast::channel(8);
        let mut guard = InFlightGuard::acquire(&b, 8, &global(64), Duration::from_secs(1))
            .await
            .expect("admitted");
        guard.events = Some(tx);
        guard.record = Some(test_record());
        drop(guard);
        assert!(rx.try_recv().is_err());
        assert_eq!(b.inflight.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn mark_first_byte_is_idempotent_and_lands_in_the_aborted_record() {
        let b = test_backend("b", 1);
        let (tx, mut rx) = broadcast::channel(8);
        let mut guard = InFlightGuard::acquire(&b, 8, &global(64), Duration::from_secs(1))
            .await
            .expect("admitted");
        guard.events = Some(tx);
        guard.record = Some(test_record());
        guard.arm();
        guard.mark_first_byte();
        let first = guard.first_byte;
        guard.mark_first_byte();
        assert_eq!(first, guard.first_byte, "TTFT is stamped once");
        drop(guard);
        match rx.try_recv().expect("aborted event") {
            Event::RequestFinished { record } => {
                assert!(record.aborted);
                assert!(record.ttft_ms.is_some());
            }
            other => panic!("unexpected event {other:?}"),
        }
    }

    #[test]
    fn the_retry_bucket_runs_dry_and_refills() {
        let tb = TokenBucket::new(6);
        for i in 0..6 {
            assert!(tb.try_take(), "token {i} should be available");
        }
        assert!(
            !tb.try_take(),
            "a burst of six exhausts a six-per-minute bucket"
        );

        // Ten seconds of accrual at 6/min is one token, and no more than one.
        tb.last_ms.store(now_unix_ms() - 10_000, Ordering::Release);
        assert!(tb.try_take());
        assert!(!tb.try_take());
    }

    #[test]
    fn a_zero_budget_never_admits_a_retry() {
        assert!(!TokenBucket::new(0).try_take());
    }
}
