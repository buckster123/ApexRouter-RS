//! OWNER: unit P-05 (providers/src/ssh.rs). Do not edit outside that unit.
//!
//! The tunnel supervisor. It **owns the `ssh` `Child`** — `pgrep` appears nowhere, because
//! `pgrep ssh` can kill an unrelated connection.
//!
//! The exact flag set (`ARCHITECTURE.md` §4.9):
//! `-N -L <local>:127.0.0.1:8000 -p <port> root@<host> -o ExitOnForwardFailure=yes
//! -o ServerAliveInterval=30 -o ServerAliveCountMax=3 -o StrictHostKeyChecking=accept-new
//! -o UserKnownHostsFile=$STATE/ssh/known_hosts -o ControlMaster=auto
//! -o ControlPath=$STATE/ssh/cm-<instance-id> -o ControlPersist=5m`.
//!
//! Two things the source proposals dropped and this does not:
//!
//! 1. **A reconnect supervisor.** `ExitOnForwardFailure` makes ssh *exit* on a dead link; it
//!    does not re-establish it. A laptop wifi blip must not leave a $3.34/hr box unreachable
//!    until a human notices. Bounded backoff 1 s → ×2 → cap 30 s, then a `Serious` alert.
//! 2. **Teardown does `ssh -O exit` and unlinks the ControlPath.** Killing the `-N -L` child
//!    leaves the ControlMaster alive for `ControlPersist` minutes against a destroyed box.

use apexrouter_core::error::Result;
use apexrouter_protocol::{InstanceId, TunnelSpec, TunnelStatus};
use std::sync::Arc;

/// Owns every `ssh` child and persists `TunnelStatus` so a daemon restart re-adopts rather
/// than colliding on the local port.
pub struct TunnelSupervisor {/* P-05: owns the Child; persists TunnelStatus */}

impl TunnelSupervisor {
    /// Bring one forward up.
    pub async fn up(&self, spec: TunnelSpec) -> Result<TunnelStatus> {
        todo!("P-05: TunnelSupervisor::up")
    }

    /// Kill the child, run `ssh -O exit`, and unlink the ControlPath.
    pub async fn down(&self, id: InstanceId) -> Result<()> {
        todo!("P-05: TunnelSupervisor::down")
    }

    /// Re-adopt persisted tunnels at startup.
    pub async fn adopt_all(&self) -> Result<Vec<TunnelStatus>> {
        todo!("P-05: TunnelSupervisor::adopt_all")
    }

    /// The bounded-retry reconnect loop. Runs for the daemon's lifetime.
    pub async fn supervise(self: Arc<Self>) {
        todo!("P-05: TunnelSupervisor::supervise")
    }
}
