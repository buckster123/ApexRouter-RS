//! OWNER: unit C-17 (core/checks.rs). Do not edit outside that unit.
//!
//! One check registry, backing `doctor`, `diagnose` and the four native smoke probes.
//!
//! Checks run **concurrently** with per-check timeouts and stream as each lands, so
//! `diagnose --only rate-limits` is instant instead of waiting through four sequential SSH
//! probes. A check that panics yields `CheckStatus::Fail`; it never poisons the run.
//!
//! [`CheckCtx::ext`] is how `apexrouter_providers::checks` injects its clients without
//! `core` ever depending on `providers`.

use crate::config::Config;
use crate::paths::Paths;
use apexrouter_protocol::{CheckId, CheckResult, InstanceId, RigSnapshot};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

/// One diagnostic.
#[async_trait]
pub trait Check: Send + Sync {
    /// `"creds.vast"`, `"ports.8888"`, `"builds.vulkan"`, …
    fn id(&self) -> CheckId;
    /// The row label.
    fn label(&self) -> &str;
    /// What this check needs in order to mean anything.
    fn needs(&self) -> CheckNeeds;
    /// Run it. Must not panic; must respect the runner's timeout.
    async fn run(&self, ctx: &CheckCtx) -> CheckResult;
}

/// What a check requires to be meaningful. Anything unavailable yields `Skipped`, not
/// `Fail` — being offline is not a defect.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CheckNeeds {
    /// Filesystem and `/proc` only.
    Local,
    /// The internet.
    Network,
    /// A running daemon.
    Daemon,
    /// A specific rented instance.
    Instance,
}

/// Everything a check may read.
pub struct CheckCtx {
    /// Resolved paths.
    pub paths: Paths,
    /// The live config.
    pub cfg: Arc<Config>,
    /// One pooled HTTP client, shared.
    pub http: reqwest::Client,
    /// The last rig scan, when there is one.
    pub rig: Option<Arc<RigSnapshot>>,
    /// Where the control plane is, when a daemon is running.
    pub proxy_url: Option<String>,
    /// Which instance an `Instance` check is about.
    pub instance: Option<InstanceId>,
    /// Provider-supplied clients, injected by `apexrouter-server` at startup.
    pub ext: HashMap<String, Arc<dyn std::any::Any + Send + Sync>>,
}

/// The registered checks.
#[derive(Default)]
pub struct Registry {
    /* C-17 */
    checks: Vec<Arc<dyn Check>>,
}

impl Registry {
    /// An empty registry.
    pub fn new() -> Registry {
        Registry { checks: Vec::new() }
    }

    /// Add one check.
    pub fn register(&mut self, c: Arc<dyn Check>) {
        self.checks.push(c);
    }

    /// Every registered id, for `GET /v1/checks`.
    pub fn ids(&self) -> Vec<CheckId> {
        self.checks.iter().map(|c| c.id()).collect()
    }

    /// Run every check (or one, when `only` is given) **concurrently**, streaming each
    /// result through `tx` as it lands and also returning the whole set.
    pub async fn run(
        &self,
        ctx: &CheckCtx,
        only: Option<&str>,
        tx: tokio::sync::mpsc::Sender<CheckResult>,
    ) -> Vec<CheckResult> {
        todo!("C-17: Registry::run")
    }
}

/// The checks that need nothing but this machine: `creds.*`, `ports.*`, `builds.*`,
/// `devices.*`, `models.*`, `state.writable`, `legacy.migration`.
pub fn local_checks() -> Vec<Arc<dyn Check>> {
    todo!("C-17: local_checks")
}
