//! OWNER: unit S-05 (server/src/{ws,assets,prober,watcher}.rs). Do not edit outside that
//! unit.
//!
//! `GET /ws`.
//!
//! **Subscribe to the broadcast BEFORE sending the snapshot** — otherwise an event that
//! lands between assembling the snapshot and subscribing is lost forever, and the client
//! renders a picture that never converges. Re-send a full snapshot on `RecvError::Lagged`,
//! and `tokio::select!` on `socket.recv()` too, so a close is noticed.

use crate::state::AppState;
use axum::extract::{State, WebSocketUpgrade};
use axum::response::Response;
use std::sync::Arc;

/// Upgrade, subscribe, snapshot, stream.
pub async fn ws_handler(ws: WebSocketUpgrade, State(s): State<Arc<AppState>>) -> Response {
    todo!("S-05: ws_handler")
}
