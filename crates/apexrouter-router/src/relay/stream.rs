//! OWNER: unit R-05 (router/src/relay/stream.rs). Do not edit outside that unit.
//!
//! SSE relay and the usage tee.
//!
//! * Bytes go into `Body::from_stream` **byte-for-byte, never re-framed**.
//! * `Content-Type: text/event-stream` is forced **only** when upstream is 2xx *and* already
//!   says so, so a `400 {"error":…}` on a `stream:true` request reaches the client as JSON.
//! * **Never a total timeout on a stream** — only an inter-chunk idle timeout.
//! * Mid-stream upstream death emits exactly one synthetic
//!   `data: {"error":{…,"type":"upstream_unavailable"}}` frame plus `data: [DONE]`. Never a
//!   silent truncation.
//! * The tee is best-effort and **never gates the relay**; a malformed tail degrades the
//!   record to `TokenCount::Estimated`.
//!
//! # Shape of the implementation
//!
//! This is **the** streaming relay: `handler::relay` hands a `Committed` straight to
//! [`sse_response`] and there is no second copy of these rules anywhere in the crate.
//! [`sse_response`] is a thin adapter over two pieces that are independent of `Committed`
//! and of `InFlightGuard`, and are unit-tested directly:
//!
//! * `relay_stream` — the byte pump: idle timeout, synthetic frames and tee bookkeeping,
//!   with one exit point (`Drop`) that reports a [`StreamOutcome`] to a callback. Whether the
//!   client vanished or the upstream finished is decided by whether the upstream stream was
//!   still open at drop time, so a disconnect cannot be mistaken for a clean end.
//! * [`UsageTee`] — a bounded rolling tail, fed a `&[u8]` per chunk. It never copies the
//!   whole stream, never allocates proportionally to it, and never delays a byte.

use crate::attempt::Committed;
use crate::limits::InFlightGuard;
use crate::relay::headers::response_headers;
use apexrouter_core::config::RouterCfg;
use apexrouter_core::upstream::{parse_timings, parse_usage, Timings, UsageFields};
use apexrouter_protocol::TokenCount;
use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use std::collections::VecDeque;
use std::pin::Pin;
use std::time::{Duration, Instant};

/// The synthetic frame emitted when an upstream dies in the middle of an SSE stream.
const MID_STREAM_FRAME: &[u8] = b"data: {\"error\":{\"message\":\"upstream ended mid-stream\",\"type\":\"upstream_unavailable\"}}\n\n";
/// The synthetic frame emitted when the inter-chunk idle timeout expires.
const IDLE_FRAME: &[u8] =
    b"data: {\"error\":{\"message\":\"upstream idle timeout\",\"type\":\"upstream_timeout\"}}\n\n";
/// The SSE terminator every OpenAI client waits for.
const DONE_FRAME: &[u8] = b"data: [DONE]\n\n";
/// How much of the stream tail the tee keeps, per buffer. Two buffers are held, so the tee
/// costs at most `2 * DEFAULT_TAIL_CAP` bytes plus one chunk, for any stream length.
const DEFAULT_TAIL_CAP: usize = 64 * 1024;

/// A pinned, error-erased upstream byte stream. Erasing the error type keeps the relay state
/// machine free of type parameters, which is what lets it carry a `Drop` impl.
type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, String>> + Send>>;

/// The four things the relay needs out of a committed upstream response.
struct Upstream {
    status: StatusCode,
    headers: HeaderMap,
    body: ByteStream,
    guard: Option<InFlightGuard>,
}

/// How the relay hands everything back when the response ends: the [`InFlightGuard`] it was
/// lent, and everything it learned.
///
/// Invoked from the relay's `Drop`, so it runs **exactly once** whether the stream ended
/// cleanly, the upstream died mid-stream, the idle timeout fired, or the client vanished —
/// and `StreamOutcome::aborted` says which. The caller (R-08) is what knows how to turn that
/// into a `RequestRecord`, so sealing the record, the ring buffer and the usage row all stay
/// on its side of the seam and this file stays free of `Router`.
pub type FinishFn = Box<dyn FnOnce(Option<InFlightGuard>, StreamOutcome) + Send>;

/// Turn a committed upstream response into a streaming axum response.
///
/// **This is the one SSE relay.** `handler::relay` calls it for every streamed response;
/// there is no second implementation of the frame rules, the idle timeout or the tee.
///
/// The returned body owns the `InFlightGuard`, so a client disconnect drops it, cancels the
/// reqwest future and aborts the upstream — freeing llama.cpp's slot — and `on_end` still
/// runs, with `StreamOutcome::aborted` set. The caller never has to remember to release
/// anything.
///
/// Response headers are built here: [`response_headers`] drops the hop-by-hop set, and
/// [`decorate_stream_headers`] adds the streaming-only ones — but **only** when the upstream
/// is 2xx and already said `text/event-stream`, so a `400 {"error":…}` on a `stream:true`
/// request reaches the client as the JSON it is. The observability stamp
/// (`X-ApexRouter-Route` and friends) is the caller's, applied to the returned response.
pub fn sse_response(c: Committed, cfg: &RouterCfg, on_end: FinishFn) -> axum::response::Response {
    let Upstream {
        status,
        headers,
        body,
        guard,
    } = upstream_parts(c);

    let sse = is_event_stream(status, &headers);
    let mut out_headers = response_headers(&headers);
    decorate_stream_headers(&mut out_headers, sse);

    let idle = Duration::from_millis(cfg.idle_timeout_ms);
    let settle: Box<dyn FnOnce(StreamOutcome) + Send> = Box::new(move |out| on_end(guard, out));

    let mut resp =
        axum::response::Response::new(Body::from_stream(relay_stream(body, idle, sse, settle)));
    *resp.status_mut() = status;
    *resp.headers_mut() = out_headers;
    resp
}

/// Take the committed upstream response apart.
///
/// The error type is erased to `String` here and nowhere else, which is what lets the relay
/// state machine below stay free of type parameters and therefore carry a `Drop` impl.
fn upstream_parts(mut c: Committed) -> Upstream {
    // The guard comes out first: `Committed::guard` is an `Option` precisely so the relay
    // can take ownership without the response having to be rebuilt around it.
    let guard = c.guard.take();
    let response = c.response;
    let status = response.status();
    let headers = response.headers().clone();
    let body: ByteStream = Box::pin(
        response
            .bytes_stream()
            .map(|r| r.map_err(|e: reqwest::Error| e.to_string())),
    );
    Upstream {
        status,
        headers,
        body,
        guard,
    }
}

/// Is this response an SSE stream we may decorate?
///
/// `Content-Type: text/event-stream` is forced **only** when the upstream is 2xx *and*
/// already says so, so a `400 {"error":…}` on a `stream:true` request reaches the client as
/// the JSON it is.
///
/// `pub(crate)` because R-08 asks the same question one step earlier — it has to decide
/// between this relay and the buffered one — and two spellings of one rule is how a
/// `TEXT/EVENT-STREAM` from some proxy ends up taking different branches in two files.
pub(crate) fn is_event_stream(status: StatusCode, upstream: &HeaderMap) -> bool {
    if !status.is_success() {
        return false;
    }
    upstream
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.trim()
                .to_ascii_lowercase()
                .starts_with("text/event-stream")
        })
}

/// Add the streaming-only response headers. A no-op for a buffered response.
///
/// `Content-Length` is dropped on a stream because a synthetic tail frame may be appended,
/// and `X-Usage` cannot exist on a stream (headers flush before the first chunk), so
/// `X-ApexRouter-Usage-Deferred: true` says so out loud instead.
fn decorate_stream_headers(h: &mut HeaderMap, sse: bool) {
    if !sse {
        return;
    }
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert("x-accel-buffering", HeaderValue::from_static("no"));
    h.insert(
        "x-apexrouter-usage-deferred",
        HeaderValue::from_static("true"),
    );
    h.remove(header::CONTENT_LENGTH);
}

/// Everything the relay learned about one response.
///
/// Handed to a [`FinishFn`] exactly once, from the relay's `Drop`.
#[derive(Debug, Default)]
pub struct StreamOutcome {
    /// Bytes relayed from the upstream. Synthetic frames are not counted.
    pub bytes: u64,
    /// Newlines seen — only ever used to estimate tokens nobody reported.
    pub newlines: u64,
    /// Reported usage, when the tail carried any.
    pub usage: Option<UsageFields>,
    /// llama.cpp `timings`, when the tail carried any.
    pub timings: Option<Timings>,
    /// The client vanished before the upstream finished.
    pub aborted: bool,
    /// The upstream ended or failed mid-stream.
    pub ended_mid_stream: bool,
    /// The inter-chunk idle timeout expired.
    pub idle_timeout: bool,
    /// Wall clock across the relay.
    pub total_ms: u32,
    /// Whether this was relayed as SSE.
    pub streamed: bool,
}

impl StreamOutcome {
    /// Reported prompt tokens, or `None` — a prompt cannot be estimated from the response.
    pub fn prompt_tokens(&self) -> Option<TokenCount> {
        self.usage.map(|u| TokenCount::Reported(u.prompt_tokens))
    }

    /// Reported completion tokens, degrading to `TokenCount::Estimated` on an absent or
    /// malformed tail. An SSE stream is estimated from its frame count (llama.cpp emits one
    /// frame per token), a buffered body from ~4 bytes per token. Never a silent zero.
    pub fn completion_tokens(&self) -> TokenCount {
        match self.usage {
            Some(u) => TokenCount::Reported(u.completion_tokens),
            None if self.streamed => TokenCount::Estimated(clamp_u32(self.newlines / 2)),
            None => TokenCount::Estimated(clamp_u32(self.bytes / 4)),
        }
    }

    /// Cached prompt tokens, from `usage.prompt_tokens_details` or llama.cpp's
    /// `timings.cache_n`, whichever the tail carried.
    pub fn cached_tokens(&self) -> Option<u32> {
        self.usage
            .and_then(|u| u.cached_tokens)
            .or_else(|| self.timings.map(|t| t.cache_n))
    }

    /// Throughput, **read** from the upstream's `timings`, never stopwatched.
    pub fn tok_per_s(&self) -> Option<f32> {
        self.timings.map(|t| t.predicted_per_second)
    }

    /// The error to record, when the stream did not end cleanly.
    pub fn error(&self) -> Option<&'static str> {
        if self.idle_timeout {
            Some("upstream idle timeout")
        } else if self.ended_mid_stream {
            Some("upstream ended mid-stream")
        } else {
            None
        }
    }
}

/// Saturate a count into the `u32` the protocol types use.
fn clamp_u32(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The byte pump. One exit point: `Drop`.
struct Relay {
    /// `None` once the upstream is finished — which is also how `Drop` tells a client
    /// disconnect from a clean end.
    inner: Option<ByteStream>,
    tee: UsageTee,
    idle: Duration,
    queue: VecDeque<&'static [u8]>,
    out: StreamOutcome,
    on_end: Option<Box<dyn FnOnce(StreamOutcome) + Send>>,
    started: Instant,
}

impl Relay {
    /// Queue the synthetic frame pair and return the first of it.
    fn synthesize(&mut self, frame: &'static [u8]) -> Option<Result<Bytes, std::io::Error>> {
        self.queue.push_back(frame);
        self.queue.push_back(DONE_FRAME);
        self.queue.pop_front().map(|f| Ok(Bytes::from_static(f)))
    }

    /// Produce the next relayed chunk, or `None` when the response is over.
    async fn next_item(&mut self) -> Option<Result<Bytes, std::io::Error>> {
        if let Some(frame) = self.queue.pop_front() {
            return Some(Ok(Bytes::from_static(frame)));
        }
        let inner = self.inner.as_mut()?;

        match tokio::time::timeout(self.idle, inner.next()).await {
            // An inter-chunk idle gap. This is the ONLY timeout on a stream.
            Err(_elapsed) => {
                self.inner = None;
                self.out.idle_timeout = true;
                tracing::warn!(idle_ms = self.idle.as_millis(), "upstream idle timeout");
                if self.out.streamed {
                    self.synthesize(IDLE_FRAME)
                } else {
                    Some(Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "upstream idle timeout",
                    )))
                }
            }
            // Clean EOF. On SSE without a `data: [DONE]` terminator that is a truncation.
            Ok(None) => {
                self.inner = None;
                if self.out.streamed && !self.tee.saw_done() {
                    self.out.ended_mid_stream = true;
                    tracing::warn!("upstream ended mid-stream (no [DONE] terminator)");
                    self.synthesize(MID_STREAM_FRAME)
                } else {
                    None
                }
            }
            // The upstream died.
            Ok(Some(Err(e))) => {
                self.inner = None;
                self.out.ended_mid_stream = true;
                tracing::warn!(error = %e, "upstream stream failed");
                if self.out.streamed {
                    self.synthesize(MID_STREAM_FRAME)
                } else {
                    // A JSON body cannot be truncated and then patched, so the client is
                    // told by a broken body rather than by a lie.
                    Some(Err(std::io::Error::other(e)))
                }
            }
            // A chunk. Relayed verbatim, never re-framed.
            Ok(Some(Ok(chunk))) => {
                self.out.bytes += chunk.len() as u64;
                self.tee.feed(&chunk);
                Some(Ok(chunk))
            }
        }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        let Some(on_end) = self.on_end.take() else {
            return;
        };
        let mut out = std::mem::take(&mut self.out);
        // The upstream stream still being open means the client went away first.
        out.aborted = self.inner.is_some();
        out.total_ms = u32::try_from(self.started.elapsed().as_millis()).unwrap_or(u32::MAX);
        out.newlines = self.tee.newlines();
        if let Some((usage, timings)) = std::mem::take(&mut self.tee).finish() {
            out.usage = Some(usage);
            out.timings = timings;
        }
        on_end(out);
    }
}

/// Build the relayed body stream.
///
/// `sse` selects the two behaviours that only make sense on an event stream: the synthetic
/// frame pair, and the `[DONE]`-terminator check.
fn relay_stream(
    inner: ByteStream,
    idle: Duration,
    sse: bool,
    on_end: Box<dyn FnOnce(StreamOutcome) + Send>,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    let relay = Relay {
        inner: Some(inner),
        tee: UsageTee::default(),
        idle,
        queue: VecDeque::new(),
        out: StreamOutcome {
            streamed: sse,
            ..StreamOutcome::default()
        },
        on_end: Some(on_end),
        started: Instant::now(),
    };
    futures_util::stream::unfold(relay, |mut relay| async move {
        let item = relay.next_item().await?;
        Some((item, relay))
    })
}

/// Watches the tail of a stream for `usage` / `timings`. Bounded buffer; never delays a byte.
///
/// Two buffers of at most `DEFAULT_TAIL_CAP` bytes are held and rotated, so `feed` is an
/// amortised `extend_from_slice` — never a re-copy of the tail, never an allocation
/// proportional to the stream. All parsing happens once, in [`UsageTee::finish`].
#[derive(Debug)]
pub struct UsageTee {
    /// The previous full buffer, kept so the tail is always at least `cap` bytes.
    prev: Vec<u8>,
    /// The buffer currently being filled.
    cur: Vec<u8>,
    /// Rotation threshold.
    cap: usize,
    /// Newlines seen across the whole stream — the fallback token estimate.
    newlines: u64,
}

impl Default for UsageTee {
    fn default() -> Self {
        UsageTee::with_capacity(DEFAULT_TAIL_CAP)
    }
}

impl UsageTee {
    /// A tee whose retained tail is between `cap` and `2 * cap` bytes.
    fn with_capacity(cap: usize) -> Self {
        UsageTee {
            prev: Vec::new(),
            cur: Vec::new(),
            cap: cap.max(1),
            newlines: 0,
        }
    }

    /// Feed one relayed chunk. Must not copy the whole stream.
    pub fn feed(&mut self, chunk: &[u8]) {
        self.newlines += chunk.iter().filter(|b| **b == b'\n').count() as u64;
        self.cur.extend_from_slice(chunk);
        if self.cur.len() >= self.cap {
            self.prev = std::mem::take(&mut self.cur);
        }
    }

    /// What the tail contained, if anything parseable.
    pub fn finish(self) -> Option<(UsageFields, Option<Timings>)> {
        let tail = self.tail();
        let text = String::from_utf8_lossy(&tail);

        let mut usage = None;
        let mut timings = None;

        // Newest frame first: the last chunk that carries usage wins.
        for line in text.lines().rev() {
            let Some(payload) = line.trim_end_matches('\r').strip_prefix("data:") else {
                continue;
            };
            let payload = payload.trim();
            if payload.is_empty() || payload == "[DONE]" {
                continue;
            }
            // A frame the tail window cut in half is skipped, never fatal.
            let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) else {
                continue;
            };
            if usage.is_none() {
                usage = parse_usage(&v);
            }
            if timings.is_none() {
                timings = parse_timings(&v);
            }
            if usage.is_some() && timings.is_some() {
                break;
            }
        }

        // A buffered body: the whole tail first, then a bounded key scan for a body whose
        // head has already fallen out of the window.
        if usage.is_none() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(text.trim()) {
                usage = parse_usage(&v);
                if timings.is_none() {
                    timings = parse_timings(&v);
                }
            }
        }
        if usage.is_none() {
            usage = object_after_key(&text, "usage")
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                .and_then(|v| parse_usage(&v));
        }
        if timings.is_none() {
            timings = object_after_key(&text, "timings")
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                .and_then(|v| parse_timings(&v));
        }

        usage.map(|u| (u, timings))
    }

    /// Did the stream carry its `data: [DONE]` terminator?
    fn saw_done(&self) -> bool {
        let tail = self.tail();
        let text = String::from_utf8_lossy(&tail);
        text.lines()
            .rev()
            .take(8)
            .any(|l| l.trim_end_matches('\r').trim() == "data: [DONE]")
    }

    /// Newlines seen so far.
    fn newlines(&self) -> u64 {
        self.newlines
    }

    /// The retained tail, oldest buffer first.
    fn tail(&self) -> Vec<u8> {
        if self.prev.is_empty() {
            return self.cur.clone();
        }
        let mut out = Vec::with_capacity(self.prev.len() + self.cur.len());
        out.extend_from_slice(&self.prev);
        out.extend_from_slice(&self.cur);
        out
    }

    /// Bytes currently retained. Bounded for any stream length.
    ///
    /// The bound is the whole point of the two-buffer rotation, so this exists to let the
    /// tests assert it. Nothing on the request path needs to ask.
    #[cfg(test)]
    fn buffered(&self) -> usize {
        self.prev.len() + self.cur.len()
    }
}

/// Find `"<key>": { … }` in a JSON fragment and return the balanced object, string-aware.
///
/// For a buffered body whose head has fallen out of the tail window, so a 4 MiB completion
/// still yields its `usage` from a 64 KiB buffer. Scans from the end, because `usage` and
/// `timings` are emitted last.
fn object_after_key<'a>(hay: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\"");
    let mut from = hay.rfind(&needle)?;
    loop {
        let after = &hay[from + needle.len()..];
        if let (Some(colon), Some(open)) = (after.find(':'), after.find('{')) {
            // The object must be this key's value, not some later one.
            if open > colon && after[colon + 1..open].trim().is_empty() {
                let start = from + needle.len() + open;
                if let Some(end) = balanced_end(&hay[start..]) {
                    return Some(&hay[start..start + end]);
                }
            }
        }
        from = hay[..from].rfind(&needle)?;
    }
}

/// Length of the balanced `{…}` starting at byte 0 of `s`, or `None` if it never closes.
fn balanced_end(s: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for (i, c) in s.bytes().enumerate() {
        if in_str {
            match c {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// A llama.cpp b9199 `/v1/chat/completions` capture with `stream_options.include_usage`
    /// on, so the penultimate frame carries both `usage` and `timings`.
    const LLAMA_SSE: &str = concat!(
        "data: {\"choices\":[{\"finish_reason\":null,\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}],\"created\":1753900000,\"id\":\"chatcmpl-Hq0\",\"model\":\"Carnice-9b-Q6_K\",\"system_fingerprint\":\"b9199-6c7a3f21\",\"object\":\"chat.completion.chunk\"}\n\n",
        "data: {\"choices\":[{\"finish_reason\":null,\"index\":0,\"delta\":{\"content\":\"Hello\"}}],\"created\":1753900000,\"id\":\"chatcmpl-Hq0\",\"model\":\"Carnice-9b-Q6_K\",\"system_fingerprint\":\"b9199-6c7a3f21\",\"object\":\"chat.completion.chunk\"}\n\n",
        "data: {\"choices\":[{\"finish_reason\":null,\"index\":0,\"delta\":{\"content\":\" there\"}}],\"created\":1753900000,\"id\":\"chatcmpl-Hq0\",\"model\":\"Carnice-9b-Q6_K\",\"system_fingerprint\":\"b9199-6c7a3f21\",\"object\":\"chat.completion.chunk\"}\n\n",
        "data: {\"choices\":[{\"finish_reason\":null,\"index\":0,\"delta\":{\"content\":\"!\"}}],\"created\":1753900000,\"id\":\"chatcmpl-Hq0\",\"model\":\"Carnice-9b-Q6_K\",\"system_fingerprint\":\"b9199-6c7a3f21\",\"object\":\"chat.completion.chunk\"}\n\n",
        "data: {\"choices\":[{\"finish_reason\":\"stop\",\"index\":0,\"delta\":{}}],\"created\":1753900000,\"id\":\"chatcmpl-Hq0\",\"model\":\"Carnice-9b-Q6_K\",\"system_fingerprint\":\"b9199-6c7a3f21\",\"object\":\"chat.completion.chunk\",\"usage\":{\"completion_tokens\":3,\"prompt_tokens\":17,\"total_tokens\":20},\"timings\":{\"prompt_n\":17,\"prompt_ms\":412.5,\"prompt_per_token_ms\":24.26,\"prompt_per_second\":41.21,\"predicted_n\":3,\"predicted_ms\":735.9,\"predicted_per_token_ms\":245.3,\"predicted_per_second\":4.08,\"cache_n\":0}}\n\n",
        "data: [DONE]\n\n",
    );

    /// A Together capture: `"usage": null` on every non-final chunk, a `finish_reason` of
    /// `"eos"`, and the `seed` / `token_id` extras Together adds.
    const TOGETHER_SSE: &str = concat!(
        "data: {\"id\":\"nzq8k2P-4Tr\",\"object\":\"chat.completion.chunk\",\"created\":1753900123,\"choices\":[{\"index\":0,\"text\":\"Hi\",\"logprobs\":null,\"finish_reason\":null,\"seed\":null,\"delta\":{\"token_id\":13347,\"role\":\"assistant\",\"content\":\"Hi\"}}],\"model\":\"meta-llama/Llama-3.3-70B-Instruct-Turbo\",\"usage\":null}\n\n",
        "data: {\"id\":\"nzq8k2P-4Tr\",\"object\":\"chat.completion.chunk\",\"created\":1753900123,\"choices\":[{\"index\":0,\"text\":\" there\",\"logprobs\":null,\"finish_reason\":null,\"seed\":null,\"delta\":{\"token_id\":1070,\"role\":\"assistant\",\"content\":\" there\"}}],\"model\":\"meta-llama/Llama-3.3-70B-Instruct-Turbo\",\"usage\":null}\n\n",
        "data: {\"id\":\"nzq8k2P-4Tr\",\"object\":\"chat.completion.chunk\",\"created\":1753900123,\"choices\":[{\"index\":0,\"text\":\"\",\"logprobs\":null,\"finish_reason\":\"eos\",\"seed\":7364823901,\"delta\":{\"token_id\":128009,\"role\":\"assistant\",\"content\":\"\"}}],\"model\":\"meta-llama/Llama-3.3-70B-Instruct-Turbo\",\"usage\":{\"prompt_tokens\":19,\"completion_tokens\":2,\"total_tokens\":21}}\n\n",
        "data: [DONE]\n\n",
    );

    /// A `400` on a `stream:true` request. Never an event stream.
    const JSON_400: &str = "{\"error\":{\"message\":\"'max_tokens' must be a positive integer\",\"type\":\"invalid_request_error\",\"code\":null,\"param\":\"max_tokens\"}}";

    /// Split a fixture into `n`-byte chunks — boundaries deliberately fall inside SSE
    /// events, which is exactly what a real socket does.
    fn chop(s: &str, n: usize) -> Vec<Bytes> {
        s.as_bytes().chunks(n).map(Bytes::copy_from_slice).collect()
    }

    fn stream_of(chunks: Vec<Bytes>) -> ByteStream {
        Box::pin(futures_util::stream::iter(
            chunks.into_iter().map(Ok::<Bytes, String>),
        ))
    }

    fn failing_stream(chunks: Vec<Bytes>) -> ByteStream {
        let head = futures_util::stream::iter(chunks.into_iter().map(Ok::<Bytes, String>));
        Box::pin(head.chain(futures_util::stream::once(async {
            Err("connection reset by peer".to_owned())
        })))
    }

    /// Drive a relay to completion and return its bytes and its one outcome.
    async fn drive(inner: ByteStream, idle: Duration, sse: bool) -> (Vec<u8>, StreamOutcome) {
        let (tx, rx) = mpsc::channel();
        let mut body = Vec::new();
        {
            let s = relay_stream(
                inner,
                idle,
                sse,
                Box::new(move |o| {
                    let _ = tx.send(o);
                }),
            );
            let mut s = Box::pin(s);
            while let Some(item) = s.next().await {
                match item {
                    Ok(b) => body.extend_from_slice(&b),
                    Err(_) => break,
                }
            }
        }
        let out = rx.recv().expect("the relay reports exactly one outcome");
        (body, out)
    }

    #[tokio::test]
    async fn llama_cpp_capture_relays_byte_identically() {
        for size in [1, 7, 64, 4096] {
            let (body, out) = drive(
                stream_of(chop(LLAMA_SSE, size)),
                Duration::from_secs(30),
                true,
            )
            .await;
            assert_eq!(
                body,
                LLAMA_SSE.as_bytes(),
                "chunking at {size} bytes must not re-frame"
            );
            assert!(!out.ended_mid_stream);
            assert!(!out.aborted);
        }
    }

    #[tokio::test]
    async fn together_capture_relays_byte_identically() {
        for size in [3, 33, 8192] {
            let (body, _) = drive(
                stream_of(chop(TOGETHER_SSE, size)),
                Duration::from_secs(30),
                true,
            )
            .await;
            assert_eq!(body, TOGETHER_SSE.as_bytes());
        }
    }

    #[tokio::test]
    async fn a_json_error_body_is_relayed_as_json_with_no_done_frame() {
        let (body, out) = drive(
            stream_of(chop(JSON_400, 5)),
            Duration::from_secs(30),
            // `is_event_stream` said no: a 400 is not 2xx.
            false,
        )
        .await;
        assert_eq!(body, JSON_400.as_bytes());
        assert!(!String::from_utf8_lossy(&body).contains("[DONE]"));
        assert!(!out.ended_mid_stream);
    }

    #[test]
    fn content_type_is_forced_only_on_a_2xx_event_stream() {
        let with_ct = |v: &'static str| {
            let mut h = HeaderMap::new();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static(v));
            h
        };
        assert!(is_event_stream(
            StatusCode::OK,
            &with_ct("text/event-stream")
        ));
        assert!(is_event_stream(
            StatusCode::OK,
            &with_ct("text/event-stream; charset=utf-8")
        ));
        // 2xx but JSON: a non-streamed completion.
        assert!(!is_event_stream(
            StatusCode::OK,
            &with_ct("application/json")
        ));
        // `stream:true` rejected: JSON, and it must stay JSON.
        assert!(!is_event_stream(
            StatusCode::BAD_REQUEST,
            &with_ct("application/json")
        ));
        // Some proxies label an error page as an event stream. Not 2xx: not forced.
        assert!(!is_event_stream(
            StatusCode::BAD_GATEWAY,
            &with_ct("text/event-stream")
        ));
        assert!(!is_event_stream(StatusCode::OK, &HeaderMap::new()));
    }

    #[test]
    fn stream_headers_are_added_only_to_streams() {
        let mut h = HeaderMap::new();
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        h.insert(header::CONTENT_LENGTH, HeaderValue::from_static("128"));

        let mut buffered = h.clone();
        decorate_stream_headers(&mut buffered, false);
        assert_eq!(buffered, h, "a buffered response is left exactly as it was");

        let mut streamed = h.clone();
        decorate_stream_headers(&mut streamed, true);
        assert_eq!(streamed[header::CONTENT_TYPE], "text/event-stream");
        assert_eq!(streamed[header::CACHE_CONTROL], "no-cache");
        assert_eq!(streamed["x-accel-buffering"], "no");
        assert_eq!(streamed["x-apexrouter-usage-deferred"], "true");
        assert!(!streamed.contains_key(header::CONTENT_LENGTH));
    }

    #[tokio::test]
    async fn upstream_death_emits_exactly_one_synthetic_frame_and_done() {
        // Everything up to, but not including, the final `data: [DONE]`.
        let truncated = &LLAMA_SSE[..LLAMA_SSE.len() - DONE_FRAME.len()];
        let (body, out) = drive(
            failing_stream(chop(truncated, 128)),
            Duration::from_secs(30),
            true,
        )
        .await;
        let text = String::from_utf8_lossy(&body);
        assert!(text.starts_with(truncated), "the prefix relays verbatim");
        assert_eq!(
            text.matches("upstream ended mid-stream").count(),
            1,
            "exactly one synthetic frame"
        );
        assert_eq!(text.matches("data: [DONE]").count(), 1);
        assert!(text.ends_with("data: [DONE]\n\n"));
        assert!(out.ended_mid_stream);
        assert!(!out.aborted);
        assert_eq!(out.error(), Some("upstream ended mid-stream"));
    }

    #[tokio::test]
    async fn a_clean_eof_without_done_is_still_never_a_silent_truncation() {
        let truncated = &LLAMA_SSE[..LLAMA_SSE.len() - DONE_FRAME.len()];
        let (body, out) = drive(
            stream_of(chop(truncated, 512)),
            Duration::from_secs(30),
            true,
        )
        .await;
        let text = String::from_utf8_lossy(&body);
        assert_eq!(text.matches("upstream ended mid-stream").count(), 1);
        assert!(text.ends_with("data: [DONE]\n\n"));
        assert!(out.ended_mid_stream);
        // The tee still read the tail that did arrive.
        assert_eq!(out.usage.map(|u| u.completion_tokens), Some(3));
    }

    #[tokio::test]
    async fn a_complete_stream_gets_no_synthetic_frame() {
        let (body, out) = drive(
            stream_of(chop(LLAMA_SSE, 96)),
            Duration::from_secs(30),
            true,
        )
        .await;
        assert!(!String::from_utf8_lossy(&body).contains("upstream ended mid-stream"));
        assert!(!out.ended_mid_stream);
        assert!(out.error().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn an_idle_gap_longer_than_idle_timeout_aborts() {
        let idle = Duration::from_millis(RouterCfg::default().idle_timeout_ms);
        let head = futures_util::stream::iter(vec![Ok::<Bytes, String>(Bytes::from_static(
            b"data: {\"choices\":[]}\n\n",
        ))]);
        let inner: ByteStream = Box::pin(head.chain(futures_util::stream::pending()));

        // The tokio clock, not the std one: this test runs on a paused virtual clock.
        let start = tokio::time::Instant::now();
        let (body, out) = drive(inner, idle, true).await;
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("upstream idle timeout"));
        assert!(text.contains("upstream_timeout"));
        assert!(text.ends_with("data: [DONE]\n\n"));
        assert!(out.idle_timeout);
        assert_eq!(out.error(), Some("upstream idle timeout"));
        assert!(start.elapsed() >= idle, "it waited the whole idle window");
    }

    #[tokio::test(start_paused = true)]
    async fn there_is_no_total_timeout() {
        // 40 chunks, each arriving at 60% of the idle window: over three hours of virtual
        // wall clock on the default 300 s idle timeout. Any total timeout would have fired.
        let idle = Duration::from_millis(RouterCfg::default().idle_timeout_ms);
        let gap = idle.mul_f32(0.6);
        let inner: ByteStream =
            Box::pin(futures_util::stream::unfold(0usize, move |i| async move {
                if i >= 40 {
                    return None;
                }
                tokio::time::sleep(gap).await;
                Some((
                    Ok::<Bytes, String>(Bytes::from_static(b"data: {}\n\n")),
                    i + 1,
                ))
            }));

        let start = tokio::time::Instant::now();
        let (body, out) = drive(inner, idle, true).await;
        assert!(
            start.elapsed() > Duration::from_secs(7_000),
            "the virtual clock really did advance past any plausible total timeout"
        );
        assert!(!out.idle_timeout, "no single gap exceeded the idle window");
        assert_eq!(
            String::from_utf8_lossy(&body).matches("data: {}").count(),
            40
        );
    }

    #[tokio::test]
    async fn the_tee_never_delays_a_byte() {
        // 2000 ready chunks: the per-chunk cost is a memcpy into the tail plus a newline
        // count, and must not be observable at millisecond resolution.
        let chunks: Vec<Bytes> = (0..2000)
            .map(|i| {
                Bytes::from(format!(
                    "data: {{\"n\":{i},\"c\":\"xxxxxxxxxxxxxxxx\"}}\n\n"
                ))
            })
            .collect();
        let s = relay_stream(
            stream_of(chunks),
            Duration::from_secs(30),
            true,
            Box::new(|_| {}),
        );
        let mut s = Box::pin(s);
        let mut n = 0usize;
        let mut slow = 0usize;
        while let Some(item) = {
            let t = Instant::now();
            let item = s.next().await;
            if item.is_some() && t.elapsed() >= Duration::from_millis(1) {
                slow += 1;
            }
            item
        } {
            let _ = item;
            n += 1;
        }
        assert_eq!(n, 2002, "2000 relayed chunks plus the synthetic pair");
        // A scheduler hiccup on a loaded build box can land on any single poll; the claim
        // is that the tee costs nothing, so allow under 1% of noise and nothing more.
        assert!(slow * 100 < n, "{slow} of {n} chunk emits took >= 1 ms");
    }

    #[test]
    fn the_tee_reads_llama_cpp_usage_and_timings() {
        let mut tee = UsageTee::default();
        for c in chop(LLAMA_SSE, 61) {
            tee.feed(&c);
        }
        assert!(tee.saw_done());
        let (usage, timings) = tee.finish().expect("the tail carries usage");
        assert_eq!(usage.prompt_tokens, 17);
        assert_eq!(usage.completion_tokens, 3);
        let timings = timings.expect("llama.cpp sends timings");
        assert_eq!(timings.predicted_n, 3);
        assert_eq!(timings.cache_n, 0);
        assert!((timings.predicted_per_second - 4.08).abs() < 0.01);
    }

    #[test]
    fn the_tee_ignores_the_null_usage_on_every_together_chunk() {
        let mut tee = UsageTee::default();
        for c in chop(TOGETHER_SSE, 17) {
            tee.feed(&c);
        }
        let (usage, timings) = tee
            .finish()
            .expect("the final Together chunk carries usage");
        assert_eq!(usage.prompt_tokens, 19);
        assert_eq!(usage.completion_tokens, 2);
        assert!(timings.is_none(), "Together sends no llama.cpp timings");
    }

    #[test]
    fn the_tee_reads_a_buffered_body_whose_head_fell_out_of_the_window() {
        let filler = "x".repeat(200_000);
        let body = format!(
            "{{\"id\":\"chatcmpl-1\",\"choices\":[{{\"message\":{{\"content\":\"{filler}\"}}}}],\"usage\":{{\"prompt_tokens\":11,\"completion_tokens\":22,\"prompt_tokens_details\":{{\"cached_tokens\":7}}}},\"timings\":{{\"prompt_n\":11,\"predicted_n\":22,\"predicted_ms\":1000.0,\"predicted_per_second\":22.0}}}}"
        );
        let mut tee = UsageTee::default();
        for c in chop(&body, 8192) {
            tee.feed(&c);
        }
        assert!(tee.buffered() < 4 * DEFAULT_TAIL_CAP);
        let (usage, timings) = tee.finish().expect("the tail still holds usage");
        assert_eq!(usage.prompt_tokens, 11);
        assert_eq!(usage.completion_tokens, 22);
        assert_eq!(usage.cached_tokens, Some(7));
        assert_eq!(timings.map(|t| t.predicted_n), Some(22));
    }

    #[test]
    fn the_tee_is_bounded_on_a_long_stream() {
        let mut tee = UsageTee::default();
        let chunk = vec![b'a'; 4096];
        for _ in 0..4096 {
            tee.feed(&chunk);
            assert!(
                tee.buffered() <= 2 * DEFAULT_TAIL_CAP + chunk.len(),
                "the tee must not grow with the stream"
            );
        }
        assert_eq!(tee.finish(), None);
    }

    #[tokio::test]
    async fn a_malformed_tail_degrades_to_estimated() {
        // Frames that never carry usage — llama.cpp without `include_usage`.
        let stream = "data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n".repeat(12)
            + "data: [DONE]\n\n";
        let (_, out) = drive(stream_of(chop(&stream, 40)), Duration::from_secs(30), true).await;
        assert!(out.usage.is_none());
        assert!(out.prompt_tokens().is_none());
        assert!(
            matches!(out.completion_tokens(), TokenCount::Estimated(n) if n > 0),
            "a stream with no reported usage degrades, it does not lie"
        );
        assert!(out.tok_per_s().is_none());

        // Truncated JSON in the tail must not panic and must not fabricate.
        let mut tee = UsageTee::default();
        tee.feed(b"data: {\"usage\":{\"prompt_tok");
        assert_eq!(tee.finish(), None);
    }

    #[tokio::test]
    async fn a_reported_tail_is_reported_not_estimated() {
        let (_, out) = drive(
            stream_of(chop(LLAMA_SSE, 128)),
            Duration::from_secs(30),
            true,
        )
        .await;
        assert_eq!(out.prompt_tokens(), Some(TokenCount::Reported(17)));
        assert_eq!(out.completion_tokens(), TokenCount::Reported(3));
        assert_eq!(out.tok_per_s(), Some(4.08));
    }

    #[tokio::test]
    async fn a_client_disconnect_is_not_a_clean_end() {
        let (tx, rx) = mpsc::channel();
        {
            let s = relay_stream(
                stream_of(chop(LLAMA_SSE, 16)),
                Duration::from_secs(30),
                true,
                Box::new(move |o| {
                    let _ = tx.send(o);
                }),
            );
            let mut s = Box::pin(s);
            let _first = s.next().await.expect("one chunk");
            // The client goes away: axum drops the body here.
        }
        let out = rx
            .recv()
            .expect("exactly one outcome, even on a disconnect");
        assert!(
            out.aborted,
            "the upstream was still open, so the client left first"
        );
        assert!(!out.ended_mid_stream);
    }

    #[test]
    fn object_after_key_takes_the_value_not_a_later_object() {
        let s = "{\"a\":{\"usage\":1},\"usage\":{\"prompt_tokens\":3},\"z\":{\"q\":{}}}";
        assert_eq!(object_after_key(s, "usage"), Some("{\"prompt_tokens\":3}"));
        assert_eq!(object_after_key(s, "nope"), None);
        // A key whose value is not an object is skipped, not mis-parsed.
        assert_eq!(
            object_after_key("{\"usage\":null,\"timings\":{\"prompt_n\":1}}", "timings"),
            Some("{\"prompt_n\":1}")
        );
        // Braces inside strings do not confuse the scanner.
        assert_eq!(
            object_after_key("{\"usage\":{\"s\":\"a}b{\\\"c\",\"n\":1}}", "usage"),
            Some("{\"s\":\"a}b{\\\"c\",\"n\":1}")
        );
        assert_eq!(balanced_end("{\"a\":1"), None);
    }
}
