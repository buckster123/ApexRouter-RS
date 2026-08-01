//! OWNER: unit S-01. The vast fleet poller.
//!
//! `GET /v1/snapshot` deliberately probes nothing, so rented boxes reach it through
//! [`crate::state::AppState::fleet`] — and this task is what keeps that cache honest when no
//! handler happens to be reading the fleet. `lib.rs` documented this task for a full release
//! before it existed; the Fleet & cost page blanked between refreshes for exactly that long
//! (GARDEN-RUNS.md, R4 tooling findings).
//!
//! Three rules:
//!
//! * **Being offline is never evidence.** A failed poll leaves the cache untouched and the
//!   ledger untouched; with active rentals on the books it raises one coalescing alert,
//!   because a box billing while the operator cannot see it is the product's original sin.
//! * **Nothing here can spend or destroy.** Read-only calls: `instances()` and `account()`.
//! * **The event bus only fires on change.** Sixty seconds of identical fleet is not news.

use crate::state::AppState;
use apexrouter_core::ledger::Ledger;
use apexrouter_protocol::{AlertLevel, Event};
use std::sync::Arc;
use std::time::Duration;

/// Never poll faster than this, whatever the config says. Vast publishes no rate limits.
const FLEET_POLL_FLOOR_SECS: u64 = 10;

/// Refresh the fleet cache forever. Spawned once at daemon start, beside the prober and
/// the config watcher; exits only when the runtime does.
pub async fn vast_fleet_poller(state: Arc<AppState>) {
    loop {
        let secs = state.cfg().vast.fleet_poll_secs;
        if secs == 0 {
            // Disabled. Re-check occasionally in case a reload turned it on.
            tokio::time::sleep(Duration::from_secs(60)).await;
            continue;
        }
        tokio::time::sleep(Duration::from_secs(secs.max(FLEET_POLL_FLOOR_SECS))).await;
        poll_once(&state).await;
    }
}

/// One refresh: read the fleet and the credit, update the cache, broadcast on change.
pub async fn poll_once(state: &Arc<AppState>) {
    let Some(api) = crate::api::vast::vast_api() else {
        return;
    };
    match api.instances().await {
        Ok(instances) => {
            let credit = api.account().await.ok().map(|a| a.credit);
            let before = state.fleet_cache();
            let changed =
                before.instances != instances || (credit.is_some() && before.credit != credit);
            state.update_fleet(instances.clone(), credit);
            if changed {
                state.emit(Event::VastFleetChanged { instances, credit });
            }
        }
        Err(e) => {
            // The cache keeps its last observation — stale-and-labelled beats empty.
            let actives = active_rentals(state);
            if actives > 0 {
                state.alert(
                    AlertLevel::Warning,
                    "vast.fleet.unreachable",
                    format!(
                        "cannot reach vast.ai ({e}) and the ledger says {actives} rental(s) \
                         may be billing — check the console when back online"
                    ),
                );
            }
            tracing::warn!(error = %e, "vast fleet poll failed; keeping the last observation");
        }
    }
}

/// How many rentals the ledger still counts as active. Zero on any read problem: an
/// unreadable ledger is its own alert elsewhere, not a reason to cry wolf here.
fn active_rentals(state: &Arc<AppState>) -> usize {
    Ledger::open(&state.paths)
        .and_then(|l| l.active())
        .map(|rows| rows.len())
        .unwrap_or(0)
}
