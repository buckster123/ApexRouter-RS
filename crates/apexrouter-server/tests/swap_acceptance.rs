//! FIX-5 acceptance: the four swap shapes MK1 measured, each with a **request loop running
//! across it**, plus the duplicate-start case.
//!
//! MK1 ACCEPTANCE, before this fix:
//!
//! | shape | result | loop |
//! |---|---|---|
//! | hot, both healthy | `mode:hot, drained_ms:405` | 661/661 x 200 — PASS |
//! | hot, starting B fails | A untouched | 1303/1303 x 200 — PASS |
//! | sequential, starting B fails | **A left drained for ever** | **1814/2056 x 503 — FAIL** |
//! | target is registered-but-down | **binds the dead one, drains the live one** | **7492/7601 x 503 — FAIL** |
//!
//! Both failures were one sin: the swap committed to a target it had not proven could serve.
//!
//! **No GPU, no GGUF, no model load.** Every backend here is either the fake `llama-server`
//! under ApexRouter's own supervisor — spawned, health-gated, argv-recorded, exactly as a
//! real one is — or the in-process stub upstream. Nothing but `127.0.0.1` is ever contacted.
//! The one number that could look like a benchmark (`drained_ms`) is a stopwatch over a
//! `sleep`, not a measurement of silicon.
//!
//! An integration test rather than a unit test in `api/routes.rs` on purpose: the same test
//! binary has to be runnable against the *pre-fix* sources to produce the "before" column,
//! which is impossible when the test lives inside the file being reverted.

use apexrouter_core::checks::Registry;
use apexrouter_core::config::{Config, ProviderCfg};
use apexrouter_core::lockfile::DaemonLock;
use apexrouter_core::paths::Paths;
use apexrouter_core::store::Store;
use apexrouter_core::usage::UsageWriter;
use apexrouter_protocol::{
    Alias, Backend, BackendId, BackendSelector, EndpointRecord, EndpointSpec, ErrorEnvelope,
    Health, LocalLlamaSpec, ModelRoute, NglPlan, SamplingMode, SplitMode, SplitPlan, SwapMode,
    SwapReport,
};
use apexrouter_providers::local::LocalProvisioner;
use apexrouter_server::AppState;
use apexrouter_tests_support::{FakeBuild, GgufSpec};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// FIX-5's own slice of the ephemeral range. Every concurrent agent gets a disjoint one so
/// two test binaries cannot fight over a port.
const PORTS: (u16, u16) = (39_700, 39_779);

/// Ports handed to one `Harness`. Enough for A, B and a restart of A with room to spare.
const PORTS_PER_HARNESS: u16 = 8;

/// Each `Harness` takes the next disjoint window of [`PORTS`].
///
/// The port reservation set lives on a `LocalProvisioner` instance, so two supervisors in one
/// test binary do not see each other's leases and will both propose the same free port —
/// which the second child then fails to bind. Sub-dividing the range is what makes these
/// tests safe to run in parallel, and running them serially instead would only hide it.
fn next_port_window() -> (u16, u16) {
    static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let lo = PORTS.0 + n * PORTS_PER_HARNESS;
    let hi = lo + PORTS_PER_HARNESS - 1;
    assert!(
        hi <= PORTS.1,
        "FIX-5's port range {PORTS:?} is exhausted at harness {n}; widen it"
    );
    (lo, hi)
}

/// How long the fake gets to bind and answer `/health`. Generous, because a loaded laptop
/// running four `cargo test` jobs is exactly when a tight deadline invents a flake.
const HEALTH_DEADLINE_MS: u64 = 20_000;

/// Gap between requests in the loop. ~2 ms puts a few hundred requests across a swap, which
/// is the same order as MK1's 661.
const LOOP_GAP: Duration = Duration::from_millis(2);

// ==========================================================================================
// harness
// ==========================================================================================

/// `Paths::resolve` reads the process environment, which is global to this test binary.
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// A `Paths` rooted at `dir`, with `$APEXROUTER_HOME` put back before anything else looks.
/// Copied from `tests-support/tests/router_wiring.rs`, which is where the README points.
fn paths_at(dir: &Path) -> Paths {
    let guard = match env_lock().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let previous = std::env::var_os("APEXROUTER_HOME");
    std::env::set_var("APEXROUTER_HOME", dir);
    let resolved = Paths::resolve();
    match previous {
        Some(v) => std::env::set_var("APEXROUTER_HOME", v),
        None => std::env::remove_var("APEXROUTER_HOME"),
    }
    drop(guard);
    let paths = resolved.expect("resolve paths");
    paths.ensure_layout().expect("layout");
    paths
}

/// The hermetic config: no discovery roots but the fake's, and `[providers.together]`
/// pointed at a **closed** loopback port so no probe can reach a paid endpoint.
fn test_config(fake: &FakeBuild, ports: (u16, u16)) -> Config {
    let mut cfg = Config::default();
    cfg.compat.mirror_usage_log = false;
    cfg.compat.read_legacy_state = false;
    cfg.router.log_usage = false;
    cfg.endpoints.build_roots = vec![fake.root().display().to_string()];
    cfg.endpoints.model_roots = vec![];
    cfg.endpoints.port_range = ports;
    cfg.supervisor.health_deadline_ms = HEALTH_DEADLINE_MS;
    cfg.supervisor.health_interval_ms = 50;
    cfg.server.drain_timeout_secs = 5;
    cfg.providers.insert(
        "together".to_owned(),
        ProviderCfg {
            base_url: "http://127.0.0.1:1/v1".to_owned(),
            api_key_env: None,
            api_key_file: None,
        },
    );
    cfg
}

/// A whole daemon: both listeners, the real supervisor, the real registry, the real table.
struct Harness {
    state: Arc<AppState>,
    /// Base URL of the control plane (`/v1/routes*`, `/v1/backends*`, `/v1/endpoints*`).
    control: String,
    /// Where the request loop points.
    proxy: SocketAddr,
    fake: FakeBuild,
    http: reqwest::Client,
    /// Kept alive: `$STATE` is inside it.
    _dir: tempfile::TempDir,
}

impl Harness {
    async fn start() -> Harness {
        let fake = FakeBuild::new();
        let dir = tempfile::TempDir::new().expect("tempdir");
        let paths = paths_at(dir.path());
        let cfg = test_config(&fake, next_port_window());

        let (tx, _rx) = tokio::sync::broadcast::channel(1024);
        let usage = UsageWriter::open(&paths, &cfg.compat).expect("usage writer");
        let router = apexrouter_router::RouterInner::new(Arc::new(cfg.clone()), tx.clone(), usage);
        let supervisor = Arc::new(LocalProvisioner::new(
            paths.clone(),
            cfg.clone(),
            tx.clone(),
        ));
        // Hand the supervisor a rig instead of letting it probe the box: 20 GB total, 19 GB
        // free, so a 1 MB fake model always `Fits` and `mode_from_fit` says hot.
        supervisor.set_rig(fake.rig(20_992, 19_518));

        let lock = DaemonLock::acquire(&paths).expect("daemon lock");
        let state = Arc::new(AppState::new(
            paths.clone(),
            cfg,
            Store::new(paths),
            router,
            tx,
            supervisor,
            Arc::new(Registry::new()),
            lock,
        ));

        // The control plane, on its own loopback port.
        let api = apexrouter_server::api::router().with_state(Arc::clone(&state));
        let listener =
            tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().expect("loopback"))
                .await
                .expect("bind control");
        let control = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            let _ = axum::serve(listener, api).await;
        });

        // The **real** proxy handler, so the loop is real traffic through the real request
        // path rather than a simulation of one.
        let proxy_app = apexrouter_router::proxy_router(Arc::clone(&state.router));
        let listener =
            tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().expect("loopback"))
                .await
                .expect("bind proxy");
        let proxy = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, proxy_app).await;
        });

        Harness {
            state,
            control,
            proxy,
            fake,
            http: reqwest::Client::new(),
            _dir: dir,
        }
    }

    /// A fake-`llama-server` spec. `behavior` rides in through `extra_args`, which the argv
    /// builder passes through verbatim — that is the documented delivery route.
    fn spec(&self, model: &Path, alias_flag: &str, ctx: u32, behavior: &str) -> EndpointSpec {
        EndpointSpec::LocalLlama(LocalLlamaSpec {
            build: self.fake.build_id(),
            model_path: model.display().to_string(),
            mmproj: None,
            alias_flag: alias_flag.to_owned(),
            host: "127.0.0.1".to_owned(),
            port: None,
            ctx: Some(ctx),
            parallel: Some(2),
            kv_type: None,
            ngl: NglPlan::All,
            split: SplitPlan {
                devices: vec!["Vulkan0".to_owned()],
                mode: SplitMode::Layer,
                main_gpu: None,
                tensor_split: None,
            },
            mode: SamplingMode::Coding,
            flash_attn: None,
            api_key: None,
            extra_args: if behavior.is_empty() {
                Vec::new()
            } else {
                vec!["--apex-behavior".to_owned(), behavior.to_owned()]
            },
        })
    }

    /// A 1 MB fake GGUF under the build tree.
    fn model(&self, name: &str) -> PathBuf {
        self.fake.model(name, &GgufSpec::default().sized_mb(1))
    }

    /// Start an endpoint under the real supervisor and bind `auto` to it.
    async fn start_and_bind(&self, spec: &EndpointSpec, alias: &str) -> BackendId {
        let res = self
            .http
            .post(format!("{}/v1/endpoints?alias={alias}", self.control))
            .json(spec)
            .send()
            .await
            .expect("POST /v1/endpoints");
        assert_eq!(
            res.status(),
            201,
            "{}",
            res.text().await.unwrap_or_default()
        );
        let rec: EndpointRecord = res.json().await.expect("EndpointRecord");
        rec.id
    }

    /// `POST /v1/routes/{alias}/swap`, returning the status and the raw body.
    async fn swap(&self, alias: &str, body: serde_json::Value) -> (u16, String) {
        let res = self
            .http
            .post(format!("{}/v1/routes/{alias}/swap", self.control))
            .json(&body)
            .send()
            .await
            .expect("swap");
        let status = res.status().as_u16();
        (status, res.text().await.unwrap_or_default())
    }

    /// Which backend the alias points at on disk, which is what a reload would reproduce.
    fn bound_to(&self, alias: &str) -> Option<BackendId> {
        let want = Alias::parse(alias).expect("alias");
        let file = self.state.store.load_routes().ok()?;
        let route: &ModelRoute = file.routes.iter().find(|r| r.alias == want)?;
        route.targets.iter().find_map(|t| match &t.backend {
            BackendSelector::Id(id) => Some(id.clone()),
            _ => None,
        })
    }

    /// What `apexrouter backend ls` would print for one id — the **computed** health, not a
    /// stored string.
    async fn backend(&self, id: &BackendId) -> Option<Backend> {
        let all: Vec<Backend> = self
            .http
            .get(format!("{}/v1/backends", self.control))
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        all.into_iter().find(|b| &b.id == id)
    }

    /// Every endpoint record. Counting these is how "did it start a second copy?" is asked.
    async fn endpoints(&self) -> Vec<EndpointRecord> {
        self.http
            .get(format!("{}/v1/endpoints", self.control))
            .send()
            .await
            .expect("GET /v1/endpoints")
            .json()
            .await
            .expect("Vec<EndpointRecord>")
    }
}

/// Kill every child this harness started, **including when a test panics**.
///
/// A `Drop` rather than a helper the tests remember to call: the runs that leak are exactly
/// the runs that failed, and a fake `llama-server` is `setsid` — it outlives the test binary
/// happily, holding a port in the range the next run wants. `stop_graceful` verifies the
/// recorded identity (pid ∧ start ticks ∧ boot id) before it signals, so a reused pid is
/// never the thing that gets killed.
impl Drop for Harness {
    fn drop(&mut self) {
        for rec in self.state.store.list_endpoints().unwrap_or_default() {
            if let Some(facts) = rec.proc.as_ref() {
                let _ = apexrouter_core::proc::stop_graceful(
                    facts,
                    Duration::from_millis(500),
                    Duration::from_millis(500),
                );
            }
        }
    }
}

// ==========================================================================================
// the request loop
// ==========================================================================================

/// What a loop saw. `five_xx` is the number under test; `transport` would mean our own
/// listener dropped a connection, which is a different bug and must not hide in `other`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Counts {
    total: u32,
    ok: u32,
    five_xx: u32,
    other: u32,
    transport: u32,
}

impl std::fmt::Display for Counts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}/{} x 200, {} x 5xx, {} x other, {} transport",
            self.ok, self.total, self.five_xx, self.other, self.transport
        )
    }
}

/// A request loop through the real proxy, running until it is stopped.
struct Hammer {
    stop: Arc<AtomicBool>,
    handle: tokio::task::JoinHandle<Counts>,
}

impl Hammer {
    fn start(addr: SocketAddr) -> Hammer {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = tokio::spawn(async move {
            let http = reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default();
            let url = format!("http://{addr}/v1/chat/completions");
            let body = serde_json::json!({
                "model": "auto",
                "messages": [{"role": "user", "content": "ping"}],
                "max_tokens": 4,
                "stream": false,
            });
            let mut c = Counts::default();
            while !flag.load(Ordering::Relaxed) {
                c.total += 1;
                match http.post(&url).json(&body).send().await {
                    Ok(r) if r.status().is_success() => c.ok += 1,
                    Ok(r) if r.status().is_server_error() => c.five_xx += 1,
                    Ok(_) => c.other += 1,
                    Err(_) => c.transport += 1,
                }
                tokio::time::sleep(LOOP_GAP).await;
            }
            c
        });
        Hammer { stop, handle }
    }

    async fn finish(self) -> Counts {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.await.expect("loop task")
    }
}

/// Run a loop for a fixed window and report what it saw. Used **after** a swap has returned,
/// to prove the steady state is healthy rather than merely that the call did not error.
async fn steady_state(addr: SocketAddr, window: Duration) -> Counts {
    let h = Hammer::start(addr);
    tokio::time::sleep(window).await;
    h.finish().await
}

/// Parse the daemon's refusal, which is always an `ErrorEnvelope` and never an axum string.
fn envelope(body: &str) -> ErrorEnvelope {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("not an ErrorEnvelope ({e}): {body}"))
}

// ==========================================================================================
// shape 1 — hot, both healthy. The control: this one passed before and must keep passing.
// ==========================================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hot_swap_between_two_healthy_backends_serves_every_request() {
    let h = Harness::start().await;
    let a = h.model("A-9b-Q6_K.gguf");
    let b = h.model("B-9b-Q6_K.gguf");
    let id_a = h
        .start_and_bind(&h.spec(&a, "model-a", 4096, ""), "auto")
        .await;

    let loop_ = Hammer::start(h.proxy);
    tokio::time::sleep(Duration::from_millis(150)).await;

    let (status, body) = h
        .swap(
            "auto",
            serde_json::json!({"to": h.spec(&b, "model-b", 4096, "")}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let report: SwapReport = serde_json::from_str(&body).expect("SwapReport");

    tokio::time::sleep(Duration::from_millis(150)).await;
    let counts = loop_.finish().await;

    assert_eq!(report.mode, SwapMode::Hot);
    assert_eq!(report.from.as_ref(), Some(&id_a));
    assert_ne!(report.to, id_a, "the alias really moved");
    assert_eq!(h.bound_to("auto").as_ref(), Some(&report.to));
    eprintln!("shape 1 (hot, both healthy): {counts}  {report:?}");
    assert_eq!(
        counts.five_xx, 0,
        "hot swap must not drop a request: {counts}"
    );
    assert_eq!(counts.transport, 0, "{counts}");
    assert!(
        counts.ok > 20,
        "the loop has to have actually run: {counts}"
    );
}

// ==========================================================================================
// shape 2 — hot, starting B fails. A is untouched, as documented.
// ==========================================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hot_swap_whose_replacement_refuses_to_start_leaves_the_old_one_serving() {
    let h = Harness::start().await;
    let a = h.model("A-9b-Q6_K.gguf");
    let b = h.model("B-9b-Q6_K.gguf");
    let id_a = h
        .start_and_bind(&h.spec(&a, "model-a", 4096, ""), "auto")
        .await;

    let loop_ = Hammer::start(h.proxy);
    tokio::time::sleep(Duration::from_millis(150)).await;

    let (status, body) = h
        .swap(
            "auto",
            serde_json::json!({
                "to": h.spec(&b, "model-b", 4096, "refuse_start"),
                "mode": "hot",
            }),
        )
        .await;
    assert!(status >= 400, "a launch that failed is not a swap: {body}");

    tokio::time::sleep(Duration::from_millis(150)).await;
    let counts = loop_.finish().await;

    eprintln!("shape 2 (hot, B refuses to start): {counts}  {status} {body}");
    assert_eq!(h.bound_to("auto").as_ref(), Some(&id_a), "A untouched");
    assert!(matches!(
        h.backend(&id_a).await.map(|b| b.health),
        Some(Health::Ready { .. })
    ));
    assert_eq!(counts.five_xx, 0, "A never stopped serving: {counts}");
    assert_eq!(counts.transport, 0, "{counts}");
    assert!(counts.ok > 20, "{counts}");
}

// ==========================================================================================
// shape 3 — sequential, starting B fails. THE FIX: A comes back.
// ==========================================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sequential_swap_whose_replacement_refuses_to_start_puts_the_old_one_back() {
    let h = Harness::start().await;
    let a = h.model("A-9b-Q6_K.gguf");
    let b = h.model("B-9b-Q6_K.gguf");
    let id_a = h
        .start_and_bind(&h.spec(&a, "model-a", 4096, ""), "auto")
        .await;

    let across = Hammer::start(h.proxy);
    tokio::time::sleep(Duration::from_millis(150)).await;

    let started = Instant::now();
    let (status, body) = h
        .swap(
            "auto",
            serde_json::json!({
                "to": h.spec(&b, "model-b", 4096, "refuse_start"),
                "mode": "sequential",
            }),
        )
        .await;
    let swap_ms = started.elapsed().as_millis();
    let during = across.finish().await;

    // **The steady state is the acceptance.** Before the fix this loop was 100% `503` for
    // ever; the gap while A is genuinely absent is inherent to a sequential swap (see the
    // module header on `parked`) and is bounded by the launch. Measured and printed *before*
    // any assertion, so the pre-fix numbers survive the failure that produces them.
    let after = steady_state(h.proxy, Duration::from_millis(400)).await;
    eprintln!(
        "shape 3 (sequential, B refuses to start): across the {swap_ms} ms swap: {during} | \
         afterwards: {after}"
    );

    assert!(status >= 400, "a launch that failed is not a swap: {body}");

    // The refusal has to say what happened to A, not just that B failed.
    let env = envelope(&body);
    assert!(
        env.error.message.contains(id_a.as_str()),
        "the refusal must name the backend it put back: {}",
        env.error.message
    );
    assert!(
        env.error.message.contains("brought back"),
        "the refusal must say A was restored: {}",
        env.error.message
    );

    // A is running again, the alias points at it again, and the record is one record.
    assert_eq!(
        h.bound_to("auto").as_ref(),
        Some(&id_a),
        "the alias must not be left pointing into a hole"
    );
    assert!(
        matches!(
            h.backend(&id_a).await.map(|b| b.health),
            Some(Health::Ready { .. })
        ),
        "A is serving again: {:?}",
        h.backend(&id_a).await.map(|b| b.health)
    );

    assert_eq!(
        after.five_xx, 0,
        "after the swap returns the alias must serve every request: {after}"
    );
    assert_eq!(after.transport, 0, "{after}");
    assert!(after.ok > 20, "{after}");
    assert!(
        during.ok > 0,
        "requests before the drain must have been served: {during}"
    );
}

// ==========================================================================================
// shape 4 — the target is a registered-but-down backend.
// ==========================================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn swapping_to_a_backend_that_cannot_answer_is_refused_and_names_the_remedy() {
    let h = Harness::start().await;
    let a = h.model("A-9b-Q6_K.gguf");
    let id_a = h
        .start_and_bind(&h.spec(&a, "model-a", 4096, ""), "auto")
        .await;

    // A node registered against a closed loopback port: enabled, `Unknown`, and therefore
    // indistinguishable from a live one to anything that reads the stored health.
    let dead: Backend = h
        .http
        .post(format!("{}/v1/backends", h.control))
        .json(&serde_json::json!({
            "base_url": "http://127.0.0.1:1",
            "credential": {"kind": "none"},
            "label": "a box that is off",
            "declared_models": ["ghost-model"],
        }))
        .send()
        .await
        .expect("register")
        .json()
        .await
        .expect("Backend");

    let loop_ = Hammer::start(h.proxy);
    tokio::time::sleep(Duration::from_millis(150)).await;

    let (status, body) = h
        .swap("auto", serde_json::json!({"to": dead.id.as_str()}))
        .await;

    tokio::time::sleep(Duration::from_millis(150)).await;
    let counts = loop_.finish().await;

    eprintln!("shape 4 (target is registered-but-down): {counts}  {status} {body}");
    assert_eq!(status, 409, "{body}");
    let env = envelope(&body);
    assert_eq!(env.error.kind, "backend_not_ready");
    assert!(
        env.error.message.contains("apexrouter backend probe"),
        "the refusal must name a command that helps: {}",
        env.error.message
    );

    assert_eq!(
        h.bound_to("auto").as_ref(),
        Some(&id_a),
        "the live backend must not have been drained for a dead one"
    );
    assert!(matches!(
        h.backend(&id_a).await.map(|b| b.health),
        Some(Health::Ready { .. })
    ));
    // The probe the refusal was based on is now the record, so `backend ls` agrees with it.
    assert!(
        matches!(
            h.backend(&dead.id).await.map(|b| b.health),
            Some(Health::Down { .. })
        ),
        "the probe's finding is folded into the record: {:?}",
        h.backend(&dead.id).await.map(|b| b.health)
    );
    assert_eq!(
        counts.five_xx, 0,
        "nothing moved, so nothing broke: {counts}"
    );
    assert_eq!(counts.transport, 0, "{counts}");
    assert!(counts.ok > 20, "{counts}");
}

/// The remedy MK1 said "nothing surfaces": a **disabled** backend names `backend enable`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn swapping_to_a_disabled_backend_names_backend_enable() {
    let h = Harness::start().await;
    let a = h.model("A-9b-Q6_K.gguf");
    let b = h.model("B-9b-Q6_K.gguf");
    let id_a = h
        .start_and_bind(&h.spec(&a, "model-a", 4096, ""), "auto")
        .await;
    let id_b = h
        .start_and_bind(&h.spec(&b, "model-b", 4096, ""), "spare")
        .await;

    let res = h
        .http
        .patch(format!("{}/v1/backends/{id_b}", h.control))
        .json(&serde_json::json!({"enabled": false}))
        .send()
        .await
        .expect("disable");
    assert_eq!(res.status(), 200);

    let loop_ = Hammer::start(h.proxy);
    tokio::time::sleep(Duration::from_millis(150)).await;
    let (status, body) = h
        .swap("auto", serde_json::json!({"to": id_b.as_str()}))
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let counts = loop_.finish().await;

    eprintln!("shape 4b (target disabled): {counts}  {status} {body}");
    assert_eq!(status, 409, "{body}");
    let env = envelope(&body);
    assert!(
        env.error
            .message
            .contains(&format!("apexrouter backend enable {id_b}")),
        "the one recovery that surfaced nowhere must be in the refusal: {}",
        env.error.message
    );
    assert_eq!(h.bound_to("auto").as_ref(), Some(&id_a));
    assert_eq!(counts.five_xx, 0, "{counts}");

    // …and it is on the backend itself, where `apexrouter backend show` and the snapshot's
    // standing alerts both read it.
    let shown = h.backend(&id_b).await.expect("backend");
    assert!(
        shown
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("apexrouter backend enable"),
        "last_error should carry the remedy: {:?}",
        shown.last_error
    );
}

// ==========================================================================================
// health is computed on read
// ==========================================================================================

/// MK1: "`backend show` still read `health ready` while the proxy answered 503". A backend
/// that has stopped accepting can never read `Ready` again, because the read computes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_backend_that_stopped_accepting_never_reads_ready() {
    let h = Harness::start().await;
    let a = h.model("A-9b-Q6_K.gguf");
    let id_a = h
        .start_and_bind(&h.spec(&a, "model-a", 4096, ""), "auto")
        .await;

    assert!(matches!(
        h.backend(&id_a).await.map(|b| b.health),
        Some(Health::Ready { .. })
    ));

    // Exactly what a swap's drain does to the live flag, without going through a swap.
    h.state
        .router
        .registry()
        .get(&id_a)
        .expect("live")
        .accepting
        .store(false, Ordering::Release);

    let shown = h.backend(&id_a).await.expect("backend");
    assert!(
        !shown.health.is_routable(),
        "a row that says ready while resolve() skips it is the lie FIX-5 closes: {:?}",
        shown.health
    );
    assert!(
        matches!(shown.health, Health::Draining { .. }),
        "{:?}",
        shown.health
    );
}

// ==========================================================================================
// swapping to a model something already serves
// ==========================================================================================

/// `swap auto --to <model>` when a healthy endpoint already serves those weights must
/// **re-point**, not start a second copy — a duplicate is what OOMs a 24 GB box, and the OOM
/// takes the first copy with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn swapping_to_an_already_served_model_repoints_instead_of_starting_a_second_copy() {
    let h = Harness::start().await;
    let a = h.model("A-9b-Q6_K.gguf");
    let id_a = h
        .start_and_bind(&h.spec(&a, "model-a", 4096, ""), "auto")
        .await;
    assert_eq!(h.endpoints().await.len(), 1);

    // The same weights, spelled differently: a different context and a different `-a`, which
    // is enough for `same_endpoint` to call it a new endpoint and start a second server.
    let duplicate = h.spec(&a, "model-a-again", 8192, "");

    let loop_ = Hammer::start(h.proxy);
    tokio::time::sleep(Duration::from_millis(150)).await;
    let (status, body) = h.swap("auto", serde_json::json!({"to": duplicate})).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let counts = loop_.finish().await;

    eprintln!("already-served: {counts}  {status} {body}");
    assert_eq!(status, 200, "{body}");
    let report: SwapReport = serde_json::from_str(&body).expect("SwapReport");
    assert_eq!(report.to, id_a, "the alias re-points at what is already up");
    assert_eq!(
        h.endpoints().await.len(),
        1,
        "a second copy of the same weights must never be started"
    );
    assert_eq!(h.bound_to("auto").as_ref(), Some(&id_a));
    assert!(matches!(
        h.backend(&id_a).await.map(|b| b.health),
        Some(Health::Ready { .. })
    ));
    assert_eq!(counts.five_xx, 0, "{counts}");
}
