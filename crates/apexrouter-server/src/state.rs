//! OWNER: unit S-01 (server/src/{lib,state,shutdown}.rs). Do not edit outside that unit.
//!
//! The shared application state. Everything in-process: the routing table, the backend
//! registry, every child process, the health prober, the file watcher, the ledger, the usage
//! writer and both listeners. The CLI, the MCP server, the web UI and the Slint app are
//! **clients**.
//!
//! Every field is `pub` on purpose: this is the daemon's one shared record, handlers in six
//! other work units read it, and an accessor per field would be ceremony without a boundary.
//! What is *not* here is as deliberate — no request-scoped data, no `&mut self`, and no lock
//! that a request path takes. `cfg` is an `ArcSwap` so a reload is a pointer store, and the
//! routing table lives behind [`apexrouter_router::Router`] for the same reason.

use crate::svc_prober::ServiceStatusCache;
use apexrouter_core::checks::Registry;
use apexrouter_core::config::Config;
use apexrouter_core::lockfile::DaemonLock;
use apexrouter_core::paths::Paths;
use apexrouter_core::store::Store;
use apexrouter_protocol::{Event, VastInstance};
use apexrouter_providers::local::LocalProvisioner;
use apexrouter_router::Telemetry;
use arc_swap::ArcSwap;
use std::sync::{Arc, RwLock};
use std::time::Instant;
use tokio::sync::{broadcast, Mutex};

/// The last fleet read: what `GET /v1/snapshot` serves for `instances` and the vast totals.
///
/// The snapshot deliberately probes nothing (`api/snapshot.rs` §"nothing in here probes an
/// upstream"), so a **cache** is the only honest way rented boxes reach it — fed by the
/// fleet poller and by every handler that already read the fleet for its own reasons. The
/// Fleet & cost page rendered `[]` between refreshes for exactly as long as this cache did
/// not exist (GARDEN-RUNS.md, R4 tooling findings).
#[derive(Clone, Debug, Default)]
pub struct FleetCache {
    /// The rented fleet as last observed. Empty means "none seen", which is why `as_of_unix`
    /// travels with it — an empty fleet at `0` has never been read at all.
    pub instances: Vec<VastInstance>,
    /// Credit as last observed. `None` when the account was never readable.
    pub credit: Option<f64>,
    /// When this was read, unix seconds. `0` = never.
    pub as_of_unix: i64,
}

/// Everything a handler may reach.
pub struct AppState {
    /// Resolved paths.
    pub paths: Paths,
    /// Hot-reloadable config. `SIGHUP` and `POST /v1/reload` swap it.
    pub cfg: ArcSwap<Config>,
    /// Atomic state writes.
    pub store: Store,
    /// The request path.
    pub router: apexrouter_router::Router,
    /// The broadcast every surface subscribes to.
    pub tx: broadcast::Sender<Event>,
    /// The local `llama-server`/vLLM supervisor.
    pub supervisor: Arc<LocalProvisioner>,
    /// Background operations behind `?no_wait=true`.
    pub jobs: crate::jobs::JobRegistry,
    /// `doctor` and `diagnose`, with provider checks registered at startup.
    pub checks: Arc<Registry>,
    /// For `uptime`.
    pub started_at: Instant,
    /// Held for the process lifetime. Released by process exit.
    pub lock: Arc<Mutex<DaemonLock>>,
    /// The rented fleet as last observed, for the network-free snapshot.
    pub fleet: RwLock<FleetCache>,
    /// Prometheus counters + gauges for `GET /metrics` (R-07). Fed from the same
    /// `RequestFinished` bus the live-request log listens on.
    pub telemetry: Arc<Telemetry>,
    /// Computed liveness for ServiceRecords (Comfy lanes). Never persisted.
    pub service_status: Arc<ServiceStatusCache>,
    /* provider slots filled in Stage 5: vast, hf, together, tunnels */
}

impl AppState {
    /// Assemble the state. Every subsystem is built by the caller ([`crate::serve`]) so
    /// that a test — or ApexOS-RS embedding the control plane — can substitute one.
    ///
    /// Nine arguments is nine subsystems; collapsing them into a builder would only move
    /// the list somewhere it is harder to see.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        paths: Paths,
        cfg: Config,
        store: Store,
        router: apexrouter_router::Router,
        tx: broadcast::Sender<Event>,
        supervisor: Arc<LocalProvisioner>,
        checks: Arc<Registry>,
        lock: DaemonLock,
    ) -> AppState {
        // Same ring depth as the live-request log (1_000): counters are unbounded; the ring
        // inside Telemetry is only for `window()` aggregation helpers.
        let telemetry = Arc::new(Telemetry::new(tx.clone(), 1_000));
        AppState {
            paths,
            cfg: ArcSwap::from_pointee(cfg),
            store,
            router,
            tx,
            supervisor,
            jobs: crate::jobs::JobRegistry::new(),
            checks,
            started_at: Instant::now(),
            lock: Arc::new(Mutex::new(lock)),
            fleet: RwLock::new(FleetCache::default()),
            telemetry,
            service_status: Arc::new(ServiceStatusCache::default()),
        }
    }

    /// Record a fleet observation for the snapshot to serve.
    ///
    /// Called by the fleet poller and by every handler that read the fleet anyway (`ls`,
    /// rent, destroy), so the cache is at worst one poll interval stale. `credit: None`
    /// keeps the last known value — not being able to ask is not evidence of `$0.00`.
    pub fn update_fleet(&self, instances: Vec<VastInstance>, credit: Option<f64>) {
        if let Ok(mut cache) = self.fleet.write() {
            cache.instances = instances;
            if credit.is_some() {
                cache.credit = credit;
            }
            cache.as_of_unix = chrono::Utc::now().timestamp();
        }
    }

    /// The last fleet observation.
    pub fn fleet_cache(&self) -> FleetCache {
        self.fleet.read().map(|c| c.clone()).unwrap_or_default()
    }

    /// The configuration in force right now.
    ///
    /// A full `Arc` clone rather than a `Guard`, because a handler that holds a `Guard`
    /// across an `.await` pins the old configuration for the life of a streaming response.
    pub fn cfg(&self) -> Arc<Config> {
        self.cfg.load_full()
    }

    /// Seconds since the daemon started, for `/health` and `Snapshot`.
    pub fn uptime_secs(&self) -> f64 {
        self.started_at.elapsed().as_secs_f64()
    }

    /// Publish an event to every surface.
    ///
    /// "Nobody is listening" is not a failure: the daemon runs with zero subscribers most
    /// of the time, and an error return here would make every call site pretend to care.
    pub fn emit(&self, ev: Event) {
        let _ = self.tx.send(ev);
    }

    /// Publish an operator-visible alert.
    ///
    /// `id` is stable per *kind* of alert so repeats coalesce in the UI instead of stacking
    /// — the reload path in particular fires this on every failed edit of a config file.
    pub fn alert(&self, level: apexrouter_protocol::AlertLevel, id: &str, message: String) {
        self.emit(Event::Alert {
            level,
            message,
            action: None,
            id: id.to_owned(),
        });
    }
}
