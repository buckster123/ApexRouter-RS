//! OWNER: unit R-10 (router/src/anthropic/{mod,translate,sse}.rs). Do not edit outside that
//! unit.
//!
//! The Anthropic ingress. **This module implements exactly one matrix cell,
//! `Anthropic → OpenAi`.** The `Anthropic → Anthropic` cell is the existing byte relay and
//! needs no code here; `OpenAi → Anthropic` is a `501` and stays one.
//!
//! The point of the surface: `ANTHROPIC_BASE_URL=http://127.0.0.1:8888` lets the Claude Code
//! harness drive a local or rented model.
//!
//! R-08's `handler.rs` calls into here; **R-10 does not own `handler.rs`**.

use axum::http::{HeaderMap, StatusCode};

pub mod sse;
pub mod translate;

pub use sse::SseTranslator;
pub use translate::{
    map_stop_reason_to_anthropic, map_stop_reason_to_openai, request_to_openai,
    response_to_anthropic, AnthropicCfg, TranslateError,
};

/// Is this request speaking Anthropic?
///
/// True for `POST /v1/messages`, and for `GET /v1/models` when an `anthropic-version` header
/// is present. **It must never fire on an OpenAI request**: `GET /v1/models` without that
/// header returns the byte-identical OpenAI list shape it returns today, because ApexOS's
/// LAN compute sweep identifies a node by exactly that shape.
pub fn is_anthropic_ingress(path: &str, headers: &HeaderMap) -> bool {
    todo!("R-10: is_anthropic_ingress")
}

/// `/v1/messages` → `/v1/chat/completions`. The ONLY path rewrite this unit performs.
pub fn upstream_path(ingress_path: &str) -> &'static str {
    todo!("R-10: upstream_path")
}

/// An **Anthropic-shaped** error body — `{"type":"error","error":{"type":…,"message":…}}`,
/// **not** the OpenAI shape — for failures that occur before or instead of an upstream hop.
///
/// The client is an Anthropic SDK and will parse it as one. Symmetrically, the
/// `OpenAi → Anthropic` 501 carries an OpenAI-shaped body.
pub fn anthropic_error(status: StatusCode, kind: &str, msg: &str) -> axum::response::Response {
    todo!("R-10: anthropic_error")
}
