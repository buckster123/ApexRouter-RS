//! OWNER: unit S-02 (server/src/auth.rs). Do not edit outside that unit.
//!
//! Auth, scopes and **the mutation gate**.
//!
//! A loopback control plane is **not** a trust boundary. A cross-origin `fetch` to
//! `POST http://127.0.0.1:8888/switch` with `Content-Type: text/plain` is a CORS *simple
//! request*: no preflight, the request is delivered, and the attacker never needs to read
//! the response. So every mutating request on **either** listener passes
//! [`require_mutation_origin`]:
//!
//! 1. `Host` must be in the bind allowlist — this closes DNS rebinding, which otherwise
//!    makes an attacker's page same-origin.
//! 2. If `Origin` is present it must be same-origin; if `Sec-Fetch-Site` is present it must
//!    be `same-origin` or `none`.
//! 3. Otherwise a bearer with `write` scope is required.
//!
//! Non-browser clients (the CLI, Slint, `curl`) send neither header, so they pass rule 2
//! unchanged. There is deliberately **no `CorsLayer`**: the embedded UI is same-origin, and
//! this gate is stronger than a CORS policy.
//!
//! # What the `Host` allowlist actually allows
//!
//! Rule 1 exists to stop **DNS rebinding**, and rebinding needs a *name*: the attacker's page
//! is served from `evil.example.com`, whose A record flips to `127.0.0.1`, and the browser
//! then sends `Host: evil.example.com` to our listener while treating the page as same-origin
//! with it. So the allowlist is "IP literals, plus `localhost`, on a port this process
//! actually listens on" — never an arbitrary DNS name. That is checkable without knowing
//! which of our two listeners took the connection, and it stays correct when the bind is
//! `0.0.0.0` and we cannot know our own LAN address.
//!
//! Rule 1 applies to **reads as well as mutations** — a successful rebinding makes the
//! attacker's page same-origin, and `GET /v1/snapshot` then hands it the whole rig. Rules 2
//! and 3 stay on mutations only, because a cross-origin *read* is exactly what
//! `[server] proxy_cors_origins` exists to permit. In both cases a valid bearer is the
//! escape hatch: a browser cannot forge one, so a deployment behind a reverse proxy that
//! rewrites `Host` keeps working (and, being non-loopback, it has a token by §9.1 anyway).
//!
//! # Refusal bodies
//!
//! Every refusal in this module is `{"ok":false,"error":"…"}` — house envelope (A). The
//! control plane otherwise answers with envelope (B) (plain text), but this gate also sits in
//! front of `POST /switch` on the **proxy** listener, where the clients being defended are
//! OpenAI SDKs and LocalRouter-era scripts that parse a JSON body and print `error`. One
//! shape for one module beats two shapes that differ by listener.
//!
//! # Response headers on the proxy — where the CORS ruling lives
//!
//! LocalRouter set `Access-Control-Allow-Origin: *` on every proxied response; ApexRouter
//! emits nothing by default and makes it configurable through `[server] proxy_cors_origins`.
//! That ruling is implemented **once**, by S-01, in `lib.rs` (`cors_middleware`,
//! `allow_origin_value`, `is_proxy_mutation`, `preflight`), because response headers are
//! added where the routers are assembled and only that unit may edit the assembly. S-02
//! deliberately ships no second emitter: two implementations of one header rule, layered in
//! one process, is how a proxy ends up answering a mutation with a CORS header nobody
//! intended. What this module owns of that ruling is the half it can enforce — §9.3's gate,
//! which governs every mutation on both listeners whatever `proxy_cors_origins` says.

#![allow(clippy::result_large_err)]

use crate::state::AppState;
use apexrouter_core::config::ServerCfg;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::header::{HeaderValue, AUTHORIZATION, HOST, ORIGIN, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

/// The product-prefixed token header, the second of the three accepted presentations.
pub const TOKEN_HEADER: &str = "x-apexrouter-token";

/// The listener's own bind address, for the `Host` allowlist.
///
/// A request carries no reliable statement of which socket accepted it, and the two
/// listeners have different ports. S-01 inserts this extension per listener
/// (`.layer(Extension(ListenerBind(local_addr)))`); when it is absent — an embedded mount
/// inside ApexOS, say — [`require_auth`] falls back to the configured control bind, which is
/// the listener this middleware belongs to.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ListenerBind(pub SocketAddr);

/// Token scopes.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    /// Read-only.
    Read,
    /// Mutations.
    Write,
    /// Token management and shutdown.
    Admin,
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Scope::Read => "read",
            Scope::Write => "write",
            Scope::Admin => "admin",
        })
    }
}

/// How a request earned its scope. Recorded so a handler can refuse to do something
/// irreversible on a bypass alone.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AuthVia {
    /// A public route (`/health`), which is never gated.
    Public,
    /// A presented bearer token matched the configured one.
    Token,
    /// `[server] loopback_bypass` plus a genuinely loopback peer IP.
    LoopbackBypass,
}

/// What [`require_auth`] inserts as a request extension once a request is allowed through.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RequestAuth {
    /// The scope the caller holds.
    pub scope: Scope,
    /// How it was obtained.
    pub via: AuthVia,
}

/// The auth middleware. **Absent `ConnectInfo` fails closed**, never open.
///
/// Two gates in one pass, in this order:
///
/// 1. every mutation ([`required_scope`] above [`Scope::Read`]) passes
///    [`require_mutation_origin`] — *before* authentication, so a cross-origin `fetch` from a
///    browser on this machine is refused even though the loopback bypass would have
///    authenticated it. That ordering is the whole point of §9.3. A read passes rule 1 of
///    that gate alone (the `Host` allowlist), because a rebound page reads as well as writes;
/// 2. then the bearer, then the loopback bypass, then `401`.
///
/// Absent connect-info cannot even reach this body — axum rejects the `ConnectInfo`
/// extractor with a `500` when the app was not served with
/// `into_make_service_with_connect_info::<SocketAddr>()` — and the decision function behind
/// this middleware treats an absent peer as "not loopback" for the same reason. Closed on
/// both paths.
pub async fn require_auth(
    State(s): State<Arc<AppState>>,
    ci: ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    let cfg = s.cfg.load_full();
    let server = &cfg.server;
    let bind = req
        .extensions()
        .get::<ListenerBind>()
        .map(|b| b.0)
        .unwrap_or_else(|| fallback_bind(server));
    let expected = configured_token(server);

    let grant = authorize(
        server,
        expected.as_deref(),
        Some(ci.0),
        &bind,
        req.method(),
        req.uri(),
        req.headers(),
    );

    match grant {
        Ok(auth) => {
            let mut req = req;
            req.extensions_mut().insert(auth);
            next.run(req).await
        }
        Err(refusal) => refusal,
    }
}

/// Bearer accepted three ways: `Authorization: Bearer <t>`, `X-ApexRouter-Token: <t>`, and
/// `?token=<t>`.
///
/// The `TraceLayer` span records **method and path only**, never the query string, because
/// `?token=` is an accepted presentation. `crate::request_span` is that span, and
/// `the_trace_span_never_records_the_query_string` in this module is what holds it to it.
///
/// The query value is percent-decoded (and `+` is a space, as in any query string), so a
/// token that had to be escaped to survive a URL still compares equal.
pub fn extract_presented_token(h: &HeaderMap, uri: &Uri) -> Option<String> {
    if let Some(v) = h.get(AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        let v = v.trim();
        // `get(..7)` rather than `[..7]`: a header value is ASCII by construction here, but a
        // slice index that could ever panic on a request-controlled string is not worth
        // saving two characters over.
        if v.get(..7)
            .is_some_and(|s| s.eq_ignore_ascii_case("bearer "))
        {
            let t = v[7..].trim();
            if !t.is_empty() {
                return Some(t.to_owned());
            }
        }
    }
    if let Some(v) = h.get(TOKEN_HEADER).and_then(|v| v.to_str().ok()) {
        let t = v.trim();
        if !t.is_empty() {
            return Some(t.to_owned());
        }
    }
    for pair in uri.query().unwrap_or_default().split('&') {
        if let Some(v) = pair.strip_prefix("token=") {
            let t = percent_decode(v);
            let t = t.trim();
            if !t.is_empty() {
                return Some(t.to_owned());
            }
        }
    }
    None
}

/// Derive the required scope from (path, method). `/v1/tokens*` is always `Admin`.
///
/// Three groups, in order:
///
/// - `/v1/tokens*` and `/v1/shutdown` are `Admin` whatever the method — minting a token is
///   privilege escalation and stopping the daemon takes every hot model with it;
/// - the **data plane** (`/v1/chat/completions`, `/v1/completions`, `/v1/embeddings`,
///   `/v1/rerank`, `/v1/messages`) is `Read` even though it is `POST`. Inference mutates
///   nothing this server owns, and a read-scoped agent that could not run a completion would
///   be a scope system nobody uses. A doubled `/v1` prefix is collapsed first, exactly as the
///   proxy does, so `/v1/v1/chat/completions` is classified identically;
/// - everything else is `Read` for safe methods and `Write` otherwise.
pub fn required_scope(path: &str, method: &Method) -> Scope {
    let p = collapse_v1(path);
    let p = p.trim_end_matches('/');
    if p.starts_with("/v1/tokens") || p == "/v1/shutdown" {
        return Scope::Admin;
    }
    if is_inference_path(p) {
        return Scope::Read;
    }
    match *method {
        Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE => Scope::Read,
        _ => Scope::Write,
    }
}

/// CSRF + DNS-rebinding defence. Applied to EVERY mutation on BOTH listeners.
///
/// The `Err` variant is a ready-to-return `Response` rather than an error code, so a
/// refusal carries the explanatory body the operator needs. That makes it a large `Err`,
/// which is fine on a path that runs once per mutation.
///
/// `cfg` supplies the ports of *both* listeners, because this gate runs in front of
/// `POST /switch` on the proxy as well as in front of every control-plane mutation, and rule
/// 3 needs `token_env` to resolve the configured bearer.
#[allow(clippy::result_large_err)]
pub fn require_mutation_origin(
    h: &HeaderMap,
    bind: &SocketAddr,
    cfg: &ServerCfg,
) -> Result<(), Response> {
    let expected = configured_token(cfg);
    require_mutation_origin_with(h, bind, cfg, expected.as_deref())
}

/// The mutation gate as a layer, for the **proxy** listener — which has no [`require_auth`].
///
/// The proxy is unauthenticated by design (`OPENAI_API_KEY=not-needed`), but it still serves
/// one mutation: `POST /switch`, which retargets the default alias and can carry a
/// `base_url` plus a Together key. `router/src/compat.rs` enforces rule 2 inside the handler
/// as defence in depth; rules 1 and 3 need the listener's bind address and `[server]`, which
/// only the server has — so they live here and S-01 wires this in front of the proxy router:
///
/// ```text
/// proxy_router(router).layer(from_fn_with_state(state.clone(), auth::mutation_gate))
/// ```
///
/// Requests the scope table calls reads — every inference path, `/v1/models`, `/health`,
/// `/providers` — pass through untouched, so this cannot slow or reject the data plane.
pub async fn mutation_gate(State(s): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    if required_scope(req.uri().path(), req.method()) == Scope::Read {
        return next.run(req).await;
    }
    let cfg = s.cfg.load_full();
    let bind = req.extensions().get::<ListenerBind>().map(|b| b.0);
    let bind = bind.unwrap_or_else(|| {
        cfg.server
            .proxy_bind
            .parse()
            .unwrap_or_else(|_| fallback_bind(&cfg.server))
    });
    match require_mutation_origin(req.headers(), &bind, &cfg.server) {
        Ok(()) => next.run(req).await,
        Err(refusal) => refusal,
    }
}

/// Refuse to bind a listener the network can reach without auth configured (§9.1).
///
/// Called by S-01's `serve()` **before** either listener binds. `Ok(())` means every
/// non-loopback bind has a token behind it; the `Err` carries the fix, not just the fact.
pub fn ensure_bind_is_authenticated(cfg: &ServerCfg) -> anyhow::Result<()> {
    check_binds(cfg, configured_token(cfg).is_some())
}

// ---------------------------------------------------------------------------------------
// the decision, as a pure function
// ---------------------------------------------------------------------------------------

/// The whole of [`require_auth`]'s decision, with every input passed in.
///
/// `peer` is `Option` on purpose: `None` is "connect-info was absent", and it must behave
/// like a non-loopback peer. That is the fails-closed rule of §9.1, and it is a test case
/// rather than a comment because the failure it prevents is silent.
fn authorize(
    server: &ServerCfg,
    expected: Option<&str>,
    peer: Option<SocketAddr>,
    bind: &SocketAddr,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
) -> Result<RequestAuth, Response> {
    if is_public(uri.path()) {
        return Ok(RequestAuth {
            scope: Scope::Read,
            via: AuthVia::Public,
        });
    }

    let need = required_scope(uri.path(), method);
    if need > Scope::Read {
        require_mutation_origin_with(headers, bind, server, expected)?;
    } else {
        require_host_allowlist(headers, bind, server, expected)?;
    }

    let presented = extract_presented_token(headers, uri);
    if let Some(want) = expected {
        if let Some(got) = presented.as_deref() {
            if token_matches(got, want) {
                // One configured token, one operator: it carries every scope. Per-token
                // scopes arrive with the `/v1/tokens` store; `required_scope` is already
                // the half of that feature this file owns.
                return Ok(RequestAuth {
                    scope: Scope::Admin,
                    via: AuthVia::Token,
                });
            }
            return Err(unauthorized("invalid token"));
        }
    }

    let peer_is_loopback = peer.map(|p| p.ip().is_loopback()).unwrap_or(false);
    if server.loopback_bypass && peer_is_loopback {
        return Ok(RequestAuth {
            scope: Scope::Admin,
            via: AuthVia::LoopbackBypass,
        });
    }

    Err(unauthorized(&format!(
        "missing token: {need} scope is required — present it as `Authorization: Bearer $\
         {}`, `{TOKEN_HEADER}: …` or `?token=…`",
        server.token_env
    )))
}

/// [`require_mutation_origin`] with the configured token passed in rather than read from the
/// environment, so the tests do not have to mutate a process-global.
fn require_mutation_origin_with(
    h: &HeaderMap,
    bind: &SocketAddr,
    cfg: &ServerCfg,
    expected: Option<&str>,
) -> Result<(), Response> {
    // Rule 1 — the Host allowlist.
    require_host_allowlist(h, bind, cfg, expected)?;

    // Rule 2 — same-origin. curl, the CLI and Slint send neither header and pass here.
    let host = host_of(h).unwrap_or_default();
    if same_origin(h, host) {
        return Ok(());
    }

    // Rule 3 — a bearer with write scope. A cross-origin browser `fetch` cannot set an
    // `Authorization` header without a preflight, and we answer no preflight, so this arm is
    // reachable only by a deliberate client.
    if bearer_matches(h, expected) {
        return Ok(());
    }

    Err(refuse(
        StatusCode::FORBIDDEN,
        "cross-origin request refused: this is a mutation. A browser page on another origin \
         may not change this daemon's state; present a write-scoped bearer token if you meant it",
    ))
}

/// Rule 1 alone — the `Host` allowlist, which gates **every** request, not only mutations.
///
/// A browser always sends `Host`; its absence means an HTTP/1.0 or prior-knowledge-HTTP/2
/// client, which no first-party client is, so it is refused rather than waved through. A
/// valid bearer overrides both failures: a rebound page cannot present one, and a reverse
/// proxy that rewrites `Host` legitimately can.
fn require_host_allowlist(
    h: &HeaderMap,
    bind: &SocketAddr,
    cfg: &ServerCfg,
    expected: Option<&str>,
) -> Result<(), Response> {
    let Some(host) = host_of(h) else {
        if bearer_matches(h, expected) {
            return Ok(());
        }
        return Err(refuse(
            StatusCode::FORBIDDEN,
            "refused: no Host header. Every HTTP/1.1 client sends one, and ApexRouter needs \
             it to tell a local request from a DNS-rebinding attack",
        ));
    };
    if host_is_allowed(host, &allowed_ports(bind, cfg)) || bearer_matches(h, expected) {
        return Ok(());
    }
    Err(refuse(
        StatusCode::FORBIDDEN,
        format!(
            "refused: Host '{host}' is not an address this daemon listens on (possible DNS \
             rebinding). Use 127.0.0.1 or localhost with the listener's port, or present a \
             bearer token"
        ),
    ))
}

/// The `Host` header, trimmed. `None` when absent or not ASCII.
fn host_of(h: &HeaderMap) -> Option<&str> {
    h.get(HOST).and_then(|v| v.to_str().ok()).map(str::trim)
}

/// Does this request carry the configured bearer in a **header**? The query string is not
/// consulted: rule 3 exists to distinguish a deliberate client from a browser that was
/// pointed at a URL, and a URL can carry a query.
fn bearer_matches(h: &HeaderMap, expected: Option<&str>) -> bool {
    let Some(want) = expected else {
        return false;
    };
    extract_presented_token(h, &Uri::default())
        .map(|got| token_matches(&got, want))
        .unwrap_or(false)
}

/// `/v1/shutdown` and friends need auth; `/health` never does — every smoke test, the Slint
/// client's reachability probe and `apexrouter serve`'s own autostart poll depend on it
/// answering before any credential exists.
fn is_public(path: &str) -> bool {
    matches!(path.trim_end_matches('/'), "/health" | "/metrics")
}

/// The data plane. `Read`, deliberately — see [`required_scope`].
fn is_inference_path(p: &str) -> bool {
    matches!(
        p,
        "/v1/chat/completions"
            | "/v1/completions"
            | "/v1/embeddings"
            | "/v1/rerank"
            | "/v1/reranking"
            | "/v1/messages"
            | "/v1/messages/count_tokens"
    )
}

/// Collapse a repeated leading `/v1`, the way the proxy does, so scope classification cannot
/// be dodged by pointing a client at `…:8888/v1` and letting it append `/v1` again.
fn collapse_v1(path: &str) -> String {
    let mut p = path.to_owned();
    while p.starts_with("/v1/v1/") || p == "/v1/v1" {
        p = p.replacen("/v1/v1", "/v1", 1);
    }
    p
}

/// Ports a `Host` may name: this listener's, plus the other one's, because the gate runs on
/// both and the extension that would disambiguate is optional.
fn allowed_ports(bind: &SocketAddr, cfg: &ServerCfg) -> Vec<u16> {
    let mut ports = vec![bind.port()];
    for s in [cfg.proxy_bind.as_str(), cfg.control_bind.as_str()] {
        if let Some(p) = port_of(s) {
            if !ports.contains(&p) {
                ports.push(p);
            }
        }
    }
    ports
}

/// The port out of a `host:port` bind string, tolerating `[::1]:2739`.
fn port_of(bind: &str) -> Option<u16> {
    if let Ok(sa) = bind.parse::<SocketAddr>() {
        return Some(sa.port());
    }
    let (_, port) = split_host_port(bind)?;
    port
}

/// Split an authority into host and optional port. `[::1]:2739` keeps its brackets off.
fn split_host_port(authority: &str) -> Option<(String, Option<u16>)> {
    let a = authority.trim();
    if a.is_empty() {
        return None;
    }
    if let Some(rest) = a.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        let port = match tail.strip_prefix(':') {
            Some(p) => Some(p.parse::<u16>().ok()?),
            None => None,
        };
        return Some((host.to_owned(), port));
    }
    match a.rsplit_once(':') {
        // An IPv6 literal without brackets has several colons; treat it as a bare host.
        Some((h, p)) if !h.contains(':') => Some((h.to_owned(), Some(p.parse::<u16>().ok()?))),
        _ => Some((a.to_owned(), None)),
    }
}

/// Rule 1's allowlist: an IP literal or `localhost`, on a port we listen on.
fn host_is_allowed(host: &str, ports: &[u16]) -> bool {
    let Some((h, port)) = split_host_port(host) else {
        return false;
    };
    let port = port.unwrap_or(80);
    if !ports.contains(&port) {
        return false;
    }
    h.eq_ignore_ascii_case("localhost") || h.parse::<IpAddr>().is_ok()
}

/// Rule 2, byte for byte the same rule `router/src/compat.rs` enforces on `/switch` — this
/// is the defence-in-depth pair, not an accident.
fn same_origin(h: &HeaderMap, host: &str) -> bool {
    if let Some(site) = h.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        let site = site.trim();
        if !site.eq_ignore_ascii_case("same-origin") && !site.eq_ignore_ascii_case("none") {
            return false;
        }
    }
    if let Some(origin) = h.get(ORIGIN).and_then(|v| v.to_str().ok()) {
        // `Origin: null` (a sandboxed iframe, a `file://` page) has no authority and must
        // never be treated as same-origin.
        let authority = origin.trim().split("://").nth(1).unwrap_or_default();
        if authority.is_empty() || !authority.eq_ignore_ascii_case(host) {
            return false;
        }
    }
    true
}

/// The configured bearer, from the env var `[server] token_env` names. Empty is absent.
fn configured_token(cfg: &ServerCfg) -> Option<String> {
    if cfg.token_env.trim().is_empty() {
        return None;
    }
    let v = std::env::var(cfg.token_env.trim()).ok()?;
    let v = v.trim().to_owned();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

/// [`ensure_bind_is_authenticated`], with the environment lookup already done.
fn check_binds(cfg: &ServerCfg, have_token: bool) -> anyhow::Result<()> {
    if have_token {
        return Ok(());
    }
    for (what, bind) in [
        ("proxy", cfg.proxy_bind.as_str()),
        ("control", cfg.control_bind.as_str()),
    ] {
        if !bind_is_loopback(bind) {
            anyhow::bail!(
                "refusing to bind the {what} plane to {bind} without auth — a non-loopback \
                 listener is reachable by anything on the network. Fix: export {}=<a long \
                 random string> (or run `apexrouter token create`), or keep the bind on \
                 127.0.0.1",
                cfg.token_env
            );
        }
    }
    Ok(())
}

/// Is this bind string one only this machine can reach? Unparseable counts as **not**
/// loopback: the config layer already falls back on a bind it cannot parse, and guessing
/// generously here is how a listener ends up on 0.0.0.0 with no token.
fn bind_is_loopback(bind: &str) -> bool {
    if let Ok(sa) = bind.parse::<SocketAddr>() {
        return sa.ip().is_loopback();
    }
    match split_host_port(bind) {
        Some((h, _)) => {
            h.eq_ignore_ascii_case("localhost")
                || h.parse::<IpAddr>()
                    .map(|ip| ip.is_loopback())
                    .unwrap_or(false)
        }
        None => false,
    }
}

/// Constant-time-enough token comparison: both sides are hashed first, so neither the length
/// nor a shared prefix of the configured token leaks through timing.
fn token_matches(presented: &str, expected: &str) -> bool {
    let a = Sha256::digest(presented.as_bytes());
    let b = Sha256::digest(expected.as_bytes());
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Query-string percent-decoding, lossy on invalid UTF-8 and `+`-as-space like every other
/// query parser. Hand-written because pulling a URL crate in for six lines is not a trade.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => match (hex(b[i + 1]), hex(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push((h << 4) | l);
                    i += 3;
                }
                _ => {
                    out.push(b[i]);
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// The control listener's configured bind, for when no [`ListenerBind`] extension is present.
fn fallback_bind(cfg: &ServerCfg) -> SocketAddr {
    cfg.control_bind
        .parse::<SocketAddr>()
        .unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], 2739)))
}

/// House envelope (A): `{"ok":false,"error":"…"}`.
fn refuse(status: StatusCode, msg: impl Into<String>) -> Response {
    let msg: String = msg.into();
    (status, Json(json!({ "ok": false, "error": msg }))).into_response()
}

fn unauthorized(msg: &str) -> Response {
    let mut r = refuse(StatusCode::UNAUTHORIZED, msg);
    r.headers_mut().insert(
        WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"apexrouter\""),
    );
    r
}

// ---------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    const CONTROL: &str = "127.0.0.1:2739";

    fn cfg() -> ServerCfg {
        ServerCfg::default()
    }

    fn bind() -> SocketAddr {
        CONTROL
            .parse()
            .expect("the default control bind must parse")
    }

    fn peer(s: &str) -> Option<SocketAddr> {
        Some(s.parse().expect("peer must parse"))
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            let name = axum::http::HeaderName::from_bytes(k.as_bytes()).expect("header name");
            h.insert(name, HeaderValue::from_str(v).expect("header value"));
        }
        h
    }

    /// A browser's `Host` for our control listener — always present in a real request.
    fn local_host() -> Vec<(&'static str, &'static str)> {
        vec![("host", CONTROL)]
    }

    fn get(path: &str) -> (Method, Uri) {
        (Method::GET, path.parse().expect("uri"))
    }

    fn post(path: &str) -> (Method, Uri) {
        (Method::POST, path.parse().expect("uri"))
    }

    fn status_of(r: &Response) -> StatusCode {
        r.status()
    }

    // ---- the fails-closed rule ---------------------------------------------------------

    /// §9.1. `ConnectInfo` absent (the app was mounted without
    /// `into_make_service_with_connect_info`) must never be read as "local, therefore
    /// trusted". The middleware itself cannot even be entered in that case — axum rejects
    /// the extractor — and the decision below refuses independently.
    #[test]
    fn absent_connect_info_fails_closed() {
        let (m, u) = get("/v1/snapshot");
        let r = authorize(&cfg(), None, None, &bind(), &m, &u, &headers(&local_host()));
        let refusal = r.expect_err("an absent peer must not be trusted");
        assert_eq!(status_of(&refusal), StatusCode::UNAUTHORIZED);
        assert!(
            refusal.headers().contains_key(WWW_AUTHENTICATE),
            "a 401 must say how to authenticate"
        );
    }

    #[test]
    fn a_genuinely_loopback_peer_passes_when_the_bypass_is_on() {
        let (m, u) = get("/v1/snapshot");
        let auth = authorize(
            &cfg(),
            None,
            peer("127.0.0.1:51234"),
            &bind(),
            &m,
            &u,
            &headers(&local_host()),
        )
        .expect("the default posture is a usable local install");
        assert_eq!(auth.via, AuthVia::LoopbackBypass);
    }

    #[test]
    fn a_non_loopback_peer_without_a_token_is_refused() {
        let (m, u) = get("/v1/snapshot");
        let r = authorize(
            &cfg(),
            None,
            peer("192.168.1.9:51234"),
            &bind(),
            &m,
            &u,
            &headers(&local_host()),
        );
        assert_eq!(
            status_of(&r.expect_err("a LAN peer is not covered by the loopback bypass")),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn the_bypass_can_be_turned_off_entirely() {
        let mut c = cfg();
        c.loopback_bypass = false;
        let (m, u) = get("/v1/snapshot");
        let r = authorize(
            &c,
            None,
            peer("127.0.0.1:51234"),
            &bind(),
            &m,
            &u,
            &headers(&local_host()),
        );
        assert!(r.is_err(), "loopback_bypass = false means no bypass");
    }

    #[test]
    fn health_is_public_on_both_listeners() {
        let (m, u) = get("/health");
        let auth = authorize(&cfg(), None, None, &bind(), &m, &u, &HeaderMap::new())
            .expect("/health must answer before any credential exists");
        assert_eq!(auth.via, AuthVia::Public);
    }

    // ---- the token -------------------------------------------------------------------

    #[test]
    fn every_documented_presentation_is_accepted() {
        let u: Uri = "/v1/snapshot".parse().expect("uri");
        assert_eq!(
            extract_presented_token(&headers(&[("authorization", "Bearer s3cret")]), &u).as_deref(),
            Some("s3cret")
        );
        assert_eq!(
            extract_presented_token(&headers(&[("authorization", "bearer s3cret")]), &u).as_deref(),
            Some("s3cret"),
            "the scheme is case-insensitive per RFC 7235"
        );
        assert_eq!(
            extract_presented_token(&headers(&[(TOKEN_HEADER, "s3cret")]), &u).as_deref(),
            Some("s3cret")
        );
        let q: Uri = "/v1/snapshot?limit=5&token=s3c%2Bret".parse().expect("uri");
        assert_eq!(
            extract_presented_token(&HeaderMap::new(), &q).as_deref(),
            Some("s3c+ret"),
            "a percent-escaped token must survive the URL"
        );
        assert_eq!(extract_presented_token(&HeaderMap::new(), &u), None);
        assert_eq!(
            extract_presented_token(&headers(&[("authorization", "Bearer   ")]), &u),
            None,
            "an empty bearer is not a presentation"
        );
    }

    #[test]
    fn a_matching_token_authenticates_and_a_wrong_one_is_401() {
        let (m, u) = get("/v1/snapshot");
        let mut c = cfg();
        c.loopback_bypass = false;

        let ok = authorize(
            &c,
            Some("s3cret"),
            None,
            &bind(),
            &m,
            &u,
            &headers(&[("host", CONTROL), ("authorization", "Bearer s3cret")]),
        )
        .expect("the configured token must work with no bypass at all");
        assert_eq!(ok.via, AuthVia::Token);
        assert_eq!(ok.scope, Scope::Admin);

        let bad = authorize(
            &c,
            Some("s3cret"),
            peer("127.0.0.1:5"),
            &bind(),
            &m,
            &u,
            &headers(&[("host", CONTROL), ("authorization", "Bearer wrong")]),
        );
        assert_eq!(
            status_of(&bad.expect_err("a wrong token must not fall through to the bypass")),
            StatusCode::UNAUTHORIZED,
            "presenting a wrong token is an error, not an invitation to try the bypass"
        );
    }

    /// `?token=` is an accepted presentation — which is precisely why the trace span must
    /// not record the query string. See [`the_trace_span_never_records_the_query_string`].
    #[test]
    fn a_query_token_authenticates() {
        let mut c = cfg();
        c.loopback_bypass = false;
        let u: Uri = "/v1/snapshot?token=s3cret".parse().expect("uri");
        let auth = authorize(
            &c,
            Some("s3cret"),
            None,
            &bind(),
            &Method::GET,
            &u,
            &headers(&local_host()),
        )
        .expect("?token= must authenticate");
        assert_eq!(auth.via, AuthVia::Token);
    }

    // ---- the scope table ---------------------------------------------------------------

    #[test]
    fn the_scope_table_is_the_documented_one() {
        assert_eq!(required_scope("/v1/snapshot", &Method::GET), Scope::Read);
        assert_eq!(required_scope("/v1/routes", &Method::PUT), Scope::Write);
        assert_eq!(
            required_scope("/v1/backends/x", &Method::DELETE),
            Scope::Write
        );
        assert_eq!(required_scope("/switch", &Method::POST), Scope::Write);
        assert_eq!(required_scope("/v1/shutdown", &Method::POST), Scope::Admin);
        assert_eq!(
            required_scope("/v1/tokens", &Method::GET),
            Scope::Admin,
            "/v1/tokens* is ALWAYS admin, even to list"
        );
        assert_eq!(
            required_scope("/v1/tokens/abc", &Method::DELETE),
            Scope::Admin
        );
        assert_eq!(
            required_scope("/v1/chat/completions", &Method::POST),
            Scope::Read,
            "inference mutates nothing this daemon owns"
        );
        assert_eq!(
            required_scope("/v1/v1/chat/completions", &Method::POST),
            Scope::Read,
            "the doubled-/v1 collapse must not change the classification"
        );
        assert!(Scope::Read < Scope::Write && Scope::Write < Scope::Admin);
    }

    // ---- the mutation gate: rule 1, DNS rebinding ---------------------------------------

    #[test]
    fn a_rebinding_host_is_rejected() {
        let h = headers(&[("host", "evil.com")]);
        let r = require_mutation_origin_with(&h, &bind(), &cfg(), None);
        let refusal = r.expect_err("Host: evil.com is a DNS-rebinding attempt");
        assert_eq!(status_of(&refusal), StatusCode::FORBIDDEN);

        // Even on our own port: the *name* is the attack.
        let h = headers(&[("host", "evil.com:2739")]);
        assert!(require_mutation_origin_with(&h, &bind(), &cfg(), None).is_err());
    }

    #[test]
    fn the_hosts_a_local_client_actually_sends_are_allowed() {
        for host in [
            "127.0.0.1:2739",
            "localhost:2739",
            "LOCALHOST:2739",
            "[::1]:2739",
            "127.0.0.1:8888",   // the proxy listener's port, same gate
            "192.168.1.9:2739", // an IP literal cannot be a rebinding target
        ] {
            let h = headers(&[("host", host)]);
            assert!(
                require_mutation_origin_with(&h, &bind(), &cfg(), None).is_ok(),
                "{host} must pass rule 1"
            );
        }
        let h = headers(&[("host", "127.0.0.1:31337")]);
        assert!(
            require_mutation_origin_with(&h, &bind(), &cfg(), None).is_err(),
            "a port this daemon does not listen on is not us"
        );
    }

    #[test]
    fn no_host_header_is_refused() {
        let r = require_mutation_origin_with(&HeaderMap::new(), &bind(), &cfg(), None);
        assert_eq!(
            status_of(
                &r.expect_err("HTTP/1.1 requires a Host; its absence is not a first-party client")
            ),
            StatusCode::FORBIDDEN
        );
    }

    // ---- the mutation gate: rule 2, CSRF -------------------------------------------------

    #[test]
    fn a_cross_origin_mutation_is_rejected() {
        let h = headers(&[("host", CONTROL), ("origin", "http://evil.com")]);
        let r = require_mutation_origin_with(&h, &bind(), &cfg(), None);
        assert_eq!(
            status_of(&r.expect_err("a page on another origin may not mutate this daemon")),
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn a_cross_site_fetch_is_rejected_even_without_an_origin_header() {
        let h = headers(&[("host", CONTROL), ("sec-fetch-site", "cross-site")]);
        assert!(require_mutation_origin_with(&h, &bind(), &cfg(), None).is_err());
        let h = headers(&[("host", CONTROL), ("sec-fetch-site", "same-site")]);
        assert!(
            require_mutation_origin_with(&h, &bind(), &cfg(), None).is_err(),
            "same-site is not same-origin: a sibling port is a different origin"
        );
    }

    #[test]
    fn an_opaque_origin_is_not_same_origin() {
        let h = headers(&[("host", CONTROL), ("origin", "null")]);
        assert!(
            require_mutation_origin_with(&h, &bind(), &cfg(), None).is_err(),
            "`Origin: null` is a sandboxed iframe or a file:// page"
        );
    }

    /// The acceptance case this whole gate is designed around: `curl`, the CLI and the Slint
    /// app send neither header and must be completely unaffected.
    #[test]
    fn curl_the_cli_and_slint_pass_unchanged() {
        let h = headers(&[("host", CONTROL), ("user-agent", "curl/8.5.0")]);
        assert!(require_mutation_origin_with(&h, &bind(), &cfg(), None).is_ok());

        let same = headers(&[
            ("host", CONTROL),
            ("origin", "http://127.0.0.1:2739"),
            ("sec-fetch-site", "same-origin"),
        ]);
        assert!(
            require_mutation_origin_with(&same, &bind(), &cfg(), None).is_ok(),
            "the embedded web UI is same-origin and must work"
        );
    }

    // ---- the mutation gate: rule 3, the bearer fallback ----------------------------------

    #[test]
    fn a_cross_origin_mutation_with_a_write_bearer_is_allowed() {
        let h = headers(&[
            ("host", CONTROL),
            ("origin", "http://studio.example"),
            ("authorization", "Bearer s3cret"),
        ]);
        assert!(
            require_mutation_origin_with(&h, &bind(), &cfg(), Some("s3cret")).is_ok(),
            "a deliberate client with the operator's token is not a CSRF victim"
        );
        let wrong = headers(&[
            ("host", CONTROL),
            ("origin", "http://studio.example"),
            ("authorization", "Bearer nope"),
        ]);
        assert!(require_mutation_origin_with(&wrong, &bind(), &cfg(), Some("s3cret")).is_err());
    }

    /// Ordering matters more than either rule: a page on `evil.com` in a browser *on this
    /// laptop* has a loopback peer IP, so if the bypass ran first, `loopback_bypass = true`
    /// (the default) would authenticate every CSRF attempt.
    #[test]
    fn the_mutation_gate_runs_before_the_loopback_bypass() {
        let (m, u) = post("/v1/routes/default");
        let h = headers(&[("host", CONTROL), ("origin", "http://evil.com")]);
        let r = authorize(&cfg(), None, peer("127.0.0.1:44444"), &bind(), &m, &u, &h);
        assert_eq!(
            status_of(&r.expect_err("the bypass must not rescue a cross-origin mutation")),
            StatusCode::FORBIDDEN
        );
    }

    /// Rebinding is not only a write attack: once `evil.com` resolves to `127.0.0.1` the
    /// attacker's page is same-origin with us and `GET /v1/snapshot` is readable. Rule 1
    /// therefore gates reads too — but rules 2 and 3 do not, or the whole point of
    /// `[server] proxy_cors_origins` (a browser app that may *read* cross-origin) is dead on
    /// arrival.
    #[test]
    fn a_rebinding_read_is_refused_but_a_cross_origin_read_is_not() {
        let (m, u) = get("/v1/snapshot");

        let rebound = headers(&[("host", "evil.com"), ("origin", "http://evil.com")]);
        let r = authorize(&cfg(), None, peer("127.0.0.1:1"), &bind(), &m, &u, &rebound);
        assert_eq!(
            status_of(&r.expect_err("a rebound page must not read the control plane")),
            StatusCode::FORBIDDEN
        );

        let cross = headers(&[("host", CONTROL), ("origin", "http://studio.example")]);
        assert!(
            authorize(&cfg(), None, peer("127.0.0.1:1"), &bind(), &m, &u, &cross).is_ok(),
            "a cross-origin READ with a correct Host is not CSRF"
        );

        // A reverse proxy rewriting Host is legitimate — and holds the token, because a
        // non-loopback bind refuses to start without one.
        let proxied = headers(&[
            ("host", "apexrouter.lan"),
            ("authorization", "Bearer s3cret"),
        ]);
        assert!(
            authorize(
                &cfg(),
                Some("s3cret"),
                peer("10.0.0.4:5"),
                &bind(),
                &m,
                &u,
                &proxied
            )
            .is_ok(),
            "a bearer overrides the Host allowlist; a browser cannot forge one"
        );
    }

    // ---- refusing to bind ---------------------------------------------------------------

    #[test]
    fn a_non_loopback_bind_without_a_configured_token_refuses_to_start() {
        let mut c = cfg();
        c.proxy_bind = "0.0.0.0:8888".to_owned();
        let e = check_binds(&c, false).expect_err("0.0.0.0 without auth must refuse to start");
        let msg = format!("{e}");
        assert!(
            msg.contains("0.0.0.0:8888"),
            "the message names the bind: {msg}"
        );
        assert!(
            msg.contains("APEXROUTER_TOKEN"),
            "the message carries the fix, not just the fact: {msg}"
        );
        assert!(
            check_binds(&c, true).is_ok(),
            "with a token configured the same bind is allowed"
        );

        let mut lan = cfg();
        lan.control_bind = "192.168.1.9:2739".to_owned();
        assert!(check_binds(&lan, false).is_err());

        assert!(
            check_binds(&cfg(), false).is_ok(),
            "the shipped loopback defaults must start with no configuration at all"
        );

        let mut broken = cfg();
        broken.control_bind = "not-an-address".to_owned();
        assert!(
            check_binds(&broken, false).is_err(),
            "an unparseable bind must not be guessed generously"
        );
    }

    // ---- the trace span -----------------------------------------------------------------

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<(String, String)>>>);

    struct Visitor<'a>(&'a mut Vec<(String, String)>);

    impl tracing::field::Visit for Visitor<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0.push((field.name().to_owned(), format!("{value:?}")));
        }
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.push((field.name().to_owned(), value.to_owned()));
        }
    }

    impl<S> tracing_subscriber::Layer<S> for Capture
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            _id: &tracing::span::Id,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut seen = self.0.lock().expect("capture lock");
            attrs.record(&mut Visitor(&mut seen));
        }
    }

    /// `?token=` is an accepted presentation ([`extract_presented_token`]), so the span
    /// wrapped around every request must never record the URI — it would write live
    /// credentials into every log line and every trace exporter.
    ///
    /// The span factory itself is S-01's (`crate::request_span`, wired into both listeners'
    /// `TraceLayer`); this test is S-02's, because the reason it must not record a query is
    /// a fact about this module. It captures the span's actual recorded fields rather than
    /// reading the source, so a later `DefaultMakeSpan` regression is caught here even
    /// though the line that changed lives in another unit's file.
    #[test]
    fn the_trace_span_never_records_the_query_string() {
        use tracing_subscriber::layer::SubscriberExt;

        let cap = Capture::default();
        let subscriber = tracing_subscriber::registry().with(cap.clone());
        tracing::subscriber::with_default(subscriber, || {
            let req = axum::http::Request::builder()
                .method(Method::POST)
                .uri("/v1/routes/default?token=s3cret-do-not-log&alias=auto")
                .body(axum::body::Body::empty())
                .expect("request");
            let _span = crate::request_span(&req);
        });

        let seen = cap.0.lock().expect("capture lock").clone();
        let names: Vec<&str> = seen.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            names,
            vec!["method", "path"],
            "the span records method and path and nothing else"
        );
        for (name, value) in &seen {
            assert!(
                !value.contains("s3cret") && !value.contains('?'),
                "{name} leaked the query string: {value}"
            );
        }
        assert_eq!(seen[0].1, "POST");
        assert_eq!(seen[1].1, "/v1/routes/default");
    }
}
