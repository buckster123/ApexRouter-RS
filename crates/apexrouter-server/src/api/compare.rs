//! OWNER: unit S-07 (server/src/api/{vast,hf,providers,checks,compare}.rs). Do not edit outside that unit.
//!
//! `POST /v1/compare` — one prompt across N aliases, in parallel.
//!
//! Two things LocalRouter got wrong and this does not:
//!
//! * **The aliases run in parallel.** LocalRouter ran them serially, so comparing four
//!   providers took the sum of four cold starts and the numbers were taken minutes apart —
//!   which is the one thing a comparison must not do.
//! * **`prompt_tokens` is the number the provider reported.** LocalRouter fabricated
//!   `word_count * 1.3` *while discarding the real count the response carried*. Here the
//!   response's own `usage` is read into a [`TokenCount::Reported`], and only a provider
//!   that sent nothing produces a [`TokenCount::Estimated`] — which renders as a guess
//!   everywhere, because it is one.
//!
//! Each alias is resolved through the **live routing table** and called on its backend
//! directly, exactly as `POST /v1/routes/{alias}/test` does: comparing through our own proxy
//! socket would also be comparing the listener, the auth layer and the loopback stack.

use super::{ApiError, ApiResult};
use crate::state::AppState;
use apexrouter_core::pricing::PriceTable;
use apexrouter_core::secret::Secret;
use apexrouter_core::upstream;
use apexrouter_protocol::{
    Alias, BackendId, CompareRow, CostEstimate, JobRecord, PriceModel, ProviderId, TokenCount,
};
use apexrouter_router::{RequestClass, UnknownModelPolicy};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Default `max_tokens` when the caller does not say. Long enough for the answers to differ
/// in a way worth reading, short enough that a four-way comparison is not a bill.
const DEFAULT_MAX_TOKENS: u32 = 128;
/// How long one alias gets. A cold managed model can take most of a minute.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// How much of each answer travels back in `preview`.
const PREVIEW_CHARS: usize = 200;

/// The `/v1/compare` route.
pub fn router() -> axum::Router<Arc<AppState>> {
    axum::Router::new().route("/v1/compare", post(compare))
}

/// `{aliases[], prompt, max_tokens}`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct CompareRequest {
    /// Which aliases to put side by side. Each is resolved through the live table.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// The one prompt every alias gets.
    pub prompt: String,
    /// Generation budget per alias.
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

/// `?no_wait=true`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct CompareQuery {
    /// Return a `JobRecord` immediately instead of waiting for every alias.
    #[serde(default)]
    pub no_wait: Option<bool>,
}

/// `POST /v1/compare` — the same prompt against N aliases, at the same time.
///
/// Blocking by default (the answer *is* the comparison), `202` with a [`JobRecord`] under
/// `?no_wait=true`, whose result is the same `Vec<CompareRow>`.
pub async fn compare(
    State(s): State<Arc<AppState>>,
    Query(q): Query<CompareQuery>,
    Json(req): Json<CompareRequest>,
) -> Result<Response, ApiError> {
    let targets = plan(&s, &req)?;

    if q.no_wait.unwrap_or(false) {
        let state = Arc::clone(&s);
        let job = s.jobs.spawn_with("compare", move |h| async move {
            h.progress(Some(5.0), format!("{} aliases, in parallel", targets.len()));
            Ok::<_, anyhow::Error>(run(&state, targets, &req.prompt, max_tokens(&req)).await)
        });
        return Ok((StatusCode::ACCEPTED, Json(job)).into_response());
    }

    let rows = run(&s, targets, &req.prompt, max_tokens(&req)).await;
    Ok(Json(rows).into_response())
}

// ----------------------------------------------------------------------------------------
// shared with the rest of S-07
// ----------------------------------------------------------------------------------------

/// Run every alias **concurrently** and collect one row each.
///
/// Published so `cli`/`mcp`-facing code and any future batch surface get exactly this
/// behaviour rather than a second, serial implementation.
pub async fn run(
    s: &Arc<AppState>,
    targets: Vec<Target>,
    prompt: &str,
    max_tokens: u32,
) -> Vec<CompareRow> {
    let calls = targets
        .into_iter()
        .map(|t| one(s, t, prompt, max_tokens))
        .collect::<Vec<_>>();
    futures_util::future::join_all(calls).await
}

/// One alias, resolved against the table at plan time so every call in a batch sees the
/// same routing decision.
#[derive(Clone, Debug)]
pub struct Target {
    /// The alias asked for.
    pub alias: Alias,
    /// What it resolved to, when it resolved.
    pub backend: Option<BackendId>,
    /// The upstream, without `/v1`.
    pub base_url: String,
    /// The **resolved route's** model id.
    pub model: String,
    /// The backend's price, when it has one.
    pub price: Option<PriceModel>,
    /// The credential to present, when the backend needs one.
    pub credential: Option<Secret<String>>,
    /// Why this alias cannot be called at all.
    pub unroutable: Option<String>,
}

/// Resolve every alias in the request. An alias that does not resolve becomes a **row**, not
/// an error: a comparison of four aliases where one is down must still show the other three.
pub fn plan(s: &Arc<AppState>, req: &CompareRequest) -> Result<Vec<Target>, ApiError> {
    if req.aliases.is_empty() {
        return Err(
            ApiError::bad_request("invalid", "at least one alias is required")
                .with_param("aliases"),
        );
    }
    if req.prompt.trim().is_empty() {
        return Err(
            ApiError::bad_request("invalid", "prompt must not be empty").with_param("prompt")
        );
    }

    let table = s.router.table();
    let mut out = Vec::with_capacity(req.aliases.len());
    for raw in &req.aliases {
        let alias = Alias::parse(raw)
            .map_err(|e| ApiError::bad_request("bad_id", e.to_string()).with_param("aliases"))?;
        let resolved = table.resolve(
            Some(alias.as_str()),
            RequestClass::Chat,
            UnknownModelPolicy::Reject,
        );
        let target = match resolved {
            Err(e) => Target {
                alias,
                backend: None,
                base_url: String::new(),
                model: String::new(),
                price: None,
                credential: None,
                unroutable: Some(e.to_string()),
            },
            Ok(plan) => match plan.candidates.first() {
                None => Target {
                    alias,
                    backend: None,
                    base_url: String::new(),
                    model: String::new(),
                    price: None,
                    credential: None,
                    unroutable: Some("nothing behind this alias is routable".to_owned()),
                },
                Some(cand) => {
                    let meta = cand.backend.meta.load();
                    Target {
                        alias,
                        backend: Some(meta.id.clone()),
                        base_url: meta.base_url.clone(),
                        model: cand.upstream_model.clone(),
                        price: meta.price.clone(),
                        credential: ProviderId::parse(meta.id.as_str())
                            .ok()
                            .and_then(|p| super::providers::credential(s, &p)),
                        unroutable: None,
                    }
                }
            },
        };
        out.push(target);
    }
    Ok(out)
}

// ----------------------------------------------------------------------------------------
// internals
// ----------------------------------------------------------------------------------------

/// The requested budget, or the default.
fn max_tokens(req: &CompareRequest) -> u32 {
    req.max_tokens
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_TOKENS)
}

/// One alias's row.
async fn one(s: &Arc<AppState>, t: Target, prompt: &str, max_tokens: u32) -> CompareRow {
    if let Some(why) = t.unroutable {
        return CompareRow {
            alias: t.alias,
            backend: None,
            model: String::new(),
            ok: false,
            ms: 0,
            ttft_ms: None,
            tok_per_s: None,
            prompt_tokens: None,
            completion_tokens: None,
            cost: CostEstimate::Unknown,
            preview: String::new(),
            error: Some(why),
        };
    }

    let started = Instant::now();
    let url = upstream::join_v1(&t.base_url, "/v1/chat/completions");
    let body = serde_json::json!({
        "model": t.model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": max_tokens,
        "stream": false,
    });
    let mut rb = super::http().post(&url).timeout(CALL_TIMEOUT).json(&body);
    if let Some(c) = t.credential.as_ref() {
        rb = rb.bearer_auth(c.expose());
    }
    let sent = rb.send().await;
    let ms = millis(started.elapsed());

    let (status, json) = match sent {
        Err(e) => {
            return CompareRow {
                alias: t.alias,
                backend: t.backend,
                model: t.model,
                ok: false,
                ms,
                ttft_ms: None,
                tok_per_s: None,
                prompt_tokens: None,
                completion_tokens: None,
                cost: CostEstimate::Unknown,
                preview: String::new(),
                error: Some(e.to_string()),
            };
        }
        Ok(res) => {
            let status = res.status();
            let json: serde_json::Value = res.json().await.unwrap_or(serde_json::Value::Null);
            (status, json)
        }
    };

    let timings = upstream::parse_timings(&json);
    let usage = upstream::parse_usage(&json);

    // THE fix: the real numbers, from the response. Only a provider that reported nothing
    // gets an `Estimated` count, and that renders as a guess wherever it is shown.
    let prompt_tokens = usage
        .map(|u| TokenCount::Reported(u.prompt_tokens))
        .or_else(|| timings.map(|t| TokenCount::Reported(t.prompt_n)))
        .or_else(|| Some(TokenCount::Estimated(estimate_tokens(prompt))));
    let completion_tokens = usage
        .map(|u| TokenCount::Reported(u.completion_tokens))
        .or_else(|| timings.map(|t| TokenCount::Reported(t.predicted_n)));

    let tok_per_s = timings
        .map(|t| t.predicted_per_second)
        .filter(|v| v.is_finite() && *v > 0.0);
    // Read, never stopwatched: prompt processing is exactly the wait before the first token.
    let ttft_ms = timings
        .map(|t| t.prompt_ms)
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(|v| v as u32);

    let cost = cost_of(
        &t,
        prompt_tokens.unwrap_or(TokenCount::Estimated(0)),
        completion_tokens.unwrap_or(TokenCount::Estimated(0)),
        tok_per_s,
    );

    CompareRow {
        alias: t.alias,
        backend: t.backend,
        model: t.model,
        ok: status.is_success(),
        ms,
        ttft_ms,
        tok_per_s,
        prompt_tokens,
        completion_tokens,
        cost,
        preview: preview_of(&json),
        error: (!status.is_success()).then(|| format!("HTTP {status}")),
    }
}

/// What this row cost, through `core::pricing` so the honesty rules and the assumption
/// strings are the ones every other money surface uses.
fn cost_of(
    t: &Target,
    prompt: TokenCount,
    completion: TokenCount,
    tps_hint: Option<f32>,
) -> CostEstimate {
    let Some(price) = t.price.clone() else {
        return CostEstimate::Unknown;
    };
    let Some(backend) = t.backend.as_ref() else {
        return CostEstimate::Unknown;
    };
    let Ok(as_provider) = ProviderId::parse(backend.as_str()) else {
        return CostEstimate::Unknown;
    };
    let mut table = PriceTable::default();
    table.set_provider_models(&as_provider, &[(t.model.clone(), price)]);
    table.estimate(backend.as_str(), &t.model, prompt, completion, tps_hint)
}

/// The first `PREVIEW_CHARS` characters of the answer, on a char boundary.
fn preview_of(json: &serde_json::Value) -> String {
    let text = json
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or_default();
    text.chars().take(PREVIEW_CHARS).collect()
}

/// The only fabricated number in this module, used **only** when the provider reported
/// nothing, and returned as [`TokenCount::Estimated`] so it can never render as a fact.
fn estimate_tokens(prompt: &str) -> u32 {
    let words = prompt.split_whitespace().count();
    u32::try_from(words.saturating_mul(4).div_ceil(3)).unwrap_or(u32::MAX)
}

/// Saturating millisecond count.
fn millis(d: Duration) -> u32 {
    u32::try_from(d.as_millis()).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::checks::serve_s07;
    use crate::api::testkit::{app, node_backend, test_config};
    use apexrouter_protocol::JobState;
    use std::time::Duration as StdDuration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Bind `alias` to a backend serving `model` at `base_url`.
    fn bind(state: &Arc<AppState>, alias: &str, id: &str, base_url: &str, model: &str) {
        let cfg = state.cfg();
        state
            .router
            .registry()
            .upsert(node_backend(id, base_url, model), &cfg.router);
        let alias = Alias::parse(alias).expect("alias");
        let id = BackendId::parse(id).expect("id");
        crate::api::bind_alias(state, &alias, &id).expect("bind");
    }

    /// A llama.cpp-shaped answer, with the real `usage` and `timings` a comparison must read.
    fn answer(content: &str, prompt_tokens: u32, completion_tokens: u32) -> serde_json::Value {
        serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": content}}],
            "usage": {"prompt_tokens": prompt_tokens, "completion_tokens": completion_tokens},
            "timings": {
                "prompt_n": prompt_tokens, "prompt_ms": 120.0,
                "predicted_n": completion_tokens, "predicted_ms": 1000.0,
                "predicted_per_second": 9.71
            }
        })
    }

    #[tokio::test]
    async fn the_reported_prompt_tokens_are_used_and_never_a_word_count() {
        let upstream_a = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(answer("alpha", 41, 12)))
            .mount(&upstream_a)
            .await;

        let state = app(test_config());
        bind(&state, "auto", "a", &upstream_a.uri(), "model-a");

        let Json(rows): Json<Vec<CompareRow>> = {
            let res = compare(
                State(Arc::clone(&state)),
                Query(CompareQuery::default()),
                Json(CompareRequest {
                    aliases: vec!["auto".to_owned()],
                    // Six words: a word-count fabrication would produce 8, not 41.
                    prompt: "one two three four five six".to_owned(),
                    max_tokens: Some(16),
                }),
            )
            .await
            .expect("compare");
            let body = axum::body::to_bytes(res.into_body(), 1 << 20)
                .await
                .expect("body");
            Json(serde_json::from_slice(&body).expect("Vec<CompareRow>"))
        };

        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert!(row.ok, "{row:?}");
        assert_eq!(
            row.prompt_tokens,
            Some(TokenCount::Reported(41)),
            "the provider's own number, not word_count * 1.3"
        );
        assert_eq!(row.completion_tokens, Some(TokenCount::Reported(12)));
        assert_eq!(row.tok_per_s, Some(9.71), "read from timings");
        assert_eq!(row.ttft_ms, Some(120), "read from timings.prompt_ms");
        assert_eq!(row.preview, "alpha");
    }

    /// THE acceptance sentence: N aliases run at the same time, not one after another.
    #[tokio::test]
    async fn aliases_run_in_parallel() {
        let slow = StdDuration::from_millis(700);
        let a = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(slow)
                    .set_body_json(answer("a", 3, 3)),
            )
            .mount(&a)
            .await;
        let b = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(slow)
                    .set_body_json(answer("b", 3, 3)),
            )
            .mount(&b)
            .await;
        let c = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(slow)
                    .set_body_json(answer("c", 3, 3)),
            )
            .mount(&c)
            .await;

        let state = app(test_config());
        // Three aliases, three backends. `bind_alias` repoints the alias it is given and
        // leaves every other route alone, so three calls leave three routes.
        for (alias, id, url, model) in [
            ("auto", "a", a.uri(), "m-a"),
            ("beta", "b", b.uri(), "m-b"),
            ("gamma", "c", c.uri(), "m-c"),
        ] {
            bind(&state, alias, id, &url, model);
        }

        let started = Instant::now();
        let targets = plan(
            &state,
            &CompareRequest {
                aliases: vec!["auto".to_owned(), "beta".to_owned(), "gamma".to_owned()],
                prompt: "hello".to_owned(),
                max_tokens: Some(8),
            },
        )
        .expect("plan");
        let rows = run(&state, targets, "hello", 8).await;
        let elapsed = started.elapsed();

        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| r.ok), "{rows:?}");
        assert!(
            elapsed < slow * 2,
            "three 700 ms calls took {elapsed:?}: they ran serially"
        );
    }

    #[tokio::test]
    async fn an_unroutable_alias_is_a_row_not_a_failed_batch() {
        let up = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(answer("ok", 1, 1)))
            .mount(&up)
            .await;

        let state = app(test_config());
        bind(&state, "auto", "a", &up.uri(), "m-a");

        let targets = plan(
            &state,
            &CompareRequest {
                aliases: vec!["auto".to_owned(), "missing".to_owned()],
                prompt: "hi".to_owned(),
                max_tokens: None,
            },
        )
        .expect("plan");
        let rows = run(&state, targets, "hi", 8).await;

        assert_eq!(rows.len(), 2);
        assert!(rows[0].ok);
        assert!(!rows[1].ok);
        assert!(rows[1].error.is_some(), "it says why");
        assert_eq!(rows[1].cost, CostEstimate::Unknown);
    }

    #[tokio::test]
    async fn no_wait_returns_a_job_whose_result_is_the_rows() {
        let up = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(answer("ok", 5, 5)))
            .mount(&up)
            .await;

        let state = app(test_config());
        bind(&state, "auto", "a", &up.uri(), "m-a");
        let base = serve_s07(Arc::clone(&state)).await;
        let http = reqwest::Client::new();

        let res = http
            .post(format!("{base}/v1/compare?no_wait=true"))
            .json(&serde_json::json!({"aliases": ["auto"], "prompt": "hi"}))
            .send()
            .await
            .expect("post");
        assert_eq!(res.status(), 202);
        let job: JobRecord = res.json().await.expect("JobRecord");
        assert_eq!(job.kind, "compare");

        for _ in 0..100 {
            let now: JobRecord = http
                .get(format!("{base}/v1/jobs/{}", job.id))
                .send()
                .await
                .expect("get")
                .json()
                .await
                .expect("JobRecord");
            if now.state == JobState::Succeeded {
                let rows: Vec<CompareRow> =
                    serde_json::from_value(now.result.clone().unwrap_or_default())
                        .expect("the result is the rows");
                assert_eq!(rows.len(), 1);
                assert!(rows[0].ok);
                return;
            }
            assert_ne!(now.state, JobState::Failed, "{:?}", now.error);
            tokio::time::sleep(StdDuration::from_millis(50)).await;
        }
        panic!("the compare job never finished");
    }

    #[tokio::test]
    async fn an_empty_request_is_a_400_that_names_the_field() {
        let state = app(test_config());
        let e = plan(
            &state,
            &CompareRequest {
                aliases: vec![],
                prompt: "hi".to_owned(),
                max_tokens: None,
            },
        )
        .expect_err("no aliases");
        assert_eq!(e.body.param.as_deref(), Some("aliases"));

        let e = plan(
            &state,
            &CompareRequest {
                aliases: vec!["auto".to_owned()],
                prompt: "   ".to_owned(),
                max_tokens: None,
            },
        )
        .expect_err("no prompt");
        assert_eq!(e.body.param.as_deref(), Some("prompt"));
    }

    /// A route table is not needed for this one: it proves the estimate is only ever a
    /// fallback, and is labelled as an estimate when it is used.
    #[test]
    fn a_fabricated_count_is_always_marked_estimated() {
        let n = estimate_tokens("one two three");
        assert!(!TokenCount::Estimated(n).is_reported());
        assert!(n >= 3, "{n}");
    }
}
