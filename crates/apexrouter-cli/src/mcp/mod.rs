//! OWNER: unit M-01 (cli/src/mcp/**, cli/src/cmd/mcp.rs). Do not edit outside that unit.
//!
//! Hand-rolled newline-delimited JSON-RPC 2.0 over stdio, copying
//! `Prefrontal-RS/prefrontal-cli/src/mcp.rs` in shape. Hard rules:
//!
//! * **compact one-line JSON** (`to_string`, never `to_string_pretty`);
//! * **all logging to stderr**; nothing but MCP ever reaches stdout;
//! * exit promptly on stdin EOF;
//! * `initialize` **echoes the client's requested `protocolVersion`** back (falling back to
//!   `"2024-11-05"`), which is instant compatibility with every legacy revision;
//! * tool failures are results with `isError: true` and helpful text — JSON-RPC error codes
//!   (`-32601`, `-32700`) are reserved for protocol breakage.
//!
//! Dual-era hedge for the 2026-07-28 revision: also answer `server/discover` advertising
//! `supportedVersions`, accept-and-ignore per-request `_meta`, and emit
//! `resultType: "complete"`.
//!
//! Streamable-HTTP is **not** implemented, but [`dispatch`] is transport-agnostic, so an
//! axum route is a day's work when ApexOS-RV nodes need it over the network.
//!
//! One subtlety worth stating, because it is the difference between "works" and "the client
//! says the server crashed": the *message* is serialised compact so it occupies exactly one
//! line, but a tool's **text payload** is pretty-printed JSON. Newlines inside a JSON string
//! are escaped as `\n` by the serialiser, so the framing rule is kept while an agent — and
//! a human reading the transcript — still gets readable output.

pub mod backend;
pub mod tools;

pub use backend::{LocalBackend, McpBackend, ProxyBackend, ToolError, ToolResult};

use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// The revision echoed back when the client does not name one. Prefrontal pins the same
/// value, and it is what every client in the wild speaks today.
pub const LEGACY_PROTOCOL_VERSION: &str = "2024-11-05";

/// Every revision this server will talk, newest first — the `server/discover` answer.
///
/// It is honest rather than aspirational: the wire shapes here are the legacy ones plus the
/// three cheap 2026-07-28 additions, and a client that picks any entry gets a server that
/// answers.
pub const SUPPORTED_VERSIONS: &[&str] = &[
    "2026-07-28",
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
];

/// How long a client may cache `tools/list`. The list is compiled in, so this is generous.
const TOOLS_TTL_MS: u64 = 300_000;
/// How long a client may cache `server/discover`.
const DISCOVER_TTL_MS: u64 = 3_600_000;

/// The `instructions` string both handshakes carry. One paragraph, operational.
const INSTRUCTIONS: &str = "\
ApexRouter aliases and supervises OpenAI-compatible inference — local llama.cpp and vLLM \
endpoints, rented vast.ai boxes, and managed providers — behind one stable base URL. Call \
apexrouter_status first: it returns the OPENAI_BASE_URL and the `model` string to use. Then \
apexrouter_models before choosing a model id, apexrouter_fit before starting anything large, \
apexrouter_up to start and bind in one call, and apexrouter_logs when a start failed. Tools \
that spend money (apexrouter_vast_rent, apexrouter_vast_destroy) refuse without \
`confirm: true` and return a priced dry run instead — show it to the human and get an \
explicit yes before resending.";

/// A JSON-RPC error. Reserved for protocol breakage; a failed tool is a **result** with
/// `isError: true`.
#[derive(Clone, Debug, PartialEq)]
pub struct RpcError {
    /// The JSON-RPC code, e.g. `-32601` (method not found), `-32700` (parse error).
    pub code: i64,
    /// Human-readable.
    pub message: String,
    /// Optional structured detail.
    pub data: Option<Value>,
}

impl RpcError {
    /// `-32700`: the line was not JSON at all.
    pub fn parse_error(why: &str) -> RpcError {
        RpcError {
            code: -32_700,
            message: format!("parse error: {why}"),
            data: None,
        }
    }

    /// `-32601`: a method this server does not implement.
    pub fn method_not_found(method: &str) -> RpcError {
        RpcError {
            code: -32_601,
            message: format!("method not found: {method}"),
            data: Some(json!({
                "supported": [
                    "initialize", "ping", "tools/list", "tools/call", "server/discover",
                ],
            })),
        }
    }

    /// The `error` member of a JSON-RPC response.
    pub fn to_value(&self) -> Value {
        match &self.data {
            Some(d) => json!({ "code": self.code, "message": self.message, "data": d }),
            None => json!({ "code": self.code, "message": self.message }),
        }
    }
}

/// Read stdin, dispatch, write stdout. Exits on EOF.
///
/// Every byte written here is one compact JSON-RPC message followed by `\n`, and stdout is
/// flushed after each — an MCP client reads line by line, and a buffered reply looks
/// exactly like a hung server.
///
/// # Errors
/// An I/O failure on stdin or stdout. A malformed *message* is not an error: it is answered
/// with `-32700` and the loop continues, because one bad line must not end the session.
pub async fn run_stdio(backend: Arc<dyn McpBackend>) -> anyhow::Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let Some(reply) = handle_line(backend.as_ref(), &line).await else {
            continue;
        };
        // COMPACT. `to_string`, never `to_string_pretty`: a message must not contain a
        // newline, because the newline is the frame.
        let mut wire = serde_json::to_string(&reply)?;
        debug_assert!(
            !wire.contains('\n'),
            "a message must occupy exactly one line"
        );
        wire.push('\n');
        out.write_all(wire.as_bytes()).await?;
        out.flush().await?;
    }
    tracing::debug!("stdin reached EOF; the MCP server is exiting");
    Ok(())
}

/// One line in, at most one message out. `None` means "that was a notification".
async fn handle_line(b: &dyn McpBackend, line: &str) -> Option<Value> {
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        // Protocol breakage, and the one case where the id is genuinely unknowable.
        Err(e) => {
            return Some(error_reply(
                Value::Null,
                &RpcError::parse_error(&e.to_string()),
            ))
        }
    };
    // No `id` ⇒ a notification (`notifications/initialized`, `notifications/cancelled`).
    // The spec says answer nothing at all, and a client that receives a response to a
    // notification treats the server as broken.
    let id = msg.get("id").cloned()?;
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
    match dispatch(b, method, params).await {
        Ok(result) => Some(json!({ "jsonrpc": "2.0", "id": id, "result": result })),
        Err(e) => Some(error_reply(id, &e)),
    }
}

/// A JSON-RPC error response.
fn error_reply(id: Value, e: &RpcError) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": e.to_value() })
}

/// Stamp `resultType: "complete"` on a result object.
///
/// Required by 2026-07-28 and ignored by every legacy client, so it goes on *every* result
/// rather than only when a modern client asked.
fn complete(mut v: Value) -> Value {
    if let Some(o) = v.as_object_mut() {
        o.insert("resultType".to_string(), json!("complete"));
    }
    v
}

/// **Transport-agnostic** dispatch, so stdio today and an axum route tomorrow share one
/// implementation.
///
/// `params` may carry a `_meta` member — the 2026-07-28 way of passing protocol version,
/// client identity and capabilities. It is **accepted and ignored**: nothing here reads it,
/// which is precisely what "accept and ignore" has to mean to be worth anything.
///
/// # Errors
/// [`RpcError`] for protocol breakage only — an unknown *method*. An unknown *tool*, a bad
/// argument, a refusal and a daemon that will not answer are all `Ok` results carrying
/// `isError: true`.
pub async fn dispatch(
    b: &dyn McpBackend,
    method: &str,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    match method {
        // The legacy handshake. Echoing the client's own revision back is the one trick
        // that makes this compatible with every revision at once.
        "initialize" => {
            let requested = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(LEGACY_PROTOCOL_VERSION);
            Ok(complete(json!({
                "protocolVersion": requested,
                "capabilities": { "tools": {} },
                "serverInfo": server_info(),
                "instructions": INSTRUCTIONS,
            })))
        }
        "ping" => Ok(complete(json!({}))),
        // The 2026-07-28 mandatory discovery method: ~10 lines, and a modern client probing
        // over stdio gets a deterministic answer instead of a `-32601`.
        "server/discover" => Ok(complete(json!({
            "supportedVersions": SUPPORTED_VERSIONS,
            "capabilities": { "tools": {} },
            "_meta": { "io.modelcontextprotocol/serverInfo": server_info() },
            "instructions": INSTRUCTIONS,
            "ttlMs": DISCOVER_TTL_MS,
            "cacheScope": "public",
        }))),
        "tools/list" => Ok(complete(json!({
            "tools": tools::definitions(),
            "ttlMs": TOOLS_TTL_MS,
            "cacheScope": "public",
        }))),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params
                .get("arguments")
                .cloned()
                .filter(Value::is_object)
                .unwrap_or_else(|| json!({}));
            Ok(complete(call_tool(b, name, &args).await))
        }
        other => Err(RpcError::method_not_found(other)),
    }
}

/// `{name, version}` for both handshakes.
fn server_info() -> Value {
    json!({
        "name": apexrouter_protocol::PRODUCT,
        "version": apexrouter_protocol::VERSION,
    })
}

/// Run one tool and shape the result. **Never returns an [`RpcError`]**: a tool that fails
/// is a result with `isError: true`, which is what lets an agent read the reason and retry.
async fn call_tool(b: &dyn McpBackend, name: &str, args: &Value) -> Value {
    match route(b, name, args).await {
        Ok(v) => json!({
            "content": [{ "type": "text", "text": render(&v) }],
            "isError": false,
        }),
        Err(e) => json!({
            "content": [{ "type": "text", "text": e.text() }],
            "isError": true,
        }),
    }
}

/// Tool name → trait method. The single place a name is spelled, so a transport other than
/// stdio needs no copy of this table.
async fn route(b: &dyn McpBackend, name: &str, a: &Value) -> ToolResult {
    match name {
        "apexrouter_status" => b.status().await,
        "apexrouter_models" => b.models().await,
        "apexrouter_rig" => b.rig().await,
        "apexrouter_fit" => b.fit(a).await,
        "apexrouter_up" => b.up(a).await,
        "apexrouter_endpoint_start" => b.endpoint_start(a).await,
        "apexrouter_endpoint_stop" => b.endpoint_stop(a).await,
        "apexrouter_swap" => b.swap(a).await,
        "apexrouter_logs" => b.logs(a).await,
        "apexrouter_backend_set" => b.backend_set(a).await,
        "apexrouter_route_set" => b.route_set(a).await,
        "apexrouter_recipe_list" => b.recipe_list().await,
        "apexrouter_recipe_save" => b.recipe_save(a).await,
        "apexrouter_recipe_run" => b.recipe_run(a).await,
        "apexrouter_usage" => b.usage(a).await,
        "apexrouter_smoke" => b.smoke(a).await,
        "apexrouter_diagnose" => b.diagnose(a).await,
        "apexrouter_hf_search" => b.hf_search(a).await,
        "apexrouter_hf_files" => b.hf_files(a).await,
        "apexrouter_hf_get" => b.hf_get(a).await,
        "apexrouter_vast_offers" => b.vast_offers(a).await,
        "apexrouter_vast_rent" => b.vast_rent(a).await,
        "apexrouter_vast_destroy" => b.vast_destroy(a).await,
        "apexrouter_compare" => b.compare(a).await,
        other => Err(ToolError::with_data(
            format!("unknown tool `{other}`"),
            json!({ "tools": tools::names() }),
        )),
    }
}

/// A tool's value as the text an agent reads.
///
/// A bare JSON string is passed through unquoted — otherwise a log tail would arrive
/// wrapped in quotes and escaped twice. Everything else is pretty-printed, which is legal
/// inside the message because the serialiser escapes the newlines.
fn render(v: &Value) -> String {
    match v.as_str() {
        Some(s) => s.to_string(),
        None => serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string()),
    }
}

#[cfg(test)]
mod tests {
    // The `$APEXROUTER_HOME` lock is a `std::sync::Mutex` held across `.await` on purpose,
    // exactly as `daemon.rs` does it: the environment is process-global, and an async mutex
    // would not exclude the synchronous tests that take the same lock.
    #![allow(clippy::await_holding_lock)]

    use super::*;
    use crate::cmd::Ctx;
    use apexrouter_core::config::Config;
    use apexrouter_core::paths::Paths;

    /// A backend that touches nothing, for the protocol-level tests.
    fn echo() -> Arc<dyn McpBackend> {
        backend::testing::echo()
    }

    /// The result of [`dispatch`] for a request that must not be a protocol error.
    async fn ok(method: &str, params: Value) -> Value {
        let b = echo();
        dispatch(b.as_ref(), method, params)
            .await
            .expect("this method must not be a protocol error")
    }

    #[tokio::test]
    async fn initialize_echoes_the_clients_requested_protocol_version() {
        for asked in ["2024-11-05", "2025-06-18", "2026-07-28", "1999-01-01"] {
            let r = ok("initialize", json!({ "protocolVersion": asked })).await;
            assert_eq!(
                r["protocolVersion"], asked,
                "the client's own revision must come straight back"
            );
        }
        // ...and with none asked for, the legacy one.
        let r = ok("initialize", json!({})).await;
        assert_eq!(r["protocolVersion"], LEGACY_PROTOCOL_VERSION);
        assert_eq!(r["serverInfo"]["name"], apexrouter_protocol::PRODUCT);
        assert_eq!(r["capabilities"]["tools"], json!({}));
        assert_eq!(r["resultType"], "complete");
    }

    #[tokio::test]
    async fn server_discover_answers_with_supported_versions() {
        let r = ok("server/discover", json!({})).await;
        let versions = r["supportedVersions"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(versions.iter().any(|v| v == "2026-07-28"));
        assert!(versions.iter().any(|v| v == "2024-11-05"));
        assert_eq!(r["resultType"], "complete");
        assert_eq!(r["cacheScope"], "public");
        assert_eq!(
            r["_meta"]["io.modelcontextprotocol/serverInfo"]["version"],
            apexrouter_protocol::VERSION
        );
    }

    #[tokio::test]
    async fn meta_is_accepted_and_ignored_on_every_request() {
        let meta = json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientInfo": { "name": "c", "version": "1" },
        });
        // A call carrying `_meta` is answered exactly like one without it.
        let bare = ok("tools/list", json!({})).await;
        let with_meta = ok("tools/list", json!({ "_meta": meta })).await;
        assert_eq!(bare, with_meta);

        let r = ok(
            "tools/call",
            json!({
                "name": "apexrouter_status",
                "arguments": {},
                "_meta": { "io.modelcontextprotocol/protocolVersion": "2026-07-28" },
            }),
        )
        .await;
        assert_eq!(r["isError"], json!(false));
    }

    #[tokio::test]
    async fn tools_list_is_every_tool_with_the_modern_cache_fields() {
        let r = ok("tools/list", json!({})).await;
        let listed = r["tools"].as_array().cloned().unwrap_or_default();
        assert_eq!(listed.len(), tools::names().len());
        assert_eq!(r["resultType"], "complete");
        assert!(r["ttlMs"].as_u64().unwrap_or(0) > 0);
    }

    /// The table in [`route`] must cover exactly the advertised tools — a name that lists
    /// but does not dispatch is the worst possible failure mode for an agent.
    #[tokio::test]
    async fn every_advertised_tool_dispatches() {
        let b = echo();
        for name in tools::names() {
            let r = call_tool(b.as_ref(), &name, &json!({})).await;
            assert_eq!(r["isError"], json!(false), "{name} did not dispatch: {r}");
        }
    }

    #[tokio::test]
    async fn an_unknown_tool_is_an_is_error_result_not_a_json_rpc_error() {
        let r = ok(
            "tools/call",
            json!({ "name": "apexrouter_frobnicate", "arguments": {} }),
        )
        .await;
        assert_eq!(r["isError"], json!(true));
        assert_eq!(r["resultType"], "complete");
        let text = r["content"][0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("unknown tool"), "{text}");
        assert!(text.contains("apexrouter_status"), "{text}");
    }

    #[tokio::test]
    async fn an_unknown_method_is_a_minus_32601() {
        let b = echo();
        let e = dispatch(b.as_ref(), "resources/list", json!({}))
            .await
            .expect_err("an unimplemented method is protocol breakage");
        assert_eq!(e.code, -32_601);
        assert!(e.message.contains("resources/list"), "{}", e.message);
    }

    #[tokio::test]
    async fn a_notification_gets_no_reply_at_all() {
        let b = echo();
        let reply = handle_line(
            b.as_ref(),
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        )
        .await;
        assert!(
            reply.is_none(),
            "a notification must be answered with silence"
        );
    }

    #[tokio::test]
    async fn a_line_that_is_not_json_is_a_minus_32700_with_a_null_id() {
        let b = echo();
        let reply = handle_line(b.as_ref(), "this is not json")
            .await
            .expect("parse errors are answered");
        assert_eq!(reply["error"]["code"], json!(-32_700));
        assert_eq!(reply["id"], Value::Null);
    }

    /// The stdio framing rule, asserted on real replies rather than trusted.
    #[tokio::test]
    async fn every_message_serialises_to_exactly_one_line() {
        let b = echo();
        for line in [
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"apexrouter_rig"}}"#,
            r#"{"jsonrpc":"2.0","id":4,"method":"server/discover"}"#,
            r#"{"jsonrpc":"2.0","id":5,"method":"resources/list"}"#,
            "not json at all",
        ] {
            let reply = handle_line(b.as_ref(), line).await.expect("a reply");
            let wire = serde_json::to_string(&reply).expect("compact");
            assert!(
                !wire.contains('\n'),
                "a message with an embedded newline breaks the framing: {wire}"
            );
            assert_eq!(reply["jsonrpc"], "2.0");
        }
    }

    // ---- the LocalBackend contract, with nothing running ------------------------------

    /// A `LocalBackend` rooted at a temp `$STATE`, with no daemon anywhere.
    ///
    /// Hermetic by construction: `$APEXROUTER_URL` is cleared, so nothing can dial out.
    fn local_at(dir: &std::path::Path) -> LocalBackend {
        std::env::set_var("APEXROUTER_HOME", dir);
        std::env::remove_var("APEXROUTER_URL");
        std::env::remove_var("APEXROUTER_TOKEN");
        let paths = Paths::resolve().expect("paths");
        paths.ensure_layout().expect("layout");
        LocalBackend::from_ctx(Ctx {
            paths,
            cfg: Config::default(),
            autostart: false,
        })
    }

    #[tokio::test]
    async fn read_only_tools_answer_with_the_daemon_down() {
        let _guard = crate::daemon::testenv::lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let b = local_at(dir.path());

        let status = b.status().await.expect("status answers offline");
        assert_eq!(status["served_by"], "offline");
        let base = status["how_to_use"]["openai_base_url"]
            .as_str()
            .unwrap_or_default();
        assert!(base.starts_with("http://127.0.0.1:"), "{base}");
        assert!(base.ends_with("/v1"), "{base}");

        let recipes = b.recipe_list().await.expect("recipes answer offline");
        assert!(recipes["recipes"].is_array());

        let usage = b
            .usage(&json!({ "since": "24h", "by": "day" }))
            .await
            .expect("usage answers offline");
        assert!(usage["summary"].is_object());

        std::env::remove_var("APEXROUTER_HOME");
    }

    #[tokio::test]
    async fn a_mutation_with_the_daemon_down_says_what_to_run() {
        let _guard = crate::daemon::testenv::lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let b = local_at(dir.path());

        let e = b
            .up(&json!({ "model": "anything" }))
            .await
            .expect_err("a mutation cannot be served offline");
        assert!(e.message.contains("apexrouter serve"), "{}", e.message);
        assert!(e.message.contains("--proxy"), "{}", e.message);

        // ...and it arrives at the client as a result, not as a transport error.
        let r = call_tool(&b, "apexrouter_up", &json!({ "model": "anything" })).await;
        assert_eq!(r["isError"], json!(true));

        std::env::remove_var("APEXROUTER_HOME");
    }

    /// The money rule, end to end and hermetically: no daemon, no network, no spend.
    #[tokio::test]
    async fn vast_rent_without_confirm_returns_the_cost_preview_and_credit() {
        let _guard = crate::daemon::testenv::lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let b = local_at(dir.path());

        let r = call_tool(
            &b,
            "apexrouter_vast_rent",
            &json!({ "launch": {}, "max_usd_per_hour": 2.5 }),
        )
        .await;
        assert_eq!(r["isError"], json!(true), "an unconfirmed rent must refuse");
        let text = r["content"][0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("created NOTHING"), "{text}");
        assert!(text.contains("cost_preview"), "{text}");
        assert!(text.contains("credit_usd"), "{text}");
        assert!(text.contains("est_total_24h_usd"), "{text}");
        assert!(text.contains("to_proceed"), "{text}");

        std::env::remove_var("APEXROUTER_HOME");
    }

    /// A destroy without `confirm` destroys nothing, with no daemon involved either.
    #[tokio::test]
    async fn vast_destroy_without_confirm_destroys_nothing() {
        let _guard = crate::daemon::testenv::lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let b = local_at(dir.path());

        let r = call_tool(&b, "apexrouter_vast_destroy", &json!({ "id": "12345" })).await;
        assert_eq!(r["isError"], json!(true));
        let text = r["content"][0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("destroyed NOTHING"), "{text}");

        std::env::remove_var("APEXROUTER_HOME");
    }
}
