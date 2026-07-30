//! OWNER: unit R-01 (router/src/table.rs, router/src/registry.rs). Do not edit outside that
//! unit.
//!
//! Live per-backend state.
//!
//! **The compiled table holds `Arc<LiveBackend>` pointers, so rebuilding the table never
//! resets live state.** `upsert` on an existing id preserves the `Semaphore`, the breaker,
//! the EWMA and the in-flight count — the regression test starts three in-flight requests,
//! recompiles the table, and asserts the permit count is unchanged.

use crate::breaker::Breaker;
use crate::limits::TokenBucket;
use apexrouter_core::config::RouterCfg;
use apexrouter_protocol::{Backend, BackendId};
use arc_swap::ArcSwap;
use std::sync::atomic::{AtomicBool, AtomicU32};
use std::sync::Arc;
use tokio::sync::Semaphore;

/// One backend, created once and mutated in place for the daemon's lifetime.
pub struct LiveBackend {
    /// Stable id.
    pub id: BackendId,
    /// The serialisable description.
    pub meta: ArcSwap<Backend>,
    /// Sized from `/props.total_slots`, falling back to `/slots`, falling back to config.
    pub sem: Arc<Semaphore>,
    /// Atomics only. Requires `min_volume` observations before it can open, so a single
    /// 200 ms blip on a 1 rps rig does not create a 30 s outage.
    pub breaker: Breaker,
    /// Per-backend retry budget, so a struggling backend cannot be amplified into a storm.
    pub retry_bucket: TokenBucket,
    /// The router's **own** in-flight counter — `/slots` 501s on `--no-slots` builds.
    pub inflight: AtomicU32,
    /// False while draining.
    pub accepting: AtomicBool,
    /// Rolling latency, for `LeastBusy` and the p50 columns.
    pub latency: LatencyEwma,
    /// Maintained by the health prober; read by `resolve()` rule 3.
    pub model_index: ArcSwap<Vec<String>>,
}

impl LiveBackend {
    /// Create live state for a backend seen for the first time.
    pub fn new(b: Backend, cfg: &RouterCfg) -> Arc<LiveBackend> {
        todo!("R-01: LiveBackend::new")
    }

    /// Replace the description without touching any live state.
    pub fn update_meta(&self, b: Backend) {
        todo!("R-01: LiveBackend::update_meta")
    }

    /// Grow or shrink the concurrency permit pool in place.
    pub fn resize_semaphore(&self, permits: u32) {
        todo!("R-01: LiveBackend::resize_semaphore")
    }

    /// Replace the model index the prober maintains.
    pub fn set_models(&self, m: Vec<String>) {
        todo!("R-01: LiveBackend::set_models")
    }
}

/// An exponentially weighted moving average of observed latency.
#[derive(Debug, Default)]
pub struct LatencyEwma {/* R-01 */}

/// Every live backend, by id. Survives table recompiles.
#[derive(Default)]
pub struct BackendRegistry {/* R-01: RwLock<HashMap<BackendId, Arc<LiveBackend>>> */}

impl BackendRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        BackendRegistry {}
    }

    /// Insert or update. **REUSES live state** when the id is already known.
    pub fn upsert(&self, b: Backend, cfg: &RouterCfg) -> Arc<LiveBackend> {
        todo!("R-01: BackendRegistry::upsert")
    }

    /// Forget a backend, returning its live state so the caller can drain it.
    pub fn remove(&self, id: &BackendId) -> Option<Arc<LiveBackend>> {
        todo!("R-01: BackendRegistry::remove")
    }

    /// Look one up.
    pub fn get(&self, id: &BackendId) -> Option<Arc<LiveBackend>> {
        todo!("R-01: BackendRegistry::get")
    }

    /// Every live backend.
    pub fn all(&self) -> Vec<Arc<LiveBackend>> {
        todo!("R-01: BackendRegistry::all")
    }

    /// Every backend's serialisable description, for a `Snapshot`.
    pub fn snapshot(&self) -> Vec<Backend> {
        todo!("R-01: BackendRegistry::snapshot")
    }
}
