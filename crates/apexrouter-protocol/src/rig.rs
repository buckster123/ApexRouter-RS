//! The rig: GPUs, llama.cpp builds and local weights. **Nothing here is singular.**
//!
//! `Gpu` is a list, `LlamaBuild` is a list, `FlagSupport` is probed rather than hardcoded —
//! b9199 already moved `-fa` to `on|off|auto`, made `--jinja` default-on and deprecated
//! `--webui`, so any compiled-in whitelist is wrong within a month.

use crate::ids::{BackendId, BuildId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Everything the daemon knows about the local machine, as of one scan.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RigSnapshot {
    /// Every enumerated device across every build. Software rasterisers are marked, not hidden.
    #[serde(default)]
    pub gpus: Vec<Gpu>,
    /// Every discovered `llama-server` binary, labelled by its build directory.
    #[serde(default)]
    pub builds: Vec<LlamaBuild>,
    /// Total system RAM, MiB.
    pub ram_total_mb: u64,
    /// Free system RAM, MiB.
    pub ram_free_mb: u64,
    /// Total swap, MiB.
    pub swap_total_mb: u64,
    /// Used swap, MiB. This laptop lives under swap pressure; it is a first-class number.
    pub swap_used_mb: u64,
    /// Logical CPUs.
    pub cpu_threads: u32,
    /// When this snapshot was taken, unix seconds.
    pub scanned_at_unix: i64,
}

/// One compute device, as some llama.cpp build enumerated it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Gpu {
    /// The exact `-dev` token, e.g. `"Vulkan0"`, `"CUDA1"`.
    pub device: String,
    /// Index within its backend.
    pub index: u32,
    /// Human name, e.g. `"AMD Radeon 840M Graphics (RADV KRACKAN1)"`.
    pub name: String,
    /// Which compute backend enumerated it.
    pub backend: Backend,
    /// Total VRAM, MiB.
    pub vram_total_mb: u64,
    /// Free VRAM, MiB, as `--list-devices` reports it.
    pub vram_free_mb: u64,
    /// Driver string, when the backend exposes one.
    #[serde(default)]
    pub driver: Option<String>,
    /// `llvmpipe` and friends. Excluded from default device selection.
    pub is_software: bool,
    /// Which builds can see this device.
    #[serde(default)]
    pub seen_by_builds: Vec<BuildId>,
    /// Endpoints currently using this device.
    #[serde(default)]
    pub held_by: Vec<BackendId>,
    /// Sum of the fit estimates of the endpoints in `held_by`. Subtracted by the fit solver.
    pub reserved_mb: u64,
}

/// A llama.cpp compute backend. Detected from `--list-devices`, **never** by grepping `--help`.
///
/// `Metal` exists so the data model does not have to change when macOS eventually matters,
/// even though mk1 is Linux-only.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    /// Vulkan (RADV on this rig).
    Vulkan,
    /// NVIDIA CUDA.
    Cuda,
    /// AMD ROCm.
    Rocm,
    /// AMD HIP.
    Hip,
    /// Apple Metal.
    Metal,
    /// Intel SYCL.
    Sycl,
    /// CPU only.
    Cpu,
    /// Anything a future llama.cpp prints that we do not have a variant for.
    Other(String),
}

/// One discovered `llama-server` binary.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LlamaBuild {
    /// The build-dir name: `"build-vulkan"`, `"build-mtp"`.
    pub id: BuildId,
    /// Absolute path to `llama-server`.
    pub server_path: String,
    /// Human label.
    pub label: String,
    /// e.g. `"b9199 (39cf5d619)"`.
    #[serde(default)]
    pub build_info: Option<String>,
    /// Backends this build actually enumerated, from `--list-devices`.
    #[serde(default)]
    pub backends: Vec<Backend>,
    /// Device tokens this build enumerated, e.g. `["Vulkan0", "Vulkan1"]`.
    #[serde(default)]
    pub devices: Vec<String>,
    /// Which flags this binary accepts. Probed, cached by `(path, mtime, size)`.
    pub flags: FlagSupport,
    /// When the probe ran, unix seconds.
    pub probed_at_unix: i64,
}

/// Feature detection for one binary. Never a hardcoded whitelist.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlagSupport {
    /// Every long/short flag the binary's `--help` advertises.
    #[serde(default)]
    pub flags: BTreeSet<String>,
    /// b9199+ turns `--jinja` on by default, which makes `--no-jinja` the meaningful flag.
    pub jinja_default_on: bool,
    /// `-fa` takes `on|off|auto` rather than being a bare switch.
    pub fa_tristate: bool,
    /// The binary has its own `--fit` sizing.
    pub has_fit: bool,
    /// The binary has router mode (`--models-dir`, `POST /models/load`). Recorded, not used in mk1.
    pub has_router_mode: bool,
    /// How many lines of help we parsed — a sanity signal when a probe silently truncates.
    pub help_lines: u32,
}

impl FlagSupport {
    /// Does the binary accept this flag? The argv builder emits nothing it has not seen.
    pub fn has(&self, flag: &str) -> bool {
        self.flags.contains(flag)
    }
}

/// A logical local model: one entry even when it is 12 shards plus a vision projector.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LocalModel {
    /// Stable slug derived from the directory and the base filename.
    pub id: String,
    /// Display name, usually the base filename without `-00001-of-000NN`.
    pub name: String,
    /// Directory holding the shards.
    pub dir: String,
    /// Every shard, in order. `-00001-of-000NN` groups into ONE logical model.
    #[serde(default)]
    pub shards: Vec<ModelShard>,
    /// Sum of every shard's size, bytes.
    pub total_bytes: u64,
    /// Vision projectors found alongside. Empty means text-only.
    #[serde(default)]
    pub mmproj: Vec<ModelShard>,
    /// Quantisation token matched by `(UD-Q\d+[^.\s_-]*|Q\d+_K_[A-Z]+|Q\d+_\d+)`.
    #[serde(default)]
    pub quant: Option<String>,
    /// Header metadata. `None` when the file could not be read.
    #[serde(default)]
    pub gguf: Option<GgufMeta>,
    /// When discovery last saw it, unix seconds.
    pub discovered_at_unix: i64,
}

impl LocalModel {
    /// The path to pass to `-m`: the first shard, which is the one llama.cpp opens.
    pub fn primary_path(&self) -> Option<&str> {
        self.shards.first().map(|s| s.path.as_str())
    }

    /// True when a vision projector was found alongside the weights.
    pub fn is_vision(&self) -> bool {
        !self.mmproj.is_empty()
    }
}

/// One file on disk belonging to a [`LocalModel`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelShard {
    /// Absolute path.
    pub path: String,
    /// Size in bytes.
    pub bytes: u64,
}

/// The fields of a GGUF header the fit solver needs. Read with a bounded read, never `mmap`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GgufMeta {
    /// `general.architecture`.
    pub arch: String,
    /// Number of blocks.
    pub n_layer: u32,
    /// KV heads.
    pub n_head_kv: u32,
    /// Key head dimension.
    pub n_embd_head_k: u32,
    /// Value head dimension.
    pub n_embd_head_v: u32,
    /// Training context length.
    pub n_ctx_train: u32,
    /// Hybrid-linear models (Qwen3.6 MoE) carry KV on only some layers. `None` = all of them.
    #[serde(default)]
    pub full_attn_layers: Option<u32>,
    /// Expert count for MoE architectures.
    #[serde(default)]
    pub n_expert: Option<u32>,
    /// The file's own quantisation description, when the header carries one.
    #[serde(default)]
    pub quant_desc: Option<String>,
}

/// The outcome of picking a binary for a requested backend.
///
/// A fallback is a **visible value** the UI renders as a warning — never a silent
/// substitution of HIP for Vulkan because a substring matched.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BinaryChoiceInfo {
    /// The build we are going to use.
    pub chosen: BuildId,
    /// True when it is exactly what was asked for.
    pub exact: bool,
    /// What the caller wanted, when it wanted something specific.
    #[serde(default)]
    pub wanted: Option<Backend>,
    /// What it actually got.
    #[serde(default)]
    pub got: Option<Backend>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_build() -> LlamaBuild {
        LlamaBuild {
            id: BuildId::parse("build-vulkan").expect("id"),
            server_path: "/home/andre/llama.cpp/build-vulkan/bin/llama-server".into(),
            label: "build-vulkan".into(),
            build_info: Some("b9199 (39cf5d619)".into()),
            backends: vec![Backend::Vulkan, Backend::Cpu],
            devices: vec!["Vulkan0".into()],
            flags: FlagSupport {
                flags: ["-m", "--port", "-fa", "--props"]
                    .iter()
                    .map(|s| (*s).to_owned())
                    .collect(),
                jinja_default_on: true,
                fa_tristate: true,
                has_fit: true,
                has_router_mode: true,
                help_lines: 412,
            },
            probed_at_unix: 1_785_412_331,
        }
    }

    #[test]
    fn backend_round_trips_including_the_open_variant() {
        for b in [
            Backend::Vulkan,
            Backend::Cuda,
            Backend::Rocm,
            Backend::Hip,
            Backend::Metal,
            Backend::Sycl,
            Backend::Cpu,
            Backend::Other("webgpu".into()),
        ] {
            let s = serde_json::to_string(&b).expect("ser");
            assert_eq!(serde_json::from_str::<Backend>(&s).expect("de"), b);
        }
        assert_eq!(
            serde_json::to_string(&Backend::Vulkan).expect("ser"),
            "\"vulkan\""
        );
        assert_eq!(
            serde_json::to_string(&Backend::Other("webgpu".into())).expect("ser"),
            r#"{"other":"webgpu"}"#
        );
    }

    #[test]
    fn rig_snapshot_round_trips() {
        let rig = RigSnapshot {
            gpus: vec![Gpu {
                device: "Vulkan0".into(),
                index: 0,
                name: "AMD Radeon 840M Graphics (RADV KRACKAN1)".into(),
                backend: Backend::Vulkan,
                vram_total_mb: 16_384,
                vram_free_mb: 12_000,
                driver: Some("RADV 25.0".into()),
                is_software: false,
                seen_by_builds: vec![BuildId::parse("build-vulkan").expect("id")],
                held_by: vec![BackendId::parse("local-carnice").expect("id")],
                reserved_mb: 5_956,
            }],
            builds: vec![sample_build()],
            ram_total_mb: 22_000,
            ram_free_mb: 2_100,
            swap_total_mb: 8_192,
            swap_used_mb: 5_300,
            cpu_threads: 12,
            scanned_at_unix: 1_785_412_331,
        };
        let s = serde_json::to_string(&rig).expect("ser");
        assert_eq!(serde_json::from_str::<RigSnapshot>(&s).expect("de"), rig);
    }

    #[test]
    fn flag_support_answers_has() {
        let b = sample_build();
        assert!(b.flags.has("--props"));
        assert!(!b.flags.has("--webui"));
    }

    #[test]
    fn local_model_groups_shards_and_reports_vision() {
        let m = LocalModel {
            id: "carnice-9b-q6-k".into(),
            name: "Carnice-9b-Q6_K".into(),
            dir: "/home/andre/models/carnice-9b".into(),
            shards: vec![
                ModelShard {
                    path: "/home/andre/models/carnice-9b/Carnice-9b-Q6_K-00001-of-00002.gguf"
                        .into(),
                    bytes: 4_000_000_000,
                },
                ModelShard {
                    path: "/home/andre/models/carnice-9b/Carnice-9b-Q6_K-00002-of-00002.gguf"
                        .into(),
                    bytes: 2_900_000_000,
                },
            ],
            total_bytes: 6_900_000_000,
            mmproj: vec![ModelShard {
                path: "/home/andre/models/carnice-9b/mmproj-f16.gguf".into(),
                bytes: 600_000_000,
            }],
            quant: Some("Q6_K".into()),
            gguf: Some(GgufMeta {
                arch: "qwen3".into(),
                n_layer: 41,
                n_head_kv: 8,
                n_embd_head_k: 128,
                n_embd_head_v: 128,
                n_ctx_train: 32_768,
                full_attn_layers: Some(10),
                n_expert: None,
                quant_desc: Some("Q6_K".into()),
            }),
            discovered_at_unix: 1_785_412_331,
        };
        assert!(m.is_vision());
        assert!(m
            .primary_path()
            .expect("shard")
            .ends_with("00001-of-00002.gguf"));
        let s = serde_json::to_string(&m).expect("ser");
        assert_eq!(serde_json::from_str::<LocalModel>(&s).expect("de"), m);
    }

    #[test]
    fn binary_choice_info_round_trips() {
        let c = BinaryChoiceInfo {
            chosen: BuildId::parse("build-hip").expect("id"),
            exact: false,
            wanted: Some(Backend::Vulkan),
            got: Some(Backend::Hip),
        };
        let s = serde_json::to_string(&c).expect("ser");
        assert_eq!(serde_json::from_str::<BinaryChoiceInfo>(&s).expect("de"), c);
    }
}
