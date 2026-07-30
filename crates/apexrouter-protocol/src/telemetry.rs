//! Telemetry: what one request did, what a day of them cost, and what a probe measured.
//!
//! [`UsageRecord`] is the one type in this crate whose field names are **not** ours to
//! choose: they are the legacy schema `cost.py` parses, preserved exactly. New fields are
//! additive and `#[serde(flatten)] extra` keeps anything we do not recognise, so **no row
//! can ever fail to load**.

use crate::backend::Protocol;
use crate::ids::{Alias, BackendId, RequestId};
use crate::money::{CostEstimate, TokenCount};
use crate::route::RouteReason;
use serde::{Deserialize, Serialize};

/// One proxied request, start to finish.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RequestRecord {
    /// Also emitted as `X-Request-Id`.
    pub id: RequestId,
    /// When it arrived, unix seconds.
    pub started_unix: i64,
    /// Which alias resolved, when one did.
    #[serde(default)]
    pub alias: Option<Alias>,
    /// Which backend served it.
    #[serde(default)]
    pub backend: Option<BackendId>,
    /// The model id actually sent upstream.
    #[serde(default)]
    pub upstream_model: Option<String>,
    /// Which resolution rule fired.
    pub route_reason: RouteReason,
    /// The dialect the CLIENT spoke. The upstream's lives on the [`crate::backend::Backend`],
    /// so the pair names the matrix cell that ran.
    #[serde(default)]
    pub ingress: Protocol,
    /// HTTP method.
    pub method: String,
    /// Normalised path.
    pub path: String,
    /// Final status returned to the client.
    pub status: u16,
    /// How many upstream attempts were made.
    pub attempts: u8,
    /// Whether the response was an SSE stream.
    pub streamed: bool,
    /// True when the client vanished before `finish()` — set by `InFlightGuard`'s `Drop`.
    pub aborted: bool,
    /// Time to first byte.
    #[serde(default)]
    pub ttft_ms: Option<u32>,
    /// Total wall clock.
    pub total_ms: u32,
    /// Prompt tokens, marked reported-or-estimated.
    #[serde(default)]
    pub prompt_tokens: Option<TokenCount>,
    /// Completion tokens, marked reported-or-estimated.
    #[serde(default)]
    pub completion_tokens: Option<TokenCount>,
    /// llama.cpp `timings.cache_n`.
    #[serde(default)]
    pub cached_tokens: Option<u32>,
    /// llama.cpp `timings.predicted_per_second`.
    #[serde(default)]
    pub tok_per_s: Option<f32>,
    /// What it cost, with its provenance.
    pub cost: CostEstimate,
    /// The error, when there was one.
    #[serde(default)]
    pub error: Option<String>,
}

/// The on-disk usage row. **LEGACY FIELD NAMES ARE PRESERVED EXACTLY** so `cost.py` still
/// parses it, and `extra` guarantees an unknown key never fails a load.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageRecord {
    /// RFC 3339 UTC going forward. Legacy `%Y-%m-%dT%H:%M:%SZ` local-time-with-a-lying-`Z`
    /// values are parsed leniently on read.
    pub timestamp: String,
    /// Legacy optional epoch seconds. Skipped on write when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<f64>,
    /// Legacy provider spelling. `"vast-gguf"` stays on the wire.
    pub provider: String,
    /// The model id as billed.
    pub model_id: String,
    /// Prompt tokens.
    pub prompt_tokens: u32,
    /// Completion tokens.
    pub completion_tokens: u32,
    /// Dollars, as an f64, because that is what the legacy schema says.
    pub cost_usd: f64,
    /// Additive: the request this row came from.
    #[serde(default)]
    pub request_id: Option<String>,
    /// Additive: which backend served it.
    #[serde(default)]
    pub backend: Option<String>,
    /// Additive: which alias resolved.
    #[serde(default)]
    pub alias: Option<String>,
    /// Additive: time to first byte.
    #[serde(default)]
    pub ttft_ms: Option<u32>,
    /// Additive: observed throughput.
    #[serde(default)]
    pub tok_per_s: Option<f32>,
    /// Additive: whether it streamed.
    #[serde(default)]
    pub stream: Option<bool>,
    /// Additive: whether the token counts were estimated rather than reported.
    #[serde(default)]
    pub estimated: Option<bool>,
    /// Everything else the file carried. Old readers ignore it; we never drop it.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// An aggregate over a window and a grouping.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageSummary {
    /// The window as the user asked for it: `"24h"`, `"7d"`, `"all"`.
    pub window: String,
    /// One bucket per group key.
    #[serde(default)]
    pub by: Vec<UsageBucket>,
    /// Sum of every bucket, degraded honestly if any bucket was a guess.
    pub total_cost: CostEstimate,
    /// Prompt tokens across the window.
    pub total_prompt: u64,
    /// Completion tokens across the window.
    pub total_completion: u64,
    /// How many rows were aggregated.
    pub rows: u64,
}

/// One group in a [`UsageSummary`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageBucket {
    /// Provider, model, backend, alias or day — whatever the grouping was.
    pub key: String,
    /// Cost for this bucket.
    pub cost: CostEstimate,
    /// Prompt tokens.
    pub prompt_tokens: u64,
    /// Completion tokens.
    pub completion_tokens: u64,
    /// Request count.
    pub requests: u64,
    /// Median throughput across the bucket.
    #[serde(default)]
    pub tok_per_s_p50: Option<f32>,
}

/// One named smoke probe. TTFT and tok/s come from the `timings` object, never a stopwatch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SmokeProbe {
    /// `smoke.models`, `smoke.warmup`, `smoke.tools`, `smoke.throughput`.
    pub name: String,
    /// Pass or fail.
    pub ok: bool,
    /// Wall clock.
    pub ms: u32,
    /// What happened, in words.
    pub detail: String,
    /// Time to first byte, from the response.
    #[serde(default)]
    pub ttft_ms: Option<u32>,
    /// Throughput, from `timings.predicted_per_second`.
    #[serde(default)]
    pub tok_per_s: Option<f32>,
    /// Tokens generated.
    #[serde(default)]
    pub tokens: Option<u32>,
}

/// One row of `POST /v1/compare`: the same prompt against a different alias.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompareRow {
    /// Which alias.
    pub alias: Alias,
    /// Which backend it resolved to.
    #[serde(default)]
    pub backend: Option<BackendId>,
    /// The upstream model id.
    pub model: String,
    /// Whether the call succeeded.
    pub ok: bool,
    /// Wall clock.
    pub ms: u32,
    /// Time to first byte.
    #[serde(default)]
    pub ttft_ms: Option<u32>,
    /// Throughput.
    #[serde(default)]
    pub tok_per_s: Option<f32>,
    /// Prompt tokens — the **real** number from the response, never `word_count * 1.3`.
    #[serde(default)]
    pub prompt_tokens: Option<TokenCount>,
    /// Completion tokens.
    #[serde(default)]
    pub completion_tokens: Option<TokenCount>,
    /// What it cost.
    pub cost: CostEstimate,
    /// First ~200 characters of the answer.
    pub preview: String,
    /// The error, when there was one.
    #[serde(default)]
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::money::{Money, PriceSource};

    #[test]
    fn request_record_round_trips_and_defaults_ingress_to_openai() {
        let r = RequestRecord {
            id: RequestId::new(),
            started_unix: 1_785_412_331,
            alias: Some(Alias::parse("auto").expect("alias")),
            backend: Some(BackendId::parse("local-carnice").expect("id")),
            upstream_model: Some("Carnice-9b-Q6_K".into()),
            route_reason: RouteReason::Alias,
            ingress: Protocol::Anthropic,
            method: "POST".into(),
            path: "/v1/messages".into(),
            status: 200,
            attempts: 1,
            streamed: true,
            aborted: false,
            ttft_ms: Some(310),
            total_ms: 4_800,
            prompt_tokens: Some(TokenCount::Reported(41)),
            completion_tokens: Some(TokenCount::Estimated(88)),
            cached_tokens: Some(12),
            tok_per_s: Some(4.1),
            cost: CostEstimate::Unknown,
            error: None,
        };
        let s = serde_json::to_string(&r).expect("ser");
        assert_eq!(serde_json::from_str::<RequestRecord>(&s).expect("de"), r);

        // An older writer that predates the Anthropic ingress omits the field entirely.
        let mut v: serde_json::Value = serde_json::from_str(&s).expect("value");
        v.as_object_mut().expect("obj").remove("ingress");
        let older: RequestRecord = serde_json::from_value(v).expect("de");
        assert_eq!(older.ingress, Protocol::OpenAi);
    }

    #[test]
    fn usage_record_preserves_legacy_field_names_and_unknown_keys() {
        // A real-shaped legacy row, including `epoch` and a key we have never heard of.
        let legacy = r#"{
            "timestamp": "2026-02-11T18:04:02Z",
            "epoch": 1770832042.5,
            "provider": "vast-gguf",
            "model_id": "deepseek-ai/DeepSeek-V4-Pro",
            "prompt_tokens": 512,
            "completion_tokens": 128,
            "cost_usd": 0.0031,
            "some_future_key": {"nested": true}
        }"#;
        let rec: UsageRecord = serde_json::from_str(legacy).expect("legacy row must load");
        assert_eq!(rec.provider, "vast-gguf");
        assert_eq!(rec.epoch, Some(1_770_832_042.5));
        assert!(rec.extra.contains_key("some_future_key"));
        assert!(rec.request_id.is_none());

        let out = serde_json::to_string(&rec).expect("ser");
        assert!(out.contains("\"provider\":\"vast-gguf\""));
        assert!(out.contains("\"some_future_key\""));
        assert_eq!(serde_json::from_str::<UsageRecord>(&out).expect("de"), rec);
    }

    #[test]
    fn usage_record_omits_epoch_when_absent() {
        let rec = UsageRecord {
            timestamp: "2026-07-30T12:00:00Z".into(),
            epoch: None,
            provider: "local-gguf".into(),
            model_id: "Carnice-9b-Q6_K".into(),
            prompt_tokens: 1,
            completion_tokens: 2,
            cost_usd: 0.0,
            request_id: None,
            backend: None,
            alias: None,
            ttft_ms: None,
            tok_per_s: None,
            stream: None,
            estimated: None,
            extra: serde_json::Map::new(),
        };
        let s = serde_json::to_string(&rec).expect("ser");
        assert!(!s.contains("epoch"), "{s}");
        assert_eq!(serde_json::from_str::<UsageRecord>(&s).expect("de"), rec);
    }

    #[test]
    fn usage_summary_and_probes_round_trip() {
        let sum = UsageSummary {
            window: "24h".into(),
            by: vec![UsageBucket {
                key: "together".into(),
                cost: CostEstimate::Metered {
                    usd: Money(3_100),
                    source: PriceSource::ProviderApi,
                },
                prompt_tokens: 512,
                completion_tokens: 128,
                requests: 4,
                tok_per_s_p50: Some(63.2),
            }],
            total_cost: CostEstimate::Metered {
                usd: Money(3_100),
                source: PriceSource::ProviderApi,
            },
            total_prompt: 512,
            total_completion: 128,
            rows: 4,
        };
        let s = serde_json::to_string(&sum).expect("ser");
        assert_eq!(serde_json::from_str::<UsageSummary>(&s).expect("de"), sum);

        let probe = SmokeProbe {
            name: "smoke.throughput".into(),
            ok: true,
            ms: 4_800,
            detail: "200 tokens".into(),
            ttft_ms: Some(310),
            tok_per_s: Some(4.1),
            tokens: Some(200),
        };
        let s = serde_json::to_string(&probe).expect("ser");
        assert_eq!(serde_json::from_str::<SmokeProbe>(&s).expect("de"), probe);

        let row = CompareRow {
            alias: Alias::parse("auto").expect("alias"),
            backend: Some(BackendId::parse("local-carnice").expect("id")),
            model: "Carnice-9b-Q6_K".into(),
            ok: true,
            ms: 4_800,
            ttft_ms: Some(310),
            tok_per_s: Some(4.1),
            prompt_tokens: Some(TokenCount::Reported(41)),
            completion_tokens: Some(TokenCount::Reported(88)),
            cost: CostEstimate::Unknown,
            preview: "Hello".into(),
            error: None,
        };
        let s = serde_json::to_string(&row).expect("ser");
        assert_eq!(serde_json::from_str::<CompareRow>(&s).expect("de"), row);
    }
}
