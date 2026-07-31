//! OWNER: unit M-01 (cli/src/mcp/**, cli/src/cmd/mcp.rs). Do not edit outside that unit.
//!
//! `apexrouter mcp [--proxy URL]` — the stdio JSON-RPC server. `--proxy`/`-p`/env is
//! parsed **by hand** so `clap` stays out of the MCP module, and nothing but MCP JSON-RPC
//! ever reaches stdout.
//!
//! Why by hand, concretely: `clap` writes usage, errors and `--help` to **stdout**, and an
//! MCP client parses stdout as newline-delimited JSON-RPC. One `error: unexpected argument`
//! line is a protocol violation the client reports as a crashed server. So this module runs
//! from `main` **before** `Cli::parse()` is reached, owns stdout for the rest of the
//! process, and sends its own usage and every log line to stderr.
//!
//! The two global flags that must still work are `--config` and `--home`: they are pushed
//! into the environment exactly as [`crate::cli::Cli::apply_env`] does it, so `Paths` and
//! `Config` resolve identically whether they arrived through clap or through here.

use crate::mcp::{self, LocalBackend, McpBackend, ProxyBackend};
use std::path::PathBuf;
use std::sync::Arc;

/// What `apexrouter mcp` was asked for.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Options {
    /// `--config PATH`, pushed into `$APEXROUTER_CONFIG`.
    pub config: Option<PathBuf>,
    /// `--home DIR`, pushed into `$APEXROUTER_HOME`.
    pub home: Option<PathBuf>,
    /// `--proxy URL` / `-p URL`. Absent falls back to `$APEXROUTER_URL`, and absent again
    /// means a [`LocalBackend`].
    pub proxy: Option<String>,
    /// `-h` / `--help`: print usage **on stderr** and serve nothing.
    pub help: bool,
}

/// Usage, printed on stderr because this verb owns stdout.
const USAGE: &str = "\
apexrouter mcp — the MCP stdio server (newline-delimited JSON-RPC 2.0)

USAGE:
    apexrouter [--config PATH] [--home DIR] mcp [--proxy URL]

OPTIONS:
    -p, --proxy URL   Forward every tool to this control plane instead of answering
                      locally. Defaults to $APEXROUTER_URL; with neither, tools are
                      answered from $STATE and a running daemon when there is one.
    -h, --help        Print this, on stderr, and exit.

The bearer is read from the variable named by [server] token_env, then $APEXROUTER_TOKEN.
Register it with an agent harness as:
    {\"command\": \"/path/to/apexrouter\", \"args\": [\"mcp\"]}
";

/// Run the MCP server if this process was invoked as `apexrouter mcp`.
///
/// Called from the very top of `main`, **before** `clap` sees anything, so no clap
/// diagnostic can ever reach the stdout an MCP client is parsing.
///
/// # Errors
/// A malformed `mcp` invocation, an unreadable config, or an I/O failure on stdio.
pub fn intercept() -> anyhow::Result<bool> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match parse(&argv)? {
        None => Ok(false),
        Some(opts) => {
            run(&opts)?;
            Ok(true)
        }
    }
}

/// Is this an `mcp` invocation, and if so what does it want?
///
/// Returns `Ok(None)` for every other verb, which is how `main` falls through to `clap`
/// unchanged. Global flags that take a value are stepped over so
/// `apexrouter --home /tmp/x mcp` is recognised.
///
/// # Errors
/// A flag that needs a value and did not get one, or an unknown flag after `mcp` — those
/// are worth failing on rather than ignoring, because an MCP client will not show them.
pub fn parse(argv: &[String]) -> anyhow::Result<Option<Options>> {
    let mut opts = Options::default();
    let mut i = 0;
    // Step over the global flags clap would have taken, so `mcp` is still found behind them.
    while i < argv.len() {
        let arg = argv[i].as_str();
        if let Some(v) = arg.strip_prefix("--config=") {
            opts.config = Some(PathBuf::from(v));
            i += 1;
            continue;
        }
        if let Some(v) = arg.strip_prefix("--home=") {
            opts.home = Some(PathBuf::from(v));
            i += 1;
            continue;
        }
        match arg {
            "--config" => {
                opts.config = Some(value_after(argv, i, "--config")?);
                i += 2;
                continue;
            }
            "--home" => {
                opts.home = Some(value_after(argv, i, "--home")?);
                i += 2;
                continue;
            }
            _ => {}
        }
        if arg.starts_with('-') {
            i += 1;
            continue;
        }
        break;
    }
    if argv.get(i).map(String::as_str) != Some("mcp") {
        return Ok(None);
    }

    i += 1;
    while i < argv.len() {
        let arg = argv[i].as_str();
        if let Some(v) = arg.strip_prefix("--proxy=") {
            opts.proxy = Some(v.to_string());
            i += 1;
            continue;
        }
        match arg {
            "--proxy" | "-p" => {
                opts.proxy = Some(string_after(argv, i, arg)?);
                i += 2;
            }
            "--help" | "-h" => {
                opts.help = true;
                i += 1;
            }
            "--config" => {
                opts.config = Some(value_after(argv, i, "--config")?);
                i += 2;
            }
            "--home" => {
                opts.home = Some(value_after(argv, i, "--home")?);
                i += 2;
            }
            "-v" | "--verbose" | "--no-autostart" => i += 1,
            other => anyhow::bail!(
                "`apexrouter mcp` does not take `{other}` — it takes only --proxy URL. \
                 Run `apexrouter mcp --help` (usage goes to stderr; stdout is the protocol)."
            ),
        }
    }
    Ok(Some(opts))
}

/// The value that must follow a flag, as a path.
///
/// # Errors
/// When the flag was last on the line.
fn value_after(argv: &[String], at: usize, flag: &str) -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(string_after(argv, at, flag)?))
}

/// The value that must follow a flag.
///
/// # Errors
/// When the flag was last on the line, or its value is empty.
fn string_after(argv: &[String], at: usize, flag: &str) -> anyhow::Result<String> {
    argv.get(at + 1)
        .map(String::as_str)
        .filter(|v| !v.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("`{flag}` needs a value"))
}

/// Serve one `apexrouter mcp` invocation to EOF on stdin.
///
/// # Errors
/// An unreadable config, or an I/O failure on stdio.
pub fn run(opts: &Options) -> anyhow::Result<()> {
    if opts.help {
        eprint!("{USAGE}");
        return Ok(());
    }
    // Env vars stay the single resolution mechanism (ARCHITECTURE §5.1), exactly as
    // `Cli::apply_env` does it for every other verb.
    if let Some(p) = &opts.config {
        std::env::set_var("APEXROUTER_CONFIG", p);
    }
    if let Some(h) = &opts.home {
        std::env::set_var("APEXROUTER_HOME", h);
    }
    init_tracing();

    let backend = backend_for(opts.proxy.as_deref())?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(mcp::run_stdio(backend))
}

/// `tracing` to **stderr**, always, and quiet by default.
///
/// An MCP client shows a server's stderr only when something goes wrong, so `warn` is the
/// right floor; `$RUST_LOG` raises it for a debugging session.
fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
}

/// Which backend answers the tools.
///
/// `--proxy` wins, then `$APEXROUTER_URL`; with neither, a [`LocalBackend`] answers
/// read-only tools from `$STATE` and forwards the rest to a daemon when one is running.
///
/// # Errors
/// When paths or config cannot be resolved for the local backend.
pub fn backend_for(proxy: Option<&str>) -> anyhow::Result<Arc<dyn McpBackend>> {
    let url = proxy
        .map(str::to_string)
        .filter(|u| !u.trim().is_empty())
        .or_else(|| env_nonempty("APEXROUTER_URL"));
    match url {
        Some(u) => {
            tracing::info!(url = %u, "apexrouter mcp: proxying to a control plane");
            Ok(Arc::new(ProxyBackend::new(&u, token())))
        }
        None => Ok(Arc::new(LocalBackend::load()?)),
    }
}

/// The bearer: the variable named by `[server] token_env` first, `$APEXROUTER_TOKEN` after.
///
/// A config that will not parse is not fatal here — a proxy invocation may be pointed at
/// another machine entirely — so the fallback name is used instead.
fn token() -> Option<String> {
    let named = apexrouter_core::config::Config::load()
        .ok()
        .map(|c| c.server.token_env)
        .unwrap_or_else(|| "APEXROUTER_TOKEN".to_string());
    env_nonempty(&named).or_else(|| env_nonempty("APEXROUTER_TOKEN"))
}

/// An env var, treating the empty string as unset.
fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_bare_mcp_invocation_is_recognised() {
        let o = parse(&argv(&["mcp"])).expect("parse").expect("an mcp verb");
        assert_eq!(o, Options::default());
    }

    #[test]
    fn every_other_verb_falls_through_to_clap() {
        for case in [
            vec!["status"],
            vec!["--json"],
            vec!["--help"],
            vec![],
            vec!["endpoint", "start", "mcp"],
            vec!["status", "mcp"],
        ] {
            assert!(
                parse(&argv(&case)).expect("parse").is_none(),
                "{case:?} must reach clap"
            );
        }
    }

    #[test]
    fn the_globals_that_take_a_value_are_stepped_over() {
        let o = parse(&argv(&[
            "--home",
            "/tmp/x",
            "--config",
            "/tmp/c.toml",
            "mcp",
        ]))
        .expect("parse")
        .expect("an mcp verb");
        assert_eq!(o.home.as_deref(), Some(std::path::Path::new("/tmp/x")));
        assert_eq!(
            o.config.as_deref(),
            Some(std::path::Path::new("/tmp/c.toml"))
        );

        // ...and their `=` spellings, and the value-less globals.
        let o = parse(&argv(&["--home=/tmp/y", "-v", "--no-autostart", "mcp"]))
            .expect("parse")
            .expect("an mcp verb");
        assert_eq!(o.home.as_deref(), Some(std::path::Path::new("/tmp/y")));
    }

    /// The trap this guards: `--home mcp` must not let `mcp` be read as the verb.
    #[test]
    fn a_global_flags_value_is_never_mistaken_for_the_verb() {
        assert!(parse(&argv(&["--home", "mcp"])).expect("parse").is_none());
    }

    #[test]
    fn proxy_is_accepted_in_all_three_spellings() {
        for case in [
            vec!["mcp", "--proxy", "http://127.0.0.1:2739"],
            vec!["mcp", "-p", "http://127.0.0.1:2739"],
            vec!["mcp", "--proxy=http://127.0.0.1:2739"],
        ] {
            let o = parse(&argv(&case)).expect("parse").expect("an mcp verb");
            assert_eq!(
                o.proxy.as_deref(),
                Some("http://127.0.0.1:2739"),
                "{case:?}"
            );
        }
    }

    #[test]
    fn a_flag_without_its_value_fails_rather_than_serving_something_wrong() {
        assert!(parse(&argv(&["mcp", "--proxy"])).is_err());
        assert!(parse(&argv(&["--home"])).is_err());
    }

    #[test]
    fn an_unknown_flag_after_mcp_is_an_error_not_a_silent_ignore() {
        let e = parse(&argv(&["mcp", "--frobnicate"])).expect_err("must fail");
        assert!(e.to_string().contains("--frobnicate"), "{e}");
        assert!(e.to_string().contains("--proxy"), "{e}");
    }

    #[test]
    fn help_is_recognised_and_serves_nothing() {
        let o = parse(&argv(&["mcp", "--help"]))
            .expect("parse")
            .expect("an mcp verb");
        assert!(o.help);
        // `run` with `help` must not build a runtime, touch $STATE or read stdin.
        run(&o).expect("help exits cleanly");
    }

    #[test]
    fn usage_never_offers_to_write_on_stdout() {
        assert!(USAGE.contains("stderr"));
        assert!(USAGE.contains("--proxy"));
    }
}
