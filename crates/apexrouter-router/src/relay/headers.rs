//! OWNER: unit R-03 (router/src/relay/{mod,headers,body}.rs). Do not edit outside that unit.
//!
//! Outbound headers are **CONSTRUCTED from an allowlist**, never cloned from the inbound
//! map. Two things follow: a client's `Authorization` cannot reach a third party, and a
//! local `llama-server --api-key` becomes reachable through the proxy for the first time.
//!
//! The Anthropic ingress adds two headers to the never-forwarded set: `x-api-key` and
//! `anthropic-version` are consumed by the proxy and never reach an upstream.

use apexrouter_core::secret::Secret;
use axum::http::HeaderMap;

/// Build the outbound header map from scratch.
///
/// Allowlist: `content-type`, `accept`, `accept-encoding: identity`, `user-agent`,
/// `x-request-id`, plus a configurable extra list — and the backend's own credential.
/// Adds `X-Request-Id` and `Via: 1.1 apexrouter`.
pub fn outbound_headers(
    inbound: &HeaderMap,
    cred: Option<&Secret<String>>,
    extra_allow: &[String],
) -> HeaderMap {
    todo!("R-03: outbound_headers")
}

/// Filter an upstream response's headers: drop hop-by-hop, keep multi-valued headers
/// multi-valued.
pub fn response_headers(upstream: &HeaderMap) -> HeaderMap {
    todo!("R-03: response_headers")
}
