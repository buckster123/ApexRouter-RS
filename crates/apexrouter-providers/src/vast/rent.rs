//! OWNER: unit P-04 (providers/src/vast/{rent,boot,stall}.rs). Do not edit outside that
//! unit.
//!
//! Renting. **Reserve before billing.**
//!
//! `ledger.reserve()` appends a `Reserved` row and returns a `PendingLaunch`; the create
//! call happens; `pending.commit(instance_id)` appends `Confirmed`. Dropping the guard
//! without committing appends `OrphanSuspect` synchronously. A `SIGKILL` skips `Drop`
//! entirely — which is why the `Reserved` row, written *before* the call, is the real
//! protection.

use super::api::VastApi;
use apexrouter_core::error::Result;
use apexrouter_core::ledger::Ledger;
use apexrouter_core::money::SpendApproval;
use apexrouter_protocol::{Event, InstanceId, RentRequest};
use tokio::sync::broadcast;

/// The vast.ai `Provisioner`.
pub struct VastProvisioner {/* P-04: impl Provisioner */}

/// Rent one box. There is no path to a billing call that does not take a [`SpendApproval`].
pub async fn rent(
    api: &dyn VastApi,
    ledger: &Ledger,
    req: &RentRequest,
    approval: SpendApproval,
    tx: &broadcast::Sender<Event>,
) -> Result<InstanceId> {
    todo!("P-04: rent")
}
