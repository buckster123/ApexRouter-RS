//! OWNER: unit U-02 (crates/apexrouter-slint/src/**, except build.rs). Do not edit outside
//! that unit.
//!
//! The `NodeClient` glue: one background task holds the `/ws` subscription and pushes
//! `Event`s into the Slint event loop, so the app renders the same `Snapshot` as the web UI
//! with **zero polling**.
//!
//! All fallible async work goes in one inner `async { … anyhow::Ok(v) }.await`, so a single
//! `match` handles every failure rather than one per call site. That is what [`Bridge::fetch`]
//! and [`Bridge::act`] exist for: the caller writes the request, this module owns the error
//! path, the toast and the hop back onto the UI thread.
//!
//! Nothing in here holds business logic. Every value the app shows came off the HTTP API as
//! a protocol type; the only thing this module decides is how to render one as a string.

use apexrouter_client::NodeClient;
use apexrouter_protocol::{
    Alert, AlertLevel, Backend, BackendId, BackendSelector, BootPhase, CheckResult, CheckStatus,
    CostEstimate, CredentialSource, DeviceBudget, Event, FitPlan, FitVerdict, Health, HfFile,
    HfFileGroup, JobRecord, JobState, LocalModel, ModelRoute, Money, Offer, PriceModel,
    ProviderStatus, RequestRecord, RigSnapshot, Snapshot, Strategy, TokenCount, TunnelStatus,
    UpstreamModel, UsageSummary, VastInstance,
};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};
use std::collections::VecDeque;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::runtime::Handle;

use crate::{
    AppWindow, BackendRow, BuildRow, CheckRow, DeviceRow, HfFileRow, InstanceRow, JobRow,
    LocalModelRow, ModelRow, OfferRow, ProbeRow, ProfileRow, ProviderRow, RecipeRow, RequestRow,
    RouteRow, State, TargetRow, TunnelRow, UsageRow,
};

/// How many finished requests the ticker keeps. The web UI holds the same window.
const REQUEST_HISTORY: usize = 200;

/// How many log lines the buffer keeps before the oldest are dropped.
const LOG_BUFFER: usize = 4000;

// ─────────────────────────────────────────────────────────────────────────────
// Where the daemon is
// ─────────────────────────────────────────────────────────────────────────────

/// The control-plane URL: `$APEXROUTER_URL`, else `[server] control_bind` from the config
/// file the daemon itself reads, else the documented loopback default.
///
/// Reading the configured bind is not a nicety. Moving the control port in `config.toml`
/// used to leave this app pointed at `127.0.0.1:2739` with nothing behind it, and the only
/// symptom was "not connected" — a debugging cycle spent on a value that was written down
/// the whole time.
///
/// This crate is GPL and cannot link `apexrouter-core`, so it does not read the lock file's
/// owner record the way the CLI does and it parses the one key it needs by hand
/// ([`control_bind_in`]) rather than taking a TOML dependency. The env var stays the
/// override, and it still wins.
pub fn control_url() -> String {
    if let Some(url) = env_nonempty("APEXROUTER_URL") {
        return url;
    }
    let bind = configured_control_bind()
        .unwrap_or_else(|| apexrouter_protocol::DEFAULT_CONTROL_BIND.to_string());
    format!("http://{}", dialable(&bind))
}

/// `[server] control_bind` as written in the resolved config file, when there is one.
fn configured_control_bind() -> Option<String> {
    let text = std::fs::read_to_string(config_path()?).ok()?;
    control_bind_in(&text)
}

/// The config file `apexrouter` itself would load, by the resolution order `ARCHITECTURE.md`
/// §5.1 fixes: `$APEXROUTER_CONFIG` → `$APEXROUTER_HOME/config.toml` →
/// `$XDG_CONFIG_HOME/apexrouter/config.toml` → `~/.config/apexrouter/config.toml`.
///
/// Mirrored rather than shared, for the licensing reason above. `core::paths` additionally
/// falls back to the passwd database when `$HOME` is unset; a GUI always has one, and
/// answering `None` here simply falls back to the documented default rather than guessing.
fn config_path() -> Option<std::path::PathBuf> {
    if let Some(p) = env_nonempty("APEXROUTER_CONFIG") {
        return Some(std::path::PathBuf::from(p));
    }
    if let Some(h) = env_nonempty("APEXROUTER_HOME") {
        return Some(std::path::PathBuf::from(h).join("config.toml"));
    }
    let cfg_home = env_nonempty("XDG_CONFIG_HOME")
        .filter(|p| p.starts_with('/'))
        .map(std::path::PathBuf::from)
        .or_else(|| env_nonempty("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))?;
    Some(cfg_home.join("apexrouter").join("config.toml"))
}

/// `control_bind` from the `[server]` table of a config document.
///
/// A deliberately small reader: it tracks the current table header, ignores comment lines,
/// and takes the first double-quoted value on the `control_bind` line — which is every form
/// `config.example.toml` and `apexrouter config init` can produce. Anything it does not
/// understand yields `None`, and `None` means "use the default", never a wrong URL.
fn control_bind_in(text: &str) -> Option<String> {
    let mut in_server = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            // `[server]` only — `[server.something]` is a different table, and so is
            // `[[server]]`, whose header starts with a second `[`.
            in_server = rest.strip_suffix(']').map(str::trim) == Some("server");
            continue;
        }
        if !in_server {
            continue;
        }
        let Some(rest) = line.strip_prefix("control_bind") else {
            continue;
        };
        let rest = rest.trim_start();
        if !rest.starts_with('=') {
            continue;
        }
        let value = rest[1..].trim();
        let inner = value.strip_prefix('"')?;
        let end = inner.find('"')?;
        let bind = inner[..end].trim();
        return (!bind.is_empty()).then(|| bind.to_string());
    }
    None
}

/// A *bind* address turned into an address a client can actually dial.
///
/// `0.0.0.0:2739` means "every interface" to a listener and nothing at all to `connect()`;
/// the interface this app is on is loopback, so a wildcard bind is dialled there. Any other
/// host is passed through untouched — someone who bound the control plane to a LAN address
/// meant that address.
fn dialable(bind: &str) -> String {
    let bind = bind.trim();
    match bind.rsplit_once(':') {
        Some(("0.0.0.0", port)) => format!("127.0.0.1:{port}"),
        Some(("[::]", port)) | Some(("*", port)) => format!("[::1]:{port}"),
        _ => bind.to_string(),
    }
}

/// The bearer, when one is configured. `None` on a loopback control plane with no auth.
pub fn control_token() -> Option<String> {
    env_nonempty("APEXROUTER_TOKEN")
}

/// `std::env::var`, with empty treated as unset.
fn env_nonempty(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        _ => None,
    }
}

/// Percent-encode everything outside the unreserved set, so a model path with a space in
/// it survives into a query string. Written by hand: this crate has no URL dependency.
pub fn q(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        let c = *b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~' | '/') {
            out.push(c);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// Formatting. Rust renders every string; Palette picks every colour.
// ─────────────────────────────────────────────────────────────────────────────

/// Unix seconds, now.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A byte count, in the unit a human would say out loud.
pub fn fmt_bytes(bytes: u64) -> String {
    const K: f64 = 1024.0;
    let b = bytes as f64;
    if b < K {
        format!("{bytes} B")
    } else if b < K * K {
        format!("{:.0} KiB", b / K)
    } else if b < K * K * K {
        format!("{:.1} MiB", b / (K * K))
    } else {
        format!("{:.2} GiB", b / (K * K * K))
    }
}

/// Mebibytes as reported by the driver. Never derived from `total - free`: on this box
/// ROCm reports free > total (GTT accounting) and the subtraction underflows into a lie.
pub fn fmt_mb(mb: u64) -> String {
    if mb >= 1024 {
        format!("{:.1} GiB", mb as f64 / 1024.0)
    } else {
        format!("{mb} MiB")
    }
}

/// A duration in seconds, at one significant unit plus its neighbour.
pub fn fmt_dur(secs: f64) -> String {
    let s = secs.max(0.0);
    if s < 60.0 {
        format!("{:.0}s", s)
    } else if s < 3600.0 {
        format!("{}m {}s", (s / 60.0) as u64, (s % 60.0) as u64)
    } else if s < 86400.0 {
        format!("{}h {}m", (s / 3600.0) as u64, ((s % 3600.0) / 60.0) as u64)
    } else {
        format!(
            "{}d {}h",
            (s / 86400.0) as u64,
            ((s % 86400.0) / 3600.0) as u64
        )
    }
}

/// "12s ago". Relative, because this crate has no timezone database and a wrong local
/// clock reading is worse than no clock reading.
pub fn fmt_ago(unix: i64) -> String {
    if unix <= 0 {
        return "—".to_string();
    }
    let delta = (now_unix() - unix) as f64;
    if delta < 0.0 {
        return "just now".to_string();
    }
    format!("{} ago", fmt_dur(delta))
}

/// Money, always with its unit. [`Money`]'s own `Display` gives `$1.23`.
pub fn fmt_money(m: Money) -> String {
    m.to_string()
}

/// A cost estimate rendered as `(text, is_metered)`. The caller pairs the flag with a
/// badge — an approximation that looks like a measurement is the one number that costs
/// real money.
pub fn cost_text(c: &CostEstimate) -> (String, bool) {
    match c {
        CostEstimate::Metered { usd, .. } => (fmt_money(*usd), true),
        CostEstimate::Approximate { usd, .. } => (fmt_money(*usd), false),
        CostEstimate::Unknown => ("—".to_string(), false),
    }
}

/// A token count, with `~` when it was estimated rather than reported.
pub fn token_text(t: Option<&TokenCount>) -> String {
    match t {
        Some(TokenCount::Reported(n)) => n.to_string(),
        Some(TokenCount::Estimated(n)) => format!("~{n}"),
        None => "—".to_string(),
    }
}

/// Health as `(label, level)`. Level is the integer every row struct carries:
/// 0 neutral · 1 good · 2 warn · 3 serious · 4 critical · 5 accent.
pub fn health_text(h: &Health) -> (String, i32) {
    match h {
        Health::Unknown => ("unknown".to_string(), 0),
        Health::Starting { phase, .. } => (boot_text(phase).0, 2),
        Health::Ready { .. } => ("ready".to_string(), 1),
        Health::Degraded {
            consecutive_failures,
            ..
        } => (format!("degraded ×{consecutive_failures}"), 3),
        Health::Down { .. } => ("down".to_string(), 4),
        Health::Draining { in_flight } => (format!("draining {in_flight}"), 2),
    }
}

/// A boot phase as `(label, level, progress 0..1)`.
pub fn boot_text(p: &BootPhase) -> (String, i32, f32) {
    match p {
        BootPhase::Reserved => ("reserved".to_string(), 5, 0.05),
        BootPhase::Provisioning => ("provisioning".to_string(), 5, 0.15),
        BootPhase::Pulling => ("pulling image".to_string(), 5, 0.30),
        BootPhase::Compiling => ("compiling".to_string(), 5, 0.40),
        BootPhase::Downloading { pct, mbps } => (
            match (pct, mbps) {
                (Some(p), Some(m)) => format!("downloading {p:.0}% at {m:.0} MB/s"),
                (Some(p), None) => format!("downloading {p:.0}%"),
                _ => "downloading".to_string(),
            },
            5,
            pct.map(|p| (p / 100.0).clamp(0.0, 1.0)).unwrap_or(0.5),
        ),
        BootPhase::Loading { pct } => (
            match pct {
                Some(p) => format!("loading {p:.0}%"),
                None => "loading".to_string(),
            },
            5,
            pct.map(|p| (p / 100.0).clamp(0.0, 1.0)).unwrap_or(0.8),
        ),
        BootPhase::Healthy => ("healthy".to_string(), 1, 1.0),
        BootPhase::Parked => (
            "parked (disk held, still billing storage)".to_string(),
            2,
            1.0,
        ),
        BootPhase::Failed { reason } => (format!("failed: {reason}"), 4, 1.0),
        BootPhase::Destroyed => ("destroyed".to_string(), 0, 1.0),
    }
}

/// A check status as `(label, level)`.
pub fn check_text(s: CheckStatus) -> (String, i32) {
    match s {
        CheckStatus::Pass => ("pass".to_string(), 1),
        CheckStatus::Warn => ("warn".to_string(), 2),
        CheckStatus::Fail => ("fail".to_string(), 4),
        CheckStatus::Skipped => ("skipped".to_string(), 0),
    }
}

/// A job state as `(label, level)`.
pub fn job_text(s: JobState) -> (String, i32) {
    match s {
        JobState::Pending => ("pending".to_string(), 0),
        JobState::Running => ("running".to_string(), 5),
        JobState::Succeeded => ("succeeded".to_string(), 1),
        JobState::Failed => ("failed".to_string(), 4),
        JobState::Cancelled => ("cancelled".to_string(), 2),
    }
}

/// An alert level as the row integer.
pub fn alert_level(l: AlertLevel) -> i32 {
    match l {
        AlertLevel::Info => 0,
        AlertLevel::Warning => 2,
        AlertLevel::Serious => 3,
        AlertLevel::Critical => 4,
    }
}

/// Where a credential came from — **never** what it is (§9.2).
pub fn credential_source(c: &CredentialSource) -> String {
    match c {
        CredentialSource::None => "none".to_string(),
        CredentialSource::Env { var } => format!("env {var}"),
        CredentialSource::File { path } => format!("file {path}"),
        CredentialSource::Managed { store } => format!("managed {store}"),
        CredentialSource::Instance => "instance".to_string(),
    }
}

/// A routing strategy in its wire spelling, which is also how it reads in the UI.
pub fn strategy_index(s: Strategy) -> i32 {
    match s {
        Strategy::FirstHealthy => 0,
        Strategy::RoundRobin => 1,
        Strategy::LeastBusy => 2,
        Strategy::Cheapest => 3,
    }
}

/// The inverse of [`strategy_index`]; anything out of range reads as `first-healthy`,
/// which is the safe default rather than a panic.
pub fn strategy_from_index(i: i32) -> Strategy {
    match i {
        1 => Strategy::RoundRobin,
        2 => Strategy::LeastBusy,
        3 => Strategy::Cheapest,
        _ => Strategy::FirstHealthy,
    }
}

/// A target selector in its editable form: `id:…`, `tag:…` or `glob:…`.
pub fn selector_text(s: &BackendSelector) -> String {
    match s {
        BackendSelector::Id(id) => format!("id:{id}"),
        BackendSelector::Tag(t) => format!("tag:{t}"),
        BackendSelector::Glob(g) => format!("glob:{g}"),
    }
}

/// Parse what the editor's field holds back into a selector.
///
/// A bare string is an id when it is a valid one, and a glob when it contains `*` — which
/// is what an operator means when they type `vast-*` into the box.
pub fn parse_selector(s: &str) -> anyhow::Result<BackendSelector> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("id:") {
        return Ok(BackendSelector::Id(BackendId::parse(rest.trim())?));
    }
    if let Some(rest) = s.strip_prefix("tag:") {
        let t = rest.trim();
        anyhow::ensure!(!t.is_empty(), "tag selector is empty");
        return Ok(BackendSelector::Tag(t.to_string()));
    }
    if let Some(rest) = s.strip_prefix("glob:") {
        let g = rest.trim();
        anyhow::ensure!(!g.is_empty(), "glob selector is empty");
        return Ok(BackendSelector::Glob(g.to_string()));
    }
    anyhow::ensure!(!s.is_empty(), "target selector is empty");
    if s.contains('*') || s.contains('?') {
        return Ok(BackendSelector::Glob(s.to_string()));
    }
    Ok(BackendSelector::Id(BackendId::parse(s)?))
}

/// Split a comma-separated field into trimmed, non-empty parts.
pub fn split_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .map(|p| p.to_string())
        .collect()
}

/// `Some(v)` for a non-blank field, `None` for a blank one. The difference between
/// "unset" and "empty string" is load-bearing on every optional spec field.
pub fn opt(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// A price model rendered for a card.
pub fn price_text(p: Option<&PriceModel>) -> String {
    match p {
        None => "—".to_string(),
        Some(PriceModel::Free) => "free".to_string(),
        Some(PriceModel::PerHour { dph }) => format!("{}/hr", fmt_money(*dph)),
        Some(PriceModel::PerToken { input, output }) => {
            format!("{}/{} per Mtok", fmt_money(*input), fmt_money(*output))
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Row builders
// ─────────────────────────────────────────────────────────────────────────────

/// One row per GPU **view**. The VRAM budget is computed per backend, so a single
/// physical GPU enumerated by both Vulkan and ROCm appears twice here on purpose and is
/// counted once in the budget the daemon returns.
pub fn device_rows(rig: &RigSnapshot, checked: &[String]) -> Vec<DeviceRow> {
    rig.gpus
        .iter()
        .map(|g| {
            let overcommit = g.reports_gtt_overcommit();
            let frac = match (overcommit, g.vram_total_mb) {
                (true, _) | (_, 0) => 0.0,
                (false, total) => {
                    let used = g.vram_used_mb().unwrap_or(0);
                    (used as f32 / total as f32).clamp(0.0, 1.0)
                }
            };
            let detail = if overcommit {
                format!(
                    "{} free (driver reports free > total)",
                    fmt_mb(g.vram_free_mb)
                )
            } else {
                format!(
                    "{} free of {}",
                    fmt_mb(g.vram_free_mb),
                    fmt_mb(g.vram_total_mb)
                )
            };
            DeviceRow {
                device: g.device.clone().into(),
                name: g.name.clone().into(),
                backend: format!("{:?}", g.backend).to_lowercase().into(),
                free_mb: g.vram_free_mb.min(i32::MAX as u64) as i32,
                total_mb: g.vram_total_mb.min(i32::MAX as u64) as i32,
                frac,
                detail: detail.into(),
                holder: g
                    .held_by
                    .iter()
                    .map(|b| b.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
                    .into(),
                builds: g
                    .seen_by_builds
                    .iter()
                    .map(|b| b.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
                    .into(),
                software: g.is_software,
                checked: checked.iter().any(|d| d == &g.device),
                level: if g.is_software {
                    2
                } else if frac > 0.92 {
                    3
                } else {
                    5
                },
            }
        })
        .collect()
}

/// The llama.cpp builds the daemon found.
pub fn build_rows(rig: &RigSnapshot) -> Vec<BuildRow> {
    rig.builds
        .iter()
        .map(|b| BuildRow {
            id: b.id.to_string().into(),
            label: b.label.clone().into(),
            backends: b
                .backends
                .iter()
                .map(|x| format!("{x:?}").to_lowercase())
                .collect::<Vec<_>>()
                .join(", ")
                .into(),
            server_path: b.server_path.clone().into(),
            devices: b.devices.join(", ").into(),
        })
        .collect()
}

/// Discovered local GGUFs, shards grouped and sized for real.
pub fn local_model_rows(models: &[LocalModel]) -> Vec<LocalModelRow> {
    models
        .iter()
        .map(|m| LocalModelRow {
            id: m.id.clone().into(),
            name: m.name.clone().into(),
            dir: m.dir.clone().into(),
            size: fmt_bytes(m.total_bytes).into(),
            quant: m.quant.clone().unwrap_or_default().into(),
            vision: m.is_vision(),
            shards: m.shards.len() as i32,
            path: m.primary_path().unwrap_or_default().into(),
            arch: m
                .gguf
                .as_ref()
                .map(|g| g.arch.clone())
                .unwrap_or_default()
                .into(),
            ctx_train: m
                .gguf
                .as_ref()
                .map(|g| g.n_ctx_train.to_string())
                .unwrap_or_else(|| "—".to_string())
                .into(),
        })
        .collect()
}

/// Backend cards. `device_filter` is the rig-strip click-through; an empty string keeps
/// everything.
pub fn backend_rows(backends: &[Backend], device_filter: &str) -> Vec<BackendRow> {
    backends
        .iter()
        .filter(|b| device_filter.is_empty() || b.devices.iter().any(|d| d == device_filter))
        .map(|b| {
            let (health, level) = health_text(&b.health);
            let (slots, uptime) = match &b.health {
                Health::Ready {
                    slots_busy,
                    slots_total,
                    since_unix,
                    ..
                } => (
                    format!("{slots_busy}/{slots_total}"),
                    fmt_dur((now_unix() - since_unix).max(0) as f64),
                ),
                Health::Starting { since_unix, .. } => (
                    "—".to_string(),
                    fmt_dur((now_unix() - since_unix).max(0) as f64),
                ),
                _ => ("—".to_string(), "—".to_string()),
            };
            let latency = match &b.health {
                Health::Ready {
                    tps_p50: Some(t), ..
                } => format!("{t:.1} tok/s"),
                _ => "—".to_string(),
            };
            BackendRow {
                id: b.id.to_string().into(),
                label: b.label.clone().into(),
                kind: format!("{:?}", b.kind).to_lowercase().into(),
                protocol: b.protocol.as_str().into(),
                health: health.into(),
                level,
                models: b
                    .models
                    .iter()
                    .map(|m| m.id.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
                    .into(),
                slots: slots.into(),
                queue: b.limits.queue_depth.to_string().into(),
                latency: latency.into(),
                price: price_text(b.price.as_ref()).into(),
                devices: b.devices.join(", ").into(),
                tags: b.tags.join(", ").into(),
                base_url: b.base_url.clone().into(),
                last_error: b.last_error.clone().unwrap_or_default().into(),
                uptime: uptime.into(),
                enabled: b.enabled,
                stoppable: b.endpoint.is_some(),
            }
        })
        .collect()
}

/// Route rows, with the health roll-up computed from the targets that currently resolve.
pub fn route_rows(routes: &[ModelRoute], backends: &[Backend]) -> Vec<RouteRow> {
    routes
        .iter()
        .map(|r| {
            let matched: Vec<&Backend> = backends
                .iter()
                .filter(|b| r.targets.iter().any(|t| selector_matches(&t.backend, b)))
                .collect();
            let routable = matched.iter().filter(|b| b.health.is_routable()).count();
            let level = if matched.is_empty() {
                4
            } else if routable == matched.len() {
                1
            } else if routable > 0 {
                2
            } else {
                3
            };
            let health = if matched.is_empty() {
                "no target resolves".to_string()
            } else {
                format!("{routable}/{} ready", matched.len())
            };
            let tps: Vec<f32> = matched
                .iter()
                .filter_map(|b| match &b.health {
                    Health::Ready { tps_p50, .. } => *tps_p50,
                    _ => None,
                })
                .collect();
            let cost = matched
                .iter()
                .filter_map(|b| b.price.as_ref())
                .map(|p| p.per_mtok(tps.first().copied()))
                .fold(CostEstimate::Unknown, CostEstimate::add);
            RouteRow {
                alias: r.alias.to_string().into(),
                targets: r
                    .targets
                    .iter()
                    .map(|t| match &t.model {
                        Some(m) => format!("{}→{m}", selector_text(&t.backend)),
                        None => selector_text(&t.backend),
                    })
                    .collect::<Vec<_>>()
                    .join("  ·  ")
                    .into(),
                strategy: format!("{:?}", r.strategy).to_lowercase().into(),
                health: health.into(),
                level,
                ttft: "—".into(),
                tps: if tps.is_empty() {
                    "—".to_string()
                } else {
                    format!("{:.1}", tps.iter().sum::<f32>() / tps.len() as f32)
                }
                .into(),
                cost: cost_text(&cost).0.into(),
                is_default: r.is_default,
                description: r.description.clone().unwrap_or_default().into(),
            }
        })
        .collect()
}

/// Does this selector name that backend? Id and tag are exact; glob is `*`/`?` only,
/// which is the same shape the daemon compiles.
pub fn selector_matches(sel: &BackendSelector, b: &Backend) -> bool {
    match sel {
        BackendSelector::Id(id) => id == &b.id,
        BackendSelector::Tag(t) => b.tags.iter().any(|x| x == t),
        BackendSelector::Glob(g) => glob_matches(g, b.id.as_str()),
    }
}

/// A `*`/`?` glob, matched iteratively so a pathological pattern cannot blow the stack.
pub fn glob_matches(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = ti;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// The editable target list for one route.
pub fn target_rows(r: &ModelRoute, backends: &[Backend]) -> Vec<TargetRow> {
    r.targets
        .iter()
        .map(|t| {
            let hits: Vec<String> = backends
                .iter()
                .filter(|b| selector_matches(&t.backend, b))
                .map(|b| b.id.to_string())
                .collect();
            TargetRow {
                backend: selector_text(&t.backend).into(),
                model: t.model.clone().unwrap_or_default().into(),
                weight: t.weight as i32,
                resolves: if hits.is_empty() {
                    "resolves to nothing right now".to_string()
                } else {
                    format!("resolves to {}", hits.join(", "))
                }
                .into(),
                level: if hits.is_empty() { 3 } else { 0 },
            }
        })
        .collect()
}

/// One finished request.
pub fn request_row(r: &RequestRecord) -> RequestRow {
    let level = if r.aborted {
        2
    } else if r.status >= 500 {
        4
    } else if r.status >= 400 {
        3
    } else {
        1
    };
    RequestRow {
        id: r.id.to_string().into(),
        time: fmt_ago(r.started_unix).into(),
        alias: r
            .alias
            .as_ref()
            .map(|a| a.to_string())
            .unwrap_or_else(|| "—".to_string())
            .into(),
        backend: r
            .backend
            .as_ref()
            .map(|b| b.to_string())
            .unwrap_or_else(|| "—".to_string())
            .into(),
        model: r.upstream_model.clone().unwrap_or_default().into(),
        status: r.status.to_string().into(),
        level,
        ttft: r
            .ttft_ms
            .map(|m| format!("{m} ms"))
            .unwrap_or_else(|| "—".to_string())
            .into(),
        tps: r
            .tok_per_s
            .map(|t| format!("{t:.1}"))
            .unwrap_or_else(|| "—".to_string())
            .into(),
        tokens: format!(
            "{}/{}",
            token_text(r.prompt_tokens.as_ref()),
            token_text(r.completion_tokens.as_ref())
        )
        .into(),
        cost: cost_text(&r.cost).0.into(),
        attempts: r.attempts as i32,
        reason: r.route_reason.as_str().into(),
        inflight: false,
    }
}

/// Rented boxes, with the accrued cost spelled out rather than implied.
pub fn instance_rows(instances: &[VastInstance], tunnels: &[TunnelStatus]) -> Vec<InstanceRow> {
    instances
        .iter()
        .map(|i| {
            let (phase, level, _) = boot_text(&i.phase());
            let uptime = i.uptime_secs();
            let accrued = match (uptime, i.dph_total) {
                (Some(secs), Some(dph)) => fmt_money(Money::from_usd(dph * secs / 3600.0)),
                _ => "—".to_string(),
            };
            InstanceRow {
                id: i.id.to_string().into(),
                label: i.label.clone().unwrap_or_default().into(),
                gpu: match (i.num_gpus, &i.gpu_name) {
                    (Some(n), Some(g)) => format!("{n}× {g}"),
                    (_, Some(g)) => g.clone(),
                    _ => "—".to_string(),
                }
                .into(),
                status: phase.into(),
                level,
                uptime: uptime
                    .map(fmt_dur)
                    .unwrap_or_else(|| "—".to_string())
                    .into(),
                accrued: accrued.into(),
                dph: i
                    .dph_total
                    .map(|d| format!("{}/hr", fmt_money(Money::from_usd(d))))
                    .unwrap_or_else(|| "—".to_string())
                    .into(),
                geo: i.geolocation.clone().unwrap_or_default().into(),
                disk: i
                    .disk_space
                    .map(|d| format!("{d:.0} GB"))
                    .unwrap_or_else(|| "—".to_string())
                    .into(),
                tunnel: tunnels.iter().any(|t| t.spec.instance_id == i.id && t.up),
                stalled: false,
                stall_note: i.status_msg.clone().unwrap_or_default().into(),
                orphan: false,
            }
        })
        .collect()
}

/// SSH tunnels.
pub fn tunnel_rows(tunnels: &[TunnelStatus]) -> Vec<TunnelRow> {
    tunnels
        .iter()
        .map(|t| TunnelRow {
            instance: t.spec.instance_id.to_string().into(),
            local_port: t.spec.local_port as i32,
            remote_port: t.spec.remote_port as i32,
            ssh: format!("{}:{}", t.spec.ssh_host, t.spec.ssh_port).into(),
            up: t.up,
            restarts: t.restarts as i32,
            last_error: t.last_error.clone().unwrap_or_default().into(),
            level: if t.up { 1 } else { 3 },
        })
        .collect()
}

/// Market offers.
pub fn offer_rows(offers: &[Offer]) -> Vec<OfferRow> {
    offers
        .iter()
        .map(|o| OfferRow {
            id: o.id.to_string().into(),
            gpu: o.gpu_name.clone().into(),
            num_gpus: o.num_gpus as i32,
            vram: fmt_mb(o.pooled_vram_mb()).into(),
            dph: fmt_money(Money::from_usd(o.dph_total)).into(),
            reliability: o
                .reliability2
                .map(|r| format!("{:.3}", r))
                .unwrap_or_else(|| "—".to_string())
                .into(),
            inet: o
                .inet_down
                .map(|d| format!("{d:.0} Mb/s"))
                .unwrap_or_else(|| "—".to_string())
                .into(),
            geo: o.geolocation.clone().unwrap_or_default().into(),
            disk: o
                .disk_space
                .map(|d| format!("{d:.0} GB"))
                .unwrap_or_else(|| "—".to_string())
                .into(),
            cuda: o
                .cuda_max_good
                .map(|c| format!("{c:.1}"))
                .unwrap_or_else(|| "—".to_string())
                .into(),
            rentable: o.rentable.unwrap_or(true) && !o.rented.unwrap_or(false),
        })
        .collect()
}

/// Managed providers. The credential's **source** is shown; the value never is.
pub fn provider_rows(providers: &[ProviderStatus]) -> Vec<ProviderRow> {
    providers
        .iter()
        .map(|p| ProviderRow {
            id: p.id.to_string().into(),
            base_url: p.base_url.clone().into(),
            source: credential_source(&p.credential).into(),
            present: p.credential_present,
            models: p.models_cached.min(i32::MAX as u32) as i32,
            last_ok: p
                .last_ok_unix
                .map(fmt_ago)
                .unwrap_or_else(|| "never".to_string())
                .into(),
            last_error: p.last_error.clone().unwrap_or_default().into(),
            level: match (&p.last_error, p.credential_present) {
                (Some(_), _) => 3,
                (None, true) => 1,
                (None, false) => 2,
            },
        })
        .collect()
}

/// A provider's live catalogue, grouped by the org prefix of the model id.
pub fn model_rows(models: &[UpstreamModel], filter: &str) -> Vec<ModelRow> {
    let f = filter.trim().to_lowercase();
    let mut rows: Vec<ModelRow> = models
        .iter()
        .filter(|m| f.is_empty() || m.id.to_lowercase().contains(&f))
        .map(|m| ModelRow {
            org: m
                .id
                .split_once('/')
                .map(|(o, _)| o.to_string())
                .unwrap_or_else(|| "—".to_string())
                .into(),
            id: m.id.clone().into(),
            ctx: m
                .ctx
                .map(|c| c.to_string())
                .unwrap_or_else(|| "—".to_string())
                .into(),
            vision: m.vision,
            tools: m.tools,
        })
        .collect();
    rows.sort_by(|a, b| (a.org.as_str(), a.id.as_str()).cmp(&(b.org.as_str(), b.id.as_str())));
    rows
}

/// Background jobs.
pub fn job_rows(jobs: &[JobRecord]) -> Vec<JobRow> {
    jobs.iter()
        .map(|j| {
            let (state, level) = job_text(j.state);
            JobRow {
                id: j.id.to_string().into(),
                kind: j.kind.clone().into(),
                state: state.into(),
                level,
                pct: j.pct.map(|p| (p / 100.0).clamp(0.0, 1.0)).unwrap_or(0.0),
                message: j
                    .message
                    .clone()
                    .or_else(|| j.error.clone())
                    .unwrap_or_default()
                    .into(),
                started: fmt_ago(j.started_unix).into(),
            }
        })
        .collect()
}

/// Standing alerts.
pub fn alert_rows(alerts: &[Alert]) -> Vec<crate::AlertRow> {
    alerts
        .iter()
        .map(|a| crate::AlertRow {
            id: a.id.clone().into(),
            level: alert_level(a.level),
            message: a.message.clone().into(),
            action: a.action.clone().unwrap_or_default().into(),
            at: fmt_ago(a.at_unix).into(),
        })
        .collect()
}

/// Check results.
pub fn check_rows(results: &[CheckResult]) -> Vec<CheckRow> {
    results
        .iter()
        .map(|c| {
            let (status, level) = check_text(c.status);
            CheckRow {
                id: c.id.to_string().into(),
                label: c.label.clone().into(),
                status: status.into(),
                level,
                ms: format!("{} ms", c.ms).into(),
                detail: c.detail.clone().into(),
                fix: c.fix.clone().unwrap_or_default().into(),
            }
        })
        .collect()
}

/// Usage buckets, scaled against the largest one so the bar row reads at a glance.
pub fn usage_rows(u: &UsageSummary) -> Vec<UsageRow> {
    let max =
        u.by.iter()
            .filter_map(|b| b.cost.usd().map(|m| m.0.max(0)))
            .max()
            .unwrap_or(0);
    u.by.iter()
        .map(|b| {
            let (cost, metered) = cost_text(&b.cost);
            UsageRow {
                key: b.key.clone().into(),
                cost: cost.into(),
                prompt: b.prompt_tokens.to_string().into(),
                completion: b.completion_tokens.to_string().into(),
                requests: b.requests.to_string().into(),
                tps: b
                    .tok_per_s_p50
                    .map(|t| format!("{t:.1} tok/s"))
                    .unwrap_or_else(|| "—".to_string())
                    .into(),
                frac: if max > 0 {
                    (b.cost.usd().map(|m| m.0.max(0)).unwrap_or(0) as f32) / (max as f32)
                } else {
                    0.0
                },
                metered,
            }
        })
        .collect()
}

/// Hugging Face file groups. `paths-info` is the authoritative size source; a group with
/// no bytes says so instead of showing a confident zero.
pub fn hf_rows(groups: &[HfFileGroup]) -> Vec<HfFileRow> {
    groups
        .iter()
        .map(|g| HfFileRow {
            label: g.label.clone().into(),
            quant: g.quant.clone().unwrap_or_default().into(),
            size: if g.total_bytes == 0 {
                "size unknown".to_string()
            } else {
                fmt_bytes(g.total_bytes)
            }
            .into(),
            files: g.files.len() as i32,
            mmproj: !g.mmproj.is_empty(),
        })
        .collect()
}

/// Group a flat `Vec<HfFile>` by quant, for the shape of `/v1/hf/models/{repo}/files`
/// that returns files rather than groups. Shards are summed, exactly as P-07 does.
pub fn group_hf_files(files: &[HfFile]) -> Vec<HfFileGroup> {
    let mut out: Vec<HfFileGroup> = Vec::new();
    for f in files {
        let key = f.quant.clone().unwrap_or_else(|| f.rfilename.clone());
        match out.iter_mut().find(|g| g.label == key) {
            Some(g) => {
                g.total_bytes = g.total_bytes.saturating_add(f.size.unwrap_or(0));
                if f.is_mmproj {
                    g.mmproj.push(f.clone());
                } else {
                    g.files.push(f.clone());
                }
            }
            None => out.push(HfFileGroup {
                label: key,
                quant: f.quant.clone(),
                total_bytes: f.size.unwrap_or(0),
                files: if f.is_mmproj { vec![] } else { vec![f.clone()] },
                mmproj: if f.is_mmproj { vec![f.clone()] } else { vec![] },
            }),
        }
    }
    out
}

/// Saved market queries.
pub fn profile_rows(profiles: &[apexrouter_protocol::SearchProfile]) -> Vec<ProfileRow> {
    profiles
        .iter()
        .map(|p| ProfileRow {
            id: p.id.to_string().into(),
            label: p.label.clone().into(),
            gpus: if p.gpu_names.is_empty() {
                "any".to_string()
            } else {
                p.gpu_names.join(", ")
            }
            .into(),
            max_dph: p
                .max_dph
                .map(fmt_money)
                .unwrap_or_else(|| "—".to_string())
                .into(),
            geo: format!("{:?}", p.geo).to_lowercase().into(),
            image_type: format!("{:?}", p.image_type).to_lowercase().into(),
        })
        .collect()
}

/// Saved launch plans, with the staleness flags that stop a recipe silently rotting.
pub fn recipe_rows(
    recipes: &[apexrouter_protocol::Recipe],
    rig: &RigSnapshot,
    models: &[LocalModel],
) -> Vec<RecipeRow> {
    recipes
        .iter()
        .map(|r| {
            let (kind, stale_why) = match &r.kind {
                apexrouter_protocol::RecipeKind::Local(s) => {
                    let build_gone = !rig.builds.iter().any(|b| b.id == s.build);
                    let model_gone = !models.is_empty()
                        && !models
                            .iter()
                            .any(|m| m.shards.iter().any(|sh| sh.path == s.model_path));
                    let mut why = Vec::new();
                    if build_gone {
                        why.push(format!("build `{}` is gone", s.build));
                    }
                    if model_gone {
                        why.push(format!("model file `{}` is gone", s.model_path));
                    }
                    ("local".to_string(), why.join("; "))
                }
                apexrouter_protocol::RecipeKind::LocalVllm(_) => {
                    ("vllm".to_string(), String::new())
                }
                apexrouter_protocol::RecipeKind::Vast { profile, .. } => (
                    "vast".to_string(),
                    format!("rents against profile `{profile}`"),
                ),
                apexrouter_protocol::RecipeKind::Managed(_) => {
                    ("managed".to_string(), String::new())
                }
            };
            let stale = matches!(&r.kind, apexrouter_protocol::RecipeKind::Local(_))
                && !stale_why.is_empty();
            RecipeRow {
                id: r.id.to_string().into(),
                label: r.label.clone().into(),
                kind: kind.into(),
                description: r.description.clone().unwrap_or_default().into(),
                stale,
                stale_why: stale_why.into(),
                updated: fmt_ago(r.updated_at_unix).into(),
            }
        })
        .collect()
}

/// The fit readout: the three fractions of the budget, the verdict, and `why[]` verbatim.
pub fn fit_view(
    plan: &FitPlan,
    budget_mb: u64,
) -> (String, i32, f32, f32, f32, String, Vec<String>) {
    let total = if budget_mb > 0 {
        budget_mb as f32
    } else {
        (plan.weights_mb + plan.kv_mb + plan.compute_mb).max(1) as f32
    };
    let (verdict, level) = match &plan.verdict {
        FitVerdict::Fits { headroom_mb } => (format!("fits · {} spare", fmt_mb(*headroom_mb)), 1),
        FitVerdict::Tight { headroom_mb } => (format!("tight · {} spare", fmt_mb(*headroom_mb)), 2),
        FitVerdict::NeedsOffload { layers_on_gpu } => {
            (format!("needs offload · {layers_on_gpu} layers on GPU"), 3)
        }
        FitVerdict::WontFit { short_by_mb } => {
            (format!("won't fit · short by {}", fmt_mb(*short_by_mb)), 4)
        }
    };
    let caption = format!(
        "weights {} · kv {} · compute {} · headroom {}",
        fmt_mb(plan.weights_mb),
        fmt_mb(plan.kv_mb),
        fmt_mb(plan.compute_mb),
        if plan.headroom_mb < 0 {
            format!("-{}", fmt_mb(plan.headroom_mb.unsigned_abs()))
        } else {
            fmt_mb(plan.headroom_mb as u64)
        }
    );
    (
        verdict,
        level,
        plan.weights_mb as f32 / total,
        plan.kv_mb as f32 / total,
        plan.compute_mb as f32 / total,
        caption,
        plan.why.clone(),
    )
}

/// A one-line summary of the budget the daemon computed, per backend.
pub fn budget_line(devices: &[DeviceBudget], margin_mb: u64) -> String {
    if devices.is_empty() {
        return "no devices selected — the plan is CPU-only".to_string();
    }
    let usable: u64 = devices
        .iter()
        .map(|d| d.free_mb.saturating_sub(d.reserved_mb))
        .fold(0, u64::saturating_add)
        .saturating_sub(margin_mb);
    format!(
        "{} usable across {} device(s), {} margin",
        fmt_mb(usable),
        devices.len(),
        fmt_mb(margin_mb)
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// The store: what the UI thread needs that is not a Slint property
// ─────────────────────────────────────────────────────────────────────────────

/// Everything a callback needs to rebuild a request body from what is on screen.
///
/// `Arc<Mutex<_>>` rather than `Rc<RefCell<_>>` because `upgrade_in_event_loop` requires a
/// `Send` closure; in practice it is only ever locked on the UI thread.
#[derive(Default)]
pub struct Store {
    /// The last full snapshot.
    pub snapshot: Option<Snapshot>,
    /// Discovered local GGUFs, from `GET /v1/models/local`.
    pub local_models: Vec<LocalModel>,
    /// The last offer search.
    pub offers: Vec<Offer>,
    /// The last provider catalogue.
    pub provider_models: Vec<UpstreamModel>,
    /// The last HF file listing.
    pub hf_groups: Vec<HfFileGroup>,
    /// Device tokens ticked in the Launch tabs.
    pub checked_devices: Vec<String>,
    /// The rig-strip click-through.
    pub device_filter: String,
    /// The whole log buffer. The UI shows a filtered view of it.
    pub log_buffer: VecDeque<String>,
    /// Which source the buffer came from.
    pub log_source: String,
    /// Live request rows, newest first.
    pub requests: VecDeque<RequestRow>,
    /// When the current boot started, for the elapsed timer.
    pub boot_started: Option<i64>,
    /// Which backend the boot drawer is following.
    pub boot_backend: String,
}

impl Store {
    /// The routes in the last snapshot, or an empty slice.
    pub fn routes(&self) -> &[ModelRoute] {
        self.snapshot.as_ref().map(|s| &s.routes[..]).unwrap_or(&[])
    }

    /// The backends in the last snapshot, or an empty slice.
    pub fn backends(&self) -> &[Backend] {
        self.snapshot
            .as_ref()
            .map(|s| &s.backends[..])
            .unwrap_or(&[])
    }

    /// Push a log line, dropping the oldest once the buffer is full.
    pub fn push_log(&mut self, line: String) {
        if self.log_buffer.len() >= LOG_BUFFER {
            self.log_buffer.pop_front();
        }
        self.log_buffer.push_back(line);
    }

    /// The filtered view of the buffer, plus how many lines the filter is hiding.
    pub fn log_view(&self, filter: &str) -> (Vec<SharedString>, i32) {
        let f = filter.trim().to_lowercase();
        if f.is_empty() {
            return (self.log_buffer.iter().map(SharedString::from).collect(), 0);
        }
        let mut shown = Vec::new();
        let mut hidden = 0i32;
        for line in &self.log_buffer {
            if line.to_lowercase().contains(&f) {
                shown.push(SharedString::from(line));
            } else {
                hidden += 1;
            }
        }
        (shown, hidden)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The bridge
// ─────────────────────────────────────────────────────────────────────────────

/// The handle every callback captures: a client, a runtime handle, a weak window and the
/// shared store. Cloning it is cheap — the client is an `Arc` inside.
#[derive(Clone)]
pub struct Bridge {
    client: NodeClient,
    handle: Handle,
    ui: Weak<AppWindow>,
    store: Arc<Mutex<Store>>,
    log_generation: Arc<AtomicU64>,
}

impl Bridge {
    /// Build a bridge around one control plane.
    pub fn new(client: NodeClient, handle: Handle, ui: Weak<AppWindow>) -> Self {
        Bridge {
            client,
            handle,
            ui,
            store: Arc::new(Mutex::new(Store::default())),
            log_generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The shared store.
    pub fn store(&self) -> Arc<Mutex<Store>> {
        self.store.clone()
    }

    /// The window, weakly.
    pub fn ui(&self) -> Weak<AppWindow> {
        self.ui.clone()
    }

    /// Run one fallible request off the UI thread and apply the value back on it.
    ///
    /// The whole request is one inner `async { … }.await`, so there is exactly one match
    /// on failure and exactly one place a toast is raised.
    pub fn fetch<T, Fut, J, A>(&self, what: &'static str, job: J, apply: A)
    where
        T: Send + 'static,
        Fut: Future<Output = anyhow::Result<T>> + Send + 'static,
        J: FnOnce(NodeClient) -> Fut + Send + 'static,
        A: FnOnce(&AppWindow, &Arc<Mutex<Store>>, T) + Send + 'static,
    {
        let ui = self.ui.clone();
        let client = self.client.clone();
        let store = self.store.clone();
        set_busy(&self.ui, true);
        self.handle.spawn(async move {
            let outcome = async { job(client).await }.await;
            let _ = ui.upgrade_in_event_loop(move |ui| {
                ui.global::<State>().set_busy(false);
                match outcome {
                    Ok(value) => apply(&ui, &store, value),
                    Err(e) => toast(&ui, &format!("{what} failed: {e}"), 4),
                }
            });
        });
    }

    /// Run one fallible **write**, toast the message it returns, and re-pull the snapshot
    /// so the screen reflects what actually happened rather than what was asked for.
    pub fn act<Fut, J>(&self, what: &'static str, job: J)
    where
        Fut: Future<Output = anyhow::Result<String>> + Send + 'static,
        J: FnOnce(NodeClient) -> Fut + Send + 'static,
    {
        let me = self.clone();
        self.fetch(what, job, move |ui, _store, msg| {
            toast(ui, &msg, 1);
            me.refresh();
        });
    }

    /// Pull `GET /v1/snapshot` and render all of it.
    pub fn refresh(&self) {
        self.fetch(
            "snapshot",
            |c| async move {
                let snap = c.snapshot().await?;
                anyhow::Ok(snap)
            },
            |ui, store, snap| {
                if let Ok(mut s) = store.lock() {
                    s.snapshot = Some(snap);
                }
                apply_snapshot(ui, store);
                ui.global::<State>().set_connected(true);
            },
        );
    }

    /// Refresh the discovered local models, which are not part of the snapshot.
    pub fn refresh_local_models(&self) {
        self.fetch(
            "local models",
            |c| async move {
                let models: Vec<LocalModel> = c.get("/v1/models/local").await?;
                anyhow::Ok(models)
            },
            |ui, store, models| {
                let rows = local_model_rows(&models);
                if let Ok(mut s) = store.lock() {
                    s.local_models = models;
                }
                ui.global::<State>()
                    .set_local_models(ModelRc::new(VecModel::from(rows)));
            },
        );
    }

    /// Hold `/ws` open for the lifetime of the app.
    ///
    /// The stream is endless by contract: an `Err` item is a blip, not a terminator, so
    /// this loop keeps polling after one and only flips the connection dot.
    pub fn spawn_ws(&self) {
        let ui = self.ui.clone();
        let client = self.client.clone();
        let store = self.store.clone();
        let me = self.clone();
        self.handle.spawn(async move {
            loop {
                let opened = async { anyhow::Ok(client.subscribe().await?) }.await;
                let mut stream = match opened {
                    Ok(s) => {
                        let _ = ui.upgrade_in_event_loop(|ui| {
                            ui.global::<State>().set_connected(true);
                        });
                        Box::pin(s)
                    }
                    Err(e) => {
                        let msg = format!("{e}");
                        let _ = ui.upgrade_in_event_loop(move |ui| {
                            ui.global::<State>().set_connected(false);
                            toast(&ui, &format!("not connected: {msg}"), 3);
                        });
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        continue;
                    }
                };
                while let Some(item) = futures_util::StreamExt::next(&mut stream).await {
                    match item {
                        Ok(event) => {
                            let store = store.clone();
                            let _ = ui.upgrade_in_event_loop(move |ui| {
                                ui.global::<State>().set_connected(true);
                                apply_event(&ui, &store, event);
                            });
                        }
                        Err(_) => {
                            let _ = ui.upgrade_in_event_loop(|ui| {
                                ui.global::<State>().set_connected(false);
                            });
                        }
                    }
                }
                // The stream only ends if the daemon went away for good; re-subscribe.
                let _ = ui.upgrade_in_event_loop(|ui| {
                    ui.global::<State>().set_connected(false);
                });
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                me.refresh();
            }
        });
    }

    /// Consume `GET /v1/diagnose[?only=…]`, which streams **one event per check**.
    ///
    /// Each `CheckResult` is folded into the table as it lands rather than batched at the
    /// end, so a doctor run that hangs on probe three still shows probes one and two.
    pub fn follow_checks(&self, path: String) {
        let ui = self.ui.clone();
        let client = self.client.clone();
        let store = self.store.clone();
        self.handle.spawn(async move {
            let opened = async { anyhow::Ok(client.sse(&path).await?) }.await;
            let mut stream = match opened {
                Ok(s) => Box::pin(s),
                Err(e) => {
                    let msg = format!("{e}");
                    let _ = ui.upgrade_in_event_loop(move |ui| {
                        ui.global::<State>().set_doctor_running(false);
                        toast(&ui, &format!("diagnose failed: {msg}"), 4);
                    });
                    return;
                }
            };
            while let Some(item) = futures_util::StreamExt::next(&mut stream).await {
                if let Ok(event) = item {
                    let store = store.clone();
                    let _ = ui.upgrade_in_event_loop(move |ui| apply_event(&ui, &store, event));
                }
            }
            let _ = ui.upgrade_in_event_loop(|ui| {
                ui.global::<State>().set_doctor_running(false);
            });
        });
    }

    /// Follow one log source over SSE. Opening another source retires this one, so two
    /// tails can never interleave into the same buffer.
    pub fn follow_logs(&self, path: String, source: String) {
        let generation = self.log_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let gen_handle = self.log_generation.clone();
        let ui = self.ui.clone();
        let client = self.client.clone();
        let store = self.store.clone();

        {
            let store = store.clone();
            let source = source.clone();
            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                if let Ok(mut s) = store.lock() {
                    s.log_buffer.clear();
                    s.log_source = source;
                }
                render_logs(&ui, &store);
            });
        }

        self.handle.spawn(async move {
            let opened = async { anyhow::Ok(client.sse(&path).await?) }.await;
            let mut stream = match opened {
                Ok(s) => Box::pin(s),
                Err(e) => {
                    let msg = format!("{e}");
                    let _ = ui.upgrade_in_event_loop(move |ui| {
                        toast(&ui, &format!("logs failed: {msg}"), 3);
                    });
                    return;
                }
            };
            while let Some(item) = futures_util::StreamExt::next(&mut stream).await {
                if gen_handle.load(Ordering::SeqCst) != generation {
                    return;
                }
                let line = match item {
                    Ok(Event::LogLine { line, .. }) => line,
                    Ok(Event::BootProgress { phase, line, .. }) => {
                        line.unwrap_or_else(|| boot_text(&phase).0)
                    }
                    Ok(_) => continue,
                    Err(e) => format!("[stream] {e}"),
                };
                let store = store.clone();
                let _ = ui.upgrade_in_event_loop(move |ui| {
                    if let Ok(mut s) = store.lock() {
                        s.push_log(line);
                    }
                    render_logs(&ui, &store);
                });
            }
        });
    }
}

/// Flip the busy flag without waiting for the round trip.
fn set_busy(ui: &Weak<AppWindow>, busy: bool) {
    let _ = ui.upgrade_in_event_loop(move |ui| ui.global::<State>().set_busy(busy));
}

/// Raise a one-line message. Level follows the row convention: 1 good, 2 warn, 3 serious,
/// 4 critical.
pub fn toast(ui: &AppWindow, message: &str, level: i32) {
    let st = ui.global::<State>();
    st.set_toast(message.into());
    st.set_toast_level(level);
}

/// Push the filtered log view into the UI.
pub fn render_logs(ui: &AppWindow, store: &Arc<Mutex<Store>>) {
    let st = ui.global::<State>();
    let filter = st.get_log_filter().to_string();
    if let Ok(s) = store.lock() {
        let (lines, hidden) = s.log_view(&filter);
        st.set_log_lines(ModelRc::new(VecModel::from(lines)));
        st.set_log_hidden(hidden);
        st.set_log_source(s.log_source.clone().into());
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Rendering a snapshot and the events that amend it
// ─────────────────────────────────────────────────────────────────────────────

/// Render the whole snapshot. Called on the UI thread, wholesale — a Slint model is
/// cheap to replace and a partial update is where a stale row hides.
pub fn apply_snapshot(ui: &AppWindow, store: &Arc<Mutex<Store>>) {
    let st = ui.global::<State>();
    let guard = match store.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let snap = match &guard.snapshot {
        Some(s) => s,
        None => return,
    };

    st.set_version(snap.version.clone().into());
    st.set_served_by(format!("{:?}", snap.served_by).to_lowercase().into());
    st.set_stale(snap.stale);
    st.set_base_url(snap.proxy.base_url.clone().into());
    st.set_control_url(snap.proxy.control_url.clone().into());
    st.set_inflight(snap.proxy.inflight.min(i32::MAX as u32) as i32);
    st.set_req_per_min(format!("{:.1}", snap.proxy.req_per_min).into());
    st.set_tok_per_s(format!("{:.1}", snap.proxy.tok_per_s).into());
    st.set_uptime(fmt_dur(snap.proxy.uptime_secs).into());
    st.set_default_alias(snap.proxy.default_alias.to_string().into());
    st.set_table_valid(snap.proxy.table_valid);
    st.set_table_error(snap.proxy.table_error.clone().unwrap_or_default().into());

    let (spend, metered) = cost_text(&snap.totals.spend_24h);
    st.set_spend_24h(spend.into());
    st.set_spend_metered(metered);
    st.set_spend_7d(cost_text(&snap.totals.spend_7d).0.into());
    st.set_credit(
        snap.totals
            .vast_credit
            .map(|c| fmt_money(Money::from_usd(c)))
            .unwrap_or_else(|| "—".to_string())
            .into(),
    );
    st.set_burn_rate(format!("{}/hr", fmt_money(snap.totals.burn_rate_usd_hr)).into());
    st.set_burn_down(
        snap.totals
            .burn_down_hours
            .map(|h| format!("{h:.1} h"))
            .unwrap_or_else(|| "—".to_string())
            .into(),
    );

    // Rig
    st.set_devices(ModelRc::new(VecModel::from(device_rows(
        &snap.rig,
        &guard.checked_devices,
    ))));
    let builds = build_rows(&snap.rig);
    st.set_build_names(ModelRc::new(VecModel::from(
        builds
            .iter()
            .map(|b| b.id.clone())
            .collect::<Vec<SharedString>>(),
    )));
    st.set_builds(ModelRc::new(VecModel::from(builds)));
    st.set_ram(
        format!(
            "{} free of {}",
            fmt_mb(snap.rig.ram_free_mb),
            fmt_mb(snap.rig.ram_total_mb)
        )
        .into(),
    );
    st.set_ram_frac(if snap.rig.ram_total_mb > 0 {
        1.0 - (snap.rig.ram_free_mb as f32 / snap.rig.ram_total_mb as f32).clamp(0.0, 1.0)
    } else {
        0.0
    });
    st.set_swap(
        format!(
            "{} of {}",
            fmt_mb(snap.rig.swap_used_mb),
            fmt_mb(snap.rig.swap_total_mb)
        )
        .into(),
    );
    st.set_swap_frac(if snap.rig.swap_total_mb > 0 {
        (snap.rig.swap_used_mb as f32 / snap.rig.swap_total_mb as f32).clamp(0.0, 1.0)
    } else {
        0.0
    });
    st.set_cpu_threads(snap.rig.cpu_threads.to_string().into());
    st.set_device_filter(guard.device_filter.clone().into());

    // Backends and routes
    st.set_backends(ModelRc::new(VecModel::from(backend_rows(
        &snap.backends,
        &guard.device_filter,
    ))));
    st.set_routes(ModelRc::new(VecModel::from(route_rows(
        &snap.routes,
        &snap.backends,
    ))));
    let aliases: Vec<SharedString> = snap
        .routes
        .iter()
        .map(|r| SharedString::from(r.alias.to_string()))
        .collect();
    let default_index = aliases
        .iter()
        .position(|a| a.as_str() == snap.proxy.default_alias.as_str())
        .unwrap_or(0) as i32;
    st.set_aliases(ModelRc::new(VecModel::from(aliases)));
    st.set_default_alias_index(default_index);

    // Fleet
    st.set_instances(ModelRc::new(VecModel::from(instance_rows(
        &snap.instances,
        &snap.tunnels,
    ))));
    st.set_tunnels(ModelRc::new(VecModel::from(tunnel_rows(&snap.tunnels))));

    // Catalog
    st.set_recipes(ModelRc::new(VecModel::from(recipe_rows(
        &snap.recipes,
        &snap.rig,
        &guard.local_models,
    ))));
    let profiles = profile_rows(&snap.profiles);
    st.set_profile_names(ModelRc::new(VecModel::from(
        profiles
            .iter()
            .map(|p| p.label.clone())
            .collect::<Vec<SharedString>>(),
    )));
    st.set_recipe_names(ModelRc::new(VecModel::from(
        snap.recipes
            .iter()
            .map(|r| SharedString::from(r.id.to_string()))
            .collect::<Vec<SharedString>>(),
    )));
    st.set_profiles(ModelRc::new(VecModel::from(profiles)));

    // Providers, alerts, jobs
    st.set_providers(ModelRc::new(VecModel::from(provider_rows(&snap.providers))));
    st.set_alerts(ModelRc::new(VecModel::from(alert_rows(&snap.alerts))));
    st.set_jobs(ModelRc::new(VecModel::from(job_rows(&snap.jobs))));
    st.set_downloads(ModelRc::new(VecModel::from(job_rows(
        &snap
            .jobs
            .iter()
            .filter(|j| j.kind.starts_with("hf."))
            .cloned()
            .collect::<Vec<_>>(),
    ))));

    // Whatever is selected stays selected, but its detail pane is re-derived.
    let selected = st.get_backend_sel().to_string();
    if let Some(b) = snap.backends.iter().find(|b| b.id.as_str() == selected) {
        apply_backend_detail(ui, b);
    }
}

/// Explode the selected backend into the detail pane's properties.
pub fn apply_backend_detail(ui: &AppWindow, b: &Backend) {
    let st = ui.global::<State>();
    let (health, level) = health_text(&b.health);
    let (slots, uptime) = match &b.health {
        Health::Ready {
            slots_busy,
            slots_total,
            since_unix,
            ..
        } => (
            format!("{slots_busy}/{slots_total}"),
            fmt_dur((now_unix() - since_unix).max(0) as f64),
        ),
        _ => ("—".to_string(), "—".to_string()),
    };
    st.set_sel_label(b.label.clone().into());
    st.set_sel_kind(format!("{:?}", b.kind).to_lowercase().into());
    st.set_sel_protocol(b.protocol.as_str().into());
    st.set_sel_base_url(b.base_url.clone().into());
    st.set_sel_health(health.into());
    st.set_sel_level(level);
    st.set_sel_models(
        b.models
            .iter()
            .map(|m| m.id.clone())
            .collect::<Vec<_>>()
            .join(", ")
            .into(),
    );
    st.set_sel_devices(b.devices.join(", ").into());
    st.set_sel_tags(b.tags.join(", ").into());
    st.set_sel_slots(slots.into());
    st.set_sel_queue(b.limits.queue_depth.to_string().into());
    st.set_sel_latency(
        match &b.health {
            Health::Ready {
                tps_p50: Some(t), ..
            } => format!("{t:.1} tok/s"),
            _ => "—".to_string(),
        }
        .into(),
    );
    st.set_sel_price(price_text(b.price.as_ref()).into());
    st.set_sel_uptime(uptime.into());
    st.set_sel_last_error(b.last_error.clone().unwrap_or_default().into());
    st.set_sel_enabled(b.enabled);
    st.set_sel_stoppable(b.endpoint.is_some());
}

/// Fold one WS event into the rendered state.
///
/// Anything that changes the shape of the world re-renders from the amended snapshot;
/// the two high-rate events (`RequestStarted`/`RequestFinished`) only touch the ticker,
/// because a router at 50 rps must not re-render its own dashboard fifty times a second.
pub fn apply_event(ui: &AppWindow, store: &Arc<Mutex<Store>>, event: Event) {
    let st = ui.global::<State>();
    match event {
        Event::Snapshot(snap) => {
            if let Ok(mut s) = store.lock() {
                s.snapshot = Some(*snap);
            }
            apply_snapshot(ui, store);
        }
        Event::BackendChanged { backend } => {
            let is_selected = st.get_backend_sel().as_str() == backend.id.as_str();
            if let Ok(mut s) = store.lock() {
                if let Some(snap) = s.snapshot.as_mut() {
                    match snap.backends.iter_mut().find(|b| b.id == backend.id) {
                        Some(slot) => *slot = (*backend).clone(),
                        None => snap.backends.push((*backend).clone()),
                    }
                }
            }
            if is_selected {
                apply_backend_detail(ui, &backend);
            }
            apply_snapshot(ui, store);
        }
        Event::BackendRemoved { id } => {
            if let Ok(mut s) = store.lock() {
                if let Some(snap) = s.snapshot.as_mut() {
                    snap.backends.retain(|b| b.id != id);
                }
            }
            apply_snapshot(ui, store);
        }
        Event::RouteTableChanged {
            routes,
            valid,
            error,
        } => {
            if let Ok(mut s) = store.lock() {
                if let Some(snap) = s.snapshot.as_mut() {
                    snap.routes = routes;
                    snap.proxy.table_valid = valid;
                    snap.proxy.table_error = error;
                }
            }
            apply_snapshot(ui, store);
        }
        Event::RigChanged { rig } => {
            if let Ok(mut s) = store.lock() {
                if let Some(snap) = s.snapshot.as_mut() {
                    snap.rig = *rig;
                }
            }
            apply_snapshot(ui, store);
        }
        Event::RequestStarted { id, alias, backend } => {
            let row = RequestRow {
                id: id.to_string().into(),
                time: "now".into(),
                alias: alias
                    .map(|a| a.to_string())
                    .unwrap_or_else(|| "—".to_string())
                    .into(),
                backend: backend
                    .map(|b| b.to_string())
                    .unwrap_or_else(|| "—".to_string())
                    .into(),
                model: "".into(),
                status: "…".into(),
                level: 5,
                ttft: "—".into(),
                tps: "—".into(),
                tokens: "—".into(),
                cost: "—".into(),
                attempts: 1,
                reason: "".into(),
                inflight: true,
            };
            push_request(ui, store, row);
        }
        Event::RequestFinished { record } => {
            push_request(ui, store, request_row(&record));
        }
        Event::BootProgress {
            backend,
            phase,
            line,
        } => {
            let (text, level, pct) = boot_text(&phase);
            st.set_boot_active(!phase.is_terminal() || level == 4);
            st.set_boot_backend(backend.to_string().into());
            st.set_boot_phase(text.into());
            st.set_boot_level(level);
            st.set_boot_pct(pct);
            if let Ok(mut s) = store.lock() {
                if s.boot_started.is_none() {
                    s.boot_started = Some(now_unix());
                }
                s.boot_backend = backend.to_string();
                if let Some(l) = line {
                    s.push_log(l);
                }
            }
            render_boot_log(ui, store);
        }
        Event::LogLine { line, .. } => {
            if let Ok(mut s) = store.lock() {
                s.push_log(line);
            }
            render_logs(ui, store);
            render_boot_log(ui, store);
        }
        Event::VastFleetChanged { instances, credit } => {
            if let Ok(mut s) = store.lock() {
                if let Some(snap) = s.snapshot.as_mut() {
                    snap.instances = instances;
                    if let Some(c) = credit {
                        snap.totals.vast_credit = Some(c);
                    }
                }
            }
            apply_snapshot(ui, store);
        }
        Event::UsageTick { window } => {
            let (total, metered) = cost_text(&window.total_cost);
            st.set_usage_total(total.into());
            st.set_usage_metered(metered);
            st.set_usage_tokens(
                format!("{} / {}", window.total_prompt, window.total_completion).into(),
            );
            st.set_usage_requests(window.rows.to_string().into());
        }
        Event::JobChanged { job } => {
            if let Ok(mut s) = store.lock() {
                if let Some(snap) = s.snapshot.as_mut() {
                    match snap.jobs.iter_mut().find(|j| j.id == job.id) {
                        Some(slot) => *slot = (*job).clone(),
                        None => snap.jobs.push((*job).clone()),
                    }
                }
            }
            apply_snapshot(ui, store);
        }
        Event::CheckResult { result } => {
            let rows = check_rows(&[result]);
            let existing: Vec<CheckRow> = st.get_checks().iter().collect();
            let mut merged = existing;
            for r in rows {
                match merged.iter_mut().find(|c| c.id == r.id) {
                    Some(slot) => *slot = r,
                    None => merged.push(r),
                }
            }
            st.set_checks(ModelRc::new(VecModel::from(merged)));
        }
        Event::Alert {
            level,
            message,
            action,
            id,
        } => {
            toast(ui, &message, alert_level(level));
            if let Ok(mut s) = store.lock() {
                if let Some(snap) = s.snapshot.as_mut() {
                    let alert = Alert {
                        id,
                        level,
                        message,
                        action,
                        at_unix: now_unix(),
                    };
                    match snap.alerts.iter_mut().find(|a| a.id == alert.id) {
                        Some(slot) => *slot = alert,
                        None => snap.alerts.push(alert),
                    }
                }
            }
            apply_snapshot(ui, store);
        }
    }
}

/// Add or replace a row in the ticker, newest first.
fn push_request(ui: &AppWindow, store: &Arc<Mutex<Store>>, row: RequestRow) {
    if let Ok(mut s) = store.lock() {
        if let Some(existing) = s.requests.iter_mut().find(|r| r.id == row.id) {
            *existing = row;
        } else {
            s.requests.push_front(row);
            while s.requests.len() > REQUEST_HISTORY {
                s.requests.pop_back();
            }
        }
        let rows: Vec<RequestRow> = s.requests.iter().cloned().collect();
        ui.global::<State>()
            .set_requests(ModelRc::new(VecModel::from(rows)));
    }
}

/// Mirror the log buffer into the boot drawer.
fn render_boot_log(ui: &AppWindow, store: &Arc<Mutex<Store>>) {
    if let Ok(s) = store.lock() {
        let lines: Vec<SharedString> = s.log_buffer.iter().map(SharedString::from).collect();
        ui.global::<State>()
            .set_boot_log(ModelRc::new(VecModel::from(lines)));
        if let Some(started) = s.boot_started {
            ui.global::<State>()
                .set_boot_elapsed(fmt_dur((now_unix() - started).max(0) as f64).into());
        }
    }
}

/// Re-render the ticker's relative timestamps and the boot timer, once a second.
///
/// The web UI does the same with a 60 s interval; a boot elapsed counter wants finer
/// grain than that, and re-rendering two small models is free.
pub fn tick(ui: &AppWindow, store: &Arc<Mutex<Store>>) {
    if let Ok(s) = store.lock() {
        if let Some(started) = s.boot_started {
            ui.global::<State>()
                .set_boot_elapsed(fmt_dur((now_unix() - started).max(0) as f64).into());
        }
    }
}

/// Lenient probe extraction for `POST /v1/smoke`.
///
/// The route streams SSE, and `NodeClient` can only POST-and-decode, so this accepts
/// whatever JSON shape comes back — a bare array, `{"probes": […]}`, or a single probe —
/// and reports honestly when it is none of them.
pub fn probe_rows(value: &serde_json::Value) -> Vec<ProbeRow> {
    let items: Vec<serde_json::Value> = match value {
        serde_json::Value::Array(a) => a.clone(),
        serde_json::Value::Object(o) => match o.get("probes") {
            Some(serde_json::Value::Array(a)) => a.clone(),
            _ => vec![value.clone()],
        },
        _ => Vec::new(),
    };
    items
        .iter()
        .filter_map(|v| serde_json::from_value::<apexrouter_protocol::SmokeProbe>(v.clone()).ok())
        .map(|p| ProbeRow {
            name: p.name.into(),
            ok: p.ok,
            ms: format!("{}", p.ms).into(),
            detail: p.detail.into(),
            ttft: p
                .ttft_ms
                .map(|m| format!("{m} ms"))
                .unwrap_or_else(|| "—".to_string())
                .into(),
            tps: p
                .tok_per_s
                .map(|t| format!("{t:.1}"))
                .unwrap_or_else(|| "—".to_string())
                .into(),
            tokens: p
                .tokens
                .map(|t| t.to_string())
                .unwrap_or_else(|| "—".to_string())
                .into(),
        })
        .collect()
}

/// Lenient check extraction for `GET /v1/checks`, which returns the registry rather than
/// results: a descriptor without a status renders as `skipped` until it is run.
pub fn registry_rows(value: &serde_json::Value) -> Vec<CheckRow> {
    if let Ok(results) = serde_json::from_value::<Vec<CheckResult>>(value.clone()) {
        return check_rows(&results);
    }
    let items = match value {
        serde_json::Value::Array(a) => a.clone(),
        serde_json::Value::Object(o) => match o.get("checks") {
            Some(serde_json::Value::Array(a)) => a.clone(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };
    items
        .iter()
        .map(|v| {
            let id = v
                .get("id")
                .and_then(|x| x.as_str())
                .unwrap_or("?")
                .to_string();
            let label = v
                .get("label")
                .and_then(|x| x.as_str())
                .unwrap_or(&id)
                .to_string();
            CheckRow {
                id: id.into(),
                label: label.into(),
                status: "not run".into(),
                level: 0,
                ms: "—".into(),
                detail: v
                    .get("detail")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .into(),
                fix: v.get("fix").and_then(|x| x.as_str()).unwrap_or("").into(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_protocol::{BackendKind, BackendLimits, Protocol, Provenance};

    fn backend(id: &str, tags: &[&str]) -> Backend {
        Backend {
            id: BackendId::parse(id).expect("id"),
            kind: BackendKind::LocalLlama,
            protocol: Protocol::OpenAi,
            label: id.to_string(),
            base_url: "http://127.0.0.1:8100".into(),
            credential: CredentialSource::None,
            tags: tags.iter().map(|t| t.to_string()).collect(),
            models: vec![],
            limits: BackendLimits::default(),
            price: None,
            health: Health::Ready {
                since_unix: 0,
                slots_busy: 0,
                slots_total: 4,
                tps_p50: Some(9.71),
            },
            provenance: Provenance::Spawned,
            endpoint: None,
            enabled: true,
            devices: vec!["Vulkan0".into()],
            last_error: None,
        }
    }

    #[test]
    fn byte_and_mb_formatting_reads_like_a_human() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(1536), "2 KiB");
        assert_eq!(fmt_bytes(5 * 1024 * 1024 * 1024), "5.00 GiB");
        assert_eq!(fmt_mb(512), "512 MiB");
        assert_eq!(fmt_mb(11397), "11.1 GiB");
    }

    #[test]
    fn durations_carry_one_unit_and_its_neighbour() {
        assert_eq!(fmt_dur(9.4), "9s");
        assert_eq!(fmt_dur(125.0), "2m 5s");
        assert_eq!(fmt_dur(7_500.0), "2h 5m");
        assert_eq!(fmt_dur(200_000.0), "2d 7h");
        assert_eq!(fmt_dur(-5.0), "0s");
    }

    #[test]
    fn cost_carries_its_metered_flag() {
        let metered = CostEstimate::Metered {
            usd: Money::from_usd(1.25),
            source: apexrouter_protocol::PriceSource::ProviderApi,
        };
        assert_eq!(cost_text(&metered), ("$1.25".to_string(), true));
        let guess = CostEstimate::Approximate {
            usd: Money::from_usd(1.25),
            source: apexrouter_protocol::PriceSource::Derived,
            assumption: "50/50 mix".into(),
        };
        assert_eq!(cost_text(&guess), ("$1.25".to_string(), false));
        assert_eq!(cost_text(&CostEstimate::Unknown), ("—".to_string(), false));
    }

    #[test]
    fn token_counts_mark_an_estimate() {
        assert_eq!(token_text(Some(&TokenCount::Reported(120))), "120");
        assert_eq!(token_text(Some(&TokenCount::Estimated(120))), "~120");
        assert_eq!(token_text(None), "—");
    }

    #[test]
    fn health_maps_to_the_row_levels() {
        assert_eq!(health_text(&Health::Unknown).1, 0);
        assert_eq!(
            health_text(&Health::Ready {
                since_unix: 0,
                slots_busy: 0,
                slots_total: 1,
                tps_p50: None
            })
            .1,
            1
        );
        assert_eq!(
            health_text(&Health::Degraded {
                reason: "x".into(),
                consecutive_failures: 3
            })
            .1,
            3
        );
        assert_eq!(
            health_text(&Health::Down {
                reason: "x".into(),
                retry_at_unix: 0
            })
            .1,
            4
        );
    }

    #[test]
    fn selectors_round_trip_through_the_editor_field() {
        for wire in ["id:local-carnice", "tag:cheap", "glob:vast-*"] {
            let parsed = parse_selector(wire).expect("parse");
            assert_eq!(selector_text(&parsed), wire);
        }
        // A bare id, and a bare glob, are both what an operator means when they type them.
        assert_eq!(
            selector_text(&parse_selector("local-carnice").expect("id")),
            "id:local-carnice"
        );
        assert_eq!(
            selector_text(&parse_selector("vast-*").expect("glob")),
            "glob:vast-*"
        );
        assert!(parse_selector("").is_err());
        assert!(parse_selector("id:NOT A SLUG").is_err());
    }

    #[test]
    fn globs_match_the_way_the_daemon_compiles_them() {
        assert!(glob_matches("vast-*", "vast-1234"));
        assert!(glob_matches("*", "anything"));
        assert!(glob_matches("local-?arnice", "local-carnice"));
        assert!(!glob_matches("vast-*", "local-carnice"));
        assert!(glob_matches("a*b*c", "azzbzzc"));
        assert!(!glob_matches("a*b*c", "azzbzz"));
    }

    #[test]
    fn selector_matching_covers_id_tag_and_glob() {
        let b = backend("local-carnice", &["cheap", "local"]);
        assert!(selector_matches(
            &parse_selector("id:local-carnice").expect("sel"),
            &b
        ));
        assert!(selector_matches(
            &parse_selector("tag:cheap").expect("sel"),
            &b
        ));
        assert!(selector_matches(
            &parse_selector("glob:local-*").expect("sel"),
            &b
        ));
        assert!(!selector_matches(
            &parse_selector("tag:expensive").expect("sel"),
            &b
        ));
    }

    #[test]
    fn strategy_indices_round_trip() {
        for s in [
            Strategy::FirstHealthy,
            Strategy::RoundRobin,
            Strategy::LeastBusy,
            Strategy::Cheapest,
        ] {
            assert_eq!(strategy_from_index(strategy_index(s)), s);
        }
        // Out of range is the safe default, not a panic.
        assert_eq!(strategy_from_index(99), Strategy::FirstHealthy);
    }

    #[test]
    fn a_credential_source_never_leaks_a_value() {
        let rendered = credential_source(&CredentialSource::Env {
            var: "TOGETHER_API_KEY".into(),
        });
        assert_eq!(rendered, "env TOGETHER_API_KEY");
        assert!(!rendered.contains("sk-"));
        assert_eq!(credential_source(&CredentialSource::None), "none");
    }

    #[test]
    fn query_encoding_keeps_a_path_but_escapes_a_space() {
        assert_eq!(
            q("/home/andre/models/a b.gguf"),
            "/home/andre/models/a%20b.gguf"
        );
        assert_eq!(q("Q4_K_M"), "Q4_K_M");
        assert_eq!(q("a&b=c"), "a%26b%3Dc");
    }

    #[test]
    fn list_and_option_fields_distinguish_empty_from_unset() {
        assert_eq!(split_list(" a, b ,, c "), vec!["a", "b", "c"]);
        assert!(split_list("  ").is_empty());
        assert_eq!(opt("  x "), Some("x".to_string()));
        assert_eq!(opt("   "), None);
    }

    #[test]
    fn a_gtt_overcommitting_gpu_never_shows_a_negative_bar() {
        // ROCm on this box reports free > total. `total - free` would underflow into a
        // lie; the row shows a 0.0 fraction and says why instead.
        let rig = RigSnapshot {
            gpus: vec![apexrouter_protocol::Gpu {
                device: "ROCm0".into(),
                index: 0,
                name: "gfx1100".into(),
                backend: apexrouter_protocol::GpuBackend::Rocm,
                vram_total_mb: 11397,
                vram_free_mb: 20000,
                pci_bus_id: None,
                driver: None,
                is_software: false,
                seen_by_builds: vec![],
                held_by: vec![],
                reserved_mb: 0,
            }],
            ..Default::default()
        };
        let rows = device_rows(&rig, &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].frac, 0.0);
        assert!(rows[0].detail.as_str().contains("free > total"));
    }

    #[test]
    fn one_physical_gpu_seen_by_two_backends_is_two_rows_and_one_budget() {
        // The rig strip shows a row per *view*, because that is what a launch plan picks.
        // The budget itself is the daemon's, computed per backend, and is never summed
        // across views here.
        let gpu =
            |backend: apexrouter_protocol::GpuBackend, device: &str| apexrouter_protocol::Gpu {
                device: device.into(),
                index: 0,
                name: "Radeon 840M".into(),
                backend,
                vram_total_mb: 8192,
                vram_free_mb: 6000,
                pci_bus_id: Some("0000:64:00.0".into()),
                driver: None,
                is_software: false,
                seen_by_builds: vec![],
                held_by: vec![],
                reserved_mb: 0,
            };
        let rig = RigSnapshot {
            gpus: vec![
                gpu(apexrouter_protocol::GpuBackend::Vulkan, "Vulkan0"),
                gpu(apexrouter_protocol::GpuBackend::Rocm, "ROCm0"),
            ],
            ..Default::default()
        };
        assert_eq!(device_rows(&rig, &[]).len(), 2);
        assert_eq!(rig.physical_devices().len(), 1);

        // And the budget line reports what the daemon sent, not a sum of both views.
        let line = budget_line(
            &[DeviceBudget {
                device: "Vulkan0".into(),
                free_mb: 6000,
                reserved_mb: 0,
            }],
            512,
        );
        assert!(line.contains("1 device(s)"), "{line}");
    }

    #[test]
    fn route_rows_flag_an_alias_whose_targets_resolve_to_nothing() {
        let route = ModelRoute {
            alias: apexrouter_protocol::Alias::parse("auto").expect("alias"),
            targets: vec![apexrouter_protocol::RouteTarget {
                backend: BackendSelector::Id(BackendId::parse("missing").expect("id")),
                model: None,
                weight: 1,
            }],
            strategy: Strategy::FirstHealthy,
            filter: Default::default(),
            retry: Default::default(),
            is_default: true,
            description: None,
        };
        let rows = route_rows(&[route], &[backend("local-carnice", &[])]);
        assert_eq!(rows[0].level, 4);
        assert!(rows[0].health.as_str().contains("no target resolves"));
    }

    #[test]
    fn hf_files_group_by_quant_and_sum_their_shards() {
        let f = |name: &str, quant: &str, size: u64| HfFile {
            rfilename: name.into(),
            size: Some(size),
            quant: Some(quant.into()),
            is_mmproj: false,
            shard_of: Some((1, 2)),
        };
        let groups = group_hf_files(&[
            f("m-00001-of-00002.gguf", "UD-Q4_K_XL", 1000),
            f("m-00002-of-00002.gguf", "UD-Q4_K_XL", 2000),
            f("m-Q8_0.gguf", "Q8_0", 9000),
        ]);
        assert_eq!(groups.len(), 2);
        let xl = groups
            .iter()
            .find(|g| g.label == "UD-Q4_K_XL")
            .expect("group");
        assert_eq!(xl.total_bytes, 3000);
        assert_eq!(xl.files.len(), 2);
    }

    #[test]
    fn the_smoke_extractor_accepts_every_shape_and_refuses_nonsense() {
        let one = serde_json::json!({
            "name": "throughput", "ok": true, "ms": 12, "detail": "ok",
            "ttft_ms": 40, "tok_per_s": 9.71, "tokens": 200
        });
        assert_eq!(probe_rows(&one).len(), 1);
        assert_eq!(probe_rows(&serde_json::json!([one.clone()])).len(), 1);
        assert_eq!(
            probe_rows(&serde_json::json!({ "probes": [one.clone(), one] })).len(),
            2
        );
        assert!(probe_rows(&serde_json::json!("not a probe")).is_empty());
    }

    #[test]
    fn the_check_registry_reader_handles_results_and_descriptors_alike() {
        let results = serde_json::json!([{
            "id": "creds.vast", "label": "vast api key", "status": "pass",
            "ms": 1, "detail": "found"
        }]);
        let rows = registry_rows(&results);
        assert_eq!(rows[0].status.as_str(), "pass");
        assert_eq!(rows[0].level, 1);

        let registry = serde_json::json!([{ "id": "creds.vast", "label": "vast api key" }]);
        let rows = registry_rows(&registry);
        assert_eq!(rows[0].status.as_str(), "not run");
        assert_eq!(rows[0].level, 0);
    }

    #[test]
    fn boot_phases_are_ordered_and_terminal_states_are_full() {
        assert!(boot_text(&BootPhase::Reserved).2 < boot_text(&BootPhase::Pulling).2);
        assert!(boot_text(&BootPhase::Pulling).2 < boot_text(&BootPhase::Loading { pct: None }).2);
        assert_eq!(boot_text(&BootPhase::Healthy).2, 1.0);
        assert_eq!(boot_text(&BootPhase::Healthy).1, 1);
        assert_eq!(
            boot_text(&BootPhase::Failed {
                reason: "oom".into()
            })
            .1,
            4
        );
    }

    #[test]
    fn the_control_url_is_a_url_and_never_a_bare_bind() {
        // Whatever it resolves from — env var, config file or the constant — the app hands
        // this to `NodeClient`, so it must always carry a scheme.
        let url = control_url();
        assert!(
            url.starts_with("http://") || url.starts_with("https://"),
            "{url}"
        );
    }

    #[test]
    fn moving_the_control_port_in_config_moves_this_client_with_it() {
        // D10: the GUI used to read `$APEXROUTER_URL` and nothing else, so an operator who
        // moved the port in config.toml got a silent "not connected" against 2739.
        let doc = "\
[server]\n\
proxy_bind = \"127.0.0.1:8888\"\n\
control_bind = \"127.0.0.1:3000\"   # moved\n\
token_env = \"APEXROUTER_TOKEN\"\n";
        assert_eq!(control_bind_in(doc).as_deref(), Some("127.0.0.1:3000"));
    }

    #[test]
    fn control_bind_is_only_read_from_the_server_table() {
        // A `control_bind` under another table is not the one the daemon binds.
        let doc = "\
[router]\n\
control_bind = \"127.0.0.1:9999\"\n\
[server]\n\
control_bind = \"127.0.0.1:2739\"\n";
        assert_eq!(control_bind_in(doc).as_deref(), Some("127.0.0.1:2739"));
        assert_eq!(
            control_bind_in("[server.tls]\ncontrol_bind = \"x:1\"\n"),
            None
        );
        // Commented out is unset, not a value.
        assert_eq!(
            control_bind_in("[server]\n# control_bind = \"1.2.3.4:1\"\n"),
            None
        );
        // A document without the key at all leaves the default in place.
        assert_eq!(control_bind_in("[server]\nautostart = true\n"), None);
    }

    #[test]
    fn a_wildcard_bind_is_dialled_on_loopback() {
        // `0.0.0.0` is a listener's answer to "which interfaces"; it is not connectable.
        assert_eq!(dialable("0.0.0.0:2739"), "127.0.0.1:2739");
        assert_eq!(dialable("[::]:2739"), "[::1]:2739");
        // A deliberate LAN bind is left exactly as written.
        assert_eq!(dialable("192.168.1.9:2739"), "192.168.1.9:2739");
        assert_eq!(dialable(" 127.0.0.1:2739 "), "127.0.0.1:2739");
    }

    #[test]
    fn the_log_buffer_filters_without_losing_lines() {
        let mut store = Store::default();
        store.push_log("main: server is listening".into());
        store.push_log("load_tensors: offloaded 43/43".into());
        let (shown, hidden) = store.log_view("tensors");
        assert_eq!(shown.len(), 1);
        assert_eq!(hidden, 1);
        // Clearing the filter must not have lost the other line.
        let (all, none) = store.log_view("");
        assert_eq!(all.len(), 2);
        assert_eq!(none, 0);
    }
}
