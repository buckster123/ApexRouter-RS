//! OWNER: unit P-06 (providers/src/together.rs). Do not edit outside that unit.
//!
//! together.ai. Four measured facts this must honour:
//!
//! * `GET /v1/models` returns a **bare array**, not a `{"data":[]}` envelope.
//! * `pricing` hangs off each model object, and the pricing **unit assumption is recorded in
//!   the `CostEstimate::Approximate.assumption` string**, never silently applied.
//! * `finish_reason` is always a `String` — Together emits `eos`, which no enum covers.
//! * A 429 reads `x-ratelimit-reset`; `x-ratelimit-remaining` is **not** relied upon.
//!
//! The base URL comes from config or the legacy file and `api.together.xyz` is **never**
//! rewritten to `.ai`.
//!
//! ## Shape of this module
//!
//! Nothing here models a *request* body. ApexRouter relays client bytes verbatim
//! (`docs/port/09` §2.3), so the only inbound parsing is [`parse_completion`], which reads
//! just enough of a response to bill it. Everything else is the catalogue: the model list,
//! its prices, and the one header a 429 is allowed to teach us.

use apexrouter_core::config::Config;
use apexrouter_core::error::{Error, Result};
use apexrouter_core::paths::Paths;
use apexrouter_core::secret::{resolve_provider, ResolvedCredential, Secret};
use apexrouter_core::upstream::{join_v1, parse_usage, UsageFields};
use apexrouter_protocol::{
    CostEstimate, CredentialSource, Money, PriceModel, PriceSource, ProviderId, ProviderStatus,
    RateLimitInfo, TokenCount, UpstreamModel,
};
use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The provider id this module speaks for.
pub const PROVIDER_ID: &str = "together";

/// The host ApexRouter ships pre-configured. **Only** a fallback: a user's configured or
/// legacy URL is used verbatim, `api.together.xyz` included.
pub const DEFAULT_BASE_URL: &str = "https://api.together.ai/v1";

/// The one thing Together's `/v1/models` does not tell us, written down instead of baked in.
///
/// The `pricing` object carries bare numbers (`"input": 0.3`) and the API reference states no
/// unit. USD per 1M tokens is the near-certain reading for a 13B model, but "near-certain" is
/// not "measured", so every money figure this module produces is a
/// [`CostEstimate::Approximate`] carrying this sentence. LocalRouter's `cost.py` buried a
/// constant; this constant is the *opposite* of buried.
pub const PRICING_ASSUMPTION: &str =
    "together `pricing.input`/`pricing.output` read as USD per 1M tokens — the models API \
     states no unit";

/// The ceiling on any backoff this module computes for itself.
pub const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// First backoff step, before jitter.
const BASE_BACKOFF_MS: u64 = 500;

/// A `x-ratelimit-reset` larger than this is a unix timestamp, not a seconds-to-wait.
/// Together documents seconds-to-wait; other OpenAI-shaped hosts behind the same config key
/// send an epoch, and confusing the two would sleep for 55 years.
const EPOCH_THRESHOLD: f64 = 1_000_000_000.0;

/// No advertised wait is honoured past this, whatever a header claims.
const SANITY_MAX_WAIT: Duration = Duration::from_secs(300);

// ---------------------------------------------------------------------------------------
// the catalogue types
// ---------------------------------------------------------------------------------------

/// One row of Together's `GET /v1/models` **bare array**.
///
/// Every field but `id` is optional: `context_length` is absent for some model types, and
/// Together adds keys without warning. `context_length` is `i64` rather than `u32` on
/// purpose — a negative or oversized value must not make the whole row unparseable.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TogetherModel {
    /// The exact id to put in an outbound `"model"`.
    pub id: String,
    /// `"model"`.
    #[serde(default)]
    pub object: Option<String>,
    /// Unix seconds.
    #[serde(default)]
    pub created: Option<i64>,
    /// `chat | language | code | image | embedding | moderation | rerank`.
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    /// Human label, e.g. `"Chronos Hermes (13B)"`.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Publishing org.
    #[serde(default)]
    pub organization: Option<String>,
    /// Model card link, when there is one.
    #[serde(default)]
    pub link: Option<String>,
    /// License, when declared.
    #[serde(default)]
    pub license: Option<String>,
    /// Context window. Absent for several model types.
    #[serde(default)]
    pub context_length: Option<i64>,
    /// The raw `pricing` object, **kept verbatim** so a future unit correction is a
    /// re-interpretation rather than a re-fetch.
    #[serde(default)]
    pub pricing: Option<TogetherPricing>,
}

impl TogetherModel {
    /// True when this row is worth offering to a router user: a chat-shaped model.
    ///
    /// A missing `type` counts as chat — an unknown row is more useful listed than hidden.
    pub fn is_chat(&self) -> bool {
        match self.kind.as_deref() {
            None => true,
            Some(k) => matches!(k, "chat" | "language" | "code"),
        }
    }

    /// The context window as the routing table wants it, when it is representable.
    pub fn ctx(&self) -> Option<u32> {
        self.context_length
            .filter(|n| *n > 0)
            .and_then(|n| u32::try_from(n).ok())
    }

    /// Render as an [`UpstreamModel`].
    ///
    /// `vision` and `tools` are both `false`: Together's model list advertises **no**
    /// capability flags, and inventing them here would put a guess where the router expects
    /// a fact. A capability probe belongs to the smoke suite (P-08), not to a catalogue read.
    pub fn upstream(&self) -> UpstreamModel {
        UpstreamModel {
            id: self.id.clone(),
            ctx: self.ctx(),
            vision: false,
            tools: false,
        }
    }
}

/// Together's per-model `pricing` object, verbatim.
///
/// All six fields are optional and all are bare numbers with **no unit stated by the API** —
/// see [`PRICING_ASSUMPTION`].
#[derive(Copy, Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TogetherPricing {
    /// Per-1M input tokens, under [`PRICING_ASSUMPTION`].
    #[serde(default)]
    pub input: Option<f64>,
    /// Per-1M output tokens, under [`PRICING_ASSUMPTION`].
    #[serde(default)]
    pub output: Option<f64>,
    /// Dollars per hour, for a dedicated endpoint.
    #[serde(default)]
    pub hourly: Option<f64>,
    /// Base price, as Together reports it.
    #[serde(default)]
    pub base: Option<f64>,
    /// Fine-tuning price, as Together reports it.
    #[serde(default)]
    pub finetune: Option<f64>,
    /// Cached-input price, when the model has one.
    #[serde(default)]
    pub cached_input: Option<f64>,
}

impl TogetherPricing {
    /// True when every billable field is zero or absent — a free tier.
    fn is_zero(&self) -> bool {
        let z = |v: Option<f64>| v.unwrap_or(0.0) == 0.0;
        z(self.input) && z(self.output) && z(self.hourly)
    }
}

/// A provider's whole price table plus the assumption it was built under.
///
/// The assumption travels **with** the rows so that a caller feeding
/// `PriceTable::set_provider_models` cannot hold the numbers without also holding the
/// sentence that qualifies them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PriceCatalogue {
    /// `(model id, price)`, ready for `apexrouter_core::pricing::PriceTable`.
    pub rows: Vec<(String, PriceModel)>,
    /// [`PRICING_ASSUMPTION`], carried rather than remembered.
    pub assumption: String,
}

// ---------------------------------------------------------------------------------------
// parsing: the bare array
// ---------------------------------------------------------------------------------------

/// Deserialise `GET /v1/models`.
///
/// **The documented shape is a bare JSON array** — `[{…},{…}]` — not the
/// `{"object":"list","data":[…]}` envelope every other OpenAI-shaped host returns. A shared
/// envelope deserialiser silently yields zero models here, which is precisely how a naive
/// Together provider "works" while listing nothing.
///
/// An envelope is *also* accepted, because a reverse proxy in front of Together may add one
/// and refusing it would be pedantry, not safety. The array is tried first.
///
/// Rows are decoded one at a time: one malformed entry is skipped with a `debug` line rather
/// than discarding a 100-model catalogue.
pub fn parse_models(v: &Value) -> Vec<TogetherModel> {
    let rows: &[Value] = match v {
        Value::Array(a) => a,
        Value::Object(o) => match o.get("data").and_then(Value::as_array) {
            Some(a) => a,
            None => {
                tracing::debug!("together /v1/models: neither a bare array nor a data envelope");
                return Vec::new();
            }
        },
        _ => return Vec::new(),
    };
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        match serde_json::from_value::<TogetherModel>(row.clone()) {
            Ok(m) if !m.id.is_empty() => out.push(m),
            Ok(_) => tracing::debug!("together /v1/models: row with an empty id, skipped"),
            Err(e) => tracing::debug!(error = %e, "together /v1/models: unparseable row, skipped"),
        }
    }
    out
}

/// The chat-shaped subset, in catalogue order.
pub fn chat_models(models: &[TogetherModel]) -> Vec<&TogetherModel> {
    models.iter().filter(|m| m.is_chat()).collect()
}

/// Render a catalogue as routing-table rows.
pub fn upstream_models(models: &[TogetherModel]) -> Vec<UpstreamModel> {
    models.iter().map(TogetherModel::upstream).collect()
}

// ---------------------------------------------------------------------------------------
// pricing
// ---------------------------------------------------------------------------------------

/// One model's price, under [`PRICING_ASSUMPTION`].
///
/// * all-zero pricing → [`PriceModel::Free`] (zero is zero in any unit, so this one arm needs
///   no assumption);
/// * per-token pricing → [`PriceModel::PerToken`], each number read as USD per 1M tokens;
/// * only `hourly` set → [`PriceModel::PerHour`], a dedicated endpoint.
///
/// A model with **no** `pricing` object never reaches here: [`price_catalogue`] omits it and
/// [`estimate_cost`] answers [`CostEstimate::Unknown`], because a fabricated zero reads as
/// "free".
pub fn price_model(pricing: &TogetherPricing) -> PriceModel {
    if pricing.is_zero() {
        return PriceModel::Free;
    }
    let input = pricing.input.unwrap_or(0.0);
    let output = pricing.output.unwrap_or(0.0);
    if input == 0.0 && output == 0.0 {
        // Only `hourly` survives `is_zero()` being false.
        return PriceModel::PerHour {
            dph: Money::from_usd(pricing.hourly.unwrap_or(0.0)),
        };
    }
    PriceModel::PerToken {
        input: Money::from_usd(input),
        output: Money::from_usd(output),
    }
}

/// The whole catalogue's prices, with the assumption attached.
///
/// Models without a `pricing` object are **omitted**: an absent price must reach
/// `PriceTable` as absent, so `estimate` answers [`CostEstimate::Unknown`] instead of "free".
pub fn price_catalogue(models: &[TogetherModel]) -> PriceCatalogue {
    let rows = models
        .iter()
        .filter_map(|m| m.pricing.as_ref().map(|p| (m.id.clone(), price_model(p))))
        .collect();
    PriceCatalogue {
        rows,
        assumption: PRICING_ASSUMPTION.to_owned(),
    }
}

/// What one request cost, **always** as a [`CostEstimate::Approximate`] carrying
/// [`PRICING_ASSUMPTION`].
///
/// This is the rule the acceptance names: the unit assumption is *recorded*, never silently
/// applied. Even an exact multiplication of reported token counts by a listed price is a
/// guess while the unit is a guess, so this function has no `Metered` arm. A model with no
/// `pricing` object yields [`CostEstimate::Unknown`] — never a zero, which would read as
/// "free".
///
/// An estimated token count adds a second clause to the same sentence, so one string tells
/// the operator everything that is uncertain about the number beside it.
pub fn estimate_cost(
    pricing: Option<&TogetherPricing>,
    prompt: TokenCount,
    completion: TokenCount,
) -> CostEstimate {
    let Some(pricing) = pricing else {
        return CostEstimate::Unknown;
    };
    let usd = match price_model(pricing) {
        PriceModel::Free => Money::ZERO,
        PriceModel::PerToken { input, output } => input
            .mul_f64(f64::from(prompt.value()) / 1_000_000.0)
            .saturating_add(output.mul_f64(f64::from(completion.value()) / 1_000_000.0)),
        // A serverless request against an hourly-priced listing has no wall clock here; the
        // hourly figure alone cannot price it, and inventing a throughput is the exact sin
        // `PRICING_ASSUMPTION` exists to prevent.
        PriceModel::PerHour { .. } => return CostEstimate::Unknown,
    };
    let mut assumption = PRICING_ASSUMPTION.to_owned();
    if let Some(note) = estimated_note(prompt, completion) {
        assumption.push_str("; ");
        assumption.push_str(&note);
    }
    CostEstimate::Approximate {
        usd,
        source: PriceSource::ProviderApi,
        assumption,
    }
}

/// Name whichever token counts were derived rather than reported.
fn estimated_note(prompt: TokenCount, completion: TokenCount) -> Option<String> {
    match (prompt.is_reported(), completion.is_reported()) {
        (true, true) => None,
        (false, true) => Some("the prompt token count was estimated".to_owned()),
        (true, false) => Some("the completion token count was estimated".to_owned()),
        (false, false) => Some("both token counts were estimated".to_owned()),
    }
}

// ---------------------------------------------------------------------------------------
// responses: finish_reason is a String, always
// ---------------------------------------------------------------------------------------

/// The only part of a chat completion we read. The bytes themselves are relayed untouched.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompletionSummary {
    /// `id`, when present.
    pub id: Option<String>,
    /// The `model` the upstream says it served — the string to bill against.
    pub model: Option<String>,
    /// One entry per choice that carried one.
    ///
    /// **`String`, never an enum.** Together emits `eos`, which is in no OpenAI enum; a
    /// `#[derive(Deserialize)]` enum would fail the whole body and lose the `usage` with it.
    pub finish_reasons: Vec<String>,
    /// `usage`, when the response carried one. A streamed response without
    /// `stream_options.include_usage` carries none, and that is not an error.
    pub usage: Option<UsageFields>,
}

impl CompletionSummary {
    /// True when any choice finished for a reason Together invented.
    ///
    /// Exists so a caller can *notice* `eos` rather than be broken by it.
    pub fn has_non_openai_finish_reason(&self) -> bool {
        const OPENAI: [&str; 4] = ["stop", "length", "tool_calls", "content_filter"];
        self.finish_reasons
            .iter()
            .any(|r| !OPENAI.contains(&r.as_str()))
    }
}

/// Read a buffered response or a streamed chunk far enough to bill it.
///
/// Works on both shapes: a chunk's `choices[].finish_reason` sits in the same place as a
/// completion's, and `"usage": null` (every non-final chunk) yields `None` rather than a
/// zeroed record.
pub fn parse_completion(v: &Value) -> CompletionSummary {
    let str_at = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    let finish_reasons = v
        .get("choices")
        .and_then(Value::as_array)
        .map(|cs| {
            cs.iter()
                .filter_map(|c| c.get("finish_reason"))
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    CompletionSummary {
        id: str_at("id"),
        model: str_at("model"),
        finish_reasons,
        usage: parse_usage(v),
    }
}

// ---------------------------------------------------------------------------------------
// 429
// ---------------------------------------------------------------------------------------

/// What a 429 told us, and what to do about it.
#[derive(Clone, Debug, PartialEq)]
pub struct RateLimited {
    /// How long to wait before the next attempt.
    pub retry_after: Duration,
    /// True when [`retry_after`](Self::retry_after) came from `x-ratelimit-reset` rather
    /// than from our own backoff.
    pub from_header: bool,
    /// Together's `error_type`: `dynamic_request_limited` or `dynamic_token_limited`.
    /// Surfaced so the operator knows whether they hit a *request* or a *token* wall.
    pub error_type: Option<String>,
    /// The header state, for `ProviderStatus.rate_limit`.
    pub info: RateLimitInfo,
}

impl RateLimited {
    /// A one-line explanation for an error body or a log.
    pub fn message(&self) -> String {
        let wall = match self.error_type.as_deref() {
            Some(t) => format!("429 rate limited ({t})"),
            None => "429 rate limited".to_owned(),
        };
        let src = if self.from_header {
            "x-ratelimit-reset says"
        } else {
            "no x-ratelimit-reset; backing off"
        };
        format!(
            "{wall}; {src} retry in {:.1} s",
            self.retry_after.as_secs_f64()
        )
    }
}

/// Read a 429.
///
/// **`x-ratelimit-reset` is the only header a decision is taken from.** Together publishes no
/// rate-limit headers on a *successful* response, and `x-ratelimit-remaining` /
/// `x-ratelimit-limit` are not reliably returned even on a 429 — so they are recorded as
/// informational fields on [`RateLimitInfo`] and **never** consulted by
/// [`retry_after`](RateLimited::retry_after). A budget display built on `remaining` would be
/// a fiction.
///
/// `attempt` is the number of attempts already made; it only matters when the header is
/// missing, in which case the wait is full-jitter exponential backoff capped at
/// [`MAX_BACKOFF`].
pub fn rate_limited(
    headers: &HeaderMap,
    body: Option<&Value>,
    attempt: u32,
    now_unix: i64,
) -> RateLimited {
    let header_wait =
        header_str(headers, "x-ratelimit-reset").and_then(|r| reset_to_wait(&r, now_unix));
    let (retry_after, from_header) = match header_wait {
        Some(d) => (d, true),
        None => (backoff_with_jitter(attempt), false),
    };
    let info = RateLimitInfo {
        limit: header_str(headers, "x-ratelimit-limit").and_then(|s| s.trim().parse().ok()),
        // Informational only. Nothing above reads this field, and nothing should.
        remaining: header_str(headers, "x-ratelimit-remaining").and_then(|s| s.trim().parse().ok()),
        reset_unix: Some(now_unix.saturating_add(retry_after.as_secs() as i64)),
    };
    RateLimited {
        retry_after,
        from_header,
        error_type: body.and_then(error_type_of),
        info,
    }
}

/// `error_type` at the top level, or an OpenAI-shaped `error.type` / `error.code`.
fn error_type_of(body: &Value) -> Option<String> {
    let direct = body.get("error_type").and_then(Value::as_str);
    let nested = body
        .get("error")
        .and_then(|e| e.get("type").or_else(|| e.get("code")))
        .and_then(Value::as_str);
    direct
        .or(nested)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// One header as a `String`, when it is present and valid UTF-8.
fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// `x-ratelimit-reset` → a wait.
///
/// Together documents **seconds to wait**. A value past [`EPOCH_THRESHOLD`] is a unix
/// timestamp instead — several OpenAI-shaped hosts that a user may point this same config key
/// at send one — and is converted rather than slept on. Anything unparseable, negative or
/// non-finite is `None`, which sends the caller to the backoff.
fn reset_to_wait(raw: &str, now_unix: i64) -> Option<Duration> {
    let v: f64 = raw.trim().parse().ok()?;
    if !v.is_finite() || v < 0.0 {
        return None;
    }
    let secs = if v >= EPOCH_THRESHOLD {
        (v - now_unix as f64).max(0.0)
    } else {
        v
    };
    Some(Duration::from_secs_f64(
        secs.min(SANITY_MAX_WAIT.as_secs_f64()),
    ))
}

/// Full-jitter exponential backoff, capped at [`MAX_BACKOFF`].
///
/// `rand` is not in this workspace's dependency graph, so the jitter comes from the
/// nanosecond field of the wall clock: enough entropy to decorrelate two clients retrying
/// against the same dynamic limit, and it cannot fail. The floor is half the ceiling, so a
/// retry never stampedes immediately.
pub fn backoff_with_jitter(attempt: u32) -> Duration {
    let shift = attempt.min(6);
    let exp = BASE_BACKOFF_MS.saturating_mul(1u64 << shift);
    let ceiling = exp.min(MAX_BACKOFF.as_millis() as u64).max(1);
    let half = ceiling / 2;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()))
        .unwrap_or(0);
    Duration::from_millis(half + nanos % (ceiling - half + 1))
}

// ---------------------------------------------------------------------------------------
// base URL and credential
// ---------------------------------------------------------------------------------------

/// The base URL, from config or the legacy file, **verbatim**.
///
/// Order:
///
/// 1. `[providers.together] base_url` in ApexRouter's config, when it differs from
///    [`DEFAULT_BASE_URL`] — a value the operator actually chose;
/// 2. `~/.vastai-gguf/config.toml`'s `[providers.together] base_url`, gated by
///    `[compat] read_legacy_state`;
/// 3. [`DEFAULT_BASE_URL`].
///
/// Step 1's "differs from the default" clause is load-bearing, not cleverness.
/// `Config::default()` already ships a `[providers.together]`, so `migrate` **skips**
/// importing the legacy one ("already configured"). Preferring our own shipped default over
/// the legacy file would therefore rewrite an unmigrated user's `api.together.xyz` to
/// `api.together.ai` behind their back — the single rewrite this module exists to refuse.
pub fn base_url(cfg: &Config, paths: &Paths) -> String {
    let legacy = cfg
        .compat
        .read_legacy_state
        .then(|| paths.legacy().vastai_gguf.join("config.toml"));
    base_url_from(cfg, legacy.as_deref())
}

/// [`base_url`] with the legacy document's path injected, so the resolution order is
/// testable without a home directory.
pub fn base_url_from(cfg: &Config, legacy_config: Option<&Path>) -> String {
    let configured = cfg
        .providers
        .get(PROVIDER_ID)
        .map(|p| p.base_url.trim())
        .filter(|s| !s.is_empty());
    if let Some(url) = configured {
        if url != DEFAULT_BASE_URL {
            return url.to_owned();
        }
    }
    if let Some(url) = legacy_config.and_then(legacy_base_url) {
        return url;
    }
    configured.unwrap_or(DEFAULT_BASE_URL).to_owned()
}

/// One `base_url` out of `~/.vastai-gguf/config.toml`, parsed with a **real TOML parser**.
///
/// The parser is `core`'s own `Config` reader rather than a hand-rolled one: LocalRouter's
/// `[providers.<id>]` table is field-for-field the shape ApexRouter ships, so reusing it
/// costs nothing and keeps `toml` out of this crate's dependency list.
///
/// The file is **read-only to us**: a malformed third-party document is skipped, never
/// rewritten and never repaired. A file that is not there is `None` rather than the parser's
/// "then use the defaults" answer, so an absent legacy file cannot masquerade as one that
/// happens to agree with us.
fn legacy_base_url(path: &Path) -> Option<String> {
    if !path.is_file() {
        return None;
    }
    let doc = match Config::load_from(Some(path), None) {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "legacy config unparseable");
            return None;
        }
    };
    let url = doc.providers.get(PROVIDER_ID)?.base_url.trim();
    (!url.is_empty()).then(|| url.to_owned())
}

/// Strip exactly one trailing `/v1`. The **host** is never touched.
///
/// `ProviderStatus.base_url` and `Backend.base_url` are both stored without the suffix; the
/// config value keeps it.
fn without_v1(url: &str) -> String {
    let t = url.trim().trim_end_matches('/');
    t.strip_suffix("/v1").unwrap_or(t).to_owned()
}

/// The validated provider id.
///
/// # Errors
/// Only if [`PROVIDER_ID`] stops being a valid slug, which a test pins.
pub fn provider_id() -> Result<ProviderId> {
    Ok(ProviderId::parse(PROVIDER_ID)?)
}

// ---------------------------------------------------------------------------------------
// the client
// ---------------------------------------------------------------------------------------

/// How hard a catalogue fetch tries in the face of a 429.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Total attempts, including the first.
    pub max_attempts: u32,
    /// Longest wait this process will actually sleep. A 429 asking for more than this is
    /// reported to the operator instead of silently stalling a daemon for five minutes.
    pub max_wait: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            max_attempts: 3,
            max_wait: MAX_BACKOFF,
        }
    }
}

/// One catalogue read, with the rate-limit state it observed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CatalogueFetch {
    /// The models, in the order Together listed them.
    pub models: Vec<TogetherModel>,
    /// `Some` only if a 429 was seen. A clean fetch teaches us nothing about limits —
    /// Together sends no rate-limit headers on success — and says so by staying `None`.
    pub rate_limit: Option<RateLimitInfo>,
    /// When the successful read completed, unix seconds. Feeds
    /// `ProviderStatus.last_ok_unix`.
    pub fetched_unix: i64,
}

/// The together.ai catalogue client.
///
/// Holds a resolved credential and the **description** of where it came from; the secret
/// goes on exactly one wire (the `Authorization` header) and into no log, no error string
/// and no serialised struct.
pub struct TogetherClient {
    http: reqwest::Client,
    base_url: String,
    cred: Secret<String>,
    source: CredentialSource,
    retry: RetryPolicy,
}

impl std::fmt::Debug for TogetherClient {
    /// No credential, not even a redacted length.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TogetherClient")
            .field("base_url", &self.base_url)
            .field("credential", &self.source)
            .field("retry", &self.retry)
            .finish()
    }
}

impl TogetherClient {
    /// Build from an already-resolved credential. The `base_url` is used **verbatim**.
    pub fn new(
        http: reqwest::Client,
        base_url: impl Into<String>,
        cred: ResolvedCredential,
    ) -> Self {
        TogetherClient {
            http,
            base_url: base_url.into(),
            cred: cred.secret,
            source: cred.source,
            retry: RetryPolicy::default(),
        }
    }

    /// Build from config: [`base_url`] for the URL, `core::secret::resolve_provider` for the
    /// key — the **full** chain (`credentials.toml` → `api_key_file` →
    /// `~/.vastai-gguf/config.toml` → `$TOGETHER_API_KEY`), not just the env var.
    ///
    /// # Errors
    /// [`Error::MissingCredential`] when no link in the chain produced a key. That is a
    /// normal state for a fresh install, and the message names how to fix it.
    pub fn from_config(http: reqwest::Client, cfg: &Config, paths: &Paths) -> Result<Self> {
        let id = provider_id()?;
        let cred = resolve_provider(cfg, paths, &id)?
            .ok_or_else(|| Error::MissingCredential(PROVIDER_ID.to_owned()))?;
        Ok(TogetherClient::new(http, base_url(cfg, paths), cred))
    }

    /// Override the retry policy.
    #[must_use]
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// The base URL, exactly as configured — `/v1` and all, `.xyz` and all.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Where the credential came from. Never the credential.
    pub fn credential_source(&self) -> &CredentialSource {
        &self.source
    }

    /// [`fetch`](Self::fetch), keeping only the catalogue.
    ///
    /// # Errors
    /// As [`fetch`](Self::fetch).
    pub async fn models(&self) -> Result<Vec<TogetherModel>> {
        Ok(self.fetch().await?.models)
    }

    /// `GET /v1/models`, as a **bare array**, plus whatever a 429 taught us on the way.
    ///
    /// Retries a 429 according to [`RetryPolicy`], waiting exactly as long as
    /// `x-ratelimit-reset` asks (or a jittered backoff when it is absent). Gives up rather
    /// than sleeping past `max_wait`, and the error names Together's `error_type` so the
    /// operator knows whether it was a request or a token wall.
    ///
    /// `rate_limit` is `Some` only when a 429 was actually seen: Together sends no
    /// rate-limit headers on a successful response, so reporting a limit after a clean fetch
    /// would be inventing one.
    ///
    /// # Errors
    /// Transport failure, a 401/403 (the key is wrong, which is worth saying plainly), any
    /// other non-2xx, or a 429 that outlasts the policy.
    pub async fn fetch(&self) -> Result<CatalogueFetch> {
        let url = join_v1(&self.base_url, "/v1/models");
        let mut attempt: u32 = 0;
        let mut rate_limit: Option<RateLimitInfo> = None;
        loop {
            let res = self
                .http
                .get(&url)
                .bearer_auth(self.cred.expose())
                .header("accept", "application/json")
                .send()
                .await?;
            let status = res.status();

            if status.as_u16() == 429 {
                let headers = res.headers().clone();
                let body = res.json::<Value>().await.ok();
                let advice = rate_limited(&headers, body.as_ref(), attempt, now_unix());
                rate_limit = Some(advice.info);
                attempt += 1;
                if attempt >= self.retry.max_attempts || advice.retry_after > self.retry.max_wait {
                    return Err(Error::Other(format!("together: {}", advice.message())));
                }
                tracing::debug!(
                    wait_ms = advice.retry_after.as_millis() as u64,
                    from_header = advice.from_header,
                    "together rate limited, retrying"
                );
                tokio::time::sleep(advice.retry_after).await;
                continue;
            }

            if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                return Err(Error::Invalid {
                    what: "together credential".to_owned(),
                    why: format!("GET /v1/models -> {status}; the resolved key was rejected"),
                });
            }
            if !status.is_success() {
                return Err(Error::Other(format!("together: GET {url} -> {status}")));
            }

            let body = res.json::<Value>().await?;
            return Ok(CatalogueFetch {
                models: parse_models(&body),
                rate_limit,
                fetched_unix: now_unix(),
            });
        }
    }

    /// Everything `GET /v1/providers` reports about this provider.
    ///
    /// `base_url` is stored **without** the trailing `/v1` per the [`ProviderStatus`]
    /// invariant, while the host is left alone. The credential is described, never included.
    ///
    /// # Errors
    /// Only an id-validation failure, which a test pins as impossible.
    pub fn status(
        &self,
        models_cached: u32,
        last_ok_unix: Option<i64>,
        last_error: Option<String>,
        rate_limit: Option<RateLimitInfo>,
    ) -> Result<ProviderStatus> {
        Ok(ProviderStatus {
            id: provider_id()?,
            base_url: without_v1(&self.base_url),
            credential: self.source.clone(),
            credential_present: true,
            models_cached,
            last_ok_unix,
            last_error,
            rate_limit,
        })
    }
}

/// Wall clock, unix seconds. Zero if the clock is before the epoch, which is not a condition
/// worth an error path.
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_core::config::ProviderCfg;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// The real shape, from `docs/port/09` §2.2 — a **bare array**.
    const MODELS_BARE_ARRAY: &str = r#"[
      {
        "id": "Austism/chronos-hermes-13b",
        "object": "model",
        "created": 1692896905,
        "type": "chat",
        "display_name": "Chronos Hermes (13B)",
        "organization": "Austism",
        "link": null,
        "license": null,
        "context_length": 2048,
        "pricing": {"base": 0, "finetune": 0, "hourly": 0,
                    "input": 0.3, "output": 0.3, "cached_input": 0.2}
      },
      {
        "id": "togethercomputer/m2-bert-80M-8k-retrieval",
        "object": "model",
        "type": "embedding",
        "context_length": null,
        "pricing": {"base": 0, "finetune": 0, "hourly": 0, "input": 0.008, "output": 0}
      },
      {
        "id": "meta-llama/Llama-3.3-70B-Instruct-Turbo",
        "object": "model",
        "type": "chat",
        "context_length": 131072,
        "unknown_future_field": {"nested": true}
      }
    ]"#;

    fn cred() -> ResolvedCredential {
        ResolvedCredential {
            secret: Secret::new("sk-test".to_owned()),
            source: CredentialSource::Env {
                var: "TOGETHER_API_KEY".to_owned(),
            },
        }
    }

    fn client(base: &str) -> TogetherClient {
        TogetherClient::new(reqwest::Client::new(), base, cred()).with_retry(RetryPolicy {
            max_attempts: 3,
            max_wait: Duration::from_secs(5),
        })
    }

    // -- the bare array ------------------------------------------------------------------

    #[test]
    fn models_deserialise_the_bare_array_not_the_data_envelope() {
        let v: Value = serde_json::from_str(MODELS_BARE_ARRAY).expect("fixture");
        assert!(v.is_array(), "the fixture is the documented shape");
        let models = parse_models(&v);
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].id, "Austism/chronos-hermes-13b");
        assert_eq!(models[0].ctx(), Some(2048));
        assert_eq!(models[1].ctx(), None, "a null context_length is None");
        assert_eq!(models[2].ctx(), Some(131072));

        // The trap: an envelope deserialiser over the same bytes yields nothing.
        #[derive(serde::Deserialize, Default)]
        struct Envelope {
            #[serde(default)]
            data: Vec<TogetherModel>,
        }
        let naive: Envelope = serde_json::from_value(v).unwrap_or_default();
        assert!(
            naive.data.is_empty(),
            "this is exactly why the bare array needs its own deserialiser"
        );
    }

    #[test]
    fn an_envelope_is_tolerated_and_a_bad_row_does_not_lose_the_catalogue() {
        let v = json!({"object": "list", "data": [
            {"id": "a/b", "type": "chat"},
            {"id": 17},
            {"id": "", "type": "chat"},
            {"id": "c/d", "context_length": -1}
        ]});
        let models = parse_models(&v);
        assert_eq!(models.len(), 2, "the two malformed rows are skipped");
        assert_eq!(models[0].id, "a/b");
        assert_eq!(models[1].ctx(), None, "a negative context is not a context");
    }

    #[test]
    fn unknown_fields_survive_and_chat_filtering_works() {
        let v: Value = serde_json::from_str(MODELS_BARE_ARRAY).expect("fixture");
        let models = parse_models(&v);
        let chat = chat_models(&models);
        assert_eq!(chat.len(), 2, "the embedding row is not a chat model");
        let up = upstream_models(&models);
        assert_eq!(up.len(), 3);
        assert!(!up[0].tools, "Together advertises no capability flags");
        assert!(!up[0].vision);
    }

    // -- pricing -------------------------------------------------------------------------

    #[test]
    fn the_pricing_unit_assumption_is_recorded_never_silently_applied() {
        let v: Value = serde_json::from_str(MODELS_BARE_ARRAY).expect("fixture");
        let models = parse_models(&v);
        let pricing = models[0].pricing.as_ref().expect("pricing rides the model");
        assert_eq!(pricing.input, Some(0.3));
        assert_eq!(pricing.cached_input, Some(0.2));

        let est = estimate_cost(
            Some(pricing),
            TokenCount::Reported(1_000_000),
            TokenCount::Reported(1_000_000),
        );
        assert!(est.is_guess());
        match &est {
            CostEstimate::Approximate {
                usd,
                source,
                assumption,
            } => {
                assert_eq!(*usd, Money::from_usd(0.6), "$0.30 in + $0.30 out per 1M");
                assert_eq!(*source, PriceSource::ProviderApi);
                assert_eq!(assumption, PRICING_ASSUMPTION);
                assert!(assumption.contains("per 1M tokens"), "{assumption}");
                assert!(assumption.contains("states no unit"), "{assumption}");
            }
            other => panic!("a guessed unit must never render as metered: {other:?}"),
        }
    }

    #[test]
    fn an_estimated_token_count_joins_the_same_assumption_string() {
        let pricing = TogetherPricing {
            input: Some(0.3),
            output: Some(0.3),
            ..TogetherPricing::default()
        };
        let est = estimate_cost(
            Some(&pricing),
            TokenCount::Reported(1000),
            TokenCount::Estimated(1000),
        );
        let CostEstimate::Approximate { assumption, .. } = est else {
            panic!("expected Approximate");
        };
        assert!(assumption.starts_with(PRICING_ASSUMPTION), "{assumption}");
        assert!(assumption.contains("completion token count was estimated"));
    }

    #[test]
    fn a_model_without_pricing_is_unknown_never_free() {
        assert_eq!(
            estimate_cost(None, TokenCount::Reported(10), TokenCount::Reported(10)),
            CostEstimate::Unknown
        );
        let v: Value = serde_json::from_str(MODELS_BARE_ARRAY).expect("fixture");
        let cat = price_catalogue(&parse_models(&v));
        assert_eq!(cat.rows.len(), 2, "the unpriced row is omitted, not zeroed");
        assert_eq!(cat.assumption, PRICING_ASSUMPTION);
        assert!(cat
            .rows
            .iter()
            .all(|(id, _)| id != "meta-llama/Llama-3.3-70B-Instruct-Turbo"));
    }

    #[test]
    fn price_models_cover_free_per_token_and_hourly() {
        assert_eq!(price_model(&TogetherPricing::default()), PriceModel::Free);
        assert_eq!(
            price_model(&TogetherPricing {
                input: Some(0.3),
                output: Some(0.9),
                ..TogetherPricing::default()
            }),
            PriceModel::PerToken {
                input: Money::from_usd(0.3),
                output: Money::from_usd(0.9),
            }
        );
        assert_eq!(
            price_model(&TogetherPricing {
                hourly: Some(2.5),
                ..TogetherPricing::default()
            }),
            PriceModel::PerHour {
                dph: Money::from_usd(2.5)
            }
        );
        // An hourly listing cannot price one serverless request without inventing a
        // throughput, so it says so.
        assert_eq!(
            estimate_cost(
                Some(&TogetherPricing {
                    hourly: Some(2.5),
                    ..TogetherPricing::default()
                }),
                TokenCount::Reported(10),
                TokenCount::Reported(10)
            ),
            CostEstimate::Unknown
        );
    }

    // -- finish_reason -------------------------------------------------------------------

    #[test]
    fn finish_reason_is_always_a_string_so_eos_does_not_break_billing() {
        let body = json!({
            "id": "8f1a", "object": "chat.completion", "model": "meta-llama/X",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"},
                         "finish_reason": "eos"}],
            "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}
        });
        let s = parse_completion(&body);
        assert_eq!(s.finish_reasons, vec!["eos".to_owned()]);
        assert!(
            s.has_non_openai_finish_reason(),
            "`eos` is Together-specific"
        );
        let usage = s.usage.expect("usage survived the unknown finish_reason");
        assert_eq!(usage.prompt_tokens, 11);
        assert_eq!(usage.completion_tokens, 7);
        assert_eq!(s.model.as_deref(), Some("meta-llama/X"));

        // The trap, spelled out: an enum over the OpenAI values fails the whole body and
        // takes the token counts with it.
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum OpenAiFinish {
            Stop,
            Length,
            ToolCalls,
        }
        assert!(
            serde_json::from_value::<OpenAiFinish>(json!("eos")).is_err(),
            "this is why finish_reason is a String"
        );
    }

    #[test]
    fn a_streamed_chunk_without_usage_is_not_a_zeroed_record() {
        let chunk = json!({
            "id": "8f1a", "object": "chat.completion.chunk", "model": "m",
            "choices": [{"index": 0, "delta": {"content": "x"}, "finish_reason": null}],
            "usage": null
        });
        let s = parse_completion(&chunk);
        assert!(s.usage.is_none(), "no usage is None, never 0/0");
        assert!(s.finish_reasons.is_empty());
        assert!(!s.has_non_openai_finish_reason());
    }

    // -- 429 -----------------------------------------------------------------------------

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            let name: reqwest::header::HeaderName = k.parse().expect("header name");
            h.insert(name, v.parse().expect("header value"));
        }
        h
    }

    #[test]
    fn a_429_reads_x_ratelimit_reset() {
        let h = headers(&[("x-ratelimit-reset", "12.5")]);
        let body = json!({"error_type": "dynamic_request_limited"});
        let r = rate_limited(&h, Some(&body), 0, 1_800_000_000);
        assert!(r.from_header);
        assert_eq!(r.retry_after, Duration::from_secs_f64(12.5));
        assert_eq!(r.error_type.as_deref(), Some("dynamic_request_limited"));
        assert_eq!(r.info.reset_unix, Some(1_800_000_012));
        assert!(
            r.message().contains("dynamic_request_limited"),
            "{}",
            r.message()
        );
    }

    #[test]
    fn x_ratelimit_remaining_is_never_relied_upon() {
        // A generous `remaining` next to a real `reset` must not shorten the wait, and a
        // `remaining` alone must not stand in for a missing `reset`.
        let h = headers(&[
            ("x-ratelimit-reset", "9"),
            ("x-ratelimit-remaining", "5000"),
        ]);
        let r = rate_limited(&h, None, 0, 1_800_000_000);
        assert_eq!(r.retry_after, Duration::from_secs(9));
        assert_eq!(r.info.remaining, Some(5000), "recorded, but not consulted");

        let only_remaining = headers(&[("x-ratelimit-remaining", "0")]);
        let r = rate_limited(&only_remaining, None, 0, 1_800_000_000);
        assert!(!r.from_header, "no reset header means our own backoff");
        assert!(r.retry_after <= MAX_BACKOFF);
        assert!(r.retry_after >= Duration::from_millis(250));
    }

    #[test]
    fn a_reset_that_is_a_unix_timestamp_is_converted_not_slept_on() {
        let h = headers(&[("x-ratelimit-reset", "1800000030")]);
        let r = rate_limited(&h, None, 0, 1_800_000_000);
        assert_eq!(r.retry_after, Duration::from_secs(30));

        let stale = headers(&[("x-ratelimit-reset", "1700000000")]);
        let r = rate_limited(&stale, None, 0, 1_800_000_000);
        assert_eq!(r.retry_after, Duration::ZERO, "a past reset is now");
    }

    #[test]
    fn a_missing_or_junk_reset_falls_back_to_capped_jittered_backoff() {
        for raw in ["", "soon", "-3", "NaN"] {
            let h = headers(&[("x-ratelimit-reset", raw)]);
            let r = rate_limited(&h, None, 9, 1_800_000_000);
            assert!(!r.from_header, "{raw:?} is not a usable reset");
            assert!(
                r.retry_after <= MAX_BACKOFF,
                "{raw:?} -> {:?}",
                r.retry_after
            );
        }
        for attempt in 0..12u32 {
            let d = backoff_with_jitter(attempt);
            assert!(d <= MAX_BACKOFF, "attempt {attempt} -> {d:?}");
            assert!(d >= Duration::from_millis(250));
        }
    }

    #[test]
    fn an_openai_shaped_error_body_still_yields_an_error_type() {
        let body = json!({"error": {"type": "dynamic_token_limited", "message": "slow down"}});
        let r = rate_limited(&HeaderMap::new(), Some(&body), 0, 0);
        assert_eq!(r.error_type.as_deref(), Some("dynamic_token_limited"));
    }

    // -- base URL ------------------------------------------------------------------------

    fn cfg_with_base(url: &str) -> Config {
        let mut c = Config::default();
        if let Some(p) = c.providers.get_mut(PROVIDER_ID) {
            p.base_url = url.to_owned();
        }
        c
    }

    #[test]
    fn api_together_xyz_is_never_rewritten() {
        let c = cfg_with_base("https://api.together.xyz/v1");
        assert_eq!(base_url_from(&c, None), "https://api.together.xyz/v1");

        let client = client("https://api.together.xyz/v1");
        assert_eq!(client.base_url(), "https://api.together.xyz/v1");
        let status = client.status(3, None, None, None).expect("status");
        assert_eq!(
            status.base_url, "https://api.together.xyz",
            "only the /v1 suffix is stripped; the host is untouched"
        );
    }

    #[test]
    fn the_legacy_file_supplies_the_base_url_when_config_is_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let legacy = dir.path().join("config.toml");
        std::fs::write(
            &legacy,
            "[providers.together]\nbase_url = \"https://api.together.xyz/v1\"\napi_key = \"nope\"\n",
        )
        .expect("write");

        // Config::default() ships the .ai default, so `migrate` skips importing the legacy
        // section — the legacy value must still win, or the host is silently rewritten.
        let untouched = Config::default();
        assert_eq!(
            base_url_from(&untouched, Some(&legacy)),
            "https://api.together.xyz/v1"
        );

        // An operator's own value outranks the legacy file.
        let chosen = cfg_with_base("http://127.0.0.1:9/v1");
        assert_eq!(
            base_url_from(&chosen, Some(&legacy)),
            "http://127.0.0.1:9/v1"
        );

        // No legacy file, untouched config: the shipped default.
        assert_eq!(base_url_from(&untouched, None), DEFAULT_BASE_URL);
    }

    #[test]
    fn a_malformed_or_absent_legacy_file_is_skipped_not_repaired() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bad = dir.path().join("config.toml");
        std::fs::write(&bad, "this is [not toml").expect("write");
        assert_eq!(
            base_url_from(&Config::default(), Some(&bad)),
            DEFAULT_BASE_URL
        );
        assert_eq!(
            base_url_from(&Config::default(), Some(&dir.path().join("nope.toml"))),
            DEFAULT_BASE_URL
        );
        // We never write to somebody else's state directory.
        assert_eq!(
            std::fs::read_to_string(&bad).expect("read"),
            "this is [not toml"
        );
    }

    #[test]
    fn an_empty_provider_section_still_resolves() {
        let mut c = Config::default();
        c.providers
            .insert(PROVIDER_ID.to_owned(), ProviderCfg::default());
        assert_eq!(base_url_from(&c, None), DEFAULT_BASE_URL);
        c.providers.remove(PROVIDER_ID);
        assert_eq!(base_url_from(&c, None), DEFAULT_BASE_URL);
    }

    #[test]
    fn the_provider_id_is_a_valid_slug() {
        assert_eq!(provider_id().expect("id").as_str(), PROVIDER_ID);
    }

    // -- the client, against loopback only -----------------------------------------------

    #[tokio::test]
    async fn the_client_fetches_a_bare_array_and_sends_a_bearer() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(MODELS_BARE_ARRAY, "application/json"),
            )
            .mount(&server)
            .await;

        // The configured URL already carries /v1; join_v1 must not produce /v1/v1.
        let c = client(&format!("{}/v1", server.uri()));
        let models = c.models().await.expect("models");
        assert_eq!(models.len(), 3);

        let req = &server.received_requests().await.expect("requests")[0];
        assert_eq!(req.url.path(), "/v1/models");
        assert_eq!(
            req.headers
                .get("authorization")
                .and_then(|v| v.to_str().ok()),
            Some("Bearer sk-test")
        );
    }

    #[tokio::test]
    async fn a_429_is_retried_after_x_ratelimit_reset_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("x-ratelimit-reset", "0")
                    .insert_header("x-ratelimit-remaining", "0")
                    .set_body_json(json!({"error_type": "dynamic_request_limited"})),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(MODELS_BARE_ARRAY, "application/json"),
            )
            .mount(&server)
            .await;

        let c = client(&server.uri());
        let fetched = c.fetch().await.expect("models");
        assert_eq!(fetched.models.len(), 3);
        assert_eq!(server.received_requests().await.expect("requests").len(), 2);

        // The 429 is the only thing that can teach us a limit, and it is carried out.
        let seen = fetched.rate_limit.expect("the 429 was observed");
        assert_eq!(seen.remaining, Some(0), "recorded, never acted on");
        assert!(seen.reset_unix.is_some());
        assert!(fetched.fetched_unix > 0);
    }

    #[tokio::test]
    async fn a_clean_fetch_invents_no_rate_limit() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(MODELS_BARE_ARRAY, "application/json"),
            )
            .mount(&server)
            .await;

        let fetched = client(&server.uri()).fetch().await.expect("models");
        assert!(
            fetched.rate_limit.is_none(),
            "Together sends no limit headers on success; reporting one would be a fiction"
        );
    }

    #[tokio::test]
    async fn a_persistent_429_names_the_error_type_and_gives_up() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("x-ratelimit-reset", "0")
                    .set_body_json(json!({"error_type": "dynamic_token_limited"})),
            )
            .mount(&server)
            .await;

        let c = client(&server.uri()).with_retry(RetryPolicy {
            max_attempts: 2,
            max_wait: Duration::from_secs(1),
        });
        let err = c.models().await.expect_err("gives up");
        let msg = err.to_string();
        assert!(msg.contains("dynamic_token_limited"), "{msg}");
        assert_eq!(server.received_requests().await.expect("requests").len(), 2);
    }

    #[tokio::test]
    async fn a_429_asking_for_longer_than_max_wait_is_reported_not_slept_on() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(429).insert_header("x-ratelimit-reset", "120"))
            .mount(&server)
            .await;

        let c = client(&server.uri()).with_retry(RetryPolicy {
            max_attempts: 5,
            max_wait: Duration::from_secs(2),
        });
        let err = c.models().await.expect_err("does not stall the daemon");
        assert!(err.to_string().contains("120.0 s"), "{err}");
        assert_eq!(
            server.received_requests().await.expect("requests").len(),
            1,
            "one attempt, then an answer for the operator"
        );
    }

    #[tokio::test]
    async fn a_401_says_the_key_was_rejected_and_never_echoes_it() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(401).set_body_json(json!({"error": "unauthorized"})),
            )
            .mount(&server)
            .await;

        let c = client(&server.uri());
        let err = c.models().await.expect_err("401");
        let msg = err.to_string();
        assert!(msg.contains("together credential"), "{msg}");
        assert!(
            !msg.contains("sk-test"),
            "the key never reaches an error string"
        );
    }

    #[test]
    fn debug_on_the_client_prints_the_source_not_the_secret() {
        let s = format!("{:?}", client("https://api.together.ai/v1"));
        assert!(s.contains("TOGETHER_API_KEY"), "{s}");
        assert!(!s.contains("sk-test"), "{s}");
    }
}
