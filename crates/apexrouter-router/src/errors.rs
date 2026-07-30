//! OWNER: unit R-06 (router/src/errors.rs, router/src/models.rs). Do not edit outside that
//! unit.
//!
//! Errors are **OpenAI-shaped everywhere** on the proxy listener:
//! `{"error":{"message":…,"type":…,"code":…,"param":null}}`.
//!
//! The status mapping is load-bearing in both house projects, particularly the
//! 502-vs-503 distinction:
//!
//! | type | status |
//! |---|---|
//! | `model_not_found` | 404 |
//! | `upstream_unavailable` | 502 |
//! | `upstream_timeout` | 504 |
//! | `no_healthy_backend` | 503 |
//! | `server_overloaded` | 503 + `Retry-After` |
//! | `request_too_large` | 413 |
//! | `loop_detected` | 508 |
//! | `provider_not_configured` | 503 |
//! | `starting` | 503 |
//! | `redacted_endpoint` | 403 |
//!
//! The Anthropic ingress has its own shape and its own helper — see
//! [`crate::anthropic::anthropic_error`].

use crate::resolve::RouteError;
use axum::http::header::{HeaderValue, CONTENT_TYPE, RETRY_AFTER};
use axum::http::StatusCode;
use serde_json::json;

/// The table in this module's doc comment, as code.
///
/// [`openai_error`] consults it purely to *notice* a caller that pairs a documented error
/// type with the wrong status — the 502-vs-503 distinction is the one both house projects
/// depend on, and it is exactly the kind of thing that rots silently. An unknown type is not
/// an error: the proxy also emits `not_implemented`, `invalid_request_error` and whatever a
/// later surface needs.
fn documented_status(kind: &str) -> Option<StatusCode> {
    Some(match kind {
        "model_not_found" => StatusCode::NOT_FOUND,
        "upstream_unavailable" => StatusCode::BAD_GATEWAY,
        "upstream_timeout" => StatusCode::GATEWAY_TIMEOUT,
        "no_healthy_backend" => StatusCode::SERVICE_UNAVAILABLE,
        "server_overloaded" => StatusCode::SERVICE_UNAVAILABLE,
        "request_too_large" => StatusCode::PAYLOAD_TOO_LARGE,
        "loop_detected" => StatusCode::LOOP_DETECTED,
        "provider_not_configured" => StatusCode::SERVICE_UNAVAILABLE,
        "starting" => StatusCode::SERVICE_UNAVAILABLE,
        "redacted_endpoint" => StatusCode::FORBIDDEN,
        _ => return None,
    })
}

/// Build an OpenAI-shaped error response.
///
/// The body is always exactly four keys — `message`, `type`, `code`, `param` — because that
/// is what the OpenAI and Anthropic SDKs destructure. `code` carries the same token as
/// `type`: LocalRouter's flat `{"error":"<string>"}` taught clients nothing, and clients in
/// the wild read one key or the other depending on their vintage. `param` is always `null`;
/// nothing on the proxy path fails at the granularity of a single request field.
///
/// `Retry-After: 1` is added for `server_overloaded`, per `ARCHITECTURE.md` §4.5. One second
/// is the smallest honest value — the queue that produced the 503 is measured in permits,
/// not minutes. A caller that knows better (a breaker holding a real `retry_at_unix`) is
/// free to overwrite the header on the returned response.
pub fn openai_error(status: StatusCode, kind: &str, msg: &str) -> axum::response::Response {
    if let Some(want) = documented_status(kind) {
        if want != status {
            tracing::warn!(
                error_type = kind,
                got = status.as_u16(),
                want = want.as_u16(),
                "openai_error called with a status the documented error table contradicts"
            );
        }
    }

    let body = json!({
        "error": {
            "message": msg,
            "type": kind,
            "code": kind,
            "param": serde_json::Value::Null,
        }
    });

    // Serialising an object `json!` just built cannot fail. The fallback keeps the
    // no-`unwrap` rule without pretending the branch is reachable.
    let bytes = serde_json::to_vec(&body).unwrap_or_else(|_| {
        br#"{"error":{"message":"error serialisation failed","type":"internal_error","code":"internal_error","param":null}}"#
            .to_vec()
    });

    let mut res = axum::response::Response::new(axum::body::Body::from(bytes));
    *res.status_mut() = status;
    res.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if kind == "server_overloaded" {
        res.headers_mut()
            .insert(RETRY_AFTER, HeaderValue::from_static("1"));
    }
    res
}

/// Map a resolution failure onto its status and error type.
///
/// Only [`RouteError::NoRoute`] is a client mistake (404, and the message names the aliases
/// we do know). The other two mean the router is fine and the fleet is not, which is a
/// **503 `no_healthy_backend`** — deliberately not a 502. 502 means "we dispatched to an
/// upstream and it failed"; 503 means "we never dispatched at all". Both house projects
/// branch on exactly that: a 502 is retried against the same target, a 503 is a reason to
/// start or rent something.
pub fn map_status(e: &RouteError) -> (StatusCode, &'static str) {
    match e {
        RouteError::NoRoute { .. } => (StatusCode::NOT_FOUND, "model_not_found"),
        RouteError::NoHealthy { .. } => (StatusCode::SERVICE_UNAVAILABLE, "no_healthy_backend"),
        RouteError::FilteredOut { .. } => (StatusCode::SERVICE_UNAVAILABLE, "no_healthy_backend"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_protocol::Alias;
    use axum::body::to_bytes;

    async fn body_json(res: axum::response::Response) -> serde_json::Value {
        let bytes = to_bytes(res.into_body(), 64 * 1024).await.expect("body");
        serde_json::from_slice(&bytes).expect("json")
    }

    #[tokio::test]
    async fn every_error_body_is_exactly_message_type_code_param() {
        for (status, kind) in [
            (StatusCode::NOT_FOUND, "model_not_found"),
            (StatusCode::BAD_GATEWAY, "upstream_unavailable"),
            (StatusCode::GATEWAY_TIMEOUT, "upstream_timeout"),
            (StatusCode::SERVICE_UNAVAILABLE, "no_healthy_backend"),
            (StatusCode::SERVICE_UNAVAILABLE, "server_overloaded"),
            (StatusCode::PAYLOAD_TOO_LARGE, "request_too_large"),
            (StatusCode::LOOP_DETECTED, "loop_detected"),
            (StatusCode::SERVICE_UNAVAILABLE, "provider_not_configured"),
            (StatusCode::SERVICE_UNAVAILABLE, "starting"),
            (StatusCode::FORBIDDEN, "redacted_endpoint"),
            (StatusCode::NOT_IMPLEMENTED, "not_implemented"),
        ] {
            let res = openai_error(status, kind, "something went wrong");
            assert_eq!(res.status(), status, "{kind}");
            assert_eq!(
                res.headers().get(CONTENT_TYPE).map(HeaderValue::as_bytes),
                Some(&b"application/json"[..]),
                "{kind}"
            );

            let v = body_json(res).await;
            let obj = v.as_object().expect("top level object");
            assert_eq!(obj.len(), 1, "{kind}: only an `error` key at the top level");
            let err = obj["error"].as_object().expect("error object");
            let keys: Vec<&str> = err.keys().map(String::as_str).collect();
            assert_eq!(keys, ["message", "type", "code", "param"], "{kind}");
            assert_eq!(err["message"], "something went wrong");
            assert_eq!(err["type"], kind);
            assert_eq!(err["code"], kind);
            assert!(err["param"].is_null(), "{kind}");
        }
    }

    #[tokio::test]
    async fn only_server_overloaded_carries_retry_after() {
        let overloaded = openai_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "server_overloaded",
            "queue full",
        );
        assert_eq!(
            overloaded
                .headers()
                .get(RETRY_AFTER)
                .map(HeaderValue::as_bytes),
            Some(&b"1"[..])
        );

        let plain = openai_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_healthy_backend",
            "nothing ready",
        );
        assert!(plain.headers().get(RETRY_AFTER).is_none());
    }

    #[test]
    fn the_documented_table_keeps_502_and_503_apart() {
        // The distinction both house projects branch on.
        assert_eq!(
            documented_status("upstream_unavailable"),
            Some(StatusCode::BAD_GATEWAY)
        );
        assert_eq!(
            documented_status("provider_not_configured"),
            Some(StatusCode::SERVICE_UNAVAILABLE)
        );
        assert_ne!(
            documented_status("upstream_unavailable"),
            documented_status("provider_not_configured")
        );

        // …and the rest of ARCHITECTURE.md §4.5, verbatim.
        for (kind, want) in [
            ("model_not_found", StatusCode::NOT_FOUND),
            ("upstream_timeout", StatusCode::GATEWAY_TIMEOUT),
            ("no_healthy_backend", StatusCode::SERVICE_UNAVAILABLE),
            ("server_overloaded", StatusCode::SERVICE_UNAVAILABLE),
            ("request_too_large", StatusCode::PAYLOAD_TOO_LARGE),
            ("loop_detected", StatusCode::LOOP_DETECTED),
            ("starting", StatusCode::SERVICE_UNAVAILABLE),
            ("redacted_endpoint", StatusCode::FORBIDDEN),
        ] {
            assert_eq!(documented_status(kind), Some(want), "{kind}");
        }
        assert_eq!(documented_status("not_a_documented_type"), None);
    }

    #[test]
    fn map_status_matches_the_table() {
        assert_eq!(
            map_status(&RouteError::NoRoute {
                known: vec!["auto".into(), "coder".into()],
            }),
            (StatusCode::NOT_FOUND, "model_not_found")
        );
        assert_eq!(
            map_status(&RouteError::NoHealthy {
                alias: Alias::parse("auto").expect("alias"),
            }),
            (StatusCode::SERVICE_UNAVAILABLE, "no_healthy_backend")
        );
        assert_eq!(
            map_status(&RouteError::FilteredOut {
                alias: Alias::parse("vision").expect("alias"),
                why: "require_tags = [vision]".into(),
            }),
            (StatusCode::SERVICE_UNAVAILABLE, "no_healthy_backend")
        );

        // Every pair `map_status` can produce agrees with the documented table.
        for e in [
            RouteError::NoRoute { known: vec![] },
            RouteError::NoHealthy {
                alias: Alias::parse("auto").expect("alias"),
            },
            RouteError::FilteredOut {
                alias: Alias::parse("auto").expect("alias"),
                why: "min_ctx".into(),
            },
        ] {
            let (status, kind) = map_status(&e);
            assert_eq!(documented_status(kind), Some(status), "{kind}");
        }
    }

    #[tokio::test]
    async fn a_route_error_renders_into_the_openai_shape() {
        let e = RouteError::NoRoute {
            known: vec!["auto".into(), "coder".into()],
        };
        let (status, kind) = map_status(&e);
        let res = openai_error(status, kind, &e.to_string());
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let v = body_json(res).await;
        assert_eq!(v["error"]["type"], "model_not_found");
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("coder"),
            "a 404 that names the alternatives is the point: {v}"
        );
    }
}
