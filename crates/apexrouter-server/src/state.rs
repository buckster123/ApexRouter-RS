//! OWNER: unit S-01 (server/src/{lib,state,shutdown}.rs). Do not edit outside that unit.
//!
//! The shared application state. Everything in-process: the routing table, the backend
//! registry, every child process, the health prober, the file watcher, the ledger, the usage
//! writer and both listeners. The CLI, the MCP server, the web UI and the Slint app are
//! **clients**.

use apexrouter_core::checks::Registry;
use apexrouter_core::config::Config;
use apexrouter_core::lockfile::DaemonLock;
use apexrouter_core::paths::Paths;
use apexrouter_core::store::Store;
use apexrouter_protocol::Event;
use apexrouter_providers::local::LocalProvisioner;
use arc_swap::ArcSwap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{broadcast, Mutex};

/// Everything a handler may reach.
pub struct AppState {
    /// Resolved paths.
    pub paths: Paths,
    /// Hot-reloadable config. `SIGHUP` and `POST /v1/reload` swap it.
    pub cfg: ArcSwap<Config>,
    /// Atomic state writes.
    pub store: Store,
    /// The request path.
    pub router: apexrouter_router::Router,
    /// The broadcast every surface subscribes to.
    pub tx: broadcast::Sender<Event>,
    /// The local `llama-server`/vLLM supervisor.
    pub supervisor: Arc<LocalProvisioner>,
    /// Background operations behind `?no_wait=true`.
    pub jobs: crate::jobs::JobRegistry,
    /// `doctor` and `diagnose`, with provider checks registered at startup.
    pub checks: Arc<Registry>,
    /// For `uptime`.
    pub started_at: Instant,
    /// Held for the process lifetime. Released by process exit.
    pub lock: Arc<Mutex<DaemonLock>>,
    /* provider slots filled in Stage 5: vast, hf, together, tunnels */
}
