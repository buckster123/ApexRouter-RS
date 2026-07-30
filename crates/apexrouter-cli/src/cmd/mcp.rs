//! OWNER: unit M-01 (cli/src/mcp/**, cli/src/cmd/mcp.rs). Do not edit outside that unit.
//!
//! `apexrouter mcp [--proxy URL]` — the stdio JSON-RPC server. `--proxy`/`-p`/env is
//! parsed **by hand** so `clap` stays out of the MCP module, and nothing but MCP JSON-RPC
//! ever reaches stdout.
