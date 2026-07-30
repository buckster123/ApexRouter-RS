//! OWNER: unit S-01 (server/src/{lib,state,shutdown}.rs). Do not edit outside that unit —
//! every other module here belongs to a different work unit.
//!
//! The axum application: the proxy listener, the control listener, `/ws`, auth, embedded
//! assets and the job runner.
//!
//! **Two listeners in one process**, because a single listener cannot satisfy both
//! contracts. The proxy is a catch-all by contract; a catch-all `any()` route and the
//! static-asset `get("/{*path}")` route panic on `Router::merge` in axum 0.8, and a shared
//! listener would permanently shadow llama.cpp's own `/health` for control clients. Two
//! listeners also make it possible to expose the proxy to the LAN without exposing the
//! control plane.
//!
//! [`api_router`] is `pub` so ApexOS-RS can mount the control plane in its own process.
//!
//! Stage 0 skeleton: every module below is a stub owned by a later work unit.

#![allow(unused)]

pub mod api;
pub mod assets;
pub mod auth;
pub mod jobs;
pub mod prober;
pub mod shutdown;
pub mod state;
pub mod watcher;
pub mod ws;

pub use state::AppState;

use apexrouter_core::config::Config;
use apexrouter_core::lockfile::DaemonLock;
use apexrouter_core::paths::Paths;
use std::sync::Arc;

/// Run the daemon: lock, load, reconcile, arm the table, bind both listeners, start the
/// pollers, and drain on signal.
///
/// Startup order matters and is not negotiable — **reconcile before binding**, so the table
/// is never armed with a picture that predates adoption.
pub async fn serve(paths: Paths, cfg: Config, lock: DaemonLock) -> anyhow::Result<()> {
    todo!("S-01: serve")
}

/// The control-plane `Router`. Exported so ApexOS-RS can mount it.
pub fn api_router(state: Arc<AppState>) -> axum::Router {
    todo!("S-01: api_router")
}

/// Compute `Adoption` for every endpoint record and re-adopt or tidy, **before** the
/// listeners bind.
///
/// Vast reconciliation is deliberately **not** on this path: it is a network call and the
/// laptop is often offline. It runs in a background task and raises an `Alert`.
pub async fn reconcile_on_start(state: &Arc<AppState>) -> anyhow::Result<()> {
    todo!("S-01: reconcile_on_start")
}
