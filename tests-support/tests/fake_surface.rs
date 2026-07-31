//! The fake's HTTP surface, judged by the real code that consumes it.
//!
//! `apexrouter_core::upstream::probe` is the supervisor's readiness gate and the health
//! prober's only view of an upstream, so if it is satisfied here it is satisfied there.
//! Everything else is checked with `reqwest`, which is the client the relay uses.

use apexrouter_core::upstream::{self, parse_timings, parse_usage};
use apexrouter_tests_support::{Behavior, RecordedRequest, Stub};
use std::time::Duration;

const T: Duration = Duration::from_secs(5);

fn client() -> reqwest::Client {
    reqwest::Client::builder().build().expect("client")
}

async fn chat(stub: &Stub, body: serde_json::Value) -> reqwest::Response {
    client()
        .post(format!("{}/v1/chat/completions", stub.base_url()))
        .json(&body)
        .send()
        .await
        .expect("chat request")
}

// ---- the readiness gate ------------------------------------------------------------

#[tokio::test]
async fn the_real_probe_reads_health_models_props_and_slots_off_the_stub() {
    let stub = Stub::start();
    let p = upstream::probe(&client(), &stub.base_url(), None, T).await;

    assert!(p.healthy, "probe error: {:?}", p.error);
    assert!(!p.loading);
    assert_eq!(p.error, None);
    assert_eq!(p.models.len(), 1);
    assert_eq!(p.models[0].id, "stub-model");
    assert_eq!(p.slots_total, Some(4), "-np 4 became total_slots");
    assert_eq!(p.slots_busy, Some(0));
    assert_eq!(p.ctx, Some(32_768), "-c 32768 became n_ctx");
    assert_eq!(
        p.build_info.as_deref(),
        Some(apexrouter_tests_support::BUILD_INFO)
    );
    assert!(p.models[0].tools, "chat_template_caps carried tools");
}

#[tokio::test]
async fn a_base_url_that_already_carries_v1_still_probes() {
    let stub = Stub::start();
    let p = upstream::probe(&client(), &format!("{}/v1", stub.base_url()), None, T).await;
    assert!(p.healthy, "probe error: {:?}", p.error);
    assert_eq!(p.models.len(), 1);
}

#[tokio::test]
async fn a_loading_model_is_alive_and_not_healthy() {
    let stub = Stub::with("loading_forever");
    let p = upstream::probe(&client(), &stub.base_url(), None, T).await;
    assert!(p.loading, "503 loading model must read as progress");
    assert!(!p.healthy);
    assert_eq!(p.status, Some(503));
}

#[tokio::test]
async fn never_healthy_is_a_503_that_is_not_progress() {
    let stub = Stub::with("never_healthy");
    let p = upstream::probe(&client(), &stub.base_url(), None, T).await;
    assert!(!p.healthy);
    assert!(
        !p.loading,
        "a 503 that is not about loading must not reset a health deadline"
    );
    assert!(p.error.is_some());
}

#[tokio::test]
async fn props_and_metrics_are_off_unless_argv_asked_for_them_and_slots_can_be_501() {
    // b9199: `--props` and `--metrics` are off by default, `--slots` is on.
    let bare = Stub::with_argv(&["-a", "bare", "-np", "2"], "");
    let p = upstream::probe(&client(), &bare.base_url(), None, T).await;
    assert!(p.healthy, "a build without --props is still healthy");
    assert_eq!(p.error, None, "404 /props is normal, not an error");
    assert_eq!(p.ctx, None, "no /props, so no context");
    assert_eq!(p.slots_total, Some(2), "/slots is on by default");

    let status = client()
        .get(format!("{}/metrics", bare.base_url()))
        .send()
        .await
        .expect("metrics")
        .status();
    assert_eq!(status.as_u16(), 404);

    let no_slots = Stub::with_argv(&["-a", "x", "--no-slots"], "");
    let p = upstream::probe(&client(), &no_slots.base_url(), None, T).await;
    assert_eq!(p.error, None, "501 /slots is normal, not an error");
    assert_eq!(p.slots_total, None);
}

#[tokio::test]
async fn metrics_are_prometheus_text_when_argv_asked_for_them() {
    let stub = Stub::start();
    let body = client()
        .get(format!("{}/metrics", stub.base_url()))
        .send()
        .await
        .expect("metrics")
        .text()
        .await
        .expect("text");
    assert!(body.contains("llamacpp:prompt_tokens_total"));
    assert!(body.contains("llamacpp:n_slots 4"));
}

#[tokio::test]
async fn busy_slots_are_visible_to_a_drain() {
    let stub = Stub::with("busy_slots=3");
    let p = upstream::probe(&client(), &stub.base_url(), None, T).await;
    assert_eq!(p.slots_busy, Some(3));
    stub.set_behavior("busy_slots=0");
    let p = upstream::probe(&client(), &stub.base_url(), None, T).await;
    assert_eq!(p.slots_busy, Some(0), "behaviour changes live");
}

// ---- completions ---------------------------------------------------------------------

#[tokio::test]
async fn a_buffered_completion_carries_usage_and_timings_the_tee_can_read() {
    let stub = Stub::with("content=hello there,tok_per_s=10");
    let body: serde_json::Value = chat(
        &stub,
        serde_json::json!({"model": "stub-model", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await
    .json()
    .await
    .expect("json");

    assert_eq!(body["choices"][0]["message"]["content"], "hello there");
    assert_eq!(body["choices"][0]["finish_reason"], "stop");

    let usage = parse_usage(&body).expect("usage");
    assert!(usage.prompt_tokens > 0);
    assert!(usage.completion_tokens > 0);
    let timings = parse_timings(&body).expect("timings");
    assert!((timings.predicted_per_second - 10.0).abs() < 0.001);
}

#[tokio::test]
async fn the_model_field_is_echoed_so_a_rewrite_is_observable() {
    let stub = Stub::start();
    let body: serde_json::Value = chat(
        &stub,
        serde_json::json!({"model": "rewritten-by-the-router", "messages": []}),
    )
    .await
    .json()
    .await
    .expect("json");
    assert_eq!(body["model"], "rewritten-by-the-router");

    let seen: RecordedRequest = stub.last_request().expect("a recorded request");
    assert_eq!(seen.path, "/v1/chat/completions");
    assert_eq!(seen.model().as_deref(), Some("rewritten-by-the-router"));
}

#[tokio::test]
async fn reasoning_content_arrives_with_an_empty_content() {
    let stub = Stub::with("reasoning,content=thinking hard");
    let body: serde_json::Value = chat(
        &stub,
        serde_json::json!({"model": "m", "messages": [{"role": "user", "content": "why"}]}),
    )
    .await
    .json()
    .await
    .expect("json");
    assert_eq!(body["choices"][0]["message"]["content"], "");
    assert_eq!(
        body["choices"][0]["message"]["reasoning_content"],
        "thinking hard"
    );
}

#[tokio::test]
async fn a_forced_status_and_a_fail_first_budget_are_both_available() {
    let stub = Stub::with("chat_status=503");
    let r = chat(&stub, serde_json::json!({"model": "m"})).await;
    assert_eq!(r.status().as_u16(), 503);
    let body: serde_json::Value = r.json().await.expect("json");
    assert_eq!(body["error"]["type"], "server_error");

    let flaky = Stub::with("fail_first=2");
    for expected in [503, 503, 200] {
        let r = chat(&flaky, serde_json::json!({"model": "m"})).await;
        assert_eq!(r.status().as_u16(), expected);
    }
}

#[tokio::test]
async fn an_echoing_stub_replies_with_what_it_was_sent() {
    let stub = Stub::with("echo");
    let body: serde_json::Value = chat(
        &stub,
        serde_json::json!({"model": "m", "messages": [
            {"role": "system", "content": "ignored"},
            {"role": "user", "content": "the faithful payload"}
        ]}),
    )
    .await
    .json()
    .await
    .expect("json");
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "the faithful payload"
    );
}

// ---- streaming --------------------------------------------------------------------------

#[tokio::test]
async fn a_stream_is_sse_frames_ending_in_done_with_usage_on_the_last_data_frame() {
    let stub = Stub::with("content=abcdefgh,chunks=4,tok_per_s=8");
    let resp = chat(
        &stub,
        serde_json::json!({"model": "m", "stream": true, "messages": []}),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );

    let text = resp.text().await.expect("body");
    let frames: Vec<&str> = text
        .split("\n\n")
        .filter_map(|f| f.trim().strip_prefix("data: "))
        .collect();
    assert_eq!(frames.last(), Some(&"[DONE]"), "stream must terminate");

    let payloads: Vec<serde_json::Value> = frames
        .iter()
        .filter(|f| **f != "[DONE]")
        .filter_map(|f| serde_json::from_str(f).ok())
        .collect();
    let content: String = payloads
        .iter()
        .filter_map(|p| p["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(content, "abcdefgh", "no byte is lost across the chunks");

    let last = payloads.last().expect("a final frame");
    assert_eq!(last["choices"][0]["finish_reason"], "stop");
    let usage = parse_usage(last).expect("usage on the final frame");
    assert_eq!(usage.completion_tokens, 4);
    let timings = parse_timings(last).expect("timings on the final frame");
    assert!((timings.predicted_per_second - 8.0).abs() < 0.001);
}

#[tokio::test]
async fn a_truncated_stream_ends_cleanly_with_no_done_terminator() {
    let stub = Stub::with("truncate_stream,chunks=3");
    let text = chat(
        &stub,
        serde_json::json!({"model": "m", "stream": true, "messages": []}),
    )
    .await
    .text()
    .await
    .expect("body");
    assert!(text.contains("data: {"), "frames were sent");
    assert!(
        !text.contains("[DONE]"),
        "the terminator must be missing: that is the point"
    );
}

#[tokio::test]
async fn a_stream_that_dies_mid_chunk_is_a_transport_error_not_an_eof() {
    let stub = Stub::with("die_mid_stream,chunks=6");
    let resp = chat(
        &stub,
        serde_json::json!({"model": "m", "stream": true, "messages": []}),
    )
    .await;
    assert_eq!(
        resp.status().as_u16(),
        200,
        "headers arrive before the death"
    );
    let err = resp.text().await.expect_err("the body must not complete");
    assert!(
        err.is_body() || err.is_decode(),
        "expected a body error, got {err}"
    );
}

#[tokio::test]
async fn a_slow_stream_is_slow_at_the_place_the_relay_measures() {
    let stub = Stub::with("content=abcdefgh,chunks=4,chunk_ms=60");
    let started = std::time::Instant::now();
    let text = chat(
        &stub,
        serde_json::json!({"model": "m", "stream": true, "messages": []}),
    )
    .await
    .text()
    .await
    .expect("body");
    assert!(text.contains("[DONE]"));
    assert!(
        started.elapsed() >= Duration::from_millis(200),
        "four chunks at 60 ms cannot arrive in {:?}",
        started.elapsed()
    );
}

// ---- behaviour plumbing -------------------------------------------------------------------

#[tokio::test]
async fn behaviour_can_be_changed_live_through_the_control_surface() {
    let stub = Stub::start();
    assert_eq!(
        chat(&stub, serde_json::json!({"model": "m"}))
            .await
            .status()
            .as_u16(),
        200
    );

    let applied = client()
        .post(format!("{}/_apex/behavior", stub.base_url()))
        .json(&serde_json::json!({"chat_status": 429}))
        .send()
        .await
        .expect("apply");
    assert_eq!(applied.status().as_u16(), 200);
    assert_eq!(
        chat(&stub, serde_json::json!({"model": "m"}))
            .await
            .status()
            .as_u16(),
        429
    );
}

#[tokio::test]
async fn the_control_surface_is_never_recorded_as_a_request() {
    let stub = Stub::start();
    let _ = client()
        .get(format!("{}/_apex/requests", stub.base_url()))
        .send()
        .await
        .expect("requests");
    assert_eq!(stub.hits(), 0, "control traffic is not upstream traffic");
}

#[tokio::test]
async fn an_api_key_file_makes_the_fake_refuse_an_unauthenticated_request() {
    let dir = tempfile::tempdir().expect("tempdir");
    let key = dir.path().join("endpoint.key");
    std::fs::write(&key, "sk-fake-123").expect("write key");
    let key_arg = key.display().to_string();
    let stub = Stub::with_argv(&["-a", "guarded", "--api-key-file", &key_arg], "");

    let anonymous = client()
        .get(format!("{}/v1/models", stub.base_url()))
        .send()
        .await
        .expect("anonymous");
    assert_eq!(anonymous.status().as_u16(), 401);

    let authorised = client()
        .get(format!("{}/v1/models", stub.base_url()))
        .bearer_auth("sk-fake-123")
        .send()
        .await
        .expect("authorised");
    assert_eq!(authorised.status().as_u16(), 200);
}

#[tokio::test]
async fn keep_alive_connections_are_reused_across_requests() {
    // One pooled client, several requests: a server that mishandled keep-alive would hang
    // or reset here rather than answer four times.
    let stub = Stub::start();
    let http = client();
    for _ in 0..4 {
        let r = http
            .get(format!("{}/health", stub.base_url()))
            .send()
            .await
            .expect("health");
        assert_eq!(r.status().as_u16(), 200);
    }
    assert_eq!(stub.hits(), 4);
}

#[tokio::test]
async fn hanging_before_headers_means_the_client_times_out_rather_than_being_answered() {
    let stub = Stub::with("hang_before_headers");
    let http = reqwest::Client::builder()
        .timeout(Duration::from_millis(300))
        .build()
        .expect("client");
    let err = http
        .post(format!("{}/v1/chat/completions", stub.base_url()))
        .json(&serde_json::json!({"model": "m", "messages": []}))
        .send()
        .await
        .expect_err("it must never answer");
    assert!(err.is_timeout(), "expected a timeout, got {err}");
    // The request still arrived: the fake read it and then said nothing.
    assert_eq!(stub.hits(), 1);
}

#[tokio::test]
async fn an_in_process_stub_asked_to_exit_goes_unhealthy_instead_of_killing_the_test_binary() {
    let stub = Stub::start();
    let _ = client()
        .post(format!("{}/_apex/exit?code=1", stub.base_url()))
        .send()
        .await
        .expect("exit");
    // If the guard were missing, this line would never run.
    tokio::time::sleep(Duration::from_millis(120)).await;
    let p = upstream::probe(&client(), &stub.base_url(), None, T).await;
    assert!(!p.healthy, "an exited stub must not read as healthy");
}

#[test]
fn a_behaviour_spec_and_its_defaults_are_what_the_docs_claim() {
    let b = Behavior::parse("load_ms=400,chunks=3");
    assert_eq!(b.load_ms, 400);
    assert_eq!(b.chunks, 3);
    assert_eq!(Behavior::default().chunks, 4);
}
