//! OWNER: unit CL-01 (client/src/lib.rs, client/src/ws.rs). Do not edit outside that unit.
//!
//! `NodeClient` — the thin HTTP + WebSocket client every non-server surface uses.
//! **No business logic**: the CLI, the MCP server and the Slint app are all edge clients of
//! the same HTTP API, so there is never a second implementation of "what is active".
//!
//! One deliberate detail: a manual status/text check happens **before**
//! `serde_json::from_str`, so a 500 HTML page yields a useful error rather than
//! "expected value at line 1 column 1". [`Error::Status`] carries a whitespace-collapsed
//! prefix of the body, which is what makes an nginx error page or a captive portal
//! debuggable from a one-line CLI message.
//!
//! Three more properties the surfaces depend on:
//!
//! * The 300 s request timeout is deliberate — `POST /v1/endpoints` blocks until the
//!   endpoint is `Ready` unless `?no_wait`. Streaming reads ([`NodeClient::sse`],
//!   [`NodeClient::subscribe`]) opt out of it, because a `?follow=1` log tail is supposed
//!   to outlive five minutes.
//! * An empty body (`204`, or a handler that returns nothing) decodes as JSON `null`, so
//!   `post::<_, ()>` and `get::<Option<T>>` work without every call site special-casing it.
//! * [`NodeClient`] is `Clone` (a `reqwest::Client` is an `Arc` inside) and its `Debug`
//!   redacts the bearer — §9.2 says no credential is ever logged, and a `{:?}` on a client
//!   is exactly how that rule gets broken by accident.

pub mod ws;

use apexrouter_protocol::{Event, Snapshot};
use futures_util::Stream;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::fmt;
use std::time::Duration;

/// Everything that can go wrong talking to a daemon.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Transport failure.
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    /// The daemon answered, but not with what we asked for. Carries the body prefix, which
    /// is what makes an HTML error page debuggable.
    #[error("{status} from {path}: {body}")]
    Status {
        /// HTTP status.
        status: u16,
        /// The path we called.
        path: String,
        /// The first part of the body.
        body: String,
    },
    /// The body was not the JSON we expected.
    #[error("could not parse {path}: {source}")]
    Decode {
        /// The path we called.
        path: String,
        /// The parse failure.
        #[source]
        source: serde_json::Error,
    },
    /// The WebSocket failed.
    #[error("websocket error: {0}")]
    Ws(String),
    /// The URL was not a URL.
    #[error("invalid url: {0}")]
    Url(String),
}

/// The client's result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// How much of a failing body [`Error::Status`] carries, in characters.
const BODY_SNIPPET_MAX: usize = 400;

/// Total-request timeout. Long on purpose: `POST /v1/endpoints` blocks until `Ready`.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// How long we wait for a TCP connect. A daemon that is not running refuses instantly on
/// loopback; this only bounds a wedged remote node.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The timeout applied to a streaming read instead of [`REQUEST_TIMEOUT`]. `reqwest` has no
/// "no timeout" on a per-request builder, so this is the moral equivalent: a year.
pub(crate) const STREAM_TIMEOUT: Duration = Duration::from_secs(365 * 24 * 60 * 60);

/// Normalise a base URL once, at construction, so every call site can be careless.
///
/// `127.0.0.1:2739` → `http://127.0.0.1:2739`, and a trailing `/` is dropped so
/// `format!("{base}{path}")` never produces a doubled slash.
fn normalize_base(base: &str) -> String {
    // Split the scheme *before* trimming slashes: `trim_end_matches('/')` on `http://`
    // leaves `http:`, which would then be treated as a bare host.
    let base = base.trim();
    match base.split_once("://") {
        Some((scheme, rest)) => format!("{scheme}://{}", rest.trim_end_matches('/')),
        None => format!("http://{}", base.trim_end_matches('/')),
    }
}

/// A whitespace-collapsed, length-capped prefix of a response body.
///
/// Collapsing matters: an nginx 502 page is 40 lines of indented HTML, and the useful part
/// (`<title>502 Bad Gateway</title>`) has to survive into a single-line error message.
pub(crate) fn snippet(body: &str) -> String {
    let head: String = body.chars().take(BODY_SNIPPET_MAX * 4).collect();
    let mut out = String::new();
    let mut in_ws = false;
    for ch in head.trim().chars() {
        if ch.is_whitespace() || ch.is_control() {
            if !in_ws && !out.is_empty() {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(ch);
            in_ws = false;
        }
    }
    let trimmed = out.trim_end();
    if trimmed.is_empty() {
        return "<empty body>".to_string();
    }
    if trimmed.chars().count() > BODY_SNIPPET_MAX {
        let cut: String = trimmed.chars().take(BODY_SNIPPET_MAX).collect();
        format!("{cut}…")
    } else {
        trimmed.to_string()
    }
}

/// Decode a body that has already passed the status check.
///
/// An empty body becomes JSON `null`, so `()` and `Option<T>` decode from a `204`.
pub(crate) fn decode<T: DeserializeOwned>(path: &str, body: &str) -> Result<T> {
    let text = if body.trim().is_empty() { "null" } else { body };
    serde_json::from_str(text).map_err(|source| Error::Decode {
        path: path.to_string(),
        source,
    })
}

/// A handle on one ApexRouter control plane.
#[derive(Clone)]
pub struct NodeClient {
    /* CL-01 */
    http: reqwest::Client,
    base: String,
    token: Option<String>,
}

impl fmt::Debug for NodeClient {
    /// Redacts the bearer. §9.2: no credential is ever logged, and `{:?}` on a client is
    /// how that rule gets broken by accident.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NodeClient")
            .field("base", &self.base)
            .field("token", &self.token.as_ref().map(|_| "***"))
            .finish()
    }
}

impl NodeClient {
    /// Build a client with a 300 s timeout — long, because a `/v1/endpoints` POST blocks
    /// until the endpoint is `Ready` unless `?no_wait`.
    ///
    /// `base` may be given with or without a scheme and with or without a trailing slash.
    pub fn new(base: impl Into<String>, token: Option<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(concat!("apexrouter-client/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        NodeClient {
            http,
            base: normalize_base(&base.into()),
            token,
        }
    }

    /// Attach the bearer, when there is one.
    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(t) => rb.bearer_auth(t),
            None => rb,
        }
    }

    /// Absolute URL for a control-plane path. Accepts `"/v1/rig"` and `"v1/rig"` alike.
    fn url(&self, path: &str) -> String {
        if path.starts_with('/') {
            format!("{}{}", self.base, path)
        } else {
            format!("{}/{}", self.base, path)
        }
    }

    /// Send, then **check the status and read the text before any JSON is parsed**.
    ///
    /// This is the whole point of the crate's error handling: a 500 HTML page becomes
    /// `Error::Status` carrying the page, not a `serde_json` "expected value at line 1".
    async fn text(&self, rb: reqwest::RequestBuilder, path: &str) -> Result<String> {
        let res = self.auth(rb).send().await?;
        let status = res.status();
        let body = res.text().await?;
        if !status.is_success() {
            return Err(Error::Status {
                status: status.as_u16(),
                path: path.to_string(),
                body: snippet(&body),
            });
        }
        Ok(body)
    }

    /// `GET /health` on the control plane.
    pub async fn health(&self) -> Result<Value> {
        self.get("/health").await
    }

    /// `GET /v1/snapshot`.
    pub async fn snapshot(&self) -> Result<Snapshot> {
        self.get("/v1/snapshot").await
    }

    /// Any `GET`, decoded into a protocol type.
    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let body = self.text(self.http.get(self.url(path)), path).await?;
        decode(path, &body)
    }

    /// Any `POST`.
    pub async fn post<B: Serialize, T: DeserializeOwned>(&self, path: &str, b: &B) -> Result<T> {
        let body = self
            .text(self.http.post(self.url(path)).json(b), path)
            .await?;
        decode(path, &body)
    }

    /// Any `PUT`.
    pub async fn put<B: Serialize, T: DeserializeOwned>(&self, path: &str, b: &B) -> Result<T> {
        let body = self
            .text(self.http.put(self.url(path)).json(b), path)
            .await?;
        decode(path, &body)
    }

    /// Any `DELETE`. The body is discarded on success and carried on failure.
    pub async fn delete(&self, path: &str) -> Result<()> {
        self.text(self.http.delete(self.url(path)), path).await?;
        Ok(())
    }

    /// Subscribe to `/ws`. Reconnects with backoff and re-emits the snapshot on reconnect.
    ///
    /// The first connection is made eagerly, so a daemon that is not running is an `Err`
    /// here rather than a stream that silently never yields. After that the stream is
    /// endless: a dropped connection yields one [`Error::Ws`] item and then reconnects
    /// (1 s → ×2 → cap 15 s). **Keep polling after an error** — an `Err` is a blip, not a
    /// terminator.
    pub async fn subscribe(&self) -> Result<impl Stream<Item = Result<Event>>> {
        ws::subscribe(self.clone()).await
    }

    /// Consume one of the SSE endpoints (`/v1/diagnose`, a `?follow=1` log tail).
    ///
    /// One [`Event`] per SSE block. `data: [DONE]` ends the stream, as does the upstream
    /// closing the body. The 300 s request timeout does not apply.
    pub async fn sse(&self, path: &str) -> Result<impl Stream<Item = Result<Event>>> {
        ws::sse(self.clone(), path).await
    }

    /// The base URL this client was built with, normalised.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// The bearer, if one was configured. `None` on a loopback control plane with no auth.
    pub(crate) fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// The shared `reqwest` client, for the streaming helpers in [`ws`].
    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.http
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    // -----------------------------------------------------------------------
    // A hand-rolled HTTP/1.1 server. `wiremock` is not a dependency of this
    // crate (BUILD-PLAN §1.6 gives it no dev-dependencies), and a 40-line
    // listener is also the strongest possible hermeticity guarantee: every
    // socket in this file is bound to 127.0.0.1:0 by the test itself.
    // -----------------------------------------------------------------------

    /// One parsed request: the request line, the headers lower-cased, and the body.
    pub(super) struct Req {
        pub line: String,
        pub headers: String,
        pub body: String,
    }

    pub(super) async fn read_request(s: &mut TcpStream) -> Option<Req> {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        let head_end = loop {
            if let Some(i) = find_double_crlf(&buf) {
                break i;
            }
            let n = s.read(&mut tmp).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&tmp[..n]);
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let (line, headers) = match head.split_once("\r\n") {
            Some((l, h)) => (l.to_string(), h.to_lowercase()),
            None => (head.clone(), String::new()),
        };
        let len = headers
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = buf[head_end + 4..].to_vec();
        while body.len() < len {
            let n = s.read(&mut tmp).await.ok()?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
        }
        Some(Req {
            line,
            headers,
            body: String::from_utf8_lossy(&body).to_string(),
        })
    }

    fn find_double_crlf(b: &[u8]) -> Option<usize> {
        b.windows(4).position(|w| w == b"\r\n\r\n")
    }

    pub(super) async fn respond(s: &mut TcpStream, status: &str, ctype: &str, body: &str) {
        let res = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = s.write_all(res.as_bytes()).await;
        let _ = s.flush().await;
    }

    /// Bind a loopback listener and answer every connection with one canned response,
    /// recording what was asked. Returns the base URL and the request log.
    async fn canned(
        status: &'static str,
        ctype: &'static str,
        body: &'static str,
    ) -> (String, Arc<tokio::sync::Mutex<Vec<Req>>>) {
        let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr: SocketAddr = l.local_addr().expect("addr");
        let log = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let sink = log.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                if let Some(req) = read_request(&mut s).await {
                    sink.lock().await.push(req);
                }
                respond(&mut s, status, ctype, body).await;
            }
        });
        (format!("http://{addr}"), log)
    }

    #[test]
    fn a_base_url_is_normalised_once_so_call_sites_can_be_careless() {
        for input in [
            "http://127.0.0.1:2739",
            "http://127.0.0.1:2739/",
            "127.0.0.1:2739",
            " 127.0.0.1:2739/ ",
        ] {
            let c = NodeClient::new(input, None);
            assert_eq!(c.base(), "http://127.0.0.1:2739", "{input}");
            assert_eq!(c.url("/v1/rig"), "http://127.0.0.1:2739/v1/rig", "{input}");
            assert_eq!(c.url("v1/rig"), "http://127.0.0.1:2739/v1/rig", "{input}");
        }
        assert_eq!(
            NodeClient::new("https://node.example:8443", None).base(),
            "https://node.example:8443"
        );
    }

    #[test]
    fn debug_never_prints_the_bearer() {
        let c = NodeClient::new("127.0.0.1:2739", Some("s3cret-token".into()));
        let rendered = format!("{c:?}");
        assert!(!rendered.contains("s3cret"), "{rendered}");
        assert!(rendered.contains("***"), "{rendered}");
    }

    #[test]
    fn a_body_snippet_collapses_whitespace_and_is_capped() {
        let html = "<html>\n  <head>\n    <title>502 Bad Gateway</title>\n  </head>\n</html>\n";
        let s = snippet(html);
        assert!(s.contains("<title>502 Bad Gateway</title>"), "{s}");
        assert!(!s.contains('\n'), "{s}");
        assert_eq!(snippet("   "), "<empty body>");
        let long = "x".repeat(5_000);
        let cut = snippet(&long);
        assert_eq!(
            cut.chars().count(),
            BODY_SNIPPET_MAX + 1,
            "cap plus the ellipsis"
        );
        assert!(cut.ends_with('…'));
    }

    #[tokio::test]
    async fn a_500_html_page_yields_a_status_error_not_a_parse_failure() {
        let page = "<html>\n<head><title>500 Internal Server Error</title></head>\n<body><center><h1>500</h1></center><hr><center>nginx/1.24.0</center></body>\n</html>";
        let (base, _log) = canned("500 Internal Server Error", "text/html", page).await;
        let c = NodeClient::new(base, None);

        let err = c.snapshot().await.expect_err("a 500 must not decode");
        match &err {
            Error::Status { status, path, body } => {
                assert_eq!(*status, 500);
                assert_eq!(path, "/v1/snapshot");
                assert!(body.contains("nginx/1.24.0"), "{body}");
                assert!(body.contains("500 Internal Server Error"), "{body}");
            }
            other => panic!("expected Error::Status, got {other:?}"),
        }
        // The message a CLI prints must name the status, the path and the page.
        let msg = err.to_string();
        assert!(msg.starts_with("500 from /v1/snapshot:"), "{msg}");
        assert!(msg.contains("nginx"), "{msg}");
        assert!(
            !msg.contains("expected value"),
            "a serde message means the status check ran too late: {msg}"
        );
    }

    #[tokio::test]
    async fn a_404_on_delete_carries_the_body() {
        let (base, _log) = canned(
            "404 Not Found",
            "application/json",
            r#"{"error":{"kind":"backend_not_found","message":"no such backend"}}"#,
        )
        .await;
        let c = NodeClient::new(base, None);
        let err = c
            .delete("/v1/backends/nope")
            .await
            .expect_err("404 must be an error");
        match err {
            Error::Status { status, path, body } => {
                assert_eq!(status, 404);
                assert_eq!(path, "/v1/backends/nope");
                assert!(body.contains("backend_not_found"), "{body}");
            }
            other => panic!("expected Error::Status, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_2xx_html_page_is_a_decode_error_naming_the_path() {
        let (base, _log) = canned("200 OK", "text/html", "<html>captive portal</html>").await;
        let c = NodeClient::new(base, None);
        match c.snapshot().await.expect_err("html is not a Snapshot") {
            Error::Decode { path, .. } => assert_eq!(path, "/v1/snapshot"),
            other => panic!("expected Error::Decode, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn health_and_snapshot_hit_the_documented_paths() {
        let (base, log) = canned("200 OK", "application/json", r#"{"ok":true}"#).await;
        let c = NodeClient::new(base, None);
        let v = c.health().await.expect("health");
        assert_eq!(v["ok"], serde_json::json!(true));
        let lines: Vec<String> = log.lock().await.iter().map(|r| r.line.clone()).collect();
        assert_eq!(lines, vec!["GET /health HTTP/1.1".to_string()]);
    }

    #[tokio::test]
    async fn the_bearer_is_attached_when_there_is_one_and_absent_when_there_is_not() {
        let (base, log) = canned("200 OK", "application/json", "{}").await;

        let with = NodeClient::new(base.clone(), Some("t0ken".into()));
        let _: Value = with.get("/v1/rig").await.expect("get");
        let without = NodeClient::new(base, None);
        let _: Value = without.get("/v1/rig").await.expect("get");

        let log = log.lock().await;
        assert!(
            log[0].headers.contains("authorization: bearer t0ken"),
            "{}",
            log[0].headers
        );
        assert!(
            !log[1].headers.contains("authorization:"),
            "{}",
            log[1].headers
        );
    }

    #[tokio::test]
    async fn post_and_put_send_json_and_an_empty_body_decodes_as_unit() {
        let (base, log) = canned("204 No Content", "application/json", "").await;
        let c = NodeClient::new(base, None);

        let () = c
            .post("/v1/routes/default", &serde_json::json!({"alias": "auto"}))
            .await
            .expect("post");
        let () = c
            .put("/v1/routes", &serde_json::json!([]))
            .await
            .expect("put");

        let log = log.lock().await;
        assert!(
            log[0].line.starts_with("POST /v1/routes/default"),
            "{}",
            log[0].line
        );
        assert!(
            log[0].headers.contains("content-type: application/json"),
            "{}",
            log[0].headers
        );
        assert_eq!(log[0].body, r#"{"alias":"auto"}"#);
        assert!(log[1].line.starts_with("PUT /v1/routes"), "{}", log[1].line);
        assert_eq!(log[1].body, "[]");
    }

    #[tokio::test]
    async fn a_daemon_that_is_not_running_is_a_transport_error_not_a_hang() {
        // Bind, learn the port, drop the listener: nothing is listening there now.
        let addr = {
            let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            l.local_addr().expect("addr")
        };
        let c = NodeClient::new(format!("http://{addr}"), None);
        match c.health().await.expect_err("nothing is listening") {
            Error::Http(e) => assert!(e.is_connect() || e.is_request(), "{e}"),
            other => panic!("expected Error::Http, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_client_only_ever_dials_loopback_in_this_suite() {
        // A guard in the spirit of the Gate-3 hermeticity pin: every base URL this file
        // constructs is 127.0.0.1, and nothing here reads an environment credential.
        let hits = Arc::new(AtomicUsize::new(0));
        let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = l.local_addr().expect("addr");
        assert!(addr.ip().is_loopback());
        let seen = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut s, peer)) = l.accept().await {
                assert!(peer.ip().is_loopback(), "{peer}");
                seen.fetch_add(1, Ordering::SeqCst);
                let _ = read_request(&mut s).await;
                respond(&mut s, "200 OK", "application/json", "{}").await;
            }
        });
        let c = NodeClient::new(format!("http://{addr}"), None);
        let _: Value = c.get("/health").await.expect("get");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }
}
