//! The fit types: one pure function's input and output, replacing 54 hand-solved recipe
//! strings.
//!
//! The budget is a **vector**, never a scalar — a two-GPU rig with 8 GiB free on each does
//! not have 16 GiB for one tensor. `ctx` is the **total** KV pool shared across `parallel`
//! slots, not a per-slot number.

use crate::rig::GgufMeta;
use serde::{Deserialize, Serialize};

/// Free and reserved VRAM on one device.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceBudget {
    /// The `-dev` token, e.g. `"Vulkan0"`.
    pub device: String,
    /// Free VRAM right now, MiB.
    pub free_mb: u64,
    /// VRAM already promised to running endpoints, MiB.
    pub reserved_mb: u64,
}

/// What the solver is allowed to spend. Computed **live** at plan time, never from a cache.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VramBudget {
    /// One entry per selected device.
    #[serde(default)]
    pub devices: Vec<DeviceBudget>,
    /// Safety margin held back across the whole budget, MiB.
    pub margin_mb: u64,
    /// Free host RAM, MiB — what a `NeedsOffload` plan would actually land in.
    pub host_ram_free_mb: u64,
}

impl VramBudget {
    /// Σ over devices of `free - reserved`, then minus the margin. Saturating throughout.
    pub fn total_usable_mb(&self) -> u64 {
        self.devices
            .iter()
            .map(|d| d.free_mb.saturating_sub(d.reserved_mb))
            .fold(0u64, u64::saturating_add)
            .saturating_sub(self.margin_mb)
    }

    /// The largest single device's usable VRAM, minus the whole margin.
    ///
    /// Deliberately conservative: a tensor that must live on one device cannot borrow the
    /// margin from a sibling.
    pub fn largest_usable_mb(&self) -> u64 {
        self.devices
            .iter()
            .map(|d| d.free_mb.saturating_sub(d.reserved_mb))
            .max()
            .unwrap_or(0)
            .saturating_sub(self.margin_mb)
    }

    /// The `-dev` tokens in this budget, in order.
    pub fn device_names(&self) -> Vec<String> {
        self.devices.iter().map(|d| d.device.clone()).collect()
    }
}

/// Everything `fit()` needs. Pure input: no paths are opened while solving.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FitInput {
    /// Local file bytes (shards summed), or HuggingFace `paths-info` sizes.
    pub weights_bytes: u64,
    /// The header fields that drive KV arithmetic.
    pub gguf: GgufMeta,
    /// LIVE budget, computed at plan time.
    pub budget: VramBudget,
    /// Requested total context pool. `None` lets the solver (or llama.cpp `--fit`) choose.
    #[serde(default)]
    pub want_ctx: Option<u32>,
    /// Requested slot count.
    #[serde(default)]
    pub want_parallel: Option<u32>,
    /// Requested KV quantisation.
    #[serde(default)]
    pub want_kv: Option<KvType>,
    /// Device selection and split policy.
    pub split: SplitPlan,
    /// Logical batch size, for the compute-buffer estimate.
    #[serde(default)]
    pub batch: Option<u32>,
}

/// KV cache element type. The serde spelling is exactly the `-ctk`/`-ctv` flag value.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KvType {
    /// 4 bytes per element.
    F32,
    /// 2 bytes per element.
    F16,
    /// 2 bytes per element.
    Bf16,
    /// ggml block of 32 in 34 bytes.
    Q8_0,
    /// ggml block of 32 in 18 bytes.
    Q4_0,
    /// ggml block of 32 in 20 bytes.
    Q4_1,
    /// ggml block of 32 in 18 bytes.
    Iq4Nl,
    /// ggml block of 32 in 22 bytes.
    Q5_0,
    /// ggml block of 32 in 24 bytes.
    Q5_1,
}

impl KvType {
    /// Bytes per stored element, including ggml block overhead.
    pub fn bytes_per_elem(self) -> f32 {
        match self {
            KvType::F32 => 4.0,
            KvType::F16 | KvType::Bf16 => 2.0,
            KvType::Q8_0 => 34.0 / 32.0,
            KvType::Q5_1 => 24.0 / 32.0,
            KvType::Q5_0 => 22.0 / 32.0,
            KvType::Q4_1 => 20.0 / 32.0,
            KvType::Q4_0 | KvType::Iq4Nl => 18.0 / 32.0,
        }
    }

    /// The literal value for `-ctk`/`-ctv`.
    pub fn as_flag(self) -> &'static str {
        match self {
            KvType::F32 => "f32",
            KvType::F16 => "f16",
            KvType::Bf16 => "bf16",
            KvType::Q8_0 => "q8_0",
            KvType::Q4_0 => "q4_0",
            KvType::Q4_1 => "q4_1",
            KvType::Iq4Nl => "iq4_nl",
            KvType::Q5_0 => "q5_0",
            KvType::Q5_1 => "q5_1",
        }
    }
}

/// The solver's answer, with the arithmetic that produced it.
///
/// `why` is rendered as tooltips beside every derived field in both GUIs. An empty `why`
/// is a bug: a number nobody can explain is a number nobody should trust.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FitPlan {
    /// TOTAL context pool, shared across `parallel` slots.
    pub ctx: u32,
    /// Slot count.
    pub parallel: u32,
    /// KV element type.
    pub kv_type: KvType,
    /// Layer offload policy.
    pub ngl: NglPlan,
    /// Device selection and split policy.
    pub split: SplitPlan,
    /// Weights, MiB.
    pub weights_mb: u64,
    /// KV cache, MiB.
    pub kv_mb: u64,
    /// Compute buffers, MiB.
    pub compute_mb: u64,
    /// What is left over. Negative means it does not fit.
    pub headroom_mb: i64,
    /// Per-device split of the total, in the order of `split.devices`.
    #[serde(default)]
    pub per_device_mb: Vec<(String, u64)>,
    /// The verdict a UI colours on.
    pub verdict: FitVerdict,
    /// Human-readable arithmetic, one line per step.
    #[serde(default)]
    pub why: Vec<String>,
}

/// Does it fit, and by how much.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum FitVerdict {
    /// Comfortable.
    Fits {
        /// Spare VRAM, MiB.
        headroom_mb: u64,
    },
    /// Fits, but close enough that a second process would break it.
    Tight {
        /// Spare VRAM, MiB.
        headroom_mb: u64,
    },
    /// Only some layers can live on the GPU.
    NeedsOffload {
        /// How many layers fit.
        layers_on_gpu: u32,
    },
    /// Not at this ctx/parallel/kv.
    WontFit {
        /// How much more VRAM would be needed, MiB.
        short_by_mb: u64,
    },
}

/// Layer-offload policy.
///
/// `Auto` means **emit no `-ngl` at all**, so llama.cpp's own `--fit` sizing decides.
///
/// Serde note: the contract spells this `#[serde(tag = "ngl")]`, but an internally tagged
/// enum cannot carry a bare `u32` newtype payload. It is adjacently tagged with
/// `content = "n"` — the same idiom [`crate::money::TokenCount`] already uses — so
/// `Layers(32)` is `{"ngl":"layers","n":32}` and the unit variants stay `{"ngl":"auto"}`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "ngl", rename_all = "snake_case", content = "n")]
pub enum NglPlan {
    /// Emit nothing; let llama.cpp size it.
    Auto,
    /// `-ngl 999`.
    All,
    /// An explicit layer count.
    Layers(u32),
}

/// Which devices, and how to spread across them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SplitPlan {
    /// `-dev` tokens, in order. Empty means "whatever llama.cpp picks".
    #[serde(default)]
    pub devices: Vec<String>,
    /// `-sm` value.
    pub mode: SplitMode,
    /// `-mg` value.
    #[serde(default)]
    pub main_gpu: Option<u32>,
    /// `--tensor-split` ratios, one per device.
    #[serde(default)]
    pub tensor_split: Option<Vec<f32>>,
}

impl Default for SplitPlan {
    /// `-sm layer` across whatever devices the caller selects: llama.cpp's own default and
    /// the only mode that is safe without knowing the model's tensor shapes.
    fn default() -> Self {
        SplitPlan {
            devices: Vec::new(),
            mode: SplitMode::Layer,
            main_gpu: None,
            tensor_split: None,
        }
    }
}

/// `-sm` values.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitMode {
    /// Single device.
    None,
    /// Split by layer.
    Layer,
    /// Split by row.
    Row,
    /// Split by tensor.
    Tensor,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kv_type_flags_match_the_serde_spelling() {
        for kv in [
            KvType::F32,
            KvType::F16,
            KvType::Bf16,
            KvType::Q8_0,
            KvType::Q4_0,
            KvType::Q4_1,
            KvType::Iq4Nl,
            KvType::Q5_0,
            KvType::Q5_1,
        ] {
            let json = serde_json::to_string(&kv).expect("ser");
            assert_eq!(json, format!("\"{}\"", kv.as_flag()));
            assert_eq!(serde_json::from_str::<KvType>(&json).expect("de"), kv);
        }
        assert!((KvType::Q8_0.bytes_per_elem() - 1.0625).abs() < 1e-6);
        assert!((KvType::F16.bytes_per_elem() - 2.0).abs() < 1e-6);
    }

    #[test]
    fn ngl_plan_round_trips_including_the_payload_variant() {
        for n in [NglPlan::Auto, NglPlan::All, NglPlan::Layers(32)] {
            let s = serde_json::to_string(&n).expect("ser");
            assert_eq!(serde_json::from_str::<NglPlan>(&s).expect("de"), n);
        }
        assert_eq!(
            serde_json::to_string(&NglPlan::Auto).expect("ser"),
            r#"{"ngl":"auto"}"#
        );
        assert_eq!(
            serde_json::to_string(&NglPlan::Layers(32)).expect("ser"),
            r#"{"ngl":"layers","n":32}"#
        );
    }

    #[test]
    fn fit_verdict_round_trips_every_variant() {
        for v in [
            FitVerdict::Fits { headroom_mb: 2_048 },
            FitVerdict::Tight { headroom_mb: 128 },
            FitVerdict::NeedsOffload { layers_on_gpu: 24 },
            FitVerdict::WontFit { short_by_mb: 4_096 },
        ] {
            let s = serde_json::to_string(&v).expect("ser");
            assert_eq!(serde_json::from_str::<FitVerdict>(&s).expect("de"), v);
        }
        assert_eq!(
            serde_json::to_string(&FitVerdict::Fits { headroom_mb: 1 }).expect("ser"),
            r#"{"verdict":"fits","headroom_mb":1}"#
        );
    }

    #[test]
    fn budget_arithmetic_is_saturating_and_vector_shaped() {
        let b = VramBudget {
            devices: vec![
                DeviceBudget {
                    device: "Vulkan0".into(),
                    free_mb: 8_000,
                    reserved_mb: 5_956,
                },
                DeviceBudget {
                    device: "Vulkan1".into(),
                    free_mb: 8_000,
                    reserved_mb: 0,
                },
            ],
            margin_mb: 1_024,
            host_ram_free_mb: 2_100,
        };
        assert_eq!(b.total_usable_mb(), (2_044 + 8_000) - 1_024);
        assert_eq!(b.largest_usable_mb(), 8_000 - 1_024);
        assert_eq!(b.device_names(), vec!["Vulkan0", "Vulkan1"]);

        let empty = VramBudget {
            devices: vec![DeviceBudget {
                device: "Vulkan0".into(),
                free_mb: 10,
                reserved_mb: 999,
            }],
            margin_mb: 1_024,
            host_ram_free_mb: 0,
        };
        assert_eq!(empty.total_usable_mb(), 0);
        assert_eq!(empty.largest_usable_mb(), 0);
    }

    #[test]
    fn split_plan_default_is_layer_across_nothing() {
        let d = SplitPlan::default();
        assert_eq!(d.mode, SplitMode::Layer);
        assert!(d.devices.is_empty());
        assert!(d.main_gpu.is_none());
        assert!(d.tensor_split.is_none());
    }

    #[test]
    fn fit_input_and_plan_round_trip() {
        let input = FitInput {
            weights_bytes: 6_900_000_000,
            gguf: crate::rig::GgufMeta {
                arch: "qwen3".into(),
                n_layer: 41,
                n_head_kv: 8,
                n_embd_head_k: 128,
                n_embd_head_v: 128,
                n_ctx_train: 32_768,
                full_attn_layers: Some(10),
                n_expert: None,
                quant_desc: None,
            },
            budget: VramBudget::default(),
            want_ctx: Some(32_768),
            want_parallel: Some(4),
            want_kv: Some(KvType::Q8_0),
            split: SplitPlan::default(),
            batch: Some(2_048),
        };
        let s = serde_json::to_string(&input).expect("ser");
        assert_eq!(serde_json::from_str::<FitInput>(&s).expect("de"), input);

        let plan = FitPlan {
            ctx: 32_768,
            parallel: 4,
            kv_type: KvType::Q8_0,
            ngl: NglPlan::All,
            split: SplitPlan::default(),
            weights_mb: 4_861,
            kv_mb: 594,
            compute_mb: 501,
            headroom_mb: -100,
            per_device_mb: vec![("Vulkan0".into(), 5_956)],
            verdict: FitVerdict::WontFit { short_by_mb: 100 },
            why: vec!["weights 4861 MiB = 6.9 GB / 1.4 (Q6_K)".into()],
        };
        let s = serde_json::to_string(&plan).expect("ser");
        assert_eq!(serde_json::from_str::<FitPlan>(&s).expect("de"), plan);
    }
}
