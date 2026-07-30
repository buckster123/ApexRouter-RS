//! OWNER: unit R-10 (router/src/anthropic/{mod,translate,sse}.rs). Do not edit outside that
//! unit.
//!
//! Pure, synchronous, no I/O, not `async`. Unit-tested against fixtures.
//!
//! The translation contract (each line is a fixture test):
//!
//! | Anthropic | OpenAI | Rule |
//! |---|---|---|
//! | top-level `system` | a `{"role":"system"}` message | hoist/lower; a block array joins on `\n\n`; absent ⇒ no system message is invented |
//! | `max_tokens` — **REQUIRED** | `max_tokens` — optional | missing ⇒ [`TranslateError::MissingMaxTokens`] ⇒ `400`. **Never defaulted silently** |
//! | typed block array | a plain string, or the parts array | one `text` block lowers to a plain string, which is what keeps llama.cpp happy |
//! | `tools[].input_schema` | `tools[].function.parameters` | rename only; the JSON Schema is copied byte-identically |
//! | `tool_use` block | `tool_calls[]` | `input` (object) → `function.arguments` (**a JSON string**) |
//! | `tool_result` in a `user` message | `{"role":"tool","tool_call_id":…}` | hoisted out of the user turn, in order |
//! | `stop_reason` ↔ `finish_reason` | `end_turn`↔`stop`, `max_tokens`↔`length`, `tool_use`↔`tool_calls` | both directions |
//! | `usage.input_tokens`/`output_tokens` | `usage.prompt_tokens`/`completion_tokens` | rename only; **never recomputed, never estimated** |
//! | `thinking` block | — | no equivalent. [`TranslateError::UnsupportedBlock`] |

/// From `[router] anthropic_tools`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct AnthropicCfg {
    /// **Off by default.** With it off, a `/v1/messages` body carrying `tools` is REFUSED
    /// with a clear error naming the config key — never silently stripped and answered
    /// wrongly, which is the failure mode that actually costs an agent an hour.
    pub tools: bool,
}

/// Why a body could not be translated. Each becomes a `400` with an Anthropic-shaped body.
#[derive(Debug, thiserror::Error)]
pub enum TranslateError {
    /// `max_tokens` is required on the Anthropic side and is never defaulted for the client.
    #[error("max_tokens is required by the Anthropic Messages API")]
    MissingMaxTokens,
    /// `tools` was present while `[router] anthropic_tools = false`.
    #[error("tool translation is off: set [router] anthropic_tools = true to enable it")]
    ToolsDisabled,
    /// A content block with no OpenAI equivalent, e.g. `thinking`.
    #[error("content block type {kind:?} has no OpenAI equivalent")]
    UnsupportedBlock {
        /// The block's `type`.
        kind: String,
    },
    /// The body did not match the Messages schema.
    #[error("malformed request at {at}: {why}")]
    Malformed {
        /// A JSON pointer-ish location.
        at: String,
        /// What was wrong.
        why: String,
    },
}

/// Anthropic `MessagesRequest` → OpenAI `ChatCompletionRequest`.
///
/// **`model` is left EXACTLY as the client sent it**: `resolve()` owns model naming, and
/// this unit never invents an alias.
pub fn request_to_openai(body: &[u8], cfg: &AnthropicCfg) -> Result<Vec<u8>, TranslateError> {
    todo!("R-10: request_to_openai")
}

/// Buffered OpenAI `ChatCompletion` → Anthropic `Message`. `id` is passed through prefixed
/// `msg_`.
pub fn response_to_anthropic(body: &[u8]) -> Result<Vec<u8>, TranslateError> {
    todo!("R-10: response_to_anthropic")
}

/// `stop` → `end_turn`, `length` → `max_tokens`, `tool_calls` → `tool_use`.
pub fn map_stop_reason_to_anthropic(finish_reason: &str) -> &'static str {
    todo!("R-10: map_stop_reason_to_anthropic")
}

/// The inverse of [`map_stop_reason_to_anthropic`].
pub fn map_stop_reason_to_openai(stop_reason: &str) -> &'static str {
    todo!("R-10: map_stop_reason_to_openai")
}
