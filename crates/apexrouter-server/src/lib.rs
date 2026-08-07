//! OWNER: unit S-01 (server/src/{lib,state,shutdown}.rs). Do not edit outside that unit —
//! every other module here belongs to a different work unit.
//!
//! The axum application: the proxy listener, the control listener, `/ws`, auth, embedded
//! assets and the job runner.
//!
//! **Two listeners in one process**, because a single listener cannot satisfy both
//! contracts. The proxy is a catch-all by contract; a catch-all `any()` route and the
//! static-asset `get("/{*path}")` route panic on `Router::merge` in axum 0.8, and a shared
//! listener would permanently shadow llama.cpp's own `/health` for control clients. Two
//! listeners also make it possible to expose the proxy to the LAN without exposing the
//! control plane.
//!
//! [`api_router`] is `pub` so ApexOS-RS can mount the control plane in its own process.
//!
//! # Startup order (ARCHITECTURE §1.3), which is not negotiable
//!
//! [`serve`] is [`build_state`] then [`run`]. `build_state` ensures the layout, refuses an
//! unauthenticated non-loopback bind, writes the owner record and constructs every
//! subsystem; `run` **reconciles before it binds**, so the table is never armed with a
//! picture that predates adoption, then binds both listeners, starts the pollers, and
//! drains on the shutdown flag.
//!
//! # CORS on the proxy listener
//!
//! LocalRouter's `endpoint_proxy.py` put `Access-Control-Allow-Origin: *` on **every**
//! proxied response. ApexRouter does not, by default — this proxy resolves credentials, and
//! a blanket `*` would sit awkwardly beside §9.3's mutation-origin gate. It is configurable
//! rather than silently dropped: `[server] proxy_cors_origins` is empty by default (no
//! header at all, today's behaviour), lists origins to echo, and accepts the single entry
//! `"*"` as an explicit opt-in to the old behaviour. The control listener and every mutating
//! route are never covered — see [`cors_middleware`].

// The modules still carrying `#[allow(unused)]` are stubs owned by units S-02 through S-05.
// The allow is per-module rather than crate-wide so that everything already wired up stays
// fully linted, and **each line comes off as its owning unit lands**. S-01's own three files
// are linted with no allowances.
#[allow(unused)]
pub mod api;
#[allow(unused)]
pub mod assets;
#[allow(unused)]
pub mod auth;
pub mod fleet;
#[allow(unused)]
pub mod jobs;
#[allow(unused)]
pub mod prober;
pub mod shutdown;
pub mod state;
#[allow(unused)]
pub mod watcher;
#[allow(unused)]
pub mod ws;

pub use shutdown::{Shutdown, ShutdownHandle};
pub use state::AppState;

use anyhow::Context as _;
use apexrouter_core::checks::Registry;
use apexrouter_core::config::Config;
use apexrouter_core::lockfile::{DaemonLock, OwnerRecord};
use apexrouter_core::paths::Paths;
use apexrouter_core::proc;
use apexrouter_core::store::Store;
use apexrouter_core::usage::UsageWriter;
use apexrouter_protocol::{AlertLevel, BackendId, Event, PRODUCT, VERSION};
use apexrouter_providers::local::{DownMode, LocalProvisioner, Provisioner as _};
use apexrouter_router::TableBuilder;
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json};
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tower_http::classify::{ServerErrorsAsFailures, SharedClassifier};
use tower_http::trace::TraceLayer;

/// Events buffered per subscriber before a slow one is `Lagged`.
///
/// Generous, because `RequestStarted`/`RequestFinished` arrive in pairs at request rate, and
/// affordable, because an `Event` is one boxed enum. A subscriber that still falls behind is
/// re-synchronised by `/ws` re-sending a whole `Snapshot` (S-05), which is what makes a
/// bounded channel safe here at all.
const EVENT_CAPACITY: usize = 1024;

/// How long the `EADDRINUSE` diagnostic gets to ask the squatter who it is.
///
/// Short on purpose: this runs on the startup path and the answer is a nicety. A port held
/// by something that does not answer at all is still reported, just less specifically.
const HOLDER_PROBE: Duration = Duration::from_millis(1500);

// =======================================================================================
// startup
// =======================================================================================

/// Run the daemon: lock, load, reconcile, arm the table, bind both listeners, start the
/// pollers, and drain on signal.
///
/// Startup order matters and is not negotiable — **reconcile before binding**, so the table
/// is never armed with a picture that predates adoption.
///
/// # Errors
/// A refused bind (see [`bind_listener`]), an unauthenticated non-loopback bind, an
/// unwritable state directory, or a listener that stops before it was asked to.
pub async fn serve(paths: Paths, cfg: Config, lock: DaemonLock) -> anyhow::Result<()> {
    let (trigger, handle) = shutdown::channel();
    tokio::spawn(async move {
        let sig = shutdown::wait_for_terminate().await;
        tracing::info!(signal = sig, "signal received; draining");
        trigger.trigger();
    });
    serve_with_shutdown(paths, cfg, lock, handle).await
}

/// [`serve`] with the shutdown flag supplied by the caller instead of by `tokio::signal`.
///
/// This is the form an embedder — ApexOS-RS, or a test — uses, because installing a
/// process-wide `SIGTERM` handler is not a library's decision to make.
///
/// It is also where **every process-global slot the control plane reads** is filled:
/// `spawn_shutdown_bridges`' notifier, and S-07's provider clients. That is the same ruling
/// in both cases, for the same reason — one process runs one daemon, so a global is correctly
/// scoped in production, and a test process runs many, so a test that builds an [`AppState`]
/// by hand must not have another daemon's client appear underneath it.
///
/// # Errors
/// As [`serve`].
pub async fn serve_with_shutdown(
    paths: Paths,
    cfg: Config,
    lock: DaemonLock,
    shutdown: ShutdownHandle,
) -> anyhow::Result<()> {
    let state = build_state(paths, cfg, lock).await?;
    install_provider_clients(&state);
    let (trigger, local) = shutdown::channel();
    spawn_shutdown_bridges(&trigger, &shutdown);
    run(state, local).await
}

/// Fill S-07's provider slots — the vast client, the HuggingFace client and the tunnel
/// supervisor — from the running configuration.
///
/// S-07 publishes `install_*` for each and documents them as "called once at daemon start,
/// and by nothing else"; this is that call site, and until it existed every `/v1/vast/*` and
/// `/v1/hf/*` route answered `503 vast_unavailable` / `503 hf_unavailable` no matter what
/// credentials the machine held. The routes were mounted and the clients were absent, which
/// is the mounting defect one level down.
///
/// **Nothing here can stop the daemon.** A missing vast key is the ordinary state of a
/// machine that does not rent GPUs, and the `503` those routes already answer names the fix
/// precisely; a daemon that refused to start over it would be a daemon that cannot serve the
/// local models it was actually asked for. Each failure is one `info` line and nothing else —
/// not even an alert, because "no vast key" is a configuration fact, not a fault.
fn install_provider_clients(state: &Arc<AppState>) {
    let cfg = state.cfg();

    match apexrouter_providers::vast::VastApiHttp::from_config(&cfg, &state.paths) {
        Ok(api) => {
            tracing::info!(base = %api.base_url(), "vast client installed");
            api::vast::install_vast_api(Some(Arc::new(api)));
        }
        Err(e) => {
            tracing::info!(reason = %e, "no vast client: /v1/vast/* will answer 503 until a key is configured");
            api::vast::install_vast_api(None);
        }
    }

    match apexrouter_providers::hf::HfClient::new(&cfg, &state.paths) {
        Ok(hf) => {
            tracing::info!("huggingface client installed");
            api::hf::install_hf_source(Some(Arc::new(hf)));
        }
        Err(e) => {
            tracing::info!(reason = %e, "no huggingface client: /v1/hf/* will answer 503");
            api::hf::install_hf_source(None);
        }
    }

    // The tunnel supervisor needs no credential — it shells out to `ssh` — so it is always
    // installed, and `GET /v1/tunnels` answers an empty list rather than a `503`.
    api::vast::install_tunnels(Some(Arc::new(
        apexrouter_providers::ssh::TunnelSupervisor::new(
            state.paths.clone(),
            (*cfg).clone(),
            state.tx.clone(),
        ),
    )));
}

/// Steps 1–5 of ARCHITECTURE §1.3: the layout, the bind-exposure refusal, the owner record,
/// and every subsystem.
///
/// Split out from [`run`] so a caller can hold the [`AppState`] it is about to serve — which
/// is what makes "did reconciliation run before the bind?" and "did `SIGHUP` swap the
/// config?" observable facts rather than log lines.
///
/// # Errors
/// [`anyhow::Error`] when the state layout cannot be created, when a non-loopback listener
/// is configured with no token, or when the owner record cannot be written.
pub async fn build_state(
    paths: Paths,
    cfg: Config,
    mut lock: DaemonLock,
) -> anyhow::Result<Arc<AppState>> {
    paths
        .ensure_layout()
        .context("creating the state and cache layout")?;

    let proxy_addr = cfg.proxy_bind();
    let control_addr = cfg.control_bind();
    refuse_unauthenticated_exposure(&cfg, proxy_addr, control_addr)?;

    let pid = std::process::id();
    lock.write_owner(&OwnerRecord {
        pid,
        start_time_ticks: proc::start_time_ticks(pid).unwrap_or(0),
        boot_id: proc::boot_id().unwrap_or_default(),
        version: VERSION.to_owned(),
        proxy_url: url_for(proxy_addr),
        control_url: url_for(control_addr),
        started_at_unix: chrono::Utc::now().timestamp(),
    })
    .context("writing the daemon owner record")?;

    let (tx, _rx) = broadcast::channel(EVENT_CAPACITY);
    let usage = UsageWriter::open(&paths, &cfg.compat).context("opening the usage log")?;
    let router = apexrouter_router::RouterInner::new(Arc::new(cfg.clone()), tx.clone(), usage);
    let supervisor = Arc::new(LocalProvisioner::new(
        paths.clone(),
        cfg.clone(),
        tx.clone(),
    ));

    // `core` defines the `Check` trait without ever depending on `providers`; the daemon is
    // what puts the two together, and it is the **only** thing that can — which is why both
    // lists are registered here rather than by the module that runs them. Registration order
    // is display order and the two lists continue each other: this machine first, then the
    // things that cost money, then the things that carry traffic.
    let mut checks = Registry::new();
    for c in apexrouter_core::checks::local_checks() {
        checks.register(c);
    }
    for c in apexrouter_providers::checks::provider_checks() {
        checks.register(c);
    }

    // `[server] ui_dir` is the live-reload hatch for `ui-web/`; `""` means the embedded copy.
    assets::set_ui_dir(ui_dir_of(&cfg));

    let store = Store::new(paths.clone());
    let state = Arc::new(AppState::new(
        paths,
        cfg,
        store,
        router,
        tx,
        supervisor,
        Arc::new(checks),
        lock,
    ));

    // The live-request ring has to be listening BEFORE anything can be served. R-08 only
    // serialises `RequestFinished` while the broadcast has a receiver — the rule that keeps a
    // router at 50 rps from drowning its own dashboard — so a log that attaches lazily on
    // first query is *structurally* blind to everything before that query. Both GUIs showed
    // an empty live-request panel on first load for exactly this reason, and it self-healed
    // from the first poll, which is the signature of a missing startup attach rather than a
    // broken ring. `api::requests::attach` is idempotent per channel, so the handlers' own
    // lazy call becomes a no-op that returns this same log.
    api::requests::attach(&state.tx);

    // Prometheus counters: same bus, same startup rule. Without a subscriber at boot,
    // `GET /metrics` would only ever show gauges from the registry.
    attach_telemetry(&state);

    // Jobs: crash-recovery and WS broadcasts must be live *before* any handler can
    // `spawn_with`. Lazy `ensure_wired` on the first `/v1/jobs` call left Pending rows from a
    // previous daemon open, and UI endpoint boots via `?no_wait=true` never hit that path —
    // jobs stayed memory-only. Wiring here makes every spawn site a no-op for ensure.
    state.jobs.ensure_wired(&state.tx, &state.paths);

    Ok(state)
}

/// Feed [`AppState::telemetry`] from `RequestFinished` events for the life of the process.
fn attach_telemetry(state: &Arc<AppState>) {
    let mut rx = state.tx.subscribe();
    let telemetry = Arc::clone(&state.telemetry);
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(Event::RequestFinished { record }) => telemetry.record(*record),
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::debug!(missed = n, "telemetry lagged the broadcast");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// Steps 6–9: reconcile, arm, bind, poll, serve, drain.
///
/// Deliberately **not** wired to `POST /v1/shutdown` — [`serve_with_shutdown`] does that,
/// via [`spawn_shutdown_bridges`], so that an embedder mounting the daemon inside a larger
/// process (and a test running two of them) keeps one shutdown per `run`, not one per
/// process.
///
/// # Errors
/// A refused bind, or a listener task that ended before the shutdown flag was set.
pub async fn run(state: Arc<AppState>, shutdown: ShutdownHandle) -> anyhow::Result<()> {
    // ---- 6/7. reconcile and arm the table, BEFORE anything can be served ---------------
    reconcile_on_start(&state).await?;

    // ---- 8. bind both listeners --------------------------------------------------------
    let cfg = state.cfg();
    let proxy_addr = cfg.proxy_bind();
    let control_addr = cfg.control_bind();
    let proxy_listener = bind_listener(proxy_addr, "proxy").await?;
    let control_listener = bind_listener(control_addr, "control").await?;
    tracing::info!(proxy = %proxy_addr, control = %control_addr, "listening");

    // ---- 9. the pollers ----------------------------------------------------------------
    tokio::spawn(prober::health_prober(Arc::clone(&state)));
    tokio::spawn(watcher::config_watcher(Arc::clone(&state)));
    tokio::spawn(fleet::vast_fleet_poller(Arc::clone(&state)));
    // Alert-only ledger ↔ live fleet compare — not on the bind critical path, never destroys.
    tokio::spawn(fleet::reconcile_ledger_once(Arc::clone(&state)));
    spawn_sighup_reloader(Arc::clone(&state), shutdown.clone());

    let proxy_app = proxy_app(&state);
    let control_app = api_router(Arc::clone(&state));

    let sd = shutdown.clone();
    let mut proxy_task = tokio::spawn(async move {
        axum::serve(
            proxy_listener,
            proxy_app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { sd.wait().await })
        .await
    });

    let sd = shutdown.clone();
    let mut control_task = tokio::spawn(async move {
        axum::serve(
            control_listener,
            control_app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { sd.wait().await })
        .await
    });

    tokio::select! {
        r = &mut proxy_task   => return listener_died("proxy", r),
        r = &mut control_task => return listener_died("control", r),
        () = shutdown.wait() => {}
    }

    // ---- drain, to the deadline in force *now* (a reload may have changed it) ----------
    let deadline = Duration::from_secs(state.cfg().server.drain_timeout_secs);
    if shutdown::drain(vec![proxy_task, control_task], deadline).await {
        tracing::info!("drained");
    } else {
        tracing::warn!(
            secs = deadline.as_secs(),
            "drain deadline expired; the remaining connections are being dropped"
        );
    }

    stop_children_if_configured(&state).await;
    Ok(())
}

/// Compute `Adoption` for every endpoint record and re-adopt or tidy, **before** the
/// listeners bind.
///
/// Vast reconciliation is deliberately **not** on this path: it is a network call and the
/// laptop is often offline. It runs in a background task and raises an `Alert`.
///
/// Neither a supervisor failure nor a broken `routes.json` aborts startup. A daemon that
/// refuses to come up because one endpoint record went stale is a daemon that cannot be used
/// to fix the endpoint record; the failure is reported as an `Alert`, the previous table
/// keeps serving, and resolution rules 2–4 still reach every registered backend.
///
/// # Errors
/// Reserved for a genuinely unusable state directory. Everything recoverable is an alert.
pub async fn reconcile_on_start(state: &Arc<AppState>) -> anyhow::Result<()> {
    let cfg = state.cfg();

    let mut backends = match state.supervisor.reconcile().await {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, "local reconciliation failed");
            state.alert(
                AlertLevel::Serious,
                "reconcile.local",
                format!("could not reconcile local endpoints: {e}"),
            );
            Vec::new()
        }
    };

    // Registered nodes and managed providers have no process to adopt; they come straight
    // off disk, and never override what adoption just established.
    let adopted: HashSet<BackendId> = backends.iter().map(|b| b.id.clone()).collect();
    match state.store.load_backends() {
        Ok(stored) => backends.extend(stored.into_iter().filter(|b| !adopted.contains(&b.id))),
        Err(e) => {
            tracing::warn!(error = %e, "backends.json could not be read");
            state.alert(
                AlertLevel::Warning,
                "reconcile.backends",
                format!("could not read backends.json: {e}"),
            );
        }
    }

    let count = backends.len();
    for b in backends {
        state.router.registry().upsert(b, &cfg.router);
    }
    tracing::info!(backends = count, "reconciled");

    arm_table(state, &cfg);
    Ok(())
}

/// Compile `routes.json` against the live registry and swap it in.
///
/// A failed compile **keeps the table that is already serving** and raises an alert — at
/// startup that is the empty table, which still reaches every registered backend through
/// resolution rules 2–4.
fn arm_table(state: &Arc<AppState>, cfg: &Config) {
    let file = match state.store.load_routes() {
        Ok(f) => f,
        Err(e) => {
            tracing::error!(error = %e, "routes.json could not be read");
            state.alert(
                AlertLevel::Serious,
                "routes.load",
                format!("could not read routes.json: {e}"),
            );
            return;
        }
    };

    match TableBuilder::compile(cfg, &file, state.router.registry()) {
        Ok(table) => {
            let generation = table.generation();
            state.router.store_table(table);
            tracing::info!(
                routes = file.routes.len(),
                generation,
                "routing table armed"
            );
            state.emit(Event::RouteTableChanged {
                routes: file.routes,
                valid: true,
                error: None,
            });
        }
        Err(report) => {
            let why = report
                .issues
                .iter()
                .map(|i| format!("{}: {}", i.field, i.message))
                .collect::<Vec<_>>()
                .join("; ");
            tracing::error!(
                issues = %why,
                "the routing table did not compile; the previous one keeps serving"
            );
            state.alert(
                AlertLevel::Serious,
                "routes.compile",
                format!("routes.json did not compile: {why}"),
            );
            state.emit(Event::RouteTableChanged {
                routes: file.routes,
                valid: false,
                error: Some(why),
            });
        }
    }
}

/// Re-read `config.toml` and re-arm the table. `SIGHUP`, `POST /v1/reload` and the config
/// watcher all land here.
///
/// A parse failure changes nothing: the running configuration and the running table keep
/// serving, and the operator gets an alert naming the file.
///
/// # Errors
/// The parse or read failure, so `POST /v1/reload` can render it. The daemon keeps running
/// either way.
pub async fn reload_config(state: &Arc<AppState>) -> anyhow::Result<()> {
    let path = state.paths.config_file();
    let parsed = tokio::task::spawn_blocking(move || Config::load_from(Some(&path), None))
        .await
        .context("the config reload task panicked")?;

    let cfg = match parsed {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "config reload failed; the running config keeps serving");
            state.alert(
                AlertLevel::Warning,
                "config.reload",
                format!("config.toml did not parse; the running config keeps serving: {e}"),
            );
            return Err(anyhow::Error::new(e).context("reloading config.toml"));
        }
    };

    let cfg = apply_config(state, cfg);
    arm_table(state, &cfg);
    tracing::info!("config reloaded");
    Ok(())
}

/// Publish a freshly parsed configuration to **every** place the daemon keeps one, and
/// return it shared.
///
/// There are four copies and none of them is optional:
///
/// * `AppState.cfg` — what the control plane renders and what `recompile` compiles against;
/// * `RouterInner.cfg` — what the **request path** reads on every request, which is where
///   `[router] anthropic_tools`, `unknown_model`, `max_body_bytes` and the timeouts live;
/// * the supervisor's copy — what the *next* endpoint launch uses (a reload never evicts a
///   model that took 90 seconds and 6 GB to load);
/// * `assets`' `[server] ui_dir` — the live-reload hatch for `ui-web/`.
///
/// This function exists because forgetting one of them is not a partial reload, it is a
/// reload that answers `{"ok":true,"issues":[]}` and changes nothing. `POST /v1/reload`
/// updated the first and third and left the request path frozen at daemon start; `SIGHUP`
/// updated the first, third and fourth. Both now land here, so the set can only be got
/// wrong in one place.
///
/// What a reload still cannot change is listed on [`apexrouter_router::RouterInner::store_cfg`]
/// (`connect_timeout_ms`, `max_inflight_bytes`) and, one level up, the two bind addresses:
/// a listener is bound once. Those need a restart.
pub fn apply_config(state: &Arc<AppState>, cfg: Config) -> Arc<Config> {
    let shared = Arc::new(cfg);
    state.cfg.store(Arc::clone(&shared));
    state.router.store_cfg(Arc::clone(&shared));
    state.supervisor.set_config((*shared).clone());
    assets::set_ui_dir(ui_dir_of(&shared));
    shared
}

/// Feed one shutdown flag from every source that can ask for one.
///
/// Two today: the handle the caller supplied (which [`serve`] drives from `SIGINT` and
/// `SIGTERM`), and `POST /v1/shutdown`, which S-03 signals through a **process-global**
/// `Notify` because `AppState` publishes no channel for it.
///
/// That globalness is why this lives in [`serve_with_shutdown`] rather than in [`run`]:
/// one process runs one daemon, so the notifier is correctly scoped in production, but a
/// test process runs many and must be able to start one that is not listening to it.
fn spawn_shutdown_bridges(trigger: &Shutdown, external: &ShutdownHandle) {
    let t = trigger.clone();
    let ext = external.clone();
    tokio::spawn(async move {
        ext.wait().await;
        t.trigger();
    });

    let t = trigger.clone();
    tokio::spawn(async move {
        let notify = api::shutdown_notify();
        let fut = notify.notified();
        tokio::pin!(fut);
        // Register the waiter before anything can notify it. The task is spawned before
        // `axum::serve` starts accepting, so no `POST /v1/shutdown` can land in the gap.
        //
        // `api::shutdown_requested()` — the sticky flag beside the notifier — is
        // deliberately **not** consulted: it is process-global and never cleared, so a
        // second daemon in the same process (which is only ever a test) would inherit the
        // first one's shutdown and exit before serving a single request.
        fut.as_mut().enable();
        fut.await;
        tracing::info!("POST /v1/shutdown: draining");
        t.trigger();
    });
}

/// Reload on every `SIGHUP` until shutdown. **`SIGHUP` never exits the daemon.**
fn spawn_sighup_reloader(state: Arc<AppState>, shutdown: ShutdownHandle) {
    use tokio::signal::unix::{signal, SignalKind};

    let mut hup = match signal(SignalKind::hangup()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "cannot install a SIGHUP handler; reload is /v1/reload only");
            return;
        }
    };
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = hup.recv() => {
                    tracing::info!("SIGHUP: reloading configuration");
                    let _ = reload_config(&state).await;
                }
                () = shutdown.wait() => break,
            }
        }
    });
}

/// Honour `[supervisor] kill_children_on_exit`, which defaults to **false**.
///
/// With the default this does nothing at all: children are `setsid`, they outlive
/// `systemctl --user restart apexrouterd`, and a manager that killed a 90-second model load
/// on every restart would be the single worst property of the old design. Vast instances are
/// never touched here at any setting — a shutdown must not delete a paid box.
async fn stop_children_if_configured(state: &Arc<AppState>) {
    let cfg = state.cfg();
    if !cfg.supervisor.kill_children_on_exit {
        return;
    }
    let records = match state.store.list_endpoints() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "kill_children_on_exit: cannot list endpoints");
            return;
        }
    };
    for rec in records {
        if rec.proc.is_none() {
            continue;
        }
        tracing::info!(id = %rec.id, "kill_children_on_exit: stopping");
        if let Err(e) = state.supervisor.down(&rec.id, DownMode::Now).await {
            tracing::warn!(id = %rec.id, error = %e, "could not stop child");
        }
    }
}

// =======================================================================================
// binding
// =======================================================================================

/// Bind one listener, and turn `EADDRINUSE` into a sentence that names the squatter.
///
/// ARCHITECTURE §1.2: the daemon probes the port's `/health` and reports the foreign holder
/// **by name**, because "Address already in use" sends an operator to `ss -ltnp` while
/// "held by something answering `product=localrouter`" sends them to the thing they actually
/// have to stop.
///
/// # Errors
/// [`anyhow::Error`] carrying that message on `EADDRINUSE`; the underlying I/O error, with
/// context, on anything else.
pub async fn bind_listener(addr: SocketAddr, which: &str) -> anyhow::Result<TcpListener> {
    match TcpListener::bind(addr).await {
        Ok(l) => Ok(l),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            anyhow::bail!(
                "the {which} listener cannot bind {addr}: {}",
                describe_holder(addr).await
            )
        }
        Err(e) => Err(anyhow::Error::new(e).context(format!(
            "binding the {which} listener to {addr}; check the address is one this host has"
        ))),
    }
}

/// Ask whoever holds `addr` who they are, and phrase the answer as an instruction.
async fn describe_holder(addr: SocketAddr) -> String {
    let port = addr.port();
    let probe_at = probe_addr(addr);

    let product = match reqwest::Client::builder().timeout(HOLDER_PROBE).build() {
        Ok(http) => match http.get(format!("http://{probe_at}/health")).send().await {
            Ok(res) => res
                .json::<serde_json::Value>()
                .await
                .ok()
                .and_then(|v| v.get("product").and_then(|p| p.as_str()).map(str::to_owned)),
            Err(_) => None,
        },
        Err(_) => None,
    };

    match product.as_deref() {
        Some("localrouter") => format!(
            "port {port} is held by something answering `product=localrouter` — stop \
             `endpoint_proxy.py` first"
        ),
        Some(PRODUCT) => format!(
            "port {port} is held by another apexrouter daemon — stop it \
             (`apexrouter shutdown`), or read $STATE/apexrouterd.lock to see whose it is"
        ),
        Some(other) => format!(
            "port {port} is held by something answering `product={other}` — stop it, or \
             change the bind in config.toml"
        ),
        None => format!(
            "port {port} is held by a process that does not answer /health — find it with \
             `ss -ltnp 'sport = :{port}'`, or change the bind in config.toml"
        ),
    }
}

/// §9.1: a non-loopback bind refuses to start unless auth is configured, and the message
/// carries the fix.
///
/// The token is read from the env var `[server] token_env` names, which is also where the
/// auth middleware (S-02) reads it, so a listener can never come up looking authenticated
/// while being open.
fn refuse_unauthenticated_exposure(
    cfg: &Config,
    proxy: SocketAddr,
    control: SocketAddr,
) -> anyhow::Result<()> {
    let exposed: Vec<(&str, SocketAddr)> = [("proxy", proxy), ("control", control)]
        .into_iter()
        .filter(|(_, a)| !a.ip().is_loopback())
        .collect();
    let Some((first, _)) = exposed.first() else {
        return Ok(());
    };

    let var = cfg.server.token_env.trim();
    let have_token = !var.is_empty()
        && std::env::var(var)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
    if have_token {
        return Ok(());
    }

    let which = exposed
        .iter()
        .map(|(n, a)| format!("{n} on {a}"))
        .collect::<Vec<_>>()
        .join(" and ");
    anyhow::bail!(
        "refusing to start: the {which} listener is not on loopback and no token is \
         configured. Export a token in ${var} (any long random string), or set \
         [server] {first}_bind back to 127.0.0.1."
    )
}

/// `[server] ui_dir` as an option: `""` means "serve the copy compiled into the binary".
fn ui_dir_of(cfg: &Config) -> Option<std::path::PathBuf> {
    let d = cfg.server.ui_dir.trim();
    (!d.is_empty()).then(|| std::path::PathBuf::from(d))
}

/// A URL a client can actually use for a bound address: `0.0.0.0` is a bind, not a
/// destination, so the owner record advertises loopback for it.
fn url_for(addr: SocketAddr) -> String {
    if addr.ip().is_unspecified() {
        format!("http://127.0.0.1:{}", addr.port())
    } else {
        format!("http://{addr}")
    }
}

/// Where to send the `EADDRINUSE` diagnostic probe. Never off-host.
fn probe_addr(addr: SocketAddr) -> SocketAddr {
    if addr.ip().is_unspecified() {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), addr.port())
    } else {
        addr
    }
}

/// Turn "a listener task ended on its own" into an error, since nothing asked it to.
fn listener_died(
    which: &str,
    joined: Result<std::io::Result<()>, tokio::task::JoinError>,
) -> anyhow::Result<()> {
    match joined {
        Ok(Ok(())) => anyhow::bail!("the {which} listener stopped before it was asked to"),
        Ok(Err(e)) => Err(anyhow::Error::new(e).context(format!("the {which} listener failed"))),
        Err(e) => Err(anyhow::Error::new(e).context(format!("the {which} listener task panicked"))),
    }
}

// =======================================================================================
// the two axum applications
// =======================================================================================

/// The control-plane `Router`. Exported so ApexOS-RS can mount it.
///
/// `/health` is public (§6.2); everything else sits behind S-02's auth middleware. The `/v1`
/// modules are merged in [`v1_routes`], which is the one place a later unit adds a line.
///
/// [`auth::ListenerBind`] is installed so the Host allowlist classifies against this
/// listener's address rather than only the configured bind string.
pub fn api_router(state: Arc<AppState>) -> axum::Router {
    let bind = auth::ListenerBind(state.cfg().control_bind());
    // `/health` and `/metrics` are the only unversioned public control paths (CHARTER D3 /
    // ARCHITECTURE §6.2). Metrics is scrape-friendly: loopback bypass covers Prometheus on
    // the same host; a non-loopback bind still requires the bearer for everything else.
    let public = axum::Router::new()
        .route("/health", get(control_health).head(control_health))
        .route("/metrics", get(control_metrics));

    let authed = v1_routes().route_layer(axum::middleware::from_fn_with_state(
        Arc::clone(&state),
        auth::require_auth,
    ));

    public
        .merge(authed)
        .fallback(control_not_found)
        .with_state(state)
        // The embedded web UI, last, because it owns "/" and "/{*path}" and refuses to
        // shadow `/v1` or `/health` itself. It is deliberately *outside* the auth layer:
        // the page has to load before it can present a token.
        .merge(assets::static_router())
        // No `CorsLayer`, ever, on this listener: the embedded UI is same-origin and §9.3's
        // mutation gate is stronger than a CORS policy. `[server] proxy_cors_origins`
        // deliberately does not reach here.
        //
        // axum applies layers outside-in (first `.layer` = outermost). Trace is outermost
        // so every refusal is logged; `ListenerBind` wraps the routes so `require_auth`
        // finds it on the request extensions.
        .layer(trace_layer())
        .layer(Extension(bind))
}

/// Every authenticated control route, in one place.
///
/// **This is the only merge point.** Each API module publishes a
/// `pub fn router() -> axum::Router<Arc<AppState>>` and gets exactly one line here;
/// `api::router()` is S-03's own aggregate of `snapshot`, `backends`, `routes` and
/// `endpoints`. Only S-01 may edit this file — deliberately, because route-table assembly is
/// exactly the file where two agents merging blind produce an axum panic at startup rather
/// than a compile error.
///
/// # Every module in `api/` must appear below
///
/// Twice now — Stage 4's `catalog` and Stage 5's `{vast, hf, providers, checks, compare}` —
/// a module shipped implemented, unit-tested green and **unreachable**, because its one line
/// here was written as prose instead of as code. A unit test cannot catch that: it builds the
/// module's own `Router` and never sees the composed application.
///
/// `tests/mounted_routes.rs` is the guard. It enumerates every `pub fn router()` under
/// `src/api/`, boots the real application, and asks axum itself — through the `Allow` header
/// of a `405` — which methods the composed router serves at each of that module's paths. A
/// module that is missing from the list below fails the build. **Do not write a merge as a
/// doc comment; write it as a line of code, or the guard is the only thing that will tell
/// you.**
fn v1_routes() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route("/ws", get(ws::ws_handler))
        .merge(api::router())
        .merge(api::rig::router())
        .merge(api::fit::router())
        .merge(api::catalog::router())
        .merge(api::usage::router())
        .merge(api::requests::router())
        .merge(api::jobs::router())
        .merge(api::vast::router())
        .merge(api::hf::router())
        .merge(api::providers::router())
        .merge(api::checks::router())
        .merge(api::compare::router())
        .merge(api::migrate::router())
}

/// `GET /health` on the control listener: `{ok, product, version}` plus uptime (§6.2).
///
/// Public, and it never probes a backend — this is the liveness answer a supervisor polls.
async fn control_health(State(s): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": true,
        "product": PRODUCT,
        "version": VERSION,
        "uptime": s.uptime_secs(),
    }))
}

/// `GET /metrics` — Prometheus text exposition (ARCHITECTURE §4.5 / R-07).
///
/// Public on the control listener so a local scraper does not need a bearer. Body is pure
/// text; gauges come from the live registry, counters from `RequestFinished` via
/// [`attach_telemetry`]. Rig VRAM gauges are omitted here (would require a network-free
/// scan on every scrape); backend gauges and request counters are the product signal.
async fn control_metrics(State(s): State<Arc<AppState>>) -> Response {
    let body = s.telemetry.prometheus(s.router.registry(), None);
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
        )],
        body,
    )
        .into_response()
}

/// The control listener's 404. JSON, because every client of this listener parses JSON.
async fn control_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({
            "error": { "type": "not_found",
                       "message": "no such control route; see GET /v1/snapshot" }
        })),
    )
        .into_response()
}

/// The proxy-plane `Router`: R-08's catch-all, the mutation gate, tracing and the optional
/// CORS header.
///
/// The catch-all lives inside `proxy_router` as `.fallback(any(proxy_handler))`, **not** as a
/// `/{*path}` route, so no `Router::merge` overlap can exist.
///
/// **§9.3's mutation gate is mounted here.** The proxy is unauthenticated for inference, but
/// it still serves one mutation — `POST /switch` — and a DNS-rebinding page that makes
/// `Host: evil.example:8888` same-origin must not reach it. Rules 1 and 3 live in
/// [`auth::mutation_gate`]; rule 2 is also enforced inside the switch handler as defence in
/// depth. Inference paths are `Scope::Read` and pass the gate untouched.
pub fn proxy_app(state: &Arc<AppState>) -> axum::Router {
    let bind = auth::ListenerBind(state.cfg().proxy_bind());
    // axum applies layers outside-in (first `.layer` = outermost). Order:
    //   request → trace → ListenerBind → mutation_gate → cors → handler
    // so a rebinding refusal is logged, never CORS-decorated, and sees the bind extension.
    apexrouter_router::proxy_router(Arc::clone(&state.router))
        .layer(trace_layer())
        .layer(Extension(bind))
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(state),
            auth::mutation_gate,
        ))
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(state),
            cors_middleware,
        ))
}

/// `[server] proxy_cors_origins`, applied to the **proxy** listener only.
///
/// Empty (the default) is a no-op: not one header is added, which is ApexRouter's behaviour
/// as shipped and a deliberate, documented difference from LocalRouter's blanket
/// `Access-Control-Allow-Origin: *`.
///
/// Two things are never covered, whatever is configured:
///
/// * the **control** listener, which does not mount this layer at all;
/// * every **mutating** proxy route — `POST /switch` — because that one retargets the
///   default alias and can carry a `base_url`, which makes it a credential-exfiltration
///   primitive rather than a read. §9.3's mutation gate governs it regardless.
pub async fn cors_middleware(State(s): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let cfg = s.cfg();
    let allowed = &cfg.server.proxy_cors_origins;
    if allowed.is_empty() {
        return next.run(req).await;
    }
    if is_proxy_mutation(req.uri().path()) {
        return next.run(req).await;
    }

    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let allow = allow_origin_value(allowed, origin.as_deref());

    // A preflight for a request that would be allowed is answered here; anything else falls
    // through to the proxy untouched.
    if req.method() == Method::OPTIONS
        && req
            .headers()
            .contains_key(header::ACCESS_CONTROL_REQUEST_METHOD)
    {
        return match allow {
            Some(value) => preflight(value),
            None => next.run(req).await,
        };
    }

    let mut res = next.run(req).await;
    if let Some(value) = allow {
        let h = res.headers_mut();
        h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
        // Even for `*`: a cache in front of us must not serve a header derived from one
        // client's `Origin` to another.
        h.append(header::VARY, HeaderValue::from_static("origin"));
    }
    res
}

/// The `Access-Control-Allow-Origin` value for this request, or `None` for "say nothing".
///
/// `"*"` is the explicit opt-in to LocalRouter's old behaviour and is emitted whether or not
/// the request carried an `Origin`. Any other entry is matched **exactly** against the
/// request's `Origin` and echoed back, which is the only form a browser will combine with
/// credentials.
fn allow_origin_value(allowed: &[String], origin: Option<&str>) -> Option<HeaderValue> {
    if allowed.iter().any(|a| a == "*") {
        return Some(HeaderValue::from_static("*"));
    }
    let origin = origin?;
    if allowed.iter().any(|a| a == origin) {
        HeaderValue::from_str(origin).ok()
    } else {
        None
    }
}

/// A minimal preflight answer, so a configured origin is actually usable from a browser.
fn preflight(allow: HeaderValue) -> Response {
    let mut res = StatusCode::NO_CONTENT.into_response();
    let h = res.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, allow);
    h.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("authorization, content-type, anthropic-version, x-api-key"),
    );
    h.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("600"),
    );
    h.append(header::VARY, HeaderValue::from_static("origin"));
    res
}

/// The mutating routes of the proxy listener. `/v1/switch` is here too because the relay
/// collapses a doubled `/v1` prefix and a browser could send either spelling.
fn is_proxy_mutation(path: &str) -> bool {
    let p = path.trim_end_matches('/');
    p == "/switch" || p == "/v1/switch"
}

/// The span factory both listeners use.
type SpanFn = fn(&Request) -> tracing::Span;

/// `TraceLayer` with §9.2's span: **method and path only**.
fn trace_layer() -> TraceLayer<SharedClassifier<ServerErrorsAsFailures>, SpanFn> {
    TraceLayer::new_for_http().make_span_with(request_span as SpanFn)
}

/// One request's span.
///
/// §9.2: the query string is **never** recorded, because `?token=<t>` is an accepted way to
/// present a bearer and `tracing` output is not a place a credential may land. That is why
/// this exists at all instead of `DefaultMakeSpan`, which records the whole URI.
pub(crate) fn request_span(req: &Request) -> tracing::Span {
    tracing::info_span!(
        "http",
        method = %req.method(),
        path = %req.uri().path(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_protocol::{
        Backend, BackendKind, BackendLimits, BuildId, CredentialSource, DesiredState,
        EndpointRecord, EndpointSpec, Health, LocalLlamaSpec, NglPlan, Protocol, Provenance,
        SamplingMode, SplitPlan,
    };
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// `std::env` is process-global and `Paths::from_env` is private, so every test that
    /// needs a `Paths` rooted in a tempdir takes this first and holds it for the whole test.
    static ENV: Mutex<()> = Mutex::new(());

    /// The variables the fixture redirects, saved so the process is left as it was found —
    /// other units' tests share this binary and read some of these.
    ///
    /// The last five are credential variables, cleared rather than redirected: HERMETICITY
    /// says no test in this workspace may resolve a real credential, and the developer
    /// running `cargo test` is exactly the person who has them exported.
    const REDIRECTED: [&str; 13] = [
        "HOME",
        "APEXROUTER_HOME",
        "APEXROUTER_CONFIG",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
        "XDG_CONFIG_HOME",
        "PROXY_PORT",
        "APEXROUTER_TOKEN",
        "VAST_API_KEY",
        "VASTAI_API_KEY",
        "HF_TOKEN",
        "HUGGING_FACE_HUB_TOKEN",
        "TOGETHER_API_KEY",
    ];

    struct Fixture {
        _guard: std::sync::MutexGuard<'static, ()>,
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
        dir: tempfile::TempDir,
        paths: Paths,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    /// A hermetic home: `$STATE`, `$CACHE`, `config.toml` and every legacy path land inside
    /// one tempdir, so nothing this suite does can touch `~/.vastai-gguf` or `~/.config`.
    fn fixture() -> Fixture {
        let guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let saved = REDIRECTED
            .iter()
            .map(|k| (*k, std::env::var_os(k)))
            .collect();
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::env::set_var("HOME", root);
        std::env::set_var("APEXROUTER_HOME", root.join("state"));
        std::env::set_var("APEXROUTER_CONFIG", root.join("config.toml"));
        std::env::set_var("XDG_STATE_HOME", root.join("xdg-state"));
        std::env::set_var("XDG_CACHE_HOME", root.join("xdg-cache"));
        std::env::set_var("XDG_CONFIG_HOME", root.join("xdg-config"));
        std::env::remove_var("PROXY_PORT");
        std::env::remove_var("APEXROUTER_TOKEN");
        // HERMETICITY: a real credential must not be resolvable from inside a test, whatever
        // the developer has exported.
        for k in ["VAST_API_KEY", "VASTAI_API_KEY", "HF_TOKEN"] {
            std::env::remove_var(k);
        }
        std::env::remove_var("HUGGING_FACE_HUB_TOKEN");
        std::env::remove_var("TOGETHER_API_KEY");
        let paths = Paths::resolve().expect("paths");
        paths.ensure_layout().expect("layout");
        Fixture {
            _guard: guard,
            saved,
            dir,
            paths,
        }
    }

    impl Fixture {
        fn config_path(&self) -> PathBuf {
            self.dir.path().join("config.toml")
        }

        /// A config bound to two free loopback ports, with every legacy mirror off.
        ///
        /// HERMETICITY: `[providers.together]` is pointed at a closed loopback port, per the
        /// house rule — no test in this workspace may resolve a real credential or reach a
        /// paid endpoint.
        fn config(&self) -> Config {
            let mut cfg = Config::default();
            cfg.server.proxy_bind = format!("127.0.0.1:{}", free_port());
            cfg.server.control_bind = format!("127.0.0.1:{}", free_port());
            cfg.server.drain_timeout_secs = 5;
            cfg.compat.mirror_usage_log = false;
            cfg.compat.read_legacy_state = false;
            if let Some(t) = cfg.providers.get_mut("together") {
                t.base_url = "http://127.0.0.1:1/v1".to_owned();
            }
            cfg
        }

        fn write_config(&self, cfg: &Config) {
            cfg.save(&self.paths).expect("write config");
        }

        fn lock(&self) -> DaemonLock {
            DaemonLock::acquire(&self.paths).expect("daemon lock")
        }
    }

    /// A port nothing is listening on. Bound and released, which is the only portable way to
    /// ask the kernel for one.
    fn free_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind :0");
        let p = l.local_addr().expect("local_addr").port();
        drop(l);
        p
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("client")
    }

    /// Poll `GET {url}` until it answers. Startup is a few milliseconds; the budget is
    /// generous so a loaded box does not flake.
    async fn wait_for(url: &str) -> reqwest::Response {
        let http = client();
        let mut last = None;
        for _ in 0..400 {
            match http.get(url).send().await {
                Ok(r) => return r,
                Err(e) => last = Some(e),
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("{url} never answered: {last:?}");
    }

    /// [`wait_for`], but a daemon that failed to start reports **why** instead of timing out.
    async fn wait_for_up(
        url: &str,
        task: &mut tokio::task::JoinHandle<anyhow::Result<()>>,
    ) -> reqwest::Response {
        let http = client();
        let mut last = None;
        for _ in 0..400 {
            match http.get(url).send().await {
                Ok(r) => return r,
                Err(e) => last = Some(e),
            }
            if task.is_finished() {
                match task.await {
                    Ok(Ok(())) => panic!("the daemon exited cleanly before serving {url}"),
                    Ok(Err(e)) => panic!("the daemon failed to start: {e:#}"),
                    Err(e) => panic!("the daemon task panicked: {e}"),
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("{url} never answered: {last:?}");
    }

    fn spec_for(model: &Path) -> EndpointSpec {
        EndpointSpec::LocalLlama(LocalLlamaSpec {
            build: BuildId::parse("test-build").expect("build id"),
            model_path: model.display().to_string(),
            mmproj: None,
            alias_flag: "test-model".to_owned(),
            host: "127.0.0.1".to_owned(),
            port: Some(8199),
            ctx: None,
            parallel: None,
            kv_type: None,
            ngl: NglPlan::Auto,
            split: SplitPlan::default(),
            mode: SamplingMode::Raw,
            flash_attn: None,
            api_key: None,
            extra_args: Vec::new(),
        })
    }

    fn a_backend(id: &str, port: u16) -> Backend {
        Backend {
            id: BackendId::parse(id).expect("backend id"),
            kind: BackendKind::LocalLlama,
            protocol: Protocol::OpenAi,
            label: id.to_owned(),
            base_url: format!("http://127.0.0.1:{port}"),
            credential: CredentialSource::None,
            tags: vec!["local".to_owned()],
            models: Vec::new(),
            limits: BackendLimits::default(),
            price: None,
            health: Health::Unknown,
            provenance: Provenance::Manual,
            endpoint: None,
            enabled: true,
            devices: Vec::new(),
            last_error: None,
        }
    }

    // ---- both listeners bind ------------------------------------------------------------

    #[tokio::test]
    async fn both_listeners_bind_and_answer_health() {
        let fx = fixture();
        let cfg = fx.config();
        let proxy = cfg.proxy_bind();
        let control = cfg.control_bind();

        let state = build_state(fx.paths.clone(), cfg, fx.lock())
            .await
            .expect("state");
        let (trigger, handle) = shutdown::channel();
        let mut task = tokio::spawn(run(state, handle));

        let body: serde_json::Value = wait_for_up(&format!("http://{control}/health"), &mut task)
            .await
            .json()
            .await
            .expect("control /health json");
        assert_eq!(body["ok"], serde_json::json!(true));
        assert_eq!(body["product"], serde_json::json!("apexrouter"));

        let body: serde_json::Value = wait_for_up(&format!("http://{proxy}/health"), &mut task)
            .await
            .json()
            .await
            .expect("proxy /health json");
        assert_eq!(body["ok"], serde_json::json!(true));
        assert_eq!(body["product"], serde_json::json!("apexrouter"));

        trigger.trigger();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("shutdown must finish inside the drain deadline")
            .expect("join")
            .expect("serve returned an error");
    }

    #[tokio::test]
    async fn the_owner_record_names_both_urls() {
        let fx = fixture();
        let cfg = fx.config();
        let (proxy, control) = (cfg.proxy_bind(), cfg.control_bind());
        let _state = build_state(fx.paths.clone(), cfg, fx.lock())
            .await
            .expect("state");

        let text = std::fs::read_to_string(fx.paths.daemon_lock()).expect("lock file");
        let rec: OwnerRecord = serde_json::from_str(&text).expect("owner record");
        assert_eq!(rec.pid, std::process::id());
        assert_eq!(rec.proxy_url, format!("http://{proxy}"));
        assert_eq!(rec.control_url, format!("http://{control}"));
    }

    // ---- EADDRINUSE names the holder ----------------------------------------------------

    #[tokio::test]
    async fn a_port_held_by_localrouter_is_named_and_the_fix_is_the_message() {
        let squatter = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/health"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "ok": true, "product": "localrouter", "version": "0.9"
                })),
            )
            .mount(&squatter)
            .await;
        let held = squatter.address().port();

        let fx = fixture();
        let mut cfg = fx.config();
        cfg.server.proxy_bind = format!("127.0.0.1:{held}");
        let state = build_state(fx.paths.clone(), cfg, fx.lock())
            .await
            .expect("state");
        let (_trigger, handle) = shutdown::channel();

        let err = run(state, handle).await.expect_err("bind must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains(&format!("port {held} is held by")), "{msg}");
        assert!(msg.contains("product=localrouter"), "{msg}");
        assert!(msg.contains("endpoint_proxy.py"), "{msg}");
    }

    #[tokio::test]
    async fn a_port_held_by_something_silent_still_names_the_port_and_a_way_to_find_it() {
        let fx = fixture();
        let squatter = std::net::TcpListener::bind("127.0.0.1:0").expect("squat");
        let held = squatter.local_addr().expect("addr").port();

        let mut cfg = fx.config();
        cfg.server.control_bind = format!("127.0.0.1:{held}");
        let state = build_state(fx.paths.clone(), cfg, fx.lock())
            .await
            .expect("state");
        let (_trigger, handle) = shutdown::channel();

        let err = run(state, handle).await.expect_err("bind must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("the control listener cannot bind"), "{msg}");
        assert!(msg.contains(&format!("port {held} is held by")), "{msg}");
        assert!(msg.contains("ss -ltnp"), "{msg}");
    }

    // ---- reconciliation happens before the table is armed, and before the bind ----------

    #[tokio::test]
    async fn reconciliation_runs_before_the_listeners_bind() {
        let fx = fixture();
        // A record for a process that is not there, which the operator already asked to be
        // stopped: reconciliation's job is to forget it.
        let rec = EndpointRecord {
            id: BackendId::parse("ghost").expect("id"),
            spec: spec_for(Path::new("/nonexistent/model.gguf")),
            desired: DesiredState::Stopped,
            proc: None,
            port: Some(8199),
            log_path: None,
            started_at_unix: 0,
            fit: None,
            adopted: false,
            alias_bindings: Vec::new(),
        };
        Store::new(fx.paths.clone())
            .put_endpoint(&rec)
            .expect("write the record");
        assert!(fx.paths.endpoint_file(&rec.id).exists());

        // ... and a listener already holding the control port, so the bind fails.
        let squatter = std::net::TcpListener::bind("127.0.0.1:0").expect("squat");
        let held = squatter.local_addr().expect("addr").port();
        let mut cfg = fx.config();
        cfg.server.control_bind = format!("127.0.0.1:{held}");

        let state = build_state(fx.paths.clone(), cfg, fx.lock())
            .await
            .expect("state");
        let (_trigger, handle) = shutdown::channel();
        run(state, handle).await.expect_err("bind must fail");

        assert!(
            !fx.paths.endpoint_file(&rec.id).exists(),
            "reconciliation must have run BEFORE the listeners bound"
        );
    }

    #[tokio::test]
    async fn reconcile_arms_the_table_with_the_backends_it_found() {
        let fx = fixture();
        Store::new(fx.paths.clone())
            .save_backends(&[a_backend("node-a", 9001)])
            .expect("save backends");

        let state = build_state(fx.paths.clone(), fx.config(), fx.lock())
            .await
            .expect("state");
        let before = state.router.table().generation();

        reconcile_on_start(&state).await.expect("reconcile");

        let ids: Vec<String> = state
            .router
            .registry()
            .snapshot()
            .iter()
            .map(|b| b.id.to_string())
            .collect();
        assert!(ids.contains(&"node-a".to_owned()), "{ids:?}");
        assert!(
            state.router.table().generation() > before,
            "the table must be re-armed after reconciliation"
        );
    }

    // ---- SIGHUP reloads, and does not exit ----------------------------------------------

    #[tokio::test]
    async fn sighup_reloads_the_config_without_exiting() {
        let fx = fixture();
        let cfg = fx.config();
        let control = cfg.control_bind();
        fx.write_config(&cfg);

        let state = build_state(fx.paths.clone(), cfg.clone(), fx.lock())
            .await
            .expect("state");
        let (trigger, handle) = shutdown::channel();
        let mut task = tokio::spawn(run(Arc::clone(&state), handle));
        wait_for_up(&format!("http://{control}/health"), &mut task).await;

        // Edit the file on disk, then raise SIGHUP at ourselves. `kill(1)` rather than a
        // libc dependency, and no `sh -c` anywhere.
        let mut edited = cfg.clone();
        edited.server.drain_timeout_secs = 7;
        edited.router.max_inflight = 99;
        fx.write_config(&edited);
        let status = std::process::Command::new("kill")
            .args(["-HUP", &std::process::id().to_string()])
            .status()
            .expect("kill -HUP");
        assert!(status.success());

        let mut reloaded = false;
        for _ in 0..200 {
            if state.cfg().router.max_inflight == 99 {
                reloaded = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(reloaded, "SIGHUP must reload config.toml");
        assert_eq!(state.cfg().server.drain_timeout_secs, 7);

        // Still serving: SIGHUP is a reload, never an exit.
        let res = wait_for(&format!("http://{control}/health")).await;
        assert_eq!(res.status(), 200);
        assert!(!task.is_finished());

        trigger.trigger();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("shutdown")
            .expect("join")
            .expect("serve");
    }

    #[tokio::test]
    async fn a_broken_config_reload_keeps_the_running_one() {
        let fx = fixture();
        let cfg = fx.config();
        fx.write_config(&cfg);
        let state = build_state(fx.paths.clone(), cfg, fx.lock())
            .await
            .expect("state");
        let before = state.cfg().router.max_inflight;

        std::fs::write(fx.config_path(), "this is not [ toml").expect("write garbage");
        reload_config(&state).await.expect_err("must report");

        assert_eq!(state.cfg().router.max_inflight, before);
    }

    // ---- POST /v1/reload reaches the REQUEST PATH, not only the control plane -------------

    /// Serve one composed app on the address it is configured for, and hand back the
    /// authority a client should use.
    ///
    /// `run` is deliberately not used by the reload test below: it also starts the config
    /// watcher, which reloads `config.toml` on its own 250 ms debounce, and a test that
    /// cannot tell `POST /v1/reload` apart from the watcher proves nothing about either.
    /// The apps themselves are the real composed ones — mutation gate included, which is why
    /// the listener must be on the **configured** port: `auth::allowed_ports` derives the
    /// `Host` allowlist from `[server] proxy_bind`/`control_bind`.
    async fn spawn_on(addr: SocketAddr, which: &str, app: axum::Router) -> String {
        let listener = bind_listener(addr, which).await.expect("bind");
        let local = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await;
        });
        local.to_string()
    }

    /// The acceptance for FIX-3 defect A.
    ///
    /// `[router] max_body_bytes` is the cheapest `[router]` key to *observe*: the handler
    /// enforces it before it resolves anything, so a 413 proves the request path re-read the
    /// configuration without a single upstream existing. Before `apply_config`, this test
    /// failed on the last assertion — `POST /v1/reload` answered `{"ok":true,"issues":[]}`
    /// and the proxy went on using the daemon's boot-time cap.
    #[tokio::test]
    async fn a_router_key_changes_behaviour_after_post_v1_reload_with_no_restart() {
        let fx = fixture();
        let cfg = fx.config();
        fx.write_config(&cfg);

        let state = build_state(fx.paths.clone(), cfg.clone(), fx.lock())
            .await
            .expect("state");
        let proxy = spawn_on(cfg.proxy_bind(), "proxy", proxy_app(&state)).await;
        let control = spawn_on(
            cfg.control_bind(),
            "control",
            api_router(Arc::clone(&state)),
        )
        .await;

        let http = client();
        let big = format!("{{\"model\":\"auto\",\"pad\":\"{}\"}}", "x".repeat(8192));
        let post_big = |authority: String, body: String| {
            let http = http.clone();
            async move {
                http.post(format!("http://{authority}/v1/chat/completions"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(body)
                    .send()
                    .await
                    .expect("proxy answered")
                    .status()
            }
        };

        let before = post_big(proxy.clone(), big.clone()).await;
        assert_ne!(
            before,
            StatusCode::PAYLOAD_TOO_LARGE,
            "the shipped cap accepts an 8 KiB body; the test needs a change to observe"
        );

        // The edit an operator makes, and the reload they then ask for.
        let mut edited = cfg.clone();
        edited.router.max_body_bytes = 64;
        fx.write_config(&edited);

        let res = http
            .post(format!("http://{control}/v1/reload"))
            .send()
            .await
            .expect("reload answered");
        assert_eq!(res.status(), StatusCode::OK);
        let report: serde_json::Value = res.json().await.expect("ValidationReport json");
        assert_eq!(report["ok"], serde_json::json!(true), "{report}");

        assert_eq!(
            post_big(proxy, big).await,
            StatusCode::PAYLOAD_TOO_LARGE,
            "POST /v1/reload must reach the request path's own config, not only AppState.cfg"
        );
        assert_eq!(state.router.cfg().router.max_body_bytes, 64);
        assert_eq!(state.cfg().router.max_body_bytes, 64);
    }

    // ---- the live-request log is attached before the first query --------------------------

    /// The acceptance for FIX-3 defect B: a **cold** daemon, real traffic, and the very first
    /// `GET /v1/requests` this process ever makes already has the row.
    ///
    /// The order is the whole point. Nothing queries `/v1/requests` before the traffic, so a
    /// lazily-attached log would still be unsubscribed while the request finished, R-08 would
    /// skip the `RequestFinished` broadcast for want of a receiver, and the answer would be
    /// `[]` — which is what both GUIs showed on first load.
    #[tokio::test]
    async fn the_live_request_log_is_populated_on_a_cold_daemon_after_traffic() {
        use apexrouter_protocol::{
            Alias, BackendSelector, ModelRoute, RouteFile, RouteTarget, Strategy, UpstreamModel,
        };
        use wiremock::matchers::{method as m_method, path as m_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // A loopback upstream. `/v1/models` is mounted too, so the health prober confirms the
        // backend instead of tripping it to Down mid-test.
        let up = MockServer::start().await;
        Mock::given(m_method("GET"))
            .and(m_path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                br#"{"object":"list","data":[{"id":"carnice","object":"model"}]}"#.to_vec(),
                "application/json",
            ))
            .mount(&up)
            .await;
        Mock::given(m_method("POST"))
            .and(m_path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                br#"{"id":"a","object":"chat.completion","choices":[],"usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}}"#.to_vec(),
                "application/json",
            ))
            .mount(&up)
            .await;

        let fx = fixture();
        let cfg = fx.config();
        let proxy = cfg.proxy_bind();
        let control = cfg.control_bind();
        fx.write_config(&cfg);

        // State on disk *before* the daemon boots: this is a cold start, not a live edit.
        let store = Store::new(fx.paths.clone());
        let mut backend = a_backend("up-a", 0);
        backend.kind = BackendKind::Node;
        backend.base_url = up.uri();
        backend.models = vec![UpstreamModel {
            id: "carnice".to_owned(),
            ctx: Some(4096),
            vision: false,
            tools: true,
        }];
        backend.health = Health::Ready {
            since_unix: 0,
            slots_busy: 0,
            slots_total: 4,
            tps_p50: None,
        };
        store.save_backends(&[backend]).expect("save backends");

        let alias = Alias::parse("auto").expect("alias");
        store
            .save_routes(&RouteFile {
                schema_version: 1,
                default_alias: alias.clone(),
                routes: vec![ModelRoute {
                    alias: alias.clone(),
                    targets: vec![RouteTarget {
                        backend: BackendSelector::Id(BackendId::parse("up-a").expect("backend id")),
                        model: None,
                        weight: 1,
                    }],
                    strategy: Strategy::FirstHealthy,
                    filter: Default::default(),
                    retry: Default::default(),
                    is_default: true,
                    description: None,
                }],
            })
            .expect("save routes");

        let state = build_state(fx.paths.clone(), cfg, fx.lock())
            .await
            .expect("state");
        let (trigger, handle) = shutdown::channel();
        let mut task = tokio::spawn(run(Arc::clone(&state), handle));
        wait_for_up(&format!("http://{control}/health"), &mut task).await;

        // ---- traffic, and NOTHING has ever asked for /v1/requests ------------------------
        let http = client();
        let mut relayed = false;
        for _ in 0..80 {
            let res = http
                .post(format!("http://{proxy}/v1/chat/completions"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(r#"{"model":"auto","messages":[]}"#)
                .send()
                .await
                .expect("proxy answered");
            if res.status() == StatusCode::OK {
                relayed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(relayed, "the proxy never relayed to the loopback upstream");

        // ---- the FIRST /v1/requests query of this daemon's life --------------------------
        let mut rows: Vec<serde_json::Value> = Vec::new();
        for _ in 0..200 {
            rows = http
                .get(format!("http://{control}/v1/requests"))
                .send()
                .await
                .expect("requests answered")
                .json()
                .await
                .expect("a JSON array of RequestRecord");
            if !rows.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            !rows.is_empty(),
            "GET /v1/requests must be populated on a cold daemon after traffic; \
             an empty answer means nothing attached the log at startup"
        );
        assert_eq!(rows[0]["backend"], serde_json::json!("up-a"));
        assert_eq!(rows[0]["status"], serde_json::json!(200));

        trigger.trigger();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("shutdown")
            .expect("join")
            .expect("serve");
    }

    // ---- shutdown never signals a child --------------------------------------------------

    #[tokio::test]
    async fn shutdown_drains_and_never_signals_a_child() {
        let fx = fixture();

        // A real detached child, recorded exactly as the supervisor records one, so
        // reconciliation adopts it and shutdown has something it *could* have killed.
        let args = ["30".to_owned()];
        let spawned = proc::spawn_detached(proc::SpawnRequest {
            program: Path::new("/bin/sleep"),
            args: &args,
            env: &[],
            cwd: fx.paths.state(),
            log: &fx.paths.logs_dir().join("sleeper.log"),
            setsid: true,
        })
        .expect("spawn a detached child");

        let rec = EndpointRecord {
            id: BackendId::parse("sleeper").expect("id"),
            spec: spec_for(Path::new("/bin/sleep")),
            desired: DesiredState::Running,
            proc: Some(spawned.facts.clone()),
            port: Some(8199),
            log_path: None,
            started_at_unix: chrono::Utc::now().timestamp(),
            fit: None,
            adopted: true,
            alias_bindings: Vec::new(),
        };
        Store::new(fx.paths.clone())
            .put_endpoint(&rec)
            .expect("record");

        let cfg = fx.config();
        let control = cfg.control_bind();
        let state = build_state(fx.paths.clone(), cfg, fx.lock())
            .await
            .expect("state");
        let (trigger, handle) = shutdown::channel();
        let mut task = tokio::spawn(run(state, handle));
        wait_for_up(&format!("http://{control}/health"), &mut task).await;

        trigger.trigger();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("shutdown must finish inside the drain deadline")
            .expect("join")
            .expect("serve");

        let alive = matches!(proc::liveness(&spawned.facts), proc::Liveness::Alive);
        // Tidy up before asserting, so a failure does not leak a `sleep` for 30 s.
        let _ = proc::signal_verified(&spawned.facts, proc::Signal::Kill);
        assert!(
            alive,
            "shutdown must NEVER signal a llama-server child (kill_children_on_exit is false)"
        );
    }

    // ---- §9.1 ----------------------------------------------------------------------------

    #[tokio::test]
    async fn a_non_loopback_bind_without_a_token_refuses_to_start() {
        let fx = fixture();
        let mut cfg = fx.config();
        cfg.server.proxy_bind = format!("0.0.0.0:{}", free_port());

        let err = match build_state(fx.paths.clone(), cfg, fx.lock()).await {
            Ok(_) => panic!("a non-loopback bind with no token must refuse to start"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("refusing to start"), "{msg}");
        assert!(msg.contains("APEXROUTER_TOKEN"), "{msg}");
    }

    /// C1 / §9.3: the proxy mutation gate must refuse a rebinding-shaped `Host` on
    /// `POST /switch`. Before the gate was mounted, the handler ran and returned a body
    /// error; with the gate, the answer is 403 and the switch never executes.
    #[tokio::test]
    async fn proxy_switch_refuses_a_dns_rebinding_host() {
        let fx = fixture();
        let cfg = fx.config();
        let proxy_port = cfg.proxy_bind().port();
        fx.write_config(&cfg);

        let state = build_state(fx.paths.clone(), cfg.clone(), fx.lock())
            .await
            .expect("state");
        let proxy = spawn_on(cfg.proxy_bind(), "proxy", proxy_app(&state)).await;

        let res = client()
            .post(format!("http://{proxy}/switch"))
            .header(header::HOST, format!("evil.example.com:{proxy_port}"))
            .header(
                header::ORIGIN,
                format!("http://evil.example.com:{proxy_port}"),
            )
            .header(header::CONTENT_TYPE, "application/json")
            .body(r#"{"name":"x"}"#)
            .send()
            .await
            .expect("proxy answered");
        assert_eq!(
            res.status(),
            StatusCode::FORBIDDEN,
            "rebinding Host must not reach POST /switch: {}",
            res.text().await.unwrap_or_default()
        );
    }

    #[tokio::test]
    async fn a_non_loopback_bind_with_a_token_is_allowed() {
        let fx = fixture();
        let mut cfg = fx.config();
        cfg.server.proxy_bind = format!("0.0.0.0:{}", free_port());
        std::env::set_var("APEXROUTER_TOKEN", "a-long-random-string");
        let built = build_state(fx.paths.clone(), cfg, fx.lock()).await;
        std::env::remove_var("APEXROUTER_TOKEN");
        if let Err(e) = built {
            panic!("a non-loopback bind WITH a token must be allowed: {e:#}");
        }
    }

    // ---- the CORS ruling ------------------------------------------------------------------

    /// Start a proxy listener with the given `proxy_cors_origins` and run one request
    /// through it.
    async fn proxy_with_cors(
        origins: Vec<String>,
        method: Method,
        path: &str,
        origin_header: Option<&str>,
    ) -> reqwest::Response {
        let fx = fixture();
        let mut cfg = fx.config();
        cfg.server.proxy_cors_origins = origins;
        let proxy = cfg.proxy_bind();
        let state = build_state(fx.paths.clone(), cfg, fx.lock())
            .await
            .expect("state");
        let (trigger, handle) = shutdown::channel();
        let mut task = tokio::spawn(run(state, handle));
        wait_for_up(&format!("http://{proxy}/health"), &mut task).await;

        let mut rb = client().request(method, format!("http://{proxy}{path}"));
        if let Some(o) = origin_header {
            rb = rb.header("origin", o);
        }
        let res = rb.send().await.expect("request");

        trigger.trigger();
        let _ = tokio::time::timeout(Duration::from_secs(10), task).await;
        res
    }

    #[tokio::test]
    async fn by_default_no_cors_header_is_emitted_at_all() {
        let res = proxy_with_cors(
            Vec::new(),
            Method::GET,
            "/health",
            Some("http://evil.example"),
        )
        .await;
        assert!(res.headers().get("access-control-allow-origin").is_none());
    }

    #[tokio::test]
    async fn a_star_entry_opts_back_in_to_the_localrouter_behaviour() {
        let res = proxy_with_cors(vec!["*".to_owned()], Method::GET, "/health", None).await;
        assert_eq!(
            res.headers()
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok()),
            Some("*")
        );
    }

    #[tokio::test]
    async fn a_listed_origin_is_echoed_and_an_unlisted_one_is_not() {
        let allowed = vec!["http://localhost:5173".to_owned()];
        let res = proxy_with_cors(
            allowed.clone(),
            Method::GET,
            "/health",
            Some("http://localhost:5173"),
        )
        .await;
        assert_eq!(
            res.headers()
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok()),
            Some("http://localhost:5173")
        );

        let res =
            proxy_with_cors(allowed, Method::GET, "/health", Some("http://evil.example")).await;
        assert!(res.headers().get("access-control-allow-origin").is_none());
    }

    #[tokio::test]
    async fn a_mutation_route_is_never_covered_by_cors() {
        let res = proxy_with_cors(
            vec!["*".to_owned()],
            Method::POST,
            "/switch",
            Some("http://localhost:5173"),
        )
        .await;
        assert!(
            res.headers().get("access-control-allow-origin").is_none(),
            "POST /switch is a mutation; §9.3 governs it, never a CORS header"
        );
    }

    #[tokio::test]
    async fn the_control_listener_is_never_covered_by_cors() {
        let fx = fixture();
        let mut cfg = fx.config();
        cfg.server.proxy_cors_origins = vec!["*".to_owned()];
        let control = cfg.control_bind();
        let state = build_state(fx.paths.clone(), cfg, fx.lock())
            .await
            .expect("state");
        let (trigger, handle) = shutdown::channel();
        let mut task = tokio::spawn(run(state, handle));

        let res = wait_for_up(&format!("http://{control}/health"), &mut task).await;
        assert!(res.headers().get("access-control-allow-origin").is_none());

        trigger.trigger();
        let _ = tokio::time::timeout(Duration::from_secs(10), task).await;
    }

    // ---- unit-level checks of the pieces the integration tests exercise ------------------

    #[tokio::test]
    async fn post_v1_shutdown_reaches_the_shutdown_flag() {
        // The bridge `serve_with_shutdown` installs, exercised without a listener: S-03
        // signals through a process-global notifier, and this is the only wire between it
        // and the drain.
        let (trigger, local) = shutdown::channel();
        let (_external_trigger, external) = shutdown::channel();
        spawn_shutdown_bridges(&trigger, &external);
        // Let the bridge register its waiter before anything notifies it.
        tokio::task::yield_now().await;

        api::request_shutdown();

        tokio::time::timeout(Duration::from_secs(5), local.wait())
            .await
            .expect("POST /v1/shutdown must reach the drain");
    }

    #[test]
    fn allow_origin_value_matches_exactly_or_says_nothing() {
        let listed = vec!["http://a.example".to_owned()];
        assert!(allow_origin_value(&listed, Some("http://a.example")).is_some());
        assert!(allow_origin_value(&listed, Some("http://a.example:8080")).is_none());
        assert!(allow_origin_value(&listed, None).is_none());
        assert!(allow_origin_value(&[], Some("http://a.example")).is_none());
        assert_eq!(
            allow_origin_value(&["*".to_owned()], None),
            Some(HeaderValue::from_static("*"))
        );
    }

    #[test]
    fn the_switch_route_is_recognised_in_both_spellings() {
        assert!(is_proxy_mutation("/switch"));
        assert!(is_proxy_mutation("/v1/switch"));
        assert!(is_proxy_mutation("/switch/"));
        assert!(!is_proxy_mutation("/v1/chat/completions"));
        assert!(!is_proxy_mutation("/switcheroo"));
    }

    #[test]
    fn the_span_records_method_and_path_but_never_the_query() {
        // §9.2: `?token=` is an accepted presentation, so the query string must not reach a
        // span field. The span is built from `uri().path()`, which cannot contain one.
        let req = Request::builder()
            .method(Method::GET)
            .uri("/v1/models?token=super-secret")
            .body(axum::body::Body::empty())
            .expect("request");
        let span = request_span(&req);
        let fields: Vec<&str> = span
            .metadata()
            .map(|m| m.fields().iter().map(|f| f.name()).collect())
            .unwrap_or_default();
        assert_eq!(
            fields,
            vec!["method", "path"],
            "the span may declare no field that could hold a query string"
        );
        assert!(!format!("{span:?}").contains("super-secret"));
    }

    #[test]
    fn a_bind_url_is_something_a_client_can_actually_use() {
        assert_eq!(
            url_for("0.0.0.0:8888".parse().expect("addr")),
            "http://127.0.0.1:8888"
        );
        assert_eq!(
            url_for("127.0.0.1:2739".parse().expect("addr")),
            "http://127.0.0.1:2739"
        );
    }
}
