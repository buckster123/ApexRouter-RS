//! **The argv acceptance: what the daemon says the child was started with is what
//! `/proc/<pid>/cmdline` says.**
//!
//! `GET /v1/endpoints/{id}/argv` used to call `supervisor.plan(&rec.spec)`, which re-runs the
//! *planner*: it re-scans the rig, re-solves `fit()` against whatever VRAM is free **now**,
//! and proposes a fresh port. For a **running** child that is a hypothetical second launch,
//! and it drifts the moment anything moves. Measured after a VRAM budget change: the daemon
//! answered **34** tokens where the child actually had **36** — `-c 4096` instead of
//! `-c 32768`, and `-ngl 999` dropped entirely, i.e. it described a CPU-only launch for a
//! fully-offloaded child, with `warnings: []` and no divergence signalled.
//!
//! The daemon-served route is the **normal** one — `apexrouter endpoint argv` asks the daemon
//! whenever one is running, and there is no flag that reaches the other path — so an operator
//! debugging a launch got the plausible lie by default.
//!
//! # The criterion
//!
//! The previous acceptance compared the daemon-served preview against the daemon-less one,
//! which with a daemon up is the same code twice: agreement was tautological and the defect
//! sailed through it. The criterion that means something is the kernel's copy:
//!
//! > **does the preview equal `/proc/<pid>/cmdline` for the running child?**
//!
//! Both routes are held to it here — the daemon's `GET /v1/endpoints/{id}/argv`, and the
//! record-resolved rendering the daemon-less CLI path performs — against a fake
//! `llama-server` started by the **real** supervisor.
//!
//! And the "before" row is produced in the same run rather than remembered: after the rig's
//! free VRAM has moved,
//! [`the_daemon_survives_a_vram_budget_that_moved_under_a_running_child`] asserts that a
//! fresh plan — literally what this route used to return — no longer describes the running
//! child, while the served preview still does.
//!
//! **No GPU, no GGUF, no model load.** Every child is the fake `llama-server`; nothing but
//! `127.0.0.1` is ever contacted.

use apexrouter_core::checks::Registry;
use apexrouter_core::config::{Config, ProviderCfg};
use apexrouter_core::lockfile::DaemonLock;
use apexrouter_core::paths::Paths;
use apexrouter_core::store::Store;
use apexrouter_core::usage::UsageWriter;
use apexrouter_protocol::{
    ArgvPreview, EndpointRecord, EndpointSpec, LocalLlamaSpec, NglPlan, SamplingMode, SplitMode,
    SplitPlan,
};
use apexrouter_providers::local::{LocalProvisioner, ResolvedSpec};
use apexrouter_providers::Provisioner;
use apexrouter_server::AppState;
use apexrouter_tests_support::{FakeBuild, GgufSpec};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// This binary's own slice of the ephemeral range, disjoint from every other test binary's.
const PORTS: (u16, u16) = (39_960, 39_999);

/// Ports handed to one `Harness`.
const PORTS_PER_HARNESS: u16 = 8;

/// How long the fake gets to bind and answer `/health`. Generous: a loaded laptop running
/// four `cargo test` jobs is exactly when a tight deadline invents a flake.
const HEALTH_DEADLINE_MS: u64 = 30_000;

/// Each `Harness` takes the next disjoint window of [`PORTS`].
fn next_port_window() -> (u16, u16) {
    static NEXT: AtomicU16 = AtomicU16::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let lo = PORTS.0 + n * PORTS_PER_HARNESS;
    let hi = lo + PORTS_PER_HARNESS - 1;
    assert!(
        hi <= PORTS.1,
        "the argv-fidelity port range {PORTS:?} is exhausted at harness {n}; widen it"
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

/// A control plane over the real supervisor, and a fake build for it to launch.
struct Harness {
    state: Arc<AppState>,
    control: String,
    fake: FakeBuild,
    paths: Paths,
    http: reqwest::Client,
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
        // A rig instead of probing the box: 20 GB total, 19 GB free, which a 1 MB fake model
        // fits many times over — so the solver sizes the context all the way up to the
        // model's training ceiling and every number in argv is one it decided.
        supervisor.set_rig(fake.rig(20_992, 19_518));

        let lock = DaemonLock::acquire(&paths).expect("daemon lock");
        let state = Arc::new(AppState::new(
            paths.clone(),
            cfg,
            Store::new(paths.clone()),
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

        Harness {
            state,
            control,
            fake,
            paths,
            http: reqwest::Client::new(),
            _dir: dir,
        }
    }

    /// A 9B-shaped fake GGUF: 36 layers and a 32768-token training context, so the solver has
    /// something to size and a smaller budget produces a visibly different answer.
    fn model(&self, name: &str) -> PathBuf {
        self.fake.model(
            name,
            &GgufSpec::default().sized_mb(1).layers(36).ctx_train(32_768),
        )
    }

    /// A draft that decides **nothing**: no port, no ctx, no slot count, no KV type, no device
    /// list, `-ngl` left to whoever knows.
    ///
    /// Every number in the child's command line therefore comes from the solver and lives in
    /// `EndpointRecord::fit` rather than in `EndpointRecord::spec` — which is exactly the gap
    /// a preview rendered from the raw draft falls into. The port is deliberately **not**
    /// pinned: a re-plan leases a fresh one, and `--port` is the cheapest tell that a preview
    /// is describing a different launch.
    fn blank_draft(&self, model: &Path) -> EndpointSpec {
        EndpointSpec::LocalLlama(LocalLlamaSpec {
            build: self.fake.build_id(),
            model_path: model.display().to_string(),
            mmproj: None,
            alias_flag: "fake-9b".to_owned(),
            host: "127.0.0.1".to_owned(),
            port: None,
            ctx: None,
            parallel: None,
            kv_type: None,
            ngl: NglPlan::Auto,
            split: SplitPlan {
                devices: Vec::new(),
                mode: SplitMode::Layer,
                main_gpu: None,
                tensor_split: None,
            },
            mode: SamplingMode::Coding,
            flash_attn: None,
            api_key: None,
            extra_args: Vec::new(),
        })
    }

    /// Start an endpoint under the real supervisor, blocking until its health gate passes.
    async fn start_endpoint(&self, spec: &EndpointSpec) -> EndpointRecord {
        let res = self
            .http
            .post(format!("{}/v1/endpoints", self.control))
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
        res.json().await.expect("EndpointRecord")
    }

    /// `GET /v1/endpoints/{id}/argv` — the route under test.
    async fn served_argv(&self, id: &str) -> ArgvPreview {
        let res = self
            .http
            .get(format!("{}/v1/endpoints/{id}/argv", self.control))
            .send()
            .await
            .expect("GET argv");
        assert_eq!(
            res.status(),
            200,
            "{}",
            res.text().await.unwrap_or_default()
        );
        res.json().await.expect("ArgvPreview")
    }

    /// One endpoint record, straight off the control plane.
    async fn record(&self, id: &str) -> EndpointRecord {
        self.http
            .get(format!("{}/v1/endpoints/{id}", self.control))
            .send()
            .await
            .expect("GET endpoint")
            .json()
            .await
            .expect("EndpointRecord")
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
// the kernel's copy
// ==========================================================================================

/// `/proc/<pid>/cmdline` as a vector, which is argv **as the kernel delivered it**.
///
/// NUL-separated with a trailing NUL, so the final empty field is dropped rather than
/// compared against nothing. This is the one description of a launch that cannot be a
/// re-derivation: it is the array `execve` was called with.
fn cmdline_of(pid: u32) -> Vec<String> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline"))
        .unwrap_or_else(|e| panic!("/proc/{pid}/cmdline is unreadable — is the child alive? {e}"));
    raw.split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

/// The preview as one argv vector, in the shape `/proc/<pid>/cmdline` uses.
fn as_argv(preview: &ArgvPreview) -> Vec<String> {
    let mut out = Vec::with_capacity(preview.args.len() + 1);
    out.push(preview.program.clone());
    out.extend(preview.args.iter().cloned());
    out
}

/// Where two command lines first differ, rendered for a failure message.
fn diff(preview: &[String], child: &[String]) -> String {
    let mut lines = vec![
        format!(
            "  preview ({:>2} tokens): {}",
            preview.len(),
            preview.join(" ")
        ),
        format!("  child   ({:>2} tokens): {}", child.len(), child.join(" ")),
    ];
    for (i, (a, b)) in preview.iter().zip(child.iter()).enumerate() {
        if a != b {
            lines.push(format!("  first difference at [{i}]: {a:?} vs {b:?}"));
            break;
        }
    }
    lines.join("\n")
}

/// The daemon-less route's algorithm, reproduced from the same public calls
/// `cli/src/cmd/endpoint.rs::offline_argv` makes.
///
/// The CLI's own test asserts that route against the fake's launch record; this holds the
/// same computation against `/proc`, so the two halves of the fix — the daemon's and the
/// CLI's — are both measured against the kernel rather than against each other.
async fn offline_argv(h: &Harness, rec: &EndpointRecord) -> ArgvPreview {
    let resolved = ResolvedSpec::from_record(rec);
    let EndpointSpec::LocalLlama(spec) = resolved.spec() else {
        panic!("the fixture starts a local llama endpoint");
    };
    let rig = apexrouter_providers::local::supervisor::scan_rig(
        &h.state.cfg.load().endpoints,
        h.paths.cache(),
    )
    .await
    .expect("scan the fake build tree");
    let build = rig
        .builds
        .iter()
        .find(|b| b.id == spec.build)
        .expect("the fake build is discoverable");
    let key_file = spec
        .api_key
        .as_ref()
        .map(|_| h.paths.endpoints_dir().join(format!("{}.key", rec.id)));
    let mut preview =
        apexrouter_core::argv::plan_local(spec, build, key_file.as_deref()).expect("render argv");
    preview.cwd = h.paths.state().display().to_string();
    preview.warnings.extend(resolved.disagreements(&preview));
    preview
}

// ==========================================================================================
// the acceptance
// ==========================================================================================

/// Both routes describe the process that is running, token for token.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_served_argv_is_the_running_child_s_own_command_line() {
    let h = Harness::start().await;
    let model = h.model("Fake-9b-Q6_K.gguf");
    let started = h.start_endpoint(&h.blank_draft(&model)).await;
    let id = started.id.as_str().to_owned();

    let pid = started
        .proc
        .as_ref()
        .expect("a started endpoint records its process")
        .pid;
    let child = cmdline_of(pid);
    assert!(
        child.len() > 10,
        "the child's command line looks empty: {child:?}"
    );

    // ---- the daemon-served route -------------------------------------------------------
    let served = h.served_argv(&id).await;
    let preview = as_argv(&served);
    assert_eq!(
        preview,
        child,
        "the daemon-served preview is not the command line the child was exec'd with.\n{}",
        diff(&preview, &child)
    );
    assert_eq!(
        served.cwd,
        h.paths.state().display().to_string(),
        "the preview names a cwd the supervisor never used"
    );
    assert!(
        served.warnings.is_empty(),
        "nothing was dropped, so nothing should be warned about: {:?}",
        served.warnings
    );

    // ---- the plan the record reports, checked against the argv just rendered ------------
    let rec = h.record(&id).await;
    let found = ResolvedSpec::from_record(&rec).disagreements(&served);
    assert!(
        found.is_empty(),
        "the served preview does not execute the plan the record reports: {found:?}"
    );

    // ---- and the numbers the solver decided are really in there -------------------------
    // Not decoration: the draft named none of these, so if the preview came from the draft
    // it would have matched a child that had none of them either — which is the shape of
    // the defect one layer down (FIX-4), and the reason this asserts on the *content* as
    // well as on the agreement.
    let fit = rec.fit.as_ref().expect("the record carries a fit");
    let flag = |name: &str| {
        served
            .args
            .iter()
            .position(|a| a == name)
            .and_then(|i| served.args.get(i + 1))
            .cloned()
    };
    assert_eq!(flag("-c"), Some(fit.ctx.to_string()), "{:?}", served.args);
    assert_eq!(
        flag("-np"),
        Some(fit.parallel.to_string()),
        "{:?}",
        served.args
    );
    assert_eq!(
        flag("--port"),
        rec.port.map(|p| p.to_string()),
        "the preview names a port the child is not listening on"
    );

    // ---- the daemon-less route, held to the same criterion ------------------------------
    let offline = offline_argv(&h, &rec).await;
    let offline_argv = as_argv(&offline);
    assert_eq!(
        offline_argv,
        child,
        "the daemon-less preview is not the command line the child was exec'd with.\n{}",
        diff(&offline_argv, &child)
    );
    assert!(offline.warnings.is_empty(), "{:?}", offline.warnings);

    eprintln!(
        "ARGV FIDELITY: {} tokens served, {} on /proc/{pid}/cmdline, identical",
        preview.len(),
        child.len()
    );
}

/// **The measured defect, reproduced and then closed in one run.**
///
/// The child is started against a rig with 19 GB free, then the rig is replaced with one that
/// has almost nothing left — the situation that produced "34 tokens where the child had 36".
/// A fresh plan is what this route used to return; the assertion is that it now differs from
/// the running child *and* that the served preview does not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_daemon_survives_a_vram_budget_that_moved_under_a_running_child() {
    let h = Harness::start().await;
    let model = h.model("Fake-9b-Q6_K.gguf");
    let started = h.start_endpoint(&h.blank_draft(&model)).await;
    let id = started.id.as_str().to_owned();
    let pid = started.proc.as_ref().expect("proc facts").pid;
    let child = cmdline_of(pid);

    // The budget moves under the running child: same box, same build, 512 MB free. Nothing
    // about the child changes — it is already loaded — but every number a *fresh* solve
    // produces does.
    h.state.supervisor.set_rig(h.fake.rig(20_992, 512));

    // ---- BEFORE: what this route used to answer ----------------------------------------
    let rec = h.record(&id).await;
    let replanned = h
        .state
        .supervisor
        .plan(&rec.spec)
        .await
        .expect("a fresh plan still succeeds; it is simply about a different launch");
    let hypothetical = as_argv(&replanned.argv);

    // ---- AFTER: what it answers now -----------------------------------------------------
    let served = h.served_argv(&id).await;
    let preview = as_argv(&served);

    eprintln!(
        "VRAM MOVED (19518 MB free -> 512 MB):\n  child      ({:>2}): {}\n  re-planned ({:>2}): {}\n  served     ({:>2}): {}",
        child.len(),
        child.join(" "),
        hypothetical.len(),
        hypothetical.join(" "),
        preview.len(),
        preview.join(" "),
    );

    assert_ne!(
        hypothetical, child,
        "the re-plan happened to agree with the child, so this run proves nothing about the \
         defect — make the budget change bite harder (fewer MB free, a bigger ctx_train)"
    );
    assert_eq!(
        preview,
        child,
        "the daemon is describing a launch that never happened.\n{}",
        diff(&preview, &child)
    );
    assert!(
        served.warnings.is_empty(),
        "the preview matches the child, so there is nothing to warn about: {:?}",
        served.warnings
    );
    assert!(
        ResolvedSpec::from_record(&rec)
            .disagreements(&served)
            .is_empty(),
        "the served preview does not execute the plan the record reports"
    );
}

/// A record whose build has left the machine is a `409` that names the remedy, not a preview
/// invented from whatever build is left.
///
/// The supervisor's own fallback — "no exact build? use whatever `choose_build` prefers" — is
/// right for a *launch* and wrong for a *description*: a preview rendered against a different
/// binary's `FlagSupport` silently emits a different set of flags, which is the same class of
/// plausible lie this route was fixed for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_endpoint_whose_build_vanished_refuses_rather_than_inventing_one() {
    let h = Harness::start().await;
    let model = h.model("Fake-9b-Q6_K.gguf");
    let started = h.start_endpoint(&h.blank_draft(&model)).await;

    // The build is gone from the rig — an uninstall, a moved checkout, a pruned build tree.
    let mut rig = h.fake.rig(20_992, 19_518);
    rig.builds.clear();
    h.state.supervisor.set_rig(rig);

    let res = h
        .http
        .get(format!(
            "{}/v1/endpoints/{}/argv",
            h.control,
            started.id.as_str()
        ))
        .send()
        .await
        .expect("GET argv");
    assert_eq!(res.status(), 409);
    let body: apexrouter_protocol::ErrorEnvelope = res.json().await.expect("ErrorEnvelope");
    assert!(
        body.error.message.contains("apexrouter rig"),
        "a refusal must name what fixes it: {}",
        body.error.message
    );
}
