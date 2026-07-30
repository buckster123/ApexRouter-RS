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

use crate::attempt::Committed;
use crate::limits::InFlightGuard;
use apexrouter_core::config::RouterCfg;
use apexrouter_core::upstream::{Timings, UsageFields};
use apexrouter_protocol::Event;
use tokio::sync::broadcast;

/// Turn a committed upstream response into a streaming axum response.
///
/// Takes ownership of the `InFlightGuard`, so a client disconnect drops it, cancels the
/// reqwest future, aborts the upstream (freeing llama.cpp's slot) and emits exactly one
/// `RequestFinished { aborted: true }`.
pub fn sse_response(
    c: Committed,
    cfg: &RouterCfg,
    tx: broadcast::Sender<Event>,
    guard: InFlightGuard,
) -> axum::response::Response {
    todo!("R-05: sse_response")
}

/// Watches the tail of a stream for `usage` / `timings`. Bounded buffer; never delays a byte.
#[derive(Debug, Default)]
pub struct UsageTee {/* R-05: rolling tail buffer, bounded */}

impl UsageTee {
    /// Feed one relayed chunk. Must not copy the whole stream.
    pub fn feed(&mut self, chunk: &[u8]) {
        todo!("R-05: UsageTee::feed")
    }

    /// What the tail contained, if anything parseable.
    pub fn finish(self) -> Option<(UsageFields, Option<Timings>)> {
        todo!("R-05: UsageTee::finish")
    }
}
