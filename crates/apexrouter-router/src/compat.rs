//! OWNER: unit R-09 (router/src/compat.rs). Do not edit outside that unit.
//!
//! The three byte-compatible legacy routes on the proxy listener: `/health`, `/providers`
//! and `POST /switch`.
//!
//! Three documented silent no-ops are fixed here: an `api_key` in a `together` body is now
//! persisted as a `CredentialRef`; `local` now copies the instance's key; and a malformed
//! instance JSON returns a JSON `400`, not an HTML 500. Provider probes run **concurrently**
//! with a 3 s cap, where the Python ran them serially for ~8 s, and Together is detected
//! from the **full credential chain**, not just `$TOGETHER_API_KEY`.
//!
//! `POST /switch` is a mutation and gets the mutation gate: unauthenticated `/switch` with
//! an arbitrary `base_url` plus an injected key is a **credential-exfiltration primitive**,
//! not merely SSRF, so any supplied URL is validated against `[compat] allow_switch_hosts`.
//!
//! # What is byte-compatible, and what is additive
//!
//! Everything the Python emitted is emitted again, in the same place, with the same type:
//!
//! | route | legacy keys, preserved exactly | additive |
//! |---|---|---|
//! | `/health` | `ok`, `provider`, `uptime` | `product`, `version` |
//! | `/providers` | `active`, `target`, `providers{available,url}`, `local_instances{name,port,running}` | `endpoints[]`, `routes[]` |
//! | `/switch` | `{"status":"ok","provider":…}` and every documented error body | the `{"provider":"endpoint","id":…}` and `{"alias":…}` request forms |
//!
//! # Where the mutation gate lives
//!
//! Rules 1 and 3 of `ARCHITECTURE.md` §9.3 (the `Host` allowlist, the bearer fallback) need
//! the listener's bind address and `[server]`, which the request path does not carry; the
//! server applies `apexrouter_server::auth::require_mutation_origin` as middleware in front
//! of this handler. Rule 2 — the CSRF rule that actually stops a cross-origin `fetch` with
//! `Content-Type: text/plain` — needs nothing but the request's own headers, so it is
//! enforced here too: defence in depth, and the reason this handler takes a `HeaderMap`.

use crate::registry::BackendRegistry;
use crate::resolve::{RequestClass, UnknownModelPolicy};
use crate::Router;
use apexrouter_core::config::{CompatCfg, Config};
use apexrouter_core::error::Result;
use apexrouter_core::paths::Paths;
use apexrouter_core::secret::{store_user_credential, Secret};
use apexrouter_core::store::Store;
use apexrouter_core::{secret, upstream};
use apexrouter_protocol::ModelRoute;
use apexrouter_protocol::{
    Alias, Backend, BackendId, BackendKind, BackendLimits, BackendSelector, CredentialSource,
    Health, Protocol, Provenance, ProviderId, RouteFile, RouteTarget, Strategy, PRODUCT, VERSION,
};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// The legacy name for a vast.ai backend. **`vast-gguf` stays on the wire** (§5.4).
const P_VAST: &str = "vast-gguf";
/// The legacy name for the Together provider.
const P_TOGETHER: &str = "together";
/// The legacy name for a local `llama-server`.
const P_LOCAL: &str = "local";

/// Where LocalRouter's `resolve_target()` fell back to when `.active_endpoint` was absent,
/// unreadable or corrupt. Preserved so a fresh install answers `/providers` the same way.
const LEGACY_FALLBACK_TARGET: &str = "http://127.0.0.1:8800/v1";
/// Together's default base URL, as the Python hardcoded it.
const TOGETHER_DEFAULT_BASE: &str = "https://api.together.ai/v1";
/// Together's default model, as the Python hardcoded it.
const TOGETHER_DEFAULT_MODEL: &str = "meta-llama/Llama-3.1-8B-Instruct-Turbo";

/// The wall-clock cap on the whole `/providers` probe fan-out. The Python spent ~8 s here
/// because it probed serially with 3 s + 5 s timeouts; these run concurrently.
const PROBE_CAP: Duration = Duration::from_secs(3);

/// `USER_HZ`. Linux fixes this at 100 for `/proc/<pid>/stat` field 22, whatever `CONFIG_HZ`
/// says.
const USER_HZ: f64 = 100.0;

/// Fallback origin for `uptime` when `/proc` cannot be read: first touch of a legacy route.
static START: OnceLock<Instant> = OnceLock::new();

// ---------------------------------------------------------------------------------------
// GET /health
// ---------------------------------------------------------------------------------------

/// A superset of the LocalRouter shape and the house shape:
/// `{"ok":true,"product":"apexrouter","version":…,"provider":…,"uptime":…}`. Always 200;
/// never probes a backend.
pub async fn legacy_health(State(r): State<Router>) -> Json<Value> {
    let view = active_view(&r);
    Json(health_body(&view.provider, process_uptime_secs()))
}

/// The `/health` document. Legacy keys first, in the legacy order.
fn health_body(provider: &str, uptime: f64) -> Value {
    json!({
        "ok": true,
        "provider": provider,
        "uptime": uptime,
        "product": PRODUCT,
        "version": VERSION,
    })
}

/// Seconds since this process started, matching the legacy `time.time() - START_TIME`.
///
/// Read from `/proc/self/stat` + `/proc/uptime` so it is the *process's* age rather than
/// the age of the first `/health` call; if either is unreadable it degrades to a monotonic
/// clock started at the first legacy request.
fn process_uptime_secs() -> f64 {
    fn from_proc() -> Option<f64> {
        let ticks = apexrouter_core::proc::start_time_ticks(std::process::id()).ok()?;
        let uptime = std::fs::read_to_string("/proc/uptime").ok()?;
        let since_boot: f64 = uptime.split_whitespace().next()?.parse().ok()?;
        let age = since_boot - (ticks as f64 / USER_HZ);
        (age.is_finite() && age >= 0.0).then_some(age)
    }
    from_proc().unwrap_or_else(|| START.get_or_init(Instant::now).elapsed().as_secs_f64())
}

// ---------------------------------------------------------------------------------------
// GET /providers
// ---------------------------------------------------------------------------------------

/// The **exact** legacy JSON shape (`active`, `target`, `providers{}`, `local_instances[]`)
/// plus additive `endpoints[]` and `routes[]`.
pub async fn legacy_providers(State(r): State<Router>) -> Json<Value> {
    let cfg = r.cfg.load_full();
    let view = active_view(&r);
    let backends = r.registry().snapshot();

    // The two legacy provider slots. Together's key comes from the FULL chain — the
    // documented inconsistency (the Python looked only at $TOGETHER_API_KEY here) is fixed.
    let vast_url = vast_url(&view, &backends);
    let together_url = together_url(&cfg);
    let together_key = together_credential(&cfg);

    let (vast_available, together_available) = probe_pair(
        &r.http,
        &vast_url,
        &together_url,
        together_key.as_ref(),
        PROBE_CAP,
    )
    .await;

    let mut instances = match cfg.compat.read_legacy_state.then(legacy_instances_dir) {
        Some(Some(dir)) => legacy_instances(&dir),
        _ => Vec::new(),
    };
    merge_instances(&mut instances, live_instances(&backends));

    Json(providers_body(
        &view.provider,
        &view.target,
        &ProviderProbe {
            url: vast_url,
            available: vast_available,
        },
        &ProviderProbe {
            url: together_url,
            available: together_available,
        },
        instances,
        json_list(&backends),
        load_routes_json(),
    ))
}

/// One row of the legacy `providers{}` map.
struct ProviderProbe {
    /// The base URL as the legacy shape reports it — always ending in `/v1`.
    url: String,
    /// Whether the probe answered inside the cap.
    available: bool,
}

/// The `/providers` document: legacy keys, in legacy order, then the additive ones.
fn providers_body(
    active: &str,
    target: &str,
    vast: &ProviderProbe,
    together: &ProviderProbe,
    local_instances: Vec<Value>,
    endpoints: Vec<Value>,
    routes: Vec<Value>,
) -> Value {
    json!({
        "active": active,
        "target": target,
        "providers": {
            P_VAST: { "available": vast.available, "url": vast.url },
            P_TOGETHER: { "available": together.available, "url": together.url },
        },
        "local_instances": local_instances,
        "endpoints": endpoints,
        "routes": routes,
    })
}

/// Probe both legacy providers **concurrently**, each bounded by `cap`.
///
/// Together is only probed when a credential resolved: an unauthenticated `GET /v1/models`
/// against Together is a guaranteed 401, and reporting that as "unavailable" is the one
/// piece of the legacy behaviour worth keeping.
async fn probe_pair(
    http: &reqwest::Client,
    vast_url: &str,
    together_url: &str,
    together_key: Option<&Secret<String>>,
    cap: Duration,
) -> (bool, bool) {
    let vast = probe_available(http, vast_url, None, cap);
    let together = async {
        match together_key {
            Some(k) => probe_available(http, together_url, Some(k), cap).await,
            None => false,
        }
    };
    tokio::join!(vast, together)
}

/// One probe. `available` means "answered like an OpenAI-compatible upstream", which is
/// strictly more than the legacy test (`curl` exiting 0, i.e. TCP+TLS connected).
async fn probe_available(
    http: &reqwest::Client,
    base_url: &str,
    cred: Option<&Secret<String>>,
    cap: Duration,
) -> bool {
    upstream::probe(http, base_url, cred, cap).await.healthy
}

/// Which URL to report for the `vast-gguf` slot: the live vast backend if there is one,
/// otherwise the legacy tunnel fallback.
fn vast_url(view: &ActiveView, backends: &[Backend]) -> String {
    if let Some(b) = view.backend.as_ref() {
        if is_vast(b) {
            return with_v1(&b.base_url);
        }
    }
    backends
        .iter()
        .find(|b| is_vast(b))
        .map(|b| with_v1(&b.base_url))
        .unwrap_or_else(|| LEGACY_FALLBACK_TARGET.to_owned())
}

/// Which URL to report for the `together` slot. The configured value is used **verbatim**:
/// a legacy `api.together.xyz` must never be rewritten to `.ai`.
fn together_url(cfg: &Config) -> String {
    cfg.providers
        .get(P_TOGETHER)
        .map(|p| p.base_url.trim())
        .filter(|s| !s.is_empty())
        .map(with_v1)
        .unwrap_or_else(|| TOGETHER_DEFAULT_BASE.to_owned())
}

/// Together's key, through the whole chain (`credentials.toml` → `api_key_file` →
/// `~/.vastai-gguf/config.toml` → `$TOGETHER_API_KEY`).
fn together_credential(cfg: &Config) -> Option<Secret<String>> {
    let paths = Paths::resolve().ok()?;
    let id = ProviderId::parse(P_TOGETHER).ok()?;
    secret::resolve_provider(cfg, &paths, &id)
        .ok()
        .flatten()
        .map(|c| c.secret)
}

/// `~/.vastai-gguf/local_instances`, when it can be resolved.
fn legacy_instances_dir() -> Option<PathBuf> {
    Paths::resolve()
        .ok()
        .map(|p| p.legacy().vastai_gguf.join("local_instances"))
}

/// The legacy glob, with the legacy error posture: a per-file failure is skipped, never
/// fatal. Sorted by name so the response is deterministic.
fn legacy_instances(dir: &Path) -> Vec<Value> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut rows: Vec<(String, Value)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(meta) = serde_json::from_slice::<LegacyInstanceMeta>(&bytes) else {
            tracing::debug!(path = %path.display(), "skipping malformed local instance");
            continue;
        };
        let name = meta
            .name
            .clone()
            .or_else(|| path.file_stem().and_then(|s| s.to_str()).map(str::to_owned))
            .unwrap_or_default();
        // The Python read `meta["port"]` unguarded; a row without one raised and was
        // swallowed by the bare `except`, so it never appeared. Same here.
        let Some(port) = meta.port else { continue };
        if name.is_empty() {
            continue;
        }
        let running = meta.pid.is_some_and(pid_alive);
        rows.push((
            name.clone(),
            json!({ "name": name, "port": port, "running": running }),
        ));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows.into_iter().map(|(_, v)| v).collect()
}

/// Our own local endpoints, rendered in the same three-key shape so the old TUI can read
/// them alongside the legacy ones.
fn live_instances(backends: &[Backend]) -> Vec<Value> {
    backends
        .iter()
        .filter(|b| matches!(b.kind, BackendKind::LocalLlama | BackendKind::LocalVllm))
        .filter_map(|b| {
            let port = url_port(&b.base_url)?;
            Some(json!({
                "name": b.id.as_str(),
                "port": port,
                "running": b.health.is_routable(),
            }))
        })
        .collect()
}

/// Append every row of `extra` whose `name` is not already present.
fn merge_instances(into: &mut Vec<Value>, extra: Vec<Value>) {
    for row in extra {
        let name = row.get("name").and_then(Value::as_str).unwrap_or_default();
        let dup = into
            .iter()
            .any(|r| r.get("name").and_then(Value::as_str) == Some(name));
        if !dup {
            into.push(row);
        }
    }
}

/// `kill(pid, 0)`, expressed as "the kernel still has a `/proc/<pid>/stat` for it".
fn pid_alive(pid: u32) -> bool {
    apexrouter_core::proc::start_time_ticks(pid).is_ok()
}

/// Serialise a slice of protocol types, dropping anything that will not serialise rather
/// than failing the whole response.
fn json_list<T: serde::Serialize>(items: &[T]) -> Vec<Value> {
    items
        .iter()
        .filter_map(|i| serde_json::to_value(i).ok())
        .collect()
}

/// The routing table's source of truth, as JSON. `/providers` must still answer on a bare
/// machine, so an unreadable route file is an empty list rather than an error.
fn load_routes_json() -> Vec<Value> {
    let Ok(paths) = Paths::resolve() else {
        return Vec::new();
    };
    match Store::new(paths).load_routes() {
        Ok(rf) => json_list(&rf.routes),
        Err(_) => Vec::new(),
    }
}

// ---------------------------------------------------------------------------------------
// POST /switch
// ---------------------------------------------------------------------------------------

/// The legacy switch verb, retargeting `default_alias`. Extended with
/// `{"provider":"endpoint","id":…}` and `{"alias":…}`.
pub async fn legacy_switch(State(r): State<Router>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(e) = require_same_origin(&headers) {
        return switch_error(&e);
    }
    let cfg = r.cfg.load_full();
    let target = match parse_switch(&body, &cfg.compat) {
        Ok(t) => t,
        Err(e) => return switch_error(&e),
    };
    match apply_switch(&r, &cfg, target).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => switch_error(&e),
    }
}

/// What a `/switch` body asked for.
#[derive(Clone, Debug, PartialEq)]
enum SwitchTarget {
    /// `{"provider":"together", "api_key"?, "base_url"?, "model_id"?}`.
    Together {
        /// Validated against `[compat] allow_switch_hosts`.
        base_url: String,
        /// Echoed into `.active_endpoint`, as the legacy file carried it.
        model_id: String,
        /// **Fix for silent no-op #1**: persisted as a `CredentialRef`.
        api_key: Option<String>,
    },
    /// `{"provider":"vast-gguf"}` — the legacy "delete `.active_endpoint`" branch.
    Vast,
    /// `{"provider":"local","name":…}`.
    Local {
        /// The instance name, i.e. `local_instances/<name>.json`.
        name: String,
    },
    /// Additive: `{"provider":"endpoint","id":…}`.
    Endpoint {
        /// A known backend id.
        id: BackendId,
    },
    /// Additive: `{"alias":…}`.
    AliasSwitch {
        /// A known alias in the routing table.
        alias: Alias,
    },
}

/// Every way `/switch` can refuse, with the legacy status and the legacy body.
#[derive(Clone, Debug, PartialEq)]
enum SwitchError {
    /// `400 {"error":"Invalid JSON body"}` — byte-identical to the Python.
    BadJson,
    /// `400 {"error":"Missing 'name' for local provider"}`.
    MissingName,
    /// `400 {"error":"Unknown provider: X"}`.
    UnknownProvider(String),
    /// `404 {"error":"Local instance 'X' not found"}`.
    InstanceNotFound(String),
    /// **Fix for the HTML 500**: a malformed instance JSON is a JSON `400`.
    BadInstance(String),
    /// `404` for the additive alias and endpoint forms.
    UnknownAlias(String),
    /// `400` for a `base_url` that is not a URL we would ever call.
    BadUrl(String),
    /// `403` — the host is not in `[compat] allow_switch_hosts`.
    HostNotAllowed(String),
    /// `403` — rule 2 of the mutation gate.
    CrossOrigin,
    /// `500`, still JSON.
    Internal(String),
}

impl SwitchError {
    /// The status and message this refusal renders as.
    fn parts(&self) -> (StatusCode, String) {
        match self {
            SwitchError::BadJson => (StatusCode::BAD_REQUEST, "Invalid JSON body".to_owned()),
            SwitchError::MissingName => (
                StatusCode::BAD_REQUEST,
                "Missing 'name' for local provider".to_owned(),
            ),
            SwitchError::UnknownProvider(p) => {
                (StatusCode::BAD_REQUEST, format!("Unknown provider: {p}"))
            }
            SwitchError::InstanceNotFound(n) => (
                StatusCode::NOT_FOUND,
                format!("Local instance '{n}' not found"),
            ),
            SwitchError::BadInstance(why) => (StatusCode::BAD_REQUEST, why.clone()),
            SwitchError::UnknownAlias(a) => (StatusCode::NOT_FOUND, format!("Unknown alias '{a}'")),
            SwitchError::BadUrl(why) => {
                (StatusCode::BAD_REQUEST, format!("Invalid base_url: {why}"))
            }
            SwitchError::HostNotAllowed(h) => (
                StatusCode::FORBIDDEN,
                format!("base_url host '{h}' is not in [compat] allow_switch_hosts"),
            ),
            SwitchError::CrossOrigin => (
                StatusCode::FORBIDDEN,
                "cross-origin request refused: /switch is a mutation".to_owned(),
            ),
            SwitchError::Internal(why) => (StatusCode::INTERNAL_SERVER_ERROR, why.clone()),
        }
    }
}

/// Every refusal is `{"error": "<message>"}`, the shape the legacy clients parse — never an
/// HTML traceback page, whatever went wrong.
fn switch_error(e: &SwitchError) -> Response {
    let (status, message) = e.parts();
    (status, Json(json!({ "error": message }))).into_response()
}

/// Rule 2 of the mutation gate (`ARCHITECTURE.md` §9.3), which needs only the request's own
/// headers. `curl`, the CLI and Slint send neither header and pass unchanged.
fn require_same_origin(h: &HeaderMap) -> std::result::Result<(), SwitchError> {
    if let Some(site) = h.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        let site = site.trim();
        if !site.eq_ignore_ascii_case("same-origin") && !site.eq_ignore_ascii_case("none") {
            return Err(SwitchError::CrossOrigin);
        }
    }
    if let Some(origin) = h
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    {
        let host = h
            .get(axum::http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .trim();
        let authority = origin.trim().split("://").nth(1).unwrap_or_default();
        if authority.is_empty() || !authority.eq_ignore_ascii_case(host) {
            return Err(SwitchError::CrossOrigin);
        }
    }
    Ok(())
}

/// Parse every documented legacy body, plus the two additive forms.
fn parse_switch(body: &[u8], compat: &CompatCfg) -> std::result::Result<SwitchTarget, SwitchError> {
    let doc: Value = serde_json::from_slice(body).map_err(|_| SwitchError::BadJson)?;
    let Some(obj) = doc.as_object() else {
        return Err(SwitchError::BadJson);
    };

    // Additive: `{"alias":"coder"}` needs no `provider` at all.
    if let Some(a) = obj.get("alias").and_then(Value::as_str) {
        let alias = Alias::parse(a.trim()).map_err(|e| SwitchError::BadInstance(e.to_string()))?;
        return Ok(SwitchTarget::AliasSwitch { alias });
    }

    let provider = obj.get("provider").and_then(Value::as_str).unwrap_or("");
    match provider {
        P_TOGETHER => {
            let base_url =
                string_field(obj, "base_url").unwrap_or_else(|| TOGETHER_DEFAULT_BASE.to_owned());
            // The whole reason this validation exists: a `base_url` the caller chose plus
            // an `api_key` the caller supplied is a credential-exfiltration primitive.
            check_host(&base_url, &compat.allow_switch_hosts)?;
            Ok(SwitchTarget::Together {
                base_url,
                model_id: string_field(obj, "model_id")
                    .unwrap_or_else(|| TOGETHER_DEFAULT_MODEL.to_owned()),
                api_key: string_field(obj, "api_key"),
            })
        }
        // One enum, serde aliases: `vast-gguf` and `vast_gguf` are the same thing (§5.4).
        P_VAST | "vast_gguf" => Ok(SwitchTarget::Vast),
        P_LOCAL | "local-gguf" | "local_gguf" => Ok(SwitchTarget::Local {
            name: string_field(obj, "name").ok_or(SwitchError::MissingName)?,
        }),
        "endpoint" => {
            let raw = string_field(obj, "id")
                .ok_or_else(|| SwitchError::BadInstance("Missing 'id' for endpoint".to_owned()))?;
            let id = BackendId::parse(&raw).map_err(|e| SwitchError::BadInstance(e.to_string()))?;
            Ok(SwitchTarget::Endpoint { id })
        }
        other => Err(SwitchError::UnknownProvider(if other.is_empty() {
            "null".to_owned()
        } else {
            other.to_owned()
        })),
    }
}

/// A trimmed, non-empty string field, or `None`.
fn string_field(obj: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// A `base_url` is only acceptable if it is `http`/`https` **and** its host is allowlisted.
fn check_host(url: &str, allow: &[String]) -> std::result::Result<(), SwitchError> {
    let parsed = reqwest::Url::parse(url.trim()).map_err(|e| SwitchError::BadUrl(e.to_string()))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(SwitchError::BadUrl(format!(
            "scheme '{}' is not http(s)",
            parsed.scheme()
        )));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| SwitchError::BadUrl("no host".to_owned()))?
        .to_ascii_lowercase();
    let with_port = match parsed.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.clone(),
    };
    let ok = allow.iter().any(|a| {
        let a = a.trim().to_ascii_lowercase();
        a == host || a == with_port
    });
    if ok {
        Ok(())
    } else {
        Err(SwitchError::HostNotAllowed(host))
    }
}

/// One `local_instances/<name>.json` document, as LocalRouter wrote it.
#[derive(Clone, Debug, Default, Deserialize)]
struct LegacyInstanceMeta {
    /// Usually equal to the file stem.
    #[serde(default)]
    name: Option<String>,
    /// Defaults to `127.0.0.1`, as `resolve_target()` did.
    #[serde(default)]
    host: Option<String>,
    /// Absent means the row is unusable, exactly as in the Python.
    #[serde(default)]
    port: Option<u16>,
    /// Liveness, by `kill(pid, 0)`.
    #[serde(default)]
    pid: Option<u32>,
    /// Shown in `.active_endpoint`; the file it names may well be gone, and that is normal.
    #[serde(default)]
    model_path: Option<String>,
    /// **Fix for silent no-op #2**: the Python never copied this, so a local backend
    /// started with `--api-key` could never be authenticated through `/switch`.
    #[serde(default)]
    api_key: Option<String>,
}

/// Read one instance file, distinguishing "absent" (404) from "malformed" (400).
fn load_instance(dir: &Path, name: &str) -> std::result::Result<LegacyInstanceMeta, SwitchError> {
    let path = dir.join(format!("{name}.json"));
    let bytes = std::fs::read(&path).map_err(|_| SwitchError::InstanceNotFound(name.to_owned()))?;
    let mut meta: LegacyInstanceMeta = serde_json::from_slice(&bytes).map_err(|e| {
        SwitchError::BadInstance(format!("Local instance '{name}' is not valid JSON: {e}"))
    })?;
    if meta.port.is_none() {
        return Err(SwitchError::BadInstance(format!(
            "Local instance '{name}' has no port"
        )));
    }
    if meta.name.is_none() {
        meta.name = Some(name.to_owned());
    }
    Ok(meta)
}

/// Persist the switch: register the backend, retarget the default alias, recompile the
/// table, mirror `.active_endpoint`.
async fn apply_switch(
    r: &Router,
    cfg: &Config,
    target: SwitchTarget,
) -> std::result::Result<Value, SwitchError> {
    let paths = Paths::resolve().map_err(|e| SwitchError::Internal(e.to_string()))?;
    let store = Store::new(paths.clone());

    // 1. Work out the backend this switch points at, persisting any credential it carries.
    let (backend, answer) = match &target {
        SwitchTarget::Together {
            base_url,
            model_id,
            api_key,
        } => {
            if let Some(key) = api_key {
                let id = ProviderId::parse(P_TOGETHER)
                    .map_err(|e| SwitchError::Internal(e.to_string()))?;
                store_user_credential(&paths, &id, Secret::new(key.clone()))
                    .map_err(|e| SwitchError::Internal(e.to_string()))?;
            }
            let b = managed_backend(P_TOGETHER, base_url, model_id, api_key.is_some())?;
            (Some(b), json!({ "status": "ok", "provider": P_TOGETHER }))
        }
        SwitchTarget::Vast => (
            Some(vast_backend(r)?),
            json!({ "status": "ok", "provider": P_VAST }),
        ),
        SwitchTarget::Local { name } => {
            let dir = paths.legacy().vastai_gguf.join("local_instances");
            let meta = load_instance(&dir, name)?;
            if let Some(key) = meta
                .api_key
                .as_deref()
                .map(str::trim)
                .filter(|k| !k.is_empty())
            {
                let id =
                    ProviderId::parse(name).map_err(|e| SwitchError::Internal(e.to_string()))?;
                store_user_credential(&paths, &id, Secret::new(key.to_owned()))
                    .map_err(|e| SwitchError::Internal(e.to_string()))?;
            }
            (
                Some(local_backend(&meta)?),
                json!({ "status": "ok", "provider": P_LOCAL }),
            )
        }
        SwitchTarget::Endpoint { id } => {
            if r.registry().get(id).is_none() {
                return Err(SwitchError::UnknownAlias(id.as_str().to_owned()));
            }
            (
                None,
                json!({ "status": "ok", "provider": "endpoint", "id": id.as_str() }),
            )
        }
        SwitchTarget::AliasSwitch { alias } => (
            None,
            json!({ "status": "ok", "provider": "alias", "alias": alias.as_str() }),
        ),
    };

    // 2. Register it, so live state (permits, breaker) is created or preserved.
    if let Some(b) = backend.as_ref() {
        r.registry().upsert(b.clone(), &cfg.router);
        let mut all = store
            .load_backends()
            .map_err(|e| SwitchError::Internal(e.to_string()))?;
        all.retain(|x| x.id != b.id);
        all.push(b.clone());
        store
            .save_backends(&all)
            .map_err(|e| SwitchError::Internal(e.to_string()))?;
    }

    // 3. Retarget the default alias and persist the route file.
    let mut routes = store
        .load_routes()
        .map_err(|e| SwitchError::Internal(e.to_string()))?;
    let alias = match (&target, backend.as_ref()) {
        (SwitchTarget::AliasSwitch { alias }, _) => {
            set_default_alias(&mut routes, alias)?;
            alias.clone()
        }
        (SwitchTarget::Endpoint { id }, _) => retarget_default(&mut routes, id),
        (_, Some(b)) => retarget_default(&mut routes, &b.id),
        (_, None) => return Err(SwitchError::Internal("no backend to target".to_owned())),
    };
    store
        .save_routes(&routes)
        .map_err(|e| SwitchError::Internal(e.to_string()))?;

    // 4. Recompile. A failed compile leaves the running table serving and says why.
    match crate::TableBuilder::compile(cfg, &routes, r.registry()) {
        Ok(table) => r.store_table(table),
        Err(report) => {
            let why = report
                .issues
                .iter()
                .map(|i| format!("{}: {}", i.field, i.message))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(SwitchError::Internal(format!("route table invalid: {why}")));
        }
    }

    // 5. Mirror `.active_endpoint` for the old TUI, when that is switched on. A mirroring
    //    failure must not fail the switch that already happened.
    if let Some(route) = routes.routes.iter().find(|r| r.alias == alias) {
        if let Err(e) = mirror_active_endpoint(&cfg.compat, route, r.registry()) {
            tracing::warn!(error = %e, "could not mirror .active_endpoint");
        }
    }

    Ok(answer)
}

/// Build the `Backend` a `{"provider":"together"}` switch points at.
fn managed_backend(
    id: &str,
    base_url: &str,
    model_id: &str,
    key_stored: bool,
) -> std::result::Result<Backend, SwitchError> {
    let id = BackendId::parse(id).map_err(|e| SwitchError::Internal(e.to_string()))?;
    let mut tags = vec!["compat".to_owned()];
    if !model_id.is_empty() {
        // The legacy `.active_endpoint` carried a model id; keep it visible rather than
        // inventing a catalogue entry we have not probed.
        tags.push(format!("model:{model_id}"));
    }
    Ok(Backend {
        label: format!("{} (via /switch)", id.as_str()),
        id,
        kind: BackendKind::Managed,
        protocol: Protocol::OpenAi,
        base_url: strip_v1(base_url),
        credential: if key_stored {
            CredentialSource::Managed {
                store: "credentials.toml".to_owned(),
            }
        } else {
            CredentialSource::Env {
                var: "TOGETHER_API_KEY".to_owned(),
            }
        },
        tags,
        models: Vec::new(),
        limits: BackendLimits::default(),
        price: None,
        health: Health::Unknown,
        provenance: Provenance::Manual,
        endpoint: None,
        enabled: true,
        devices: Vec::new(),
        last_error: None,
    })
}

/// Build the `Backend` a `{"provider":"vast-gguf"}` switch points at: the live vast backend
/// if we have one, otherwise the legacy tunnel fallback.
fn vast_backend(r: &Router) -> std::result::Result<Backend, SwitchError> {
    if let Some(b) = r.registry().snapshot().into_iter().find(is_vast) {
        return Ok(b);
    }
    Ok(Backend {
        id: BackendId::parse(P_VAST).map_err(|e| SwitchError::Internal(e.to_string()))?,
        kind: BackendKind::Node,
        protocol: Protocol::OpenAi,
        label: "vast tunnel (legacy fallback)".to_owned(),
        base_url: strip_v1(LEGACY_FALLBACK_TARGET),
        credential: CredentialSource::None,
        tags: vec!["compat".to_owned()],
        models: Vec::new(),
        limits: BackendLimits::default(),
        price: None,
        health: Health::Unknown,
        provenance: Provenance::Manual,
        endpoint: None,
        enabled: true,
        devices: Vec::new(),
        last_error: None,
    })
}

/// Build the `Backend` a `{"provider":"local","name":…}` switch points at, **copying the
/// instance's key** — the second documented silent no-op.
fn local_backend(meta: &LegacyInstanceMeta) -> std::result::Result<Backend, SwitchError> {
    let name = meta.name.clone().unwrap_or_default();
    let id = BackendId::parse(&name)
        .map_err(|e| SwitchError::BadInstance(format!("Local instance '{name}': {e}")))?;
    let host = meta.host.clone().unwrap_or_else(|| "127.0.0.1".to_owned());
    let port = meta.port.unwrap_or(8100);
    let has_key = meta
        .api_key
        .as_deref()
        .map(str::trim)
        .is_some_and(|k| !k.is_empty());
    let mut tags = vec!["compat".to_owned()];
    if let Some(p) = meta.model_path.as_deref().filter(|p| !p.is_empty()) {
        tags.push(format!("model_path:{p}"));
    }
    Ok(Backend {
        label: format!("{name} (legacy local instance)"),
        id,
        kind: BackendKind::LocalLlama,
        protocol: Protocol::OpenAi,
        base_url: format!("http://{host}:{port}"),
        credential: if has_key {
            CredentialSource::Managed {
                store: "credentials.toml".to_owned(),
            }
        } else {
            CredentialSource::None
        },
        tags,
        models: Vec::new(),
        limits: BackendLimits::default(),
        price: None,
        health: Health::Unknown,
        provenance: Provenance::Imported,
        endpoint: None,
        enabled: true,
        devices: Vec::new(),
        last_error: None,
    })
}

/// Point the default alias's route at exactly one backend, creating the route when the
/// table has none. Returns the alias that was retargeted.
fn retarget_default(routes: &mut RouteFile, id: &BackendId) -> Alias {
    let alias = routes.default_alias.clone();
    let target = RouteTarget {
        backend: BackendSelector::Id(id.clone()),
        model: None,
        weight: 1,
    };
    match routes.routes.iter_mut().find(|r| r.alias == alias) {
        Some(route) => route.targets = vec![target],
        None => routes.routes.push(ModelRoute {
            alias: alias.clone(),
            targets: vec![target],
            strategy: Strategy::FirstHealthy,
            filter: Default::default(),
            retry: Default::default(),
            is_default: true,
            description: Some("set by POST /switch".to_owned()),
        }),
    }
    for route in routes.routes.iter_mut() {
        route.is_default = route.alias == alias;
    }
    alias
}

/// Make an existing alias the default one.
fn set_default_alias(
    routes: &mut RouteFile,
    alias: &Alias,
) -> std::result::Result<(), SwitchError> {
    if !routes.routes.iter().any(|r| &r.alias == alias) {
        return Err(SwitchError::UnknownAlias(alias.as_str().to_owned()));
    }
    routes.default_alias = alias.clone();
    for route in routes.routes.iter_mut() {
        route.is_default = &route.alias == alias;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// .active_endpoint mirroring
// ---------------------------------------------------------------------------------------

/// Mirror the default alias into `.active_endpoint` in the legacy shape, atomically.
/// Off by default (`[compat] active_endpoint_path = ""`).
///
/// The three legacy shapes are reproduced exactly: a `together` document, a `local`
/// document, and — for a vast target — **no file at all**, because `resolve_target()`'s
/// vast branch was "the file is absent".
///
/// # Errors
/// Returns [`apexrouter_core::error::Error::Io`] when the mirror path cannot be written,
/// and [`apexrouter_core::error::Error::Json`] if the document will not serialise.
pub fn mirror_active_endpoint(
    cfg: &CompatCfg,
    route: &ModelRoute,
    reg: &BackendRegistry,
) -> Result<()> {
    let configured = cfg.active_endpoint_path.trim();
    if configured.is_empty() {
        return Ok(());
    }
    let path = expand_tilde(configured);

    match primary_backend(route, reg)
        .as_ref()
        .and_then(active_endpoint_doc)
    {
        Some(doc) => {
            let mut bytes = serde_json::to_vec(&doc)?;
            bytes.push(b'\n');
            write_active_endpoint(&path, &bytes)
        }
        // vast-gguf, or nothing resolvable: the legacy representation is an absent file.
        None => remove_active_endpoint(&path),
    }
}

/// The backend a route's first resolvable target names.
fn primary_backend(route: &ModelRoute, reg: &BackendRegistry) -> Option<Backend> {
    for target in &route.targets {
        match &target.backend {
            BackendSelector::Id(id) => {
                if let Some(live) = reg.get(id) {
                    return Some((*live.meta.load_full()).clone());
                }
            }
            BackendSelector::Tag(tag) => {
                for live in reg.all() {
                    let meta = live.meta.load_full();
                    if meta.tags.iter().any(|t| t == tag) {
                        return Some((*meta).clone());
                    }
                }
            }
            BackendSelector::Glob(pattern) => {
                for live in reg.all() {
                    let meta = live.meta.load_full();
                    if glob_match(pattern, meta.id.as_str()) {
                        return Some((*meta).clone());
                    }
                }
            }
        }
    }
    None
}

/// The legacy `.active_endpoint` document for a backend, or `None` for a vast target.
fn active_endpoint_doc(b: &Backend) -> Option<Value> {
    let switched_at = now_utc_z();
    if is_vast(b) {
        return None;
    }
    match b.kind {
        BackendKind::VastLlama | BackendKind::VastVllm => None,
        BackendKind::LocalLlama | BackendKind::LocalVllm => Some(json!({
            "provider": P_LOCAL,
            "name": b.id.as_str(),
            "host": url_host(&b.base_url).unwrap_or_else(|| "127.0.0.1".to_owned()),
            "port": url_port(&b.base_url).unwrap_or(8100),
            "model_path": tag_value(b, "model_path:"),
            "switched_at": switched_at,
        })),
        BackendKind::Managed | BackendKind::Node => {
            let base = with_v1(&b.base_url);
            Some(json!({
                "provider": legacy_provider_name(b),
                "model_id": model_id_of(b),
                "base_url": base,
                "endpoint": format!("{base}/chat/completions"),
                "switched_at": switched_at,
            }))
        }
    }
}

/// The value behind a `<prefix>value` tag, or `""`.
fn tag_value(b: &Backend, prefix: &str) -> String {
    b.tags
        .iter()
        .find_map(|t| t.strip_prefix(prefix))
        .map(str::to_owned)
        .unwrap_or_default()
}

/// The model id `/switch` recorded, falling back to the first catalogued model.
fn model_id_of(b: &Backend) -> String {
    let tagged = tag_value(b, "model:");
    if !tagged.is_empty() {
        return tagged;
    }
    b.models.first().map(|m| m.id.clone()).unwrap_or_default()
}

/// tmp in the same directory → `fsync` → `rename` → `fsync(dir)`, mode `0600` set at
/// `OpenOptions` time. The old TUI reads this file while we write it, so a reader must see
/// either the whole old document or the whole new one.
fn write_active_endpoint(path: &Path, bytes: &[u8]) -> Result<()> {
    use apexrouter_core::error::Error;
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let dir = path.parent().ok_or_else(|| Error::Invalid {
        what: "active_endpoint_path".to_owned(),
        why: format!("{} has no parent directory", path.display()),
    })?;
    let io = |p: &Path, e: std::io::Error| Error::Io {
        path: p.display().to_string(),
        source: e,
    };
    std::fs::create_dir_all(dir).map_err(|e| io(dir, e))?;

    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("active_endpoint"),
        std::process::id()
    ));
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| io(&tmp, e))?;
        f.write_all(bytes).map_err(|e| io(&tmp, e))?;
        f.sync_all().map_err(|e| io(&tmp, e))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        io(path, e)
    })?;
    let dir_file = std::fs::File::open(dir).map_err(|e| io(dir, e))?;
    dir_file.sync_all().map_err(|e| io(dir, e))?;
    Ok(())
}

/// The vast branch of `resolve_target()` was "the file is absent"; reproduce that.
fn remove_active_endpoint(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(apexrouter_core::error::Error::Io {
            path: path.display().to_string(),
            source: e,
        }),
    }
}

// ---------------------------------------------------------------------------------------
// small shared helpers
// ---------------------------------------------------------------------------------------

/// What the legacy routes report as "the thing traffic currently goes to".
struct ActiveView {
    /// The legacy provider name: `vast-gguf`, `together`, `local`, or a backend id.
    provider: String,
    /// The legacy target URL, which **always** ends in `/v1`.
    target: String,
    /// The backend behind the default alias, when one resolved.
    backend: Option<Backend>,
}

/// Resolve the default alias to the backend traffic would go to right now. Synchronous, no
/// I/O, no probing — `/health` must never block on a backend.
fn active_view(r: &Router) -> ActiveView {
    let table = r.table();
    let resolved = table
        .resolve(None, RequestClass::Opaque, UnknownModelPolicy::Fallback)
        .ok()
        .and_then(|plan| plan.candidates.first().map(|c| c.backend.meta.load_full()));
    match resolved {
        Some(meta) => ActiveView {
            provider: legacy_provider_name(&meta),
            target: with_v1(&meta.base_url),
            backend: Some((*meta).clone()),
        },
        None => ActiveView {
            provider: P_VAST.to_owned(),
            target: LEGACY_FALLBACK_TARGET.to_owned(),
            backend: None,
        },
    }
}

/// The legacy provider name for a backend. Only three names ever existed; anything else
/// reports its own backend id, which is honest rather than a lie shaped like `together`.
fn legacy_provider_name(b: &Backend) -> String {
    if is_vast(b) {
        return P_VAST.to_owned();
    }
    match b.kind {
        BackendKind::VastLlama | BackendKind::VastVllm => P_VAST.to_owned(),
        BackendKind::LocalLlama | BackendKind::LocalVllm => P_LOCAL.to_owned(),
        BackendKind::Managed | BackendKind::Node => {
            if is_together(b) {
                P_TOGETHER.to_owned()
            } else {
                b.id.as_str().to_owned()
            }
        }
    }
}

/// Is this the vast tunnel, by kind, by id, or by the legacy tunnel port?
fn is_vast(b: &Backend) -> bool {
    matches!(b.kind, BackendKind::VastLlama | BackendKind::VastVllm)
        || b.id.as_str() == P_VAST
        || url_port(&b.base_url) == Some(apexrouter_protocol::DEFAULT_TUNNEL_PORT_RANGE.0)
}

/// Is this the Together provider, by id or by host?
fn is_together(b: &Backend) -> bool {
    b.id.as_str() == P_TOGETHER || url_host(&b.base_url).is_some_and(|h| h.contains("together."))
}

/// The legacy `target`/`url` fields always end in `/v1`; `Backend.base_url` never does.
fn with_v1(base_url: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    if base.ends_with("/v1") {
        base.to_owned()
    } else {
        format!("{base}/v1")
    }
}

/// The `Backend.base_url` invariant: never ends in `/v1`.
fn strip_v1(base_url: &str) -> String {
    let mut base = base_url.trim().trim_end_matches('/');
    while let Some(stripped) = base.strip_suffix("/v1") {
        base = stripped.trim_end_matches('/');
    }
    base.to_owned()
}

/// The host of a URL, lowercased.
fn url_host(url: &str) -> Option<String> {
    reqwest::Url::parse(url.trim())
        .ok()?
        .host_str()
        .map(str::to_ascii_lowercase)
}

/// The port of a URL, including the scheme's default.
fn url_port(url: &str) -> Option<u16> {
    reqwest::Url::parse(url.trim())
        .ok()?
        .port_or_known_default()
}

/// `~` against `$HOME`, and nothing else — `~other` is left alone, as everywhere else in
/// this workspace.
fn expand_tilde(s: &str) -> PathBuf {
    let Some(rest) = s.strip_prefix('~') else {
        return PathBuf::from(s);
    };
    if !rest.is_empty() && !rest.starts_with('/') {
        return PathBuf::from(s);
    }
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(rest.trim_start_matches('/')),
        None => PathBuf::from(s),
    }
}

/// The legacy `switched_at`: real UTC, seconds precision, trailing `Z` — the format the old
/// TUI's `%Y-%m-%dT%H:%M:%SZ` parses.
fn now_utc_z() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// The one wildcard `BackendSelector::Glob` needs: `*` matching any run of characters.
fn glob_match(pattern: &str, s: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == s,
        Some((prefix, suffix)) => {
            s.len() >= prefix.len() + suffix.len() && s.starts_with(prefix) && s.ends_with(suffix)
        }
    }
}

// ---------------------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ---- golden captures of the Python responses ----------------------------------------
    // Verbatim from `docs/port/05-proxy.md` §2, which was read out of
    // `LocalRouter/endpoint_proxy.py`.

    /// `GET /health` as `endpoint_proxy.py` emitted it.
    const PY_HEALTH: &str = r#"{"ok": true, "provider": "vast-gguf", "uptime": 1234.56}"#;

    /// `GET /providers` as `endpoint_proxy.py` emitted it.
    const PY_PROVIDERS: &str = r#"{
      "active": "vast-gguf",
      "target": "http://127.0.0.1:8800/v1",
      "providers": {
        "vast-gguf": {"available": false, "url": "http://127.0.0.1:8800/v1"},
        "together":  {"available": false, "url": "https://api.together.ai/v1"}
      },
      "local_instances": [{"name": "local-qwen35-9b", "port": 8100, "running": false}]
    }"#;

    /// The `POST /switch` success body, for all three legacy providers.
    const PY_SWITCH_OK: &str = r#"{"status":"ok","provider":"together"}"#;
    /// The `POST /switch` bad-body response.
    const PY_SWITCH_BAD_JSON: &str = r#"{"error":"Invalid JSON body"}"#;
    /// The `POST /switch` missing-name response.
    const PY_SWITCH_NO_NAME: &str = r#"{"error":"Missing 'name' for local provider"}"#;
    /// The `POST /switch` unknown-provider response.
    const PY_SWITCH_UNKNOWN: &str = r#"{"error":"Unknown provider: nope"}"#;
    /// The `POST /switch` missing-instance response.
    const PY_SWITCH_NO_INSTANCE: &str = r#"{"error":"Local instance 'ghost' not found"}"#;

    /// `.active_endpoint`, together shape.
    const PY_ACTIVE_TOGETHER: &str = r#"{"provider":"together","model_id":"meta-llama/Llama-3.1-8B-Instruct-Turbo","base_url":"https://api.together.ai/v1","endpoint":"https://api.together.ai/v1/chat/completions","switched_at":"2026-07-30T12:00:00Z"}"#;
    /// `.active_endpoint`, local shape.
    const PY_ACTIVE_LOCAL: &str = r#"{"provider":"local","name":"local-qwen35-9b","host":"127.0.0.1","port":8100,"model_path":"~/models/x.gguf","switched_at":"2026-07-30T12:00:00Z"}"#;

    fn parse(s: &str) -> Value {
        serde_json::from_str(s).expect("golden capture is valid JSON")
    }

    /// Every key of `legacy` must exist in `ours`, at the same place, with the same type
    /// and the same value — except where `skip` names the key (uptime, timestamps).
    fn assert_superset(ours: &Value, legacy: &Value, at: &str, skip: &[&str]) {
        match legacy {
            Value::Object(map) => {
                let obj = ours
                    .as_object()
                    .unwrap_or_else(|| panic!("{at}: expected an object, got {ours}"));
                for (k, v) in map {
                    let got = obj
                        .get(k)
                        .unwrap_or_else(|| panic!("{at}.{k} is missing from {ours}"));
                    assert_superset(got, v, &format!("{at}.{k}"), skip);
                }
            }
            Value::Array(items) => {
                let got = ours
                    .as_array()
                    .unwrap_or_else(|| panic!("{at}: expected an array, got {ours}"));
                assert_eq!(got.len(), items.len(), "{at}: array length");
                for (i, v) in items.iter().enumerate() {
                    assert_superset(&got[i], v, &format!("{at}[{i}]"), skip);
                }
            }
            other => {
                let key = at.rsplit('.').next().unwrap_or(at);
                if skip.contains(&key) {
                    assert_eq!(
                        std::mem::discriminant(ours),
                        std::mem::discriminant(other),
                        "{at}: type changed"
                    );
                } else {
                    assert_eq!(ours, other, "{at}: value changed");
                }
            }
        }
    }

    fn keys(v: &Value) -> Vec<String> {
        v.as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default()
    }

    fn backend_fixture(kind: BackendKind, id: &str, base_url: &str) -> Backend {
        Backend {
            id: BackendId::parse(id).expect("id"),
            kind,
            protocol: Protocol::OpenAi,
            label: id.to_owned(),
            base_url: base_url.to_owned(),
            credential: CredentialSource::Env {
                var: "TOGETHER_API_KEY".to_owned(),
            },
            tags: vec![],
            models: vec![],
            limits: BackendLimits::default(),
            price: None,
            health: Health::Unknown,
            provenance: Provenance::Manual,
            endpoint: None,
            enabled: true,
            devices: vec![],
            last_error: None,
        }
    }

    // ---- /health -------------------------------------------------------------------------

    #[test]
    fn health_is_a_superset_of_the_python_capture() {
        let body = health_body(P_VAST, 1234.56);
        assert_superset(&body, &parse(PY_HEALTH), "$", &[]);
        // …plus the house fields.
        assert_eq!(body["product"], json!(PRODUCT));
        assert_eq!(body["version"], json!(VERSION));
        // Legacy keys come first, in the legacy order.
        assert_eq!(keys(&body)[..3], ["ok", "provider", "uptime"]);
    }

    #[test]
    fn health_uptime_is_a_finite_non_negative_float() {
        let up = process_uptime_secs();
        assert!(up.is_finite() && up >= 0.0, "uptime was {up}");
        let body = health_body(P_LOCAL, up);
        assert!(body["uptime"].is_f64(), "uptime must stay a JSON float");
        assert_eq!(body["ok"], json!(true));
    }

    // ---- /providers ----------------------------------------------------------------------

    fn fixture_providers() -> Value {
        providers_body(
            P_VAST,
            LEGACY_FALLBACK_TARGET,
            &ProviderProbe {
                url: LEGACY_FALLBACK_TARGET.to_owned(),
                available: false,
            },
            &ProviderProbe {
                url: TOGETHER_DEFAULT_BASE.to_owned(),
                available: false,
            },
            vec![json!({"name": "local-qwen35-9b", "port": 8100, "running": false})],
            vec![],
            vec![],
        )
    }

    #[test]
    fn providers_matches_the_python_capture_exactly() {
        let body = fixture_providers();
        assert_superset(&body, &parse(PY_PROVIDERS), "$", &[]);
        assert_eq!(
            keys(&body),
            [
                "active",
                "target",
                "providers",
                "local_instances",
                "endpoints",
                "routes"
            ],
            "legacy keys keep their order; the additive ones come last"
        );
    }

    #[test]
    fn providers_rows_have_exactly_the_legacy_keys() {
        let body = fixture_providers();
        for name in [P_VAST, P_TOGETHER] {
            assert_eq!(
                keys(&body["providers"][name]),
                ["available", "url"],
                "the {name} row must have exactly the legacy keys"
            );
        }
        let row = &body["local_instances"][0];
        assert_eq!(keys(row), ["name", "port", "running"]);
        assert!(row["port"].is_u64() && row["running"].is_boolean());
    }

    #[test]
    fn the_probe_cap_is_three_seconds() {
        assert_eq!(PROBE_CAP, Duration::from_secs(3));
    }

    #[tokio::test]
    async fn probes_run_concurrently_not_serially() {
        // Two upstreams, each answering /health after 900 ms. Serial would take ~1.8 s.
        async fn slow_upstream() -> MockServer {
            let s = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/health"))
                .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(900)))
                .mount(&s)
                .await;
            Mock::given(method("GET"))
                .and(path("/v1/models"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
                .mount(&s)
                .await;
            s
        }
        let (a, b) = tokio::join!(slow_upstream(), slow_upstream());
        let key = Secret::new("tgp_test".to_owned());

        let started = Instant::now();
        let (vast_ok, together_ok) = probe_pair(
            &reqwest::Client::new(),
            &a.uri(),
            &b.uri(),
            Some(&key),
            Duration::from_secs(3),
        )
        .await;
        let elapsed = started.elapsed();

        assert!(vast_ok && together_ok, "both upstreams answered 200");
        assert!(
            elapsed < Duration::from_millis(1700),
            "the probes ran serially: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn a_hanging_upstream_is_capped_and_reported_unavailable() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
            .mount(&s)
            .await;

        let started = Instant::now();
        let ok = probe_available(
            &reqwest::Client::new(),
            &s.uri(),
            None,
            Duration::from_millis(300),
        )
        .await;
        let elapsed = started.elapsed();

        assert!(!ok, "a hanging upstream is not available");
        assert!(
            elapsed < Duration::from_secs(2),
            "the cap was ignored: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn together_without_a_credential_is_never_probed() {
        let started = Instant::now();
        let (_, together) = probe_pair(
            &reqwest::Client::new(),
            "http://127.0.0.1:1",
            "http://127.0.0.1:1",
            None,
            Duration::from_secs(3),
        )
        .await;
        assert!(!together);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn local_instances_are_globbed_and_junk_is_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("local-qwen35-9b.json"),
            r#"{"name":"local-qwen35-9b","pid":4242424,"port":8100,"host":"127.0.0.1"}"#,
        )
        .expect("write");
        std::fs::write(dir.path().join("broken.json"), "{not json").expect("write");
        std::fs::write(dir.path().join("noport.json"), r#"{"name":"noport"}"#).expect("write");
        std::fs::write(dir.path().join("notes.txt"), "ignored").expect("write");

        let rows = legacy_instances(dir.path());
        assert_eq!(rows.len(), 1, "only the well-formed row survives: {rows:?}");
        assert_eq!(rows[0]["name"], json!("local-qwen35-9b"));
        assert_eq!(rows[0]["port"], json!(8100));
        assert_eq!(rows[0]["running"], json!(false));
    }

    #[test]
    fn live_local_backends_join_the_legacy_list_without_duplicating_it() {
        let b = backend_fixture(
            BackendKind::LocalLlama,
            "local-qwen35-9b",
            "http://127.0.0.1:8100",
        );
        let other = backend_fixture(
            BackendKind::LocalLlama,
            "carnice-9b",
            "http://127.0.0.1:8101",
        );
        let mut rows = vec![json!({"name": "local-qwen35-9b", "port": 8100, "running": false})];
        merge_instances(&mut rows, live_instances(&[b, other]));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1]["name"], json!("carnice-9b"));
        assert_eq!(rows[1]["port"], json!(8101));
        assert_eq!(keys(&rows[1]), ["name", "port", "running"]);
    }

    #[test]
    fn the_additive_endpoints_list_carries_no_secret() {
        let b = backend_fixture(BackendKind::Managed, "together", "https://api.together.ai");
        let rendered = json_list(&[b]);
        assert_eq!(rendered.len(), 1);
        assert_eq!(rendered[0]["id"], json!("together"));
        assert!(
            !rendered[0].to_string().contains("tgp_"),
            "a credential value reached the wire"
        );
    }

    // ---- /switch -------------------------------------------------------------------------

    fn compat() -> CompatCfg {
        CompatCfg::default()
    }

    #[test]
    fn every_documented_legacy_body_is_accepted() {
        assert_eq!(
            parse_switch(br#"{"provider":"together"}"#, &compat()).expect("together"),
            SwitchTarget::Together {
                base_url: TOGETHER_DEFAULT_BASE.to_owned(),
                model_id: TOGETHER_DEFAULT_MODEL.to_owned(),
                api_key: None,
            },
            "the two Python defaults are preserved"
        );
        assert_eq!(
            parse_switch(br#"{"provider":"vast-gguf"}"#, &compat()).expect("vast"),
            SwitchTarget::Vast
        );
        assert_eq!(
            parse_switch(br#"{"provider":"vast_gguf"}"#, &compat()).expect("vast alias"),
            SwitchTarget::Vast
        );
        assert_eq!(
            parse_switch(
                br#"{"provider":"local","name":"local-qwen35-9b"}"#,
                &compat()
            )
            .expect("local"),
            SwitchTarget::Local {
                name: "local-qwen35-9b".to_owned()
            }
        );

        // The two additive forms.
        assert_eq!(
            parse_switch(br#"{"provider":"endpoint","id":"carnice-9b"}"#, &compat())
                .expect("endpoint"),
            SwitchTarget::Endpoint {
                id: BackendId::parse("carnice-9b").expect("id")
            }
        );
        assert_eq!(
            parse_switch(br#"{"alias":"coder"}"#, &compat()).expect("alias"),
            SwitchTarget::AliasSwitch {
                alias: Alias::parse("coder").expect("alias")
            }
        );
    }

    #[test]
    fn the_together_api_key_is_no_longer_a_silent_no_op() {
        let t = parse_switch(
            br#"{"provider":"together","api_key":"tgp_v1_secret","model_id":"zai/glm"}"#,
            &compat(),
        )
        .expect("together");
        match t {
            SwitchTarget::Together {
                api_key, model_id, ..
            } => {
                assert_eq!(api_key.as_deref(), Some("tgp_v1_secret"));
                assert_eq!(model_id, "zai/glm");
            }
            other => panic!("wrong target: {other:?}"),
        }

        // …and the backend it builds says where the key now lives, never what it is.
        let b = managed_backend(P_TOGETHER, TOGETHER_DEFAULT_BASE, "zai/glm", true).expect("built");
        assert!(matches!(b.credential, CredentialSource::Managed { .. }));
        assert_eq!(b.base_url, "https://api.together.ai", "no /v1 on a Backend");
        assert!(!format!("{b:?}").contains("tgp_v1_secret"));
    }

    #[test]
    fn a_local_instances_key_is_copied_rather_than_dropped() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("keyed.json"),
            r#"{"name":"keyed","host":"127.0.0.1","port":8100,"api_key":"sk-local","model_path":"~/models/x.gguf"}"#,
        )
        .expect("write");
        let meta = load_instance(dir.path(), "keyed").expect("instance");
        assert_eq!(meta.api_key.as_deref(), Some("sk-local"));

        let b = local_backend(&meta).expect("backend");
        assert_eq!(b.base_url, "http://127.0.0.1:8100");
        assert!(
            matches!(b.credential, CredentialSource::Managed { .. }),
            "a keyed instance must describe where its key now lives: {:?}",
            b.credential
        );

        // A keyless instance stays keyless rather than pretending to have one.
        std::fs::write(
            dir.path().join("plain.json"),
            r#"{"port":8101,"host":"127.0.0.1"}"#,
        )
        .expect("write");
        let plain = load_instance(dir.path(), "plain").expect("instance");
        assert_eq!(plain.name.as_deref(), Some("plain"));
        assert!(matches!(
            local_backend(&plain).expect("backend").credential,
            CredentialSource::None
        ));
    }

    #[test]
    fn malformed_json_is_a_json_400_not_an_html_500() {
        for body in [&b"{not json"[..], &b""[..], &b"[]"[..]] {
            let e = parse_switch(body, &compat()).expect_err("must refuse");
            assert_eq!(e, SwitchError::BadJson);
            let (status, msg) = e.parts();
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(json!({ "error": msg }), parse(PY_SWITCH_BAD_JSON));
        }
    }

    #[test]
    fn a_malformed_instance_json_is_a_json_400_not_an_html_500() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("broken.json"), "{\"port\": ").expect("write");
        let e = load_instance(dir.path(), "broken").expect_err("must refuse");
        let (status, msg) = e.parts();
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "the Python returned an HTML 500 here"
        );
        assert!(msg.contains("broken"), "{msg}");
        assert!(json!({ "error": msg })["error"].is_string());

        // A structurally valid document that is missing the one field the row needs.
        std::fs::write(dir.path().join("noport.json"), r#"{"name":"noport"}"#).expect("write");
        assert!(matches!(
            load_instance(dir.path(), "noport"),
            Err(SwitchError::BadInstance(_))
        ));
    }

    #[test]
    fn a_missing_instance_is_the_legacy_404() {
        let dir = tempfile::tempdir().expect("tempdir");
        let e = load_instance(dir.path(), "ghost").expect_err("must refuse");
        let (status, msg) = e.parts();
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(json!({ "error": msg }), parse(PY_SWITCH_NO_INSTANCE));
    }

    #[test]
    fn the_legacy_error_bodies_are_byte_compatible() {
        let (status, msg) = parse_switch(br#"{"provider":"local"}"#, &compat())
            .expect_err("no name")
            .parts();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json!({ "error": msg }), parse(PY_SWITCH_NO_NAME));

        let (status, msg) = parse_switch(br#"{"provider":"nope"}"#, &compat())
            .expect_err("unknown")
            .parts();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json!({ "error": msg }), parse(PY_SWITCH_UNKNOWN));
    }

    #[test]
    fn the_success_body_is_byte_compatible() {
        let ours = json!({ "status": "ok", "provider": P_TOGETHER });
        assert_eq!(ours, parse(PY_SWITCH_OK));
        assert_eq!(keys(&ours), ["status", "provider"]);
    }

    #[test]
    fn base_url_is_validated_against_allow_switch_hosts() {
        let allow = compat().allow_switch_hosts;
        assert!(check_host("https://api.together.ai/v1", &allow).is_ok());
        assert!(check_host("http://127.0.0.1:8100/v1", &allow).is_ok());
        assert!(check_host("http://localhost:8100/v1", &allow).is_ok());

        // The exfiltration primitive: an arbitrary host plus an injected key.
        assert_eq!(
            check_host("https://evil.example.com/v1", &allow),
            Err(SwitchError::HostNotAllowed("evil.example.com".to_owned()))
        );
        let e = parse_switch(
            br#"{"provider":"together","base_url":"https://evil.example.com/v1","api_key":"tgp_v1_secret"}"#,
            &compat(),
        )
        .expect_err("must refuse");
        assert!(matches!(e, SwitchError::HostNotAllowed(_)));
        assert_eq!(
            e.parts().0,
            StatusCode::FORBIDDEN,
            "refused before the key is ever persisted"
        );

        // Not-a-URL and non-http schemes are refused too.
        assert!(matches!(
            check_host("file:///etc/passwd", &allow),
            Err(SwitchError::BadUrl(_))
        ));
        assert!(matches!(
            check_host("not a url", &allow),
            Err(SwitchError::BadUrl(_))
        ));
    }

    #[test]
    fn a_cross_origin_switch_is_refused_and_curl_still_passes() {
        let hv = |s: &str| s.parse().expect("header value");
        let mut browser = HeaderMap::new();
        browser.insert("host", hv("127.0.0.1:8888"));
        browser.insert("origin", hv("http://evil.example.com"));
        assert_eq!(
            require_same_origin(&browser),
            Err(SwitchError::CrossOrigin),
            "a cross-origin fetch must not reach the switch"
        );

        let mut simple = HeaderMap::new();
        simple.insert("host", hv("127.0.0.1:8888"));
        simple.insert("sec-fetch-site", hv("cross-site"));
        assert_eq!(require_same_origin(&simple), Err(SwitchError::CrossOrigin));

        // curl / the CLI / Slint: neither header.
        let mut curl = HeaderMap::new();
        curl.insert("host", hv("127.0.0.1:8888"));
        assert_eq!(require_same_origin(&curl), Ok(()));

        // The embedded UI.
        let mut same = HeaderMap::new();
        same.insert("host", hv("127.0.0.1:8888"));
        same.insert("origin", hv("http://127.0.0.1:8888"));
        same.insert("sec-fetch-site", hv("same-origin"));
        assert_eq!(require_same_origin(&same), Ok(()));
    }

    #[test]
    fn switch_error_responses_are_json_with_the_legacy_status() {
        let r = switch_error(&SwitchError::BadJson);
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        let ct = r
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        assert!(
            ct.starts_with("application/json"),
            "content-type was {ct}, not JSON"
        );
    }

    #[test]
    fn retargeting_the_default_alias_rewrites_exactly_one_route() {
        let alias = Alias::parse("auto").expect("alias");
        let mut routes = RouteFile {
            schema_version: 1,
            default_alias: alias.clone(),
            routes: vec![],
        };
        let id = BackendId::parse("carnice-9b").expect("id");
        assert_eq!(retarget_default(&mut routes, &id), alias);
        assert_eq!(routes.routes.len(), 1);
        assert!(routes.routes[0].is_default);
        assert_eq!(
            routes.routes[0].targets,
            vec![RouteTarget {
                backend: BackendSelector::Id(id),
                model: None,
                weight: 1
            }]
        );

        // A second switch replaces the targets rather than appending to them.
        let other = BackendId::parse("together").expect("id");
        retarget_default(&mut routes, &other);
        assert_eq!(routes.routes.len(), 1);
        assert_eq!(routes.routes[0].targets.len(), 1);

        // The alias form only accepts an alias the table knows.
        let unknown = Alias::parse("ghost").expect("alias");
        assert_eq!(
            set_default_alias(&mut routes, &unknown),
            Err(SwitchError::UnknownAlias("ghost".to_owned()))
        );
        assert_eq!(set_default_alias(&mut routes, &alias), Ok(()));
        assert_eq!(routes.default_alias, alias);
    }

    // ---- .active_endpoint ------------------------------------------------------------------

    fn route_fixture(target: &str) -> ModelRoute {
        ModelRoute {
            alias: Alias::parse("auto").expect("alias"),
            targets: vec![RouteTarget {
                backend: BackendSelector::Id(BackendId::parse(target).expect("id")),
                model: None,
                weight: 1,
            }],
            strategy: Strategy::FirstHealthy,
            filter: Default::default(),
            retry: Default::default(),
            is_default: true,
            description: None,
        }
    }

    #[test]
    fn mirroring_is_off_by_default_and_writes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = CompatCfg::default();
        assert_eq!(cfg.active_endpoint_path, "", "the default must stay off");

        // An empty path returns before it ever touches the registry or the filesystem.
        mirror_active_endpoint(&cfg, &route_fixture("together"), &BackendRegistry::new())
            .expect("off is a no-op");
        assert_eq!(
            std::fs::read_dir(dir.path()).expect("read_dir").count(),
            0,
            "nothing may be written when mirroring is off"
        );
    }

    #[test]
    fn the_together_active_endpoint_shape_matches_the_legacy_capture() {
        let mut b = backend_fixture(BackendKind::Managed, "together", "https://api.together.ai");
        b.tags.push(format!("model:{TOGETHER_DEFAULT_MODEL}"));
        let doc = active_endpoint_doc(&b).expect("together writes a file");
        assert_superset(&doc, &parse(PY_ACTIVE_TOGETHER), "$", &["switched_at"]);
        assert_eq!(
            keys(&doc),
            [
                "provider",
                "model_id",
                "base_url",
                "endpoint",
                "switched_at"
            ]
        );
    }

    #[test]
    fn the_local_active_endpoint_shape_matches_the_legacy_capture() {
        let mut b = backend_fixture(
            BackendKind::LocalLlama,
            "local-qwen35-9b",
            "http://127.0.0.1:8100",
        );
        b.tags.push("model_path:~/models/x.gguf".to_owned());
        let doc = active_endpoint_doc(&b).expect("local writes a file");
        assert_superset(&doc, &parse(PY_ACTIVE_LOCAL), "$", &["switched_at"]);
        assert_eq!(
            keys(&doc),
            [
                "provider",
                "name",
                "host",
                "port",
                "model_path",
                "switched_at"
            ]
        );
    }

    #[test]
    fn a_vast_target_is_represented_by_an_absent_file() {
        let b = backend_fixture(BackendKind::VastLlama, "vast-a1", "http://127.0.0.1:8800");
        assert!(
            active_endpoint_doc(&b).is_none(),
            "the legacy vast branch DELETED .active_endpoint"
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(".active_endpoint");
        std::fs::write(&path, b"{}").expect("write");
        remove_active_endpoint(&path).expect("remove");
        assert!(!path.exists());
        // Removing an absent file is not an error.
        remove_active_endpoint(&path).expect("idempotent");
    }

    #[test]
    fn the_mirror_write_is_atomic_and_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join(".active_endpoint");
        write_active_endpoint(&path, b"{\"provider\":\"local\"}\n").expect("write");

        let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o7777;
        assert_eq!(mode, 0o600, "the mirror must not be world-readable");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "{\"provider\":\"local\"}\n"
        );

        // Overwriting works, and no temp file survives a successful write.
        write_active_endpoint(&path, b"{\"provider\":\"together\"}\n").expect("rewrite");
        let parent = path.parent().expect("parent");
        let names: Vec<String> = std::fs::read_dir(parent)
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, [".active_endpoint"], "a temp file leaked: {names:?}");
    }

    // ---- shared helpers ---------------------------------------------------------------------

    #[test]
    fn the_legacy_target_always_ends_in_v1_and_a_backend_never_does() {
        assert_eq!(with_v1("http://127.0.0.1:8800"), "http://127.0.0.1:8800/v1");
        assert_eq!(
            with_v1("http://127.0.0.1:8800/"),
            "http://127.0.0.1:8800/v1"
        );
        assert_eq!(
            with_v1("https://api.together.ai/v1"),
            "https://api.together.ai/v1"
        );
        assert_eq!(
            strip_v1("https://api.together.ai/v1/"),
            "https://api.together.ai"
        );
        assert_eq!(strip_v1("http://127.0.0.1:8100"), "http://127.0.0.1:8100");
    }

    #[test]
    fn the_legacy_provider_name_never_lies_about_an_unknown_upstream() {
        assert_eq!(
            legacy_provider_name(&backend_fixture(
                BackendKind::VastLlama,
                "vast-a1",
                "http://127.0.0.1:8800"
            )),
            P_VAST
        );
        assert_eq!(
            legacy_provider_name(&backend_fixture(
                BackendKind::LocalLlama,
                "carnice-9b",
                "http://127.0.0.1:8100"
            )),
            P_LOCAL
        );
        assert_eq!(
            legacy_provider_name(&backend_fixture(
                BackendKind::Managed,
                "together",
                "https://api.together.ai"
            )),
            P_TOGETHER
        );
        // An OpenRouter node is not called "together" just because the field is legacy.
        assert_eq!(
            legacy_provider_name(&backend_fixture(
                BackendKind::Node,
                "openrouter",
                "https://openrouter.ai/api"
            )),
            "openrouter"
        );
    }

    #[test]
    fn glob_and_tilde_helpers_behave() {
        assert!(glob_match("vast-*", "vast-a1"));
        assert!(glob_match("*-9b", "carnice-9b"));
        assert!(!glob_match("vast-*", "local-a1"));
        assert!(glob_match("carnice-9b", "carnice-9b"));

        assert_eq!(expand_tilde("/etc/x"), PathBuf::from("/etc/x"));
        assert_eq!(expand_tilde("~other/x"), PathBuf::from("~other/x"));
    }

    #[test]
    fn switched_at_is_real_utc_with_a_trailing_z() {
        let ts = now_utc_z();
        assert!(ts.ends_with('Z'), "{ts}");
        assert_eq!(
            ts.len(),
            20,
            "seconds precision, as the old TUI parses: {ts}"
        );
        chrono::DateTime::parse_from_rfc3339(&ts).expect("valid RFC 3339");
    }
}
