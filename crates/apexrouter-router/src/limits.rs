//! OWNER: unit R-04 (router/src/{attempt,breaker,limits}.rs). Do not edit outside that unit.
//!
//! Concurrency, the global byte budget, and the RAII guard that makes a client disconnect
//! harmless.
//!
//! A count cap alone permits 64 × 32 MiB of resident bodies, so there is a **global
//! `max_inflight_bytes`** budget as well as a count. The retry budget is a **per-backend
//! token bucket**, so a struggling backend cannot be amplified into a storm.

use crate::registry::LiveBackend;
use apexrouter_protocol::RequestRecord;
use std::sync::Arc;
use std::time::Duration;

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
pub struct InFlightGuard {/* R-04: OwnedSemaphorePermit + byte permit + gauge + partial record */}

impl InFlightGuard {
    /// Acquire everything, or fail with the reason.
    pub async fn acquire(
        b: &Arc<LiveBackend>,
        bytes: usize,
        global: &Arc<tokio::sync::Semaphore>,
        queue_timeout: Duration,
    ) -> Result<InFlightGuard, LimitError> {
        todo!("R-04: InFlightGuard::acquire")
    }

    /// Stamp TTFT. Called exactly once, at the first upstream byte.
    pub fn mark_first_byte(&mut self) {
        todo!("R-04: InFlightGuard::mark_first_byte")
    }

    /// Complete the record and release everything. Consuming `self` is what makes the
    /// aborted-vs-finished distinction unforgeable.
    pub fn finish(self, rec: RequestRecord) {
        todo!("R-04: InFlightGuard::finish")
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        // R-04: release permits, decrement the gauge, and emit
        // RequestFinished { aborted: true } if finish() never ran.
    }
}

/// A per-backend retry budget.
#[derive(Debug, Default)]
pub struct TokenBucket {/* R-04 */}
