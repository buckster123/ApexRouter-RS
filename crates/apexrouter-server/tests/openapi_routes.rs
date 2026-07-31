//! OWNER: unit D-01 (`docs/API.md`, `docs/ROUTING.md`, `openapi/apexrouter-v1.yaml`).
//!
//! The acceptance gate for D-01: **the OpenAPI file is checked in CI against the route table.**
//!
//! `axum::Router` exposes no way to enumerate what it holds, so the route table is recovered
//! from the source of truth that produces it — every `.route("…", …)` call in the modules that
//! build the two applications:
//!
//! * control plane — `apexrouter-server/src/api/*.rs` plus `lib.rs` (`/health`, `/ws`)
//! * proxy plane — `apexrouter-router/src/handler.rs` (the three legacy compat routes)
//!
//! Everything else on the proxy listener is served by `.fallback(any(proxy_handler))` and so
//! cannot be enumerated at all; those paths are listed in [`PROXY_FALLBACK`] and the test
//! asserts the fallback registration itself still exists.
//!
//! Three properties are enforced, and each one catches a different kind of drift:
//!
//! 1. **Every route axum registers is documented.** Adding a handler without documenting it
//!    fails the build.
//! 2. **Every documented path is registered, or is on the explicit [`PENDING`] list** naming
//!    the work unit that will wire it. Nothing can be documented into existence quietly.
//! 3. **A [`PENDING`] entry that has since been wired fails too**, so the list cannot rot.
//!
//! Plus, for non-`PENDING` paths, the HTTP methods must agree in both directions.
//!
//! The test reads files only, never writes, and touches no socket — it is hermetic by
//! construction.
//!
//! It lives in `apexrouter-server` rather than in `apexrouter-cli` because this is the crate
//! whose route table it diffs, and because `apexrouter-cli` does not currently build (S-06/S-08
//! are mid-flight); a gate that cannot run is not a gate. CI reaches it through
//! `cargo test --workspace` either way.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The HTTP methods a `MethodRouter` can carry, in the spelling both axum and OpenAPI use.
const METHODS: [&str; 8] = [
    "get", "post", "put", "patch", "delete", "head", "options", "trace",
];

/// Documented paths that are deliberately not registered yet, each with the unit that owes it.
///
/// A path may only sit here while it is genuinely absent from the source; the moment it is
/// wired, this test fails and the entry must be deleted. That is the point.
const PENDING: [(&str, &str); 2] = [
    (
        "/metrics",
        "R-07/S-01 — the Prometheus text exposition (ARCHITECTURE.md §4.5)",
    ),
    (
        "/v1/migrate",
        "I-01/S-08 — POST /v1/migrate (ARCHITECTURE.md §6.2)",
    ),
];

/// Proxy-listener paths served by `.fallback(any(proxy_handler))` rather than by a `.route()`.
///
/// A static route cannot express `/v1` normalisation — a client sending `/v1/v1/models` must be
/// answered, and `Router::route` would 404 it before any handler ran — so these are documented
/// but unregisterable by design.
const PROXY_FALLBACK: [&str; 9] = [
    "/v1/chat/completions",
    "/v1/completions",
    "/v1/embeddings",
    "/v1/rerank",
    "/v1/messages",
    "/v1/messages/count_tokens",
    "/v1/models",
    "/v1/models/{id}",
    "/slots",
];

// =======================================================================================
// locating the tree
// =======================================================================================

/// The repository root, derived from this crate's manifest directory.
fn repo_root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .expect("crates/<crate> always has two ancestors")
        .to_path_buf()
}

/// Read a file that must exist, with a message naming it when it does not.
fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

// =======================================================================================
// the axum side: recover `.route("…", …)` calls from source
// =======================================================================================

/// Every `(path, methods)` pair registered by one file, ignoring anything after `#[cfg(test)]`.
///
/// Test modules build their own routers (`handler.rs` has a whole `merge_does_not_panic` case),
/// and those are not the product's route table.
fn routes_in_file(src: &str) -> BTreeMap<String, BTreeSet<String>> {
    let src = match src.find("#[cfg(test)]") {
        Some(i) => &src[..i],
        None => src,
    };

    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let bytes = src.as_bytes();
    let mut cursor = 0usize;

    while let Some(rel) = src[cursor..].find(".route(") {
        let open = cursor + rel + ".route(".len() - 1; // index of '('
        let end = match matching_paren(bytes, open) {
            Some(e) => e,
            None => break,
        };
        let call = &src[open + 1..end];

        if let Some(path) = first_string_literal(call) {
            let methods = methods_in(call);
            out.entry(path).or_default().extend(methods);
        }
        cursor = end + 1;
    }
    out
}

/// Index of the `)` closing the `(` at `open`, skipping over string literals.
fn matching_paren(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_str = false;
    let mut i = open;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            match c {
                b'\\' => i += 1,
                b'"' => in_str = false,
                _ => {}
            }
        } else {
            match c {
                b'"' => in_str = true,
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

/// The first `"…"` literal in a `.route(…)` argument list — the path template.
fn first_string_literal(call: &str) -> Option<String> {
    let start = call.find('"')? + 1;
    let rest = &call[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_owned())
}

/// Which `MethodRouter` constructors appear in a `.route(…)` argument list.
///
/// Matches an identifier from [`METHODS`] followed immediately by `(`, with a non-identifier
/// character before it — so `post(post_reload)` yields `post` once, not twice, and
/// `axum::routing::delete(destroy)` is found through its path qualifier.
fn methods_in(call: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for m in METHODS {
        let mut from = 0usize;
        while let Some(rel) = call[from..].find(m) {
            let at = from + rel;
            let after = at + m.len();
            let before_ok = at == 0
                || !call[..at]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_');
            let after_ok = call[after..].starts_with('(');
            if before_ok && after_ok {
                found.insert(m.to_owned());
                break;
            }
            from = after;
        }
    }
    found
}

/// The control-plane route table: every `api/*.rs` module plus `lib.rs`'s `/health` and `/ws`.
fn control_routes(root: &Path) -> BTreeMap<String, BTreeSet<String>> {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let api = root.join("crates/apexrouter-server/src/api");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&api)
        .unwrap_or_else(|e| panic!("cannot list {}: {e}", api.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .collect();
    files.push(root.join("crates/apexrouter-server/src/lib.rs"));
    files.sort();

    for f in files {
        for (path, methods) in routes_in_file(&read(&f)) {
            out.entry(path).or_default().extend(methods);
        }
    }
    out
}

/// The proxy-plane route table: only the three legacy compat routes are explicit.
fn proxy_routes(root: &Path) -> BTreeMap<String, BTreeSet<String>> {
    routes_in_file(&read(&root.join("crates/apexrouter-router/src/handler.rs")))
}

// =======================================================================================
// the OpenAPI side: a scanner for the strict subset this file is written in
// =======================================================================================

/// One documented operation set: the methods, the listeners, and the axum spelling if it differs.
#[derive(Default, Debug)]
struct DocPath {
    methods: BTreeSet<String>,
    listeners: BTreeSet<String>,
    axum_route: Option<String>,
}

/// Parse `openapi/apexrouter-v1.yaml`'s `paths:` block.
///
/// The file is deliberately written in a regular subset — two-space indentation, no anchors, no
/// block scalars inside `paths:` — so this needs no YAML dependency in the CLI crate.
fn documented(yaml: &str) -> BTreeMap<String, DocPath> {
    let mut out: BTreeMap<String, DocPath> = BTreeMap::new();
    let mut in_paths = false;
    let mut current: Option<String> = None;
    let mut method: Option<String> = None;

    for line in yaml.lines() {
        if line.trim_start().starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim_end();

        if indent == 0 {
            in_paths = trimmed.starts_with("paths:");
            current = None;
            method = None;
            continue;
        }
        if !in_paths {
            continue;
        }

        if indent == 2 && trimmed.starts_with("  /") && trimmed.ends_with(':') {
            let p = trimmed.trim().trim_end_matches(':').to_owned();
            out.entry(p.clone()).or_default();
            current = Some(p);
            method = None;
            continue;
        }

        let Some(path) = current.clone() else {
            continue;
        };

        if indent == 4 {
            let key = trimmed.trim().trim_end_matches(':');
            if METHODS.contains(&key) {
                out.entry(path.clone())
                    .or_default()
                    .methods
                    .insert(key.to_owned());
                method = Some(key.to_owned());
            } else {
                method = None;
            }
            continue;
        }

        if indent == 6 && method.is_some() {
            let t = trimmed.trim();
            if let Some(rest) = t.strip_prefix("x-listener:") {
                let listeners = rest
                    .trim()
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .split(',')
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty());
                out.entry(path).or_default().listeners.extend(listeners);
            } else if let Some(rest) = t.strip_prefix("x-axum-route:") {
                out.entry(path).or_default().axum_route = Some(rest.trim().to_owned());
            }
        }
    }
    out
}

/// The documented path as axum spells it: `x-axum-route` when present, else the path itself.
fn as_axum(p: &str, d: &DocPath) -> String {
    d.axum_route.clone().unwrap_or_else(|| p.to_owned())
}

// =======================================================================================
// the tests
// =======================================================================================

/// Every path axum registers on the control listener is in the OpenAPI file, with its methods.
#[test]
fn every_registered_control_route_is_documented() {
    let root = repo_root();
    let doc = documented(&read(&root.join("openapi/apexrouter-v1.yaml")));
    let live = control_routes(&root);

    // documented-as-axum -> (documented path, methods)
    let by_axum: BTreeMap<String, (String, &DocPath)> = doc
        .iter()
        .filter(|(_, d)| d.listeners.contains("control"))
        .map(|(p, d)| (as_axum(p, d), (p.clone(), d)))
        .collect();

    let mut missing = Vec::new();
    let mut wrong_methods = Vec::new();

    for (path, methods) in &live {
        match by_axum.get(path) {
            None => missing.push(path.clone()),
            Some((doc_path, d)) => {
                let undocumented: Vec<_> = methods.difference(&d.methods).cloned().collect();
                if !undocumented.is_empty() {
                    wrong_methods.push(format!(
                        "{doc_path}: {undocumented:?} registered, not documented"
                    ));
                }
                let unregistered: Vec<_> = d.methods.difference(methods).cloned().collect();
                if !unregistered.is_empty() {
                    wrong_methods.push(format!(
                        "{doc_path}: {unregistered:?} documented, not registered"
                    ));
                }
            }
        }
    }

    assert!(
        missing.is_empty() && wrong_methods.is_empty(),
        "openapi/apexrouter-v1.yaml is out of date with the axum route table.\n\
         registered but undocumented paths: {missing:#?}\n\
         method mismatches: {wrong_methods:#?}\n\
         Fix the OpenAPI file (owner: work unit D-01), not this test."
    );
}

/// Every documented control path is registered, or is on the explicit `PENDING` list.
#[test]
fn every_documented_control_path_is_registered_or_declared_pending() {
    let root = repo_root();
    let doc = documented(&read(&root.join("openapi/apexrouter-v1.yaml")));
    let live = control_routes(&root);
    let pending: BTreeMap<&str, &str> = PENDING.iter().copied().collect();

    let mut orphans = Vec::new();
    for (p, d) in doc.iter().filter(|(_, d)| d.listeners.contains("control")) {
        if live.contains_key(&as_axum(p, d)) || pending.contains_key(p.as_str()) {
            continue;
        }
        orphans.push(p.clone());
    }

    assert!(
        orphans.is_empty(),
        "documented but neither registered nor declared pending: {orphans:#?}\n\
         Either wire the route, or add it to PENDING with the unit that owes it."
    );
}

/// A `PENDING` entry that has since been wired must be deleted, so the list cannot rot.
#[test]
fn the_pending_list_does_not_rot() {
    let root = repo_root();
    let live = control_routes(&root);
    let doc = documented(&read(&root.join("openapi/apexrouter-v1.yaml")));

    let mut landed = Vec::new();
    for (path, owed_by) in PENDING {
        if live.contains_key(path) {
            landed.push(format!("{path} (was owed by {owed_by})"));
        }
        assert!(
            doc.contains_key(path),
            "{path} is on PENDING but is not in the OpenAPI file at all — remove it or document it"
        );
    }

    assert!(
        landed.is_empty(),
        "these routes are now registered and must be removed from PENDING in \
         crates/apexrouter-server/tests/openapi_routes.rs: {landed:#?}"
    );
}

/// The three explicit proxy routes are documented, and the catch-all still exists.
#[test]
fn the_proxy_surface_matches_the_document() {
    let root = repo_root();
    let doc = documented(&read(&root.join("openapi/apexrouter-v1.yaml")));
    let live = proxy_routes(&root);
    let handler = read(&root.join("crates/apexrouter-router/src/handler.rs"));

    assert!(
        handler.contains(".fallback(any(proxy_handler))"),
        "the proxy catch-all must stay `.fallback(any(proxy_handler))` — a `/{{*path}}` route \
         panics on Router::merge against the static-asset router in axum 0.8"
    );

    for (path, methods) in &live {
        let d = doc
            .get(path)
            .unwrap_or_else(|| panic!("proxy route {path} is registered but not documented"));
        assert!(
            d.listeners.contains("proxy"),
            "{path} is registered on the proxy listener but x-listener does not say so"
        );
        let undocumented: Vec<_> = methods.difference(&d.methods).cloned().collect();
        assert!(
            undocumented.is_empty(),
            "{path}: {undocumented:?} registered on the proxy listener but not documented"
        );
    }

    // Every remaining proxy-listener path is served by the fallback, and is listed as such.
    let fallback: BTreeSet<&str> = PROXY_FALLBACK.iter().copied().collect();
    for (p, _d) in doc.iter().filter(|(_, d)| d.listeners.contains("proxy")) {
        if live.contains_key(p) || fallback.contains(p.as_str()) {
            continue;
        }
        panic!(
            "{p} is documented on the proxy listener but is neither registered nor listed in \
             PROXY_FALLBACK"
        );
    }
}

/// Every operation carries the four things a generator needs, and ids are unique.
#[test]
fn the_openapi_document_is_well_formed() {
    let root = repo_root();
    let raw = read(&root.join("openapi/apexrouter-v1.yaml"));
    let doc = documented(&raw);

    assert!(
        raw.starts_with("openapi: 3.1.0\n"),
        "the document must declare OpenAPI 3.1.0 on its first line"
    );
    assert!(
        !doc.is_empty(),
        "no paths were parsed — the scanner or the file is broken"
    );

    for (p, d) in &doc {
        assert!(!d.methods.is_empty(), "{p} declares no operations");
        assert!(
            !d.listeners.is_empty(),
            "{p} declares no x-listener; every operation must name the listener that serves it"
        );
        for l in &d.listeners {
            assert!(
                l == "proxy" || l == "control",
                "{p} has an unknown x-listener {l:?}"
            );
        }
    }

    // operationId uniqueness, and every $ref resolves to a declared component.
    let mut ids: BTreeSet<&str> = BTreeSet::new();
    for line in raw.lines() {
        if let Some(rest) = line.trim().strip_prefix("operationId:") {
            let id = rest.trim();
            assert!(ids.insert(id), "duplicate operationId {id:?}");
        }
    }
    assert!(ids.len() >= doc.len(), "fewer operationIds than paths");

    let declared = declared_components(&raw);
    for (kind, name) in referenced_components(&raw) {
        assert!(
            declared.contains(&format!("{kind}/{name}")),
            "$ref '#/components/{kind}/{name}' does not resolve"
        );
    }
}

/// Component names declared under `components:` as `<kind>/<name>`.
fn declared_components(yaml: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut in_components = false;
    let mut kind: Option<String> = None;
    for line in yaml.lines() {
        if line.trim_start().starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent == 0 {
            in_components = line.starts_with("components:");
            kind = None;
            continue;
        }
        if !in_components {
            continue;
        }
        if indent == 2 && line.trim_end().ends_with(':') {
            kind = Some(line.trim().trim_end_matches(':').to_owned());
        } else if indent == 4 && line.trim_end().ends_with(':') {
            if let Some(k) = &kind {
                out.insert(format!("{k}/{}", line.trim().trim_end_matches(':')));
            }
        }
    }
    out
}

/// Every `#/components/<kind>/<name>` reference in the document.
fn referenced_components(yaml: &str) -> BTreeSet<(String, String)> {
    let mut out = BTreeSet::new();
    let needle = "#/components/";
    let mut from = 0usize;
    while let Some(rel) = yaml[from..].find(needle) {
        let at = from + rel + needle.len();
        let rest = &yaml[at..];
        let end = rest
            .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '/'))
            .unwrap_or(rest.len());
        let mut parts = rest[..end].splitn(2, '/');
        if let (Some(kind), Some(name)) = (parts.next(), parts.next()) {
            if !kind.is_empty() && !name.is_empty() {
                out.insert((kind.to_owned(), name.to_owned()));
            }
        }
        from = at;
    }
    out
}

/// Report any `api::<module>::router()` that exists but is never merged into an application.
///
/// This is a **warning, not an assertion**: it names a defect in another unit's file, and a
/// hard failure here would break the build the moment that unit fixes it. `cargo test -- --nocapture`
/// shows the list.
#[test]
fn every_api_module_router_is_merged_somewhere() {
    let root = repo_root();
    let api = root.join("crates/apexrouter-server/src/api");
    let lib = read(&root.join("crates/apexrouter-server/src/lib.rs"));
    let modrs = read(&api.join("mod.rs"));

    // Doc comments in lib.rs list the merges that are still to come; they are not code.
    let code: String = lib
        .lines()
        .chain(modrs.lines())
        .filter(|l| !l.trim_start().starts_with("///") && !l.trim_start().starts_with("//!"))
        .collect::<Vec<_>>()
        .join("\n");

    let mut unmerged = Vec::new();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&api)
        .unwrap_or_else(|e| panic!("cannot list {}: {e}", api.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    entries.sort();

    for path in entries {
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if stem == "mod" || path.extension().is_none_or(|x| x != "rs") {
            continue;
        }
        let src = read(&path);
        if !src.contains("pub fn router()") || routes_in_file(&src).is_empty() {
            continue;
        }
        let merged = code.contains(&format!("{stem}::router()"));
        if !merged {
            unmerged.push(stem.to_owned());
        }
    }

    if !unmerged.is_empty() {
        eprintln!(
            "WARNING: these api modules publish a router() that nothing merges, so their routes \
             are documented and compiled but unreachable at runtime: {unmerged:?}. \
             Only unit S-01 may edit server/src/lib.rs; each needs one `.merge(api::<m>::router())` \
             line in `v1_routes()`."
        );
    }
}
