//! OWNER: unit S-05 (server/src/{ws,assets,prober,watcher}.rs). Do not edit outside that
//! unit.
//!
//! The health prober. Sizes each backend's semaphore from `/props.total_slots`, falling back
//! to `/slots` length, falling back to config — which is why the argv builder passes
//! `--props`, `--metrics` and `--slots` to every server we launch, feature-detected.
//! Otherwise the sizing would read an endpoint we never enabled.
//!
//! It requires N consecutive failures before `Degraded`, and it maintains the
//! `model_index` that `resolve()` rule 3 reads.

use crate::state::AppState;
use std::sync::Arc;

/// Run for the daemon's lifetime, probing every enabled backend on an interval.
pub async fn health_prober(state: Arc<AppState>) {
    todo!("S-05: health_prober")
}
