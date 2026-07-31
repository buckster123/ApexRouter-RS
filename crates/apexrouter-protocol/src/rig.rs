//! The rig: GPUs, llama.cpp builds and local weights. **Nothing here is singular.**
//!
//! `Gpu` is a list, `LlamaBuild` is a list, `FlagSupport` is probed rather than hardcoded —
//! b9199 already moved `-fa` to `on|off|auto`, made `--jinja` default-on and deprecated
//! `--webui`, so any compiled-in whitelist is wrong within a month.
//!
//! # One `Gpu` is one *enumeration*, not one piece of silicon
//!
//! [`Gpu`] is what one backend of one build reported. The same card is a different `Gpu` in
//! every backend that can reach it: on the machine in `docs/port/00-machine-ground-truth.md`
//! the single Radeon 840M is `ROCm0` (11397 MiB) to `~/llama.cpp/build` and `Vulkan0`
//! (20992 MiB) to `build-vulkan`. Both readings are true; neither may be added to the other.
//! [`RigSnapshot::physical_devices`] folds the enumerations back onto the silicon so the
//! operator is shown one GPU with two backends rather than two GPUs, and
//! [`Gpu::vram_used_mb`] is the only sanctioned way to ask what a device has spent.

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

impl RigSnapshot {
    /// The physical GPUs behind [`RigSnapshot::gpus`] — one entry per piece of silicon, each
    /// naming every backend that can reach it and carrying that backend's own VRAM reading.
    ///
    /// This is what "how many GPUs does this box have" means. `gpus` answers a different
    /// question — "what `-dev` tokens exist" — and on a box with a ROCm build and a Vulkan
    /// build it answers `2` for a laptop with one iGPU.
    pub fn physical_devices(&self) -> Vec<PhysicalDevice> {
        physical_devices(&self.gpus)
    }
}

/// One compute device, **as one backend enumerated it** — not one piece of silicon.
///
/// Two `Gpu`s with different `backend`s may be the same card. Compare
/// [`Gpu::physical_key`], never `device`, when the question is "is this the same hardware".
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
    /// PCI address of the silicon behind this enumeration, e.g. `"0000:04:00.0"`.
    ///
    /// Filled by `core::discover` from `/sys/bus/pci/devices` when the enumeration can be
    /// aligned with sysfs unambiguously; `None` on a rig where it cannot (a container
    /// without `/sys`, a rented box, a backend that reorders its devices). It is the
    /// *strong* half of physical identity — see [`Gpu::physical_key`] for the fallback.
    #[serde(default)]
    pub pci_bus_id: Option<String>,
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

impl Gpu {
    /// VRAM in use, MiB — or `None` when the driver's own numbers cannot express it.
    ///
    /// **The only sanctioned way to compute "used".** ROCm on the machine in
    /// `docs/port/00-machine-ground-truth.md` reports free (12821 MiB) *greater* than total
    /// (11397 MiB), because a GTT-backed device allocates past its carve-out. `total - free`
    /// is then not a small number, it is an underflowed `u64` the size of the universe. This
    /// returns `None` instead, so a caller has to decide what to render rather than
    /// accidentally rendering 17 EiB.
    pub fn vram_used_mb(&self) -> Option<u64> {
        self.vram_total_mb.checked_sub(self.vram_free_mb)
    }

    /// True when the driver reports more free memory than it claims to have — AMD's GTT
    /// accounting, and the reason [`Gpu::vram_used_mb`] is an `Option`.
    pub fn reports_gtt_overcommit(&self) -> bool {
        self.vram_free_mb > self.vram_total_mb
    }

    /// A stable key for the **silicon** behind this enumeration.
    ///
    /// `ordinal` is how many devices *earlier in the same backend* carry the same normalised
    /// name; [`RigSnapshot::physical_devices`] supplies it. Two rules, in order:
    ///
    /// 1. `pci:<bus id>` when [`Gpu::pci_bus_id`] is known. Physical, exact, and the only
    ///    form that survives a backend renumbering its devices.
    /// 2. `name:<normalised name>#<ordinal>` otherwise — the **documented heuristic**.
    ///    The name is lowercased with parenthesised driver suffixes removed, so
    ///    `"AMD Radeon 840M Graphics (RADV KRACKAN1)"` and `"AMD Radeon 840M Graphics"`
    ///    agree. The ordinal keeps four identical cards apart within a backend while still
    ///    pairing card *n* of one backend with card *n* of another. It assumes the backends
    ///    enumerate identical cards in the same order — true for PCI-ordered enumeration,
    ///    and the reason rule 1 exists.
    ///
    /// A software rasteriser is never keyed by PCI: it is not on the bus.
    pub fn physical_key(&self, ordinal: usize) -> String {
        match &self.pci_bus_id {
            Some(bus) if !self.is_software => format!("pci:{bus}"),
            _ => format!("name:{}#{ordinal}", normalise_device_name(&self.name)),
        }
    }
}

/// Lowercase, drop parenthesised suffixes, collapse whitespace.
///
/// `"AMD Radeon 840M Graphics (RADV KRACKAN1)"` and `"AMD Radeon 840M Graphics"` both become
/// `"amd radeon 840m graphics"`: the Vulkan driver names its ICD, ROCm does not, and the
/// silicon does not care.
pub fn normalise_device_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut depth = 0usize;
    for c in name.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c.to_ascii_lowercase()),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One piece of silicon, with every backend enumeration that reaches it.
///
/// This is the shape `apexrouter rig` prints and the shape a human means by "how many GPUs
/// do I have". It is **derived**, never stored: [`RigSnapshot::gpus`] stays the raw
/// per-backend truth, because that is what a `-dev` flag takes.
///
/// VRAM is deliberately absent from this struct. The backends disagree about it — 11397 MiB
/// to ROCm, 20992 MiB to Vulkan, on the same card — and picking one to promote would be
/// inventing an answer. Read it from [`PhysicalDevice::views`], per backend.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PhysicalDevice {
    /// [`Gpu::physical_key`] of every view: `pci:0000:04:00.0` or `name:…#0`.
    pub key: String,
    /// The PCI address, when identity was established the strong way.
    #[serde(default)]
    pub pci_bus_id: Option<String>,
    /// The name of the first enumeration, verbatim.
    pub name: String,
    /// A software rasteriser. Excluded from default device selection.
    pub is_software: bool,
    /// One entry per backend that can reach this device, in first-seen order.
    pub views: Vec<Gpu>,
}

impl PhysicalDevice {
    /// The backends that can reach this device, in first-seen order.
    pub fn backends(&self) -> Vec<Backend> {
        let mut out: Vec<Backend> = Vec::with_capacity(self.views.len());
        for v in &self.views {
            if !out.contains(&v.backend) {
                out.push(v.backend.clone());
            }
        }
        out
    }

    /// The `-dev` tokens this device answers to, one per backend.
    pub fn device_tokens(&self) -> Vec<String> {
        self.views.iter().map(|v| v.device.clone()).collect()
    }

    /// This device as one backend sees it — including its VRAM, which is backend-specific.
    pub fn view_for(&self, backend: &Backend) -> Option<&Gpu> {
        self.views.iter().find(|v| &v.backend == backend)
    }

    /// Every endpoint holding this silicon, through any backend, deduplicated.
    pub fn held_by(&self) -> Vec<BackendId> {
        let mut out: Vec<BackendId> = Vec::new();
        for v in &self.views {
            for h in &v.held_by {
                if !out.contains(h) {
                    out.push(h.clone());
                }
            }
        }
        out
    }

    /// Every build that can see this silicon, through any backend, deduplicated.
    pub fn seen_by_builds(&self) -> Vec<BuildId> {
        let mut out: Vec<BuildId> = Vec::new();
        for v in &self.views {
            for b in &v.seen_by_builds {
                if !out.contains(b) {
                    out.push(b.clone());
                }
            }
        }
        out
    }
}

/// Fold per-backend enumerations onto the silicon they describe.
///
/// Order is first-seen, both for the devices and for each device's views, so two scans of an
/// unchanged machine render identically. See [`Gpu::physical_key`] for how identity is
/// established, and note that a hardware device and a software one never merge however alike
/// their names look.
pub fn physical_devices(gpus: &[Gpu]) -> Vec<PhysicalDevice> {
    // Ordinal is per (backend, normalised name): card n of one backend pairs with card n of
    // another, while two identical cards inside one backend stay apart.
    let mut counts: Vec<((String, String), usize)> = Vec::new();
    let mut out: Vec<PhysicalDevice> = Vec::new();

    for gpu in gpus {
        let bucket = (
            format!("{:?}", gpu.backend),
            normalise_device_name(&gpu.name),
        );
        let ordinal = match counts.iter_mut().find(|(k, _)| k == &bucket) {
            Some((_, n)) => {
                *n += 1;
                *n
            }
            None => {
                counts.push((bucket, 0));
                0
            }
        };
        let key = gpu.physical_key(ordinal);
        match out
            .iter_mut()
            .find(|p| p.key == key && p.is_software == gpu.is_software)
        {
            Some(existing) => existing.views.push(gpu.clone()),
            None => out.push(PhysicalDevice {
                key,
                pci_bus_id: gpu.pci_bus_id.clone(),
                name: gpu.name.clone(),
                is_software: gpu.is_software,
                views: vec![gpu.clone()],
            }),
        }
    }
    out
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
                pci_bus_id: None,
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

    /// The live reading from `docs/port/00-machine-ground-truth.md`: one card, two backends,
    /// two different VRAM numbers, and a `free` that exceeds `total`.
    #[test]
    fn one_card_seen_by_two_backends_is_one_physical_device() {
        let card = |device: &str, backend: Backend, name: &str, total: u64, free: u64| Gpu {
            device: device.into(),
            index: 0,
            name: name.into(),
            backend,
            vram_total_mb: total,
            vram_free_mb: free,
            pci_bus_id: Some("0000:04:00.0".into()),
            driver: None,
            is_software: false,
            seen_by_builds: vec![BuildId::parse("build-vulkan").expect("id")],
            held_by: vec![BackendId::parse("local-carnice").expect("id")],
            reserved_mb: 5_956,
        };
        let gpus = vec![
            card(
                "ROCm0",
                Backend::Rocm,
                "AMD Radeon 840M Graphics",
                11_397,
                12_821,
            ),
            card(
                "Vulkan0",
                Backend::Vulkan,
                "AMD Radeon 840M Graphics (RADV KRACKAN1)",
                20_992,
                19_626,
            ),
        ];
        let physical = physical_devices(&gpus);
        assert_eq!(physical.len(), 1);
        assert_eq!(physical[0].key, "pci:0000:04:00.0");
        assert_eq!(physical[0].backends(), vec![Backend::Rocm, Backend::Vulkan]);
        assert_eq!(physical[0].device_tokens(), vec!["ROCm0", "Vulkan0"]);
        assert_eq!(physical[0].held_by().len(), 1, "one holder, not two");
        // Per-backend VRAM survives, because the backends really do disagree.
        assert_eq!(
            physical[0]
                .view_for(&Backend::Rocm)
                .map(|v| v.vram_total_mb),
            Some(11_397)
        );
        assert_eq!(
            physical[0]
                .view_for(&Backend::Vulkan)
                .map(|v| v.vram_total_mb),
            Some(20_992)
        );
        // A PhysicalDevice is wire-shaped like everything else here.
        let s = serde_json::to_string(&physical[0]).expect("ser");
        assert_eq!(
            serde_json::from_str::<PhysicalDevice>(&s).expect("de"),
            physical[0]
        );
    }

    /// GTT accounting: `used` must be unable to express the underflow.
    #[test]
    fn vram_used_refuses_to_answer_when_free_exceeds_total() {
        let mut g = Gpu {
            device: "ROCm0".into(),
            index: 0,
            name: "AMD Radeon 840M Graphics".into(),
            backend: Backend::Rocm,
            vram_total_mb: 11_397,
            vram_free_mb: 12_821,
            pci_bus_id: None,
            driver: None,
            is_software: false,
            seen_by_builds: vec![],
            held_by: vec![],
            reserved_mb: 0,
        };
        assert_eq!(g.vram_used_mb(), None);
        assert!(g.reports_gtt_overcommit());
        g.vram_free_mb = 9_000;
        assert_eq!(g.vram_used_mb(), Some(2_397));
        assert!(!g.reports_gtt_overcommit());
    }

    #[test]
    fn device_names_normalise_past_the_driver_suffix() {
        assert_eq!(
            normalise_device_name("AMD Radeon 840M Graphics (RADV KRACKAN1)"),
            "amd radeon 840m graphics"
        );
        assert_eq!(
            normalise_device_name("  NVIDIA   H100 PCIe  "),
            "nvidia h100 pcie"
        );
        assert_eq!(
            normalise_device_name("llvmpipe (LLVM 19.1.0, 256 bits)"),
            "llvmpipe"
        );
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
