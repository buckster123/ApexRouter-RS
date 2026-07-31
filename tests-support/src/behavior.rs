//! How to make the fake misbehave.
//!
//! One syntax everywhere: a comma-separated list of `key=value` pairs and bare flags.
//!
//! ```text
//! load_ms=500,chunks=8,chunk_ms=20        # a slow start and a slow stream
//! reasoning                                # reasoning_content, empty content
//! chat_status=503                          # a 503 from the chat endpoint
//! die_mid_stream                           # abort the connection mid-SSE
//! ```
//!
//! It arrives by three routes, in this order of precedence:
//!
//! 1. `--apex-behavior <spec>` in argv — **per launch**, and it rides through
//!    `LocalLlamaSpec::extra_args`, which the argv builder passes through verbatim.
//! 2. `$APEX_FAKE_LLAMA_BEHAVIOR` — per process tree.
//! 3. `POST /_apex/behavior` with a JSON object — **live**, so a backend can be made to
//!    start failing while a test watches the breaker open.

use std::collections::BTreeMap;

/// A three-state endpoint switch: follow argv, force on, force off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Toggle {
    /// Behave like b9199: `/props` and `/metrics` need their flag, `/slots` is on unless
    /// `--no-slots`.
    #[default]
    Auto,
    /// Serve it whatever argv said.
    On,
    /// Refuse it whatever argv said.
    Off,
}

impl Toggle {
    /// Resolve against what argv asked for.
    pub fn resolve(self, from_argv: bool) -> bool {
        match self {
            Toggle::Auto => from_argv,
            Toggle::On => true,
            Toggle::Off => false,
        }
    }

    /// `on|off|auto|true|false|1|0|yes|no`.
    fn parse(v: &str) -> Toggle {
        match v.trim().to_ascii_lowercase().as_str() {
            "on" | "true" | "1" | "yes" => Toggle::On,
            "off" | "false" | "0" | "no" => Toggle::Off,
            _ => Toggle::Auto,
        }
    }
}

/// Everything the fake can be told to do wrong (and the handful of things it can be told
/// to do slowly).
#[derive(Clone, Debug)]
pub struct Behavior {
    // ---- start-up -------------------------------------------------------------------
    /// Answer `/health` with `503 {"status":"loading model"}` for this long before
    /// becoming healthy, emitting llama.cpp load lines as it goes. This is *progress*: the
    /// supervisor's deadline resets on it.
    pub load_ms: u64,
    /// Print a load failure and exit non-zero **before** binding. `--fake-exit-early`.
    pub refuse_start: bool,
    /// Exit code for [`Behavior::refuse_start`] and `POST /_apex/exit`.
    pub exit_code: i32,
    /// Never bind at all; just sleep. Connection refused is *not* progress, so this is how
    /// a health-gate timeout is provoked. `--fake-never-healthy`.
    pub stall: bool,
    /// Bind, but answer `/health` with a 503 that is **not** about loading — alive, wrong,
    /// and not progress.
    pub never_healthy: bool,
    /// Bind and stay loading for ever. Progress every tick, so the deadline never fires:
    /// the test for "a real deadline resets on observed progress".
    pub loading_forever: bool,
    /// Exit this long after becoming healthy, without being asked. Simulates a crash.
    pub exit_after_ms: Option<u64>,

    // ---- the endpoints ---------------------------------------------------------------
    /// Force a status on `/health`.
    pub health_status: Option<u16>,
    /// Accept the `/health` connection and never answer it.
    pub health_hang: bool,
    /// `/props`. `Auto` = only when argv carried `--props`, as b9199 does.
    pub props: Toggle,
    /// `/slots`. `Auto` = on unless argv carried `--no-slots`; off answers **501**.
    pub slots: Toggle,
    /// `/metrics`. `Auto` = only when argv carried `--metrics`.
    pub metrics: Toggle,
    /// `/v1/models`. `Off` makes it 404 — an upstream with neither health nor a model list.
    pub models_endpoint: Toggle,
    /// Model ids to advertise instead of the one derived from `-a`/`-m`.
    pub models: Vec<String>,
    /// `total_slots` in `/props`, and the length of `/slots`.
    pub slots_total: u32,
    /// How many slots report `is_processing` — what a drain waits on.
    pub busy_slots: u32,
    /// `default_generation_settings.n_ctx` in `/props`.
    pub ctx: u32,

    // ---- completions -------------------------------------------------------------------
    /// Force a status on `/v1/chat/completions` and `/v1/completions`.
    pub chat_status: Option<u16>,
    /// Fail the first N completion requests with 503, then behave. Failover and breaker
    /// tests need exactly this.
    pub fail_first: u32,
    /// Delay before the response headers.
    pub ttft_ms: u64,
    /// Read the request, then never answer. The `headers_timeout` case.
    pub hang_before_headers: bool,
    /// Delay between streamed chunks.
    pub chunk_ms: u64,
    /// How many content chunks a stream emits. Capped at one character per chunk, so a
    /// long stream needs a long `content` — `content=abcdefgh,chunks=8`.
    pub chunks: u32,
    /// Abort the TCP connection mid-chunk: a transport error, not an EOF.
    pub die_mid_stream: bool,
    /// End the stream cleanly with **no** `data: [DONE]` — the truncation the relay must
    /// call death rather than success.
    pub truncate_stream: bool,
    /// Put the text in `reasoning_content` and leave `content` empty, as a reasoning build
    /// with no parser does.
    pub reasoning: bool,
    /// Reply with the last user message instead of [`Behavior::content`], so a test can
    /// assert on what the upstream actually received.
    pub echo: bool,
    /// What the assistant says.
    pub content: String,
    /// `usage.prompt_tokens`. Defaults to a word count of the request.
    pub prompt_tokens: Option<u32>,
    /// `timings.predicted_per_second`. Deliberately **not** any number this box has ever
    /// measured: a fabricated rate must never be mistakable for a benchmark.
    pub tok_per_s: f32,
    /// Emit a `tool_calls` delta/message instead of content.
    pub tool_call: Option<String>,
}

impl Default for Behavior {
    fn default() -> Self {
        Behavior {
            load_ms: 0,
            refuse_start: false,
            exit_code: 3,
            stall: false,
            never_healthy: false,
            loading_forever: false,
            exit_after_ms: None,
            health_status: None,
            health_hang: false,
            props: Toggle::Auto,
            slots: Toggle::Auto,
            metrics: Toggle::Auto,
            models_endpoint: Toggle::Auto,
            models: Vec::new(),
            slots_total: 0,
            busy_slots: 0,
            ctx: 4096,
            chat_status: None,
            fail_first: 0,
            ttft_ms: 0,
            hang_before_headers: false,
            chunk_ms: 0,
            chunks: 4,
            die_mid_stream: false,
            truncate_stream: false,
            reasoning: false,
            echo: false,
            content: "ok".to_owned(),
            prompt_tokens: None,
            tok_per_s: 12.5,
            tool_call: None,
        }
    }
}

impl Behavior {
    /// Parse `"load_ms=200,reasoning,chunks=8"`. Unknown keys are reported on stderr and
    /// ignored — a typo in a behaviour spec must be visible, but it must not stop a test
    /// binary that is already running.
    pub fn parse(spec: &str) -> Behavior {
        let mut b = Behavior::default();
        b.apply_spec(spec);
        b
    }

    /// Apply a spec on top of this behaviour.
    pub fn apply_spec(&mut self, spec: &str) {
        for item in spec.split(',') {
            let item = item.trim();
            if item.is_empty() {
                continue;
            }
            let (key, value) = match item.split_once('=') {
                Some((k, v)) => (k.trim(), Some(v.trim())),
                None => (item, None),
            };
            if !self.set(key, value) {
                eprintln!("apex-fake: unknown behaviour key `{key}`");
            }
        }
    }

    /// Apply a JSON object, as `POST /_apex/behavior` delivers one. Values may be strings,
    /// numbers or booleans; `true` means "the bare flag".
    pub fn apply_json(&mut self, v: &serde_json::Value) -> Vec<String> {
        let mut unknown = Vec::new();
        let Some(obj) = v.as_object() else {
            return unknown;
        };
        for (k, raw) in obj {
            let rendered = match raw {
                serde_json::Value::Bool(true) => None,
                serde_json::Value::Bool(false) => Some("0".to_owned()),
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Array(items) => Some(
                    items
                        .iter()
                        .map(|i| i.as_str().unwrap_or_default().to_owned())
                        .collect::<Vec<_>>()
                        .join("|"),
                ),
                other => Some(other.to_string()),
            };
            if !self.set(k, rendered.as_deref()) {
                unknown.push(k.clone());
            }
        }
        unknown
    }

    /// One knob. Returns false when the key is not one of ours.
    ///
    /// A bare flag (`value == None`) sets a boolean; `key=0` clears it. Lists are split on
    /// `|` so a spec can survive being comma-separated.
    pub fn set(&mut self, key: &str, value: Option<&str>) -> bool {
        let on = !matches!(value, Some("0") | Some("false") | Some("off") | Some("no"));
        let num = |d: u64| value.and_then(|v| v.parse::<u64>().ok()).unwrap_or(d);
        let n32 = |d: u32| value.and_then(|v| v.parse::<u32>().ok()).unwrap_or(d);
        match key {
            "load_ms" => self.load_ms = num(200),
            "refuse_start" | "fake-exit-early" => self.refuse_start = on,
            "exit_code" => self.exit_code = value.and_then(|v| v.parse().ok()).unwrap_or(3),
            "stall" | "fake-never-healthy" => self.stall = on,
            "never_healthy" => self.never_healthy = on,
            "loading_forever" => self.loading_forever = on,
            "exit_after_ms" => self.exit_after_ms = Some(num(100)),
            "health_status" => self.health_status = value.and_then(|v| v.parse().ok()),
            "health_hang" => self.health_hang = on,
            "props" => self.props = Toggle::parse(value.unwrap_or("on")),
            "slots" => self.slots = Toggle::parse(value.unwrap_or("on")),
            "metrics" => self.metrics = Toggle::parse(value.unwrap_or("on")),
            "models_endpoint" => self.models_endpoint = Toggle::parse(value.unwrap_or("on")),
            "models" => self.models = split_list(value),
            "slots_total" => self.slots_total = n32(1),
            "busy_slots" => self.busy_slots = n32(1),
            "ctx" => self.ctx = n32(4096),
            "chat_status" | "status" => self.chat_status = value.and_then(|v| v.parse().ok()),
            "fail_first" => self.fail_first = n32(1),
            "ttft_ms" | "latency_ms" => self.ttft_ms = num(100),
            "hang_before_headers" | "hang" => self.hang_before_headers = on,
            "chunk_ms" => self.chunk_ms = num(10),
            "chunks" => self.chunks = n32(4),
            "die_mid_stream" => self.die_mid_stream = on,
            "truncate_stream" => self.truncate_stream = on,
            "reasoning" => self.reasoning = on,
            "echo" => self.echo = on,
            "content" => self.content = value.unwrap_or("ok").to_owned(),
            "prompt_tokens" => self.prompt_tokens = value.and_then(|v| v.parse().ok()),
            "tok_per_s" => self.tok_per_s = value.and_then(|v| v.parse().ok()).unwrap_or(12.5),
            "tool_call" => self.tool_call = value.map(str::to_owned),
            _ => return false,
        }
        true
    }

    /// The current settings, for `GET /_apex/behavior`. Enough to tell two fakes apart in
    /// a failing test's output; not a serialisation format.
    pub fn to_json(&self) -> serde_json::Value {
        let mut m = BTreeMap::new();
        m.insert("load_ms", serde_json::json!(self.load_ms));
        m.insert("chunks", serde_json::json!(self.chunks));
        m.insert("chunk_ms", serde_json::json!(self.chunk_ms));
        m.insert("ttft_ms", serde_json::json!(self.ttft_ms));
        m.insert("chat_status", serde_json::json!(self.chat_status));
        m.insert("fail_first", serde_json::json!(self.fail_first));
        m.insert("reasoning", serde_json::json!(self.reasoning));
        m.insert("echo", serde_json::json!(self.echo));
        m.insert("die_mid_stream", serde_json::json!(self.die_mid_stream));
        m.insert("truncate_stream", serde_json::json!(self.truncate_stream));
        m.insert("busy_slots", serde_json::json!(self.busy_slots));
        m.insert("never_healthy", serde_json::json!(self.never_healthy));
        m.insert("content", serde_json::json!(self.content));
        m.insert("models", serde_json::json!(self.models));
        serde_json::json!(m)
    }
}

/// `"a|b|c"` or `"a"` -> `["a","b","c"]`.
fn split_list(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or_default()
        .split('|')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spec_sets_pairs_and_bare_flags() {
        let b = Behavior::parse("load_ms=250,reasoning,chunks=9,content=hello");
        assert_eq!(b.load_ms, 250);
        assert!(b.reasoning);
        assert_eq!(b.chunks, 9);
        assert_eq!(b.content, "hello");
        assert!(!b.die_mid_stream);
    }

    #[test]
    fn a_bare_flag_can_be_turned_off_again() {
        let b = Behavior::parse("reasoning,reasoning=0");
        assert!(!b.reasoning);
    }

    #[test]
    fn toggles_resolve_against_argv_unless_forced() {
        assert!(Toggle::Auto.resolve(true));
        assert!(!Toggle::Auto.resolve(false));
        assert!(Toggle::On.resolve(false));
        assert!(!Toggle::Off.resolve(true));
        assert_eq!(Toggle::parse("off"), Toggle::Off);
        assert_eq!(Toggle::parse("auto"), Toggle::Auto);
    }

    #[test]
    fn json_applies_the_same_knobs_and_names_what_it_did_not_recognise() {
        let mut b = Behavior::default();
        let unknown = b.apply_json(&serde_json::json!({
            "chat_status": 503, "reasoning": true, "wat": 1
        }));
        assert_eq!(b.chat_status, Some(503));
        assert!(b.reasoning);
        assert_eq!(unknown, vec!["wat".to_owned()]);
    }

    #[test]
    fn a_model_list_survives_a_comma_separated_spec() {
        let b = Behavior::parse("models=gpt-4o-mini|carnice-9b");
        assert_eq!(b.models, vec!["gpt-4o-mini", "carnice-9b"]);
    }
}
