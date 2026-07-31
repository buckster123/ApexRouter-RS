//! OWNER: unit S-07 (server/src/api/{vast,hf,providers,checks,compare}.rs). Do not edit outside that unit.
//!
//! The `/v1/providers*` set. `PUT` with an `api_key` writes `credentials.toml` at 0600 and **never** `config.toml`.
//!
//! # Why the two writes are split
//!
//! A managed provider carries two very different pieces of state. `base_url`,
//! `api_key_env` and `api_key_file` are *configuration*: they describe where to look, they
//! are worth a comment in a hand-edited file, and they belong in `config.toml`. The key
//! itself is a *secret*: it belongs in `$STATE/credentials.toml`, written at `0600` by
//! [`apexrouter_core::secret::store_user_credential`], and it must never be written into a
//! file that is routinely pasted into a bug report. So [`put`] writes at most one of the
//! two files per field, and a body carrying only `api_key` does not touch `config.toml`
//! at all — asserted by a test, because "we were careful" is not a mechanism.
//!
//! # What this module does not do
//!
//! `GET /v1/providers` **never probes**. It resolves the credential chain (which is file
//! and environment reads only) and reports what it found; the last success and the last
//! failure come from [`record_provider_result`], which the calls that really do reach a
//! provider — [`test`], [`models`], and P-06's client — report into. A list endpoint that
//! opened four TLS connections would be a list endpoint nobody dares call from a render
//! loop.

use super::{now_unix, ApiError, ApiResult};
use crate::state::AppState;
use apexrouter_core::config::ProviderCfg;
use apexrouter_core::paths::Paths;
use apexrouter_core::secret::{resolve_provider, store_user_credential, Secret};
use apexrouter_core::upstream;
use apexrouter_protocol::{
    BackendKind, CheckId, CheckResult, CheckStatus, CredentialSource, ProviderId, ProviderStatus,
    RateLimitInfo, UpstreamModel,
};
use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

/// How long the connection half of `POST /v1/providers/{id}/test` gets.
const TEST_TIMEOUT: Duration = Duration::from_secs(8);
/// How long the optional completion half gets. A cold managed model can take a while.
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(60);
/// How many tokens the optional completion asks for. Enough to prove the pipe, cheap enough
/// that a `test` button is not a billing event worth thinking about.
const COMPLETION_TOKENS: u32 = 16;

/// The `/v1/providers*` routes.
pub fn router() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route("/v1/providers", get(list))
        .route("/v1/providers/{id}", get(one).put(put))
        .route("/v1/providers/{id}/test", post(test))
        .route("/v1/providers/{id}/models", get(models))
}

/// The editable half of a provider. Everything absent is left exactly as it was.
///
/// The four fields are deliberately not mutually exclusive on the wire — a GUI that sets a
/// key and a base URL in one save should not have to make two calls — but they land in two
/// different files, and only the files whose fields were present are written.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ProviderPatch {
    /// The API root, stored **without** a trailing `/v1`. Never rewritten: a legacy
    /// `api.together.xyz` stays `api.together.xyz`.
    #[serde(default)]
    pub base_url: Option<String>,
    /// A key the **user typed**. Goes to `$STATE/credentials.toml` at `0600`.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Name an environment variable to read the key from instead.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Name a file to read the key from instead.
    #[serde(default)]
    pub api_key_file: Option<String>,
}

/// `?completion=1` on `POST /v1/providers/{id}/test`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct TestQuery {
    /// Also send a 16-token completion, not just a connection probe.
    #[serde(default)]
    pub completion: Option<u8>,
    /// Which model the completion should name. Defaults to the first model the connection
    /// probe listed — **never** the hardcoded `"x"` that 400s on a managed provider.
    #[serde(default)]
    pub model: Option<String>,
}

/// `GET /v1/providers` — every configured provider, its credential **source** and what we
/// last saw. Never the key, and never a probe.
pub async fn list(State(s): State<Arc<AppState>>) -> ApiResult<Vec<ProviderStatus>> {
    Ok(Json(all_providers(&s)))
}

/// `GET /v1/providers/{id}` — one provider's status.
pub async fn one(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<ProviderStatus> {
    let id = parse_provider(&id)?;
    known(&s, &id)?;
    Ok(Json(status_of(&s, &id)))
}

/// `PUT /v1/providers/{id}` — set the base URL, the key, or where the key lives.
///
/// The key path and the config path are separate on purpose (see the module docs). A body
/// with only `api_key` writes `$STATE/credentials.toml` and leaves `config.toml` untouched;
/// a body with only `base_url` writes `config.toml` and never sees a secret.
///
/// A provider id that is not in `config.toml` yet is **created**, because "add together"
/// from a GUI must not require hand-editing a TOML file first.
pub async fn put(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(patch): Json<ProviderPatch>,
) -> ApiResult<ProviderStatus> {
    let id = parse_provider(&id)?;

    // ---- the secret, into `$STATE/credentials.toml` at 0600 ------------------------------
    if let Some(key) = patch.api_key.as_deref() {
        let key = key.trim().to_owned();
        if key.is_empty() {
            return Err(
                ApiError::bad_request("invalid", "api_key must not be empty").with_param("api_key"),
            );
        }
        let paths: Paths = s.paths.clone();
        let for_id = id.clone();
        tokio::task::spawn_blocking(move || {
            store_user_credential(&paths, &for_id, Secret::new(key))
        })
        .await
        .map_err(|e| ApiError::internal(format!("the credential write panicked: {e}")))?
        .map_err(ApiError::from)?;
    }

    // ---- the configuration, into `config.toml` -------------------------------------------
    let touches_config =
        patch.base_url.is_some() || patch.api_key_env.is_some() || patch.api_key_file.is_some();
    if touches_config {
        let mut cfg = (*s.cfg()).clone();
        let entry = cfg
            .providers
            .entry(id.as_str().to_owned())
            .or_insert_with(|| ProviderCfg {
                base_url: String::new(),
                api_key_env: None,
                api_key_file: None,
            });
        if let Some(url) = patch.base_url.as_deref() {
            let url = url.trim();
            if url.is_empty() {
                return Err(
                    ApiError::bad_request("invalid", "base_url must not be empty")
                        .with_param("base_url"),
                );
            }
            // Stored verbatim apart from the `/v1` invariant: `api.together.xyz` is NEVER
            // rewritten to `.ai`, which is the documented bug this port refuses to inherit.
            entry.base_url = strip_v1(url);
        }
        if let Some(var) = patch.api_key_env.as_deref() {
            entry.api_key_env = non_empty(var);
        }
        if let Some(file) = patch.api_key_file.as_deref() {
            entry.api_key_file = non_empty(file);
        }

        let cfg = Arc::new(cfg);
        let paths: Paths = s.paths.clone();
        let to_save = Arc::clone(&cfg);
        tokio::task::spawn_blocking(move || to_save.save(&paths))
            .await
            .map_err(|e| ApiError::internal(format!("the config write panicked: {e}")))?
            .map_err(ApiError::from)?;
        s.cfg.store(cfg);
    }

    Ok(Json(status_of(&s, &id)))
}

/// `POST /v1/providers/{id}/test` — connection, and optionally a real completion.
///
/// Two rows rather than one boolean: "the URL answers but the key is wrong" and "the key is
/// fine but the model name is wrong" are different problems with different fixes, and a
/// single green tick cannot say which one you have.
pub async fn test(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<TestQuery>,
) -> ApiResult<Vec<CheckResult>> {
    let id = parse_provider(&id)?;
    let cfg = known(&s, &id)?;
    let base_url = strip_v1(&cfg.base_url);
    let cred = credential(&s, &id);

    let started = Instant::now();
    let probe = upstream::probe(super::http(), &base_url, cred.as_ref(), TEST_TIMEOUT).await;
    let reachable = probe.healthy || probe.loading || !probe.models.is_empty();
    if reachable {
        record_provider_result(&id, Ok(None));
    } else {
        record_provider_result(
            &id,
            Err(probe
                .error
                .clone()
                .unwrap_or_else(|| "the provider did not answer".to_owned())),
        );
    }

    let mut out = vec![CheckResult {
        id: CheckId::from(format!("providers.{id}.connection")),
        label: format!("{id} answers at {base_url}"),
        status: if reachable {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        ms: millis(started.elapsed()),
        detail: if reachable {
            format!("{} models advertised", probe.models.len())
        } else {
            probe
                .error
                .clone()
                .unwrap_or_else(|| "no answer, and no error either".to_owned())
        },
        fix: (!reachable).then(|| {
            format!(
                "check `[providers.{id}] base_url` and the credential: \
                 `apexrouter provider test {id}`"
            )
        }),
    }];

    if q.completion.unwrap_or(0) == 0 {
        return Ok(Json(out));
    }

    // The completion probe names **a model the provider actually listed**. `smoke.sh`'s
    // hardcoded `"model":"x"` 400s on every managed provider, and reproducing that here
    // would make the test button lie about a provider that works.
    let model = q
        .model
        .clone()
        .or_else(|| probe.models.first().map(|m| m.id.clone()));
    let Some(model) = model else {
        out.push(CheckResult {
            id: CheckId::from(format!("providers.{id}.completion")),
            label: format!("{id} answers a completion"),
            status: CheckStatus::Skipped,
            ms: 0,
            detail: "no model to ask for: the catalogue came back empty".to_owned(),
            fix: Some("pass ?model=<id>, or fix the connection first".to_owned()),
        });
        return Ok(Json(out));
    };

    let started = Instant::now();
    let url = upstream::join_v1(&base_url, "/v1/chat/completions");
    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "ping"}],
        "max_tokens": COMPLETION_TOKENS,
        "stream": false,
    });
    let mut rb = super::http()
        .post(&url)
        .timeout(COMPLETION_TIMEOUT)
        .json(&body);
    if let Some(c) = cred.as_ref() {
        rb = rb.bearer_auth(c.expose());
    }
    let sent = rb.send().await;
    let ms = millis(started.elapsed());

    let row = match sent {
        Err(e) => {
            record_provider_result(&id, Err(e.to_string()));
            CheckResult {
                id: CheckId::from(format!("providers.{id}.completion")),
                label: format!("{id} answers a completion"),
                status: CheckStatus::Fail,
                ms,
                detail: format!("POST /v1/chat/completions ({model}): {e}"),
                fix: Some("check the credential and the model id".to_owned()),
            }
        }
        Ok(res) => {
            let status = res.status();
            let rate = rate_limit_of(res.headers());
            let json: serde_json::Value = res.json().await.unwrap_or(serde_json::Value::Null);
            let tokens = upstream::parse_usage(&json).map(|u| u.completion_tokens);
            if status.is_success() {
                record_provider_result(&id, Ok(rate));
            } else {
                record_provider_result(&id, Err(format!("HTTP {status} on a completion")));
            }
            CheckResult {
                id: CheckId::from(format!("providers.{id}.completion")),
                label: format!("{id} answers a completion"),
                status: if status.is_success() {
                    CheckStatus::Pass
                } else {
                    CheckStatus::Fail
                },
                ms,
                detail: if status.is_success() {
                    match tokens {
                        Some(n) => format!("{model}: {n} tokens"),
                        None => format!("{model}: answered, no usage reported"),
                    }
                } else {
                    format!("{model}: HTTP {status}")
                },
                fix: (!status.is_success())
                    .then(|| "check the credential and the model id".to_owned()),
            }
        }
    };
    out.push(row);
    Ok(Json(out))
}

/// `GET /v1/providers/{id}/models` — the live catalogue, grouped by org.
///
/// "Grouped by org" is a **sort**, not a nesting: the rows stay a flat `Vec<UpstreamModel>`
/// so the wire type is the same one `Backend.models` uses, ordered by the namespace before
/// the `/` and then by id, which is what makes the UI's org headers a one-pass render.
pub async fn models(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Vec<UpstreamModel>> {
    let id = parse_provider(&id)?;
    let cfg = known(&s, &id)?;
    let base_url = strip_v1(&cfg.base_url);
    let cred = credential(&s, &id);

    let probe = upstream::probe(super::http(), &base_url, cred.as_ref(), TEST_TIMEOUT).await;
    if probe.models.is_empty() {
        if let Some(e) = probe.error.clone() {
            record_provider_result(&id, Err(e.clone()));
            return Err(ApiError::new(
                axum::http::StatusCode::BAD_GATEWAY,
                "upstream_unreachable",
                format!("{id} at {base_url}: {e}"),
            ));
        }
    }
    record_provider_result(&id, Ok(None));

    let mut rows = probe.models;
    rows.sort_by(|a, b| org_of(&a.id).cmp(org_of(&b.id)).then(a.id.cmp(&b.id)));
    rows.dedup_by(|a, b| a.id == b.id);
    Ok(Json(rows))
}

// ----------------------------------------------------------------------------------------
// shared with the rest of S-07
// ----------------------------------------------------------------------------------------

/// Every configured provider's status, in config order.
///
/// Published because `GET /v1/snapshot` and `apexrouter status` both want exactly this list
/// and neither should re-derive the credential chain.
pub fn all_providers(s: &Arc<AppState>) -> Vec<ProviderStatus> {
    let cfg = s.cfg();
    cfg.providers
        .keys()
        .filter_map(|k| ProviderId::parse(k).ok())
        .map(|id| status_of(s, &id))
        .collect()
}

/// One provider's status: where its key lives, whether that produced anything, and what the
/// last call that really reached it saw.
pub fn status_of(s: &Arc<AppState>, id: &ProviderId) -> ProviderStatus {
    let cfg = s.cfg();
    let base_url = cfg
        .provider(id)
        .map(|p| strip_v1(&p.base_url))
        .unwrap_or_default();

    // Resolving is file and environment reads only — no provider is contacted here.
    let resolved = resolve_provider(&cfg, &s.paths, id).ok().flatten();
    let (credential, credential_present) = match resolved {
        Some(r) => (r.source, true),
        None => (CredentialSource::None, false),
    };

    let models_cached = s
        .router
        .registry()
        .snapshot()
        .iter()
        .filter(|b| b.kind == BackendKind::Managed && b.id.as_str() == id.as_str())
        .map(|b| u32::try_from(b.models.len()).unwrap_or(u32::MAX))
        .max()
        .unwrap_or(0);

    let seen = observations()
        .read()
        .ok()
        .and_then(|m| m.get(id.as_str()).cloned())
        .unwrap_or_default();

    ProviderStatus {
        id: id.clone(),
        base_url,
        credential,
        credential_present,
        models_cached,
        last_ok_unix: seen.last_ok_unix,
        last_error: seen.last_error,
        rate_limit: seen.rate_limit,
    }
}

/// The resolved key for one provider, or `None` when the chain produced nothing.
///
/// Published so `checks.rs` and `compare.rs` reach a managed provider with the same
/// credential the router would use, rather than inventing a second resolution order.
pub fn credential(s: &Arc<AppState>, id: &ProviderId) -> Option<Secret<String>> {
    let cfg = s.cfg();
    resolve_provider(&cfg, &s.paths, id)
        .ok()
        .flatten()
        .map(|r| r.secret)
}

/// Record the outcome of a call that really reached a provider.
///
/// `Ok(rate)` stamps `last_ok_unix` and clears `last_error`; `Err(msg)` records the message
/// and leaves the last success where it was, because "worked at 14:02, broken since 14:09"
/// is the sentence an operator needs and either half alone is not.
pub fn record_provider_result(id: &ProviderId, outcome: Result<Option<RateLimitInfo>, String>) {
    let Ok(mut map) = observations().write() else {
        return;
    };
    let entry = map.entry(id.as_str().to_owned()).or_default();
    match outcome {
        Ok(rate) => {
            entry.last_ok_unix = Some(now_unix());
            entry.last_error = None;
            if rate.is_some() {
                entry.rate_limit = rate;
            }
        }
        Err(msg) => entry.last_error = Some(msg),
    }
}

/// Read `x-ratelimit-*` off a response. `remaining` is recorded but **never** relied upon
/// for a decision: providers disagree about whether it counts requests or tokens.
pub fn rate_limit_of(headers: &reqwest::header::HeaderMap) -> Option<RateLimitInfo> {
    let num = |name: &str| -> Option<u64> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
    };
    let limit = num("x-ratelimit-limit");
    let remaining = num("x-ratelimit-remaining");
    // Together sends `x-ratelimit-reset` as seconds-from-now on some routes and as an
    // absolute unix stamp on others. Anything smaller than a plausible epoch is treated as
    // a delta, which is the only reading that cannot produce a reset in 1970.
    let reset_unix = num("x-ratelimit-reset").map(|v| {
        if v < 1_000_000_000 {
            now_unix().saturating_add(i64::try_from(v).unwrap_or(i64::MAX))
        } else {
            i64::try_from(v).unwrap_or(i64::MAX)
        }
    });
    (limit.is_some() || remaining.is_some() || reset_unix.is_some()).then_some(RateLimitInfo {
        limit,
        remaining,
        reset_unix,
    })
}

// ----------------------------------------------------------------------------------------
// internals
// ----------------------------------------------------------------------------------------

/// What the last call that really reached a provider saw.
#[derive(Clone, Debug, Default)]
struct Observation {
    last_ok_unix: Option<i64>,
    last_error: Option<String>,
    rate_limit: Option<RateLimitInfo>,
}

/// Provider id → last observation. Process-global for the same reason
/// [`super::shutdown_notify`] is: `AppState` is S-01's file and publishes no slot for it,
/// and there is one daemon per process, so a global is exactly as scoped as the thing it
/// names.
fn observations() -> &'static RwLock<BTreeMap<String, Observation>> {
    static OBS: OnceLock<RwLock<BTreeMap<String, Observation>>> = OnceLock::new();
    OBS.get_or_init(|| RwLock::new(BTreeMap::new()))
}

/// Parse a path segment into a `ProviderId`.
fn parse_provider(raw: &str) -> Result<ProviderId, ApiError> {
    ProviderId::parse(raw)
        .map_err(|e| ApiError::bad_request("bad_id", e.to_string()).with_param("id"))
}

/// The provider's config, or a `404` naming what is configured.
fn known(s: &Arc<AppState>, id: &ProviderId) -> Result<ProviderCfg, ApiError> {
    let cfg = s.cfg();
    cfg.provider(id).cloned().ok_or_else(|| {
        let known: Vec<&str> = cfg.providers.keys().map(String::as_str).collect();
        ApiError::not_found(format!(
            "no provider `{id}`; configured: {}",
            if known.is_empty() {
                "none".to_owned()
            } else {
                known.join(", ")
            }
        ))
        .with_param("id")
    })
}

/// Strip a trailing `/v1` (and any trailing slash) — the invariant every stored base URL
/// carries. `join_v1` puts exactly one back on.
fn strip_v1(url: &str) -> String {
    let mut s = url.trim().trim_end_matches('/');
    while let Some(stripped) = s.strip_suffix("/v1") {
        s = stripped.trim_end_matches('/');
    }
    s.to_owned()
}

/// `Some(trimmed)` for a non-blank string, `None` for a blank one — which is how a GUI
/// clears a field.
fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

/// The namespace before the first `/`, or `""` for a bare model id.
fn org_of(id: &str) -> &str {
    id.split_once('/').map(|(org, _)| org).unwrap_or("")
}

/// Saturating millisecond count.
fn millis(d: Duration) -> u32 {
    u32::try_from(d.as_millis()).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::testkit::{app, test_config};

    /// The acceptance sentence: a key goes to `credentials.toml` at `0600`, and
    /// `config.toml` is not created, let alone written into.
    #[tokio::test]
    async fn a_key_lands_in_credentials_toml_at_0600_and_never_in_config_toml() {
        use std::os::unix::fs::PermissionsExt;

        let state = app(test_config());
        let config_file = state.paths.config_file();
        let credentials = state.paths.credentials_file();
        assert!(!config_file.exists(), "no config.toml before the PUT");

        let patch = ProviderPatch {
            api_key: Some("sk-user-typed-this".to_owned()),
            ..ProviderPatch::default()
        };
        let Json(status) = put(
            State(Arc::clone(&state)),
            Path("together".to_owned()),
            Json(patch),
        )
        .await
        .expect("put");

        assert!(credentials.exists(), "the key was written");
        let mode = std::fs::metadata(&credentials)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o600, "credentials.toml must be 0600");
        assert!(
            !config_file.exists(),
            "a body with only api_key must not write config.toml"
        );

        let text = std::fs::read_to_string(&credentials).expect("read");
        assert!(text.contains("sk-user-typed-this"), "the key is stored");

        // and the status reports the SOURCE, never the value
        assert!(status.credential_present);
        assert!(matches!(
            status.credential,
            CredentialSource::Managed { .. }
        ));
        let json = serde_json::to_string(&status).expect("ser");
        assert!(
            !json.contains("sk-user-typed-this"),
            "the key must never reach the wire: {json}"
        );
    }

    #[tokio::test]
    async fn a_base_url_lands_in_config_toml_and_is_never_rewritten() {
        let state = app(test_config());
        let patch = ProviderPatch {
            // The documented trap: this host must NOT become `api.together.ai`.
            base_url: Some("https://api.together.xyz/v1".to_owned()),
            ..ProviderPatch::default()
        };
        let Json(status) = put(
            State(Arc::clone(&state)),
            Path("together".to_owned()),
            Json(patch),
        )
        .await
        .expect("put");

        assert_eq!(
            status.base_url, "https://api.together.xyz",
            "the /v1 invariant is enforced and the host is untouched"
        );
        let text = std::fs::read_to_string(state.paths.config_file()).expect("config.toml");
        assert!(
            text.contains("base_url = \"https://api.together.xyz\""),
            "the host is written verbatim: {text}"
        );
        assert!(
            !text.contains("base_url = \"https://api.together.ai\""),
            "never rewritten to .ai: {text}"
        );
        // the live config was swapped, not just the file
        assert_eq!(
            state
                .cfg()
                .provider(&ProviderId::parse("together").expect("id"))
                .map(|p| p.base_url.clone()),
            Some("https://api.together.xyz".to_owned())
        );
    }

    #[tokio::test]
    async fn an_empty_key_is_a_400_rather_than_a_stored_blank() {
        let state = app(test_config());
        let e = put(
            State(Arc::clone(&state)),
            Path("together".to_owned()),
            Json(ProviderPatch {
                api_key: Some("   ".to_owned()),
                ..ProviderPatch::default()
            }),
        )
        .await
        .expect_err("blank keys are refused");
        assert_eq!(e.status, axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(e.body.param.as_deref(), Some("api_key"));
        assert!(
            !state.paths.credentials_file().exists(),
            "nothing was written"
        );
    }

    #[tokio::test]
    async fn listing_reports_the_source_and_never_probes() {
        let mut cfg = test_config();
        cfg.providers.insert(
            "fixture".to_owned(),
            ProviderCfg {
                // A closed loopback port: if `list` probed, this test would take the
                // connect timeout instead of microseconds.
                base_url: "http://127.0.0.1:1".to_owned(),
                api_key_env: Some("APEXROUTER_TEST_NO_SUCH_VAR".to_owned()),
                api_key_file: None,
            },
        );
        let state = app(cfg);

        let started = Instant::now();
        let Json(all) = list(State(Arc::clone(&state))).await.expect("list");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "GET /v1/providers must not probe"
        );

        let fixture = all
            .iter()
            .find(|p| p.id.as_str() == "fixture")
            .expect("fixture is listed");
        assert!(!fixture.credential_present, "the env var does not exist");
        assert_eq!(fixture.credential, CredentialSource::None);
        assert_eq!(fixture.models_cached, 0);
        assert!(fixture.last_ok_unix.is_none());
    }

    #[tokio::test]
    async fn an_unknown_provider_is_a_404_that_names_what_is_configured() {
        let state = app(test_config());
        let e = one(State(Arc::clone(&state)), Path("nope".to_owned()))
            .await
            .expect_err("no such provider");
        assert_eq!(e.status, axum::http::StatusCode::NOT_FOUND);
        assert!(e.body.message.contains("together"), "{}", e.body.message);
    }

    /// HERMETIC: `together`'s base URL in `test_config()` is a closed loopback port, so this
    /// exercises the whole `test` path — including credential resolution — without reaching
    /// anything that could bill.
    #[tokio::test]
    async fn a_connection_test_against_a_closed_port_fails_with_a_fix_line() {
        let state = app(test_config());
        let Json(rows) = test(
            State(Arc::clone(&state)),
            Path("together".to_owned()),
            Query(TestQuery::default()),
        )
        .await
        .expect("test");

        assert_eq!(rows.len(), 1, "no completion was asked for");
        assert_eq!(rows[0].id.as_str(), "providers.together.connection");
        assert_eq!(rows[0].status, CheckStatus::Fail);
        assert!(rows[0].fix.is_some(), "a failure names the fix");

        // and the failure is remembered for the next `GET /v1/providers`
        let Json(status) = one(State(Arc::clone(&state)), Path("together".to_owned()))
            .await
            .expect("one");
        assert!(status.last_error.is_some());
    }

    #[test]
    fn rate_limit_headers_are_read_and_a_delta_reset_is_absolutised() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("x-ratelimit-limit", "600".parse().expect("header"));
        h.insert("x-ratelimit-remaining", "599".parse().expect("header"));
        h.insert("x-ratelimit-reset", "60".parse().expect("header"));
        let r = rate_limit_of(&h).expect("some");
        assert_eq!(r.limit, Some(600));
        assert_eq!(r.remaining, Some(599));
        assert!(
            r.reset_unix.unwrap_or(0) > 1_700_000_000,
            "a delta became an absolute stamp: {r:?}"
        );

        assert!(
            rate_limit_of(&reqwest::header::HeaderMap::new()).is_none(),
            "no headers is None, not a row of zeroes"
        );
    }

    #[test]
    fn base_urls_are_stripped_of_v1_and_orgs_are_read_off_model_ids() {
        assert_eq!(
            strip_v1("https://api.together.xyz/v1/"),
            "https://api.together.xyz"
        );
        assert_eq!(
            strip_v1("http://127.0.0.1:8100/v1/v1"),
            "http://127.0.0.1:8100"
        );
        assert_eq!(strip_v1("http://127.0.0.1:8100"), "http://127.0.0.1:8100");
        assert_eq!(org_of("meta-llama/Llama-3.3-70B"), "meta-llama");
        assert_eq!(org_of("gpt-4o"), "");
    }
}
