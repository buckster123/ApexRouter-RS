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
use serde::Deserialize;
use serde_json::Value;
use std::time::{Duration, Instant};

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

/// Statuses that mean "this build did not enable that endpoint", not "this upstream is broken".
///
/// llama.cpp answers **501** on `/slots` when started with `--no-slots`, and older builds —
/// plus every non-llama.cpp upstream — simply 404 both `/props` and `/slots`. A 403 or 405 is
/// what a reverse proxy in front of a rented box tends to say about the same thing.
const ENDPOINT_ABSENT: [u16; 4] = [403, 404, 405, 501];

/// Probe one upstream. Never panics, never returns `Err`: an unreachable upstream is a
/// finding, not an exception.
///
/// `timeout` is the budget for the **whole** probe, not for each request: `/health` is asked
/// first (it is the readiness gate, and the only answer that is load-bearing), then
/// `/v1/models`, `/props` and `/slots` are asked concurrently with whatever budget is left.
/// Running out of budget leaves the corresponding fields `None`; that is not an error.
///
/// Outcomes, in the order they are decided:
///
/// * transport failure (connection refused, DNS, TLS, timeout) — `error` set, `status`
///   `None`, and **both** `healthy` and `loading` false. This is the "not there" case the
///   supervisor's health deadline must not mistake for progress.
/// * `/health` 200 — `healthy`.
/// * `/health` 503 whose body says it is loading a model — `loading`, and nothing else is
///   asked, because nothing else will answer usefully yet.
/// * `/health` 404/405/501 — the upstream has no health endpoint (Together has none); a
///   successful `/v1/models` then stands in for it.
/// * any other status — `error` set, `healthy` false.
pub async fn probe(
    http: &reqwest::Client,
    base_url: &str,
    cred: Option<&Secret<String>>,
    timeout: Duration,
) -> UpstreamProbe {
    let started = Instant::now();
    let mut out = UpstreamProbe::default();

    // ---- the readiness gate -------------------------------------------------------------
    let mut health_absent = false;
    match get(http, &join_v1(base_url, "/health"), cred, timeout).await {
        Err(e) => {
            out.error = Some(e);
            out.ms = elapsed_ms(started);
            return out;
        }
        Ok(res) => {
            out.status = Some(res.status);
            if res.status == 200 {
                out.healthy = true;
            } else if res.status == 503 && says_loading(&res) {
                out.loading = true;
                out.ms = elapsed_ms(started);
                return out;
            } else if ENDPOINT_ABSENT.contains(&res.status) {
                health_absent = true;
            } else {
                out.error = Some(format!("GET /health -> {}", res.status));
                out.ms = elapsed_ms(started);
                return out;
            }
        }
    }

    // ---- the informative endpoints, concurrently ----------------------------------------
    let left = match timeout.checked_sub(started.elapsed()) {
        Some(d) if !d.is_zero() => d,
        _ => {
            out.ms = elapsed_ms(started);
            return out;
        }
    };
    let (models_url, props_url, slots_url) = (
        join_v1(base_url, "/v1/models"),
        join_v1(base_url, "/props"),
        join_v1(base_url, "/slots"),
    );
    let (models, props, slots) = tokio::join!(
        get(http, &models_url, cred, left),
        get(http, &props_url, cred, left),
        get(http, &slots_url, cred, left),
    );

    // `/v1/models` — the one endpoint every OpenAI-compatible upstream really has.
    match models {
        Ok(res) if res.status == 200 => {
            if health_absent {
                out.healthy = true;
                out.status = Some(res.status);
            }
            out.models = parse_models(res.json.as_ref());
        }
        Ok(res) => note_error(&mut out, format!("GET /v1/models -> {}", res.status)),
        Err(e) => note_error(&mut out, format!("GET /v1/models: {e}")),
    }

    // `/props` — off by default on llama.cpp, absent on every other vendor.
    if let Ok(res) = props {
        if res.status == 200 {
            if let Some(v) = res.json.as_ref() {
                apply_props(&mut out, v);
            }
        } else if !ENDPOINT_ABSENT.contains(&res.status) {
            note_error(&mut out, format!("GET /props -> {}", res.status));
        }
    }

    // `/slots` — **501** on `--no-slots` builds. Also the fallback source of `slots_total`.
    if let Ok(res) = slots {
        if res.status == 200 {
            if let Some(v) = res.json.as_ref() {
                apply_slots(&mut out, v);
            }
        } else if !ENDPOINT_ABSENT.contains(&res.status) {
            note_error(&mut out, format!("GET /slots -> {}", res.status));
        }
    }

    out.ms = elapsed_ms(started);
    out
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
///
/// Accepts either a whole body (`{"choices":[…],"timings":{…}}` — llama.cpp puts `timings` on
/// `/v1/chat/completions` responses too, and with `"timings_per_token": true` on every
/// streamed chunk) or the `timings` object on its own.
///
/// Returns `None` for `/slots/{id}?action=save|restore`, whose response *also* has a
/// `timings` key but holds `{"save_ms": …}` / `{"restore_ms": …}` — a different object that
/// must never be read through this struct. `predicted_per_second` is taken from the upstream
/// when it is there and derived from `predicted_n / predicted_ms` when it is not; it is never
/// stopwatched by us.
pub fn parse_timings(v: &serde_json::Value) -> Option<Timings> {
    let t = v.get("timings").unwrap_or(v);
    let obj = t.as_object()?;
    // The save/restore object shares the key and shares nothing else.
    const OURS: [&str; 5] = [
        "prompt_n",
        "predicted_n",
        "predicted_per_second",
        "prompt_ms",
        "predicted_ms",
    ];
    if !OURS.iter().any(|k| obj.contains_key(*k)) {
        return None;
    }
    let predicted_n = u32_at(t, "predicted_n").unwrap_or(0);
    let predicted_ms = f32_at(t, "predicted_ms").unwrap_or(0.0);
    let predicted_per_second = match f32_at(t, "predicted_per_second") {
        Some(v) => v,
        None if predicted_ms > 0.0 => predicted_n as f32 * 1000.0 / predicted_ms,
        None => 0.0,
    };
    Some(Timings {
        cache_n: u32_at(t, "cache_n").unwrap_or(0),
        prompt_n: u32_at(t, "prompt_n").unwrap_or(0),
        prompt_ms: f32_at(t, "prompt_ms").unwrap_or(0.0),
        predicted_n,
        predicted_ms,
        predicted_per_second,
    })
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
///
/// Accepts a whole body or the `usage` object alone. A `"usage": null` — what every
/// non-final streamed chunk carries — is `None`, not a zeroed record, so the tee never
/// reports a real request as having used nothing.
pub fn parse_usage(v: &serde_json::Value) -> Option<UsageFields> {
    let u = match v.get("usage") {
        Some(Value::Null) => return None,
        Some(u) => u,
        None => v,
    };
    let obj = u.as_object()?;
    if !obj.contains_key("prompt_tokens") && !obj.contains_key("completion_tokens") {
        return None;
    }
    let cached_tokens = u
        .get("prompt_tokens_details")
        .and_then(|d| u32_at(d, "cached_tokens"))
        .or_else(|| u32_at(u, "cached_tokens"));
    Some(UsageFields {
        prompt_tokens: u32_at(u, "prompt_tokens").unwrap_or(0),
        completion_tokens: u32_at(u, "completion_tokens").unwrap_or(0),
        cached_tokens,
    })
}

/// Join a segment onto a base URL that is stored **without** a trailing `/v1`.
///
/// This is the highest-risk drop-in bug in the whole project: both
/// `http://127.0.0.1:8888` and `http://127.0.0.1:8888/v1` must work as client base URLs,
/// because `smoke.sh` appends `/v1` to whatever you give it.
///
/// So: every trailing `/v1` (and every trailing `/`) comes off the base, a repeated leading
/// `v1/` is collapsed in the segment, and the two are joined with exactly one `/`. The
/// segment is passed **whole** rather than prefixed, because `/health`, `/props` and
/// `/slots` do not live under `/v1`:
///
/// * `join_v1("http://h:8888/v1/", "/v1/models") == "http://h:8888/v1/models"`
/// * `join_v1("http://h:8888", "/health") == "http://h:8888/health"`
pub fn join_v1(base_url: &str, segment: &str) -> String {
    let mut base = base_url.trim().trim_end_matches('/');
    while let Some(stripped) = base.strip_suffix("/v1") {
        base = stripped.trim_end_matches('/');
    }
    let mut seg = segment.trim().trim_start_matches('/');
    while let Some(rest) = seg.strip_prefix("v1/") {
        if !rest.starts_with("v1/") {
            break;
        }
        seg = rest;
    }
    if seg.is_empty() {
        base.to_string()
    } else {
        format!("{base}/{seg}")
    }
}

// ----------------------------------------------------------------------------------------
// internals
// ----------------------------------------------------------------------------------------

/// One answered request: the status, and the body parsed as JSON when it was JSON.
struct Res {
    status: u16,
    text: String,
    json: Option<Value>,
}

/// `GET url`, with the bearer token when there is one. `Err` is a transport failure only —
/// every status code, 500 included, comes back as `Ok`.
async fn get(
    http: &reqwest::Client,
    url: &str,
    cred: Option<&Secret<String>>,
    timeout: Duration,
) -> Result<Res, String> {
    let mut rb = http.get(url).timeout(timeout).header("accept", "*/*");
    if let Some(c) = cred {
        rb = rb.bearer_auth(c.expose());
    }
    let resp = rb.send().await.map_err(|e| redact(&e.to_string()))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let json = serde_json::from_str::<Value>(&text).ok();
    Ok(Res { status, text, json })
}

/// A 503 that says a model is loading, in any of the shapes llama.cpp has used:
/// `{"status":"loading model"}`, `{"error":{"message":"Loading model", …}}`, and the bare
/// text a proxy may pass through.
fn says_loading(res: &Res) -> bool {
    let says = |s: &str| s.to_ascii_lowercase().contains("loading");
    if let Some(v) = res.json.as_ref() {
        let fields = [
            v.get("status"),
            v.get("message"),
            v.get("error").and_then(|e| e.get("message")),
            v.get("error").and_then(|e| e.get("type")),
        ];
        return fields
            .iter()
            .filter_map(|f| f.and_then(Value::as_str))
            .any(says)
            || v.as_str().is_some_and(says);
    }
    says(&res.text)
}

/// Record a non-fatal problem without overwriting the first, more proximate one.
fn note_error(out: &mut UpstreamProbe, msg: String) {
    if out.error.is_none() {
        out.error = Some(msg);
    }
}

/// Milliseconds since `started`, saturating rather than wrapping.
fn elapsed_ms(started: Instant) -> u32 {
    u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX)
}

/// Keep a credential out of an error string even if a URL somehow carried one.
fn redact(msg: &str) -> String {
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

/// A count that upstreams write sometimes as a number and sometimes as a string.
fn u32_at(v: &Value, key: &str) -> Option<u32> {
    let f = v.get(key)?;
    f.as_u64()
        .or_else(|| f.as_str().and_then(|s| s.parse::<u64>().ok()))
        .or_else(|| f.as_f64().filter(|n| *n >= 0.0).map(|n| n as u64))
        .and_then(|n| u32::try_from(n).ok())
}

/// The same, for the timing floats.
fn f32_at(v: &Value, key: &str) -> Option<f32> {
    let f = v.get(key)?;
    f.as_f64()
        .or_else(|| f.as_str().and_then(|s| s.parse::<f64>().ok()))
        .map(|n| n as f32)
}

/// A row of `/v1/models`, in the union of the shapes we have actually seen.
#[derive(Debug, Deserialize)]
struct ModelRow {
    id: String,
    /// Together: the real context length. `null` for some model types.
    #[serde(default)]
    context_length: Option<u32>,
    /// llama.cpp: `null` while the model is still loading, so `Option` all the way down.
    #[serde(default)]
    meta: Option<ModelMeta>,
    /// Some OpenAI-compatible servers advertise capabilities inline.
    #[serde(default)]
    capabilities: Option<ModelCaps>,
}

/// llama.cpp's per-model `meta` block, abridged to the one field we use.
#[derive(Debug, Deserialize)]
struct ModelMeta {
    #[serde(default)]
    n_ctx_train: Option<u32>,
}

/// An inline capability advertisement, when there is one.
#[derive(Debug, Default, Deserialize)]
struct ModelCaps {
    #[serde(default)]
    vision: bool,
    #[serde(default)]
    tools: bool,
}

/// The llama.cpp / OpenAI envelope. **Deserialiser one of two.**
#[derive(Debug, Deserialize)]
struct ModelEnvelope {
    #[serde(default)]
    data: Vec<ModelRow>,
}

/// `/v1/models`, in both shapes: llama.cpp's `{"object":"list","data":[…]}` envelope and
/// Together's **bare array**. Two deserialisers, one function — a single shared one does not
/// work, and pretending otherwise is how the Together provider breaks.
fn parse_models(v: Option<&Value>) -> Vec<UpstreamModel> {
    let Some(v) = v else {
        return Vec::new();
    };
    let rows: Vec<ModelRow> = if v.is_array() {
        // Deserialiser two of two: Together.
        serde_json::from_value(v.clone()).unwrap_or_default()
    } else {
        serde_json::from_value::<ModelEnvelope>(v.clone())
            .map(|e| e.data)
            .unwrap_or_default()
    };
    rows.into_iter()
        .map(|r| {
            let caps = r.capabilities.unwrap_or_default();
            UpstreamModel {
                id: r.id,
                ctx: r
                    .context_length
                    .or_else(|| r.meta.and_then(|m| m.n_ctx_train)),
                vision: caps.vision,
                tools: caps.tools,
            }
        })
        .collect()
}

/// Fold a `/props` body into the probe: `total_slots`, the **per-slot** context (which is
/// what `default_generation_settings.n_ctx` is — total ÷ slots — and so the number one
/// request may actually use), `build_info`, `model_path`, and the modality/template
/// capabilities, which are server-global and therefore apply to every advertised model.
fn apply_props(out: &mut UpstreamProbe, v: &Value) {
    if let Some(n) = u32_at(v, "total_slots") {
        out.slots_total = Some(n);
    }
    out.ctx = v
        .get("default_generation_settings")
        .and_then(|g| u32_at(g, "n_ctx"))
        .or_else(|| u32_at(v, "n_ctx"))
        .or(out.ctx);
    if let Some(s) = v.get("build_info").and_then(Value::as_str) {
        out.build_info = Some(s.to_string());
    }
    if let Some(s) = v.get("model_path").and_then(Value::as_str) {
        out.model_path = Some(s.to_string());
    }
    let vision = v
        .get("modalities")
        .and_then(|m| m.get("vision"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let caps = v.get("chat_template_caps");
    let tools = caps
        .and_then(|c| c.get("supports_tools").or_else(|| c.get("tools")))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    for m in out.models.iter_mut() {
        m.vision |= vision;
        m.tools |= tools;
        if m.ctx.is_none() {
            m.ctx = out.ctx;
        }
    }
}

/// Fold a `/slots` body — a **bare array** of slot objects — into the probe. Nothing from
/// the `prompt` field is ever kept: `/slots` echoes prompts, which is exactly why it is
/// never proxied outward.
fn apply_slots(out: &mut UpstreamProbe, v: &Value) {
    let Some(slots) = v.as_array() else {
        return;
    };
    let busy = slots
        .iter()
        .filter(|s| {
            s.get("is_processing")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .count();
    out.slots_busy = Some(u32::try_from(busy).unwrap_or(u32::MAX));
    if out.slots_total.is_none() {
        out.slots_total = Some(u32::try_from(slots.len()).unwrap_or(u32::MAX));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const T: Duration = Duration::from_secs(5);

    fn client() -> reqwest::Client {
        reqwest::Client::builder().build().expect("client")
    }

    async fn mock_health(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "ok"})))
            .mount(server)
            .await;
    }

    async fn mock_models(server: &MockServer, body: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    // ---- join_v1 ------------------------------------------------------------------------

    #[test]
    fn join_v1_accepts_both_base_forms() {
        for base in [
            "http://127.0.0.1:8888",
            "http://127.0.0.1:8888/",
            "http://127.0.0.1:8888/v1",
            "http://127.0.0.1:8888/v1/",
            "http://127.0.0.1:8888/v1/v1",
        ] {
            assert_eq!(
                join_v1(base, "/v1/chat/completions"),
                "http://127.0.0.1:8888/v1/chat/completions",
                "base {base}"
            );
            assert_eq!(
                join_v1(base, "/health"),
                "http://127.0.0.1:8888/health",
                "base {base}"
            );
        }
    }

    #[test]
    fn join_v1_collapses_a_doubled_segment_and_keeps_other_path_prefixes() {
        assert_eq!(
            join_v1("http://h:8888", "v1/v1/models"),
            "http://h:8888/v1/models"
        );
        assert_eq!(join_v1("http://h:8888", ""), "http://h:8888");
        assert_eq!(
            join_v1("https://api.together.ai/v1", "/v1/models"),
            "https://api.together.ai/v1/models"
        );
        // A path prefix that is not `/v1` survives untouched.
        assert_eq!(
            join_v1("https://gw.example/box7/v1/", "/props"),
            "https://gw.example/box7/props"
        );
    }

    // ---- the readiness gate -------------------------------------------------------------

    #[tokio::test]
    async fn health_200_is_healthy() {
        let server = MockServer::start().await;
        mock_health(&server).await;
        mock_models(&server, json!({"object": "list", "data": []})).await;
        let p = probe(&client(), &server.uri(), None, T).await;
        assert!(p.healthy);
        assert!(!p.loading);
        assert_eq!(p.status, Some(200));
        assert_eq!(p.error, None);
    }

    #[tokio::test]
    async fn health_503_loading_model_is_loading_not_dead() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(
                ResponseTemplate::new(503).set_body_json(json!({"status": "loading model"})),
            )
            .mount(&server)
            .await;
        let p = probe(&client(), &server.uri(), None, T).await;
        assert!(p.loading, "503 loading model must set loading");
        assert!(!p.healthy);
        assert_eq!(p.status, Some(503));
    }

    #[tokio::test]
    async fn health_503_in_the_openai_error_shape_is_also_loading() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(ResponseTemplate::new(503).set_body_json(json!({
                "error": {"code": 503, "message": "Loading model", "type": "unavailable_error"}
            })))
            .mount(&server)
            .await;
        let p = probe(&client(), &server.uri(), None, T).await;
        assert!(p.loading);
        assert!(!p.healthy);
    }

    #[tokio::test]
    async fn connection_refused_is_distinguishable_from_loading() {
        // Port 1 on loopback: nothing listens there, and nothing unprivileged may bind it.
        let p = probe(&client(), "http://127.0.0.1:1", None, T).await;
        assert!(!p.healthy);
        assert!(
            !p.loading,
            "refused is NOT loading — the health deadline must not reset on it"
        );
        assert_eq!(p.status, None);
        assert!(p.error.is_some());
    }

    #[tokio::test]
    async fn a_503_that_is_not_about_loading_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(ResponseTemplate::new(503).set_body_json(json!({"status": "overloaded"})))
            .mount(&server)
            .await;
        let p = probe(&client(), &server.uri(), None, T).await;
        assert!(!p.healthy);
        assert!(!p.loading);
        assert_eq!(p.status, Some(503));
        assert!(p.error.is_some());
    }

    // ---- /v1/models, both shapes --------------------------------------------------------

    #[tokio::test]
    async fn models_envelope_shape_llamacpp() {
        let server = MockServer::start().await;
        mock_health(&server).await;
        mock_models(
            &server,
            json!({"object": "list", "data": [{
                "id": "carnice-9b", "object": "model", "created": 1735142223,
                "owned_by": "llamacpp",
                "meta": {"n_vocab": 128256, "n_ctx_train": 131072}
            }]}),
        )
        .await;
        let p = probe(&client(), &server.uri(), None, T).await;
        assert_eq!(p.models.len(), 1);
        assert_eq!(p.models[0].id, "carnice-9b");
        assert_eq!(p.models[0].ctx, Some(131_072));
    }

    #[tokio::test]
    async fn models_bare_array_shape_together() {
        let server = MockServer::start().await;
        mock_health(&server).await;
        mock_models(
            &server,
            json!([
                {"id": "Austism/chronos-hermes-13b", "object": "model", "type": "chat",
                 "context_length": 2048},
                {"id": "meta-llama/Llama-3.1-8B-Instruct-Turbo", "object": "model",
                 "type": "chat", "context_length": null}
            ]),
        )
        .await;
        let p = probe(&client(), &server.uri(), None, T).await;
        assert_eq!(p.models.len(), 2);
        assert_eq!(p.models[0].id, "Austism/chronos-hermes-13b");
        assert_eq!(p.models[0].ctx, Some(2048));
        assert_eq!(p.models[1].ctx, None);
    }

    #[tokio::test]
    async fn meta_null_while_loading_is_not_a_parse_failure() {
        let server = MockServer::start().await;
        mock_health(&server).await;
        mock_models(
            &server,
            json!({"object": "list", "data": [{"id": "m", "meta": null}]}),
        )
        .await;
        let p = probe(&client(), &server.uri(), None, T).await;
        assert_eq!(p.models.len(), 1);
        assert_eq!(p.models[0].ctx, None);
    }

    #[tokio::test]
    async fn an_upstream_without_health_is_judged_by_its_model_list() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        mock_models(&server, json!([{"id": "x"}])).await;
        let p = probe(&client(), &server.uri(), None, T).await;
        assert!(p.healthy);
        assert_eq!(p.error, None);
        assert_eq!(p.models.len(), 1);
    }

    // ---- /props and /slots --------------------------------------------------------------

    #[tokio::test]
    async fn props_populates_ctx_build_info_model_path_and_slots() {
        let server = MockServer::start().await;
        mock_health(&server).await;
        mock_models(
            &server,
            json!({"object": "list", "data": [{"id": "carnice"}]}),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/props"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "default_generation_settings": {"id": 0, "n_ctx": 8192},
                "total_slots": 4,
                "model_path": "/home/andre/models/carnice-9b/Carnice-9b-Q6_K.gguf",
                "modalities": {"vision": false},
                "chat_template_caps": {"supports_tools": true},
                "build_info": "b9199 (39cf5d619)"
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/slots"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"id": 0, "is_processing": true,  "prompt": "a private prompt"},
                {"id": 1, "is_processing": false, "prompt": ""},
                {"id": 2, "is_processing": false, "prompt": ""},
                {"id": 3, "is_processing": false, "prompt": ""}
            ])))
            .mount(&server)
            .await;
        let p = probe(&client(), &server.uri(), None, T).await;
        assert_eq!(p.ctx, Some(8192));
        assert_eq!(p.build_info.as_deref(), Some("b9199 (39cf5d619)"));
        assert_eq!(
            p.model_path.as_deref(),
            Some("/home/andre/models/carnice-9b/Carnice-9b-Q6_K.gguf")
        );
        assert_eq!(p.slots_total, Some(4));
        assert_eq!(p.slots_busy, Some(1));
        assert_eq!(p.models[0].ctx, Some(8192));
        assert!(p.models[0].tools);
        assert!(!p.models[0].vision);
        assert_eq!(p.error, None);
    }

    #[tokio::test]
    async fn props_404_and_slots_501_are_not_errors() {
        let server = MockServer::start().await;
        mock_health(&server).await;
        mock_models(&server, json!({"object": "list", "data": [{"id": "m"}]})).await;
        Mock::given(method("GET"))
            .and(path("/props"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/slots"))
            .respond_with(ResponseTemplate::new(501).set_body_string("slots endpoint is disabled"))
            .mount(&server)
            .await;
        let p = probe(&client(), &server.uri(), None, T).await;
        assert!(p.healthy);
        assert_eq!(p.error, None, "404 /props and 501 /slots are normal");
        assert_eq!(p.ctx, None);
        assert_eq!(p.slots_total, None);
        assert_eq!(p.slots_busy, None);
    }

    #[tokio::test]
    async fn slots_length_is_the_fallback_slot_count() {
        let server = MockServer::start().await;
        mock_health(&server).await;
        mock_models(&server, json!({"object": "list", "data": []})).await;
        Mock::given(method("GET"))
            .and(path("/props"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/slots"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"id": 0, "is_processing": true},
                {"id": 1, "is_processing": true}
            ])))
            .mount(&server)
            .await;
        let p = probe(&client(), &server.uri(), None, T).await;
        assert_eq!(p.slots_total, Some(2));
        assert_eq!(p.slots_busy, Some(2));
    }

    // ---- transport ----------------------------------------------------------------------

    #[tokio::test]
    async fn the_credential_travels_as_a_bearer_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health"))
            .and(header("authorization", "Bearer sk-test-123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "ok"})))
            .mount(&server)
            .await;
        mock_models(&server, json!({"object": "list", "data": []})).await;
        let cred = Secret::new("sk-test-123".to_string());
        let p = probe(&client(), &server.uri(), Some(&cred), T).await;
        assert!(p.healthy, "the mock only matches when the bearer is sent");
    }

    #[tokio::test]
    async fn a_base_url_with_a_trailing_v1_still_probes() {
        let server = MockServer::start().await;
        mock_health(&server).await;
        mock_models(&server, json!({"object": "list", "data": [{"id": "m"}]})).await;
        let p = probe(&client(), &format!("{}/v1", server.uri()), None, T).await;
        assert!(p.healthy);
        assert_eq!(p.models.len(), 1);
    }

    // ---- timings ------------------------------------------------------------------------

    #[test]
    fn timings_are_read_off_a_chat_completion_body() {
        let body = json!({
            "choices": [],
            "usage": {"prompt_tokens": 237, "completion_tokens": 35, "total_tokens": 272},
            "timings": {
                "cache_n": 236, "prompt_n": 1, "prompt_ms": 30.958,
                "prompt_per_token_ms": 30.958, "prompt_per_second": 32.301828283480845,
                "predicted_n": 35, "predicted_ms": 661.064,
                "predicted_per_token_ms": 18.887542857142858,
                "predicted_per_second": 52.94494935437416
            }
        });
        let t = parse_timings(&body).expect("timings");
        assert_eq!(t.cache_n, 236);
        assert_eq!(t.prompt_n, 1);
        assert_eq!(t.predicted_n, 35);
        assert!((t.predicted_per_second - 52.944_95).abs() < 0.001);
        // The supported way to compute live context usage.
        assert_eq!(t.prompt_n + t.cache_n + t.predicted_n, 272);
    }

    #[test]
    fn the_slot_save_timings_object_is_not_our_timings_object() {
        assert_eq!(
            parse_timings(&json!({"id_slot": 0, "timings": {"save_ms": 49.865}})),
            None
        );
        assert_eq!(
            parse_timings(&json!({"id_slot": 0, "timings": {"restore_ms": 42.937}})),
            None
        );
        assert_eq!(parse_timings(&json!({"choices": []})), None);
    }

    #[test]
    fn timings_may_be_passed_bare_and_tok_per_second_is_derived_when_absent() {
        let t = parse_timings(&json!({"predicted_n": 100, "predicted_ms": 25000.0}))
            .expect("bare timings");
        assert_eq!(t.predicted_n, 100);
        assert!((t.predicted_per_second - 4.0).abs() < 0.0001);
    }

    // ---- usage --------------------------------------------------------------------------

    #[test]
    fn usage_with_cached_tokens() {
        let u = parse_usage(&json!({
            "usage": {
                "prompt_tokens": 1200, "completion_tokens": 64, "total_tokens": 1264,
                "prompt_tokens_details": {"cached_tokens": 1024}
            }
        }))
        .expect("usage");
        assert_eq!(u.prompt_tokens, 1200);
        assert_eq!(u.completion_tokens, 64);
        assert_eq!(u.cached_tokens, Some(1024));
    }

    #[test]
    fn usage_without_details_and_null_usage() {
        let u = parse_usage(&json!({"usage": {"prompt_tokens": 7, "completion_tokens": 3}}))
            .expect("usage");
        assert_eq!(u.cached_tokens, None);
        assert_eq!(parse_usage(&json!({"usage": null, "choices": []})), None);
        assert_eq!(parse_usage(&json!({"choices": []})), None);
    }

    #[test]
    fn usage_may_be_passed_bare() {
        let u = parse_usage(&json!({"prompt_tokens": 1, "completion_tokens": 2})).expect("usage");
        assert_eq!(u.prompt_tokens, 1);
        assert_eq!(u.completion_tokens, 2);
    }
}
