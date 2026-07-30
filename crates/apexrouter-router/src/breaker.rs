//! OWNER: unit R-04 (router/src/{attempt,breaker,limits}.rs). Do not edit outside that unit.
//!
//! The circuit breaker. Atomics only — it is read on every request.
//!
//! It requires `min_volume` (default 5) observations before it can open, so a single 200 ms
//! blip on a 1 rps rig does not create a 30 second outage. Half-open admits exactly one
//! probe.
//!
//! Two ways in, and they are deliberately different:
//!
//! * [`Breaker::record`] is the statistical path. It opens only once `min_volume`
//!   observations have accumulated **and** at least [`FAILURE_PERCENT`] of them failed.
//! * [`Breaker::trip`] is the conclusive path, used by [`crate::attempt::attempt`] for a
//!   connect/DNS/TLS failure or a pre-header timeout (`ARCHITECTURE.md` §4.3) and for an
//!   upstream that sent a `Retry-After`. Evidence that the port is dead is not a blip, so
//!   it does not wait for a quorum.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Closed: everything goes out.
const CLOSED: u8 = 0;
/// Open: nothing goes out until `open_until_ms`.
const OPEN: u8 = 1;
/// Half-open: exactly one probe goes out.
const HALF_OPEN: u8 = 2;

/// Observations required before [`Breaker::record`] may open the breaker.
const DEFAULT_MIN_VOLUME: u32 = 5;
/// How long the breaker stays open when the upstream gave us no `Retry-After`.
const DEFAULT_OPEN_MS: u64 = 30_000;
/// Percentage of a window that must fail before the breaker opens.
const FAILURE_PERCENT: u32 = 50;

/// Unix milliseconds. Wall clock, because [`BreakerDecision::Deny`] must hand a client an
/// absolute `retry_at_unix`, and a monotonic instant cannot be serialised.
pub(crate) fn now_unix_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_millis()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

/// Round unix milliseconds up to unix seconds — never tell a client to retry early.
fn ceil_secs(ms: i64) -> i64 {
    ms.div_euclid(1000) + i64::from(ms.rem_euclid(1000) != 0)
}

/// Per-backend breaker state.
#[derive(Debug)]
pub struct Breaker {
    /// [`CLOSED`], [`OPEN`] or [`HALF_OPEN`].
    state: AtomicU8,
    /// When it last opened, unix ms. Observability only.
    opened_at_ms: AtomicI64,
    /// When it will admit a probe, unix ms.
    open_until_ms: AtomicI64,
    /// Failures in the current window.
    failures: AtomicU32,
    /// Successes in the current window.
    successes: AtomicU32,
    /// Observations in the current window.
    volume: AtomicU32,
    /// True while the one half-open probe is out.
    probe: AtomicBool,
    /// Observations required before opening.
    min_volume: AtomicU32,
    /// Default cool-down, when the upstream sent no `Retry-After`.
    open_ms: AtomicU64,
}

impl Default for Breaker {
    fn default() -> Self {
        Breaker {
            state: AtomicU8::new(CLOSED),
            opened_at_ms: AtomicI64::new(0),
            open_until_ms: AtomicI64::new(0),
            failures: AtomicU32::new(0),
            successes: AtomicU32::new(0),
            volume: AtomicU32::new(0),
            probe: AtomicBool::new(false),
            min_volume: AtomicU32::new(DEFAULT_MIN_VOLUME),
            open_ms: AtomicU64::new(DEFAULT_OPEN_MS),
        }
    }
}

impl Breaker {
    /// May this request go out?
    pub fn check(&self) -> BreakerDecision {
        match self.state.load(Ordering::Acquire) {
            CLOSED => BreakerDecision::Allow,
            OPEN => {
                let until = self.open_until_ms.load(Ordering::Acquire);
                if now_unix_ms() < until {
                    return BreakerDecision::Deny {
                        retry_at_unix: ceil_secs(until),
                    };
                }
                // The cool-down elapsed. Move to half-open; whoever wins the CAS clears the
                // probe flag, then everybody races for the single probe below.
                if self
                    .state
                    .compare_exchange(OPEN, HALF_OPEN, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    self.probe.store(false, Ordering::Release);
                }
                self.take_probe(until)
            }
            _ => self.take_probe(self.open_until_ms.load(Ordering::Acquire)),
        }
    }

    /// Record an outcome.
    pub fn record(&self, ok: bool) {
        let state = self.state.load(Ordering::Acquire);
        if state == OPEN {
            // A straggler from before the trip. It says nothing about the new window.
            return;
        }
        self.volume.fetch_add(1, Ordering::AcqRel);
        if ok {
            self.successes.fetch_add(1, Ordering::AcqRel);
            if state == HALF_OPEN {
                self.close();
            }
            return;
        }
        self.failures.fetch_add(1, Ordering::AcqRel);
        if state == HALF_OPEN {
            // The probe failed: straight back to open, for a full cool-down.
            self.trip(None);
            return;
        }
        let volume = self.volume.load(Ordering::Acquire);
        let failures = self.failures.load(Ordering::Acquire);
        if volume >= self.min_volume.load(Ordering::Acquire)
            && failures.saturating_mul(100) >= volume.saturating_mul(FAILURE_PERCENT)
        {
            self.trip(None);
        }
    }

    /// Force open, honouring an upstream `Retry-After` when one was sent.
    pub fn trip(&self, retry_after: Option<Duration>) {
        let now = now_unix_ms();
        let cool_down = match retry_after {
            Some(d) => i64::try_from(d.as_millis()).unwrap_or(i64::MAX),
            None => i64::try_from(self.open_ms.load(Ordering::Acquire)).unwrap_or(i64::MAX),
        };
        self.opened_at_ms.store(now, Ordering::Release);
        self.open_until_ms
            .store(now.saturating_add(cool_down), Ordering::Release);
        self.failures.store(0, Ordering::Release);
        self.successes.store(0, Ordering::Release);
        self.volume.store(0, Ordering::Release);
        self.probe.store(false, Ordering::Release);
        self.state.store(OPEN, Ordering::Release);
    }

    /// Claim the single half-open probe, or deny.
    fn take_probe(&self, until: i64) -> BreakerDecision {
        if self
            .probe
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return BreakerDecision::AllowProbe;
        }
        // Somebody else is probing. Come back after the probe should have landed — never
        // a timestamp in the past.
        let now = ceil_secs(now_unix_ms());
        let at = ceil_secs(until);
        BreakerDecision::Deny {
            retry_at_unix: if at > now { at } else { now + 1 },
        }
    }

    /// Back to closed, with a fresh window.
    fn close(&self) {
        self.failures.store(0, Ordering::Release);
        self.successes.store(0, Ordering::Release);
        self.volume.store(0, Ordering::Release);
        self.open_until_ms.store(0, Ordering::Release);
        self.probe.store(false, Ordering::Release);
        self.state.store(CLOSED, Ordering::Release);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Pretend the cool-down elapsed, without sleeping for 30 seconds.
    fn expire(b: &Breaker) {
        b.open_until_ms
            .store(now_unix_ms() - 1_000, Ordering::Release);
    }

    #[test]
    fn starts_closed() {
        assert_eq!(Breaker::default().check(), BreakerDecision::Allow);
    }

    #[test]
    fn min_volume_gates_opening() {
        let b = Breaker::default();
        for _ in 0..(DEFAULT_MIN_VOLUME - 1) {
            b.record(false);
        }
        assert_eq!(
            b.check(),
            BreakerDecision::Allow,
            "four failures must not open a breaker whose min_volume is five"
        );
        b.record(false);
        assert!(
            matches!(b.check(), BreakerDecision::Deny { .. }),
            "the fifth failure completes the quorum and opens it"
        );
    }

    #[test]
    fn a_healthy_minority_of_failures_never_opens() {
        let b = Breaker::default();
        for i in 0..20 {
            b.record(i % 5 != 0); // 20 % failures, well over min_volume
        }
        assert_eq!(b.check(), BreakerDecision::Allow);
    }

    #[test]
    fn half_open_admits_exactly_one_probe() {
        let b = Breaker::default();
        b.trip(None);
        assert!(matches!(b.check(), BreakerDecision::Deny { .. }));
        expire(&b);
        assert_eq!(b.check(), BreakerDecision::AllowProbe);
        for _ in 0..5 {
            assert!(
                matches!(b.check(), BreakerDecision::Deny { .. }),
                "only one probe is ever in flight"
            );
        }
    }

    #[test]
    fn a_successful_probe_closes_the_breaker() {
        let b = Breaker::default();
        b.trip(None);
        expire(&b);
        assert_eq!(b.check(), BreakerDecision::AllowProbe);
        b.record(true);
        assert_eq!(b.check(), BreakerDecision::Allow);
    }

    #[test]
    fn a_failed_probe_reopens_for_a_full_cool_down() {
        let b = Breaker::default();
        b.trip(None);
        expire(&b);
        assert_eq!(b.check(), BreakerDecision::AllowProbe);
        b.record(false);
        match b.check() {
            BreakerDecision::Deny { retry_at_unix } => {
                let now = now_unix_ms() / 1000;
                assert!(retry_at_unix >= now + 25 && retry_at_unix <= now + 31);
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn retry_after_is_honoured() {
        let b = Breaker::default();
        b.trip(Some(Duration::from_secs(120)));
        match b.check() {
            BreakerDecision::Deny { retry_at_unix } => {
                let now = now_unix_ms() / 1000;
                assert!(
                    retry_at_unix >= now + 118 && retry_at_unix <= now + 121,
                    "expected ~now+120, got {retry_at_unix} (now {now})"
                );
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn stragglers_after_a_trip_do_not_disturb_the_new_window() {
        let b = Breaker::default();
        b.trip(None);
        for _ in 0..10 {
            b.record(true);
        }
        assert!(
            matches!(b.check(), BreakerDecision::Deny { .. }),
            "in-flight requests landing after a trip cannot close it early"
        );
    }

    #[test]
    fn deny_never_points_into_the_past() {
        let b = Breaker::default();
        b.trip(None);
        expire(&b);
        assert_eq!(b.check(), BreakerDecision::AllowProbe);
        match b.check() {
            BreakerDecision::Deny { retry_at_unix } => {
                assert!(retry_at_unix > now_unix_ms() / 1000);
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }
}
