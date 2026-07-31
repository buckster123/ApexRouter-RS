//! OWNER: unit S-06 (cli/src/{main,cli,daemon,render}.rs, cli/src/cmd/{mod,status,serve,
//! config,rig,models,fit,endpoint,route,switch,url,version,completions,update}.rs). Do not
//! edit outside that unit — the remaining `cmd/*` modules belong to S-08 and `cmd/mcp.rs`
//! to M-01.
//!
//! One module per noun group. `--json` is **per subcommand, never global**, and prints
//! `serde_json::to_string_pretty` of the protocol type and **nothing else** on stdout.
//! `tracing` always goes to stderr, because `mcp` shares the binary and owns stdout.
//!
//! [`Ctx`] is the one place `Paths` and `Config` are resolved, and [`Ctx::serving`] the one
//! place a subcommand asks "daemon or `$STATE`?". No command opens `$STATE` by hand.

pub mod approvals;
pub mod backend;
pub mod compare;
pub mod completions;
pub mod config;
pub mod doctor;
pub mod endpoint;
pub mod env;
pub mod fit;
pub mod hf;
pub mod mcp;
pub mod migrate;
pub mod models;
pub mod open;
pub mod profile;
pub mod provider;
pub mod recipe;
pub mod rig;
pub mod route;
pub mod serve;
pub mod smoke;
pub mod status;
pub mod swap;
pub mod switch;
pub mod token;
pub mod tunnel;
pub mod up;
pub mod update;
pub mod url;
pub mod usage;
pub mod vast;
pub mod version;

use crate::cli::{Cli, Command};
use crate::daemon::{resolve_serving, Need, Serving};
use apexrouter_core::config::Config;
use apexrouter_core::paths::Paths;

/// Everything a subcommand needs before it decides how it will be served.
pub struct Ctx {
    /// Where everything lives. Resolved once.
    pub paths: Paths,
    /// The effective configuration.
    pub cfg: Config,
    /// May this invocation start a daemon?
    pub autostart: bool,
}

impl Ctx {
    /// Resolve paths and config, **after** [`Cli::apply_env`] has pushed `--config`/`--home`
    /// into the environment.
    ///
    /// # Errors
    /// When no home directory can be determined, or a config file exists but will not parse.
    pub fn load(cli: &Cli) -> anyhow::Result<Ctx> {
        let paths = Paths::resolve()?;
        let cfg = Config::load()?;
        Ok(Ctx {
            paths,
            cfg,
            autostart: cli.autostart(),
        })
    }

    /// Daemon, `$STATE`, or a clean reason for neither.
    ///
    /// # Errors
    /// Propagates a failure that makes both paths impossible.
    pub async fn serving(&self, need: Need) -> anyhow::Result<Serving> {
        resolve_serving(need, &self.cfg, &self.paths, self.autostart).await
    }
}

/// Run the parsed verb.
///
/// # Errors
/// Whatever the verb produced. `main` turns it into `Error: …` on stderr, or the
/// `{"error":{…}}` envelope on stdout when the leaf asked for `--json`.
pub async fn dispatch(cli: &Cli) -> anyhow::Result<()> {
    let verb = cli.verb();
    // `completions` must work before anything on disk is consulted: it is the one verb a
    // packaging script runs in a sandbox with no $HOME.
    if let Command::Completions(args) = &verb {
        return completions::run(args);
    }
    if let Command::External(args) = &verb {
        return pending(args);
    }
    // `config validate` must answer about a config that does not parse; `Ctx::load` would
    // refuse to build the very report the verb exists to print.
    if let Command::Config {
        cmd: crate::cli::ConfigCmd::Validate(args),
    } = &verb
    {
        return config::validate(args);
    }

    let ctx = Ctx::load(cli)?;
    match &verb {
        Command::Status(a) => status::run(&ctx, a).await,
        Command::Serve(a) => serve::run(&ctx, a).await,
        Command::Update(a) => update::run(&ctx, a),
        Command::Url(a) => url::run(&ctx, a).await,
        Command::Version(a) => version::run(&ctx, a).await,
        Command::Config { cmd } => config::run(&ctx, cmd),
        Command::Rig(a) => rig::run(&ctx, a).await,
        Command::Models { cmd } => models::run(&ctx, cmd).await,
        Command::Fit(a) => fit::run(&ctx, a).await,
        Command::Endpoint { cmd } => endpoint::run(&ctx, cmd).await,
        Command::Route { cmd } => route::run(&ctx, cmd).await,
        Command::Switch { cmd } => switch::run(&ctx, cmd).await,
        // ---- S-08 ------------------------------------------------------------------
        Command::Up(a) => up::run(&ctx, a).await,
        Command::Swap(a) => swap::run(&ctx, a).await,
        Command::Open => open::run(&ctx).await,
        Command::Env(a) => env::run(&ctx, a).await,
        Command::Backend { cmd } => backend::run(&ctx, cmd).await,
        Command::Recipe { cmd } => recipe::run(&ctx, cmd).await,
        Command::Profile { cmd } => profile::run(&ctx, cmd).await,
        Command::Provider { cmd } => provider::run(&ctx, cmd).await,
        Command::Vast { cmd } => vast::run(&ctx, cmd).await,
        Command::Tunnel { cmd } => tunnel::run(&ctx, cmd).await,
        Command::Approvals { cmd } => approvals::run(&ctx, cmd).await,
        Command::Hf { cmd } => hf::run(&ctx, cmd).await,
        Command::Usage(a) => usage::run(&ctx, a).await,
        Command::Compare(a) => compare::run(&ctx, a).await,
        Command::Smoke(a) => smoke::run(&ctx, a).await,
        Command::Doctor(a) => doctor::run(&ctx, a).await,
        Command::Migrate(a) => migrate::run(&ctx, a),
        Command::Token { cmd } => token::run(&ctx, cmd),
        // Handled above, before the on-disk state is touched.
        Command::Completions(_) | Command::External(_) => Ok(()),
    }
}

/// The verbs the build plan delivers in a later stage, and who owns each one.
///
/// Reporting this beats clap's "unrecognized subcommand": the operator running the
/// MK1-CORE transcript learns which unit is missing and what to type meanwhile.
const PENDING: &[(&str, &str, &str)] = &[("mcp", "M-01", "")];

/// Report an unimplemented — or simply unknown — verb.
///
/// # Errors
/// Always. This function exists to produce a good failure.
fn pending(args: &[String]) -> anyhow::Result<()> {
    let verb = args.first().map(String::as_str).unwrap_or("");
    match PENDING.iter().find(|(name, _, _)| *name == verb) {
        Some((_, unit, "")) => Err(anyhow::anyhow!(
            "`apexrouter {verb}` is delivered by work unit {unit} and is not in this build"
        )),
        Some((_, unit, hint)) => Err(anyhow::anyhow!(
            "`apexrouter {verb}` is delivered by work unit {unit} and is not in this build — {hint}"
        )),
        None => Err(anyhow::anyhow!(
            "unknown verb `{verb}` — run `apexrouter --help` for the list"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_planned_verb_names_its_work_unit() {
        let e = pending(&["mcp".to_string(), "x".to_string()]).expect_err("must fail");
        let msg = e.to_string();
        assert!(msg.contains("M-01"), "{msg}");
    }

    #[test]
    fn an_unknown_verb_points_at_help() {
        let e = pending(&["frobnicate".to_string()]).expect_err("must fail");
        assert!(e.to_string().contains("--help"), "{e}");
    }
}
