//! Endpoint lifecycle. An **Endpoint** is a thing ApexRouter knows how to start and stop.
//!
//! [`EndpointRecord`] is what lands in `$STATE/endpoints/<id>.json`, and it holds **facts
//! only**: pid, start ticks, boot id, port, argv hash, desired state. There is deliberately
//! no `status` field — a `status: "running"` string is a lie the moment somebody types
//! `kill`. Liveness is computed on read by `apexrouter_core::proc`.

use crate::backend::{BackendKind, CredentialSource, Protocol};
use crate::catalog::{ContainerLaunch, ContainerRuntime};
use crate::fit::{FitPlan, KvType, NglPlan, SplitPlan};
use crate::ids::{Alias, BackendId, BuildId, InstanceId, ProviderId};
use crate::route::SwapMode;
use serde::{Deserialize, Serialize};

/// A back-pointer from a [`crate::backend::Backend`] to the endpoint that owns its lifecycle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointRef {
    /// The endpoint id, which is also the backend id.
    pub id: BackendId,
    /// Which provisioner owns it.
    pub kind: BackendKind,
}

/// Persisted at `$STATE/endpoints/<id>.json`. **Facts only.**
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EndpointRecord {
    /// Stable id.
    pub id: BackendId,
    /// Exactly what we were asked to run.
    pub spec: EndpointSpec,
    /// Expectation IS state, and an expectation is a fact.
    pub desired: DesiredState,
    /// Process identity. `None` for `Managed`/`Node` endpoints, which have no process.
    #[serde(default)]
    pub proc: Option<ProcFacts>,
    /// The port we reserved.
    #[serde(default)]
    pub port: Option<u16>,
    /// Where its stdout/stderr go.
    #[serde(default)]
    pub log_path: Option<String>,
    /// When it was started, unix seconds.
    pub started_at_unix: i64,
    /// What we planned. Used for VRAM reservation accounting by `budget_from_rig`.
    #[serde(default)]
    pub fit: Option<FitPlan>,
    /// True when we did not spawn it but verified its identity and took ownership.
    pub adopted: bool,
    /// Which aliases currently point here. Convenience for the UI; the table is authoritative.
    #[serde(default)]
    pub alias_bindings: Vec<Alias>,
}

/// What the operator asked for. Not what is true right now.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesiredState {
    /// It should be up. If it is not, that is a failure to report.
    Running,
    /// It should be down. If it is up, that is an orphan to tidy.
    Stopped,
}

/// Process identity, strong enough to survive a PID reuse and a reboot.
///
/// `boot_id` is part of it because `start_time_ticks` is measured since boot and is *not*
/// comparable across one. `exe` is advisory only and its `" (deleted)"` suffix is stripped,
/// so rebuilding `build-vulkan` under a running server does not un-adopt it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcFacts {
    /// The pid.
    pub pid: u32,
    /// `/proc/<pid>/stat` field 22, parsed after the LAST `)`.
    pub start_time_ticks: u64,
    /// `/proc/sys/kernel/random/boot_id`.
    pub boot_id: String,
    /// Resolved `/proc/<pid>/exe`, with any `" (deleted)"` suffix stripped.
    pub exe: String,
    /// SHA-256 of the NUL-joined argv, so a re-exec with different flags is detectable.
    pub cmdline_sha256: String,
}

/// The five things ApexRouter knows how to bring into existence.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EndpointSpec {
    /// Local `llama-server`.
    LocalLlama(LocalLlamaSpec),
    /// Local vLLM.
    LocalVllm(LocalVllmSpec),
    /// A rented vast.ai container.
    Vast(VastSpec),
    /// A plain OpenAI-compatible URL somebody registered.
    Node(NodeSpec),
    /// A managed provider.
    Managed(ManagedSpec),
}

impl EndpointSpec {
    /// Which provisioner owns this spec.
    pub fn kind(&self) -> BackendKind {
        match self {
            EndpointSpec::LocalLlama(_) => BackendKind::LocalLlama,
            EndpointSpec::LocalVllm(_) => BackendKind::LocalVllm,
            EndpointSpec::Vast(s) => match s.runtime {
                ContainerRuntime::LlamaCpp => BackendKind::VastLlama,
                ContainerRuntime::Vllm => BackendKind::VastVllm,
            },
            EndpointSpec::Node(_) => BackendKind::Node,
            EndpointSpec::Managed(_) => BackendKind::Managed,
        }
    }

    /// A readable, charset-valid id proposal. The caller still has to de-duplicate it,
    /// and still has to run it through [`BackendId::parse`].
    pub fn suggested_id(&self) -> String {
        match self {
            EndpointSpec::LocalLlama(s) => {
                let stem = s
                    .model_path
                    .rsplit('/')
                    .next()
                    .unwrap_or(&s.model_path)
                    .split('.')
                    .next()
                    .unwrap_or("model");
                slugify(&format!("local-{stem}"))
            }
            EndpointSpec::LocalVllm(s) => slugify(&format!("vllm-{}", tail(&s.model_id))),
            EndpointSpec::Vast(s) => slugify(&format!("vast-{}", s.instance_id.0)),
            EndpointSpec::Node(s) => slugify(&format!("node-{}", host_of(&s.base_url))),
            EndpointSpec::Managed(s) => match &s.model_id {
                Some(m) => slugify(&format!("{}-{}", s.provider.as_str(), tail(m))),
                None => slugify(s.provider.as_str()),
            },
        }
    }
}

/// Last path segment of a slash-separated id, e.g. an HF repo's model name.
fn tail(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

/// Host part of a URL, without a scheme, port or path.
fn host_of(url: &str) -> &str {
    let no_scheme = url.rsplit("//").next().unwrap_or(url);
    let host = no_scheme.split('/').next().unwrap_or(no_scheme);
    host.split(':').next().unwrap_or(host)
}

/// Lowercase, keep `[a-z0-9._-]`, collapse everything else to `-`, and guarantee the result
/// starts with `[a-z0-9]` and is at most 64 bytes — i.e. always a valid [`BackendId`].
fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(64));
    let mut last_dash = false;
    for ch in s.chars() {
        let c = ch.to_ascii_lowercase();
        let keep = c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_' || c == '-';
        if keep {
            if c == '-' {
                if last_dash {
                    continue;
                }
                last_dash = true;
            } else {
                last_dash = false;
            }
            out.push(c);
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out
        .chars()
        .next()
        .is_some_and(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit()))
    {
        out.remove(0);
    }
    while out.ends_with('-') || out.ends_with('.') || out.ends_with('_') {
        out.pop();
    }
    out.truncate(64);
    while out.ends_with('-') || out.ends_with('.') || out.ends_with('_') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("endpoint");
    }
    out
}

/// Everything needed to launch one local `llama-server`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LocalLlamaSpec {
    /// Which discovered build to use.
    pub build: BuildId,
    /// Absolute, expanded path. The raw `~` form is NEVER stored.
    pub model_path: String,
    /// Vision projector, when the model has one.
    #[serde(default)]
    pub mmproj: Option<String>,
    /// The `-a` value: the model id this server advertises upstream.
    pub alias_flag: String,
    /// Bind host. Default `127.0.0.1`.
    pub host: String,
    /// `None` allocates from `[endpoints] port_range`.
    #[serde(default)]
    pub port: Option<u16>,
    /// `None` leaves `--ctx-size` UNSET, so llama.cpp's own `--fit` can size it.
    #[serde(default)]
    pub ctx: Option<u32>,
    /// `-np`.
    #[serde(default)]
    pub parallel: Option<u32>,
    /// `-ctk`/`-ctv`.
    #[serde(default)]
    pub kv_type: Option<KvType>,
    /// `-ngl` policy. `Auto` emits nothing.
    pub ngl: NglPlan,
    /// `-dev`, `-sm`, `-mg`, `--tensor-split`.
    pub split: SplitPlan,
    /// Sampling preset.
    pub mode: SamplingMode,
    /// `-fa on|off|auto`, when the build supports the tristate.
    #[serde(default)]
    pub flash_attn: Option<TriState>,
    /// Written via `--api-key-file`. **Never** argv: that would put it in `/proc/*/cmdline`.
    #[serde(default)]
    pub api_key: Option<CredentialSource>,
    /// Appended verbatim, after everything the builder emits.
    #[serde(default)]
    pub extra_args: Vec<String>,
}

/// The three sampling presets, plus `Raw` for "emit nothing".
///
/// `launch.sh` is authoritative and **all three presets include `--top-k 20`** — the
/// divergence between `config.SAMPLING_PRESETS` and `launch.sh` is exactly the class of bug
/// one argv builder exists to prevent.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SamplingMode {
    /// `--temp 1.0 --top-p 0.95 --top-k 20 --min-p 0.0 --presence-penalty 1.5`.
    Thinking,
    /// `--temp 0.6 --top-p 0.95 --top-k 20 --min-p 0.0 --presence-penalty 0.0`.
    Coding,
    /// Thinking off, via `--chat-template-kwargs`.
    Nonthinking,
    /// Emit no sampling flags at all.
    Raw,
}

/// A three-state flag, for b9199's `-fa on|off|auto`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriState {
    /// Force on.
    On,
    /// Force off.
    Off,
    /// Let llama.cpp decide.
    Auto,
}

/// Everything needed to launch a local vLLM.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LocalVllmSpec {
    /// The vLLM binary.
    pub bin: String,
    /// HuggingFace model id.
    pub model_id: String,
    /// Tensor parallelism.
    #[serde(default)]
    pub tp: Option<u32>,
    /// `--max-model-len`.
    #[serde(default)]
    pub ctx: Option<u32>,
    /// `--quantization`.
    #[serde(default)]
    pub quantization: Option<String>,
    /// `--kv-cache-dtype`.
    #[serde(default)]
    pub kv_cache_dtype: Option<String>,
    /// `--enforce-eager`. On the container side this is the literal string `"true"`/`"false"`.
    pub enforce_eager: bool,
    /// `--reasoning-parser`.
    #[serde(default)]
    pub reasoning_parser: Option<String>,
    /// `--gpu-memory-utilization`.
    #[serde(default)]
    pub gpu_util: Option<f32>,
    /// `--max-num-seqs`.
    #[serde(default)]
    pub max_num_seqs: Option<u32>,
    /// `--trust-remote-code`.
    pub trust_remote: bool,
    /// `--enable-chunked-prefill`.
    pub chunked_prefill: bool,
    /// Bind host.
    pub host: String,
    /// `None` allocates from the port range.
    #[serde(default)]
    pub port: Option<u16>,
    /// Device tokens, for `CUDA_VISIBLE_DEVICES` and friends.
    #[serde(default)]
    pub devices: Vec<String>,
    /// Appended verbatim.
    #[serde(default)]
    pub extra_args: Vec<String>,
}

/// A plain OpenAI-compatible (or Anthropic-native) URL somebody registered. No lifecycle.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeSpec {
    /// INVARIANT: stored without a trailing `/v1`.
    pub base_url: String,
    /// Where its credential lives.
    pub credential: CredentialSource,
    /// Human label.
    pub label: String,
    /// Models to advertise before the prober has spoken to it.
    #[serde(default)]
    pub declared_models: Vec<String>,
    /// The dialect it speaks. `OpenAi` unless stated.
    #[serde(default)]
    pub protocol: Protocol,
}

/// A managed provider (together.ai and friends). No lifecycle.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ManagedSpec {
    /// Provider id. `vast-gguf`, `together`, …
    pub provider: ProviderId,
    /// INVARIANT: stored without a trailing `/v1`. `api.together.xyz` is never rewritten.
    pub base_url: String,
    /// Where its credential lives.
    pub credential: CredentialSource,
    /// Pin one model id, when the provider serves many.
    #[serde(default)]
    pub model_id: Option<String>,
    /// The dialect it speaks. `OpenAi` unless stated.
    #[serde(default)]
    pub protocol: Protocol,
}

/// A rented vast.ai container.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VastSpec {
    /// The contract id, which the create call returns as `new_contract`.
    pub instance_id: InstanceId,
    /// What runs inside.
    pub runtime: ContainerRuntime,
    /// Image, env map, onstart and exposure posture.
    pub launch: ContainerLaunch,
    /// The `ssh -L` tunnel, when this instance is reached tunnel-only (the default).
    #[serde(default)]
    pub tunnel: Option<TunnelSpec>,
}

/// One `ssh -N -L` forward.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelSpec {
    /// Which instance it serves.
    pub instance_id: InstanceId,
    /// Local port, from the 8800+ pool.
    pub local_port: u16,
    /// Remote port inside the container, normally 8000.
    pub remote_port: u16,
    /// `sshN.vast.ai`. Recycled by vast, which is why we keep a dedicated `known_hosts`.
    pub ssh_host: String,
    /// SSH port.
    pub ssh_port: u16,
}

/// A tunnel's computed state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TunnelStatus {
    /// What it is forwarding.
    pub spec: TunnelSpec,
    /// Whether the forward is currently up.
    pub up: bool,
    /// Identity of the `ssh` child, so a daemon restart re-adopts rather than colliding.
    #[serde(default)]
    pub proc: Option<ProcFacts>,
    /// When it came up, unix seconds.
    #[serde(default)]
    pub since_unix: Option<i64>,
    /// How many times the supervisor has reconnected it.
    pub restarts: u32,
    /// The last failure.
    #[serde(default)]
    pub last_error: Option<String>,
}

/// Exactly what we are about to exec, before we exec it. Rendered by `endpoint argv`,
/// `GET /v1/endpoints/{id}/argv` and the Launch drawer.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArgvPreview {
    /// The binary.
    pub program: String,
    /// argv, excluding argv[0]. **No credential ever appears here.**
    #[serde(default)]
    pub args: Vec<String>,
    /// The child's environment additions, including `LD_LIBRARY_PATH`.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Working directory. `$STATE`, and never load-bearing.
    pub cwd: String,
    /// Anything the operator should read before confirming.
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// What a swap actually did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapReport {
    /// Which alias moved.
    pub alias: Alias,
    /// Hot or sequential — chosen by `fit()` unless overridden.
    pub mode: SwapMode,
    /// Where it pointed before.
    #[serde(default)]
    pub from: Option<BackendId>,
    /// Where it points now.
    pub to: BackendId,
    /// How many requests parked on the `Notify` during a sequential swap.
    pub parked: u32,
    /// How long draining the old backend took.
    pub drained_ms: u32,
    /// Wall clock for the whole operation.
    pub total_ms: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::BackendId;

    fn local_spec() -> LocalLlamaSpec {
        LocalLlamaSpec {
            build: BuildId::parse("build-vulkan").expect("id"),
            model_path: "/home/andre/models/carnice-9b/Carnice-9b-Q6_K.gguf".into(),
            mmproj: None,
            alias_flag: "Carnice-9b-Q6_K".into(),
            host: "127.0.0.1".into(),
            port: Some(8100),
            ctx: None,
            parallel: Some(4),
            kv_type: Some(KvType::Q8_0),
            ngl: NglPlan::Auto,
            split: SplitPlan::default(),
            mode: SamplingMode::Thinking,
            flash_attn: Some(TriState::Auto),
            api_key: None,
            extra_args: vec![],
        }
    }

    #[test]
    fn endpoint_spec_round_trips_every_variant() {
        let specs = [
            EndpointSpec::LocalLlama(local_spec()),
            EndpointSpec::LocalVllm(LocalVllmSpec {
                bin: "/usr/bin/vllm".into(),
                model_id: "Qwen/Qwen3-8B".into(),
                tp: Some(2),
                ctx: Some(32_768),
                quantization: None,
                kv_cache_dtype: Some("fp8".into()),
                enforce_eager: false,
                reasoning_parser: None,
                gpu_util: Some(0.92),
                max_num_seqs: Some(64),
                trust_remote: true,
                chunked_prefill: true,
                host: "127.0.0.1".into(),
                port: None,
                devices: vec!["CUDA0".into()],
                extra_args: vec![],
            }),
            EndpointSpec::Node(NodeSpec {
                base_url: "http://192.168.1.44:8080".into(),
                credential: CredentialSource::None,
                label: "the workshop box".into(),
                declared_models: vec!["gpt-oss-20b".into()],
                protocol: Protocol::OpenAi,
            }),
            EndpointSpec::Managed(ManagedSpec {
                provider: ProviderId::parse("together").expect("id"),
                base_url: "https://api.together.xyz".into(),
                credential: CredentialSource::Env {
                    var: "TOGETHER_API_KEY".into(),
                },
                model_id: Some("meta-llama/Llama-3.3-70B-Instruct-Turbo".into()),
                protocol: Protocol::OpenAi,
            }),
        ];
        for spec in &specs {
            let s = serde_json::to_string(spec).expect("ser");
            assert_eq!(&serde_json::from_str::<EndpointSpec>(&s).expect("de"), spec);
        }
    }

    #[test]
    fn spec_kind_maps_vast_runtime_to_the_right_backend_kind() {
        assert_eq!(
            EndpointSpec::LocalLlama(local_spec()).kind(),
            BackendKind::LocalLlama
        );
        let vast = |runtime| {
            EndpointSpec::Vast(VastSpec {
                instance_id: InstanceId(28_675_431),
                runtime,
                launch: ContainerLaunch {
                    runtime,
                    image: "ghcr.io/buckster123/vastai-gguf:prebuilt".into(),
                    image_type: crate::catalog::ImageType::Prebuilt,
                    disk_gb: 80,
                    env: Default::default(),
                    onstart: "bash /app/launch.sh > /var/log/launch.log 2>&1 &".into(),
                    host: "127.0.0.1".into(),
                    port: 8000,
                    expose_public: false,
                },
                tunnel: None,
            })
        };
        assert_eq!(
            vast(ContainerRuntime::LlamaCpp).kind(),
            BackendKind::VastLlama
        );
        assert_eq!(vast(ContainerRuntime::Vllm).kind(), BackendKind::VastVllm);
    }

    #[test]
    fn suggested_ids_are_always_valid_backend_ids() {
        let cases = [
            EndpointSpec::LocalLlama(local_spec()),
            EndpointSpec::LocalVllm(LocalVllmSpec {
                bin: "vllm".into(),
                model_id: "Qwen/Qwen3-8B".into(),
                tp: None,
                ctx: None,
                quantization: None,
                kv_cache_dtype: None,
                enforce_eager: false,
                reasoning_parser: None,
                gpu_util: None,
                max_num_seqs: None,
                trust_remote: false,
                chunked_prefill: false,
                host: "127.0.0.1".into(),
                port: None,
                devices: vec![],
                extra_args: vec![],
            }),
            EndpointSpec::Node(NodeSpec {
                base_url: "http://192.168.1.44:8080/v1".into(),
                credential: CredentialSource::None,
                label: "lan".into(),
                declared_models: vec![],
                protocol: Protocol::OpenAi,
            }),
            EndpointSpec::Managed(ManagedSpec {
                provider: ProviderId::parse("together").expect("id"),
                base_url: "https://api.together.xyz".into(),
                credential: CredentialSource::None,
                model_id: Some("meta-llama/Llama-3.3-70B-Instruct-Turbo".into()),
                protocol: Protocol::OpenAi,
            }),
        ];
        for spec in &cases {
            let id = spec.suggested_id();
            assert!(
                BackendId::parse(&id).is_ok(),
                "suggested_id {id:?} is not a valid BackendId"
            );
        }
        assert_eq!(
            EndpointSpec::LocalLlama(local_spec()).suggested_id(),
            "local-carnice-9b-q6_k"
        );
    }

    #[test]
    fn endpoint_record_round_trips_and_has_no_status_field() {
        let rec = EndpointRecord {
            id: BackendId::parse("local-carnice").expect("id"),
            spec: EndpointSpec::LocalLlama(local_spec()),
            desired: DesiredState::Running,
            proc: Some(ProcFacts {
                pid: 41_233,
                start_time_ticks: 8_123_441,
                boot_id: "6a1f...".into(),
                exe: "/home/andre/llama.cpp/build-vulkan/bin/llama-server".into(),
                cmdline_sha256: "deadbeef".into(),
            }),
            port: Some(8100),
            log_path: Some("/home/andre/.local/state/apexrouter/logs/local-carnice.log".into()),
            started_at_unix: 1_785_412_331,
            fit: None,
            adopted: false,
            alias_bindings: vec![Alias::parse("auto").expect("alias")],
        };
        let s = serde_json::to_string(&rec).expect("ser");
        assert!(
            !s.contains("\"status\""),
            "EndpointRecord must never carry a status string: {s}"
        );
        assert_eq!(serde_json::from_str::<EndpointRecord>(&s).expect("de"), rec);
    }

    #[test]
    fn tunnel_and_argv_and_swap_round_trip() {
        let t = TunnelStatus {
            spec: TunnelSpec {
                instance_id: InstanceId(1),
                local_port: 8800,
                remote_port: 8000,
                ssh_host: "ssh4.vast.ai".into(),
                ssh_port: 22,
            },
            up: true,
            proc: None,
            since_unix: Some(1_785_412_331),
            restarts: 2,
            last_error: None,
        };
        let s = serde_json::to_string(&t).expect("ser");
        assert_eq!(serde_json::from_str::<TunnelStatus>(&s).expect("de"), t);

        let a = ArgvPreview {
            program: "/home/andre/llama.cpp/build-vulkan/bin/llama-server".into(),
            args: vec!["-m".into(), "/models/x.gguf".into()],
            env: vec![(
                "LD_LIBRARY_PATH".into(),
                "/home/andre/llama.cpp/build-vulkan/bin".into(),
            )],
            cwd: "/home/andre/.local/state/apexrouter".into(),
            warnings: vec![],
        };
        let s = serde_json::to_string(&a).expect("ser");
        assert_eq!(serde_json::from_str::<ArgvPreview>(&s).expect("de"), a);

        let r = SwapReport {
            alias: Alias::parse("auto").expect("alias"),
            mode: SwapMode::Hot,
            from: Some(BackendId::parse("local-a").expect("id")),
            to: BackendId::parse("local-b").expect("id"),
            parked: 0,
            drained_ms: 120,
            total_ms: 4_300,
        };
        let s = serde_json::to_string(&r).expect("ser");
        assert_eq!(serde_json::from_str::<SwapReport>(&s).expect("de"), r);
    }
}
