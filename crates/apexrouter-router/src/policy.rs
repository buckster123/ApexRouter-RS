//! OWNER: unit R-02 (router/src/resolve.rs, router/src/policy.rs). Do not edit outside that
//! unit.
//!
//! Candidate ordering. Separate from `resolve()` because the *rules* pick the candidate set
//! and the *strategy* orders it, and only the second one is configurable.
//!
//! Every strategy orders by **health first** and its own key second: a `Ready` backend is
//! tried before an unprobed one, however cheap or idle the unprobed one looks. The sorts are
//! stable, so within one health rank the route's declared order survives — which is what
//! makes `FirstHealthy` mean what its name says.
//!
//! Nothing here reads the breaker. A breaker-open target is skipped by the attempt loop
//! (R-04), where the skip is recorded as an attempt instead of silently shrinking the plan.

use crate::resolve::Candidate;
use apexrouter_protocol::{Health, Money, Strategy};
use std::sync::atomic::{AtomicUsize, Ordering};

/// The rotation cursor for [`Strategy::RoundRobin`].
///
/// One counter for the whole process is deliberate: per-route state would have to live in
/// the compiled table, and the table is rebuilt on every reload — the rotation would then
/// reset every time somebody edited `routes.json`.
static ROUND_ROBIN: AtomicUsize = AtomicUsize::new(0);

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
    if cands.len() < 2 {
        return;
    }
    match strategy {
        // Route order, healthiest first. The stable sort keeps the declared order intact.
        Strategy::FirstHealthy => cands.sort_by_key(health_rank),
        Strategy::RoundRobin => {
            // `resolve()` hands a weighted route in with each target repeated `weight`
            // times, so the weighting is already expressed in the candidate list and
            // rotation alone implements weighted round robin.
            let n = ROUND_ROBIN.fetch_add(1, Ordering::Relaxed) % cands.len();
            cands.rotate_left(n);
            cands.sort_by_key(health_rank);
        }
        // The router's OWN in-flight counter: `/slots` 501s on a `--no-slots` build, so the
        // upstream's own number is not always there to be asked for.
        Strategy::LeastBusy => {
            cands.sort_by_key(|c| (health_rank(c), c.backend.inflight.load(Ordering::Relaxed)));
        }
        Strategy::Cheapest => cands.sort_by_key(|c| (health_rank(c), price_key(c))),
    }
}

/// `Ready` before everything else. `resolve()` has already dropped anything that is neither
/// `Ready` nor `Unknown`, so this is a two-way split in practice; the third rank keeps the
/// ordering total if a caller ever hands in a wider set.
fn health_rank(c: &Candidate) -> u8 {
    match c.backend.meta.load().health {
        Health::Ready { .. } => 0,
        Health::Unknown => 1,
        _ => 2,
    }
}

/// Sort key for `Cheapest`: `(the price is unknown, $/Mtok)`, so an unknown price sorts last
/// instead of sorting as free.
fn price_key(c: &Candidate) -> (bool, Money) {
    let meta = c.backend.meta.load();
    let tps = match meta.health {
        Health::Ready { tps_p50, .. } => tps_p50,
        _ => None,
    };
    match meta.price.as_ref().and_then(|p| p.per_mtok(tps).usd()) {
        Some(usd) => (false, usd),
        None => (true, Money::ZERO),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::LiveBackend;
    use apexrouter_core::config::RouterCfg;
    use apexrouter_protocol::{
        Backend, BackendId, BackendKind, BackendLimits, CredentialSource, PriceModel, Protocol,
        Provenance,
    };

    fn candidate(
        name: &str,
        price: Option<PriceModel>,
        inflight: u32,
        health: Health,
    ) -> Candidate {
        let meta = Backend {
            id: BackendId::parse(name).expect("test id"),
            kind: BackendKind::Node,
            protocol: Protocol::OpenAi,
            label: name.to_owned(),
            base_url: "http://127.0.0.1:8080".to_owned(),
            credential: CredentialSource::None,
            tags: Vec::new(),
            models: Vec::new(),
            limits: BackendLimits {
                max_concurrent: 8,
                queue_depth: 8,
                ctx: None,
                slots_total: Some(8),
            },
            price,
            health,
            provenance: Provenance::Manual,
            endpoint: None,
            enabled: true,
            devices: Vec::new(),
            last_error: None,
        };
        let backend = LiveBackend::new(meta, &RouterCfg::default());
        backend.inflight.store(inflight, Ordering::SeqCst);
        Candidate {
            backend,
            upstream_model: "m".to_owned(),
        }
    }

    fn ready() -> Health {
        Health::Ready {
            since_unix: 0,
            slots_busy: 0,
            slots_total: 4,
            tps_p50: Some(100.0),
        }
    }

    fn ids(cands: &[Candidate]) -> Vec<String> {
        cands
            .iter()
            .map(|c| c.backend.id.as_str().to_owned())
            .collect()
    }

    #[test]
    fn first_healthy_keeps_the_route_order() {
        let mut c = vec![
            candidate("a", None, 9, ready()),
            candidate("b", None, 0, ready()),
            candidate("c", None, 1, ready()),
        ];
        order_candidates(Strategy::FirstHealthy, &mut c);
        assert_eq!(ids(&c), ["a", "b", "c"]);
    }

    #[test]
    fn every_strategy_puts_a_ready_backend_before_an_unprobed_one() {
        for s in [
            Strategy::FirstHealthy,
            Strategy::RoundRobin,
            Strategy::LeastBusy,
            Strategy::Cheapest,
        ] {
            let mut c = vec![
                candidate("cold", Some(PriceModel::Free), 0, Health::Unknown),
                candidate("warm", None, 7, ready()),
            ];
            order_candidates(s, &mut c);
            assert_eq!(
                c[0].backend.id.as_str(),
                "warm",
                "{s:?} must prefer a Ready backend"
            );
        }
    }

    #[test]
    fn least_busy_orders_by_the_routers_own_in_flight_counter() {
        let mut c = vec![
            candidate("busy", None, 9, ready()),
            candidate("idle", None, 0, ready()),
            candidate("mid", None, 3, ready()),
        ];
        order_candidates(Strategy::LeastBusy, &mut c);
        assert_eq!(ids(&c), ["idle", "mid", "busy"]);
    }

    #[test]
    fn cheapest_orders_by_per_mtok_with_unknown_last() {
        let mut c = vec![
            candidate("unpriced", None, 0, ready()),
            candidate(
                "dear",
                Some(PriceModel::PerToken {
                    input: Money::from_usd(3.0),
                    output: Money::from_usd(9.0),
                }),
                0,
                ready(),
            ),
            candidate("free", Some(PriceModel::Free), 0, ready()),
            candidate(
                "rented",
                Some(PriceModel::PerHour {
                    dph: Money::from_usd(1.0),
                }),
                0,
                ready(),
            ),
        ];
        order_candidates(Strategy::Cheapest, &mut c);
        // free ($0) < rented ($1/hr at 100 tok/s = $2.78/Mtok) < dear ($6/Mtok blended);
        // the unpriced one is never assumed cheap and sorts last.
        assert_eq!(ids(&c), ["free", "rented", "dear", "unpriced"]);
    }

    #[test]
    fn cheapest_never_assumes_an_unhinted_per_hour_price_is_cheap() {
        // `PerHour` without a tps hint is `CostEstimate::Unknown` — it must not sort as $0.
        let mut c = vec![
            candidate(
                "rented",
                Some(PriceModel::PerHour {
                    dph: Money::from_usd(2.0),
                }),
                0,
                Health::Ready {
                    since_unix: 0,
                    slots_busy: 0,
                    slots_total: 1,
                    tps_p50: None,
                },
            ),
            candidate(
                "metered",
                Some(PriceModel::PerToken {
                    input: Money::from_usd(10.0),
                    output: Money::from_usd(10.0),
                }),
                0,
                ready(),
            ),
        ];
        order_candidates(Strategy::Cheapest, &mut c);
        assert_eq!(ids(&c), ["metered", "rented"]);
    }

    #[test]
    fn round_robin_advances_the_head_without_dropping_candidates() {
        let mk = || {
            vec![
                candidate("a", None, 0, ready()),
                candidate("b", None, 0, ready()),
                candidate("c", None, 0, ready()),
            ]
        };
        let mut seen = Vec::new();
        for _ in 0..12 {
            let mut c = mk();
            order_candidates(Strategy::RoundRobin, &mut c);
            assert_eq!(c.len(), 3, "rotation never drops a candidate");
            seen.push(c[0].backend.id.as_str().to_owned());
        }
        seen.sort();
        seen.dedup();
        assert_eq!(
            seen.len(),
            3,
            "the head must rotate over all three: {seen:?}"
        );
    }

    #[test]
    fn ordering_one_candidate_is_a_no_op_for_every_strategy() {
        for s in [
            Strategy::FirstHealthy,
            Strategy::RoundRobin,
            Strategy::LeastBusy,
            Strategy::Cheapest,
        ] {
            let mut c = vec![candidate("only", None, 0, Health::Unknown)];
            order_candidates(s, &mut c);
            assert_eq!(ids(&c), ["only"], "{s:?}");
        }
    }
}
