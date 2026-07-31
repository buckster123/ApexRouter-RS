//! OWNER: unit R-10 (router/src/anthropic/{mod,translate,sse}.rs). Do not edit outside that
//! unit.
//!
//! **The state machine. This is the unit's main risk.**
//!
//! Anthropic emits *named* SSE events — `message_start`, `content_block_start`,
//! `content_block_delta`, `content_block_stop`, `message_delta`, `message_stop` — carrying
//! explicit content-block **indices** and a final `usage` on `message_delta`. OpenAI emits
//! one delta shape repeatedly and terminates with `data: [DONE]`.
//!
//! Rebuilding that correctly means: opening a block on the first delta of a kind, keeping
//! the index monotonic, closing every opened block exactly once, and emitting
//! `message_delta` with `stop_reason` **and** the final usage before `message_stop`.
//!
//! Chunk boundaries must not be observable: a property test feeds arbitrary splits of the
//! same capture — including splits **inside** a `data:` line — and asserts an identical
//! frame sequence.
//!
//! # How it stays chunk-agnostic
//!
//! [`SseTranslator::feed`] is documented as taking "one OpenAI SSE frame", and a caller that
//! hands it exactly one frame gets exactly that. It is *implemented* against the weaker
//! promise a relay can actually keep: an arbitrary byte slice. Bytes accumulate in an internal
//! buffer and only **complete lines** are interpreted, so a `data:` line split across two TCP
//! reads is reassembled instead of being parsed as two broken ones. That is the whole of why
//! chunk boundaries are unobservable; there is no other trick.
//!
//! SSE is line-oriented, and every OpenAI-dialect server on this request path (llama.cpp,
//! vLLM, Together, OpenAI itself) writes each event's payload as one `data:` line, so the
//! buffer only ever holds one line. A multi-line `data:` field — legal in the SSE spec,
//! emitted by nobody in this ecosystem — would be read as one event per line.
//!
//! # Invariants the tests hold this file to
//!
//! * exactly one `message_start`, and it is first
//! * block indices are `0..n`, with no gaps and no reuse
//! * every `content_block_start` has exactly one matching `content_block_stop`
//! * no `content_block_delta` for an index that is not currently open
//! * exactly one `message_delta`, carrying `stop_reason` **and** usage, then exactly one
//!   `message_stop`, and nothing after it
//! * all of the above whether the stream ended with `data: [DONE]`, with a bare EOF
//!   ([`SseTranslator::finish`]), or with an upstream error frame

use bytes::Bytes;
use serde_json::{json, Value};

use super::translate::map_stop_reason_to_anthropic;

/// How much of an unterminated line the buffer will hold before giving up on it.
///
/// A `data:` line is one JSON object; llama.cpp's largest is a few hundred bytes and a
/// tool-call argument burst is a few kilobytes. 4 MiB is far past any honest frame, and
/// exists so that a wedged or hostile upstream cannot grow this buffer without bound.
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

/// What kind of Anthropic content block is currently open.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum BlockKind {
    /// `{"type":"text"}`, fed by `delta.content`.
    Text,
    /// `{"type":"tool_use"}`, fed by `delta.tool_calls[].function.arguments`.
    ToolUse,
}

/// A tool call the upstream has started, keyed by its OpenAI `tool_calls[].index`.
#[derive(Clone, Debug)]
struct ToolSlot {
    /// The `tool_calls[].index` the upstream uses for it.
    upstream_index: u64,
    /// `id`, echoed into the Anthropic `tool_use` block.
    id: String,
    /// `function.name`, echoed into the Anthropic `tool_use` block.
    name: String,
    /// Which Anthropic block index it is currently open at, if it is open.
    open_at: Option<usize>,
}

/// Translates one OpenAI SSE stream into one Anthropic SSE stream.
#[derive(Debug)]
pub struct SseTranslator {
    /// The model echoed in `message_start` when the upstream chunk does not name one.
    model: String,
    /// `message_start` has been emitted.
    started: bool,
    /// The closing frames have been emitted; everything after them is ignored.
    finished: bool,
    /// The next content-block index to hand out. Monotonic, never reused.
    next_index: usize,
    /// The one block that is open, if any — Anthropic's blocks are sequential.
    open: Option<(usize, BlockKind)>,
    /// Tool calls seen so far, in upstream order.
    tools: Vec<ToolSlot>,
    /// `stop_reason`, already in the Anthropic spelling.
    stop_reason: Option<&'static str>,
    /// `usage.prompt_tokens` as last reported.
    input_tokens: u64,
    /// `usage.completion_tokens` as last reported.
    output_tokens: u64,
    /// The `msg_`-prefixed id, taken from the first chunk that carries one.
    message_id: Option<String>,
    /// Bytes of an incomplete trailing line.
    buf: Vec<u8>,
    /// How many `delta.reasoning_content` deltas went by unmapped.
    reasoning_deltas: u64,
}

impl SseTranslator {
    /// Start a translation for one response. `model` is echoed in `message_start`.
    pub fn new(model: String) -> Self {
        SseTranslator {
            model,
            started: false,
            finished: false,
            next_index: 0,
            open: None,
            tools: Vec::new(),
            stop_reason: None,
            input_tokens: 0,
            output_tokens: 0,
            message_id: None,
            buf: Vec::new(),
            reasoning_deltas: 0,
        }
    }

    /// Feed one OpenAI SSE frame; get back zero or more Anthropic frames, already framed
    /// `event: <name>\ndata: <json>\n\n`. `data: [DONE]` yields the closing frames.
    ///
    /// A *partial* frame is legal input: bytes are buffered and only complete lines are
    /// interpreted, so the caller may split the upstream stream anywhere at all.
    pub fn feed(&mut self, frame: &[u8]) -> Vec<Bytes> {
        let mut out = Vec::new();
        self.buf.extend_from_slice(frame);

        if let Some(last_newline) = self.buf.iter().rposition(|b| *b == b'\n') {
            let tail = self.buf.split_off(last_newline + 1);
            let complete = std::mem::replace(&mut self.buf, tail);
            for line in complete.split(|b| *b == b'\n') {
                self.line(line, &mut out);
            }
        }

        if self.buf.len() > MAX_LINE_BYTES {
            tracing::warn!(
                bytes = self.buf.len(),
                "anthropic ingress: dropping an SSE line with no terminator inside the cap"
            );
            self.buf.clear();
        }
        out
    }

    /// Upstream ended without `[DONE]`: close every open block **honestly**, then
    /// `message_stop`. Never a truncated block, never a dangling index.
    ///
    /// A trailing line the upstream never terminated is interpreted first — a socket that
    /// closed after the last byte of a valid payload but before its newline still carries a
    /// real delta, and dropping it would lose the end of the answer.
    pub fn finish(mut self) -> Vec<Bytes> {
        let mut out = Vec::new();
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            self.line(&line, &mut out);
        }
        self.close(&mut out);
        out
    }

    // ---- parsing ---------------------------------------------------------------------------------

    /// Interpret one complete SSE line.
    fn line(&mut self, line: &[u8], out: &mut Vec<Bytes>) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        // Blank separators and `: keep-alive` comments carry nothing to translate. Nor do
        // `event:`/`id:`/`retry:` lines: no OpenAI-dialect server names its events, and if
        // one starts, the payload is still on the `data:` line.
        let Some(payload) = line.strip_prefix(b"data:") else {
            return;
        };
        let payload = trim_ascii(payload);
        if payload.is_empty() {
            return;
        }
        if payload == b"[DONE]" {
            self.close(out);
            return;
        }
        match serde_json::from_slice::<Value>(payload) {
            Ok(v) => self.chunk(&v, out),
            Err(e) => tracing::debug!(
                error = %e,
                "anthropic ingress: skipping an SSE payload that is not JSON"
            ),
        }
    }

    /// Interpret one decoded OpenAI chunk.
    fn chunk(&mut self, v: &Value, out: &mut Vec<Bytes>) {
        if self.finished {
            return;
        }

        // An upstream that fails mid-stream sends its error as a data frame. Anthropic has a
        // named `error` event for exactly this; the stream is then closed properly rather
        // than left with a block hanging open.
        if let Some(err) = v.get("error") {
            self.start(v, out);
            let message = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("the upstream reported an error mid-stream");
            out.push(frame(
                "error",
                &json!({"type":"error","error":{"type":"api_error","message":message}}),
            ));
            self.close(out);
            return;
        }

        self.start(v, out);
        self.take_usage(v.get("usage"));

        let Some(choices) = v.get("choices").and_then(Value::as_array) else {
            return;
        };
        for choice in choices {
            // The Messages API has no `n`; only the first choice is representable.
            if choice.get("index").and_then(Value::as_u64).unwrap_or(0) != 0 {
                continue;
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                self.stop_reason = Some(map_stop_reason_to_anthropic(reason));
            }
            let Some(delta) = choice.get("delta") else {
                continue;
            };
            if let Some(r) = delta.get("reasoning_content").and_then(Value::as_str) {
                if !r.is_empty() {
                    self.reasoning_deltas += 1;
                }
            }
            if let Some(text) = delta.get("content").and_then(Value::as_str) {
                if !text.is_empty() {
                    self.text_delta(text, out);
                }
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    self.tool_delta(call, out);
                }
            }
        }
    }

    /// Record whichever token counts the upstream reported. Renamed, never recomputed.
    fn take_usage(&mut self, usage: Option<&Value>) {
        let Some(usage) = usage else { return };
        if let Some(n) = usage.get("prompt_tokens").and_then(Value::as_u64) {
            self.input_tokens = n;
        }
        if let Some(n) = usage.get("completion_tokens").and_then(Value::as_u64) {
            self.output_tokens = n;
        }
    }

    // ---- emitting --------------------------------------------------------------------------------

    /// Emit `message_start`, exactly once, from the first chunk that arrives.
    fn start(&mut self, v: &Value, out: &mut Vec<Bytes>) {
        if self.started {
            return;
        }
        self.started = true;
        let id = self.message_id.get_or_insert_with(|| {
            let upstream = v.get("id").and_then(Value::as_str).unwrap_or_default();
            format!("msg_{upstream}")
        });
        let model = v
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(self.model.as_str());
        out.push(frame(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": id,
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "content": [],
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    // The prompt count is not known until the upstream reports it, which for
                    // an OpenAI stream is the final usage chunk. Reported as 0 here and
                    // corrected on `message_delta`; never guessed from the bytes seen.
                    "usage": {"input_tokens": self.input_tokens, "output_tokens": 0},
                }
            }),
        ));
    }

    /// One `delta.content` chunk, opening a text block if one is not already open.
    fn text_delta(&mut self, text: &str, out: &mut Vec<Bytes>) {
        if self.open.map(|(_, kind)| kind) != Some(BlockKind::Text) {
            self.close_block(out);
            let index = self.open_block(out, &json!({"type": "text", "text": ""}));
            self.open = Some((index, BlockKind::Text));
        }
        let Some((index, _)) = self.open else { return };
        out.push(frame(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "text_delta", "text": text},
            }),
        ));
    }

    /// One `delta.tool_calls[]` chunk: open the matching `tool_use` block on first sight,
    /// then stream its arguments as `input_json_delta`.
    fn tool_delta(&mut self, call: &Value, out: &mut Vec<Bytes>) {
        let upstream_index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
        let id = call.get("id").and_then(Value::as_str);
        let name = call
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str);

        let slot = match self
            .tools
            .iter()
            .position(|s| s.upstream_index == upstream_index)
        {
            Some(i) => i,
            None => {
                self.tools.push(ToolSlot {
                    upstream_index,
                    id: id.unwrap_or_default().to_owned(),
                    name: name.unwrap_or_default().to_owned(),
                    open_at: None,
                });
                self.tools.len() - 1
            }
        };
        // A late `id`/`name` on a slot that started without one still belongs to it.
        if let Some(id) = id.filter(|s| !s.is_empty()) {
            self.tools[slot].id = id.to_owned();
        }
        if let Some(name) = name.filter(|s| !s.is_empty()) {
            self.tools[slot].name = name.to_owned();
        }

        // Open a block for this call unless it is the one already open. A call the upstream
        // returns to after starting another gets a NEW block rather than a delta on a closed
        // index: the indices stay honest and nothing is dropped.
        let already_open = self.tools[slot]
            .open_at
            .is_some_and(|at| self.open == Some((at, BlockKind::ToolUse)));
        if !already_open {
            self.close_block(out);
            let block = json!({
                "type": "tool_use",
                "id": self.tools[slot].id,
                "name": self.tools[slot].name,
                "input": {},
            });
            let index = self.open_block(out, &block);
            self.open = Some((index, BlockKind::ToolUse));
            self.tools[slot].open_at = Some(index);
        }

        let args = call
            .get("function")
            .and_then(|f| f.get("arguments"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if args.is_empty() {
            return;
        }
        let Some((index, _)) = self.open else { return };
        out.push(frame(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "input_json_delta", "partial_json": args},
            }),
        ));
    }

    /// Emit `content_block_start` at the next index, and hand that index back.
    fn open_block(&mut self, out: &mut Vec<Bytes>, block: &Value) -> usize {
        let index = self.next_index;
        self.next_index += 1;
        out.push(frame(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": index,
                "content_block": block,
            }),
        ));
        index
    }

    /// Close whatever block is open. Idempotent, so no index is ever stopped twice.
    fn close_block(&mut self, out: &mut Vec<Bytes>) {
        let Some((index, _)) = self.open.take() else {
            return;
        };
        for slot in &mut self.tools {
            if slot.open_at == Some(index) {
                slot.open_at = None;
            }
        }
        out.push(frame(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": index}),
        ));
    }

    /// The closing sequence: close the open block, then `message_delta`, then `message_stop`.
    /// Runs at most once per stream, whichever way the stream ended.
    fn close(&mut self, out: &mut Vec<Bytes>) {
        if self.finished {
            return;
        }
        self.finished = true;
        // Even a stream that died before its first byte gets a well-formed message rather
        // than a bare `message_stop` an SDK cannot attach to anything.
        if !self.started {
            self.start(&Value::Null, out);
        }
        self.close_block(out);

        if self.reasoning_deltas > 0 {
            tracing::debug!(
                deltas = self.reasoning_deltas,
                "anthropic ingress: upstream reasoning_content has no Anthropic spelling in mk1 \
                 and was not mapped to a thinking block"
            );
        }

        out.push(frame(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {
                    // `null` when the upstream died before saying why. Claiming `end_turn`
                    // for a truncated stream would be a lie the client cannot detect.
                    "stop_reason": self.stop_reason.map_or(Value::Null, |s| json!(s)),
                    "stop_sequence": Value::Null,
                },
                "usage": {
                    "input_tokens": self.input_tokens,
                    "output_tokens": self.output_tokens,
                },
            }),
        ));
        out.push(frame("message_stop", &json!({"type": "message_stop"})));
    }
}

/// `event: <name>\ndata: <json>\n\n`, the wire form of one Anthropic SSE frame.
fn frame(event: &str, data: &Value) -> Bytes {
    // Serialising an object `json!` just built cannot fail; the fallback keeps the
    // no-`unwrap` rule without pretending the branch is reachable.
    let body = serde_json::to_string(data).unwrap_or_else(|_| String::from("{}"));
    Bytes::from(format!("event: {event}\ndata: {body}\n\n"))
}

/// `<[u8]>::trim_ascii` without depending on the MSRV that stabilised it.
fn trim_ascii(mut s: &[u8]) -> &[u8] {
    while let Some((first, rest)) = s.split_first() {
        if first.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    while let Some((last, rest)) = s.split_last() {
        if last.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    // ==============================================================================================
    // Recorded captures.
    //
    // Every byte below came off `llama-server` version `b9199 (39cf5d619)` from
    // `~/llama.cpp/build-vulkan`, serving `/home/andre/models/carnice-9b/Carnice-9b-Q6_K.gguf`
    // on 127.0.0.1 with `stream_options.include_usage`. Frames were *selected*, never written:
    // the long reasoning runs are truncated to a couple of representatives and nothing else is
    // edited. Hand-writing these would have hidden the two facts that actually shaped this
    // file — that llama.cpp puts a reasoning model's chain of thought in
    // `delta.reasoning_content`, and that the final usage arrives on a chunk whose `choices`
    // array is EMPTY, one frame before `[DONE]`.
    // ==============================================================================================

    /// Plain streaming text: role chunk, six content deltas, `finish_reason: "stop"`, the
    /// usage-only chunk, `[DONE]`.
    const CAPTURE_TEXT: &str = r#"data: {"choices":[{"finish_reason":null,"index":0,"delta":{"role":"assistant","content":null}}],"created":1785448607,"id":"chatcmpl-zwksN84YFlRuJk31X4rVCZ4aMRLGShAU","model":"carnice","system_fingerprint":"b9199-39cf5d619","object":"chat.completion.chunk"}

data: {"choices":[{"finish_reason":null,"index":0,"delta":{"content":"Here"}}],"created":1785448607,"id":"chatcmpl-zwksN84YFlRuJk31X4rVCZ4aMRLGShAU","model":"carnice","system_fingerprint":"b9199-39cf5d619","object":"chat.completion.chunk"}

data: {"choices":[{"finish_reason":null,"index":0,"delta":{"content":" are"}}],"created":1785448607,"id":"chatcmpl-zwksN84YFlRuJk31X4rVCZ4aMRLGShAU","model":"carnice","system_fingerprint":"b9199-39cf5d619","object":"chat.completion.chunk"}

data: {"choices":[{"finish_reason":null,"index":0,"delta":{"content":" the"}}],"created":1785448607,"id":"chatcmpl-zwksN84YFlRuJk31X4rVCZ4aMRLGShAU","model":"carnice","system_fingerprint":"b9199-39cf5d619","object":"chat.completion.chunk"}

data: {"choices":[{"finish_reason":null,"index":0,"delta":{"content":" words"}}],"created":1785448607,"id":"chatcmpl-zwksN84YFlRuJk31X4rVCZ4aMRLGShAU","model":"carnice","system_fingerprint":"b9199-39cf5d619","object":"chat.completion.chunk"}

data: {"choices":[{"finish_reason":null,"index":0,"delta":{"content":" as"}}],"created":1785448608,"id":"chatcmpl-zwksN84YFlRuJk31X4rVCZ4aMRLGShAU","model":"carnice","system_fingerprint":"b9199-39cf5d619","object":"chat.completion.chunk"}

data: {"choices":[{"finish_reason":null,"index":0,"delta":{"content":" requested"}}],"created":1785448608,"id":"chatcmpl-zwksN84YFlRuJk31X4rVCZ4aMRLGShAU","model":"carnice","system_fingerprint":"b9199-39cf5d619","object":"chat.completion.chunk"}

data: {"choices":[{"finish_reason":"stop","index":0,"delta":{}}],"created":1785448609,"id":"chatcmpl-zwksN84YFlRuJk31X4rVCZ4aMRLGShAU","model":"carnice","system_fingerprint":"b9199-39cf5d619","object":"chat.completion.chunk"}

data: {"choices":[],"created":1785448609,"id":"chatcmpl-zwksN84YFlRuJk31X4rVCZ4aMRLGShAU","model":"carnice","system_fingerprint":"b9199-39cf5d619","object":"chat.completion.chunk","usage":{"completion_tokens":17,"prompt_tokens":22,"total_tokens":39,"prompt_tokens_details":{"cached_tokens":0}},"timings":{"cache_n":0,"prompt_n":22,"prompt_ms":576.053,"predicted_n":17,"predicted_ms":1645.739,"predicted_per_second":10.329705986186145}}

data: [DONE]

"#;

    /// The same model reasoning first: two `reasoning_content` deltas, then the one visible
    /// content delta. The untruncated capture had 190 of the former and 1 of the latter.
    const CAPTURE_REASONING: &str = r#"data: {"choices":[{"finish_reason":null,"index":0,"delta":{"role":"assistant","content":null}}],"created":1785448442,"id":"chatcmpl-cfGK1VC1y97JcvhS1lHUS9ha38foUiZO","model":"carnice","system_fingerprint":"b9199-39cf5d619","object":"chat.completion.chunk"}

data: {"choices":[{"finish_reason":null,"index":0,"delta":{"reasoning_content":"Thinking"}}],"created":1785448442,"id":"chatcmpl-cfGK1VC1y97JcvhS1lHUS9ha38foUiZO","model":"carnice","system_fingerprint":"b9199-39cf5d619","object":"chat.completion.chunk"}

data: {"choices":[{"finish_reason":null,"index":0,"delta":{"reasoning_content":" Process"}}],"created":1785448442,"id":"chatcmpl-cfGK1VC1y97JcvhS1lHUS9ha38foUiZO","model":"carnice","system_fingerprint":"b9199-39cf5d619","object":"chat.completion.chunk"}

data: {"choices":[{"finish_reason":null,"index":0,"delta":{"content":"OK"}}],"created":1785448462,"id":"chatcmpl-cfGK1VC1y97JcvhS1lHUS9ha38foUiZO","model":"carnice","system_fingerprint":"b9199-39cf5d619","object":"chat.completion.chunk"}

data: {"choices":[{"finish_reason":"stop","index":0,"delta":{}}],"created":1785448462,"id":"chatcmpl-cfGK1VC1y97JcvhS1lHUS9ha38foUiZO","model":"carnice","system_fingerprint":"b9199-39cf5d619","object":"chat.completion.chunk"}

data: {"choices":[],"created":1785448462,"id":"chatcmpl-cfGK1VC1y97JcvhS1lHUS9ha38foUiZO","model":"carnice","system_fingerprint":"b9199-39cf5d619","object":"chat.completion.chunk","usage":{"completion_tokens":191,"prompt_tokens":15,"total_tokens":206,"prompt_tokens_details":{"cached_tokens":0}},"timings":{"predicted_per_second":9.180658880025948}}

data: [DONE]

"#;

    /// A streaming tool call: the first `tool_calls` chunk carries `id`, `type` and `name`
    /// alongside the opening brace, and the arguments arrive a fragment at a time.
    const CAPTURE_TOOL: &str = r#"data: {"choices":[{"finish_reason":null,"index":0,"delta":{"role":"assistant","content":null}}],"created":1785448476,"id":"chatcmpl-I4WgrztxOTEX0xEmPSlDiBPzNt7AelGx","model":"carnice","system_fingerprint":"b9199-39cf5d619","object":"chat.completion.chunk"}

data: {"choices":[{"finish_reason":null,"index":0,"delta":{"reasoning_content":"The"}}],"created":1785448476,"id":"chatcmpl-I4WgrztxOTEX0xEmPSlDiBPzNt7AelGx","model":"carnice","system_fingerprint":"b9199-39cf5d619","object":"chat.completion.chunk"}

data: {"choices":[{"finish_reason":null,"index":0,"delta":{"tool_calls":[{"index":0,"id":"MXwBDrauDogFz0tKPbvYoNN7HWXYmtSt","type":"function","function":{"name":"get_weather","arguments":"{"}}]}}],"created":1785448482,"id":"chatcmpl-I4WgrztxOTEX0xEmPSlDiBPzNt7AelGx","model":"carnice","system_fingerprint":"b9199-39cf5d619","object":"chat.completion.chunk"}

data: {"choices":[{"finish_reason":null,"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"location\":\""}}]}}],"created":1785448482,"id":"chatcmpl-I4WgrztxOTEX0xEmPSlDiBPzNt7AelGx","model":"carnice","system_fingerprint":"b9199-39cf5d619","object":"chat.completion.chunk"}

data: {"choices":[{"finish_reason":null,"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"Paris"}}]}}],"created":1785448482,"id":"chatcmpl-I4WgrztxOTEX0xEmPSlDiBPzNt7AelGx","model":"carnice","system_fingerprint":"b9199-39cf5d619","object":"chat.completion.chunk"}

data: {"choices":[{"finish_reason":null,"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\""}}]}}],"created":1785448483,"id":"chatcmpl-I4WgrztxOTEX0xEmPSlDiBPzNt7AelGx","model":"carnice","system_fingerprint":"b9199-39cf5d619","object":"chat.completion.chunk"}

data: {"choices":[{"finish_reason":null,"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"}"}}]}}],"created":1785448483,"id":"chatcmpl-I4WgrztxOTEX0xEmPSlDiBPzNt7AelGx","model":"carnice","system_fingerprint":"b9199-39cf5d619","object":"chat.completion.chunk"}

data: {"choices":[{"finish_reason":"tool_calls","index":0,"delta":{}}],"created":1785448484,"id":"chatcmpl-I4WgrztxOTEX0xEmPSlDiBPzNt7AelGx","model":"carnice","system_fingerprint":"b9199-39cf5d619","object":"chat.completion.chunk"}

data: {"choices":[],"created":1785448484,"id":"chatcmpl-I4WgrztxOTEX0xEmPSlDiBPzNt7AelGx","model":"carnice","system_fingerprint":"b9199-39cf5d619","object":"chat.completion.chunk","usage":{"completion_tokens":67,"prompt_tokens":272,"total_tokens":339,"prompt_tokens_details":{"cached_tokens":0}},"timings":{"predicted_per_second":9.451881178285614}}

data: [DONE]

"#;

    // ---- harness -----------------------------------------------------------------------------------

    /// One translated frame, split back into its event name and decoded payload.
    #[derive(Debug, Clone, PartialEq)]
    struct Frame {
        event: String,
        data: Value,
    }

    fn parse(frames: &[Bytes]) -> Vec<Frame> {
        frames
            .iter()
            .map(|b| {
                let s = std::str::from_utf8(b).expect("utf8");
                assert!(
                    s.ends_with("\n\n"),
                    "every frame is blank-line terminated: {s:?}"
                );
                let (head, rest) = s.split_once('\n').expect("event line");
                let event = head
                    .strip_prefix("event: ")
                    .expect("event: prefix")
                    .to_owned();
                let body = rest
                    .strip_prefix("data: ")
                    .expect("data: prefix")
                    .trim_end();
                Frame {
                    event,
                    data: serde_json::from_str(body).expect("frame payload is json"),
                }
            })
            .collect()
    }

    /// Feed a whole capture in one go, then finish.
    fn translate_all(capture: &str) -> Vec<Frame> {
        let mut t = SseTranslator::new("carnice".to_owned());
        let mut raw = t.feed(capture.as_bytes());
        raw.extend(t.finish());
        parse(&raw)
    }

    /// Feed a capture in chunks of exactly `n` bytes.
    fn translate_in_chunks(capture: &str, n: usize) -> Vec<Frame> {
        let mut t = SseTranslator::new("carnice".to_owned());
        let mut raw = Vec::new();
        for piece in capture.as_bytes().chunks(n) {
            raw.extend(t.feed(piece));
        }
        raw.extend(t.finish());
        parse(&raw)
    }

    fn events(frames: &[Frame]) -> Vec<&str> {
        frames.iter().map(|f| f.event.as_str()).collect()
    }

    /// Every structural invariant in the module doc, as one pass over the output.
    fn assert_well_formed(frames: &[Frame]) {
        assert_eq!(
            frames.iter().filter(|f| f.event == "message_start").count(),
            1,
            "exactly one message_start"
        );
        assert_eq!(
            frames.first().map(|f| f.event.as_str()),
            Some("message_start")
        );
        assert_eq!(
            frames.last().map(|f| f.event.as_str()),
            Some("message_stop")
        );
        assert_eq!(
            frames.iter().filter(|f| f.event == "message_stop").count(),
            1,
            "exactly one message_stop"
        );
        assert_eq!(
            frames.iter().filter(|f| f.event == "message_delta").count(),
            1,
            "exactly one message_delta"
        );

        let mut open: Option<u64> = None;
        let mut opened: Vec<u64> = Vec::new();
        let mut closed: Vec<u64> = Vec::new();
        for f in frames {
            let index = f.data.get("index").and_then(Value::as_u64);
            match f.event.as_str() {
                "content_block_start" => {
                    let i = index.expect("content_block_start carries an index");
                    assert!(
                        open.is_none(),
                        "block {i} opened while {open:?} was still open"
                    );
                    assert!(!opened.contains(&i), "index {i} opened twice");
                    opened.push(i);
                    open = Some(i);
                }
                "content_block_delta" => {
                    let i = index.expect("content_block_delta carries an index");
                    assert_eq!(open, Some(i), "delta for an index that is not open: {i}");
                }
                "content_block_stop" => {
                    let i = index.expect("content_block_stop carries an index");
                    assert_eq!(open, Some(i), "stop for an index that is not open: {i}");
                    closed.push(i);
                    open = None;
                }
                "message_delta" | "message_stop" => {
                    assert!(open.is_none(), "a block was still open at {}", f.event);
                }
                _ => {}
            }
        }
        assert_eq!(open, None, "a block was left open");
        assert_eq!(opened, closed, "every opened block is closed exactly once");
        let want: Vec<u64> = (0..opened.len() as u64).collect();
        assert_eq!(opened, want, "indices are 0..n with no gaps");
    }

    // ---- stage 2: streaming text ---------------------------------------------------------------------

    #[test]
    fn a_recorded_text_stream_produces_the_documented_event_sequence() {
        let frames = translate_all(CAPTURE_TEXT);
        assert_well_formed(&frames);
        assert_eq!(
            events(&frames),
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_delta",
                "content_block_delta",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );

        let start = &frames[0].data["message"];
        assert_eq!(start["id"], "msg_chatcmpl-zwksN84YFlRuJk31X4rVCZ4aMRLGShAU");
        assert_eq!(start["role"], "assistant");
        assert_eq!(start["model"], "carnice");

        assert_eq!(frames[1].data["content_block"]["type"], "text");
        assert_eq!(frames[1].data["index"], 0);
        assert_eq!(frames[2].data["delta"]["type"], "text_delta");

        let text: String = frames
            .iter()
            .filter(|f| f.event == "content_block_delta")
            .filter_map(|f| f.data["delta"]["text"].as_str())
            .collect();
        assert_eq!(text, "Here are the words as requested");
    }

    #[test]
    fn message_delta_carries_the_stop_reason_and_the_final_usage() {
        let frames = translate_all(CAPTURE_TEXT);
        let delta = frames
            .iter()
            .find(|f| f.event == "message_delta")
            .expect("message_delta");
        assert_eq!(delta.data["delta"]["stop_reason"], "end_turn");
        assert!(delta.data["delta"]["stop_sequence"].is_null());
        // The usage-only chunk one frame before [DONE] is where these come from.
        assert_eq!(delta.data["usage"]["input_tokens"], 22);
        assert_eq!(delta.data["usage"]["output_tokens"], 17);
    }

    #[test]
    fn reasoning_deltas_open_no_block_and_are_not_mapped() {
        let frames = translate_all(CAPTURE_REASONING);
        assert_well_formed(&frames);
        // Two reasoning deltas and one content delta went in; exactly one text block and one
        // delta come out. A `thinking` block here would 400 on the client's next turn.
        assert_eq!(
            events(&frames),
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(frames[1].data["content_block"]["type"], "text");
        assert_eq!(frames[2].data["delta"]["text"], "OK");
    }

    // ---- the property: chunk boundaries are not observable -----------------------------------------------

    #[test]
    fn arbitrary_chunk_splits_yield_an_identical_frame_sequence() {
        for capture in [CAPTURE_TEXT, CAPTURE_REASONING, CAPTURE_TOOL] {
            let want = translate_all(capture);
            // Sizes chosen to land inside a `data:` line, inside the JSON, on a frame
            // boundary, and (1) between every single byte.
            for n in [1, 2, 3, 7, 13, 33, 64, 127, 256, 999, 4096] {
                let got = translate_in_chunks(capture, n);
                assert_eq!(got, want, "chunk size {n} changed the output");
            }
        }
    }

    #[test]
    fn a_split_inside_a_data_line_is_reassembled_not_parsed_twice() {
        let want = translate_all(CAPTURE_TEXT);
        let bytes = CAPTURE_TEXT.as_bytes();
        // A deterministic walk of split points, most of which fall inside the JSON of a
        // `data:` line rather than on any frame boundary.
        let mut seed: usize = 0x5eed;
        for _ in 0..64 {
            let mut t = SseTranslator::new("carnice".to_owned());
            let mut raw = Vec::new();
            let mut at = 0;
            let mut local = seed;
            while at < bytes.len() {
                // xorshift, so the split points are reproducible from the seed.
                local ^= local << 13;
                local ^= local >> 7;
                local ^= local << 17;
                let take = (local % 97 + 1).min(bytes.len() - at);
                raw.extend(t.feed(&bytes[at..at + take]));
                at += take;
            }
            raw.extend(t.finish());
            assert_eq!(parse(&raw), want, "seed {seed} changed the output");
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        }
    }

    #[test]
    fn a_frame_delivered_one_byte_at_a_time_emits_nothing_until_it_is_complete() {
        let mut t = SseTranslator::new("carnice".to_owned());
        let line = br#"data: {"choices":[{"index":0,"delta":{"content":"hi"}}],"id":"c"}"#;
        for b in line {
            assert!(
                t.feed(&[*b]).is_empty(),
                "no frame may escape before the line terminator"
            );
        }
        let out = t.feed(b"\n");
        let frames = parse(&out);
        assert_eq!(
            events(&frames),
            [
                "message_start",
                "content_block_start",
                "content_block_delta"
            ]
        );
    }

    // ---- death mid-stream ------------------------------------------------------------------------------

    #[test]
    fn an_upstream_that_dies_mid_block_closes_it_and_stops_cleanly() {
        let mut t = SseTranslator::new("carnice".to_owned());
        // Everything up to and including the third content delta, then the socket dies.
        let cut = CAPTURE_TEXT
            .match_indices("\n\n")
            .nth(3)
            .map(|(i, _)| i + 2)
            .expect("four frames");
        let mut raw = t.feed(&CAPTURE_TEXT.as_bytes()[..cut]);
        raw.extend(t.finish());
        let frames = parse(&raw);

        assert_well_formed(&frames);
        assert_eq!(
            events(&frames),
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        let delta = &frames[frames.len() - 2].data;
        assert!(
            delta["delta"]["stop_reason"].is_null(),
            "a truncated stream never claims end_turn"
        );
    }

    #[test]
    fn a_stream_that_dies_before_its_first_byte_still_produces_a_whole_message() {
        let t = SseTranslator::new("carnice".to_owned());
        let frames = parse(&t.finish());
        assert_eq!(
            events(&frames),
            ["message_start", "message_delta", "message_stop"]
        );
        assert_eq!(frames[0].data["message"]["model"], "carnice");
        assert_eq!(frames[0].data["message"]["id"], "msg_");
    }

    #[test]
    fn a_final_line_with_no_newline_is_still_interpreted() {
        let mut t = SseTranslator::new("carnice".to_owned());
        let mut raw =
            t.feed(br#"data: {"id":"c","choices":[{"index":0,"delta":{"content":"x"}}]}"#);
        assert!(raw.is_empty());
        raw.extend(t.finish());
        let frames = parse(&raw);
        assert_well_formed(&frames);
        assert_eq!(frames[2].data["delta"]["text"], "x");
    }

    #[test]
    fn nothing_is_emitted_after_done() {
        let mut t = SseTranslator::new("carnice".to_owned());
        let _ = t.feed(CAPTURE_TEXT.as_bytes());
        let after = t.feed(
            b"data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"late\"}}]}\n\n",
        );
        assert!(after.is_empty(), "a frame after [DONE] is ignored");
        assert!(t.finish().is_empty(), "and finish() adds nothing either");
    }

    #[test]
    fn an_upstream_error_frame_becomes_an_error_event_and_a_clean_close() {
        let mut t = SseTranslator::new("carnice".to_owned());
        let mut raw = t.feed(
            b"data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"par\"}}]}\n\n",
        );
        raw.extend(t.feed(b"data: {\"error\":{\"message\":\"context shift disabled\"}}\n\n"));
        raw.extend(t.finish());
        let frames = parse(&raw);

        let err = frames.iter().find(|f| f.event == "error").expect("error");
        assert_eq!(err.data["error"]["message"], "context shift disabled");
        // The open text block is still closed and the message still ends properly.
        assert_eq!(
            events(&frames),
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "error",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
    }

    #[test]
    fn garbage_between_good_frames_is_skipped_not_fatal() {
        let mut t = SseTranslator::new("carnice".to_owned());
        let mut raw = t.feed(b": keep-alive\n\nevent: something\n\n");
        raw.extend(t.feed(b"data: {not json at all\n\n"));
        raw.extend(t.feed(
            b"data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"}}]}\n\n",
        ));
        raw.extend(t.feed(b"data: [DONE]\n\n"));
        let frames = parse(&raw);
        assert_well_formed(&frames);
        assert_eq!(frames[2].data["delta"]["text"], "ok");
    }

    // ---- stage 3: streaming tool use -------------------------------------------------------------------

    #[test]
    fn a_recorded_tool_stream_becomes_one_tool_use_block_with_input_json_deltas() {
        let frames = translate_all(CAPTURE_TOOL);
        assert_well_formed(&frames);
        assert_eq!(
            events(&frames),
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_delta",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );

        let start = &frames[1].data;
        assert_eq!(start["index"], 0);
        assert_eq!(start["content_block"]["type"], "tool_use");
        assert_eq!(
            start["content_block"]["id"],
            "MXwBDrauDogFz0tKPbvYoNN7HWXYmtSt"
        );
        assert_eq!(start["content_block"]["name"], "get_weather");
        assert_eq!(start["content_block"]["input"], json!({}));

        let partial: String = frames
            .iter()
            .filter(|f| f.event == "content_block_delta")
            .map(|f| {
                assert_eq!(f.data["delta"]["type"], "input_json_delta");
                f.data["delta"]["partial_json"].as_str().unwrap_or_default()
            })
            .collect();
        assert_eq!(partial, r#"{"location":"Paris"}"#);
        let reassembled: Value = serde_json::from_str(&partial).expect("valid JSON once joined");
        assert_eq!(reassembled["location"], "Paris");

        let delta = &frames[frames.len() - 2].data;
        assert_eq!(delta["delta"]["stop_reason"], "tool_use");
        assert_eq!(delta["usage"]["input_tokens"], 272);
        assert_eq!(delta["usage"]["output_tokens"], 67);
    }

    #[test]
    fn text_then_a_tool_call_closes_the_text_block_before_opening_the_tool_block() {
        let mut t = SseTranslator::new("m".to_owned());
        let mut raw = t.feed(
            b"data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"one moment\"}}]}\n\n",
        );
        raw.extend(t.feed(
            br#"data: {"id":"c","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"t1","type":"function","function":{"name":"f","arguments":"{}"}}]}}]}"#,
        ));
        raw.extend(t.feed(b"\n\ndata: [DONE]\n\n"));
        let frames = parse(&raw);
        assert_well_formed(&frames);
        assert_eq!(
            events(&frames),
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(frames[1].data["content_block"]["type"], "text");
        assert_eq!(frames[4].data["content_block"]["type"], "tool_use");
        assert_eq!(frames[4].data["index"], 1);
    }

    #[test]
    fn two_parallel_tool_calls_get_two_blocks_with_monotonic_indices() {
        let mut t = SseTranslator::new("m".to_owned());
        let mut raw = t.feed(
            br#"data: {"id":"c","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"a","type":"function","function":{"name":"f","arguments":"{\"x\":1}"}}]}}]}
"#,
        );
        raw.extend(t.feed(
            br#"data: {"id":"c","choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"b","type":"function","function":{"name":"g","arguments":"{\"y\":2}"}}]}}]}
"#,
        ));
        raw.extend(t.feed(b"data: [DONE]\n"));
        let frames = parse(&raw);
        assert_well_formed(&frames);

        let starts: Vec<&Frame> = frames
            .iter()
            .filter(|f| f.event == "content_block_start")
            .collect();
        assert_eq!(starts.len(), 2);
        assert_eq!(starts[0].data["index"], 0);
        assert_eq!(starts[0].data["content_block"]["id"], "a");
        assert_eq!(starts[1].data["index"], 1);
        assert_eq!(starts[1].data["content_block"]["id"], "b");
    }

    #[test]
    fn an_upstream_returning_to_an_earlier_tool_index_opens_a_new_block_not_a_dead_one() {
        // Interleaved tool indices are not something llama.cpp does, but the rule "never a
        // delta on a closed index" has to hold anyway.
        let mut t = SseTranslator::new("m".to_owned());
        let call = |i: u64, id: &str, args: &str| {
            format!(
                r#"data: {{"id":"c","choices":[{{"index":0,"delta":{{"tool_calls":[{{"index":{i},"id":"{id}","type":"function","function":{{"name":"f","arguments":"{args}"}}}}]}}}}]}}
"#
            )
        };
        let mut raw = t.feed(call(0, "a", "{").as_bytes());
        raw.extend(t.feed(call(1, "b", "{}").as_bytes()));
        raw.extend(t.feed(call(0, "a", "}").as_bytes()));
        raw.extend(t.feed(b"data: [DONE]\n"));
        let frames = parse(&raw);

        assert_well_formed(&frames);
        let starts: Vec<u64> = frames
            .iter()
            .filter(|f| f.event == "content_block_start")
            .filter_map(|f| f.data["index"].as_u64())
            .collect();
        assert_eq!(starts, [0, 1, 2], "the revisited call gets a fresh index");
    }
}
