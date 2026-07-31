//! OWNER: unit P-08 (providers/src/{checks,smoke,compare}.rs). Do not edit outside that
//! unit.
//!
//! `smoke.sh`, reimplemented natively: `smoke.models`, `smoke.warmup` (80 tokens),
//! `smoke.tools` (a `get_weather` probe with `tool_choice: auto`) and `smoke.throughput`
//! (200 tokens).
//!
//! Two deliberate improvements over the shell script: TTFT and tok/s are read from the
//! **`timings` object**, not a stopwatch, and the probe uses **the resolved route's model
//! id** rather than the hardcoded `"model":"x"` that 400s on every managed provider.
//!
//! The consequence of reading rather than stopwatching is that an upstream which does not
//! publish `timings` — every managed provider — reports `ttft_ms: None` and
//! `tok_per_s: None`. That is the honest answer: a wall-clock division across a shared
//! network path is not a throughput measurement, and presenting one as if it were is what
//! this reimplementation drops. Wall clock is still reported, in [`SmokeProbe::ms`], where
//! it is labelled as what it is.
//!
//! Probe order is `smoke.sh`'s order and it is load-bearing: `smoke.models` is what supplies
//! a model id when the caller did not resolve one, and `smoke.warmup` pays the weight-load
//! and prompt-cache cost so `smoke.throughput` measures generation rather than a cold start.
//!
//! # A reasoning model answers in a different field
//!
//! llama.cpp b9199 under `--reasoning-format deepseek` (and every managed provider that
//! exposes chain-of-thought separately) puts the model's thinking in
//! `choices[].message.reasoning_content` and may leave `content` empty or `null`. A probe
//! that reads only `content` calls that "answered, but with empty content" — a **false**
//! failure, and one that makes `smoke` untrustworthy on exactly the models this project
//! runs. [`answer`] is therefore the one place either field is read, it names which field
//! replied, and every probe and `compare` go through it.

use apexrouter_core::secret::Secret;
use apexrouter_core::upstream::{join_v1, parse_timings, parse_usage};
use apexrouter_protocol::{CheckId, CheckResult, CheckStatus, SmokeProbe};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// `GET /v1/models`.
pub const SMOKE_MODELS: &str = "smoke.models";
/// An 80-token warm-up.
pub const SMOKE_WARMUP: &str = "smoke.warmup";
/// A `get_weather` tool-calling probe with `tool_choice: auto`.
pub const SMOKE_TOOLS: &str = "smoke.tools";
/// A 200-token throughput run.
pub const SMOKE_THROUGHPUT: &str = "smoke.throughput";

/// The four probe names, in run order.
pub const SMOKE_PROBES: [&str; 4] = [SMOKE_MODELS, SMOKE_WARMUP, SMOKE_TOOLS, SMOKE_THROUGHPUT];

/// How many characters of an upstream body travel into a `detail`.
const DETAIL_BODY_CHARS: usize = 200;

/// What one smoke run is aimed at.
///
/// `model` is **the resolved route's model id**, not a placeholder. An empty string is
/// allowed and means "ask `/v1/models` and use the first id it lists", which is the only
/// safe automatic answer; it is never silently turned into `"x"`.
#[derive(Clone, Debug)]
pub struct SmokeTarget {
    /// Base URL, with or without a trailing `/v1` — [`join_v1`] normalises both.
    pub base_url: String,
    /// The upstream model id to send. Empty means "discover it from `/v1/models`".
    pub model: String,
    /// Bearer credential, when the upstream wants one.
    pub cred: Option<Secret<String>>,
    /// Per-request budget. A 200-token run on a 4 tok/s iGPU is a minute of honest work.
    pub timeout: Duration,
}

impl SmokeTarget {
    /// A target with the default 180-second per-request budget and no credential.
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> SmokeTarget {
        SmokeTarget {
            base_url: base_url.into(),
            model: model.into(),
            cred: None,
            timeout: Duration::from_secs(180),
        }
    }

    /// Attach a bearer credential.
    pub fn with_cred(mut self, cred: Option<Secret<String>>) -> SmokeTarget {
        self.cred = cred;
        self
    }

    /// Override the per-request budget.
    pub fn with_timeout(mut self, timeout: Duration) -> SmokeTarget {
        self.timeout = timeout;
        self
    }

    /// `smoke.sh`'s provider label heuristic, kept because it is what an operator reads
    /// first: an `api.together` in the base URL is the managed service, anything else is a
    /// box of ours.
    pub fn label(&self) -> &'static str {
        if self.base_url.contains("api.together") {
            "Together AI (managed)"
        } else {
            "Self-hosted (tunnel/localhost)"
        }
    }
}

/// Run all four probes in order, streaming each through `tx` as it lands and returning the
/// whole set in run order.
///
/// The probes are **sequential on purpose** — see the module docs — which is the one place
/// this file deliberately does not copy [`compare`](crate::compare)'s parallelism.
///
/// When `target.model` is empty, the id `smoke.models` discovered is used for the other
/// three. A closed receiver is not an error: `apexrouter smoke --json` attaches none.
pub async fn run(
    http: &reqwest::Client,
    target: &SmokeTarget,
    tx: mpsc::Sender<SmokeProbe>,
) -> Vec<SmokeProbe> {
    let mut out = Vec::with_capacity(SMOKE_PROBES.len());

    let (first, ids) = list_models(http, target).await;
    let _ = tx.send(first.clone()).await;
    out.push(first);

    let mut resolved = target.clone();
    if resolved.model.trim().is_empty() {
        if let Some(id) = ids.first() {
            resolved.model = id.clone();
        }
    }

    for p in [
        warmup(http, &resolved).await,
        tools(http, &resolved).await,
        throughput(http, &resolved).await,
    ] {
        let _ = tx.send(p.clone()).await;
        out.push(p);
    }
    out
}

/// `smoke.models` — `GET /v1/models`, in both the llama.cpp envelope and Together's bare
/// array.
pub async fn models(http: &reqwest::Client, target: &SmokeTarget) -> SmokeProbe {
    list_models(http, target).await.0
}

/// `smoke.warmup` — one 80-token completion. Its job is to pay the load cost, so a slow
/// answer here is still a pass; an empty one is not.
pub async fn warmup(http: &reqwest::Client, target: &SmokeTarget) -> SmokeProbe {
    let body = json!({
        "model": target.model,
        "messages": [{"role": "user", "content": "In one sentence, what is a hash table?"}],
        "max_tokens": 80,
    });
    let started = Instant::now();
    let outcome = chat(http, target, &body).await;
    let mut p = finish(SMOKE_WARMUP, started, &outcome);
    if let (true, Some(v)) = (p.ok, outcome.body.as_ref()) {
        match answer(v) {
            // A reasoning model with an empty `content` **answered**. Naming the field is
            // the point: "it replied in reasoning_content" is a fact about the endpoint's
            // `--reasoning-format`, not a defect, and the operator should see which it was.
            Some(Answer { text, field }) => {
                p.ok = true;
                p.detail = match field {
                    AnswerField::Content => first_chars(&text, 120),
                    AnswerField::Reasoning => {
                        format!("via {}: {}", field.as_str(), first_chars(&text, 120))
                    }
                };
            }
            None => {
                p.ok = false;
                p.detail =
                    "answered, but with neither `content` nor `reasoning_content`".to_owned();
            }
        }
    }
    p
}

/// `smoke.tools` — the `get_weather` probe with `tool_choice: auto`.
///
/// `ok` means *the model actually called the tool*. A 200 that answers in prose is reported
/// as a failure of tool calling with the endpoint's health stated explicitly in `detail`,
/// because "the endpoint is up but the chat template does not do tools" is the finding this
/// probe exists to produce.
pub async fn tools(http: &reqwest::Client, target: &SmokeTarget) -> SmokeProbe {
    let body = json!({
        "model": target.model,
        "messages": [{
            "role": "user",
            "content": "What is the weather in Reykjavik right now? Use the tool."
        }],
        "tools": [{
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "Get current weather for a city.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "city": {"type": "string", "description": "City name"},
                        "unit": {"type": "string", "enum": ["c", "f"], "description": "Temp unit"}
                    },
                    "required": ["city"]
                }
            }
        }],
        "tool_choice": "auto",
        "max_tokens": 256,
    });
    let started = Instant::now();
    let outcome = chat(http, target, &body).await;
    let mut p = finish(SMOKE_TOOLS, started, &outcome);
    if let (true, Some(v)) = (p.ok, outcome.body.as_ref()) {
        let names = tool_call_names(v);
        if names.is_empty() {
            p.ok = false;
            // The prose it answered with instead is the finding, and on a thinking model
            // that prose lives in `reasoning_content` — reading only `content` here would
            // report "no tool_calls: " with nothing after the colon.
            p.detail = format!(
                "the endpoint answered, but with no tool_calls: {}",
                first_chars(&answer_text(v).unwrap_or_default(), 120)
            );
        } else {
            p.ok = names.iter().any(|n| n == "get_weather");
            p.detail = format!("tool_calls: {}", names.join(", "));
            if !p.ok {
                p.detail.push_str(" — but not get_weather");
            }
        }
    }
    p
}

/// `smoke.throughput` — a 200-token run. tok/s is `timings.predicted_per_second`, read.
pub async fn throughput(http: &reqwest::Client, target: &SmokeTarget) -> SmokeProbe {
    let body = json!({
        "model": target.model,
        "messages": [{
            "role": "user",
            "content": "Write a 200-word explanation of how a B-tree differs from a hash \
                        table for database indexing."
        }],
        "max_tokens": 200,
        "stream": false,
    });
    let started = Instant::now();
    let outcome = chat(http, target, &body).await;
    let mut p = finish(SMOKE_THROUGHPUT, started, &outcome);
    if let (true, Some(v)) = (p.ok, outcome.body.as_ref()) {
        let model = response_model(v).unwrap_or_else(|| target.model.clone());
        p.detail = match (p.tok_per_s, p.tokens) {
            (Some(rate), Some(n)) => format!("{n} tokens at {rate:.2} tok/s (model {model})"),
            (None, Some(n)) => format!(
                "{n} tokens (model {model}); the upstream published no `timings`, so tok/s \
                 is not reported rather than stopwatched"
            ),
            _ => "completed, but the response carried neither `timings` nor `usage`".to_owned(),
        };
    }
    p
}

/// Render a probe as a [`CheckResult`], so `doctor` can show the smoke rows next to the
/// registry's own without either side learning about the other's type.
pub fn as_check_result(p: &SmokeProbe) -> CheckResult {
    CheckResult {
        id: CheckId::from(p.name.as_str()),
        label: p.name.clone(),
        status: if p.ok {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        ms: p.ms,
        detail: p.detail.clone(),
        fix: (!p.ok).then(|| format!("re-run just this one: `apexrouter smoke` ({})", p.name)),
    }
}

// ----------------------------------------------------------------------------------------
// the shared chat call — `compare` uses it too
// ----------------------------------------------------------------------------------------

/// One answered (or unanswered) `POST /v1/chat/completions`.
pub(crate) struct ChatOutcome {
    /// The parsed body, when there was one and it was JSON.
    pub(crate) body: Option<Value>,
    /// Transport failure or a non-2xx, already rendered for a human.
    pub(crate) error: Option<String>,
}

/// `POST {base}/v1/chat/completions`. Never `Err`: an unreachable upstream is a finding.
pub(crate) async fn chat(
    http: &reqwest::Client,
    target: &SmokeTarget,
    body: &Value,
) -> ChatOutcome {
    let url = join_v1(&target.base_url, "/v1/chat/completions");
    let mut rb = http
        .post(&url)
        .timeout(target.timeout)
        .header("content-type", "application/json")
        .json(body);
    if let Some(c) = target.cred.as_ref() {
        rb = rb.bearer_auth(c.expose());
    }

    let resp = match rb.send().await {
        Ok(r) => r,
        Err(e) => {
            return ChatOutcome {
                body: None,
                error: Some(redact(&e.to_string())),
            }
        }
    };
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let parsed = serde_json::from_str::<Value>(&text).ok();

    if !(200..300).contains(&status) {
        return ChatOutcome {
            body: parsed,
            error: Some(format!(
                "HTTP {status}: {}",
                first_chars(&text, DETAIL_BODY_CHARS)
            )),
        };
    }
    match parsed {
        Some(v) => ChatOutcome {
            body: Some(v),
            error: None,
        },
        None => ChatOutcome {
            body: None,
            error: Some(format!(
                "HTTP {status} with a body that is not JSON: {}",
                first_chars(&text, DETAIL_BODY_CHARS)
            )),
        },
    }
}

/// TTFT, tok/s and the generated-token count, **read** off a response body.
///
/// TTFT is `timings.prompt_ms` — the time the upstream spent before it could emit a token —
/// and tok/s is `timings.predicted_per_second`. Neither is derived from our own clock. The
/// token count falls back to `usage.completion_tokens`, which is a count rather than a
/// timing and so is safe to take from anywhere.
pub(crate) fn read_timings(body: &Value) -> (Option<u32>, Option<f32>, Option<u32>) {
    let t = parse_timings(body);
    let ttft = t.map(|t| t.prompt_ms.max(0.0).round() as u32);
    let rate = t.map(|t| t.predicted_per_second);
    let tokens = t
        .map(|t| t.predicted_n)
        .filter(|n| *n > 0)
        .or_else(|| parse_usage(body).map(|u| u.completion_tokens));
    (ttft, rate, tokens)
}

/// `choices[0].message.content`, when it is a non-empty string.
///
/// Empty is `None` on purpose: a thinking model sends `"content": ""` alongside a full
/// `reasoning_content`, and an empty string is not an answer. [`answer`] is what callers
/// should use; this is its first half.
pub(crate) fn content(body: &Value) -> Option<String> {
    message_field(body, "content")
}

/// `choices[0].message.reasoning_content`, when it is a non-empty string.
///
/// llama.cpp b9199 emits this under `--reasoning-format deepseek`; several managed
/// providers emit it unconditionally.
pub(crate) fn reasoning_content(body: &Value) -> Option<String> {
    message_field(body, "reasoning_content")
}

/// Which field of the assistant message carried the reply.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum AnswerField {
    /// The ordinary `message.content`.
    Content,
    /// `message.reasoning_content`, with `content` empty or absent.
    Reasoning,
}

impl AnswerField {
    /// The wire name, for a `detail` line.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            AnswerField::Content => "content",
            AnswerField::Reasoning => "reasoning_content",
        }
    }
}

/// What the assistant actually said, and where it said it.
#[derive(Clone, Debug)]
pub(crate) struct Answer {
    /// The text.
    pub(crate) text: String,
    /// Which field it came out of.
    pub(crate) field: AnswerField,
}

/// The assistant's reply, from `content` if there is one and `reasoning_content` if there
/// is not.
///
/// `content` wins when both are present and non-empty: the reasoning is the model's working,
/// the content is its answer. `None` means the endpoint returned a message with neither,
/// which is the only shape that is genuinely a failed probe.
pub(crate) fn answer(body: &Value) -> Option<Answer> {
    if let Some(text) = content(body) {
        return Some(Answer {
            text,
            field: AnswerField::Content,
        });
    }
    reasoning_content(body).map(|text| Answer {
        text,
        field: AnswerField::Reasoning,
    })
}

/// [`answer`]'s text, for the callers that only want something to show a human.
pub(crate) fn answer_text(body: &Value) -> Option<String> {
    answer(body).map(|a| a.text)
}

/// One string field of `choices[0].message`, `None` when absent, non-string or blank.
fn message_field(body: &Value, name: &str) -> Option<String> {
    let text = body
        .get("choices")?
        .get(0)?
        .get("message")?
        .get(name)?
        .as_str()?;
    (!text.trim().is_empty()).then(|| text.to_owned())
}

/// The response's own `model` field. Managed providers echo the canonical id here, which is
/// more truthful than the one we sent.
pub(crate) fn response_model(body: &Value) -> Option<String> {
    body.get("model")?.as_str().map(str::to_owned)
}

/// The leading `n` **characters** — never bytes, because slicing a byte offset out of a
/// non-ASCII answer panics.
pub(crate) fn first_chars(s: &str, n: usize) -> String {
    let trimmed = s.trim();
    let mut out: String = trimmed.chars().take(n).collect();
    if trimmed.chars().nth(n).is_some() {
        out.push('…');
    }
    out
}

/// Keep a credential out of an error string even if a URL somehow carried one.
pub(crate) fn redact(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len());
    for part in msg.split_inclusive(char::is_whitespace) {
        if part.contains("://") && part.contains('@') {
            out.push_str("<url>");
        } else {
            out.push_str(part);
        }
    }
    out
}

// ----------------------------------------------------------------------------------------
// internals
// ----------------------------------------------------------------------------------------

/// The models probe, plus the ids it found — `run` needs both, callers usually need one.
async fn list_models(http: &reqwest::Client, target: &SmokeTarget) -> (SmokeProbe, Vec<String>) {
    let started = Instant::now();
    let url = join_v1(&target.base_url, "/v1/models");
    let mut rb = http
        .get(&url)
        .timeout(target.timeout)
        .header("accept", "*/*");
    if let Some(c) = target.cred.as_ref() {
        rb = rb.bearer_auth(c.expose());
    }

    let (status, text) = match rb.send().await {
        Ok(r) => {
            let status = r.status().as_u16();
            (status, r.text().await.unwrap_or_default())
        }
        Err(e) => {
            return (
                probe(SMOKE_MODELS, false, started, redact(&e.to_string())),
                Vec::new(),
            )
        }
    };
    if !(200..300).contains(&status) {
        let detail = format!("HTTP {status}: {}", first_chars(&text, DETAIL_BODY_CHARS));
        return (probe(SMOKE_MODELS, false, started, detail), Vec::new());
    }

    let ids = match serde_json::from_str::<Value>(&text) {
        Ok(v) => model_ids(&v),
        Err(e) => {
            let detail = format!("the model list is not JSON: {e}");
            return (probe(SMOKE_MODELS, false, started, detail), Vec::new());
        }
    };
    if ids.is_empty() {
        let detail = "the upstream answered 200 with an empty model list";
        return (probe(SMOKE_MODELS, false, started, detail), ids);
    }
    let shown = ids.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
    let detail = format!("{} model(s): {shown}", ids.len());
    (probe(SMOKE_MODELS, true, started, detail), ids)
}

/// `/v1/models` in **both** shapes: llama.cpp's `{"data":[…]}` envelope and Together's bare
/// array. `smoke.sh` did this with `jq '[.data // .] | flatten'`; this is the same rule.
fn model_ids(v: &Value) -> Vec<String> {
    let rows: &[Value] = match v.get("data").and_then(Value::as_array) {
        Some(rows) => rows,
        None => match v.as_array() {
            Some(rows) => rows,
            None => return Vec::new(),
        },
    };
    rows.iter()
        .filter_map(|r| r.get("id").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

/// Every function name in `choices[0].message.tool_calls`.
fn tool_call_names(body: &Value) -> Vec<String> {
    let Some(calls) = body
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("tool_calls"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    calls
        .iter()
        .filter_map(|c| {
            c.get("function")
                .and_then(|f| f.get("name"))
                .or_else(|| c.get("name"))
                .and_then(Value::as_str)
        })
        .map(str::to_owned)
        .collect()
}

/// Build a probe with the wall clock filled in.
fn probe(name: &str, ok: bool, started: Instant, detail: impl Into<String>) -> SmokeProbe {
    SmokeProbe {
        name: name.to_owned(),
        ok,
        ms: millis(started),
        detail: detail.into(),
        ttft_ms: None,
        tok_per_s: None,
        tokens: None,
    }
}

/// Turn a chat outcome into a probe, timings and all. The caller refines `detail`.
fn finish(name: &str, started: Instant, outcome: &ChatOutcome) -> SmokeProbe {
    match (&outcome.error, &outcome.body) {
        (Some(e), _) => probe(name, false, started, e.clone()),
        (None, Some(v)) => {
            let (ttft_ms, tok_per_s, tokens) = read_timings(v);
            let mut p = probe(name, true, started, "ok");
            p.ttft_ms = ttft_ms;
            p.tok_per_s = tok_per_s;
            p.tokens = tokens;
            p
        }
        (None, None) => probe(name, false, started, "no response body"),
    }
}

/// Saturating millisecond count.
fn millis(started: Instant) -> u32 {
    u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client() -> reqwest::Client {
        reqwest::Client::new()
    }

    fn target(server: &MockServer) -> SmokeTarget {
        SmokeTarget::new(server.uri(), "Carnice-9b-Q6_K").with_timeout(Duration::from_secs(5))
    }

    /// A llama.cpp-shaped completion, with the `timings` object.
    fn completion(text: &str, predicted: u32, rate: f64) -> Value {
        json!({
            "model": "Carnice-9b-Q6_K",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": text},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 37, "completion_tokens": predicted},
            "timings": {"prompt_n": 37, "prompt_ms": 412.5, "predicted_n": predicted,
                        "predicted_ms": 20000.0, "predicted_per_second": rate}
        })
    }

    async fn mock_models(server: &MockServer, body: Value) {
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn all_four_probes_run_in_order_and_stream() {
        let server = MockServer::start().await;
        mock_models(
            &server,
            json!({"object": "list", "data": [{"id": "Carnice-9b-Q6_K"}]}),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(body_partial_json(json!({"tool_choice": "auto"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "model": "Carnice-9b-Q6_K",
                "choices": [{"index": 0, "message": {"role": "assistant", "content": null,
                    "tool_calls": [{"id": "call_1", "type": "function",
                        "function": {"name": "get_weather",
                                     "arguments": "{\"city\":\"Reykjavik\"}"}}]}}],
                "timings": {"prompt_n": 90, "prompt_ms": 300.0, "predicted_n": 24,
                            "predicted_ms": 2000.0, "predicted_per_second": 12.0}
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(completion(
                "A hash table is…",
                80,
                9.71,
            )))
            .mount(&server)
            .await;

        let (tx, mut rx) = mpsc::channel(8);
        let out = run(&client(), &target(&server), tx).await;

        let names: Vec<&str> = out.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, SMOKE_PROBES.to_vec());
        assert!(out.iter().all(|p| p.ok), "{out:#?}");

        let mut streamed = Vec::new();
        while let Ok(p) = rx.try_recv() {
            streamed.push(p.name);
        }
        assert_eq!(streamed, SMOKE_PROBES.to_vec());
    }

    #[tokio::test]
    async fn ttft_and_rate_are_read_from_timings_never_stopwatched() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(120))
                    .set_body_json(completion("A B-tree keeps order…", 200, 9.71)),
            )
            .mount(&server)
            .await;

        let p = throughput(&client(), &target(&server)).await;
        assert!(p.ok);
        // 412.5 ms of prompt processing, not the ~120 ms we waited.
        assert_eq!(p.ttft_ms, Some(413));
        assert_eq!(p.tok_per_s, Some(9.71));
        assert_eq!(p.tokens, Some(200));
    }

    #[tokio::test]
    async fn an_upstream_without_timings_reports_no_rate_rather_than_a_stopwatched_one() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "model": "meta-llama/Llama-3.3-70B-Instruct-Turbo",
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "…"},
                             "finish_reason": "eos"}],
                "usage": {"prompt_tokens": 41, "completion_tokens": 200}
            })))
            .mount(&server)
            .await;

        let p = throughput(&client(), &target(&server)).await;
        assert!(p.ok);
        assert_eq!(p.ttft_ms, None);
        assert_eq!(p.tok_per_s, None);
        assert_eq!(
            p.tokens,
            Some(200),
            "a token count is a count, not a timing"
        );
        assert!(p.detail.contains("no `timings`"), "{}", p.detail);
    }

    #[tokio::test]
    async fn the_resolved_model_id_is_sent_never_the_hardcoded_x() {
        let server = MockServer::start().await;
        mock_models(
            &server,
            json!([{"id": "meta-llama/Llama-3.3-70B-Instruct-Turbo", "object": "model"}]),
        )
        .await;
        // Only a request naming the resolved id matches; `"model":"x"` falls through to the
        // 400 every managed provider answers with.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(body_partial_json(
                json!({"model": "meta-llama/Llama-3.3-70B-Instruct-Turbo"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(completion("hi", 8, 60.0)))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": {"message": "Unable to access model x"}
            })))
            .mount(&server)
            .await;

        // An empty model id means "discover it", never "send x".
        let t = SmokeTarget::new(server.uri(), "").with_timeout(Duration::from_secs(5));
        let (tx, _rx) = mpsc::channel(8);
        let out = run(&client(), &t, tx).await;
        assert!(out[0].ok, "{:#?}", out[0]);
        assert!(
            out[1].ok,
            "warm-up must use the discovered id: {:#?}",
            out[1]
        );
    }

    #[tokio::test]
    async fn a_400_is_a_failed_probe_with_the_body_in_the_detail() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": {"message": "Unable to access model x", "type": "invalid_request_error"}
            })))
            .mount(&server)
            .await;

        let p = warmup(&client(), &target(&server)).await;
        assert!(!p.ok);
        assert!(p.detail.starts_with("HTTP 400"), "{}", p.detail);
        assert!(p.detail.contains("Unable to access model"), "{}", p.detail);
    }

    /// A thinking model's completion: the answer is in `reasoning_content` and `content` is
    /// empty. This is what llama.cpp b9199 sends under `--reasoning-format deepseek`, and
    /// what the fake `llama-server`'s `reasoning` behaviour produces.
    fn reasoning_completion(thought: &str, content: Value) -> Value {
        json!({
            "model": "Carnice-9b-Q6_K",
            "choices": [{"index": 0, "finish_reason": "stop", "message": {
                "role": "assistant", "content": content, "reasoning_content": thought}}],
            "usage": {"prompt_tokens": 37, "completion_tokens": 80},
            "timings": {"prompt_n": 37, "prompt_ms": 412.5, "predicted_n": 80,
                        "predicted_ms": 8000.0, "predicted_per_second": 10.0}
        })
    }

    #[tokio::test]
    async fn a_thinking_model_that_answers_in_reasoning_content_is_a_pass() {
        // Both spellings of "no content": the field absent, and the field present but empty.
        for empty in [json!(""), json!(null)] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(reasoning_completion(
                        "A hash table maps keys to buckets by hashing the key.",
                        empty.clone(),
                    )),
                )
                .mount(&server)
                .await;

            let p = warmup(&client(), &target(&server)).await;

            assert!(p.ok, "a reasoning model answered: {p:#?}");
            assert!(
                p.detail.contains("reasoning_content"),
                "the detail must say which field replied: {}",
                p.detail
            );
            assert!(p.detail.contains("hash table maps keys"), "{}", p.detail);
        }
    }

    #[tokio::test]
    async fn a_message_with_neither_field_is_still_a_failed_warmup() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "model": "Carnice-9b-Q6_K",
                "choices": [{"index": 0, "message": {"role": "assistant", "content": ""}}]
            })))
            .mount(&server)
            .await;

        let p = warmup(&client(), &target(&server)).await;
        assert!(!p.ok);
        assert!(p.detail.contains("reasoning_content"), "{}", p.detail);
    }

    /// The same thing again, but against the **fake `llama-server`** rather than a
    /// hand-written body: `reasoning` is its b9199 `--reasoning-format` shape, and this is
    /// the difference between "our JSON reader handles the shape we imagined" and "the
    /// probe passes against a server that answers the way the real one does".
    #[tokio::test]
    async fn the_fake_server_answering_in_reasoning_content_passes_the_whole_run() {
        use apexrouter_tests_support::Stub;

        let upstream = Stub::with("reasoning,content=A hash table maps keys to buckets.");
        let t = SmokeTarget::new(upstream.base_url(), "stub-model")
            .with_timeout(Duration::from_secs(10));

        let p = warmup(&client(), &t).await;
        assert!(
            p.ok,
            "the fake answered and the probe called it empty: {p:#?}"
        );
        assert!(p.detail.contains("reasoning_content"), "{}", p.detail);
        assert!(p.detail.contains("hash table"), "{}", p.detail);

        // The warm-up is what `smoke` gates on, so the whole run must not fail on it either.
        let (tx, _rx) = mpsc::channel(8);
        let all = run(&client(), &t, tx).await;
        let warm = all
            .iter()
            .find(|p| p.name == SMOKE_WARMUP)
            .expect("the warm-up ran");
        assert!(warm.ok, "{warm:#?}");

        // …and the endpoint really did leave `content` empty, so the pass came from the
        // fallback rather than from the fake being unfaithful.
        let seen = upstream.last_request().expect("a request reached the fake");
        assert_eq!(seen.model().as_deref(), Some("stub-model"));
    }

    #[test]
    fn content_wins_over_reasoning_when_both_are_there() {
        let both = json!({"choices": [{"message": {
            "content": "the answer", "reasoning_content": "the working"}}]});
        let a = answer(&both).expect("an answer");
        assert_eq!(a.field, AnswerField::Content);
        assert_eq!(a.text, "the answer");

        // Whitespace is not an answer, so the reasoning is used.
        let blank = json!({"choices": [{"message": {
            "content": "   ", "reasoning_content": "the working"}}]});
        let a = answer(&blank).expect("an answer");
        assert_eq!(a.field, AnswerField::Reasoning);
        assert_eq!(a.text, "the working");

        assert!(answer(&json!({"choices": [{"message": {"content": null}}]})).is_none());
        assert!(answer(&json!({})).is_none());
    }

    #[tokio::test]
    async fn a_prose_answer_to_the_tool_probe_is_not_a_pass() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(completion(
                "I do not have live weather access.",
                20,
                9.0,
            )))
            .mount(&server)
            .await;

        let p = tools(&client(), &target(&server)).await;
        assert!(!p.ok);
        assert!(p.detail.contains("no tool_calls"), "{}", p.detail);
    }

    #[tokio::test]
    async fn the_credential_travels_as_a_bearer_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer sk-test-123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"id": "m"}])))
            .mount(&server)
            .await;

        let t = target(&server).with_cred(Some(Secret::new("sk-test-123".to_owned())));
        let p = models(&client(), &t).await;
        assert!(p.ok, "{}", p.detail);
    }

    #[tokio::test]
    async fn a_base_url_with_a_trailing_v1_still_smokes() {
        let server = MockServer::start().await;
        mock_models(&server, json!({"object": "list", "data": [{"id": "m"}]})).await;
        let t = SmokeTarget::new(format!("{}/v1", server.uri()), "m")
            .with_timeout(Duration::from_secs(5));
        assert!(models(&client(), &t).await.ok);
    }

    #[tokio::test]
    async fn an_unreachable_upstream_fails_every_probe_without_panicking() {
        // Port 1 on loopback: nothing listens there, and nothing unprivileged may bind it.
        let t = SmokeTarget::new("http://127.0.0.1:1", "m").with_timeout(Duration::from_secs(2));
        let (tx, _rx) = mpsc::channel(8);
        let out = run(&client(), &t, tx).await;
        assert_eq!(out.len(), 4);
        assert!(out.iter().all(|p| !p.ok));
    }

    #[test]
    fn model_ids_reads_both_list_shapes() {
        assert_eq!(
            model_ids(&json!({"object": "list", "data": [{"id": "a"}, {"id": "b"}]})),
            vec!["a", "b"]
        );
        assert_eq!(model_ids(&json!([{"id": "a"}])), vec!["a"]);
        assert!(model_ids(&json!({"data": null})).is_empty());
    }

    #[test]
    fn a_preview_never_splits_a_character() {
        let s = "héllo wörld";
        assert_eq!(first_chars(s, 4), "héll…");
        assert_eq!(first_chars(s, 99), s);
    }

    #[test]
    fn the_provider_label_is_the_shell_scripts_heuristic() {
        assert_eq!(
            SmokeTarget::new("https://api.together.xyz/v1", "m").label(),
            "Together AI (managed)"
        );
        assert_eq!(
            SmokeTarget::new("http://127.0.0.1:8888", "m").label(),
            "Self-hosted (tunnel/localhost)"
        );
    }

    #[test]
    fn a_probe_renders_as_a_check_row() {
        let p = SmokeProbe {
            name: SMOKE_TOOLS.to_owned(),
            ok: false,
            ms: 12,
            detail: "no tool_calls".to_owned(),
            ttft_ms: None,
            tok_per_s: None,
            tokens: None,
        };
        let r = as_check_result(&p);
        assert_eq!(r.id, CheckId::from("smoke.tools"));
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.fix.is_some());
    }
}
