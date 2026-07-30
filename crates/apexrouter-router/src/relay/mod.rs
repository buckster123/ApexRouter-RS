//! OWNER: unit R-03 (router/src/relay/{mod,headers,body}.rs). Do not edit outside that
//! unit — `stream.rs` belongs to R-05.
//!
//! The relay: what goes out, what comes back, and the promise that **bytes are relayed
//! verbatim**. SSE is never re-framed, because a chunk boundary may split an event and
//! every OpenAI SDK buffers on `\n\n`.

pub mod body;
pub mod headers;
pub mod stream;

pub use body::{normalize_path, peek, plan_body, BodyPlan, RequestPeek};
pub use headers::{outbound_headers, response_headers};
pub use stream::{sse_response, UsageTee};
