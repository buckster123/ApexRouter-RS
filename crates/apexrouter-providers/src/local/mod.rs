//! OWNER: unit P-01 (providers/src/local/**). Do not edit outside that unit.
//!
//! The local supervisor, and the `Provisioner` trait every other provider implements.
//!
//! Every measured defect in `docs/port/03` is closed **by construction**, not by care:
//! `LD_LIBRARY_PATH` is always set (the `build-vulkan` trailing-colon RUNPATH trap), the
//! port bind-probe holds its reservation under a per-endpoint lock until the health gate
//! passes, the health gate is a real wall-clock deadline that **resets on observed
//! progress**, and **the failure path is the stop path** — on expiry we kill the child,
//! remove the record, mark `Failed` with the log tail, and clear the route.

use apexrouter_core::config::Config;
use apexrouter_core::error::{Error, Result};
use apexrouter_core::money::SpendApproval;
use apexrouter_core::proc::{self, Adoption, Liveness, SpawnRequest};
use apexrouter_core::secret::Secret;
use apexrouter_core::store::Store;
use apexrouter_core::upstream::UpstreamProbe;
use apexrouter_core::{argv, discover, fit as fit_solver, Paths};
use apexrouter_protocol::{
    AlertLevel, ArgvPreview, Backend, BackendId, BackendKind, BackendLimits, BootPhase,
    CostEstimate, CredentialSource, DesiredState, EndpointRecord, EndpointRef, EndpointSpec, Event,
    FitInput, FitPlan, FitVerdict, GgufMeta, Health, LlamaBuild, LocalLlamaSpec, Money, PriceModel,
    PriceSource, ProcFacts, Protocol, Provenance, RigSnapshot, UpstreamModel,
};
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

pub mod adopt;
pub mod resolve;
pub mod supervisor;

pub use resolve::{ResolvedSpec, SpecOverride};
use supervisor::{GateInput, GateOutcome, PortDenied, PortLease};

/// How long a `SIGTERM` gets before the `SIGKILL`.
const TERM_WAIT: Duration = Duration::from_secs(10);
/// How long the `SIGKILL` gets before we report a timeout.
const KILL_WAIT: Duration = Duration::from_secs(5);
/// How long a freshly exec'd child gets to publish its real `/proc/<pid>/cmdline`.
const SETTLE_BUDGET: Duration = Duration::from_millis(1_000);
/// How long a rig scan stays warm. Only the *device enumeration* is cached; free VRAM is
/// re-derived on every plan.
const RIG_TTL: Duration = Duration::from_secs(60);
/// Lines of log carried on a failure. Enough to hold a llama.cpp load failure and its
/// context, short enough to put in a JSON error body.
const FAIL_TAIL_LINES: usize = 40;

/// How a backend is brought up and taken down. Implemented by the local supervisor, the
/// vast provisioner and (trivially) by node/managed registration.
#[async_trait]
pub trait Provisioner: Send + Sync {
    /// Which kind of backend this provisioner owns.
    fn kind(&self) -> BackendKind;
    /// Resolve a draft into an executable plan, with the fit, the cost and the warnings the
    /// operator must see **before** confirming.
    async fn plan(&self, draft: &EndpointSpec) -> Result<LaunchPlan>;
    /// Execute a plan. Anything that costs money requires a [`SpendApproval`].
    async fn up(&self, plan: LaunchPlan, approval: Option<SpendApproval>) -> Result<Backend>;
    /// Take it down.
    async fn down(&self, id: &BackendId, mode: DownMode) -> Result<()>;
    /// The last `tail` lines. **The call an agent makes when a start failed.**
    async fn logs(&self, id: &BackendId, tail: usize) -> Result<Vec<String>>;
    /// Re-derive live backends from persisted facts at startup.
    async fn reconcile(&self) -> Result<Vec<Backend>>;
}

/// How hard to take something down.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DownMode {
    /// Stop accepting, wait for in-flight to finish, then stop.
    Drain,
    /// Stop now.
    Now,
    /// Stop and delete the record.
    Forget,
}

/// Exactly what will happen, before it happens.
#[derive(Clone, Debug)]
pub struct LaunchPlan {
    /// The resolved spec.
    pub spec: EndpointSpec,
    /// The exact argv and env. **No credential is ever in `argv`.**
    pub argv: ArgvPreview,
    /// What we expect it to occupy.
    pub fit: Option<FitPlan>,
    /// What it will cost.
    pub cost: CostEstimate,
    /// Anything the operator must read before confirming.
    pub warnings: Vec<String>,
    /// The port reserved for it, held under a per-endpoint lock until the health gate passes.
    pub port: u16,
}

/// The local `llama-server`/vLLM supervisor.
///
/// Owns no children until [`Provisioner::up`] is called, and — decision D3 of the charter —
/// does not kill them when it exits either. Everything durable it knows lives in
/// `$STATE/endpoints/<id>.json`; the only things held in memory are a cache (the rig scan)
/// and two locks (the per-endpoint launch mutex and the port reservation set), all of which
/// are reconstructible from disk.
pub struct LocalProvisioner {
    paths: Paths,
    cfg: RwLock<Config>,
    store: Store,
    tx: broadcast::Sender<Event>,
    rig: RwLock<Option<(RigSnapshot, Instant)>>,
    /// One mutex per endpoint id. Serialises `up`/`down` for the *same* endpoint.
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Ports promised to a launch that has not yet passed its health gate. Shared across
    /// endpoints, which is what stops two concurrent launches both taking 8100.
    ports: Arc<Mutex<HashSet<u16>>>,
    http: reqwest::Client,
}

/// Why a launch did not happen, or did not survive.
///
/// Each variant names **who** is in the way, because that is what makes the message
/// actionable: which backend holds the port, which endpoints hold the VRAM, what the log
/// said.
#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    /// Somebody already has the port.
    #[error("port {port} is in use{}", .held_by.as_ref().map(|b| format!(" by {b}")).unwrap_or_default())]
    PortInUse {
        /// The port.
        port: u16,
        /// Which of our backends holds it, when it is one of ours.
        held_by: Option<BackendId>,
    },
    /// `fit()` says this would not fit against **live** free VRAM minus reservations.
    #[error("insufficient VRAM: need {need_mb} MiB, {free_mb} MiB free (held by {})", .held_by.iter().map(|b| b.as_str()).collect::<Vec<_>>().join(", "))]
    InsufficientVram {
        /// What the plan needs.
        need_mb: u64,
        /// What is actually free.
        free_mb: u64,
        /// Which endpoints hold the rest.
        held_by: Vec<BackendId>,
    },
    /// The `llama-server` binary is not where the build said it was.
    #[error("binary missing: {0}")]
    BinaryMissing(String),
    /// The weights are not where the spec said they were.
    #[error("model missing: {0}")]
    ModelMissing(String),
    /// The health gate's wall-clock deadline expired. The child has already been killed and
    /// the record removed — **the failure path is the stop path**.
    #[error("health gate timed out")]
    HealthTimeout {
        /// The last lines of its log, so the operator does not have to go looking.
        log_tail: Vec<String>,
    },
    /// It exited before ever answering.
    #[error("exited early{}", .code.map(|c| format!(" with code {c}")).unwrap_or_default())]
    ExitedEarly {
        /// Its exit code, when it had one.
        code: Option<i32>,
        /// The last lines of its log.
        log_tail: Vec<String>,
    },
}

/// Carry a launch failure through the `Result<T, core::Error>` the trait returns, without
/// losing the structure the operator needs.
///
/// `PortInUse` and `InsufficientVram` map onto the core variants that already exist for
/// exactly those conditions, so the CLI and the API render one message per condition rather
/// than two. The rest become `Invalid` with the log tail folded into `why`, because a start
/// failure whose log you have to go and find is the defect this crate exists to remove.
impl From<LaunchError> for Error {
    fn from(e: LaunchError) -> Error {
        let rendered = e.to_string();
        match e {
            LaunchError::PortInUse { port, held_by } => Error::PortInUse {
                port,
                held_by: held_by.map(BackendId::into_string),
            },
            LaunchError::InsufficientVram {
                need_mb, free_mb, ..
            } => Error::InsufficientVram { need_mb, free_mb },
            LaunchError::BinaryMissing(what) => Error::NotFound(format!("binary {what}")),
            LaunchError::ModelMissing(what) => Error::NotFound(format!("model {what}")),
            LaunchError::HealthTimeout { log_tail } | LaunchError::ExitedEarly { log_tail, .. } => {
                Error::Invalid {
                    what: "launch".to_owned(),
                    why: format!("{rendered}: {}", log_tail.join(" | ")),
                }
            }
        }
    }
}

impl LocalProvisioner {
    /// Build the supervisor. Owns no children until [`Provisioner::up`] is called.
    ///
    /// The `Store` is derived from `paths` rather than passed in, because a supervisor whose
    /// state root disagreed with the daemon's would write records nobody reads.
    ///
    /// The HTTP client has **connection pooling disabled**: the health gate opens a socket
    /// per probe against an upstream that is, by definition, not yet up, and one idle pool
    /// entry per attempt is exactly how "start and stop ten times in a loop" becomes an fd
    /// leak.
    pub fn new(paths: Paths, cfg: Config, tx: broadcast::Sender<Event>) -> Self {
        let http = reqwest::Client::builder()
            .pool_max_idle_per_host(0)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        LocalProvisioner {
            store: Store::new(paths.clone()),
            paths,
            cfg: RwLock::new(cfg),
            tx,
            rig: RwLock::new(None),
            locks: Mutex::new(HashMap::new()),
            ports: Arc::new(Mutex::new(HashSet::new())),
            http,
        }
    }

    /// Replace the configuration after a `SIGHUP` reload.
    ///
    /// Deliberately does not touch running children: a reload changes what the *next* launch
    /// does, and a model that took 90 seconds to load must not be evicted because somebody
    /// edited a timeout.
    pub fn set_config(&self, cfg: Config) {
        match self.cfg.write() {
            Ok(mut slot) => *slot = cfg,
            Err(poisoned) => *poisoned.into_inner() = cfg,
        }
    }

    /// Install a rig snapshot somebody else already scanned, so the control plane and the
    /// supervisor agree on what hardware exists instead of probing every binary twice.
    pub fn set_rig(&self, rig: RigSnapshot) {
        let slot = Some((rig, Instant::now()));
        match self.rig.write() {
            Ok(mut w) => *w = slot,
            Err(poisoned) => *poisoned.into_inner() = slot,
        }
    }

    /// The rig, scanning if the cache is cold or stale.
    ///
    /// Only the *device enumeration* is cached. Free VRAM is re-derived on every plan by
    /// `budget_from_rig` against the live endpoint records, which is what makes
    /// [`LaunchError::InsufficientVram`] fire before a second launch OOMs the first.
    ///
    /// # Errors
    /// Propagates a discovery failure.
    pub async fn rig(&self) -> Result<RigSnapshot> {
        if let Ok(slot) = self.rig.read() {
            if let Some((rig, at)) = slot.as_ref() {
                if at.elapsed() < RIG_TTL {
                    return Ok(rig.clone());
                }
            }
        }
        let cfg = self.cfg();
        let rig = supervisor::scan_rig(&cfg.endpoints, self.paths.cache()).await?;
        self.set_rig(rig.clone());
        Ok(rig)
    }

    /// Execute a plan, optionally overriding VRAM admission control.
    ///
    /// [`Provisioner::up`] is this with `force = false`. `force = true` is the `--force` flag
    /// of `apexrouter up`: it skips the [`LaunchError::InsufficientVram`] refusal and nothing
    /// else. Every other check — the binary, the weights, the port, the health gate — still
    /// applies, because none of them is a judgement call the operator can overrule.
    ///
    /// # Errors
    /// A [`LaunchError`] converted into [`Error`], or an I/O failure writing the record.
    pub async fn up_forced(
        &self,
        plan: LaunchPlan,
        _approval: Option<SpendApproval>,
        force: bool,
    ) -> Result<Backend> {
        let cfg = self.cfg();
        let id = self.allocate_id(&plan.spec)?;
        let lock = self.endpoint_lock(id.as_str());
        let _guard = lock.lock().await;

        // ---- is it already up? ----------------------------------------------------------
        // Reusing the id of a live endpoint would overwrite the record that owns its pid,
        // and the running child would become an orphan nobody can stop.
        if let Some(existing) = self
            .store
            .list_endpoints()?
            .into_iter()
            .find(|r| r.id == id)
        {
            if let Adoption::Adopted(facts) = adopt::adopt(&existing) {
                return Err(Error::Conflict(format!(
                    "{id} is already running as pid {}; stop it first",
                    facts.pid
                )));
            }
        }

        // ---- admission control ---------------------------------------------------------
        if !force {
            if let Some(fit) = plan.fit.as_ref() {
                if let FitVerdict::WontFit { short_by_mb } = fit.verdict {
                    let need_mb = fit
                        .weights_mb
                        .saturating_add(fit.kv_mb)
                        .saturating_add(fit.compute_mb);
                    return Err(LaunchError::InsufficientVram {
                        need_mb,
                        free_mb: need_mb.saturating_sub(short_by_mb),
                        held_by: self.vram_holders()?,
                    }
                    .into());
                }
            }
        }

        // ---- the port, reserved until the health gate settles ---------------------------
        // A port the *operator* pinned is honoured exactly or refused by name. A port the
        // *planner* proposed is only a preference: two plans drawn up before either was
        // executed necessarily propose the same free port, and refusing the second would
        // turn planning ahead into a self-inflicted collision.
        let range = cfg.endpoints.port_range;
        let pinned = explicit_port(&plan.spec);
        let lease = match supervisor::reserve_port(&self.ports, range, pinned.or(Some(plan.port))) {
            Ok(lease) => lease,
            Err(PortDenied::Taken(port)) if pinned.is_some() => {
                return Err(LaunchError::PortInUse {
                    port,
                    held_by: self.holder_of(port)?,
                }
                .into())
            }
            Err(_) => match supervisor::reserve_port(&self.ports, range, None) {
                Ok(lease) => {
                    tracing::info!(
                        proposed = plan.port,
                        reserved = lease.port(),
                        "the planned port was taken between plan and up"
                    );
                    lease
                }
                Err(PortDenied::Taken(port)) => {
                    return Err(LaunchError::PortInUse {
                        port,
                        held_by: self.holder_of(port)?,
                    }
                    .into())
                }
                Err(PortDenied::RangeExhausted(lo, hi)) => {
                    return Err(Error::Invalid {
                        what: "[endpoints] port_range".to_owned(),
                        why: format!("every port in {lo}..={hi} is bound or reserved"),
                    })
                }
            },
        };

        // The lease lives until this scope ends, which is after the gate has settled either
        // way. `launch` is a separate function so every exit path releases it.
        //
        // `spawned` says whether this call got as far as writing its own record. A refusal
        // *before* that point — a missing binary, unreadable weights — must not unwind
        // somebody else's state under the same id.
        let mut spawned = false;
        let out = self.launch(&id, plan, &lease, &cfg, &mut spawned).await;
        match (out.as_ref(), spawned) {
            (Err(e), true) => self.fail(&id, e),
            (Err(e), false) => {
                tracing::warn!(%id, error = %e, "refused before anything was started")
            }
            (Ok(_), _) => {}
        }
        out
    }

    // -----------------------------------------------------------------------------------
    // internals
    // -----------------------------------------------------------------------------------

    /// The current configuration.
    fn cfg(&self) -> Config {
        match self.cfg.read() {
            Ok(c) => c.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Broadcast, ignoring "nobody is listening" — the daemon must not care.
    fn emit(&self, event: Event) {
        let _ = self.tx.send(event);
    }

    /// The per-endpoint launch mutex, created on first use.
    fn endpoint_lock(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = match self.locks.lock() {
            Ok(l) => l,
            Err(poisoned) => poisoned.into_inner(),
        };
        Arc::clone(locks.entry(id.to_owned()).or_default())
    }

    /// Which of our endpoints has `port` recorded against it, when one does.
    fn holder_of(&self, port: u16) -> Result<Option<BackendId>> {
        Ok(self
            .store
            .list_endpoints()?
            .into_iter()
            .find(|r| r.port == Some(port))
            .map(|r| r.id))
    }

    /// Every endpoint currently holding VRAM we accounted for.
    fn vram_holders(&self) -> Result<Vec<BackendId>> {
        Ok(self
            .store
            .list_endpoints()?
            .into_iter()
            .filter(|r| r.desired == DesiredState::Running && r.fit.is_some())
            .map(|r| r.id)
            .collect())
    }

    /// The id this spec should launch under.
    ///
    /// An existing record whose spec differs only by port is the *same* endpoint being
    /// restarted and keeps its id — otherwise stopping and starting one model ten times
    /// would leave ten records and ten log files behind. Anything else is deduplicated with
    /// a numeric suffix.
    fn allocate_id(&self, spec: &EndpointSpec) -> Result<BackendId> {
        let existing = self.store.list_endpoints()?;
        if let Some(rec) = existing.iter().find(|r| same_endpoint(&r.spec, spec)) {
            return Ok(rec.id.clone());
        }
        let base = spec.suggested_id();
        let taken: HashSet<&str> = existing.iter().map(|r| r.id.as_str()).collect();
        if !taken.contains(base.as_str()) {
            return Ok(BackendId::parse(&base)?);
        }
        for n in 2..1_000u32 {
            let candidate = format!("{base}-{n}");
            if !taken.contains(candidate.as_str()) {
                return Ok(BackendId::parse(&candidate)?);
            }
        }
        Err(Error::Conflict(format!(
            "cannot allocate an id for {base}: 1000 endpoints already use that name"
        )))
    }

    /// Spawn, record, health-gate.
    async fn launch(
        &self,
        id: &BackendId,
        plan: LaunchPlan,
        lease: &PortLease,
        cfg: &Config,
        spawned_flag: &mut bool,
    ) -> Result<Backend> {
        let port = lease.port();
        // The SAME resolution `plan()` did, from the same draft and the same `FitPlan`, with
        // only the port free to differ: the planned port can be taken between plan and up.
        // `ResolvedSpec::resolve` is pure, so the argv the operator was shown and the argv
        // the child gets cannot say different things about `-c`, `-np`, `-ngl`, `-dev` or
        // the KV type.
        let resolved = ResolvedSpec::resolve(&plan.spec, port, plan.fit.clone());
        let spec = resolved.spec().clone();

        // ---- everything that must be true before we exec --------------------------------
        let rig = self.rig().await?;
        let build = build_for(&rig, &spec);
        if let Some(b) = build.as_ref() {
            if !Path::new(&b.server_path).exists() {
                return Err(LaunchError::BinaryMissing(b.server_path.clone()).into());
            }
        }
        if let Some(model) = model_path(&spec) {
            if !Path::new(&model).exists() {
                return Err(LaunchError::ModelMissing(model).into());
            }
        }

        // ---- the credential, as a file. NEVER argv ---------------------------------------
        let key_file = self.materialise_key(id, &spec)?;

        // ---- the log, rotated with copytruncate before we append to it -------------------
        let log = self.paths.log_file(id);
        if let Some(parent) = log.parent() {
            std::fs::create_dir_all(parent).map_err(|source| Error::Io {
                path: parent.display().to_string(),
                source,
            })?;
        }
        supervisor::rotate_log(
            &log,
            cfg.supervisor.log_rotate_mb.saturating_mul(1024 * 1024),
        )?;

        // ---- argv -------------------------------------------------------------------------
        let preview = self.argv_for(&resolved, build.as_ref(), key_file.as_deref())?;
        let program = PathBuf::from(&preview.program);
        let cwd = PathBuf::from(&preview.cwd);
        std::fs::create_dir_all(&cwd).map_err(|source| Error::Io {
            path: cwd.display().to_string(),
            source,
        })?;

        // ---- exec -------------------------------------------------------------------------
        let spawned = proc::spawn_detached(SpawnRequest {
            program: &program,
            args: &preview.args,
            env: &preview.env,
            cwd: &cwd,
            log: &log,
            setsid: true,
        })?;
        let facts = settle_facts(&spawned.facts, preview.args.last().map(String::as_str)).await;

        // ---- facts on disk BEFORE we wait for anything ------------------------------------
        // A crash, a `systemctl --user restart` or a panic between here and the health gate
        // must leave a record we can adopt, not an orphan holding 6 GB of VRAM.
        // `spec` is the operator's DRAFT plus the port, not the resolved spec: `same_endpoint`
        // compares a record against the next draft, and a record holding solver-filled
        // numbers would stop matching the moment free VRAM moved — leaving a second record,
        // and a second log file, behind every restart. The numbers that were executed live
        // in `fit`, which `ResolvedSpec` has annotated with the argv they became; put them
        // back together with `ResolvedSpec::from_record`.
        let mut record = EndpointRecord {
            id: id.clone(),
            spec: with_port(&plan.spec, port),
            desired: DesiredState::Running,
            proc: Some(facts.clone()),
            port: Some(port),
            log_path: Some(log.display().to_string()),
            started_at_unix: chrono::Utc::now().timestamp(),
            fit: resolved.fit().cloned(),
            adopted: false,
            alias_bindings: Vec::new(),
        };
        self.store.put_endpoint(&record)?;
        *spawned_flag = true;

        // ---- the health gate ---------------------------------------------------------------
        let url = base_url(&spec, port);
        let id_for_events = id.clone();
        let tx = self.tx.clone();
        let outcome = supervisor::health_gate(
            GateInput {
                http: &self.http,
                base_url: &url,
                log: &log,
                facts: &facts,
                deadline: Duration::from_millis(cfg.supervisor.health_deadline_ms),
                interval: Duration::from_millis(cfg.supervisor.health_interval_ms.max(50)),
            },
            &move |phase, line| {
                let _ = tx.send(Event::BootProgress {
                    backend: id_for_events.clone(),
                    phase,
                    line,
                });
            },
        )
        .await;

        match outcome {
            GateOutcome::Ready(probe) => {
                // Re-read the identity now that the process has settled, so `adopt` — and
                // therefore every future `down` — can own the child we just started.
                record.proc = Some(refresh_facts(&facts));
                self.store.put_endpoint(&record)?;
                let backend = self.backend_of(&record, Some(&probe), ready_health(&probe));
                self.emit(Event::BackendChanged {
                    backend: Box::new(backend.clone()),
                });
                Ok(backend)
            }
            // THE FAILURE PATH IS THE STOP PATH. Both arms kill and report; the caller's
            // `fail` removes the record, marks `Failed` and clears the route.
            GateOutcome::Timeout => {
                let _ = proc::stop_graceful(&facts, TERM_WAIT, KILL_WAIT);
                supervisor::reap(facts.pid);
                Err(LaunchError::HealthTimeout {
                    log_tail: supervisor::log_tail(&log, FAIL_TAIL_LINES),
                }
                .into())
            }
            GateOutcome::Exited { code } => Err(LaunchError::ExitedEarly {
                code,
                log_tail: supervisor::log_tail(&log, FAIL_TAIL_LINES),
            }
            .into()),
        }
    }

    /// Unwind a failed launch: remove the record, drop the credential file, mark the boot
    /// machine `Failed` and tell the table to clear any route pointing here.
    ///
    /// `Event::BackendRemoved` is *how* the route is cleared: `apexrouter-providers` must
    /// not depend on `apexrouter-router` (that edge would be a cycle), so the table reacts
    /// to the broadcast. Removing the record is not conditional on anybody listening.
    fn fail(&self, id: &BackendId, why: &Error) {
        let reason = why.to_string();
        if let Err(e) = self.store.remove_endpoint(id) {
            tracing::warn!(%id, error = %e, "could not remove the record of a failed launch");
        }
        let _ = std::fs::remove_file(self.key_path(id));
        self.emit(Event::BootProgress {
            backend: id.clone(),
            phase: BootPhase::Failed {
                reason: reason.clone(),
            },
            line: None,
        });
        self.emit(Event::BackendRemoved { id: id.clone() });
        self.emit(Event::Alert {
            level: AlertLevel::Serious,
            message: format!("{id} failed to start: {reason}"),
            action: Some(format!("apexrouter endpoint logs {id}")),
            id: format!("launch-failed:{id}"),
        });
    }

    /// Where an endpoint's `--api-key-file` lives: inside `$STATE/endpoints/`, which is
    /// `0700`, and never inside the repo.
    fn key_path(&self, id: &BackendId) -> PathBuf {
        self.paths.endpoints_dir().join(format!("{id}.key"))
    }

    /// Write the endpoint's API key to a `0600` file, or `None` when it has none.
    ///
    /// The key never reaches argv: `/proc/<pid>/cmdline` is world-readable, so `--api-key`
    /// would publish it to every process on the box.
    fn materialise_key(&self, id: &BackendId, spec: &EndpointSpec) -> Result<Option<PathBuf>> {
        let EndpointSpec::LocalLlama(s) = spec else {
            return Ok(None);
        };
        let Some(source) = s.api_key.as_ref() else {
            let _ = std::fs::remove_file(self.key_path(id));
            return Ok(None);
        };
        let secret = match source {
            CredentialSource::Env { var } => std::env::var(var).ok().map(Secret::new),
            CredentialSource::File { path } => std::fs::read_to_string(path)
                .ok()
                .map(|v| Secret::new(v.trim().to_owned())),
            CredentialSource::None
            | CredentialSource::Managed { .. }
            | CredentialSource::Instance => None,
        };
        let Some(secret) = secret.filter(|s| !s.expose().is_empty()) else {
            return Err(Error::MissingCredential(format!(
                "endpoint {id}: {source:?} produced nothing"
            )));
        };
        let path = self.key_path(id);
        self.store
            .write_atomic(&path, secret.expose().as_bytes(), 0o600)?;
        Ok(Some(path))
    }

    /// The exact argv and env, with **this** supervisor's state directory as the cwd.
    ///
    /// `core::argv` resolves the cwd from the process environment; the supervisor's own
    /// `Paths` is authoritative, so the preview and the exec cannot disagree. The cwd is
    /// never load-bearing anyway — that is what the always-present `LD_LIBRARY_PATH` is for.
    ///
    /// **Takes a [`ResolvedSpec`], never a draft.** That is the whole of the fix for the
    /// MK1 ACCEPTANCE defect: the solver used to compute a plan that argv never read, and
    /// the type now makes "render argv from the operator's draft" fail to compile. Any
    /// residual disagreement between the rendered argv and the plan it claims to execute is
    /// logged rather than left for a log tail to imply.
    fn argv_for(
        &self,
        resolved: &ResolvedSpec,
        build: Option<&LlamaBuild>,
        key_file: Option<&Path>,
    ) -> Result<ArgvPreview> {
        let spec = resolved.spec();
        let mut preview = match (spec, build) {
            (EndpointSpec::LocalLlama(s), Some(b)) => argv::plan_local(s, b, key_file)?,
            (EndpointSpec::LocalLlama(s), None) => {
                return Err(LaunchError::BinaryMissing(format!(
                    "no discovered build matches `{}` for {}",
                    s.build, s.model_path
                ))
                .into())
            }
            (EndpointSpec::LocalVllm(s), _) => argv::plan_local_vllm(s)?,
            (other, _) => {
                return Err(Error::Invalid {
                    what: "endpoint spec".to_owned(),
                    why: format!(
                        "{:?} is not a local endpoint; the local supervisor does not own it",
                        other.kind()
                    ),
                })
            }
        };
        preview.cwd = self.paths.state().display().to_string();
        // Impossible by construction — `preview` was rendered from `resolved` — but the
        // defect this type closed was exactly "the plan and the process disagreed and
        // nothing said so", so the check is cheap insurance rather than decoration.
        for line in resolved.disagreements(&preview) {
            tracing::error!(%line, "the rendered argv does not execute the plan it reports");
        }
        Ok(preview)
    }

    /// Solve the fit against a **live** budget: this rig's free VRAM, minus what every
    /// running endpoint already reserved.
    ///
    /// The budget is scoped to `spec.build`, because that is the binary about to be exec'd
    /// and one `llama-server` process uses one compute backend. Scoping it any other way is
    /// how the same physical GPU, enumerated as `ROCm0` by one build and `Vulkan0` by
    /// another, got its VRAM counted twice.
    ///
    /// Returns `None` — with a warning — when the weights cannot be read as GGUF. A model we
    /// cannot measure is not a model we refuse to start; it is one we start without a VRAM
    /// promise, and say so.
    fn fit_for(
        &self,
        spec: &LocalLlamaSpec,
        rig: &RigSnapshot,
        warnings: &mut Vec<String>,
    ) -> Result<Option<FitPlan>> {
        let path = Path::new(&spec.model_path);
        let gguf: GgufMeta = match discover::read_gguf_meta(path) {
            Ok(m) => m,
            Err(e) => {
                warnings.push(format!(
                    "{} could not be read as GGUF ({e}); launching without a VRAM estimate",
                    spec.model_path
                ));
                return Ok(None);
            }
        };
        let weights_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let cfg = self.cfg();
        let running = self.store.list_endpoints()?;
        let budget = fit_solver::budget_from_rig(
            rig,
            fit_solver::BackendScope::Build(&spec.build),
            &spec.split.devices,
            cfg.endpoints.vram_margin_mb,
            &running,
        );
        warnings.extend(budget.notes.iter().cloned());
        Ok(Some(fit_solver::fit(&FitInput {
            weights_bytes,
            gguf,
            budget,
            want_ctx: spec.ctx,
            want_parallel: spec.parallel,
            want_kv: spec.kv_type,
            split: spec.split.clone(),
            batch: None,
        })))
    }

    /// The routing-table description of one endpoint record.
    fn backend_of(
        &self,
        rec: &EndpointRecord,
        probe: Option<&UpstreamProbe>,
        health: Health,
    ) -> Backend {
        let port = rec.port.unwrap_or(0);
        let (label, credential, devices, alias) = match &rec.spec {
            EndpointSpec::LocalLlama(s) => (
                file_stem(&s.model_path),
                s.api_key.clone().unwrap_or(CredentialSource::None),
                s.split.devices.clone(),
                s.alias_flag.clone(),
            ),
            EndpointSpec::LocalVllm(s) => (
                file_stem(&s.model_id),
                CredentialSource::None,
                Vec::new(),
                s.model_id.clone(),
            ),
            _ => (
                rec.id.to_string(),
                CredentialSource::None,
                Vec::new(),
                String::new(),
            ),
        };

        let models = match probe {
            Some(p) if !p.models.is_empty() => p.models.clone(),
            _ if !alias.is_empty() => vec![UpstreamModel {
                id: alias,
                ctx: rec.fit.as_ref().map(|f| f.ctx),
                vision: false,
                tools: false,
            }],
            _ => Vec::new(),
        };
        let slots_total = probe.and_then(|p| p.slots_total);

        Backend {
            id: rec.id.clone(),
            kind: rec.spec.kind(),
            protocol: Protocol::OpenAi,
            label,
            base_url: base_url(&rec.spec, port),
            credential,
            tags: tags_for(&rec.spec),
            models,
            limits: BackendLimits {
                max_concurrent: slots_total.unwrap_or(1).max(1),
                queue_depth: self.cfg().router.max_inflight,
                ctx: probe
                    .and_then(|p| p.ctx)
                    .or(rec.fit.as_ref().map(|f| f.ctx)),
                slots_total,
            },
            // A local server bills nothing, and that is exact rather than an assumption.
            // Electricity is real but is not a number we can meter, and inventing one is
            // precisely the buried constant this codebase refuses to carry.
            price: Some(PriceModel::Free),
            health,
            provenance: if rec.adopted {
                Provenance::Adopted
            } else {
                Provenance::Spawned
            },
            endpoint: Some(EndpointRef {
                id: rec.id.clone(),
                kind: rec.spec.kind(),
            }),
            enabled: true,
            devices,
            last_error: None,
        }
    }

    /// Wait for the upstream to finish what it is doing, up to `drain_timeout_secs`.
    ///
    /// Uses the upstream's own busy-slot count, which is all a provisioner can see. During a
    /// swap the router's in-flight counter is the authoritative one (§4.7), and it is not
    /// this crate's to read — a `--no-slots` build 501s on `/slots`, which is why the swap
    /// path must never depend on this.
    async fn drain(&self, record: &EndpointRecord) {
        let cfg = self.cfg();
        let deadline = Instant::now() + Duration::from_secs(cfg.server.drain_timeout_secs.max(1));
        let url = base_url(&record.spec, record.port.unwrap_or(0));
        self.emit(Event::BackendChanged {
            backend: Box::new(self.backend_of(record, None, Health::Draining { in_flight: 0 })),
        });
        while Instant::now() < deadline {
            let probe =
                apexrouter_core::upstream::probe(&self.http, &url, None, Duration::from_secs(2))
                    .await;
            match probe.slots_busy {
                Some(0) | None => return,
                Some(_) => tokio::time::sleep(Duration::from_millis(250)).await,
            }
        }
    }
}

#[async_trait]
impl Provisioner for LocalProvisioner {
    fn kind(&self) -> BackendKind {
        BackendKind::LocalLlama
    }

    /// Resolve a draft into an executable plan.
    ///
    /// Everything expensive and everything refusable happens here, before anything is
    /// spawned: the build is chosen (a fallback is a **visible warning**, never a silent
    /// substitution of HIP for Vulkan), the binary and the weights are checked to exist, the
    /// fit is solved against live VRAM, and a port is proposed. The reservation itself
    /// belongs to [`Provisioner::up`], because a plan the operator never confirms must not
    /// hold 8100 hostage.
    ///
    /// The proposal lands in [`LaunchPlan::port`] and in the rendered argv, but **not** in
    /// `plan.spec.port`, which keeps meaning exactly what the operator asked for. That is
    /// what lets `up` honour a pinned port strictly while treating a proposed one as a
    /// preference — two plans drawn up before either was executed necessarily propose the
    /// same free port.
    async fn plan(&self, draft: &EndpointSpec) -> Result<LaunchPlan> {
        let mut warnings: Vec<String> = Vec::new();
        let rig = self.rig().await?;
        let cfg = self.cfg();

        let build = match draft {
            EndpointSpec::LocalLlama(spec) => {
                let build = build_for(&rig, draft).ok_or_else(|| {
                    LaunchError::BinaryMissing(format!(
                        "no discovered build matches `{}`",
                        spec.build
                    ))
                })?;
                if build.id != spec.build {
                    warnings.push(format!(
                        "build `{}` was not discovered; falling back to `{}` ({}) — \
                         this is a substitution, not a match",
                        spec.build,
                        build.id,
                        build
                            .backends
                            .first()
                            .map(|b| format!("{b:?}"))
                            .unwrap_or_else(|| "no enumerated device".to_owned())
                    ));
                }
                if !Path::new(&build.server_path).exists() {
                    return Err(LaunchError::BinaryMissing(build.server_path.clone()).into());
                }
                Some(build)
            }
            EndpointSpec::LocalVllm(_) => None,
            other => {
                return Err(Error::Invalid {
                    what: "endpoint spec".to_owned(),
                    why: format!(
                        "{:?} is not a local endpoint; the local supervisor does not own it",
                        other.kind()
                    ),
                })
            }
        };

        if let Some(model) = model_path(draft) {
            if !Path::new(&model).exists() {
                return Err(LaunchError::ModelMissing(model).into());
            }
        }

        // A proposal, bind-probed but not held: `up` takes the reservation.
        let port = match explicit_port(draft) {
            Some(p) => p,
            None => {
                let taken: Vec<u16> = self
                    .store
                    .list_endpoints()?
                    .into_iter()
                    .filter_map(|r| r.port)
                    .collect();
                proc::alloc_port(cfg.endpoints.port_range, &taken).ok_or_else(|| {
                    Error::Invalid {
                        what: "[endpoints] port_range".to_owned(),
                        why: format!(
                            "every port in {}..={} is bound or already promised",
                            cfg.endpoints.port_range.0, cfg.endpoints.port_range.1
                        ),
                    }
                })?
            }
        };

        // Only the *preview* is bound to the proposed port; `plan.spec` keeps the operator's
        // own answer, including `None` for "allocate one for me".
        let previewed = with_port(draft, port);
        let fit = match &previewed {
            EndpointSpec::LocalLlama(s) => self.fit_for(s, &rig, &mut warnings)?,
            _ => None,
        };
        if let Some(FitVerdict::WontFit { short_by_mb }) = fit.as_ref().map(|f| f.verdict.clone()) {
            warnings.push(format!(
                "this does not fit: {short_by_mb} MiB short. `up` refuses unless forced"
            ));
        }

        // The preview names the key file only when the spec declares a credential — an
        // `--api-key-file` pointing at a path we never write would make the server refuse
        // every request with a 401 nobody could explain.
        let key_file = match &previewed {
            EndpointSpec::LocalLlama(s) if s.api_key.is_some() => {
                Some(self.paths.endpoints_dir().join("<id>.key"))
            }
            _ => None,
        };

        // THE fix for "the centrepiece is decorative": argv is rendered from the solver's
        // plan, not from the draft. `-c`, `-np`, `-ngl`, `-dev` and the KV cache type are
        // emitted explicitly instead of being left to llama.cpp's own auto-sizing, and an
        // operator value that overrules the solver is carried into the plan as an override
        // rather than quietly replacing it.
        let resolved = ResolvedSpec::resolve(draft, port, fit);
        warnings.extend(resolved.notes());
        let preview = self.argv_for(&resolved, build.as_ref(), key_file.as_deref())?;
        warnings.extend(preview.warnings.iter().cloned());

        Ok(LaunchPlan {
            spec: draft.clone(),
            argv: preview,
            fit: resolved.into_fit(),
            cost: CostEstimate::Metered {
                usd: Money(0),
                source: PriceSource::Derived,
            },
            warnings,
            port,
        })
    }

    /// Execute a plan. A local endpoint costs nothing, so `approval` is accepted and
    /// ignored — the parameter exists because the trait is shared with the vast
    /// provisioner, where it is load-bearing.
    async fn up(&self, plan: LaunchPlan, approval: Option<SpendApproval>) -> Result<Backend> {
        self.up_forced(plan, approval, false).await
    }

    /// Take an endpoint down.
    ///
    /// `Drain` waits for the upstream to report no busy slots (up to `drain_timeout_secs`)
    /// before signalling; `Now` signals immediately; `Forget` also deletes the record and the
    /// credential file. All three go through [`adopt::signallable`], so a record whose
    /// identity no longer matches is reported and **never** signalled — the alternative is
    /// killing whatever process inherited that pid.
    async fn down(&self, id: &BackendId, mode: DownMode) -> Result<()> {
        let lock = self.endpoint_lock(id.as_str());
        let _guard = lock.lock().await;

        let mut record = self
            .store
            .list_endpoints()?
            .into_iter()
            .find(|r| &r.id == id)
            .ok_or_else(|| Error::NotFound(format!("endpoint {id}")))?;

        let adoption = adopt::adopt(&record);
        match adopt::signallable(&adoption) {
            Some(facts) => {
                let facts = facts.clone();
                if mode == DownMode::Drain {
                    self.drain(&record).await;
                }
                proc::stop_graceful(&facts, TERM_WAIT, KILL_WAIT)?;
                supervisor::reap(facts.pid);
            }
            None => {
                if let Adoption::Foreign { pid, why } | Adoption::Ambiguous { pid, why } = &adoption
                {
                    tracing::warn!(%id, pid, why, "not ours: no signal sent");
                    self.emit(Event::Alert {
                        level: AlertLevel::Warning,
                        message: format!("{id}: pid {pid} is not ours ({why}); nothing was killed"),
                        action: Some(format!("apexrouter endpoint forget {id}")),
                        id: format!("foreign:{id}"),
                    });
                }
            }
        }

        match mode {
            DownMode::Forget => {
                self.store.remove_endpoint(id)?;
                let _ = std::fs::remove_file(self.key_path(id));
            }
            DownMode::Drain | DownMode::Now => {
                // Expectation IS state: the record survives as `Stopped` with no process, so
                // the UI can still show it and `up` restarts it under the same id.
                record.desired = DesiredState::Stopped;
                record.proc = None;
                self.store.put_endpoint(&record)?;
            }
        }
        self.emit(Event::BackendRemoved { id: id.clone() });
        Ok(())
    }

    /// The last `tail` lines of an endpoint's log, whether it is running or not.
    async fn logs(&self, id: &BackendId, tail: usize) -> Result<Vec<String>> {
        let record = self
            .store
            .list_endpoints()?
            .into_iter()
            .find(|r| &r.id == id);
        let path = match record.as_ref().and_then(|r| r.log_path.as_deref()) {
            Some(p) => PathBuf::from(p),
            None => self.paths.log_file(id),
        };
        Ok(supervisor::log_tail(&path, tail))
    }

    /// Re-derive live backends from persisted facts at startup.
    ///
    /// This is what makes "children outlive the manager" true rather than aspirational: a
    /// record whose identity still matches is re-adopted without so much as a signal, so a
    /// `systemctl --user restart` costs nothing. A record whose process vanished while it was
    /// supposed to be running becomes a visible `Down` backend; one that vanished while it
    /// was supposed to be stopped is simply tidied away.
    async fn reconcile(&self) -> Result<Vec<Backend>> {
        let cfg = self.cfg();
        if !cfg.supervisor.adopt_on_start {
            return Ok(Vec::new());
        }
        let now = chrono::Utc::now().timestamp();
        let mut out = Vec::new();

        for mut record in self.store.list_endpoints()? {
            // Another provisioner owns anything that is not a local process.
            if !matches!(
                record.spec,
                EndpointSpec::LocalLlama(_) | EndpointSpec::LocalVllm(_)
            ) {
                continue;
            }

            match adopt::adopt(&record) {
                Adoption::Adopted(facts) => {
                    record.proc = Some(facts);
                    record.adopted = true;
                    self.store.put_endpoint(&record)?;
                    out.push(self.backend_of(&record, None, Health::Unknown));
                }
                // NEVER signalled. It is reported, and it keeps its record so the operator
                // can decide; the port it holds is real and the table has to know.
                Adoption::Foreign { pid, why } | Adoption::Ambiguous { pid, why } => {
                    tracing::warn!(id = %record.id, pid, why, "endpoint is not ours");
                    out.push(self.backend_of(
                        &record,
                        None,
                        Health::Down {
                            reason: format!("pid {pid} is not ours: {why}"),
                            retry_at_unix: now,
                        },
                    ));
                }
                Adoption::Vanished => match record.desired {
                    DesiredState::Running => {
                        record.proc = None;
                        self.store.put_endpoint(&record)?;
                        self.emit(Event::BootProgress {
                            backend: record.id.clone(),
                            phase: BootPhase::Failed {
                                reason: "process vanished while it was supposed to be running"
                                    .to_owned(),
                            },
                            line: None,
                        });
                        out.push(self.backend_of(
                            &record,
                            None,
                            Health::Down {
                                reason: "process vanished".to_owned(),
                                retry_at_unix: now,
                            },
                        ));
                    }
                    DesiredState::Stopped => {
                        self.store.remove_endpoint(&record.id)?;
                        let _ = std::fs::remove_file(self.key_path(&record.id));
                    }
                },
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------------------
// free functions
// ---------------------------------------------------------------------------------------

/// Wait for a freshly exec'd child to publish its real `/proc/<pid>/cmdline`.
///
/// Between `fork()` and `execve()` the child still reports the *manager's* argv, so facts
/// captured immediately after a spawn can hash the wrong command line and un-adopt the child
/// on the next restart. A wrapper — a shebang script, `python3`, a `taskset` prefix — makes
/// the kernel's argv permanently different from the one we passed. So the record stores what
/// the kernel actually reports, which is what `core::proc::adopt` re-hashes.
///
/// `marker` is an argument we know is ours; until it appears, the exec has not happened.
/// That alone is not enough — `#!/usr/bin/env python3` produces *two* execs and the
/// intermediate `env` carries our arguments too — so the reading must also be **stable**
/// across two consecutive samples of `(cmdline, exe)`. Falling back to the spawn-time facts
/// after the budget expires is correct rather than merely safe: a child that never published
/// an argv has usually already exited, and the health gate is about to say so.
async fn settle_facts(spawned: &ProcFacts, marker: Option<&str>) -> ProcFacts {
    let deadline = Instant::now() + SETTLE_BUDGET;
    let mut previous: Option<(Vec<String>, String)> = None;
    while Instant::now() < deadline {
        if let (Ok(argv), Ok(exe)) = (proc::cmdline(spawned.pid), proc::exe_path(spawned.pid)) {
            let ours = match marker {
                Some(m) => argv.iter().any(|a| a == m),
                None => !argv.is_empty(),
            };
            let sample = (argv, exe);
            if ours && previous.as_ref() == Some(&sample) {
                if let Ok(facts) = proc::identify(spawned.pid, &sample.0, "") {
                    return facts;
                }
            }
            previous = Some(sample);
        }
        if matches!(proc::liveness(spawned), Liveness::Dead | Liveness::Zombie) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    spawned.clone()
}

/// Re-read a live child's identity from `/proc`.
///
/// Called once the health gate has passed, so the record describes the process **as it
/// actually is** at the moment it became routable. Without this a wrapper, a re-exec or a
/// slow interpreter could leave the record hashing an argv the kernel never had, and `adopt`
/// would then refuse to own — and therefore refuse to stop — a child we started ourselves.
/// The start-tick check makes the refresh a no-op if the pid stopped being ours in between.
fn refresh_facts(current: &ProcFacts) -> ProcFacts {
    match proc::cmdline(current.pid).and_then(|argv| proc::identify(current.pid, &argv, "")) {
        Ok(facts) if facts.start_time_ticks == current.start_time_ticks => facts,
        _ => current.clone(),
    }
}

/// The build a spec asks for, or the closest discovered one.
///
/// A fallback is *returned*, not applied silently: [`Provisioner::plan`] compares the
/// returned id against the requested one and warns when they differ, which is the visible
/// `BinaryChoice::Fallback` the UI renders. Choosing HIP because a substring matched Vulkan
/// is the defect this shape exists to make impossible.
fn build_for(rig: &RigSnapshot, spec: &EndpointSpec) -> Option<LlamaBuild> {
    let EndpointSpec::LocalLlama(s) = spec else {
        return None;
    };
    if let Some(exact) = rig.builds.iter().find(|b| b.id == s.build) {
        return Some(exact.clone());
    }
    let choice = discover::choose_build(&rig.builds, None)?;
    rig.builds.iter().find(|b| b.id == choice.chosen).cloned()
}

/// The weights path a spec names, when it names one on the local filesystem.
fn model_path(spec: &EndpointSpec) -> Option<String> {
    match spec {
        EndpointSpec::LocalLlama(s) => Some(s.model_path.clone()),
        // A vLLM `model_id` is a HuggingFace repo id far more often than a path, and
        // refusing to start because `Qwen/Qwen3-8B` is not a file would be wrong.
        _ => None,
    }
}

/// The port the operator pinned, when they pinned one.
fn explicit_port(spec: &EndpointSpec) -> Option<u16> {
    match spec {
        EndpointSpec::LocalLlama(s) => s.port,
        EndpointSpec::LocalVllm(s) => s.port,
        _ => None,
    }
}

/// The same spec, bound to a port.
fn with_port(spec: &EndpointSpec, port: u16) -> EndpointSpec {
    match spec {
        EndpointSpec::LocalLlama(s) => EndpointSpec::LocalLlama(LocalLlamaSpec {
            port: Some(port),
            ..s.clone()
        }),
        EndpointSpec::LocalVllm(s) => {
            let mut s = s.clone();
            s.port = Some(port);
            EndpointSpec::LocalVllm(s)
        }
        other => other.clone(),
    }
}

/// Are these two specs the same endpoint, up to the port it happened to get?
///
/// The port is allocated, not requested, on most launches; two specs differing only there
/// describe the same server being restarted.
fn same_endpoint(a: &EndpointSpec, b: &EndpointSpec) -> bool {
    with_port(a, 0) == with_port(b, 0)
}

/// The base URL of a local endpoint. **Never** carries a trailing `/v1` (§6.1).
fn base_url(spec: &EndpointSpec, port: u16) -> String {
    let host = match spec {
        EndpointSpec::LocalLlama(s) => s.host.as_str(),
        EndpointSpec::LocalVllm(s) => s.host.as_str(),
        _ => "127.0.0.1",
    };
    // A server bound to 0.0.0.0 is still reached over loopback from here, and `http://0.0.0.0`
    // is not a valid destination on every stack.
    let host = if host == "0.0.0.0" || host.is_empty() {
        "127.0.0.1"
    } else {
        host
    };
    format!("http://{host}:{port}")
}

/// `"/models/carnice-9b/Carnice-9b-Q6_K.gguf"` → `"Carnice-9b-Q6_K"`.
fn file_stem(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.')
        .map(|(stem, _)| stem.to_owned())
        .unwrap_or_else(|| name.to_owned())
}

/// The tags a local backend carries, which is what a `Tag` route selector matches on.
fn tags_for(spec: &EndpointSpec) -> Vec<String> {
    let mut tags = vec!["local".to_owned()];
    if let EndpointSpec::LocalLlama(s) = spec {
        for device in &s.split.devices {
            let backend = device.trim_end_matches(|c: char| c.is_ascii_digit());
            if !backend.is_empty() {
                let tag = format!("gpu:{}", backend.to_ascii_lowercase());
                if !tags.contains(&tag) {
                    tags.push(tag);
                }
            }
        }
    }
    tags
}

/// `Health::Ready` from a successful probe.
fn ready_health(probe: &UpstreamProbe) -> Health {
    Health::Ready {
        since_unix: chrono::Utc::now().timestamp(),
        slots_busy: probe.slots_busy.unwrap_or(0),
        slots_total: probe.slots_total.unwrap_or(1),
        tps_p50: None,
    }
}

#[cfg(test)]
mod tests;
