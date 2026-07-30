//! OWNER: unit M-01 (cli/src/mcp/**, cli/src/cmd/mcp.rs). Do not edit outside that unit.
//!
//! The tool definitions and their JSON schemas. All names are prefixed `apexrouter_`,
//! because three MCP servers share `~/Projects/.mcp.json`.
//!
//! Descriptions are **long and operational**: an agent should get from `apexrouter_status`
//! to a working `OPENAI_BASE_URL` without reading a doc.
//!
//! The money tool is deliberately shaped as a refusal that doubles as a dry run:
//! `apexrouter_vast_rent` without `confirm` and `max_usd_per_hour` returns `isError: true`
//! **carrying the full cost preview and the current credit**, and creates nothing.
