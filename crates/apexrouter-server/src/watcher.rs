//! OWNER: unit S-05 (server/src/{ws,assets,prober,watcher}.rs). Do not edit outside that
//! unit.
//!
//! The config watcher.
//!
//! It watches **`$CONFIG` and `$STATE/routes.json` only** — **never** a directory containing
//! endpoint logs, which children write to continuously; a recursive state-dir watch fires
//! ten times a second and there is a regression test that writes 1000 log lines and asserts
//! zero reloads.
//!
//! `notify` + a 250 ms debounce + a 10 s poll fallback, alongside `SIGHUP` and
//! `POST /v1/reload`. **A failed compile keeps the running table** and raises an `Alert`.

use crate::state::AppState;
use std::sync::Arc;

/// Run for the daemon's lifetime, reloading config and routes on change.
pub async fn config_watcher(state: Arc<AppState>) {
    todo!("S-05: config_watcher")
}
