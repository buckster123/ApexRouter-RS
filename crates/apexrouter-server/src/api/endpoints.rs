//! OWNER: unit S-03 (server/src/api/{mod,snapshot,backends,routes,endpoints}.rs). Do not edit outside that unit.
//!
//! The `/v1/endpoints*` set, including `argv` and `adopt`.
//!
//! An **endpoint** is a thing ApexRouter knows how to start and stop. This module is the
//! thin control-plane skin over P-01's supervisor: it does not spawn, health-gate, reserve
//! ports or reap anything itself — every one of those is `LocalProvisioner`'s, where the
//! port reservation is held under a per-endpoint lock until the health gate passes and where
//! **the failure path is the stop path**.
//!
//! What this module *does* own is the two things the supervisor deliberately does not know
//! about: putting the resulting `Backend` into the live registry, and binding an alias to it
//! so that `apexrouter up <model> --alias auto` ends with `auto` actually resolving.
//!
//! `POST /v1/endpoints` is a **blocking** call: it returns when the child has passed its
//! health gate, which for a 9B model on an iGPU is tens of seconds. `?no_wait=true` is the
//! house pattern for that — a `202` with a `JobRecord` immediately, and S-04's `JobRegistry`
//! guaranteeing the row is flipped to `Failed` on every error path, including a panic.

use super::{bind_alias, register_backend, register_started, ApiError, ApiResult};
use crate::state::AppState;
use apexrouter_core::argv;
use apexrouter_core::error::Error as CoreError;
use apexrouter_core::proc::Adoption;
use apexrouter_protocol::{
    Alias, ArgvPreview, BackendId, DesiredState, EndpointRecord, EndpointSpec, Event,
};
use apexrouter_providers::local::{adopt, ResolvedSpec};
use apexrouter_providers::{DownMode, Provisioner};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

/// How long `POST /v1/endpoints/{id}/adopt` gives `/props` to answer.
const ADOPT_TIMEOUT: Duration = Duration::from_secs(5);

/// The `/v1/endpoints*` set.
pub fn router() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route("/v1/endpoints", get(list).post(create))
        .route("/v1/endpoints/{id}", get(one).delete(remove))
        .route("/v1/endpoints/{id}/stop", post(stop))
        .route("/v1/endpoints/{id}/restart", post(restart))
        .route("/v1/endpoints/{id}/adopt", post(adopt_one))
        .route("/v1/endpoints/{id}/argv", get(argv))
}

/// `?no_wait=&alias=&force=`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct CreateQuery {
    /// Return a `JobRecord` immediately instead of waiting for the health gate.
    #[serde(default)]
    pub no_wait: Option<bool>,
    /// Bind this alias to the endpoint once it is `Ready`.
    #[serde(default)]
    pub alias: Option<String>,
    /// Skip the VRAM admission refusal — and **only** that one.
    #[serde(default)]
    pub force: Option<bool>,
}

/// `GET /v1/endpoints` — every persisted endpoint record.
///
/// There is deliberately no `status` field on an [`EndpointRecord`]: expectation
/// (`desired`) and observation (`proc`, plus the backend's `Health`) are different things,
/// and collapsing them is how a stopped endpoint ends up displayed as running.
pub async fn list(State(s): State<Arc<AppState>>) -> ApiResult<Vec<EndpointRecord>> {
    Ok(Json(s.store.list_endpoints().map_err(ApiError::from)?))
}

/// `GET /v1/endpoints/{id}`.
pub async fn one(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<EndpointRecord> {
    Ok(Json(record(&s, &id)?))
}

/// `POST /v1/endpoints` — plan, launch, register, and optionally bind an alias.
///
/// The order matters and is not negotiable:
///
/// 1. `plan()` — the build is chosen (a fallback is a **visible warning**, never a silent
///    substitution), the binary and weights are checked, the fit is solved against live
///    VRAM, a port is proposed;
/// 2. `up()` — the port reservation is taken and held until the health gate passes;
/// 3. the resulting `Backend` goes into the registry and the table is recompiled;
/// 4. only then is the alias bound, because binding an alias to a backend that is not yet
///    in the registry is a dangling target and would fail the compile.
pub async fn create(
    State(s): State<Arc<AppState>>,
    Query(q): Query<CreateQuery>,
    Json(spec): Json<EndpointSpec>,
) -> Result<Response, ApiError> {
    // The alias is parsed before anything is planned, so a typo costs nothing and a `400`
    // arrives before a 90-second model load rather than after it.
    let alias = match q.alias.as_deref() {
        Some(a) => Some(
            Alias::parse(a)
                .map_err(|e| ApiError::bad_request("bad_id", e.to_string()).with_param("alias"))?,
        ),
        None => None,
    };
    let force = q.force.unwrap_or(false);

    if q.no_wait.unwrap_or(false) {
        let state = Arc::clone(&s);
        let job = s.jobs.spawn_with("endpoint.start", move |h| async move {
            h.progress(Some(5.0), "planning");
            let plan = state.supervisor.plan(&spec).await?;
            h.progress(Some(20.0), "starting, waiting for the health gate");
            let backend = state.supervisor.up_forced(plan, None, force).await?;
            let id = backend.id.clone();
            // New process under a possibly recycled id: must arm `accepting`, not inherit a
            // drain flag from the corpse (register_started, not register_backend).
            register_started(&state, backend);
            if let Some(alias) = alias {
                h.progress(Some(90.0), format!("binding {alias}"));
                bind(&state, &alias, &id)?;
            }
            record(&state, id.as_str()).map_err(|e| anyhow::anyhow!("{}", e.body.message))
        });
        return Ok((StatusCode::ACCEPTED, Json(job)).into_response());
    }

    let plan = s.supervisor.plan(&spec).await.map_err(ApiError::from)?;
    let backend = s
        .supervisor
        .up_forced(plan, None, force)
        .await
        .map_err(ApiError::from)?;
    let id = backend.id.clone();
    register_started(&s, backend);

    if let Some(alias) = alias {
        bind(&s, &alias, &id).map_err(|e| {
            ApiError::bad_request("invalid_routes", e.to_string()).with_param("alias")
        })?;
    }

    Ok((StatusCode::CREATED, Json(record(&s, id.as_str())?)).into_response())
}

/// `DELETE /v1/endpoints/{id}` — stop it and forget it.
///
/// `DownMode::Forget` deletes the record and the credential file. The backend leaves the
/// registry, the table is recompiled, and any alias that pointed at it now fails to
/// compile — visibly, with the alias named, rather than silently routing somewhere else.
pub async fn remove(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let id = parse_id(&id)?;
    down(&s, &id, DownMode::Forget).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `POST /v1/endpoints/{id}/stop` — drain, then stop, keeping the record.
///
/// Expectation **is** state: the record survives as `Stopped` with no process, so the UI can
/// still show it and `POST /v1/endpoints` restarts it under the same id.
pub async fn stop(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<EndpointRecord> {
    let id = parse_id(&id)?;
    down(&s, &id, DownMode::Drain).await?;
    Ok(Json(record(&s, id.as_str())?))
}

/// `POST /v1/endpoints/{id}/restart` — stop now, then start the same spec again.
///
/// Alias bindings are re-applied afterwards, because a restart that silently unbinds `auto`
/// looks exactly like a restart that worked until the next request 404s.
pub async fn restart(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<EndpointRecord> {
    let id = parse_id(&id)?;
    let previous = record(&s, id.as_str())?;
    down(&s, &id, DownMode::Now).await?;

    let plan = s
        .supervisor
        .plan(&previous.spec)
        .await
        .map_err(ApiError::from)?;
    let backend = s.supervisor.up(plan, None).await.map_err(ApiError::from)?;
    let new_id = backend.id.clone();
    register_started(&s, backend);

    for alias in &previous.alias_bindings {
        if let Err(report) = bind_alias(&s, alias, &new_id) {
            tracing::warn!(
                %alias, issues = %super::render_issues(&report),
                "restarted, but the alias could not be re-bound"
            );
        }
    }
    Ok(Json(record(&s, new_id.as_str())?))
}

/// `POST /v1/endpoints/{id}/adopt` — take ownership of a process we did not spawn.
///
/// Two gates, both of which must pass:
///
/// * `core::proc::adopt` must say the recorded identity still matches (pid ∧ start ticks ∧
///   `boot_id` ∧ exe ∧ cmdline hash);
/// * the thing listening on the port must actually be serving the model the spec names,
///   checked through `/props` and falling back to `/v1/models`.
///
/// Anything else refuses. Adopting the wrong process is worse than refusing to adopt the
/// right one: the wrong process is one we would later send `SIGTERM`.
pub async fn adopt_one(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<EndpointRecord> {
    let id = parse_id(&id)?;
    let mut rec = record(&s, id.as_str())?;

    let adoption = adopt::adopt(&rec);
    let facts = match &adoption {
        Adoption::Adopted(facts) => facts.clone(),
        Adoption::Foreign { pid, why } | Adoption::Ambiguous { pid, why } => {
            return Err(ApiError::conflict(format!(
                "pid {pid} is not ours ({why}); nothing was adopted and nothing was signalled"
            )))
        }
        Adoption::Vanished => {
            return Err(ApiError::not_found(format!(
                "{id} has no live process to adopt"
            )))
        }
    };

    let port = rec.port.ok_or_else(|| {
        ApiError::conflict(format!("{id} has no recorded port to verify against"))
    })?;
    let base_url = format!("http://127.0.0.1:{port}");
    let serving = adopt::verify_serving(super::http(), &base_url, &rec.spec, ADOPT_TIMEOUT)
        .await
        .map_err(ApiError::from)?;
    if !serving {
        return Err(ApiError::conflict(format!(
            "{base_url} is not serving the model {id}'s spec names; refusing to adopt it"
        )));
    }

    rec.proc = Some(facts);
    rec.adopted = true;
    rec.desired = DesiredState::Running;
    s.store.put_endpoint(&rec).map_err(ApiError::from)?;
    Ok(Json(rec))
}

/// `GET /v1/endpoints/{id}/argv` — the command line **this** endpoint was exec'd with.
///
/// # It describes the launch that happened, not a launch that might
///
/// This route used to call `supervisor.plan(&rec.spec)`, which re-runs the *planner*: it
/// re-scans the rig, re-solves `fit()` against whatever VRAM is free **now**, and leases a
/// fresh port. For a running child that is a hypothetical *second* launch, and the two
/// answers diverge the moment anything moves. Measured after a VRAM budget change: the daemon
/// served 34 tokens where `/proc/<pid>/cmdline` had 36 — `-c 4096` instead of `-c 32768`, and
/// `-ngl 999` gone entirely, i.e. it described a CPU-only launch for a fully-offloaded child.
/// `warnings` was empty; nothing said the preview and the process disagreed.
///
/// It is the daemon-served route, so it is the **normal** answer — `apexrouter endpoint argv`
/// asks the daemon whenever one is running, and there is no flag that reaches the other path.
/// An operator debugging a launch is the person least able to afford a plausible lie.
///
/// The fix is [`ResolvedSpec::from_record`], the same line the daemon-less route in
/// `cli/src/cmd/endpoint.rs` uses: `EndpointRecord::spec` holds the operator's **draft** (that
/// is what keeps `same_endpoint` matching, so a restart does not leave a second record) and
/// the solver's numbers live in `EndpointRecord::fit`, so putting them back together
/// reproduces exactly what the supervisor folded together at launch. Nothing is re-solved and
/// nothing is re-leased.
///
/// [`ResolvedSpec::disagreements`] then checks the rendered argv against the plan the record
/// says it executes, and any divergence lands in [`ArgvPreview::warnings`] where both
/// `--json` and `print_argv` show it — rather than being invisible because nothing asked.
///
/// **No credential is ever in `argv`**: a key is passed as `--api-key-file`, and the preview
/// names the file the supervisor really wrote, never its contents.
///
/// # Errors
/// `404` for an unknown id; `409` when the record's build is no longer on the machine, or
/// when the endpoint is not one this supervisor launches (a LAN node has no local argv).
pub async fn argv(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<ArgvPreview> {
    let rec = record(&s, &id)?;
    Ok(Json(argv_of_record(&s, &rec).await?))
}

// ----------------------------------------------------------------------------------------
// internals
// ----------------------------------------------------------------------------------------

/// Render the argv of an endpoint **from its record**, never from a fresh plan.
///
/// Every input is a fact about the launch that happened:
///
/// * the resolved spec is [`ResolvedSpec::from_record`] — the record's draft with the
///   record's `fit` folded back in, at the port the record was leased;
/// * the build is the one the record names, looked up in the supervisor's own rig snapshot,
///   because `FlagSupport` decides which flags were emitted at all and a different build
///   would drop a different set;
/// * the key file is the path `LocalProvisioner::materialise_key` writes, which is derived
///   from the id and so cannot be guessed wrong;
/// * `cwd` is **this** daemon's state directory, exactly as the supervisor overrides it after
///   `core::argv` resolves one from the process environment.
///
/// The only thing left to disagree about is the rendering itself, and
/// [`ResolvedSpec::disagreements`] reports that into `warnings`.
///
/// # Errors
/// `409` when the named build is gone, when a local spec cannot be rendered, or when the
/// endpoint is not locally supervised.
async fn argv_of_record(s: &Arc<AppState>, rec: &EndpointRecord) -> Result<ArgvPreview, ApiError> {
    let resolved = ResolvedSpec::from_record(rec);
    let mut preview = match resolved.spec() {
        EndpointSpec::LocalLlama(spec) => {
            // The supervisor's cached snapshot, not a fresh scan: the same rig the launch
            // chose its build from, and the one `set_rig` installs in tests.
            let rig = s.supervisor.rig().await.map_err(ApiError::from)?;
            let build = rig
                .builds
                .iter()
                .find(|b| b.id == spec.build)
                .ok_or_else(|| {
                    ApiError::conflict(format!(
                        "build `{}` is no longer on this machine, so the argv {} was started \
                         with cannot be re-rendered faithfully; `apexrouter rig` lists what is",
                        spec.build.as_str(),
                        rec.id
                    ))
                })?;
            let key_file = spec
                .api_key
                .as_ref()
                .map(|_| s.paths.endpoints_dir().join(format!("{}.key", rec.id)));
            let mut preview = argv::plan_local(spec, build, key_file.as_deref())
                .map_err(|e| ApiError::conflict(e.to_string()))?;
            preview.warnings.extend(resolved.disagreements(&preview));
            preview
        }
        EndpointSpec::LocalVllm(spec) => {
            argv::plan_local_vllm(spec).map_err(|e| ApiError::conflict(e.to_string()))?
        }
        other => {
            return Err(ApiError::conflict(format!(
                "{} is a {:?} endpoint, which this daemon does not launch, so it has no argv",
                rec.id,
                other.kind()
            )))
        }
    };
    // `core::argv` resolves a cwd from the process environment; this daemon's own `Paths` is
    // what the supervisor hands the child, so it is authoritative here too.
    preview.cwd = s.paths.state().display().to_string();
    Ok(preview)
}

/// Parse a path segment into a `BackendId`.
fn parse_id(id: &str) -> Result<BackendId, ApiError> {
    BackendId::parse(id)
        .map_err(|e| ApiError::bad_request("bad_id", e.to_string()).with_param("id"))
}

/// Bind an alias to a freshly started endpoint and record the binding.
///
/// The binding lands in the routing table **and** on the endpoint record: the table is what
/// serves, and the record is what a restart re-binds from and what `endpoint ls` shows.
fn bind(state: &Arc<AppState>, alias: &Alias, id: &BackendId) -> anyhow::Result<()> {
    bind_alias(state, alias, id)
        .map_err(|report| anyhow::anyhow!("{}", super::render_issues(&report)))?;
    if let Ok(mut rec) = record(state, id.as_str()) {
        if !rec.alias_bindings.contains(alias) {
            rec.alias_bindings.push(alias.clone());
            state.store.put_endpoint(&rec)?;
        }
    }
    Ok(())
}

/// One endpoint record, by id.
fn record(state: &Arc<AppState>, id: &str) -> Result<EndpointRecord, ApiError> {
    let parsed = parse_id(id)?;
    state
        .store
        .list_endpoints()
        .map_err(ApiError::from)?
        .into_iter()
        .find(|r| r.id == parsed)
        .ok_or_else(|| ApiError::not_found(format!("endpoint {id}")))
}

/// Take an endpoint down and keep the registry and the table honest about it.
///
/// `Forget` removes the backend from the registry; `Drain`/`Now` leave it there as a `Down`
/// row, because the record survives and the operator should still see it.
async fn down(state: &Arc<AppState>, id: &BackendId, mode: DownMode) -> Result<(), ApiError> {
    match state.supervisor.down(id, mode).await {
        Ok(()) => {}
        Err(CoreError::NotFound(what)) => return Err(ApiError::not_found(what)),
        Err(e) => return Err(ApiError::from(e)),
    }
    if mode == DownMode::Forget {
        state.router.registry().remove(id);
        let _ = super::recompile(state);
        super::emit(state, Event::BackendRemoved { id: id.clone() });
    } else if let Some(live) = state.router.registry().get(id) {
        let mut meta = live.meta.load_full().as_ref().clone();
        meta.health = apexrouter_protocol::Health::Down {
            reason: "stopped".to_owned(),
            retry_at_unix: super::now_unix(),
        };
        register_backend(state, meta);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;
    use apexrouter_protocol::{BuildId, CredentialSource, LocalLlamaSpec, NglPlan, SamplingMode};

    /// A spec naming a model file that does not exist. `plan()` must refuse it *before*
    /// anything is spawned, which is what makes this test safe to run anywhere.
    fn missing_model_spec() -> EndpointSpec {
        EndpointSpec::LocalLlama(LocalLlamaSpec {
            build: BuildId::parse("build-vulkan").expect("build id"),
            model_path: "/nonexistent/Carnice-9b-Q6_K.gguf".to_owned(),
            mmproj: None,
            alias_flag: "carnice".to_owned(),
            host: "127.0.0.1".to_owned(),
            port: Some(8199),
            ctx: Some(4096),
            parallel: Some(1),
            kv_type: None,
            ngl: NglPlan::Auto,
            split: Default::default(),
            mode: SamplingMode::Nonthinking,
            flash_attn: None,
            api_key: Some(CredentialSource::None),
            extra_args: vec![],
        })
    }

    #[tokio::test]
    async fn listing_endpoints_on_a_bare_machine_is_an_empty_protocol_type() {
        let state = app(test_config());
        let base = serve_api(Arc::clone(&state)).await;
        let all: Vec<EndpointRecord> = reqwest::get(format!("{base}/v1/endpoints"))
            .await
            .expect("get")
            .json()
            .await
            .expect("Vec<EndpointRecord>");
        assert!(all.is_empty());
    }

    #[tokio::test]
    async fn no_wait_returns_a_job_that_finishes_even_when_the_launch_cannot() {
        let state = app(test_config());
        let base = serve_api(Arc::clone(&state)).await;
        let res = reqwest::Client::new()
            .post(format!("{base}/v1/endpoints?no_wait=true"))
            .json(&missing_model_spec())
            .send()
            .await
            .expect("post");
        assert_eq!(res.status(), 202);
        let job: apexrouter_protocol::JobRecord = res.json().await.expect("JobRecord");
        assert_eq!(job.kind, "endpoint.start");

        // Nothing may sit `Pending` forever — not even a launch that could never happen.
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            let now = state.jobs.get(job.id).expect("the row exists");
            if matches!(
                now.state,
                apexrouter_protocol::JobState::Failed
                    | apexrouter_protocol::JobState::Succeeded
                    | apexrouter_protocol::JobState::Cancelled
            ) {
                assert_eq!(now.state, apexrouter_protocol::JobState::Failed);
                assert!(now.error.is_some(), "a failed job says why");
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "job never finished: {now:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            state.store.list_endpoints().expect("list").is_empty(),
            "a refused launch leaves no record"
        );
    }

    /// The launch is refused at plan time, with a body that names the file — and nothing is
    /// spawned, nothing is recorded, and no port is held.
    #[tokio::test]
    async fn a_launch_whose_weights_are_missing_is_refused_before_anything_is_spawned() {
        let state = app(test_config());
        let base = serve_api(Arc::clone(&state)).await;
        let res = reqwest::Client::new()
            .post(format!("{base}/v1/endpoints"))
            .json(&missing_model_spec())
            .send()
            .await
            .expect("post");
        assert!(
            res.status().is_client_error() || res.status().is_server_error(),
            "a missing binary or missing weights must refuse"
        );
        let body: apexrouter_protocol::ErrorEnvelope = res.json().await.expect("ErrorEnvelope");
        assert!(!body.error.kind.is_empty());
        assert!(
            state.store.list_endpoints().expect("list").is_empty(),
            "a refused launch leaves no record"
        );
    }

    #[tokio::test]
    async fn a_bad_alias_is_a_400_that_names_the_parameter() {
        let state = app(test_config());
        let base = serve_api(Arc::clone(&state)).await;
        let res = reqwest::Client::new()
            .post(format!("{base}/v1/endpoints?alias=NOT%2FAN%2FALIAS"))
            .json(&missing_model_spec())
            .send()
            .await
            .expect("post");
        assert_eq!(res.status(), 400);
        let body: apexrouter_protocol::ErrorEnvelope = res.json().await.expect("ErrorEnvelope");
        assert_eq!(body.error.kind, "bad_id");
        assert_eq!(body.error.param.as_deref(), Some("alias"));
    }

    #[tokio::test]
    async fn every_verb_on_a_missing_endpoint_is_a_typed_404() {
        let state = app(test_config());
        let base = serve_api(Arc::clone(&state)).await;
        let http = reqwest::Client::new();
        for (verb, url) in [
            ("GET", format!("{base}/v1/endpoints/ghost")),
            ("GET", format!("{base}/v1/endpoints/ghost/argv")),
            ("POST", format!("{base}/v1/endpoints/ghost/stop")),
            ("POST", format!("{base}/v1/endpoints/ghost/adopt")),
            ("POST", format!("{base}/v1/endpoints/ghost/restart")),
        ] {
            let res = match verb {
                "GET" => http.get(&url).send().await,
                _ => http.post(&url).send().await,
            }
            .expect("send");
            assert_eq!(res.status(), 404, "{verb} {url}");
            let body: apexrouter_protocol::ErrorEnvelope = res.json().await.expect("envelope");
            assert_eq!(body.error.kind, "not_found");
        }
    }

    /// A backend nobody launches has no command line, and saying so is the whole answer.
    ///
    /// Rendering *something* for a LAN node would mean inventing a `llama-server` invocation
    /// for a URL somebody registered — argv for a process that does not exist and never will,
    /// which is the same class of plausible lie the record-resolved preview exists to end.
    #[tokio::test]
    async fn an_endpoint_this_daemon_does_not_launch_has_no_argv() {
        let state = app(test_config());
        let base = serve_api(Arc::clone(&state)).await;
        state
            .store
            .put_endpoint(&EndpointRecord {
                id: BackendId::parse("lan-box").expect("id"),
                spec: EndpointSpec::Node(apexrouter_protocol::NodeSpec {
                    base_url: "http://127.0.0.2:8080".to_owned(),
                    credential: CredentialSource::None,
                    label: "the box in the cupboard".to_owned(),
                    declared_models: vec![],
                    protocol: Default::default(),
                }),
                desired: DesiredState::Running,
                proc: None,
                port: None,
                log_path: None,
                started_at_unix: 0,
                fit: None,
                adopted: false,
                alias_bindings: vec![],
            })
            .expect("write the record");

        let res = reqwest::get(format!("{base}/v1/endpoints/lan-box/argv"))
            .await
            .expect("get");
        assert_eq!(res.status(), 409);
        let body: apexrouter_protocol::ErrorEnvelope = res.json().await.expect("envelope");
        assert!(
            body.error.message.contains("no argv"),
            "{}",
            body.error.message
        );
    }
}
