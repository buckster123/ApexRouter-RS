//! OWNER: unit R-03 (router/src/relay/{mod,headers,body}.rs). Do not edit outside that unit.
//!
//! Outbound headers are **CONSTRUCTED from an allowlist**, never cloned from the inbound
//! map. Two things follow: a client's `Authorization` cannot reach a third party, and a
//! local `llama-server --api-key` becomes reachable through the proxy for the first time.
//!
//! The Anthropic ingress adds two headers to the never-forwarded set: `x-api-key` and
//! `anthropic-version` are consumed by the proxy and never reach an upstream.

use apexrouter_core::secret::Secret;
use axum::http::header::{
    HeaderName, HeaderValue, ACCEPT_ENCODING, AUTHORIZATION, CONNECTION, CONTENT_LENGTH, VIA,
};
use axum::http::HeaderMap;

/// The base allowlist. Everything else must be named in `extra_allow` to travel.
///
/// `accept-encoding` is deliberately absent: it is *forced* to `identity` below, because the
/// relay promises verbatim bytes and a re-encoded stream is not verbatim.
const ALLOW: &[&str] = &["content-type", "accept", "user-agent", "x-request-id"];

/// Headers that never leave this process, whatever `extra_allow` says.
///
/// Client credentials (`authorization`, `proxy-authorization`, `cookie`, `x-api-key`),
/// connection-scoped metadata (`host`, `content-length`, `connection`, `keep-alive`,
/// `transfer-encoding`, `te`, `trailer`, `upgrade`, `expect`), and the two headers the
/// Anthropic ingress consumes (`x-api-key`, `anthropic-version`). `via` is here because it
/// is rebuilt, not copied.
const NEVER: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "proxy-authenticate",
    "cookie",
    "host",
    "content-length",
    "connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "trailer",
    "trailers",
    "upgrade",
    "expect",
    "x-api-key",
    "anthropic-version",
    "via",
];

/// Hop-by-hop response headers, per RFC 9110 §7.6.1.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

/// What this proxy calls itself in `Via`. The inbound loop guard greps for it.
const VIA_SELF: &str = "1.1 apexrouter";

/// An inbound `Via` chain longer than this is dropped rather than forwarded — a proxy loop
/// somewhere upstream must not become an unbounded header.
const VIA_MAX: usize = 2048;

/// How many `Connection:` tokens are honoured before the rest are ignored.
const MAX_CONNECTION_TOKENS: usize = 32;

/// Build the outbound header map from scratch.
///
/// Allowlist: `content-type`, `accept`, `accept-encoding: identity`, `user-agent`,
/// `x-request-id`, plus a configurable extra list — and the backend's own credential.
/// Adds `X-Request-Id` and `Via: 1.1 apexrouter`.
///
/// Nothing in `inbound` reaches the result unless its name is on a list: the map is
/// constructed, never cloned. `extra_allow` cannot re-admit a never-forwarded header, so a
/// mistyped config line cannot leak a client's `Authorization` to a rented GPU. Allowlisted
/// headers that arrived multi-valued stay multi-valued.
///
/// The credential is written as `Authorization: Bearer …` and marked sensitive, so `http`'s
/// own `Debug` prints `Sensitive`. It is the **only** way an `authorization` header can
/// appear in the output. An inbound `x-request-id` is forwarded; when there is none a fresh
/// ULID is minted, so every upstream call is correlatable. An inbound `Via` chain is carried
/// through with this proxy appended, which is what lets a downstream ApexRouter see the loop.
pub fn outbound_headers(
    inbound: &HeaderMap,
    cred: Option<&Secret<String>>,
    extra_allow: &[String],
) -> HeaderMap {
    let mut out = HeaderMap::new();

    for (name, value) in inbound.iter() {
        // `HeaderName::as_str` is always lowercase.
        let n = name.as_str();
        if NEVER.contains(&n) {
            continue;
        }
        let allowed =
            ALLOW.contains(&n) || extra_allow.iter().any(|e| e.trim().eq_ignore_ascii_case(n));
        if allowed {
            out.append(name.clone(), value.clone());
        }
    }

    // Verbatim relay means no transport-level re-encoding, ever.
    out.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));

    if !out.contains_key("x-request-id") {
        if let Ok(v) = HeaderValue::from_str(&ulid::Ulid::new().to_string()) {
            out.insert(HeaderName::from_static("x-request-id"), v);
        }
    }

    out.insert(VIA, via_value(inbound));

    if let Some(c) = cred {
        // A key with a newline or a control byte in it: drop it rather than send a
        // half-built header. The upstream's 401 is then the honest outcome.
        if let Ok(mut v) = HeaderValue::from_str(&format!("Bearer {}", c.expose())) {
            v.set_sensitive(true);
            out.insert(AUTHORIZATION, v);
        }
    }

    out
}

/// The `Via` value to send: the inbound chain, then us.
fn via_value(inbound: &HeaderMap) -> HeaderValue {
    let mut chain = String::new();
    for v in inbound.get_all(VIA).iter() {
        let Ok(s) = v.to_str() else { continue };
        let s = s.trim();
        if s.is_empty() {
            continue;
        }
        if !chain.is_empty() {
            chain.push_str(", ");
        }
        chain.push_str(s);
        if chain.len() > VIA_MAX {
            chain.clear();
            break;
        }
    }
    if chain.is_empty() {
        return HeaderValue::from_static(VIA_SELF);
    }
    chain.push_str(", ");
    chain.push_str(VIA_SELF);
    HeaderValue::from_str(&chain).unwrap_or_else(|_| HeaderValue::from_static(VIA_SELF))
}

/// Filter an upstream response's headers: drop hop-by-hop, keep multi-valued headers
/// multi-valued.
///
/// Dropped: the RFC 9110 hop-by-hop set, every field name listed in the upstream's own
/// `Connection:` header, and `content-length` — the length belongs to the hop, and a
/// streamed body has none. Everything else, including several `set-cookie` lines or several
/// `x-ratelimit-*` lines, is appended in order and reaches the client unchanged.
pub fn response_headers(upstream: &HeaderMap) -> HeaderMap {
    // Field names the upstream itself declared connection-scoped.
    let mut listed: Vec<String> = Vec::new();
    for v in upstream.get_all(CONNECTION).iter() {
        let Ok(s) = v.to_str() else { continue };
        for tok in s.split(',') {
            let tok = tok.trim();
            if tok.is_empty() || listed.len() >= MAX_CONNECTION_TOKENS {
                continue;
            }
            listed.push(tok.to_ascii_lowercase());
        }
    }

    let mut out = HeaderMap::new();
    for (name, value) in upstream.iter() {
        let n = name.as_str();
        if HOP_BY_HOP.contains(&n) || n == CONTENT_LENGTH.as_str() {
            continue;
        }
        if listed.iter().any(|t| t == n) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hv(s: &str) -> HeaderValue {
        HeaderValue::from_str(s).expect("test header value")
    }

    fn client_map() -> HeaderMap {
        let mut m = HeaderMap::new();
        m.insert("content-type", hv("application/json"));
        m.insert("accept", hv("text/event-stream"));
        m.insert("accept-encoding", hv("gzip, br"));
        m.insert("user-agent", hv("openai-python/1.40.0"));
        m.insert("authorization", hv("Bearer sk-client-secret"));
        m.insert("proxy-authorization", hv("Basic Zm9vOmJhcg=="));
        m.insert("cookie", hv("session=abc123"));
        m.insert("host", hv("127.0.0.1:8888"));
        m.insert("content-length", hv("4096"));
        m.insert("connection", hv("keep-alive"));
        m.insert("transfer-encoding", hv("chunked"));
        m.insert("te", hv("trailers"));
        m.insert("x-api-key", hv("sk-ant-client"));
        m.insert("anthropic-version", hv("2023-06-01"));
        m
    }

    /// The acceptance test: none of these ever reach an upstream.
    #[test]
    fn client_credentials_and_hop_by_hop_never_travel() {
        let out = outbound_headers(&client_map(), None, &[]);
        for banned in [
            "authorization",
            "proxy-authorization",
            "cookie",
            "host",
            "content-length",
            "connection",
            "transfer-encoding",
            "te",
            "x-api-key",
            "anthropic-version",
        ] {
            assert!(
                !out.contains_key(banned),
                "{banned} leaked onto the outbound request"
            );
        }
        // What is allowed did travel.
        assert_eq!(out.get("content-type"), Some(&hv("application/json")));
        assert_eq!(out.get("accept"), Some(&hv("text/event-stream")));
        assert_eq!(out.get("user-agent"), Some(&hv("openai-python/1.40.0")));
    }

    #[test]
    fn the_only_authorization_is_the_backends_own() {
        let cred = Secret::new("sk-upstream-key".to_owned());
        let out = outbound_headers(&client_map(), Some(&cred), &[]);
        assert_eq!(out.get(AUTHORIZATION), Some(&hv("Bearer sk-upstream-key")));
        assert!(
            out.get(AUTHORIZATION).map(|v| v.is_sensitive()) == Some(true),
            "the credential header must be marked sensitive"
        );
        // Exactly one — the client's was not appended alongside it.
        assert_eq!(out.get_all(AUTHORIZATION).iter().count(), 1);
    }

    #[test]
    fn a_credential_with_a_control_byte_is_dropped_not_half_sent() {
        let cred = Secret::new("sk-with-a\nnewline".to_owned());
        let out = outbound_headers(&client_map(), Some(&cred), &[]);
        assert!(!out.contains_key(AUTHORIZATION));
    }

    #[test]
    fn extra_allow_cannot_re_admit_a_never_header() {
        let extra = vec![
            "authorization".to_owned(),
            "Cookie".to_owned(),
            "x-api-key".to_owned(),
            "x-title".to_owned(),
        ];
        let mut m = client_map();
        m.insert("x-title", hv("ApexOS"));
        let out = outbound_headers(&m, None, &extra);
        assert!(!out.contains_key("authorization"));
        assert!(!out.contains_key("cookie"));
        assert!(!out.contains_key("x-api-key"));
        // …but an extra header that is not on the never-list does travel, case-insensitively.
        assert_eq!(out.get("x-title"), Some(&hv("ApexOS")));
    }

    #[test]
    fn accept_encoding_is_forced_to_identity() {
        let out = outbound_headers(&client_map(), None, &["accept-encoding".to_owned()]);
        assert_eq!(out.get(ACCEPT_ENCODING), Some(&hv("identity")));
        assert_eq!(out.get_all(ACCEPT_ENCODING).iter().count(), 1);
    }

    #[test]
    fn via_is_appended_and_request_id_is_always_present() {
        let mut m = HeaderMap::new();
        m.insert("via", hv("1.1 someproxy"));
        m.insert("x-request-id", hv("req-42"));
        let out = outbound_headers(&m, None, &[]);
        assert_eq!(out.get(VIA), Some(&hv("1.1 someproxy, 1.1 apexrouter")));
        assert_eq!(out.get("x-request-id"), Some(&hv("req-42")));

        let out = outbound_headers(&HeaderMap::new(), None, &[]);
        assert_eq!(out.get(VIA), Some(&hv("1.1 apexrouter")));
        let id = out
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        assert_eq!(id.len(), 26, "a ULID is minted when the client sent none");
    }

    #[test]
    fn multi_valued_allowlisted_headers_stay_multi_valued() {
        let mut m = HeaderMap::new();
        m.append("accept", hv("application/json"));
        m.append("accept", hv("text/event-stream"));
        let out = outbound_headers(&m, None, &[]);
        let vals: Vec<_> = out
            .get_all("accept")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        assert_eq!(vals, vec!["application/json", "text/event-stream"]);
    }

    #[test]
    fn response_drops_hop_by_hop_and_keeps_multi_valued() {
        let mut m = HeaderMap::new();
        m.insert("content-type", hv("text/event-stream"));
        m.insert("content-length", hv("123"));
        m.insert("transfer-encoding", hv("chunked"));
        m.insert("connection", hv("keep-alive, x-hop-only"));
        m.insert("keep-alive", hv("timeout=5"));
        m.insert("upgrade", hv("h2c"));
        m.insert("x-hop-only", hv("gone"));
        m.append("set-cookie", hv("a=1"));
        m.append("set-cookie", hv("b=2"));
        m.insert("x-usage", hv("prompt=10;completion=20"));

        let out = response_headers(&m);
        for banned in [
            "content-length",
            "transfer-encoding",
            "connection",
            "keep-alive",
            "upgrade",
            "x-hop-only",
        ] {
            assert!(!out.contains_key(banned), "{banned} survived the filter");
        }
        assert_eq!(out.get("content-type"), Some(&hv("text/event-stream")));
        assert_eq!(out.get("x-usage"), Some(&hv("prompt=10;completion=20")));
        let cookies: Vec<_> = out
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        assert_eq!(cookies, vec!["a=1", "b=2"]);
    }
}
