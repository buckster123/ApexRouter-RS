//! OWNER: studio phase 7 (server/src/api/studio.rs).
//!
//! The one studio verb surface (STUDIO.md S1 / S9):
//!
//! * `GET  /v1/studio`       — status (facts + computed liveness)
//! * `POST /v1/studio/up`    — wake → converge → rent (one resolution order)
//! * `POST /v1/studio/down`  — park only (never destroy)
//!
//! Money: rent and wake require `confirm` + `max_usd_per_hour` through
//! [`SpendApproval`]. Down is unconfirmed (reduces spend). Nothing auto-destroys.
//!
//! Comfy lanes are ServiceRecords, never Backends (S2). Only OpenAI-routed services
//! enter the routing table.

use super::{now_unix, register_backend, ApiError, ApiResult};
use crate::api::vast::{self, ensure_tunnel};
use crate::jobs::JobHandle;
use crate::state::AppState;
use apexrouter_core::catalog;
use apexrouter_core::ledger::Ledger;
use apexrouter_core::money::{ApprovalSource, SpendApproval};
use apexrouter_protocol::{
    Alias, BackendId, BootPhase, ContainerLaunch, ContainerRuntime, DesiredState, Event, ImageType,
    InstanceId, JobRecord, Money, RecipeId, RecipeKind, RentRequest, SearchProfile, ServiceId,
    ServiceRecord, ServiceRouting, ServiceRuntime, StudioLaunch, StudioRecord, StudioStatus,
    StudioUpPath, StudioUpRequest, VastSpec,
};
use apexrouter_providers::vast::{profile_to_query, search_unified, QueryOverrides, VastApi};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;

/// Default recipe when the body omits `recipe_id`.
const DEFAULT_RECIPE: &str = "studio-96gb";

/// The `/v1/studio*` routes. Merged once by `crate::v1_routes` (S-01).
pub fn router() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route("/v1/studio", get(status))
        .route("/v1/studio/up", post(up))
        .route("/v1/studio/down", post(down))
}

// ----------------------------------------------------------------------------------------
// status
// ----------------------------------------------------------------------------------------

/// `GET /v1/studio` — the whole studio as facts + computed liveness.
pub async fn status(State(s): State<Arc<AppState>>) -> ApiResult<StudioStatus> {
    Ok(Json(build_status(&s).await?))
}

async fn build_status(s: &Arc<AppState>) -> Result<StudioStatus, ApiError> {
    let studio = s.store.load_studio().map_err(ApiError::from)?;
    let all_services = s.store.load_services().map_err(ApiError::from)?;
    let services: Vec<ServiceRecord> = match &studio {
        Some(st) => all_services
            .into_iter()
            .filter(|svc| svc.instance_id == st.instance_id)
            .collect(),
        None => all_services,
    };
    let service_status = s
        .service_status
        .all()
        .into_iter()
        .filter(|st| services.iter().any(|r| r.id == st.record.id))
        .collect();
    let tunnels = s
        .store
        .load_tunnels()
        .map_err(ApiError::from)?
        .into_iter()
        .filter(|t| {
            studio
                .as_ref()
                .is_some_and(|st| t.spec.instance_id == st.instance_id)
        })
        .collect();

    let (instance_phase, dph_total, next_up_path, summary) = match &studio {
        None => (
            None,
            None,
            StudioUpPath::Rent,
            "no studio recorded — `studio up` will rent".to_owned(),
        ),
        Some(st) => {
            let mut inst = s
                .fleet_cache()
                .instances
                .iter()
                .find(|i| i.id == st.instance_id)
                .cloned();
            if inst.is_none() {
                if let Some(api) = vast::vast_api() {
                    inst = api.instance(st.instance_id).await.ok().flatten();
                }
            }
            match inst {
                None => (
                    None,
                    None,
                    StudioUpPath::Rent,
                    format!(
                        "studio names instance {} but it is not in the fleet — up will re-rent",
                        st.instance_id
                    ),
                ),
                Some(i) => {
                    let phase = i.phase();
                    let dph = i.dph_total;
                    let (path, summary) = match &phase {
                        BootPhase::Parked => (
                            StudioUpPath::Wake,
                            format!(
                                "instance {} is parked — up will wake and restore tunnels",
                                st.instance_id
                            ),
                        ),
                        BootPhase::Healthy => (
                            StudioUpPath::Converge,
                            format!(
                                "instance {} is running — up will converge tunnels + readiness",
                                st.instance_id
                            ),
                        ),
                        other => (
                            StudioUpPath::Converge,
                            format!(
                                "instance {} is in phase {other:?} — up will try to converge",
                                st.instance_id
                            ),
                        ),
                    };
                    (Some(phase), dph, path, summary)
                }
            }
        }
    };

    Ok(StudioStatus {
        studio,
        services,
        service_status,
        tunnels,
        instance_phase,
        dph_total,
        next_up_path,
        summary,
    })
}

// ----------------------------------------------------------------------------------------
// up
// ----------------------------------------------------------------------------------------

/// Query params shared with the vast money gate (`?source=cli`).
#[derive(Debug, Default, Deserialize)]
pub struct StudioQuery {
    /// Approval source label.
    #[serde(default)]
    pub source: Option<String>,
}

/// `POST /v1/studio/up` — the one verb (S1).
pub async fn up(
    State(s): State<Arc<AppState>>,
    Query(q): Query<StudioQuery>,
    Json(req): Json<StudioUpRequest>,
) -> Result<Response, ApiError> {
    let snap = build_status(&s).await?;
    match snap.next_up_path {
        StudioUpPath::Wake => up_wake(s, q, req, snap).await,
        StudioUpPath::Converge => up_converge(s, snap).await,
        StudioUpPath::Rent => up_rent(s, q, req).await,
    }
}

async fn up_wake(
    s: Arc<AppState>,
    q: StudioQuery,
    req: StudioUpRequest,
    snap: StudioStatus,
) -> Result<Response, ApiError> {
    let studio = snap
        .studio
        .clone()
        .ok_or_else(|| ApiError::conflict("wake path without a studio record"))?;
    let id = studio.instance_id;
    let api = vast::require_vast()?;
    let cfg = s.cfg();
    let instance = api
        .instance(id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::not_found(format!("no instance {}", id.0)))?;
    let dph = instance
        .dph_total
        .unwrap_or(cfg.vast.max_usd_per_hour_ceiling);
    let credit = api.account().await.ok().map(|a| a.credit);

    if !req.confirm || !req.max_usd_per_hour.is_finite() || req.max_usd_per_hour <= 0.0 {
        return Ok(refuse_money(
            "confirmation_required",
            format!(
                "waking studio instance {} resumes billing at ${dph:.4}/hr — re-send with \
                 confirm:true and max_usd_per_hour",
                id.0
            ),
            dph,
            credit,
            StudioUpPath::Wake,
        ));
    }
    if dph > req.max_usd_per_hour {
        return Ok(refuse_money(
            "above_ceiling",
            format!(
                "instance bills ${dph:.4}/hr which exceeds max_usd_per_hour ${:.4}",
                req.max_usd_per_hour
            ),
            dph,
            credit,
            StudioUpPath::Wake,
        ));
    }

    let approval =
        SpendApproval::confirm(Money::from_usd(dph), approval_source(&q), &cfg.vast, credit)
            .map_err(|e| ApiError::conflict(e.to_string()))?;

    let ledger = Ledger::open(&s.paths).map_err(ApiError::from)?;
    let state = Arc::clone(&s);
    let max_boot = cfg.vast.max_boot_secs;
    let poll_min = cfg.vast.poll_min_ms;
    let snap_for_fallback = snap.clone();
    let job = s.jobs.spawn_with("studio.wake", move |h| async move {
        h.progress(Some(10.0), format!("waking instance {}", id.0));
        apexrouter_providers::vast::wake(
            api.as_ref(),
            &ledger,
            id,
            approval,
            &state.tx,
            max_boot,
            poll_min,
        )
        .await?;
        h.progress(Some(50.0), "restoring studio tunnels");
        converge(&state, api.as_ref(), &studio, &h).await?;
        crate::fleet::poll_once(&state).await;
        let status = build_status(&state).await.unwrap_or(snap_for_fallback);
        h.progress(Some(100.0), "studio awake");
        Ok(serde_json::to_value(status)?)
    });
    Ok(job_accepted(job))
}

async fn up_converge(s: Arc<AppState>, snap: StudioStatus) -> Result<Response, ApiError> {
    let studio = snap
        .studio
        .clone()
        .ok_or_else(|| ApiError::conflict("converge path without a studio record"))?;
    let api = vast::require_vast()?;
    let state = Arc::clone(&s);
    let snap_for_fallback = snap;
    let job = s.jobs.spawn_with("studio.converge", move |h| async move {
        h.progress(Some(20.0), "converging tunnels + service records");
        converge(&state, api.as_ref(), &studio, &h).await?;
        let status = build_status(&state).await.unwrap_or(snap_for_fallback);
        h.progress(Some(100.0), "studio converged");
        Ok(serde_json::to_value(status)?)
    });
    Ok(job_accepted(job))
}

async fn up_rent(
    s: Arc<AppState>,
    q: StudioQuery,
    req: StudioUpRequest,
) -> Result<Response, ApiError> {
    let cfg = s.cfg();
    let recipe_id = req
        .recipe_id
        .clone()
        .unwrap_or_else(|| RecipeId::parse(DEFAULT_RECIPE).expect("seed id"));
    // Ensure seeds exist even if the daemon was upgraded mid-session.
    let _ = catalog::ensure_studio_seeds(&s.paths, &cfg.docker.studio);
    let cat = catalog::load(&s.paths).map_err(ApiError::from)?;
    let recipe = cat
        .recipes
        .iter()
        .find(|r| r.id == recipe_id)
        .cloned()
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "no recipe `{recipe_id}` — daemon seeds studio-96gb on start"
            ))
        })?;
    let (profile_id, machine_id, launch) = match &recipe.kind {
        RecipeKind::VastStudio {
            profile,
            machine_id,
            launch,
        } => (profile.clone(), *machine_id, launch.clone()),
        _ => {
            return Err(ApiError::conflict(format!(
                "recipe `{recipe_id}` is not a VastStudio recipe"
            )));
        }
    };
    let machine_id = req.machine_id.or(machine_id);

    let api_opt = vast::vast_api();
    let credit = match api_opt.as_ref() {
        Some(api) => api.account().await.ok().map(|a| a.credit),
        None => None,
    };

    if !req.confirm || !req.max_usd_per_hour.is_finite() || req.max_usd_per_hour <= 0.0 {
        return Ok(refuse_money(
            "confirmation_required",
            format!(
                "renting studio recipe `{recipe_id}` costs money: re-send with confirm:true \
                 and a positive max_usd_per_hour (daemon ceiling ${:.2}/hr)",
                cfg.vast.max_usd_per_hour_ceiling
            ),
            req.max_usd_per_hour.max(0.0),
            credit,
            StudioUpPath::Rent,
        ));
    }

    let approval = SpendApproval::confirm(
        Money::from_usd(req.max_usd_per_hour),
        approval_source(&q),
        &cfg.vast,
        credit,
    )
    .map_err(|e| ApiError::conflict(e.to_string()))?;

    let api = api_opt.ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "vast_unavailable",
            "no vast client — configure a vast API key",
        )
    })?;

    let profile = cat
        .profiles
        .iter()
        .find(|p| p.id == profile_id)
        .cloned()
        .or_else(|| {
            catalog::default_profiles()
                .into_iter()
                .find(|p| p.id == profile_id)
        })
        .ok_or_else(|| ApiError::not_found(format!("no profile `{profile_id}`")))?;

    let offer_id = if let Some(oid) = req.offer_id {
        oid
    } else {
        resolve_studio_offer(api.as_ref(), &profile, machine_id, req.max_usd_per_hour).await?
    };

    let container = studio_to_container(&launch, &cfg.docker.studio);
    let rent_req = RentRequest {
        profile: Some(profile_id.clone()),
        offer_id: Some(offer_id),
        launch: container,
        confirm: true,
        max_usd_per_hour: req.max_usd_per_hour,
        auto_tunnel: false,
        bind_alias: None,
    };

    let ledger = Ledger::open(&s.paths).map_err(ApiError::from)?;
    let state = Arc::clone(&s);
    let max_boot = cfg.vast.max_boot_secs;
    let recipe_id_job = recipe.id.clone();
    let job = s.jobs.spawn_with("studio.rent", move |h| async move {
        h.progress(Some(5.0), format!("reserving offer {offer_id}"));
        let id =
            apexrouter_providers::vast::rent(api.as_ref(), &ledger, &rent_req, approval, &state.tx)
                .await?;
        h.progress(Some(30.0), format!("instance {} created; booting", id.0));
        let phase =
            apexrouter_providers::vast::watch_boot(api.as_ref(), id, max_boot, &state.tx).await?;
        if phase != BootPhase::Healthy {
            crate::fleet::poll_once(&state).await;
            anyhow::bail!("studio instance {} reached {phase:?}, not Healthy", id.0);
        }

        let studio = StudioRecord {
            instance_id: id,
            machine_id,
            recipe_id: Some(recipe_id_job),
            profile_id: Some(profile_id),
            service_ids: Vec::new(),
            endpoint_ids: Vec::new(),
            created_at_unix: now_unix(),
            updated_at_unix: now_unix(),
        };
        state
            .store
            .save_studio(Some(&studio))
            .map_err(|e| anyhow::anyhow!(e))?;

        h.progress(Some(60.0), "opening studio tunnels");
        converge(&state, api.as_ref(), &studio, &h).await?;

        crate::fleet::poll_once(&state).await;
        let status = build_status(&state).await.unwrap_or(StudioStatus {
            studio: Some(studio),
            services: vec![],
            service_status: vec![],
            tunnels: vec![],
            instance_phase: Some(BootPhase::Healthy),
            dph_total: None,
            next_up_path: StudioUpPath::Converge,
            summary: "studio rented".into(),
        });
        h.progress(Some(100.0), "studio up");
        Ok(serde_json::to_value(status)?)
    });
    Ok(job_accepted(job))
}

// ----------------------------------------------------------------------------------------
// down = park
// ----------------------------------------------------------------------------------------

/// `POST /v1/studio/down` — park the studio box. Never destroys (S9).
pub async fn down(State(s): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let studio = s
        .store
        .load_studio()
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::not_found("no active studio to park"))?;
    let api = vast::require_vast()?;
    let id = studio.instance_id;
    let ledger = Ledger::open(&s.paths).map_err(ApiError::from)?;
    let cfg = s.cfg();

    if let Ok(tunnels) = s.store.load_tunnels() {
        if tunnels.iter().any(|t| t.spec.instance_id == id) {
            if let Ok(sup) = vast::require_tunnels() {
                let _ = sup.down(id).await;
            }
            let rest: Vec<_> = tunnels
                .into_iter()
                .filter(|t| t.spec.instance_id != id)
                .collect();
            let _ = s.store.save_tunnels(&rest);
        }
    }

    let parked = apexrouter_providers::vast::park(
        api.as_ref(),
        &ledger,
        id,
        &s.tx,
        90,
        cfg.vast.poll_min_ms,
    )
    .await
    .map_err(ApiError::from)?;

    if let Ok(mut services) = s.store.load_services() {
        for svc in &mut services {
            if svc.instance_id == id {
                svc.desired = DesiredState::Stopped;
            }
        }
        let _ = s.store.save_services(&services);
    }
    if let Ok(Some(mut st)) = s.store.load_studio() {
        st.updated_at_unix = now_unix();
        let _ = s.store.save_studio(Some(&st));
    }

    crate::fleet::poll_once(&s).await;
    Ok(Json(serde_json::json!({
        "instance_id": id,
        "phase": parked.phase(),
        "disk_gb": parked.disk_space,
        "weekly_disk_usd": apexrouter_providers::vast::weekly_disk_usd(&parked),
        "note": "studio parked — disk held; run studio up to wake",
    }))
    .into_response())
}

// ----------------------------------------------------------------------------------------
// converge: tunnels + ServiceRecords + OpenAI backends
// ----------------------------------------------------------------------------------------

async fn converge(
    state: &Arc<AppState>,
    api: &dyn VastApi,
    studio: &StudioRecord,
    h: &JobHandle,
) -> anyhow::Result<()> {
    let launch = launch_for(state, studio)?;
    h.progress(
        Some(55.0),
        format!("tunnels for {} services", launch.services.len()),
    );
    let studio = write_services_and_tunnels(state, api, studio, &launch).await?;
    h.progress(Some(80.0), "registering OpenAI lanes");
    register_openai_lanes(state, api, &studio, &launch).await?;
    let mut st = studio;
    st.updated_at_unix = now_unix();
    state.store.save_studio(Some(&st))?;
    let _ = state.tx.send(Event::StudioChanged {
        studio: Some(Box::new(st)),
    });
    Ok(())
}

fn launch_for(state: &Arc<AppState>, studio: &StudioRecord) -> anyhow::Result<StudioLaunch> {
    if let Some(rid) = &studio.recipe_id {
        if let Ok(cat) = catalog::load(&state.paths) {
            if let Some(r) = cat.recipes.iter().find(|r| r.id == *rid) {
                if let RecipeKind::VastStudio { launch, .. } = &r.kind {
                    return Ok(launch.clone());
                }
            }
        }
    }
    // Fall back to the seed services so converge still works without a recipe.
    Ok(StudioLaunch {
        image: state.cfg().docker.studio.clone(),
        image_type: ImageType::Studio,
        disk_gb: 2000,
        env: Default::default(),
        onstart: "bash /app/studio.sh > /var/log/studio.log 2>&1 &".into(),
        host: "127.0.0.1".into(),
        expose_public: false,
        services: apexrouter_protocol::studio_96gb_services(),
    })
}

async fn write_services_and_tunnels(
    state: &Arc<AppState>,
    api: &dyn VastApi,
    studio: &StudioRecord,
    launch: &StudioLaunch,
) -> anyhow::Result<StudioRecord> {
    let id = studio.instance_id;
    let mut service_ids = Vec::new();
    let mut records = Vec::new();
    let now = now_unix();

    for spec in &launch.services {
        let local = match ensure_tunnel(state, api, id, spec.local_port, Some(spec.port)).await {
            Ok(t) => t.spec.local_port,
            Err(e) => {
                state.alert(
                    apexrouter_protocol::AlertLevel::Serious,
                    &format!("studio.tunnel.{}.{}.{}", id.0, spec.name, spec.port),
                    format!(
                        "studio tunnel for `{}` (remote {}) failed: {}; instance {} is billing",
                        spec.name, spec.port, e.body.message, id.0
                    ),
                );
                anyhow::bail!(
                    "tunnel for service `{}` remote_port {} failed: {}",
                    spec.name,
                    spec.port,
                    e.body.message
                );
            }
        };

        let sid =
            ServiceId::parse(&format!("studio-{}", spec.name)).map_err(|e| anyhow::anyhow!(e))?;
        service_ids.push(sid.clone());
        records.push(ServiceRecord {
            id: sid,
            instance_id: id,
            name: spec.name.clone(),
            runtime: spec.runtime,
            remote_port: spec.port,
            local_port: local,
            health: spec.health.clone(),
            devices: spec.devices.clone(),
            reserved_mb: spec.reserved_mb,
            desired: DesiredState::Running,
            started_at_unix: now,
        });
        let _ = state.tx.send(Event::ServiceChanged {
            service: Box::new(records.last().expect("just pushed").clone()),
        });
    }

    // Replace services for this instance; keep any orphan unrelated rows.
    let mut all = state.store.load_services().unwrap_or_default();
    all.retain(|r| r.instance_id != id);
    all.extend(records);
    state.store.save_services(&all)?;

    let mut st = studio.clone();
    st.service_ids = service_ids;
    st.updated_at_unix = now;
    state.store.save_studio(Some(&st))?;
    Ok(st)
}

async fn register_openai_lanes(
    state: &Arc<AppState>,
    api: &dyn VastApi,
    studio: &StudioRecord,
    launch: &StudioLaunch,
) -> anyhow::Result<()> {
    let id = studio.instance_id;
    let inst = api
        .instance(id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("instance {} gone during register", id.0))?;

    let mut endpoint_ids = studio.endpoint_ids.clone();
    for spec in &launch.services {
        let ServiceRouting::OpenAi { alias } = &spec.routing else {
            continue;
        };
        let local = state
            .store
            .load_services()?
            .into_iter()
            .find(|r| r.instance_id == id && r.name == spec.name)
            .map(|r| r.local_port)
            .unwrap_or(spec.local_port.unwrap_or(0));
        if local == 0 {
            continue;
        }

        let vast_spec = VastSpec {
            instance_id: id,
            runtime: match spec.runtime {
                ServiceRuntime::Vllm => ContainerRuntime::Vllm,
                _ => ContainerRuntime::LlamaCpp,
            },
            launch: studio_to_container(launch, &state.cfg().docker.studio),
            tunnel: state
                .store
                .load_tunnels()?
                .into_iter()
                .find(|t| t.spec.instance_id == id && t.spec.remote_port == spec.port)
                .map(|t| t.spec),
        };
        let mut backend = apexrouter_providers::vast::rented_backend(&inst, &vast_spec)?;
        // Force loopback URL to the tunnel local port for this service.
        backend.base_url = format!("http://127.0.0.1:{local}");
        let bid = backend.id.clone();
        if !endpoint_ids.contains(&bid) {
            endpoint_ids.push(bid.clone());
        }
        register_backend(state, backend);

        if let Some(a) = alias
            .clone()
            .or_else(|| Alias::parse(&format!("studio-{}", spec.name)).ok())
        {
            if let Ok(mut routes) = state.store.load_routes() {
                use apexrouter_protocol::{
                    BackendSelector, ModelRoute, RetryPolicy, RouteFilter, RouteTarget, Strategy,
                };
                let target = RouteTarget {
                    backend: BackendSelector::Id(bid.clone()),
                    model: None,
                    weight: 1,
                };
                if let Some(existing) = routes.routes.iter_mut().find(|r| r.alias == a) {
                    existing.targets = vec![target];
                    existing.strategy = Strategy::FirstHealthy;
                } else {
                    routes.routes.push(ModelRoute {
                        alias: a,
                        targets: vec![target],
                        strategy: Strategy::FirstHealthy,
                        filter: RouteFilter::default(),
                        retry: RetryPolicy::default(),
                        is_default: false,
                        description: Some("studio OpenAI lane".into()),
                    });
                }
                let _ = super::apply_routes(state, &routes);
            }
        }
    }

    let mut st = studio.clone();
    st.endpoint_ids = endpoint_ids;
    st.updated_at_unix = now_unix();
    state.store.save_studio(Some(&st))?;
    Ok(())
}

// ----------------------------------------------------------------------------------------
// offer resolution + container conversion
// ----------------------------------------------------------------------------------------

async fn resolve_studio_offer(
    api: &dyn VastApi,
    profile: &SearchProfile,
    machine_id: Option<u64>,
    max_usd: f64,
) -> Result<u64, ApiError> {
    let mut overrides = QueryOverrides::none().with_max_dph(max_usd).with_limit(50);
    // Prefer verified when the profile does not say otherwise (S21).
    let mut q = profile_to_query(profile, &overrides);
    if q.verified.is_none() {
        q.verified = Some(true);
    }
    // search_unified takes profile + overrides; put verified into overrides.extra if needed.
    let _ = q;
    let result = search_unified(api, profile, &overrides)
        .await
        .map_err(ApiError::from)?;
    let mut offers = result.offers;
    if offers.is_empty() {
        // Retry without verified preference.
        overrides = QueryOverrides::none().with_max_dph(max_usd).with_limit(50);
        let result = search_unified(api, profile, &overrides)
            .await
            .map_err(ApiError::from)?;
        offers = result.offers;
    }
    if let Some(mid) = machine_id {
        if let Some(o) = offers.iter().find(|o| o.machine_id == Some(mid)) {
            return Ok(o.id);
        }
        // Machine pin missed — fall through to best offer with a note in the error if empty.
        tracing::warn!(
            machine_id = mid,
            "studio machine pin not in market; falling back to ranked search"
        );
    }
    offers.first().map(|o| o.id).ok_or_else(|| {
        ApiError::conflict(format!(
            "no offers matched profile `{}` under ${max_usd:.2}/hr",
            profile.id
        ))
    })
}

fn studio_to_container(launch: &StudioLaunch, default_image: &str) -> ContainerLaunch {
    let image = if launch.image.is_empty() {
        default_image.to_owned()
    } else {
        launch.image.clone()
    };
    ContainerLaunch {
        runtime: ContainerRuntime::LlamaCpp,
        image,
        image_type: ImageType::Studio,
        disk_gb: launch.disk_gb,
        env: launch.env.clone(),
        onstart: launch.onstart.clone(),
        host: launch.host.clone(),
        port: 8000,
        expose_public: launch.expose_public,
    }
}

// ----------------------------------------------------------------------------------------
// helpers
// ----------------------------------------------------------------------------------------

fn approval_source(q: &StudioQuery) -> ApprovalSource {
    match q.source.as_deref() {
        Some("mcp") => ApprovalSource::Mcp {
            human_cleared: false,
        },
        Some("web_ui") | Some("web") => ApprovalSource::WebUi,
        Some("api") => ApprovalSource::Api,
        Some("slint") => ApprovalSource::SlintUi,
        _ => ApprovalSource::Cli,
    }
}

fn refuse_money(
    kind: &str,
    message: String,
    dph: f64,
    credit: Option<f64>,
    path: StudioUpPath,
) -> Response {
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({
            "error": {
                "kind": kind,
                "message": message,
                "param": "confirm",
            },
            "path": path,
            "dph_total": dph,
            "credit": credit,
            "burn_down_hours": credit.filter(|_| dph > 0.0).map(|c| c / dph),
        })),
    )
        .into_response()
}

fn job_accepted(job: JobRecord) -> Response {
    (StatusCode::ACCEPTED, Json(job)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_protocol::studio_96gb_services;

    #[test]
    fn studio_to_container_uses_studio_image_type() {
        let launch = StudioLaunch {
            image: "ghcr.io/buckster123/vastai-studio:cu128".into(),
            image_type: ImageType::Studio,
            disk_gb: 2000,
            env: Default::default(),
            onstart: "bash /app/studio.sh &".into(),
            host: "127.0.0.1".into(),
            expose_public: false,
            services: studio_96gb_services(),
        };
        let c = studio_to_container(&launch, "fallback");
        assert_eq!(c.image_type, ImageType::Studio);
        assert_eq!(c.port, 8000);
        assert_eq!(c.image, launch.image);
        assert!(!c.expose_public);
    }

    #[test]
    fn default_recipe_id_parses() {
        assert_eq!(
            RecipeId::parse(DEFAULT_RECIPE).unwrap().as_str(),
            "studio-96gb"
        );
    }
}
