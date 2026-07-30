//! OWNER: unit CL-01 (client/src/lib.rs, client/src/ws.rs). Do not edit outside that unit.
//!
//! The WebSocket half of [`crate::NodeClient`]: connect, decode `Event`s, and reconnect with
//! exponential backoff (1 s → ×2 → cap 15 s), re-emitting a full snapshot on reconnect so a
//! surface never renders a stale picture after a blip.
//!
//! The re-emit is done by this crate rather than trusted to the daemon. `GET /ws` does send a
//! `Snapshot` first (ARCHITECTURE §6.2), but a client that only *hopes* for it renders a
//! frozen dashboard the one time it does not arrive — and the reconnect is exactly the moment
//! the picture is least trustworthy, because everything that happened while the socket was
//! down was never broadcast to us. So on every reconnect we `GET /v1/snapshot` over HTTP and
//! emit it ourselves. A duplicate `Snapshot` is harmless: it is a whole-state replace.
//!
//! The same module carries the SSE reader, because `/v1/diagnose` and a `?follow=1` log tail
//! are the same problem — a long-lived stream of `Event`s — reached over a different
//! transport. Framing is done on **bytes**, not on a `String`, because a chunk boundary can
//! land in the middle of a UTF-8 sequence.

use crate::{decode, snippet, Error, Event, NodeClient, Result, STREAM_TIMEOUT};
use futures_util::{Stream, StreamExt};
use std::collections::VecDeque;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::http::Uri;
use tokio_tungstenite::tungstenite::{ClientRequestBuilder, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// A connected control-plane socket.
pub type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The first reconnect delay.
pub const BACKOFF_START: Duration = Duration::from_secs(1);

/// The reconnect delay never grows past this.
pub const BACKOFF_CAP: Duration = Duration::from_secs(15);

/// 1 s → ×2 → cap 15 s.
///
/// Capped rather than unbounded because a daemon that is down is usually being restarted:
/// a fifteen-second worst case is the difference between a GUI that comes back by itself
/// and one the operator has to restart.
pub fn next_backoff(current: Duration) -> Duration {
    let doubled = current.saturating_mul(2);
    if doubled > BACKOFF_CAP {
        BACKOFF_CAP
    } else {
        doubled
    }
}

/// Turn a control-plane base URL into a WebSocket URL for `path`.
///
/// `http` → `ws`, `https` → `wss`, a bare `host:port` → `ws`. Anything else is an error
/// rather than a guess, because silently dialling the wrong scheme is a very confusing
/// failure to debug.
pub fn ws_url(base: &str, path: &str) -> Result<String> {
    let base = base.trim();
    // Split the scheme *before* trimming slashes, or `http://` degrades into the host
    // `http:` and we cheerfully dial it.
    let (scheme, rest) = match base.split_once("://") {
        Some(("http", r)) | Some(("ws", r)) => ("ws", r),
        Some(("https", r)) | Some(("wss", r)) => ("wss", r),
        Some((other, _)) => {
            return Err(Error::Url(format!(
                "unsupported scheme `{other}` in `{base}` (want http, https, ws or wss)"
            )))
        }
        None => ("ws", base),
    };
    let rest = rest.trim_end_matches('/');
    if rest.is_empty() {
        return Err(Error::Url(format!("no host in `{base}`")));
    }
    if path.starts_with('/') {
        Ok(format!("{scheme}://{rest}{path}"))
    } else {
        Ok(format!("{scheme}://{rest}/{path}"))
    }
}

/// Open one `/ws` connection, presenting the bearer as an `Authorization` header.
///
/// The header rather than `?token=`: §9.4 accepts both, but a query string ends up in
/// proxy access logs that the header does not.
pub async fn connect(base: &str, token: Option<&str>) -> Result<Socket> {
    let url = ws_url(base, "/ws")?;
    let uri: Uri = url.parse().map_err(|e| Error::Url(format!("{url}: {e}")))?;
    let mut req = ClientRequestBuilder::new(uri);
    if let Some(t) = token {
        req = req.with_header("Authorization", format!("Bearer {t}"));
    }
    let (socket, _res) = tokio_tungstenite::connect_async(req)
        .await
        .map_err(|e| Error::Ws(format!("connect {url}: {e}")))?;
    Ok(socket)
}

/// The reconnecting `/ws` subscription behind [`crate::NodeClient::subscribe`].
///
/// The first connection is eager, so "the daemon is not running" is an `Err` from this
/// function. Afterwards the stream is endless: a drop yields one [`Error::Ws`] item, then
/// the backoff runs, then a fresh `Snapshot` is emitted.
pub async fn subscribe(client: NodeClient) -> Result<impl Stream<Item = Result<Event>>> {
    let socket = connect(client.base(), client.token()).await?;
    let state = Sub {
        client,
        socket: Some(socket),
        backoff: BACKOFF_START,
        pending: VecDeque::new(),
    };
    Ok(futures_util::stream::unfold(state, |s| async move {
        sub_step(s).await
    }))
}

/// Everything the subscription carries between yields.
struct Sub {
    client: NodeClient,
    socket: Option<Socket>,
    backoff: Duration,
    pending: VecDeque<Result<Event>>,
}

/// Produce the next item, reconnecting as many times as it takes.
async fn sub_step(mut st: Sub) -> Option<(Result<Event>, Sub)> {
    loop {
        if let Some(item) = st.pending.pop_front() {
            return Some((item, st));
        }

        if st.socket.is_none() {
            tokio::time::sleep(st.backoff).await;
            match connect(st.client.base(), st.client.token()).await {
                Ok(socket) => {
                    st.socket = Some(socket);
                    st.backoff = BACKOFF_START;
                    // Re-emit the snapshot. A failure here is not fatal — the daemon's own
                    // first frame still carries one.
                    if let Ok(snap) = st.client.snapshot().await {
                        st.pending.push_back(Ok(Event::Snapshot(Box::new(snap))));
                    }
                }
                Err(e) => {
                    st.backoff = next_backoff(st.backoff);
                    return Some((Err(e), st));
                }
            }
            continue;
        }

        let next = match st.socket.as_mut() {
            Some(s) => s.next().await,
            None => continue,
        };
        match next {
            Some(Ok(Message::Text(t))) => return Some((decode("/ws", &t), st)),
            Some(Ok(Message::Binary(b))) => {
                let t = String::from_utf8_lossy(&b).into_owned();
                return Some((decode("/ws", &t), st));
            }
            Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => continue,
            Some(Ok(Message::Close(_))) | None => {
                st.socket = None;
                return Some((Err(Error::Ws("/ws closed by the daemon".into())), st));
            }
            Some(Err(e)) => {
                st.socket = None;
                return Some((Err(Error::Ws(format!("/ws: {e}"))), st));
            }
        }
    }
}

/// The SSE reader behind [`crate::NodeClient::sse`].
///
/// The status is checked and the body read as text **before** anything is parsed, exactly as
/// on the JSON path, so a 500 HTML page from a reverse proxy is an [`Error::Status`] and not
/// a stream that yields one decode failure per chunk.
pub async fn sse(client: NodeClient, path: &str) -> Result<impl Stream<Item = Result<Event>>> {
    let url = if path.starts_with('/') {
        format!("{}{}", client.base(), path)
    } else {
        format!("{}/{}", client.base(), path)
    };
    let mut rb = client
        .http()
        .get(&url)
        .header("Accept", "text/event-stream")
        .timeout(STREAM_TIMEOUT);
    if let Some(t) = client.token() {
        rb = rb.bearer_auth(t);
    }
    let res = rb.send().await?;
    let status = res.status();
    if !status.is_success() {
        let body = res.text().await?;
        return Err(Error::Status {
            status: status.as_u16(),
            path: path.to_string(),
            body: snippet(&body),
        });
    }

    let state = SseState {
        body: Box::pin(res.bytes_stream()),
        buf: Vec::new(),
        pending: VecDeque::new(),
        path: path.to_string(),
        done: false,
    };
    Ok(futures_util::stream::unfold(state, |s| async move {
        sse_step(s).await
    }))
}

/// Everything the SSE reader carries between yields.
struct SseState<S> {
    body: S,
    buf: Vec<u8>,
    pending: VecDeque<Result<Event>>,
    path: String,
    done: bool,
}

/// Produce the next SSE-borne event.
///
/// Generic over the byte stream so the crate never has to name `bytes::Bytes`, which is not
/// one of its declared dependencies.
async fn sse_step<S, B>(mut st: SseState<S>) -> Option<(Result<Event>, SseState<S>)>
where
    S: Stream<Item = std::result::Result<B, reqwest::Error>> + Unpin,
    B: AsRef<[u8]>,
{
    loop {
        if let Some(item) = st.pending.pop_front() {
            return Some((item, st));
        }
        if st.done {
            return None;
        }
        match st.body.next().await {
            Some(Ok(chunk)) => {
                st.buf.extend_from_slice(chunk.as_ref());
                if drain_blocks(&mut st.buf, &mut st.pending, &st.path) {
                    st.done = true;
                }
            }
            Some(Err(e)) => {
                st.done = true;
                return Some((Err(Error::Http(e)), st));
            }
            None => {
                // A final block with no trailing blank line is still a block.
                st.done = true;
                let tail = std::mem::take(&mut st.buf);
                let block = String::from_utf8_lossy(&tail).into_owned();
                if let Some(data) = block_data(&block) {
                    push_event(&mut st.pending, &st.path, &data);
                }
                if st.pending.is_empty() {
                    return None;
                }
            }
        }
    }
}

/// Pull every complete SSE block out of `buf`, leaving the trailing partial behind.
///
/// Returns `true` when a `[DONE]` sentinel was seen, which ends the stream.
fn drain_blocks(buf: &mut Vec<u8>, pending: &mut VecDeque<Result<Event>>, path: &str) -> bool {
    while let Some((end, next)) = find_terminator(buf) {
        let block = String::from_utf8_lossy(&buf[..end]).into_owned();
        buf.drain(..next);
        if let Some(data) = block_data(&block) {
            if data.trim() == "[DONE]" {
                return true;
            }
            push_event(pending, path, &data);
        }
    }
    false
}

/// Parse one `data:` payload into an [`Event`], queuing the decode failure if it is not one.
fn push_event(pending: &mut VecDeque<Result<Event>>, path: &str, data: &str) {
    pending.push_back(decode(path, data));
}

/// Find the blank line ending an SSE block: `\n\n` or `\r\n\r\n`.
///
/// Returns `(end of the block, start of the next one)`.
fn find_terminator(b: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0usize;
    while i + 1 < b.len() {
        if b[i] == b'\r'
            && i + 3 < b.len()
            && b[i + 1] == b'\n'
            && b[i + 2] == b'\r'
            && b[i + 3] == b'\n'
        {
            return Some((i, i + 4));
        }
        if b[i] == b'\n' && b[i + 1] == b'\n' {
            return Some((i, i + 2));
        }
        i += 1;
    }
    None
}

/// The concatenated `data:` lines of one SSE block, or `None` for a comment/keep-alive.
///
/// Multiple `data:` lines join with `\n`, per the EventSource grammar; `event:`, `id:` and
/// `retry:` are accepted and ignored, because the control plane carries the discriminator
/// inside the `Event` JSON itself.
fn block_data(block: &str) -> Option<String> {
    let mut data = String::new();
    let mut any = false;
    for raw in block.split('\n') {
        let line = raw.trim_end_matches('\r');
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => {
                let v = if let Some(stripped) = v.strip_prefix(' ') {
                    stripped
                } else {
                    v
                };
                (f, v)
            }
            None => (line, ""),
        };
        if field == "data" {
            if any {
                data.push('\n');
            }
            data.push_str(value);
            any = true;
        }
    }
    if any {
        Some(data)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{read_request, respond};
    use apexrouter_protocol::{
        Alias, CostEstimate, LogSource, Money, ProxyStatus, RigSnapshot, ServedBy, Snapshot, Totals,
    };
    use futures_util::SinkExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    fn test_snapshot(marker: &str) -> Snapshot {
        Snapshot {
            product: "apexrouter".into(),
            version: marker.into(),
            served_by: ServedBy::Daemon,
            as_of_unix: 1_780_000_000,
            stale: false,
            proxy: ProxyStatus {
                base_url: "http://127.0.0.1:8888".into(),
                control_url: "http://127.0.0.1:2739".into(),
                uptime_secs: 1.0,
                inflight: 0,
                req_per_min: 0.0,
                tok_per_s: 0.0,
                default_alias: Alias::parse("auto").expect("alias"),
                table_valid: true,
                table_error: None,
            },
            backends: vec![],
            routes: vec![],
            endpoints: vec![],
            rig: RigSnapshot {
                gpus: vec![],
                builds: vec![],
                ram_total_mb: 24_000,
                ram_free_mb: 8_000,
                swap_total_mb: 8_000,
                swap_used_mb: 0,
                cpu_threads: 12,
                scanned_at_unix: 1_780_000_000,
            },
            instances: vec![],
            tunnels: vec![],
            providers: vec![],
            recipes: vec![],
            profiles: vec![],
            totals: Totals {
                spend_24h: CostEstimate::Unknown,
                spend_7d: CostEstimate::Unknown,
                tokens_24h: 0,
                vast_credit: None,
                burn_rate_usd_hr: Money::ZERO,
                burn_down_hours: None,
            },
            alerts: vec![],
            jobs: vec![],
        }
    }

    #[test]
    fn ws_url_maps_http_to_ws_and_https_to_wss() {
        assert_eq!(
            ws_url("http://127.0.0.1:2739", "/ws").expect("ws"),
            "ws://127.0.0.1:2739/ws"
        );
        assert_eq!(
            ws_url("http://127.0.0.1:2739/", "ws").expect("ws"),
            "ws://127.0.0.1:2739/ws"
        );
        assert_eq!(
            ws_url("https://node.example:8443", "/ws").expect("ws"),
            "wss://node.example:8443/ws"
        );
        assert_eq!(
            ws_url("ws://127.0.0.1:2739", "/ws").expect("ws"),
            "ws://127.0.0.1:2739/ws"
        );
        assert_eq!(
            ws_url("127.0.0.1:2739", "/ws").expect("ws"),
            "ws://127.0.0.1:2739/ws"
        );
        assert!(matches!(ws_url("ftp://host", "/ws"), Err(Error::Url(_))));
        assert!(matches!(ws_url("http://", "/ws"), Err(Error::Url(_))));
    }

    #[test]
    fn backoff_is_one_second_doubling_to_a_fifteen_second_cap() {
        let mut d = BACKOFF_START;
        let mut seen = vec![d];
        for _ in 0..6 {
            d = next_backoff(d);
            seen.push(d);
        }
        assert_eq!(
            seen,
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
                Duration::from_secs(15),
                Duration::from_secs(15),
                Duration::from_secs(15),
            ]
        );
    }

    #[test]
    fn sse_blocks_split_on_a_blank_line_in_either_line_ending() {
        let mut buf = b"data: {\"a\":1}\n\ndata: {\"b\":2}\r\n\r\ndata: part".to_vec();
        let mut got = Vec::new();
        while let Some((end, next)) = find_terminator(&buf) {
            got.push(String::from_utf8_lossy(&buf[..end]).into_owned());
            buf.drain(..next);
        }
        assert_eq!(got, vec!["data: {\"a\":1}", "data: {\"b\":2}"]);
        assert_eq!(buf, b"data: part", "the partial block stays buffered");
    }

    #[test]
    fn sse_data_lines_join_with_a_newline_and_metadata_is_ignored() {
        assert_eq!(
            block_data("event: check\ndata: one\ndata: two\nid: 7"),
            Some("one\ntwo".to_string())
        );
        assert_eq!(block_data(": keep-alive"), None);
        assert_eq!(block_data("event: ping"), None);
        // Exactly one leading space is stripped, per the EventSource grammar.
        assert_eq!(block_data("data:  x"), Some(" x".to_string()));
        assert_eq!(block_data("data:x"), Some("x".to_string()));
    }

    #[test]
    fn a_utf8_sequence_split_across_chunks_still_decodes() {
        // "…" is three bytes; feed it one byte at a time.
        let payload =
            r#"data: {"type":"log_line","source":{"src":"daemon"},"line":"…"}"#.to_string();
        let mut buf: Vec<u8> = Vec::new();
        let mut pending = VecDeque::new();
        for b in payload.as_bytes().iter().chain(b"\n\n") {
            buf.push(*b);
            drain_blocks(&mut buf, &mut pending, "/v1/diagnose");
        }
        assert_eq!(pending.len(), 1);
        match pending.pop_front() {
            Some(Ok(Event::LogLine { line, .. })) => assert_eq!(line, "…"),
            other => panic!("expected a LogLine, got {other:?}"),
        }
    }

    /// A loopback server that speaks both HTTP (`/v1/snapshot`) and WebSocket (`/ws`) on
    /// one port, so the reconnect path can be exercised end to end.
    async fn dual_server(ws_hits: Arc<AtomicUsize>) -> String {
        let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = l.local_addr().expect("addr");
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let hits = ws_hits.clone();
                tokio::spawn(async move {
                    // Peek, do not consume: the tungstenite handshake needs the bytes.
                    let mut probe = [0u8; 2048];
                    let n = loop {
                        let n = match s.peek(&mut probe).await {
                            Ok(n) => n,
                            Err(_) => return,
                        };
                        if probe[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                            break n;
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    };
                    let head = String::from_utf8_lossy(&probe[..n]).to_lowercase();

                    if head.contains("upgrade: websocket") {
                        let nth = hits.fetch_add(1, Ordering::SeqCst);
                        let ws = match tokio_tungstenite::accept_async(s).await {
                            Ok(w) => w,
                            Err(_) => return,
                        };
                        let (mut tx, _rx) = ws.split();
                        let snap = Event::Snapshot(Box::new(test_snapshot(&format!("ws-{nth}"))));
                        let _ = tx
                            .send(Message::Text(serde_json::to_string(&snap).expect("encode")))
                            .await;
                        let log = Event::LogLine {
                            source: LogSource::Daemon,
                            line: format!("conn {nth}"),
                        };
                        let _ = tx
                            .send(Message::Text(serde_json::to_string(&log).expect("encode")))
                            .await;
                        if nth == 0 {
                            // Drop the first connection on the floor to force a reconnect.
                            let _ = tx.close().await;
                        } else {
                            tokio::time::sleep(Duration::from_secs(60)).await;
                        }
                    } else {
                        let _ = read_request(&mut s).await;
                        let body =
                            serde_json::to_string(&test_snapshot("http")).expect("encode snapshot");
                        respond(&mut s, "200 OK", "application/json", &body).await;
                    }
                });
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn subscribe_reconnects_with_backoff_and_reemits_the_snapshot() {
        let hits = Arc::new(AtomicUsize::new(0));
        let base = dual_server(hits.clone()).await;
        let client = NodeClient::new(base, None);

        let started = Instant::now();
        let stream = client.subscribe().await.expect("first connect");
        futures_util::pin_mut!(stream);

        let mut got: Vec<std::result::Result<Event, String>> = Vec::new();
        for _ in 0..4 {
            let item = tokio::time::timeout(Duration::from_secs(20), stream.next())
                .await
                .expect("no stall")
                .expect("the subscription is endless");
            got.push(item.map_err(|e| e.to_string()));
        }

        // 1. the daemon's own snapshot on the first connection
        match &got[0] {
            Ok(Event::Snapshot(s)) => assert_eq!(s.version, "ws-0"),
            other => panic!("expected the connect snapshot, got {other:?}"),
        }
        // 2. a normal event
        match &got[1] {
            Ok(Event::LogLine { line, .. }) => assert_eq!(line, "conn 0"),
            other => panic!("expected a LogLine, got {other:?}"),
        }
        // 3. the drop, surfaced but not fatal
        match &got[2] {
            Err(msg) => assert!(msg.contains("websocket error"), "{msg}"),
            other => panic!("expected the disconnect, got {other:?}"),
        }
        // 4. the re-emitted snapshot, fetched over HTTP the moment the socket came back
        match &got[3] {
            Ok(Event::Snapshot(s)) => assert_eq!(
                s.version, "http",
                "the reconnect snapshot comes from GET /v1/snapshot"
            ),
            other => panic!("expected the reconnect snapshot, got {other:?}"),
        }

        assert!(
            hits.load(Ordering::SeqCst) >= 2,
            "the client must have dialled /ws twice"
        );
        assert!(
            started.elapsed() >= BACKOFF_START,
            "the reconnect must have waited out the backoff"
        );
    }

    #[tokio::test]
    async fn sse_yields_one_event_per_block_and_stops_at_done() {
        let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = l.local_addr().expect("addr");
        tokio::spawn(async move {
            let (mut s, _) = match l.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            let _ = read_request(&mut s).await;
            let head =
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
            let _ = s.write_all(head.as_bytes()).await;
            for line in ["one", "two"] {
                let ev = Event::LogLine {
                    source: LogSource::Daemon,
                    line: line.to_string(),
                };
                let frame = format!(
                    ": keep-alive\nevent: log\ndata: {}\n\n",
                    serde_json::to_string(&ev).expect("encode")
                );
                let _ = s.write_all(frame.as_bytes()).await;
                let _ = s.flush().await;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let _ = s.write_all(b"data: [DONE]\n\n").await;
            let _ = s.flush().await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let client = NodeClient::new(format!("http://{addr}"), None);
        let stream = client.sse("/v1/diagnose").await.expect("sse");
        futures_util::pin_mut!(stream);

        let mut lines = Vec::new();
        while let Some(item) = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("no stall")
        {
            match item.expect("event") {
                Event::LogLine { line, .. } => lines.push(line),
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(lines, vec!["one".to_string(), "two".to_string()]);
    }

    #[tokio::test]
    async fn an_sse_endpoint_that_answers_500_html_is_a_status_error() {
        let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = l.local_addr().expect("addr");
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let _ = read_request(&mut s).await;
                respond(
                    &mut s,
                    "502 Bad Gateway",
                    "text/html",
                    "<html><title>502 Bad Gateway</title></html>",
                )
                .await;
            }
        });
        let client = NodeClient::new(format!("http://{addr}"), None);
        match client.sse("/v1/diagnose").await {
            Err(Error::Status { status, path, body }) => {
                assert_eq!(status, 502);
                assert_eq!(path, "/v1/diagnose");
                assert!(body.contains("502 Bad Gateway"), "{body}");
            }
            Ok(_) => panic!("a 502 must not open a stream"),
            Err(other) => panic!("expected Error::Status, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn subscribe_reports_a_dead_daemon_immediately() {
        let addr = {
            let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            l.local_addr().expect("addr")
        };
        let client = NodeClient::new(format!("http://{addr}"), None);
        match client.subscribe().await {
            Err(Error::Ws(msg)) => assert!(msg.contains("connect ws://"), "{msg}"),
            Ok(_) => panic!("nothing is listening"),
            Err(other) => panic!("expected Error::Ws, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_subscription_can_be_moved_into_a_spawned_task() {
        // U-02 holds the /ws subscription in a background task; this is the compile-time
        // proof that the opaque stream type does not out-borrow the client.
        let hits = Arc::new(AtomicUsize::new(0));
        let base = dual_server(hits).await;
        let client = NodeClient::new(base, None);
        let handle = tokio::spawn(async move {
            let stream = client.subscribe().await.expect("connect");
            futures_util::pin_mut!(stream);
            stream.next().await.is_some()
        });
        assert!(handle.await.expect("join"));
    }
}
