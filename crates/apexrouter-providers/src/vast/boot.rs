//! OWNER: unit P-04 (providers/src/vast/{rent,boot,stall}.rs). Do not edit outside that
//! unit.
//!
//! The boot watchdog. Polls no faster than `[providers.vast] poll_min_ms` (vast publishes no
//! rate limits), treats `exited | offline | unknown` as **terminal** — they never recover —
//! and auto-destroys a wedged instance at `max_boot_secs`.

use super::api::VastApi;
use apexrouter_core::error::Result;
use apexrouter_protocol::{BootPhase, Event, InstanceId};
use tokio::sync::broadcast;

/// Drive one instance's boot state machine to a terminal phase, broadcasting each transition.
pub async fn watch_boot(
    api: &dyn VastApi,
    id: InstanceId,
    max_secs: u64,
    tx: &broadcast::Sender<Event>,
) -> Result<BootPhase> {
    todo!("P-04: watch_boot")
}
