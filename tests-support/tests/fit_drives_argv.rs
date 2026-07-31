//! FIX-4 acceptance: **the solver's plan is what gets exec'd.**
//!
//! MK1 ACCEPTANCE found the fit solver — the one function that replaced 54 hand-solved
//! recipe strings — computing a plan, storing it in the record, printing it, and then not
//! being used to build argv. The evidence was `fit.parallel = 1` in the stored record, **no
//! `-np` in argv at all**, and llama.cpp logging `n_parallel is set to auto, using
//! n_parallel = 4`. ApexRouter was reporting a plan it did not execute.
//!
//! These tests spawn the fake `llama-server` under the **real** supervisor and read the
//! launch record the child wrote — the argv as the kernel delivered it. No GPU, no weights,
//! no real model: what is under test is whether a decision reaches `execve`, and a real
//! model would only make that slower and flakier to observe.
//!
//! Three things are asserted, in the order the defect report names them:
//!
//! 1. the solver's `-c`, `-np`, `-ngl`, `-dev` and KV type are in the argv the child got;
//! 2. an explicit operator value beats the solver **and** is recorded as an override;
//! 3. the printed plan and the executed argv cannot disagree — which is the real invariant,
//!    and is asserted token for token as well as through `ResolvedSpec::disagreements`.

use apexrouter_core::config::Config;
use apexrouter_core::store::Store;
use apexrouter_core::Paths;
use apexrouter_protocol::{
    ArgvPreview, EndpointSpec, KvType, LocalLlamaSpec, NglPlan, SamplingMode, SplitMode, SplitPlan,
    TriState,
};
use apexrouter_providers::local::{DownMode, LocalProvisioner, Provisioner, ResolvedSpec};
use apexrouter_tests_support::{FakeBuild, GgufSpec, LaunchRecord};
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
    state: tempfile::TempDir,
    fake: FakeBuild,
    prov: LocalProvisioner,
    model: std::path::PathBuf,
}

impl Harness {
    fn new(port_range: (u16, u16)) -> Harness {
        let state = tempfile::tempdir().expect("tempdir");
        let paths = paths_at(state.path());
        let fake = FakeBuild::new();
        // 36 layers, 32768 train context, GQA 32/8 — a 9B-shaped model that fits this rig
        // several times over, so the auto-sizer runs all the way to the ctx_train ceiling
        // and the difference between "the solver decided" and "the draft said" is visible.
        let model = fake.model("Fake-9b-Q6_K.gguf", &GgufSpec::default().sized_mb(1));

        let mut cfg = Config::default();
        cfg.endpoints.port_range = port_range;
        cfg.endpoints.build_roots = vec![fake.root().display().to_string()];
        cfg.supervisor.health_deadline_ms = 15_000;
        cfg.supervisor.health_interval_ms = 50;

        let (tx, _rx) = tokio::sync::broadcast::channel(256);
        let prov = LocalProvisioner::new(paths, cfg, tx);
        prov.set_rig(fake.rig(20_992, 19_518));

        Harness {
            state,
            fake,
            prov,
            model,
        }
    }

    /// A draft that decides **nothing**: no ctx, no slot count, no KV type, no device list
    /// and `-ngl` left to whoever knows. Every number in the argv therefore has to come from
    /// the solver, which is precisely what the defect proved it did not.
    fn blank_draft(&self) -> LocalLlamaSpec {
        LocalLlamaSpec {
            build: self.fake.build_id(),
            model_path: self.model.display().to_string(),
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
            flash_attn: Some(TriState::Auto),
            api_key: None,
            extra_args: Vec::new(),
        }
    }

    fn records(&self) -> Vec<apexrouter_protocol::EndpointRecord> {
        Store::new(paths_at(self.state.path()))
            .list_endpoints()
            .expect("list endpoints")
    }
}

/// The recorded argv as an [`ArgvPreview`], so the same invariant checker can be pointed at
/// what the kernel delivered as at what the operator was shown.
fn as_preview(rec: &LaunchRecord) -> ArgvPreview {
    ArgvPreview {
        program: rec.argv0.clone(),
        args: rec.argv.clone(),
        env: rec
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        cwd: rec.cwd.clone(),
        warnings: Vec::new(),
    }
}

/// The port out of `http://127.0.0.1:39501`.
fn port_of(base_url: &str) -> u16 {
    base_url
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .expect("a port in the base url")
}

// ----------------------------------------------------------------------------------------
// 1. the solver's numbers reach argv
// ----------------------------------------------------------------------------------------

#[tokio::test]
async fn every_number_the_solver_decided_is_in_the_argv_the_child_was_exec_with() {
    let h = Harness::new((39_800, 39_819));
    let spec = EndpointSpec::LocalLlama(h.blank_draft());

    let plan = h.prov.plan(&spec).await.expect("plan");
    let fit = plan.fit.clone().expect("the synthetic GGUF is readable");

    // What the solver decided, none of which the draft asked for.
    assert!(
        fit.ctx > 8_192,
        "the auto-sizer should have grown the context to the train ceiling, got {}",
        fit.ctx
    );
    assert_eq!(fit.parallel, 1, "the -np the defect never emitted");
    assert_eq!(fit.kv_type, KvType::Q8_0, "the house default, not f16");
    assert_eq!(fit.ngl, NglPlan::All);
    assert_eq!(
        fit.split.devices,
        vec!["Vulkan0".to_owned()],
        "the budget's spendable device, not an empty list"
    );

    let backend = h.prov.up(plan.clone(), None).await.expect("up");
    let port = port_of(&backend.base_url);
    let rec = h
        .fake
        .records()
        .for_port(port)
        .expect("a launch record for the port the supervisor allocated");

    // ---- THE assertion: the plan is in the argv --------------------------------------
    assert_eq!(
        rec.flag_as::<u32>("-c"),
        Some(fit.ctx),
        "the solver's context never reached argv: {}",
        rec.argv_line()
    );
    assert_eq!(
        rec.flag_as::<u32>("-np"),
        Some(fit.parallel),
        "`-np` absent is the MK1 defect verbatim — llama.cpp then picks n_parallel itself: {}",
        rec.argv_line()
    );
    assert_eq!(
        rec.flag("-ngl"),
        Some("999"),
        "NglPlan::All is -ngl 999: {}",
        rec.argv_line()
    );
    assert_eq!(rec.flag("-ctk"), Some(fit.kv_type.as_flag()));
    assert_eq!(rec.flag("-ctv"), Some(fit.kv_type.as_flag()));
    assert_eq!(
        rec.flag("-dev"),
        Some(fit.split.devices.join(",").as_str()),
        "the device the budget was scoped to must be named, not left to enumeration order"
    );
    assert_eq!(rec.flag("-sm"), Some("layer"));
    // …and the env mask follows the same device list.
    assert_eq!(rec.env_var("GGML_VK_VISIBLE_DEVICES"), Some("0"));

    // ---- corroborated from the other end: the server reports the slots it was given ----
    assert_eq!(
        backend.limits.slots_total,
        Some(fit.parallel),
        "/props disagrees with the plan, which is the symptom the operator sees"
    );

    // ---- and the record on disk explains the argv --------------------------------------
    let stored = h.records();
    let stored = stored.first().expect("one endpoint record");
    let stored_fit = stored.fit.as_ref().expect("the record keeps its plan");
    assert_eq!(stored_fit.ctx, fit.ctx);
    assert_eq!(stored_fit.parallel, fit.parallel);
    assert!(
        stored_fit
            .why
            .iter()
            .any(|w| w.starts_with("launch: argv carries")),
        "the record must say which flags the plan became: {:?}",
        stored_fit.why
    );
    // Resolving the record back gives the argv its child really got — the fix for any
    // surface that rebuilds an argv offline from `$STATE/endpoints/<id>.json`.
    let reresolved = ResolvedSpec::from_record(stored);
    assert!(
        reresolved.disagreements(&as_preview(&rec)).is_empty(),
        "the record and the process disagree: {:?}",
        reresolved.disagreements(&as_preview(&rec))
    );

    h.prov.down(&backend.id, DownMode::Now).await.expect("down");
}

// ----------------------------------------------------------------------------------------
// 2. an explicit override beats the solver, and is recorded as one
// ----------------------------------------------------------------------------------------

#[tokio::test]
async fn an_explicit_operator_value_beats_the_solver_and_is_recorded_as_an_override() {
    let h = Harness::new((39_820, 39_839));

    // First, what the solver would have chosen on its own.
    let planned = h
        .prov
        .plan(&EndpointSpec::LocalLlama(h.blank_draft()))
        .await
        .expect("plan");
    let solver_ctx = planned.fit.as_ref().expect("fit").ctx;
    assert!(solver_ctx > 8_192, "the fixture must leave room to differ");

    // Now the same launch with the operator's hand on three of the dials — including one,
    // `-ngl`, that the solver decides *without* being told what was asked for, so it is a
    // real contradiction rather than a confirmation.
    let mut draft = h.blank_draft();
    draft.ctx = Some(8_192);
    draft.parallel = Some(3);
    draft.kv_type = Some(KvType::F16);
    draft.ngl = NglPlan::Layers(7);
    draft.split.devices = vec!["Vulkan0".to_owned()];

    let plan = h
        .prov
        .plan(&EndpointSpec::LocalLlama(draft))
        .await
        .expect("plan");
    let fit = plan.fit.clone().expect("fit");

    // The solver still reports its own answer for the field it owns…
    assert_eq!(
        fit.ngl,
        NglPlan::All,
        "the plan must keep saying what the solver computed"
    );
    // …and the operator's contradiction is in front of them before they confirm.
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.contains("-ngl 7 overrides the planned 999")),
        "the override was not surfaced: {:?}",
        plan.warnings
    );
    // …and it is recorded, not merely printed.
    assert!(
        fit.why
            .iter()
            .any(|w| w.contains("-ngl 7 pinned by the operator, overriding the planned 999")),
        "the override never reached the plan: {:?}",
        fit.why
    );
    assert!(
        fit.why.iter().any(|w| w.contains("-c 8192 pinned")),
        "a pinned context is provenance too: {:?}",
        fit.why
    );

    let backend = h.prov.up(plan, None).await.expect("up");
    let port = port_of(&backend.base_url);
    let rec = h.fake.records().for_port(port).expect("launch record");

    // The override wins where it was asked for…
    assert_eq!(rec.flag_as::<u32>("-c"), Some(8_192));
    assert_ne!(rec.flag_as::<u32>("-c"), Some(solver_ctx));
    assert_eq!(rec.flag_as::<u32>("-np"), Some(3));
    assert_eq!(rec.flag("-ctk"), Some("f16"));
    assert_eq!(rec.flag("-ctv"), Some("f16"));
    assert_eq!(rec.flag("-ngl"), Some("7"), "{}", rec.argv_line());
    // …and the record on disk carries the same story.
    let stored = h.records();
    let stored_fit = stored
        .first()
        .and_then(|r| r.fit.as_ref())
        .expect("the record keeps its plan");
    assert!(stored_fit
        .why
        .iter()
        .any(|w| w.contains("overriding the planned 999")));

    h.prov.down(&backend.id, DownMode::Now).await.expect("down");
}

// ----------------------------------------------------------------------------------------
// 3. the printed plan and the executed argv cannot disagree
// ----------------------------------------------------------------------------------------

#[tokio::test]
async fn the_argv_the_operator_was_shown_is_the_argv_the_child_received() {
    let h = Harness::new((39_840, 39_859));
    let plan = h
        .prov
        .plan(&EndpointSpec::LocalLlama(h.blank_draft()))
        .await
        .expect("plan");

    let printed = plan.argv.clone();
    let proposed_port = plan.port;
    let fit = plan.fit.clone().expect("fit");

    let backend = h.prov.up(plan, None).await.expect("up");
    let port = port_of(&backend.base_url);
    assert_eq!(
        port, proposed_port,
        "this range is the test's alone, so the planned port must be the leased one"
    );

    let rec = h.fake.records().for_port(port).expect("launch record");

    // Token for token. Not "the flags we remembered to assert on" — the whole vector, which
    // is the only version of this assertion a future flag cannot slip past.
    assert_eq!(
        printed.args,
        rec.argv,
        "the preview and the launch differ:\n  printed:  {:?}\n  executed: {}",
        printed.args,
        rec.argv_line()
    );
    assert_eq!(printed.program, rec.argv0);
    assert_eq!(printed.cwd, rec.cwd);

    // And the machine-checkable form of the same statement, against the plan rather than
    // against the other rendering: neither side may silently drop or contradict a decision.
    let resolved = ResolvedSpec::resolve(
        &EndpointSpec::LocalLlama(h.blank_draft()),
        port,
        Some(fit.clone()),
    );
    for (what, argv) in [("printed", printed), ("executed", as_preview(&rec))] {
        let found = resolved.disagreements(&argv);
        assert!(found.is_empty(), "the {what} argv disagrees: {found:?}");
    }

    h.prov.down(&backend.id, DownMode::Now).await.expect("down");
}

// ----------------------------------------------------------------------------------------
// 4. the honest case: a plan we could not compute invents nothing
// ----------------------------------------------------------------------------------------

#[tokio::test]
async fn a_model_the_solver_cannot_measure_is_started_without_invented_numbers() {
    let h = Harness::new((39_860, 39_879));
    // Not a GGUF. `fit_for` warns and returns None rather than refusing the launch.
    let junk = h.fake.root().join("models").join("Not-A-Gguf.gguf");
    std::fs::write(&junk, b"this is not a gguf header").expect("write");

    let mut draft = h.blank_draft();
    draft.model_path = junk.display().to_string();
    let plan = h
        .prov
        .plan(&EndpointSpec::LocalLlama(draft))
        .await
        .expect("plan");
    assert!(plan.fit.is_none(), "there is nothing to solve");
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.contains("without a VRAM estimate")),
        "{:?}",
        plan.warnings
    );

    let backend = h.prov.up(plan, None).await.expect("up");
    let port = port_of(&backend.base_url);
    let rec = h.fake.records().for_port(port).expect("launch record");
    // No plan means no fabricated flags: the draft asked for none of these, so argv carries
    // none of them and llama.cpp's own defaults apply — visibly, not behind a plan.
    for flag in ["-c", "-np", "-ctk", "-ctv", "-ngl", "-dev"] {
        assert!(
            rec.flag(flag).is_none(),
            "{flag} was invented with no plan behind it: {}",
            rec.argv_line()
        );
    }

    h.prov.down(&backend.id, DownMode::Now).await.expect("down");
}
