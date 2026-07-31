//! OWNER: unit P-08 (providers/src/{checks,smoke,compare}.rs). Do not edit outside that
//! unit.
//!
//! `POST /v1/compare` — one prompt across N aliases.
//!
//! Runs them **in parallel** (LocalRouter ran them serially) and reports the **real**
//! `prompt_tokens` from the response; LocalRouter fabricated `word_count * 1.3` while
//! discarding the number the provider had actually sent.
//!
//! Two consequences worth stating, because they are what makes the numbers usable:
//!
//! * a row whose response carried no `usage` reports `prompt_tokens: None` and
//!   `cost: Unknown`. There is no estimator here; an invented token count is what made
//!   LocalRouter's `usage.log` unauditable.
//! * TTFT and tok/s come from the `timings` object, exactly as in [`crate::smoke`]. A
//!   managed provider that publishes none leaves both `None` rather than being timed with
//!   our own clock across the internet.
//!
//! Local and vLLM endpoints are first-class targets here. LocalRouter's compare screen
//! refused to list them, which meant the one comparison an operator actually wants —
//! "is the rented box worth it against the laptop?" — was the one it could not make.

use crate::smoke::{
    chat, content, first_chars, read_timings, response_model, ChatOutcome, SmokeTarget,
};
use apexrouter_core::pricing::PriceTable;
use apexrouter_core::secret::Secret;
use apexrouter_protocol::{Alias, BackendId, CompareRow, CostEstimate, TokenCount};
use futures_util::stream::{FuturesUnordered, StreamExt};
use serde_json::json;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// How many characters of the answer travel in [`CompareRow::preview`].
const PREVIEW_CHARS: usize = 200;

/// One alias, already resolved to something callable.
///
/// The router owns resolution, so this crate never guesses: `base_url` and `model` are the
/// *resolved* route's, which is the same rule the smoke probes follow.
#[derive(Clone, Debug)]
pub struct CompareTarget {
    /// The alias the operator named.
    pub alias: Alias,
    /// The backend it resolved to, when it resolved to one we track.
    pub backend: Option<BackendId>,
    /// Base URL, with or without a trailing `/v1`.
    pub base_url: String,
    /// The upstream model id to send.
    pub model: String,
    /// Bearer credential, when the upstream wants one.
    pub cred: Option<Secret<String>>,
    /// The pricing key: a provider id (`"together"`) or an instance (`"vast:12345"`).
    /// Empty means "no price is known", which yields [`CostEstimate::Unknown`].
    pub provider: String,
}

impl CompareTarget {
    /// A target with no credential, no backend and no price source.
    pub fn new(
        alias: Alias,
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> CompareTarget {
        CompareTarget {
            alias,
            backend: None,
            base_url: base_url.into(),
            model: model.into(),
            cred: None,
            provider: String::new(),
        }
    }
}

/// The prompt, once, for every target.
#[derive(Clone, Debug)]
pub struct CompareRequest {
    /// What to ask.
    pub prompt: String,
    /// Generation cap. LocalRouter's compare used 200 and it is still the right default.
    pub max_tokens: u32,
    /// Per-target budget.
    pub timeout: Duration,
}

impl CompareRequest {
    /// 200 tokens, 120 seconds.
    pub fn new(prompt: impl Into<String>) -> CompareRequest {
        CompareRequest {
            prompt: prompt.into(),
            max_tokens: 200,
            timeout: Duration::from_secs(120),
        }
    }
}

/// Run one prompt across every target **in parallel**, streaming rows through `tx` as they
/// land and returning them in *target* order.
///
/// Completion order is what the operator watches; target order is what `--json` prints, so
/// a comparison is diffable between runs. This is the same split [`Registry::run`] makes
/// for checks.
///
/// `prices` is the daemon's live [`PriceTable`]; without one every row costs
/// [`CostEstimate::Unknown`], which is the honest answer rather than a zero. A closed
/// receiver is not an error.
///
/// [`Registry::run`]: apexrouter_core::checks::Registry::run
pub async fn compare(
    http: &reqwest::Client,
    targets: &[CompareTarget],
    req: &CompareRequest,
    prices: Option<&PriceTable>,
    tx: mpsc::Sender<CompareRow>,
) -> Vec<CompareRow> {
    let mut slots: Vec<Option<CompareRow>> = (0..targets.len()).map(|_| None).collect();

    let mut running = FuturesUnordered::new();
    for (slot, target) in targets.iter().enumerate() {
        let tx = tx.clone();
        running.push(async move {
            let row = one(http, target, req, prices).await;
            // A gone receiver is normal: `apexrouter compare --json` attaches none.
            let _ = tx.send(row.clone()).await;
            (slot, row)
        });
    }
    while let Some((slot, row)) = running.next().await {
        slots[slot] = Some(row);
    }
    slots.into_iter().flatten().collect()
}

/// One alias, one prompt, one row. Never `Err`: a dead alias is a row with `ok: false`.
pub async fn one(
    http: &reqwest::Client,
    target: &CompareTarget,
    req: &CompareRequest,
    prices: Option<&PriceTable>,
) -> CompareRow {
    let started = Instant::now();
    let smoke = SmokeTarget {
        base_url: target.base_url.clone(),
        model: target.model.clone(),
        cred: target.cred.clone(),
        timeout: req.timeout,
    };
    let body = json!({
        "model": target.model,
        "messages": [{"role": "user", "content": req.prompt}],
        "max_tokens": req.max_tokens,
        "stream": false,
    });
    let outcome: ChatOutcome = chat(http, &smoke, &body).await;
    let ms = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);

    let mut row = CompareRow {
        alias: target.alias.clone(),
        backend: target.backend.clone(),
        model: target.model.clone(),
        ok: false,
        ms,
        ttft_ms: None,
        tok_per_s: None,
        prompt_tokens: None,
        completion_tokens: None,
        cost: CostEstimate::Unknown,
        preview: String::new(),
        error: outcome.error.clone(),
    };

    let Some(v) = outcome.body.as_ref() else {
        return row;
    };
    if let Some(m) = response_model(v) {
        // The provider's own spelling of the id beats the one we sent.
        row.model = m;
    }
    if outcome.error.is_some() {
        // A JSON error body: keep what it told us, but the row still failed.
        return row;
    }

    row.ok = true;
    let (ttft_ms, tok_per_s, tokens) = read_timings(v);
    row.ttft_ms = ttft_ms;
    row.tok_per_s = tok_per_s;
    row.preview = first_chars(&content(v).unwrap_or_default(), PREVIEW_CHARS);

    // The **real** numbers, or none at all. `word_count * 1.3` appears nowhere.
    if let Some(usage) = apexrouter_core::upstream::parse_usage(v) {
        row.prompt_tokens = Some(TokenCount::Reported(usage.prompt_tokens));
        row.completion_tokens = Some(TokenCount::Reported(usage.completion_tokens));
    } else if let Some(n) = tokens {
        // llama.cpp with `usage` suppressed still counts what it predicted; the prompt side
        // stays unknown rather than being back-derived.
        row.completion_tokens = Some(TokenCount::Reported(n));
    }

    row.cost = match (prices, row.prompt_tokens, row.completion_tokens) {
        (Some(table), Some(prompt), Some(completion)) if !target.provider.is_empty() => {
            table.estimate(&target.provider, &row.model, prompt, completion, tok_per_s)
        }
        _ => CostEstimate::Unknown,
    };
    row
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_protocol::{Money, PriceModel, PriceSource, ProviderId};
    use serde_json::Value;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client() -> reqwest::Client {
        reqwest::Client::new()
    }

    fn alias(s: &str) -> Alias {
        Alias::parse(s).expect("alias")
    }

    fn req() -> CompareRequest {
        CompareRequest {
            prompt: "Explain a B-tree.".to_owned(),
            max_tokens: 200,
            timeout: Duration::from_secs(5),
        }
    }

    fn completion(text: &str, prompt_tokens: u32, completion_tokens: u32) -> Value {
        json!({
            "model": "Carnice-9b-Q6_K",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": text}}],
            "usage": {"prompt_tokens": prompt_tokens, "completion_tokens": completion_tokens},
            "timings": {"prompt_n": prompt_tokens, "prompt_ms": 250.0,
                        "predicted_n": completion_tokens, "predicted_ms": 20000.0,
                        "predicted_per_second": 9.71}
        })
    }

    /// A server that sleeps `delay_ms` before answering.
    async fn slow_server(delay_ms: u64, text: &'static str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(delay_ms))
                    .set_body_json(completion(text, 41, 200)),
            )
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn the_aliases_run_in_parallel_not_serially() {
        // Three 300 ms upstreams: serial is ≥ 900 ms, parallel is ~300 ms.
        let a = slow_server(300, "answer a").await;
        let b = slow_server(300, "answer b").await;
        let c = slow_server(300, "answer c").await;
        let targets = vec![
            CompareTarget::new(alias("a"), a.uri(), "m"),
            CompareTarget::new(alias("b"), b.uri(), "m"),
            CompareTarget::new(alias("c"), c.uri(), "m"),
        ];

        let started = Instant::now();
        let (tx, _rx) = mpsc::channel(8);
        let rows = compare(&client(), &targets, &req(), None, tx).await;
        let elapsed = started.elapsed();

        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| r.ok), "{rows:#?}");
        assert!(
            elapsed < Duration::from_millis(800),
            "three 300 ms aliases took {elapsed:?} — that is serial"
        );
    }

    #[tokio::test]
    async fn rows_come_back_in_target_order_however_they_finish() {
        let slow = slow_server(250, "slow").await;
        let fast = slow_server(0, "fast").await;
        let targets = vec![
            CompareTarget::new(alias("slow"), slow.uri(), "m"),
            CompareTarget::new(alias("fast"), fast.uri(), "m"),
        ];

        let (tx, mut rx) = mpsc::channel(8);
        let rows = compare(&client(), &targets, &req(), None, tx).await;
        assert_eq!(rows[0].alias, alias("slow"));
        assert_eq!(rows[1].alias, alias("fast"));

        // …while the stream is in completion order, which is the point of streaming.
        let mut streamed = Vec::new();
        while let Ok(r) = rx.try_recv() {
            streamed.push(r.alias);
        }
        assert_eq!(streamed, vec![alias("fast"), alias("slow")]);
    }

    #[tokio::test]
    async fn prompt_tokens_are_the_reported_number_never_a_word_count_guess() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(completion("hello", 237, 35)))
            .mount(&server)
            .await;

        let t = CompareTarget::new(alias("local"), server.uri(), "m");
        let row = one(&client(), &t, &req(), None).await;
        // `"Explain a B-tree."` is 3 words; LocalRouter would have reported 3.9 ≈ 3.
        assert_eq!(row.prompt_tokens, Some(TokenCount::Reported(237)));
        assert_eq!(row.completion_tokens, Some(TokenCount::Reported(35)));
        assert!(row.prompt_tokens.is_some_and(|t| t.is_reported()));
        assert_eq!(row.ttft_ms, Some(250));
        assert_eq!(row.tok_per_s, Some(9.71));
    }

    #[tokio::test]
    async fn a_response_without_usage_reports_no_prompt_tokens_and_no_cost() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "model": "m",
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}}]
            })))
            .mount(&server)
            .await;

        let t = CompareTarget::new(alias("bare"), server.uri(), "m");
        let row = one(&client(), &t, &req(), None).await;
        assert!(row.ok);
        assert_eq!(row.prompt_tokens, None, "never invented");
        assert_eq!(row.cost, CostEstimate::Unknown);
    }

    #[tokio::test]
    async fn cost_comes_from_the_price_table_when_there_is_one() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(completion("hello", 1_000_000, 1_000_000)),
            )
            .mount(&server)
            .await;

        let mut prices = PriceTable::default();
        prices.set_provider_models(
            &ProviderId::parse("together").expect("id"),
            &[(
                "Carnice-9b-Q6_K".to_owned(),
                PriceModel::PerToken {
                    input: Money::from_usd(0.18),
                    output: Money::from_usd(0.59),
                },
            )],
        );

        let mut t = CompareTarget::new(alias("together"), server.uri(), "Carnice-9b-Q6_K");
        t.provider = "together".to_owned();
        let row = one(&client(), &t, &req(), Some(&prices)).await;
        assert_eq!(
            row.cost,
            CostEstimate::Metered {
                usd: Money::from_usd(0.77),
                source: PriceSource::ProviderApi,
            }
        );
    }

    #[tokio::test]
    async fn a_dead_alias_is_a_row_not_an_error() {
        let ok = slow_server(0, "alive").await;
        let targets = vec![
            CompareTarget::new(alias("dead"), "http://127.0.0.1:1", "m"),
            CompareTarget::new(alias("alive"), ok.uri(), "m"),
        ];
        let (tx, _rx) = mpsc::channel(8);
        let rows = compare(&client(), &targets, &req(), None, tx).await;

        assert_eq!(rows.len(), 2);
        assert!(!rows[0].ok);
        assert!(rows[0].error.is_some());
        assert!(rows[1].ok, "one dead alias must not sink the comparison");
    }

    #[tokio::test]
    async fn a_400_keeps_the_body_as_the_error_and_the_row_fails() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": {"message": "Unable to access model x"}
            })))
            .mount(&server)
            .await;

        let t = CompareTarget::new(alias("managed"), server.uri(), "m");
        let row = one(&client(), &t, &req(), None).await;
        assert!(!row.ok);
        assert!(row
            .error
            .as_deref()
            .is_some_and(|e| e.contains("Unable to access model")));
        assert_eq!(row.cost, CostEstimate::Unknown);
    }

    #[tokio::test]
    async fn the_preview_is_two_hundred_characters_of_the_answer() {
        let long = "x".repeat(500);
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(completion(&long, 5, 500)))
            .mount(&server)
            .await;

        let t = CompareTarget::new(alias("verbose"), server.uri(), "m");
        let row = one(&client(), &t, &req(), None).await;
        assert_eq!(
            row.preview.chars().count(),
            PREVIEW_CHARS + 1,
            "200 + ellipsis"
        );
    }
}
