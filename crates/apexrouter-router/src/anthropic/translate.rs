//! OWNER: unit R-10 (router/src/anthropic/{mod,translate,sse}.rs). Do not edit outside that
//! unit.
//!
//! Pure, synchronous, no I/O, not `async`. Unit-tested against fixtures.
//!
//! The translation contract (each line is a fixture test):
//!
//! | Anthropic | OpenAI | Rule |
//! |---|---|---|
//! | top-level `system` | a `{"role":"system"}` message | hoist/lower; a block array joins on `\n\n`; absent ⇒ no system message is invented |
//! | `{"role":"system"}` inside `messages[]` | a `{"role":"system"}` message **in position** | passed through where it stands; never hoisted, never merged into the top-level one |
//! | `max_tokens` — **REQUIRED** | `max_tokens` — optional | missing ⇒ [`TranslateError::MissingMaxTokens`] ⇒ `400`. **Never defaulted silently** |
//! | typed block array | a plain string, or the parts array | one `text` block lowers to a plain string, which is what keeps llama.cpp happy |
//! | `tools[].input_schema` | `tools[].function.parameters` | rename only; the JSON Schema is copied byte-identically |
//! | `tool_use` block | `tool_calls[]` | `input` (object) → `function.arguments` (**a JSON string**) |
//! | `tool_result` in a `user` message | `{"role":"tool","tool_call_id":…}` | hoisted out of the user turn, in order |
//! | `stop_reason` ↔ `finish_reason` | `end_turn`↔`stop`, `max_tokens`↔`length`, `tool_use`↔`tool_calls` | both directions |
//! | `usage.input_tokens`/`output_tokens` | `usage.prompt_tokens`/`completion_tokens` | rename only; **never recomputed, never estimated** |
//! | `thinking` block | — | no equivalent. [`TranslateError::UnsupportedBlock`] |
//!
//! # Three decisions this file records rather than guesses
//!
//! **A `{"role":"system"}` message inside `messages[]` is accepted and lowered in place.** This
//! is not a client bug: a mid-conversation system message is a current Messages API feature —
//! an operator instruction is appended as `{"role":"system", "content": …}` in `messages[]`
//! *instead of* editing the top-level `system` field, specifically so it does not invalidate
//! the cached prefix. Claude Code 2.1.220 uses it on its **first** request (its Agent-tool
//! catalogue, observed on the wire as `messages = ['user', 'system']`, alongside
//! `anthropic-beta: …,mid-conversation-system-2026-04-07,…`), so an ingress that answers
//! `400 role "system" is not a Messages role` kills the harness before it has said anything.
//! OpenAI natively accepts a system message anywhere in `messages[]`, so this lowers 1:1 and
//! the fix is to pass it through in position.
//!
//! The Messages API also states *where* such a message may appear — it must follow a user turn
//! (or an assistant turn ending in server-tool use), must be last or followed by an assistant
//! turn, and can never be `messages[0]`. **This unit does not re-validate any of that.** Every
//! arrangement it forbids is still legal OpenAI, so enforcing it here could only ever turn a
//! request the upstream would have answered into a `400` — which is the exact failure this
//! decision exists to remove. Placement is Anthropic's to police, upstream of us.
//!
//! **`reasoning_content` is not mapped.** llama.cpp b9199 splits a reasoning model's chain of
//! thought into `choices[].message.reasoning_content` (buffered) and
//! `choices[].delta.reasoning_content` (streaming) — verified live on this machine against
//! `Carnice-9b-Q6_K` on `build-vulkan`, where a 206-token answer arrived as 190 reasoning
//! deltas and one content delta. It is the closest thing to an Anthropic `thinking` block that
//! exists, and mk1 still does **not** map it, for a reason larger than cost: a `thinking` block
//! on the way *out* comes straight back *in* on the client's next turn, where this same
//! contract says it is [`TranslateError::UnsupportedBlock`]. Emitting one would turn a working
//! turn 1 into a `400` on turn 2. So reasoning text is left out of the content, counted, and
//! logged at `debug`. `ARCHITECTURE.md` §12 records the same decision.
//!
//! **Sampling knobs with no OpenAI spelling are dropped, not smuggled.** `top_k`, the
//! top-level `thinking` budget, `container`, `mcp_servers` and `service_tier` have no field in
//! a `ChatCompletionRequest`. A strict upstream answers `400` for an unknown key, so passing
//! them through would trade a slightly different sampler for a dead request. Every dropped key
//! is logged at `debug`.

use serde_json::{json, Map, Value};

/// From `[router] anthropic_tools`.
///
/// **Stock config defaults `tools: true`** (CHARTER amendment 2026-07-31 — Claude Code
/// sends tools on every request). This struct's [`Default`] is `tools: false` so a unit
/// test that does not pass config stays conservative; production always builds from
/// `RouterCfg.anthropic_tools`, not from `AnthropicCfg::default()`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct AnthropicCfg {
    /// When false, a `/v1/messages` body carrying `tools` is REFUSED with a clear error
    /// naming the config key — never silently stripped and answered wrongly.
    pub tools: bool,
}

/// Why a body could not be translated. Each becomes a `400` with an Anthropic-shaped body.
#[derive(Debug, thiserror::Error)]
pub enum TranslateError {
    /// `max_tokens` is required on the Anthropic side and is never defaulted for the client.
    #[error("max_tokens is required by the Anthropic Messages API")]
    MissingMaxTokens,
    /// `tools` was present while `[router] anthropic_tools = false`.
    #[error("tool translation is off: set [router] anthropic_tools = true to enable it")]
    ToolsDisabled,
    /// A content block with no OpenAI equivalent, e.g. `thinking`.
    #[error("content block type {kind:?} has no OpenAI equivalent")]
    UnsupportedBlock {
        /// The block's `type`.
        kind: String,
    },
    /// The body did not match the Messages schema.
    #[error("malformed request at {at}: {why}")]
    Malformed {
        /// A JSON pointer-ish location.
        at: String,
        /// What was wrong.
        why: String,
    },
}

impl TranslateError {
    /// The Anthropic `error.type` token this failure is reported under.
    ///
    /// Every variant is a client-side mistake or a refused capability, so every one is
    /// `invalid_request_error` — the token an Anthropic SDK turns into a `BadRequestError`
    /// rather than a retry.
    pub fn kind(&self) -> &'static str {
        "invalid_request_error"
    }
}

/// Build a [`TranslateError::Malformed`] without four lines of `to_owned()` per call site.
fn malformed(at: &str, why: &str) -> TranslateError {
    TranslateError::Malformed {
        at: at.to_owned(),
        why: why.to_owned(),
    }
}

// ==============================================================================================
// request:  Anthropic MessagesRequest  ->  OpenAI ChatCompletionRequest
// ==============================================================================================

/// Anthropic `MessagesRequest` → OpenAI `ChatCompletionRequest`.
///
/// **`model` is left EXACTLY as the client sent it**: `resolve()` owns model naming, and
/// this unit never invents an alias.
///
/// The checks run in the order the Messages schema states them: shape, then the required
/// `max_tokens`, then the tools capability gate, then the message array. A body that is both
/// missing `max_tokens` and carrying `tools` is reported as
/// [`TranslateError::MissingMaxTokens`], because that field is missing whatever the operator
/// has configured.
///
/// When `stream` is true the outbound body also carries
/// `stream_options: {"include_usage": true}`. That is not an optimisation: Anthropic's
/// `message_delta` frame must carry the real final usage, and an OpenAI stream only emits a
/// usage chunk when it is asked to. Verified against llama.cpp b9199, which honours it.
pub fn request_to_openai(body: &[u8], cfg: &AnthropicCfg) -> Result<Vec<u8>, TranslateError> {
    let root: Value =
        serde_json::from_slice(body).map_err(|e| malformed("$", &format!("invalid JSON: {e}")))?;
    let obj = root
        .as_object()
        .ok_or_else(|| malformed("$", "the body is not a JSON object"))?;

    let mut out = Map::new();

    // `model` verbatim. `resolve()` owns naming; this unit never invents an alias.
    match obj.get("model") {
        Some(m) if m.is_string() => {
            out.insert("model".to_owned(), m.clone());
        }
        Some(_) => return Err(malformed("$.model", "model must be a string")),
        None => return Err(malformed("$.model", "model is required")),
    }

    // REQUIRED, and never defaulted silently.
    match obj.get("max_tokens") {
        None | Some(Value::Null) => return Err(TranslateError::MissingMaxTokens),
        Some(v) if v.is_u64() => {
            out.insert("max_tokens".to_owned(), v.clone());
        }
        Some(_) => {
            return Err(malformed(
                "$.max_tokens",
                "max_tokens must be a positive integer",
            ))
        }
    }

    // The capability gate, before a single message is looked at and long before any upstream
    // is contacted.
    if let Some(tools) = obj.get("tools").filter(|v| !v.is_null()) {
        let listed = tools
            .as_array()
            .ok_or_else(|| malformed("$.tools", "tools must be an array"))?;
        if !listed.is_empty() {
            if !cfg.tools {
                return Err(TranslateError::ToolsDisabled);
            }
            out.insert("tools".to_owned(), Value::Array(lower_tools(listed)?));
        }
        // `"tools": []` declares no tool, so there is no capability to refuse and no wrong
        // answer to give. It is dropped rather than rejected.
    }
    if let Some(choice) = obj.get("tool_choice").filter(|v| !v.is_null()) {
        if out.contains_key("tools") {
            out.insert("tool_choice".to_owned(), lower_tool_choice(choice)?);
        }
    }

    // ---- messages, with the top-level `system` hoisted in front -------------------------------
    //
    // A body may legitimately carry BOTH a top-level `system` and a `{"role":"system"}` message
    // inside `messages[]` — Claude Code sends exactly that — and their order has to survive:
    // the top-level one first, then the messages in the order the client wrote them.
    let mut msgs: Vec<Value> = Vec::new();
    if let Some(system) = obj.get("system").filter(|v| !v.is_null()) {
        let text = system_text(system, "$.system")?;
        msgs.push(json!({ "role": "system", "content": text }));
    }

    let listed = obj
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| malformed("$.messages", "messages must be an array"))?;
    for (i, m) in listed.iter().enumerate() {
        let at = format!("$.messages[{i}]");
        let m = m
            .as_object()
            .ok_or_else(|| malformed(&at, "a message must be an object"))?;
        let content = m
            .get("content")
            .ok_or_else(|| malformed(&at, "a message must carry content"))?;
        match m.get("role").and_then(Value::as_str) {
            Some("user") => lower_user(content, &at, cfg, &mut msgs)?,
            Some("assistant") => lower_assistant(content, &at, cfg, &mut msgs)?,
            // A mid-conversation operator instruction. It lowers 1:1 and stays exactly where
            // the client put it — see the module doc.
            Some("system") => {
                let text = system_text(content, &format!("{at}.content"))?;
                msgs.push(json!({ "role": "system", "content": text }));
            }
            Some(other) => {
                return Err(malformed(
                    &at,
                    &format!("role {other:?} is not a Messages role (user | assistant | system)"),
                ))
            }
            None => return Err(malformed(&at, "a message must carry a role")),
        }
    }
    out.insert("messages".to_owned(), Value::Array(msgs));

    // ---- the knobs that survive -----------------------------------------------------------------
    for key in ["temperature", "top_p"] {
        if let Some(v) = obj.get(key).filter(|v| !v.is_null()) {
            out.insert(key.to_owned(), v.clone());
        }
    }
    if let Some(stop) = obj.get("stop_sequences").filter(|v| !v.is_null()) {
        out.insert("stop".to_owned(), stop.clone());
    }
    if obj.get("stream").and_then(Value::as_bool).unwrap_or(false) {
        out.insert("stream".to_owned(), Value::Bool(true));
        out.insert(
            "stream_options".to_owned(),
            json!({ "include_usage": true }),
        );
    }
    if let Some(user) = obj
        .get("metadata")
        .and_then(Value::as_object)
        .and_then(|m| m.get("user_id"))
        .filter(|v| v.is_string())
    {
        out.insert("user".to_owned(), user.clone());
    }

    // ---- and the knobs that do not, said out loud -------------------------------------------------
    for key in [
        "top_k",
        "thinking",
        "container",
        "mcp_servers",
        "service_tier",
    ] {
        if obj.get(key).is_some_and(|v| !v.is_null()) {
            tracing::debug!(
                key,
                "anthropic ingress: dropping a Messages field with no ChatCompletion spelling"
            );
        }
    }

    serde_json::to_vec(&Value::Object(out)).map_err(|e| {
        malformed(
            "$",
            &format!("could not serialise the translated body: {e}"),
        )
    })
}

/// System content as one string: a bare string verbatim, a block array joined on `\n\n`.
///
/// `at` is where the array lives, so the same routine reports `$.system[1]` for the top-level
/// field and `$.messages[2].content[1]` for a mid-conversation system message. Both spellings
/// carry text and nothing else — there is no image or tool block in a system turn.
fn system_text(system: &Value, at: &str) -> Result<String, TranslateError> {
    if let Some(s) = system.as_str() {
        return Ok(s.to_owned());
    }
    let blocks = system
        .as_array()
        .ok_or_else(|| malformed(at, "system content must be a string or a text block array"))?;
    let mut parts: Vec<&str> = Vec::with_capacity(blocks.len());
    for (i, b) in blocks.iter().enumerate() {
        let kind = b.get("type").and_then(Value::as_str).unwrap_or("");
        if kind != "text" {
            return Err(TranslateError::UnsupportedBlock {
                kind: format!("{kind} (in {at}[{i}])"),
            });
        }
        parts.push(b.get("text").and_then(Value::as_str).unwrap_or(""));
    }
    Ok(parts.join("\n\n"))
}

/// Lower one `user` turn, hoisting its `tool_result` blocks out in front of it.
fn lower_user(
    content: &Value,
    at: &str,
    cfg: &AnthropicCfg,
    out: &mut Vec<Value>,
) -> Result<(), TranslateError> {
    if let Some(s) = content.as_str() {
        out.push(json!({ "role": "user", "content": s }));
        return Ok(());
    }
    let blocks = content
        .as_array()
        .ok_or_else(|| malformed(at, "content must be a string or a block array"))?;

    let mut tool_msgs: Vec<Value> = Vec::new();
    let mut parts: Vec<Value> = Vec::new();
    for (i, b) in blocks.iter().enumerate() {
        let at = format!("{at}.content[{i}]");
        match block_type(b, &at)? {
            "text" => parts.push(json!({
                "type": "text",
                "text": b.get("text").and_then(Value::as_str).unwrap_or(""),
            })),
            "image" => parts.push(lower_image(b, &at)?),
            "tool_result" => {
                if !cfg.tools {
                    return Err(TranslateError::ToolsDisabled);
                }
                tool_msgs.push(lower_tool_result(b, &at)?);
            }
            kind => {
                return Err(TranslateError::UnsupportedBlock {
                    kind: kind.to_owned(),
                })
            }
        }
    }

    // One `tool_result` becomes one `tool` message, in order, ahead of whatever the user
    // actually said in the same turn.
    out.append(&mut tool_msgs);
    match parts.len() {
        0 => {}
        // The common case, and the one that keeps llama.cpp happy: a lone text block lowers
        // to a plain string, not a one-element parts array.
        1 if parts[0].get("type").and_then(Value::as_str) == Some("text") => {
            let text = parts[0].get("text").cloned().unwrap_or(Value::Null);
            out.push(json!({ "role": "user", "content": text }));
        }
        _ => out.push(json!({ "role": "user", "content": Value::Array(parts) })),
    }
    Ok(())
}

/// Lower one `assistant` turn: text becomes `content`, `tool_use` becomes `tool_calls`.
fn lower_assistant(
    content: &Value,
    at: &str,
    cfg: &AnthropicCfg,
    out: &mut Vec<Value>,
) -> Result<(), TranslateError> {
    if let Some(s) = content.as_str() {
        out.push(json!({ "role": "assistant", "content": s }));
        return Ok(());
    }
    let blocks = content
        .as_array()
        .ok_or_else(|| malformed(at, "content must be a string or a block array"))?;

    let mut texts: Vec<&str> = Vec::new();
    let mut calls: Vec<Value> = Vec::new();
    for (i, b) in blocks.iter().enumerate() {
        let at = format!("{at}.content[{i}]");
        match block_type(b, &at)? {
            "text" => texts.push(b.get("text").and_then(Value::as_str).unwrap_or("")),
            "tool_use" => {
                if !cfg.tools {
                    return Err(TranslateError::ToolsDisabled);
                }
                calls.push(lower_tool_use(b, &at)?);
            }
            kind => {
                return Err(TranslateError::UnsupportedBlock {
                    kind: kind.to_owned(),
                })
            }
        }
    }

    let mut msg = Map::new();
    msg.insert("role".to_owned(), Value::String("assistant".to_owned()));
    // An assistant turn that is nothing but tool calls carries `content: null` — the shape
    // OpenAI itself emits, and the one llama.cpp b9199 accepts on the way back in.
    if texts.is_empty() && !calls.is_empty() {
        msg.insert("content".to_owned(), Value::Null);
    } else {
        msg.insert("content".to_owned(), Value::String(texts.join("\n\n")));
    }
    if !calls.is_empty() {
        msg.insert("tool_calls".to_owned(), Value::Array(calls));
    }
    out.push(Value::Object(msg));
    Ok(())
}

/// A block's `type`, or a [`TranslateError::Malformed`] naming where it was missing.
fn block_type<'a>(b: &'a Value, at: &str) -> Result<&'a str, TranslateError> {
    b.get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| malformed(at, "a content block must carry a type"))
}

/// `image` → the OpenAI `image_url` part, base64 sources re-spelled as a `data:` URI.
fn lower_image(b: &Value, at: &str) -> Result<Value, TranslateError> {
    let source = b
        .get("source")
        .and_then(Value::as_object)
        .ok_or_else(|| malformed(at, "an image block must carry a source object"))?;
    let url = match source.get("type").and_then(Value::as_str) {
        Some("base64") => {
            let media = source
                .get("media_type")
                .and_then(Value::as_str)
                .ok_or_else(|| malformed(at, "a base64 image source needs a media_type"))?;
            let data = source
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(|| malformed(at, "a base64 image source needs data"))?;
            format!("data:{media};base64,{data}")
        }
        Some("url") => source
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| malformed(at, "a url image source needs a url"))?
            .to_owned(),
        Some(other) => {
            return Err(TranslateError::UnsupportedBlock {
                kind: format!("image/{other}"),
            })
        }
        None => return Err(malformed(at, "an image source needs a type")),
    };
    Ok(json!({ "type": "image_url", "image_url": { "url": url } }))
}

/// `tool_use` → one entry of an assistant message's `tool_calls[]`.
///
/// `input` is an object on the Anthropic side and a **JSON string** on the OpenAI side; that
/// re-serialisation is the whole of the difference.
fn lower_tool_use(b: &Value, at: &str) -> Result<Value, TranslateError> {
    let id = b
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| malformed(at, "a tool_use block needs an id"))?;
    let name = b
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| malformed(at, "a tool_use block needs a name"))?;
    let input = b.get("input").cloned().unwrap_or_else(|| json!({}));
    let arguments = serde_json::to_string(&input)
        .map_err(|e| malformed(at, &format!("tool_use input is not serialisable: {e}")))?;
    Ok(json!({
        "id": id,
        "type": "function",
        "function": { "name": name, "arguments": arguments },
    }))
}

/// `tool_result` → one `{"role":"tool","tool_call_id":…}` message.
///
/// OpenAI's `tool` message content is a string, so a block array is joined on `\n\n`. A
/// non-text block inside a tool result (a screenshot, say) has nowhere to go and is refused
/// rather than dropped.
fn lower_tool_result(b: &Value, at: &str) -> Result<Value, TranslateError> {
    let id = b
        .get("tool_use_id")
        .and_then(Value::as_str)
        .ok_or_else(|| malformed(at, "a tool_result block needs a tool_use_id"))?;
    let content = match b.get("content") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => {
            let mut parts: Vec<&str> = Vec::with_capacity(blocks.len());
            for inner in blocks {
                let kind = inner.get("type").and_then(Value::as_str).unwrap_or("");
                if kind != "text" {
                    return Err(TranslateError::UnsupportedBlock {
                        kind: format!("{kind} (inside a tool_result)"),
                    });
                }
                parts.push(inner.get("text").and_then(Value::as_str).unwrap_or(""));
            }
            parts.join("\n\n")
        }
        Some(_) => {
            return Err(malformed(
                at,
                "tool_result content must be a string or array",
            ))
        }
    };
    Ok(json!({ "role": "tool", "tool_call_id": id, "content": content }))
}

/// `tools[]` → OpenAI function tools. `input_schema` is renamed to `parameters` and the JSON
/// Schema underneath it is copied byte-identically.
fn lower_tools(tools: &[Value]) -> Result<Vec<Value>, TranslateError> {
    let mut out = Vec::with_capacity(tools.len());
    for (i, t) in tools.iter().enumerate() {
        let at = format!("$.tools[{i}]");
        let obj = t
            .as_object()
            .ok_or_else(|| malformed(&at, "a tool must be an object"))?;
        let Some(schema) = obj.get("input_schema") else {
            // A server-side tool (`computer_20241022`, `web_search_20250305`, …) runs inside
            // Anthropic's own infrastructure and has no ChatCompletion spelling at all.
            let kind = obj.get("type").and_then(Value::as_str).unwrap_or("unknown");
            return Err(TranslateError::UnsupportedBlock {
                kind: format!("tool/{kind}"),
            });
        };
        let name = obj
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| malformed(&at, "a tool needs a name"))?;
        let mut function = Map::new();
        function.insert("name".to_owned(), Value::String(name.to_owned()));
        if let Some(d) = obj.get("description").filter(|v| v.is_string()) {
            function.insert("description".to_owned(), d.clone());
        }
        function.insert("parameters".to_owned(), schema.clone());
        out.push(json!({ "type": "function", "function": Value::Object(function) }));
    }
    Ok(out)
}

/// `tool_choice` → the OpenAI spelling.
fn lower_tool_choice(choice: &Value) -> Result<Value, TranslateError> {
    let at = "$.tool_choice";
    let obj = choice
        .as_object()
        .ok_or_else(|| malformed(at, "tool_choice must be an object"))?;
    match obj.get("type").and_then(Value::as_str) {
        Some("auto") => Ok(Value::String("auto".to_owned())),
        Some("any") => Ok(Value::String("required".to_owned())),
        Some("none") => Ok(Value::String("none".to_owned())),
        Some("tool") => {
            let name = obj
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| malformed(at, "tool_choice type=tool needs a name"))?;
            Ok(json!({ "type": "function", "function": { "name": name } }))
        }
        Some(other) => Err(malformed(
            at,
            &format!("unknown tool_choice type {other:?}"),
        )),
        None => Err(malformed(at, "tool_choice needs a type")),
    }
}

// ==============================================================================================
// response:  buffered OpenAI ChatCompletion  ->  Anthropic Message
// ==============================================================================================

/// Buffered OpenAI `ChatCompletion` → Anthropic `Message`. `id` is passed through prefixed
/// `msg_`.
///
/// Only `choices[0]` is translated: the Anthropic Messages API has no `n`, so a second choice
/// is not representable. Its presence is logged at `debug` rather than invented into a second
/// content block.
pub fn response_to_anthropic(body: &[u8]) -> Result<Vec<u8>, TranslateError> {
    let root: Value = serde_json::from_slice(body)
        .map_err(|e| malformed("$", &format!("upstream sent invalid JSON: {e}")))?;
    let obj = root
        .as_object()
        .ok_or_else(|| malformed("$", "the upstream body is not a JSON object"))?;

    let choices = obj
        .get("choices")
        .and_then(Value::as_array)
        .ok_or_else(|| malformed("$.choices", "a ChatCompletion must carry choices"))?;
    if choices.len() > 1 {
        tracing::debug!(
            choices = choices.len(),
            "anthropic ingress: the Messages API has no `n`; only choices[0] is translated"
        );
    }
    let choice = choices
        .first()
        .ok_or_else(|| malformed("$.choices", "a ChatCompletion must carry one choice"))?;
    let message = choice
        .get("message")
        .and_then(Value::as_object)
        .ok_or_else(|| malformed("$.choices[0].message", "missing"))?;

    // Recorded, never mapped — see the module doc.
    if let Some(r) = message.get("reasoning_content").and_then(Value::as_str) {
        if !r.is_empty() {
            tracing::debug!(
                chars = r.len(),
                "anthropic ingress: upstream reasoning_content has no Anthropic spelling in mk1 \
                 and is not mapped to a thinking block"
            );
        }
    }

    let mut content: Vec<Value> = Vec::new();
    if let Some(text) = message.get("content").and_then(Value::as_str) {
        if !text.is_empty() {
            content.push(json!({ "type": "text", "text": text }));
        }
    }
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for (i, c) in calls.iter().enumerate() {
            let at = format!("$.choices[0].message.tool_calls[{i}]");
            content.push(raise_tool_call(c, &at)?);
        }
    }

    let stop_reason = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .map(map_stop_reason_to_anthropic);

    let id = obj.get("id").and_then(Value::as_str).unwrap_or_default();
    let mut out = Map::new();
    out.insert("id".to_owned(), Value::String(format!("msg_{id}")));
    out.insert("type".to_owned(), Value::String("message".to_owned()));
    out.insert("role".to_owned(), Value::String("assistant".to_owned()));
    if let Some(model) = obj.get("model").filter(|v| v.is_string()) {
        out.insert("model".to_owned(), model.clone());
    }
    out.insert("content".to_owned(), Value::Array(content));
    out.insert(
        "stop_reason".to_owned(),
        stop_reason.map_or(Value::Null, |s| Value::String(s.to_owned())),
    );
    out.insert("stop_sequence".to_owned(), Value::Null);
    out.insert("usage".to_owned(), raise_usage(obj.get("usage")));

    serde_json::to_vec(&Value::Object(out)).map_err(|e| {
        malformed(
            "$",
            &format!("could not serialise the translated body: {e}"),
        )
    })
}

/// One OpenAI `tool_calls[]` entry → a `tool_use` content block.
///
/// `arguments` is a JSON **string** upstream and an object downstream. A model that emitted
/// unparseable arguments is reported, not papered over with an empty object: an agent handed
/// `{}` calls the tool with no arguments and then blames the tool.
fn raise_tool_call(c: &Value, at: &str) -> Result<Value, TranslateError> {
    let id = c
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| malformed(at, "a tool_call needs an id"))?;
    let f = c
        .get("function")
        .and_then(Value::as_object)
        .ok_or_else(|| malformed(at, "a tool_call needs a function"))?;
    let name = f
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| malformed(at, "a tool_call function needs a name"))?;
    let raw = f.get("arguments").and_then(Value::as_str).unwrap_or("");
    let input: Value = if raw.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(raw).map_err(|e| {
            malformed(
                &format!("{at}.function.arguments"),
                &format!("the model emitted tool arguments that are not JSON: {e}"),
            )
        })?
    };
    Ok(json!({ "type": "tool_use", "id": id, "name": name, "input": input }))
}

/// `prompt_tokens`/`completion_tokens` → `input_tokens`/`output_tokens`. A rename and nothing
/// else: a count the upstream did not report is reported as `0`, never estimated from the
/// bytes we happen to have seen.
fn raise_usage(usage: Option<&Value>) -> Value {
    let get = |k: &str| {
        usage
            .and_then(|u| u.get(k))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    json!({ "input_tokens": get("prompt_tokens"), "output_tokens": get("completion_tokens") })
}

// ==============================================================================================
// stop reasons
// ==============================================================================================

/// `stop` → `end_turn`, `length` → `max_tokens`, `tool_calls` → `tool_use`.
///
/// `content_filter` becomes Anthropic's `refusal`, which is what it means. An unrecognised
/// `finish_reason` becomes `end_turn`: the generation did stop, and inventing a token an
/// Anthropic SDK has never heard of would fail its enum instead of degrading.
pub fn map_stop_reason_to_anthropic(finish_reason: &str) -> &'static str {
    match finish_reason {
        "length" => "max_tokens",
        "tool_calls" | "function_call" => "tool_use",
        "content_filter" => "refusal",
        _ => "end_turn",
    }
}

/// The inverse of [`map_stop_reason_to_anthropic`].
///
/// `stop_sequence` also lands on `stop`: OpenAI reports a stop-sequence hit as `stop` and
/// carries no separate token for it.
pub fn map_stop_reason_to_openai(stop_reason: &str) -> &'static str {
    match stop_reason {
        "max_tokens" => "length",
        "tool_use" => "tool_calls",
        "refusal" => "content_filter",
        _ => "stop",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method as m_method, path as m_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn tools_off() -> AnthropicCfg {
        AnthropicCfg { tools: false }
    }
    fn tools_on() -> AnthropicCfg {
        AnthropicCfg { tools: true }
    }

    fn tr(body: &str, cfg: &AnthropicCfg) -> Value {
        let out = request_to_openai(body.as_bytes(), cfg).expect("translated");
        serde_json::from_slice(&out).expect("json")
    }

    /// A `/v1/messages` body of the shape the Claude Code harness sends: a system prompt as a
    /// block array, a prior assistant turn, `max_tokens`, `metadata`.
    const CLAUDE_CODE_REQUEST: &str = r#"{
      "model": "claude-sonnet-4-5-20250929",
      "max_tokens": 4096,
      "temperature": 1,
      "system": [
        {"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."},
        {"type":"text","text":"IMPORTANT: never guess a file path."}
      ],
      "messages": [
        {"role":"user","content":[{"type":"text","text":"what does src/main.rs do?"}]},
        {"role":"assistant","content":[{"type":"text","text":"Let me read it."}]},
        {"role":"user","content":"go on"}
      ],
      "metadata": {"user_id": "andre"},
      "stream": false
    }"#;

    /// A buffered `/v1/chat/completions` body captured verbatim from llama.cpp
    /// `b9199 (39cf5d619)` serving `Carnice-9b-Q6_K.gguf` on `build-vulkan`, reasoning text
    /// truncated. Note the `reasoning_content` sibling — this is the real shape, not a guess.
    const LLAMACPP_BUFFERED: &str = r#"{"choices":[{"finish_reason":"stop","index":0,"message":{"role":"assistant","content":"OK","reasoning_content":"The user is repeatedly asking me to respond with 'OK'"}}],"created":1785448506,"model":"carnice","system_fingerprint":"b9199-39cf5d619","object":"chat.completion","usage":{"completion_tokens":85,"prompt_tokens":15,"total_tokens":100,"prompt_tokens_details":{"cached_tokens":0}},"id":"chatcmpl-VXbkIMz3XZNTaE35HqRcSRGQ9QdEG0pq","timings":{"predicted_per_second":9.37296930484877}}"#;

    /// The shape real Claude Code 2.1.220 puts on the wire on its **first** request, captured
    /// by the acceptance gate and abridged: a top-level `system` block array **and** a
    /// `{"role":"system"}` message inside `messages[]` — observed as `messages = ['user',
    /// 'system']` — sent alongside
    /// `anthropic-beta: …,mid-conversation-system-2026-04-07,…`.
    ///
    /// Before FIX-2 this body was answered
    /// `400 malformed request at $.messages[1]: role "system" is not a Messages role`, and the
    /// harness died with `API Error: 400` before printing a token.
    // `r##"…"##`: the fixture's own text contains `"#`, which would close an `r#"…"#`.
    const CLAUDE_CODE_MID_CONVERSATION_SYSTEM: &str = r##"{
      "model": "carnice",
      "max_tokens": 512,
      "temperature": 1,
      "system": [
        {"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."},
        {"type":"text","text":"You are an interactive CLI tool that helps users with software engineering tasks."}
      ],
      "messages": [
        {"role":"user","content":[
          {"type":"text","text":"Reply with exactly the word PONG and nothing else."}]},
        {"role":"system","content":[
          {"type":"text","text":"# Agent tools"},
          {"type":"text","text":"The following tools are available to the Agent tool."}]}
      ],
      "metadata": {"user_id": "andre"},
      "stream": false
    }"##;

    const WEATHER_TOOLS: &str = r#"[{"name":"get_weather","description":"Get the weather",
        "input_schema":{"type":"object","properties":{"location":{"type":"string",
        "description":"City"}},"required":["location"]}}]"#;

    // ---- stage 1: non-streaming text -------------------------------------------------------------

    #[test]
    fn max_tokens_is_required_and_never_defaulted() {
        let body = r#"{"model":"auto","messages":[{"role":"user","content":"hi"}]}"#;
        assert!(matches!(
            request_to_openai(body.as_bytes(), &tools_off()),
            Err(TranslateError::MissingMaxTokens)
        ));
        // …and an explicit null is just as absent.
        let body = r#"{"model":"auto","max_tokens":null,"messages":[]}"#;
        assert!(matches!(
            request_to_openai(body.as_bytes(), &tools_off()),
            Err(TranslateError::MissingMaxTokens)
        ));
    }

    #[test]
    fn the_model_is_left_exactly_as_the_client_sent_it() {
        let v = tr(CLAUDE_CODE_REQUEST, &tools_off());
        assert_eq!(v["model"], "claude-sonnet-4-5-20250929");
    }

    #[test]
    fn system_is_hoisted_as_the_first_message_and_joined_on_two_newlines() {
        let v = tr(CLAUDE_CODE_REQUEST, &tools_off());
        let msgs = v["messages"].as_array().expect("messages");
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(
            msgs[0]["content"],
            "You are Claude Code, Anthropic's official CLI for Claude.\n\n\
             IMPORTANT: never guess a file path."
        );
    }

    #[test]
    fn no_system_message_is_invented_when_there_is_none() {
        let v = tr(
            r#"{"model":"auto","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}"#,
            &tools_off(),
        );
        let msgs = v["messages"].as_array().expect("messages");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
    }

    // ---- mid-conversation system messages (the FIX-2 regression) ---------------------------------

    #[test]
    fn a_mid_conversation_system_message_lowers_in_position() {
        // THE regression. Claude Code's own first request: a top-level `system` AND a
        // `{"role":"system"}` message in `messages[]`. Both must survive, in that order.
        let v = tr(CLAUDE_CODE_MID_CONVERSATION_SYSTEM, &tools_off());
        let msgs = v["messages"].as_array().expect("messages");
        assert_eq!(
            msgs.len(),
            3,
            "top-level system, the user turn, then the mid-conversation system: {v}"
        );

        // 1. the top-level `system`, hoisted in front exactly as before.
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(
            msgs[0]["content"],
            "You are Claude Code, Anthropic's official CLI for Claude.\n\n\
             You are an interactive CLI tool that helps users with software engineering tasks."
        );

        // 2. the user turn, untouched.
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(
            msgs[1]["content"],
            "Reply with exactly the word PONG and nothing else."
        );

        // 3. the operator instruction, IN POSITION — after the user turn, not merged into
        //    msgs[0] and not hoisted to the front.
        assert_eq!(msgs[2]["role"], "system");
        assert_eq!(
            msgs[2]["content"],
            "# Agent tools\n\nThe following tools are available to the Agent tool.",
            "a system message's block array joins on \\n\\n like the top-level field"
        );

        // …and nothing about it leaks into the top level of the outbound body.
        assert!(
            v.get("system").is_none(),
            "system is a message on the OpenAI side, never a top-level field: {v}"
        );
    }

    #[test]
    fn a_system_message_takes_a_plain_string_too() {
        let v = tr(
            r#"{"model":"auto","max_tokens":8,"messages":[
                 {"role":"user","content":"hi"},
                 {"role":"system","content":"Terse mode enabled."},
                 {"role":"assistant","content":"ok"}]}"#,
            &tools_off(),
        );
        let msgs = v["messages"].as_array().expect("messages");
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1]["role"], "system");
        assert_eq!(msgs[1]["content"], "Terse mode enabled.");
        assert_eq!(msgs[2]["role"], "assistant");
    }

    #[test]
    fn placement_rules_are_anthropics_to_police_not_ours() {
        // The Messages API says a system message cannot be `messages[0]`. Every arrangement it
        // forbids is still legal OpenAI, so refusing one here could only ever turn a request
        // the upstream would have answered into a 400 — the exact failure FIX-2 removed.
        let v = tr(
            r#"{"model":"auto","max_tokens":8,"messages":[
                 {"role":"system","content":"Operator note."},
                 {"role":"user","content":"hi"}]}"#,
            &tools_off(),
        );
        let msgs = v["messages"].as_array().expect("messages");
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "Operator note.");
        assert_eq!(msgs[1]["role"], "user");
    }

    #[test]
    fn a_non_text_block_in_a_system_message_is_refused_and_names_where() {
        // A system turn carries text and nothing else — an image there has nowhere to go, and
        // the error has to say which message and which block.
        let body = r#"{"model":"auto","max_tokens":8,"messages":[
            {"role":"user","content":"hi"},
            {"role":"system","content":[{"type":"text","text":"note"},
              {"type":"image","source":{"type":"base64","media_type":"image/png","data":"QQ=="}}]}]}"#;
        match request_to_openai(body.as_bytes(), &tools_off()) {
            Err(TranslateError::UnsupportedBlock { kind }) => {
                assert_eq!(kind, "image (in $.messages[1].content[1])")
            }
            other => panic!("expected UnsupportedBlock, got {other:?}"),
        }
    }

    #[test]
    fn a_single_text_block_lowers_to_a_plain_string() {
        let v = tr(CLAUDE_CODE_REQUEST, &tools_off());
        let msgs = v["messages"].as_array().expect("messages");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(
            msgs[1]["content"], "what does src/main.rs do?",
            "a lone text block must be a plain string, not a one-element parts array"
        );
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["content"], "Let me read it.");
        assert_eq!(msgs[3]["content"], "go on");
    }

    #[test]
    fn several_blocks_lower_to_the_parts_array() {
        let v = tr(
            r#"{"model":"auto","max_tokens":8,"messages":[{"role":"user","content":[
                 {"type":"text","text":"look"},
                 {"type":"image","source":{"type":"base64","media_type":"image/png","data":"QUJD"}}
               ]}]}"#,
            &tools_off(),
        );
        let parts = v["messages"][0]["content"].as_array().expect("parts");
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,QUJD");
    }

    #[test]
    fn streaming_asks_for_the_usage_chunk_because_message_delta_must_carry_it() {
        let v = tr(
            r#"{"model":"auto","max_tokens":8,"stream":true,
                "messages":[{"role":"user","content":"hi"}]}"#,
            &tools_off(),
        );
        assert_eq!(v["stream"], true);
        assert_eq!(v["stream_options"]["include_usage"], true);
    }

    #[test]
    fn stop_sequences_and_metadata_land_on_their_openai_spellings() {
        let v = tr(
            r#"{"model":"auto","max_tokens":8,"stop_sequences":["END"],"top_k":40,
                "metadata":{"user_id":"andre"},
                "messages":[{"role":"user","content":"hi"}]}"#,
            &tools_off(),
        );
        assert_eq!(v["stop"][0], "END");
        assert_eq!(v["user"], "andre");
        assert!(
            v.get("top_k").is_none(),
            "top_k has no ChatCompletion spelling and a strict upstream 400s on it"
        );
    }

    #[test]
    fn a_thinking_block_is_unsupported_not_a_panic_and_not_a_silent_drop() {
        let body = r#"{"model":"auto","max_tokens":8,"messages":[{"role":"assistant","content":[
            {"type":"thinking","thinking":"hmm","signature":"abc"},
            {"type":"text","text":"answer"}]}]}"#;
        match request_to_openai(body.as_bytes(), &tools_on()) {
            Err(TranslateError::UnsupportedBlock { kind }) => assert_eq!(kind, "thinking"),
            other => panic!("expected UnsupportedBlock, got {other:?}"),
        }
        // redacted_thinking is the same story, and must name itself.
        let body = r#"{"model":"auto","max_tokens":8,"messages":[{"role":"assistant","content":[
            {"type":"redacted_thinking","data":"xx"}]}]}"#;
        match request_to_openai(body.as_bytes(), &tools_on()) {
            Err(TranslateError::UnsupportedBlock { kind }) => assert_eq!(kind, "redacted_thinking"),
            other => panic!("expected UnsupportedBlock, got {other:?}"),
        }
    }

    #[test]
    fn a_malformed_body_names_where_it_broke() {
        match request_to_openai(b"not json", &tools_off()) {
            Err(TranslateError::Malformed { at, .. }) => assert_eq!(at, "$"),
            other => panic!("expected Malformed, got {other:?}"),
        }
        // `tool` is an OpenAI role, not a Messages one: a tool result travels as a block
        // inside a `user` turn. Unknown roles are still refused, and the message names the
        // three that are not — including `system`, now that it is one of them.
        let body = r#"{"model":"auto","max_tokens":8,
            "messages":[{"role":"tool","tool_call_id":"t1","content":"x"}]}"#;
        match request_to_openai(body.as_bytes(), &tools_off()) {
            Err(TranslateError::Malformed { at, why }) => {
                assert_eq!(at, "$.messages[0]");
                assert!(why.contains("tool"), "{why}");
                assert!(why.contains("user | assistant | system"), "{why}");
            }
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    // ---- the response direction ------------------------------------------------------------------

    #[test]
    fn a_real_llamacpp_completion_becomes_a_valid_anthropic_message() {
        let out = response_to_anthropic(LLAMACPP_BUFFERED.as_bytes()).expect("translated");
        let v: Value = serde_json::from_slice(&out).expect("json");

        assert_eq!(v["type"], "message");
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["id"], "msg_chatcmpl-VXbkIMz3XZNTaE35HqRcSRGQ9QdEG0pq");
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(v["content"][0]["text"], "OK");
        assert_eq!(v["content"].as_array().map(Vec::len), Some(1));
        assert_eq!(v["stop_reason"], "end_turn");
        assert!(v["stop_sequence"].is_null());
        assert_eq!(v["usage"]["input_tokens"], 15);
        assert_eq!(v["usage"]["output_tokens"], 85);
        assert!(
            v["usage"].get("prompt_tokens").is_none(),
            "the OpenAI field names must not survive the rename"
        );
    }

    #[test]
    fn reasoning_content_is_recorded_and_not_mapped() {
        let out = response_to_anthropic(LLAMACPP_BUFFERED.as_bytes()).expect("translated");
        let v: Value = serde_json::from_slice(&out).expect("json");
        let kinds: Vec<&str> = v["content"]
            .as_array()
            .map(|a| a.iter().filter_map(|b| b["type"].as_str()).collect())
            .unwrap_or_default();
        assert_eq!(
            kinds,
            ["text"],
            "mk1 emits no thinking block: it would come straight back in as UnsupportedBlock"
        );
    }

    #[test]
    fn a_tool_call_becomes_a_tool_use_block_with_a_parsed_input_object() {
        let body = r#"{"id":"chatcmpl-1","model":"carnice","choices":[{"index":0,
            "finish_reason":"tool_calls","message":{"role":"assistant","content":null,
            "tool_calls":[{"id":"call_x","type":"function","function":{
              "name":"get_weather","arguments":"{\"location\":\"Paris\"}"}}]}}],
            "usage":{"prompt_tokens":272,"completion_tokens":67}}"#;
        let out = response_to_anthropic(body.as_bytes()).expect("translated");
        let v: Value = serde_json::from_slice(&out).expect("json");
        assert_eq!(v["stop_reason"], "tool_use");
        assert_eq!(v["content"][0]["type"], "tool_use");
        assert_eq!(v["content"][0]["id"], "call_x");
        assert_eq!(v["content"][0]["name"], "get_weather");
        assert_eq!(v["content"][0]["input"]["location"], "Paris");
    }

    #[test]
    fn unparseable_tool_arguments_are_reported_not_papered_over() {
        let body = r#"{"id":"c","choices":[{"finish_reason":"tool_calls","message":{
            "role":"assistant","tool_calls":[{"id":"c1","type":"function",
            "function":{"name":"f","arguments":"{oops"}}]}}]}"#;
        match response_to_anthropic(body.as_bytes()) {
            Err(TranslateError::Malformed { at, .. }) => {
                assert!(at.ends_with("function.arguments"), "{at}")
            }
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_usage_is_zero_never_estimated() {
        let body = r#"{"id":"c","choices":[{"finish_reason":"stop",
            "message":{"role":"assistant","content":"hello there, this is many tokens"}}]}"#;
        let out = response_to_anthropic(body.as_bytes()).expect("translated");
        let v: Value = serde_json::from_slice(&out).expect("json");
        assert_eq!(v["usage"]["input_tokens"], 0);
        assert_eq!(v["usage"]["output_tokens"], 0);
    }

    #[test]
    fn stop_reasons_map_both_directions() {
        for (openai, anthropic) in [
            ("stop", "end_turn"),
            ("length", "max_tokens"),
            ("tool_calls", "tool_use"),
        ] {
            assert_eq!(map_stop_reason_to_anthropic(openai), anthropic);
            assert_eq!(map_stop_reason_to_openai(anthropic), openai);
        }
        assert_eq!(map_stop_reason_to_anthropic("content_filter"), "refusal");
        assert_eq!(map_stop_reason_to_openai("refusal"), "content_filter");
        assert_eq!(map_stop_reason_to_openai("stop_sequence"), "stop");
        // Unknown tokens degrade to the neutral end, never to an enum an SDK will reject.
        assert_eq!(map_stop_reason_to_anthropic("who_knows"), "end_turn");
        assert_eq!(map_stop_reason_to_openai("pause_turn"), "stop");
    }

    // ---- stage 3: tools, behind the flag ------------------------------------------------------------

    #[test]
    fn tools_off_refuses_a_body_carrying_tools_and_names_the_config_key() {
        let body = format!(
            r#"{{"model":"auto","max_tokens":8,"tools":{WEATHER_TOOLS},
                "messages":[{{"role":"user","content":"weather in Paris?"}}]}}"#
        );
        match request_to_openai(body.as_bytes(), &tools_off()) {
            Err(e @ TranslateError::ToolsDisabled) => {
                let msg = e.to_string();
                assert!(msg.contains("anthropic_tools"), "{msg}");
            }
            other => panic!("expected ToolsDisabled, got {other:?}"),
        }
    }

    #[test]
    fn tools_off_refuses_tool_blocks_in_the_transcript_too() {
        // A silent drop here would answer the wrong question with total confidence.
        let body = r#"{"model":"auto","max_tokens":8,"messages":[
            {"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"f","input":{}}]}]}"#;
        assert!(matches!(
            request_to_openai(body.as_bytes(), &tools_off()),
            Err(TranslateError::ToolsDisabled)
        ));
        let body = r#"{"model":"auto","max_tokens":8,"messages":[
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}]}"#;
        assert!(matches!(
            request_to_openai(body.as_bytes(), &tools_off()),
            Err(TranslateError::ToolsDisabled)
        ));
    }

    #[test]
    fn an_empty_tools_array_declares_no_capability_and_is_not_refused() {
        let v = tr(
            r#"{"model":"auto","max_tokens":8,"tools":[],
                "messages":[{"role":"user","content":"hi"}]}"#,
            &tools_off(),
        );
        assert!(v.get("tools").is_none());
    }

    #[test]
    fn input_schema_is_renamed_to_parameters_and_copied_byte_identically() {
        let body = format!(
            r#"{{"model":"auto","max_tokens":8,"tools":{WEATHER_TOOLS},
                "tool_choice":{{"type":"any"}},
                "messages":[{{"role":"user","content":"weather?"}}]}}"#
        );
        let v = tr(&body, &tools_on());
        let want: Value = serde_json::from_str(
            r#"{"type":"object","properties":{"location":{"type":"string",
               "description":"City"}},"required":["location"]}"#,
        )
        .expect("schema");
        assert_eq!(v["tools"][0]["type"], "function");
        assert_eq!(v["tools"][0]["function"]["name"], "get_weather");
        assert_eq!(v["tools"][0]["function"]["parameters"], want);
        assert_eq!(v["tool_choice"], "required");
    }

    #[test]
    fn tool_choice_maps_every_documented_form() {
        let with = |choice: &str| {
            let body = format!(
                r#"{{"model":"auto","max_tokens":8,"tools":{WEATHER_TOOLS},
                    "tool_choice":{choice},"messages":[{{"role":"user","content":"x"}}]}}"#
            );
            tr(&body, &tools_on())["tool_choice"].clone()
        };
        assert_eq!(with(r#"{"type":"auto"}"#), "auto");
        assert_eq!(with(r#"{"type":"any"}"#), "required");
        assert_eq!(with(r#"{"type":"none"}"#), "none");
        assert_eq!(
            with(r#"{"type":"tool","name":"get_weather"}"#),
            json!({"type":"function","function":{"name":"get_weather"}})
        );
    }

    #[test]
    fn tool_use_and_tool_result_lower_to_tool_calls_and_a_tool_message() {
        let body = r#"{"model":"auto","max_tokens":64,"messages":[
            {"role":"user","content":"weather in Paris?"},
            {"role":"assistant","content":[
              {"type":"tool_use","id":"toolu_01","name":"get_weather","input":{"location":"Paris"}}]},
            {"role":"user","content":[
              {"type":"tool_result","tool_use_id":"toolu_01","content":"18C and sunny"},
              {"type":"text","text":"and tomorrow?"}]}]}"#;
        let v = tr(body, &tools_on());
        let msgs = v["messages"].as_array().expect("messages");
        assert_eq!(msgs.len(), 4);

        assert_eq!(msgs[1]["role"], "assistant");
        assert!(msgs[1]["content"].is_null(), "tool-only turns carry null");
        assert_eq!(msgs[1]["tool_calls"][0]["id"], "toolu_01");
        assert_eq!(msgs[1]["tool_calls"][0]["type"], "function");
        assert_eq!(msgs[1]["tool_calls"][0]["function"]["name"], "get_weather");
        assert_eq!(
            msgs[1]["tool_calls"][0]["function"]["arguments"], r#"{"location":"Paris"}"#,
            "arguments is a JSON *string* on the OpenAI side"
        );

        // The tool_result is hoisted OUT of the user turn and lands in front of it.
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["tool_call_id"], "toolu_01");
        assert_eq!(msgs[2]["content"], "18C and sunny");
        assert_eq!(msgs[3]["role"], "user");
        assert_eq!(msgs[3]["content"], "and tomorrow?");
    }

    #[test]
    fn two_tool_results_in_one_turn_stay_in_order() {
        let body = r#"{"model":"auto","max_tokens":8,"messages":[{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"a","content":[{"type":"text","text":"one"}]},
            {"type":"tool_result","tool_use_id":"b","content":"two"}]}]}"#;
        let v = tr(body, &tools_on());
        let msgs = v["messages"].as_array().expect("messages");
        assert_eq!(
            msgs.len(),
            2,
            "a turn of nothing but tool results has no user message"
        );
        assert_eq!(msgs[0]["tool_call_id"], "a");
        assert_eq!(msgs[0]["content"], "one");
        assert_eq!(msgs[1]["tool_call_id"], "b");
    }

    #[test]
    fn an_image_inside_a_tool_result_is_refused_rather_than_flattened() {
        let body = r#"{"model":"auto","max_tokens":8,"messages":[{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"a","content":[
              {"type":"image","source":{"type":"base64","media_type":"image/png","data":"QQ=="}}]}]}]}"#;
        match request_to_openai(body.as_bytes(), &tools_on()) {
            Err(TranslateError::UnsupportedBlock { kind }) => {
                assert!(kind.contains("image"), "{kind}")
            }
            other => panic!("expected UnsupportedBlock, got {other:?}"),
        }
    }

    #[test]
    fn a_server_side_tool_has_no_openai_spelling_at_all() {
        let body = r#"{"model":"auto","max_tokens":8,
            "tools":[{"type":"web_search_20250305","name":"web_search"}],
            "messages":[{"role":"user","content":"x"}]}"#;
        match request_to_openai(body.as_bytes(), &tools_on()) {
            Err(TranslateError::UnsupportedBlock { kind }) => {
                assert_eq!(kind, "tool/web_search_20250305")
            }
            other => panic!("expected UnsupportedBlock, got {other:?}"),
        }
    }

    // ---- the buffered round trip, against a wiremock OpenAI upstream -------------------------------

    /// The acceptance round trip. `wiremock` binds 127.0.0.1 only, so this stays inside the
    /// hermeticity rule: no test may connect anywhere but loopback.
    #[tokio::test]
    async fn a_claude_code_body_round_trips_through_an_openai_upstream() {
        let up = MockServer::start().await;
        Mock::given(m_method("POST"))
            .and(m_path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(LLAMACPP_BUFFERED.as_bytes().to_vec(), "application/json"),
            )
            .mount(&up)
            .await;

        let outbound = request_to_openai(CLAUDE_CODE_REQUEST.as_bytes(), &tools_off())
            .expect("request translated");
        let sent: Value = serde_json::from_slice(&outbound).expect("json");
        assert!(sent.get("system").is_none(), "system is a message now");
        assert_eq!(sent["max_tokens"], 4096);

        let res = reqwest::Client::new()
            .post(format!("{}/v1/chat/completions", up.uri()))
            .header("content-type", "application/json")
            .body(outbound)
            .send()
            .await
            .expect("upstream reachable");
        assert_eq!(res.status(), 200);
        let raw = res.bytes().await.expect("body");

        let out = response_to_anthropic(&raw).expect("response translated");
        let v: Value = serde_json::from_slice(&out).expect("json");
        assert_eq!(v["type"], "message");
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(v["stop_reason"], "end_turn");
        assert_eq!(v["usage"]["input_tokens"], 15);
        assert_eq!(v["usage"]["output_tokens"], 85);
    }

    /// The FIX-2 regression at the wire, not at the function: the captured Claude Code body
    /// reaches an OpenAI upstream as a `ChatCompletion` whose `messages` are
    /// `system, user, system` **in that order**, and the answer comes back as an Anthropic
    /// `Message`. This is the exchange that used to end at `400` before the first token.
    #[tokio::test]
    async fn the_captured_claude_code_body_reaches_the_upstream_in_order() {
        let up = MockServer::start().await;
        Mock::given(m_method("POST"))
            .and(m_path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(LLAMACPP_BUFFERED.as_bytes().to_vec(), "application/json"),
            )
            .mount(&up)
            .await;

        let outbound =
            request_to_openai(CLAUDE_CODE_MID_CONVERSATION_SYSTEM.as_bytes(), &tools_off())
                .expect("the mid-conversation system message must not be a 400");

        let res = reqwest::Client::new()
            .post(format!("{}/v1/chat/completions", up.uri()))
            .header("content-type", "application/json")
            .body(outbound)
            .send()
            .await
            .expect("upstream reachable");
        assert_eq!(res.status(), 200);
        let raw = res.bytes().await.expect("body");

        let seen = up.received_requests().await.expect("recording");
        assert_eq!(seen.len(), 1);
        let sent: Value = serde_json::from_slice(&seen[0].body).expect("json");
        let roles: Vec<&str> = sent["messages"]
            .as_array()
            .map(|a| a.iter().filter_map(|m| m["role"].as_str()).collect())
            .unwrap_or_default();
        assert_eq!(
            roles,
            ["system", "user", "system"],
            "the top-level system leads and the mid-conversation one keeps its place: {sent}"
        );
        assert!(sent.get("system").is_none());

        let out = response_to_anthropic(&raw).expect("response translated");
        let v: Value = serde_json::from_slice(&out).expect("json");
        assert_eq!(v["type"], "message");
        assert_eq!(v["content"][0]["text"], "OK");
    }

    /// The `get_weather` fixture surviving the whole loop with `anthropic_tools = true`:
    /// `tool_use` → `tool_calls` → upstream → `tool_result` → `role:"tool"` → final answer.
    /// The two upstream bodies are the shapes llama.cpp b9199 actually returned for exactly
    /// this exchange on this machine.
    #[tokio::test]
    async fn the_get_weather_loop_survives_with_tools_enabled() {
        let up = MockServer::start().await;
        Mock::given(m_method("POST"))
            .and(m_path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(
                    br#"{"id":"chatcmpl-I4Wgr","model":"carnice","choices":[{"index":0,
                     "finish_reason":"tool_calls","message":{"role":"assistant","content":null,
                     "tool_calls":[{"id":"MXwBDrau","type":"function","function":{
                       "name":"get_weather","arguments":"{\"location\":\"Paris\"}"}}]}}],
                     "usage":{"prompt_tokens":272,"completion_tokens":67}}"#
                        .to_vec(),
                    "application/json",
                ),
            )
            .up_to_n_times(1)
            .mount(&up)
            .await;
        Mock::given(m_method("POST"))
            .and(m_path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(
                    br#"{"id":"chatcmpl-2","model":"carnice","choices":[{"index":0,
                     "finish_reason":"stop","message":{"role":"assistant",
                     "content":"The weather in Paris is currently 18C and sunny."}}],
                     "usage":{"prompt_tokens":300,"completion_tokens":12}}"#
                        .to_vec(),
                    "application/json",
                ),
            )
            .mount(&up)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/v1/chat/completions", up.uri());
        let hop = |body: Vec<u8>| {
            let client = client.clone();
            let url = url.clone();
            async move {
                let res = client
                    .post(url)
                    .header("content-type", "application/json")
                    .body(body)
                    .send()
                    .await
                    .expect("upstream reachable");
                res.bytes().await.expect("body")
            }
        };

        // turn 1 — the client asks, the model calls the tool.
        let turn1 = format!(
            r#"{{"model":"auto","max_tokens":64,"tools":{WEATHER_TOOLS},
                "messages":[{{"role":"user","content":"What is the weather in Paris?"}}]}}"#
        );
        let raw = hop(request_to_openai(turn1.as_bytes(), &tools_on()).expect("t1")).await;
        let msg: Value =
            serde_json::from_slice(&response_to_anthropic(&raw).expect("t1 back")).expect("json");
        assert_eq!(msg["stop_reason"], "tool_use");
        let call = msg["content"][0].clone();
        assert_eq!(call["name"], "get_weather");
        assert_eq!(call["input"]["location"], "Paris");

        // turn 2 — the harness replays the tool_use and answers with a tool_result.
        let turn2 = json!({
            "model": "auto",
            "max_tokens": 64,
            "tools": serde_json::from_str::<Value>(WEATHER_TOOLS).expect("tools"),
            "messages": [
                {"role":"user","content":"What is the weather in Paris?"},
                {"role":"assistant","content":[call.clone()]},
                {"role":"user","content":[{"type":"tool_result",
                    "tool_use_id": call["id"].clone(), "content":"18C and sunny"}]}
            ]
        });
        let outbound = request_to_openai(turn2.to_string().as_bytes(), &tools_on()).expect("t2");
        let sent: Value = serde_json::from_slice(&outbound).expect("json");
        let msgs = sent["messages"].as_array().expect("messages");
        assert_eq!(msgs[1]["tool_calls"][0]["function"]["name"], "get_weather");
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["tool_call_id"], "MXwBDrau");

        let raw = hop(outbound).await;
        let msg: Value =
            serde_json::from_slice(&response_to_anthropic(&raw).expect("t2 back")).expect("json");
        assert_eq!(msg["stop_reason"], "end_turn");
        assert_eq!(msg["content"][0]["type"], "text");
        assert!(
            msg["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .contains("sunny"),
            "{msg}"
        );
    }
}
