//! OWNER: unit P-01 (providers/src/local/**). Do not edit outside that unit.
//!
//! The acceptance suite for the local supervisor.
//!
//! Nothing here needs llama.cpp, a GPU or 7 GB of weights: a stand-in `llama-server` with
//! the same argv shape and the same `/health`, `/v1/models` and `/props` endpoints exercises
//! every path the real one does, including the ones that only happen when a start *fails*.
//! The one test that does use `Carnice-9b-Q6_K.gguf` on `build-vulkan` is `#[ignore]`d and
//! named in the notes, because it takes minutes and 7 GB of page cache.

use super::*;
use apexrouter_protocol::{
    Alias, BuildId, DeviceBudget, FlagSupport, Gpu, GpuBackend, KvType, LlamaBuild, NglPlan,
    SamplingMode, SplitMode, SplitPlan,
};
use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::sync::OnceLock;

// ---------------------------------------------------------------------------------------
// The stand-in llama-server
// ---------------------------------------------------------------------------------------

/// Same argv shape, same endpoints, no weights.
///
/// `--fake-exit-early` and `--fake-never-healthy` ride in through `extra_args`, which the
/// argv builder passes through verbatim, so the two failure paths are reachable without a
/// second binary.
const FAKE_SERVER: &str = r#"#!/usr/bin/env python3
import json, os, sys, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ARGV = sys.argv[1:]

def flag(name, default=None):
    if name in ARGV:
        i = ARGV.index(name)
        if i + 1 < len(ARGV):
            return ARGV[i + 1]
    return default

port = int(flag('--port', '0'))
host = flag('--host', '127.0.0.1')
model = flag('-m', '')
alias = flag('-a', 'fake-model')

print('LD_LIBRARY_PATH=' + os.environ.get('LD_LIBRARY_PATH', '<unset>'), flush=True)
print('cwd=' + os.getcwd(), flush=True)
print('llama_model_loader: loaded meta data from ' + model, flush=True)

if '--fake-exit-early' in ARGV:
    print("common_init_from_params: failed to load model '" + model + "'", flush=True)
    sys.exit(3)

if '--fake-never-healthy' in ARGV:
    while True:
        time.sleep(3600)

class Handler(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.0'

    def _send(self, code, body):
        raw = json.dumps(body).encode()
        self.send_response(code)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):
        p = self.path.split('?')[0]
        if p == '/health':
            self._send(200, {'status': 'ok'})
        elif p == '/v1/models':
            self._send(200, {'object': 'list', 'data': [{'id': alias, 'object': 'model'}]})
        elif p == '/props':
            self._send(200, {'total_slots': 2, 'model_path': model,
                             'default_generation_settings': {'n_ctx': 4096}})
        else:
            self._send(404, {'error': 'no such endpoint'})

    def log_message(self, fmt, *args):
        pass

srv = ThreadingHTTPServer((host, port), Handler)
print('load_tensors: offloading 36 repeating layers to GPU', flush=True)
print('main: server is listening on http://%s:%d - starting the main loop' % (host, port), flush=True)
srv.serve_forever()
"#;

// ---------------------------------------------------------------------------------------
// A synthetic GGUF header
// ---------------------------------------------------------------------------------------

/// Write a GGUF header the fit solver can read, followed by `pad` bytes of pretend weights.
fn write_gguf(path: &Path, n_layer: u32, n_ctx_train: u32, pad: usize) {
    fn put_str(b: &mut Vec<u8>, s: &str) {
        b.extend_from_slice(&(s.len() as u64).to_le_bytes());
        b.extend_from_slice(s.as_bytes());
    }
    fn put_u32_kv(b: &mut Vec<u8>, key: &str, v: u32) {
        put_str(b, key);
        b.extend_from_slice(&4u32.to_le_bytes()); // T_UINT32
        b.extend_from_slice(&v.to_le_bytes());
    }

    let mut b = Vec::new();
    b.extend_from_slice(b"GGUF");
    b.extend_from_slice(&3u32.to_le_bytes()); // version
    b.extend_from_slice(&0u64.to_le_bytes()); // tensor count
    b.extend_from_slice(&6u64.to_le_bytes()); // kv count

    put_str(&mut b, "general.architecture");
    b.extend_from_slice(&8u32.to_le_bytes()); // T_STRING
    put_str(&mut b, "llama");

    put_u32_kv(&mut b, "llama.block_count", n_layer);
    put_u32_kv(&mut b, "llama.context_length", n_ctx_train);
    put_u32_kv(&mut b, "llama.embedding_length", 4096);
    put_u32_kv(&mut b, "llama.attention.head_count", 32);
    put_u32_kv(&mut b, "llama.attention.head_count_kv", 8);

    b.resize(b.len() + pad, 0);
    std::fs::write(path, b).expect("write gguf");
}

// ---------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------

/// `Paths::resolve` reads the process environment, which is global. One lock, and the
/// variable is restored before it is released.
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// A `Paths` rooted at `dir`.
fn paths_at(dir: &Path) -> Paths {
    let guard = match env_lock().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let previous = std::env::var_os("APEXROUTER_HOME");
    std::env::set_var("APEXROUTER_HOME", dir);
    let paths = Paths::resolve();
    match previous {
        Some(v) => std::env::set_var("APEXROUTER_HOME", v),
        None => std::env::remove_var("APEXROUTER_HOME"),
    }
    drop(guard);
    let paths = paths.expect("resolve paths");
    paths.ensure_layout().expect("layout");
    paths
}

struct Harness {
    _dir: tempfile::TempDir,
    paths: Paths,
    prov: Arc<LocalProvisioner>,
    rx: broadcast::Receiver<Event>,
    build: LlamaBuild,
    model: PathBuf,
}

impl Harness {
    /// A supervisor over a temp state root, a stand-in `llama-server` and a synthetic GGUF.
    fn new(port_range: (u16, u16), tune: impl FnOnce(&mut Config)) -> Harness {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths_at(&dir.path().join("state"));

        // A build directory that looks like `~/llama.cpp/build-fake/bin/llama-server`.
        let bin_dir = dir.path().join("build-fake").join("bin");
        std::fs::create_dir_all(&bin_dir).expect("build dir");
        let server = bin_dir.join("llama-server");
        std::fs::write(&server, FAKE_SERVER).expect("write server");
        std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o755))
            .expect("chmod server");

        let model = dir.path().join("models").join("Fake-9b-Q6_K.gguf");
        std::fs::create_dir_all(model.parent().unwrap_or(dir.path())).expect("model dir");
        write_gguf(&model, 36, 32_768, 4096);

        let mut flags = BTreeSet::new();
        for f in ["-m", "--host", "--port", "-a"] {
            flags.insert(f.to_owned());
        }
        let build = LlamaBuild {
            id: BuildId::parse("build-fake").expect("build id"),
            server_path: server.display().to_string(),
            label: "build-fake".to_owned(),
            build_info: Some("b9199 (fake)".to_owned()),
            backends: vec![GpuBackend::Vulkan],
            devices: vec!["Vulkan0".to_owned()],
            flags: FlagSupport {
                flags,
                jinja_default_on: true,
                fa_tristate: true,
                has_fit: true,
                has_router_mode: false,
                help_lines: 635,
            },
            probed_at_unix: 0,
        };

        let mut cfg = Config::default();
        cfg.endpoints.port_range = port_range;
        cfg.supervisor.health_deadline_ms = 15_000;
        cfg.supervisor.health_interval_ms = 100;
        cfg.server.drain_timeout_secs = 1;
        tune(&mut cfg);

        let (tx, rx) = broadcast::channel(512);
        let prov = Arc::new(LocalProvisioner::new(paths.clone(), cfg, tx));
        prov.set_rig(rig_with(&build, 20_000, 19_000));

        Harness {
            _dir: dir,
            paths,
            prov,
            rx,
            build,
            model,
        }
    }

    fn spec(&self, extra: &[&str]) -> EndpointSpec {
        EndpointSpec::LocalLlama(LocalLlamaSpec {
            build: self.build.id.clone(),
            model_path: self.model.display().to_string(),
            mmproj: None,
            alias_flag: "fake-9b".to_owned(),
            host: "127.0.0.1".to_owned(),
            port: None,
            ctx: Some(4096),
            parallel: None,
            kv_type: Some(KvType::Q8_0),
            ngl: NglPlan::All,
            split: SplitPlan {
                devices: vec!["Vulkan0".to_owned()],
                mode: SplitMode::Layer,
                main_gpu: None,
                tensor_split: None,
            },
            mode: SamplingMode::Raw,
            flash_attn: None,
            api_key: None,
            extra_args: extra.iter().map(|s| (*s).to_owned()).collect(),
        })
    }

    /// Every event broadcast so far.
    fn drain_events(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(e) = self.rx.try_recv() {
            out.push(e);
        }
        out
    }

    fn records(&self) -> Vec<EndpointRecord> {
        Store::new(self.paths.clone())
            .list_endpoints()
            .expect("list endpoints")
    }
}

/// A rig with one GPU, sized to taste.
fn rig_with(build: &LlamaBuild, total_mb: u64, free_mb: u64) -> RigSnapshot {
    RigSnapshot {
        gpus: vec![Gpu {
            device: "Vulkan0".to_owned(),
            index: 0,
            name: "Fake Radeon".to_owned(),
            backend: GpuBackend::Vulkan,
            vram_total_mb: total_mb,
            vram_free_mb: free_mb,
            pci_bus_id: None,
            driver: Some("radv".to_owned()),
            is_software: false,
            seen_by_builds: vec![build.id.clone()],
            held_by: Vec::new(),
            reserved_mb: 0,
        }],
        builds: vec![build.clone()],
        ram_total_mb: 22_000,
        ram_free_mb: 10_000,
        swap_total_mb: 8_000,
        swap_used_mb: 0,
        cpu_threads: 12,
        scanned_at_unix: 0,
    }
}

/// How many file descriptors this process has open.
fn open_fds() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .map(Iterator::count)
        .unwrap_or(0)
}

/// `/proc/<pid>/stat` fields after the last `)`, which is the only correct way to parse it.
fn stat_fields(pid: u32) -> Option<Vec<String>> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = raw.rsplit_once(')')?.1;
    Some(tail.split_whitespace().map(str::to_owned).collect())
}

/// Is this pid gone, or at least not a zombie we left behind?
fn gone_or_reaped(pid: u32) -> bool {
    match stat_fields(pid) {
        None => true,
        // Field 0 after the comm is the state character.
        Some(fields) => fields.first().map(String::as_str) != Some("Z"),
    }
}

// ---------------------------------------------------------------------------------------
// The headline acceptance
// ---------------------------------------------------------------------------------------

/// **The highest-value acceptance in the plan**, against the stand-in server.
///
/// Ten starts and ten stops must leave: no process still listening on the port, no zombie
/// we forgot to `waitpid`, no growth in our own fd table, and no record on disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ten_starts_and_stops_leave_no_orphan_no_zombie_no_leaked_fd_and_no_stale_state_file() {
    let mut h = Harness::new((39_200, 39_209), |_| {});
    let spec = h.spec(&[]);
    let mut fds_after_first = 0usize;

    for round in 0..10u32 {
        let plan = h.prov.plan(&spec).await.expect("plan");
        let backend = h
            .prov
            .up(plan, None)
            .await
            .unwrap_or_else(|e| panic!("round {round}: {e}"));
        assert!(matches!(backend.health, Health::Ready { .. }));

        let record = h
            .records()
            .into_iter()
            .find(|r| r.id == backend.id)
            .expect("record on disk while running");
        let pid = record.proc.as_ref().expect("proc facts").pid;
        let port = record.port.expect("port");

        // Restarting the same spec must reuse its id rather than accumulating records.
        assert_eq!(
            h.records().len(),
            1,
            "round {round}: one endpoint, not many"
        );

        h.prov
            .down(&backend.id, DownMode::Forget)
            .await
            .expect("down");

        // No orphan: nothing is listening on the port any more.
        assert!(
            proc::port_free(port),
            "round {round}: something is still holding {port}"
        );
        // No zombie: we reaped what we spawned.
        assert!(gone_or_reaped(pid), "round {round}: pid {pid} is a zombie");
        // No stale state file.
        assert!(
            h.records().is_empty(),
            "round {round}: a record survived `forget`"
        );

        if round == 0 {
            // Measured after the first round so lazily-initialised runtime fds are already
            // accounted for; from here the count must not grow with the loop.
            fds_after_first = open_fds();
        }
    }

    let grew_by = open_fds().saturating_sub(fds_after_first);
    assert!(
        grew_by <= 2,
        "fd table grew by {grew_by} over nine further rounds (from {fds_after_first})"
    );
}

// ---------------------------------------------------------------------------------------
// LD_LIBRARY_PATH and the cwd
// ---------------------------------------------------------------------------------------

/// The `build-vulkan` trailing-colon RUNPATH trap: the child's `LD_LIBRARY_PATH` is always
/// `dirname(binary)`, and the cwd is `$STATE` — deliberately *not* the build directory. A
/// child that started from the "wrong" cwd is proof that the cwd was never load-bearing.
#[tokio::test]
async fn ld_library_path_is_set_so_the_child_starts_from_a_cwd_that_is_not_its_build_dir() {
    let mut h = Harness::new((39_210, 39_219), |_| {});
    let spec = h.spec(&[]);

    let plan = h.prov.plan(&spec).await.expect("plan");
    let bin_dir = Path::new(&h.build.server_path)
        .parent()
        .expect("bin dir")
        .display()
        .to_string();
    assert_eq!(
        plan.argv
            .env
            .iter()
            .find(|(k, _)| k == "LD_LIBRARY_PATH")
            .map(|(_, v)| v.as_str()),
        Some(bin_dir.as_str()),
        "LD_LIBRARY_PATH must always be dirname(binary)"
    );
    assert_eq!(
        plan.argv.cwd,
        h.paths.state().display().to_string(),
        "the cwd is $STATE, never the build dir"
    );

    let backend = h.prov.up(plan, None).await.expect("up");
    let log = h.prov.logs(&backend.id, 50).await.expect("logs");
    assert!(
        log.iter()
            .any(|l| l == &format!("LD_LIBRARY_PATH={bin_dir}")),
        "the child did not receive LD_LIBRARY_PATH: {log:?}"
    );
    assert!(
        log.iter()
            .any(|l| l == &format!("cwd={}", h.paths.state().display())),
        "the child ran from the wrong cwd and still started: {log:?}"
    );

    h.prov
        .down(&backend.id, DownMode::Forget)
        .await
        .expect("down");
}

// ---------------------------------------------------------------------------------------
// Port reservation
// ---------------------------------------------------------------------------------------

/// Two launches racing for a two-port pool must not both take the first one. The lease is
/// held from the bind probe until the health gate settles, which is the whole window.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_concurrent_launches_cannot_both_take_the_same_port() {
    let h = Harness::new((39_220, 39_229), |_| {});

    // Two different models, so they are two different endpoints with two different locks —
    // the reservation set, not the per-endpoint mutex, is what has to separate them.
    let second_model = h.model.with_file_name("Other-9b-Q6_K.gguf");
    write_gguf(&second_model, 36, 32_768, 4096);
    let mut spec_b = match h.spec(&[]) {
        EndpointSpec::LocalLlama(s) => s,
        _ => unreachable!(),
    };
    spec_b.model_path = second_model.display().to_string();
    spec_b.alias_flag = "other-9b".to_owned();

    let a = h.spec(&[]);
    let b = EndpointSpec::LocalLlama(spec_b);
    let plan_a = h.prov.plan(&a).await.expect("plan a");
    let plan_b = h.prov.plan(&b).await.expect("plan b");

    let (pa, pb) = (Arc::clone(&h.prov), Arc::clone(&h.prov));
    let (ra, rb) = tokio::join!(pa.up(plan_a, None), pb.up(plan_b, None));
    let (ba, bb) = (ra.expect("up a"), rb.expect("up b"));

    let ports: Vec<u16> = h.records().into_iter().filter_map(|r| r.port).collect();
    assert_eq!(ports.len(), 2);
    assert_ne!(ports[0], ports[1], "both launches took the same port");

    for id in [ba.id, bb.id] {
        h.prov.down(&id, DownMode::Forget).await.expect("down");
    }
}

/// A port the operator pinned by hand, already held by one of ours, names its holder.
#[tokio::test]
async fn a_pinned_port_that_is_already_ours_names_the_holder() {
    let h = Harness::new((39_230, 39_239), |_| {});
    let first = h.spec(&[]);
    let plan = h.prov.plan(&first).await.expect("plan");
    let port = plan.port;
    let running = h.prov.up(plan, None).await.expect("up");

    let second_model = h.model.with_file_name("Second-9b-Q6_K.gguf");
    write_gguf(&second_model, 36, 32_768, 4096);
    let mut spec = match h.spec(&[]) {
        EndpointSpec::LocalLlama(s) => s,
        _ => unreachable!(),
    };
    spec.model_path = second_model.display().to_string();
    spec.port = Some(port);
    let clash = EndpointSpec::LocalLlama(spec);

    let plan = h.prov.plan(&clash).await.expect("plan the clash");
    let err = h.prov.up(plan, None).await.expect_err("must refuse");
    match err {
        Error::PortInUse {
            port: p,
            held_by: Some(who),
        } => {
            assert_eq!(p, port);
            assert_eq!(who, running.id.as_str());
        }
        other => panic!("expected PortInUse naming the holder, got {other}"),
    }

    h.prov
        .down(&running.id, DownMode::Forget)
        .await
        .expect("down");
}

// ---------------------------------------------------------------------------------------
// The health gate
// ---------------------------------------------------------------------------------------

/// The deadline is real wall clock, and **the failure path is the stop path**.
///
/// The stand-in is told to behave the way a `llama-server` handed a model it cannot load
/// does at its worst: it prints its opening lines and then never binds, never answers and
/// never exits. On expiry the child must be killed, the record removed, `Failed` broadcast
/// with the log tail, and the route cleared.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_model_that_never_loads_is_killed_unrecorded_and_reported_with_its_log_tail() {
    let mut h = Harness::new((39_240, 39_249), |cfg| {
        cfg.supervisor.health_deadline_ms = 1_200;
        cfg.supervisor.health_interval_ms = 100;
    });
    let spec = h.spec(&["--fake-never-healthy"]);
    let plan = h.prov.plan(&spec).await.expect("plan");
    let port = plan.port;

    let err = h.prov.up(plan, None).await.expect_err("must not come up");
    let rendered = err.to_string();
    assert!(
        rendered.contains("health gate timed out"),
        "expected a health-gate timeout, got {rendered}"
    );
    assert!(
        rendered.contains("llama_model_loader"),
        "the log tail must travel with the error: {rendered}"
    );

    // The child is dead and the port is free: the failure path really is the stop path.
    assert!(proc::port_free(port));
    // The record is gone.
    assert!(h.records().is_empty(), "a failed launch left a record");

    let events = h.drain_events();
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::BootProgress {
                phase: BootPhase::Failed { .. },
                ..
            }
        )),
        "no Failed phase was broadcast: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::BackendRemoved { .. })),
        "the route was never cleared: {events:?}"
    );
}

/// A child that exits before answering is reported as such, with its exit code and its log
/// — not as a timeout the operator has to wait out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_that_exits_before_answering_is_reported_immediately() {
    let mut h = Harness::new((39_250, 39_259), |cfg| {
        cfg.supervisor.health_deadline_ms = 60_000;
        cfg.supervisor.health_interval_ms = 100;
    });
    let spec = h.spec(&["--fake-exit-early"]);
    let plan = h.prov.plan(&spec).await.expect("plan");

    let started = Instant::now();
    let err = h.prov.up(plan, None).await.expect_err("must not come up");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "an early exit must not wait out the whole deadline"
    );
    let rendered = err.to_string();
    assert!(
        rendered.contains("exited early"),
        "expected ExitedEarly, got {rendered}"
    );
    assert!(
        rendered.contains("failed to load model"),
        "the log tail must travel with the error: {rendered}"
    );
    assert!(h.records().is_empty());
    assert!(h
        .drain_events()
        .iter()
        .any(|e| matches!(e, Event::BackendRemoved { .. })));
}

// ---------------------------------------------------------------------------------------
// setsid, adoption, reconciliation
// ---------------------------------------------------------------------------------------

/// `setsid` is what lets a child outlive the manager: it leads its own session and process
/// group, so it is not in the terminal's foreground group and is reparented to pid 1 the
/// moment we exit. (`PPid` is still ours *while the test is running* — that is the point of
/// the design, not a gap in it.)
#[tokio::test]
async fn a_spawned_child_leads_its_own_session_so_it_survives_the_manager() {
    let h = Harness::new((39_260, 39_269), |_| {});
    let spec = h.spec(&[]);
    let plan = h.prov.plan(&spec).await.expect("plan");
    let backend = h.prov.up(plan, None).await.expect("up");

    let record = h.records().into_iter().next().expect("record");
    let pid = record.proc.as_ref().expect("facts").pid;
    let fields = stat_fields(pid).expect("stat");
    // After the comm: state, ppid, pgrp, session, ...
    let pgrp = fields.get(2).and_then(|v| v.parse::<u32>().ok());
    let session = fields.get(3).and_then(|v| v.parse::<u32>().ok());
    assert_eq!(pgrp, Some(pid), "the child must lead its own process group");
    assert_eq!(session, Some(pid), "the child must lead its own session");

    h.prov
        .down(&backend.id, DownMode::Forget)
        .await
        .expect("down");
}

/// A restarted daemon re-adopts its children from the record alone, without signalling
/// anything and without restarting a model that took 90 seconds to load.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconcile_re_adopts_across_a_simulated_daemon_restart() {
    let h = Harness::new((39_270, 39_279), |_| {});
    let spec = h.spec(&[]);
    let plan = h.prov.plan(&spec).await.expect("plan");
    let backend = h.prov.up(plan, None).await.expect("up");
    let pid = h.records()[0].proc.as_ref().expect("facts").pid;

    // The daemon dies and comes back: a brand-new supervisor over the same state root.
    let (tx, _rx) = broadcast::channel(64);
    let restarted = LocalProvisioner::new(h.paths.clone(), Config::default(), tx);
    let adopted = restarted.reconcile().await.expect("reconcile");

    assert_eq!(adopted.len(), 1);
    assert_eq!(adopted[0].id, backend.id);
    assert_eq!(adopted[0].provenance, Provenance::Adopted);
    assert!(
        matches!(
            proc::liveness(h.records()[0].proc.as_ref().expect("facts")),
            Liveness::Alive
        ),
        "re-adoption must not have disturbed the child"
    );
    assert_eq!(
        stat_fields(pid).and_then(|f| f.first().cloned()).as_deref(),
        Some("S"),
        "the adopted child is still sleeping happily"
    );

    h.prov
        .down(&backend.id, DownMode::Forget)
        .await
        .expect("down");
}

/// An identity that no longer matches is `Foreign`/`Ambiguous`, is surfaced as a `Down`
/// backend, and is **never signalled** — the process it names keeps running.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mismatched_identity_is_reported_and_never_signalled() {
    let h = Harness::new((39_280, 39_289), |_| {});
    let spec = h.spec(&[]);
    let plan = h.prov.plan(&spec).await.expect("plan");
    let backend = h.prov.up(plan, None).await.expect("up");

    // Somebody edited the record, or the child re-exec'd: same live pid, different argv.
    let store = Store::new(h.paths.clone());
    let mut record = store.list_endpoints().expect("list")[0].clone();
    let pid = record.proc.as_ref().expect("facts").pid;
    if let Some(facts) = record.proc.as_mut() {
        facts.cmdline_sha256 = "0".repeat(64);
    }
    store.put_endpoint(&record).expect("put");

    let adoption = adopt::adopt(&record);
    assert!(
        !matches!(adoption, Adoption::Adopted(_)),
        "a changed argv must not adopt: {adoption:?}"
    );
    assert!(adopt::signallable(&adoption).is_none());

    let (tx, _rx) = broadcast::channel(64);
    let restarted = LocalProvisioner::new(h.paths.clone(), Config::default(), tx);
    let seen = restarted.reconcile().await.expect("reconcile");
    assert_eq!(seen.len(), 1);
    assert!(matches!(seen[0].health, Health::Down { .. }));

    // Nothing was killed.
    assert_eq!(
        stat_fields(pid).and_then(|f| f.first().cloned()).as_deref(),
        Some("S"),
        "a foreign identity must never be signalled"
    );

    // `down` must also refuse to signal it — and still tidy the record away.
    restarted
        .down(&backend.id, DownMode::Forget)
        .await
        .expect("down");
    assert!(
        stat_fields(pid).is_some(),
        "down() signalled a process it does not own"
    );

    // Clean up the process we deliberately orphaned for this test.
    let facts = record.proc.take().expect("facts");
    let _ = proc::signal_verified(
        &proc::identify(pid, &proc::cmdline(pid).unwrap_or_default(), &facts.exe)
            .expect("re-identify"),
        proc::Signal::Kill,
    );
    supervisor::reap(pid);
}

/// A record whose process is gone tidies itself away when it was supposed to be stopped,
/// and becomes a visible failure when it was supposed to be running.
#[tokio::test]
async fn a_vanished_process_is_a_failure_when_running_and_tidy_up_when_stopped() {
    let h = Harness::new((39_290, 39_299), |_| {});
    let store = Store::new(h.paths.clone());

    let mut record = EndpointRecord {
        id: BackendId::parse("ghost").expect("id"),
        spec: h.spec(&[]),
        desired: DesiredState::Running,
        // A pid that cannot be alive: identity fails at the boot id.
        proc: Some(ProcFacts {
            pid: 2,
            start_time_ticks: 1,
            boot_id: "00000000-0000-0000-0000-000000000000".to_owned(),
            exe: "/nonexistent".to_owned(),
            cmdline_sha256: "0".repeat(64),
        }),
        port: Some(39_299),
        log_path: None,
        started_at_unix: 0,
        fit: None,
        adopted: false,
        alias_bindings: vec![Alias::parse("ghost").expect("alias")],
    };
    store.put_endpoint(&record).expect("put");

    let running = h.prov.reconcile().await.expect("reconcile");
    assert_eq!(running.len(), 1);
    assert!(matches!(running[0].health, Health::Down { .. }));
    assert_eq!(h.records().len(), 1, "a running ghost stays visible");

    record.desired = DesiredState::Stopped;
    store.put_endpoint(&record).expect("put stopped");
    let stopped = h.prov.reconcile().await.expect("reconcile");
    assert!(stopped.is_empty());
    assert!(h.records().is_empty(), "a stopped ghost is tidied away");
}

// ---------------------------------------------------------------------------------------
// VRAM admission control
// ---------------------------------------------------------------------------------------

/// A plan the solver says will not fit is refused — unless the operator forces it.
#[tokio::test]
async fn insufficient_vram_fires_unless_forced() {
    let h = Harness::new((39_300, 39_309), |_| {});
    let spec = h.spec(&[]);
    let mut plan = h.prov.plan(&spec).await.expect("plan");

    // The solver's own verdict, replaced with the one this test is about. Everything else —
    // the argv, the binary, the port — stays exactly as `plan` produced it.
    let fit = plan.fit.as_mut().expect("a synthetic gguf must solve");
    fit.weights_mb = 20_000;
    fit.kv_mb = 1_000;
    fit.compute_mb = 500;
    fit.headroom_mb = -2_500;
    fit.verdict = FitVerdict::WontFit { short_by_mb: 2_500 };

    let err = h
        .prov
        .up(plan.clone(), None)
        .await
        .expect_err("must refuse");
    match err {
        Error::InsufficientVram { need_mb, free_mb } => {
            assert_eq!(need_mb, 21_500);
            assert_eq!(free_mb, 19_000);
        }
        other => panic!("expected InsufficientVram, got {other}"),
    }
    assert!(h.records().is_empty(), "a refusal must not leave a record");

    let backend = h
        .prov
        .up_forced(plan, None, true)
        .await
        .expect("--force overrides the fit and nothing else");
    h.prov
        .down(&backend.id, DownMode::Forget)
        .await
        .expect("down");
}

/// The budget is **live**: a running endpoint's reservation is subtracted before the next
/// plan is solved, which is what makes the refusal above fire before the second launch OOMs
/// the first.
#[tokio::test]
async fn a_running_endpoints_reservation_shrinks_the_next_plans_budget() {
    let h = Harness::new((39_310, 39_319), |_| {});
    let spec = h.spec(&[]);
    let first = h.prov.plan(&spec).await.expect("plan");
    let headroom_alone = match first.fit.as_ref().expect("fit").verdict.clone() {
        FitVerdict::Fits { headroom_mb } | FitVerdict::Tight { headroom_mb } => headroom_mb,
        other => panic!("a 4 KiB model on a 19 GiB budget must fit, got {other:?}"),
    };

    // A neighbour that already holds 12 GiB.
    let store = Store::new(h.paths.clone());
    let mut neighbour = EndpointRecord {
        id: BackendId::parse("neighbour").expect("id"),
        spec: h.spec(&[]),
        desired: DesiredState::Running,
        proc: None,
        port: Some(39_319),
        log_path: None,
        started_at_unix: 0,
        fit: first.fit.clone(),
        adopted: false,
        alias_bindings: Vec::new(),
    };
    if let Some(fit) = neighbour.fit.as_mut() {
        fit.weights_mb = 12_000;
        fit.kv_mb = 0;
        fit.compute_mb = 0;
        fit.per_device_mb = vec![("Vulkan0".to_owned(), 12_000)];
    }
    store.put_endpoint(&neighbour).expect("put");

    let second = h.prov.plan(&spec).await.expect("plan again");
    let headroom_beside = match second.fit.as_ref().expect("fit").verdict.clone() {
        FitVerdict::Fits { headroom_mb } | FitVerdict::Tight { headroom_mb } => headroom_mb,
        other => panic!("expected a fit, got {other:?}"),
    };
    assert!(
        headroom_beside + 11_000 < headroom_alone,
        "the neighbour's 12 GiB was not subtracted: {headroom_alone} -> {headroom_beside}"
    );
}

// ---------------------------------------------------------------------------------------
// Refusals that happen before anything is spawned
// ---------------------------------------------------------------------------------------

/// Weights that are not there are refused at plan time, by name.
#[tokio::test]
async fn a_missing_model_is_refused_before_anything_is_spawned() {
    let h = Harness::new((39_320, 39_329), |_| {});
    let mut spec = match h.spec(&[]) {
        EndpointSpec::LocalLlama(s) => s,
        _ => unreachable!(),
    };
    spec.model_path = "/nonexistent/Definitely-Not-Here.gguf".to_owned();

    let err = h
        .prov
        .plan(&EndpointSpec::LocalLlama(spec))
        .await
        .expect_err("must refuse");
    let rendered = err.to_string();
    assert!(rendered.contains("Definitely-Not-Here.gguf"), "{rendered}");
    assert!(h.records().is_empty());
}

/// A build that was never discovered is a **visible** fallback warning, never a silent
/// substitution of one backend for another.
#[tokio::test]
async fn an_undiscovered_build_falls_back_visibly() {
    let h = Harness::new((39_330, 39_339), |_| {});
    let mut spec = match h.spec(&[]) {
        EndpointSpec::LocalLlama(s) => s,
        _ => unreachable!(),
    };
    spec.build = BuildId::parse("build-rocm").expect("id");

    let plan = h
        .prov
        .plan(&EndpointSpec::LocalLlama(spec))
        .await
        .expect("plan");
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.contains("build-rocm") && w.contains("substitution")),
        "the fallback was not surfaced: {:?}",
        plan.warnings
    );
}

/// A spec another provisioner owns is refused rather than half-handled.
#[tokio::test]
async fn a_non_local_spec_is_refused() {
    let h = Harness::new((39_340, 39_349), |_| {});
    let node = EndpointSpec::Node(apexrouter_protocol::NodeSpec {
        base_url: "http://10.0.0.5:8080".to_owned(),
        credential: CredentialSource::None,
        label: "lan node".to_owned(),
        declared_models: Vec::new(),
        protocol: Protocol::OpenAi,
    });
    assert!(matches!(
        h.prov.plan(&node).await,
        Err(Error::Invalid { .. })
    ));
}

// ---------------------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------------------

#[test]
fn a_base_url_never_carries_a_trailing_v1() {
    let spec = EndpointSpec::LocalLlama(LocalLlamaSpec {
        build: BuildId::parse("b").expect("id"),
        model_path: "/m.gguf".to_owned(),
        mmproj: None,
        alias_flag: "m".to_owned(),
        host: "0.0.0.0".to_owned(),
        port: Some(8100),
        ctx: None,
        parallel: None,
        kv_type: None,
        ngl: NglPlan::Auto,
        split: SplitPlan::default(),
        mode: SamplingMode::Raw,
        flash_attn: None,
        api_key: None,
        extra_args: Vec::new(),
    });
    // 0.0.0.0 is a bind address, not a destination.
    assert_eq!(base_url(&spec, 8100), "http://127.0.0.1:8100");
    assert!(!base_url(&spec, 8100).ends_with("/v1"));
}

#[test]
fn two_specs_differing_only_by_port_are_the_same_endpoint() {
    let a = EndpointSpec::LocalLlama(LocalLlamaSpec {
        build: BuildId::parse("b").expect("id"),
        model_path: "/m.gguf".to_owned(),
        mmproj: None,
        alias_flag: "m".to_owned(),
        host: "127.0.0.1".to_owned(),
        port: Some(8100),
        ctx: None,
        parallel: None,
        kv_type: None,
        ngl: NglPlan::Auto,
        split: SplitPlan::default(),
        mode: SamplingMode::Raw,
        flash_attn: None,
        api_key: None,
        extra_args: Vec::new(),
    });
    let b = with_port(&a, 8199);
    assert!(same_endpoint(&a, &b));

    let mut different = match a.clone() {
        EndpointSpec::LocalLlama(s) => s,
        _ => unreachable!(),
    };
    different.ctx = Some(65_536);
    assert!(!same_endpoint(&a, &EndpointSpec::LocalLlama(different)));
}

#[test]
fn device_tokens_become_gpu_tags() {
    let spec = EndpointSpec::LocalLlama(LocalLlamaSpec {
        build: BuildId::parse("b").expect("id"),
        model_path: "/m.gguf".to_owned(),
        mmproj: None,
        alias_flag: "m".to_owned(),
        host: "127.0.0.1".to_owned(),
        port: None,
        ctx: None,
        parallel: None,
        kv_type: None,
        ngl: NglPlan::Auto,
        split: SplitPlan {
            devices: vec!["Vulkan0".to_owned(), "Vulkan1".to_owned()],
            mode: SplitMode::Layer,
            main_gpu: None,
            tensor_split: None,
        },
        mode: SamplingMode::Raw,
        flash_attn: None,
        api_key: None,
        extra_args: Vec::new(),
    });
    assert_eq!(tags_for(&spec), vec!["local", "gpu:vulkan"]);
}

#[test]
fn a_launch_error_keeps_its_shape_when_it_becomes_a_core_error() {
    let e: Error = LaunchError::PortInUse {
        port: 8100,
        held_by: Some(BackendId::parse("local-carnice").expect("id")),
    }
    .into();
    assert!(matches!(
        e,
        Error::PortInUse {
            port: 8100,
            held_by: Some(_)
        }
    ));

    let e: Error = LaunchError::HealthTimeout {
        log_tail: vec!["load_tensors: ...".to_owned()],
    }
    .into();
    assert!(e.to_string().contains("load_tensors"));
}

#[test]
fn a_device_budget_is_a_plain_value_we_can_reason_about() {
    // Guards the assumption `fit_for` leans on: a budget is per-device and additive.
    let b = apexrouter_protocol::VramBudget {
        devices: vec![DeviceBudget {
            device: "Vulkan0".to_owned(),
            free_mb: 19_000,
            reserved_mb: 12_000,
        }],
        margin_mb: 512,
        host_ram_free_mb: 10_000,
        backend: Some(GpuBackend::Vulkan),
        notes: vec![],
    };
    assert_eq!(b.total_usable_mb(), 19_000 - 12_000 - 512);
}

// ---------------------------------------------------------------------------------------
// The real machine
// ---------------------------------------------------------------------------------------

/// The literal acceptance from the build plan, on the real box.
///
/// Ignored by default: it loads 7 GB of weights ten times and takes minutes. Run it with
/// `cargo test -p apexrouter-providers --lib -- --ignored real_carnice` on a machine that
/// has `~/llama.cpp/build-vulkan/bin/llama-server` and
/// `~/models/carnice-9b/Carnice-9b-Q6_K.gguf`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs llama.cpp build-vulkan and 7 GB of real weights"]
async fn real_carnice_starts_and_stops_ten_times_on_build_vulkan() {
    let home = std::env::var("HOME").expect("HOME");
    let model = PathBuf::from(&home).join("models/carnice-9b/Carnice-9b-Q6_K.gguf");
    let server = PathBuf::from(&home).join("llama.cpp/build-vulkan/bin/llama-server");
    if !model.exists() || !server.exists() {
        eprintln!("skipping: {model:?} or {server:?} is absent");
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let paths = paths_at(&dir.path().join("state"));
    let mut cfg = Config::default();
    cfg.endpoints.port_range = (39_400, 39_409);
    cfg.endpoints.build_roots = vec![format!("{home}/llama.cpp")];
    cfg.supervisor.health_deadline_ms = 600_000;
    cfg.supervisor.health_interval_ms = 2_000;
    let (tx, _rx) = broadcast::channel(512);
    let prov = LocalProvisioner::new(paths.clone(), cfg, tx);

    let spec = EndpointSpec::LocalLlama(LocalLlamaSpec {
        build: BuildId::parse("build-vulkan").expect("id"),
        model_path: model.display().to_string(),
        mmproj: None,
        alias_flag: "carnice-9b".to_owned(),
        host: "127.0.0.1".to_owned(),
        port: None,
        ctx: Some(4096),
        parallel: Some(1),
        kv_type: Some(KvType::Q8_0),
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
        extra_args: Vec::new(),
    });

    let store = Store::new(paths.clone());
    let mut fds_after_first = 0usize;
    for round in 0..10u32 {
        let plan = prov.plan(&spec).await.expect("plan");
        let backend = prov
            .up(plan, None)
            .await
            .unwrap_or_else(|e| panic!("round {round}: {e}"));
        let record = store.list_endpoints().expect("list")[0].clone();
        let pid = record.proc.as_ref().expect("facts").pid;
        let port = record.port.expect("port");

        prov.down(&backend.id, DownMode::Forget)
            .await
            .expect("down");
        assert!(proc::port_free(port), "round {round}: orphan on {port}");
        assert!(gone_or_reaped(pid), "round {round}: zombie {pid}");
        assert!(
            store.list_endpoints().expect("list").is_empty(),
            "round {round}: stale state file"
        );
        if round == 0 {
            fds_after_first = open_fds();
        }
    }
    assert!(open_fds() <= fds_after_first + 2, "fd leak");
}
