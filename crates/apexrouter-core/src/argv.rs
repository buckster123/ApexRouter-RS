//! OWNER: unit C-12 (core/argv.rs). Do not edit outside that unit.
//!
//! **ONE** argv/env builder, both targets.
//!
//! The local argv and the container env map come out of the same code, so the `--top-k 20`
//! divergence between `config.SAMPLING_PRESETS` and `launch.sh` cannot recur. `launch.sh` is
//! authoritative: **all three presets include `--top-k 20`.**
//!
//! Two rules that are not negotiable:
//!
//! * A flag is emitted **only if** `build.flags.has(flag)`. `--jinja` is never emitted when
//!   `jinja_default_on` — `--no-jinja` is the meaningful flag on b9199.
//! * **No credential ever reaches argv.** `llama-server` gets `--api-key-file` (a `0600`
//!   file in `$STATE`) or `LLAMA_ARG_API_KEY`; `HF_TOKEN` goes in the vast `env` **map** and
//!   never in the `--onstart-cmd` string, which vast persists and echoes back.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::paths::Paths;
use crate::secret::Secret;
use apexrouter_protocol::{
    ArgvPreview, ContainerEnvPreview, ContainerLaunch, ContainerRuntime, GpuBackend, ImageType,
    KvType, LlamaBuild, LocalLlamaSpec, LocalVllmSpec, NglPlan, SamplingMode, SplitMode, TriState,
};
use std::collections::BTreeMap;
use std::path::Path;

// ---------------------------------------------------------------------------------------
// Sampling presets. `launch.sh` is authoritative; ALL THREE carry `--top-k 20`.
// ---------------------------------------------------------------------------------------

/// `thinking` — the default preset.
const THINKING: &[&str] = &[
    "--temp",
    "1.0",
    "--top-p",
    "0.95",
    "--top-k",
    "20",
    "--min-p",
    "0.0",
    "--presence-penalty",
    "1.5",
];

/// `coding` — lower temperature, no presence penalty.
const CODING: &[&str] = &[
    "--temp",
    "0.6",
    "--top-p",
    "0.95",
    "--top-k",
    "20",
    "--min-p",
    "0.0",
    "--presence-penalty",
    "0.0",
];

/// `nonthinking` — thinking disabled through the chat template.
const NONTHINKING: &[&str] = &[
    "--temp",
    "0.7",
    "--top-p",
    "0.80",
    "--top-k",
    "20",
    "--min-p",
    "0.0",
    "--presence-penalty",
    "1.5",
    "--chat-template-kwargs",
    "{\"enable_thinking\":false}",
];

/// `raw` — emit nothing at all.
const RAW: &[&str] = &[];

// ---------------------------------------------------------------------------------------
// The container variable contracts (ARCHITECTURE.md §3.7). Order is display order.
// ---------------------------------------------------------------------------------------

/// `launch.sh`'s 16 variables, in the order every surface renders them.
const LLAMA_VARS: [&str; 16] = [
    "MODEL_REPO",
    "MODEL_QUANT",
    "IMAGE_TYPE",
    "CTX",
    "KV_TYPE",
    "MODE",
    "N_GPU_LAYERS",
    "PARALLEL",
    "EXTRA_ARGS",
    "HF_TOKEN",
    "MODELS_DIR",
    "MMPROJ",
    "PORT",
    "HOST",
    "LLAMA_CPP_REPO",
    "LLAMA_CPP_REF",
];

/// `launch_vllm.sh`'s 16 variables, in the order every surface renders them.
const VLLM_VARS: [&str; 16] = [
    "MODEL_ID",
    "TP",
    "CTX",
    "QUANTIZATION",
    "KV_CACHE_DTYPE",
    "GPU_UTIL",
    "EXTRA_ARGS",
    "HF_TOKEN",
    "PORT",
    "HOST",
    "DTYPE",
    "MAX_NUM_SEQS",
    "TRUST_REMOTE",
    "ENFORCE_EAGER",
    "CHUNKED_PREFILL",
    "REASONING_PARSER",
];

/// The port the container serves on. Fixed by both launch scripts.
const CONTAINER_PORT: u16 = 8000;

/// The loopback posture: everything reaches a rented box through the SSH tunnel.
const LOOPBACK: &str = "127.0.0.1";

/// What the image is told to do instead of its own `ENTRYPOINT`, so exactly one server
/// starts on port 8000.
const ARGS_OVERRIDE: [&str; 2] = ["sleep", "infinity"];

/// Emitted when `known_forks` hits.
const FORK_WARNING: &str = "custom fork → builder image → +12–18 min cold start";

/// Emitted whenever the resolved image family is `Builder`.
const BUILDER_WARNING: &str = "builder image → +12–18 min cold start (pull + SM compile)";

/// Upstream llama.cpp, used when no `known_forks` entry matches.
const DEFAULT_LLAMA_REPO: &str = "ggml-org/llama.cpp";

/// The default ref of [`DEFAULT_LLAMA_REPO`].
const DEFAULT_LLAMA_REF: &str = "master";

// ---------------------------------------------------------------------------------------
// Local llama.cpp
// ---------------------------------------------------------------------------------------

/// Build the exact argv and env for one local `llama-server`.
///
/// Emits `-m`, `--host`, `--port`, `-a`, `-dev`, `-sm`, `-mg`, `--tensor-split`,
/// `-c` *(omitted when `ctx` is `None`, so llama.cpp's own `--fit` can size it)*, `-np`,
/// `-ctk`, `-ctv`, `-ngl` *(omitted when `NglPlan::Auto`)*, `-fa on|off|auto`, `--no-jinja`,
/// `--metrics`, `--props`, `--slots`, `--mmproj`, `--api-key-file`, the sampling preset,
/// then `extra_args`.
///
/// `LD_LIBRARY_PATH = dirname(server_path)` is **always** in the env: `build-vulkan`'s
/// trailing-colon RUNPATH otherwise picks up a sibling build's `.so`.
///
/// # Errors
/// Returns [`Error::Invalid`] when the state root cannot be resolved for `cwd`.
pub fn plan_local(
    spec: &LocalLlamaSpec,
    build: &LlamaBuild,
    key_file: Option<&Path>,
) -> Result<ArgvPreview> {
    let mut args: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    // A flag is emitted ONLY if the probed FlagSupport advertises it. Every intent we had
    // to drop becomes a warning, so the operator sees the difference in the preview
    // instead of discovering it in a log tail.
    let emit =
        |args: &mut Vec<String>, warnings: &mut Vec<String>, flag: &str, val: Option<&str>| {
            if build.flags.has(flag) {
                args.push(flag.to_owned());
                if let Some(v) = val {
                    args.push(v.to_owned());
                }
            } else {
                warnings.push(format!(
                    "{} does not advertise `{flag}`; it was not emitted",
                    build.label
                ));
            }
        };

    // -m
    emit(
        &mut args,
        &mut warnings,
        "-m",
        Some(spec.model_path.as_str()),
    );

    // --host / --port
    emit(&mut args, &mut warnings, "--host", Some(spec.host.as_str()));
    match spec.port {
        Some(p) => {
            emit(&mut args, &mut warnings, "--port", Some(&p.to_string()));
        }
        None => warnings.push(
            "no port assigned yet: the supervisor allocates one from `[endpoints] port_range` \
             before exec"
                .to_owned(),
        ),
    }

    // -a
    if !spec.alias_flag.is_empty() {
        emit(
            &mut args,
            &mut warnings,
            "-a",
            Some(spec.alias_flag.as_str()),
        );
    }

    // -dev / -sm / -mg / --tensor-split, all from SplitPlan.
    if !spec.split.devices.is_empty() {
        emit(
            &mut args,
            &mut warnings,
            "-dev",
            Some(&spec.split.devices.join(",")),
        );
    }
    emit(
        &mut args,
        &mut warnings,
        "-sm",
        Some(split_mode_flag(spec.split.mode)),
    );
    if let Some(mg) = spec.split.main_gpu {
        emit(&mut args, &mut warnings, "-mg", Some(&mg.to_string()));
    }
    if let Some(ts) = spec.split.tensor_split.as_ref().filter(|t| !t.is_empty()) {
        let joined = ts
            .iter()
            .map(|f| format_ratio(*f))
            .collect::<Vec<_>>()
            .join(",");
        emit(&mut args, &mut warnings, "--tensor-split", Some(&joined));
    }

    // -c is OMITTED when ctx is None, so llama.cpp's own --fit can size the context.
    if let Some(ctx) = spec.ctx {
        emit(&mut args, &mut warnings, "-c", Some(&ctx.to_string()));
    }

    // -np
    if let Some(np) = spec.parallel {
        emit(&mut args, &mut warnings, "-np", Some(&np.to_string()));
    }

    // -ctk / -ctv — one type, applied to both halves of the cache.
    if let Some(kv) = spec.kv_type {
        emit(&mut args, &mut warnings, "-ctk", Some(kv.as_flag()));
        emit(&mut args, &mut warnings, "-ctv", Some(kv.as_flag()));
    }

    // -ngl is OMITTED for NglPlan::Auto, for the same reason as -c.
    match spec.ngl {
        NglPlan::Auto => {}
        NglPlan::All => {
            emit(&mut args, &mut warnings, "-ngl", Some("999"));
        }
        NglPlan::Layers(n) => {
            emit(&mut args, &mut warnings, "-ngl", Some(&n.to_string()));
        }
    }

    // -fa on|off|auto on a tristate build; the bare switch only means "on".
    if let Some(fa) = spec.flash_attn {
        if build.flags.fa_tristate {
            emit(&mut args, &mut warnings, "-fa", Some(tri_state_flag(fa)));
        } else if matches!(fa, TriState::On) {
            emit(&mut args, &mut warnings, "-fa", None);
        } else {
            warnings.push(format!(
                "{} takes a bare `-fa`, so `-fa {}` was not emitted",
                build.label,
                tri_state_flag(fa)
            ));
        }
    }

    // Jinja. On b9199 `--jinja` is default-ON, so passing it is a no-op and we never do.
    // `--no-jinja` is the meaningful flag and reaches argv only through `extra_args`.
    let user_disabled_jinja = spec.extra_args.iter().any(|a| a == "--no-jinja");
    if !build.flags.jinja_default_on && !user_disabled_jinja && build.flags.has("--jinja") {
        args.push("--jinja".to_owned());
    }

    // Observability. All three are read by the router and never proxied outward.
    for flag in ["--metrics", "--props", "--slots"] {
        if build.flags.has(flag) {
            args.push(flag.to_owned());
        }
    }

    // --mmproj
    if let Some(mm) = spec.mmproj.as_deref() {
        emit(&mut args, &mut warnings, "--mmproj", Some(mm));
    }

    // --api-key-file. NEVER `--api-key <secret>`: that lands in /proc/<pid>/cmdline.
    match key_file {
        Some(f) => {
            emit(
                &mut args,
                &mut warnings,
                "--api-key-file",
                Some(&f.display().to_string()),
            );
        }
        None => {
            if spec.api_key.is_some() {
                warnings.push(
                    "the spec declares an api key but no key file was materialised; the server \
                     will start unauthenticated"
                        .to_owned(),
                );
            }
        }
    }

    // The sampling preset, pair by pair, each half gated like every other flag.
    for pair in sampling_flags(spec.mode).chunks(2) {
        let Some(flag) = pair.first() else { continue };
        if build.flags.has(flag) {
            args.push((*flag).to_owned());
            if let Some(v) = pair.get(1) {
                args.push((*v).to_owned());
            }
        } else {
            warnings.push(format!(
                "{} does not advertise `{flag}`; the sampling preset is incomplete",
                build.label
            ));
        }
    }

    // extra_args are the operator's own words and go through verbatim, last.
    args.extend(spec.extra_args.iter().cloned());

    let mut env = vec![("LD_LIBRARY_PATH".to_owned(), lib_dir(&build.server_path))];
    if let Some(backend) = primary_backend(build) {
        let devices = if spec.split.devices.is_empty() {
            build.devices.as_slice()
        } else {
            spec.split.devices.as_slice()
        };
        env.extend(backend_env(backend, devices));
    }

    Ok(ArgvPreview {
        program: build.server_path.clone(),
        args,
        env,
        cwd: state_cwd()?,
        warnings,
    })
}

// ---------------------------------------------------------------------------------------
// Local vLLM
// ---------------------------------------------------------------------------------------

/// Build the exact argv and env for one local vLLM.
///
/// Mirrors `launch_vllm.sh` flag for flag, so a local vLLM and a rented one differ only in
/// where they run. `vllm` itself needs the `serve` subcommand; a wrapper binary does not.
///
/// # Errors
/// Returns [`Error::Invalid`] when the state root cannot be resolved for `cwd`.
pub fn plan_local_vllm(spec: &LocalVllmSpec) -> Result<ArgvPreview> {
    let mut args: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    if Path::new(&spec.bin)
        .file_name()
        .is_some_and(|f| f == "vllm")
    {
        args.push("serve".to_owned());
    }

    args.push("--model".to_owned());
    args.push(spec.model_id.clone());

    if let Some(tp) = spec.tp {
        args.push("--tensor-parallel-size".to_owned());
        args.push(tp.to_string());
    }
    if let Some(ctx) = spec.ctx {
        args.push("--max-model-len".to_owned());
        args.push(ctx.to_string());
    }
    if let Some(util) = spec.gpu_util {
        args.push("--gpu-memory-utilization".to_owned());
        args.push(format_ratio(util));
    }
    if let Some(n) = spec.max_num_seqs {
        args.push("--max-num-seqs".to_owned());
        args.push(n.to_string());
    }

    args.push("--host".to_owned());
    args.push(spec.host.clone());
    match spec.port {
        Some(p) => {
            args.push("--port".to_owned());
            args.push(p.to_string());
        }
        None => warnings.push(
            "no port assigned yet: the supervisor allocates one from `[endpoints] port_range` \
             before exec"
                .to_owned(),
        ),
    }

    if spec.trust_remote {
        args.push("--trust-remote-code".to_owned());
    }
    if let Some(q) = spec
        .quantization
        .as_deref()
        .filter(|q| meaningful(q, "none"))
    {
        args.push("--quantization".to_owned());
        args.push(q.to_owned());
    }
    if let Some(k) = spec
        .kv_cache_dtype
        .as_deref()
        .filter(|k| meaningful(k, "auto"))
    {
        args.push("--kv-cache-dtype".to_owned());
        args.push(k.to_owned());
    }
    if spec.enforce_eager {
        args.push("--enforce-eager".to_owned());
    }
    if spec.chunked_prefill {
        args.push("--enable-chunked-prefill".to_owned());
    }
    if let Some(r) = spec.reasoning_parser.as_deref().filter(|r| !r.is_empty()) {
        args.push("--enable-reasoning".to_owned());
        args.push("--reasoning-parser".to_owned());
        args.push(r.to_owned());
    }

    args.extend(spec.extra_args.iter().cloned());

    // vLLM is CUDA-only in practice, so the device list is a CUDA_VISIBLE_DEVICES.
    let env = backend_env(GpuBackend::Cuda, &spec.devices);

    Ok(ArgvPreview {
        program: spec.bin.clone(),
        args,
        env,
        cwd: state_cwd()?,
        warnings,
    })
}

// ---------------------------------------------------------------------------------------
// Containers
// ---------------------------------------------------------------------------------------

/// Build the container contract for a rented box.
///
/// Emits exactly the 16 llama.cpp vars or the 16 vLLM vars of `ARCHITECTURE.md` §3.7,
/// forces `HOST=127.0.0.1` unless `expose_public`, resolves the image from `[docker]` by
/// `image_type`, applies `known_forks` (forcing `Builder` and pushing the
/// "+12–18 min cold start" warning), and sets `args_override` so the image's own
/// `ENTRYPOINT` cannot start a second server on port 8000.
///
/// `HF_TOKEN` is placed in [`ContainerLaunch::env`] and **never** in `onstart`, which vast
/// persists and echoes back in `show instance`. The returned [`ContainerEnvPreview`] is a
/// *display* object and carries `***` in its place.
///
/// # Errors
/// Returns [`Error::Invalid`] when a required variable is missing (`MODEL_REPO`/
/// `MODEL_QUANT` for llama.cpp, `MODEL_ID` for vLLM) or when a llama.cpp launch asks for
/// [`SamplingMode::Raw`], which `launch.sh` treats as fatal.
pub fn plan_container(
    launch_in: &ContainerLaunchInput,
    cfg: &Config,
) -> Result<(ContainerLaunch, ContainerEnvPreview)> {
    let mut warnings: Vec<String> = Vec::new();

    // ---- image family -------------------------------------------------------------
    let mut image_type = launch_in.image_type.unwrap_or(match launch_in.runtime {
        ContainerRuntime::LlamaCpp => ImageType::Prebuilt,
        ContainerRuntime::Vllm => ImageType::Vllm,
    });
    match launch_in.runtime {
        ContainerRuntime::Vllm if image_type != ImageType::Vllm => {
            warnings.push(
                "vLLM runtime forces the vllm image; the requested image family was ignored"
                    .to_owned(),
            );
            image_type = ImageType::Vllm;
        }
        ContainerRuntime::LlamaCpp if image_type == ImageType::Vllm => {
            warnings.push(
                "llama.cpp runtime cannot use the vllm image; falling back to the prebuilt image"
                    .to_owned(),
            );
            image_type = ImageType::Prebuilt;
        }
        _ => {}
    }

    let host = if launch_in.expose_public {
        warnings.push(
            "expose_public: a vast direct port is plaintext HTTP on a shared public IP — a \
             freshly minted per-instance api key is REQUIRED"
                .to_owned(),
        );
        "0.0.0.0".to_owned()
    } else {
        LOOPBACK.to_owned()
    };

    let mut env: BTreeMap<String, String> = BTreeMap::new();
    let onstart;

    match launch_in.runtime {
        ContainerRuntime::LlamaCpp => {
            let repo = required(launch_in.model_repo.as_deref(), "MODEL_REPO")?;
            let quant = required(launch_in.model_quant.as_deref(), "MODEL_QUANT")?;

            // known_forks is a READ table: a hit forces the builder image and names the
            // fork to compile, and the cost of that decision is a warning, not a surprise.
            let (llama_repo, llama_ref) = match cfg.known_fork_for(repo) {
                Some(fork) => {
                    image_type = ImageType::Builder;
                    warnings.push(FORK_WARNING.to_owned());
                    (fork.llama_cpp_repo.clone(), fork.llama_cpp_ref.clone())
                }
                None => (DEFAULT_LLAMA_REPO.to_owned(), DEFAULT_LLAMA_REF.to_owned()),
            };

            env.insert("MODEL_REPO".into(), repo.to_owned());
            env.insert("MODEL_QUANT".into(), quant.to_owned());
            env.insert("IMAGE_TYPE".into(), image_type_flag(image_type).to_owned());
            env.insert("CTX".into(), launch_in.ctx.unwrap_or(65_536).to_string());
            env.insert(
                "KV_TYPE".into(),
                launch_in
                    .kv_type
                    .unwrap_or(KvType::Q8_0)
                    .as_flag()
                    .to_owned(),
            );
            env.insert("MODE".into(), container_mode(launch_in.mode)?.to_owned());
            env.insert("N_GPU_LAYERS".into(), "999".to_owned());
            env.insert(
                "PARALLEL".into(),
                launch_in.parallel.unwrap_or(1).to_string(),
            );
            env.insert("EXTRA_ARGS".into(), String::new());
            env.insert("HF_TOKEN".into(), token_value(launch_in));
            env.insert("MODELS_DIR".into(), "/workspace/models".to_owned());
            env.insert(
                "MMPROJ".into(),
                launch_in.mmproj.clone().unwrap_or_default(),
            );
            env.insert("PORT".into(), CONTAINER_PORT.to_string());
            env.insert("HOST".into(), host.clone());
            env.insert("LLAMA_CPP_REPO".into(), llama_repo);
            env.insert("LLAMA_CPP_REF".into(), llama_ref);

            onstart = "bash /app/launch.sh > /var/log/launch.log 2>&1 &".to_owned();
        }
        ContainerRuntime::Vllm => {
            let model_id = required(launch_in.model_id.as_deref(), "MODEL_ID")?;

            env.insert("MODEL_ID".into(), model_id.to_owned());
            env.insert(
                "TP".into(),
                launch_in.tp.map(|t| t.to_string()).unwrap_or_default(),
            );
            env.insert("CTX".into(), launch_in.ctx.unwrap_or(131_072).to_string());
            env.insert(
                "QUANTIZATION".into(),
                launch_in.quantization.clone().unwrap_or_default(),
            );
            env.insert(
                "KV_CACHE_DTYPE".into(),
                launch_in
                    .kv_cache_dtype
                    .clone()
                    .unwrap_or_else(|| "auto".to_owned()),
            );
            env.insert("GPU_UTIL".into(), "0.95".to_owned());
            env.insert("EXTRA_ARGS".into(), String::new());
            env.insert("HF_TOKEN".into(), token_value(launch_in));
            env.insert("PORT".into(), CONTAINER_PORT.to_string());
            env.insert("HOST".into(), host.clone());
            env.insert("DTYPE".into(), "auto".to_owned());
            env.insert("MAX_NUM_SEQS".into(), "64".to_owned());
            env.insert("TRUST_REMOTE".into(), "true".to_owned());
            // The contract compares these to the literal strings, never to a JSON bool.
            env.insert(
                "ENFORCE_EAGER".into(),
                bool_flag(launch_in.enforce_eager).to_owned(),
            );
            env.insert("CHUNKED_PREFILL".into(), "true".to_owned());
            env.insert(
                "REASONING_PARSER".into(),
                launch_in.reasoning_parser.clone().unwrap_or_default(),
            );

            onstart = "bash /app/launch_vllm.sh > /var/log/launch.log 2>&1 &".to_owned();
        }
    }

    if image_type == ImageType::Builder && !warnings.iter().any(|w| w == FORK_WARNING) {
        warnings.push(BUILDER_WARNING.to_owned());
    }

    let image = cfg.image_for(image_type);

    let launch = ContainerLaunch {
        runtime: launch_in.runtime,
        image: image.clone(),
        image_type,
        disk_gb: launch_in.disk_gb,
        env: env.clone(),
        onstart: onstart.clone(),
        host,
        port: CONTAINER_PORT,
        expose_public: launch_in.expose_public,
    };

    // The preview is rendered and logged, so the token is redacted out of it. The real
    // value only ever travels in `ContainerLaunch::env`.
    let order: &[&str] = match launch_in.runtime {
        ContainerRuntime::LlamaCpp => &LLAMA_VARS,
        ContainerRuntime::Vllm => &VLLM_VARS,
    };
    let preview_env = order
        .iter()
        .map(|k| {
            let raw = env.get(*k).cloned().unwrap_or_default();
            let shown = if *k == "HF_TOKEN" && !raw.is_empty() {
                "***".to_owned()
            } else {
                raw
            };
            ((*k).to_owned(), shown)
        })
        .collect();

    let preview = ContainerEnvPreview {
        env: preview_env,
        onstart,
        image,
        args_override: ARGS_OVERRIDE.iter().map(|s| (*s).to_owned()).collect(),
        warnings,
    };

    Ok((launch, preview))
}

/// Everything a container launch needs before the builder resolves it.
#[derive(Clone, Debug)]
pub struct ContainerLaunchInput {
    /// llama.cpp or vLLM.
    pub runtime: ContainerRuntime,
    /// `None` lets `known_forks` and the runtime pick.
    pub image_type: Option<ImageType>,
    /// HF repo for a GGUF launch.
    pub model_repo: Option<String>,
    /// Which quant within the repo.
    pub model_quant: Option<String>,
    /// HF model id for a vLLM launch.
    pub model_id: Option<String>,
    /// Total context pool.
    pub ctx: Option<u32>,
    /// Slot count.
    pub parallel: Option<u32>,
    /// KV element type.
    pub kv_type: Option<KvType>,
    /// Sampling preset.
    pub mode: SamplingMode,
    /// Vision projector filename within the repo.
    pub mmproj: Option<String>,
    /// Disk to request.
    pub disk_gb: u32,
    /// Tensor parallelism (vLLM).
    pub tp: Option<u32>,
    /// `--quantization` (vLLM).
    pub quantization: Option<String>,
    /// `--kv-cache-dtype` (vLLM).
    pub kv_cache_dtype: Option<String>,
    /// The container contract sends this as the literal string `"true"`/`"false"`.
    pub enforce_eager: bool,
    /// `--reasoning-parser` (vLLM).
    pub reasoning_parser: Option<String>,
    /// Opt in to a public direct port. **Requires** a freshly minted per-instance api key.
    pub expose_public: bool,
    /// Goes in the env **map**, never in `onstart`.
    pub hf_token: Option<Secret<String>>,
}

/// The flags for one sampling preset. All three include `--top-k 20`; `nonthinking` also
/// emits `--chat-template-kwargs {"enable_thinking":false}`.
pub fn sampling_flags(mode: SamplingMode) -> &'static [&'static str] {
    match mode {
        SamplingMode::Thinking => THINKING,
        SamplingMode::Coding => CODING,
        SamplingMode::Nonthinking => NONTHINKING,
        SamplingMode::Raw => RAW,
    }
}

/// `GGML_VK_VISIBLE_DEVICES` / `HIP_VISIBLE_DEVICES` / `CUDA_VISIBLE_DEVICES`, chosen from
/// the build's detected backend. `-dev` carries the explicit device list regardless.
///
/// An empty `devices` returns an empty vector: masking every device is not the same thing
/// as expressing no preference, and the former would silently strand a launch on the CPU.
pub fn backend_env(backend: GpuBackend, devices: &[String]) -> Vec<(String, String)> {
    let var = match backend {
        GpuBackend::Vulkan => "GGML_VK_VISIBLE_DEVICES",
        GpuBackend::Cuda => "CUDA_VISIBLE_DEVICES",
        GpuBackend::Rocm | GpuBackend::Hip => "HIP_VISIBLE_DEVICES",
        GpuBackend::Metal | GpuBackend::Sycl | GpuBackend::Cpu | GpuBackend::Other(_) => {
            return Vec::new()
        }
    };
    if devices.is_empty() {
        return Vec::new();
    }
    let list = devices
        .iter()
        .map(|d| device_index(d))
        .collect::<Vec<_>>()
        .join(",");
    vec![(var.to_owned(), list)]
}

// ---------------------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------------------

/// `$STATE` as a string. The child's cwd is never load-bearing, but it must exist.
fn state_cwd() -> Result<String> {
    Ok(Paths::resolve()?.state().display().to_string())
}

/// `dirname(server_path)` — the whole point of the always-present `LD_LIBRARY_PATH`.
fn lib_dir(server_path: &str) -> String {
    Path::new(server_path)
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

/// The first non-CPU backend a build enumerated, which is the one whose device mask matters.
fn primary_backend(build: &LlamaBuild) -> Option<GpuBackend> {
    build
        .backends
        .iter()
        .find(|b| **b != GpuBackend::Cpu)
        .cloned()
}

/// `"Vulkan1"` -> `"1"`. A token with no trailing digits is passed through untouched.
fn device_index(token: &str) -> String {
    let digits: String = token
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if digits.is_empty() {
        token.to_owned()
    } else {
        digits
    }
}

/// The `-sm` value.
fn split_mode_flag(mode: SplitMode) -> &'static str {
    match mode {
        SplitMode::None => "none",
        SplitMode::Layer => "layer",
        SplitMode::Row => "row",
        SplitMode::Tensor => "tensor",
    }
}

/// The `-fa` value on a tristate build.
fn tri_state_flag(t: TriState) -> &'static str {
    match t {
        TriState::On => "on",
        TriState::Off => "off",
        TriState::Auto => "auto",
    }
}

/// The `IMAGE_TYPE` value, matching the serde spelling.
fn image_type_flag(t: ImageType) -> &'static str {
    match t {
        ImageType::Prebuilt => "prebuilt",
        ImageType::Builder => "builder",
        ImageType::Vllm => "vllm",
        ImageType::Studio => "studio",
    }
}

/// The `MODE` value. `launch.sh` dies on anything else, so `Raw` is a hard error here
/// rather than a silent no-preset launch, which is how the previous generation lost
/// `--top-k` without anyone noticing.
fn container_mode(mode: SamplingMode) -> Result<&'static str> {
    match mode {
        SamplingMode::Thinking => Ok("thinking"),
        SamplingMode::Coding => Ok("coding"),
        SamplingMode::Nonthinking => Ok("nonthinking"),
        SamplingMode::Raw => Err(Error::Invalid {
            what: "sampling mode".into(),
            why: "launch.sh has no `raw` preset and exits fatally on an unknown MODE".into(),
        }),
    }
}

/// The literal string the container contract compares against.
fn bool_flag(b: bool) -> &'static str {
    if b {
        "true"
    } else {
        "false"
    }
}

/// The token as it goes into the env **map**. Empty when there is none.
fn token_value(launch_in: &ContainerLaunchInput) -> String {
    launch_in
        .hf_token
        .as_ref()
        .map(|t| t.expose().to_owned())
        .unwrap_or_default()
}

/// A required container variable, or a named error.
fn required<'a>(v: Option<&'a str>, name: &str) -> Result<&'a str> {
    v.filter(|s| !s.is_empty()).ok_or_else(|| Error::Invalid {
        what: "container launch".into(),
        why: format!("{name} is required and was not supplied"),
    })
}

/// True when a value is worth passing on: non-empty and not the script's own "unset" word.
fn meaningful(v: &str, unset: &str) -> bool {
    !v.is_empty() && !v.eq_ignore_ascii_case(unset)
}

/// A ratio rendered without a trailing `.0`-less integer surprise: `0.92`, `1`, `0.5`.
fn format_ratio(f: f32) -> String {
    format!("{f}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_protocol::{BuildId, FlagSupport, SplitPlan};
    use std::collections::BTreeSet;

    /// Every flag the argv builder can emit, as a real b9199 `--help` advertises them.
    fn full_flags() -> FlagSupport {
        let flags: BTreeSet<String> = [
            "-m",
            "--host",
            "--port",
            "-a",
            "-dev",
            "-sm",
            "-mg",
            "--tensor-split",
            "-c",
            "-np",
            "-ctk",
            "-ctv",
            "-ngl",
            "-fa",
            "--jinja",
            "--no-jinja",
            "--metrics",
            "--props",
            "--slots",
            "--mmproj",
            "--api-key-file",
            "--temp",
            "--top-p",
            "--top-k",
            "--min-p",
            "--presence-penalty",
            "--chat-template-kwargs",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        FlagSupport {
            flags,
            jinja_default_on: true,
            fa_tristate: true,
            has_fit: true,
            has_router_mode: true,
            help_lines: 412,
        }
    }

    fn build() -> LlamaBuild {
        LlamaBuild {
            id: BuildId::parse("build-vulkan").expect("id"),
            server_path: "/home/andre/llama.cpp/build-vulkan/bin/llama-server".into(),
            label: "build-vulkan".into(),
            build_info: Some("b9199 (39cf5d619)".into()),
            backends: vec![GpuBackend::Vulkan, GpuBackend::Cpu],
            devices: vec!["Vulkan0".into()],
            flags: full_flags(),
            probed_at_unix: 1_785_412_331,
        }
    }

    fn spec() -> LocalLlamaSpec {
        LocalLlamaSpec {
            build: BuildId::parse("build-vulkan").expect("id"),
            model_path: "/home/andre/models/carnice-9b/Carnice-9b-Q6_K.gguf".into(),
            mmproj: None,
            alias_flag: "carnice-9b".into(),
            host: "127.0.0.1".into(),
            port: Some(8100),
            ctx: Some(32_768),
            parallel: Some(4),
            kv_type: Some(KvType::Q8_0),
            ngl: NglPlan::All,
            split: SplitPlan::default(),
            mode: SamplingMode::Thinking,
            flash_attn: Some(TriState::Auto),
            api_key: None,
            extra_args: vec![],
        }
    }

    fn vllm_spec() -> LocalVllmSpec {
        LocalVllmSpec {
            bin: "/usr/bin/vllm".into(),
            model_id: "Qwen/Qwen3-8B".into(),
            tp: Some(2),
            ctx: Some(32_768),
            quantization: None,
            kv_cache_dtype: Some("fp8".into()),
            enforce_eager: false,
            reasoning_parser: Some("deepseek_r1".into()),
            gpu_util: Some(0.92),
            max_num_seqs: Some(64),
            trust_remote: true,
            chunked_prefill: true,
            host: "127.0.0.1".into(),
            port: Some(8100),
            devices: vec!["CUDA0".into(), "CUDA1".into()],
            extra_args: vec!["--seed".into(), "7".into()],
        }
    }

    fn container_input() -> ContainerLaunchInput {
        ContainerLaunchInput {
            runtime: ContainerRuntime::LlamaCpp,
            image_type: None,
            model_repo: Some("unsloth/Qwen3-30B-A3B-GGUF".into()),
            model_quant: Some("Q4_K_M".into()),
            model_id: None,
            ctx: Some(65_536),
            parallel: Some(2),
            kv_type: Some(KvType::Q8_0),
            mode: SamplingMode::Thinking,
            mmproj: None,
            disk_gb: 80,
            tp: None,
            quantization: None,
            kv_cache_dtype: None,
            enforce_eager: false,
            reasoning_parser: None,
            expose_public: false,
            hf_token: None,
        }
    }

    /// Index of `flag` in `args`, so a test can assert on the value that follows it.
    fn value_of<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        let i = args.iter().position(|a| a == flag)?;
        args.get(i + 1).map(String::as_str)
    }

    // -- sampling ----------------------------------------------------------------------

    #[test]
    fn all_three_presets_carry_top_k_20() {
        for mode in [
            SamplingMode::Thinking,
            SamplingMode::Coding,
            SamplingMode::Nonthinking,
        ] {
            let f = sampling_flags(mode);
            let i = f
                .iter()
                .position(|s| *s == "--top-k")
                .unwrap_or_else(|| panic!("{mode:?} has no --top-k"));
            assert_eq!(f[i + 1], "20", "{mode:?} top-k");
        }
        assert!(sampling_flags(SamplingMode::Raw).is_empty());
    }

    #[test]
    fn nonthinking_disables_thinking_through_the_chat_template() {
        let f = sampling_flags(SamplingMode::Nonthinking);
        let i = f
            .iter()
            .position(|s| *s == "--chat-template-kwargs")
            .expect("kwargs");
        assert_eq!(f[i + 1], "{\"enable_thinking\":false}");
        // and it is the ONLY preset that does
        for mode in [SamplingMode::Thinking, SamplingMode::Coding] {
            assert!(!sampling_flags(mode).contains(&"--chat-template-kwargs"));
        }
    }

    #[test]
    fn every_preset_is_flag_value_pairs() {
        for mode in [
            SamplingMode::Thinking,
            SamplingMode::Coding,
            SamplingMode::Nonthinking,
            SamplingMode::Raw,
        ] {
            let f = sampling_flags(mode);
            assert_eq!(f.len() % 2, 0, "{mode:?} is not pairs");
            for pair in f.chunks(2) {
                assert!(pair[0].starts_with('-'), "{:?} is not a flag", pair[0]);
                assert!(!pair[1].starts_with("--"), "{:?} is not a value", pair[1]);
            }
        }
    }

    #[test]
    fn the_preset_reaches_the_local_argv() {
        let mut s = spec();
        s.mode = SamplingMode::Coding;
        let p = plan_local(&s, &build(), None).expect("plan");
        assert_eq!(value_of(&p.args, "--top-k"), Some("20"));
        assert_eq!(value_of(&p.args, "--temp"), Some("0.6"));
        assert_eq!(value_of(&p.args, "--presence-penalty"), Some("0.0"));
    }

    // -- flag gating -------------------------------------------------------------------

    #[test]
    fn a_flag_is_emitted_only_when_the_build_advertises_it() {
        let mut b = build();
        b.flags.flags.remove("--props");
        b.flags.flags.remove("-np");
        b.flags.flags.remove("--tensor-split");
        let mut s = spec();
        s.split.tensor_split = Some(vec![0.5, 0.5]);
        s.split.devices = vec!["Vulkan0".into(), "Vulkan1".into()];

        let p = plan_local(&s, &b, None).expect("plan");
        assert!(!p.args.iter().any(|a| a == "--props"));
        assert!(!p.args.iter().any(|a| a == "-np"));
        assert!(!p.args.iter().any(|a| a == "--tensor-split"));
        // and the drop is visible, not silent
        assert!(p.warnings.iter().any(|w| w.contains("-np")));
        assert!(p.args.iter().any(|a| a == "--metrics"));
        assert!(p.args.iter().any(|a| a == "--slots"));
    }

    #[test]
    fn props_metrics_and_slots_are_emitted_when_supported() {
        let p = plan_local(&spec(), &build(), None).expect("plan");
        for flag in ["--props", "--metrics", "--slots"] {
            assert!(p.args.iter().any(|a| a == flag), "missing {flag}");
        }
    }

    #[test]
    fn jinja_is_never_emitted_when_it_is_already_the_default() {
        let p = plan_local(&spec(), &build(), None).expect("plan");
        assert!(
            !p.args.iter().any(|a| a == "--jinja"),
            "--jinja is a no-op on b9199 and must not be emitted"
        );
    }

    #[test]
    fn jinja_is_emitted_for_an_older_binary_but_never_against_the_operators_no_jinja() {
        let mut b = build();
        b.flags.jinja_default_on = false;
        let p = plan_local(&spec(), &b, None).expect("plan");
        assert!(p.args.iter().any(|a| a == "--jinja"));

        let mut s = spec();
        s.extra_args = vec!["--no-jinja".into()];
        let p = plan_local(&s, &b, None).expect("plan");
        assert!(!p.args.iter().any(|a| a == "--jinja"));
        assert!(p.args.iter().any(|a| a == "--no-jinja"));
    }

    // -- tristate / omission ------------------------------------------------------------

    #[test]
    fn flash_attn_is_on_off_auto_on_a_tristate_build() {
        for (t, want) in [
            (TriState::On, "on"),
            (TriState::Off, "off"),
            (TriState::Auto, "auto"),
        ] {
            let mut s = spec();
            s.flash_attn = Some(t);
            let p = plan_local(&s, &build(), None).expect("plan");
            assert_eq!(value_of(&p.args, "-fa"), Some(want));
        }
        let mut s = spec();
        s.flash_attn = None;
        let p = plan_local(&s, &build(), None).expect("plan");
        assert!(!p.args.iter().any(|a| a == "-fa"));
    }

    #[test]
    fn flash_attn_on_a_bare_switch_build_is_a_switch() {
        let mut b = build();
        b.flags.fa_tristate = false;
        let mut s = spec();
        s.flash_attn = Some(TriState::On);
        let p = plan_local(&s, &b, None).expect("plan");
        let i = p.args.iter().position(|a| a == "-fa").expect("-fa");
        assert_ne!(p.args.get(i + 1).map(String::as_str), Some("on"));

        s.flash_attn = Some(TriState::Off);
        let p = plan_local(&s, &b, None).expect("plan");
        assert!(!p.args.iter().any(|a| a == "-fa"));
        assert!(p.warnings.iter().any(|w| w.contains("-fa off")));
    }

    #[test]
    fn ctx_and_ngl_are_omitted_so_llama_cpp_fit_can_size_them() {
        let mut s = spec();
        s.ctx = None;
        s.ngl = NglPlan::Auto;
        let p = plan_local(&s, &build(), None).expect("plan");
        assert!(!p.args.iter().any(|a| a == "-c"), "-c must be omitted");
        assert!(!p.args.iter().any(|a| a == "-ngl"), "-ngl must be omitted");

        let mut s = spec();
        s.ctx = Some(32_768);
        s.ngl = NglPlan::Layers(28);
        let p = plan_local(&s, &build(), None).expect("plan");
        assert_eq!(value_of(&p.args, "-c"), Some("32768"));
        assert_eq!(value_of(&p.args, "-ngl"), Some("28"));

        let mut s = spec();
        s.ngl = NglPlan::All;
        let p = plan_local(&s, &build(), None).expect("plan");
        assert_eq!(value_of(&p.args, "-ngl"), Some("999"));
    }

    // -- split -------------------------------------------------------------------------

    #[test]
    fn the_split_plan_produces_dev_sm_mg_and_tensor_split() {
        let mut s = spec();
        s.split = SplitPlan {
            devices: vec!["Vulkan0".into(), "Vulkan1".into()],
            mode: SplitMode::Row,
            main_gpu: Some(1),
            tensor_split: Some(vec![0.6, 0.4]),
        };
        let p = plan_local(&s, &build(), None).expect("plan");
        assert_eq!(value_of(&p.args, "-dev"), Some("Vulkan0,Vulkan1"));
        assert_eq!(value_of(&p.args, "-sm"), Some("row"));
        assert_eq!(value_of(&p.args, "-mg"), Some("1"));
        assert_eq!(value_of(&p.args, "--tensor-split"), Some("0.6,0.4"));
    }

    #[test]
    fn kv_type_is_applied_to_both_halves_of_the_cache() {
        let mut s = spec();
        s.kv_type = Some(KvType::Q4_0);
        let p = plan_local(&s, &build(), None).expect("plan");
        assert_eq!(value_of(&p.args, "-ctk"), Some("q4_0"));
        assert_eq!(value_of(&p.args, "-ctv"), Some("q4_0"));

        s.kv_type = None;
        let p = plan_local(&s, &build(), None).expect("plan");
        assert!(!p.args.iter().any(|a| a == "-ctk"));
    }

    // -- env ---------------------------------------------------------------------------

    #[test]
    fn ld_library_path_is_always_the_binarys_own_directory() {
        let p = plan_local(&spec(), &build(), None).expect("plan");
        let ld = p
            .env
            .iter()
            .find(|(k, _)| k == "LD_LIBRARY_PATH")
            .map(|(_, v)| v.as_str());
        assert_eq!(ld, Some("/home/andre/llama.cpp/build-vulkan/bin"));
    }

    #[test]
    fn backend_env_picks_the_variable_from_the_backend() {
        let d = vec!["Vulkan0".to_owned(), "Vulkan1".to_owned()];
        assert_eq!(
            backend_env(GpuBackend::Vulkan, &d),
            vec![("GGML_VK_VISIBLE_DEVICES".to_owned(), "0,1".to_owned())]
        );
        assert_eq!(
            backend_env(GpuBackend::Cuda, &["CUDA3".to_owned()]),
            vec![("CUDA_VISIBLE_DEVICES".to_owned(), "3".to_owned())]
        );
        for b in [GpuBackend::Rocm, GpuBackend::Hip] {
            assert_eq!(
                backend_env(b, &["ROCm0".to_owned()]),
                vec![("HIP_VISIBLE_DEVICES".to_owned(), "0".to_owned())]
            );
        }
        assert!(backend_env(GpuBackend::Cpu, &d).is_empty());
        assert!(backend_env(GpuBackend::Other("webgpu".into()), &d).is_empty());
    }

    #[test]
    fn an_empty_device_list_masks_nothing() {
        assert!(backend_env(GpuBackend::Vulkan, &[]).is_empty());
        let p = plan_local(&spec(), &build(), None).expect("plan");
        // falls back to the build's own enumeration rather than masking everything
        assert!(p
            .env
            .iter()
            .any(|(k, v)| k == "GGML_VK_VISIBLE_DEVICES" && v == "0"));
    }

    // -- credentials -------------------------------------------------------------------

    #[test]
    fn no_credential_ever_reaches_argv_only_a_key_file_path() {
        let p =
            plan_local(&spec(), &build(), Some(Path::new("/state/keys/local.key"))).expect("plan");
        assert_eq!(
            value_of(&p.args, "--api-key-file"),
            Some("/state/keys/local.key")
        );
        assert!(
            !p.args.iter().any(|a| a == "--api-key"),
            "--api-key would land in /proc/<pid>/cmdline"
        );
    }

    #[test]
    fn a_declared_key_with_no_key_file_is_a_visible_warning() {
        use apexrouter_protocol::CredentialSource;
        let mut s = spec();
        s.api_key = Some(CredentialSource::Instance);
        let p = plan_local(&s, &build(), None).expect("plan");
        assert!(!p.args.iter().any(|a| a == "--api-key-file"));
        assert!(p.warnings.iter().any(|w| w.contains("unauthenticated")));
    }

    // -- vllm --------------------------------------------------------------------------

    #[test]
    fn local_vllm_mirrors_launch_vllm_sh() {
        let p = plan_local_vllm(&vllm_spec()).expect("plan");
        assert_eq!(p.program, "/usr/bin/vllm");
        assert_eq!(p.args.first().map(String::as_str), Some("serve"));
        assert_eq!(value_of(&p.args, "--model"), Some("Qwen/Qwen3-8B"));
        assert_eq!(value_of(&p.args, "--tensor-parallel-size"), Some("2"));
        assert_eq!(value_of(&p.args, "--max-model-len"), Some("32768"));
        assert_eq!(value_of(&p.args, "--gpu-memory-utilization"), Some("0.92"));
        assert_eq!(value_of(&p.args, "--kv-cache-dtype"), Some("fp8"));
        assert_eq!(value_of(&p.args, "--reasoning-parser"), Some("deepseek_r1"));
        assert!(p.args.iter().any(|a| a == "--trust-remote-code"));
        assert!(p.args.iter().any(|a| a == "--enable-chunked-prefill"));
        assert!(!p.args.iter().any(|a| a == "--enforce-eager"));
        assert_eq!(p.args.last().map(String::as_str), Some("7"));
        assert_eq!(
            p.env,
            vec![("CUDA_VISIBLE_DEVICES".to_owned(), "0,1".to_owned())]
        );
    }

    #[test]
    fn vllm_drops_the_scripts_own_unset_words() {
        let mut s = vllm_spec();
        s.kv_cache_dtype = Some("auto".into());
        s.quantization = Some("None".into());
        let p = plan_local_vllm(&s).expect("plan");
        assert!(!p.args.iter().any(|a| a == "--kv-cache-dtype"));
        assert!(!p.args.iter().any(|a| a == "--quantization"));
    }

    // -- containers --------------------------------------------------------------------

    #[test]
    fn llama_container_emits_exactly_the_sixteen_variables() {
        let cfg = Config::default();
        let (launch, preview) = plan_container(&container_input(), &cfg).expect("plan");
        assert_eq!(launch.env.len(), 16);
        for k in LLAMA_VARS {
            assert!(launch.env.contains_key(k), "missing {k}");
        }
        assert_eq!(preview.env.len(), 16);
        let order: Vec<&str> = preview.env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(order, LLAMA_VARS.to_vec());
        assert_eq!(
            launch.env.get("MODEL_QUANT").map(String::as_str),
            Some("Q4_K_M")
        );
        assert_eq!(launch.env.get("MODE").map(String::as_str), Some("thinking"));
        assert_eq!(launch.env.get("KV_TYPE").map(String::as_str), Some("q8_0"));
        assert_eq!(launch.env.get("PARALLEL").map(String::as_str), Some("2"));
        assert_eq!(
            launch.env.get("MODELS_DIR").map(String::as_str),
            Some("/workspace/models")
        );
        assert_eq!(launch.port, 8000);
        assert_eq!(launch.disk_gb, 80);
    }

    #[test]
    fn vllm_container_emits_exactly_the_sixteen_variables() {
        let cfg = Config::default();
        let mut input = container_input();
        input.runtime = ContainerRuntime::Vllm;
        input.model_repo = None;
        input.model_quant = None;
        input.model_id = Some("Qwen/Qwen3-30B-A3B".into());
        input.tp = Some(4);
        input.enforce_eager = true;
        let (launch, preview) = plan_container(&input, &cfg).expect("plan");
        assert_eq!(launch.env.len(), 16);
        for k in VLLM_VARS {
            assert!(launch.env.contains_key(k), "missing {k}");
        }
        let order: Vec<&str> = preview.env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(order, VLLM_VARS.to_vec());
        // the contract compares literal strings, never JSON booleans
        assert_eq!(
            launch.env.get("ENFORCE_EAGER").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            launch.env.get("TRUST_REMOTE").map(String::as_str),
            Some("true")
        );
        assert_eq!(launch.env.get("TP").map(String::as_str), Some("4"));
        assert_eq!(launch.image_type, ImageType::Vllm);
        assert_eq!(launch.image, cfg.docker.vllm);
        assert!(preview.onstart.contains("launch_vllm.sh"));
    }

    #[test]
    fn host_is_loopback_unless_expose_public_is_asked_for_explicitly() {
        let cfg = Config::default();
        let (launch, _) = plan_container(&container_input(), &cfg).expect("plan");
        assert_eq!(launch.host, "127.0.0.1");
        assert_eq!(
            launch.env.get("HOST").map(String::as_str),
            Some("127.0.0.1")
        );
        assert!(!launch.expose_public);

        let mut input = container_input();
        input.expose_public = true;
        let (launch, preview) = plan_container(&input, &cfg).expect("plan");
        assert_eq!(launch.host, "0.0.0.0");
        assert_eq!(launch.env.get("HOST").map(String::as_str), Some("0.0.0.0"));
        assert!(preview.warnings.iter().any(|w| w.contains("api key")));
    }

    #[test]
    fn hf_token_lives_in_the_env_map_and_never_in_onstart_or_the_preview() {
        let cfg = Config::default();
        let mut input = container_input();
        input.hf_token = Some(Secret::new("hf_SECRETVALUE".into()));
        let (launch, preview) = plan_container(&input, &cfg).expect("plan");
        assert_eq!(
            launch.env.get("HF_TOKEN").map(String::as_str),
            Some("hf_SECRETVALUE")
        );
        assert!(!launch.onstart.contains("hf_SECRETVALUE"));
        assert!(!launch.onstart.contains("HF_TOKEN"));
        assert!(!preview.onstart.contains("HF_TOKEN"));
        for (_, v) in &preview.env {
            assert!(
                !v.contains("hf_SECRETVALUE"),
                "token leaked into the preview"
            );
        }
        assert_eq!(
            preview.env.iter().find(|(k, _)| k == "HF_TOKEN"),
            Some(&("HF_TOKEN".to_owned(), "***".to_owned()))
        );
    }

    #[test]
    fn the_image_is_resolved_from_docker_by_image_type() {
        let cfg = Config::default();
        let (launch, preview) = plan_container(&container_input(), &cfg).expect("plan");
        assert_eq!(launch.image_type, ImageType::Prebuilt);
        assert_eq!(launch.image, cfg.docker.prebuilt);
        assert_eq!(preview.image, cfg.docker.prebuilt);

        let mut input = container_input();
        input.image_type = Some(ImageType::Builder);
        let (launch, preview) = plan_container(&input, &cfg).expect("plan");
        assert_eq!(launch.image, cfg.docker.builder);
        assert!(preview.warnings.iter().any(|w| w.contains("12–18 min")));
    }

    #[test]
    fn a_known_fork_forces_the_builder_image_and_warns_about_the_cold_start() {
        let cfg = Config::default();
        let mut input = container_input();
        input.image_type = Some(ImageType::Prebuilt);
        input.model_repo = Some("deepseek-ai/DeepSeek-V4-GGUF".into());
        let (launch, preview) = plan_container(&input, &cfg).expect("plan");
        assert_eq!(launch.image_type, ImageType::Builder);
        assert_eq!(launch.image, cfg.docker.builder);
        assert_eq!(
            launch.env.get("IMAGE_TYPE").map(String::as_str),
            Some("builder")
        );
        assert_eq!(
            launch.env.get("LLAMA_CPP_REPO").map(String::as_str),
            Some("fairydreaming/llama.cpp")
        );
        assert_eq!(
            launch.env.get("LLAMA_CPP_REF").map(String::as_str),
            Some("deepseek-dsa")
        );
        assert!(preview.warnings.iter().any(|w| w.contains("12–18 min")));
        assert!(preview.warnings.iter().any(|w| w.contains("custom fork")));
        // exactly one cold-start warning, not two
        assert_eq!(
            preview
                .warnings
                .iter()
                .filter(|w| w.contains("12–18 min"))
                .count(),
            1
        );
    }

    #[test]
    fn an_ordinary_repo_gets_upstream_llama_cpp() {
        let cfg = Config::default();
        let (launch, _) = plan_container(&container_input(), &cfg).expect("plan");
        assert_eq!(
            launch.env.get("LLAMA_CPP_REPO").map(String::as_str),
            Some("ggml-org/llama.cpp")
        );
        assert_eq!(
            launch.env.get("LLAMA_CPP_REF").map(String::as_str),
            Some("master")
        );
    }

    #[test]
    fn args_override_stops_the_entrypoint_starting_a_second_server() {
        let cfg = Config::default();
        let (launch, preview) = plan_container(&container_input(), &cfg).expect("plan");
        assert_eq!(preview.args_override, vec!["sleep", "infinity"]);
        assert_eq!(
            launch.onstart,
            "bash /app/launch.sh > /var/log/launch.log 2>&1 &"
        );
        assert_eq!(preview.onstart, launch.onstart);
    }

    #[test]
    fn a_missing_required_variable_is_a_named_error() {
        let cfg = Config::default();
        let mut input = container_input();
        input.model_quant = None;
        let e = plan_container(&input, &cfg).expect_err("must fail");
        assert!(format!("{e}").contains("MODEL_QUANT"));

        let mut input = container_input();
        input.runtime = ContainerRuntime::Vllm;
        input.model_id = None;
        let e = plan_container(&input, &cfg).expect_err("must fail");
        assert!(format!("{e}").contains("MODEL_ID"));
    }

    #[test]
    fn raw_is_fatal_for_a_container_because_launch_sh_dies_on_it() {
        let cfg = Config::default();
        let mut input = container_input();
        input.mode = SamplingMode::Raw;
        let e = plan_container(&input, &cfg).expect_err("must fail");
        assert!(format!("{e}").contains("raw"));
        // ...but locally `raw` simply emits no sampling flags
        let mut s = spec();
        s.mode = SamplingMode::Raw;
        let p = plan_local(&s, &build(), None).expect("plan");
        assert!(!p.args.iter().any(|a| a == "--temp"));
    }

    #[test]
    fn the_runtime_and_the_image_family_cannot_disagree() {
        let cfg = Config::default();
        let mut input = container_input();
        input.image_type = Some(ImageType::Vllm);
        let (launch, preview) = plan_container(&input, &cfg).expect("plan");
        assert_eq!(launch.image_type, ImageType::Prebuilt);
        assert!(!preview.warnings.is_empty());

        let mut input = container_input();
        input.runtime = ContainerRuntime::Vllm;
        input.model_id = Some("Qwen/Qwen3-8B".into());
        input.image_type = Some(ImageType::Prebuilt);
        let (launch, _) = plan_container(&input, &cfg).expect("plan");
        assert_eq!(launch.image_type, ImageType::Vllm);
    }

    #[test]
    fn device_indices_come_out_of_the_enumerated_tokens() {
        assert_eq!(device_index("Vulkan0"), "0");
        assert_eq!(device_index("CUDA12"), "12");
        assert_eq!(device_index("none"), "none");
    }
}
