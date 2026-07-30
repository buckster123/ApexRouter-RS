//! OWNER: unit S-04 (server/src/api/{rig,fit,catalog,usage,requests,jobs}.rs,
//! server/src/jobs.rs). Do not edit outside that unit.
//!
//! The `/v1/recipes*` and `/v1/profiles*` CRUD sets — this is "dynamic recipe building in
//! the GUI".
//!
//! A [`Recipe`] is the saved *result* of a discovery session, not a hand-written tier: the 71
//! rows of `recipes.toml` are replaced by discovery plus saved drafts, and every recipe
//! carries a [`Provenance2`](apexrouter_protocol::Provenance2) so "this is stale" is
//! answerable without guessing. That is why `POST /v1/recipes/{id}/validate` exists as a
//! first-class verb: a recipe pointing at a model that has since been deleted, or at a build
//! that no longer exists, is *normal*, and the fix belongs in the response rather than in a
//! launch failure ninety seconds later.
//!
//! Every write goes through `core::catalog`, which round-trips `catalog.toml` with
//! `toml_edit` so a hand-written comment survives a GUI save. That is blocking I/O behind a
//! file lock, so every call here is wrapped in `spawn_blocking`.

use crate::api::rig::{local_model_list, rig_snapshot};
use crate::api::{bind_alias, register_backend, ApiError, ApiResult};
use crate::state::AppState;
use apexrouter_core::catalog;
use apexrouter_core::error::Result as CoreResult;
use apexrouter_core::paths::Paths;
use apexrouter_protocol::{
    Alias, Backend, EndpointRecord, EndpointSpec, ProfileId, Recipe, RecipeId, RecipeKind,
    SearchProfile, ValidationReport,
};
use apexrouter_providers::Provisioner;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post, put};
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;

/// `POST /v1/recipes/{id}/instantiate?alias=&no_wait=&force=`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct InstantiateQuery {
    /// Bind this alias to whatever comes up.
    #[serde(default)]
    pub alias: Option<String>,
    /// Return a `JobRecord` immediately instead of waiting for the health gate.
    #[serde(default)]
    pub no_wait: Option<bool>,
    /// Skip the VRAM admission refusal, and nothing else.
    #[serde(default)]
    pub force: Option<bool>,
}

/// `POST /v1/recipes/from-endpoint/{id}?label=`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct FromEndpointQuery {
    /// Human label for the new recipe. Defaults to the endpoint id.
    #[serde(default)]
    pub label: Option<String>,
}

/// The `/v1/recipes*` and `/v1/profiles*` routes.
pub fn router() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route("/v1/recipes", get(list_recipes).post(create_recipe))
        .route("/v1/recipes/from-endpoint/{id}", post(recipe_from_endpoint))
        .route("/v1/recipes/{id}", get(get_recipe))
        .route("/v1/recipes/{id}", put(put_recipe))
        .route("/v1/recipes/{id}", delete(delete_recipe))
        .route("/v1/recipes/{id}/validate", post(validate))
        .route("/v1/recipes/{id}/instantiate", post(instantiate))
        .route("/v1/profiles", get(list_profiles).post(create_profile))
        .route("/v1/profiles/{id}", get(get_profile))
        .route("/v1/profiles/{id}", put(put_profile))
        .route("/v1/profiles/{id}", delete(delete_profile))
}

// ----------------------------------------------------------------------------------------
// recipes
// ----------------------------------------------------------------------------------------

/// `GET /v1/recipes`.
pub async fn list_recipes(State(s): State<Arc<AppState>>) -> ApiResult<Vec<Recipe>> {
    Ok(Json(load(&s).await?.recipes))
}

/// `POST /v1/recipes` — save a draft. The id is generated from the label; a client-supplied
/// id is honoured only when it names a recipe that already exists.
pub async fn create_recipe(
    State(s): State<Arc<AppState>>,
    Json(r): Json<Recipe>,
) -> ApiResult<Recipe> {
    Ok(Json(
        blocking(&s, move |p| catalog::upsert_recipe(&p, r)).await?,
    ))
}

/// `GET /v1/recipes/{id}`.
pub async fn get_recipe(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Recipe> {
    let want = recipe_id(&id)?;
    load(&s)
        .await?
        .recipes
        .into_iter()
        .find(|r| r.id == want)
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("no recipe {id}")).with_param("id"))
}

/// `PUT /v1/recipes/{id}` — replace one. The path id wins over the body's, so a copy-pasted
/// draft cannot silently overwrite the recipe it was copied from.
pub async fn put_recipe(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(mut r): Json<Recipe>,
) -> ApiResult<Recipe> {
    let want = recipe_id(&id)?;
    ensure_recipe_exists(&s, &want).await?;
    r.id = want;
    Ok(Json(
        blocking(&s, move |p| catalog::upsert_recipe(&p, r)).await?,
    ))
}

/// `DELETE /v1/recipes/{id}`.
pub async fn delete_recipe(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let want = recipe_id(&id)?;
    ensure_recipe_exists(&s, &want).await?;
    blocking(&s, move |p| catalog::remove_recipe(&p, &want)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/recipes/{id}/validate` — check it against the rig and the weights **now**.
///
/// A stale recipe is not an error, it is a finding: the report names the field, says what is
/// wrong and carries the fix, which is what the GUI renders beside the Launch button.
pub async fn validate(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<ValidationReport> {
    let want = recipe_id(&id)?;
    let recipe = find_recipe(&s, &want).await?;
    let rig = rig_snapshot(&s, false).await?;
    let models = local_model_list(&s, false).await?;
    Ok(Json(catalog::validate_recipe(&recipe, &rig, &models)))
}

/// `POST /v1/recipes/{id}/instantiate` — bring the recipe up.
///
/// Returns an [`EndpointRecord`] when it waited for the health gate, or a `JobRecord` under
/// `?no_wait=true`. The two shapes are why the return type is a `Value`: `ARCHITECTURE.md`
/// §6.2 documents this route as `EndpointRecord | JobRecord`, and inventing a wrapper object
/// would break both clients rather than neither.
///
/// Only the two local kinds are implemented here. `Vast` and `Managed` recipes belong to
/// Stage 5 (P-04 and S-07 own those provisioners) and answer `501` naming the unit, rather
/// than pretending to launch something that would cost money.
pub async fn instantiate(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<InstantiateQuery>,
) -> ApiResult<serde_json::Value> {
    let want = recipe_id(&id)?;
    let recipe = find_recipe(&s, &want).await?;
    let spec = spec_for(&recipe)?;
    let alias = q
        .alias
        .as_deref()
        .map(|a| {
            Alias::parse(a)
                .map_err(|e| ApiError::bad_request("bad_id", e.to_string()).with_param("alias"))
        })
        .transpose()?;
    let force = q.force.unwrap_or(false);

    if q.no_wait.unwrap_or(false) {
        s.jobs.ensure_wired(&s.tx, &s.paths);
        let state = Arc::clone(&s);
        let job = s
            .jobs
            .spawn_with("recipe.instantiate", move |h| async move {
                h.progress(Some(5.0), "planning");
                // `ApiError` is a `Response`, not a `std::error::Error`, so it is rendered
                // into the job row's `error` string here rather than through `?`.
                match launch(&state, spec, alias, force).await {
                    Ok(rec) => Ok(rec),
                    Err(e) => Err(anyhow::anyhow!("{}", e.body.message)),
                }
            });
        return Ok(Json(serde_json::to_value(job).map_err(|e| {
            ApiError::internal(format!("job record would not serialise: {e}"))
        })?));
    }

    let rec = launch(&s, spec, alias, force).await?;
    Ok(Json(serde_json::to_value(rec).map_err(|e| {
        ApiError::internal(format!("endpoint record would not serialise: {e}"))
    })?))
}

/// `POST /v1/recipes/from-endpoint/{id}` — "save this running thing as a recipe".
pub async fn recipe_from_endpoint(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<FromEndpointQuery>,
) -> ApiResult<Recipe> {
    let records = s.store.list_endpoints().map_err(ApiError::from)?;
    let rec = records
        .into_iter()
        .find(|r| r.id.as_str() == id)
        .ok_or_else(|| ApiError::not_found(format!("no endpoint {id}")).with_param("id"))?;
    let label = q.label.unwrap_or_else(|| rec.id.to_string());
    let draft = catalog::recipe_from_endpoint(&rec, &label);
    Ok(Json(
        blocking(&s, move |p| catalog::upsert_recipe(&p, draft)).await?,
    ))
}

// ----------------------------------------------------------------------------------------
// search profiles
// ----------------------------------------------------------------------------------------

/// `GET /v1/profiles`.
///
/// A machine that has never saved one gets the shipped defaults rather than an empty list:
/// a market query template the user has to invent from scratch is the thing this whole
/// feature exists to remove.
pub async fn list_profiles(State(s): State<Arc<AppState>>) -> ApiResult<Vec<SearchProfile>> {
    let saved = load(&s).await?.profiles;
    Ok(Json(if saved.is_empty() {
        catalog::default_profiles()
    } else {
        saved
    }))
}

/// `POST /v1/profiles`.
pub async fn create_profile(
    State(s): State<Arc<AppState>>,
    Json(p): Json<SearchProfile>,
) -> ApiResult<SearchProfile> {
    Ok(Json(
        blocking(&s, move |paths| catalog::upsert_profile(&paths, p)).await?,
    ))
}

/// `GET /v1/profiles/{id}`.
pub async fn get_profile(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<SearchProfile> {
    let want = profile_id(&id)?;
    let saved = load(&s).await?.profiles;
    let pool = if saved.is_empty() {
        catalog::default_profiles()
    } else {
        saved
    };
    pool.into_iter()
        .find(|p| p.id == want)
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("no search profile {id}")).with_param("id"))
}

/// `PUT /v1/profiles/{id}`.
pub async fn put_profile(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(mut p): Json<SearchProfile>,
) -> ApiResult<SearchProfile> {
    p.id = profile_id(&id)?;
    Ok(Json(
        blocking(&s, move |paths| catalog::upsert_profile(&paths, p)).await?,
    ))
}

/// `DELETE /v1/profiles/{id}`.
pub async fn delete_profile(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let want = profile_id(&id)?;
    let known = load(&s).await?.profiles.iter().any(|p| p.id == want);
    if !known {
        return Err(ApiError::not_found(format!("no search profile {id}")).with_param("id"));
    }
    blocking(&s, move |p| catalog::remove_profile(&p, &want)).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ----------------------------------------------------------------------------------------
// plumbing
// ----------------------------------------------------------------------------------------

/// Plan and start a local endpoint, register it, and bind an alias to it.
///
/// The order is the one the operator needs: the endpoint exists and is healthy **before** an
/// alias points at it, so a client that follows the alias never reaches a port that is still
/// loading weights.
///
/// # Errors
/// Anything the supervisor refuses with, mapped through [`ApiError`]. A failed alias bind is
/// reported *without* tearing the endpoint down — it is up, it works, and its route is one
/// `PUT /v1/routes` away.
async fn launch(
    state: &Arc<AppState>,
    spec: EndpointSpec,
    alias: Option<Alias>,
    force: bool,
) -> Result<EndpointRecord, ApiError> {
    let plan = state.supervisor.plan(&spec).await.map_err(ApiError::from)?;
    for w in &plan.warnings {
        tracing::warn!(warning = %w, "launch plan warning");
    }
    let backend: Backend = state
        .supervisor
        .up_forced(plan, None, force)
        .await
        .map_err(ApiError::from)?;

    register_backend(state, backend.clone());

    if let Some(alias) = alias {
        if let Err(report) = bind_alias(state, &alias, &backend.id) {
            tracing::warn!(
                alias = %alias,
                issues = %crate::api::render_issues(&report),
                "the endpoint is up but the alias could not be bound"
            );
        }
    }

    state
        .store
        .list_endpoints()
        .map_err(ApiError::from)?
        .into_iter()
        .find(|r| r.id == backend.id)
        .ok_or_else(|| {
            ApiError::internal(format!(
                "{} started but no record was written; the state directory may not be writable",
                backend.id
            ))
        })
}

/// The [`EndpointSpec`] a recipe launches.
///
/// # Errors
/// `501` for the two kinds whose provisioners land in Stage 5, naming the work unit so the
/// message is a fact rather than an apology.
fn spec_for(r: &Recipe) -> Result<EndpointSpec, ApiError> {
    match &r.kind {
        RecipeKind::Local(spec) => Ok(EndpointSpec::LocalLlama(spec.clone())),
        RecipeKind::LocalVllm(spec) => Ok(EndpointSpec::LocalVllm(spec.clone())),
        RecipeKind::Vast { .. } => Err(ApiError::not_implemented(
            "vast recipes are instantiated by the vast provisioner (unit P-04, Stage 5); \
             this build has no way to rent a box and will not pretend otherwise",
        )),
        RecipeKind::Managed(_) => Err(ApiError::not_implemented(
            "managed-provider recipes are registered by unit S-07 (Stage 5); \
             use POST /v1/backends to register the provider by URL in the meantime",
        )),
    }
}

/// The whole catalog, read off disk without blocking the runtime.
async fn load(state: &Arc<AppState>) -> Result<catalog::Catalog, ApiError> {
    blocking(state, |p| catalog::load(&p)).await
}

/// One recipe, or a `404` naming it.
async fn find_recipe(state: &Arc<AppState>, id: &RecipeId) -> Result<Recipe, ApiError> {
    load(state)
        .await?
        .recipes
        .into_iter()
        .find(|r| &r.id == id)
        .ok_or_else(|| ApiError::not_found(format!("no recipe {id}")).with_param("id"))
}

/// Refuse a `PUT`/`DELETE` against a recipe that is not there.
///
/// `upsert_recipe` would happily *create* one under a fresh id, which would turn a typo in a
/// URL into a duplicate rather than a `404`.
async fn ensure_recipe_exists(state: &Arc<AppState>, id: &RecipeId) -> Result<(), ApiError> {
    find_recipe(state, id).await.map(|_| ())
}

/// Run a `core::catalog` call — `toml_edit` under a file lock — off the runtime's workers.
async fn blocking<T, F>(state: &Arc<AppState>, f: F) -> Result<T, ApiError>
where
    F: FnOnce(Paths) -> CoreResult<T> + Send + 'static,
    T: Send + 'static,
{
    let paths = state.paths.clone();
    tokio::task::spawn_blocking(move || f(paths))
        .await
        .map_err(|e| ApiError::internal(format!("the catalog task failed: {e}")))?
        .map_err(ApiError::from)
}

/// Parse a recipe id out of a path segment.
fn recipe_id(raw: &str) -> Result<RecipeId, ApiError> {
    RecipeId::parse(raw)
        .map_err(|e| ApiError::bad_request("bad_id", e.to_string()).with_param("id"))
}

/// Parse a profile id out of a path segment.
fn profile_id(raw: &str) -> Result<ProfileId, ApiError> {
    ProfileId::parse(raw)
        .map_err(|e| ApiError::bad_request("bad_id", e.to_string()).with_param("id"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::testkit::{app, test_config};
    use apexrouter_protocol::{
        BuildId, GeoFilter, ImageType, KvType, LocalLlamaSpec, NglPlan, Provenance2, SamplingMode,
        SplitPlan,
    };

    fn local_recipe(label: &str, model_path: &str) -> Recipe {
        Recipe {
            id: RecipeId::parse("draft").expect("id"),
            label: label.to_owned(),
            description: None,
            kind: RecipeKind::Local(LocalLlamaSpec {
                build: BuildId::parse("build-vulkan").expect("build"),
                model_path: model_path.to_owned(),
                mmproj: None,
                alias_flag: "carnice".to_owned(),
                host: "127.0.0.1".to_owned(),
                port: None,
                ctx: Some(16_384),
                parallel: Some(1),
                kv_type: Some(KvType::Q8_0),
                ngl: NglPlan::All,
                split: SplitPlan::default(),
                mode: SamplingMode::Thinking,
                flash_attn: None,
                api_key: None,
                extra_args: vec![],
            }),
            provenance: Provenance2 {
                discovered_at_unix: 0,
                size_bytes: None,
                source: "test".to_owned(),
                fit: None,
            },
            created_at_unix: 0,
            updated_at_unix: 0,
        }
    }

    fn profile(label: &str) -> SearchProfile {
        SearchProfile {
            id: ProfileId::parse("draft").expect("id"),
            label: label.to_owned(),
            gpu_names: vec!["RTX 3090".to_owned()],
            num_gpus_min: 1,
            num_gpus_max: 2,
            max_dph: None,
            min_reliability: 0.98,
            min_inet_down: 500,
            min_disk_gb: 60,
            min_cuda: None,
            geo: GeoFilter::Eu,
            image_type: ImageType::Prebuilt,
            extra: serde_json::Map::new(),
        }
    }

    #[tokio::test]
    async fn a_recipe_round_trips_through_the_crud_set() {
        let state = app(test_config());

        let Json(saved) = create_recipe(
            State(Arc::clone(&state)),
            Json(local_recipe("carnice thinking", "/models/carnice.gguf")),
        )
        .await
        .expect("create");
        assert_eq!(saved.label, "carnice thinking");
        assert!(saved.updated_at_unix > 0, "the write is stamped");

        let Json(all) = list_recipes(State(Arc::clone(&state))).await.expect("list");
        assert_eq!(all.len(), 1);

        let Json(one) = get_recipe(State(Arc::clone(&state)), Path(saved.id.to_string()))
            .await
            .expect("get");
        assert_eq!(one.id, saved.id);

        let mut edited = saved.clone();
        edited.description = Some("edited".to_owned());
        let Json(updated) = put_recipe(
            State(Arc::clone(&state)),
            Path(saved.id.to_string()),
            Json(edited),
        )
        .await
        .expect("put");
        assert_eq!(updated.description.as_deref(), Some("edited"));
        assert_eq!(updated.id, saved.id, "the path id wins");

        let code = delete_recipe(State(Arc::clone(&state)), Path(saved.id.to_string()))
            .await
            .expect("delete");
        assert_eq!(code, StatusCode::NO_CONTENT);
        let Json(all) = list_recipes(State(state)).await.expect("list");
        assert!(all.is_empty());
    }

    /// A `PUT` against an id that does not exist must be a `404`, not a quiet create under a
    /// freshly minted id — that would turn a typo into a duplicate.
    #[tokio::test]
    async fn put_and_delete_refuse_an_unknown_recipe() {
        let state = app(test_config());
        let e = put_recipe(
            State(Arc::clone(&state)),
            Path("nope".to_owned()),
            Json(local_recipe("x", "/models/x.gguf")),
        )
        .await
        .expect_err("no such recipe");
        assert_eq!(e.status, StatusCode::NOT_FOUND);

        let e = delete_recipe(State(state), Path("nope".to_owned()))
            .await
            .expect_err("no such recipe");
        assert_eq!(e.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_malformed_id_is_a_400() {
        let state = app(test_config());
        let e = get_recipe(State(state), Path("NOT A SLUG".to_owned()))
            .await
            .expect_err("bad id");
        assert_eq!(e.status, StatusCode::BAD_REQUEST);
        assert_eq!(e.body.param.as_deref(), Some("id"));
    }

    /// The load-bearing case for the whole feature: a recipe whose weights are gone is
    /// **normal**, and it must produce a report rather than a launch failure later.
    #[tokio::test]
    async fn a_recipe_pointing_at_missing_weights_validates_to_a_finding() {
        let mut cfg = test_config();
        let dir = tempfile::TempDir::new().expect("tempdir");
        cfg.endpoints.model_roots = vec![dir.path().display().to_string()];
        let state = app(cfg);
        crate::api::rig::invalidate_models();
        state
            .supervisor
            .set_rig(apexrouter_protocol::RigSnapshot::default());

        let Json(saved) = create_recipe(
            State(Arc::clone(&state)),
            Json(local_recipe("gone", "/nonexistent/weights.gguf")),
        )
        .await
        .expect("create");

        let Json(report) = validate(State(Arc::clone(&state)), Path(saved.id.to_string()))
            .await
            .expect("validate");
        assert!(!report.issues.is_empty(), "missing weights is a finding");
        assert!(
            report
                .issues
                .iter()
                .any(|i| i.field == "kind.model_path" && i.message.contains("gone")),
            "the missing weights are named: {:?}",
            report.issues
        );
        assert!(
            report.ok,
            "staleness is a Warning with a fix, not an Error: a recipe whose weights were \
             deleted is normal and still launchable once they come back"
        );
        assert!(
            report.issues.iter().all(|i| i.fix.is_some()),
            "every issue carries a fix: {:?}",
            report.issues
        );
        crate::api::rig::invalidate_models();
    }

    #[tokio::test]
    async fn a_vast_recipe_refuses_rather_than_spending_money() {
        let mut r = local_recipe("rented", "/models/x.gguf");
        r.kind = RecipeKind::Vast {
            profile: ProfileId::parse("cheap").expect("id"),
            launch: apexrouter_protocol::ContainerLaunch {
                runtime: apexrouter_protocol::ContainerRuntime::LlamaCpp,
                image: "example/llama:latest".to_owned(),
                image_type: ImageType::Prebuilt,
                disk_gb: 60,
                env: std::collections::BTreeMap::new(),
                onstart: "bash /app/launch.sh > /var/log/launch.log 2>&1 &".to_owned(),
                host: "127.0.0.1".to_owned(),
                port: 8000,
                expose_public: false,
            },
            fit: None,
        };
        let e = spec_for(&r).expect_err("no money is spent in Stage 4");
        assert_eq!(e.status, StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn profiles_round_trip_and_default_when_none_are_saved() {
        let state = app(test_config());
        let Json(defaults) = list_profiles(State(Arc::clone(&state)))
            .await
            .expect("list");
        assert!(
            !defaults.is_empty(),
            "an empty catalog answers with the shipped templates"
        );

        let Json(saved) = create_profile(State(Arc::clone(&state)), Json(profile("cheap eu")))
            .await
            .expect("create");
        let Json(all) = list_profiles(State(Arc::clone(&state)))
            .await
            .expect("list");
        assert_eq!(all.len(), 1, "a saved profile replaces the defaults");

        let Json(one) = get_profile(State(Arc::clone(&state)), Path(saved.id.to_string()))
            .await
            .expect("get");
        assert_eq!(one.label, "cheap eu");

        let mut edited = saved.clone();
        edited.min_reliability = 0.95;
        let Json(updated) = put_profile(
            State(Arc::clone(&state)),
            Path(saved.id.to_string()),
            Json(edited),
        )
        .await
        .expect("put");
        assert!((updated.min_reliability - 0.95).abs() < f32::EPSILON);

        let code = delete_profile(State(Arc::clone(&state)), Path(saved.id.to_string()))
            .await
            .expect("delete");
        assert_eq!(code, StatusCode::NO_CONTENT);
    }

    /// `/v1/recipes/from-endpoint/{id}` and `/v1/recipes/{id}/validate` are both three
    /// segments deep. Building the router proves they do not collide.
    #[test]
    fn the_recipe_routes_do_not_overlap() {
        let _ = router();
    }
}
