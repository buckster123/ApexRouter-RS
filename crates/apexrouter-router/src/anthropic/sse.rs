//! OWNER: unit R-10 (router/src/anthropic/{mod,translate,sse}.rs). Do not edit outside that
//! unit.
//!
//! **The state machine. This is the unit's main risk.**
//!
//! Anthropic emits *named* SSE events — `message_start`, `content_block_start`,
//! `content_block_delta`, `content_block_stop`, `message_delta`, `message_stop` — carrying
//! explicit content-block **indices** and a final `usage` on `message_delta`. OpenAI emits
//! one delta shape repeatedly and terminates with `data: [DONE]`.
//!
//! Rebuilding that correctly means: opening a block on the first delta of a kind, keeping
//! the index monotonic, closing every opened block exactly once, and emitting
//! `message_delta` with `stop_reason` **and** the final usage before `message_stop`.
//!
//! Chunk boundaries must not be observable: a property test feeds arbitrary splits of the
//! same capture — including splits **inside** a `data:` line — and asserts an identical
//! frame sequence.

use bytes::Bytes;

/// Translates one OpenAI SSE stream into one Anthropic SSE stream.
pub struct SseTranslator {/* R-10: block index, open/close state, accumulated usage, message id */}

impl SseTranslator {
    /// Start a translation for one response. `model` is echoed in `message_start`.
    pub fn new(model: String) -> Self {
        todo!("R-10: SseTranslator::new")
    }

    /// Feed one OpenAI SSE frame; get back zero or more Anthropic frames, already framed
    /// `event: <name>\ndata: <json>\n\n`. `data: [DONE]` yields the closing frames.
    pub fn feed(&mut self, frame: &[u8]) -> Vec<Bytes> {
        todo!("R-10: SseTranslator::feed")
    }

    /// Upstream ended without `[DONE]`: close every open block **honestly**, then
    /// `message_stop`. Never a truncated block, never a dangling index.
    pub fn finish(self) -> Vec<Bytes> {
        todo!("R-10: SseTranslator::finish")
    }
}
