//! OWNER: unit M-01 (cli/src/mcp/**, cli/src/cmd/mcp.rs). Do not edit outside that unit.
//!
//! Hand-rolled newline-delimited JSON-RPC 2.0 over stdio, copying
//! `Prefrontal-RS/prefrontal-cli/src/mcp.rs` in shape. Hard rules:
//!
//! * **compact one-line JSON** (`to_string`, never `to_string_pretty`);
//! * **all logging to stderr**; nothing but MCP ever reaches stdout;
//! * exit promptly on stdin EOF;
//! * `initialize` **echoes the client's requested `protocolVersion`** back (falling back to
//!   `"2024-11-05"`), which is instant compatibility with every legacy revision;
//! * tool failures are results with `isError: true` and helpful text — JSON-RPC error codes
//!   (`-32601`, `-32700`) are reserved for protocol breakage.
//!
//! Dual-era hedge for the 2026-07-28 revision: also answer `server/discover` advertising
//! `supportedVersions`, accept-and-ignore per-request `_meta`, and emit
//! `resultType: "complete"`.
//!
//! Streamable-HTTP is **not** implemented, but [`dispatch`] is transport-agnostic, so an
//! axum route is a day's work when ApexOS-RV nodes need it over the network.

pub mod backend;
pub mod tools;

pub use backend::{LocalBackend, McpBackend, ProxyBackend};

use serde_json::Value;
use std::sync::Arc;

/// A JSON-RPC error. Reserved for protocol breakage; a failed tool is a **result** with
/// `isError: true`.
#[derive(Clone, Debug, PartialEq)]
pub struct RpcError {
    /// The JSON-RPC code, e.g. `-32601` (method not found), `-32700` (parse error).
    pub code: i64,
    /// Human-readable.
    pub message: String,
    /// Optional structured detail.
    pub data: Option<Value>,
}

/// Read stdin, dispatch, write stdout. Exits on EOF.
pub async fn run_stdio(backend: Arc<dyn McpBackend>) -> anyhow::Result<()> {
    todo!("M-01: run_stdio")
}

/// **Transport-agnostic** dispatch, so stdio today and an axum route tomorrow share one
/// implementation.
pub async fn dispatch(
    b: &dyn McpBackend,
    method: &str,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    todo!("M-01: dispatch")
}
