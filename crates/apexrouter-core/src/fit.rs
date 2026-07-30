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

use apexrouter_protocol::{EndpointRecord, FitInput, FitPlan, RigSnapshot, VramBudget};

/// Solve. Pure: no I/O, no clock, no allocation surprises.
///
/// `FitPlan::why` must come back non-empty and human-readable — a number nobody can explain
/// is a number nobody should trust.
pub fn fit(input: &FitInput) -> FitPlan {
    todo!("C-11: fit")
}

/// Build a **live** budget from the rig, subtracting what running endpoints already hold.
///
/// Subtracts `weights + kv + compute` of every `EndpointRecord` whose
/// `desired == DesiredState::Running`, which is what makes `InsufficientVram` fire before a
/// second launch OOMs the first.
pub fn budget_from_rig(
    rig: &RigSnapshot,
    devices: &[String],
    margin_mb: u64,
    running: &[EndpointRecord],
) -> VramBudget {
    todo!("C-11: budget_from_rig")
}
