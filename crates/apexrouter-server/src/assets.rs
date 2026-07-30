//! OWNER: unit S-05 (server/src/{ws,assets,prober,watcher}.rs). Do not edit outside that
//! unit.
//!
//! The embedded web UI: `rust-embed` pointed straight at `../../ui-web` — no `dist/`, no
//! build step — with `[server] ui_dir` as a live-reload escape hatch.
//!
//! No `mime_guess`: [`mime_for`] is a hand-written match over the fourteen extensions three
//! files can possibly need.

/// The three files of `ui-web/`, compiled in.
#[derive(rust_embed::Embed)]
#[folder = "../../ui-web"]
pub struct Assets;

/// `"/"` plus `"/{*path}"`. Refuses to shadow `/v1` or `/health`.
///
/// This router is **only** ever mounted on the control listener. On the proxy listener the
/// catch-all is `.fallback(any(proxy_handler))`, because a `/{*path}` route and an `any()`
/// route panic on `Router::merge` in axum 0.8.
pub fn static_router() -> axum::Router {
    todo!("S-05: static_router")
}

/// Content type by extension. Fourteen arms, hand-written.
pub fn mime_for(path: &str) -> &'static str {
    todo!("S-05: mime_for")
}
