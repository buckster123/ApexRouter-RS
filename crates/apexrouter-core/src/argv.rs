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
use crate::error::Result;
use crate::secret::Secret;
use apexrouter_protocol::{
    ArgvPreview, ContainerEnvPreview, ContainerLaunch, ContainerRuntime, GpuBackend, ImageType,
    KvType, LlamaBuild, LocalLlamaSpec, LocalVllmSpec, SamplingMode,
};
use std::path::Path;

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
pub fn plan_local(
    spec: &LocalLlamaSpec,
    build: &LlamaBuild,
    key_file: Option<&Path>,
) -> Result<ArgvPreview> {
    todo!("C-12: plan_local")
}

/// Build the exact argv and env for one local vLLM.
pub fn plan_local_vllm(spec: &LocalVllmSpec) -> Result<ArgvPreview> {
    todo!("C-12: plan_local_vllm")
}

/// Build the container contract for a rented box.
///
/// Emits exactly the 16 llama.cpp vars or the 16 vLLM vars of `ARCHITECTURE.md` §3.7,
/// forces `HOST=127.0.0.1` unless `expose_public`, resolves the image from `[docker]` by
/// `image_type`, applies `known_forks` (forcing `Builder` and pushing the
/// "+12–18 min cold start" warning), and sets `args_override` so the image's own
/// `ENTRYPOINT` cannot start a second server on port 8000.
pub fn plan_container(
    launch_in: &ContainerLaunchInput,
    cfg: &Config,
) -> Result<(ContainerLaunch, ContainerEnvPreview)> {
    todo!("C-12: plan_container")
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
    todo!("C-12: sampling_flags")
}

/// `GGML_VK_VISIBLE_DEVICES` / `HIP_VISIBLE_DEVICES` / `CUDA_VISIBLE_DEVICES`, chosen from
/// the build's detected backend. `-dev` carries the explicit device list regardless.
pub fn backend_env(backend: GpuBackend, devices: &[String]) -> Vec<(String, String)> {
    todo!("C-12: backend_env")
}
