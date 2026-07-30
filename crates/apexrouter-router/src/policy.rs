//! OWNER: unit R-02 (router/src/resolve.rs, router/src/policy.rs). Do not edit outside that
//! unit.
//!
//! Candidate ordering. Separate from `resolve()` because the *rules* pick the candidate set
//! and the *strategy* orders it, and only the second one is configurable.

use crate::resolve::Candidate;
use apexrouter_protocol::Strategy;

/// Order candidates in place, according to the route's strategy.
///
/// `Cheapest` orders by `PriceModel::per_mtok`, with `CostEstimate::Unknown` **last** — an
/// unpriced backend is never assumed cheap.
///
/// Takes `&mut Vec`, not `&mut [_]`: this is the published signature, and an ordering pass
/// is allowed to *drop* candidates (a breaker-open target, a weight-zero entry), which a
/// slice cannot express.
#[allow(clippy::ptr_arg)]
pub fn order_candidates(strategy: Strategy, cands: &mut Vec<Candidate>) {
    todo!("R-02: order_candidates")
}
