//! OWNER: unit S-07 (server/src/api/{vast,hf,providers,checks,compare}.rs). Do not edit outside that unit.
//!
//! The `/v1/vast/*` set. `POST /v1/vast/instances` returns **409** without `{confirm:true, max_usd_per_hour}`, and the 409 body carries the cost preview and the current credit.
//!
//! # The money gate, concretely
//!
//! There is exactly one route in this daemon that can start a bill, and it is unreachable
//! without a [`SpendApproval`] — a value whose only constructor enforces
//! `[providers.vast] max_usd_per_hour_ceiling` and `require_human_confirm`. The order here
//! is deliberate and each step is refusable:
//!
//! 1. **Shape.** No `confirm: true`, or a `max_usd_per_hour` that is not positive, and the
//!    answer is a `409` *before anything is asked of vast* except the free
//!    `GET /users/current/` that reads the credit. The body carries [`RentPreview`] — what
//!    it will cost, what the ceiling is, and what the credit is — beside a normal
//!    `ErrorEnvelope`, so a client that only knows `{"error":{…}}` still parses it.
//! 2. **Approval.** `SpendApproval::confirm` applies the daemon-side ceiling, then the human
//!    gate, then the credit. Each refusal is its own `409` kind, again carrying the preview.
//! 3. **Reserve, then bill.** P-04's `rent()` appends the `Reserved` ledger row *before* the
//!    create call. A `SIGKILL` between the two leaves a row that startup reconciliation
//!    finds; that row, not a `Drop` handler, is the real protection.
//!
//! `DELETE /v1/vast/instances/{id}` needs `?confirm=true` for the mirror-image reason, and
//! **verifies before forgetting**: destroy, poll until the instance is gone or terminal, and
//! only then append `Destroyed` with the accrued cost. Instances are never auto-destroyed on
//! daemon shutdown, at any setting.
//!
//! # The client is injected, not constructed
//!
//! P-02 publishes `VastApiHttp` with private fields and no constructor, and `AppState` is
//! S-01's file with no vast slot in it. [`install_vast_api`] is how a client arrives, and
//! everything here talks to `&dyn VastApi` — which is also what lets the whole money gate be
//! tested for free against a fake that has no network at all.

use super::{now_unix, ApiError, ApiResult};
use crate::state::AppState;
use apexrouter_core::catalog;
use apexrouter_core::exec::SshOpts;
use apexrouter_core::ledger::Ledger;
use apexrouter_core::money::{ApprovalError, ApprovalSource, SpendApproval};
use apexrouter_protocol::{
    AlertLevel, ApprovalRequest, CheckResult, CostEstimate, DownloadHealth, ErrorBody,
    ErrorEnvelope, Event, FavoriteHost, FavoriteVerdict, InstanceId, JobId, JobRecord, LedgerRow,
    LedgerState, Money, OfferQuery, OfferSearchResult, PriceSource, ProfileId, RentRequest,
    TunnelSpec, TunnelStatus, VastAccount, VastInstance,
};
use apexrouter_providers::vast::{
    constraint_failures, gpu_name_vocabulary, profile_to_query, restart_download, sample_download,
    search_unified, watch_boot, QueryOverrides, VastApi,
};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

/// How long `?confirm=true` waits for a destroyed instance to actually disappear before it
/// gives up and says so rather than claiming success.
const DESTROY_VERIFY_SECS: u64 = 90;
/// Floor on how often anything here polls vast. Vast publishes no rate-limit headers, so the
/// only safe policy is our own floor.
const POLL_FLOOR_MS: u64 = 2_000;
/// Default `?tail=` on the instance log.
const DEFAULT_TAIL: u32 = 200;
/// The container port a rented `llama-server` listens on (`ARCHITECTURE.md` §3.7).
const CONTAINER_PORT: u16 = 8000;
/// The minimum billable window a cost preview assumes. Vast bills by the second, but nobody
/// rents a box for less than an hour of work, and a preview that says `$0.00` is useless.
const PREVIEW_HOURS: f64 = 1.0;

/// The `/v1/vast/*`, `/v1/tunnels` and `/v1/approvals*` routes.
pub fn router() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route("/v1/vast/account", get(account))
        .route("/v1/vast/gpu-names", get(gpu_names))
        .route("/v1/vast/offers/search", post(search_offers))
        .route("/v1/vast/instances", get(list_instances).post(rent))
        .route("/v1/vast/instances/{id}", axum::routing::delete(destroy))
        .route("/v1/vast/instances/{id}/log", get(instance_log))
        .route(
            "/v1/vast/instances/{id}/restart-download",
            post(restart_stalled_download),
        )
        .route(
            "/v1/vast/instances/{id}/tunnel",
            post(tunnel_up).delete(tunnel_down),
        )
        .route("/v1/vast/instances/{id}/park", post(park_instance))
        .route("/v1/vast/instances/{id}/wake", post(wake_instance))
        .route("/v1/vast/instances/{id}/diagnose", get(diagnose_instance))
        .route("/v1/tunnels", get(list_tunnels))
        .route("/v1/vast/favorites", get(list_favorites))
        .route(
            "/v1/vast/favorites/{machine_id}",
            axum::routing::put(put_favorite).delete(delete_favorite),
        )
        .route("/v1/approvals", get(list_approvals))
        .route("/v1/approvals/{id}/grant", post(grant_approval))
        .route("/v1/approvals/{id}/deny", post(deny_approval))
}

// ----------------------------------------------------------------------------------------
// wire types this unit owns
// ----------------------------------------------------------------------------------------

/// What a rental will cost, what the ceiling is, and what the credit is.
///
/// Returned in the `409` body beside a normal `ErrorEnvelope`, which is what makes
/// "confirm this" a *renderable* refusal rather than a sentence a GUI has to parse.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RentPreview {
    /// What is being asked for, in words.
    pub what: String,
    /// The specific offer, when one was named.
    #[serde(default)]
    pub offer_id: Option<u64>,
    /// The search profile, when renting `--auto`.
    #[serde(default)]
    pub profile: Option<ProfileId>,
    /// The ceiling the caller asked to be allowed to spend.
    pub max_usd_per_hour: f64,
    /// The daemon-side hard cap from `[providers.vast] max_usd_per_hour_ceiling`.
    pub ceiling_usd_per_hour: f64,
    /// The bill a human is actually approving, with its assumption attached.
    pub est_total: CostEstimate,
    /// Credit remaining, when we could read it. `None` means "we could not ask" — being
    /// offline never invents a number.
    #[serde(default)]
    pub credit: Option<f64>,
    /// How many hours the credit buys at this rate.
    #[serde(default)]
    pub burn_down_hours: Option<f64>,
    /// What the request is missing, e.g. `["confirm", "max_usd_per_hour"]`.
    #[serde(default)]
    pub needs: Vec<String>,
}

/// A `409` that carries both the standard error body and the preview that explains it.
///
/// A superset of [`ErrorEnvelope`]: a client that deserialises only `error` is unaffected,
/// which is why the money refusal does not need its own error type on every surface.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RentRefusal {
    /// The standard refusal.
    pub error: ErrorBody,
    /// The numbers behind it.
    pub preview: RentPreview,
}

/// What `DELETE /v1/vast/instances/{id}` refuses with, when `?confirm=true` is missing.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DestroyRefusal {
    /// The standard refusal.
    pub error: ErrorBody,
    /// What is about to be destroyed, and what it has cost so far.
    pub instance: Box<VastInstance>,
    /// Accrued cost, with its provenance.
    pub accrued: CostEstimate,
    /// How long it has been billing.
    #[serde(default)]
    pub uptime_secs: Option<f64>,
}

/// `POST /v1/vast/offers/search` takes either a whole query or the id of a saved profile.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum OfferSearchBody {
    /// `{"profile": "rtx3090"}` — the saved market query.
    Profile {
        /// Which profile.
        profile: ProfileId,
        /// An explicit offer to re-check **by constraints**. When it is not in the
        /// profile's price-ordered window, it is fetched by id and judged against the
        /// profile's actual floors — membership in a top-N-by-price list is not a
        /// constraint, and treating it as one made a compliant machine unrentable
        /// (GARDEN-RUNS.md, R4).
        #[serde(default)]
        offer_id: Option<u64>,
        /// Scope the search to one physical machine (`{"machine_id": {"eq": N}}` upstream).
        /// Machine ids are stable where ask ids re-mint; this is how a ★ host is re-found.
        #[serde(default)]
        machine_id: Option<u64>,
    },
    /// A whole [`OfferQuery`], for the browser's re-query button.
    Query(Box<OfferQuery>),
}

/// `?confirm=`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ConfirmQuery {
    /// Money moves only when this is `true`.
    #[serde(default)]
    pub confirm: Option<bool>,
}

/// `?source=&approval=` on `POST /v1/vast/instances`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct RentQuery {
    /// Which surface is asking: `cli`, `web_ui`, `slint_ui`, `mcp`, `api` (the default).
    /// `mcp` is the one that `require_human_confirm` gates.
    #[serde(default)]
    pub source: Option<String>,
    /// The approval a human already granted, for an `mcp` call.
    #[serde(default)]
    pub approval: Option<String>,
    /// Return the `JobRecord` immediately. A rental is always a job; this is accepted for
    /// symmetry and changes nothing.
    #[serde(default)]
    pub no_wait: Option<bool>,
}

/// `?tail=&follow=` on the instance log.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct LogQuery {
    /// How many lines. Defaults to 200.
    #[serde(default)]
    pub tail: Option<u32>,
    /// `1` streams new lines as SSE instead of returning text.
    #[serde(default)]
    pub follow: Option<u8>,
}

/// `?remote_port=&local_port=` on the tunnel verb.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct TunnelQuery {
    /// The port inside the container. Defaults to 8000.
    #[serde(default)]
    pub remote_port: Option<u16>,
    /// Pin the local port instead of taking the next free one from the 8800 pool.
    #[serde(default)]
    pub local_port: Option<u16>,
}

// ----------------------------------------------------------------------------------------
// the fleet and the market
// ----------------------------------------------------------------------------------------

/// `GET /v1/vast/account` — credit, balance, `can_pay`. **Never** the api key: the type has
/// no field for one, so it cannot leak through this route.
pub async fn account(State(_s): State<Arc<AppState>>) -> ApiResult<VastAccount> {
    let api = require_vast()?;
    Ok(Json(api.account().await.map_err(ApiError::from)?))
}

/// `GET /v1/vast/gpu-names` — the **live** `gpu_name` vocabulary for the dropdown.
///
/// From a broad search, never a compiled-in constant: `docs/port/00c` proves those strings
/// change, and a stale constant is how a profile stops matching anything.
pub async fn gpu_names(State(_s): State<Arc<AppState>>) -> ApiResult<Vec<String>> {
    let api = require_vast()?;
    Ok(Json(
        gpu_name_vocabulary(api.as_ref())
            .await
            .map_err(ApiError::from)?,
    ))
}

/// `POST /v1/vast/offers/search` — the market, through **one** query builder.
///
/// A saved profile and the browser's own query go through the same path, so "auto —
/// cheapest" can never rent out of a stricter candidate set than the operator was shown.
/// Any relaxation comes back in `relaxations`, which every surface renders as a banner.
pub async fn search_offers(
    State(s): State<Arc<AppState>>,
    Json(body): Json<OfferSearchBody>,
) -> ApiResult<OfferSearchResult> {
    let api = require_vast()?;
    let result = match body {
        OfferSearchBody::Query(q) => api.search(&q).await.map_err(ApiError::from)?,
        OfferSearchBody::Profile {
            profile,
            offer_id,
            machine_id,
        } => {
            let paths = s.paths.clone();
            let wanted = profile.clone();
            let found = tokio::task::spawn_blocking(move || catalog::load(&paths))
                .await
                .map_err(|e| ApiError::internal(format!("the catalog read panicked: {e}")))?
                .map_err(ApiError::from)?
                .profiles
                .into_iter()
                .find(|p| p.id == wanted)
                .ok_or_else(|| {
                    ApiError::not_found(format!("no search profile `{profile}`"))
                        .with_param("profile")
                })?;
            let mut overrides = QueryOverrides::default();
            if let Some(m) = machine_id {
                overrides
                    .extra
                    .insert("machine_id".to_owned(), serde_json::json!({"eq": m}));
            }
            let mut result = search_unified(api.as_ref(), &found, &overrides)
                .await
                .map_err(ApiError::from)?;
            if let Some(oid) = offer_id {
                if !result.offers.iter().any(|o| o.id == oid) {
                    let strict = profile_to_query(&found, &overrides);
                    let offer = fetch_offer_by_id(api.as_ref(), oid).await?;
                    let fails = constraint_failures(&strict, &offer);
                    if fails.is_empty() {
                        // The only thing "wrong" with it was losing a price contest.
                        result.relaxations.push(format!(
                            "offer {oid} is outside `{profile}`'s top-{} by price; every \
                             constraint checks out",
                            strict.limit
                        ));
                        result.offers.push(offer);
                    } else {
                        return Err(ApiError::conflict(format!(
                            "offer {oid} does not satisfy `{profile}`: {}",
                            fails.join("; ")
                        ))
                        .with_param("offer_id"));
                    }
                }
            }
            apply_favorites(&s, &mut result, offer_id.is_some() || machine_id.is_some());
            result
        }
    };
    Ok(Json(result))
}

/// Fold the operator's host verdicts into a search result.
///
/// On an anonymous profile search, offers on a burned machine are **dropped** with a
/// banner naming them — "auto — cheapest" must never rent the containerd-corpse host that
/// relists at the lowest price (GARDEN-RUNS.md, ☠ offer 45761361). When the operator
/// *named* an offer or pinned a machine, nothing is dropped: their explicit choice wins,
/// with the verdict echoed as a warning banner instead.
fn apply_favorites(s: &Arc<AppState>, result: &mut OfferSearchResult, explicit: bool) {
    let favorites = s.store.load_favorites().unwrap_or_default();
    let burned: HashMap<u64, &FavoriteHost> = favorites
        .iter()
        .filter(|f| f.verdict == FavoriteVerdict::Skull)
        .map(|f| (f.machine_id, f))
        .collect();
    if burned.is_empty() {
        return;
    }
    if explicit {
        for o in &result.offers {
            if let Some(f) = o.machine_id.and_then(|m| burned.get(&m)) {
                result.relaxations.push(format!(
                    "offer {} is on burned machine {}{} — renting it anyway is your call",
                    o.id,
                    f.machine_id,
                    f.note
                        .as_deref()
                        .map(|n| format!(" ({n})"))
                        .unwrap_or_default()
                ));
            }
        }
        return;
    }
    let mut excluded: Vec<u64> = Vec::new();
    result.offers.retain(|o| match o.machine_id {
        Some(m) if burned.contains_key(&m) => {
            excluded.push(m);
            false
        }
        _ => true,
    });
    if !excluded.is_empty() {
        excluded.sort_unstable();
        excluded.dedup();
        result.relaxations.push(format!(
            "excluded {} offer(s) on burned machine(s) {excluded:?}",
            excluded.len()
        ));
    }
}

/// One offer by id, through the same search endpoint (`{"id": {"eq": N}}` upstream).
///
/// A miss is a `404` that explains the re-mint behaviour: several hosts re-issue ask ids
/// per search snapshot, so "gone" usually means "renamed", and `--auto` or `--machine`
/// is the answer, not a retry of the dead id.
async fn fetch_offer_by_id(
    api: &dyn VastApi,
    offer_id: u64,
) -> Result<apexrouter_protocol::Offer, ApiError> {
    let mut probe = OfferQuery {
        gpu_names: Vec::new(),
        num_gpus_min: 1,
        num_gpus_max: 64,
        max_dph: None,
        min_reliability: None,
        min_inet_down: None,
        min_disk_gb: None,
        min_cuda: None,
        geo: apexrouter_protocol::GeoFilter::Any,
        verified: None,
        limit: 4,
        order: Vec::new(),
        extra: serde_json::Map::new(),
    };
    probe
        .extra
        .insert("id".to_owned(), serde_json::json!({"eq": offer_id}));
    api.search(&probe)
        .await
        .map_err(ApiError::from)?
        .offers
        .into_iter()
        .find(|o| o.id == offer_id)
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "offer {offer_id} no longer exists — several hosts re-mint ask ids per \
                 search snapshot; re-run the search, rent with `--auto`, or pin the host \
                 with `--machine <machine_id>`"
            ))
            .with_param("offer_id")
        })
}

/// `GET /v1/vast/instances` — the whole rented fleet, plus a credit re-read broadcast to
/// every surface.
pub async fn list_instances(State(s): State<Arc<AppState>>) -> ApiResult<Vec<VastInstance>> {
    let api = require_vast()?;
    let instances = api.instances().await.map_err(ApiError::from)?;
    let credit = api.account().await.ok().map(|a| a.credit);
    // A fresh read feeds the snapshot's cache too — the Fleet & cost page must never show
    // less than the surface that asked.
    s.update_fleet(instances.clone(), credit);
    super::emit(
        &s,
        Event::VastFleetChanged {
            instances: instances.clone(),
            credit,
        },
    );
    Ok(Json(instances))
}

// ----------------------------------------------------------------------------------------
// the money gate
// ----------------------------------------------------------------------------------------

/// `POST /v1/vast/instances` — rent a box. **409 without `{confirm:true,
/// max_usd_per_hour}`**, and the 409 body carries the cost preview and the current credit.
///
/// Answers `202` with a [`JobRecord`]: renting is a multi-minute boot machine and blocking
/// an HTTP request on it would be a lie about what is happening.
pub async fn rent(
    State(s): State<Arc<AppState>>,
    Query(q): Query<RentQuery>,
    Json(req): Json<RentRequest>,
) -> Result<Response, ApiError> {
    let cfg = s.cfg();

    // The client is resolved but not *required* yet: a malformed money request is a fact
    // about the request, and answering `503 no client` to it would hide the `409` a GUI
    // needs to render its confirm dialog. Free and read-only, and the only thing asked of
    // vast before the gate: the credit a human is entitled to see beside the number they
    // are approving.
    let api = vast_api();
    let credit = match api.as_ref() {
        Some(api) => api.account().await.ok().map(|a| a.credit),
        None => None,
    };

    // ---- step 1: shape ------------------------------------------------------------------
    let mut needs = Vec::new();
    if !req.confirm {
        needs.push("confirm".to_owned());
    }
    if !req.max_usd_per_hour.is_finite() || req.max_usd_per_hour <= 0.0 {
        needs.push("max_usd_per_hour".to_owned());
    }
    if !needs.is_empty() {
        return Ok(refuse_rent(
            &cfg.vast,
            &req,
            credit,
            needs,
            "confirmation_required",
            "renting costs money: re-send with `confirm: true` and a positive \
             `max_usd_per_hour`",
            None,
        ));
    }

    // ---- step 2: approval ---------------------------------------------------------------
    let source = approval_source(&s, &q);
    let approval = match SpendApproval::confirm(
        Money::from_usd(req.max_usd_per_hour),
        source,
        &cfg.vast,
        credit,
    ) {
        Ok(a) => a,
        Err(e) => {
            let (kind, pending) = match &e {
                ApprovalError::AboveCeiling { .. } => ("above_ceiling", None),
                ApprovalError::InsufficientCredit { .. } => ("insufficient_credit", None),
                ApprovalError::HumanConfirmationRequired { pending } => {
                    // Park it where `GET /v1/approvals` and `apexrouter approvals grant`
                    // will find it, so the agent's caller has something to act on.
                    write_approval(&s, *pending, &req, credit, &q);
                    ("approval_required", Some(*pending))
                }
            };
            return Ok(refuse_rent(
                &cfg.vast,
                &req,
                credit,
                Vec::new(),
                kind,
                e.to_string(),
                pending,
            ));
        }
    };

    // ---- step 3: reserve, then bill -----------------------------------------------------
    let api = match api {
        Some(api) => api,
        None => return Err(no_vast_client()),
    };
    let ledger = Ledger::open(&s.paths).map_err(ApiError::from)?;
    let state = Arc::clone(&s);
    let max_boot = cfg.vast.max_boot_secs;
    let job = s.jobs.spawn_with("vast.rent", move |h| async move {
        h.progress(Some(5.0), "reserving before billing");
        let id = apexrouter_providers::vast::rent(api.as_ref(), &ledger, &req, approval, &state.tx)
            .await?;
        h.progress(Some(30.0), format!("instance {} created; booting", id.0));
        let phase = watch_boot(api.as_ref(), id, max_boot, &state.tx).await?;
        if phase != apexrouter_protocol::BootPhase::Healthy {
            crate::fleet::poll_once(&state).await;
            h.progress(Some(100.0), format!("{phase:?}"));
            return Ok(serde_json::json!({ "instance_id": id, "phase": phase }));
        }

        // ---- the box is up; finish the chain the request asked for ---------------------
        //
        // `auto_tunnel` and `bind_alias` were write-only for a full release: the job ended
        // right here and the operator hand-ran tunnel/backend/route while a healthy box
        // billed (GARDEN-RUNS.md, R4 "rentals never feed daemon state"). Every step below
        // acts against a billing machine, so a failure alerts with the manual command and
        // fails the job loudly — the box itself is deliberately left alone.
        let mut tunnel: Option<TunnelSpec> = None;
        if req.auto_tunnel && !req.launch.expose_public {
            h.progress(Some(60.0), "opening the ssh tunnel");
            match ensure_tunnel(&state, api.as_ref(), id, None, None).await {
                Ok(status) => tunnel = Some(status.spec),
                Err(e) => {
                    let why = e.body.message.clone();
                    let fix = format!("apexrouter tunnel up {}", id.0);
                    state.alert(
                        AlertLevel::Serious,
                        &format!("vast.rent.tunnel.{}", id.0),
                        format!(
                            "instance {} is healthy and BILLING but its tunnel failed \
                             ({why}); run `{fix}`",
                            id.0
                        ),
                    );
                    anyhow::bail!(
                        "instance {} is up (and billing) but the tunnel failed: {why}; \
                         run `{fix}` and bind the route by hand",
                        id.0
                    );
                }
            }
        }

        h.progress(Some(80.0), "registering the backend");
        let inst = api.instance(id).await?.ok_or_else(|| {
            anyhow::anyhow!("instance {} vanished right after becoming healthy", id.0)
        })?;
        let spec = apexrouter_protocol::VastSpec {
            instance_id: id,
            runtime: req.launch.runtime,
            launch: req.launch.clone(),
            tunnel,
        };
        let local_port = spec.tunnel.as_ref().map(|t| t.local_port);
        let backend = apexrouter_providers::vast::rented_backend(&inst, &spec)?;
        let backend_id = backend.id.clone();
        let base_url = backend.base_url.clone();
        state
            .store
            .put_endpoint(&apexrouter_protocol::EndpointRecord {
                id: backend_id.clone(),
                spec: apexrouter_protocol::EndpointSpec::Vast(spec),
                desired: apexrouter_protocol::DesiredState::Running,
                proc: None,
                port: local_port,
                log_path: None,
                started_at_unix: now_unix(),
                fit: None,
                adopted: false,
                alias_bindings: req.bind_alias.iter().cloned().collect(),
            })?;
        // `register_started`, not `register_backend`: a fresh rental must arm the
        // registry entry even if a drained corpse once held the same id.
        crate::api::register_started(&state, backend);

        let mut bound = None;
        if let Some(alias) = &req.bind_alias {
            h.progress(Some(95.0), format!("binding alias `{alias}`"));
            match crate::api::bind_alias(&state, alias, &backend_id) {
                Ok(_) => bound = Some(alias.clone()),
                Err(report) => {
                    let why = crate::api::render_issues(&report);
                    state.alert(
                        AlertLevel::Serious,
                        &format!("vast.rent.alias.{}", id.0),
                        format!(
                            "instance {} is routable as `{backend_id}` but binding alias \
                             `{alias}` failed: {why}",
                            id.0
                        ),
                    );
                    anyhow::bail!(
                        "instance {} is routable as `{backend_id}` but binding alias \
                         `{alias}` failed: {why}; fix with `apexrouter route set {alias} \
                         --target {backend_id}`",
                        id.0
                    );
                }
            }
        }

        crate::fleet::poll_once(&state).await;
        h.progress(Some(100.0), "healthy and wired");
        Ok::<_, anyhow::Error>(serde_json::json!({
            "instance_id": id,
            "phase": phase,
            "backend": backend_id,
            "base_url": base_url,
            "tunnel_local_port": local_port,
            "alias": bound,
        }))
    });
    Ok((StatusCode::ACCEPTED, Json(job)).into_response())
}

/// `DELETE /v1/vast/instances/{id}?confirm=true` — destroy, **verify**, then forget.
///
/// Without `?confirm=true` this is a `409` carrying the instance and what it has cost so
/// far. With it: `DELETE`, then poll until the instance is gone or terminal, and only then
/// append the `Destroyed` ledger row. A row written before the box is actually gone is how a
/// fleet view starts lying about money.
pub async fn destroy(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<ConfirmQuery>,
) -> Result<Response, ApiError> {
    let api = require_vast()?;
    let id = parse_instance(&id)?;
    let instance = api
        .instance(id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::not_found(format!("no instance {}", id.0)).with_param("id"))?;

    let uptime_secs = instance.uptime_secs();
    let accrued = accrued_cost(&instance);

    if !q.confirm.unwrap_or(false) {
        return Ok((
            StatusCode::CONFLICT,
            Json(DestroyRefusal {
                error: ErrorBody {
                    kind: "confirmation_required".to_owned(),
                    message: format!(
                        "destroying instance {} is not reversible: re-send with ?confirm=true",
                        id.0
                    ),
                    param: Some("confirm".to_owned()),
                    code: None,
                },
                instance: Box::new(instance),
                accrued,
                uptime_secs,
            }),
        )
            .into_response());
    }

    api.destroy(id).await.map_err(ApiError::from)?;

    // Verify before forgetting.
    let poll = Duration::from_millis(s.cfg().vast.poll_min_ms.max(POLL_FLOOR_MS));
    let deadline = std::time::Instant::now() + Duration::from_secs(DESTROY_VERIFY_SECS);
    let mut gone = false;
    while std::time::Instant::now() < deadline {
        match api.instance(id).await {
            Ok(None) => {
                gone = true;
                break;
            }
            Ok(Some(now)) if now.is_terminal() => {
                gone = true;
                break;
            }
            // An unreachable API is not evidence that a box is gone. Keep asking.
            _ => tokio::time::sleep(poll).await,
        }
    }

    if let Ok(ledger) = Ledger::open(&s.paths) {
        let _ = ledger.append(&LedgerRow {
            seq: 0,
            at_unix: now_unix(),
            instance_id: Some(id),
            state: if gone {
                LedgerState::Destroyed
            } else {
                LedgerState::DestroyRequested
            },
            offer_id: None,
            profile: None,
            gpu: None,
            num_gpus: None,
            dph: None,
            approved_max_dph: None,
            approval_source: None,
            destroyed_at_unix: gone.then(now_unix),
            est_cost: accrued.clone(),
            note: Some(if gone {
                "destroy verified".to_owned()
            } else {
                "destroy issued; the instance had not disappeared before the deadline".to_owned()
            }),
        });
    }

    if !gone {
        s.alert(
            AlertLevel::Serious,
            &format!("vast.destroy.{}", id.0),
            format!(
                "instance {} was asked to destroy but was still listed {DESTROY_VERIFY_SECS}s \
                 later — check the vast console before assuming it stopped billing",
                id.0
            ),
        );
    }
    let fleet = api.instances().await.unwrap_or_default();
    let credit = api.account().await.ok().map(|a| a.credit);
    s.update_fleet(fleet.clone(), credit);
    super::emit(
        &s,
        Event::VastFleetChanged {
            instances: fleet,
            credit,
        },
    );

    Ok(Json(serde_json::json!({
        "instance_id": id,
        "destroyed": gone,
        "accrued": accrued,
    }))
    .into_response())
}

// ----------------------------------------------------------------------------------------
// one instance
// ----------------------------------------------------------------------------------------

/// `GET /v1/vast/instances/{id}/log?tail=&follow=1` — text, or SSE when following.
///
/// The two-phase `result_url` poll lives in P-02's client; this is the follow loop on top of
/// it, which never polls faster than `[providers.vast] poll_min_ms` because vast publishes
/// no rate-limit headers at all.
pub async fn instance_log(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<LogQuery>,
) -> Result<Response, ApiError> {
    let api = require_vast()?;
    let id = parse_instance(&id)?;
    let tail = q.tail.unwrap_or(DEFAULT_TAIL);

    if q.follow.unwrap_or(0) == 0 {
        let lines = api.logs(id, tail).await.map_err(ApiError::from)?;
        return Ok(lines.join("\n").into_response());
    }

    let poll = Duration::from_millis(s.cfg().vast.poll_min_ms.max(POLL_FLOOR_MS));
    let stream =
        futures_util::stream::unfold((api, 0usize, true), move |(api, seen, first)| async move {
            loop {
                let lines = api.logs(id, tail).await.unwrap_or_default();
                if lines.len() > seen {
                    let fresh = lines[seen..].join("\n");
                    return Some((
                        Ok::<_, Infallible>(SseEvent::default().event("log").data(fresh)),
                        (api, lines.len(), false),
                    ));
                }
                if first && lines.is_empty() {
                    // Say something immediately rather than leaving a UI on a spinner.
                    return Some((
                        Ok(SseEvent::default().event("log").data("(no output yet)")),
                        (api, 0, false),
                    ));
                }
                tokio::time::sleep(poll).await;
            }
        });
    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response())
}

/// `POST /v1/vast/instances/{id}/restart-download` — the one-click stall recovery.
///
/// Answers with a **fresh** [`DownloadHealth`] sample rather than `{"ok":true}`: the
/// question the operator has is "is it moving now?", and the only honest answer is another
/// measurement.
pub async fn restart_stalled_download(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<DownloadHealth> {
    let api = require_vast()?;
    let id = parse_instance(&id)?;
    let (host, port) = ssh_endpoint(api.as_ref(), id).await?;
    let opts = ssh_opts(&s, id);

    restart_download(&opts, &host, port)
        .await
        .map_err(ApiError::from)?;
    let health = sample_download(&opts, &host, port)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(health))
}

/// `GET /v1/vast/instances/{id}/diagnose` — the four SSH probes plus the RX sample.
///
/// Runs the registry with `CheckCtx::instance` set, selecting the `ssh.*` checks and
/// `net.stall`. Anything that cannot meet its precondition comes back `Skipped`, never
/// `Fail` — a box that has not finished booting is not a defect.
pub async fn diagnose_instance(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<super::checks::OnlyQuery>,
) -> ApiResult<Vec<CheckResult>> {
    let id = parse_instance(&id)?;
    let ctx = super::checks::check_ctx(&s, Some(id), HashMap::new()).await;

    let mut out = Vec::new();
    let patterns: Vec<String> = match q.only {
        Some(only) => vec![only],
        None => vec!["ssh".to_owned(), "net.stall".to_owned()],
    };
    for pattern in patterns {
        for result in super::checks::run_checks(&s, &ctx, Some(&pattern)).await {
            if !out.iter().any(|r: &CheckResult| r.id == result.id) {
                out.push(result);
            }
        }
    }
    Ok(Json(out))
}

// ----------------------------------------------------------------------------------------
// tunnels
// ----------------------------------------------------------------------------------------

/// `GET /v1/tunnels` — every persisted forward.
///
/// Served from `$STATE/tunnels.json` rather than from the supervisor, so it answers even
/// when no supervisor is installed and so a daemon restart shows what it is about to
/// re-adopt.
pub async fn list_tunnels(State(s): State<Arc<AppState>>) -> ApiResult<Vec<TunnelStatus>> {
    Ok(Json(s.store.load_tunnels().map_err(ApiError::from)?))
}

/// `POST /v1/vast/instances/{id}/tunnel` — bring up `ssh -N -L` to the container.
pub async fn tunnel_up(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<TunnelQuery>,
) -> ApiResult<TunnelStatus> {
    let api = require_vast()?;
    let id = parse_instance(&id)?;
    Ok(Json(
        ensure_tunnel(&s, api.as_ref(), id, q.local_port, q.remote_port).await?,
    ))
}

/// Bring the instance's tunnel up (idempotent) and persist it — the **one** implementation
/// behind the `POST …/tunnel` route and the rent job's `auto_tunnel` step.
pub(crate) async fn ensure_tunnel(
    s: &Arc<AppState>,
    api: &dyn VastApi,
    id: InstanceId,
    local_port: Option<u16>,
    remote_port: Option<u16>,
) -> Result<TunnelStatus, ApiError> {
    let tunnels = require_tunnels()?;
    let (host, port) = ssh_endpoint(api, id).await?;

    let existing = s.store.load_tunnels().map_err(ApiError::from)?;
    if let Some(up) = existing.iter().find(|t| t.spec.instance_id == id) {
        if up.up {
            return Ok(up.clone());
        }
    }

    let spec = TunnelSpec {
        instance_id: id,
        local_port: match local_port {
            Some(p) => p,
            None => next_local_port(s, &existing)?,
        },
        remote_port: remote_port.unwrap_or(CONTAINER_PORT),
        ssh_host: host,
        ssh_port: port,
    };
    let status = tunnels.up(spec).await.map_err(ApiError::from)?;

    let mut all: Vec<TunnelStatus> = existing
        .into_iter()
        .filter(|t| t.spec.instance_id != id)
        .collect();
    all.push(status.clone());
    s.store.save_tunnels(&all).map_err(ApiError::from)?;
    Ok(status)
}

/// `DELETE /v1/vast/instances/{id}/tunnel` — kill the child, `ssh -O exit`, unlink the
/// ControlPath. Idempotent.
pub async fn tunnel_down(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Vec<TunnelStatus>> {
    let tunnels = require_tunnels()?;
    let id = parse_instance(&id)?;
    tunnels.down(id).await.map_err(ApiError::from)?;
    let all: Vec<TunnelStatus> = s
        .store
        .load_tunnels()
        .map_err(ApiError::from)?
        .into_iter()
        .filter(|t| t.spec.instance_id != id)
        .collect();
    s.store.save_tunnels(&all).map_err(ApiError::from)?;
    Ok(Json(all))
}

// ----------------------------------------------------------------------------------------
// park and wake
// ----------------------------------------------------------------------------------------

/// `POST /v1/vast/instances/{id}/park` — stop the box, keep the disk.
///
/// No `confirm` gate: parking *reduces* spend, and the stop button must never be hard to
/// find. The answer carries the weekly disk figure so every surface can render what the
/// held disk keeps costing.
pub async fn park_instance(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let api = require_vast()?;
    let id = parse_instance(&id)?;
    let ledger = Ledger::open(&s.paths).map_err(ApiError::from)?;
    let cfg = s.cfg();

    let parked = apexrouter_providers::vast::park(
        api.as_ref(),
        &ledger,
        id,
        &s.tx,
        DESTROY_VERIFY_SECS,
        cfg.vast.poll_min_ms,
    )
    .await
    .map_err(ApiError::from)?;

    crate::fleet::poll_once(&s).await;
    Ok(Json(serde_json::json!({
        "instance_id": id,
        "phase": parked.phase(),
        "disk_gb": parked.disk_space,
        "weekly_disk_usd": apexrouter_providers::vast::weekly_disk_usd(&parked),
    }))
    .into_response())
}

/// `POST /v1/vast/instances/{id}/wake?confirm=true` — restart a parked box.
///
/// **Resumes the hourly bill**, so it is gated exactly like a rent: without
/// `?confirm=true` the answer is a `409` carrying the instance's rate and the credit; with
/// it, the instance's live `dph_total` goes through `SpendApproval::confirm` (daemon
/// ceiling, human gate, credit) before any state changes. Answers `202` with a job — a
/// wake is a boot, and boots take minutes.
pub async fn wake_instance(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<RentQuery>,
    Query(c): Query<ConfirmQuery>,
) -> Result<Response, ApiError> {
    let api = require_vast()?;
    let id = parse_instance(&id)?;
    let cfg = s.cfg();
    let instance = api
        .instance(id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::not_found(format!("no instance {}", id.0)).with_param("id"))?;
    let dph = instance
        .dph_total
        .unwrap_or(cfg.vast.max_usd_per_hour_ceiling);
    let credit = api.account().await.ok().map(|a| a.credit);

    if !c.confirm.unwrap_or(false) {
        return Ok((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": ErrorBody {
                    kind: "confirmation_required".to_owned(),
                    message: format!(
                        "waking instance {} resumes billing at ${dph:.4}/hr — re-send \
                         with ?confirm=true",
                        id.0
                    ),
                    param: Some("confirm".to_owned()),
                    code: None,
                },
                "instance_id": id,
                "dph_total": instance.dph_total,
                "credit": credit,
                "burn_down_hours": credit.filter(|_| dph > 0.0).map(|c| c / dph),
            })),
        )
            .into_response());
    }

    let source = approval_source(&s, &q);
    let approval = SpendApproval::confirm(Money::from_usd(dph), source, &cfg.vast, credit)
        .map_err(|e| ApiError::conflict(e.to_string()))?;

    let ledger = Ledger::open(&s.paths).map_err(ApiError::from)?;
    let state = Arc::clone(&s);
    let max_boot = cfg.vast.max_boot_secs;
    let poll_min = cfg.vast.poll_min_ms;
    let job = s.jobs.spawn_with("vast.wake", move |h| async move {
        h.progress(Some(10.0), format!("starting instance {}", id.0));
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

        // The persisted tunnel, if this box had one, needs its forward re-established
        // against the (possibly re-mapped) ssh port.
        let mut local_port = None;
        let had_tunnel = state
            .store
            .load_tunnels()
            .ok()
            .and_then(|all| all.into_iter().find(|t| t.spec.instance_id == id));
        if let Some(t) = had_tunnel {
            h.progress(Some(80.0), "re-establishing the tunnel");
            match ensure_tunnel(&state, api.as_ref(), id, Some(t.spec.local_port), None).await {
                Ok(status) => local_port = Some(status.spec.local_port),
                Err(e) => {
                    let why = e.body.message.clone();
                    state.alert(
                        AlertLevel::Serious,
                        &format!("vast.wake.tunnel.{}", id.0),
                        format!(
                            "instance {} is awake and BILLING but its tunnel failed \
                             ({why}); run `apexrouter tunnel up {}`",
                            id.0, id.0
                        ),
                    );
                    anyhow::bail!(
                        "instance {} is awake (and billing) but the tunnel failed: {why}",
                        id.0
                    );
                }
            }
        }

        crate::fleet::poll_once(&state).await;
        h.progress(Some(100.0), "awake");
        Ok::<_, anyhow::Error>(serde_json::json!({
            "instance_id": id,
            "phase": apexrouter_protocol::BootPhase::Healthy,
            "tunnel_local_port": local_port,
        }))
    });
    Ok((StatusCode::ACCEPTED, Json(job)).into_response())
}

// ----------------------------------------------------------------------------------------
// favorites
// ----------------------------------------------------------------------------------------

/// Body of `PUT /v1/vast/favorites/{machine_id}`.
#[derive(Clone, Debug, Deserialize)]
pub struct FavoriteBody {
    /// Star or skull.
    pub verdict: FavoriteVerdict,
    /// Why, in the operator's words.
    #[serde(default)]
    pub note: Option<String>,
    /// GPU model, for recognisability in the list.
    #[serde(default)]
    pub gpu_name: Option<String>,
    /// Location, same reason.
    #[serde(default)]
    pub geolocation: Option<String>,
}

/// `GET /v1/vast/favorites` — every remembered host verdict, from `$STATE/favorites.json`.
pub async fn list_favorites(State(s): State<Arc<AppState>>) -> ApiResult<Vec<FavoriteHost>> {
    Ok(Json(s.store.load_favorites().map_err(ApiError::from)?))
}

/// `PUT /v1/vast/favorites/{machine_id}` — record or update one verdict. Idempotent upsert:
/// a machine that turned out to be a lemon overwrites its old star.
pub async fn put_favorite(
    State(s): State<Arc<AppState>>,
    Path(machine_id): Path<u64>,
    Json(body): Json<FavoriteBody>,
) -> ApiResult<FavoriteHost> {
    let row = FavoriteHost {
        machine_id,
        verdict: body.verdict,
        note: body.note,
        gpu_name: body.gpu_name,
        geolocation: body.geolocation,
        added_at_unix: now_unix(),
    };
    let mut all = s.store.load_favorites().map_err(ApiError::from)?;
    all.retain(|f| f.machine_id != machine_id);
    all.push(row.clone());
    all.sort_by_key(|f| f.machine_id);
    s.store.save_favorites(&all).map_err(ApiError::from)?;
    Ok(Json(row))
}

/// `DELETE /v1/vast/favorites/{machine_id}` — forget one verdict. Idempotent.
pub async fn delete_favorite(
    State(s): State<Arc<AppState>>,
    Path(machine_id): Path<u64>,
) -> ApiResult<Vec<FavoriteHost>> {
    let mut all = s.store.load_favorites().map_err(ApiError::from)?;
    all.retain(|f| f.machine_id != machine_id);
    s.store.save_favorites(&all).map_err(ApiError::from)?;
    Ok(Json(all))
}

// ----------------------------------------------------------------------------------------
// approvals
// ----------------------------------------------------------------------------------------

/// `GET /v1/approvals` — every approval still waiting on a human, oldest first.
pub async fn list_approvals(State(s): State<Arc<AppState>>) -> ApiResult<Vec<ApprovalRequest>> {
    let mut out = Vec::new();
    let dir = s.paths.approvals_dir();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match s.store.read_json::<ApprovalRequest>(&path) {
                Ok(Some(req)) if !decided(&s, req.id) => out.push(req),
                Ok(_) => {}
                Err(e) => tracing::warn!(path = %path.display(), error = %e,
                    "skipping unreadable approval"),
            }
        }
    }
    out.sort_by_key(|a| a.requested_at_unix);
    Ok(Json(out))
}

/// `POST /v1/approvals/{id}/grant` — a human clears one pending spend.
pub async fn grant_approval(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<ApprovalRequest> {
    decide(&s, &id, true)
}

/// `POST /v1/approvals/{id}/deny` — a human refuses one pending spend.
pub async fn deny_approval(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<ApprovalRequest> {
    decide(&s, &id, false)
}

/// Whether a human has granted this approval. `false` for one that was denied, decided
/// against, or never existed — the safe direction in every case.
pub fn approval_granted(s: &Arc<AppState>, id: JobId) -> bool {
    s.paths
        .approvals_dir()
        .join(format!("{id}.granted"))
        .exists()
}

// ----------------------------------------------------------------------------------------
// the injected clients
// ----------------------------------------------------------------------------------------

/// The tunnel operations the control plane needs, object-safe.
///
/// Mirrors P-05's `TunnelSupervisor`, which publishes no constructor and cannot be held in
/// `AppState` (S-01's file). The trait exists so this module can hold one and so a test can
/// substitute a fake without an `ssh` binary anywhere near it.
pub trait Tunnels: Send + Sync {
    /// Bring one forward up.
    fn up(&self, spec: TunnelSpec)
        -> futures_util::future::BoxFuture<'_, CoreResult<TunnelStatus>>;
    /// Kill the child, run `ssh -O exit`, unlink the ControlPath.
    fn down(&self, id: InstanceId) -> futures_util::future::BoxFuture<'_, CoreResult<()>>;
}

/// `apexrouter_core::error::Result`, spelled once.
type CoreResult<T> = apexrouter_core::error::Result<T>;

impl Tunnels for apexrouter_providers::ssh::TunnelSupervisor {
    fn up(
        &self,
        spec: TunnelSpec,
    ) -> futures_util::future::BoxFuture<'_, CoreResult<TunnelStatus>> {
        Box::pin(async move { apexrouter_providers::ssh::TunnelSupervisor::up(self, spec).await })
    }

    fn down(&self, id: InstanceId) -> futures_util::future::BoxFuture<'_, CoreResult<()>> {
        Box::pin(async move { apexrouter_providers::ssh::TunnelSupervisor::down(self, id).await })
    }
}

/// Install the vast client this control plane should use. `None` removes it.
///
/// Called once at daemon start, and by nothing else. Process-global for the same reason
/// [`super::shutdown_notify`] is.
pub fn install_vast_api(api: Option<Arc<dyn VastApi>>) {
    if let Ok(mut slot) = vast_slot().write() {
        *slot = api;
    }
}

/// The installed vast client, if there is one.
pub fn vast_api() -> Option<Arc<dyn VastApi>> {
    vast_slot().read().ok().and_then(|s| s.clone())
}

/// Install the tunnel supervisor. `None` removes it.
pub fn install_tunnels(t: Option<Arc<dyn Tunnels>>) {
    if let Ok(mut slot) = tunnel_slot().write() {
        *slot = t;
    }
}

/// The installed tunnel supervisor, if there is one.
pub fn tunnels() -> Option<Arc<dyn Tunnels>> {
    tunnel_slot().read().ok().and_then(|s| s.clone())
}

// ----------------------------------------------------------------------------------------
// internals
// ----------------------------------------------------------------------------------------

/// Where [`vast_api`] reads from.
fn vast_slot() -> &'static RwLock<Option<Arc<dyn VastApi>>> {
    static SLOT: OnceLock<RwLock<Option<Arc<dyn VastApi>>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(None))
}

/// Where [`tunnels`] reads from.
fn tunnel_slot() -> &'static RwLock<Option<Arc<dyn Tunnels>>> {
    static SLOT: OnceLock<RwLock<Option<Arc<dyn Tunnels>>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(None))
}

/// The vast client, or a `503` that names the credential that is probably missing.
fn require_vast() -> Result<Arc<dyn VastApi>, ApiError> {
    vast_api().ok_or_else(no_vast_client)
}

/// The `503` for "there is no vast client in this daemon".
fn no_vast_client() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "vast_unavailable",
        "this daemon has no vast.ai client: set a key with \
         `apexrouter provider set vast --key-stdin`, or `$VAST_API_KEY`, and restart",
    )
}

/// The tunnel supervisor, or a `503`.
fn require_tunnels() -> Result<Arc<dyn Tunnels>, ApiError> {
    tunnels().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "tunnels_unavailable",
            "this daemon has no ssh tunnel supervisor installed",
        )
    })
}

/// Parse an instance id out of a path segment.
fn parse_instance(raw: &str) -> Result<InstanceId, ApiError> {
    raw.trim().parse::<u64>().map(InstanceId).map_err(|_| {
        ApiError::bad_request(
            "bad_id",
            format!("`{raw}` is not an instance id; vast contract ids are integers"),
        )
        .with_param("id")
    })
}

/// Which surface asked, from `?source=`. Anything unrecognised is [`ApprovalSource::Api`],
/// which is the *stricter* reading for everything except `mcp` and cannot be used to dodge
/// the human gate.
fn approval_source(s: &Arc<AppState>, q: &RentQuery) -> ApprovalSource {
    match q.source.as_deref().map(str::trim) {
        Some("cli") => ApprovalSource::Cli,
        Some("web_ui") => ApprovalSource::WebUi,
        Some("slint_ui") => ApprovalSource::SlintUi,
        Some("mcp") => ApprovalSource::Mcp {
            human_cleared: q
                .approval
                .as_deref()
                .and_then(|a| a.parse::<ulid::Ulid>().ok())
                .map(JobId)
                .is_some_and(|id| approval_granted(s, id)),
        },
        _ => ApprovalSource::Api,
    }
}

/// Build the `409` body: a normal error envelope, plus the numbers behind it.
fn refuse_rent(
    vast: &apexrouter_core::config::VastCfg,
    req: &RentRequest,
    credit: Option<f64>,
    needs: Vec<String>,
    kind: &str,
    message: impl Into<String>,
    pending: Option<JobId>,
) -> Response {
    let rate = req.max_usd_per_hour.max(0.0);
    let est_total = if rate > 0.0 {
        CostEstimate::Approximate {
            usd: Money::from_usd(rate * PREVIEW_HOURS),
            source: PriceSource::VastOffer,
            assumption: format!(
                "{PREVIEW_HOURS:.0} hour at the approved ceiling of ${rate:.4}/hr; \
                 the offer that is actually rented may be cheaper"
            ),
        }
    } else {
        CostEstimate::Unknown
    };
    let preview = RentPreview {
        what: match (req.offer_id, req.profile.as_ref()) {
            (Some(offer), _) => format!("rent offer {offer}"),
            (None, Some(p)) => format!("rent the cheapest offer matching profile `{p}`"),
            (None, None) => "rent a vast.ai instance".to_owned(),
        },
        offer_id: req.offer_id,
        profile: req.profile.clone(),
        max_usd_per_hour: rate,
        ceiling_usd_per_hour: vast.max_usd_per_hour_ceiling,
        est_total,
        credit,
        burn_down_hours: credit.filter(|_| rate > 0.0).map(|c| c / rate),
        needs,
    };
    (
        StatusCode::CONFLICT,
        Json(RentRefusal {
            error: ErrorBody {
                kind: kind.to_owned(),
                message: message.into(),
                param: Some("confirm".to_owned()),
                code: pending.map(|p| p.to_string()),
            },
            preview,
        }),
    )
        .into_response()
}

/// Park a pending approval where a human can grant it.
fn write_approval(
    s: &Arc<AppState>,
    pending: JobId,
    req: &RentRequest,
    credit: Option<f64>,
    q: &RentQuery,
) {
    let record = ApprovalRequest {
        id: pending,
        what: match (req.offer_id, req.profile.as_ref()) {
            (Some(offer), _) => format!("rent vast.ai offer {offer}"),
            (None, Some(p)) => format!("rent the cheapest offer matching profile `{p}`"),
            (None, None) => "rent a vast.ai instance".to_owned(),
        },
        max_usd_per_hour: req.max_usd_per_hour,
        est_total_usd: req.max_usd_per_hour * PREVIEW_HOURS,
        credit,
        requested_at_unix: now_unix(),
        source: q.source.clone().unwrap_or_else(|| "api".to_owned()),
    };
    let path = s.paths.approvals_dir().join(format!("{pending}.json"));
    if let Err(e) = s.store.write_json(&path, &record) {
        tracing::warn!(error = %e, "could not park the pending approval");
        return;
    }
    s.alert(
        AlertLevel::Warning,
        &format!("approval.{pending}"),
        format!(
            "{} needs a human: `apexrouter approvals grant {pending}`",
            record.what
        ),
    );
}

/// Whether an approval has already been granted or denied.
fn decided(s: &Arc<AppState>, id: JobId) -> bool {
    let dir = s.paths.approvals_dir();
    dir.join(format!("{id}.granted")).exists() || dir.join(format!("{id}.denied")).exists()
}

/// Grant or deny one approval, leaving a marker beside the request.
fn decide(s: &Arc<AppState>, raw: &str, grant: bool) -> ApiResult<ApprovalRequest> {
    let id = raw.parse::<ulid::Ulid>().map(JobId).map_err(|_| {
        ApiError::bad_request("bad_id", format!("`{raw}` is not an approval id")).with_param("id")
    })?;
    let dir = s.paths.approvals_dir();
    let request: ApprovalRequest = s
        .store
        .read_json(&dir.join(format!("{id}.json")))
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::not_found(format!("no pending approval {id}")).with_param("id"))?;
    if decided(s, id) {
        return Err(ApiError::conflict(format!(
            "approval {id} was already decided"
        )));
    }
    let marker = dir.join(format!("{id}.{}", if grant { "granted" } else { "denied" }));
    s.store
        .write_atomic(&marker, format!("{}\n", now_unix()).as_bytes(), 0o600)
        .map_err(ApiError::from)?;
    s.alert(
        AlertLevel::Info,
        &format!("approval.{id}"),
        format!(
            "{} was {} by a human",
            request.what,
            if grant { "granted" } else { "denied" }
        ),
    );
    Ok(Json(request))
}

/// What one instance has cost so far, from its own `dph_total` and uptime.
fn accrued_cost(i: &VastInstance) -> CostEstimate {
    match (i.dph_total, i.uptime_secs()) {
        (Some(dph), Some(secs)) if dph.is_finite() && secs.is_finite() => {
            CostEstimate::Approximate {
                usd: Money::from_usd(dph * secs / 3600.0),
                source: PriceSource::VastOffer,
                assumption: format!(
                    "${dph:.4}/hr x {:.2} h of uptime; vast bills storage and bandwidth \
                     separately",
                    secs / 3600.0
                ),
            }
        }
        _ => CostEstimate::Unknown,
    }
}

/// The instance's ssh endpoint, or a `409` explaining that it is not up yet.
///
/// **Direct `host:port` first, the `sshN.vast.ai` proxy as the fallback.** Every create
/// requests `runtype ssh_direc ssh_proxy`, so most boxes map container port 22 to a direct
/// host port — and on some hosts the proxy's reverse listener simply never binds (endless
/// port-29448 failures on the R4 box; GARDEN-RUNS.md calls direct-port the CN doctrine).
/// The proxy is a shared bastion; the direct port is the box itself.
async fn ssh_endpoint(api: &dyn VastApi, id: InstanceId) -> Result<(String, u16), ApiError> {
    let instance = api
        .instance(id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::not_found(format!("no instance {}", id.0)).with_param("id"))?;
    ssh_target(&instance).ok_or_else(|| {
        ApiError::conflict(format!(
            "instance {} has no ssh endpoint yet (status {})",
            id.0,
            instance.actual_status.as_deref().unwrap_or("unknown")
        ))
    })
}

/// Direct `ip:port` when the row maps container port 22, the proxy otherwise.
fn ssh_target(instance: &VastInstance) -> Option<(String, u16)> {
    if let Some((host, port)) = instance.external_port(22) {
        tracing::debug!(
            instance = instance.id.0,
            host,
            port,
            "ssh via the direct port"
        );
        return Some((host, port));
    }
    match (instance.ssh_host.clone(), instance.ssh_port) {
        (Some(host), Some(port)) => Some((host, port)),
        _ => None,
    }
}

/// The ssh options every remote call in this module uses: the dedicated `known_hosts`
/// (vast recycles `sshN.vast.ai` hostnames) and the per-instance ControlMaster socket.
fn ssh_opts(s: &Arc<AppState>, id: InstanceId) -> SshOpts {
    SshOpts {
        known_hosts: s.paths.known_hosts(),
        control_path: Some(s.paths.control_path(id)),
        connect_timeout: 10,
        extra: Vec::new(),
    }
}

/// The next free local port in `[providers.vast] tunnel_port_range`.
fn next_local_port(s: &Arc<AppState>, taken: &[TunnelStatus]) -> Result<u16, ApiError> {
    let (lo, hi) = s.cfg().vast.tunnel_port_range;
    (lo..=hi)
        .find(|p| !taken.iter().any(|t| t.spec.local_port == *p))
        .ok_or_else(|| {
            ApiError::conflict(format!(
                "every local port in {lo}..={hi} is already forwarding something"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::checks::serve_s07;
    use crate::api::testkit::{app, test_config};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The client slot is process-global, so tests that install one serialise here.
    fn slot_lock() -> &'static tokio::sync::Mutex<()> {
        static L: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        L.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    /// A vast.ai that never touches the network.
    ///
    /// **`create` and `destroy` panic**: this unit's tests must be structurally unable to
    /// reach a billing call, and a counter that is merely asserted to be zero would not
    /// prove that. Anything that would spend money fails the test loudly instead.
    struct FakeVast {
        credit: f64,
        account_calls: Arc<AtomicUsize>,
    }

    impl FakeVast {
        fn new(credit: f64) -> FakeVast {
            FakeVast {
                credit,
                account_calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    /// `VastApi` is declared with `#[async_trait]` and this crate does not link
    /// `async-trait`, so every method is written in the macro's desugared form: the same
    /// `'life*`/`'async_trait` parameters it generates, and a `Pin<Box<dyn Future + Send>>`
    /// return. A test double is not worth a new dependency.
    impl VastApi for FakeVast {
        fn account<'life0, 'async_trait>(
            &'life0 self,
        ) -> Pin<Box<dyn Future<Output = CoreResult<VastAccount>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            self.account_calls.fetch_add(1, Ordering::SeqCst);
            let credit = self.credit;
            Box::pin(async move {
                Ok(VastAccount {
                    id: 1,
                    credit,
                    balance: Some(credit),
                    can_pay: Some(true),
                    has_billing: Some(true),
                })
            })
        }

        fn search<'life0, 'life1, 'async_trait>(
            &'life0 self,
            _q: &'life1 OfferQuery,
        ) -> Pin<Box<dyn Future<Output = CoreResult<OfferSearchResult>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move {
                Ok(OfferSearchResult {
                    offers: Vec::new(),
                    relaxations: vec!["widened: geo dropped".to_owned()],
                    queried_at_unix: 0,
                    gpu_name_vocabulary: vec!["RTX 3090".to_owned()],
                })
            })
        }

        fn create<'life0, 'life1, 'life2, 'async_trait>(
            &'life0 self,
            _offer_id: u64,
            _launch: &'life1 apexrouter_protocol::ContainerLaunch,
            _label: &'life2 str,
        ) -> Pin<Box<dyn Future<Output = CoreResult<InstanceId>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            'life2: 'async_trait,
            Self: 'async_trait,
        {
            panic!("MONEY: no test in this unit may reach a create call");
        }

        fn instances<'life0, 'async_trait>(
            &'life0 self,
        ) -> Pin<Box<dyn Future<Output = CoreResult<Vec<VastInstance>>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move { Ok(Vec::new()) })
        }

        fn instance<'life0, 'async_trait>(
            &'life0 self,
            _id: InstanceId,
        ) -> Pin<Box<dyn Future<Output = CoreResult<Option<VastInstance>>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move { Ok(None) })
        }

        fn set_target_state<'life0, 'async_trait>(
            &'life0 self,
            _id: InstanceId,
            _running: bool,
        ) -> Pin<Box<dyn Future<Output = CoreResult<()>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            panic!("MONEY: no test in this unit may reach a park/wake call");
        }

        fn destroy<'life0, 'async_trait>(
            &'life0 self,
            _id: InstanceId,
        ) -> Pin<Box<dyn Future<Output = CoreResult<()>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            panic!("MONEY: no test in this unit may reach a destroy call");
        }

        fn logs<'life0, 'async_trait>(
            &'life0 self,
            _id: InstanceId,
            _tail: u32,
        ) -> Pin<Box<dyn Future<Output = CoreResult<Vec<String>>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move { Ok(vec!["booting".to_owned()]) })
        }

        fn exec<'life0, 'life1, 'async_trait>(
            &'life0 self,
            _id: InstanceId,
            _cmd: &'life1 str,
        ) -> Pin<Box<dyn Future<Output = CoreResult<String>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move { Ok(String::new()) })
        }
    }

    fn rent_body(confirm: bool, max_usd_per_hour: f64) -> serde_json::Value {
        serde_json::json!({
            "offer_id": 12345,
            "launch": {
                "runtime": "llama_cpp",
                "image": "ghcr.io/buckster123/vastai-gguf:prebuilt",
                "image_type": "prebuilt",
                "disk_gb": 80,
                "env": {},
                "onstart": "bash /app/launch.sh > /var/log/launch.log 2>&1 &",
                "host": "127.0.0.1",
                "port": 8000,
                "expose_public": false
            },
            "confirm": confirm,
            "max_usd_per_hour": max_usd_per_hour,
            "auto_tunnel": true
        })
    }

    /// THE acceptance sentence: no confirmation, `409`, and the body carries the preview and
    /// the credit.
    #[tokio::test]
    async fn renting_without_confirmation_is_a_409_carrying_the_preview_and_the_credit() {
        let _guard = slot_lock().lock().await;
        install_vast_api(Some(Arc::new(FakeVast::new(7.73))));

        let state = app(test_config());
        let base = serve_s07(Arc::clone(&state)).await;
        let res = reqwest::Client::new()
            .post(format!("{base}/v1/vast/instances"))
            .json(&rent_body(false, 0.40))
            .send()
            .await
            .expect("post");

        assert_eq!(res.status(), 409);
        let body: RentRefusal = res.json().await.expect("RentRefusal");
        assert_eq!(body.error.kind, "confirmation_required");
        assert_eq!(body.preview.needs, vec!["confirm".to_owned()]);
        assert_eq!(body.preview.credit, Some(7.73), "the credit is in the body");
        assert_eq!(body.preview.offer_id, Some(12345));
        assert_eq!(
            body.preview.ceiling_usd_per_hour, 4.0,
            "the shipped ceiling"
        );
        match body.preview.est_total {
            CostEstimate::Approximate {
                usd, assumption, ..
            } => {
                assert_eq!(usd, Money::from_usd(0.40), "one hour at the ceiling");
                assert!(assumption.contains("hour"), "{assumption}");
            }
            other => panic!("a preview must carry a number and its assumption: {other:?}"),
        }
        assert!(
            body.preview
                .burn_down_hours
                .is_some_and(|h| (h - 7.73 / 0.40).abs() < 1e-6),
            "{:?}",
            body.preview.burn_down_hours
        );

        install_vast_api(None);
    }

    /// The other half of the shape gate: `confirm: true` with no ceiling is still a `409`.
    #[tokio::test]
    async fn confirm_without_a_positive_ceiling_is_still_refused() {
        let _guard = slot_lock().lock().await;
        install_vast_api(Some(Arc::new(FakeVast::new(7.73))));

        let state = app(test_config());
        let base = serve_s07(Arc::clone(&state)).await;
        let res = reqwest::Client::new()
            .post(format!("{base}/v1/vast/instances"))
            .json(&rent_body(true, 0.0))
            .send()
            .await
            .expect("post");

        assert_eq!(res.status(), 409);
        let body: RentRefusal = res.json().await.expect("RentRefusal");
        assert_eq!(body.preview.needs, vec!["max_usd_per_hour".to_owned()]);
        assert_eq!(body.preview.est_total, CostEstimate::Unknown);

        install_vast_api(None);
    }

    /// The daemon-side hard cap: an agent filling in a big number still cannot spend more
    /// than the human configured, and the refusal is renderable.
    #[tokio::test]
    async fn a_request_above_the_ceiling_is_refused_with_the_ceiling_in_the_body() {
        let _guard = slot_lock().lock().await;
        install_vast_api(Some(Arc::new(FakeVast::new(1000.0))));

        let state = app(test_config());
        let base = serve_s07(Arc::clone(&state)).await;
        let res = reqwest::Client::new()
            .post(format!("{base}/v1/vast/instances"))
            .json(&rent_body(true, 9.00))
            .send()
            .await
            .expect("post");

        assert_eq!(res.status(), 409);
        let body: RentRefusal = res.json().await.expect("RentRefusal");
        assert_eq!(body.error.kind, "above_ceiling");
        assert_eq!(body.preview.ceiling_usd_per_hour, 4.0);
        assert!(
            body.error.message.contains("ceiling"),
            "{}",
            body.error.message
        );

        install_vast_api(None);
    }

    #[tokio::test]
    async fn credit_that_cannot_cover_an_hour_is_refused() {
        let _guard = slot_lock().lock().await;
        install_vast_api(Some(Arc::new(FakeVast::new(0.10))));

        let state = app(test_config());
        let base = serve_s07(Arc::clone(&state)).await;
        let res = reqwest::Client::new()
            .post(format!("{base}/v1/vast/instances"))
            .json(&rent_body(true, 3.34))
            .send()
            .await
            .expect("post");

        assert_eq!(res.status(), 409);
        let body: RentRefusal = res.json().await.expect("RentRefusal");
        assert_eq!(body.error.kind, "insufficient_credit");
        assert_eq!(body.preview.credit, Some(0.10));

        install_vast_api(None);
    }

    /// An agent that has not been cleared parks an approval a human can act on, and the
    /// grant/deny verbs move it.
    #[tokio::test]
    async fn an_agent_request_parks_an_approval_that_a_human_grants() {
        let _guard = slot_lock().lock().await;
        install_vast_api(Some(Arc::new(FakeVast::new(7.73))));

        let mut cfg = test_config();
        cfg.vast.require_human_confirm = true;
        let state = app(cfg);
        let base = serve_s07(Arc::clone(&state)).await;
        let http = reqwest::Client::new();

        let res = http
            .post(format!("{base}/v1/vast/instances?source=mcp"))
            .json(&rent_body(true, 0.40))
            .send()
            .await
            .expect("post");
        assert_eq!(res.status(), 409);
        let body: RentRefusal = res.json().await.expect("RentRefusal");
        assert_eq!(body.error.kind, "approval_required");
        let pending = body.error.code.clone().expect("the pending id is carried");

        let listed: Vec<ApprovalRequest> = http
            .get(format!("{base}/v1/approvals"))
            .send()
            .await
            .expect("get")
            .json()
            .await
            .expect("Vec<ApprovalRequest>");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id.to_string(), pending);
        assert_eq!(listed[0].credit, Some(7.73));

        let granted: ApprovalRequest = http
            .post(format!("{base}/v1/approvals/{pending}/grant"))
            .send()
            .await
            .expect("post")
            .json()
            .await
            .expect("ApprovalRequest");
        assert_eq!(granted.id.to_string(), pending);

        // Granted approvals leave the pending list, and a second decision is a conflict.
        let listed: Vec<ApprovalRequest> = http
            .get(format!("{base}/v1/approvals"))
            .send()
            .await
            .expect("get")
            .json()
            .await
            .expect("Vec<ApprovalRequest>");
        assert!(listed.is_empty());
        let again = http
            .post(format!("{base}/v1/approvals/{pending}/deny"))
            .send()
            .await
            .expect("post");
        assert_eq!(again.status(), 409);

        install_vast_api(None);
    }

    #[tokio::test]
    async fn the_free_read_only_routes_answer_from_the_client() {
        let _guard = slot_lock().lock().await;
        install_vast_api(Some(Arc::new(FakeVast::new(7.73))));

        let state = app(test_config());
        let base = serve_s07(Arc::clone(&state)).await;
        let http = reqwest::Client::new();

        let acct: VastAccount = http
            .get(format!("{base}/v1/vast/account"))
            .send()
            .await
            .expect("get")
            .json()
            .await
            .expect("VastAccount");
        assert_eq!(acct.credit, 7.73);
        let raw = serde_json::to_string(&acct).expect("ser");
        assert!(
            !raw.contains("api_key"),
            "the key can never be echoed: {raw}"
        );

        let fleet: Vec<VastInstance> = http
            .get(format!("{base}/v1/vast/instances"))
            .send()
            .await
            .expect("get")
            .json()
            .await
            .expect("Vec<VastInstance>");
        assert!(fleet.is_empty());

        let found: OfferSearchResult = http
            .post(format!("{base}/v1/vast/offers/search"))
            .json(&serde_json::json!({
                "gpu_names": ["RTX 3090"], "num_gpus_min": 1, "num_gpus_max": 2,
                "geo": "any", "limit": 8
            }))
            .send()
            .await
            .expect("post")
            .json()
            .await
            .expect("OfferSearchResult");
        assert_eq!(
            found.relaxations,
            vec!["widened: geo dropped".to_owned()],
            "a relaxation always travels with the result"
        );

        install_vast_api(None);
    }

    #[tokio::test]
    async fn without_a_client_the_money_routes_are_503_and_tunnels_still_answer() {
        let _guard = slot_lock().lock().await;
        install_vast_api(None);
        install_tunnels(None);

        let state = app(test_config());
        let base = serve_s07(Arc::clone(&state)).await;
        let http = reqwest::Client::new();

        let res = http
            .post(format!("{base}/v1/vast/instances"))
            .json(&rent_body(true, 0.40))
            .send()
            .await
            .expect("post");
        assert_eq!(res.status(), 503);
        let body: ErrorEnvelope = res.json().await.expect("envelope");
        assert_eq!(body.error.kind, "vast_unavailable");

        // `GET /v1/tunnels` reads `$STATE`, so it works with no supervisor at all.
        let tunnels: Vec<TunnelStatus> = http
            .get(format!("{base}/v1/tunnels"))
            .send()
            .await
            .expect("get")
            .json()
            .await
            .expect("Vec<TunnelStatus>");
        assert!(tunnels.is_empty());
    }

    #[tokio::test]
    async fn an_instance_id_that_is_not_an_integer_is_a_400() {
        let _guard = slot_lock().lock().await;
        install_vast_api(Some(Arc::new(FakeVast::new(7.73))));
        let state = app(test_config());
        let base = serve_s07(Arc::clone(&state)).await;

        let res = reqwest::Client::new()
            .delete(format!("{base}/v1/vast/instances/not-a-number"))
            .send()
            .await
            .expect("delete");
        assert_eq!(res.status(), 400);
        let body: ErrorEnvelope = res.json().await.expect("envelope");
        assert_eq!(body.error.kind, "bad_id");

        install_vast_api(None);
    }

    #[test]
    fn accrued_cost_is_unknown_rather_than_zero_when_vast_told_us_nothing() {
        let bare: VastInstance = serde_json::from_str(r#"{"id": 1}"#).expect("de");
        assert_eq!(accrued_cost(&bare), CostEstimate::Unknown);

        let billing: VastInstance =
            serde_json::from_str(r#"{"id": 1, "dph_total": 0.36, "start_date": 1000000000.0}"#)
                .expect("de");
        assert!(
            matches!(accrued_cost(&billing), CostEstimate::Approximate { .. }),
            "a derived number is always labelled as one"
        );
    }

    #[test]
    fn ssh_prefers_the_direct_port_and_falls_back_to_the_proxy() {
        // The R4 box: proxy reverse listener never bound; `ssh -p 4013 root@129.204.7.16`
        // (the 22/tcp mapping) is what actually worked. Direct wins when mapped.
        let both: VastInstance = serde_json::from_value(serde_json::json!({
            "id": 46509449,
            "ssh_host": "ssh5.vast.ai", "ssh_port": 41234,
            "public_ipaddr": "129.204.7.16",
            "ports": {"22/tcp": [{"HostIp": "0.0.0.0", "HostPort": "4013"}]}
        }))
        .expect("de");
        assert_eq!(ssh_target(&both), Some(("129.204.7.16".to_owned(), 4013)));

        // No direct mapping: the proxy path still works.
        let proxy_only: VastInstance = serde_json::from_value(serde_json::json!({
            "id": 1, "ssh_host": "ssh5.vast.ai", "ssh_port": 41234
        }))
        .expect("de");
        assert_eq!(
            ssh_target(&proxy_only),
            Some(("ssh5.vast.ai".to_owned(), 41234))
        );

        // Neither yet: the caller renders "not up yet", not a panic.
        let bare: VastInstance = serde_json::from_str(r#"{"id": 1}"#).expect("de");
        assert_eq!(ssh_target(&bare), None);
    }
}
