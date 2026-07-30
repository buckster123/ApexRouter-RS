//! OWNER: unit S-05 (server/src/{ws,assets,prober,watcher}.rs). Do not edit outside that
//! unit.
//!
//! The embedded web UI: `rust-embed` pointed straight at `../../ui-web` — no `dist/`, no
//! build step — with `[server] ui_dir` as a live-reload escape hatch.
//!
//! No `mime_guess`: [`mime_for`] is a hand-written match over the fourteen extensions three
//! files can possibly need.
//!
//! **The escape hatch is a process global**, set by [`set_ui_dir`], because
//! [`static_router`]'s published signature takes no arguments and `AppState` has no slot for
//! it. That is not a compromise in practice: it is read on every request, so
//! `POST /v1/reload` and the config watcher noticing a changed `[server] ui_dir` take effect
//! without a restart, which is the whole point of the hatch. One daemon per process, one
//! value.

use axum::body::Body;
use axum::extract::Path as UrlPath;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::get;
use sha2::{Digest, Sha256};
use std::path::{Component, PathBuf};
use std::sync::{OnceLock, RwLock};

/// The three files of `ui-web/`, compiled in.
#[derive(rust_embed::Embed)]
#[folder = "../../ui-web"]
pub struct Assets;

/// What a request for `/` gets.
const INDEX: &str = "index.html";

/// Path prefixes the asset router will never answer for, so a UI file called `health` or a
/// directory called `v1` can never shadow the control plane. Checked on the **first** path
/// segment only.
///
/// This is belt-and-braces: axum's matcher already prefers a literal segment over
/// `/{*path}`. The braces are here because the failure mode — the web UI quietly answering
/// `GET /v1/snapshot` after somebody adds a file — is invisible until a client breaks.
const RESERVED: [&str; 4] = ["v1", "health", "ws", "metrics"];

/// The live-reload directory, when one is configured.
fn ui_dir_slot() -> &'static RwLock<Option<PathBuf>> {
    static SLOT: OnceLock<RwLock<Option<PathBuf>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(None))
}

/// Recover a poisoned lock rather than panicking: what is behind it is one `Option<PathBuf>`,
/// and a panic elsewhere must not take the web UI down with it.
fn unpoison<T>(r: Result<T, std::sync::PoisonError<T>>) -> T {
    r.unwrap_or_else(|p| p.into_inner())
}

/// Point the static router at a directory instead of the embedded copy — `[server] ui_dir`.
///
/// `None` (the default, and what `ui_dir = ""` means) serves the compiled-in files. Call it
/// at startup and again on every config reload, so the hatch opens and closes live.
pub fn set_ui_dir(dir: Option<PathBuf>) {
    let normalised = dir.filter(|p| !p.as_os_str().is_empty());
    *unpoison(ui_dir_slot().write()) = normalised;
}

/// The directory currently overriding the embedded assets, if any.
pub fn ui_dir() -> Option<PathBuf> {
    unpoison(ui_dir_slot().read()).clone()
}

/// Serialises every test in this crate that touches the process-global `ui_dir`.
///
/// `cargo test` runs the modules on one thread pool, so `assets`' own tests and the config
/// watcher's `sync_ui_dir` test would otherwise clobber each other's global.
#[cfg(test)]
pub(crate) fn ui_dir_test_lock() -> &'static std::sync::Mutex<()> {
    static L: std::sync::Mutex<()> = std::sync::Mutex::new(());
    &L
}

/// `"/"` plus `"/{*path}"`. Refuses to shadow `/v1` or `/health`.
///
/// This router is **only** ever mounted on the control listener. On the proxy listener the
/// catch-all is `.fallback(any(proxy_handler))`, because a `/{*path}` route and an `any()`
/// route panic on `Router::merge` in axum 0.8.
pub fn static_router() -> axum::Router {
    axum::Router::new()
        .route("/", get(index_handler))
        .route("/{*path}", get(asset_handler))
}

/// Content type by extension. Fourteen arms, hand-written.
pub fn mime_for(path: &str) -> &'static str {
    let ext = path
        .rsplit('/')
        .next()
        .and_then(|f| f.rsplit_once('.'))
        .map(|(_, e)| e)
        .unwrap_or_default();
    // Compared lowercase, so `PHOTO.JPEG` is still a JPEG.
    let lower = ext.to_ascii_lowercase();
    match lower.as_str() {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "wasm" => "application/wasm",
        "txt" => "text/plain; charset=utf-8",
        "md" => "text/markdown; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// `GET /`.
async fn index_handler(headers: HeaderMap) -> Response {
    respond(INDEX, &headers)
}

/// `GET /{*path}`.
async fn asset_handler(UrlPath(path): UrlPath<String>, headers: HeaderMap) -> Response {
    if is_reserved(&path) {
        return not_found();
    }
    let wanted = if path.is_empty() || path.ends_with('/') {
        format!("{path}{INDEX}")
    } else {
        path
    };
    respond(&wanted, &headers)
}

/// True when the first path segment belongs to the control plane, not to the UI.
fn is_reserved(path: &str) -> bool {
    let head = path
        .trim_start_matches('/')
        .split('/')
        .next()
        .unwrap_or_default();
    RESERVED.contains(&head)
}

/// Serve one asset, from `ui_dir` when the hatch is open and from the embedded copy
/// otherwise, with an `ETag` so a reload is one 304 rather than three bodies.
fn respond(path: &str, headers: &HeaderMap) -> Response {
    let Some(rel) = safe_relative(path) else {
        return not_found();
    };
    let Some((bytes, etag)) = load(&rel) else {
        return not_found();
    };
    let fresh = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|c| c.trim() == etag));
    if fresh {
        return build(StatusCode::NOT_MODIFIED, None, &etag, Body::empty());
    }
    build(
        StatusCode::OK,
        Some(mime_for(&rel)),
        &etag,
        Body::from(bytes),
    )
}

/// One response, with the headers a same-origin control-plane UI wants and nothing else.
fn build(status: StatusCode, ct: Option<&str>, etag: &str, body: Body) -> Response {
    let mut b = Response::builder()
        .status(status)
        .header(header::ETAG, etag)
        .header(header::CACHE_CONTROL, "no-cache")
        // The UI is same-origin and inline-free; these two cost nothing and close the holes
        // a static file server can open on a control plane.
        .header("x-content-type-options", "nosniff")
        .header("referrer-policy", "no-referrer");
    if let Some(ct) = ct {
        b = b.header(header::CONTENT_TYPE, ct);
    }
    match b.body(body) {
        Ok(r) => r,
        // Every header above is a literal or hex, so this is unreachable. It exists so the
        // function contains no panic at all.
        Err(_) => not_found(),
    }
}

/// Reject anything that could escape the asset root before it reaches the filesystem.
///
/// `..`, a leading `/`, a Windows prefix and a bare `.` are dropped rather than resolved.
/// Returns the cleaned relative path.
fn safe_relative(path: &str) -> Option<String> {
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() {
        return Some(INDEX.to_owned());
    }
    let mut out = Vec::new();
    for c in PathBuf::from(trimmed).components() {
        match c {
            Component::Normal(seg) => out.push(seg.to_str()?.to_owned()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if out.is_empty() {
        return None;
    }
    Some(out.join("/"))
}

/// The bytes and the strong `ETag` for one already-validated relative path.
fn load(rel: &str) -> Option<(Vec<u8>, String)> {
    if let Some(dir) = ui_dir() {
        let bytes = std::fs::read(dir.join(rel)).ok()?;
        let etag = etag_of(&bytes);
        return Some((bytes, etag));
    }
    let file = Assets::get(rel)?;
    let etag = format!("\"{}\"", hex(&file.metadata.sha256_hash()));
    Some((file.data.into_owned(), etag))
}

/// A strong `ETag` over content we hashed ourselves — the `ui_dir` path.
fn etag_of(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("\"{}\"", hex(&h.finalize()))
}

/// Lowercase hex, without pulling in another crate for sixteen characters.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // Writing to a String is infallible; the Result is discarded deliberately rather
        // than unwrapped, because `unwrap()` is banned outside tests.
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// The one 404. Plain text, never an HTML page a JSON client would have to parse.
fn not_found() -> Response {
    match Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from("not found"))
    {
        Ok(r) => r,
        Err(_) => Response::new(Body::from("not found")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::Request;
    use tower::ServiceExt;

    /// Serialises the tests that mutate the process-global `ui_dir`.
    fn ui_dir_lock() -> &'static std::sync::Mutex<()> {
        super::ui_dir_test_lock()
    }

    /// These tests are **synchronous**, and drive the router through `block_on`.
    ///
    /// `ui_dir` is a process global shared with the config watcher's test, so it needs a
    /// lock, and holding a `std::sync::Mutex` guard across an `.await` is exactly what
    /// `clippy::await_holding_lock` is for. Keeping the awaits inside `block_on` means the
    /// guard is never held across a suspension point at all.
    fn get_path(uri: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
        get_path_with(uri, HeaderMap::new())
    }

    fn get_path_with(uri: &str, extra: HeaderMap) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut b = Request::builder().method("GET").uri(uri);
        for (k, v) in extra.iter() {
            b = b.header(k, v);
        }
        let req = b.body(Body::empty()).expect("request");
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async move {
                let res = static_router().oneshot(req).await.expect("response");
                let status = res.status();
                let headers = res.headers().clone();
                let body = to_bytes(res.into_body(), 1 << 20).await.expect("body");
                (status, headers, body.to_vec())
            })
    }

    #[test]
    fn mime_for_covers_every_extension_the_ui_can_ship() {
        assert_eq!(mime_for("index.html"), "text/html; charset=utf-8");
        assert_eq!(mime_for("/style.css"), "text/css; charset=utf-8");
        assert_eq!(mime_for("app.js"), "text/javascript; charset=utf-8");
        assert_eq!(mime_for("app.mjs"), "text/javascript; charset=utf-8");
        assert_eq!(mime_for("data.json"), "application/json");
        assert_eq!(mime_for("app.js.map"), "application/json");
        assert_eq!(mime_for("icon.svg"), "image/svg+xml");
        assert_eq!(mime_for("a/b/c.png"), "image/png");
        assert_eq!(mime_for("photo.jpg"), "image/jpeg");
        assert_eq!(mime_for("photo.JPEG"), "image/jpeg");
        assert_eq!(mime_for("anim.gif"), "image/gif");
        assert_eq!(mime_for("shot.webp"), "image/webp");
        assert_eq!(mime_for("favicon.ico"), "image/x-icon");
        assert_eq!(mime_for("mono.woff2"), "font/woff2");
        assert_eq!(mime_for("thing.wasm"), "application/wasm");
        assert_eq!(mime_for("readme.txt"), "text/plain; charset=utf-8");
        assert_eq!(mime_for("readme.md"), "text/markdown; charset=utf-8");
        // No extension, an unknown one, and a dot that is in a directory name only.
        assert_eq!(mime_for("LICENSE"), "application/octet-stream");
        assert_eq!(mime_for("thing.exe"), "application/octet-stream");
        assert_eq!(mime_for("a.b/c"), "application/octet-stream");
    }

    #[test]
    fn root_serves_the_embedded_index_with_a_strong_etag() {
        let _g = ui_dir_lock().lock().expect("lock");
        set_ui_dir(None);
        let (status, headers, body) = get_path("/");
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/html; charset=utf-8")
        );
        let etag = headers
            .get(header::ETAG)
            .and_then(|v| v.to_str().ok())
            .expect("etag")
            .to_owned();
        assert!(etag.starts_with('"') && etag.len() > 10, "{etag}");
        assert!(!body.is_empty());

        // A conditional request is one 304, not a second body.
        let mut cond = HeaderMap::new();
        cond.insert(header::IF_NONE_MATCH, etag.parse().expect("header value"));
        let (status, _, body) = get_path_with("/", cond);
        assert_eq!(status, StatusCode::NOT_MODIFIED);
        assert!(body.is_empty());
    }

    #[test]
    fn the_three_ui_files_are_all_embedded_and_typed() {
        let _g = ui_dir_lock().lock().expect("lock");
        set_ui_dir(None);
        for (path, ct) in [
            ("/index.html", "text/html; charset=utf-8"),
            ("/app.js", "text/javascript; charset=utf-8"),
            ("/style.css", "text/css; charset=utf-8"),
        ] {
            let (status, headers, body) = get_path(path);
            assert_eq!(status, StatusCode::OK, "{path}");
            assert_eq!(
                headers
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok()),
                Some(ct),
                "{path}"
            );
            assert!(!body.is_empty(), "{path}");
        }
    }

    #[test]
    fn the_asset_router_refuses_to_shadow_the_control_plane() {
        let _g = ui_dir_lock().lock().expect("lock");
        set_ui_dir(None);
        for path in ["/v1", "/v1/snapshot", "/health", "/ws", "/metrics"] {
            let (status, _, _) = get_path(path);
            assert_eq!(status, StatusCode::NOT_FOUND, "{path} must not be served");
        }
    }

    #[test]
    fn traversal_out_of_the_asset_root_is_refused() {
        let _g = ui_dir_lock().lock().expect("lock");
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("index.html"), b"<p>dev</p>").expect("write");
        if let Some(parent) = dir.path().parent() {
            std::fs::write(parent.join("apexrouter-s05-secret.txt"), b"do not serve me")
                .expect("write");
        }
        set_ui_dir(Some(dir.path().to_path_buf()));

        for path in [
            "/../apexrouter-s05-secret.txt",
            "/a/../../apexrouter-s05-secret.txt",
        ] {
            let (status, _, body) = get_path(path);
            assert_ne!(
                String::from_utf8_lossy(&body),
                "do not serve me",
                "{path} escaped the root"
            );
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        }
        set_ui_dir(None);
    }

    #[test]
    fn ui_dir_overrides_the_embedded_copy_and_reverts() {
        let _g = ui_dir_lock().lock().expect("lock");
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("index.html"), b"<p>live reload</p>").expect("write");

        set_ui_dir(Some(dir.path().to_path_buf()));
        let (status, _, body) = get_path("/");
        assert_eq!(status, StatusCode::OK);
        assert_eq!(String::from_utf8_lossy(&body), "<p>live reload</p>");

        // The hatch is live: editing the file changes what is served, no restart.
        std::fs::write(dir.path().join("index.html"), b"<p>edited</p>").expect("write");
        let (_, _, body) = get_path("/");
        assert_eq!(String::from_utf8_lossy(&body), "<p>edited</p>");

        // `ui_dir = ""` means "off", not "serve the current directory".
        set_ui_dir(Some(PathBuf::new()));
        assert_eq!(ui_dir(), None);
        let (status, _, body) = get_path("/");
        assert_eq!(status, StatusCode::OK);
        assert_ne!(String::from_utf8_lossy(&body), "<p>edited</p>");
        set_ui_dir(None);
    }

    #[test]
    fn a_missing_asset_is_a_plain_text_404_not_an_html_page() {
        let _g = ui_dir_lock().lock().expect("lock");
        set_ui_dir(None);
        let (status, headers, body) = get_path("/nope.js");
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/plain; charset=utf-8")
        );
        assert_eq!(String::from_utf8_lossy(&body), "not found");
    }

    #[test]
    fn safe_relative_normalises_and_refuses() {
        assert_eq!(safe_relative("app.js").as_deref(), Some("app.js"));
        assert_eq!(safe_relative("/app.js").as_deref(), Some("app.js"));
        assert_eq!(safe_relative("./a/./b.css").as_deref(), Some("a/b.css"));
        assert_eq!(safe_relative("").as_deref(), Some(INDEX));
        assert_eq!(safe_relative("../etc/passwd"), None);
        assert_eq!(safe_relative("a/../../b"), None);
        assert_eq!(safe_relative("/etc/../../x"), None);
    }
}
