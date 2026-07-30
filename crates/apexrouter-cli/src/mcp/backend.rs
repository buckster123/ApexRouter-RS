//! OWNER: unit M-01 (cli/src/mcp/**, cli/src/cmd/mcp.rs). Do not edit outside that unit.
//!
//! The two ways an MCP tool call can be answered.
//!
//! [`LocalBackend`] answers `Pure`/`ReadState` tools directly from `apexrouter-core` **even
//! when the daemon is down**, and returns a helpful `isError` result for mutations
//! ("run `apexrouter serve`"). [`ProxyBackend`] forwards to `$APEXROUTER_URL` with
//! `$APEXROUTER_TOKEN`.

use async_trait::async_trait;

/// One method per tool. Filled in by M-01 against `ARCHITECTURE.md` §8.
#[async_trait]
pub trait McpBackend: Send + Sync {
    /* M-01: one method per tool */
}

/// Answers from `apexrouter-core` and `$STATE`, with no daemon required.
pub struct LocalBackend {/* M-01 */}

/// Forwards to a running daemon's control plane.
pub struct ProxyBackend {/* M-01 */}
