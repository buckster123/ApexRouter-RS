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
use apexrouter_core::usage::GroupBy;
use apexrouter_protocol::{GeoFilter, KvType, SamplingMode, SplitMode, Strategy, SwapMode};
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
    /// Pull the installed checkout and re-run its installer.
    Update(UpdateArgs),
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
    /// The one-command happy path: resolve, start, bind, print the URL.
    Up(UpArgs),
    /// Move an alias onto something else, with the mode chosen for you.
    Swap(SwapArgs),
    /// Ensure a daemon and open the web UI in a browser.
    Open,
    /// Print the two shell exports an OpenAI client needs.
    Env(JsonFlag),
    /// Upstreams in the routing table.
    Backend {
        /// What to do with them.
        #[command(subcommand)]
        cmd: BackendCmd,
    },
    /// Saved launch plans.
    Recipe {
        /// What to do with them.
        #[command(subcommand)]
        cmd: RecipeCmd,
    },
    /// Saved vast.ai search profiles.
    Profile {
        /// What to do with them.
        #[command(subcommand)]
        cmd: ProfileCmd,
    },
    /// Managed providers and where their credentials live.
    Provider {
        /// What to do with them.
        #[command(subcommand)]
        cmd: ProviderCmd,
    },
    /// vast.ai — the verbs that spend money.
    Vast {
        /// What to do.
        #[command(subcommand)]
        cmd: VastCmd,
    },
    /// Supervised `ssh -L` tunnels to rented boxes.
    Tunnel {
        /// What to do.
        #[command(subcommand)]
        cmd: TunnelCmd,
    },
    /// Spend approvals waiting for a human.
    Approvals {
        /// What to do.
        #[command(subcommand)]
        cmd: ApprovalsCmd,
    },
    /// HuggingFace search and download.
    Hf {
        /// What to do.
        #[command(subcommand)]
        cmd: HfCmd,
    },
    /// Tokens and cost over a window.
    Usage(UsageArgs),
    /// One prompt against several aliases, side by side.
    Compare(CompareArgs),
    /// The four native smoke probes.
    Smoke(SmokeArgs),
    /// The check registry, with a fix line per row.
    Doctor(DoctorArgs),
    /// Import `~/.vastai-gguf` and a LocalRouter checkout.
    Migrate(MigrateArgs),
    /// Bearer tokens for a non-loopback bind.
    Token {
        /// What to do.
        #[command(subcommand)]
        cmd: TokenCmd,
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
    /// Does the file parse, what does this build ignore, and what would be bound.
    Validate(JsonFlag),
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
// S-08 — the remainder of the surface in ARCHITECTURE §7
// ---------------------------------------------------------------------------------------

/// `apexrouter up <model|recipe> [--alias A] [--yes] …`.
#[derive(Debug, Clone, Args)]
pub struct UpArgs {
    /// Recipe id, model id, unique model prefix, or a path on disk — resolved in that order.
    pub what: String,
    /// Bind this alias once the thing is `Ready`. Defaults to the default alias.
    #[arg(long, value_name = "ALIAS")]
    pub alias: Option<String>,
    /// Total context pool.
    #[arg(long)]
    pub ctx: Option<u32>,
    /// Slots sharing that pool.
    #[arg(long)]
    pub parallel: Option<u32>,
    /// `-dev` tokens, comma separated.
    #[arg(long, value_name = "D,D")]
    pub devices: Option<String>,
    /// Sampling preset.
    #[arg(long, value_enum)]
    pub mode: Option<ModeArg>,
    /// Return a job id immediately instead of waiting for `Ready`.
    #[arg(long)]
    pub no_wait: bool,
    /// Start even when the fit solver says it will not fit.
    #[arg(long)]
    pub force: bool,
    /// Required before anything that costs money runs.
    #[arg(long)]
    pub yes: bool,
    /// Print the JSON envelope and nothing else.
    #[arg(long)]
    pub json: bool,
}

/// `apexrouter swap <alias> --to <model|recipe|backend-id> [--mode hot|sequential]`.
#[derive(Debug, Clone, Args)]
pub struct SwapArgs {
    /// The alias to move.
    pub alias: String,
    /// Where to move it: a backend id, a recipe id, or a model.
    #[arg(long, value_name = "TARGET")]
    pub to: String,
    /// `hot` keeps both up; `sequential` frees the VRAM first. Chosen for you when omitted.
    #[arg(long, value_enum)]
    pub mode: Option<SwapModeArg>,
    /// Print the JSON envelope and nothing else.
    #[arg(long)]
    pub json: bool,
}

/// `apexrouter backend …`.
#[derive(Debug, Clone, Subcommand)]
pub enum BackendCmd {
    /// Every upstream in the table.
    Ls(JsonFlag),
    /// One upstream in detail.
    Show {
        /// Backend id.
        id: String,
        /// Print the JSON envelope and nothing else.
        #[arg(long)]
        json: bool,
    },
    /// Register a URL as a backend.
    Add(BackendAddArgs),
    /// Let it take traffic again.
    Enable {
        /// Backend id.
        id: String,
    },
    /// Take it out of the table without forgetting it.
    Disable {
        /// Backend id.
        id: String,
    },
    /// Finish in-flight requests, then stop routing to it.
    Drain {
        /// Backend id.
        id: String,
    },
    /// Probe it now rather than waiting for the poller.
    Probe {
        /// Backend id.
        id: String,
        /// Print the JSON envelope and nothing else.
        #[arg(long)]
        json: bool,
    },
    /// Forget it.
    Rm {
        /// Backend id.
        id: String,
    },
}

/// `apexrouter backend add <url> …`.
#[derive(Debug, Clone, Args)]
pub struct BackendAddArgs {
    /// Base URL, with or without a trailing `/v1`.
    pub url: String,
    /// Human label.
    #[arg(long, value_name = "L")]
    pub label: Option<String>,
    /// A tag, repeatable. Tags are what `tag:` route targets select on.
    #[arg(long = "tag", value_name = "T")]
    pub tags: Vec<String>,
    /// Name the environment variable holding this backend's key.
    #[arg(long, value_name = "VAR")]
    pub key_env: Option<String>,
    /// Models the backend serves, comma separated, when it will not list them itself.
    #[arg(long, value_name = "M,M")]
    pub models: Option<String>,
    /// Print the JSON envelope and nothing else.
    #[arg(long)]
    pub json: bool,
}

/// `apexrouter recipe …`.
#[derive(Debug, Clone, Subcommand)]
pub enum RecipeCmd {
    /// Every saved recipe.
    Ls(JsonFlag),
    /// One recipe in detail.
    Show {
        /// Recipe id.
        id: String,
        /// Print the JSON envelope and nothing else.
        #[arg(long)]
        json: bool,
    },
    /// Save a running endpoint as a recipe.
    New {
        /// Which endpoint to snapshot. Required: a recipe is never invented from nothing.
        #[arg(long, value_name = "ID")]
        from_endpoint: String,
        /// Open the result in `$VISUAL`/`$EDITOR` afterwards.
        #[arg(long)]
        edit: bool,
        /// Print the JSON envelope and nothing else.
        #[arg(long)]
        json: bool,
    },
    /// Edit a recipe as JSON in `$VISUAL`/`$EDITOR`.
    Edit {
        /// Recipe id.
        id: String,
    },
    /// Delete a recipe.
    Rm {
        /// Recipe id.
        id: String,
    },
    /// Check that everything a recipe names still exists.
    Validate {
        /// Recipe id.
        id: String,
        /// Print the JSON envelope and nothing else.
        #[arg(long)]
        json: bool,
    },
    /// Instantiate a recipe.
    Run {
        /// Recipe id.
        id: String,
        /// Bind this alias once it is `Ready`.
        #[arg(long, value_name = "ALIAS")]
        alias: Option<String>,
        /// Return a job id immediately.
        #[arg(long)]
        no_wait: bool,
        /// Required when the recipe rents hardware.
        #[arg(long)]
        yes: bool,
        /// Print the JSON envelope and nothing else.
        #[arg(long)]
        json: bool,
    },
}

/// `apexrouter profile …`.
#[derive(Debug, Clone, Subcommand)]
pub enum ProfileCmd {
    /// Every saved search profile.
    Ls(JsonFlag),
    /// One profile in detail.
    Show {
        /// Profile id.
        id: String,
        /// Print the JSON envelope and nothing else.
        #[arg(long)]
        json: bool,
    },
    /// Create a profile.
    New(ProfileNewArgs),
    /// Edit a profile as JSON in `$VISUAL`/`$EDITOR`.
    Edit {
        /// Profile id.
        id: String,
    },
    /// Delete a profile.
    Rm {
        /// Profile id.
        id: String,
    },
}

/// `apexrouter profile new …`.
#[derive(Debug, Clone, Args)]
pub struct ProfileNewArgs {
    /// Human label. The id is derived from it.
    pub label: String,
    /// Exact GPU names from the live vocabulary, comma separated.
    #[arg(long, value_name = "N,N")]
    pub gpu: Option<String>,
    /// Minimum GPU count.
    #[arg(long, default_value_t = 1)]
    pub num_gpus_min: u32,
    /// Maximum GPU count.
    #[arg(long, default_value_t = 1)]
    pub num_gpus_max: u32,
    /// Ceiling on `dph_total`.
    #[arg(long, value_name = "USD")]
    pub max_price: Option<f64>,
    /// `any`, `eu`, `eu-nordic`, `us`, or a comma-separated ISO-3166 alpha-2 list.
    #[arg(long, value_name = "GEO")]
    pub geo: Option<String>,
    /// Minimum `reliability2`.
    #[arg(long, default_value_t = 0.98)]
    pub min_reliability: f32,
    /// Minimum inbound bandwidth, Mbps.
    #[arg(long, default_value_t = 200)]
    pub min_inet_down: u32,
    /// Minimum disk, GB.
    #[arg(long, default_value_t = 60)]
    pub min_disk_gb: u32,
    /// Print the JSON envelope and nothing else.
    #[arg(long)]
    pub json: bool,
}

/// `apexrouter provider …`.
#[derive(Debug, Clone, Subcommand)]
pub enum ProviderCmd {
    /// Every configured provider and where its credential lives.
    Ls(JsonFlag),
    /// One provider in detail.
    Show {
        /// Provider id.
        id: String,
        /// Print the JSON envelope and nothing else.
        #[arg(long)]
        json: bool,
    },
    /// Set the base URL, the key, or where the key lives.
    Set(ProviderSetArgs),
    /// Connection probe, plus an optional 16-token completion.
    Test {
        /// Provider id.
        id: String,
        /// Also send a completion, not just a connection probe.
        #[arg(long)]
        completion: bool,
        /// Which model the completion should name.
        #[arg(long, value_name = "M")]
        model: Option<String>,
        /// Print the JSON envelope and nothing else.
        #[arg(long)]
        json: bool,
    },
    /// The live catalogue, grouped by org.
    Models {
        /// Provider id.
        id: String,
        /// Show only one org.
        #[arg(long, value_name = "O")]
        org: Option<String>,
        /// Print the JSON envelope and nothing else.
        #[arg(long)]
        json: bool,
    },
}

/// `apexrouter provider set <id> …`.
#[derive(Debug, Clone, Args)]
pub struct ProviderSetArgs {
    /// Provider id.
    pub id: String,
    /// The API root, stored **without** a trailing `/v1` and never rewritten.
    #[arg(long, value_name = "U")]
    pub base_url: Option<String>,
    /// Name the environment variable holding the key.
    #[arg(long, value_name = "VAR")]
    pub key_env: Option<String>,
    /// Name a file holding the key.
    #[arg(long, value_name = "P")]
    pub key_file: Option<PathBuf>,
    /// Read the key from stdin. The only way a typed key reaches `credentials.toml`.
    #[arg(long)]
    pub key_stdin: bool,
    /// Print the JSON envelope and nothing else.
    #[arg(long)]
    pub json: bool,
}

/// `apexrouter vast …`.
#[derive(Debug, Clone, Subcommand)]
pub enum VastCmd {
    /// Credit, balance and whether the account can pay.
    Account(JsonFlag),
    /// Search the market.
    Offers(VastOffersArgs),
    /// The LIVE GPU-name vocabulary. Never a hardcoded enum.
    GpuNames(JsonFlag),
    /// Rent a box. **Spends money.**
    Rent(VastRentArgs),
    /// Rented instances, from the ledger when the daemon is down.
    Ls {
        /// Show only rows that are billing with no live record.
        #[arg(long)]
        orphans: bool,
        /// Print the JSON envelope and nothing else.
        #[arg(long)]
        json: bool,
    },
    /// Follow one instance's boot state machine until it is serving or dead.
    Watch {
        /// Instance id.
        id: u64,
    },
    /// The instance's container log.
    Log {
        /// Instance id.
        id: u64,
        /// Keep printing as it grows.
        #[arg(short = 'f', long)]
        follow: bool,
    },
    /// The four SSH probes plus an RX sample.
    Diagnose {
        /// Instance id.
        id: u64,
        /// Print the JSON envelope and nothing else.
        #[arg(long)]
        json: bool,
    },
    /// Recover a stalled model download in place.
    RestartDownload {
        /// Instance id.
        id: u64,
    },
    /// Destroy an instance. **Stops the billing; verifies before forgetting.**
    Destroy {
        /// Instance id. Omit with `--all`.
        id: Option<u64>,
        /// Destroy every instance the ledger believes is live.
        #[arg(long)]
        all: bool,
        /// Required. There is no interactive prompt.
        #[arg(long)]
        yes: bool,
    },
}

/// `apexrouter vast offers …`.
#[derive(Debug, Clone, Default, Args)]
pub struct VastOffersArgs {
    /// Start from a saved search profile.
    #[arg(long, value_name = "P")]
    pub profile: Option<String>,
    /// An exact GPU name from the live vocabulary, repeatable.
    #[arg(long = "gpu", value_name = "NAME")]
    pub gpus: Vec<String>,
    /// Exactly this many GPUs.
    #[arg(long)]
    pub num_gpus: Option<u32>,
    /// `any`, `eu`, `eu-nordic`, `us`, or a comma-separated ISO-3166 alpha-2 list.
    #[arg(long, value_name = "GEO")]
    pub geo: Option<String>,
    /// Ceiling on `dph_total`.
    #[arg(long, value_name = "USD")]
    pub max_price: Option<f64>,
    /// Row cap.
    #[arg(long, default_value_t = 20)]
    pub limit: u32,
    /// Print the JSON envelope and nothing else.
    #[arg(long)]
    pub json: bool,
}

/// `apexrouter vast rent …`. Every field the money gate reads.
#[derive(Debug, Clone, Args)]
pub struct VastRentArgs {
    /// The offer to take. Omit with `--auto`.
    pub offer_id: Option<u64>,
    /// Take the cheapest offer the profile matches.
    #[arg(long, conflicts_with = "offer_id")]
    pub auto: bool,
    /// Which search profile describes the hardware.
    #[arg(long, value_name = "P")]
    pub profile: String,
    /// HF repo for a GGUF launch.
    #[arg(long, value_name = "R")]
    pub model_repo: Option<String>,
    /// Which quant within that repo.
    #[arg(long, value_name = "Q")]
    pub quant: Option<String>,
    /// HF model id for a vLLM launch.
    #[arg(long, value_name = "M")]
    pub model_id: Option<String>,
    /// Total context pool.
    #[arg(long)]
    pub ctx: Option<u32>,
    /// Disk to request, GB.
    #[arg(long, default_value_t = 120)]
    pub disk_gb: u32,
    /// The ceiling this approval carries. **Required.**
    #[arg(long, value_name = "USD")]
    pub max_hourly: f64,
    /// Bind this alias once the box is serving.
    #[arg(long, value_name = "ALIAS")]
    pub alias: Option<String>,
    /// Required. There is no interactive prompt.
    #[arg(long)]
    pub yes: bool,
    /// Return a job id immediately instead of streaming `BootPhase`.
    #[arg(long)]
    pub no_wait: bool,
    /// Print the quote and stop. Nothing is rented, nothing is reserved.
    #[arg(long)]
    pub dry_run: bool,
    /// Print the JSON envelope and nothing else.
    #[arg(long)]
    pub json: bool,
}

/// `apexrouter tunnel …`.
#[derive(Debug, Clone, Subcommand)]
pub enum TunnelCmd {
    /// Open a supervised tunnel to an instance.
    Up {
        /// Instance id.
        instance_id: u64,
        /// Print the JSON envelope and nothing else.
        #[arg(long)]
        json: bool,
    },
    /// Close one tunnel, or every tunnel.
    Down {
        /// Instance id. Omit to close them all.
        id: Option<u64>,
    },
    /// Every tunnel and whether it is up.
    Status(JsonFlag),
}

/// `apexrouter approvals …`.
#[derive(Debug, Clone, Subcommand)]
pub enum ApprovalsCmd {
    /// Every request waiting for a human.
    Ls(JsonFlag),
    /// Approve one. **This is the money decision.**
    Grant {
        /// Approval id.
        id: String,
        /// Required. Granting is spending.
        #[arg(long)]
        yes: bool,
    },
    /// Refuse one.
    Deny {
        /// Approval id.
        id: String,
    },
}

/// `apexrouter hf …`.
#[derive(Debug, Clone, Subcommand)]
pub enum HfCmd {
    /// Search GGUF repos.
    Search {
        /// The query. Empty lists the most-downloaded GGUF repos.
        #[arg(default_value = "")]
        query: String,
        /// Row cap.
        #[arg(long, default_value_t = 20)]
        limit: u32,
        /// Print the JSON envelope and nothing else.
        #[arg(long)]
        json: bool,
    },
    /// Authoritative per-file sizes, grouped by quant.
    Files {
        /// `owner/repo`.
        repo: String,
        /// Print the JSON envelope and nothing else.
        #[arg(long)]
        json: bool,
    },
    /// Download a quant, resume-capable and size-verified.
    Get {
        /// `owner/repo`.
        repo: String,
        /// A quant label from the grouped listing.
        #[arg(long, value_name = "Q")]
        quant: Option<String>,
        /// An exact repo-relative path, repeatable. Wins over `--quant`.
        #[arg(long = "file", value_name = "F")]
        files: Vec<String>,
        /// Also fetch the vision projector that pairs with the group.
        #[arg(long)]
        mmproj: bool,
        /// Where to put it.
        #[arg(long, value_name = "DIR")]
        dest: Option<String>,
        /// Return a job id immediately instead of following the download.
        #[arg(long)]
        no_wait: bool,
        /// Print the JSON envelope and nothing else.
        #[arg(long)]
        json: bool,
    },
}

/// `apexrouter usage …`.
#[derive(Debug, Clone, Default, Args)]
pub struct UsageArgs {
    /// `all`, a duration (`30m`, `24h`, `7d`, `4w`), or an absolute timestamp.
    #[arg(long, value_name = "WINDOW", default_value = "24h")]
    pub since: String,
    /// How to bucket.
    #[arg(long, value_enum, default_value_t = GroupByArg::Provider)]
    pub by: GroupByArg,
    /// Print the JSON envelope and nothing else.
    #[arg(long)]
    pub json: bool,
}

/// `apexrouter compare …`.
#[derive(Debug, Clone, Args)]
pub struct CompareArgs {
    /// An alias to include, repeatable. At least two is the point.
    #[arg(long = "alias", value_name = "A", required = true, num_args = 1..)]
    pub aliases: Vec<String>,
    /// The one prompt every alias gets.
    #[arg(long, value_name = "P")]
    pub prompt: String,
    /// Generation budget per alias.
    #[arg(long)]
    pub max_tokens: Option<u32>,
    /// Print the JSON envelope and nothing else.
    #[arg(long)]
    pub json: bool,
}

/// `apexrouter smoke …`.
#[derive(Debug, Clone, Default, Args)]
pub struct SmokeArgs {
    /// Smoke whatever this alias resolves to right now.
    #[arg(long, value_name = "A")]
    pub alias: Option<String>,
    /// Smoke a URL directly, with or without a trailing `/v1`.
    #[arg(long, value_name = "URL", conflicts_with = "alias")]
    pub base_url: Option<String>,
    /// Override the model id the probes ask for.
    #[arg(long, value_name = "M")]
    pub model: Option<String>,
    /// Print the JSON envelope and nothing else.
    #[arg(long)]
    pub json: bool,
}

/// `apexrouter doctor …`.
#[derive(Debug, Clone, Default, Args)]
pub struct DoctorArgs {
    /// An exact check id, a namespace, or any fragment. Separators are ignored.
    #[arg(long, value_name = "CHECK")]
    pub only: Option<String>,
    /// Print the JSON envelope and nothing else.
    #[arg(long)]
    pub json: bool,
}

/// `apexrouter migrate …`.
#[derive(Debug, Clone, Default, Args)]
pub struct MigrateArgs {
    /// Print the plan and write nothing. The default.
    #[arg(long)]
    pub dry_run: bool,
    /// Actually import. Without it, `migrate` only ever prints.
    #[arg(long, conflicts_with = "dry_run")]
    pub apply: bool,
    /// The legacy state directory. Must be named `.vastai-gguf`.
    #[arg(long, value_name = "DIR")]
    pub from: Option<PathBuf>,
    /// The LocalRouter checkout.
    #[arg(long, value_name = "PATH")]
    pub localrouter: Option<PathBuf>,
    /// Strike rows out of the plan before it is printed or applied. A pattern that
    /// exactly names a category (`recipe`, `known_fork`, …) strikes that category;
    /// anything else strikes rows whose FROM contains it. Repeatable; a pattern that
    /// matches nothing is an error.
    #[arg(long, value_name = "PATTERN")]
    pub skip: Vec<String>,
    /// Print the JSON envelope and nothing else.
    #[arg(long)]
    pub json: bool,
}

/// `apexrouter update [--no-pull]`.
#[derive(Debug, Clone, Default, Args)]
pub struct UpdateArgs {
    /// Rebuild and reinstall whatever the checkout already holds, without pulling.
    #[arg(long)]
    pub no_pull: bool,
}

/// `apexrouter token …`.
#[derive(Debug, Clone, Subcommand)]
pub enum TokenCmd {
    /// Mint one. Shown **once**, never stored by this command.
    Create {
        /// Which scope the operator intends it to carry.
        #[arg(long, value_enum, default_value_t = ScopeArg::Admin)]
        scope: ScopeArg,
        /// Print the JSON envelope and nothing else.
        #[arg(long)]
        json: bool,
    },
    /// Where the daemon looks for a token, and whether it finds one.
    Ls(JsonFlag),
    /// How to take a token out of service.
    Revoke {
        /// The env var name, or the token's first characters.
        id: String,
    },
}

// ---------------------------------------------------------------------------------------
// value enums — mirrors of protocol enums, because the protocol crate has no clap dep
// ---------------------------------------------------------------------------------------

/// `--mode` on `swap`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum SwapModeArg {
    /// Bring the new one up first. Needs VRAM for both.
    Hot,
    /// Stop the old one first. The only option when VRAM is tight.
    Sequential,
}

impl From<SwapModeArg> for SwapMode {
    fn from(m: SwapModeArg) -> SwapMode {
        match m {
            SwapModeArg::Hot => SwapMode::Hot,
            SwapModeArg::Sequential => SwapMode::Sequential,
        }
    }
}

/// `--by` on `usage`. Defaults to `provider`, which is what the legacy `cost.py` printed.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum GroupByArg {
    /// By provider id.
    #[default]
    Provider,
    /// By upstream model id.
    Model,
    /// By backend id.
    Backend,
    /// By alias.
    Alias,
    /// By calendar day, UTC.
    Day,
}

impl From<GroupByArg> for GroupBy {
    fn from(g: GroupByArg) -> GroupBy {
        match g {
            GroupByArg::Provider => GroupBy::Provider,
            GroupByArg::Model => GroupBy::Model,
            GroupByArg::Backend => GroupBy::Backend,
            GroupByArg::Alias => GroupBy::Alias,
            GroupByArg::Day => GroupBy::Day,
        }
    }
}

impl GroupByArg {
    /// The spelling `GET /v1/usage?by=` expects.
    pub fn as_query(self) -> &'static str {
        match self {
            GroupByArg::Provider => "provider",
            GroupByArg::Model => "model",
            GroupByArg::Backend => "backend",
            GroupByArg::Alias => "alias",
            GroupByArg::Day => "day",
        }
    }
}

/// `--scope` on `token create`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum ScopeArg {
    /// Read-only.
    Read,
    /// Read and mutate.
    Write,
    /// Everything, including `/v1/tokens*` and `/v1/shutdown`.
    Admin,
}

impl ScopeArg {
    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            ScopeArg::Read => "read",
            ScopeArg::Write => "write",
            ScopeArg::Admin => "admin",
        }
    }
}

/// Parse a `--geo` argument into the protocol filter.
///
/// `any`, `eu`, `eu-nordic`/`eu_nordic`/`nordic`, `us`, or a comma-separated ISO-3166
/// alpha-2 list. The list form is what makes the filter honest about a country the four
/// named groups do not cover.
///
/// # Errors
/// When a code in the list is not two ASCII letters — a typo there silently rents in the
/// wrong hemisphere otherwise.
pub fn parse_geo(s: &str) -> anyhow::Result<GeoFilter> {
    match s.trim().to_lowercase().as_str() {
        "" | "any" => Ok(GeoFilter::Any),
        "eu" => Ok(GeoFilter::Eu),
        "eu-nordic" | "eu_nordic" | "nordic" => Ok(GeoFilter::EuNordic),
        "us" => Ok(GeoFilter::Us),
        _ => {
            let codes = split_list(s);
            for c in &codes {
                if c.len() != 2 || !c.chars().all(|ch| ch.is_ascii_alphabetic()) {
                    anyhow::bail!(
                        "`{c}` is not an ISO-3166 alpha-2 country code — --geo takes \
                         `any`, `eu`, `eu-nordic`, `us`, or a list like `CZ,PL,DE`"
                    );
                }
            }
            Ok(GeoFilter::Codes(
                codes.iter().map(|c| c.to_uppercase()).collect(),
            ))
        }
    }
}

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
            // `migrate` is an offline *writer*, like `config init`: it edits `config.toml`,
            // the catalog and the ledger, so autostarting a daemon that would then be
            // holding a stale copy of all three is exactly the wrong reflex.
            Command::Migrate(_) => Need::Pure,
            // `update` hands over to the recorded installer, which owns the daemon
            // question itself (its verify step proves the serving pid runs the new inode).
            Command::Update(_) => Need::Pure,
            // A token is minted here and shown once; no daemon has to exist for that.
            Command::Token { .. } => Need::Pure,
            // Every check either reads this machine or reaches the network, and §7 puts
            // `doctor` in ReadState so it answers with nothing running.
            Command::Doctor(_) | Command::Usage(_) | Command::Env(_) => Need::ReadState,
            Command::Recipe { cmd } => match cmd {
                RecipeCmd::Ls(_) | RecipeCmd::Show { .. } => Need::ReadState,
                _ => Need::Mutate,
            },
            Command::Profile { cmd } => match cmd {
                ProfileCmd::Ls(_) | ProfileCmd::Show { .. } => Need::ReadState,
                _ => Need::Mutate,
            },
            Command::Backend { cmd } => match cmd {
                BackendCmd::Ls(_) | BackendCmd::Show { .. } => Need::ReadState,
                _ => Need::Mutate,
            },
            Command::Tunnel { cmd } => match cmd {
                TunnelCmd::Status(_) => Need::ReadState,
                _ => Need::Mutate,
            },
            Command::Vast { cmd } => match cmd {
                // The cached listing: the ledger is a file, and a box that is billing must
                // stay visible when the daemon is not running.
                VastCmd::Ls { .. } => Need::ReadState,
                _ => Need::Mutate,
            },
            Command::Up(_)
            | Command::Swap(_)
            | Command::Open
            | Command::Provider { .. }
            | Command::Approvals { .. }
            | Command::Hf { .. }
            | Command::Compare(_)
            | Command::Smoke(_) => Need::Mutate,
            Command::Config { cmd } => match cmd {
                // `config init` is an offline *writer*: it takes the daemon lock itself,
                // and thereby proves no daemon is running. It never wants one started.
                ConfigCmd::Init { .. } | ConfigCmd::Edit => Need::Pure,
                ConfigCmd::Show(_) | ConfigCmd::Path(_) | ConfigCmd::Validate(_) => Need::Pure,
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
            Command::Serve(_)
            | Command::Update(_)
            | Command::Completions(_)
            | Command::External(_) => false,
            Command::Config { cmd } => match cmd {
                ConfigCmd::Show(a) | ConfigCmd::Path(a) | ConfigCmd::Validate(a) => a.json,
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
            Command::Up(a) => a.json,
            Command::Swap(a) => a.json,
            Command::Open => false,
            Command::Env(a) => a.json,
            Command::Usage(a) => a.json,
            Command::Compare(a) => a.json,
            Command::Smoke(a) => a.json,
            Command::Doctor(a) => a.json,
            Command::Migrate(a) => a.json,
            Command::Backend { cmd } => match cmd {
                BackendCmd::Ls(a) => a.json,
                BackendCmd::Show { json, .. } | BackendCmd::Probe { json, .. } => *json,
                BackendCmd::Add(a) => a.json,
                BackendCmd::Enable { .. }
                | BackendCmd::Disable { .. }
                | BackendCmd::Drain { .. }
                | BackendCmd::Rm { .. } => false,
            },
            Command::Recipe { cmd } => match cmd {
                RecipeCmd::Ls(a) => a.json,
                RecipeCmd::Show { json, .. }
                | RecipeCmd::New { json, .. }
                | RecipeCmd::Validate { json, .. }
                | RecipeCmd::Run { json, .. } => *json,
                RecipeCmd::Edit { .. } | RecipeCmd::Rm { .. } => false,
            },
            Command::Profile { cmd } => match cmd {
                ProfileCmd::Ls(a) => a.json,
                ProfileCmd::Show { json, .. } => *json,
                ProfileCmd::New(a) => a.json,
                ProfileCmd::Edit { .. } | ProfileCmd::Rm { .. } => false,
            },
            Command::Provider { cmd } => match cmd {
                ProviderCmd::Ls(a) => a.json,
                ProviderCmd::Show { json, .. }
                | ProviderCmd::Test { json, .. }
                | ProviderCmd::Models { json, .. } => *json,
                ProviderCmd::Set(a) => a.json,
            },
            Command::Vast { cmd } => match cmd {
                VastCmd::Account(a) | VastCmd::GpuNames(a) => a.json,
                VastCmd::Offers(a) => a.json,
                VastCmd::Rent(a) => a.json,
                VastCmd::Ls { json, .. } | VastCmd::Diagnose { json, .. } => *json,
                VastCmd::Watch { .. }
                | VastCmd::Log { .. }
                | VastCmd::RestartDownload { .. }
                | VastCmd::Destroy { .. } => false,
            },
            Command::Tunnel { cmd } => match cmd {
                TunnelCmd::Status(a) => a.json,
                TunnelCmd::Up { json, .. } => *json,
                TunnelCmd::Down { .. } => false,
            },
            Command::Approvals { cmd } => match cmd {
                ApprovalsCmd::Ls(a) => a.json,
                ApprovalsCmd::Grant { .. } | ApprovalsCmd::Deny { .. } => false,
            },
            Command::Hf { cmd } => match cmd {
                HfCmd::Search { json, .. }
                | HfCmd::Files { json, .. }
                | HfCmd::Get { json, .. } => *json,
            },
            Command::Token { cmd } => match cmd {
                TokenCmd::Ls(a) => a.json,
                TokenCmd::Create { json, .. } => *json,
                TokenCmd::Revoke { .. } => false,
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
        // `mcp` belongs to M-01 and is intercepted in `main` before clap; until it lands it
        // is the verb that proves the fallback still reports an owner rather than clap's
        // bare "unrecognized subcommand".
        let cli = Cli::parse_from(["apexrouter", "mcp", "--proxy", "http://127.0.0.1:2739"]);
        match cli.verb() {
            Command::External(args) => assert_eq!(args[0], "mcp"),
            other => panic!("expected External, got {other:?}"),
        }
    }

    /// The S-08 half of ARCHITECTURE §7, line by line.
    #[test]
    fn the_s08_surface_parses() {
        let cases: Vec<Vec<&str>> = vec![
            vec!["apexrouter", "up", "Carnice-9b-Q6_K", "--alias", "auto"],
            vec![
                "apexrouter",
                "swap",
                "auto",
                "--to",
                "local-carnice",
                "--mode",
                "sequential",
            ],
            vec!["apexrouter", "open"],
            vec!["apexrouter", "env"],
            vec!["apexrouter", "backend", "ls", "--json"],
            vec![
                "apexrouter",
                "backend",
                "add",
                "http://127.0.0.1:8100",
                "--label",
                "box",
                "--tag",
                "local",
                "--key-env",
                "SOME_KEY",
            ],
            vec!["apexrouter", "backend", "probe", "local-carnice"],
            vec!["apexrouter", "backend", "rm", "local-carnice"],
            vec!["apexrouter", "recipe", "ls", "--json"],
            vec![
                "apexrouter",
                "recipe",
                "new",
                "--from-endpoint",
                "local-carnice",
            ],
            vec!["apexrouter", "recipe", "validate", "carnice"],
            vec!["apexrouter", "recipe", "run", "carnice", "--alias", "auto"],
            vec!["apexrouter", "profile", "ls"],
            vec![
                "apexrouter",
                "profile",
                "new",
                "two-3090s",
                "--gpu",
                "RTX 3090",
                "--num-gpus-min",
                "2",
                "--geo",
                "EU",
            ],
            vec!["apexrouter", "provider", "ls", "--json"],
            vec![
                "apexrouter",
                "provider",
                "set",
                "together",
                "--base-url",
                "https://api.together.ai",
                "--key-env",
                "TOGETHER_API_KEY",
            ],
            vec!["apexrouter", "provider", "test", "together"],
            vec![
                "apexrouter",
                "provider",
                "models",
                "together",
                "--org",
                "Qwen",
            ],
            vec!["apexrouter", "vast", "account", "--json"],
            vec![
                "apexrouter",
                "vast",
                "offers",
                "--profile",
                "two-3090s",
                "--gpu",
                "RTX 3090",
                "--num-gpus",
                "2",
                "--geo",
                "EU",
                "--max-price",
                "0.5",
                "--json",
            ],
            vec!["apexrouter", "vast", "gpu-names", "--json"],
            vec![
                "apexrouter",
                "vast",
                "rent",
                "--auto",
                "--profile",
                "two-3090s",
                "--model-repo",
                "unsloth/Qwen3-GGUF",
                "--quant",
                "Q4_K_M",
                "--max-hourly",
                "0.6",
                "--yes",
                "--no-wait",
            ],
            vec!["apexrouter", "vast", "ls", "--orphans", "--json"],
            vec!["apexrouter", "vast", "watch", "12345"],
            vec!["apexrouter", "vast", "log", "12345", "-f"],
            vec!["apexrouter", "vast", "diagnose", "12345"],
            vec!["apexrouter", "vast", "restart-download", "12345"],
            vec!["apexrouter", "vast", "destroy", "12345", "--yes"],
            vec!["apexrouter", "vast", "destroy", "--all", "--yes"],
            vec!["apexrouter", "tunnel", "up", "12345"],
            vec!["apexrouter", "tunnel", "down"],
            vec!["apexrouter", "tunnel", "status", "--json"],
            vec!["apexrouter", "approvals", "ls"],
            vec!["apexrouter", "approvals", "grant", "01J", "--yes"],
            vec!["apexrouter", "approvals", "deny", "01J"],
            vec!["apexrouter", "hf", "search", "qwen3 gguf", "--json"],
            vec!["apexrouter", "hf", "files", "unsloth/Qwen3-GGUF", "--json"],
            vec![
                "apexrouter",
                "hf",
                "get",
                "unsloth/Qwen3-GGUF",
                "--quant",
                "UD-Q4_K_XL",
                "--no-wait",
            ],
            vec![
                "apexrouter",
                "usage",
                "--since",
                "7d",
                "--by",
                "model",
                "--json",
            ],
            vec![
                "apexrouter",
                "compare",
                "--alias",
                "a",
                "--alias",
                "b",
                "--prompt",
                "hi",
                "--max-tokens",
                "64",
                "--json",
            ],
            vec!["apexrouter", "smoke", "--alias", "auto", "--json"],
            vec!["apexrouter", "smoke", "--base-url", "http://127.0.0.1:8100"],
            vec!["apexrouter", "doctor", "--only", "creds", "--json"],
            vec!["apexrouter", "migrate", "--dry-run"],
            vec!["apexrouter", "migrate", "--apply"],
            vec!["apexrouter", "token", "create", "--scope", "admin"],
            vec!["apexrouter", "token", "ls"],
            vec!["apexrouter", "token", "revoke", "APEXROUTER_TOKEN"],
        ];
        for case in cases {
            Cli::try_parse_from(&case).unwrap_or_else(|e| panic!("{case:?} did not parse: {e}"));
        }
    }

    #[test]
    fn a_money_verb_refuses_to_parse_without_its_required_ceiling() {
        // `--max-hourly` is not optional: a rent with no ceiling is an unbounded approval.
        assert!(Cli::try_parse_from([
            "apexrouter",
            "vast",
            "rent",
            "--auto",
            "--profile",
            "p",
            "--yes"
        ])
        .is_err());
    }

    #[test]
    fn geo_accepts_the_named_groups_and_an_explicit_list() {
        assert_eq!(parse_geo("any").expect("any"), GeoFilter::Any);
        assert_eq!(parse_geo("EU").expect("eu"), GeoFilter::Eu);
        assert_eq!(parse_geo("eu-nordic").expect("nordic"), GeoFilter::EuNordic);
        assert_eq!(
            parse_geo("cz, pl").expect("codes"),
            GeoFilter::Codes(vec!["CZ".to_string(), "PL".to_string()])
        );
        assert!(parse_geo("Czechia").is_err(), "a country NAME is a typo");
    }

    #[test]
    fn the_s08_need_and_json_tables_agree_with_the_architecture() {
        let need = |args: &[&str]| Cli::parse_from(args).verb().need();
        assert_eq!(need(&["apexrouter", "usage"]), Need::ReadState);
        assert_eq!(need(&["apexrouter", "doctor"]), Need::ReadState);
        assert_eq!(need(&["apexrouter", "vast", "ls"]), Need::ReadState);
        assert_eq!(need(&["apexrouter", "recipe", "ls"]), Need::ReadState);
        assert_eq!(need(&["apexrouter", "profile", "ls"]), Need::ReadState);
        assert_eq!(need(&["apexrouter", "migrate"]), Need::Pure);
        assert_eq!(need(&["apexrouter", "token", "create"]), Need::Pure);
        assert_eq!(
            need(&["apexrouter", "up", "carnice"]),
            Need::Mutate,
            "starting something is a mutation"
        );
        assert_eq!(
            need(&[
                "apexrouter",
                "vast",
                "rent",
                "--auto",
                "--profile",
                "p",
                "--max-hourly",
                "1",
                "--yes"
            ]),
            Need::Mutate
        );
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
