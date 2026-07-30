//! OWNER: unit P-01 (providers/src/local/**). Do not edit outside that unit.
//!
//! The local supervisor, and the `Provisioner` trait every other provider implements.
//!
//! Every measured defect in `docs/port/03` is closed **by construction**, not by care:
//! `LD_LIBRARY_PATH` is always set (the `build-vulkan` trailing-colon RUNPATH trap), the
//! port bind-probe holds its reservation under a per-endpoint lock until the health gate
//! passes, the health gate is a real wall-clock deadline that **resets on observed
//! progress**, and **the failure path is the stop path** — on expiry we kill the child,
//! remove the record, mark `Failed` with the log tail, and clear the route.

use apexrouter_core::error::Result;
use apexrouter_core::money::SpendApproval;
use apexrouter_protocol::{
    ArgvPreview, Backend, BackendId, BackendKind, CostEstimate, EndpointSpec, FitPlan,
};
use async_trait::async_trait;

pub mod adopt;
pub mod supervisor;

/// How a backend is brought up and taken down. Implemented by the local supervisor, the
/// vast provisioner and (trivially) by node/managed registration.
#[async_trait]
pub trait Provisioner: Send + Sync {
    /// Which kind of backend this provisioner owns.
    fn kind(&self) -> BackendKind;
    /// Resolve a draft into an executable plan, with the fit, the cost and the warnings the
    /// operator must see **before** confirming.
    async fn plan(&self, draft: &EndpointSpec) -> Result<LaunchPlan>;
    /// Execute a plan. Anything that costs money requires a [`SpendApproval`].
    async fn up(&self, plan: LaunchPlan, approval: Option<SpendApproval>) -> Result<Backend>;
    /// Take it down.
    async fn down(&self, id: &BackendId, mode: DownMode) -> Result<()>;
    /// The last `tail` lines. **The call an agent makes when a start failed.**
    async fn logs(&self, id: &BackendId, tail: usize) -> Result<Vec<String>>;
    /// Re-derive live backends from persisted facts at startup.
    async fn reconcile(&self) -> Result<Vec<Backend>>;
}

/// How hard to take something down.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DownMode {
    /// Stop accepting, wait for in-flight to finish, then stop.
    Drain,
    /// Stop now.
    Now,
    /// Stop and delete the record.
    Forget,
}

/// Exactly what will happen, before it happens.
#[derive(Clone, Debug)]
pub struct LaunchPlan {
    /// The resolved spec.
    pub spec: EndpointSpec,
    /// The exact argv and env. **No credential is ever in `argv`.**
    pub argv: ArgvPreview,
    /// What we expect it to occupy.
    pub fit: Option<FitPlan>,
    /// What it will cost.
    pub cost: CostEstimate,
    /// Anything the operator must read before confirming.
    pub warnings: Vec<String>,
    /// The port reserved for it, held under a per-endpoint lock until the health gate passes.
    pub port: u16,
}

/// The local `llama-server`/vLLM supervisor.
pub struct LocalProvisioner {/* P-01: Paths, Config, Store, tx, rig cache */}

/// Why a launch did not happen, or did not survive.
///
/// Each variant names **who** is in the way, because that is what makes the message
/// actionable: which backend holds the port, which endpoints hold the VRAM, what the log
/// said.
#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    /// Somebody already has the port.
    #[error("port {port} is in use{}", .held_by.as_ref().map(|b| format!(" by {b}")).unwrap_or_default())]
    PortInUse {
        /// The port.
        port: u16,
        /// Which of our backends holds it, when it is one of ours.
        held_by: Option<BackendId>,
    },
    /// `fit()` says this would not fit against **live** free VRAM minus reservations.
    #[error("insufficient VRAM: need {need_mb} MiB, {free_mb} MiB free (held by {})", .held_by.iter().map(|b| b.as_str()).collect::<Vec<_>>().join(", "))]
    InsufficientVram {
        /// What the plan needs.
        need_mb: u64,
        /// What is actually free.
        free_mb: u64,
        /// Which endpoints hold the rest.
        held_by: Vec<BackendId>,
    },
    /// The `llama-server` binary is not where the build said it was.
    #[error("binary missing: {0}")]
    BinaryMissing(String),
    /// The weights are not where the spec said they were.
    #[error("model missing: {0}")]
    ModelMissing(String),
    /// The health gate's wall-clock deadline expired. The child has already been killed and
    /// the record removed — **the failure path is the stop path**.
    #[error("health gate timed out")]
    HealthTimeout {
        /// The last lines of its log, so the operator does not have to go looking.
        log_tail: Vec<String>,
    },
    /// It exited before ever answering.
    #[error("exited early{}", .code.map(|c| format!(" with code {c}")).unwrap_or_default())]
    ExitedEarly {
        /// Its exit code, when it had one.
        code: Option<i32>,
        /// The last lines of its log.
        log_tail: Vec<String>,
    },
}

impl LocalProvisioner {
    /// Build the supervisor. Owns no children until [`Provisioner::up`] is called.
    pub fn new() -> Self {
        LocalProvisioner {}
    }
}

impl Default for LocalProvisioner {
    fn default() -> Self {
        Self::new()
    }
}
