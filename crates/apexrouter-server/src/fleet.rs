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

/// One-shot, **alert-only** compare of the local ledger against the live vast fleet.
///
/// Spawned at daemon start after bind is scheduled — never blocks listening, never creates
/// or destroys anything. Surfaces:
/// * ledger-active ids missing from the live fleet (orphans / already-gone boxes),
/// * live fleet ids with no active ledger row (silent billing risk — the original A1 shape),
/// * unresolved `Reserved` / `OrphanSuspect` rows without an instance id.
///
/// Being offline is never evidence: a failed list leaves no alert beyond the fleet poller's
/// own "unreachable with actives" path.
pub async fn reconcile_ledger_once(state: Arc<AppState>) {
    let Some(api) = crate::api::vast::vast_api() else {
        return;
    };
    let Ok(ledger) = Ledger::open(&state.paths) else {
        return;
    };
    let Ok(active) = ledger.active() else {
        return;
    };
    let Ok(instances) = api.instances().await else {
        return;
    };

    // Seed the fleet cache so the first snapshot is not empty when we just read the world.
    let credit = api.account().await.ok().map(|a| a.credit);
    state.update_fleet(instances.clone(), credit);

    let live_ids: std::collections::HashSet<u64> = instances.iter().map(|i| i.id.0).collect();
    let mut ledger_ids: std::collections::HashSet<u64> = std::collections::HashSet::new();

    for row in &active {
        match row.instance_id {
            Some(id) => {
                ledger_ids.insert(id.0);
                if !live_ids.contains(&id.0) {
                    state.alert(
                        AlertLevel::Warning,
                        &format!("vast.ledger.missing_live.{}", id.0),
                        format!(
                            "ledger still counts instance {} as active (state {:?}) but vast \
                             does not list it — confirm on the console, then \
                             `apexrouter vast forget {} --yes`",
                            id.0, row.state, id.0
                        ),
                    );
                }
            }
            None => {
                state.alert(
                    AlertLevel::Serious,
                    &format!("vast.ledger.unresolved.{}", row.seq),
                    format!(
                        "ledger has an unresolved {:?} row (seq {}) with no instance id — a \
                         create may have succeeded without a local record; check \
                         `apexrouter vast ls --orphans`",
                        row.state, row.seq
                    ),
                );
            }
        }
    }

    for inst in &instances {
        if !ledger_ids.contains(&inst.id.0) {
            state.alert(
                AlertLevel::Critical,
                &format!("vast.fleet.unknown_to_ledger.{}", inst.id.0),
                format!(
                    "vast lists instance {} ({}) but the local ledger has no active row for \
                     it — a box may be billing with no local record; run \
                     `apexrouter vast ls --orphans` and destroy or import it",
                    inst.id.0,
                    inst.gpu_name.as_deref().unwrap_or("unknown GPU")
                ),
            );
        }
    }
}
