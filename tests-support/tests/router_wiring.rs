//! The stub upstream, wired into the **real** routing table and driven through the real
//! proxy handler.
//!
//! This exists because "a capability is not shipped until something outside its own module
//! proves it is reachable". The README tells three agents to copy this wiring; this file
//! is the copy, compiled and executed, so the instructions cannot rot into prose.

use apexrouter_core::config::{Config, ProviderCfg};
use apexrouter_core::usage::UsageWriter;
use apexrouter_core::Paths;
use apexrouter_router::{proxy_router, RouterInner, TableBuilder};
use apexrouter_tests_support::{routing, Stub};
use axum::body::Body;
use axum::http::{Method, Request};
use std::sync::{Arc, Mutex, OnceLock};
use tower::ServiceExt;

/// `Paths::resolve` reads the process environment, which is global to this test binary.
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn paths_at(dir: &std::path::Path) -> Paths {
    let guard = match env_lock().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let previous = std::env::var_os("APEXROUTER_HOME");
    std::env::set_var("APEXROUTER_HOME", dir);
    let resolved = Paths::resolve();
    match previous {
        Some(v) => std::env::set_var("APEXROUTER_HOME", v),
        None => std::env::remove_var("APEXROUTER_HOME"),
    }
    drop(guard);
    let paths = resolved.expect("resolve paths");
    paths.ensure_layout().expect("layout");
    paths
}

/// The hermetic config every router test uses: the Together probe points at a **closed**
/// loopback port, so no test can reach a paid endpoint with a real key.
fn test_config() -> Config {
    let mut cfg = Config::default();
    cfg.providers.insert(
        "together".to_owned(),
        ProviderCfg {
            base_url: "http://127.0.0.1:1/v1".to_owned(),
            api_key_env: None,
            api_key_file: None,
        },
    );
    cfg
}

/// Everything the README's third incantation says, in one function.
fn router_with(
    paths: &Paths,
    backends: Vec<apexrouter_protocol::Backend>,
    file: apexrouter_protocol::RouteFile,
) -> Arc<RouterInner> {
    let cfg = Arc::new(test_config());
    let (tx, _rx) = tokio::sync::broadcast::channel(64);
    let usage = UsageWriter::open(paths, &cfg.compat).expect("usage writer");
    let router = RouterInner::new(Arc::clone(&cfg), tx, usage);
    for b in backends {
        router.registry().upsert(b, &cfg.router);
    }
    let table = TableBuilder::compile(&cfg, &file, router.registry()).expect("compile");
    router.store_table(table);
    router
}

fn post_chat(body: &str) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .expect("request")
}

#[tokio::test]
async fn an_alias_pointed_at_a_stub_upstream_relays_a_completion_and_rewrites_the_model() {
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = paths_at(dir.path());

    let upstream = Stub::with("echo");
    let file = routing::route_file(
        "auto",
        vec![routing::route_to("auto", &["stub-a"], Some("stub-model"))],
    );
    let router = router_with(&paths, vec![upstream.backend("stub-a")], file);
    let app = proxy_router(router);

    let response = app
        .oneshot(post_chat(
            r#"{"model":"auto","messages":[{"role":"user","content":"through the proxy"}]}"#,
        ))
        .await
        .expect("infallible");

    assert_eq!(response.status().as_u16(), 200);
    let route = response
        .headers()
        .get("x-apexrouter-route")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(route.starts_with("auto|"), "route header was {route:?}");

    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(
        body["choices"][0]["message"]["content"], "through the proxy",
        "the stub echoed the payload the relay forwarded"
    );

    // What the upstream actually received — the assertion a translation test lives on.
    let seen = upstream.last_request().expect("the stub saw a request");
    assert_eq!(seen.path, "/v1/chat/completions");
    assert_eq!(
        seen.model().as_deref(),
        Some("stub-model"),
        "the route rewrote `model` from the alias to the upstream id"
    );
}

#[tokio::test]
async fn a_failing_stub_fails_over_to_the_next_target_in_the_chain() {
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = paths_at(dir.path());

    let broken = Stub::with("chat_status=503");
    let good = Stub::with("content=served by the second");
    let file = routing::route_file(
        "auto",
        vec![routing::route_to(
            "auto",
            &["stub-broken", "stub-good"],
            Some("stub-model"),
        )],
    );
    let router = router_with(
        &paths,
        vec![broken.backend("stub-broken"), good.backend("stub-good")],
        file,
    );

    let response = proxy_router(router)
        .oneshot(post_chat(r#"{"model":"auto","messages":[]}"#))
        .await
        .expect("infallible");
    assert_eq!(response.status().as_u16(), 200);

    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "served by the second"
    );
    assert_eq!(broken.hits(), 1, "the first target was tried");
    assert_eq!(good.hits(), 1, "and the second answered");
}
