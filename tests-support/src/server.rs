//! The fake `llama-server`: enough of the b9199 HTTP surface that the real supervisor,
//! the real health gate and the real relay cannot tell the difference.
//!
//! Served, faithfully to b9199 (`docs/port/00-machine-ground-truth.md`):
//!
//! | Endpoint | Note |
//! |---|---|
//! | `GET /health` | `200`, or `503 {"status":"loading model"}` while `load_ms` runs |
//! | `GET /v1/models` | llama.cpp's `{"object":"list","data":[…]}` envelope |
//! | `GET /props` | **404 unless argv carried `--props`** — it is off by default |
//! | `GET /slots` | on by default, **501** when argv carried `--no-slots` |
//! | `GET /metrics` | **404 unless argv carried `--metrics`** |
//! | `POST /v1/chat/completions` | buffered and SSE, both with `usage` and `timings` |
//! | `POST /v1/completions` | the same, in the legacy shape |
//! | `POST /v1/embeddings` | a deterministic vector |
//!
//! Plus a control surface no real server has, under `/_apex/`: `record`, `requests`,
//! `behavior` (GET and POST) and `exit`. That is how a test reads back the argv it was
//! launched with, sees what the router actually forwarded, and changes the fake's mind
//! half way through.
//!
//! Every number in a `timings` block is **fabricated**. Nothing here is a benchmark.

use crate::behavior::Behavior;
use crate::http::{
    begin_sse, end_chunked, error_body, read_request, respond, respond_json, write_chunk, Req,
};
use crate::record::LaunchRecord;
use std::io::{BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The build string `/props` reports, matching the machine ground truth.
pub const BUILD_INFO: &str = "b9199 (39cf5d619)";

/// How long an idle keep-alive connection is held before the thread lets it go.
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// How many requests the ring keeps for `/_apex/requests`.
const MAX_RECORDED_REQUESTS: usize = 200;

/// One request the fake received, kept so a test can assert on what was *sent* to it.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RecordedRequest {
    /// HTTP method.
    pub method: String,
    /// Path without the query.
    pub path: String,
    /// Query string.
    pub query: String,
    /// Headers, lowercased. `authorization` is kept: knowing whether the relay forwarded a
    /// credential is the point of several tests.
    pub headers: std::collections::BTreeMap<String, String>,
    /// The body as text.
    pub body: String,
}

impl RecordedRequest {
    /// The body parsed as JSON.
    pub fn json(&self) -> Option<serde_json::Value> {
        serde_json::from_str(&self.body).ok()
    }

    /// One header.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    /// `body.model`, the field the relay rewrites.
    pub fn model(&self) -> Option<String> {
        self.json()?
            .get("model")?
            .as_str()
            .map(std::borrow::ToOwned::to_owned)
    }
}

/// Where the fake prints its llama.cpp-shaped log lines. The subprocess writes to stdout,
/// which the supervisor has redirected into the endpoint's log file; an in-process stub
/// swallows them.
pub type LogFn = Box<dyn Fn(&str) + Send + Sync>;

/// Everything one fake server instance knows.
pub struct State {
    /// Live behaviour; `POST /_apex/behavior` replaces parts of it.
    pub behavior: Mutex<Behavior>,
    /// The launch, parsed. `record.flags` is how argv facts are read.
    pub record: LaunchRecord,
    /// Requests received, newest last, bounded.
    pub requests: Mutex<Vec<RecordedRequest>>,
    /// False while the model is "loading".
    pub ready: AtomicBool,
    /// Completion requests served, for `fail_first`.
    pub completions: AtomicU64,
    /// Set when the server should stop accepting.
    pub shutdown: Arc<AtomicBool>,
    /// The api key the server was started with, when `--api-key-file` named a readable one.
    pub api_key: Option<String>,
    /// Log sink.
    pub log: LogFn,
    /// The address actually bound.
    pub addr: SocketAddr,
    /// Whether `exit_after_ms` and `POST /_apex/exit` may call `std::process::exit`.
    ///
    /// True in the subprocess, where dying is the behaviour under test. **False in the
    /// in-process stub**, where it would take the test binary down with it — there, the
    /// same request stops the server and turns it unhealthy instead.
    pub allow_process_exit: bool,
}

impl State {
    /// The model ids `/v1/models` advertises: the behaviour override, else `-a`, else the
    /// file stem of `-m`, else `fake-model`.
    pub fn model_ids(&self) -> Vec<String> {
        let over = self.behavior().models;
        if !over.is_empty() {
            return over;
        }
        if let Some(alias) = self.record.alias.as_ref().filter(|a| !a.is_empty()) {
            return vec![alias.clone()];
        }
        match self.record.model.as_deref() {
            Some(path) => vec![file_stem(path)],
            None => vec!["fake-model".to_owned()],
        }
    }

    /// A snapshot of the current behaviour.
    pub fn behavior(&self) -> Behavior {
        match self.behavior.lock() {
            Ok(b) => b.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// How many slots to report: `--parallel`, then the behaviour override, then 1.
    fn slots_total(&self) -> u32 {
        let b = self.behavior();
        if b.slots_total > 0 {
            return b.slots_total;
        }
        self.record
            .flag("-np")
            .or_else(|| self.record.flag("--parallel"))
            .and_then(|v| v.parse().ok())
            .filter(|n| *n > 0)
            .unwrap_or(1)
    }

    /// The context to report: `-c`, else the behaviour default.
    fn ctx(&self) -> u32 {
        self.record
            .flag("-c")
            .or_else(|| self.record.flag("--ctx-size"))
            .and_then(|v| v.parse().ok())
            .filter(|n| *n > 0)
            .unwrap_or_else(|| self.behavior().ctx)
    }

    fn say(&self, line: &str) {
        (self.log)(line);
    }
}

/// A bound, not-yet-serving fake.
pub struct Server {
    listener: TcpListener,
    state: Arc<State>,
}

/// How to start one.
pub struct Config {
    /// Interface to bind. `127.0.0.1` unless argv said otherwise.
    pub host: String,
    /// Port to bind; `0` asks the kernel for one.
    pub port: u16,
    /// The launch as recorded. Supply [`LaunchRecord::from_process`] in the binary, or a
    /// synthetic one in-process.
    pub record: LaunchRecord,
    /// Behaviour to start with.
    pub behavior: Behavior,
    /// Where log lines go.
    pub log: LogFn,
    /// Whether this instance is allowed to end the process. The binary says true; the
    /// in-process stub says false.
    pub allow_process_exit: bool,
}

impl Server {
    /// Bind, without serving yet. Binding first is what lets `/health` answer
    /// `503 loading` while the "model" loads, exactly as llama.cpp does.
    ///
    /// # Errors
    /// Whatever `TcpListener::bind` says — in practice `EADDRINUSE`.
    pub fn bind(cfg: Config) -> std::io::Result<Server> {
        let listener = TcpListener::bind((cfg.host.as_str(), cfg.port))?;
        let addr = listener.local_addr()?;
        let api_key = cfg
            .record
            .flag("--api-key-file")
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|k| k.trim().to_owned())
            .filter(|k| !k.is_empty())
            .or_else(|| cfg.record.flag("--api-key").map(str::to_owned));

        Ok(Server {
            listener,
            state: Arc::new(State {
                behavior: Mutex::new(cfg.behavior),
                record: cfg.record,
                requests: Mutex::new(Vec::new()),
                ready: AtomicBool::new(false),
                completions: AtomicU64::new(0),
                shutdown: Arc::new(AtomicBool::new(false)),
                api_key,
                log: cfg.log,
                addr,
                allow_process_exit: cfg.allow_process_exit,
            }),
        })
    }

    /// The address it bound.
    pub fn addr(&self) -> SocketAddr {
        self.state.addr
    }

    /// `http://host:port`, **without** a trailing `/v1` — the form every `base_url` in this
    /// codebase is stored in.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.state.addr)
    }

    /// Shared state, so an in-process caller can read requests without HTTP.
    pub fn state(&self) -> Arc<State> {
        Arc::clone(&self.state)
    }

    /// Print the load lines, then flip to ready. Spawned so `/health` is answerable while
    /// it runs.
    pub fn start_loading(&self) {
        let state = Arc::clone(&self.state);
        std::thread::spawn(move || simulate_boot(&state));
    }

    /// Accept until told to stop. Blocks.
    pub fn serve(self) {
        let Server { listener, state } = self;
        for incoming in listener.incoming() {
            if state.shutdown.load(Ordering::SeqCst) {
                break;
            }
            let Ok(stream) = incoming else { continue };
            let state = Arc::clone(&state);
            std::thread::spawn(move || {
                if let Err(e) = serve_conn(stream, &state) {
                    if e.kind() != std::io::ErrorKind::UnexpectedEof {
                        state.say(&format!("apex-fake: connection ended: {e}"));
                    }
                }
            });
        }
    }
}

/// Emit llama.cpp's load sequence, then become healthy.
fn simulate_boot(state: &Arc<State>) {
    let b = state.behavior();
    let model = state.record.model.clone().unwrap_or_default();
    state.say(&format!("srv    load_model: loading model '{model}'"));
    state.say(&format!(
        "llama_model_loader: loaded meta data from {model}"
    ));

    if b.load_ms > 0 {
        let step = Duration::from_millis(b.load_ms / 4);
        for pct in [25, 50, 75, 100] {
            std::thread::sleep(step);
            state.say(&format!(
                "load_tensors: loading model tensors, this can take a while... {pct}%"
            ));
        }
    }
    if b.loading_forever {
        state.say("load_tensors: this build never finishes loading, on purpose");
        return;
    }
    state.say("load_tensors: offloaded 37/37 layers to GPU");
    state.say(&format!("llama_context: n_ctx = {}", state.ctx()));
    state.say("main: model loaded");
    state.ready.store(true, Ordering::SeqCst);
    state.say(&format!(
        "main: server is listening on http://{} - starting the main loop",
        state.addr
    ));

    if let Some(ms) = b.exit_after_ms {
        let state = Arc::clone(state);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(ms));
            state.say("apex-fake: exiting on demand (exit_after_ms)");
            stop(&state, state.behavior().exit_code);
        });
    }
}

/// One connection, request after request until it closes.
fn serve_conn(stream: TcpStream, state: &Arc<State>) -> std::io::Result<()> {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(IDLE_TIMEOUT));
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);

    loop {
        let req = match read_request(&mut reader) {
            Ok(Some(r)) => r,
            Ok(None) => return Ok(()),
            // A pooled connection that went idle, or a client that vanished.
            Err(_) => return Ok(()),
        };
        let keep = req.keep_alive();
        record_request(state, &req);
        let handled = handle(&req, &mut writer, state, keep);
        match handled {
            Handled::Continue if keep => continue,
            Handled::Continue => return Ok(()),
            Handled::Close => return Ok(()),
            Handled::Failed(e) => return Err(e),
        }
    }
}

/// What a handler decided about the connection.
enum Handled {
    /// The response was written; honour keep-alive.
    Continue,
    /// The connection must not be reused (a stream, or a deliberate abort).
    Close,
    /// Writing failed.
    Failed(std::io::Error),
}

impl From<std::io::Result<()>> for Handled {
    fn from(r: std::io::Result<()>) -> Handled {
        match r {
            Ok(()) => Handled::Continue,
            Err(e) => Handled::Failed(e),
        }
    }
}

/// Keep the last [`MAX_RECORDED_REQUESTS`] requests, minus the control surface's own.
fn record_request(state: &Arc<State>, req: &Req) {
    if req.path.starts_with("/_apex/") {
        return;
    }
    let entry = RecordedRequest {
        method: req.method.clone(),
        path: req.path.clone(),
        query: req.query.clone(),
        headers: req.headers.clone(),
        body: req.text(),
    };
    let mut kept = match state.requests.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if kept.len() >= MAX_RECORDED_REQUESTS {
        kept.remove(0);
    }
    kept.push(entry);
}

/// Route one request.
fn handle(req: &Req, w: &mut TcpStream, state: &Arc<State>, keep: bool) -> Handled {
    let b = state.behavior();

    // The control surface is never authenticated and never gated on readiness: a test that
    // cannot read the record of a launch that failed has lost the evidence it came for.
    if let Some(rest) = req.path.strip_prefix("/_apex/") {
        return control(rest, req, w, state, keep);
    }

    if let Some(expected) = state.api_key.as_deref() {
        let offered = req
            .header("authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim);
        if offered != Some(expected) {
            return respond_json(
                w,
                401,
                &error_body(401, "Invalid API Key", "authentication_error"),
                keep,
            )
            .into();
        }
    }

    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/health") => health(w, state, &b, keep),
        ("GET", "/v1/models") | ("GET", "/models") => {
            if !b.models_endpoint.resolve(true) {
                return not_found(w, keep);
            }
            respond_json(w, 200, &models_body(state), keep).into()
        }
        ("GET", "/props") => {
            if !b.props.resolve(state.record.has("--props")) {
                return not_found(w, keep);
            }
            respond_json(w, 200, &props_body(state), keep).into()
        }
        ("GET", "/slots") => {
            if !b.slots.resolve(!state.record.has("--no-slots")) {
                return respond_json(
                    w,
                    501,
                    &error_body(
                        501,
                        "This server does not support slots endpoint. Start it with --slots",
                        "not_supported_error",
                    ),
                    keep,
                )
                .into();
            }
            respond_json(w, 200, &slots_body(state), keep).into()
        }
        ("GET", "/metrics") => {
            if !b.metrics.resolve(state.record.has("--metrics")) {
                return not_found(w, keep);
            }
            respond(
                w,
                200,
                "text/plain; version=0.0.4",
                metrics_body(state).as_bytes(),
                keep,
            )
            .into()
        }
        ("POST", "/v1/chat/completions") | ("POST", "/chat/completions") => {
            completions(req, w, state, &b, keep, true)
        }
        ("POST", "/v1/completions") | ("POST", "/completion") | ("POST", "/completions") => {
            completions(req, w, state, &b, keep, false)
        }
        ("POST", "/v1/embeddings") | ("POST", "/embedding") | ("POST", "/embeddings") => {
            respond_json(w, 200, &embeddings_body(state), keep).into()
        }
        ("GET", "/") => {
            respond(w, 200, "text/html", b"<html>fake llama-server</html>", keep).into()
        }
        _ => not_found(w, keep),
    }
}

/// `/health`, including the three states the readiness gate must tell apart.
fn health(w: &mut TcpStream, state: &Arc<State>, b: &Behavior, keep: bool) -> Handled {
    if b.health_hang {
        park();
        return Handled::Close;
    }
    if let Some(status) = b.health_status {
        let body = if status == 200 {
            serde_json::json!({"status": "ok"})
        } else {
            error_body(status, "forced by behaviour", "server_error")
        };
        return respond_json(w, status, &body, keep).into();
    }
    if b.never_healthy {
        return respond_json(
            w,
            503,
            &error_body(503, "the model is not available", "unavailable_error"),
            keep,
        )
        .into();
    }
    if state.ready.load(Ordering::SeqCst) {
        return respond_json(w, 200, &serde_json::json!({"status": "ok"}), keep).into();
    }
    respond_json(
        w,
        503,
        &serde_json::json!({"status": "loading model"}),
        keep,
    )
    .into()
}

/// `/v1/models`, in llama.cpp's envelope shape.
fn models_body(state: &Arc<State>) -> serde_json::Value {
    let ctx = state.ctx();
    let data: Vec<serde_json::Value> = state
        .model_ids()
        .into_iter()
        .map(|id| {
            serde_json::json!({
                "id": id,
                "object": "model",
                "created": now_unix(),
                "owned_by": "llamacpp",
                "meta": {"n_ctx_train": ctx, "n_vocab": 128256, "n_params": 0, "size": 0}
            })
        })
        .collect();
    serde_json::json!({"object": "list", "data": data})
}

/// `/props`.
fn props_body(state: &Arc<State>) -> serde_json::Value {
    serde_json::json!({
        "default_generation_settings": {
            "id": 0,
            "n_ctx": state.ctx(),
            "params": {"n_predict": -1, "temperature": 0.8}
        },
        "total_slots": state.slots_total(),
        "model_path": state.record.model.clone().unwrap_or_default(),
        "modalities": {"vision": false, "audio": false},
        "chat_template_caps": {"supports_tools": true, "supports_tool_calls": true},
        "bos_token": "<|begin_of_text|>",
        "eos_token": "<|eot_id|>",
        "build_info": BUILD_INFO
    })
}

/// `/slots` — a bare array, and it echoes prompts, which is why ApexRouter never proxies it.
fn slots_body(state: &Arc<State>) -> serde_json::Value {
    let total = state.slots_total();
    let busy = state.behavior().busy_slots.min(total);
    let slots: Vec<serde_json::Value> = (0..total)
        .map(|i| {
            let processing = i < busy;
            serde_json::json!({
                "id": i,
                "id_task": if processing { i as i64 } else { -1 },
                "is_processing": processing,
                "prompt": if processing { "a prompt that must never be proxied" } else { "" },
                "next_token": {"has_next_token": processing, "n_decoded": 0}
            })
        })
        .collect();
    serde_json::Value::Array(slots)
}

/// `/metrics`, in the Prometheus text exposition format.
fn metrics_body(state: &Arc<State>) -> String {
    let served = state.completions.load(Ordering::SeqCst);
    let total = state.slots_total() as u64;
    let busy = u64::from(state.behavior().busy_slots.min(state.slots_total()));
    format!(
        "# HELP llamacpp:prompt_tokens_total Number of prompt tokens processed.\n\
         # TYPE llamacpp:prompt_tokens_total counter\n\
         llamacpp:prompt_tokens_total {}\n\
         # HELP llamacpp:tokens_predicted_total Number of generation tokens processed.\n\
         # TYPE llamacpp:tokens_predicted_total counter\n\
         llamacpp:tokens_predicted_total {}\n\
         # HELP llamacpp:requests_processing Number of requests processing.\n\
         # TYPE llamacpp:requests_processing gauge\n\
         llamacpp:requests_processing {busy}\n\
         # HELP llamacpp:requests_deferred Number of requests deferred.\n\
         # TYPE llamacpp:requests_deferred gauge\n\
         llamacpp:requests_deferred 0\n\
         # HELP llamacpp:n_slots Number of slots.\n\
         # TYPE llamacpp:n_slots gauge\n\
         llamacpp:n_slots {total}\n",
        served * 7,
        served * 11,
    )
}

/// `/v1/embeddings`, deterministic so a test can assert on it.
fn embeddings_body(state: &Arc<State>) -> serde_json::Value {
    let model = state.model_ids().first().cloned().unwrap_or_default();
    let vector: Vec<f32> = (0..8).map(|i| i as f32 / 8.0).collect();
    serde_json::json!({
        "object": "list",
        "model": model,
        "data": [{"object": "embedding", "index": 0, "embedding": vector}],
        "usage": {"prompt_tokens": 4, "total_tokens": 4}
    })
}

/// `POST /v1/chat/completions` and `POST /v1/completions`.
fn completions(
    req: &Req,
    w: &mut TcpStream,
    state: &Arc<State>,
    b: &Behavior,
    keep: bool,
    chat: bool,
) -> Handled {
    let n = state.completions.fetch_add(1, Ordering::SeqCst);

    if let Some(status) = b.chat_status {
        return respond_json(
            w,
            status,
            &error_body(status, "forced by behaviour", "server_error"),
            keep,
        )
        .into();
    }
    if u64::from(b.fail_first) > n {
        return respond_json(
            w,
            503,
            &error_body(503, "the model is overloaded", "unavailable_error"),
            keep,
        )
        .into();
    }
    if b.hang_before_headers {
        park();
        return Handled::Close;
    }
    if b.ttft_ms > 0 {
        std::thread::sleep(Duration::from_millis(b.ttft_ms));
    }

    let body = req.json().unwrap_or(serde_json::Value::Null);
    let model = body
        .get("model")
        .and_then(|m| m.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| state.model_ids().first().cloned().unwrap_or_default());
    let stream = body
        .get("stream")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let text = reply_text(b, &body);
    let prompt_tokens = b.prompt_tokens.unwrap_or_else(|| count_tokens(&body));
    let pieces = split_pieces(&text, b.chunks.max(1) as usize);
    let completion_tokens = u32::try_from(pieces.len()).unwrap_or(u32::MAX);
    let id = format!("chatcmpl-{}", n + 1);

    if !stream {
        let body = buffered_body(
            &id,
            &model,
            &text,
            b,
            prompt_tokens,
            completion_tokens,
            chat,
        );
        return respond_json(w, 200, &body, keep).into();
    }
    sse(
        w,
        state,
        b,
        &id,
        &model,
        &pieces,
        prompt_tokens,
        completion_tokens,
        chat,
    )
}

/// A buffered completion, with `usage` **and** llama.cpp's `timings`.
fn buffered_body(
    id: &str,
    model: &str,
    text: &str,
    b: &Behavior,
    prompt_tokens: u32,
    completion_tokens: u32,
    chat: bool,
) -> serde_json::Value {
    let choice = if chat {
        let mut message = serde_json::json!({"role": "assistant", "content": text});
        if b.reasoning {
            message = serde_json::json!({
                "role": "assistant", "content": "", "reasoning_content": text
            });
        }
        if let Some(args) = b.tool_call.as_ref() {
            message = serde_json::json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_fake_1", "type": "function",
                    "function": {"name": "fake_tool", "arguments": args}
                }]
            });
        }
        serde_json::json!({
            "index": 0,
            "message": message,
            "finish_reason": if b.tool_call.is_some() { "tool_calls" } else { "stop" }
        })
    } else {
        serde_json::json!({"index": 0, "text": text, "finish_reason": "stop"})
    };

    serde_json::json!({
        "id": id,
        "object": if chat { "chat.completion" } else { "text_completion" },
        "created": now_unix(),
        "model": model,
        "system_fingerprint": "b9199-apexfake",
        "choices": [choice],
        "usage": usage_block(prompt_tokens, completion_tokens),
        "timings": timings_block(b, prompt_tokens, completion_tokens)
    })
}

/// The SSE path. Chunked, flushed per frame, and able to end three different ways.
#[allow(clippy::too_many_arguments)]
fn sse(
    w: &mut TcpStream,
    state: &Arc<State>,
    b: &Behavior,
    id: &str,
    model: &str,
    pieces: &[String],
    prompt_tokens: u32,
    completion_tokens: u32,
    chat: bool,
) -> Handled {
    if let Err(e) = begin_sse(w) {
        return Handled::Failed(e);
    }
    let frame = |w: &mut TcpStream, v: &serde_json::Value| -> std::io::Result<()> {
        write_chunk(w, format!("data: {v}\n\n").as_bytes())
    };
    let envelope = |choices: serde_json::Value| {
        serde_json::json!({
            "id": id,
            "object": if chat { "chat.completion.chunk" } else { "text_completion" },
            "created": now_unix(),
            "model": model,
            "system_fingerprint": "b9199-apexfake",
            "choices": choices
        })
    };

    // The role-only opener, exactly as llama.cpp sends it.
    if chat {
        let opener = envelope(serde_json::json!([{
            "index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": null
        }]));
        if let Err(e) = frame(w, &opener) {
            return Handled::Failed(e);
        }
    }

    for (i, piece) in pieces.iter().enumerate() {
        if b.chunk_ms > 0 {
            std::thread::sleep(Duration::from_millis(b.chunk_ms));
        }
        // Half way through, on demand, the connection simply dies: no terminating chunk,
        // so the client sees a transport error rather than an end of stream.
        if b.die_mid_stream && i * 2 >= pieces.len() {
            state.say("apex-fake: aborting the stream mid-chunk (die_mid_stream)");
            let _ = w.write_all(b"1a\r\ndata: {\"choices\":[{\"del");
            let _ = w.flush();
            let _ = w.shutdown(std::net::Shutdown::Both);
            return Handled::Close;
        }
        let delta = if !chat {
            serde_json::json!([{"index": 0, "text": piece, "finish_reason": null}])
        } else if b.reasoning {
            serde_json::json!([{"index": 0, "delta": {"reasoning_content": piece}, "finish_reason": null}])
        } else if let Some(args) = b.tool_call.as_ref() {
            serde_json::json!([{"index": 0, "delta": {"tool_calls": [{
                "index": 0, "id": "call_fake_1", "type": "function",
                "function": {"name": "fake_tool", "arguments": args}
            }]}, "finish_reason": null}])
        } else {
            serde_json::json!([{"index": 0, "delta": {"content": piece}, "finish_reason": null}])
        };
        if let Err(e) = frame(w, &envelope(delta)) {
            return Handled::Failed(e);
        }
    }

    // The final frame carries `finish_reason`, `usage` and `timings` together, which is
    // where llama.cpp puts them and where the relay's tee looks for them.
    let mut last = envelope(serde_json::json!([{
        "index": 0,
        "delta": if chat { serde_json::json!({}) } else { serde_json::Value::Null },
        "text": if chat { serde_json::Value::Null } else { serde_json::json!("") },
        "finish_reason": if b.tool_call.is_some() { "tool_calls" } else { "stop" }
    }]));
    if let Some(obj) = last.as_object_mut() {
        obj.insert(
            "usage".to_owned(),
            usage_block(prompt_tokens, completion_tokens),
        );
        obj.insert(
            "timings".to_owned(),
            timings_block(b, prompt_tokens, completion_tokens),
        );
    }
    if let Err(e) = frame(w, &last) {
        return Handled::Failed(e);
    }

    // A clean end with no `[DONE]` is a truncation, and the relay must call it death.
    if b.truncate_stream {
        state.say("apex-fake: ending the stream without [DONE] (truncate_stream)");
        let _ = end_chunked(w);
        return Handled::Close;
    }
    if let Err(e) = write_chunk(w, b"data: [DONE]\n\n") {
        return Handled::Failed(e);
    }
    let _ = end_chunked(w);
    Handled::Close
}

/// `/_apex/*` — the control surface.
fn control(rest: &str, req: &Req, w: &mut TcpStream, state: &Arc<State>, keep: bool) -> Handled {
    match (req.method.as_str(), rest) {
        ("GET", "record") => {
            let body = serde_json::to_value(&state.record).unwrap_or(serde_json::Value::Null);
            respond_json(w, 200, &body, keep).into()
        }
        ("GET", "requests") => {
            let kept = match state.requests.lock() {
                Ok(g) => g.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            };
            let body = serde_json::to_value(kept).unwrap_or(serde_json::Value::Null);
            respond_json(w, 200, &body, keep).into()
        }
        ("DELETE", "requests") => {
            match state.requests.lock() {
                Ok(mut g) => g.clear(),
                Err(poisoned) => poisoned.into_inner().clear(),
            }
            respond_json(w, 200, &serde_json::json!({"cleared": true}), keep).into()
        }
        ("GET", "behavior") => respond_json(w, 200, &state.behavior().to_json(), keep).into(),
        ("POST", "behavior") => {
            let mut guard = match state.behavior.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            // Three accepted shapes: a JSON object of knobs, a JSON string holding a
            // spec, or a raw spec as the body.
            let unknown = match req.json() {
                Some(v) if v.is_object() => guard.apply_json(&v),
                Some(serde_json::Value::String(spec)) => {
                    guard.apply_spec(&spec);
                    Vec::new()
                }
                _ => {
                    guard.apply_spec(&req.text());
                    Vec::new()
                }
            };
            let body = serde_json::json!({"applied": true, "unknown": unknown});
            drop(guard);
            respond_json(w, 200, &body, keep).into()
        }
        ("POST", "exit") => {
            let code: i32 = req
                .param("code")
                .and_then(|c| c.parse().ok())
                .unwrap_or_else(|| state.behavior().exit_code);
            let _ = respond_json(w, 200, &serde_json::json!({"exiting": code}), false);
            state.say("apex-fake: exiting on request");
            // Give the response a moment to leave the socket before the process goes.
            let s = Arc::clone(state);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                stop(&s, code);
            });
            Handled::Close
        }
        ("POST", "ready") => {
            state.ready.store(true, Ordering::SeqCst);
            respond_json(w, 200, &serde_json::json!({"ready": true}), keep).into()
        }
        _ => not_found(w, keep),
    }
}

/// The 404 body llama.cpp itself sends.
fn not_found(w: &mut TcpStream, keep: bool) -> Handled {
    respond_json(
        w,
        404,
        &error_body(404, "File Not Found", "not_found_error"),
        keep,
    )
    .into()
}

/// End this server.
///
/// In the subprocess that means `exit(code)`, which is the whole point of `exit_after_ms`
/// and `POST /_apex/exit`. In the in-process stub it would take the **test binary** down,
/// so it stops accepting and goes permanently unhealthy instead — observably dead to the
/// code under test, harmless to the harness.
fn stop(state: &Arc<State>, code: i32) {
    if state.allow_process_exit {
        std::process::exit(code);
    }
    state.say("apex-fake: in-process stub cannot exit(); going unhealthy instead");
    state.ready.store(false, Ordering::SeqCst);
    match state.behavior.lock() {
        Ok(mut b) => b.never_healthy = true,
        Err(poisoned) => poisoned.into_inner().never_healthy = true,
    }
    state.shutdown.store(true, Ordering::SeqCst);
}

/// Block this connection's thread for a very long time, on purpose.
fn park() {
    std::thread::sleep(Duration::from_secs(86_400));
}

/// What the assistant says: the echo of the last user message, or the configured content.
fn reply_text(b: &Behavior, body: &serde_json::Value) -> String {
    if !b.echo {
        return b.content.clone();
    }
    let from_messages = body
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .and_then(|msgs| msgs.iter().rev().find(|m| role_is(m, "user")))
        .and_then(|m| m.get("content"))
        .map(flatten_content);
    from_messages
        .or_else(|| {
            body.get("prompt")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| b.content.clone())
}

/// Whether a message has this role.
fn role_is(m: &serde_json::Value, role: &str) -> bool {
    m.get("role").and_then(serde_json::Value::as_str) == Some(role)
}

/// OpenAI content is either a string or an array of typed blocks.
fn flatten_content(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        other => other.to_string(),
    }
}

/// A crude token count over the whole request — deterministic, which is all a test needs.
fn count_tokens(body: &serde_json::Value) -> u32 {
    let text = body.to_string();
    u32::try_from(text.split_whitespace().count().max(1)).unwrap_or(u32::MAX)
}

/// Cut the reply into `n` pieces, keeping every byte.
fn split_pieces(text: &str, n: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let chars: Vec<char> = text.chars().collect();
    let n = n.min(chars.len()).max(1);
    let per = chars.len().div_ceil(n);
    chars
        .chunks(per)
        .map(|c| c.iter().collect::<String>())
        .collect()
}

/// The OpenAI `usage` object, with the cached-token detail the tee reads.
fn usage_block(prompt_tokens: u32, completion_tokens: u32) -> serde_json::Value {
    serde_json::json!({
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
        "total_tokens": prompt_tokens + completion_tokens,
        "prompt_tokens_details": {"cached_tokens": 0}
    })
}

/// llama.cpp's `timings` object. **Fabricated**, derived from `tok_per_s`.
fn timings_block(b: &Behavior, prompt_tokens: u32, completion_tokens: u32) -> serde_json::Value {
    let tps = if b.tok_per_s > 0.0 { b.tok_per_s } else { 1.0 };
    let prompt_tps = tps * 5.0;
    let prompt_ms = f64::from(prompt_tokens) * 1000.0 / f64::from(prompt_tps);
    let predicted_ms = f64::from(completion_tokens) * 1000.0 / f64::from(tps);
    serde_json::json!({
        "cache_n": 0,
        "prompt_n": prompt_tokens,
        "prompt_ms": prompt_ms,
        "prompt_per_token_ms": if prompt_tokens > 0 { prompt_ms / f64::from(prompt_tokens) } else { 0.0 },
        "prompt_per_second": prompt_tps,
        "predicted_n": completion_tokens,
        "predicted_ms": predicted_ms,
        "predicted_per_token_ms": if completion_tokens > 0 { predicted_ms / f64::from(completion_tokens) } else { 0.0 },
        "predicted_per_second": tps
    })
}

/// `"/models/carnice-9b/Carnice-9b-Q6_K.gguf"` -> `"Carnice-9b-Q6_K"`.
fn file_stem(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.')
        .map(|(stem, _)| stem.to_owned())
        .unwrap_or_else(|| name.to_owned())
}

/// Seconds since the epoch.
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_is_cut_into_pieces_without_losing_a_byte() {
        let pieces = split_pieces("hello world", 4);
        assert_eq!(pieces.len(), 4);
        assert_eq!(pieces.concat(), "hello world");
        assert_eq!(split_pieces("", 4), vec![String::new()]);
        assert_eq!(split_pieces("ab", 9).concat(), "ab");
    }

    #[test]
    fn timings_are_derived_from_the_configured_rate_and_never_measured() {
        let b = Behavior::parse("tok_per_s=10");
        let t = timings_block(&b, 100, 50);
        assert_eq!(t["predicted_n"], 50);
        assert_eq!(t["predicted_per_second"], 10.0);
        // 50 tokens at 10/s is 5000 ms, exactly.
        assert_eq!(t["predicted_ms"], 5000.0);
    }

    #[test]
    fn an_echo_reply_reads_the_last_user_message_in_both_content_shapes() {
        let b = Behavior::parse("echo");
        let flat = serde_json::json!({"messages": [
            {"role": "system", "content": "be nice"},
            {"role": "user", "content": "first"},
            {"role": "user", "content": "second"}
        ]});
        assert_eq!(reply_text(&b, &flat), "second");

        let blocks = serde_json::json!({"messages": [
            {"role": "user", "content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]}
        ]});
        assert_eq!(reply_text(&b, &blocks), "a b");
    }

    #[test]
    fn the_file_stem_is_the_model_id_when_there_is_no_alias() {
        assert_eq!(
            file_stem("/home/andre/models/carnice-9b/Carnice-9b-Q6_K.gguf"),
            "Carnice-9b-Q6_K"
        );
    }
}
