//! OWNER: unit R-04 (router/src/{attempt,breaker,limits}.rs). Do not edit outside that unit.
//!
//! The attempt state machine.
//!
//! **The retry loop consumes `PreFlight` values and can only exit by producing a
//! `Committed`.** "Never retry after the first byte" is unrepresentable, not merely
//! documented — there is no code path that calls [`attempt`] twice on the same `PreFlight`,
//! and ownership is what enforces it.

use crate::limits::InFlightGuard;
use crate::resolve::Candidate;
use std::time::Duration;

/// Everything one upstream attempt needs. Consumed by [`attempt`].
pub struct PreFlight<'a> {
    /* R-04: candidate, body plan, headers, deadline, cfg */
    _marker: std::marker::PhantomData<&'a Candidate>,
}

/// An upstream response whose first byte has arrived. **Past this point there is no retry.**
///
/// Owns the `InFlightGuard`, so dropping it releases the permit, decrements the gauge and —
/// if `finish()` never ran — emits `RequestFinished { aborted: true }`.
pub struct Committed {/* R-04: upstream Response + the InFlightGuard it owns */}

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

/// One upstream attempt.
pub async fn attempt(p: PreFlight<'_>) -> std::result::Result<Committed, Retryable> {
    todo!("R-04: attempt")
}
