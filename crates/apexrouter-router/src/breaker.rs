//! OWNER: unit R-04 (router/src/{attempt,breaker,limits}.rs). Do not edit outside that unit.
//!
//! The circuit breaker. Atomics only — it is read on every request.
//!
//! It requires `min_volume` (default 5) observations before it can open, so a single 200 ms
//! blip on a 1 rps rig does not create a 30 second outage. Half-open admits exactly one
//! probe.

use std::time::Duration;

/// Per-backend breaker state.
#[derive(Debug, Default)]
pub struct Breaker {/* R-04: atomics: state, opened_at, failures, successes, volume */}

impl Breaker {
    /// May this request go out?
    pub fn check(&self) -> BreakerDecision {
        todo!("R-04: Breaker::check")
    }

    /// Record an outcome.
    pub fn record(&self, ok: bool) {
        todo!("R-04: Breaker::record")
    }

    /// Force open, honouring an upstream `Retry-After` when one was sent.
    pub fn trip(&self, retry_after: Option<Duration>) {
        todo!("R-04: Breaker::trip")
    }
}

/// What the breaker says about one request.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BreakerDecision {
    /// Closed. Go.
    Allow,
    /// Half-open, and this is the one probe.
    AllowProbe,
    /// Open. Skip this candidate.
    Deny {
        /// When it will admit a probe, unix seconds.
        retry_at_unix: i64,
    },
}
