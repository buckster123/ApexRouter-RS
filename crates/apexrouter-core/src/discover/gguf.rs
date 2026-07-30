//! OWNER: unit C-10 (core/discover/models.rs, core/discover/gguf.rs). Do not edit outside
//! that unit.
//!
//! GGUF header reading. **Header only, bounded read** — never `mmap`, never more than
//! 8 MiB, because a discovery scan touches every weight file on the box and this laptop
//! lives under swap pressure.

use crate::error::Result;
use apexrouter_protocol::GgufMeta;
use std::path::Path;

/// Read the fields the fit solver needs out of a GGUF header.
///
/// Handles typed KV, array and string values. Extracts `n_layer`, `n_head_kv`,
/// `n_embd_head_k/v`, `n_ctx_train` and, when present, `full_attn_layers` — hybrid-linear
/// models like Qwen3.6 MoE carry KV on only 10 of 41 layers, and assuming otherwise
/// over-estimates the cache by 4×.
pub fn read_gguf_meta(path: &Path) -> Result<GgufMeta> {
    todo!("C-10: read_gguf_meta")
}
