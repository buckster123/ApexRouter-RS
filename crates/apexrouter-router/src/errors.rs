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
use axum::http::StatusCode;

/// Build an OpenAI-shaped error response.
pub fn openai_error(status: StatusCode, kind: &str, msg: &str) -> axum::response::Response {
    todo!("R-06: openai_error")
}

/// Map a resolution failure onto its status and error type.
pub fn map_status(e: &RouteError) -> (StatusCode, &'static str) {
    todo!("R-06: map_status")
}
