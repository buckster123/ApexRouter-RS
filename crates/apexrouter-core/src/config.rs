//! OWNER: unit C-02 (core/config.rs, config.example.toml). Do not edit outside that unit.
//!
//! `config.toml` — hand-edited, **every field defaulted**, so a missing file is a working
//! zero-config install. The struct shapes below are the contract Stage 3 and Stage 4 code
//! reads fields off; C-02 owns the loading, saving and env-override behaviour.
//!
//! A **borrowed** credential is never copied into our config: [`ConfigFile`] deliberately
//! has no field capable of holding one. Only a key the user typed into a GUI or
//! `--key-stdin` is written, and it goes to `$STATE/credentials.toml` at `0600`.
//!
//! # Unknown keys are reported, never obeyed and never fatal
//!
//! `[server] proxy_port = 18888` is a plausible typo for `proxy_bind`, and for one build it
//! did exactly nothing: the daemon bound `8888` while `config show` printed the default, with
//! no warning anywhere. A hard `#[serde(deny_unknown_fields)]` would have caught it and would
//! also have made an *older* binary refuse to start on a *newer* file, which is a worse
//! failure — so the rule here is **warn, loudly, on every surface**:
//!
//! * [`parse_config`] diffs the document the user wrote against the document the loaded
//!   config re-serialises to. Anything present in the first and absent in the second is a key
//!   this build does not know.
//! * Each one is logged at `warn` (stderr, per house rule 5) *and* recorded on
//!   [`Config::unknown_keys`], which [`Config::serializable`] carries — so `config show` and
//!   `config show --json` both surface it without the CLI having to remember to ask.
//! * [`Config::validate_file`] is the same check as a report, for a `config validate` verb:
//!   it prints the unknown keys next to the addresses actually bound, which is the pair that
//!   makes a typo obvious.
//! * `save()` never writes `unknown_keys` back, and never deletes the offending key either —
//!   it is the user's text, in the user's file.

use crate::error::{Error, Result};
use apexrouter_protocol::{ImageType, ProviderId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The whole configuration, with every section defaulted.
///
/// [`Config::default()`] is byte-for-byte the configuration `config.example.toml`
/// describes, which is what makes "a missing config file is a working zero-config
/// install" true rather than aspirational. A unit test parses that file and asserts the
/// result equals `Config::default()`, so the two can never drift.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Listeners, auth posture, drain timeout.
    pub server: ServerCfg,
    /// The request path's knobs.
    pub router: RouterCfg,
    /// Child-process supervision.
    pub supervisor: SupervisorCfg,
    /// Where to look for models and builds.
    pub endpoints: EndpointsCfg,
    /// Managed providers, by id.
    pub providers: BTreeMap<String, ProviderCfg>,
    /// vast.ai. Its own section because money lives here.
    pub vast: VastCfg,
    /// HuggingFace.
    pub hf: HfCfg,
    /// The published container images.
    pub docker: DockerCfg,
    /// Genuinely undiscoverable knowledge: which model needs which llama.cpp fork.
    pub known_forks: BTreeMap<String, KnownFork>,
    /// Interoperability with `~/.vastai-gguf` and the old TUI.
    pub compat: CompatCfg,
    /// Keys the file carried that this build does not know — a typo, or a section from a
    /// newer build. **Runtime-only**: `#[serde(skip)]`, so it is never read from or written
    /// back to `config.toml`; it exists so that every surface that renders a config renders
    /// the fact that part of the file was ignored.
    #[serde(skip)]
    pub unknown_keys: Vec<UnknownKey>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            server: ServerCfg::default(),
            router: RouterCfg::default(),
            supervisor: SupervisorCfg::default(),
            endpoints: EndpointsCfg::default(),
            // `[providers.together]` and the one `known_forks` entry are part of the
            // default *file* in ARCHITECTURE §5.2, so they are part of the default
            // *value* too. Otherwise "a missing file behaves exactly like the shipped
            // example" would be false, and the drift would be invisible.
            providers: default_providers(),
            vast: VastCfg::default(),
            hf: HfCfg::default(),
            docker: DockerCfg::default(),
            known_forks: default_known_forks(),
            compat: CompatCfg::default(),
            unknown_keys: Vec::new(),
        }
    }
}

/// The providers ARCHITECTURE §5.2 ships pre-configured.
fn default_providers() -> BTreeMap<String, ProviderCfg> {
    let mut m = BTreeMap::new();
    m.insert(
        "together".to_owned(),
        ProviderCfg {
            base_url: "https://api.together.ai/v1".to_owned(),
            api_key_env: Some("TOGETHER_API_KEY".to_owned()),
            api_key_file: None,
        },
    );
    m
}

/// The fork mapping ARCHITECTURE §5.2 ships pre-configured.
fn default_known_forks() -> BTreeMap<String, KnownFork> {
    let mut m = BTreeMap::new();
    m.insert(
        "deepseek-v4".to_owned(),
        KnownFork {
            match_repo: "deepseek-ai/DeepSeek-V4*".to_owned(),
            llama_cpp_repo: "fairydreaming/llama.cpp".to_owned(),
            llama_cpp_ref: "deepseek-dsa".to_owned(),
        },
    );
    m
}

/// `[server]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerCfg {
    /// Data plane. `$PROXY_PORT` overrides the port and IS honoured.
    pub proxy_bind: String,
    /// Control plane. `APEX` on a phone keypad.
    pub control_bind: String,
    /// Name of the env var holding the bearer token. Required for ANY non-loopback bind.
    pub token_env: String,
    /// Allow unauthenticated access from a genuinely loopback peer IP. Absent
    /// `ConnectInfo` **fails closed**, never open.
    pub loopback_bypass: bool,
    /// `""` = the embedded `ui-web`; a path = a live-reload dev loop.
    pub ui_dir: String,
    /// How long in-flight requests get after a shutdown signal.
    pub drain_timeout_secs: u64,
    /// May a `Mutate` CLI verb start the daemon?
    pub autostart: bool,
    /// Origins that may read a **proxy-listener** response cross-origin.
    ///
    /// Empty (the default) emits no CORS header at all, which is ApexRouter's behaviour and
    /// is a deliberate difference from LocalRouter — `endpoint_proxy.py` set
    /// `Access-Control-Allow-Origin: *` on every proxied response. A non-empty list emits
    /// the header for a matching `Origin`; the single entry `"*"` is an explicit opt-in to
    /// the old blanket behaviour.
    ///
    /// The **control** listener and every mutating route are never covered by this — the
    /// mutation gate of ARCHITECTURE §9.3 governs there regardless of what is listed here.
    pub proxy_cors_origins: Vec<String>,
}

impl Default for ServerCfg {
    fn default() -> Self {
        ServerCfg {
            proxy_bind: apexrouter_protocol::DEFAULT_PROXY_BIND.to_owned(),
            control_bind: apexrouter_protocol::DEFAULT_CONTROL_BIND.to_owned(),
            token_env: "APEXROUTER_TOKEN".to_owned(),
            loopback_bypass: true,
            ui_dir: String::new(),
            drain_timeout_secs: 30,
            autostart: true,
            proxy_cors_origins: Vec::new(),
        }
    }
}

/// `[router]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RouterCfg {
    /// Where legacy model names and (optionally) unknown ones land.
    pub default_alias: String,
    /// Strategy for rule 4, when one upstream model id lives on several backends.
    pub implicit_strategy: String,
    /// `reject` | `fallback`. **`reject` by default**: a fat-fingered `gpt-4o-mimi` must
    /// not silently bill a rented H100.
    pub unknown_model: String,
    /// Global in-flight request cap.
    pub max_inflight: u32,
    /// GLOBAL byte budget. A count cap alone permits 64 × 32 MiB of resident bodies.
    pub max_inflight_bytes: u64,
    /// Per-request body cap. aiohttp's silent 1 MiB was a bug.
    pub max_body_bytes: u64,
    /// TCP connect.
    pub connect_timeout_ms: u64,
    /// Time to response headers. Long, because a non-streaming completion sends none until
    /// generation finishes.
    pub headers_timeout_ms: u64,
    /// BETWEEN stream chunks. **Never** a total timeout on a stream.
    pub idle_timeout_ms: u64,
    /// How long to wait for a backend permit before 503 + `Retry-After`.
    pub queue_timeout_ms: u64,
    /// How many requests a **warm window** admits before it refuses (`ARCHITECTURE.md`
    /// §4.7). During a sequential swap the alias cannot serve, so arriving requests park
    /// rather than `503`; past this depth the honest answer is `503 warm_queue_full` with a
    /// `Retry-After`, because deepening a queue that is already the wrong answer only moves
    /// the failure later. `apexrouter_router::DEFAULT_WARM_QUEUE_MAX` is the same number and
    /// is the fallback for callers that have no `Config`.
    pub warm_queue_max: u32,
    /// Per-backend retry token bucket, so a struggling backend is not amplified into a storm.
    pub retry_budget_per_min: u32,
    /// Observations required before the breaker may open.
    pub breaker_min_volume: u32,
    /// `off` | `passthrough`. **Parsed and documented; not yet applied on the OpenAI request
    /// path in mk1** — the Anthropic → OpenAI cell always injects usage for its own
    /// accounting, and the OpenAI cell leaves the client body alone. Kept so a config that
    /// already names the key does not fail to load, and so mk2 can wire it without a schema
    /// change. Injecting `stream_options.include_usage` changes what every streaming client
    /// receives, so opting in stays a choice, not a default.
    pub request_usage: String,
    /// Prompts are NEVER stored unless this is on.
    pub capture_bodies: bool,
    /// Append a row to `usage.jsonl` per request.
    pub log_usage: bool,
    /// Serve `POST /v1/messages` on the proxy listener.
    pub anthropic_ingress: bool,
    /// Translate `tool_use`/`tool_result` <-> `tool_calls`. **On by default** since
    /// 2026-07-31 (CHARTER amendment): Claude Code sends 92 tool definitions on every
    /// request, so with this off the Anthropic ingress is dead on arrival for the one
    /// client it exists to serve. Translation is best-effort and allowed to be imperfect;
    /// the alternative was not "no tools", it was "the feature does not work at all".
    /// Turned off explicitly, a `/v1/messages` body carrying `tools` is still REFUSED
    /// loudly, naming this key — never silently stripped and answered wrongly.
    pub anthropic_tools: bool,
}

impl Default for RouterCfg {
    fn default() -> Self {
        RouterCfg {
            default_alias: "auto".to_owned(),
            implicit_strategy: "first_healthy".to_owned(),
            unknown_model: "reject".to_owned(),
            max_inflight: 64,
            max_inflight_bytes: 536_870_912,
            max_body_bytes: 33_554_432,
            connect_timeout_ms: 5_000,
            headers_timeout_ms: 600_000,
            idle_timeout_ms: 300_000,
            queue_timeout_ms: 30_000,
            warm_queue_max: 32,
            retry_budget_per_min: 30,
            breaker_min_volume: 5,
            request_usage: "off".to_owned(),
            capture_bodies: false,
            log_usage: true,
            anthropic_ingress: true,
            anthropic_tools: true,
        }
    }
}

/// `[supervisor]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SupervisorCfg {
    /// A REAL wall-clock deadline for the health gate, **reset on observed progress**.
    pub health_deadline_ms: u64,
    /// How often to probe while starting.
    pub health_interval_ms: u64,
    /// Re-adopt identity-verified children at startup.
    pub adopt_on_start: bool,
    /// **False by default, and honest about what it means**: a model that took 90 seconds
    /// and 6 GB to load must survive `systemctl --user restart`.
    pub kill_children_on_exit: bool,
    /// `never` | `on-failure`.
    pub restart: String,
    /// Restart budget.
    pub max_restarts_per_hour: u32,
    /// Rotate at this size, with **copytruncate** semantics — an adopted child holds an fd
    /// to that inode and renaming would send its output into a deleted file.
    pub log_rotate_mb: u64,
}

impl Default for SupervisorCfg {
    fn default() -> Self {
        SupervisorCfg {
            health_deadline_ms: 600_000,
            health_interval_ms: 3_000,
            adopt_on_start: true,
            kill_children_on_exit: false,
            restart: "never".to_owned(),
            max_restarts_per_hour: 5,
            log_rotate_mb: 32,
        }
    }
}

/// `[endpoints]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EndpointsCfg {
    /// Where to look for GGUFs.
    pub model_roots: Vec<String>,
    /// Where to look for `build*/bin/llama-server`.
    pub build_roots: Vec<String>,
    /// Globs to skip while walking.
    pub ignore_globs: Vec<String>,
    /// Port pool for local servers.
    pub port_range: (u16, u16),
    /// Sampling preset for `apexrouter up`.
    pub default_mode: String,
    /// Safety margin held back from every VRAM budget.
    pub vram_margin_mb: u64,
    /// Background rescan interval. **Plan-time queries are always LIVE** regardless.
    pub scan_interval_secs: u64,
}

impl Default for EndpointsCfg {
    fn default() -> Self {
        EndpointsCfg {
            model_roots: vec!["~/models".to_owned(), "~/.cache/huggingface/hub".to_owned()],
            build_roots: vec![
                "~/llama.cpp".to_owned(),
                "~/Projects/llama.cpp".to_owned(),
                "/usr/local/bin".to_owned(),
            ],
            ignore_globs: vec!["**/.cache/**".to_owned()],
            port_range: apexrouter_protocol::DEFAULT_LOCAL_PORT_RANGE,
            default_mode: "thinking".to_owned(),
            vram_margin_mb: 1_024,
            scan_interval_secs: 300,
        }
    }
}

/// `[providers.<id>]`. **A key is never a required plaintext field here.**
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderCfg {
    /// Used **verbatim**: a legacy `https://api.together.xyz` is never rewritten to
    /// `.ai` (§5.4). ARCHITECTURE §5.2 ships this with a trailing `/v1`; the
    /// `Backend.base_url` invariant ("never ends in `/v1`") applies where a
    /// [`apexrouter_protocol::Backend`] is *built* from this, not here.
    pub base_url: String,
    /// Name of an env var holding the key.
    pub api_key_env: Option<String>,
    /// Path to a file holding the key.
    pub api_key_file: Option<String>,
}

/// `[providers.vast]` plus the money ceilings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VastCfg {
    /// API root.
    pub base_url: String,
    /// Conventional key path.
    pub api_key_file: String,
    /// vast publishes no rate limits; never poll faster than this.
    pub poll_min_ms: u64,
    /// How often the daemon refreshes the fleet cache the snapshot serves. Zero disables
    /// the poller (the cache is then fed only by handlers that read the fleet anyway).
    pub fleet_poll_secs: u64,
    /// Auto-destroy a wedged instance after this long.
    pub max_boot_secs: u64,
    /// Local port pool for `ssh -L`.
    pub tunnel_port_range: (u16, u16),
    /// `adopt` | `down`.
    pub tunnels_on_shutdown: String,
    /// **HARD daemon-side cap.** `SpendApproval::confirm` cannot exceed it, so an agent that
    /// fills in a big number still cannot spend more than the human configured.
    pub max_usd_per_hour_ceiling: f64,
    /// `true` => an MCP-sourced approval returns `HumanConfirmationRequired`.
    pub require_human_confirm: bool,
}

impl Default for VastCfg {
    fn default() -> Self {
        VastCfg {
            base_url: "https://console.vast.ai/api/v0".to_owned(),
            api_key_file: "~/.config/vastai/vast_api_key".to_owned(),
            poll_min_ms: 5_000,
            fleet_poll_secs: 60,
            max_boot_secs: 1_800,
            tunnel_port_range: apexrouter_protocol::DEFAULT_TUNNEL_PORT_RANGE,
            tunnels_on_shutdown: "adopt".to_owned(),
            max_usd_per_hour_ceiling: 4.00,
            require_human_confirm: false,
        }
    }
}

/// `[hf]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HfCfg {
    /// Conventional token path.
    pub token_file: String,
    /// Where downloads land.
    pub download_dir: String,
}

impl Default for HfCfg {
    fn default() -> Self {
        HfCfg {
            token_file: "~/.cache/huggingface/token".to_owned(),
            download_dir: "~/models".to_owned(),
        }
    }
}

/// `[docker]` — genuine config: Andre publishes these artifacts.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DockerCfg {
    /// Prebuilt llama.cpp image.
    pub prebuilt: String,
    /// Image that builds llama.cpp from a named repo/ref.
    pub builder: String,
    /// vLLM image.
    pub vllm: String,
    /// Multi-service studio image (llama-server + ComfyUI). See `docs/STUDIO.md` S6.
    pub studio: String,
}

impl Default for DockerCfg {
    fn default() -> Self {
        DockerCfg {
            prebuilt: "ghcr.io/buckster123/vastai-gguf:prebuilt".to_owned(),
            builder: "ghcr.io/buckster123/vastai-gguf:builder".to_owned(),
            vllm: "ghcr.io/buckster123/vastai-gguf:vllm".to_owned(),
            studio: "ghcr.io/buckster123/vastai-studio:cu128".to_owned(),
        }
    }
}

/// `[known_forks."<name>"]` — a **read** table, not a decoration.
///
/// A hit **forces** `image_type = Builder`, sets `LLAMA_CPP_REPO`/`LLAMA_CPP_REF`, and
/// pushes a `"custom fork → builder image → +12–18 min cold start"` warning that every
/// surface renders before the confirm.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct KnownFork {
    /// Glob against the HF repo id, e.g. `"deepseek-ai/DeepSeek-V4*"`.
    pub match_repo: String,
    /// Which llama.cpp fork to build.
    pub llama_cpp_repo: String,
    /// Which ref of it.
    pub llama_cpp_ref: String,
}

/// `[compat]`. Both write-side toggles default to the safe setting.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompatCfg {
    /// Read `~/.vastai-gguf` for usage, providers and instances.
    pub read_legacy_state: bool,
    /// **DEFAULT OFF.** Append every usage row to `~/.vastai-gguf/usage.log` too, so the old
    /// LocalRouter TUI's usage view keeps working during a transition.
    ///
    /// `~/.vastai-gguf` is *another tool's state directory*. Starting our daemon must not
    /// silently append to a file we do not own, so this is opt-in: `apexrouter migrate`
    /// offers it, and the config comment explains it. ApexRouter's own usage log under
    /// `Paths::state()` is unaffected and stays on regardless.
    pub mirror_usage_log: bool,
    /// `""` = off. A path mirrors `.active_endpoint` for the old TUI.
    pub active_endpoint_path: String,
    /// **DEFAULT OFF, and the reason is in the config comment**: LocalRouter's
    /// `_proxy_down()` reads `/tmp/vastai-gguf-proxy.pid` and SIGTERMs whatever it names.
    /// Turning this on hands the old TUI's "Proxy → stop" menu item a kill switch for the
    /// whole daemon.
    pub legacy_proxy_pidfile: bool,
    /// `POST /switch` validates any supplied `base_url` against this list. Unauthenticated
    /// `/switch` with an arbitrary URL plus an injected key is a credential-exfiltration
    /// primitive, not merely SSRF.
    pub allow_switch_hosts: Vec<String>,
}

impl Default for CompatCfg {
    fn default() -> Self {
        CompatCfg {
            read_legacy_state: true,
            mirror_usage_log: false,
            active_endpoint_path: String::new(),
            legacy_proxy_pidfile: false,
            allow_switch_hosts: vec![
                "api.together.ai".to_owned(),
                "127.0.0.1".to_owned(),
                "localhost".to_owned(),
            ],
        }
    }
}

/// What `save()` writes.
///
/// **It has no field capable of holding a borrowed credential**, by construction: a key we
/// merely *read* from an env var or a third-party file is described by a
/// `CredentialSource`, never copied here. The only writable credential surface in the whole
/// product is `$STATE/credentials.toml` at mode `0600`, and only a key the user *typed*
/// ever reaches it (§9.2).
///
/// The two assertions below are compile-time, not conventions:
///
/// ```compile_fail
/// // There is no field on the config document that can hold key material.
/// let mut f = apexrouter_core::config::ConfigFile::default();
/// f.api_key = "sk-live-borrowed".to_string();
/// ```
///
/// ```compile_fail
/// // Nor on a provider section: it names *where* the key is, never what it is.
/// let mut p = apexrouter_core::config::ProviderCfg::default();
/// p.api_key = "sk-live-borrowed".to_string();
/// ```
///
/// `ConfigFile` mirrors [`Config`] section for section. It is a distinct type so that a
/// future runtime-only field on `Config` (a `#[serde(skip)]` one, resolved from flags or
/// the environment) cannot accidentally be written back to disk.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConfigFile {
    /// Keys the file carried that this build does not know.
    ///
    /// **Declared first, and cleared before every write.** First, because TOML puts bare
    /// values before tables and a root key rendered after `[compat]` would be read back as
    /// part of `[compat]`. Cleared, because this is a *report about* the file, not content
    /// of it: [`Config::save_to`] blanks it so the round-trip cannot invent a key, while the
    /// key the user actually mistyped is left exactly where they wrote it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unknown_keys: Vec<UnknownKey>,
    /// `[server]`.
    pub server: ServerCfg,
    /// `[router]`.
    pub router: RouterCfg,
    /// `[supervisor]`.
    pub supervisor: SupervisorCfg,
    /// `[endpoints]`.
    pub endpoints: EndpointsCfg,
    /// `[providers.<id>]`. Names where each key lives; never a key.
    pub providers: BTreeMap<String, ProviderCfg>,
    /// `[vast]`.
    pub vast: VastCfg,
    /// `[hf]`.
    pub hf: HfCfg,
    /// `[docker]`.
    pub docker: DockerCfg,
    /// `[known_forks.<name>]`.
    pub known_forks: BTreeMap<String, KnownFork>,
    /// `[compat]`.
    pub compat: CompatCfg,
}

/// One key in a `config.toml` that this build does not know.
///
/// Not an error and not a refusal: an older binary must survive a newer file. It is a
/// *statement*, carried on [`Config::unknown_keys`] and rendered by `config show`, because
/// the alternative — a key that does nothing and says nothing — is how an operator ends up
/// debugging a port they thought they had set.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnknownKey {
    /// Dotted path as written, e.g. `server.proxy_port`.
    pub path: String,
    /// The known key at the same level it is closest to, when one is plausible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub did_you_mean: Option<String>,
}

impl std::fmt::Display for UnknownKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.did_you_mean {
            Some(s) => write!(f, "{} (did you mean `{s}`?)", self.path),
            None => write!(f, "{}", self.path),
        }
    }
}

/// What [`Config::validate_file`] answers: does this file parse, what in it is ignored, and
/// what would actually be bound.
///
/// The last two belong in one report on purpose. The `proxy_port`/`proxy_bind` typo was
/// invisible precisely because "the key you wrote" and "the port that got bound" were never
/// printed next to each other.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ConfigValidation {
    /// The file inspected.
    pub path: String,
    /// False means "defaults everywhere", which is a supported install, not a fault.
    pub exists: bool,
    /// `None` when the file parses. A parse failure is the one hard error here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parse_error: Option<String>,
    /// Everything in the file this build ignored.
    #[serde(default)]
    pub unknown_keys: Vec<UnknownKey>,
    /// The address the proxy listener would actually take, `$PROXY_PORT` included.
    pub proxy_bind: String,
    /// The address the control listener would actually take.
    pub control_bind: String,
}

impl ConfigValidation {
    /// True when nothing at all is worth telling the operator.
    pub fn is_clean(&self) -> bool {
        self.parse_error.is_none() && self.unknown_keys.is_empty()
    }
}

/// The bundled `config.example.toml`, embedded at build time.
///
/// Embedded rather than read from disk because **nothing is ever read from or written into
/// the repo directory at runtime** (invariant 5). A unit test parses it and asserts it
/// equals [`Config::default()`], so the shipped file and the code cannot drift.
const EXAMPLE_CONFIG: &str = include_str!("../../../config.example.toml");

/// Mode for anything we write that lives next to credentials: owner read/write only.
const FILE_MODE: u32 = 0o600;

/// Mode for directories we create on the way to a config file.
const DIR_MODE: u32 = 0o700;

/// Read an environment variable, treating the empty string as "unset".
fn env_path(key: &str) -> Option<PathBuf> {
    let v = std::env::var_os(key)?;
    if v.is_empty() {
        None
    } else {
        Some(PathBuf::from(v))
    }
}

/// Where `load_from` will look, given the CLI's two overrides.
///
/// Deliberately mirrors [`crate::paths::Paths`]'s config chain rather than calling it:
/// `--config`/`--home` are pushed into the process env before `Config::load()`, so the
/// override arguments exist for embedders and tests, which must not mutate a global.
fn resolve_config_path(path: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = path {
        return Some(p.to_path_buf());
    }
    if let Some(h) = home {
        return Some(h.join("config.toml"));
    }
    if let Some(p) = env_path("APEXROUTER_CONFIG") {
        return Some(p);
    }
    if let Some(h) = env_path("APEXROUTER_HOME") {
        return Some(h.join("config.toml"));
    }
    let cfg_home = env_path("XDG_CONFIG_HOME")
        .filter(|p| p.is_absolute())
        .or_else(|| env_path("HOME").map(|h| h.join(".config")))
        .or_else(dirs::config_dir)?;
    Some(cfg_home.join("apexrouter").join("config.toml"))
}

/// `[providers.vast]` is the spelling ARCHITECTURE §5.2 prints; `[vast]` is the canonical
/// section this crate deserialises. Copy the former over the latter so a config pasted out
/// of the documentation works, without losing the `providers` map entry a caller asking for
/// `provider("vast")` expects to find.
///
/// An explicit `[vast]` key always wins over the `[providers.vast]` spelling of the same
/// key, so the canonical section is never silently overridden by the compatibility one.
fn lift_providers_vast(doc: &mut toml::Value) {
    let legacy = match doc.get("providers").and_then(|p| p.get("vast")) {
        Some(toml::Value::Table(t)) => t.clone(),
        _ => return,
    };
    let Some(root) = doc.as_table_mut() else {
        return;
    };
    let entry = root
        .entry("vast".to_owned())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let Some(canonical) = entry.as_table_mut() else {
        return;
    };
    for (k, v) in legacy {
        canonical.entry(k).or_insert(v);
    }
}

/// Parse a whole `config.toml` document.
///
/// Unknown keys are **obeyed by nobody and hidden from nobody**: they are ignored for the
/// purpose of building the [`Config`] — we must survive additive changes and an older binary
/// reading a newer file — and then reported, on `stderr` and on [`Config::unknown_keys`].
fn parse_config(text: &str) -> Result<Config> {
    let written: toml::Value = toml::from_str(text)?;
    let mut value = written.clone();
    lift_providers_vast(&mut value);
    let mut cfg: Config = value.try_into()?;
    cfg.unknown_keys = unknown_keys(&written, &cfg);
    for k in &cfg.unknown_keys {
        match &k.did_you_mean {
            Some(near) => tracing::warn!(
                key = %k.path,
                did_you_mean = %near,
                "config.toml key is not one this build knows; it has NO effect"
            ),
            None => tracing::warn!(
                key = %k.path,
                "config.toml key is not one this build knows; it has NO effect"
            ),
        }
    }
    Ok(cfg)
}

/// Every key the document carries that the loaded config did not keep.
///
/// **Schema-free by construction.** A hand-maintained list of legal keys has exactly the
/// failure mode this function exists to catch — it would go stale, silently — so the
/// comparison is against the config's *own* re-serialisation: whatever survived a round trip
/// through the real structs is known, whatever did not is not. Free-form maps
/// (`[providers.<id>]`, `[known_forks.<name>]`) therefore need no special handling at all:
/// the user's own key is in both documents.
fn unknown_keys(written: &toml::Value, cfg: &Config) -> Vec<UnknownKey> {
    let Ok(toml::Value::Table(mut effective)) = toml::Value::try_from(cfg.serializable()) else {
        // Unreachable in practice; a config that cannot re-serialise is not a reason to
        // fail a load that has already succeeded.
        return Vec::new();
    };
    let Some(written) = written.as_table() else {
        return Vec::new();
    };
    // `[providers.vast]` is the spelling ARCHITECTURE §5.2 prints, and `lift_providers_vast`
    // copies its keys into the canonical `[vast]`. Both spellings are legal in that one
    // table, so widen it to the union before the walk — otherwise a config pasted straight
    // out of the documentation warns about every money key in it.
    if let Some(toml::Value::Table(vast)) = effective.get("vast").cloned() {
        if let Some(pv) = effective
            .get_mut("providers")
            .and_then(toml::Value::as_table_mut)
            .and_then(|p| p.get_mut("vast"))
            .and_then(toml::Value::as_table_mut)
        {
            for (k, v) in vast {
                pv.entry(k).or_insert(v);
            }
        }
    }
    // The defaults are consulted for *suggestions only*. An `Option` field left unset
    // serialises to nothing, so `[providers.<id>] api_key_env` is missing from a config
    // that does not set it — present in the defaults, though, and that is enough to answer
    // "did you mean".
    let defaults = match toml::Value::try_from(Config::default().serializable()) {
        Ok(toml::Value::Table(t)) => t,
        _ => toml::Table::new(),
    };
    let mut out = Vec::new();
    diff_table(written, &effective, &defaults, "", &mut out);
    out
}

/// Walk `written` against `effective`, recording every path present only in `written`.
fn diff_table(
    written: &toml::Table,
    effective: &toml::Table,
    defaults: &toml::Table,
    prefix: &str,
    out: &mut Vec<UnknownKey>,
) {
    for (key, value) in written {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match effective.get(key) {
            None => out.push(UnknownKey {
                did_you_mean: nearest_key(key, effective, defaults),
                path,
            }),
            Some(known) => {
                if let (Some(w), Some(e)) = (value.as_table(), known.as_table()) {
                    // A free-form map — `[providers.<id>]`, `[known_forks.<name>]` — has no
                    // entry of the user's name in the defaults. Its *siblings* all have the
                    // same shape, so the first default entry answers for it. For a fixed
                    // struct this branch never fires: every non-`Option` field is always
                    // serialised, so the key is always there.
                    let d = defaults
                        .get(key)
                        .and_then(toml::Value::as_table)
                        .or_else(|| defaults.values().next().and_then(toml::Value::as_table));
                    diff_table(w, e, d.unwrap_or(e), &path, out);
                }
            }
        }
    }
}

/// The known sibling key an unknown one was most likely meant to be.
///
/// A pure edit distance is useless here: `proxy_port` → `proxy_bind` is four substitutions,
/// well past any sane threshold, yet it is obviously the intended key. Shared prefix decides
/// first — that is what a typo in a *suffix* looks like — and the distance only breaks ties.
/// Best-effort by nature: no suggestion at all beats a confidently wrong one, so a candidate
/// that is neither close nor prefix-sharing is not offered.
fn nearest_key(key: &str, known: &toml::Table, defaults: &toml::Table) -> Option<String> {
    let mut best: Option<(usize, usize, &str)> = None;
    for candidate in known.keys().chain(defaults.keys()) {
        let shared = key
            .bytes()
            .zip(candidate.bytes())
            .take_while(|(a, b)| a == b)
            .count();
        let distance = edit_distance(key, candidate);
        if shared < 3 && distance > 2 {
            continue;
        }
        // Longest shared prefix wins; the closest spelling breaks the tie.
        let better = match best {
            None => true,
            Some((s, d, _)) => shared > s || (shared == s && distance < d),
        };
        if better {
            best = Some((shared, distance, candidate));
        }
    }
    best.map(|(_, _, c)| c.to_owned())
}

/// Levenshtein distance, two rows at a time. Small inputs — these are TOML key names.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Parse a bind string, falling back twice so the accessor can stay infallible.
fn parse_bind(configured: &str, fallback: &str, last_resort_port: u16) -> SocketAddr {
    configured
        .trim()
        .parse::<SocketAddr>()
        .or_else(|_| fallback.parse::<SocketAddr>())
        .unwrap_or_else(|_| {
            SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                last_resort_port,
            )
        })
}

/// Create `path`'s parent directories at `0700`.
fn ensure_parent(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() || parent.exists() {
        return Ok(());
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(parent)
        .map_err(|source| Error::Io {
            path: parent.display().to_string(),
            source,
        })
}

/// Write `bytes` to `path` at mode `0600`, atomically: a temp file in the **same**
/// directory, `fsync`, then `rename`. A reader never sees a half-written config.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;

    ensure_parent(path)?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(
        ".{}.tmp{}",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "config.toml".to_owned()),
        std::process::id()
    ));

    let io = |p: &Path, source: std::io::Error| Error::Io {
        path: p.display().to_string(),
        source,
    };

    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(FILE_MODE)
            .open(&tmp)
            .map_err(|e| io(&tmp, e))?;
        f.write_all(bytes).map_err(|e| io(&tmp, e))?;
        f.sync_all().map_err(|e| io(&tmp, e))?;
    }
    // An existing file keeps its own inode's mode through a rename, so force ours on.
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(FILE_MODE))
        .map_err(|e| io(&tmp, e))?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(io(path, e));
    }
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Copy every key of `src` into `dst`, **preserving `dst`'s existing decor** — that is,
/// the comments and spacing a human wrote around the key and around the value.
fn merge_table(dst: &mut toml_edit::Table, src: &toml_edit::Table) {
    for (key, src_item) in src.iter() {
        match src_item {
            toml_edit::Item::Table(src_t) => {
                match dst.get_mut(key).and_then(|i| i.as_table_mut()) {
                    Some(dst_t) => merge_table(dst_t, src_t),
                    None => {
                        dst.insert(key, src_item.clone());
                    }
                }
            }
            _ => {
                let decor = dst
                    .get(key)
                    .and_then(|i| i.as_value())
                    .map(|v| v.decor().clone());
                let mut item = src_item.clone();
                if let (Some(decor), Some(v)) = (decor, item.as_value_mut()) {
                    *v.decor_mut() = decor;
                }
                // A comment written on its own line ABOVE a `key = value` line is the
                // **key's** prefix decor, not the value's, and `Table::insert` installs a
                // fresh `Key` — which silently drops it. `insert_formatted`, given the key
                // already in the document, is the only call that keeps both decors. This
                // matters because `config.example.toml` — the file `config init` writes —
                // documents every field with exactly that shape, so the naive insert
                // stripped a user's whole config of its comments on the first `config set`.
                // Same idiom as `catalog.rs`, which has the same requirement.
                match dst.key(key).cloned() {
                    Some(existing) => {
                        dst.insert_formatted(&existing, item);
                    }
                    None => {
                        dst.insert(key, item);
                    }
                }
            }
        }
    }
}

impl Config {
    /// Load from the resolved paths. A missing file yields a fully working `Config`.
    ///
    /// # Errors
    /// Only when a config file exists but cannot be read or parsed. **Absence is not an
    /// error** — a zero-config install is a supported install.
    pub fn load() -> Result<Config> {
        Config::load_from(None, None)
    }

    /// Load with explicit overrides, as the CLI's `--config`/`--home` do.
    ///
    /// `path` wins over `home`, which wins over `$APEXROUTER_CONFIG`, which wins over
    /// `$APEXROUTER_HOME/config.toml`, which wins over
    /// `$XDG_CONFIG_HOME/apexrouter/config.toml`. If nothing resolves — a process with no
    /// `$HOME` and no XDG vars — the defaults are returned rather than an error.
    ///
    /// # Errors
    /// [`Error::Io`] if the file exists but cannot be read; [`Error::Toml`] if it does not
    /// parse. Unknown keys never fail: an older binary must survive a newer file.
    pub fn load_from(path: Option<&Path>, home: Option<&Path>) -> Result<Config> {
        match resolve_config_path(path, home) {
            Some(p) => Config::read_file(&p),
            None => Ok(Config::default()),
        }
    }

    /// Read one specific file, treating "not there" as "defaults".
    fn read_file(path: &Path) -> Result<Config> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(source) => {
                return Err(Error::Io {
                    path: path.display().to_string(),
                    source,
                })
            }
        };
        parse_config(&text)
    }

    /// Write `config.example.toml`'s content to the resolved config path.
    ///
    /// Returns the path written, so the caller can print it.
    ///
    /// # Errors
    /// [`Error::Conflict`] if the file already exists and `force` is false; [`Error::Io`]
    /// if the directory cannot be created or the file cannot be written.
    pub fn init_file(paths: &crate::paths::Paths, force: bool) -> Result<PathBuf> {
        let path = paths.config_file();
        Config::write_example_to(&path, force)?;
        Ok(path)
    }

    /// The whole of [`init_file`](Self::init_file) against an explicit path.
    fn write_example_to(path: &Path, force: bool) -> Result<()> {
        if path.exists() && !force {
            return Err(Error::Conflict(format!(
                "{} already exists; pass --force to overwrite it",
                path.display()
            )));
        }
        write_private(path, EXAMPLE_CONFIG.as_bytes())
    }

    /// Check one config file without adopting it: does it parse, what does this build
    /// ignore in it, and what would actually be bound.
    ///
    /// Infallible on purpose — it is a *report*, and "the file does not parse" is one of the
    /// things it reports rather than a reason to have no answer. This is the whole of a
    /// `config validate` verb; the CLI needs only to render it.
    pub fn validate_file(path: &Path) -> ConfigValidation {
        let mut report = ConfigValidation {
            path: path.display().to_string(),
            exists: path.exists(),
            ..ConfigValidation::default()
        };
        let cfg = match Config::read_file(path) {
            Ok(c) => c,
            Err(e) => {
                report.parse_error = Some(e.to_string());
                Config::default()
            }
        };
        report.unknown_keys = cfg.unknown_keys.clone();
        report.proxy_bind = cfg.proxy_bind().to_string();
        report.control_bind = cfg.control_bind().to_string();
        report
    }

    /// [`validate_file`](Self::validate_file) against the resolved config path.
    pub fn validate(paths: &crate::paths::Paths) -> ConfigValidation {
        Config::validate_file(&paths.config_file())
    }

    /// Persist through `toml_edit`, so hand-written comments survive. Mode `0600`.
    ///
    /// # Errors
    /// [`Error::Invalid`] if the file on disk exists but is not parseable TOML — we refuse
    /// to clobber a document we do not understand; [`Error::Io`] on a write failure.
    pub fn save(&self, paths: &crate::paths::Paths) -> Result<()> {
        self.save_to(&paths.config_file())
    }

    /// The whole of [`save`](Self::save) against an explicit path.
    fn save_to(&self, path: &Path) -> Result<()> {
        let base_text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            // No file yet: start from the fully commented example, so the config a user
            // ends up hand-editing explains itself.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => EXAMPLE_CONFIG.to_owned(),
            Err(source) => {
                return Err(Error::Io {
                    path: path.display().to_string(),
                    source,
                })
            }
        };

        let mut base: toml_edit::DocumentMut =
            base_text
                .parse()
                .map_err(|e: toml_edit::TomlError| Error::Invalid {
                    what: format!("config file {}", path.display()),
                    why: format!("refusing to overwrite a document that does not parse: {e}"),
                })?;

        // `unknown_keys` is a report about the file, not a section of it. Blanked here so a
        // save can never materialise it as a real key — and note that nothing removes the
        // user's mistyped key either: `merge_table` only ever writes the keys we know, so
        // whatever they wrote stays exactly where they wrote it, comments and all.
        let mut on_disk = self.serializable();
        on_disk.unknown_keys.clear();
        let fresh_text = toml::to_string_pretty(&on_disk)?;
        let mut fresh: toml_edit::DocumentMut =
            fresh_text
                .parse()
                .map_err(|e: toml_edit::TomlError| Error::Invalid {
                    what: "serialised config".into(),
                    why: e.to_string(),
                })?;

        // Keep writing the vast section where this document already keeps it, rather than
        // growing a second, competing table next to it.
        let base_uses_providers_vast = base
            .get("providers")
            .and_then(|p| p.get("vast"))
            .is_some_and(|v| v.is_table() || v.is_inline_table());
        if base_uses_providers_vast && base.get("vast").is_none() {
            if let Some(item) = fresh.as_table_mut().remove("vast") {
                fresh["providers"]["vast"] = item;
            }
        }

        merge_table(base.as_table_mut(), fresh.as_table());
        write_private(path, base.to_string().as_bytes())
    }

    /// The serialisable projection. Runtime-only fields are `#[serde(skip)]`.
    ///
    /// [`ConfigFile::unknown_keys`] rides along so that every renderer of a config —
    /// `config show`, `config show --json` — reports what the file said and this build
    /// ignored, without each of them having to remember to ask. [`Config::save_to`] clears
    /// it before writing.
    pub fn serializable(&self) -> ConfigFile {
        ConfigFile {
            unknown_keys: self.unknown_keys.clone(),
            server: self.server.clone(),
            router: self.router.clone(),
            supervisor: self.supervisor.clone(),
            endpoints: self.endpoints.clone(),
            providers: self.providers.clone(),
            vast: self.vast.clone(),
            hf: self.hf.clone(),
            docker: self.docker.clone(),
            known_forks: self.known_forks.clone(),
            compat: self.compat.clone(),
        }
    }

    /// Resolve `[docker]` by image family.
    pub fn image_for(&self, t: ImageType) -> String {
        match t {
            ImageType::Prebuilt => self.docker.prebuilt.clone(),
            ImageType::Builder => self.docker.builder.clone(),
            ImageType::Vllm => self.docker.vllm.clone(),
            ImageType::Studio => self.docker.studio.clone(),
        }
    }

    /// Look a model repo up in `known_forks`.
    ///
    /// `match_repo` is a glob over the HuggingFace repo id, so `deepseek-ai/DeepSeek-V4*`
    /// matches every quantisation of that family. Entries are tried in key order, so the
    /// answer does not depend on how the file was written. A hit forces the *builder*
    /// image — see `core::argv::plan_container`.
    pub fn known_fork_for(&self, repo: &str) -> Option<&KnownFork> {
        self.known_forks.values().find(|f| {
            if f.match_repo.is_empty() {
                return false;
            }
            if f.match_repo == repo {
                return true;
            }
            glob::Pattern::new(&f.match_repo).is_ok_and(|p| p.matches(repo))
        })
    }

    /// The proxy listener's address. Honours `$PROXY_PORT`.
    ///
    /// `$PROXY_PORT` is honoured because LocalRouter honoured it and shell aliases on this
    /// machine depend on it. An unparseable bind falls back to
    /// [`apexrouter_protocol::DEFAULT_PROXY_BIND`] rather than failing: a typo in a config
    /// file must not leave the machine with no endpoint at all.
    pub fn proxy_bind(&self) -> SocketAddr {
        let mut addr = parse_bind(
            &self.server.proxy_bind,
            apexrouter_protocol::DEFAULT_PROXY_BIND,
            8888,
        );
        if let Some(port) = std::env::var("PROXY_PORT")
            .ok()
            .and_then(|v| v.trim().parse::<u16>().ok())
        {
            addr.set_port(port);
        }
        addr
    }

    /// The control listener's address.
    ///
    /// Deliberately **not** overridable by an environment variable: clients discover it
    /// from the daemon lock file's owner record or `$APEXROUTER_URL`, and a second
    /// discovery mechanism is how LocalRouter ended up with four disagreeing answers.
    pub fn control_bind(&self) -> SocketAddr {
        parse_bind(
            &self.server.control_bind,
            apexrouter_protocol::DEFAULT_CONTROL_BIND,
            2739,
        )
    }

    /// One provider's section, by id.
    pub fn provider(&self, id: &ProviderId) -> Option<&ProviderCfg> {
        self.providers.get(id.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// `std::env` is process-global. Every test in this module that reads or writes it
    /// takes this first, so `cargo test`'s thread pool cannot interleave them.
    static ENV: Mutex<()> = Mutex::new(());

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn mode_of(p: &Path) -> u32 {
        std::fs::metadata(p).expect("stat").permissions().mode() & 0o7777
    }

    // ---- defaults ---------------------------------------------------------------------

    /// ARCHITECTURE §5.2, field by field. If a default changes, this test is where the
    /// change has to be argued for.
    #[test]
    fn defaults_are_exactly_architecture_5_2() {
        let c = Config::default();

        assert_eq!(c.server.proxy_bind, "127.0.0.1:8888");
        assert_eq!(c.server.control_bind, "127.0.0.1:2739");
        assert_eq!(c.server.token_env, "APEXROUTER_TOKEN");
        assert!(c.server.loopback_bypass);
        assert_eq!(c.server.ui_dir, "");
        assert_eq!(c.server.drain_timeout_secs, 30);
        assert!(c.server.autostart);

        assert_eq!(c.router.default_alias, "auto");
        assert_eq!(c.router.implicit_strategy, "first_healthy");
        assert_eq!(c.router.unknown_model, "reject");
        assert_eq!(c.router.max_inflight, 64);
        assert_eq!(c.router.max_inflight_bytes, 536_870_912);
        assert_eq!(c.router.max_body_bytes, 33_554_432);
        assert_eq!(c.router.connect_timeout_ms, 5_000);
        assert_eq!(c.router.headers_timeout_ms, 600_000);
        assert_eq!(c.router.idle_timeout_ms, 300_000);
        assert_eq!(c.router.queue_timeout_ms, 30_000);
        // The same 32 `apexrouter_router::DEFAULT_WARM_QUEUE_MAX` names. The router crate is
        // not a dependency of core, so the two cannot be compared by the compiler; this is
        // the assertion that keeps them from drifting, and `docs/API.md` prints the number.
        assert_eq!(c.router.warm_queue_max, 32);
        assert_eq!(c.router.retry_budget_per_min, 30);
        assert_eq!(c.router.breaker_min_volume, 5);
        assert_eq!(c.router.request_usage, "off");
        assert!(
            !c.router.capture_bodies,
            "prompts are never stored by default"
        );
        assert!(c.router.log_usage);
        assert!(c.router.anthropic_ingress);
        assert!(
            c.router.anthropic_tools,
            "a stock config must drive real Claude Code, which sends tools on every request"
        );

        assert_eq!(c.supervisor.health_deadline_ms, 600_000);
        assert_eq!(c.supervisor.health_interval_ms, 3_000);
        assert!(c.supervisor.adopt_on_start);
        assert!(
            !c.supervisor.kill_children_on_exit,
            "children outlive the manager (ARCHITECTURE §1.4)"
        );
        assert_eq!(c.supervisor.restart, "never");
        assert_eq!(c.supervisor.max_restarts_per_hour, 5);
        assert_eq!(c.supervisor.log_rotate_mb, 32);

        assert_eq!(
            c.endpoints.model_roots,
            ["~/models", "~/.cache/huggingface/hub"]
        );
        assert_eq!(
            c.endpoints.build_roots,
            ["~/llama.cpp", "~/Projects/llama.cpp", "/usr/local/bin"]
        );
        assert_eq!(c.endpoints.ignore_globs, ["**/.cache/**"]);
        assert_eq!(c.endpoints.port_range, (8100, 8199));
        assert_eq!(c.endpoints.default_mode, "thinking");
        assert_eq!(c.endpoints.vram_margin_mb, 1_024);
        assert_eq!(c.endpoints.scan_interval_secs, 300);

        let together = c.providers.get("together").expect("[providers.together]");
        assert_eq!(together.base_url, "https://api.together.ai/v1");
        assert_eq!(together.api_key_env.as_deref(), Some("TOGETHER_API_KEY"));
        assert_eq!(together.api_key_file, None);

        assert_eq!(c.vast.base_url, "https://console.vast.ai/api/v0");
        assert_eq!(c.vast.api_key_file, "~/.config/vastai/vast_api_key");
        assert_eq!(c.vast.poll_min_ms, 5_000);
        assert_eq!(c.vast.max_boot_secs, 1_800);
        assert_eq!(c.vast.tunnel_port_range, (8800, 8899));
        assert_eq!(c.vast.tunnels_on_shutdown, "adopt");
        assert_eq!(c.vast.max_usd_per_hour_ceiling, 4.00);
        assert!(!c.vast.require_human_confirm);

        assert_eq!(c.hf.token_file, "~/.cache/huggingface/token");
        assert_eq!(c.hf.download_dir, "~/models");

        assert_eq!(
            c.docker.prebuilt,
            "ghcr.io/buckster123/vastai-gguf:prebuilt"
        );
        assert_eq!(c.docker.builder, "ghcr.io/buckster123/vastai-gguf:builder");
        assert_eq!(c.docker.vllm, "ghcr.io/buckster123/vastai-gguf:vllm");

        let fork = c.known_forks.get("deepseek-v4").expect("known fork");
        assert_eq!(fork.match_repo, "deepseek-ai/DeepSeek-V4*");
        assert_eq!(fork.llama_cpp_repo, "fairydreaming/llama.cpp");
        assert_eq!(fork.llama_cpp_ref, "deepseek-dsa");

        assert!(c.compat.read_legacy_state);
        assert!(
            !c.compat.mirror_usage_log,
            "starting the daemon must never append to another tool's usage.log"
        );
        assert_eq!(c.compat.active_endpoint_path, "");
        assert!(
            !c.compat.legacy_proxy_pidfile,
            "the old TUI must not get a kill switch by default"
        );
        assert_eq!(
            c.compat.allow_switch_hosts,
            ["api.together.ai", "127.0.0.1", "localhost"]
        );
    }

    /// A missing file is a working install, not an error.
    #[test]
    fn a_missing_file_yields_the_defaults() {
        let d = tmp();
        let missing = d.path().join("nowhere/config.toml");
        assert!(!missing.exists());
        assert_eq!(
            Config::load_from(Some(&missing), None).expect("missing file must not error"),
            Config::default()
        );
        // …and via the `--home` override too.
        assert_eq!(
            Config::load_from(None, Some(d.path())).expect("missing file must not error"),
            Config::default()
        );
    }

    /// The shipped example and the compiled defaults cannot drift.
    #[test]
    fn the_shipped_example_is_exactly_the_defaults() {
        let parsed = parse_config(EXAMPLE_CONFIG).expect("config.example.toml must parse");
        assert_eq!(parsed, Config::default());
    }

    // ---- loading ----------------------------------------------------------------------

    #[test]
    fn a_partial_file_keeps_every_other_default_and_reports_unknown_keys() {
        let d = tmp();
        let p = d.path().join("config.toml");
        std::fs::write(
            &p,
            "[router]\nmax_inflight = 7\nsomething_from_a_newer_build = true\n",
        )
        .expect("write");

        let c = Config::load_from(Some(&p), None).expect("load");
        assert_eq!(c.router.max_inflight, 7);
        assert_eq!(c.router.max_body_bytes, RouterCfg::default().max_body_bytes);
        assert_eq!(c.server, ServerCfg::default());
        assert_eq!(c.vast, VastCfg::default());

        // Unknown is not fatal — an older binary must survive a newer file — but it is not
        // silent either.
        assert_eq!(
            c.unknown_keys
                .iter()
                .map(|k| k.path.as_str())
                .collect::<Vec<_>>(),
            ["router.something_from_a_newer_build"]
        );
    }

    // ---- D5: a typo'd key is surfaced, not swallowed ------------------------------------

    /// The defect, exactly as it was reported: `proxy_port = 18888` bound nothing, warned
    /// about nothing, and `config show` printed the default as if the file agreed with it.
    #[test]
    fn a_typod_key_is_surfaced_with_the_key_it_was_meant_to_be() {
        let d = tmp();
        let p = d.path().join("config.toml");
        std::fs::write(&p, "[server]\nproxy_port = 18888\n").expect("write");

        let c = Config::load_from(Some(&p), None).expect("an unknown key must not be fatal");
        assert_eq!(c.unknown_keys.len(), 1, "{:?}", c.unknown_keys);
        assert_eq!(c.unknown_keys[0].path, "server.proxy_port");
        assert_eq!(
            c.unknown_keys[0].did_you_mean.as_deref(),
            Some("proxy_bind"),
            "four substitutions apart, and still obviously the intended key"
        );
        assert_eq!(
            c.unknown_keys[0].to_string(),
            "server.proxy_port (did you mean `proxy_bind`?)"
        );

        // `config show` renders exactly this projection, so the CLI surfaces it for free.
        let shown = serde_json::to_value(c.serializable()).expect("json");
        assert_eq!(shown["unknown_keys"][0]["path"], "server.proxy_port");
        assert_eq!(shown["unknown_keys"][0]["did_you_mean"], "proxy_bind");

        // …and the report a `config validate` verb renders prints the ignored key next to
        // the address that is actually bound. That pairing is the whole point.
        let v = Config::validate_file(&p);
        assert!(v.exists);
        assert_eq!(v.parse_error, None);
        assert!(!v.is_clean());
        assert_eq!(v.unknown_keys, c.unknown_keys);
        assert_eq!(
            v.proxy_bind, "127.0.0.1:8888",
            "the file did NOT move the listener, whatever the operator thought"
        );
    }

    /// Every level of the document, and every shape of unknown: a section nobody knows, a
    /// key under a known section, a key under a free-form map entry.
    #[test]
    fn unknown_keys_are_found_at_every_level_and_free_form_maps_are_not_false_positives() {
        let d = tmp();
        let p = d.path().join("config.toml");
        std::fs::write(
            &p,
            "[nonsense]\nwhatever = 1\n\n\
             [router]\nmax_inflght = 7\n\n\
             [providers.mycorp]\nbase_url = \"http://127.0.0.1:9/v1\"\napi_kye_env = \"X\"\n\n\
             [known_forks.mine]\nmatch_repo = \"me/*\"\nllama_cpp_repo = \"me/llama.cpp\"\n\
             llama_cpp_ref = \"main\"\n",
        )
        .expect("write");

        let c = Config::load_from(Some(&p), None).expect("load");
        let mut paths: Vec<&str> = c.unknown_keys.iter().map(|k| k.path.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(
            paths,
            [
                "nonsense",
                "providers.mycorp.api_kye_env",
                "router.max_inflght",
            ],
            "a user's own provider id and fork name are data, not typos"
        );
        let near = |p: &str| {
            c.unknown_keys
                .iter()
                .find(|k| k.path == p)
                .and_then(|k| k.did_you_mean.clone())
        };
        assert_eq!(near("router.max_inflght").as_deref(), Some("max_inflight"));
        assert_eq!(
            near("providers.mycorp.api_kye_env").as_deref(),
            Some("api_key_env")
        );
        assert_eq!(near("nonsense"), None, "nothing at the root is close to it");
    }

    /// The documented `[providers.vast]` spelling carries `[vast]` keys. Those are legal in
    /// that table and must not be reported as typos.
    #[test]
    fn the_providers_vast_spelling_is_not_reported_as_unknown() {
        let c = parse_config(
            "[providers.vast]\n\
             base_url = \"https://console.vast.ai/api/v0\"\n\
             poll_min_ms = 9000\n\
             max_usd_per_hour_ceiling = 1.25\n",
        )
        .expect("parse");
        assert_eq!(c.vast.poll_min_ms, 9_000);
        assert!(
            c.unknown_keys.is_empty(),
            "the documented spelling must not warn: {:?}",
            c.unknown_keys
        );
    }

    /// A key we do not know is left alone, not deleted, and never written back as data.
    #[test]
    fn saving_neither_writes_the_report_nor_destroys_the_key_it_reports() {
        let d = tmp();
        let p = d.path().join("config.toml");
        std::fs::write(
            &p,
            "# hand written\n[server]\n# a typo, kept\nproxy_port = 18888\nproxy_bind = \"127.0.0.1:8888\"\n",
        )
        .expect("write");

        let c = Config::load_from(Some(&p), None).expect("load");
        assert_eq!(c.unknown_keys.len(), 1);
        c.save_to(&p).expect("save");

        let after = std::fs::read_to_string(&p).expect("read");
        assert!(
            after.contains("proxy_port = 18888"),
            "the user's own text is theirs; we report it, we do not delete it:\n{after}"
        );
        assert!(after.contains("# a typo, kept"), "decor survives:\n{after}");
        assert!(
            !after.contains("unknown_keys"),
            "the report must never become a key in the file:\n{after}"
        );
        // …and it is still reported on the next load.
        assert_eq!(
            Config::load_from(Some(&p), None)
                .expect("reload")
                .unknown_keys
                .len(),
            1
        );
    }

    /// A file with nothing wrong with it says nothing.
    #[test]
    fn a_clean_file_reports_no_unknown_keys() {
        let d = tmp();
        let p = d.path().join("config.toml");
        std::fs::write(&p, EXAMPLE_CONFIG).expect("write");
        let v = Config::validate_file(&p);
        assert!(v.is_clean(), "{v:?}");
        assert!(v.exists);
        assert_eq!(v.control_bind, "127.0.0.1:2739");

        let missing = d.path().join("nowhere.toml");
        let v = Config::validate_file(&missing);
        assert!(!v.exists);
        assert!(v.is_clean(), "a missing file is a working install");
    }

    /// A file that does not parse is reported, not panicked over.
    #[test]
    fn validate_reports_a_parse_failure_rather_than_refusing_to_answer() {
        let d = tmp();
        let p = d.path().join("config.toml");
        std::fs::write(&p, "[router\nmax_inflight = ").expect("write");
        let v = Config::validate_file(&p);
        assert!(v.parse_error.is_some());
        assert!(!v.is_clean());
        assert_eq!(
            v.proxy_bind, "127.0.0.1:8888",
            "defaults are still answered"
        );
    }

    #[test]
    fn an_explicit_path_beats_a_home() {
        let d = tmp();
        let home = d.path().join("home");
        std::fs::create_dir_all(&home).expect("mkdir");
        std::fs::write(home.join("config.toml"), "[router]\nmax_inflight = 1\n").expect("write");
        let explicit = d.path().join("explicit.toml");
        std::fs::write(&explicit, "[router]\nmax_inflight = 2\n").expect("write");

        let c = Config::load_from(Some(&explicit), Some(&home)).expect("load");
        assert_eq!(c.router.max_inflight, 2);
    }

    #[test]
    fn a_malformed_file_is_an_error_not_a_silent_default() {
        let d = tmp();
        let p = d.path().join("config.toml");
        std::fs::write(&p, "[router\nmax_inflight = ").expect("write");
        assert!(matches!(
            Config::load_from(Some(&p), None),
            Err(Error::Toml(_))
        ));
    }

    /// ARCHITECTURE §5.2 prints the vast section as `[providers.vast]`; a config pasted
    /// out of the documentation has to work.
    #[test]
    fn providers_vast_is_lifted_into_the_canonical_vast_section() {
        let c = parse_config(
            "[providers.vast]\n\
             base_url = \"https://example.invalid/api/v0\"\n\
             poll_min_ms = 12345\n\
             max_usd_per_hour_ceiling = 0.5\n",
        )
        .expect("parse");
        assert_eq!(c.vast.base_url, "https://example.invalid/api/v0");
        assert_eq!(c.vast.poll_min_ms, 12_345);
        assert_eq!(c.vast.max_usd_per_hour_ceiling, 0.5);
        // The providers map still lists it, so `provider("vast")` keeps working.
        assert!(c.providers.contains_key("vast"));
    }

    #[test]
    fn an_explicit_vast_section_beats_the_compatibility_spelling() {
        let c =
            parse_config("[vast]\npoll_min_ms = 1000\n\n[providers.vast]\npoll_min_ms = 9999\n")
                .expect("parse");
        assert_eq!(c.vast.poll_min_ms, 1_000);
    }

    // ---- binds ------------------------------------------------------------------------

    #[test]
    fn proxy_port_env_overrides_the_proxy_bind_only() {
        let _g = ENV.lock().expect("env lock");
        let before = std::env::var("PROXY_PORT").ok();
        std::env::set_var("PROXY_PORT", "9999");

        let c = Config::default();
        assert_eq!(c.proxy_bind().port(), 9999);
        assert_eq!(c.proxy_bind().ip().to_string(), "127.0.0.1");
        assert_eq!(
            c.control_bind().port(),
            2739,
            "control is not env-overridable"
        );

        std::env::set_var("PROXY_PORT", "not-a-port");
        assert_eq!(c.proxy_bind().port(), 8888, "garbage is ignored, not fatal");

        match before {
            Some(v) => std::env::set_var("PROXY_PORT", v),
            None => std::env::remove_var("PROXY_PORT"),
        }
    }

    #[test]
    fn an_unparseable_bind_falls_back_instead_of_leaving_no_endpoint() {
        let _g = ENV.lock().expect("env lock");
        let before = std::env::var("PROXY_PORT").ok();
        std::env::remove_var("PROXY_PORT");

        let mut c = Config::default();
        c.server.proxy_bind = "definitely not an address".into();
        c.server.control_bind = String::new();
        assert_eq!(c.proxy_bind(), "127.0.0.1:8888".parse().expect("addr"));
        assert_eq!(c.control_bind(), "127.0.0.1:2739".parse().expect("addr"));

        if let Some(v) = before {
            std::env::set_var("PROXY_PORT", v);
        }
    }

    // ---- lookups ----------------------------------------------------------------------

    #[test]
    fn image_for_resolves_every_family() {
        let c = Config::default();
        assert_eq!(c.image_for(ImageType::Prebuilt), c.docker.prebuilt);
        assert_eq!(c.image_for(ImageType::Builder), c.docker.builder);
        assert_eq!(c.image_for(ImageType::Vllm), c.docker.vllm);
        assert_eq!(c.image_for(ImageType::Studio), c.docker.studio);
    }

    #[test]
    fn known_fork_for_globs_the_repo_id() {
        let c = Config::default();
        let hit = c
            .known_fork_for("deepseek-ai/DeepSeek-V4-Pro")
            .expect("the shipped fork mapping must match");
        assert_eq!(hit.llama_cpp_repo, "fairydreaming/llama.cpp");
        assert_eq!(hit.llama_cpp_ref, "deepseek-dsa");
        assert!(c.known_fork_for("Qwen/Qwen3.5-9B-GGUF").is_none());
        assert!(c.known_fork_for("").is_none());
    }

    #[test]
    fn an_empty_match_repo_never_matches_everything() {
        let mut c = Config::default();
        c.known_forks.clear();
        c.known_forks.insert("broken".into(), KnownFork::default());
        assert!(c.known_fork_for("anyone/anything").is_none());
    }

    #[test]
    fn provider_looks_up_by_id() {
        let c = Config::default();
        let id = ProviderId::parse("together").expect("id");
        assert!(c.provider(&id).is_some());
        let missing = ProviderId::parse("nobody").expect("id");
        assert!(c.provider(&missing).is_none());
    }

    // ---- writing ----------------------------------------------------------------------

    #[test]
    fn save_round_trips_through_toml_edit_and_preserves_hand_written_comments() {
        let d = tmp();
        let p = d.path().join("config.toml");
        std::fs::write(
            &p,
            "# Andre's note: 9001 because 8888 is taken by the old proxy.\n\
             [server]\n\
             proxy_bind = \"127.0.0.1:9001\"   # keep this\n",
        )
        .expect("write");

        let mut c = Config::load_from(Some(&p), None).expect("load");
        assert_eq!(c.server.proxy_bind, "127.0.0.1:9001");
        c.router.max_inflight = 7;
        c.save_to(&p).expect("save");

        let text = std::fs::read_to_string(&p).expect("read back");
        assert!(
            text.contains("# Andre's note:"),
            "leading comment lost:\n{text}"
        );
        assert!(
            text.contains("# keep this"),
            "trailing comment lost:\n{text}"
        );
        assert!(
            text.contains("max_inflight = 7"),
            "new value missing:\n{text}"
        );

        let again = Config::load_from(Some(&p), None).expect("reload");
        assert_eq!(again, c);
        assert_eq!(mode_of(&p), FILE_MODE);
    }

    /// The shape `config.example.toml` is made of: a comment on its **own line, above** a
    /// `key = value` line, inside a table.
    ///
    /// This is the key's prefix decor rather than the value's, and it is the case the
    /// sibling test above does not reach — that one covers a comment above a `[table]`
    /// header (the table's own decor) and a trailing comment after a value (the value's
    /// suffix decor), both of which survived even the naive `Table::insert`. `config init`
    /// writes `config.example.toml` verbatim, and every field in it is documented in this
    /// third shape, so getting it wrong stripped a user's entire config of its comments on
    /// the first `apexrouter config set`.
    #[test]
    fn a_comment_above_a_key_survives_a_save_that_changes_that_key() {
        let d = tmp();
        let p = d.path().join("config.toml");
        std::fs::write(
            &p,
            "[server]\n\
             # Data plane: the one base URL every agent on this machine points at.\n\
             proxy_bind = \"127.0.0.1:9001\"\n\
             # Control plane: REST + WebSocket + /metrics + the embedded web UI.\n\
             control_bind = \"127.0.0.1:2739\"\n\
             \n\
             [router]\n\
             # How many requests may be in flight at once.\n\
             max_inflight = 3\n",
        )
        .expect("write");

        let mut c = Config::load_from(Some(&p), None).expect("load");
        assert_eq!(c.server.proxy_bind, "127.0.0.1:9001");
        assert_eq!(c.router.max_inflight, 3);
        // Change one of the documented keys: its own comment, and every sibling's, must live.
        c.router.max_inflight = 7;
        c.save_to(&p).expect("save");

        let text = std::fs::read_to_string(&p).expect("read back");
        for comment in [
            "# Data plane: the one base URL every agent on this machine points at.",
            "# Control plane: REST + WebSocket + /metrics + the embedded web UI.",
            "# How many requests may be in flight at once.",
        ] {
            assert!(text.contains(comment), "above-key comment lost:\n{text}");
        }
        assert!(
            text.contains("max_inflight = 7"),
            "new value missing:\n{text}"
        );
        assert!(
            text.contains("proxy_bind = \"127.0.0.1:9001\""),
            "an untouched value must keep its value:\n{text}"
        );
        assert_eq!(Config::load_from(Some(&p), None).expect("reload"), c);
    }

    #[test]
    fn save_is_idempotent() {
        let d = tmp();
        let p = d.path().join("config.toml");
        let c = Config::default();
        c.save_to(&p).expect("first save");
        let once = std::fs::read_to_string(&p).expect("read");
        c.save_to(&p).expect("second save");
        let twice = std::fs::read_to_string(&p).expect("read");
        assert_eq!(once, twice);
        assert_eq!(Config::load_from(Some(&p), None).expect("reload"), c);
    }

    /// A document that spells the section `[providers.vast]` keeps that spelling instead
    /// of growing a second, competing `[vast]` table.
    #[test]
    fn save_keeps_the_vast_section_where_the_document_already_puts_it() {
        let d = tmp();
        let p = d.path().join("config.toml");
        std::fs::write(&p, "[providers.vast]\npoll_min_ms = 7000\n").expect("write");

        let mut c = Config::load_from(Some(&p), None).expect("load");
        assert_eq!(c.vast.poll_min_ms, 7_000);
        c.vast.poll_min_ms = 8_000;
        c.save_to(&p).expect("save");

        let text = std::fs::read_to_string(&p).expect("read");
        assert!(
            !text.contains("\n[vast]"),
            "grew a competing table:\n{text}"
        );
        assert_eq!(
            Config::load_from(Some(&p), None)
                .expect("reload")
                .vast
                .poll_min_ms,
            8_000
        );
    }

    #[test]
    fn save_refuses_to_clobber_a_document_it_cannot_parse() {
        let d = tmp();
        let p = d.path().join("config.toml");
        std::fs::write(&p, "[server\nthis is not toml").expect("write");
        assert!(matches!(
            Config::default().save_to(&p),
            Err(Error::Invalid { .. })
        ));
        assert_eq!(
            std::fs::read_to_string(&p).expect("read"),
            "[server\nthis is not toml",
            "the unparseable file must be left alone"
        );
    }

    #[test]
    fn init_writes_the_example_at_0600_and_refuses_to_overwrite() {
        let d = tmp();
        let p = d.path().join("nested/dir/config.toml");

        Config::write_example_to(&p, false).expect("first init");
        assert_eq!(mode_of(&p), FILE_MODE);
        assert_eq!(
            mode_of(p.parent().expect("parent")) & 0o777,
            DIR_MODE,
            "the config dir is owner-only"
        );
        assert_eq!(
            std::fs::read_to_string(&p).expect("read"),
            EXAMPLE_CONFIG,
            "init writes config.example.toml verbatim"
        );
        assert_eq!(
            Config::load_from(Some(&p), None).expect("load"),
            Config::default()
        );

        assert!(matches!(
            Config::write_example_to(&p, false),
            Err(Error::Conflict(_))
        ));
        Config::write_example_to(&p, true).expect("forced init");
        assert_eq!(mode_of(&p), FILE_MODE);
    }

    #[test]
    fn serializable_mirrors_every_section() {
        let mut c = Config::default();
        c.router.max_inflight = 11;
        c.vast.poll_min_ms = 6_000;
        let f = c.serializable();
        assert_eq!(f.router, c.router);
        assert_eq!(f.vast, c.vast);
        assert_eq!(f.providers, c.providers);
        assert_eq!(f.known_forks, c.known_forks);
        assert_eq!(f.compat, c.compat);
    }
}
