# `apexrouter-tests-support` — the fake `llama-server` and the stub upstream

Two test doubles, one implementation, no GPU.

| You are testing | Use | It gives you |
|---|---|---|
| the **supervisor** (spawn, health gate, argv, adopt, stop) | `FakeBuild` | a llama.cpp-shaped build tree the real discovery finds, and a **launch record** of the exact argv and environment the child got |
| the **router** (swap, failover, relay, Anthropic translation, usage tee) | `Stub` | an OpenAI-compatible upstream on a loopback port, in this process, plus every request it received |
| a fake the supervisor started, from outside | `Control` | its launch record, its requests and its behaviour, over HTTP |

Add it as a dev-dependency of whichever crate your test lives in:

```toml
[dev-dependencies]
apexrouter-tests-support = { workspace = true }
```

The fake binary is `target/debug/llama-server`, produced by `cargo test --workspace` and by
`cargo build -p apexrouter-tests-support --bin llama-server`. If it is missing when
`FakeBuild::new()` runs, the helper builds it `--offline` into `target/apex-tests-support/`
(≈6 s, once) — a separate target directory on purpose, because `cargo test` holds a lock on
its own. `$APEX_FAKE_LLAMA_BIN` overrides the lookup entirely.

---

## 1. Spawn a fake endpoint under ApexRouter's own supervisor

```rust
use apexrouter_core::config::Config;
use apexrouter_protocol::{EndpointSpec, KvType, LocalLlamaSpec, NglPlan, SamplingMode,
                          SplitMode, SplitPlan, TriState};
use apexrouter_providers::local::{DownMode, LocalProvisioner, Provisioner};
use apexrouter_tests_support::{FakeBuild, GgufSpec};

let fake  = FakeBuild::new();                                     // build tree + binary
let model = fake.model("Fake-9b-Q6_K.gguf", &GgufSpec::default().sized_mb(1));

let mut cfg = Config::default();
cfg.endpoints.port_range      = (39_500, 39_540);                 // yours alone, please
cfg.endpoints.build_roots     = vec![fake.root().display().to_string()];
cfg.supervisor.health_deadline_ms = 15_000;
cfg.supervisor.health_interval_ms = 50;                           // 3 s is the shipped default

let (tx, mut rx) = tokio::sync::broadcast::channel(256);
let prov = LocalProvisioner::new(paths, cfg, tx);                 // `paths` — see below
prov.set_rig(fake.rig(20_992, 19_518));                           // instead of probing the box

let spec = EndpointSpec::LocalLlama(LocalLlamaSpec {
    build:       fake.build_id(),                                 // "build-fake"
    model_path:  model.display().to_string(),
    port:        None,                                            // let the allocator choose
    ctx:         Some(8192),
    parallel:    Some(2),
    kv_type:     Some(KvType::Q8_0),
    ngl:         NglPlan::All,
    mode:        SamplingMode::Coding,
    flash_attn:  Some(TriState::Auto),
    split: SplitPlan { devices: vec!["Vulkan0".into()], mode: SplitMode::Layer,
                       main_gpu: None, tensor_split: None },
    mmproj: None, alias_flag: "fake-9b".into(), host: "127.0.0.1".into(),
    api_key: None, extra_args: vec![],
});

let plan    = prov.plan(&spec).await.expect("plan");
let backend = prov.up(plan, None).await.expect("up");             // the health gate passed
prov.down(&backend.id, DownMode::Now).await.expect("down");
```

`set_rig` is the fast path: the supervisor takes the rig you hand it and never shells out to
`--help` / `--list-devices` / `--version`. Drop it and real discovery runs against the fake
instead — that works too (`discovery_finds_the_fake_build_by_globbing_the_tree`), it is just
slower and it makes the test depend on process spawning three more times.

`fake.root()` is the **build root**, not the build directory: discovery globs
`build*/bin/llama-server` under it.

`paths` is a `Paths` over a `TempDir`, and getting one means touching `$APEXROUTER_HOME`,
which is process-global. Copy `paths_at()` from `tests/supervisor_e2e.rs`: it takes a
`Mutex`, sets the variable, resolves, and puts the old value back before anything else
looks. Do not hold that lock across an `.await`.

## 2. Read back the recorded argv

The fake writes its launch record before it does anything else, so it exists even when the
launch then fails.

```rust
let port = /* backend.base_url's port */;
let rec  = fake.records().for_port(port).expect("launch record");

assert_eq!(rec.flag("-c"),   Some("8192"));          // -flag value pairs
assert_eq!(rec.flag("-ngl"), Some("999"));
assert_eq!(rec.flag_as::<u32>("-np"), Some(2));
assert!(rec.has("--props"));                         // bare switches
assert!(!rec.argv_contains("--jinja"));              // it is default-on in b9199
assert_eq!(rec.env_var("LD_LIBRARY_PATH"), Some(bin_dir));
assert_eq!(rec.env_var("GGML_VK_VISIBLE_DEVICES"), Some("0"));
eprintln!("{}", rec.argv_line());                    // for the failure message
```

Other ways in:

```rust
fake.records().latest()                       // the most recent launch
fake.records().all()                          // every launch, in order
fake.records().wait_for_port(port, timeout)   // only if you are racing the launch
Control::at(&backend.base_url).record()?      // the same record, over HTTP
```

Where it lands, in precedence order: `--apex-record <path>` in argv, then
`$APEX_FAKE_LLAMA_RECORD`, then `<build root>/records/` — which is why the zero-config case
works. A directory gets `port-<port>.json` plus a line in `launches.jsonl`.

**Environment values are redacted** for names that look like credentials (`*KEY*`,
`*TOKEN*`, `*SECRET*`, …); the names are listed in `rec.env_redacted`. `LD_*`, `GGML_*`,
`CUDA_*`, `HIP_*`, `PATH`, `HOME` and `APEX*` are never redacted. `APEX_FAKE_LLAMA_RECORD_ENV=all`
turns redaction off.

## 3. Point an alias at a stub upstream

```rust
use apexrouter_router::{proxy_router, RouterInner, TableBuilder};
use apexrouter_tests_support::{routing, Stub};

let upstream = Stub::with("echo");                       // or Stub::start()
let file = routing::route_file(
    "auto",
    vec![routing::route_to("auto", &["stub-a"], Some("stub-model"))],
);

let cfg   = Arc::new(test_config());                     // together -> 127.0.0.1:1
let usage = UsageWriter::open(&paths, &cfg.compat).expect("usage");
let router = RouterInner::new(Arc::clone(&cfg), tx, usage);
router.registry().upsert(upstream.backend("stub-a"), &cfg.router);
router.store_table(TableBuilder::compile(&cfg, &file, router.registry()).expect("compile"));

let response = proxy_router(router).oneshot(post_chat(r#"{"model":"auto",...}"#)).await?;

// What did the upstream actually receive?
let seen = upstream.last_request().expect("a request");
assert_eq!(seen.model().as_deref(), Some("stub-model"));   // the route's rewrite
assert_eq!(seen.header("authorization"), None);            // the client's key never travels
let body = seen.json().expect("json");                     // the whole translated request
```

`tests/router_wiring.rs` is that snippet, compiled and run, including a failover across two
stubs. Copy from there rather than from here if the two ever disagree.

`upstream.backend(id)` returns an `apexrouter_protocol::Backend`: `Node`, `OpenAi`, `Ready`,
`Free`, four slots, `base_url` stored **without** `/v1`.

---

## Making it misbehave

One syntax, three delivery routes:

* `--apex-behavior <spec>` in argv — per launch, and it rides in through
  `LocalLlamaSpec::extra_args`, which the argv builder passes through verbatim.
* `$APEX_FAKE_LLAMA_BEHAVIOR` — per process tree.
* `POST /_apex/behavior` — **live**, via `Control::set_behavior` or `Stub::set_behavior`.
  This is how you make a healthy backend start failing while a breaker test watches.

`Stub::with("…")` takes the same spec. Later keys win; `key=0` clears a flag.

| Spec | What happens |
|---|---|
| `load_ms=600` | `/health` answers `503 {"status":"loading model"}` for 600 ms, printing llama.cpp load lines. **This is progress** — the health deadline resets on it. |
| `refuse_start` | prints `failed to load model` and exits 3 **before binding**. (`--fake-exit-early` also works.) |
| `stall` | never binds, sleeps for ever → connection refused → the health gate times out. (`--fake-never-healthy`.) |
| `never_healthy` | binds, `/health` 503 that is **not** about loading → not progress. |
| `loading_forever` | binds, loads for ever → progress every tick, deadline never fires. |
| `exit_after_ms=250` | becomes healthy, then dies. |
| `health_hang` | accepts the `/health` connection and never answers. |
| `chat_status=503` | every completion returns that status with an OpenAI error body. |
| `fail_first=2` | the first two completions 503, the rest succeed. |
| `hang_before_headers` | reads the request, never responds. |
| `ttft_ms=250` | delay before the response headers. |
| `chunks=8,chunk_ms=20,content=abcdefgh` | stream pacing. `chunks` is capped at one character per chunk, so give it content. |
| `die_mid_stream` | aborts the connection mid-chunk → a transport error. |
| `truncate_stream` | ends the stream cleanly with **no** `data: [DONE]` → the truncation the relay must call death. |
| `reasoning` | `reasoning_content` set, `content` empty. |
| `tool_call={"a":1}` | a `tool_calls` message/delta and `finish_reason: "tool_calls"`. |
| `echo` | replies with the last user message — assert on what the upstream received. |
| `busy_slots=3` | `/slots` reports three processing, which is what a drain waits on. |
| `props=off` / `slots=off` / `metrics=on` | override the argv-derived endpoint switches. |
| `models=a|b` | what `/v1/models` advertises. |
| `content=…`, `ctx=…`, `slots_total=…`, `tok_per_s=…`, `prompt_tokens=…`, `exit_code=…` | the obvious things. |

## What it serves

Faithful to b9199 (`docs/port/00-machine-ground-truth.md`), which means some of it is **off
by default**:

| Endpoint | Behaviour |
|---|---|
| `GET /health` | `200 {"status":"ok"}`, or `503 {"status":"loading model"}` while loading |
| `GET /v1/models` | llama.cpp's `{"object":"list","data":[…]}` with `meta.n_ctx_train` |
| `GET /props` | **404 unless argv carried `--props`** — `total_slots`, `n_ctx`, `model_path`, `build_info`, `chat_template_caps` |
| `GET /slots` | a bare array; **501 when argv carried `--no-slots`** |
| `GET /metrics` | **404 unless argv carried `--metrics`** — Prometheus text |
| `POST /v1/chat/completions` | buffered and SSE, both with `usage` **and** llama.cpp `timings` |
| `POST /v1/completions`, `POST /v1/embeddings` | the legacy and embedding shapes |
| `--api-key-file` | if argv named a readable one, everything but `/_apex/*` needs that bearer |

Streams end `data: [DONE]`; the frame before it carries `finish_reason`, `usage` and
`timings` together, which is where the relay's tee looks.

`--help`, `--version` and `--list-devices` answer the three probes `core::discover::builds`
makes, and are **not** recorded as launches.

Control surface, which no real server has:

```
GET    /_apex/record      the LaunchRecord as JSON
GET    /_apex/requests    every request received (method, path, headers, body)
DELETE /_apex/requests    forget them
GET    /_apex/behavior    the current knobs
POST   /_apex/behavior    {"chat_status": 503} or a bare "chat_status=503" spec
POST   /_apex/exit?code=1 exit now, so the supervisor can notice
POST   /_apex/ready       stop pretending to load
```

## House rules this obeys

* **Every socket is `127.0.0.1`.** Nothing here can reach a network.
* **No `sh -c`.** The on-demand build is an argv vector, and `--offline`.
* **Nothing is written into the repo at runtime** — build trees are `TempDir`s, and the
  on-demand build writes to `target/`, which is build output.
* **No number this crate emits is a benchmark.** Every `timings` block is derived from the
  configured `tok_per_s`. If you see 12.5 tok/s in a test, that is arithmetic, not silicon.
