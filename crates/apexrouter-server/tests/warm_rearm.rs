//! **A park must not expire while the thing it waits for is still working.**
//!
//! `tests/warm_queue.rs` proved that a sequential swap parks instead of dropping requests.
//! This proves the number that decides how long it parks, because as shipped it was a
//! stopwatch and the swap it was timing was not:
//!
//! | shape | `warm_timeout` | swap | result |
//! |---|---|---|---|
//! | sequential, B loads for 12 s | 3000 ms | 12,038 ms | 4 parked requests `503` at 2977 ms, then **74,550** x `no_healthy_backend` over the remaining 9 s |
//!
//! That is precisely the outage `ARCHITECTURE.md` §4.7 exists to prevent, merely delayed —
//! and delayed past the point where a client has already retried. The cause is an arithmetic
//! mismatch that cannot be fixed by choosing a bigger number: `[supervisor]
//! health_deadline_ms` is not how long a launch may take, it is how long it may spend making
//! **no progress**, so the health gate's real budget is unbounded while a load is progressing
//! and any fixed park is eventually shorter than it.
//!
//! The ruling (`docs/CHARTER.md`, 2026-07-31) is to re-arm the window on the same liveness
//! signal the gate uses, keeping a bound for the case where progress genuinely stops. What is
//! measured here, in one binary and with no GPU:
//!
//! * [`the_twelve_second_swap_that_produced_the_storm_now_costs_zero_5xx`] — the **after**
//!   number, from the same 12 s swap against a 3000 ms `warm_timeout`;
//! * [`the_same_twelve_second_outage_without_the_rearm_is_the_storm`] — the **before**
//!   number, produced in this run rather than remembered, from a window that is not re-armed;
//! * [`the_warm_queue_bound_is_whatever_the_caller_passes_not_a_constant`] — `warm_queue_max`
//!   observed at two different values (D6: the wiring is real, the config key is not).
//!
//! **No GPU, no GGUF, no model load.** The "12 second model load" is the fake
//! `llama-server`'s `load_ms=` sleep, every socket is `127.0.0.1`, and no number here is a
//! benchmark.

use apexrouter_core::checks::Registry;
use apexrouter_core::config::{Config, ProviderCfg};
use apexrouter_core::lockfile::DaemonLock;
use apexrouter_core::paths::Paths;
use apexrouter_core::store::Store;
use apexrouter_core::usage::UsageWriter;
use apexrouter_protocol::{
    Alias, BackendId, EndpointRecord, EndpointSpec, LocalLlamaSpec, NglPlan, SamplingMode,
    SplitMode, SplitPlan, SwapMode, SwapReport,
};
use apexrouter_providers::local::LocalProvisioner;
use apexrouter_server::AppState;
use apexrouter_tests_support::{FakeBuild, GgufSpec};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// This binary's own slice of the ephemeral range, disjoint from every other test binary's.
const PORTS: (u16, u16) = (39_800, 39_879);

/// Ports handed to one `Harness`: A, B and a restart of A, with room to spare.
const PORTS_PER_HARNESS: u16 = 8;

/// `[supervisor] health_deadline_ms`. Deliberately **shorter than the load below**, which is
/// the whole point: the gate survives it by resetting on progress, and so must the park.
const HEALTH_DEADLINE_MS: u64 = 2_000;

/// `[server] drain_timeout_secs`.
const DRAIN_TIMEOUT_SECS: u64 = 1;

/// `[router] queue_timeout_ms`, kept low so it does not dominate `warm_timeout`.
const QUEUE_TIMEOUT_MS: u64 = 500;

/// `warm_timeout`, as `api/routes.rs` derives it: `health_deadline + drain`, floored at
/// `queue_timeout`. Restated here so this test fails loudly if that arithmetic moves.
const WARM_TIMEOUT: Duration =
    Duration::from_millis(HEALTH_DEADLINE_MS + DRAIN_TIMEOUT_SECS * 1_000);

/// How long the replacement pretends to load its weights: **four times** `warm_timeout`.
///
/// The measured incident was a 12,038 ms swap against a 3000 ms window. In production this
/// is a 7 GB GGUF on an iGPU and the ratio is far worse.
const SLOW_LOAD_MS: u64 = 12_000;

/// Gap between one worker's requests.
const LOOP_GAP: Duration = Duration::from_millis(5);

/// Concurrent workers in the request loop.
const WORKERS: usize = 8;

/// Each `Harness` takes the next disjoint window of [`PORTS`].
fn next_port_window() -> (u16, u16) {
    static NEXT: AtomicU16 = AtomicU16::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let lo = PORTS.0 + n * PORTS_PER_HARNESS;
    let hi = lo + PORTS_PER_HARNESS - 1;
    assert!(
        hi <= PORTS.1,
        "the warm-rearm port range {PORTS:?} is exhausted at harness {n}; widen it"
    );
    (lo, hi)
}

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

/// The hermetic config: no discovery roots but the fake's, `[providers.together]` pointed at
/// a **closed** loopback port, and the three numbers `warm_timeout` is made of pinned.
fn test_config(fake: &FakeBuild, ports: (u16, u16)) -> Config {
    let mut cfg = Config::default();
    cfg.compat.mirror_usage_log = false;
    cfg.compat.read_legacy_state = false;
    cfg.router.log_usage = false;
    cfg.router.queue_timeout_ms = QUEUE_TIMEOUT_MS;
    cfg.endpoints.build_roots = vec![fake.root().display().to_string()];
    cfg.endpoints.model_roots = vec![];
    cfg.endpoints.port_range = ports;
    cfg.supervisor.health_deadline_ms = HEALTH_DEADLINE_MS;
    cfg.supervisor.health_interval_ms = 50;
    cfg.server.drain_timeout_secs = DRAIN_TIMEOUT_SECS;
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
        // always fits. Sequential is therefore an explicit `mode` override here, which is
        // the honest way to test it without pretending the laptop is full.
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
            .timeout(Duration::from_secs(120))
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
            .timeout(Duration::from_secs(180))
            .send()
            .await
            .expect("swap");
        let status = res.status().as_u16();
        (status, res.text().await.unwrap_or_default())
    }

    /// `POST /v1/endpoints/{id}/stop` — the same outage a sequential swap's drain produces,
    /// with **no** warm window unless the test opens one itself.
    async fn stop(&self, id: &BackendId) {
        let res = self
            .http
            .post(format!("{}/v1/endpoints/{id}/stop", self.control))
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .expect("stop");
        assert!(res.status().is_success(), "stop failed: {}", res.status());
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
                        .timeout(Duration::from_secs(120))
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
/// nothing else.
fn openai_error_code(body: &str) -> String {
    let json: serde_json::Value = serde_json::from_str(body)
        .unwrap_or_else(|e| panic!("the proxy must answer OpenAI-shaped JSON ({e}): {body}"));
    json["error"]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("no error.code in {body}"))
        .to_owned()
}

// ==========================================================================================
// AFTER — the 12 second swap, against a 3 second window
// ==========================================================================================

/// The incident, re-run: `warm_timeout` 3000 ms, replacement loads for 12,000 ms, zero `5xx`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_twelve_second_swap_that_produced_the_storm_now_costs_zero_5xx() {
    let h = Harness::start().await;
    let auto = Alias::parse("auto").expect("alias");
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
    let rearms = h.state.router.warm().rearms(&auto);

    assert_eq!(status, 200, "{body}");
    let report: SwapReport = serde_json::from_str(&body).expect("SwapReport");
    eprintln!(
        "AFTER  (warm_timeout {} ms, B loads for {SLOW_LOAD_MS} ms): the swap took {swap_ms} ms, \
         the window was re-armed {rearms} times: {counts}  {report:?}",
        WARM_TIMEOUT.as_millis()
    );

    assert_eq!(report.mode, SwapMode::Sequential, "the mode under test");
    assert!(
        swap_ms >= u128::from(SLOW_LOAD_MS),
        "the replacement has to have actually been slow: {swap_ms} ms"
    );
    assert!(
        swap_ms > 2 * WARM_TIMEOUT.as_millis(),
        "the swap has to have out-run the park's own budget, or the fix is not under test: \
         {swap_ms} ms against a {} ms window",
        WARM_TIMEOUT.as_millis()
    );

    // ---- the number the incident report is about ----------------------------------------
    assert_eq!(
        counts.five_xx, 0,
        "a swap whose replacement is still loading must not drop a request: {counts}"
    );
    assert_eq!(counts.transport, 0, "{counts}");
    assert_eq!(counts.other, 0, "{counts}");
    assert!(counts.ok > 50, "the loop has to have run: {counts}");

    // ---- and it survived by patience, not by luck ---------------------------------------
    // Without this, a `warm_timeout` that silently became enormous would pass the test above
    // while re-introducing the unbounded wait the ruling explicitly kept a bound against.
    assert!(
        rearms >= 10,
        "the window was not re-armed, so it either never expired or never parked anybody: \
         {rearms} re-arms across a {swap_ms} ms swap"
    );
    assert!(
        report.parked > 0,
        "requests crossed the gap without parking, which means something served them that \
         should have been stopped: {report:?}"
    );

    // …and the swap really happened underneath all that.
    assert_ne!(report.to, id_a, "the alias really moved");
    assert!(
        !h.state.router.warm().any_open(),
        "the window outlived the swap that opened it"
    );
}

// ==========================================================================================
// BEFORE — the same outage, with a window that is not re-armed
// ==========================================================================================

/// The pre-fix number, produced in this run rather than remembered from an older build.
///
/// The window is opened directly on the primitive with the same `warm_timeout` the swap
/// derives, and then simply not re-armed — which is exactly what the swap did before. The
/// alias is down for the same 12 seconds. Everything else is real: the real daemon, the real
/// proxy listener, the real request path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_same_twelve_second_outage_without_the_rearm_is_the_storm() {
    let h = Harness::start().await;
    let auto = Alias::parse("auto").expect("alias");
    let a = h.model("A-9b-Q6_K.gguf");
    let id_a = h.start_and_bind(&h.spec(&a, "model-a", ""), "auto").await;

    let across = Hammer::start(h.proxy, WORKERS);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let window = h.state.router.warm().open(
        &auto,
        WARM_TIMEOUT,
        apexrouter_router::DEFAULT_WARM_QUEUE_MAX,
    );
    h.stop(&id_a).await;
    tokio::time::sleep(Duration::from_millis(SLOW_LOAD_MS)).await;
    let parked = window.close();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let counts = across.finish().await;

    eprintln!(
        "BEFORE (the same {SLOW_LOAD_MS} ms outage, a {} ms window, no re-arm): {counts}, \
         peak parked {parked}",
        WARM_TIMEOUT.as_millis()
    );
    assert!(
        counts.five_xx > 0,
        "if this is zero the comparison above is measuring nothing: {counts}"
    );
    assert!(
        counts.ok > 20,
        "the loop ran before the outage too: {counts}"
    );
    assert_eq!(
        h.state.router.warm().rearms(&auto),
        0,
        "nothing re-armed this window; that is the point of the row"
    );
}

// ==========================================================================================
// D6 — the bound is an argument, not a constant
// ==========================================================================================

/// `warm_queue_max` observed at two different values against the same outage.
///
/// **The `[router] warm_queue_max` config key still does not exist.** `RouterCfg` lives in
/// `core/src/config.rs`, which belongs to another unit, so what is proved here is the half
/// this unit owns: nothing reads a constant to decide the bound. `WarmRegistry::open` takes
/// it as an argument and `api/routes.rs::warm_queue_max` is the single line that computes it,
/// so the day the key lands the behaviour below is what a change to it produces.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_warm_queue_bound_is_whatever_the_caller_passes_not_a_constant() {
    let h = Harness::start().await;
    let auto = Alias::parse("auto").expect("alias");
    let a = h.model("A-9b-Q6_K.gguf");
    let id_a = h.start_and_bind(&h.spec(&a, "model-a", ""), "auto").await;
    // The alias cannot serve, which is the only state in which a request parks at all.
    h.stop(&id_a).await;

    let clients = 10u32;
    let mut refused_at = Vec::new();
    for max in [4u32, 8u32] {
        // A short window: every parked request is meant to time out, because what is being
        // counted is how many got *in*, not what happened to them afterwards.
        let window = h
            .state
            .router
            .warm()
            .open(&auto, Duration::from_millis(1_500), max);

        let url = format!("http://{}/v1/chat/completions", h.proxy);
        let fired: Vec<_> = (0..clients)
            .map(|_| {
                let url = url.clone();
                tokio::spawn(async move {
                    let http = reqwest::Client::builder()
                        .timeout(Duration::from_secs(30))
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
                    (status, res.text().await.unwrap_or_default())
                })
            })
            .collect();

        let mut full = 0u32;
        let mut timed_out = 0u32;
        for c in fired {
            let (status, body) = c.await.expect("client task");
            assert_eq!(status, 503, "{body}");
            match openai_error_code(&body).as_str() {
                "warm_queue_full" => full += 1,
                "warm_timeout" => timed_out += 1,
                other => panic!("unexpected refusal {other}: {body}"),
            }
        }
        drop(window);

        eprintln!(
            "BOUND {max}: {clients} clients -> {timed_out} parked (and timed out), \
             {full} refused with warm_queue_full"
        );
        assert_eq!(
            timed_out, max,
            "exactly the bound may park: {timed_out} parked against a bound of {max}"
        );
        assert_eq!(full, clients - max, "everything past the bound is refused");
        refused_at.push(full);
    }

    assert_ne!(
        refused_at[0], refused_at[1],
        "the two bounds produced the same behaviour, so the bound is not being honoured"
    );
}
