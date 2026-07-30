//! OWNER: unit R-03 (router/src/relay/{mod,headers,body}.rs). Do not edit outside that unit.
//!
//! Request bodies. Two rules:
//!
//! * When the alias equals the upstream id, the body is [`BodyPlan::Passthrough`] — the
//!   original `Bytes`, zero copies. Otherwise it is [`BodyPlan::Rewritten`] and **only the
//!   `model` value changes**, which a tool-calling fixture round-trip asserts down to float
//!   formatting inside `tools[]`.
//! * [`peek`] is a top-level key **scanner**, not a `serde_json::Value` parse: a 4 MiB body
//!   must not allocate 4 MiB of DOM to learn whether `stream` is true.

use apexrouter_core::error::Result;
use bytes::Bytes;

/// What to send upstream.
pub enum BodyPlan {
    /// The original bytes, untouched.
    Passthrough(Bytes),
    /// The original bytes with exactly one value replaced.
    Rewritten(Bytes),
}

/// Decide whether the body needs rewriting, and do it if so.
pub fn plan_body(original: &Bytes, rewrite_model_to: Option<&str>) -> Result<BodyPlan> {
    todo!("R-03: plan_body")
}

/// The three things the request path needs to know before it can route.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RequestPeek {
    /// The `"model"` value, when there is one.
    pub model: Option<String>,
    /// `"stream"`, read **strictly** as a bool — `"stream": "true"` is not streaming.
    pub stream: bool,
    /// `"stream_options": {"include_usage": true}`.
    pub include_usage: bool,
    /// Body length, for the global byte budget.
    pub bytes: usize,
}

/// Top-level key scanner. Does NOT build a full `serde_json::Value`.
pub fn peek(body: &[u8]) -> RequestPeek {
    todo!("R-03: peek")
}

/// Collapse a repeated leading `/v1` to one, and report whether anything was collapsed.
///
/// Both `http://127.0.0.1:8888` and `http://127.0.0.1:8888/v1` must work as client base
/// URLs — mandatory, because `smoke.sh` appends `/v1` to whatever you give it and the
/// project's own SKILL.md told agents to use the form that 404s today.
///
/// Returns `(normalized, collapsed_a_duplicate_v1)`.
pub fn normalize_path(path: &str) -> (String, bool) {
    todo!("R-03: normalize_path")
}
