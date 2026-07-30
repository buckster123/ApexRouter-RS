//! OWNER: unit R-02 (router/src/resolve.rs, router/src/policy.rs). Do not edit outside that
//! unit.
//!
//! **There is exactly one `resolve()` and every surface calls it.** It is synchronous, does
//! no I/O, and the answer is observable on every response as
//! `X-ApexRouter-Route: <alias-or-"-">|<reason>`.
//!
//! The six rules, in order (`ARCHITECTURE.md` §4.2):
//!
//! 1. an **alias** → that route
//! 2. `"<backend_id>/<upstream_model>"` → **explicit pin**, one candidate
//! 3. an exact **upstream model id** on exactly one enabled backend
//! 4. the same id on several backends → `[router] implicit_strategy`, plus a one-shot alert
//! 5. `""`, `"x"`, `"auto"`, `"default"`, absent → the default alias
//! 6. anything else → `[router] unknown_model`, **default `reject`**

use crate::registry::LiveBackend;
use crate::table::RoutingTable;
use apexrouter_protocol::{Alias, RouteReason};
use std::sync::Arc;

/// `smallvec` is deliberately not a dependency — `ARCHITECTURE.md` §2.1 pins the crate set
/// and `arc-swap` is the one addition. The published `SmallVecLike` is therefore a `Vec`;
/// the name is kept so the signature reads as the document writes it and so a future
/// small-vector optimisation is a one-line change here.
pub type SmallVecLike<T> = Vec<T>;

/// What the resolver decided, in dispatch order.
pub struct Plan {
    /// Ordered candidates. The retry loop walks these, bounded by `RetryPolicy.attempts`.
    pub candidates: SmallVecLike<Candidate>,
    /// Which rule fired.
    pub reason: RouteReason,
    /// The alias, when one resolved.
    pub alias: Option<Alias>,
    /// `Some` only when the outbound `"model"` differs from what the client sent. This is
    /// the **only** key the body rewriter is allowed to touch.
    pub rewrite_model_to: Option<String>,
}

/// One dispatchable target.
pub struct Candidate {
    /// Live state, including the permit pool and the breaker.
    pub backend: Arc<LiveBackend>,
    /// The model id to send upstream.
    pub upstream_model: String,
}

/// What kind of request this is. Drives which backends are eligible.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RequestClass {
    /// `GET /v1/models`.
    Models,
    /// `/v1/chat/completions` and `/v1/messages`.
    Chat,
    /// `/v1/completions`.
    Completion,
    /// `/v1/embeddings` — only embedding-capable backends.
    Embedding,
    /// `/v1/rerank`.
    Rerank,
    /// Anything else, proxied to the default alias's primary target.
    Opaque,
}

/// Why nothing could be dispatched to.
#[derive(Debug, thiserror::Error)]
pub enum RouteError {
    /// Rule 6 under `reject`. Lists the aliases we do know, because a 404 that names the
    /// alternatives is the difference between a fixed typo and a support thread.
    #[error("unknown model; known aliases: {}", known.join(", "))]
    NoRoute {
        /// The aliases we do know.
        known: Vec<String>,
    },
    /// The alias resolved but nothing behind it is `Ready`.
    #[error("no healthy backend for alias {alias}")]
    NoHealthy {
        /// Which alias.
        alias: Alias,
    },
    /// Everything behind the alias failed the route's filter.
    #[error("every candidate for alias {alias} was filtered out: {why}")]
    FilteredOut {
        /// Which alias.
        alias: Alias,
        /// Which filter, and what it wanted.
        why: String,
    },
}

/// What to do with a model string that matched no rule.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum UnknownModelPolicy {
    /// `404 model_not_found`, listing known aliases. The default: a fat-fingered
    /// `gpt-4o-mimi` must not silently bill a rented H100.
    Reject,
    /// LocalRouter's old behaviour — send it to the default alias.
    Fallback,
}

impl RoutingTable {
    /// **SYNCHRONOUS. NO I/O.** Six rules, in the documented order.
    ///
    /// Rule 3 reads the prober-maintained `model_index`; a cold index means rule 3 misses
    /// and rule 5/6 applies, which is documented and visible in `X-ApexRouter-Route`.
    pub fn resolve(
        &self,
        model: Option<&str>,
        class: RequestClass,
        unknown: UnknownModelPolicy,
    ) -> Result<Plan, RouteError> {
        todo!("R-02: RoutingTable::resolve")
    }
}
