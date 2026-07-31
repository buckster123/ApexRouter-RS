//! The fake, under the **real** supervisor.
//!
//! This is the acceptance test for this crate: `LocalProvisioner::plan` + `up` against a
//! `FakeBuild`, with nothing stubbed on the ApexRouter side. It proves the three things
//! the other agents are about to depend on — the health gate passes, the launch record
//! appears, and what is in that record is what `core::argv` decided.
//!
//! No GPU, no GGUF weights, no `llama-server`. The rig is installed rather than probed, so
//! the test does not care what is on the machine it runs on.

use apexrouter_core::config::Config;
use apexrouter_core::Paths;
use apexrouter_protocol::{
    EndpointSpec, Event, KvType, LocalLlamaSpec, NglPlan, SamplingMode, SplitMode, SplitPlan,
    TriState,
};
use apexrouter_providers::local::{DownMode, LocalProvisioner, Provisioner};
use apexrouter_tests_support::{Control, FakeBuild, GgufSpec};
use std::sync::{Mutex, OnceLock};

/// `Paths::resolve` reads the process environment, which is global to this test binary.
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// A `Paths` rooted at `dir`, with the variable restored before anybody else looks.
fn paths_at(dir: &std::path::Path) -> Paths {
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

/// A supervisor over a temp state root, told about the fake build instead of probing.
struct Harness {
    _state: tempfile::TempDir,
    fake: FakeBuild,
    prov: LocalProvisioner,
    rx: tokio::sync::broadcast::Receiver<Event>,
    model: std::path::PathBuf,
}

impl Harness {
    fn new(port_range: (u16, u16)) -> Harness {
        let state = tempfile::tempdir().expect("tempdir");
        let paths = paths_at(state.path());
        let fake = FakeBuild::new();
        let model = fake.model("Fake-9b-Q6_K.gguf", &GgufSpec::default().sized_mb(1));

        let mut cfg = Config::default();
        cfg.endpoints.port_range = port_range;
        cfg.endpoints.build_roots = vec![fake.root().display().to_string()];
        cfg.supervisor.health_deadline_ms = 15_000;
        cfg.supervisor.health_interval_ms = 50;
        cfg.server.drain_timeout_secs = 1;

        let (tx, rx) = tokio::sync::broadcast::channel(256);
        let prov = LocalProvisioner::new(paths, cfg, tx);
        prov.set_rig(fake.rig(20_992, 19_518));

        Harness {
            _state: state,
            fake,
            prov,
            rx,
            model,
        }
    }

    fn spec(&self, extra: &[&str]) -> EndpointSpec {
        EndpointSpec::LocalLlama(LocalLlamaSpec {
            build: self.fake.build_id(),
            model_path: self.model.display().to_string(),
            mmproj: None,
            alias_flag: "fake-9b".to_owned(),
            host: "127.0.0.1".to_owned(),
            port: None,
            ctx: Some(8192),
            parallel: Some(2),
            kv_type: Some(KvType::Q8_0),
            ngl: NglPlan::All,
            split: SplitPlan {
                devices: vec!["Vulkan0".to_owned()],
                mode: SplitMode::Layer,
                main_gpu: None,
                tensor_split: None,
            },
            mode: SamplingMode::Coding,
            flash_attn: Some(TriState::Auto),
            api_key: None,
            extra_args: extra.iter().map(|s| (*s).to_owned()).collect(),
        })
    }

    fn events(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(e) = self.rx.try_recv() {
            out.push(e);
        }
        out
    }
}

#[tokio::test]
async fn the_supervisor_starts_the_fake_passes_its_health_gate_and_the_argv_is_readable() {
    let mut h = Harness::new((39_500, 39_540));
    let plan = h.prov.plan(&h.spec(&[])).await.expect("plan");
    let planned_ctx = plan.fit.as_ref().map(|f| f.ctx);
    assert!(
        plan.fit.is_some(),
        "the synthetic GGUF must be readable by the fit solver"
    );

    let backend = h.prov.up(plan, None).await.expect("up");
    let port = backend
        .base_url
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .expect("a port in the base url");

    // ---- the health gate passed, which is the whole point --------------------------
    assert!(
        matches!(backend.health, apexrouter_protocol::Health::Ready { .. }),
        "health was {:?}",
        backend.health
    );
    assert!(!backend.base_url.ends_with("/v1"), "stored without /v1");
    assert_eq!(
        backend.models.first().map(|m| m.id.as_str()),
        Some("fake-9b"),
        "the model list came from the fake's /v1/models"
    );
    assert_eq!(
        backend.limits.slots_total,
        Some(2),
        "-np 2 reached argv and came back through /props"
    );

    // ---- the launch record: what actually reached argv ------------------------------
    let rec = h
        .fake
        .records()
        .for_port(port)
        .expect("a launch record for the port the supervisor allocated");
    assert_eq!(
        rec.flag("-m").map(str::to_owned),
        Some(h.model.display().to_string())
    );
    assert_eq!(rec.flag("--port"), Some(port.to_string()).as_deref());
    assert_eq!(rec.flag("--host"), Some("127.0.0.1"));
    assert_eq!(rec.flag("-a"), Some("fake-9b"));
    assert_eq!(rec.flag("-c"), Some("8192"));
    assert_eq!(rec.flag("-np"), Some("2"));
    assert_eq!(rec.flag("-ngl"), Some("999"));
    assert_eq!(rec.flag("-ctk"), Some("q8_0"));
    assert_eq!(rec.flag("-ctv"), Some("q8_0"));
    assert_eq!(rec.flag("-fa"), Some("auto"));
    assert_eq!(rec.flag("-dev"), Some("Vulkan0"));
    assert_eq!(rec.flag("-sm"), Some("layer"));
    // The `coding` sampling preset, all the way through.
    assert_eq!(rec.flag("--temp"), Some("0.6"));
    assert_eq!(rec.flag("--top-k"), Some("20"));
    // The observability flags are bare switches.
    for switch in ["--props", "--metrics", "--slots"] {
        assert!(rec.has(switch), "{switch} missing from {}", rec.argv_line());
    }
    assert!(
        !rec.argv_contains("--jinja"),
        "b9199 has jinja on by default; emitting it would be a no-op"
    );

    // ---- and the environment the argv builder set -----------------------------------
    let bin_dir = h
        .fake
        .server_path()
        .parent()
        .expect("bin dir")
        .display()
        .to_string();
    assert_eq!(
        rec.env_var("LD_LIBRARY_PATH"),
        Some(bin_dir.as_str()),
        "the trailing-colon RUNPATH trap is closed by an explicit LD_LIBRARY_PATH"
    );
    assert_eq!(rec.env_var("GGML_VK_VISIBLE_DEVICES"), Some("0"));
    assert!(
        rec.env_redacted.iter().all(|k| k != "LD_LIBRARY_PATH"),
        "the variables tests assert on are never redacted"
    );

    // ---- the same record, over HTTP, without knowing where the directory is ----------
    let over_http = Control::at(&backend.base_url)
        .record()
        .expect("GET /_apex/record");
    assert_eq!(over_http.argv, rec.argv);

    // Worth knowing, for whoever asserts that "the fit plan reaches argv": `-c` carries
    // the value the *spec* asked for, not `FitPlan::ctx`. They agree here because the fit
    // granted the request; a fit that had to shrink the context would not change argv.
    assert_eq!(rec.flag_as::<u32>("-c"), Some(8192));
    assert_eq!(
        planned_ctx,
        Some(8192),
        "this fixture fits at the asked-for ctx"
    );

    // ---- and it stops --------------------------------------------------------------
    h.prov.down(&backend.id, DownMode::Now).await.expect("down");
    let events = h.events();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::BackendRemoved { .. })),
        "stopping must announce itself"
    );
}

#[tokio::test]
async fn a_slow_start_keeps_the_health_gate_alive_through_its_load_lines() {
    let mut h = Harness::new((39_541, 39_580));
    // 600 ms of "loading model" against a 15 s deadline: the gate must ride it out on
    // 503-loading answers and the log lines the fake prints.
    let plan = h
        .prov
        .plan(&h.spec(&["--apex-behavior", "load_ms=600"]))
        .await
        .expect("plan");
    let backend = h.prov.up(plan, None).await.expect("up");
    assert!(matches!(
        backend.health,
        apexrouter_protocol::Health::Ready { .. }
    ));

    let phases: Vec<String> = h
        .events()
        .iter()
        .filter_map(|e| match e {
            Event::BootProgress { phase, .. } => Some(format!("{phase:?}")),
            _ => None,
        })
        .collect();
    assert!(
        phases.iter().any(|p| p.starts_with("Loading")),
        "the boot machine saw no loading phase: {phases:?}"
    );
    assert!(
        phases.iter().any(|p| p.starts_with("Healthy")),
        "the boot machine never reached healthy: {phases:?}"
    );

    h.prov.down(&backend.id, DownMode::Now).await.expect("down");
}

#[tokio::test]
async fn a_child_that_refuses_to_start_is_reported_with_its_log_tail_and_leaves_no_record() {
    let mut h = Harness::new((39_581, 39_620));
    let plan = h
        .prov
        .plan(&h.spec(&["--apex-behavior", "refuse_start"]))
        .await
        .expect("plan");
    let err = h.prov.up(plan, None).await.expect_err("must fail");
    let rendered = err.to_string();
    assert!(
        rendered.contains("failed to load model"),
        "the log tail must name the failure: {rendered}"
    );

    // The failure path is the stop path: no endpoint record survives.
    let records = apexrouter_core::store::Store::new(paths_at(h._state.path()))
        .list_endpoints()
        .expect("list");
    assert!(records.is_empty(), "a failed launch left {records:?}");
    assert!(
        h.events()
            .iter()
            .any(|e| matches!(e, Event::BackendRemoved { .. })),
        "the route must be cleared"
    );

    // …and the launch record is still there, which is how the operator sees the argv of
    // the thing that would not start.
    let rec = h.fake.records().latest().expect("a launch record");
    assert!(rec.argv_contains("refuse_start"));
}

#[tokio::test]
async fn a_child_that_never_binds_hits_the_health_deadline_and_is_killed() {
    let h = Harness::new((39_621, 39_660));
    // A deliberately short deadline: nothing about this child will ever be progress.
    let spec = h.spec(&["--apex-behavior", "stall"]);
    let mut cfg = Config::default();
    cfg.endpoints.port_range = (39_621, 39_660);
    cfg.endpoints.build_roots = vec![h.fake.root().display().to_string()];
    cfg.supervisor.health_deadline_ms = 1_200;
    cfg.supervisor.health_interval_ms = 50;
    h.prov.set_config(cfg);

    let plan = h.prov.plan(&spec).await.expect("plan");
    let err = h.prov.up(plan, None).await.expect_err("must time out");
    assert!(
        err.to_string().contains("health gate timed out"),
        "unexpected error: {err}"
    );
    let rec = h.fake.records().latest().expect("a launch record");
    assert_eq!(rec.flag("-a"), Some("fake-9b"));
}

#[tokio::test]
async fn a_child_killed_from_outside_is_reconciled_as_down() {
    let h = Harness::new((39_661, 39_700));
    let plan = h.prov.plan(&h.spec(&[])).await.expect("plan");
    let backend = h.prov.up(plan, None).await.expect("up");

    // The fake ends itself, which is what a crashed llama-server looks like from here.
    Control::at(&backend.base_url).exit(1);
    for _ in 0..100 {
        if Control::at(&backend.base_url).record().is_err() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let reconciled = h.prov.reconcile().await.expect("reconcile");
    let seen = reconciled
        .iter()
        .find(|b| b.id == backend.id)
        .expect("the endpoint is still known");
    assert!(
        matches!(seen.health, apexrouter_protocol::Health::Down { .. }),
        "a vanished child must be Down, was {:?}",
        seen.health
    );
}

#[tokio::test]
async fn discovery_finds_the_fake_build_by_globbing_the_tree() {
    // The other path: no `set_rig`, so the supervisor really runs `--help`,
    // `--list-devices` and `--version` against the fake.
    let fake = FakeBuild::new();
    let cfg = apexrouter_core::config::EndpointsCfg {
        build_roots: vec![fake.root().display().to_string()],
        ..apexrouter_core::config::EndpointsCfg::default()
    };
    let cache = tempfile::tempdir().expect("tempdir");

    let builds = apexrouter_core::discover::discover_builds(&cfg, cache.path())
        .await
        .expect("discover");
    let build = builds
        .iter()
        .find(|b| b.id.as_str() == "build-fake")
        .unwrap_or_else(|| {
            panic!(
                "build-fake not among {:?}",
                builds.iter().map(|b| b.id.as_str()).collect::<Vec<_>>()
            )
        });

    assert_eq!(
        build.build_info.as_deref(),
        Some(apexrouter_tests_support::BUILD_INFO)
    );
    assert_eq!(build.devices, vec!["Vulkan0".to_owned()]);
    assert!(
        build.flags.jinja_default_on,
        "b9199 has jinja on by default"
    );
    assert!(build.flags.fa_tristate, "-fa takes on|off|auto");
    assert!(build.flags.has_fit);
    for flag in ["-m", "--port", "-ngl", "-ctk", "--props", "--api-key-file"] {
        assert!(build.flags.has(flag), "--help did not advertise {flag}");
    }
    // The probes must not be mistaken for launches.
    assert!(
        fake.records().all().is_empty(),
        "discovery probes wrote a launch record"
    );
}
