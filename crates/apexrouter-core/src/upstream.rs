//! OWNER: unit C-13 (core/upstream.rs). Do not edit outside that unit.
//!
//! Probing an OpenAI-compatible upstream. **This lives in `core`, not in a provider**, so
//! that `core -> providers` never becomes an edge.
//!
//! The readiness gate must distinguish three states that look alike from a distance:
//! `/health` 200 (healthy), `/health` 503 `{"status":"loading model"}` (alive and working —
//! a real deadline resets on this), and connection-refused (not there).

use crate::secret::Secret;
use apexrouter_protocol::UpstreamModel;
use std::time::Duration;

/// One probe's findings. Nothing here is fatal: a 404 on `/props` and a **501** on `/slots`
/// are normal for builds that did not enable those endpoints.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UpstreamProbe {
    /// `/health` answered 200.
    pub healthy: bool,
    /// `/health` answered 503 `{"status":"loading model"}` — alive, and making progress.
    pub loading: bool,
    /// The status we got, when we got one.
    pub status: Option<u16>,
    /// From `/v1/models`.
    pub models: Vec<UpstreamModel>,
    /// From `/slots` or `/props`.
    pub slots_busy: Option<u32>,
    /// From `/props.total_slots`, falling back to `/slots` length.
    pub slots_total: Option<u32>,
    /// From `/props`.
    pub ctx: Option<u32>,
    /// From `/props`, e.g. `"b9199 (39cf5d619)"`.
    pub build_info: Option<String>,
    /// From `/props` — used by `POST /v1/endpoints/{id}/adopt` to verify a foreign process
    /// is actually serving the model the spec names.
    pub model_path: Option<String>,
    /// Round trip.
    pub ms: u32,
    /// What went wrong, when something did.
    pub error: Option<String>,
}

/// Probe one upstream. Never panics, never returns `Err`: an unreachable upstream is a
/// finding, not an exception.
pub async fn probe(
    http: &reqwest::Client,
    base_url: &str,
    cred: Option<&Secret<String>>,
    timeout: Duration,
) -> UpstreamProbe {
    todo!("C-13: probe")
}

/// llama.cpp's `timings` object.
///
/// **Not** the same shape as `/slots?action=save`'s different `timings` object — they must
/// not share a struct.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Timings {
    /// Prompt tokens served from cache.
    pub cache_n: u32,
    /// Prompt tokens.
    pub prompt_n: u32,
    /// Prompt processing time.
    pub prompt_ms: f32,
    /// Generated tokens.
    pub predicted_n: u32,
    /// Generation time.
    pub predicted_ms: f32,
    /// The number we report as tok/s. Read, never stopwatched.
    pub predicted_per_second: f32,
}

/// Pull `timings` out of a response body, if it has one.
pub fn parse_timings(v: &serde_json::Value) -> Option<Timings> {
    todo!("C-13: parse_timings")
}

/// The OpenAI `usage` object.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct UsageFields {
    /// `usage.prompt_tokens`.
    pub prompt_tokens: u32,
    /// `usage.completion_tokens`.
    pub completion_tokens: u32,
    /// `usage.prompt_tokens_details.cached_tokens`, when present.
    pub cached_tokens: Option<u32>,
}

/// Pull `usage` out of a response body, if it has one.
pub fn parse_usage(v: &serde_json::Value) -> Option<UsageFields> {
    todo!("C-13: parse_usage")
}

/// Join a segment onto a base URL that is stored **without** a trailing `/v1`.
///
/// This is the highest-risk drop-in bug in the whole project: both
/// `http://127.0.0.1:8888` and `http://127.0.0.1:8888/v1` must work as client base URLs,
/// because `smoke.sh` appends `/v1` to whatever you give it.
pub fn join_v1(base_url: &str, segment: &str) -> String {
    todo!("C-13: join_v1")
}
