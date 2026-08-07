//! OWNER: unit R-10 (router/src/anthropic/{mod,translate,sse}.rs). Do not edit outside that
//! unit.
//!
//! The Anthropic ingress. **This module implements exactly one matrix cell,
//! `Anthropic → OpenAi`.** The `Anthropic → Anthropic` cell is the existing byte relay and
//! needs no code here; `OpenAi → Anthropic` is a `501` and stays one.
//!
//! The point of the surface: `ANTHROPIC_BASE_URL=http://127.0.0.1:8888` lets the Claude Code
//! harness drive a local or rented model.
//!
//! R-08's `handler.rs` calls into here; **R-10 does not own `handler.rs`**.
//!
//! # The surface, and what a caller has to do with it
//!
//! For the one translating cell, in order:
//!
//! 1. [`check_version_header`] — a missing or unrecognised `anthropic-version` is a `400`
//!    before anything else happens.
//! 2. [`request_to_openai`], with [`AnthropicCfg`] built from `[router] anthropic_tools`.
//!    On `Err`, [`translate_error`] turns it into the `400` the client expects. **No upstream
//!    is contacted on any of these paths** — that is the whole point of doing them here.
//! 3. [`upstream_path`] for the outbound path, and the translated bytes for the body.
//! 4. Buffered response ⇒ [`response_to_anthropic`]. Streaming response ⇒ [`SseTranslator`],
//!    fed every upstream chunk and finished when the upstream ends.
//!
//! `x-api-key` and `anthropic-version` are consumed here and never appear on the outbound
//! request. Nothing in this module forwards a header: R-03's outbound map is **constructed**,
//! not cloned, so that guarantee is structural rather than a rule to remember.

use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use serde_json::json;

pub mod sse;
pub mod translate;

pub use sse::SseTranslator;
pub use translate::{
    map_stop_reason_to_anthropic, map_stop_reason_to_openai, request_to_openai,
    response_to_anthropic, AnthropicCfg, TranslateError,
};

/// The `/v1/messages` ingress path, and the only path this unit rewrites.
const MESSAGES_PATH: &str = "/v1/messages";

/// Where [`MESSAGES_PATH`] lands on an OpenAI upstream.
const CHAT_COMPLETIONS_PATH: &str = "/v1/chat/completions";

/// The `anthropic-version` values this ingress accepts.
///
/// Anthropic has shipped exactly two, and every current SDK — Claude Code included — sends
/// `2023-06-01`. An allowlist rather than "any non-empty string" because the header is what
/// tells `GET /v1/models` to change shape, and a typo there would silently hand ApexOS's LAN
/// sweep a list it cannot read.
pub const SUPPORTED_VERSIONS: [&str; 2] = ["2023-06-01", "2023-01-01"];

/// Is this request speaking Anthropic?
///
/// True for `POST /v1/messages`, and for `GET /v1/models` when an `anthropic-version` header
/// is present. **It must never fire on an OpenAI request**: `GET /v1/models` without that
/// header returns the byte-identical OpenAI list shape it returns today, because ApexOS's
/// LAN compute sweep identifies a node by exactly that shape.
///
/// The `path` is the **normalised** one (R-03's `normalize_path` has already collapsed a
/// doubled leading `/v1`), which is what R-08's dispatch passes.
pub fn is_anthropic_ingress(path: &str, headers: &HeaderMap) -> bool {
    if path == MESSAGES_PATH || path.starts_with("/v1/messages/") {
        return true;
    }
    (path == "/v1/models" || path.starts_with("/v1/models/"))
        && headers.contains_key("anthropic-version")
}

/// `/v1/messages` → `/v1/chat/completions`. The ONLY path rewrite this unit performs.
///
/// The signature returns a `&'static str`, so there is no "echo the input back" arm: the
/// caller is the `Anthropic → OpenAi` cell, which is only ever reached for `/v1/messages`.
/// Anything else is a caller bug, and gets the chat-completions path plus a warning that
/// names the offender rather than a silent nonsense path.
pub fn upstream_path(ingress_path: &str) -> &'static str {
    if ingress_path != MESSAGES_PATH {
        tracing::warn!(
            path = %ingress_path,
            "anthropic ingress: upstream_path called off the /v1/messages cell"
        );
    }
    CHAT_COMPLETIONS_PATH
}

/// An **Anthropic-shaped** error body — `{"type":"error","error":{"type":…,"message":…}}`,
/// **not** the OpenAI shape — for failures that occur before or instead of an upstream hop.
///
/// The client is an Anthropic SDK and will parse it as one. Symmetrically, the
/// `OpenAi → Anthropic` 501 carries an OpenAI-shaped body (R-06's `openai_error`).
pub fn anthropic_error(status: StatusCode, kind: &str, msg: &str) -> axum::response::Response {
    let body = json!({
        "type": "error",
        "error": { "type": kind, "message": msg },
    });
    // Serialising an object `json!` just built cannot fail. The fallback keeps the
    // no-`unwrap` rule without pretending the branch is reachable.
    let bytes = serde_json::to_vec(&body).unwrap_or_else(|_| {
        br#"{"type":"error","error":{"type":"api_error","message":"error serialisation failed"}}"#
            .to_vec()
    });

    let mut res = axum::response::Response::new(axum::body::Body::from(bytes));
    *res.status_mut() = status;
    res.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    res
}

/// Turn a [`TranslateError`] into the `400` an Anthropic SDK expects.
///
/// Every translation failure is the client's request being unanswerable — a missing required
/// field, a block with no equivalent, or a capability the operator has turned off — so every
/// one is a `400 invalid_request_error`, never a `500` and never a retryable status. A
/// `ToolsDisabled` in particular has to be unmistakable: its message names
/// `[router] anthropic_tools`, because the alternative failure mode is an agent silently
/// answering without the tools it asked for.
pub fn translate_error(e: &TranslateError) -> axum::response::Response {
    anthropic_error(StatusCode::BAD_REQUEST, e.kind(), &e.to_string())
}

/// `anthropic-version` must be present and recognised.
///
/// Returns the `400` response to send when it is not, so a caller can `if let Some(res) = …
/// { return res; }`. The message names the header, because a client that forgot it has no
/// other way to find out.
pub fn check_version_header(headers: &HeaderMap) -> Option<axum::response::Response> {
    let value = headers
        .get("anthropic-version")
        .and_then(|v| v.to_str().ok());
    match value {
        Some(v) if SUPPORTED_VERSIONS.contains(&v) => None,
        Some(v) => Some(anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            &format!(
                "anthropic-version: {v:?} is not supported; this ingress speaks {}",
                SUPPORTED_VERSIONS.join(", ")
            ),
        )),
        None => Some(anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "the anthropic-version header is required (send anthropic-version: 2023-06-01)",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use serde_json::Value;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            if let Ok(v) = HeaderValue::from_str(v) {
                h.insert(*k, v);
            }
        }
        h
    }

    fn anthropic() -> HeaderMap {
        headers(&[
            ("anthropic-version", "2023-06-01"),
            ("x-api-key", "not-needed"),
        ])
    }

    async fn body_json(res: axum::response::Response) -> Value {
        let bytes = to_bytes(res.into_body(), 64 * 1024).await.expect("body");
        serde_json::from_slice(&bytes).expect("json")
    }

    // ---- ingress detection ---------------------------------------------------------------------

    #[test]
    fn ingress_detection_never_fires_on_an_openai_request() {
        // Every path and header combination R-08's suite exercises, plus the legacy compat
        // routes and the opaque catch-all. Not one of them may be seen as Anthropic.
        let openai = headers(&[
            ("content-type", "application/json"),
            ("authorization", "Bearer sk-test"),
            ("user-agent", "openai-python/1.0"),
        ]);
        for path in [
            "/v1/chat/completions",
            "/v1/completions",
            "/v1/embeddings",
            "/v1/rerank",
            "/v1/models",
            "/v1/models/auto",
            "/health",
            "/providers",
            "/switch",
            "/slots",
            "/props",
            "/metrics",
            "/",
        ] {
            for h in [&HeaderMap::new(), &openai] {
                assert!(
                    !is_anthropic_ingress(path, h),
                    "{path} must stay an OpenAI request"
                );
            }
        }
    }

    #[test]
    fn the_apexos_lan_sweep_shape_is_the_default_for_v1_models() {
        // The load-bearing default: no header, no Anthropic ingress, so R-08 serves the
        // byte-identical OpenAI list shape ApexOS's compute sweep identifies a node by.
        assert!(!is_anthropic_ingress("/v1/models", &HeaderMap::new()));
        assert!(!is_anthropic_ingress("/v1/models/auto", &HeaderMap::new()));
        // …and the header, and only the header, opts into the other shape.
        assert!(is_anthropic_ingress("/v1/models", &anthropic()));
        assert!(is_anthropic_ingress("/v1/models/auto", &anthropic()));
    }

    #[test]
    fn messages_is_anthropic_with_or_without_the_header() {
        // Detection is by path; the *validation* of `anthropic-version` is a separate 400, so
        // a client that forgot the header still gets an Anthropic-shaped error rather than an
        // OpenAI-shaped one.
        assert!(is_anthropic_ingress("/v1/messages", &HeaderMap::new()));
        assert!(is_anthropic_ingress("/v1/messages", &anthropic()));
        assert!(is_anthropic_ingress(
            "/v1/messages/count_tokens",
            &HeaderMap::new()
        ));
    }

    // ---- path rewrite --------------------------------------------------------------------------

    #[test]
    fn the_only_rewrite_is_messages_to_chat_completions() {
        assert_eq!(upstream_path("/v1/messages"), "/v1/chat/completions");
    }

    // ---- error bodies --------------------------------------------------------------------------

    #[tokio::test]
    async fn the_error_body_is_anthropic_shaped_not_openai_shaped() {
        let res = anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "max_tokens is required",
        );
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            res.headers().get(CONTENT_TYPE).map(HeaderValue::as_bytes),
            Some(&b"application/json"[..])
        );

        let v = body_json(res).await;
        assert_eq!(v["type"], "error");
        assert_eq!(v["error"]["type"], "invalid_request_error");
        assert_eq!(v["error"]["message"], "max_tokens is required");
        // The OpenAI shape has `error.code` and `error.param` and no top-level `type`; an
        // Anthropic SDK destructures the other one.
        assert!(v["error"].get("code").is_none());
        assert!(v["error"].get("param").is_none());
    }

    #[tokio::test]
    async fn a_missing_max_tokens_is_a_400_in_the_anthropic_shape() {
        let body = br#"{"model":"auto","messages":[{"role":"user","content":"hi"}]}"#;
        let e = request_to_openai(body, &AnthropicCfg { tools: false })
            .expect_err("max_tokens is required");
        let res = translate_error(&e);
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let v = body_json(res).await;
        assert_eq!(v["type"], "error");
        assert_eq!(v["error"]["type"], "invalid_request_error");
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("max_tokens"),
            "{v}"
        );
    }

    #[tokio::test]
    async fn a_tools_body_with_the_flag_off_is_a_400_naming_the_config_key() {
        let body = br#"{"model":"auto","max_tokens":8,
            "tools":[{"name":"f","input_schema":{"type":"object"}}],
            "messages":[{"role":"user","content":"hi"}]}"#;
        let e = request_to_openai(body, &AnthropicCfg { tools: false })
            .expect_err("tools flag off must refuse");
        let res = translate_error(&e);
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let v = body_json(res).await;
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("anthropic_tools"),
            "the error must name the key the operator has to flip: {v}"
        );
    }

    // ---- the version header ----------------------------------------------------------------------

    #[tokio::test]
    async fn a_missing_or_unknown_anthropic_version_is_a_400_naming_the_header() {
        assert!(check_version_header(&anthropic()).is_none());

        let res = check_version_header(&HeaderMap::new()).expect("missing header is a 400");
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let v = body_json(res).await;
        assert_eq!(v["type"], "error");
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("anthropic-version"),
            "{v}"
        );

        let wrong = headers(&[("anthropic-version", "2024-13-99")]);
        let res = check_version_header(&wrong).expect("unknown version is a 400");
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let v = body_json(res).await;
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("2024-13-99"),
            "{v}"
        );
    }
}
