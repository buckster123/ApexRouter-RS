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
//!   script can tell whether the answer came from a daemon or off disk. A `--json`
//!   *failure* is `{"error":{"kind":…,"message":…}}` on stdout, exit 1.
//! * No colour crate, no emoji.

// Stage 0 set this crate-wide allow because every `cmd/*` module was a stub. It stays
// until S-08 and M-01 land their modules; dropping it now would fail `clippy -D warnings`
// on other units' unfinished files rather than on anything this unit wrote.
#![allow(unused)]

mod cli;
mod cmd;
mod daemon;
mod mcp;
mod render;

use clap::Parser;

fn main() -> anyhow::Result<()> {
    // M-01 note: `apexrouter mcp` parses its own `--proxy` by hand and must reach
    // `mcp::run_stdio` *before* clap is involved, so nothing but JSON-RPC can reach stdout.
    // That interception goes here, at the top of `main`.
    let cli = cli::Cli::parse();
    init_tracing(cli.verbose);
    cli.apply_env();

    let json = cli.verb().json();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    match rt.block_on(cmd::dispatch(&cli)) {
        Ok(()) => Ok(()),
        // In `--json` mode the failure is data too, and it goes where the success would
        // have gone. Exit 1 without anyhow's `Error:` line, so stdout stays parseable.
        Err(e) if json => {
            let kind = render::error_kind(&e);
            render::print_error_json(kind, &format!("{e:#}"))?;
            std::process::exit(1);
        }
        Err(e) => Err(e),
    }
}

/// `tracing` to **stderr**, always. `-v` raises the level; `$RUST_LOG` wins when no `-v`
/// was given, so `RUST_LOG=apexrouter_router=debug` still works.
fn init_tracing(verbose: u8) {
    let level = match verbose {
        0 => None,
        1 => Some("info"),
        2 => Some("debug"),
        _ => Some("trace"),
    };
    let filter = match level {
        Some(l) => tracing_subscriber::EnvFilter::new(l),
        None => tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
    };
    // `try_init` because the daemon path may install its own subscriber later; a second
    // attempt must not abort the process.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
}
