//! OWNER: unit R-08 (router/src/lib.rs, router/src/handler.rs). Do not edit outside that
//! unit — every other module here belongs to a different work unit.
//!
//! **The request path.** Routing table, `resolve()`, relay, SSE, retry/failover, breaker,
//! limits, aggregated `/v1/models`, telemetry, legacy compat handlers, and the one
//! Anthropic ingress translator. Knows nothing about vast, HuggingFace or process spawning.
//!
//! Two invariants are expressed as **types**, not comments:
//!
//! * The retry loop consumes `PreFlight` values and can only exit by producing a
//!   `Committed`. "Never retry after the first byte" is unrepresentable, not merely
//!   documented.
//! * `InFlightGuard` owns the permit, the byte budget, the gauge and the `RequestRecord`;
//!   its `Drop` emits `RequestFinished { aborted: true }` when `finish()` was never called.
//!   A client Ctrl-C therefore cannot leak a permit or a zombie UI row.

// The modules still carrying `#[allow(unused)]` hold surface that later stages consume: the
// `anthropic` stubs R-10 fills in during Stage 5, and helpers the server crate calls. Each
// one trips `dead_code`/`unused_variables` today. The allow is deliberately per-module rather
// than crate-wide, so everything already wired up stays fully linted, and **each line comes
// off as its owning unit's surface is consumed** — `attempt`, `limits` and `relay` lost
// theirs when the handler was wired to the one SSE relay and to `Plan::retry`.
#[allow(unused)]
pub mod anthropic;
pub mod attempt;
#[allow(unused)]
pub mod breaker;
#[allow(unused)]
pub mod compat;
#[allow(unused)]
pub mod errors;
pub mod handler;
pub mod limits;
#[allow(unused)]
pub mod models;
#[allow(unused)]
pub mod policy;
#[allow(unused)]
pub mod registry;
pub mod relay;
#[allow(unused)]
pub mod resolve;
#[allow(unused)]
pub mod table;
#[allow(unused)]
pub mod telemetry;

pub use handler::{proxy_handler, proxy_router};
pub use registry::{
    BackendRegistry, LiveBackend, Parked, WarmRegistry, WarmSlot, WarmWindow,
    DEFAULT_WARM_QUEUE_MAX,
};
pub use resolve::{Candidate, Plan, RequestClass, RouteError, UnknownModelPolicy};
pub use table::{RoutingTable, TableBuilder};
pub use telemetry::Telemetry;

use apexrouter_core::config::Config;
use apexrouter_core::usage::UsageWriter;
use apexrouter_protocol::{Alias, Event, RequestRecord, RouteFile};
use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, Semaphore};

/// How many finished requests the in-process ring keeps for `GET /v1/requests`.
const RING_CAPACITY: usize = 1_000;

/// How many `(user-agent, path)` pairs the "collapsed a doubled `/v1`" debug log remembers
/// before it stops de-duplicating. Bounded, because the key is client-supplied.
const COLLAPSE_LOG_CAPACITY: usize = 512;

/// Everything the request path holds. Rebuilt rarely; read on every request.
pub struct RouterInner {
    /// The compiled table. Read is a pointer load — the request path never takes a lock
    /// beyond one `Semaphore`, and never touches the filesystem.
    table: arc_swap::ArcSwap<RoutingTable>,
    /// Live per-backend state, which **survives** a table recompile.
    registry: BackendRegistry,
    /// **The warm queue** (`ARCHITECTURE.md` §4.7). Pure in-memory state — a `Notify`, an
    /// `AtomicU32` depth and a deadline per alias — so a **Sequential** swap, which is the
    /// common path on a box where two 7 GB models cannot coexist, parks arriving requests
    /// instead of `503`ing them for the whole of the replacement's model load.
    ///
    /// Read on the request path only after a dispatch has already failed, and then only
    /// through [`WarmRegistry::any_open`], one relaxed atomic load. The happy path never
    /// touches it.
    warm: WarmRegistry,
    /// ONE pooled client, with `no_gzip`/`no_brotli`/`no_deflate` so bytes relay verbatim.
    http: reqwest::Client,
    /// GLOBAL byte budget, not just a request count.
    inflight_bytes: Arc<Semaphore>,
    /// Recent requests, for `GET /v1/requests` and the live table.
    ///
    /// A **`std::sync::Mutex`**, not tokio's: the streaming relay seals its record from the
    /// response body's `Drop`, which is synchronous, and that is what makes a client
    /// disconnect land in the ring instead of disappearing. Nothing is ever awaited while it
    /// is held — the critical section is a `pop_front` and a `push_back`.
    ring: Mutex<VecDeque<RequestRecord>>,
    /// The broadcast every surface subscribes to.
    events: broadcast::Sender<Event>,
    /// Append-only usage rows.
    usage: UsageWriter,
    /// Hot-reloadable config.
    cfg: arc_swap::ArcSwap<Config>,
    /// De-duplicates the `/v1`-collapse debug log, keyed by `(user-agent, path)`, so a
    /// genuinely broken client stays discoverable without flooding the log. Private
    /// bookkeeping; not part of `ARCHITECTURE.md` §4.1.
    collapse_seen: Mutex<HashSet<(String, String)>>,
}

/// The shared handle every surface passes around.
pub type Router = Arc<RouterInner>;

impl RouterInner {
    /// Build the request path. The table is armed separately, after reconciliation.
    ///
    /// The initial table is an **empty** one — no aliases, no backends — compiled through
    /// the same `TableBuilder::compile` every later reload uses, so there is exactly one
    /// way a `RoutingTable` comes into existence.
    pub fn new(cfg: Arc<Config>, tx: broadcast::Sender<Event>, usage: UsageWriter) -> Router {
        let http = build_http_client(&cfg);
        let registry = BackendRegistry::new();
        let table = empty_table(&cfg, &registry);

        // `Semaphore` permits are `usize`; the configured budget is a `u64`.
        let byte_permits = cfg
            .router
            .max_inflight_bytes
            .min(Semaphore::MAX_PERMITS as u64) as usize;

        Arc::new(RouterInner {
            table: arc_swap::ArcSwap::from_pointee(table),
            registry,
            warm: WarmRegistry::new(tx.clone()),
            http,
            inflight_bytes: Arc::new(Semaphore::new(byte_permits)),
            ring: Mutex::new(VecDeque::with_capacity(RING_CAPACITY)),
            events: tx,
            usage,
            cfg: arc_swap::ArcSwap::new(cfg),
            collapse_seen: Mutex::new(HashSet::new()),
        })
    }

    /// Swap in a freshly compiled table. A failed compile never reaches here — the running
    /// table keeps serving.
    pub fn store_table(&self, t: RoutingTable) {
        self.table.store(Arc::new(t));
    }

    /// Load the current table. This is the pointer load on the request path.
    pub fn table(&self) -> arc_swap::Guard<Arc<RoutingTable>> {
        self.table.load()
    }

    /// Swap in a freshly parsed configuration. **The counterpart of [`store_table`].**
    ///
    /// [`store_table`]: RouterInner::store_table
    ///
    /// The request path re-reads this `ArcSwap` on every request, so every `[router]` key it
    /// consults takes effect on the **next** request with no restart: `anthropic_tools`,
    /// `unknown_model`, `max_body_bytes`, `log_usage`, `implicit_strategy`, and the
    /// `headers_timeout_ms` / `idle_timeout_ms` / `queue_timeout_ms` family. An in-flight
    /// stream keeps the configuration it started with, because the handler took a
    /// `load_full()` at its first line.
    ///
    /// Until this existed there was no way to reach the field: `AppState.cfg` and
    /// `RouterInner.cfg` are two `ArcSwap<Config>`s and only the first had a setter, so
    /// `POST /v1/reload` answered `{"ok":true,"issues":[]}` while the proxy went on serving
    /// the configuration the daemon booted with. A reload that reports success and changes
    /// nothing is worse than one that fails.
    ///
    /// **Three things are still not hot** and it is a deliberate limit, not an oversight:
    /// `connect_timeout_ms` and the pool shape are baked into the one pooled
    /// `reqwest::Client` ([`build_http_client`]), and `max_inflight_bytes` sized the global
    /// `Semaphore`, both at construction. Rebuilding either would drop live connections and
    /// in-flight permits mid-request. Changing those three keys still needs a restart.
    pub fn store_cfg(&self, cfg: Arc<Config>) {
        self.cfg.store(cfg);
    }

    /// The configuration the request path is using right now.
    ///
    /// A full `Arc` clone rather than a `Guard`, for the same reason `AppState::cfg` is: a
    /// caller that holds a `Guard` across an `.await` pins the old configuration for the
    /// life of a streaming response.
    pub fn cfg(&self) -> Arc<Config> {
        self.cfg.load_full()
    }

    /// The live backend registry.
    pub fn registry(&self) -> &BackendRegistry {
        &self.registry
    }

    /// **The parking primitive `ARCHITECTURE.md` §4.7 specifies.**
    ///
    /// A sequential swap calls [`WarmRegistry::open`] before it drains anything and holds the
    /// returned window until the alias points at the replacement; the request path calls
    /// [`WarmRegistry::parking_for`] only once a dispatch has already failed. Nothing else
    /// may open a window: parking a request is a promise that something is coming back, and
    /// a promise nobody is keeping is just a slower `503`.
    pub fn warm(&self) -> &WarmRegistry {
        &self.warm
    }
}

/// The one pooled `reqwest::Client`.
///
/// `no_gzip`/`no_brotli`/`no_deflate` are not an optimisation — a decoded body is not the
/// body the upstream sent, and the relay promises the bytes go through verbatim.
fn build_http_client(cfg: &Config) -> reqwest::Client {
    let built = reqwest::Client::builder()
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .connect_timeout(Duration::from_millis(cfg.router.connect_timeout_ms))
        .pool_max_idle_per_host(32)
        .build();
    match built {
        Ok(c) => c,
        Err(e) => {
            // The only documented failure is a broken TLS backend, which would make every
            // upstream unreachable anyway. Fall back to the default client so the daemon
            // still starts and reports the failure per request rather than at boot.
            tracing::error!(error = %e, "reqwest client builder failed; using the default client");
            reqwest::Client::new()
        }
    }
}

/// The empty table [`RouterInner::new`] starts from.
///
/// `RoutingTable` has no public constructor other than `TableBuilder::compile` (R-01), so
/// the initial value is a compile of a `RouteFile` with zero routes. That input cannot trip
/// any of the documented validation rules — every one of them is about a *route* — so the
/// `Err` arm below is unreachable in a correct build.
fn empty_table(cfg: &Config, registry: &BackendRegistry) -> RoutingTable {
    let default_alias = parse_alias_or_auto(&cfg.router.default_alias);
    let file = RouteFile {
        schema_version: 1,
        default_alias,
        routes: Vec::new(),
    };
    match TableBuilder::compile(cfg, &file, registry) {
        Ok(t) => t,
        Err(report) => panic!(
            "apexrouter-router: TableBuilder::compile rejected an EMPTY RouteFile ({report:?}). \
             RouterInner::new has no other way to construct an initial RoutingTable — R-01 \
             needs to publish an infallible `RoutingTable::empty(default_alias)`."
        ),
    }
}

/// `[router] default_alias`, falling back to the shipped default when it is not a slug.
///
/// `Alias` is a validated newtype with no `Default`, so the fallback chain is spelled out
/// rather than unwrapped. A configuration this broken is caught and reported by C-02's
/// loader long before the router sees it.
fn parse_alias_or_auto(configured: &str) -> Alias {
    if let Ok(a) = Alias::parse(configured) {
        return a;
    }
    match Alias::parse("auto") {
        Ok(a) => a,
        // `"auto"` is inside `^[a-z0-9][a-z0-9._-]{0,63}$` by inspection; this arm exists
        // only because the compiler cannot know that.
        Err(e) => panic!("apexrouter-router: the literal alias \"auto\" failed to parse: {e}"),
    }
}
