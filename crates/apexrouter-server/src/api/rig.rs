//! OWNER: unit S-04 (server/src/api/{rig,fit,catalog,usage,requests,jobs}.rs,
//! server/src/jobs.rs). Do not edit outside that unit.
//!
//! `GET /v1/rig`, `POST /v1/rig/rescan`, `GET /v1/models/local`.
//!
//! Two facts shape this module.
//!
//! **The rig is plural** (`ARCHITECTURE.md` §3.2 and `00b`): a list of GPUs across a list of
//! builds. Nothing here assumes one device, one backend or one binary, and software
//! rasterisers are *marked* (`is_software`) rather than hidden — the fit solver excludes
//! them, the operator still gets to see them.
//!
//! **A scan is expensive and a reservation is not.** `scan_rig` execs every discovered
//! `llama-server` with `--list-devices`, so the enumeration is cached by the supervisor for
//! 60 s and `POST /v1/rig/rescan` is what forces it. What is *never* cached is the
//! reservation arithmetic: [`annotate_holders`] recomputes `held_by`/`reserved_mb` from the
//! live endpoint records on every read, because that is the number deciding whether the next
//! launch fits.
//!
//! Free VRAM is reported exactly as `--list-devices` gave it. On this machine ROCm reports
//! free (12877 MiB) greater than total (11397 MiB) because of GTT accounting; that is a true
//! reading of a real driver and it is not laundered here. Anything computing `total - free`
//! must saturate — see the test that pins it.

use crate::api::{ApiError, ApiResult};
use crate::state::AppState;
use apexrouter_core::discover::discover_models;
use apexrouter_core::error::Result as CoreResult;
use apexrouter_protocol::{BackendId, EndpointRecord, EndpointSpec, LocalModel, RigSnapshot};
use apexrouter_providers::local::supervisor::scan_rig;
use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::Json;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long a model walk stays warm. The rig cache is the supervisor's (60 s); this one is
/// ours, and it exists because `GET /v1/fit` resolves a model name on every call.
const MODEL_TTL: Duration = Duration::from_secs(60);

/// `POST /v1/rig/rescan?builds=&models=`.
#[derive(Debug, Default, Deserialize)]
pub struct RescanQuery {
    /// Re-enumerate builds and devices. Defaults to true.
    #[serde(default)]
    pub builds: Option<bool>,
    /// Re-walk the model roots. Defaults to true.
    #[serde(default)]
    pub models: Option<bool>,
}

/// `GET /v1/models/local?refresh=`.
#[derive(Debug, Default, Deserialize)]
pub struct ModelsQuery {
    /// Skip the cache and walk the roots again.
    #[serde(default)]
    pub refresh: Option<bool>,
}

/// The `/v1/rig*` and `/v1/models/local` routes.
pub fn router() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route("/v1/rig", get(get_rig))
        .route("/v1/rig/rescan", post(rescan))
        .route("/v1/models/local", get(local_models))
}

/// `GET /v1/rig` — the cached enumeration with live reservations folded in.
pub async fn get_rig(State(s): State<Arc<AppState>>) -> ApiResult<RigSnapshot> {
    Ok(Json(rig_snapshot(&s, false).await?))
}

/// `POST /v1/rig/rescan` — force the scan the cache would otherwise have served.
///
/// `?builds=false` keeps the enumeration and only re-walks the weights; `?models=false` does
/// the opposite. Both default to true, which is what a bare `POST` means.
pub async fn rescan(
    State(s): State<Arc<AppState>>,
    Query(q): Query<RescanQuery>,
) -> ApiResult<RigSnapshot> {
    let want_builds = q.builds.unwrap_or(true);
    let want_models = q.models.unwrap_or(true);
    if want_models {
        invalidate_models();
        // Warm it again, so the next `/v1/fit` does not pay for the walk. A failure here is
        // not fatal to a rescan of the builds.
        if let Err(e) = local_model_list(&s, true).await {
            tracing::warn!(error = %e, "model rescan failed");
        }
    }
    Ok(Json(rig_snapshot(&s, want_builds).await?))
}

/// `GET /v1/models/local` — every discovered GGUF, shards grouped into one logical model.
pub async fn local_models(
    State(s): State<Arc<AppState>>,
    Query(q): Query<ModelsQuery>,
) -> ApiResult<Vec<LocalModel>> {
    Ok(Json(
        local_model_list(&s, q.refresh.unwrap_or(false)).await?,
    ))
}

/// The rig, with `held_by`/`reserved_mb` recomputed from the endpoint records.
///
/// `force` re-execs `--list-devices` on every discovered build; otherwise the supervisor's
/// 60 s cache answers. Shared with [`crate::api::fit`], which needs the same picture the
/// launcher will see.
///
/// # Errors
/// Propagates a discovery failure.
pub async fn rig_snapshot(state: &Arc<AppState>, force: bool) -> CoreResult<RigSnapshot> {
    let mut rig = if force {
        let cfg = state.cfg.load_full();
        let fresh = scan_rig(&cfg.endpoints, state.paths.cache()).await?;
        // Hand it to the supervisor so the control plane and the launcher agree on what
        // hardware exists instead of probing every binary twice.
        state.supervisor.set_rig(fresh.clone());
        fresh
    } else {
        state.supervisor.rig().await?
    };
    let running = state.store.list_endpoints().unwrap_or_default();
    annotate_holders(&mut rig, &running);
    Ok(rig)
}

/// Every discovered local model, cached for [`MODEL_TTL`] unless `force`.
///
/// The cache is keyed by the configured roots, so two `AppState`s with different
/// `model_roots` — which is what every test is — never see each other's answer.
///
/// # Errors
/// Propagates a discovery failure.
pub async fn local_model_list(state: &Arc<AppState>, force: bool) -> CoreResult<Vec<LocalModel>> {
    let cfg = state.cfg.load_full();
    let key = cache_key(&cfg.endpoints.model_roots);
    if !force {
        if let Some(hit) = cached_models(&key) {
            return Ok(hit);
        }
    }
    let found = discover_models(&cfg.endpoints).await?;
    store_models(key, &found);
    Ok(found)
}

/// Recompute `held_by` and `reserved_mb` on every GPU from the running endpoints.
///
/// An endpoint is attributed by its `fit.per_device_mb` when it has one, else spread evenly
/// over `fit.split.devices`, else charged whole to the first non-software device — the same
/// conservative reading `budget_from_rig` uses, so the two never disagree about who holds
/// what.
///
/// Every arithmetic step saturates. A driver reporting free > total (ROCm's GTT accounting
/// does, on this very machine) must not produce an underflowed `u64` the size of the
/// universe in somebody's "used" bar.
pub fn annotate_holders(rig: &mut RigSnapshot, running: &[EndpointRecord]) {
    let mut held: BTreeMap<String, Vec<BackendId>> = BTreeMap::new();
    let mut reserved: BTreeMap<String, u64> = BTreeMap::new();

    let fallback = rig
        .gpus
        .iter()
        .find(|g| !g.is_software)
        .map(|g| g.device.clone());

    for rec in running.iter().filter(|r| is_local(&r.spec)) {
        let Some(fit) = rec.fit.as_ref() else {
            continue;
        };
        let total = fit
            .weights_mb
            .saturating_add(fit.kv_mb)
            .saturating_add(fit.compute_mb);

        if !fit.per_device_mb.is_empty() {
            for (dev, mb) in &fit.per_device_mb {
                let slot = reserved.entry(dev.clone()).or_default();
                *slot = slot.saturating_add(*mb);
                held.entry(dev.clone()).or_default().push(rec.id.clone());
            }
        } else if !fit.split.devices.is_empty() {
            let n = fit.split.devices.len() as u64;
            let each = total / n.max(1);
            for dev in &fit.split.devices {
                let slot = reserved.entry(dev.clone()).or_default();
                *slot = slot.saturating_add(each);
                held.entry(dev.clone()).or_default().push(rec.id.clone());
            }
        } else if let Some(dev) = fallback.clone() {
            let slot = reserved.entry(dev.clone()).or_default();
            *slot = slot.saturating_add(total);
            held.entry(dev).or_default().push(rec.id.clone());
        }
    }

    for gpu in &mut rig.gpus {
        let mut holders = held.remove(&gpu.device).unwrap_or_default();
        holders.sort();
        holders.dedup();
        gpu.held_by = holders;
        // A reservation can exceed free VRAM when a driver lies about free; clamping would
        // hide that, so the number is left alone and only kept from overflowing.
        gpu.reserved_mb = reserved.remove(&gpu.device).unwrap_or(0);
    }
}

/// True for the two specs the local supervisor owns — the only ones holding VRAM here.
fn is_local(spec: &EndpointSpec) -> bool {
    matches!(
        spec,
        EndpointSpec::LocalLlama(_) | EndpointSpec::LocalVllm(_)
    )
}

/// The model cache: `(roots key, models, when)`.
#[allow(clippy::type_complexity)]
fn model_cache() -> &'static Mutex<Option<(String, Vec<LocalModel>, Instant)>> {
    static C: OnceLock<Mutex<Option<(String, Vec<LocalModel>, Instant)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

/// A stable key for a set of model roots.
fn cache_key(roots: &[String]) -> String {
    roots.join("\u{1}")
}

/// The cached list, when it is for `key` and still warm.
fn cached_models(key: &str) -> Option<Vec<LocalModel>> {
    let slot = model_cache().lock().ok()?;
    let (k, v, at) = slot.as_ref()?;
    (k == key && at.elapsed() < MODEL_TTL).then(|| v.clone())
}

/// Replace the cache.
fn store_models(key: String, models: &[LocalModel]) {
    if let Ok(mut slot) = model_cache().lock() {
        *slot = Some((key, models.to_vec(), Instant::now()));
    }
}

/// Drop the cached model walk, so the next read goes to disk.
pub fn invalidate_models() {
    if let Ok(mut slot) = model_cache().lock() {
        *slot = None;
    }
}

/// Find one model by id, by display name, or by the path of any of its shards.
///
/// Operators type what they see, and what they see differs by surface: `apexrouter models ls`
/// prints the id, a recipe carries a path, a shell completion offers the name. All three
/// resolve, and an exact id always wins over a name so a model cannot be shadowed.
pub fn find_model<'a>(models: &'a [LocalModel], want: &str) -> Option<&'a LocalModel> {
    let want = want.trim();
    if want.is_empty() {
        return None;
    }
    models
        .iter()
        .find(|m| m.id == want)
        .or_else(|| models.iter().find(|m| m.name == want))
        .or_else(|| {
            models
                .iter()
                .find(|m| m.shards.iter().any(|s| s.path == want))
        })
        .or_else(|| {
            models
                .iter()
                .find(|m| m.name.eq_ignore_ascii_case(want) || m.id.eq_ignore_ascii_case(want))
        })
}

/// A `404` naming the model that was asked for, and what is actually on disk.
pub fn model_not_found(want: &str, models: &[LocalModel]) -> ApiError {
    let mut names: Vec<&str> = models.iter().map(|m| m.id.as_str()).take(8).collect();
    names.sort_unstable();
    let known = if names.is_empty() {
        "no local models were discovered; check `[endpoints] model_roots`".to_owned()
    } else {
        format!("known: {}", names.join(", "))
    };
    ApiError::not_found(format!("no local model matches `{want}` — {known}")).with_param("model")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::testkit::{app, test_config};
    use apexrouter_protocol::{
        BuildId, DesiredState, FitPlan, FitVerdict, Gpu, GpuBackend, KvType, LocalLlamaSpec,
        ModelShard, NglPlan, SamplingMode, SplitPlan,
    };

    fn gpu(device: &str, total: u64, free: u64, software: bool) -> Gpu {
        Gpu {
            device: device.to_owned(),
            index: 0,
            name: format!("test {device}"),
            backend: GpuBackend::Vulkan,
            vram_total_mb: total,
            vram_free_mb: free,
            driver: None,
            is_software: software,
            seen_by_builds: vec![],
            held_by: vec![],
            reserved_mb: 0,
        }
    }

    fn plan(devices: &[&str], per_device: &[(&str, u64)]) -> FitPlan {
        FitPlan {
            ctx: 4096,
            parallel: 1,
            kv_type: KvType::Q8_0,
            ngl: NglPlan::All,
            split: SplitPlan {
                devices: devices.iter().map(|d| (*d).to_owned()).collect(),
                ..SplitPlan::default()
            },
            weights_mb: 600,
            kv_mb: 300,
            compute_mb: 100,
            headroom_mb: 1000,
            per_device_mb: per_device
                .iter()
                .map(|(d, m)| ((*d).to_owned(), *m))
                .collect(),
            verdict: FitVerdict::Fits { headroom_mb: 1000 },
            why: vec!["test".to_owned()],
        }
    }

    fn record(id: &str, fit: Option<FitPlan>) -> EndpointRecord {
        EndpointRecord {
            id: BackendId::parse(id).expect("id"),
            spec: EndpointSpec::LocalLlama(LocalLlamaSpec {
                build: BuildId::parse("build-vulkan").expect("build"),
                model_path: "/models/x.gguf".to_owned(),
                mmproj: None,
                alias_flag: "x".to_owned(),
                host: "127.0.0.1".to_owned(),
                port: Some(8100),
                ctx: Some(4096),
                parallel: Some(1),
                kv_type: Some(KvType::Q8_0),
                ngl: NglPlan::All,
                split: SplitPlan::default(),
                mode: SamplingMode::Raw,
                flash_attn: None,
                api_key: None,
                extra_args: vec![],
            }),
            desired: DesiredState::Running,
            proc: None,
            port: Some(8100),
            log_path: None,
            started_at_unix: 0,
            fit,
            adopted: false,
            alias_bindings: vec![],
        }
    }

    #[test]
    fn per_device_placement_is_attributed_device_by_device() {
        let mut rig = RigSnapshot {
            gpus: vec![
                gpu("Vulkan0", 8192, 8000, false),
                gpu("Vulkan1", 8192, 8000, false),
            ],
            ..RigSnapshot::default()
        };
        let rec = record(
            "a",
            Some(plan(
                &["Vulkan0", "Vulkan1"],
                &[("Vulkan0", 700), ("Vulkan1", 300)],
            )),
        );
        annotate_holders(&mut rig, &[rec]);
        assert_eq!(rig.gpus[0].reserved_mb, 700);
        assert_eq!(rig.gpus[1].reserved_mb, 300);
        assert_eq!(rig.gpus[0].held_by.len(), 1);
    }

    #[test]
    fn a_plan_with_no_placement_is_spread_over_its_devices() {
        let mut rig = RigSnapshot {
            gpus: vec![
                gpu("Vulkan0", 8192, 8000, false),
                gpu("Vulkan1", 8192, 8000, false),
            ],
            ..RigSnapshot::default()
        };
        let rec = record("a", Some(plan(&["Vulkan0", "Vulkan1"], &[])));
        annotate_holders(&mut rig, &[rec]);
        // 600 + 300 + 100 = 1000, halved.
        assert_eq!(rig.gpus[0].reserved_mb, 500);
        assert_eq!(rig.gpus[1].reserved_mb, 500);
    }

    #[test]
    fn a_plan_with_no_devices_is_charged_to_the_first_real_gpu() {
        let mut rig = RigSnapshot {
            gpus: vec![
                gpu("llvmpipe", 4096, 4096, true),
                gpu("Vulkan0", 8192, 8000, false),
            ],
            ..RigSnapshot::default()
        };
        let rec = record("a", Some(plan(&[], &[])));
        annotate_holders(&mut rig, &[rec]);
        assert_eq!(rig.gpus[0].reserved_mb, 0, "software devices hold nothing");
        assert_eq!(rig.gpus[1].reserved_mb, 1000);
    }

    /// ROCm on this machine reports free (12877) > total (11397) through GTT accounting.
    /// Nothing here may underflow on that, and nothing may quietly "fix" the reading.
    #[test]
    fn free_greater_than_total_is_reported_verbatim_and_never_underflows() {
        let mut rig = RigSnapshot {
            gpus: vec![gpu("ROCm0", 11397, 12877, false)],
            ..RigSnapshot::default()
        };
        annotate_holders(&mut rig, &[record("a", Some(plan(&[], &[])))]);
        assert_eq!(rig.gpus[0].vram_total_mb, 11397);
        assert_eq!(rig.gpus[0].vram_free_mb, 12877);
        assert_eq!(rig.gpus[0].reserved_mb, 1000);
        assert_eq!(
            rig.gpus[0]
                .vram_total_mb
                .saturating_sub(rig.gpus[0].vram_free_mb),
            0,
            "a saturating `total - free` is 0, never u64::MAX"
        );
    }

    #[test]
    fn an_endpoint_with_no_fit_plan_reserves_nothing() {
        let mut rig = RigSnapshot {
            gpus: vec![gpu("Vulkan0", 8192, 8000, false)],
            ..RigSnapshot::default()
        };
        annotate_holders(&mut rig, &[record("a", None)]);
        assert_eq!(rig.gpus[0].reserved_mb, 0);
        assert!(rig.gpus[0].held_by.is_empty());
    }

    #[test]
    fn a_model_resolves_by_id_by_name_and_by_path() {
        let m = LocalModel {
            id: "carnice-9b-q6-k".to_owned(),
            name: "Carnice-9b-Q6_K".to_owned(),
            dir: "/models/carnice-9b".to_owned(),
            shards: vec![ModelShard {
                path: "/models/carnice-9b/Carnice-9b-Q6_K.gguf".to_owned(),
                bytes: 10,
            }],
            total_bytes: 10,
            mmproj: vec![],
            quant: Some("Q6_K".to_owned()),
            gguf: None,
            discovered_at_unix: 0,
        };
        let all = vec![m];
        assert!(find_model(&all, "carnice-9b-q6-k").is_some());
        assert!(find_model(&all, "Carnice-9b-Q6_K").is_some());
        assert!(find_model(&all, "/models/carnice-9b/Carnice-9b-Q6_K.gguf").is_some());
        assert!(
            find_model(&all, "carnice-9b-q6_k").is_some(),
            "case-insensitive fallback"
        );
        assert!(find_model(&all, "").is_none());
        assert!(find_model(&all, "nope").is_none());
    }

    /// The scan must never run against Andre's real `~/models` or `~/llama.cpp` from a test.
    #[tokio::test]
    async fn the_rig_read_uses_the_cache_and_never_scans_in_a_test() {
        let mut cfg = test_config();
        cfg.endpoints.model_roots = vec!["/nonexistent/models".to_owned()];
        cfg.endpoints.build_roots = vec!["/nonexistent/builds".to_owned()];
        let state = app(cfg);
        state.supervisor.set_rig(RigSnapshot {
            gpus: vec![gpu("Vulkan0", 8192, 8000, false)],
            ram_total_mb: 24_000,
            ..RigSnapshot::default()
        });
        let rig = rig_snapshot(&state, false).await.expect("rig");
        assert_eq!(rig.gpus.len(), 1);
        assert_eq!(rig.ram_total_mb, 24_000);
    }

    #[tokio::test]
    async fn a_model_walk_over_an_empty_root_is_empty_not_an_error() {
        let mut cfg = test_config();
        let dir = tempfile::TempDir::new().expect("tempdir");
        cfg.endpoints.model_roots = vec![dir.path().display().to_string()];
        let state = app(cfg);
        let found = local_model_list(&state, true).await.expect("walk");
        assert!(found.is_empty());
        invalidate_models();
    }

    /// The whole S-04 surface, over a real loopback socket.
    ///
    /// Calling handlers directly proves the bodies; only serving them proves the *routes* —
    /// that the paths are spelled as `ARCHITECTURE.md` §6.2 spells them, that the query
    /// extractors accept what the CLI sends, that nothing collides on merge, and that a
    /// refusal arrives as an `ErrorEnvelope` rather than an axum default. Nothing but
    /// `127.0.0.1` is ever contacted.
    #[tokio::test]
    async fn the_whole_s04_surface_answers_over_http() {
        let mut cfg = test_config();
        let dir = tempfile::TempDir::new().expect("tempdir");
        cfg.endpoints.model_roots = vec![dir.path().display().to_string()];
        cfg.endpoints.build_roots = vec!["/nonexistent/builds".to_owned()];
        let state = app(cfg);
        invalidate_models();
        state.supervisor.set_rig(RigSnapshot {
            gpus: vec![gpu("Vulkan0", 8192, 8000, false)],
            ram_total_mb: 24_000,
            ..RigSnapshot::default()
        });

        let app_router = axum::Router::new()
            .merge(router())
            .merge(crate::api::fit::router())
            .merge(crate::api::usage::router())
            .merge(crate::api::requests::router())
            .merge(crate::api::jobs::router())
            .merge(crate::api::catalog::router())
            .with_state(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind::<std::net::SocketAddr>(
            "127.0.0.1:0".parse().expect("loopback"),
        )
        .await
        .expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app_router).await;
        });
        let http = reqwest::Client::new();

        // Every documented GET answers with its protocol type.
        for (path, probe) in [
            ("/v1/rig", "gpus"),
            ("/v1/models/local", ""),
            ("/v1/usage?since=24h&by=day", "window"),
            ("/v1/requests?limit=5", ""),
            ("/v1/jobs", ""),
            ("/v1/recipes", ""),
            ("/v1/profiles", ""),
        ] {
            let r = http
                .get(format!("{base}{path}"))
                .send()
                .await
                .unwrap_or_else(|e| panic!("{path}: {e}"));
            assert_eq!(r.status(), 200, "{path}");
            let body: serde_json::Value = r.json().await.unwrap_or_else(|e| panic!("{path}: {e}"));
            if probe.is_empty() {
                assert!(body.is_array(), "{path} is a list: {body}");
            } else {
                assert!(
                    body.get(probe).is_some(),
                    "{path} carries `{probe}`: {body}"
                );
            }
        }

        // `?builds=false&models=false` must not exec anything.
        let r = http
            .post(format!("{base}/v1/rig/rescan?builds=false&models=false"))
            .send()
            .await
            .expect("rescan");
        assert_eq!(r.status(), 200);

        // A refusal is an ErrorEnvelope, with a kind a script can branch on.
        let r = http
            .get(format!("{base}/v1/fit?model=no-such-model"))
            .send()
            .await
            .expect("fit");
        assert_eq!(r.status(), 404);
        let body: serde_json::Value = r.json().await.expect("json");
        assert_eq!(body["error"]["kind"], "not_found");
        assert_eq!(body["error"]["param"], "model");

        let r = http
            .get(format!("{base}/v1/usage?since=last%20tuesday"))
            .send()
            .await
            .expect("usage");
        assert_eq!(r.status(), 400);
        let body: serde_json::Value = r.json().await.expect("json");
        assert_eq!(body["error"]["param"], "since");

        // POST /v1/fit is pure: it solves exactly what it is handed.
        let input = serde_json::json!({
            "weights_bytes": 7_000_000_000u64,
            "gguf": {
                "arch": "qwen3", "n_layer": 32, "n_head_kv": 8,
                "n_embd_head_k": 128, "n_embd_head_v": 128, "n_ctx_train": 262144,
                "full_attn_layers": 8
            },
            "budget": {
                "devices": [{"device": "Vulkan0", "free_mb": 16384, "reserved_mb": 0}],
                "margin_mb": 1024, "host_ram_free_mb": 8192
            },
            "want_ctx": 32768,
            "split": {"mode": "layer"}
        });
        let r = http
            .post(format!("{base}/v1/fit"))
            .json(&input)
            .send()
            .await
            .expect("fit post");
        assert_eq!(r.status(), 200);
        let plan: serde_json::Value = r.json().await.expect("json");
        assert_eq!(plan["ctx"], 32768);
        assert!(
            plan["why"].as_array().is_some_and(|w| !w.is_empty()),
            "an unexplained verdict is a bug: {plan}"
        );

        invalidate_models();
    }
}
