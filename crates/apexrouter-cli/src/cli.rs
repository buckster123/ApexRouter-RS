//! OWNER: unit S-06 (cli/src/{main,cli,daemon,render}.rs and the core `cmd/*` modules).
//! Do not edit outside that unit.
//!
//! The `clap` derive tree: noun-grouped, house verb vocabulary, `--json` **per subcommand**.
//!
//! Global flags `--config` and `--home` are pushed into the process env **before**
//! `Config::load()`, so env vars stay the single resolution mechanism.
//!
//! Verbs this build does not implement yet fall into [`Command::External`], which reports
//! which work unit owns them instead of clap's bare "unrecognized subcommand". The noun
//! groups delivered by S-08 (`up`, `swap`, `doctor`, `usage`, `vast`, `hf`, …) and M-01
//! (`mcp`) add their own variants here when they land.

use crate::daemon::Need;
use apexrouter_protocol::{KvType, SamplingMode, SplitMode, Strategy};
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

/// `apexrouter` — one binary: the CLI, the daemon entrypoint and the MCP stdio server.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "apexrouter",
    version = apexrouter_protocol::VERSION,
    about = "Model aliasing and endpoint supervision for local and rented inference",
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Path to `config.toml`. Pushed into `$APEXROUTER_CONFIG` before anything loads.
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,
    /// State home. Pushed into `$APEXROUTER_HOME` before anything loads.
    #[arg(long, global = true, value_name = "DIR")]
    pub home: Option<PathBuf>,
    /// Never start a daemon on this invocation's behalf.
    #[arg(long, global = true)]
    pub no_autostart: bool,
    /// More logging, on stderr. Repeatable.
    #[arg(short = 'v', long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
    /// The verb. Absent means `status`.
    #[command(subcommand)]
    pub command: Option<Command>,
}

impl Cli {
    /// Push `--config`/`--home` into the process environment, **before** `Paths::resolve()`
    /// or `Config::load()` runs anywhere.
    ///
    /// Env vars stay the single resolution mechanism (ARCHITECTURE §5.1); this also means a
    /// daemon this CLI autostarts inherits the same resolution without re-passing flags.
    pub fn apply_env(&self) {
        if let Some(p) = &self.config {
            std::env::set_var("APEXROUTER_CONFIG", p);
        }
        if let Some(h) = &self.home {
            std::env::set_var("APEXROUTER_HOME", h);
        }
    }

    /// The verb to run. A bare `apexrouter` is `status`.
    pub fn verb(&self) -> Command {
        self.command
            .clone()
            .unwrap_or(Command::Status(StatusArgs::default()))
    }

    /// May this invocation start a daemon?
    pub fn autostart(&self) -> bool {
        !self.no_autostart
    }
}

/// Every noun group. `--json` lives on the leaves, never here.
#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// What is running, what is bound, and what it is doing.
    Status(StatusArgs),
    /// Run the daemon (or stop it).
    Serve(ServeArgs),
    /// Print the OpenAI base URL, and nothing else.
    Url(JsonFlag),
    /// Version information.
    Version(JsonFlag),
    /// Write a shell completion script to stdout.
    Completions(CompletionsArgs),
    /// Inspect and create `config.toml`.
    Config {
        /// What to do with it.
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// GPUs, llama.cpp builds, RAM and swap.
    Rig(RigArgs),
    /// Local GGUF weights.
    Models {
        /// What to do with them.
        #[command(subcommand)]
        cmd: ModelsCmd,
    },
    /// Solve context, KV type and layer offload for a model on this rig.
    Fit(FitArgs),
    /// Local `llama-server` and vLLM lifecycle.
    Endpoint {
        /// What to do with them.
        #[command(subcommand)]
        cmd: EndpointCmd,
    },
    /// Aliases and their target chains.
    Route {
        /// What to do with them.
        #[command(subcommand)]
        cmd: RouteCmd,
    },
    /// Re-point the default alias. Kept for muscle memory.
    Switch {
        /// Where to point it.
        #[command(subcommand)]
        cmd: SwitchCmd,
    },
    /// Anything this build does not implement yet, reported by name.
    #[command(external_subcommand)]
    External(Vec<String>),
}

/// The `--json` flag on its own, for verbs that take nothing else.
#[derive(Debug, Clone, Default, Args)]
pub struct JsonFlag {
    /// Print the JSON envelope and nothing else.
    #[arg(long)]
    pub json: bool,
}

/// `apexrouter status [--json] [--watch]`.
#[derive(Debug, Clone, Default, Args)]
pub struct StatusArgs {
    /// Print the JSON envelope and nothing else.
    #[arg(long)]
    pub json: bool,
    /// Redraw on an interval instead of printing once.
    #[arg(long)]
    pub watch: bool,
    /// Seconds between redraws under `--watch`.
    #[arg(long, value_name = "SECS", default_value_t = 2)]
    pub interval: u64,
}

/// `apexrouter serve …`.
#[derive(Debug, Clone, Default, Args)]
pub struct ServeArgs {
    /// Override `[server] proxy_bind`.
    #[arg(long, value_name = "ADDR")]
    pub proxy_bind: Option<String>,
    /// Override `[server] control_bind`.
    #[arg(long, value_name = "ADDR")]
    pub control_bind: Option<String>,
    /// Stay in the foreground. The default, and what a systemd unit wants.
    #[arg(long)]
    pub foreground: bool,
    /// Fork into the background and return once `/health` answers.
    #[arg(long, conflicts_with = "foreground")]
    pub detach: bool,
    /// Stop the running daemon instead of starting one.
    #[arg(long, conflicts_with_all = ["foreground", "detach"])]
    pub stop: bool,
    /// Do not serve the embedded web UI.
    #[arg(long)]
    pub no_ui: bool,
    /// Permit a non-loopback bind. Requires a token in the `--token-env` variable.
    #[arg(long)]
    pub allow_remote: bool,
    /// Name of the environment variable holding the bearer token.
    #[arg(long, value_name = "VAR")]
    pub token_env: Option<String>,
}

/// `apexrouter completions <shell>`.
#[derive(Debug, Clone, Args)]
pub struct CompletionsArgs {
    /// Which shell to generate for.
    #[arg(value_enum)]
    pub shell: clap_complete::Shell,
}

/// `apexrouter config …`.
#[derive(Debug, Clone, Subcommand)]
pub enum ConfigCmd {
    /// Write the fully commented example config to the resolved path.
    Init {
        /// Overwrite an existing file.
        #[arg(long)]
        force: bool,
    },
    /// Print the effective configuration.
    Show(JsonFlag),
    /// Print the path the config resolves to.
    Path(JsonFlag),
    /// Open the config in `$VISUAL`/`$EDITOR`.
    Edit,
}

/// `apexrouter rig [--json] [--rescan]`.
#[derive(Debug, Clone, Default, Args)]
pub struct RigArgs {
    /// Print the JSON envelope and nothing else.
    #[arg(long)]
    pub json: bool,
    /// Force a fresh scan rather than the daemon's cached one.
    #[arg(long)]
    pub rescan: bool,
}

/// `apexrouter models …`.
#[derive(Debug, Clone, Subcommand)]
pub enum ModelsCmd {
    /// Every local GGUF, shards grouped into one row.
    Ls(JsonFlag),
    /// One model in detail.
    Show {
        /// Model id, name, unique prefix, or a path on disk.
        model: String,
        /// Print the JSON envelope and nothing else.
        #[arg(long)]
        json: bool,
    },
}

/// `apexrouter fit <model> …`.
#[derive(Debug, Clone, Args)]
pub struct FitArgs {
    /// Model id, name, unique prefix, or a path on disk.
    pub model: String,
    /// `-dev` tokens, comma separated. Empty means every non-software GPU.
    #[arg(long, value_name = "D,D")]
    pub devices: Option<String>,
    /// Total context pool. Omit to let the solver search.
    #[arg(long)]
    pub ctx: Option<u32>,
    /// Slots sharing that pool.
    #[arg(long)]
    pub parallel: Option<u32>,
    /// KV cache element type.
    #[arg(long, value_enum)]
    pub kv: Option<KvArg>,
    /// `-sm` value.
    #[arg(long, value_enum)]
    pub split_mode: Option<SplitModeArg>,
    /// `--tensor-split` ratios, comma separated.
    #[arg(long, value_name = "R,R")]
    pub tensor_split: Option<String>,
    /// `-mg` value.
    #[arg(long)]
    pub main_gpu: Option<u32>,
    /// Logical batch size used for the compute-buffer estimate.
    #[arg(long)]
    pub batch: Option<u32>,
    /// Print the JSON envelope and nothing else.
    #[arg(long)]
    pub json: bool,
}

/// `apexrouter endpoint …`.
#[derive(Debug, Clone, Subcommand)]
pub enum EndpointCmd {
    /// Every endpoint record.
    Ls(JsonFlag),
    /// One endpoint in detail.
    Show {
        /// Endpoint id.
        id: String,
        /// Print the JSON envelope and nothing else.
        #[arg(long)]
        json: bool,
    },
    /// Tail an endpoint's log.
    Logs {
        /// Endpoint id.
        id: String,
        /// How many lines.
        #[arg(short = 'n', long, default_value_t = 200)]
        lines: usize,
        /// Keep printing as the file grows.
        #[arg(short = 'f', long)]
        follow: bool,
    },
    /// The exact argv (and env) an endpoint was, or would be, launched with.
    Argv {
        /// Endpoint id.
        id: String,
        /// Print the JSON envelope and nothing else.
        #[arg(long)]
        json: bool,
    },
    /// Start a local `llama-server`.
    Start(EndpointStartArgs),
    /// Stop an endpoint. The child is signalled; the record is kept.
    Stop {
        /// Endpoint id. Omit with `--all`.
        id: Option<String>,
        /// Stop every running endpoint.
        #[arg(long)]
        all: bool,
    },
    /// Restart an endpoint, optionally with new sizing.
    Restart {
        /// Endpoint id.
        id: String,
        /// New total context pool.
        #[arg(long)]
        ctx: Option<u32>,
        /// New slot count.
        #[arg(long)]
        parallel: Option<u32>,
    },
    /// Re-adopt a child that outlived its manager.
    Adopt {
        /// Endpoint id.
        id: String,
    },
    /// Stop and forget an endpoint.
    Rm {
        /// Endpoint id.
        id: String,
    },
    /// Local vLLM.
    Vllm {
        /// What to do with it.
        #[command(subcommand)]
        cmd: VllmCmd,
    },
}

/// `apexrouter endpoint start <model|recipe> …`.
#[derive(Debug, Clone, Args)]
pub struct EndpointStartArgs {
    /// Model id, name, unique prefix, or a path on disk.
    pub model: String,
    /// Bind this alias to the endpoint once it is `Ready`.
    #[arg(long, value_name = "ALIAS")]
    pub alias: Option<String>,
    /// Which discovered llama.cpp build to use. Chosen for you when omitted.
    #[arg(long, value_name = "BUILD")]
    pub build: Option<String>,
    /// `-dev` tokens, comma separated.
    #[arg(long, value_name = "D,D")]
    pub devices: Option<String>,
    /// Listen port. Allocated from `[endpoints] port_range` when omitted.
    #[arg(long)]
    pub port: Option<u16>,
    /// Total context pool.
    #[arg(long)]
    pub ctx: Option<u32>,
    /// Slots sharing that pool.
    #[arg(long)]
    pub parallel: Option<u32>,
    /// KV cache element type.
    #[arg(long, value_enum)]
    pub kv: Option<KvArg>,
    /// Layer offload: `auto`, `all`, or a layer count.
    #[arg(long, value_name = "auto|all|N")]
    pub ngl: Option<String>,
    /// `-sm` value.
    #[arg(long, value_enum)]
    pub split_mode: Option<SplitModeArg>,
    /// `--tensor-split` ratios, comma separated.
    #[arg(long, value_name = "R,R")]
    pub tensor_split: Option<String>,
    /// `-mg` value.
    #[arg(long)]
    pub main_gpu: Option<u32>,
    /// Sampling preset.
    #[arg(long, value_enum)]
    pub mode: Option<ModeArg>,
    /// Vision projector to load alongside the weights.
    #[arg(long, value_name = "PATH")]
    pub mmproj: Option<String>,
    /// Return a job id immediately instead of waiting for `Ready`.
    #[arg(long)]
    pub no_wait: bool,
    /// Start even when the fit solver says it will not fit.
    #[arg(long)]
    pub force: bool,
    /// Print the JSON envelope and nothing else.
    #[arg(long)]
    pub json: bool,
}

/// `apexrouter endpoint vllm …`.
#[derive(Debug, Clone, Subcommand)]
pub enum VllmCmd {
    /// Start a local vLLM.
    Start {
        /// HuggingFace model id.
        #[arg(long, value_name = "ID")]
        model_id: String,
        /// Tensor-parallel size.
        #[arg(long)]
        tp: Option<u32>,
        /// Max model length.
        #[arg(long)]
        ctx: Option<u32>,
        /// Listen port.
        #[arg(long)]
        port: Option<u16>,
        /// Bind this alias once it is `Ready`.
        #[arg(long, value_name = "ALIAS")]
        alias: Option<String>,
        /// Return a job id immediately.
        #[arg(long)]
        no_wait: bool,
        /// Print the JSON envelope and nothing else.
        #[arg(long)]
        json: bool,
    },
}

/// `apexrouter route …`.
#[derive(Debug, Clone, Subcommand)]
pub enum RouteCmd {
    /// Every alias and its chain.
    Ls(JsonFlag),
    /// One alias in detail.
    Show {
        /// The alias.
        alias: String,
        /// Print the JSON envelope and nothing else.
        #[arg(long)]
        json: bool,
    },
    /// Create or replace an alias.
    Set(RouteSetArgs),
    /// Delete an alias.
    Rm {
        /// The alias.
        alias: String,
    },
    /// Make an alias the default.
    Default {
        /// The alias.
        alias: String,
    },
    /// Send a 20-token probe through an alias.
    Test {
        /// The alias.
        alias: String,
        /// Print the JSON envelope and nothing else.
        #[arg(long)]
        json: bool,
    },
}

/// `apexrouter route set <alias> --target … `.
#[derive(Debug, Clone, Args)]
pub struct RouteSetArgs {
    /// The alias a client puts in `"model"`.
    pub alias: String,
    /// A target: `<backend>[:<model>]`, `tag:<tag>[:<model>]` or `glob:<pat>[:<model>]`.
    /// Repeatable; order is the chain order.
    #[arg(long = "target", value_name = "TARGET", required = true, num_args = 1..)]
    pub targets: Vec<String>,
    /// How to pick among healthy targets.
    #[arg(long, value_enum)]
    pub strategy: Option<StrategyArg>,
    /// Allow a retry to go to a different backend.
    #[arg(long)]
    pub failover: bool,
    /// Keep retries on the backend that failed.
    #[arg(long, conflicts_with = "failover")]
    pub no_failover: bool,
    /// Total attempts, including the first.
    #[arg(long)]
    pub retries: Option<u8>,
    /// A tag every candidate must carry. Repeatable.
    #[arg(long = "require-tag", value_name = "TAG")]
    pub require_tags: Vec<String>,
    /// Ceiling on the blended $/Mtok.
    #[arg(long, value_name = "USD")]
    pub max_cost: Option<f64>,
    /// Minimum advertised context.
    #[arg(long)]
    pub min_ctx: Option<u32>,
    /// Print the JSON envelope and nothing else.
    #[arg(long)]
    pub json: bool,
}

impl RouteSetArgs {
    /// `Some(true)`/`Some(false)` when the operator said so, `None` to keep what the route
    /// already has.
    pub fn failover(&self) -> Option<bool> {
        match (self.failover, self.no_failover) {
            (true, _) => Some(true),
            (_, true) => Some(false),
            _ => None,
        }
    }
}

/// `apexrouter switch …` — sugar over `route set <default alias>`.
#[derive(Debug, Clone, Subcommand)]
pub enum SwitchCmd {
    /// Point the default alias at the `together` provider.
    Together(JsonFlag),
    /// Point it at a local endpoint by model name.
    Local {
        /// Model name, id or unique prefix.
        name: String,
        /// Print the JSON envelope and nothing else.
        #[arg(long)]
        json: bool,
    },
    /// Point it at whatever is rented on vast.ai.
    VastGguf(JsonFlag),
    /// Point it at one endpoint by id.
    Endpoint {
        /// Endpoint id.
        id: String,
        /// Print the JSON envelope and nothing else.
        #[arg(long)]
        json: bool,
    },
    /// Make another alias the default.
    Alias {
        /// The alias.
        alias: String,
        /// Print the JSON envelope and nothing else.
        #[arg(long)]
        json: bool,
    },
}

// ---------------------------------------------------------------------------------------
// value enums — mirrors of protocol enums, because the protocol crate has no clap dep
// ---------------------------------------------------------------------------------------

/// `--kv`. The names are exactly the `-ctk`/`-ctv` flag values.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
#[allow(non_camel_case_types)]
pub enum KvArg {
    /// `f32`.
    #[value(name = "f32")]
    F32,
    /// `f16`.
    #[value(name = "f16")]
    F16,
    /// `bf16`.
    #[value(name = "bf16")]
    Bf16,
    /// `q8_0`.
    #[value(name = "q8_0")]
    Q8_0,
    /// `q5_1`.
    #[value(name = "q5_1")]
    Q5_1,
    /// `q5_0`.
    #[value(name = "q5_0")]
    Q5_0,
    /// `q4_1`.
    #[value(name = "q4_1")]
    Q4_1,
    /// `q4_0`.
    #[value(name = "q4_0")]
    Q4_0,
    /// `iq4_nl`.
    #[value(name = "iq4_nl")]
    Iq4Nl,
}

impl From<KvArg> for KvType {
    fn from(k: KvArg) -> KvType {
        match k {
            KvArg::F32 => KvType::F32,
            KvArg::F16 => KvType::F16,
            KvArg::Bf16 => KvType::Bf16,
            KvArg::Q8_0 => KvType::Q8_0,
            KvArg::Q5_1 => KvType::Q5_1,
            KvArg::Q5_0 => KvType::Q5_0,
            KvArg::Q4_1 => KvType::Q4_1,
            KvArg::Q4_0 => KvType::Q4_0,
            KvArg::Iq4Nl => KvType::Iq4Nl,
        }
    }
}

/// `--split-mode`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum SplitModeArg {
    /// Single device.
    None,
    /// Split by layer.
    Layer,
    /// Split by row.
    Row,
    /// Split by tensor.
    Tensor,
}

impl From<SplitModeArg> for SplitMode {
    fn from(m: SplitModeArg) -> SplitMode {
        match m {
            SplitModeArg::None => SplitMode::None,
            SplitModeArg::Layer => SplitMode::Layer,
            SplitModeArg::Row => SplitMode::Row,
            SplitModeArg::Tensor => SplitMode::Tensor,
        }
    }
}

/// `--mode`, the sampling preset.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum ModeArg {
    /// The `launch.sh` thinking preset.
    Thinking,
    /// The `launch.sh` coding preset.
    Coding,
    /// Thinking off, via `--chat-template-kwargs`.
    Nonthinking,
    /// Emit no sampling flags at all.
    Raw,
}

impl From<ModeArg> for SamplingMode {
    fn from(m: ModeArg) -> SamplingMode {
        match m {
            ModeArg::Thinking => SamplingMode::Thinking,
            ModeArg::Coding => SamplingMode::Coding,
            ModeArg::Nonthinking => SamplingMode::Nonthinking,
            ModeArg::Raw => SamplingMode::Raw,
        }
    }
}

/// `--strategy`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum StrategyArg {
    /// First healthy target in order.
    FirstHealthy,
    /// Weighted round robin.
    RoundRobin,
    /// Fewest in-flight requests.
    LeastBusy,
    /// Lowest $/Mtok.
    Cheapest,
}

impl From<StrategyArg> for Strategy {
    fn from(s: StrategyArg) -> Strategy {
        match s {
            StrategyArg::FirstHealthy => Strategy::FirstHealthy,
            StrategyArg::RoundRobin => Strategy::RoundRobin,
            StrategyArg::LeastBusy => Strategy::LeastBusy,
            StrategyArg::Cheapest => Strategy::Cheapest,
        }
    }
}

// ---------------------------------------------------------------------------------------
// classification
// ---------------------------------------------------------------------------------------

impl Command {
    /// What this verb needs in order to answer (ARCHITECTURE §7).
    pub fn need(&self) -> Need {
        match self {
            // Pure: no daemon is involved, ever.
            Command::Version(_)
            | Command::Completions(_)
            | Command::Fit(_)
            | Command::Serve(_)
            | Command::External(_) => Need::Pure,
            Command::Config { cmd } => match cmd {
                // `config init` is an offline *writer*: it takes the daemon lock itself,
                // and thereby proves no daemon is running. It never wants one started.
                ConfigCmd::Init { .. } | ConfigCmd::Edit => Need::Pure,
                ConfigCmd::Show(_) | ConfigCmd::Path(_) => Need::Pure,
            },

            // ReadState: `$STATE` can answer when no daemon is running.
            // `rig --rescan` is a refresh, not a mutation: with no daemon it simply scans
            // the machine here and now.
            Command::Status(_) | Command::Url(_) | Command::Rig(_) | Command::Models { .. } => {
                Need::ReadState
            }
            Command::Endpoint { cmd } => match cmd {
                EndpointCmd::Ls(_)
                | EndpointCmd::Show { .. }
                | EndpointCmd::Logs { .. }
                | EndpointCmd::Argv { .. } => Need::ReadState,
                _ => Need::Mutate,
            },
            Command::Route { cmd } => match cmd {
                RouteCmd::Ls(_) | RouteCmd::Show { .. } => Need::ReadState,
                _ => Need::Mutate,
            },

            // Mutate: everything that changes something.
            Command::Switch { .. } => Need::Mutate,
        }
    }

    /// Did this leaf ask for `--json`? Drives the failure shape as well as the success one.
    pub fn json(&self) -> bool {
        match self {
            Command::Status(a) => a.json,
            Command::Url(a) | Command::Version(a) => a.json,
            Command::Rig(a) => a.json,
            Command::Fit(a) => a.json,
            Command::Serve(_) | Command::Completions(_) | Command::External(_) => false,
            Command::Config { cmd } => match cmd {
                ConfigCmd::Show(a) | ConfigCmd::Path(a) => a.json,
                ConfigCmd::Init { .. } | ConfigCmd::Edit => false,
            },
            Command::Models { cmd } => match cmd {
                ModelsCmd::Ls(a) => a.json,
                ModelsCmd::Show { json, .. } => *json,
            },
            Command::Endpoint { cmd } => match cmd {
                EndpointCmd::Ls(a) => a.json,
                EndpointCmd::Show { json, .. }
                | EndpointCmd::Argv { json, .. }
                | EndpointCmd::Vllm {
                    cmd: VllmCmd::Start { json, .. },
                } => *json,
                EndpointCmd::Start(a) => a.json,
                EndpointCmd::Logs { .. }
                | EndpointCmd::Stop { .. }
                | EndpointCmd::Restart { .. }
                | EndpointCmd::Adopt { .. }
                | EndpointCmd::Rm { .. } => false,
            },
            Command::Route { cmd } => match cmd {
                RouteCmd::Ls(a) => a.json,
                RouteCmd::Show { json, .. } | RouteCmd::Test { json, .. } => *json,
                RouteCmd::Set(a) => a.json,
                RouteCmd::Rm { .. } | RouteCmd::Default { .. } => false,
            },
            Command::Switch { cmd } => match cmd {
                SwitchCmd::Together(a) | SwitchCmd::VastGguf(a) => a.json,
                SwitchCmd::Local { json, .. }
                | SwitchCmd::Endpoint { json, .. }
                | SwitchCmd::Alias { json, .. } => *json,
            },
        }
    }
}

/// Split a `"a,b,c"` argument into trimmed, non-empty parts.
pub fn split_list(v: &str) -> Vec<String> {
    v.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_tree_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn a_bare_invocation_is_status() {
        let cli = Cli::parse_from(["apexrouter"]);
        assert!(matches!(cli.verb(), Command::Status(_)));
        assert_eq!(cli.verb().need(), Need::ReadState);
        assert!(!cli.verb().json());
    }

    #[test]
    fn global_flags_are_accepted_after_the_subcommand_too() {
        let cli = Cli::parse_from([
            "apexrouter",
            "status",
            "--json",
            "--home",
            "/tmp/x",
            "--no-autostart",
        ]);
        assert_eq!(cli.home.as_deref(), Some(std::path::Path::new("/tmp/x")));
        assert!(!cli.autostart());
        assert!(cli.verb().json());
    }

    #[test]
    fn the_documented_surface_parses() {
        // ARCHITECTURE §7, the lines this unit owns.
        let cases: Vec<Vec<&str>> = vec![
            vec!["apexrouter", "status", "--json", "--watch"],
            vec!["apexrouter", "serve", "--foreground"],
            vec!["apexrouter", "serve", "--stop"],
            vec!["apexrouter", "serve", "--detach"],
            vec![
                "apexrouter",
                "serve",
                "--allow-remote",
                "--token-env",
                "APEXROUTER_TOKEN",
            ],
            vec!["apexrouter", "url", "--json"],
            vec!["apexrouter", "version"],
            vec!["apexrouter", "completions", "bash"],
            vec!["apexrouter", "config", "show", "--json"],
            vec!["apexrouter", "config", "path"],
            vec!["apexrouter", "config", "init"],
            vec!["apexrouter", "rig", "--json"],
            vec!["apexrouter", "models", "ls", "--json"],
            vec!["apexrouter", "models", "show", "Carnice-9b-Q6_K"],
            vec![
                "apexrouter",
                "fit",
                "Carnice-9b-Q6_K",
                "--devices",
                "Vulkan0,Vulkan1",
                "--ctx",
                "32768",
                "--parallel",
                "2",
                "--kv",
                "q8_0",
                "--split-mode",
                "layer",
                "--tensor-split",
                "3,1",
                "--main-gpu",
                "0",
                "--json",
            ],
            vec!["apexrouter", "endpoint", "ls", "--json"],
            vec![
                "apexrouter",
                "endpoint",
                "logs",
                "local-carnice",
                "-f",
                "-n",
                "200",
            ],
            vec!["apexrouter", "endpoint", "argv", "local-carnice"],
            vec![
                "apexrouter",
                "endpoint",
                "start",
                "Carnice-9b-Q6_K",
                "--alias",
                "auto",
                "--ctx",
                "32768",
                "--ngl",
                "all",
                "--mode",
                "thinking",
                "--no-wait",
                "--force",
            ],
            vec!["apexrouter", "endpoint", "stop", "local-carnice"],
            vec!["apexrouter", "endpoint", "stop", "--all"],
            vec![
                "apexrouter",
                "endpoint",
                "restart",
                "local-carnice",
                "--ctx",
                "8192",
            ],
            vec!["apexrouter", "endpoint", "adopt", "local-carnice"],
            vec!["apexrouter", "endpoint", "rm", "local-carnice"],
            vec![
                "apexrouter",
                "endpoint",
                "vllm",
                "start",
                "--model-id",
                "Qwen/Qwen3",
                "--tp",
                "2",
            ],
            vec!["apexrouter", "route", "ls"],
            vec![
                "apexrouter",
                "route",
                "set",
                "auto",
                "--target",
                "local-carnice",
                "--target",
                "tag:rented",
                "--strategy",
                "first-healthy",
                "--failover",
                "--retries",
                "3",
                "--require-tag",
                "local",
                "--max-cost",
                "0.5",
                "--min-ctx",
                "8192",
            ],
            vec!["apexrouter", "route", "default", "auto"],
            vec!["apexrouter", "route", "test", "auto", "--json"],
            vec!["apexrouter", "route", "rm", "auto"],
            vec!["apexrouter", "switch", "together"],
            vec!["apexrouter", "switch", "local", "carnice"],
            vec!["apexrouter", "switch", "vast-gguf"],
            vec!["apexrouter", "switch", "endpoint", "local-carnice"],
            vec!["apexrouter", "switch", "alias", "fast"],
        ];
        for case in cases {
            Cli::try_parse_from(&case).unwrap_or_else(|e| panic!("{case:?} did not parse: {e}"));
        }
    }

    #[test]
    fn an_unimplemented_verb_lands_in_external_rather_than_a_clap_error() {
        let cli = Cli::parse_from(["apexrouter", "up", "Carnice-9b-Q6_K", "--alias", "auto"]);
        match cli.verb() {
            Command::External(args) => assert_eq!(args[0], "up"),
            other => panic!("expected External, got {other:?}"),
        }
    }

    #[test]
    fn need_and_json_agree_with_the_architecture_table() {
        let need = |args: &[&str]| Cli::parse_from(args).verb().need();
        assert_eq!(need(&["apexrouter", "version"]), Need::Pure);
        assert_eq!(need(&["apexrouter", "fit", "x"]), Need::Pure);
        assert_eq!(need(&["apexrouter", "completions", "zsh"]), Need::Pure);
        assert_eq!(need(&["apexrouter", "config", "show"]), Need::Pure);
        assert_eq!(need(&["apexrouter", "status"]), Need::ReadState);
        assert_eq!(need(&["apexrouter", "rig"]), Need::ReadState);
        assert_eq!(need(&["apexrouter", "models", "ls"]), Need::ReadState);
        assert_eq!(need(&["apexrouter", "endpoint", "ls"]), Need::ReadState);
        assert_eq!(need(&["apexrouter", "route", "ls"]), Need::ReadState);
        assert_eq!(need(&["apexrouter", "endpoint", "stop", "x"]), Need::Mutate);
        assert_eq!(
            need(&["apexrouter", "route", "default", "auto"]),
            Need::Mutate
        );
        assert_eq!(need(&["apexrouter", "switch", "together"]), Need::Mutate);

        let json = |args: &[&str]| Cli::parse_from(args).verb().json();
        assert!(json(&["apexrouter", "config", "show", "--json"]));
        assert!(!json(&["apexrouter", "config", "show"]));
        assert!(json(&["apexrouter", "endpoint", "ls", "--json"]));
    }

    #[test]
    fn value_enums_map_onto_the_protocol_spellings() {
        assert_eq!(KvType::from(KvArg::Q8_0).as_flag(), "q8_0");
        assert_eq!(SplitMode::from(SplitModeArg::Layer), SplitMode::Layer);
        assert_eq!(SamplingMode::from(ModeArg::Coding), SamplingMode::Coding);
        assert_eq!(Strategy::from(StrategyArg::LeastBusy), Strategy::LeastBusy);
        let parsed = Cli::parse_from(["apexrouter", "fit", "m", "--kv", "iq4_nl"]);
        match parsed.verb() {
            Command::Fit(a) => assert_eq!(a.kv, Some(KvArg::Iq4Nl)),
            _ => panic!("expected fit"),
        }
    }

    #[test]
    fn failover_is_tri_state_so_route_set_can_leave_it_alone() {
        let args = |v: &[&str]| match Cli::parse_from(v).verb() {
            Command::Route {
                cmd: RouteCmd::Set(a),
            } => a,
            _ => panic!("expected route set"),
        };
        let base = ["apexrouter", "route", "set", "auto", "--target", "b"];
        assert_eq!(args(&base).failover(), None);
        let mut on = base.to_vec();
        on.push("--failover");
        assert_eq!(args(&on).failover(), Some(true));
        let mut off = base.to_vec();
        off.push("--no-failover");
        assert_eq!(args(&off).failover(), Some(false));
    }

    #[test]
    fn split_list_trims_and_drops_empties() {
        assert_eq!(split_list("Vulkan0, Vulkan1 ,"), ["Vulkan0", "Vulkan1"]);
        assert!(split_list("  ").is_empty());
    }
}
