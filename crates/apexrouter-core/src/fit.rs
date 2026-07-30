//! OWNER: unit C-11 (core/fit.rs). Do not edit outside that unit.
//!
//! The fit solver: one **pure** function replacing 54 hand-solved recipe strings.
//!
//! ```text
//! kv_bytes = kv_layers × ctx × n_head_kv × (n_embd_head_k + n_embd_head_v) × bytes_per_elem(kv_type)
//! kv_layers = full_attn_layers.unwrap_or(n_layer)
//! ```
//!
//! `ctx` is the **total** pool shared across `parallel` slots, not a per-slot number.
//! The compute-buffer estimate is calibrated against the archived run log in `docs/port/03`:
//! Qwen3.5-9B Q4_K_M, ctx 32768, kv q8_0, Vulkan → 5956 MiB = 4861 model + 594 context +
//! 501 compute. That fixture is a unit test.
//!
//! The same function is exposed by `apexrouter fit`, `GET /v1/fit`, `apexrouter_fit` (MCP),
//! the Launch drawer's live headroom bar in both GUIs, and the vast rent panel.
//!
//! # What is estimated, and how honestly
//!
//! * **weights** — file bytes, rounded up to MiB. A fact, not a guess.
//! * **kv** — the formula above. Exact arithmetic on header fields.
//! * **compute** — a *heuristic*. The GGUF fields we parse carry neither `n_embd` nor
//!   `n_vocab`, so graph buffers are scaled from the logical batch and a model-width proxy
//!   (weight MiB per layer), normalised to the one archived measurement we have.
//!
//! Every one of those steps appends a line to [`FitPlan::why`], because a number nobody can
//! explain is a number nobody should trust.

use apexrouter_protocol::{
    DesiredState, DeviceBudget, EndpointRecord, FitInput, FitPlan, FitVerdict, GgufMeta, Gpu,
    KvType, NglPlan, RigSnapshot, SplitMode, SplitPlan, VramBudget,
};
use std::collections::BTreeMap;

/// Bytes in one MiB. llama.cpp reports every buffer in MiB, so we do too.
const MIB: u64 = 1024 * 1024;

/// Logical batch assumed when the caller does not say. llama.cpp's `-b` default.
const DEFAULT_BATCH: u32 = 2_048;

/// Slot count assumed when the caller does not say. `-np` default.
const DEFAULT_PARALLEL: u32 = 1;

/// KV type assumed when the caller does not say — the house default from the legacy
/// recipe schema (`kv_type = "q8_0"`), not llama.cpp's f16.
const DEFAULT_KV: KvType = KvType::Q8_0;

/// Smallest context the auto-sizer will propose. Mirrors llama.cpp's `--fit-ctx` default.
const MIN_CTX: u32 = 4_096;

/// Context ceiling used when the header does not report `n_ctx_train`.
const CTX_FALLBACK: u32 = 32_768;

/// Absolute floor for "so little headroom that a second process breaks it", MiB.
const TIGHT_FLOOR_MB: u64 = 512;

/// A plan is [`FitVerdict::Tight`] below this fraction of its own footprint, as a divisor:
/// `total / 20` is 5 %.
const TIGHT_FRACTION_DIVISOR: u64 = 20;

// -- compute-buffer calibration, from docs/port/03-local-endpoint.md ------------------------
//
// "Qwen3.5-9B Q4_K_M, ctx 32768, kv q8_0 → 5956 MiB device (4861 model + 594 context +
// 501 compute). Prompt processing ran at ~2048-token batches."

/// Compute buffers observed in the archived Vulkan run, MiB.
const REF_COMPUTE_MB: f64 = 501.0;
/// Logical batch of that run.
const REF_BATCH: f64 = 2_048.0;
/// Model-width proxy of that run: 4861 MiB of weights over 36 blocks.
const REF_WEIGHTS_MB_PER_LAYER: f64 = 135.0;
/// No graph is free, however small the model.
const COMPUTE_FLOOR_MB: u64 = 64;

/// Solve. Pure: no I/O, no clock, no allocation surprises.
///
/// `FitPlan::why` must come back non-empty and human-readable — a number nobody can explain
/// is a number nobody should trust.
///
/// Behaviour worth knowing before you read the output:
///
/// * `want_ctx: None` makes the solver **search**: the largest power-of-two context at or
///   below `n_ctx_train` that still fits, floored at 4096. An explicit `want_ctx` is never
///   silently shrunk — it is reported as not fitting instead.
/// * `ctx` is the total KV pool. Raising `parallel` divides that pool, it does not multiply
///   the cache, so `kv_mb` is independent of `want_parallel`.
/// * A budget with **no devices** is judged against `host_ram_free_mb` instead, which is the
///   honest reading for a CPU-only or not-yet-scanned rig. `ngl` then comes back
///   [`NglPlan::Auto`] so llama.cpp's own `--fit` decides.
/// * `per_device_mb` describes only what actually lands on the GPUs — under
///   [`FitVerdict::NeedsOffload`] that is less than `weights_mb + kv_mb + compute_mb`.
pub fn fit(input: &FitInput) -> FitPlan {
    let gguf = &input.gguf;
    let mut why: Vec<String> = Vec::new();

    let kv_type = input.want_kv.unwrap_or(DEFAULT_KV);
    let parallel = input.want_parallel.unwrap_or(DEFAULT_PARALLEL).max(1);
    let batch = input.batch.unwrap_or(DEFAULT_BATCH).max(1);

    let weights_mb = input.weights_bytes.div_ceil(MIB);
    let compute_mb = compute_buffer_mb(weights_mb, gguf.n_layer, batch);

    // ---- what we are allowed to spend -----------------------------------------------
    let gpu_backed = !input.budget.devices.is_empty();
    let usable_mb = if gpu_backed {
        input.budget.total_usable_mb()
    } else {
        input
            .budget
            .host_ram_free_mb
            .saturating_sub(input.budget.margin_mb)
    };
    if gpu_backed {
        why.push(format!(
            "budget {usable_mb} MiB usable = Σ(free − reserved) over {} device(s) − {} MiB margin",
            input.budget.devices.len(),
            input.budget.margin_mb
        ));
    } else {
        why.push(format!(
            "budget {usable_mb} MiB usable = {} MiB free host RAM − {} MiB margin (no GPU device in the budget, so -ngl is left to llama.cpp --fit)",
            input.budget.host_ram_free_mb, input.budget.margin_mb
        ));
    }

    // ---- context ---------------------------------------------------------------------
    let ctx = match input.want_ctx {
        Some(c) => {
            let c = c.max(1);
            why.push(format!(
                "ctx {c} requested (total pool shared across {parallel} slot(s); model trained for {})",
                gguf.n_ctx_train
            ));
            c
        }
        None => {
            let head = weights_mb.saturating_add(compute_mb);
            let room = usable_mb.saturating_sub(head);
            let chosen = largest_ctx_within(gguf, kv_type, room);
            why.push(format!(
                "ctx {chosen} auto-sized: largest power of two ≤ {} leaving KV inside {room} MiB (floor {MIN_CTX})",
                ctx_ceiling(gguf)
            ));
            chosen
        }
    };

    let kv_mb = kv_cache_mb(gguf, ctx, kv_type);
    let kv_layers = gguf.full_attn_layers.unwrap_or(gguf.n_layer);

    why.push(format!(
        "weights {weights_mb} MiB = {} B of GGUF tensors",
        input.weights_bytes
    ));
    why.push(format!(
        "kv {kv_mb} MiB = {kv_layers} kv layer(s){} × {ctx} ctx × {} kv head(s) × ({}+{}) × {:.4} B/elem ({})",
        match gguf.full_attn_layers {
            Some(f) => format!(" of {} (hybrid: full_attn_layers={f})", gguf.n_layer),
            None => String::new(),
        },
        gguf.n_head_kv,
        gguf.n_embd_head_k,
        gguf.n_embd_head_v,
        kv_type.bytes_per_elem(),
        kv_type.as_flag()
    ));
    why.push(format!(
        "compute {compute_mb} MiB estimated at batch {batch} (≈{:.0} MiB of weights per layer; calibrated to the archived {REF_COMPUTE_MB:.0} MiB Vulkan run at batch {REF_BATCH:.0})",
        weights_mb as f64 / f64::from(gguf.n_layer.max(1))
    ));

    // ---- the verdict -------------------------------------------------------------------
    let total_mb = weights_mb.saturating_add(kv_mb).saturating_add(compute_mb);
    let headroom_mb = i64::try_from(usable_mb)
        .unwrap_or(i64::MAX)
        .saturating_sub(i64::try_from(total_mb).unwrap_or(i64::MAX));
    why.push(format!(
        "total {total_mb} MiB = {weights_mb} weights + {kv_mb} kv + {compute_mb} compute vs {usable_mb} MiB usable → {headroom_mb} MiB headroom"
    ));

    let tight_at = TIGHT_FLOOR_MB.max(total_mb / TIGHT_FRACTION_DIVISOR);
    let (verdict, ngl, weights_on_gpu_mb) = if headroom_mb >= 0 {
        let spare = headroom_mb.unsigned_abs();
        if spare < tight_at {
            why.push(format!(
                "tight: {spare} MiB spare is under the {tight_at} MiB threshold — a second process on this device will break it"
            ));
            (
                FitVerdict::Tight { headroom_mb: spare },
                all_or_auto(gpu_backed),
                weights_mb,
            )
        } else {
            why.push(format!("fits with {spare} MiB spare"));
            (
                FitVerdict::Fits { headroom_mb: spare },
                all_or_auto(gpu_backed),
                weights_mb,
            )
        }
    } else {
        let short_by_mb = headroom_mb.unsigned_abs();
        let for_weights = usable_mb.saturating_sub(kv_mb.saturating_add(compute_mb));
        let per_layer = weights_mb as f64 / f64::from(gguf.n_layer.max(1));
        let layers = if per_layer > 0.0 {
            (for_weights as f64 / per_layer).floor().max(0.0) as u32
        } else {
            0
        };
        if gpu_backed && gguf.n_layer > 0 && layers >= 1 && layers < gguf.n_layer {
            let on_gpu = (f64::from(layers) * per_layer).round() as u64;
            why.push(format!(
                "short by {short_by_mb} MiB: {layers}/{} layers fit on the GPU ({on_gpu} MiB), the rest runs from the {} MiB of free host RAM",
                gguf.n_layer, input.budget.host_ram_free_mb
            ));
            (
                FitVerdict::NeedsOffload {
                    layers_on_gpu: layers,
                },
                NglPlan::Layers(layers),
                on_gpu,
            )
        } else {
            why.push(format!(
                "short by {short_by_mb} MiB even before offload (kv + compute alone need {} MiB) — lower ctx, quantise the KV cache, or add a device",
                kv_mb.saturating_add(compute_mb)
            ));
            (
                FitVerdict::WontFit { short_by_mb },
                NglPlan::Auto,
                weights_mb,
            )
        }
    };

    // ---- how it lands on the devices ----------------------------------------------------
    let devices = if input.split.devices.is_empty() {
        input.budget.device_names()
    } else {
        input.split.devices.clone()
    };
    let on_gpu_mb = weights_on_gpu_mb
        .saturating_add(kv_mb)
        .saturating_add(compute_mb);
    let per_device_mb = split_across(on_gpu_mb, &devices, &input.split);
    if !per_device_mb.is_empty() {
        why.push(format!(
            "split: {} across {} — {}",
            split_mode_flag(input.split.mode),
            devices.join(", "),
            per_device_mb
                .iter()
                .map(|(d, mb)| format!("{d} {mb} MiB"))
                .collect::<Vec<_>>()
                .join(" · ")
        ));
    }

    FitPlan {
        ctx,
        parallel,
        kv_type,
        ngl,
        split: SplitPlan {
            devices,
            mode: input.split.mode,
            main_gpu: input.split.main_gpu,
            tensor_split: input.split.tensor_split.clone(),
        },
        weights_mb,
        kv_mb,
        compute_mb,
        headroom_mb,
        per_device_mb,
        verdict,
        why,
    }
}

/// Build a **live** budget from the rig, subtracting what running endpoints already hold.
///
/// Subtracts `weights + kv + compute` of every `EndpointRecord` whose
/// `desired == DesiredState::Running`, which is what makes `InsufficientVram` fire before a
/// second launch OOMs the first.
///
/// An empty `devices` selects every non-software GPU on the rig; a non-empty one selects
/// exactly those `-dev` tokens, **in the order given**, and silently drops names the rig does
/// not have. A running endpoint's reservation is attributed by its `per_device_mb` when it has
/// one, else spread evenly over its `split.devices`, else charged whole to the first selected
/// device — the conservative reading, since an endpoint with no recorded placement is most
/// likely on the primary GPU. `Gpu::reserved_mb` from the snapshot is respected as a floor, so
/// a holder the caller did not pass in `running` still cannot be double-spent.
pub fn budget_from_rig(
    rig: &RigSnapshot,
    devices: &[String],
    margin_mb: u64,
    running: &[EndpointRecord],
) -> VramBudget {
    let selected: Vec<&Gpu> = if devices.is_empty() {
        rig.gpus.iter().filter(|g| !g.is_software).collect()
    } else {
        devices
            .iter()
            .filter_map(|want| rig.gpus.iter().find(|g| &g.device == want))
            .collect()
    };

    let mut reserved: BTreeMap<&str, u64> = BTreeMap::new();
    let fallback = selected.first().copied().map(|g| g.device.as_str());
    for rec in running
        .iter()
        .filter(|r| r.desired == DesiredState::Running)
    {
        let Some(plan) = rec.fit.as_ref() else {
            continue;
        };
        let total = plan
            .weights_mb
            .saturating_add(plan.kv_mb)
            .saturating_add(plan.compute_mb);
        if !plan.per_device_mb.is_empty() {
            for (device, mb) in &plan.per_device_mb {
                let slot = reserved.entry(device.as_str()).or_default();
                *slot = slot.saturating_add(*mb);
            }
        } else if !plan.split.devices.is_empty() {
            for (device, mb) in even_shares(total, &plan.split.devices) {
                let slot = reserved.entry(device).or_default();
                *slot = slot.saturating_add(mb);
            }
        } else if let Some(device) = fallback {
            let slot = reserved.entry(device).or_default();
            *slot = slot.saturating_add(total);
        }
    }

    VramBudget {
        devices: selected
            .into_iter()
            .map(|g| DeviceBudget {
                device: g.device.clone(),
                free_mb: g.vram_free_mb,
                reserved_mb: g
                    .reserved_mb
                    .max(reserved.get(g.device.as_str()).copied().unwrap_or(0)),
            })
            .collect(),
        margin_mb,
        host_ram_free_mb: rig.ram_free_mb,
    }
}

// ---------------------------------------------------------------------------------------
// internals
// ---------------------------------------------------------------------------------------

/// `-ngl 999` when we know the devices, otherwise let llama.cpp's `--fit` size it.
fn all_or_auto(gpu_backed: bool) -> NglPlan {
    if gpu_backed {
        NglPlan::All
    } else {
        NglPlan::Auto
    }
}

/// The `-sm` spelling, for the `why` line.
fn split_mode_flag(mode: SplitMode) -> &'static str {
    match mode {
        SplitMode::None => "none",
        SplitMode::Layer => "layer",
        SplitMode::Row => "row",
        SplitMode::Tensor => "tensor",
    }
}

/// KV cache size in MiB for one total context pool.
///
/// `kv_layers = full_attn_layers.unwrap_or(n_layer)`: hybrid-linear models carry KV on only
/// some of their blocks, and paying for all 41 when only 10 are attention layers is how a
/// 27B MoE gets wrongly declared unfittable.
fn kv_cache_mb(gguf: &GgufMeta, ctx: u32, kv_type: KvType) -> u64 {
    let layers = u64::from(gguf.full_attn_layers.unwrap_or(gguf.n_layer));
    let per_token = u64::from(gguf.n_head_kv)
        .saturating_mul(u64::from(gguf.n_embd_head_k) + u64::from(gguf.n_embd_head_v));
    let elems = layers
        .saturating_mul(u64::from(ctx))
        .saturating_mul(per_token);
    let bytes = (elems as f64 * f64::from(kv_type.bytes_per_elem())).ceil();
    if !bytes.is_finite() || bytes <= 0.0 {
        return 0;
    }
    (bytes as u64).div_ceil(MIB)
}

/// Compute-buffer estimate, MiB.
///
/// The GGUF fields we parse carry neither `n_embd` nor `n_vocab`, so this scales the one
/// archived Vulkan measurement (501 MiB at batch 2048 for a model with ≈135 MiB of weights
/// per block) linearly in batch and in weight-bytes per layer. It is a heuristic and the
/// `why` line says so.
fn compute_buffer_mb(weights_mb: u64, n_layer: u32, batch: u32) -> u64 {
    let width = (weights_mb as f64 / f64::from(n_layer.max(1))) / REF_WEIGHTS_MB_PER_LAYER;
    let scaled = REF_COMPUTE_MB * (f64::from(batch) / REF_BATCH) * width;
    let mb = if scaled.is_finite() && scaled > 0.0 {
        scaled.round() as u64
    } else {
        0
    };
    mb.max(COMPUTE_FLOOR_MB)
}

/// The largest context the model admits: `n_ctx_train`, or a sane fallback when the header
/// did not report one.
fn ctx_ceiling(gguf: &GgufMeta) -> u32 {
    if gguf.n_ctx_train == 0 {
        CTX_FALLBACK
    } else {
        gguf.n_ctx_train
    }
}

/// Largest context whose KV cache fits in `room_mb`: the ceiling itself first, then powers of
/// two down to [`MIN_CTX`]. Returns [`MIN_CTX`] (or the ceiling, if smaller) when none fits —
/// the caller then reports a shortfall rather than pretending.
fn largest_ctx_within(gguf: &GgufMeta, kv_type: KvType, room_mb: u64) -> u32 {
    let ceiling = ctx_ceiling(gguf);
    let mut candidates: Vec<u32> = vec![ceiling];
    // Highest power of two ≤ ceiling, computed without `next_power_of_two`, which overflows
    // for a ceiling above 2^31.
    let mut c = 1u32 << (31 - ceiling.leading_zeros());
    while c >= MIN_CTX {
        candidates.push(c);
        c /= 2;
    }
    candidates.dedup();
    for cand in &candidates {
        if kv_cache_mb(gguf, *cand, kv_type) <= room_mb {
            return *cand;
        }
    }
    MIN_CTX.min(ceiling.max(1))
}

/// Spread `total` over `devices` as evenly as integers allow, remainder to the front.
fn even_shares<'a, S: AsRef<str> + 'a>(total: u64, devices: &'a [S]) -> Vec<(&'a str, u64)> {
    let n = devices.len() as u64;
    if n == 0 {
        return Vec::new();
    }
    let base = total / n;
    let rem = total % n;
    devices
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let extra = u64::from((i as u64) < rem);
            (d.as_ref(), base + extra)
        })
        .collect()
}

/// How `total` MiB of resident buffers lands on the selected devices.
///
/// `tensor_split` wins when it has one ratio per device and a positive sum; otherwise
/// [`SplitMode::None`] puts everything on `main_gpu` (default device 0) and every other mode
/// splits evenly.
fn split_across(total: u64, devices: &[String], split: &SplitPlan) -> Vec<(String, u64)> {
    if devices.is_empty() {
        return Vec::new();
    }
    if let Some(ratios) = split.tensor_split.as_ref() {
        let sum: f64 = ratios.iter().map(|r| f64::from(*r).max(0.0)).sum();
        if ratios.len() == devices.len() && sum > 0.0 {
            let mut out: Vec<(String, u64)> = Vec::with_capacity(devices.len());
            let mut assigned = 0u64;
            for (i, device) in devices.iter().enumerate() {
                let mb = if i + 1 == devices.len() {
                    total.saturating_sub(assigned)
                } else {
                    let share = (total as f64 * f64::from(ratios[i]).max(0.0) / sum).round() as u64;
                    let share = share.min(total.saturating_sub(assigned));
                    assigned = assigned.saturating_add(share);
                    share
                };
                out.push((device.clone(), mb));
            }
            return out;
        }
    }
    if split.mode == SplitMode::None {
        let main = split.main_gpu.unwrap_or(0) as usize;
        let main = if main < devices.len() { main } else { 0 };
        return devices
            .iter()
            .enumerate()
            .map(|(i, d)| (d.clone(), if i == main { total } else { 0 }))
            .collect();
    }
    even_shares(total, devices)
        .into_iter()
        .map(|(d, mb)| (d.to_string(), mb))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_protocol::{BackendId, BuildId, EndpointSpec, LocalLlamaSpec, SamplingMode};

    // ---- the archived run log ------------------------------------------------------------
    //
    // docs/port/03-local-endpoint.md §"Hardware reality":
    //   Qwen3.5-9B Q4_K_M, ctx 32768, kv q8_0 → 5956 MiB device
    //   (4861 model + 594 context + 501 compute).
    //
    // The GGUF that produced that log is gone (docs/port/00-machine-ground-truth.md notes the
    // saved instance points at a `~/models/Qwen3.5-9B-Q4_K_M.gguf` that no longer exists), so
    // the header below is reconstructed to the shape the log implies: a 9 B-class GQA model
    // with 36 blocks, 2 KV heads and 128-wide K/V heads. Weight bytes are the observed model
    // buffer. Every derived number must land within ±10 % of the log.

    const LOG_TOTAL_MB: u64 = 5_956;
    const LOG_WEIGHTS_MB: u64 = 4_861;
    const LOG_KV_MB: u64 = 594;
    const LOG_COMPUTE_MB: u64 = 501;
    const LOG_WEIGHTS_BYTES: u64 = LOG_WEIGHTS_MB * MIB;

    fn log_gguf() -> GgufMeta {
        GgufMeta {
            arch: "qwen3".into(),
            n_layer: 36,
            n_head_kv: 2,
            n_embd_head_k: 128,
            n_embd_head_v: 128,
            n_ctx_train: 32_768,
            full_attn_layers: None,
            n_expert: None,
            quant_desc: Some("Q4_K_M".into()),
        }
    }

    fn budget(devices: &[(&str, u64, u64)], margin_mb: u64, host_ram_free_mb: u64) -> VramBudget {
        VramBudget {
            devices: devices
                .iter()
                .map(|(d, free, reserved)| DeviceBudget {
                    device: (*d).into(),
                    free_mb: *free,
                    reserved_mb: *reserved,
                })
                .collect(),
            margin_mb,
            host_ram_free_mb,
        }
    }

    /// The 840M reports 20992 MiB total / 19519 MiB free (00-machine-ground-truth).
    fn one_vulkan_gpu() -> VramBudget {
        budget(&[("Vulkan0", 19_519, 0)], 1_024, 8_000)
    }

    fn log_input() -> FitInput {
        FitInput {
            weights_bytes: LOG_WEIGHTS_BYTES,
            gguf: log_gguf(),
            budget: one_vulkan_gpu(),
            want_ctx: Some(32_768),
            want_parallel: Some(1),
            want_kv: Some(KvType::Q8_0),
            split: SplitPlan {
                devices: vec!["Vulkan0".into()],
                mode: SplitMode::Layer,
                main_gpu: None,
                tensor_split: None,
            },
            batch: Some(2_048),
        }
    }

    fn within_pct(got: u64, want: u64, pct: f64) -> bool {
        let delta = (got as f64 - want as f64).abs();
        delta <= want as f64 * pct / 100.0
    }

    #[test]
    fn calibrates_against_the_archived_run_log_within_ten_percent() {
        let plan = fit(&log_input());
        assert!(
            within_pct(plan.weights_mb, LOG_WEIGHTS_MB, 10.0),
            "weights {} vs {LOG_WEIGHTS_MB}",
            plan.weights_mb
        );
        assert!(
            within_pct(plan.kv_mb, LOG_KV_MB, 10.0),
            "kv {} vs {LOG_KV_MB}",
            plan.kv_mb
        );
        assert!(
            within_pct(plan.compute_mb, LOG_COMPUTE_MB, 10.0),
            "compute {} vs {LOG_COMPUTE_MB}",
            plan.compute_mb
        );
        let total = plan.weights_mb + plan.kv_mb + plan.compute_mb;
        assert!(
            within_pct(total, LOG_TOTAL_MB, 10.0),
            "total {total} vs {LOG_TOTAL_MB}"
        );
        assert_eq!(plan.ctx, 32_768);
        assert_eq!(plan.parallel, 1);
        assert_eq!(plan.kv_type, KvType::Q8_0);
        assert_eq!(plan.ngl, NglPlan::All);
        assert!(matches!(plan.verdict, FitVerdict::Fits { .. }));
        // 19519 free - 1024 margin - total.
        assert_eq!(plan.headroom_mb, 19_519 - 1_024 - total as i64);
        assert_eq!(plan.per_device_mb, vec![("Vulkan0".to_string(), total)]);
    }

    #[test]
    fn why_is_non_empty_and_names_every_component() {
        let plan = fit(&log_input());
        assert!(plan.why.len() >= 5, "{:?}", plan.why);
        let joined = plan.why.join("\n");
        for needle in ["budget", "weights", "kv", "compute", "total", "split"] {
            assert!(joined.contains(needle), "no `{needle}` in why:\n{joined}");
        }
        // The arithmetic, not just the answer.
        assert!(joined.contains("32768"), "{joined}");
        assert!(joined.contains("q8_0"), "{joined}");
        for line in &plan.why {
            assert!(!line.trim().is_empty());
        }
    }

    #[test]
    fn hybrid_models_pay_kv_only_for_full_attn_layers() {
        let mut dense = log_gguf();
        dense.n_layer = 41;
        dense.full_attn_layers = None;
        let mut hybrid = dense.clone();
        hybrid.full_attn_layers = Some(10);

        let mk = |g: GgufMeta| FitInput {
            gguf: g,
            ..log_input()
        };
        let dense_plan = fit(&mk(dense));
        let hybrid_plan = fit(&mk(hybrid));

        // 10 of 41 layers carry KV.
        let expected = dense_plan.kv_mb * 10 / 41;
        assert!(
            (hybrid_plan.kv_mb as i64 - expected as i64).abs() <= 1,
            "hybrid {} vs expected {expected}",
            hybrid_plan.kv_mb
        );
        assert!(hybrid_plan.kv_mb < dense_plan.kv_mb);
        assert!(hybrid_plan.why.iter().any(|w| w.contains("hybrid")));
    }

    #[test]
    fn ctx_is_the_total_pool_not_a_per_slot_number() {
        let one = fit(&FitInput {
            want_parallel: Some(1),
            ..log_input()
        });
        let eight = fit(&FitInput {
            want_parallel: Some(8),
            ..log_input()
        });
        assert_eq!(one.kv_mb, eight.kv_mb, "kv must not scale with -np");
        assert_eq!(one.ctx, eight.ctx);
        assert_eq!(eight.parallel, 8);

        // It does scale with ctx, linearly.
        let half = fit(&FitInput {
            want_ctx: Some(16_384),
            ..log_input()
        });
        assert_eq!(half.kv_mb, one.kv_mb / 2);
    }

    #[test]
    fn kv_quantisation_changes_the_cache_size() {
        let q8 = fit(&FitInput {
            want_kv: Some(KvType::Q8_0),
            ..log_input()
        });
        let f16 = fit(&FitInput {
            want_kv: Some(KvType::F16),
            ..log_input()
        });
        let q4 = fit(&FitInput {
            want_kv: Some(KvType::Q4_0),
            ..log_input()
        });
        assert!(q4.kv_mb < q8.kv_mb && q8.kv_mb < f16.kv_mb);
        // f16 is exactly 2 bytes/elem against q8_0's 34/32.
        assert_eq!(f16.kv_mb, q8.kv_mb * 32 / 17);
        // No want_kv at all falls back to the house default, q8_0.
        let defaulted = fit(&FitInput {
            want_kv: None,
            ..log_input()
        });
        assert_eq!(defaulted.kv_type, KvType::Q8_0);
        assert_eq!(defaulted.kv_mb, q8.kv_mb);
    }

    #[test]
    fn a_multi_device_budget_sums_and_splits_evenly_by_layer() {
        let two = budget(&[("CUDA0", 24_576, 0), ("CUDA1", 24_576, 0)], 1_024, 64_000);
        assert_eq!(two.total_usable_mb(), 24_576 * 2 - 1_024);
        let plan = fit(&FitInput {
            budget: two,
            split: SplitPlan {
                devices: vec!["CUDA0".into(), "CUDA1".into()],
                mode: SplitMode::Layer,
                main_gpu: None,
                tensor_split: None,
            },
            ..log_input()
        });
        let total = plan.weights_mb + plan.kv_mb + plan.compute_mb;
        assert_eq!(plan.per_device_mb.len(), 2);
        let summed: u64 = plan.per_device_mb.iter().map(|(_, mb)| mb).sum();
        assert_eq!(summed, total);
        let (a, b) = (plan.per_device_mb[0].1, plan.per_device_mb[1].1);
        assert!(a.abs_diff(b) <= 1, "{a} vs {b}");
        assert!(matches!(plan.verdict, FitVerdict::Fits { .. }));
    }

    #[test]
    fn tensor_split_overrides_the_even_layer_split() {
        let two = budget(&[("CUDA0", 24_576, 0), ("CUDA1", 8_192, 0)], 1_024, 64_000);
        let plan = fit(&FitInput {
            budget: two,
            split: SplitPlan {
                devices: vec!["CUDA0".into(), "CUDA1".into()],
                mode: SplitMode::Layer,
                main_gpu: Some(0),
                tensor_split: Some(vec![3.0, 1.0]),
            },
            ..log_input()
        });
        let total = plan.weights_mb + plan.kv_mb + plan.compute_mb;
        let summed: u64 = plan.per_device_mb.iter().map(|(_, mb)| mb).sum();
        assert_eq!(summed, total, "a split must not invent or lose VRAM");
        let (a, b) = (plan.per_device_mb[0].1, plan.per_device_mb[1].1);
        assert!(
            (a as f64 / total as f64 - 0.75).abs() < 0.01,
            "{a}/{total} should be ≈3/4"
        );
        assert!((b as f64 / total as f64 - 0.25).abs() < 0.01);
        // `-sm none` puts it all on the main gpu instead.
        let solo = fit(&FitInput {
            budget: budget(&[("CUDA0", 24_576, 0), ("CUDA1", 24_576, 0)], 1_024, 64_000),
            split: SplitPlan {
                devices: vec!["CUDA0".into(), "CUDA1".into()],
                mode: SplitMode::None,
                main_gpu: Some(1),
                tensor_split: None,
            },
            ..log_input()
        });
        assert_eq!(solo.per_device_mb[0].1, 0);
        assert_eq!(
            solo.per_device_mb[1].1,
            solo.weights_mb + solo.kv_mb + solo.compute_mb
        );
    }

    #[test]
    fn an_absent_want_ctx_auto_sizes_down_to_what_fits() {
        // Plenty of room: take the trained ceiling.
        let roomy = fit(&FitInput {
            want_ctx: None,
            budget: budget(&[("CUDA0", 24_576, 0)], 1_024, 64_000),
            ..log_input()
        });
        assert_eq!(roomy.ctx, 32_768);
        assert!(roomy.why.iter().any(|w| w.contains("auto-sized")));

        // Barely any: shrink rather than overflow.
        let cramped = fit(&FitInput {
            want_ctx: None,
            budget: budget(&[("Vulkan0", 6_200, 0)], 256, 8_000),
            ..log_input()
        });
        assert!(cramped.ctx < 32_768, "ctx {}", cramped.ctx);
        assert!(cramped.ctx >= MIN_CTX);
        assert!(cramped.ctx.is_power_of_two());

        // An explicit ctx is never silently shrunk.
        let explicit = fit(&FitInput {
            want_ctx: Some(32_768),
            budget: budget(&[("Vulkan0", 6_200, 0)], 256, 8_000),
            ..log_input()
        });
        assert_eq!(explicit.ctx, 32_768);
    }

    #[test]
    fn a_model_bigger_than_the_gpu_needs_offload_then_wont_fit() {
        // Room for some layers, not all.
        let partial = fit(&FitInput {
            budget: budget(&[("Vulkan0", 3_500, 0)], 256, 32_000),
            ..log_input()
        });
        match partial.verdict {
            FitVerdict::NeedsOffload { layers_on_gpu } => {
                assert!((1..36).contains(&layers_on_gpu), "{layers_on_gpu}");
                assert_eq!(partial.ngl, NglPlan::Layers(layers_on_gpu));
                // per_device_mb describes only the resident part.
                let on_gpu: u64 = partial.per_device_mb.iter().map(|(_, mb)| mb).sum();
                assert!(on_gpu < partial.weights_mb + partial.kv_mb + partial.compute_mb);
            }
            other => panic!("expected NeedsOffload, got {other:?}"),
        }
        assert!(partial.headroom_mb < 0);

        // Not even the KV and the graph fit.
        let hopeless = fit(&FitInput {
            budget: budget(&[("Vulkan0", 900, 0)], 256, 32_000),
            ..log_input()
        });
        match hopeless.verdict {
            FitVerdict::WontFit { short_by_mb } => {
                assert_eq!(short_by_mb, hopeless.headroom_mb.unsigned_abs());
                assert_eq!(hopeless.ngl, NglPlan::Auto);
            }
            other => panic!("expected WontFit, got {other:?}"),
        }
    }

    #[test]
    fn a_nearly_full_gpu_is_tight_not_fits() {
        let total_of = |p: &FitPlan| p.weights_mb + p.kv_mb + p.compute_mb;
        let reference = fit(&log_input());
        let need = total_of(&reference);
        let plan = fit(&FitInput {
            budget: budget(&[("Vulkan0", need + 100, 0)], 0, 32_000),
            ..log_input()
        });
        match plan.verdict {
            FitVerdict::Tight { headroom_mb } => assert_eq!(headroom_mb, 100),
            other => panic!("expected Tight, got {other:?}"),
        }
        assert!(plan.why.iter().any(|w| w.contains("tight")));
    }

    #[test]
    fn an_empty_budget_is_judged_against_host_ram_and_leaves_ngl_to_llama_cpp() {
        let plan = fit(&FitInput {
            budget: VramBudget {
                devices: vec![],
                margin_mb: 1_024,
                host_ram_free_mb: 20_000,
            },
            split: SplitPlan::default(),
            ..log_input()
        });
        assert_eq!(plan.ngl, NglPlan::Auto);
        assert!(matches!(plan.verdict, FitVerdict::Fits { .. }));
        assert!(plan.per_device_mb.is_empty());
        assert!(plan.why.iter().any(|w| w.contains("host RAM")));
    }

    // ---- budget_from_rig -------------------------------------------------------------------

    fn gpu(device: &str, index: u32, free: u64, reserved: u64) -> Gpu {
        Gpu {
            device: device.into(),
            index,
            name: format!("test {device}"),
            backend: apexrouter_protocol::GpuBackend::Vulkan,
            vram_total_mb: 20_992,
            vram_free_mb: free,
            driver: None,
            is_software: false,
            seen_by_builds: vec![],
            held_by: vec![],
            reserved_mb: reserved,
        }
    }

    fn rig(gpus: Vec<Gpu>) -> RigSnapshot {
        RigSnapshot {
            gpus,
            builds: vec![],
            ram_total_mb: 24_000,
            ram_free_mb: 9_000,
            swap_total_mb: 8_000,
            swap_used_mb: 1_000,
            cpu_threads: 12,
            scanned_at_unix: 1_700_000_000,
        }
    }

    fn record(id: &str, desired: DesiredState, plan: Option<FitPlan>) -> EndpointRecord {
        EndpointRecord {
            id: BackendId::parse(id).expect("valid id"),
            spec: EndpointSpec::LocalLlama(LocalLlamaSpec {
                build: BuildId::parse("build-vulkan").expect("valid build id"),
                model_path: "/models/x.gguf".into(),
                mmproj: None,
                alias_flag: "x".into(),
                host: "127.0.0.1".into(),
                port: None,
                ctx: None,
                parallel: None,
                kv_type: None,
                ngl: NglPlan::All,
                split: SplitPlan::default(),
                mode: SamplingMode::Thinking,
                flash_attn: None,
                api_key: None,
                extra_args: vec![],
            }),
            desired,
            proc: None,
            port: Some(8_100),
            log_path: None,
            started_at_unix: 1_700_000_000,
            fit: plan,
            adopted: false,
            alias_bindings: vec![],
        }
    }

    fn held(devices: &[&str], per_device: &[(&str, u64)]) -> FitPlan {
        FitPlan {
            ctx: 32_768,
            parallel: 1,
            kv_type: KvType::Q8_0,
            ngl: NglPlan::All,
            split: SplitPlan {
                devices: devices.iter().map(|d| (*d).to_string()).collect(),
                mode: SplitMode::Layer,
                main_gpu: None,
                tensor_split: None,
            },
            weights_mb: 4_861,
            kv_mb: 594,
            compute_mb: 501,
            headroom_mb: 0,
            per_device_mb: per_device
                .iter()
                .map(|(d, mb)| ((*d).to_string(), *mb))
                .collect(),
            verdict: FitVerdict::Fits { headroom_mb: 0 },
            why: vec![],
        }
    }

    #[test]
    fn budget_from_rig_subtracts_running_reservations_only() {
        let r = rig(vec![gpu("Vulkan0", 0, 19_519, 0)]);
        let running = vec![
            record(
                "a",
                DesiredState::Running,
                Some(held(&["Vulkan0"], &[("Vulkan0", 5_956)])),
            ),
            // Stopped: not a reservation.
            record(
                "b",
                DesiredState::Stopped,
                Some(held(&["Vulkan0"], &[("Vulkan0", 5_956)])),
            ),
            // No fit recorded: nothing to subtract.
            record("c", DesiredState::Running, None),
        ];
        let b = budget_from_rig(&r, &[], 1_024, &running);
        assert_eq!(b.devices.len(), 1);
        assert_eq!(b.devices[0].device, "Vulkan0");
        assert_eq!(b.devices[0].free_mb, 19_519);
        assert_eq!(b.devices[0].reserved_mb, 5_956);
        assert_eq!(b.margin_mb, 1_024);
        assert_eq!(b.host_ram_free_mb, 9_000);
        assert_eq!(b.total_usable_mb(), 19_519 - 5_956 - 1_024);
    }

    #[test]
    fn budget_from_rig_falls_back_to_weights_plus_kv_plus_compute() {
        let r = rig(vec![gpu("Vulkan0", 0, 19_519, 0)]);
        // No per_device_mb: the record's own weights + kv + compute is the reservation.
        let running = vec![record(
            "a",
            DesiredState::Running,
            Some(held(&["Vulkan0"], &[])),
        )];
        let b = budget_from_rig(&r, &[], 0, &running);
        assert_eq!(b.devices[0].reserved_mb, 4_861 + 594 + 501);

        // Not even a device list: charged whole to the first selected device.
        let running = vec![record("a", DesiredState::Running, Some(held(&[], &[])))];
        let b = budget_from_rig(&r, &[], 0, &running);
        assert_eq!(b.devices[0].reserved_mb, 5_956);
    }

    #[test]
    fn budget_from_rig_selects_devices_in_the_order_asked_and_skips_software() {
        let mut soft = gpu("Vulkan1", 1, 4_096, 0);
        soft.is_software = true;
        let r = rig(vec![
            gpu("Vulkan0", 0, 10_000, 0),
            soft,
            gpu("CUDA0", 0, 24_576, 0),
        ]);

        // Empty selection: every non-software device, in rig order.
        let all = budget_from_rig(&r, &[], 0, &[]);
        assert_eq!(all.device_names(), vec!["Vulkan0", "CUDA0"]);

        // Explicit selection wins, in the caller's order, and may name a software device.
        let picked = budget_from_rig(
            &r,
            &["CUDA0".to_string(), "Vulkan1".to_string(), "nope".into()],
            0,
            &[],
        );
        assert_eq!(picked.device_names(), vec!["CUDA0", "Vulkan1"]);
    }

    #[test]
    fn budget_from_rig_spreads_a_multi_device_reservation_and_honours_the_snapshot_floor() {
        let r = rig(vec![
            gpu("CUDA0", 0, 24_576, 0),
            // The snapshot already knows about a holder we were not told about.
            gpu("CUDA1", 1, 24_576, 2_000),
        ]);
        let running = vec![record(
            "a",
            DesiredState::Running,
            Some(held(&["CUDA0", "CUDA1"], &[])),
        )];
        let b = budget_from_rig(&r, &[], 512, &running);
        let total = 4_861 + 594 + 501;
        assert_eq!(b.devices[0].reserved_mb, total / 2);
        // max(snapshot 2000, computed half) — the snapshot floor wins here.
        assert_eq!(b.devices[1].reserved_mb, 2_000.max(total / 2 + total % 2));
    }

    #[test]
    fn a_budget_from_the_rig_feeds_straight_back_into_fit() {
        let r = rig(vec![gpu("Vulkan0", 0, 19_519, 0)]);
        let b = budget_from_rig(&r, &["Vulkan0".to_string()], 1_024, &[]);
        let first = fit(&FitInput {
            budget: b,
            ..log_input()
        });
        assert!(matches!(first.verdict, FitVerdict::Fits { .. }));

        // Now that endpoint is running; a second copy no longer fits comfortably.
        let running = vec![record("a", DesiredState::Running, Some(first.clone()))];
        let b2 = budget_from_rig(&r, &["Vulkan0".to_string()], 1_024, &running);
        let second = fit(&FitInput {
            budget: b2,
            ..log_input()
        });
        assert!(second.headroom_mb < first.headroom_mb);
        assert!(
            matches!(
                second.verdict,
                FitVerdict::Fits { .. } | FitVerdict::Tight { .. }
            ),
            "{:?}",
            second.verdict
        );

        // Three of them do not.
        let running = vec![
            record("a", DesiredState::Running, Some(first.clone())),
            record("b", DesiredState::Running, Some(first.clone())),
            record("c", DesiredState::Running, Some(first.clone())),
        ];
        let b3 = budget_from_rig(&r, &["Vulkan0".to_string()], 1_024, &running);
        let third = fit(&FitInput {
            budget: b3,
            ..log_input()
        });
        assert!(
            !matches!(third.verdict, FitVerdict::Fits { .. }),
            "{:?}",
            third.verdict
        );
    }

    #[test]
    fn degenerate_inputs_do_not_panic() {
        let empty = FitInput {
            weights_bytes: 0,
            gguf: GgufMeta::default(),
            budget: VramBudget::default(),
            want_ctx: None,
            want_parallel: Some(0),
            want_kv: None,
            split: SplitPlan::default(),
            batch: Some(0),
        };
        let plan = fit(&empty);
        assert_eq!(plan.parallel, 1);
        assert!(plan.ctx >= MIN_CTX);
        assert!(!plan.why.is_empty());

        let huge = FitInput {
            weights_bytes: u64::MAX,
            gguf: GgufMeta {
                arch: "x".into(),
                n_layer: u32::MAX,
                n_head_kv: u32::MAX,
                n_embd_head_k: u32::MAX,
                n_embd_head_v: u32::MAX,
                n_ctx_train: u32::MAX,
                full_attn_layers: None,
                n_expert: None,
                quant_desc: None,
            },
            budget: one_vulkan_gpu(),
            want_ctx: Some(u32::MAX),
            want_parallel: Some(u32::MAX),
            want_kv: Some(KvType::F32),
            split: SplitPlan::default(),
            batch: Some(u32::MAX),
        };
        let plan = fit(&huge);
        assert!(matches!(plan.verdict, FitVerdict::WontFit { .. }));
    }
}
