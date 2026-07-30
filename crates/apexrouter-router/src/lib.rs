//! OWNER: unit R-08 (router/src/lib.rs, router/src/handler.rs). Do not edit outside that
//! unit — every other module here belongs to a different work unit.
//!
//! **The request path.** Routing table, `resolve()`, relay, SSE, retry/failover, breaker,
//! limits, aggregated `/v1/models`, telemetry, legacy compat handlers, and the one
//! Anthropic ingress translator. Knows nothing about vast, HuggingFace or process spawning.
//!
//! Two invariants are expressed as **types**, not comments:
//!
//! * The retry loop consumes `PreFlight` values and can only exit by producing a
//!   `Committed`. "Never retry after the first byte" is unrepresentable, not merely
//!   documented.
//! * `InFlightGuard` owns the permit, the byte budget, the gauge and the `RequestRecord`;
//!   its `Drop` emits `RequestFinished { aborted: true }` when `finish()` was never called.
//!   A client Ctrl-C therefore cannot leak a permit or a zombie UI row.
//!
//! Stage 0 skeleton: every module below is a stub owned by a later work unit.

#![allow(unused)]

pub mod anthropic;
pub mod attempt;
pub mod breaker;
pub mod compat;
pub mod errors;
pub mod handler;
pub mod limits;
pub mod models;
pub mod policy;
pub mod registry;
pub mod relay;
pub mod resolve;
pub mod table;
pub mod telemetry;

pub use handler::{proxy_handler, proxy_router};
pub use registry::{BackendRegistry, LiveBackend};
pub use resolve::{Candidate, Plan, RequestClass, RouteError, UnknownModelPolicy};
pub use table::{RoutingTable, TableBuilder};

use apexrouter_core::config::Config;
use apexrouter_core::usage::UsageWriter;
use apexrouter_protocol::{Event, RequestRecord};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex, Semaphore};

/// Everything the request path holds. Rebuilt rarely; read on every request.
pub struct RouterInner {
    /// The compiled table. Read is a pointer load — the request path never takes a lock
    /// beyond one `Semaphore`, and never touches the filesystem.
    table: arc_swap::ArcSwap<RoutingTable>,
    /// Live per-backend state, which **survives** a table recompile.
    registry: BackendRegistry,
    /// ONE pooled client, with `no_gzip`/`no_brotli`/`no_deflate` so bytes relay verbatim.
    http: reqwest::Client,
    /// GLOBAL byte budget, not just a request count.
    inflight_bytes: Arc<Semaphore>,
    /// Recent requests, for `GET /v1/requests` and the live table.
    ring: Mutex<VecDeque<RequestRecord>>,
    /// The broadcast every surface subscribes to.
    events: broadcast::Sender<Event>,
    /// Append-only usage rows.
    usage: UsageWriter,
    /// Hot-reloadable config.
    cfg: arc_swap::ArcSwap<Config>,
}

/// The shared handle every surface passes around.
pub type Router = Arc<RouterInner>;

impl RouterInner {
    /// Build the request path. The table is armed separately, after reconciliation.
    pub fn new(cfg: Arc<Config>, tx: broadcast::Sender<Event>, usage: UsageWriter) -> Router {
        todo!("R-08: RouterInner::new")
    }

    /// Swap in a freshly compiled table. A failed compile never reaches here — the running
    /// table keeps serving.
    pub fn store_table(&self, t: RoutingTable) {
        todo!("R-08: RouterInner::store_table")
    }

    /// Load the current table. This is the pointer load on the request path.
    pub fn table(&self) -> arc_swap::Guard<Arc<RoutingTable>> {
        self.table.load()
    }

    /// The live backend registry.
    pub fn registry(&self) -> &BackendRegistry {
        &self.registry
    }
}
