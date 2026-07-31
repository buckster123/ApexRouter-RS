//! **The warm-queue acceptance.** `ARCHITECTURE.md` §4.7's parking primitive, measured with a
//! request loop running across a real **Sequential** swap whose replacement is deliberately
//! slow to pass its health gate.
//!
//! The defect this closes, as MK1 measured it:
//!
//! | shape | result | loop |
//! |---|---|---|
//! | sequential, B starts normally | alias moved | **33/51 x 200 plus 18 x 5xx across a 72 ms window** |
//!
//! 72 ms is a *fake* exiting. In production that window is the model-load time for a 7 GB
//! GGUF — minutes — and on a box where two 7 GB models cannot coexist, **Sequential is the
//! common path**. A swap the client notices breaks the one promise the product makes.
//!
//! What is measured here, in one binary and with no GPU:
//!
//! * [`sequential_swap_across_a_slow_replacement_parks_instead_of_dropping_requests`] — the
//!   **after** number: zero 5xx across a swap whose replacement takes seconds to become
//!   healthy, with the parked depth reported in `SwapReport::parked`;
//! * [`the_same_outage_without_a_warm_window_is_the_503_storm`] — the **before** number, from
//!   the identical outage produced without a swap (`POST /v1/endpoints/{id}/stop`), so the
//!   comparison is two rows of the same run rather than a memory of an older build;
//! * [`overflowing_the_warm_queue_is_a_503_with_a_retry_after`] — `warm_queue_max` at its
//!   shipped default of 32, overflowed by 40 concurrent clients;
//! * [`a_warm_window_that_expires_is_a_503_with_a_retry_after`] — the timeout arm, against the
//!   real daemon and the real proxy.
//!
//! **No GPU, no GGUF, no model load.** Every backend is the fake `llama-server` under
//! ApexRouter's own supervisor, and `load_ms=` is how it is told to be slow. Nothing but
//! `127.0.0.1` is ever contacted, and no number here is a benchmark: the "model load" is a
//! `sleep` and the throughput is arithmetic.

use apexrouter_core::checks::Registry;
use apexrouter_core::config::{Config, ProviderCfg};
use apexrouter_core::lockfile::DaemonLock;
use apexrouter_core::paths::Paths;
use apexrouter_core::store::Store;
use apexrouter_core::usage::UsageWriter;
use apexrouter_protocol::{
    Alias, BackendId, BackendSelector, EndpointRecord, EndpointSpec, Health, LocalLlamaSpec,
    ModelRoute, NglPlan, SamplingMode, SplitMode, SplitPlan, SwapMode, SwapReport,
};
use apexrouter_providers::local::LocalProvisioner;
use apexrouter_server::AppState;
use apexrouter_tests_support::{FakeBuild, GgufSpec};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// This binary's own slice of the ephemeral range, disjoint from every other test binary's.
const PORTS: (u16, u16) = (39_880, 39_959);

/// Ports handed to one `Harness`: A, B and a restart of A, with room to spare.
const PORTS_PER_HARNESS: u16 = 8;

/// How long the fake gets to bind and answer `/health`. Generous, because a loaded laptop
/// running four `cargo test` jobs is exactly when a tight deadline invents a flake.
const HEALTH_DEADLINE_MS: u64 = 30_000;

/// How long the replacement pretends to load its weights.
///
/// The point of the whole exercise: long enough that a request loop unquestionably crosses
/// the gap, short enough that the suite stays quick. In production this is minutes.
const SLOW_LOAD_MS: u64 = 2_500;

/// Gap between one worker's requests.
const LOOP_GAP: Duration = Duration::from_millis(5);

/// Concurrent workers in the request loop. More than one on purpose: a serial loop can only
/// ever park a single request, and the number under test is a *queue depth*.
const WORKERS: usize = 8;

/// `warm_queue_max`'s shipped default, restated here so the overflow test fails loudly if the
/// constant ever moves without this test being reconsidered.
const WARM_QUEUE_MAX: usize = 32;

/// Each `Harness` takes the next disjoint window of [`PORTS`].
fn next_port_window() -> (u16, u16) {
    static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let lo = PORTS.0 + n * PORTS_PER_HARNESS;
    let hi = lo + PORTS_PER_HARNESS - 1;
    assert!(
        hi <= PORTS.1,
        "the warm-queue port range {PORTS:?} is exhausted at harness {n}; widen it"
    );
    (lo, hi)
}

// ==========================================================================================
// harness — the same daemon `swap_acceptance.rs` boots, on its own ports
// ==========================================================================================

/// `Paths::resolve` reads the process environment, which is global to this test binary.
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// A `Paths` rooted at `dir`, with `$APEXROUTER_HOME` put back before anything else looks.
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

/// The hermetic config: no discovery roots but the fake's, and `[providers.together]` pointed
/// at a **closed** loopback port so no probe can reach a paid endpoint.
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
    control: String,
    proxy: SocketAddr,
    fake: FakeBuild,
    http: reqwest::Client,
    _dir: tempfile::TempDir,
}

impl Harness {
    async fn start() -> Harness {
        let fake = FakeBuild::new();
        let dir = tempfile::TempDir::new().expect("tempdir");
        let paths = paths_at(dir.path());
        let cfg = test_config(&fake, next_port_window());

        let (tx, _rx) = tokio::sync::broadcast::channel(4096);
        let usage = UsageWriter::open(&paths, &cfg.compat).expect("usage writer");
        let router = apexrouter_router::RouterInner::new(Arc::new(cfg.clone()), tx.clone(), usage);
        let supervisor = Arc::new(LocalProvisioner::new(
            paths.clone(),
            cfg.clone(),
            tx.clone(),
        ));
        // A rig instead of probing the box: 20 GB total, 19 GB free, so a 1 MB fake model
        // always `Fits`. Sequential is therefore always an explicit `mode` override here,
        // which is the honest way to test it without pretending the laptop is full.
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
    /// builder passes through verbatim — the documented delivery route.
    fn spec(&self, model: &Path, alias_flag: &str, behavior: &str) -> EndpointSpec {
        EndpointSpec::LocalLlama(LocalLlamaSpec {
            build: self.fake.build_id(),
            model_path: model.display().to_string(),
            mmproj: None,
            alias_flag: alias_flag.to_owned(),
            host: "127.0.0.1".to_owned(),
            port: None,
            ctx: Some(4096),
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

    /// Start an endpoint under the real supervisor and bind an alias to it.
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
            .timeout(Duration::from_secs(120))
            .send()
            .await
            .expect("swap");
        let status = res.status().as_u16();
        (status, res.text().await.unwrap_or_default())
    }

    /// `POST /v1/endpoints/{id}/stop` — the same outage a sequential swap's drain produces,
    /// with **no** warm window, which is what the code did before §4.7 was implemented.
    async fn stop(&self, id: &BackendId) {
        let res = self
            .http
            .post(format!("{}/v1/endpoints/{id}/stop", self.control))
            .send()
            .await
            .expect("stop");
        assert!(res.status().is_success(), "stop failed: {}", res.status());
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

    /// The **computed** health of one backend, straight off the control plane.
    async fn health(&self, id: &BackendId) -> Option<Health> {
        let all: Vec<apexrouter_protocol::Backend> = self
            .http
            .get(format!("{}/v1/backends", self.control))
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        all.into_iter().find(|b| &b.id == id).map(|b| b.health)
    }

    /// Block until a warm window is open on `alias`, or give up.
    ///
    /// Polls the primitive itself rather than sleeping a guessed number of milliseconds: the
    /// window is opened *before* the drain, so this is the earliest moment at which a request
    /// can park, and starting the clients any earlier would test the wrong thing.
    async fn await_warming(&self, alias: &str, within: Duration) -> bool {
        let alias = Alias::parse(alias).expect("alias");
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if self.state.router.warm().parking_for(&alias).is_some() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        false
    }
}

/// Kill every child this harness started, **including when a test panics**.
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

impl Counts {
    fn merge(mut self, o: Counts) -> Counts {
        self.total += o.total;
        self.ok += o.ok;
        self.five_xx += o.five_xx;
        self.other += o.other;
        self.transport += o.transport;
        self
    }
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

/// A concurrent request loop through the real proxy, running until it is stopped.
struct Hammer {
    stop: Arc<AtomicBool>,
    handles: Vec<tokio::task::JoinHandle<Counts>>,
}

impl Hammer {
    fn start(addr: SocketAddr, workers: usize) -> Hammer {
        let stop = Arc::new(AtomicBool::new(false));
        let handles = (0..workers)
            .map(|_| {
                let flag = Arc::clone(&stop);
                tokio::spawn(async move {
                    // Comfortably longer than the load the replacement fakes: a client that
                    // gave up at 10 s would be counted as a transport failure and would hide
                    // the very thing being measured.
                    let http = reqwest::Client::builder()
                        .timeout(Duration::from_secs(60))
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
                })
            })
            .collect();
        Hammer { stop, handles }
    }

    async fn finish(self) -> Counts {
        self.stop.store(true, Ordering::Relaxed);
        let mut total = Counts::default();
        for h in self.handles {
            total = total.merge(h.await.expect("loop task"));
        }
        total
    }
}

/// `error.code` out of a refusal from the **proxy** listener.
///
/// Deliberately not `ErrorEnvelope`: that is the control plane's shape, and a client of the
/// proxy is an OpenAI SDK which will parse `{"error":{"message","type","code","param"}}` and
/// nothing else. Asserting on the OpenAI shape here is asserting on what the client sees.
fn openai_error_code(body: &str) -> String {
    let json: serde_json::Value = serde_json::from_str(body)
        .unwrap_or_else(|e| panic!("the proxy must answer OpenAI-shaped JSON ({e}): {body}"));
    json["error"]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("no error.code in {body}"))
        .to_owned()
}

// ==========================================================================================
// AFTER — zero 5xx across a sequential swap whose replacement is slow
// ==========================================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sequential_swap_across_a_slow_replacement_parks_instead_of_dropping_requests() {
    let h = Harness::start().await;
    let a = h.model("A-9b-Q6_K.gguf");
    let b = h.model("B-9b-Q6_K.gguf");
    let id_a = h.start_and_bind(&h.spec(&a, "model-a", ""), "auto").await;

    let across = Hammer::start(h.proxy, WORKERS);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let started = Instant::now();
    let (status, body) = h
        .swap(
            "auto",
            serde_json::json!({
                "to": h.spec(&b, "model-b", &format!("load_ms={SLOW_LOAD_MS}")),
                "mode": "sequential",
            }),
        )
        .await;
    let swap_ms = started.elapsed().as_millis();

    tokio::time::sleep(Duration::from_millis(200)).await;
    let counts = across.finish().await;

    assert_eq!(status, 200, "{body}");
    let report: SwapReport = serde_json::from_str(&body).expect("SwapReport");
    eprintln!("AFTER  (sequential, B loads for {SLOW_LOAD_MS} ms): across the {swap_ms} ms swap: {counts}  {report:?}");

    assert_eq!(
        report.mode,
        SwapMode::Sequential,
        "the mode under test, not the one fit() would have picked"
    );
    assert!(
        swap_ms >= u128::from(SLOW_LOAD_MS),
        "the replacement has to have actually been slow, or nothing was parked across \
         anything: {swap_ms} ms"
    );
    assert_eq!(
        counts.five_xx, 0,
        "a sequential swap must not drop a request: {counts}"
    );
    assert_eq!(counts.transport, 0, "{counts}");
    assert_eq!(counts.other, 0, "{counts}");
    assert!(
        counts.ok > 50,
        "the loop has to have actually run: {counts}"
    );

    // The depth is the number both GUIs render and `SwapReport::parked` carries. Every worker
    // has exactly one request in flight, and each of those parks for the whole load.
    assert!(
        report.parked > 0,
        "requests crossed the gap without parking, which means they were served by something \
         that should have been stopped: {report:?}"
    );
    assert!(
        report.parked <= WORKERS as u32,
        "more parked than there are clients: {report:?}"
    );

    // …and the swap really happened underneath all that.
    assert_ne!(report.to, id_a, "the alias really moved");
    assert_eq!(h.bound_to("auto").as_ref(), Some(&report.to));
    assert!(matches!(
        h.health(&report.to).await,
        Some(Health::Ready { .. })
    ));
    // Nothing is left warming once the swap has returned.
    assert!(
        !h.state.router.warm().any_open(),
        "the window outlived the swap that opened it"
    );
}

// ==========================================================================================
// BEFORE — the same outage, with no warm window
// ==========================================================================================

/// The pre-fix number, produced in this run rather than remembered from an older build.
///
/// `POST /v1/endpoints/{id}/stop` is exactly what a sequential swap's drain does to A — stop
/// accepting, wait for the router's own in-flight counter, take the process down, mark the
/// row `Down` — with the one difference that no warm window is opened. That difference is the
/// whole fix, and this is what it looks like without it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_same_outage_without_a_warm_window_is_the_503_storm() {
    let h = Harness::start().await;
    let a = h.model("A-9b-Q6_K.gguf");
    let id_a = h.start_and_bind(&h.spec(&a, "model-a", ""), "auto").await;

    let across = Hammer::start(h.proxy, WORKERS);
    tokio::time::sleep(Duration::from_millis(200)).await;

    h.stop(&id_a).await;
    // The same wall-clock gap the replacement's load costs in the test above.
    tokio::time::sleep(Duration::from_millis(SLOW_LOAD_MS)).await;
    let counts = across.finish().await;

    eprintln!("BEFORE (the same {SLOW_LOAD_MS} ms outage, no warm window): {counts}");
    assert!(
        counts.five_xx > 0,
        "if this is zero the comparison above is measuring nothing: {counts}"
    );
    assert!(
        counts.ok > 50,
        "the loop ran before the outage too: {counts}"
    );
    assert!(matches!(h.health(&id_a).await, Some(Health::Down { .. })));
}

// ==========================================================================================
// overflow — warm_queue_max, at its shipped default
// ==========================================================================================

/// 40 concurrent clients against a 32-deep queue: 32 park and are served, the rest get an
/// OpenAI-shaped `503` with `Retry-After` **immediately**.
///
/// Refusing at the bound rather than queueing deeper is the point: a queue that grows without
/// limit turns a two-minute model load into two minutes of clients that have all already
/// timed out by the time they are woken.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overflowing_the_warm_queue_is_a_503_with_a_retry_after() {
    let h = Harness::start().await;
    let a = h.model("A-9b-Q6_K.gguf");
    let b = h.model("B-9b-Q6_K.gguf");
    let _id_a = h.start_and_bind(&h.spec(&a, "model-a", ""), "auto").await;

    // A long load, so all forty clients are unquestionably inside the window.
    let spec = h.spec(&b, "model-b", "load_ms=4000");
    let control = h.control.clone();
    let swapping = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("{control}/v1/routes/auto/swap"))
            .json(&serde_json::json!({"to": spec, "mode": "sequential"}))
            .timeout(Duration::from_secs(120))
            .send()
            .await
            .expect("swap")
            .status()
            .as_u16()
    });

    assert!(
        h.await_warming("auto", Duration::from_secs(10)).await,
        "the swap never opened a warm window"
    );

    let url = format!("http://{}/v1/chat/completions", h.proxy);
    let clients: Vec<_> = (0..40)
        .map(|_| {
            let url = url.clone();
            tokio::spawn(async move {
                let http = reqwest::Client::builder()
                    .timeout(Duration::from_secs(60))
                    .build()
                    .unwrap_or_default();
                let res = http
                    .post(&url)
                    .json(&serde_json::json!({
                        "model": "auto",
                        "messages": [{"role": "user", "content": "ping"}],
                        "max_tokens": 4,
                    }))
                    .send()
                    .await
                    .expect("request");
                let status = res.status().as_u16();
                let retry_after = res
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                (status, retry_after, res.text().await.unwrap_or_default())
            })
        })
        .collect();

    let mut served = 0u32;
    let mut refused = 0u32;
    for c in clients {
        let (status, retry_after, body) = c.await.expect("client task");
        match status {
            200 => served += 1,
            503 => {
                refused += 1;
                let secs: u32 = retry_after
                    .as_deref()
                    .unwrap_or("")
                    .parse()
                    .unwrap_or_else(|_| {
                        panic!("a 503 from a full warm queue must carry Retry-After: {body}")
                    });
                assert!(secs >= 1, "Retry-After must be usable: {secs}");
                assert_eq!(openai_error_code(&body), "warm_queue_full", "{body}");
            }
            other => panic!("unexpected {other}: {body}"),
        }
    }
    let swap_status = swapping.await.expect("swap task");
    let deepest = h
        .state
        .router
        .warm()
        .parked(&Alias::parse("auto").expect("alias"));

    eprintln!(
        "OVERFLOW (40 clients, warm_queue_max = {WARM_QUEUE_MAX}): {served} x 200, \
         {refused} x 503 + Retry-After, swap {swap_status}, {deepest} still parked"
    );
    assert_eq!(swap_status, 200, "the swap itself still succeeded");
    assert_eq!(served + refused, 40);
    assert!(
        refused >= 1,
        "40 clients against a {WARM_QUEUE_MAX}-deep queue must overflow it: {refused}"
    );
    assert!(
        refused <= 40 - WARM_QUEUE_MAX as u32,
        "at most the clients past the bound may be refused: {refused}"
    );
    assert!(
        served >= WARM_QUEUE_MAX as u32,
        "everything that fitted in the queue must have been served: {served}"
    );
    assert_eq!(deepest, 0, "nothing may be left parked after the window");
}

// ==========================================================================================
// timeout — the window's own deadline
// ==========================================================================================

/// The third exit §4.7 names: the window expires with the alias still unable to serve.
///
/// The window is opened directly on the primitive rather than by a swap, and deliberately far
/// narrower than any real launch, because the swap's own `warm_timeout` is derived from
/// `[supervisor] health_deadline_ms` — the launch would fail and close the window long before
/// a realistically-sized park could expire. Everything else is real: the real daemon, the
/// real proxy listener, the real drain, the real request path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_warm_window_that_expires_is_a_503_with_a_retry_after() {
    let h = Harness::start().await;
    let a = h.model("A-9b-Q6_K.gguf");
    let id_a = h.start_and_bind(&h.spec(&a, "model-a", ""), "auto").await;
    let auto = Alias::parse("auto").expect("alias");

    // A first, so the window is opened fresh against an alias that already cannot serve —
    // otherwise the drain itself could eat the whole 500 ms and this would be measuring
    // `no_healthy_backend` rather than the park.
    h.stop(&id_a).await;
    let _window = h
        .state
        .router
        .warm()
        .open(&auto, Duration::from_millis(500), 32);

    let started = Instant::now();
    let res = reqwest::Client::new()
        .post(format!("http://{}/v1/chat/completions", h.proxy))
        .json(&serde_json::json!({
            "model": "auto",
            "messages": [{"role": "user", "content": "ping"}],
            "max_tokens": 4,
        }))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .expect("request");
    let waited = started.elapsed().as_millis();

    let status = res.status().as_u16();
    let retry_after = res
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = res.text().await.unwrap_or_default();

    eprintln!("TIMEOUT (500 ms window, alias unserved): {status} after {waited} ms, Retry-After: {retry_after:?}");
    assert_eq!(status, 503, "{body}");
    let secs: u32 = retry_after
        .as_deref()
        .unwrap_or("")
        .parse()
        .unwrap_or_else(|_| panic!("a timed-out park must carry Retry-After: {body}"));
    assert!(secs >= 1, "Retry-After must be usable: {secs}");
    assert_eq!(openai_error_code(&body), "warm_timeout", "{body}");
    assert!(
        waited >= 400,
        "it must actually have waited out the window rather than failing fast: {waited} ms"
    );
}
