//! OWNER: unit S-06 (cli/src/{main,cli,daemon,render}.rs and the core `cmd/*` modules).
//! Do not edit outside that unit.
//!
//! `apexrouter` — the CLI, the daemon entrypoint and the MCP stdio server, in one binary.
//! `~/Projects/.mcp.json` registers `target/release/apexrouter` with `args: ["mcp"]`, so
//! there is no third fat link on the release critical path.
//!
//! House rules this binary enforces:
//!
//! * `fn main() -> anyhow::Result<()>`; failures via `?`/`bail!`, so anyhow prints
//!   `Error: …` to **stderr** and exits 1.
//! * **Tracing always to stderr**, because `mcp` shares the binary and owns stdout.
//! * `--json` prints the protocol type and nothing else, wrapped in the `Envelope` so a
//!   script can tell whether the answer came from a daemon or off disk.
//! * No colour crate, no emoji.

#![allow(unused)]

mod cli;
mod cmd;
mod daemon;
mod mcp;
mod render;

fn main() -> anyhow::Result<()> {
    // Stage 0 skeleton. Unit S-06 replaces this with the clap dispatch.
    anyhow::bail!("apexrouter: not implemented yet — the CLI is built by work unit S-06")
}
