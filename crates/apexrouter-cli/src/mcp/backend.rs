//! OWNER: unit M-01 (cli/src/mcp/**, cli/src/cmd/mcp.rs). Do not edit outside that unit.
//!
//! The two ways an MCP tool call can be answered.
//!
//! [`LocalBackend`] answers `Pure`/`ReadState` tools directly from `apexrouter-core` **even
//! when the daemon is down**, and returns a helpful `isError` result for mutations
//! ("run `apexrouter serve`"). [`ProxyBackend`] forwards to `$APEXROUTER_URL` with
//! `$APEXROUTER_TOKEN`.
//!
//! Three rules this module keeps, because an MCP client is the least forgiving surface we
//! have:
//!
//! * **A tool failure is data, not a transport error.** Every method returns
//!   [`ToolResult`]; the JSON-RPC layer turns `Err` into `isError: true` with the message
//!   and any structured detail attached, and never into a `-32xxx` code.
//! * **Nothing here spends money without `confirm`.** [`McpBackend::vast_rent`] refuses
//!   without `confirm: true` *and* a positive `max_usd_per_hour`, and the refusal is a
//!   priced dry run: it carries $/hr, the 1 h and 24 h projections, the daemon's hard
//!   ceiling and the account credit. On that path **no create endpoint is called at all**,
//!   so the refusal cannot itself cost anything.
//! * **`LocalBackend` never autostarts a daemon.** An MCP server is a subprocess of an
//!   agent harness; forking a long-lived daemon out from under it because a tool was called
//!   is exactly the kind of surprise §1.4 exists to prevent. Read-only tools answer from
//!   `$STATE`, mutations say what to run.

use crate::cmd::{models, rig as rig_cmd, status as status_cmd, Ctx};
use crate::daemon::{self, Need, Serving};
use apexrouter_client::NodeClient;
use apexrouter_core::config::Config;
use apexrouter_core::paths::Paths;
use apexrouter_core::store::Store;
use apexrouter_core::{catalog, discover, fit as solver, usage};
use apexrouter_protocol::{
    BackendId, EndpointSpec, FitInput, KvType, LlamaBuild, LocalLlamaSpec, LocalModel, NglPlan,
    RigSnapshot, SamplingMode, SplitMode, SplitPlan,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

/// How many trailing log lines `apexrouter_logs` returns when the caller does not say.
const DEFAULT_TAIL: usize = 200;
/// The window `apexrouter_usage` aggregates over when the caller does not say.
const DEFAULT_SINCE: &str = "24h";
/// Cap on `tail`, so a runaway argument cannot push a 200 MB log through a JSON-RPC pipe.
const MAX_TAIL: usize = 5_000;

// ----------------------------------------------------------------------------------------
// the result type
// ----------------------------------------------------------------------------------------

/// A tool-level failure: a **result** with `isError: true`, never a JSON-RPC error.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolError {
    /// What went wrong, in words an agent can act on.
    pub message: String,
    /// Structured detail rendered under the message — the cost preview, a candidate list,
    /// the fields to resend.
    pub data: Option<Value>,
}

impl ToolError {
    /// A failure with nothing but a message.
    pub fn msg(m: impl Into<String>) -> ToolError {
        ToolError {
            message: m.into(),
            data: None,
        }
    }

    /// A failure carrying structured detail, which the text rendering appends as pretty
    /// JSON so a human reading the transcript sees the numbers too.
    pub fn with_data(m: impl Into<String>, data: Value) -> ToolError {
        ToolError {
            message: m.into(),
            data: Some(data),
        }
    }

    /// What goes in the `content[0].text` of the `isError` result.
    pub fn text(&self) -> String {
        match &self.data {
            None => self.message.clone(),
            Some(d) => format!(
                "{}\n\n{}",
                self.message,
                serde_json::to_string_pretty(d).unwrap_or_else(|_| d.to_string())
            ),
        }
    }
}

/// What every tool method returns. `Ok` is the JSON an agent reads; `Err` is an `isError`.
pub type ToolResult = std::result::Result<Value, ToolError>;

/// The message a mutation gets when nothing is running. Names the verb to type.
fn no_daemon() -> ToolError {
    ToolError::msg(
        "this tool needs a running ApexRouter daemon and there is none. Start one with \
         `apexrouter serve --detach`, or point this MCP server at an existing node with \
         `apexrouter mcp --proxy http://127.0.0.1:2739` (or $APEXROUTER_URL). Read-only \
         tools — status, models, rig, fit, logs, recipe_list, usage — answer from $STATE \
         without a daemon.",
    )
}

/// A client failure, with the path and the body prefix `NodeClient` already collected.
fn client_err(e: apexrouter_client::Error) -> ToolError {
    ToolError::msg(format!("the daemon refused or could not answer: {e}"))
}

/// An `apexrouter-core` failure.
fn core_err(e: impl std::fmt::Display) -> ToolError {
    ToolError::msg(format!("{e}"))
}

// ----------------------------------------------------------------------------------------
// argument helpers — MCP arguments are a `Value`, and every read of one is fallible
// ----------------------------------------------------------------------------------------

/// A string argument, treating `""` as absent.
pub fn arg_str(a: &Value, key: &str) -> Option<String> {
    a.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// A string argument that must be there.
pub fn need_str(a: &Value, key: &str) -> std::result::Result<String, ToolError> {
    arg_str(a, key)
        .ok_or_else(|| ToolError::msg(format!("`{key}` is required and must be a string")))
}

/// A `u32` argument. A negative or oversized number is treated as absent rather than
/// silently wrapped.
pub fn arg_u32(a: &Value, key: &str) -> Option<u32> {
    a.get(key)
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
}

/// A `usize` argument.
pub fn arg_usize(a: &Value, key: &str) -> Option<usize> {
    a.get(key)
        .and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())
}

/// An `f64` argument.
pub fn arg_f64(a: &Value, key: &str) -> Option<f64> {
    a.get(key).and_then(Value::as_f64)
}

/// A boolean argument. **Only a real JSON `true` counts** — this is what makes
/// `confirm: "true"` not spend money.
pub fn arg_bool(a: &Value, key: &str) -> Option<bool> {
    a.get(key).and_then(Value::as_bool)
}

/// A string-array argument, dropping blanks.
pub fn arg_strs(a: &Value, key: &str) -> Vec<String> {
    a.get(key)
        .and_then(Value::as_array)
        .map(|xs| {
            xs.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Percent-encode a path segment enough for an id or alias: our ids are already restricted
/// to `[a-z0-9._-]`, so this only ever has to defend against a caller who ignored that.
fn seg(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '~') {
                c.to_string()
            } else {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf)
                    .as_bytes()
                    .iter()
                    .map(|b| format!("%{b:02X}"))
                    .collect()
            }
        })
        .collect()
}

/// `["a=1", "b=2"]` into `"?a=1&b=2"`, or `""` when there is nothing to add.
fn query(parts: &[String]) -> String {
    if parts.is_empty() {
        String::new()
    } else {
        format!("?{}", parts.join("&"))
    }
}

// ----------------------------------------------------------------------------------------
// the trait
// ----------------------------------------------------------------------------------------

/// One method per tool in `ARCHITECTURE.md` §8.
///
/// The dispatcher owns the name → method mapping, so the trait never sees a tool name and
/// a transport other than stdio needs no changes here.
#[async_trait]
pub trait McpBackend: Send + Sync {
    /// `apexrouter_status` — the whole picture, plus how to point a client at it.
    async fn status(&self) -> ToolResult;
    /// `apexrouter_models` — what to put in `"model"`.
    async fn models(&self) -> ToolResult;
    /// `apexrouter_rig` — GPUs, builds, RAM, swap.
    async fn rig(&self) -> ToolResult;
    /// `apexrouter_fit` — will it fit, and why.
    async fn fit(&self, a: &Value) -> ToolResult;
    /// `apexrouter_up` — the one-call happy path.
    async fn up(&self, a: &Value) -> ToolResult;
    /// `apexrouter_endpoint_start` — the same thing with every knob exposed.
    async fn endpoint_start(&self, a: &Value) -> ToolResult;
    /// `apexrouter_endpoint_stop`.
    async fn endpoint_stop(&self, a: &Value) -> ToolResult;
    /// `apexrouter_swap` — atomic model swap behind a stable alias.
    async fn swap(&self, a: &Value) -> ToolResult;
    /// `apexrouter_logs` — the call to make when a start failed.
    async fn logs(&self, a: &Value) -> ToolResult;
    /// `apexrouter_backend_set` — quarantine or re-tag.
    async fn backend_set(&self, a: &Value) -> ToolResult;
    /// `apexrouter_route_set` — point an alias.
    async fn route_set(&self, a: &Value) -> ToolResult;
    /// `apexrouter_recipe_list`.
    async fn recipe_list(&self) -> ToolResult;
    /// `apexrouter_recipe_save`.
    async fn recipe_save(&self, a: &Value) -> ToolResult;
    /// `apexrouter_recipe_run`.
    async fn recipe_run(&self, a: &Value) -> ToolResult;
    /// `apexrouter_usage`.
    async fn usage(&self, a: &Value) -> ToolResult;
    /// `apexrouter_smoke`.
    async fn smoke(&self, a: &Value) -> ToolResult;
    /// `apexrouter_diagnose`.
    async fn diagnose(&self, a: &Value) -> ToolResult;
    /// `apexrouter_hf_search`.
    async fn hf_search(&self, a: &Value) -> ToolResult;
    /// `apexrouter_hf_files`.
    async fn hf_files(&self, a: &Value) -> ToolResult;
    /// `apexrouter_hf_get`.
    async fn hf_get(&self, a: &Value) -> ToolResult;
    /// `apexrouter_vast_offers` — read-only, free, safe.
    async fn vast_offers(&self, a: &Value) -> ToolResult;
    /// `apexrouter_vast_rent` — **spends money**, and only with `confirm: true`.
    async fn vast_rent(&self, a: &Value) -> ToolResult;
    /// `apexrouter_vast_destroy` — only with `confirm: true`.
    async fn vast_destroy(&self, a: &Value) -> ToolResult;
    /// `apexrouter_compare`.
    async fn compare(&self, a: &Value) -> ToolResult;
}

// ----------------------------------------------------------------------------------------
// LocalBackend
// ----------------------------------------------------------------------------------------

/// Answers from `apexrouter-core` and `$STATE`, with no daemon required.
///
/// When a daemon *is* running it is preferred for everything except log tailing, because
/// only the daemon knows live health, throughput and spend. When one is not, the read-only
/// tools still answer — from the routing table, the endpoint records, the catalogue, the
/// usage log and a live rig scan — and every mutation returns [`no_daemon`].
pub struct LocalBackend {
    /// Paths, config and the autostart flag, resolved once at construction.
    ctx: Ctx,
}

impl LocalBackend {
    /// Resolve paths and config the way every other subcommand does.
    ///
    /// # Errors
    /// When no home directory can be determined, or `config.toml` exists but will not parse.
    pub fn load() -> anyhow::Result<LocalBackend> {
        Ok(LocalBackend {
            ctx: Ctx {
                paths: Paths::resolve()?,
                cfg: Config::load()?,
                // An MCP server is somebody else's subprocess. It never forks a daemon.
                autostart: false,
            },
        })
    }

    /// Build one over an already-resolved context. The seam the tests use.
    pub fn from_ctx(ctx: Ctx) -> LocalBackend {
        LocalBackend { ctx }
    }

    /// The daemon, or `$STATE`, resolved without ever autostarting.
    async fn serving(&self) -> Serving {
        match daemon::resolve_serving(Need::ReadState, &self.ctx.cfg, &self.ctx.paths, false).await
        {
            Ok(s) => s,
            Err(e) => Serving::None(e),
        }
    }

    /// A client for a daemon that is already running, or `None`.
    async fn client(&self) -> Option<NodeClient> {
        match self.serving().await {
            Serving::Daemon(c) => Some(c),
            Serving::Offline(_) | Serving::None(_) => None,
        }
    }

    /// A client, or the "start one" refusal.
    async fn need_daemon(&self) -> std::result::Result<NodeClient, ToolError> {
        self.client().await.ok_or_else(no_daemon)
    }

    /// `$STATE`, under whichever lock the reader takes.
    fn store(&self) -> Store {
        Store::new(self.ctx.paths.clone())
    }

    /// The offline picture: facts from disk, poller-derived fields left at zero.
    fn offline_snapshot(&self) -> std::result::Result<Value, ToolError> {
        let store = self.store();
        let snap = status_cmd::offline_snapshot(&self.ctx, &store).map_err(core_err)?;
        serde_json::to_value(snap).map_err(core_err)
    }

    /// Every local GGUF, from a live scan.
    async fn local_models(&self) -> std::result::Result<Vec<LocalModel>, ToolError> {
        let serving = self.serving().await;
        models::load(&self.ctx, &serving).await.map_err(core_err)
    }

    /// The rig, from the daemon when there is one and from a live scan when there is not.
    async fn rig_snapshot(&self) -> std::result::Result<RigSnapshot, ToolError> {
        let serving = self.serving().await;
        rig_cmd::load(&self.ctx, &serving, false)
            .await
            .map_err(core_err)
    }

    /// `apexrouter_fit` with nothing running: resolve the model, scan the rig, build a
    /// **single-backend** budget minus what running endpoints reserve, and solve.
    async fn local_fit(&self, a: &Value) -> ToolResult {
        let want = need_str(a, "model")?;
        let all = self.local_models().await?;
        let model = models::resolve_model(&all, &want).map_err(core_err)?;

        let rig = self.rig_snapshot().await?;
        let asked = arg_strs(a, "devices");
        let devices = if asked.is_empty() {
            default_devices(&rig)
        } else {
            asked
        };
        let running = self.store().list_endpoints().unwrap_or_default();
        // `BackendScope::Auto` picks ONE compute backend. A budget is never a sum across
        // backends: the single card on this box is both `ROCm0` and `Vulkan0`.
        let budget = solver::budget_from_rig(
            &rig,
            solver::BackendScope::Auto,
            &devices,
            self.ctx.cfg.endpoints.vram_margin_mb,
            &running,
        );

        let gguf = model.gguf.clone().ok_or_else(|| {
            ToolError::msg(format!(
                "the GGUF header of {} could not be read, so there is nothing to solve — fit \
                 needs n_layer, n_head_kv and n_embd_head_k/v",
                model.name
            ))
        })?;
        let split = SplitPlan {
            devices: if devices.is_empty() {
                budget.device_names()
            } else {
                devices
            },
            mode: SplitMode::Layer,
            main_gpu: None,
            tensor_split: None,
        };
        let plan = solver::fit(&FitInput {
            weights_bytes: model.total_bytes,
            gguf,
            budget,
            want_ctx: arg_u32(a, "ctx"),
            want_parallel: arg_u32(a, "parallel"),
            want_kv: parse_kv(a)?,
            split,
            batch: arg_u32(a, "batch"),
        });
        Ok(json!({
            "served_by": "offline",
            "model": { "id": model.id, "name": model.name, "bytes": model.total_bytes },
            "plan": plan,
        }))
    }

    /// The tail of a local endpoint's log, read straight off disk.
    ///
    /// Deliberately local even when a daemon is running: `GET /v1/backends/{id}/logs`
    /// answers `text/plain`, and the file is right here.
    fn local_logs(&self, a: &Value) -> ToolResult {
        let id = need_str(a, "id")?;
        let tail = arg_usize(a, "tail").unwrap_or(DEFAULT_TAIL).min(MAX_TAIL);
        let records = self.store().list_endpoints().unwrap_or_default();
        let parsed = BackendId::parse(&id)
            .map_err(|e| ToolError::msg(format!("`{id}` is not a valid endpoint id: {e}")))?;
        let path = records
            .iter()
            .find(|r| r.id == parsed)
            .and_then(|r| r.log_path.clone())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| self.ctx.paths.log_file(&parsed));

        let text = std::fs::read_to_string(&path).map_err(|e| {
            let known: Vec<&str> = records.iter().map(|r| r.id.as_str()).collect();
            ToolError::with_data(
                format!("could not read {}: {e}", path.display()),
                json!({ "known_endpoints": known }),
            )
        })?;
        let lines: Vec<&str> = text.lines().collect();
        let from = lines.len().saturating_sub(tail);
        Ok(json!({
            "id": id,
            "path": path.display().to_string(),
            "total_lines": lines.len(),
            "lines": lines[from..],
        }))
    }

    /// Recipes and search profiles, read from `$STATE/catalog.json`.
    fn local_recipes(&self) -> ToolResult {
        let cat = catalog::load(&self.ctx.paths).map_err(core_err)?;
        Ok(json!({
            "served_by": "offline",
            "recipes": cat.recipes,
            "profiles": cat.profiles,
        }))
    }

    /// Tokens, cost and tok/s from the append-only usage log.
    fn local_usage(&self, a: &Value) -> ToolResult {
        let since_spec = arg_str(a, "since").unwrap_or_else(|| DEFAULT_SINCE.to_string());
        let since = parse_since(&since_spec)?;
        let by = parse_group_by(arg_str(a, "by").as_deref())?;
        let rows = usage::read_all(&self.ctx.paths, &self.ctx.cfg.compat).map_err(core_err)?;
        Ok(json!({
            "served_by": "offline",
            "since": since_spec,
            "by": arg_str(a, "by").unwrap_or_else(|| "provider".to_string()),
            "summary": usage::aggregate(&rows, since, by),
        }))
    }
}

#[async_trait]
impl McpBackend for LocalBackend {
    async fn status(&self) -> ToolResult {
        match self.client().await {
            Some(c) => remote::status(&c).await,
            None => Ok(status_payload(&self.offline_snapshot()?)),
        }
    }

    async fn models(&self) -> ToolResult {
        match self.client().await {
            Some(c) => remote::models(&c).await,
            None => {
                let local = serde_json::to_value(self.local_models().await?).map_err(core_err)?;
                Ok(models_payload(&self.offline_snapshot()?, &local))
            }
        }
    }

    async fn rig(&self) -> ToolResult {
        let rig = self.rig_snapshot().await?;
        Ok(rig_payload(&serde_json::to_value(rig).map_err(core_err)?))
    }

    async fn fit(&self, a: &Value) -> ToolResult {
        match self.client().await {
            Some(c) => remote::fit(&c, a).await,
            None => self.local_fit(a).await,
        }
    }

    async fn up(&self, a: &Value) -> ToolResult {
        let c = self.need_daemon().await?;
        remote::up(&c, a, sampling_mode(&self.ctx.cfg.endpoints.default_mode)).await
    }

    async fn endpoint_start(&self, a: &Value) -> ToolResult {
        remote::endpoint_start(&self.need_daemon().await?, a).await
    }

    async fn endpoint_stop(&self, a: &Value) -> ToolResult {
        remote::endpoint_stop(&self.need_daemon().await?, a).await
    }

    async fn swap(&self, a: &Value) -> ToolResult {
        remote::swap(&self.need_daemon().await?, a).await
    }

    async fn logs(&self, a: &Value) -> ToolResult {
        self.local_logs(a)
    }

    async fn backend_set(&self, a: &Value) -> ToolResult {
        remote::backend_set(&self.need_daemon().await?, a).await
    }

    async fn route_set(&self, a: &Value) -> ToolResult {
        remote::route_set(&self.need_daemon().await?, a).await
    }

    async fn recipe_list(&self) -> ToolResult {
        match self.client().await {
            Some(c) => remote::recipe_list(&c).await,
            None => self.local_recipes(),
        }
    }

    async fn recipe_save(&self, a: &Value) -> ToolResult {
        remote::recipe_save(&self.need_daemon().await?, a).await
    }

    async fn recipe_run(&self, a: &Value) -> ToolResult {
        remote::recipe_run(&self.need_daemon().await?, a).await
    }

    async fn usage(&self, a: &Value) -> ToolResult {
        match self.client().await {
            Some(c) => remote::usage(&c, a).await,
            None => self.local_usage(a),
        }
    }

    async fn smoke(&self, a: &Value) -> ToolResult {
        remote::smoke(&self.need_daemon().await?, a).await
    }

    async fn diagnose(&self, a: &Value) -> ToolResult {
        remote::diagnose(&self.need_daemon().await?, a).await
    }

    async fn hf_search(&self, a: &Value) -> ToolResult {
        remote::hf_search(&self.need_daemon().await?, a).await
    }

    async fn hf_files(&self, a: &Value) -> ToolResult {
        remote::hf_files(&self.need_daemon().await?, a).await
    }

    async fn hf_get(&self, a: &Value) -> ToolResult {
        remote::hf_get(&self.need_daemon().await?, a).await
    }

    async fn vast_offers(&self, a: &Value) -> ToolResult {
        remote::vast_offers(&self.need_daemon().await?, a).await
    }

    /// The money path. The refusal is built **before** any daemon is required, so it works
    /// with nothing running, and no create endpoint is reachable from it.
    async fn vast_rent(&self, a: &Value) -> ToolResult {
        if !approved(a) {
            let account = match self.client().await {
                Some(c) => c.get::<Value>("/v1/vast/account").await.ok(),
                None => None,
            };
            return Err(rent_preview(
                a,
                Some(self.ctx.cfg.vast.max_usd_per_hour_ceiling),
                self.ctx.cfg.vast.require_human_confirm,
                account,
            ));
        }
        remote::vast_rent_confirmed(&self.need_daemon().await?, a).await
    }

    async fn vast_destroy(&self, a: &Value) -> ToolResult {
        if arg_bool(a, "confirm") != Some(true) {
            return Err(destroy_refusal(a));
        }
        remote::vast_destroy_confirmed(&self.need_daemon().await?, a).await
    }

    async fn compare(&self, a: &Value) -> ToolResult {
        remote::compare(&self.need_daemon().await?, a).await
    }
}

// ----------------------------------------------------------------------------------------
// ProxyBackend
// ----------------------------------------------------------------------------------------

/// Forwards every tool to a running daemon's control plane.
///
/// Selected by `--proxy URL` / `-p URL` / `$APEXROUTER_URL`, with the bearer read from
/// `$APEXROUTER_TOKEN`. It holds no config and no `$STATE`, so it has no offline mode: the
/// node it points at may not even be this machine.
pub struct ProxyBackend {
    /// The control-plane client.
    client: NodeClient,
}

impl ProxyBackend {
    /// Point one at a control-plane URL, with an optional bearer.
    pub fn new(url: &str, token: Option<String>) -> ProxyBackend {
        ProxyBackend {
            client: NodeClient::new(url, token),
        }
    }

    /// The URL this proxy talks to. Handy in a startup log line on stderr.
    pub fn base(&self) -> &str {
        self.client.base()
    }
}

#[async_trait]
impl McpBackend for ProxyBackend {
    async fn status(&self) -> ToolResult {
        remote::status(&self.client).await
    }

    async fn models(&self) -> ToolResult {
        remote::models(&self.client).await
    }

    async fn rig(&self) -> ToolResult {
        let rig: Value = self.client.get("/v1/rig").await.map_err(client_err)?;
        Ok(rig_payload(&rig))
    }

    async fn fit(&self, a: &Value) -> ToolResult {
        remote::fit(&self.client, a).await
    }

    async fn up(&self, a: &Value) -> ToolResult {
        remote::up(&self.client, a, SamplingMode::Thinking).await
    }

    async fn endpoint_start(&self, a: &Value) -> ToolResult {
        remote::endpoint_start(&self.client, a).await
    }

    async fn endpoint_stop(&self, a: &Value) -> ToolResult {
        remote::endpoint_stop(&self.client, a).await
    }

    async fn swap(&self, a: &Value) -> ToolResult {
        remote::swap(&self.client, a).await
    }

    /// The one tool a proxy cannot serve: `GET /v1/backends/{id}/logs` answers `text/plain`
    /// and [`NodeClient`] has no raw-text getter, so this names the URL to curl instead of
    /// returning a decode failure nobody can act on.
    async fn logs(&self, a: &Value) -> ToolResult {
        let id = need_str(a, "id")?;
        let tail = arg_usize(a, "tail").unwrap_or(DEFAULT_TAIL).min(MAX_TAIL);
        Err(ToolError::with_data(
            "log tailing is not available through --proxy: the control plane answers \
             text/plain and this client only decodes JSON. Run the MCP server on the node \
             itself (`apexrouter mcp`, no --proxy) or fetch the URL below directly.",
            json!({
                "url": format!("{}/v1/backends/{}/logs?tail={tail}", self.client.base(), seg(&id)),
                "id": id,
            }),
        ))
    }

    async fn backend_set(&self, a: &Value) -> ToolResult {
        remote::backend_set(&self.client, a).await
    }

    async fn route_set(&self, a: &Value) -> ToolResult {
        remote::route_set(&self.client, a).await
    }

    async fn recipe_list(&self) -> ToolResult {
        remote::recipe_list(&self.client).await
    }

    async fn recipe_save(&self, a: &Value) -> ToolResult {
        remote::recipe_save(&self.client, a).await
    }

    async fn recipe_run(&self, a: &Value) -> ToolResult {
        remote::recipe_run(&self.client, a).await
    }

    async fn usage(&self, a: &Value) -> ToolResult {
        remote::usage(&self.client, a).await
    }

    async fn smoke(&self, a: &Value) -> ToolResult {
        remote::smoke(&self.client, a).await
    }

    async fn diagnose(&self, a: &Value) -> ToolResult {
        remote::diagnose(&self.client, a).await
    }

    async fn hf_search(&self, a: &Value) -> ToolResult {
        remote::hf_search(&self.client, a).await
    }

    async fn hf_files(&self, a: &Value) -> ToolResult {
        remote::hf_files(&self.client, a).await
    }

    async fn hf_get(&self, a: &Value) -> ToolResult {
        remote::hf_get(&self.client, a).await
    }

    async fn vast_offers(&self, a: &Value) -> ToolResult {
        remote::vast_offers(&self.client, a).await
    }

    async fn vast_rent(&self, a: &Value) -> ToolResult {
        if !approved(a) {
            let account = self.client.get::<Value>("/v1/vast/account").await.ok();
            // A remote node's ceiling is its own business; the refusal says so rather than
            // quoting this machine's config at a daemon that will not use it.
            return Err(rent_preview(a, None, false, account));
        }
        remote::vast_rent_confirmed(&self.client, a).await
    }

    async fn vast_destroy(&self, a: &Value) -> ToolResult {
        if arg_bool(a, "confirm") != Some(true) {
            return Err(destroy_refusal(a));
        }
        remote::vast_destroy_confirmed(&self.client, a).await
    }

    async fn compare(&self, a: &Value) -> ToolResult {
        remote::compare(&self.client, a).await
    }
}

/// Either backend, boxed for the dispatcher.
pub type SharedBackend = Arc<dyn McpBackend>;

// ----------------------------------------------------------------------------------------
// the money gate
// ----------------------------------------------------------------------------------------

/// Has the caller supplied **both** halves of the spend gate?
///
/// `confirm` must be a real JSON `true` and `max_usd_per_hour` a positive number. A string
/// `"true"`, a `1`, or a zero ceiling all fail this, which is the point.
fn approved(a: &Value) -> bool {
    arg_bool(a, "confirm") == Some(true)
        && matches!(arg_f64(a, "max_usd_per_hour"), Some(m) if m > 0.0)
}

/// The refusal that doubles as a dry run: what it would cost, what the ceiling is, what
/// credit is left, and exactly what to resend.
///
/// **Nothing is created on this path**, and no create endpoint is called to build it.
fn rent_preview(
    a: &Value,
    ceiling: Option<f64>,
    require_human_confirm: bool,
    account: Option<Value>,
) -> ToolError {
    let asked = arg_f64(a, "max_usd_per_hour").filter(|m| *m > 0.0);
    let rate = match (asked, ceiling) {
        (Some(m), Some(c)) => Some(m.min(c)),
        (Some(m), None) => Some(m),
        (None, c) => c,
    };
    let rate_source = match (asked, ceiling) {
        (Some(_), _) => "your max_usd_per_hour, capped by the daemon ceiling",
        (None, Some(_)) => "the daemon's ceiling — you did not name a rate",
        (None, None) => "unknown: name max_usd_per_hour to price this",
    };
    let credit = account
        .as_ref()
        .and_then(|v| v.get("credit"))
        .and_then(Value::as_f64);
    let hours_of_credit = match (credit, rate) {
        (Some(c), Some(r)) if r > 0.0 => Some(c / r),
        _ => None,
    };

    ToolError::with_data(
        "apexrouter_vast_rent created NOTHING. Renting needs `confirm: true` AND a positive \
         `max_usd_per_hour`, and at most one of them was present. Below is the bill you \
         would be signing — show it to the human and get an explicit yes before resending \
         with both fields set.",
        json!({
            "created": false,
            "spent_usd": 0.0,
            "cost_preview": {
                "usd_per_hour": rate,
                "rate_source": rate_source,
                "est_total_1h_usd": rate,
                "est_total_24h_usd": rate.map(|r| r * 24.0),
                "note": "vast bills by the second while the instance exists, running or not. \
                         Destroy is what stops the meter.",
            },
            "daemon_ceiling_usd_per_hour": ceiling,
            "requires_human_approval": require_human_confirm,
            "credit_usd": credit,
            "credit_source": match (&account, credit) {
                (Some(_), Some(_)) => "vast.ai GET /users/current, read live",
                (Some(_), None) => "the daemon answered but reported no credit field",
                (None, _) => "unavailable — no daemon is running, so the account was not read",
            },
            "hours_of_credit_at_this_rate": hours_of_credit,
            "to_proceed": {
                "confirm": true,
                "max_usd_per_hour": rate,
                "and": "resend every other argument (profile or offer_id, launch, \
                        auto_tunnel, bind_alias) exactly as you sent it here",
            },
        }),
    )
}

/// The destroy refusal. Destroying is what stops the meter, so the message says so.
fn destroy_refusal(a: &Value) -> ToolError {
    ToolError::with_data(
        "apexrouter_vast_destroy destroyed NOTHING: `confirm: true` is required. Destroying \
         is irreversible and the box's disk goes with it, so confirm it with the human \
         first — but note that an instance nobody is using is still billing, and destroying \
         is what stops the meter.",
        json!({
            "destroyed": false,
            "id": arg_str(a, "id"),
            "to_proceed": { "id": arg_str(a, "id"), "confirm": true },
        }),
    )
}

// ----------------------------------------------------------------------------------------
// payload shaping — shared by the local and remote paths so both answer the same shape
// ----------------------------------------------------------------------------------------

/// Wrap a snapshot with the four strings an agent needs to make a request.
///
/// This is the whole reason `apexrouter_status` exists: §8 asks that an agent get from this
/// call to a working `OPENAI_BASE_URL` without reading a doc.
pub fn status_payload(snap: &Value) -> Value {
    let base = snap
        .pointer("/proxy/base_url")
        .and_then(Value::as_str)
        .unwrap_or("http://127.0.0.1:8888/v1");
    let alias = snap
        .pointer("/proxy/default_alias")
        .and_then(Value::as_str)
        .unwrap_or("auto");
    let served_by = snap
        .get("served_by")
        .and_then(Value::as_str)
        .unwrap_or("offline");
    json!({
        "how_to_use": {
            "openai_base_url": base,
            "openai_api_key": "any non-empty string, unless [server] token_env is configured",
            "model": alias,
            "env": { "OPENAI_BASE_URL": base, "OPENAI_API_KEY": "sk-local" },
            "anthropic_base_url": base.strip_suffix("/v1").unwrap_or(base),
            "note": "`model` may be any alias below; an unknown or absent one falls through \
                     to the default alias.",
        },
        "served_by": served_by,
        "stale": snap.get("stale").cloned().unwrap_or(Value::Bool(false)),
        "snapshot": snap,
    })
}

/// The model catalogue: what to put in `"model"`, and what could be started.
pub fn models_payload(snap: &Value, local: &Value) -> Value {
    let empty = Vec::new();
    let backends = snap
        .get("backends")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let mut routable: Vec<Value> = Vec::new();
    for b in backends {
        if b.get("enabled").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        let id = b.get("id").and_then(Value::as_str).unwrap_or("");
        let tps = b.pointer("/health/ready/tps_p50").cloned();
        for m in b.get("models").and_then(Value::as_array).unwrap_or(&empty) {
            routable.push(json!({
                "model": m.get("id").cloned().unwrap_or(Value::Null),
                "backend": id,
                "ctx": m.get("ctx").cloned().unwrap_or(Value::Null),
                "vision": m.get("vision").cloned().unwrap_or(Value::Null),
                "tools": m.get("tools").cloned().unwrap_or(Value::Null),
                "price": b.get("price").cloned().unwrap_or(Value::Null),
                "tok_per_s_p50": tps.clone().unwrap_or(Value::Null),
                "health": b.get("health").cloned().unwrap_or(Value::Null),
                "tags": b.get("tags").cloned().unwrap_or(Value::Null),
            }));
        }
    }
    let aliases: Vec<Value> = snap
        .get("routes")
        .and_then(Value::as_array)
        .unwrap_or(&empty)
        .iter()
        .map(|r| {
            json!({
                "alias": r.get("alias").cloned().unwrap_or(Value::Null),
                "is_default": r.get("is_default").cloned().unwrap_or(Value::Null),
                "targets": r.get("targets").cloned().unwrap_or(Value::Null),
                "strategy": r.get("strategy").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    json!({
        "served_by": snap.get("served_by").cloned().unwrap_or(Value::Null),
        "how_to_choose":
            "Put an `alias` in \"model\" for stability across swaps, or a `routable[].model` \
             to pin one backend. An id that appears in neither list is what produces a 404.",
        "aliases": aliases,
        "routable": routable,
        "local_gguf": local,
    })
}

/// The rig, with the two traps spelled out beside the numbers.
pub fn rig_payload(rig: &Value) -> Value {
    json!({
        "rig": rig,
        "read_this_first": [
            "One physical card enumerated by two builds appears as two rows (ROCm0 and \
             Vulkan0 can be the same silicon). VRAM budgets are per backend and are never \
             summed across them.",
            "Free VRAM can exceed total on an APU because of GTT accounting. Never compute \
             `total - free`; `vram_used_mb` returns null instead of lying.",
        ],
    })
}

// ----------------------------------------------------------------------------------------
// small parsers
// ----------------------------------------------------------------------------------------

/// `--kv` as the protocol enum.
fn parse_kv(a: &Value) -> std::result::Result<Option<KvType>, ToolError> {
    let Some(raw) = arg_str(a, "kv") else {
        return Ok(None);
    };
    let kv = match raw.to_ascii_lowercase().as_str() {
        "f32" => KvType::F32,
        "f16" => KvType::F16,
        "bf16" => KvType::Bf16,
        "q8_0" => KvType::Q8_0,
        "q5_1" => KvType::Q5_1,
        "q5_0" => KvType::Q5_0,
        "q4_1" => KvType::Q4_1,
        "q4_0" => KvType::Q4_0,
        "iq4_nl" => KvType::Iq4Nl,
        other => {
            return Err(ToolError::msg(format!(
                "`{other}` is not a KV type; use f32, f16, bf16, q8_0, q5_1, q5_0, q4_1, \
                 q4_0 or iq4_nl"
            )))
        }
    };
    Ok(Some(kv))
}

/// `[endpoints] default_mode` as the enum. An unknown value is `Thinking`, the documented
/// default, rather than a failure.
fn sampling_mode(s: &str) -> SamplingMode {
    match s.to_lowercase().as_str() {
        "coding" => SamplingMode::Coding,
        "nonthinking" => SamplingMode::Nonthinking,
        "raw" => SamplingMode::Raw,
        _ => SamplingMode::Thinking,
    }
}

/// Which devices an unqualified `fit` may spend: the ones belonging to the build a launch
/// would pick, because one `llama-server` process uses exactly one build.
fn default_devices(rig: &RigSnapshot) -> Vec<String> {
    let chosen = discover::choose_build(&rig.builds, None)
        .and_then(|c| rig.builds.iter().find(|b| b.id == c.chosen));
    if let Some(b) = chosen {
        if !b.devices.is_empty() {
            return b.devices.clone();
        }
    }
    rig.builds
        .iter()
        .find(|b| !b.devices.is_empty())
        .map(|b| b.devices.clone())
        .unwrap_or_default()
}

/// `all` / `24h` / `7d` / a timestamp into a unix-seconds cutoff.
fn parse_since(raw: &str) -> std::result::Result<Option<i64>, ToolError> {
    let raw = raw.trim();
    if raw.is_empty()
        || raw == "0"
        || raw.eq_ignore_ascii_case("all")
        || raw.eq_ignore_ascii_case("forever")
    {
        return Ok(None);
    }
    if let Some(secs) = parse_duration_secs(raw) {
        if secs == 0 {
            return Ok(None);
        }
        return Ok(Some(chrono::Utc::now().timestamp() - secs));
    }
    if let Some(at) = usage::parse_lenient_timestamp(raw) {
        return Ok(Some(at));
    }
    Err(ToolError::msg(format!(
        "`{raw}` is not a window; use `all`, a duration like `24h`, `7d` or `30m`, or an \
         absolute timestamp"
    )))
}

/// `24h` -> 86400. `None` when the string is not `<digits><unit>`.
fn parse_duration_secs(raw: &str) -> Option<i64> {
    let cut = raw.find(|c: char| !c.is_ascii_digit())?;
    let (digits, unit) = raw.split_at(cut);
    let n: i64 = digits.parse().ok()?;
    let mult = match unit.trim().to_ascii_lowercase().as_str() {
        "s" | "sec" | "secs" => 1,
        "m" | "min" | "mins" => 60,
        "h" | "hr" | "hrs" => 3_600,
        "d" | "day" | "days" => 86_400,
        "w" | "wk" | "weeks" => 604_800,
        _ => return None,
    };
    n.checked_mul(mult)
}

/// `by=` as the enum.
fn parse_group_by(spec: Option<&str>) -> std::result::Result<usage::GroupBy, ToolError> {
    match spec
        .unwrap_or("provider")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "" | "provider" => Ok(usage::GroupBy::Provider),
        "model" => Ok(usage::GroupBy::Model),
        "backend" => Ok(usage::GroupBy::Backend),
        "alias" => Ok(usage::GroupBy::Alias),
        "day" => Ok(usage::GroupBy::Day),
        other => Err(ToolError::msg(format!(
            "`{other}` is not a grouping; use provider, model, backend, alias or day"
        ))),
    }
}

// ----------------------------------------------------------------------------------------
// remote — every tool as a control-plane call, shared by both backends
// ----------------------------------------------------------------------------------------

/// One function per tool, over a [`NodeClient`].
///
/// `LocalBackend` calls these when a daemon is up; `ProxyBackend` always does. Keeping them
/// free functions rather than a third `impl` is what stops the two backends drifting.
mod remote {
    use super::*;

    /// `GET`, as a raw `Value` so an additive daemon change never breaks a tool.
    async fn get(c: &NodeClient, path: &str) -> ToolResult {
        c.get::<Value>(path).await.map_err(client_err)
    }

    /// `POST` with a JSON body.
    async fn post(c: &NodeClient, path: &str, body: &Value) -> ToolResult {
        c.post::<Value, Value>(path, body).await.map_err(client_err)
    }

    /// `PUT` with a JSON body.
    async fn put(c: &NodeClient, path: &str, body: &Value) -> ToolResult {
        c.put::<Value, Value>(path, body).await.map_err(client_err)
    }

    pub async fn status(c: &NodeClient) -> ToolResult {
        Ok(status_payload(&get(c, "/v1/snapshot").await?))
    }

    pub async fn models(c: &NodeClient) -> ToolResult {
        let snap = get(c, "/v1/snapshot").await?;
        let local = get(c, "/v1/models/local").await.unwrap_or(Value::Null);
        Ok(models_payload(&snap, &local))
    }

    pub async fn fit(c: &NodeClient, a: &Value) -> ToolResult {
        let model = need_str(a, "model")?;
        let mut q = vec![format!("model={}", seg(&model))];
        if let Some(n) = arg_u32(a, "ctx") {
            q.push(format!("ctx={n}"));
        }
        if let Some(n) = arg_u32(a, "parallel") {
            q.push(format!("parallel={n}"));
        }
        if let Some(kv) = arg_str(a, "kv") {
            q.push(format!("kv={}", seg(&kv)));
        }
        let devices = arg_strs(a, "devices");
        if !devices.is_empty() {
            q.push(format!("devices={}", seg(&devices.join(","))));
        }
        let plan = get(c, &format!("/v1/fit{}", query(&q))).await?;
        Ok(json!({ "served_by": "daemon", "plan": plan }))
    }

    /// The one-call happy path: resolve the model, pick the build, post the spec.
    pub async fn up(c: &NodeClient, a: &Value, mode: SamplingMode) -> ToolResult {
        let want = need_str(a, "model")?;
        let rig: RigSnapshot = c.get("/v1/rig").await.map_err(client_err)?;
        let all: Vec<LocalModel> = c.get("/v1/models/local").await.map_err(client_err)?;
        let model = models::resolve_model(&all, &want).map_err(core_err)?;
        let devices = arg_strs(a, "devices");
        let build = choose_build(&rig, &devices)?;
        let model_path = model
            .primary_path()
            .ok_or_else(|| ToolError::msg(format!("{} has no shard to load", model.name)))?
            .to_string();

        let spec = EndpointSpec::LocalLlama(LocalLlamaSpec {
            build: build.id.clone(),
            model_path,
            mmproj: model.mmproj.first().map(|s| s.path.clone()),
            alias_flag: model.name.clone(),
            host: "127.0.0.1".to_string(),
            port: None,
            ctx: arg_u32(a, "ctx"),
            parallel: arg_u32(a, "parallel"),
            kv_type: parse_kv(a)?,
            ngl: NglPlan::Auto,
            split: SplitPlan {
                devices,
                mode: SplitMode::Layer,
                main_gpu: None,
                tensor_split: None,
            },
            mode,
            flash_attn: None,
            api_key: None,
            extra_args: Vec::new(),
        });
        let spec = serde_json::to_value(spec).map_err(core_err)?;

        let mut q: Vec<String> = Vec::new();
        if let Some(alias) = arg_str(a, "alias") {
            q.push(format!("alias={}", seg(&alias)));
        }
        if arg_bool(a, "wait") == Some(false) {
            q.push("no_wait=true".to_string());
        }
        let started = post(c, &format!("/v1/endpoints{}", query(&q)), &spec).await?;

        // What the caller actually needs back: where to send requests, and as what.
        let snap = get(c, "/v1/snapshot").await.unwrap_or(Value::Null);
        let base = snap
            .pointer("/proxy/base_url")
            .and_then(Value::as_str)
            .unwrap_or("http://127.0.0.1:8888/v1")
            .to_string();
        Ok(json!({
            "started": started,
            "use": {
                "openai_base_url": base,
                "model": arg_str(a, "alias").unwrap_or_else(|| model.name.clone()),
            },
            "build": build.id.as_str(),
            "if_this_failed": "call apexrouter_logs with the id above — the reason is in the \
                               last 50 lines, and guessing at it wastes a turn",
        }))
    }

    pub async fn endpoint_start(c: &NodeClient, a: &Value) -> ToolResult {
        let spec = a
            .get("spec")
            .cloned()
            .filter(|v| v.is_object())
            .ok_or_else(|| {
                ToolError::msg("`spec` is required and must be an EndpointSpec object")
            })?;
        let mut q: Vec<String> = Vec::new();
        if let Some(alias) = arg_str(a, "alias") {
            q.push(format!("alias={}", seg(&alias)));
        }
        if arg_bool(a, "no_wait") == Some(true) {
            q.push("no_wait=true".to_string());
        }
        if arg_bool(a, "force") == Some(true) {
            q.push("force=true".to_string());
        }
        post(c, &format!("/v1/endpoints{}", query(&q)), &spec).await
    }

    pub async fn endpoint_stop(c: &NodeClient, a: &Value) -> ToolResult {
        let id = match arg_str(a, "id") {
            Some(id) => id,
            None => {
                let alias = arg_str(a, "alias").ok_or_else(|| {
                    ToolError::msg("one of `id` or `alias` is required to say what to stop")
                })?;
                id_for_alias(c, &alias).await?
            }
        };
        let mode = arg_str(a, "mode").unwrap_or_else(|| "drain".to_string());
        let path = format!("/v1/endpoints/{}/stop?mode={}", seg(&id), seg(&mode));
        post(c, &path, &json!({})).await
    }

    pub async fn swap(c: &NodeClient, a: &Value) -> ToolResult {
        let alias = need_str(a, "alias")?;
        let to = a
            .get("to")
            .cloned()
            .filter(|v| !v.is_null())
            .ok_or_else(|| {
                ToolError::msg(
                    "`to` is required: a backend id (string) or an EndpointSpec object to \
                     start and swap onto",
                )
            })?;
        let mut body = json!({ "to": to });
        if let Some(mode) = arg_str(a, "mode") {
            body["mode"] = Value::String(mode);
        }
        post(c, &format!("/v1/routes/{}/swap", seg(&alias)), &body).await
    }

    /// Enable, disable and drain have their own routes; tags need a `PATCH`, which
    /// [`NodeClient`] cannot issue, so that half reports itself rather than lying.
    pub async fn backend_set(c: &NodeClient, a: &Value) -> ToolResult {
        let id = need_str(a, "id")?;
        let mut applied: Vec<Value> = Vec::new();
        let mut skipped: Vec<Value> = Vec::new();

        if let Some(enabled) = arg_bool(a, "enabled") {
            let verb = if enabled { "enable" } else { "disable" };
            let r = post(c, &format!("/v1/backends/{}/{verb}", seg(&id)), &json!({})).await?;
            applied.push(json!({ "enabled": enabled, "result": r }));
        }
        if arg_bool(a, "drain") == Some(true) {
            let r = post(c, &format!("/v1/backends/{}/drain", seg(&id)), &json!({})).await?;
            applied.push(json!({ "drain": true, "result": r }));
        }
        let tags = arg_strs(a, "tags");
        if !tags.is_empty() {
            skipped.push(json!({
                "tags": tags,
                "why": "replacing tags needs PATCH /v1/backends/{id}, and this client only \
                        issues GET/POST/PUT/DELETE. Use `apexrouter backend set` on the CLI.",
            }));
        }
        if applied.is_empty() && skipped.is_empty() {
            return Err(ToolError::msg(
                "nothing to do: pass at least one of `enabled`, `drain` or `tags`",
            ));
        }
        Ok(json!({ "id": id, "applied": applied, "not_applied": skipped }))
    }

    pub async fn route_set(c: &NodeClient, a: &Value) -> ToolResult {
        let alias = need_str(a, "alias")?;
        let raw = arg_strs(a, "targets");
        if raw.is_empty() {
            return Err(ToolError::msg(
                "`targets` must name at least one target: `<backend-id>[:<model>]`, \
                 `tag:<tag>[:<model>]` or `glob:<pattern>[:<model>]`",
            ));
        }
        let targets = raw
            .iter()
            .map(|t| crate::cmd::route::parse_target(t).map_err(core_err))
            .collect::<std::result::Result<Vec<_>, ToolError>>()?;

        // Start from what the alias already is, so an unspecified knob is left alone.
        let existing: Vec<Value> = c.get("/v1/routes").await.map_err(client_err)?;
        let mut route = existing
            .into_iter()
            .find(|r| r.get("alias").and_then(Value::as_str) == Some(alias.as_str()))
            .unwrap_or_else(|| {
                json!({
                    "alias": alias,
                    "targets": [],
                    "strategy": "first_healthy",
                    "filter": {
                        "require_tags": [], "exclude_tags": [], "max_cost_per_mtok": null,
                        "min_ctx": null, "require_vision": false, "require_tools": false,
                    },
                    "retry": { "attempts": 2, "failover": true, "honor_retry_after": true },
                    "is_default": false,
                    "description": null,
                })
            });
        route["alias"] = Value::String(alias.clone());
        route["targets"] = serde_json::to_value(targets).map_err(core_err)?;
        if let Some(s) = arg_str(a, "strategy") {
            route["strategy"] = Value::String(s.to_ascii_lowercase());
        }
        if let Some(f) = arg_bool(a, "failover") {
            route["retry"]["failover"] = Value::Bool(f);
        }
        if let Some(d) = arg_bool(a, "default") {
            route["is_default"] = Value::Bool(d);
        }

        let after = put(c, &format!("/v1/routes/{}", seg(&alias)), &route).await?;
        if arg_bool(a, "default") == Some(true) {
            post(c, "/v1/routes/default", &json!({ "alias": alias })).await?;
        }
        Ok(json!({ "route": after, "effective": "on the next request; nothing restarted" }))
    }

    pub async fn recipe_list(c: &NodeClient) -> ToolResult {
        Ok(json!({
            "served_by": "daemon",
            "recipes": get(c, "/v1/recipes").await?,
            "profiles": get(c, "/v1/profiles").await.unwrap_or(Value::Null),
        }))
    }

    pub async fn recipe_save(c: &NodeClient, a: &Value) -> ToolResult {
        let recipe = a
            .get("recipe")
            .cloned()
            .filter(Value::is_object)
            .ok_or_else(|| ToolError::msg("`recipe` is required and must be a Recipe object"))?;
        match recipe
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            Some(id) => put(c, &format!("/v1/recipes/{}", seg(id)), &recipe).await,
            None => post(c, "/v1/recipes", &recipe).await,
        }
    }

    pub async fn recipe_run(c: &NodeClient, a: &Value) -> ToolResult {
        let id = need_str(a, "id")?;
        let mut q: Vec<String> = Vec::new();
        if let Some(alias) = arg_str(a, "alias") {
            q.push(format!("alias={}", seg(&alias)));
        }
        if arg_bool(a, "no_wait") == Some(true) {
            q.push("no_wait=true".to_string());
        }
        let path = format!("/v1/recipes/{}/instantiate{}", seg(&id), query(&q));
        post(c, &path, &json!({})).await
    }

    pub async fn usage(c: &NodeClient, a: &Value) -> ToolResult {
        let since = arg_str(a, "since").unwrap_or_else(|| DEFAULT_SINCE.to_string());
        let by = arg_str(a, "by").unwrap_or_else(|| "provider".to_string());
        let path = format!("/v1/usage?since={}&by={}", seg(&since), seg(&by));
        Ok(
            json!({ "served_by": "daemon", "since": since, "by": by, "summary": get(c, &path).await? }),
        )
    }

    /// `POST /v1/smoke` when the daemon has it; otherwise the route test, which exists
    /// today and is one honest probe rather than four invented ones.
    pub async fn smoke(c: &NodeClient, a: &Value) -> ToolResult {
        let alias = arg_str(a, "alias");
        let base_url = arg_str(a, "base_url");
        if alias.is_none() && base_url.is_none() {
            return Err(ToolError::msg("one of `alias` or `base_url` is required"));
        }
        let mut body = json!({});
        if let Some(a) = &alias {
            body["alias"] = Value::String(a.clone());
        }
        if let Some(u) = &base_url {
            body["base_url"] = Value::String(u.clone());
        }
        match post(c, "/v1/smoke", &body).await {
            Ok(v) => Ok(json!({ "probes": v })),
            Err(first) => match &alias {
                Some(al) => {
                    let probe =
                        post(c, &format!("/v1/routes/{}/test", seg(al)), &json!({})).await?;
                    Ok(json!({
                        "probes": [probe],
                        "degraded": "the four-probe suite was unavailable, so this is the \
                                     single 20-token route test instead",
                        "why": first.message,
                    }))
                }
                None => Err(first),
            },
        }
    }

    pub async fn diagnose(c: &NodeClient, a: &Value) -> ToolResult {
        let path = match arg_str(a, "only") {
            Some(only) => format!("/v1/diagnose?only={}", seg(&only)),
            None => "/v1/diagnose".to_string(),
        };
        Ok(json!({ "checks": get(c, &path).await? }))
    }

    pub async fn hf_search(c: &NodeClient, a: &Value) -> ToolResult {
        let q = need_str(a, "q")?;
        let limit = arg_u32(a, "limit").unwrap_or(20);
        get(c, &format!("/v1/hf/search?q={}&limit={limit}", seg(&q))).await
    }

    pub async fn hf_files(c: &NodeClient, a: &Value) -> ToolResult {
        let repo = need_str(a, "repo")?;
        // The repo id has a `/` in it and the route is a wildcard, so it is NOT escaped.
        get(c, &format!("/v1/hf/models/{repo}/files")).await
    }

    pub async fn hf_get(c: &NodeClient, a: &Value) -> ToolResult {
        let repo = need_str(a, "repo")?;
        let mut body = json!({ "repo": repo });
        if let Some(q) = arg_str(a, "quant") {
            body["quant"] = Value::String(q);
        }
        let files = arg_strs(a, "files");
        if !files.is_empty() {
            body["files"] = serde_json::to_value(files).map_err(core_err)?;
        }
        let path = if arg_bool(a, "no_wait") == Some(true) {
            "/v1/hf/downloads?no_wait=true"
        } else {
            "/v1/hf/downloads"
        };
        post(c, path, &body).await
    }

    /// Read-only market search. `PUT /api/v0/search/asks/` upstream; nothing is created.
    pub async fn vast_offers(c: &NodeClient, a: &Value) -> ToolResult {
        let mut body = json!({});
        if let Some(p) = arg_str(a, "profile") {
            body["profile"] = Value::String(p);
        }
        if let Some(g) = arg_str(a, "gpu") {
            body["gpu_names"] = json!([g]);
        }
        if let Some(n) = arg_u32(a, "num_gpus") {
            body["num_gpus_min"] = json!(n);
        }
        if let Some(g) = arg_str(a, "geo") {
            body["geo"] = Value::String(g);
        }
        if let Some(p) = arg_f64(a, "max_price") {
            body["max_dph"] = json!(p);
        }
        body["limit"] = json!(arg_u32(a, "limit").unwrap_or(20));
        post(c, "/v1/vast/offers/search", &body).await
    }

    /// **Reachable only after [`approved`] returned true.** This is the deliberate-spend
    /// path; the daemon's own ceiling and approval gate still apply on top.
    pub async fn vast_rent_confirmed(c: &NodeClient, a: &Value) -> ToolResult {
        let launch = a
            .get("launch")
            .cloned()
            .filter(Value::is_object)
            .ok_or_else(|| ToolError::msg("`launch` is required and must be a ContainerLaunch"))?;
        let mut body = json!({
            "launch": launch,
            "confirm": true,
            "max_usd_per_hour": arg_f64(a, "max_usd_per_hour").unwrap_or_default(),
            "auto_tunnel": arg_bool(a, "auto_tunnel").unwrap_or(true),
        });
        if let Some(p) = arg_str(a, "profile") {
            body["profile"] = Value::String(p);
        }
        if let Some(o) = a.get("offer_id").and_then(Value::as_u64) {
            body["offer_id"] = json!(o);
        }
        if let Some(al) = arg_str(a, "bind_alias") {
            body["bind_alias"] = Value::String(al);
        }
        // Always identify as MCP so `require_human_confirm` can see us. A bare POST was
        // classified as Api and used to skip the human gate entirely.
        let mut path = "/v1/vast/instances?source=mcp".to_owned();
        if let Some(a) = arg_str(a, "approval") {
            path.push_str("&approval=");
            path.push_str(&a);
        }
        post(c, &path, &body).await
    }

    /// **Reachable only with `confirm: true`.**
    pub async fn vast_destroy_confirmed(c: &NodeClient, a: &Value) -> ToolResult {
        let id = need_str(a, "id")?;
        let path = format!("/v1/vast/instances/{}?confirm=true", seg(&id));
        c.delete(&path).await.map_err(client_err)?;
        Ok(json!({ "destroyed": true, "id": id }))
    }

    pub async fn compare(c: &NodeClient, a: &Value) -> ToolResult {
        let aliases = arg_strs(a, "aliases");
        if aliases.len() < 2 {
            return Err(ToolError::msg(
                "`aliases` needs at least two entries — comparing one alias with itself \
                 tells you nothing",
            ));
        }
        let body = json!({
            "aliases": aliases,
            "prompt": need_str(a, "prompt")?,
            "max_tokens": arg_u32(a, "max_tokens").unwrap_or(200),
        });
        post(c, "/v1/compare", &body).await
    }

    /// The endpoint an alias is currently bound to.
    async fn id_for_alias(c: &NodeClient, alias: &str) -> std::result::Result<String, ToolError> {
        let eps: Value = c.get("/v1/endpoints").await.map_err(client_err)?;
        let empty = Vec::new();
        let rows = eps.as_array().unwrap_or(&empty);
        for r in rows {
            let bound = r
                .get("alias_bindings")
                .and_then(Value::as_array)
                .map(|xs| xs.iter().any(|v| v.as_str() == Some(alias)))
                .unwrap_or(false);
            if bound {
                if let Some(id) = r.get("id").and_then(Value::as_str) {
                    return Ok(id.to_string());
                }
            }
        }
        let known: Vec<&str> = rows
            .iter()
            .filter_map(|r| r.get("id").and_then(Value::as_str))
            .collect();
        Err(ToolError::with_data(
            format!("no endpoint is currently bound to alias `{alias}`"),
            json!({ "endpoints": known }),
        ))
    }

    /// The build a launch would pick, narrowed by the first named device's backend.
    fn choose_build(
        rig: &RigSnapshot,
        devices: &[String],
    ) -> std::result::Result<LlamaBuild, ToolError> {
        let want = devices.first().and_then(|d| backend_of(rig, d));
        let choice = discover::choose_build(&rig.builds, want).ok_or_else(|| {
            ToolError::msg(
                "no llama.cpp build was discovered — `apexrouter_rig` shows where it looked, \
                 and [endpoints] build_roots is what it searched",
            )
        })?;
        rig.builds
            .iter()
            .find(|b| b.id == choice.chosen)
            .cloned()
            .ok_or_else(|| ToolError::msg("the chosen build vanished between scan and use"))
    }

    /// Which compute backend a `-dev` token belongs to, per the rig we just read.
    fn backend_of(rig: &RigSnapshot, device: &str) -> Option<apexrouter_protocol::GpuBackend> {
        rig.gpus
            .iter()
            .find(|g| g.device.eq_ignore_ascii_case(device))
            .map(|g| g.backend.clone())
    }
}

/// An inert backend for the dispatcher's tests.
///
/// It answers every tool with its own name and touches nothing — no `$STATE`, no socket —
/// so the name → method table can be proved exhaustive without any I/O at all.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// Answers every tool with the tool's own name.
    struct Echo;

    #[async_trait]
    impl McpBackend for Echo {
        async fn status(&self) -> ToolResult {
            Ok(json!("status"))
        }
        async fn models(&self) -> ToolResult {
            Ok(json!("models"))
        }
        async fn rig(&self) -> ToolResult {
            Ok(json!("rig"))
        }
        async fn fit(&self, _: &Value) -> ToolResult {
            Ok(json!("fit"))
        }
        async fn up(&self, _: &Value) -> ToolResult {
            Ok(json!("up"))
        }
        async fn endpoint_start(&self, _: &Value) -> ToolResult {
            Ok(json!("endpoint_start"))
        }
        async fn endpoint_stop(&self, _: &Value) -> ToolResult {
            Ok(json!("endpoint_stop"))
        }
        async fn swap(&self, _: &Value) -> ToolResult {
            Ok(json!("swap"))
        }
        async fn logs(&self, _: &Value) -> ToolResult {
            Ok(json!("logs"))
        }
        async fn backend_set(&self, _: &Value) -> ToolResult {
            Ok(json!("backend_set"))
        }
        async fn route_set(&self, _: &Value) -> ToolResult {
            Ok(json!("route_set"))
        }
        async fn recipe_list(&self) -> ToolResult {
            Ok(json!("recipe_list"))
        }
        async fn recipe_save(&self, _: &Value) -> ToolResult {
            Ok(json!("recipe_save"))
        }
        async fn recipe_run(&self, _: &Value) -> ToolResult {
            Ok(json!("recipe_run"))
        }
        async fn usage(&self, _: &Value) -> ToolResult {
            Ok(json!("usage"))
        }
        async fn smoke(&self, _: &Value) -> ToolResult {
            Ok(json!("smoke"))
        }
        async fn diagnose(&self, _: &Value) -> ToolResult {
            Ok(json!("diagnose"))
        }
        async fn hf_search(&self, _: &Value) -> ToolResult {
            Ok(json!("hf_search"))
        }
        async fn hf_files(&self, _: &Value) -> ToolResult {
            Ok(json!("hf_files"))
        }
        async fn hf_get(&self, _: &Value) -> ToolResult {
            Ok(json!("hf_get"))
        }
        async fn vast_offers(&self, _: &Value) -> ToolResult {
            Ok(json!("vast_offers"))
        }
        async fn vast_rent(&self, _: &Value) -> ToolResult {
            Ok(json!("vast_rent"))
        }
        async fn vast_destroy(&self, _: &Value) -> ToolResult {
            Ok(json!("vast_destroy"))
        }
        async fn compare(&self, _: &Value) -> ToolResult {
            Ok(json!("compare"))
        }
    }

    /// A backend that touches nothing.
    pub(crate) fn echo() -> Arc<dyn McpBackend> {
        Arc::new(Echo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirm_must_be_a_real_json_true_and_the_rate_positive() {
        assert!(!approved(&json!({})));
        assert!(!approved(&json!({ "confirm": true })));
        assert!(!approved(
            &json!({ "confirm": "true", "max_usd_per_hour": 2.0 })
        ));
        assert!(!approved(&json!({ "confirm": 1, "max_usd_per_hour": 2.0 })));
        assert!(!approved(
            &json!({ "confirm": true, "max_usd_per_hour": 0.0 })
        ));
        assert!(!approved(
            &json!({ "confirm": true, "max_usd_per_hour": -3.0 })
        ));
        assert!(approved(
            &json!({ "confirm": true, "max_usd_per_hour": 2.0 })
        ));
    }

    #[test]
    fn the_refusal_carries_the_cost_preview_and_the_credit() {
        let e = rent_preview(
            &json!({ "max_usd_per_hour": 2.0, "offer_id": 42 }),
            Some(4.0),
            true,
            Some(json!({ "credit": 7.73 })),
        );
        let d = e.data.clone().expect("the refusal carries data");
        assert_eq!(d["created"], json!(false));
        assert_eq!(d["cost_preview"]["usd_per_hour"], json!(2.0));
        assert_eq!(d["cost_preview"]["est_total_24h_usd"], json!(48.0));
        assert_eq!(d["credit_usd"], json!(7.73));
        assert_eq!(d["daemon_ceiling_usd_per_hour"], json!(4.0));
        assert_eq!(d["requires_human_approval"], json!(true));
        assert_eq!(d["to_proceed"]["confirm"], json!(true));
        // And the text an agent reads carries the numbers, not just the prose.
        assert!(e.text().contains("7.73"), "{}", e.text());
    }

    #[test]
    fn the_ceiling_caps_a_greedy_request_in_the_preview() {
        let e = rent_preview(&json!({ "max_usd_per_hour": 99.0 }), Some(4.0), false, None);
        let d = e.data.expect("data");
        assert_eq!(d["cost_preview"]["usd_per_hour"], json!(4.0));
        assert_eq!(d["credit_usd"], Value::Null);
    }

    #[test]
    fn a_destroy_without_confirm_destroys_nothing_and_says_so() {
        let e = destroy_refusal(&json!({ "id": "12345" }));
        let d = e.data.expect("data");
        assert_eq!(d["destroyed"], json!(false));
        assert_eq!(d["to_proceed"]["confirm"], json!(true));
    }

    #[test]
    fn status_payload_hands_over_a_usable_base_url() {
        let snap = json!({
            "served_by": "offline",
            "stale": true,
            "proxy": { "base_url": "http://127.0.0.1:8888/v1", "default_alias": "auto" },
        });
        let p = status_payload(&snap);
        assert_eq!(
            p["how_to_use"]["openai_base_url"],
            "http://127.0.0.1:8888/v1"
        );
        assert_eq!(p["how_to_use"]["model"], "auto");
        assert_eq!(
            p["how_to_use"]["anthropic_base_url"],
            "http://127.0.0.1:8888"
        );
        assert_eq!(p["served_by"], "offline");
    }

    #[test]
    fn models_payload_lists_one_row_per_advertised_model() {
        let snap = json!({
            "served_by": "daemon",
            "routes": [{ "alias": "auto", "is_default": true, "targets": [], "strategy": "first_healthy" }],
            "backends": [
                {
                    "id": "local-carnice", "enabled": true, "tags": ["local"],
                    "health": { "ready": { "tps_p50": 9.71 } },
                    "models": [{ "id": "Carnice-9b", "ctx": 262144, "vision": false, "tools": true }],
                },
                { "id": "off", "enabled": false, "models": [{ "id": "hidden" }] },
            ],
        });
        let p = models_payload(&snap, &json!([]));
        let rows = p["routable"].as_array().cloned().unwrap_or_default();
        assert_eq!(rows.len(), 1, "a disabled backend is not routable");
        assert_eq!(rows[0]["model"], "Carnice-9b");
        assert_eq!(rows[0]["tok_per_s_p50"], json!(9.71));
        assert_eq!(p["aliases"][0]["alias"], "auto");
    }

    #[test]
    fn since_parses_the_documented_windows() {
        assert_eq!(parse_since("all").expect("all"), None);
        assert_eq!(parse_since("").expect("empty"), None);
        let day = parse_since("24h").expect("24h").expect("a cutoff");
        let now = chrono::Utc::now().timestamp();
        assert!((now - day - 86_400).abs() <= 2, "24h is a day back");
        assert!(parse_since("banana").is_err());
    }

    #[test]
    fn group_by_rejects_a_bucket_that_does_not_exist() {
        assert!(parse_group_by(None).is_ok());
        assert!(parse_group_by(Some("day")).is_ok());
        assert!(parse_group_by(Some("phase-of-moon")).is_err());
    }

    #[test]
    fn a_path_segment_cannot_smuggle_a_slash_or_a_query() {
        assert_eq!(seg("local-carnice"), "local-carnice");
        assert_eq!(seg("../../etc/passwd"), "..%2F..%2Fetc%2Fpasswd");
        assert_eq!(seg("a b&c=d"), "a%20b%26c%3Dd");
    }

    #[test]
    fn argument_readers_treat_blank_and_wrong_typed_values_as_absent() {
        let a = json!({ "s": "  ", "n": -1, "b": "yes", "xs": ["a", "", "  b "] });
        assert_eq!(arg_str(&a, "s"), None);
        assert_eq!(arg_u32(&a, "n"), None);
        assert_eq!(arg_bool(&a, "b"), None);
        assert_eq!(arg_strs(&a, "xs"), vec!["a".to_string(), "b".to_string()]);
        assert!(need_str(&a, "missing").is_err());
    }
}
