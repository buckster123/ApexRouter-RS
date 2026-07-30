//! OWNER: unit S-02 (server/src/auth.rs). Do not edit outside that unit.
//!
//! Auth, scopes and **the mutation gate**.
//!
//! A loopback control plane is **not** a trust boundary. A cross-origin `fetch` to
//! `POST http://127.0.0.1:8888/switch` with `Content-Type: text/plain` is a CORS *simple
//! request*: no preflight, the request is delivered, and the attacker never needs to read
//! the response. So every mutating request on **either** listener passes
//! [`require_mutation_origin`]:
//!
//! 1. `Host` must be in the bind allowlist — this closes DNS rebinding, which otherwise
//!    makes an attacker's page same-origin.
//! 2. If `Origin` is present it must be same-origin; if `Sec-Fetch-Site` is present it must
//!    be `same-origin` or `none`.
//! 3. Otherwise a bearer with `write` scope is required.
//!
//! Non-browser clients (the CLI, Slint, `curl`) send neither header, so they pass rule 2
//! unchanged. There is deliberately **no `CorsLayer`**: the embedded UI is same-origin, and
//! this gate is stronger than a CORS policy.

use crate::state::AppState;
use apexrouter_core::config::ServerCfg;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, Method, Uri};
use axum::middleware::Next;
use axum::response::Response;
use std::net::SocketAddr;
use std::sync::Arc;

/// The auth middleware. **Absent `ConnectInfo` fails closed**, never open.
pub async fn require_auth(
    State(s): State<Arc<AppState>>,
    ci: ConnectInfo<SocketAddr>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    todo!("S-02: require_auth")
}

/// Bearer accepted three ways: `Authorization: Bearer <t>`, `X-ApexRouter-Token: <t>`, and
/// `?token=<t>`.
///
/// The `TraceLayer` span records **method and path only**, never the query string, because
/// `?token=` is an accepted presentation.
pub fn extract_presented_token(h: &HeaderMap, uri: &Uri) -> Option<String> {
    todo!("S-02: extract_presented_token")
}

/// Derive the required scope from (path, method). `/v1/tokens*` is always `Admin`.
pub fn required_scope(path: &str, method: &Method) -> Scope {
    todo!("S-02: required_scope")
}

/// Token scopes.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    /// Read-only.
    Read,
    /// Mutations.
    Write,
    /// Token management and shutdown.
    Admin,
}

/// CSRF + DNS-rebinding defence. Applied to EVERY mutation on BOTH listeners.
///
/// The `Err` variant is a ready-to-return `Response` rather than an error code, so a
/// refusal carries the explanatory body the operator needs. That makes it a large `Err`,
/// which is fine on a path that runs once per mutation.
#[allow(clippy::result_large_err)]
pub fn require_mutation_origin(
    h: &HeaderMap,
    bind: &SocketAddr,
    cfg: &ServerCfg,
) -> Result<(), Response> {
    todo!("S-02: require_mutation_origin")
}
