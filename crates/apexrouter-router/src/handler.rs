//! OWNER: unit R-08 (router/src/lib.rs, router/src/handler.rs). Do not edit outside that
//! unit.
//!
//! The proxy listener's wiring, and the `(ingress, upstream)` matrix dispatch.
//!
//! The catch-all is registered with `.fallback(any(proxy_handler))` — **never** as a
//! `/{*path}` route — because a catch-all `any()` route and the static-asset
//! `get("/{*path}")` route panic on `Router::merge` in axum 0.8 ("Overlapping method
//! route"). There is an explicit `merge_does_not_panic` test.
//!
//! The matrix (`ARCHITECTURE.md` §3.4), all four cells owned here:
//!
//! | ingress → upstream | behaviour |
//! |---|---|
//! | `OpenAi` → `OpenAi` | relay, byte-for-byte |
//! | `Anthropic` → `Anthropic` | passthrough relay; only the credential is swapped |
//! | `Anthropic` → `OpenAi` | call into [`crate::anthropic`] (unit R-10) |
//! | `OpenAi` → `Anthropic` | **501** with an **OpenAI-shaped** body. Permanently out of scope |

use crate::Router;
use axum::extract::State;

/// The axum `Router` for the PROXY listener.
pub fn proxy_router(r: Router) -> axum::Router {
    todo!("R-08: proxy_router")
}

/// The catch-all handler: normalise, classify, peek, resolve, dispatch, relay.
///
/// Records `RequestRecord::ingress`, and emits `X-ApexRouter-Protocol: <ingress>-><upstream>`
/// whenever the ingress is not `open_ai`, so which matrix cell ran is observable exactly
/// like `X-ApexRouter-Route`.
pub async fn proxy_handler(
    State(r): State<Router>,
    req: axum::extract::Request,
) -> axum::response::Response {
    todo!("R-08: proxy_handler")
}
