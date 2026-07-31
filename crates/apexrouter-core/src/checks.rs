//! OWNER: unit C-17 (core/checks.rs). Do not edit outside that unit.
//!
//! One check registry, backing `doctor`, `diagnose` and the four native smoke probes.
//!
//! Checks run **concurrently** with per-check timeouts and stream as each lands, so
//! `diagnose --only rate-limits` is instant instead of waiting through four sequential SSH
//! probes. A check that panics yields `CheckStatus::Fail`; it never poisons the run.
//!
//! [`CheckCtx::ext`] is how `apexrouter_providers::checks` injects its clients without
//! `core` ever depending on `providers`.
//!
//! Three rules the runner enforces so no individual check has to:
//!
//! * **The runner times every check** and overwrites [`CheckResult::ms`]. A check that
//!   forgets to time itself still reports honestly.
//! * **The runner owns the id.** The `CheckResult` a check returns is stamped with
//!   `Check::id()`, so a copy-paste mistake inside a check cannot make `--only` filter one
//!   thing and report another.
//! * **A precondition a check cannot meet is `Skipped`, not `Fail`.** Being offline, having
//!   no daemon and having rented nothing are normal states, not defects.

use crate::config::Config;
use crate::discover;
use crate::paths::Paths;
use crate::secret;
use apexrouter_protocol::{
    CheckId, CheckResult, CheckStatus, CredentialSource, InstanceId, ProviderId, RigSnapshot,
};
use async_trait::async_trait;
use futures_util::stream::{FuturesUnordered, StreamExt};
use futures_util::FutureExt;
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// One diagnostic.
#[async_trait]
pub trait Check: Send + Sync {
    /// `"creds.vast"`, `"ports.8888"`, `"builds.vulkan"`, …
    fn id(&self) -> CheckId;
    /// The row label.
    fn label(&self) -> &str;
    /// What this check needs in order to mean anything.
    fn needs(&self) -> CheckNeeds;
    /// Run it. Must not panic; must respect the runner's timeout.
    async fn run(&self, ctx: &CheckCtx) -> CheckResult;
}

/// What a check requires to be meaningful. Anything unavailable yields `Skipped`, not
/// `Fail` — being offline is not a defect.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CheckNeeds {
    /// Filesystem and `/proc` only.
    Local,
    /// The internet.
    Network,
    /// A running daemon.
    Daemon,
    /// A specific rented instance.
    Instance,
}

/// Everything a check may read.
pub struct CheckCtx {
    /// Resolved paths.
    pub paths: Paths,
    /// The live config.
    pub cfg: Arc<Config>,
    /// One pooled HTTP client, shared.
    pub http: reqwest::Client,
    /// The last rig scan, when there is one.
    pub rig: Option<Arc<RigSnapshot>>,
    /// Where the control plane is, when a daemon is running.
    pub proxy_url: Option<String>,
    /// Which instance an `Instance` check is about.
    pub instance: Option<InstanceId>,
    /// Provider-supplied clients, injected by `apexrouter-server` at startup.
    pub ext: HashMap<String, Arc<dyn std::any::Any + Send + Sync>>,
}

/// The registered checks.
#[derive(Default)]
pub struct Registry {
    /* C-17 */
    checks: Vec<Arc<dyn Check>>,
}

impl Registry {
    /// An empty registry.
    pub fn new() -> Registry {
        Registry { checks: Vec::new() }
    }

    /// Add one check.
    pub fn register(&mut self, c: Arc<dyn Check>) {
        self.checks.push(c);
    }

    /// Every registered id, for `GET /v1/checks`.
    pub fn ids(&self) -> Vec<CheckId> {
        self.checks.iter().map(|c| c.id()).collect()
    }

    /// Run every check (or one, when `only` is given) **concurrently**, streaming each
    /// result through `tx` as it lands and also returning the whole set.
    ///
    /// The stream is in **completion** order — that is the point of streaming — while the
    /// returned `Vec` is in **registration** order, so `doctor --json` is stable between
    /// runs on the same machine.
    ///
    /// `only` matches loosely, because operators type what they see: an exact id
    /// (`together.ratelimits`), a namespace (`creds` matches `creds.vast` and `creds.hf`)
    /// or any fragment, with `.`, `-` and `_` ignored on both sides so `--only rate-limits`
    /// finds `together.ratelimits`. An empty `only` selects everything.
    ///
    /// Sending is per-check, so a slow receiver applies backpressure to that check's
    /// delivery rather than serialising the run. A closed receiver is not an error: `doctor`
    /// with no SSE client attached still returns its `Vec`.
    pub async fn run(
        &self,
        ctx: &CheckCtx,
        only: Option<&str>,
        tx: tokio::sync::mpsc::Sender<CheckResult>,
    ) -> Vec<CheckResult> {
        let selected: Vec<&Arc<dyn Check>> = self
            .checks
            .iter()
            .filter(|c| matches_only(c.id().as_str(), only))
            .collect();

        let mut slots: Vec<Option<CheckResult>> = (0..selected.len()).map(|_| None).collect();

        let mut running = FuturesUnordered::new();
        for (slot, check) in selected.into_iter().enumerate() {
            let tx = tx.clone();
            running.push(async move {
                let result = run_one(check.as_ref(), ctx).await;
                // A gone receiver is normal: `doctor` does not attach one.
                let _ = tx.send(result.clone()).await;
                (slot, result)
            });
        }

        while let Some((slot, result)) = running.next().await {
            slots[slot] = Some(result);
        }
        slots.into_iter().flatten().collect()
    }
}

/// Run one check under its timeout, catching a panic and stamping id, label and duration.
async fn run_one(check: &dyn Check, ctx: &CheckCtx) -> CheckResult {
    let id = check.id();
    let label = check.label().to_owned();
    let needs = check.needs();

    if let Some(why) = unmet(needs, ctx) {
        return CheckResult {
            id,
            label,
            status: CheckStatus::Skipped,
            ms: 0,
            detail: why.to_owned(),
            fix: None,
        };
    }

    let budget = timeout_for(needs);
    let started = Instant::now();
    // `AssertUnwindSafe` is honest here: on a panic we discard everything the check
    // touched and report `Fail`, so no observer can see a half-mutated value.
    let guarded = AssertUnwindSafe(check.run(ctx)).catch_unwind();

    let mut result = match tokio::time::timeout(budget, guarded).await {
        Ok(Ok(r)) => r,
        Ok(Err(panic)) => CheckResult {
            id: id.clone(),
            label: label.clone(),
            status: CheckStatus::Fail,
            ms: 0,
            detail: format!("the check panicked: {}", panic_message(panic.as_ref())),
            fix: Some(format!(
                "this is a bug in the `{id}` check — re-run with `RUST_BACKTRACE=1` and report it"
            )),
        },
        Err(_) => CheckResult {
            id: id.clone(),
            label: label.clone(),
            status: CheckStatus::Fail,
            ms: 0,
            detail: format!("timed out after {} ms", budget.as_millis()),
            fix: Some(format!(
                "re-run just this one: `apexrouter doctor --only {id}`"
            )),
        },
    };

    result.id = id;
    if result.label.is_empty() {
        result.label = label;
    }
    result.ms = millis(started.elapsed());
    result
}

/// The per-check wall-clock budget. Local work is quick or broken; an SSH probe into a
/// rented box is neither.
fn timeout_for(needs: CheckNeeds) -> Duration {
    match needs {
        CheckNeeds::Local => Duration::from_secs(10),
        CheckNeeds::Network => Duration::from_secs(20),
        CheckNeeds::Daemon => Duration::from_secs(10),
        CheckNeeds::Instance => Duration::from_secs(60),
    }
}

/// Why this check cannot run here, when it cannot. `None` means "go ahead".
///
/// Being offline is not detectable up front, so `Network` checks always run and report
/// their own transport failure.
fn unmet(needs: CheckNeeds, ctx: &CheckCtx) -> Option<&'static str> {
    match needs {
        CheckNeeds::Local | CheckNeeds::Network => None,
        CheckNeeds::Daemon => ctx
            .proxy_url
            .is_none()
            .then_some("no daemon is running — start one with `apexrouter serve`"),
        CheckNeeds::Instance => ctx
            .instance
            .is_none()
            .then_some("no instance selected — pass one, or rent one first"),
    }
}

/// Read whatever a panic payload will give us.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

/// Saturating millisecond count; a check cannot run for 49 days.
fn millis(d: Duration) -> u32 {
    u32::try_from(d.as_millis()).unwrap_or(u32::MAX)
}

/// `--only` matching: exact, namespace prefix or fragment, with separators ignored.
fn matches_only(id: &str, only: Option<&str>) -> bool {
    let Some(pattern) = only else {
        return true;
    };
    let pattern = normalise(pattern);
    if pattern.is_empty() {
        return true;
    }
    normalise(id).contains(&pattern)
}

/// Lower-case, with `.`, `-`, `_` and whitespace removed.
fn normalise(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// The checks that need nothing but this machine: `creds.*`, `ports.*`, `builds.*`,
/// `devices.*`, `models.*`, `state.writable`, `legacy.migration`.
///
/// Registration order is display order: credentials, then listeners, then the rig, then
/// state, then the legacy install.
pub fn local_checks() -> Vec<Arc<dyn Check>> {
    vec![
        Arc::new(CredsCheck {
            id: CheckId::from("creds.vast"),
            label: "vast.ai credential".to_owned(),
            which: Which::Vast,
        }),
        Arc::new(CredsCheck {
            id: CheckId::from("creds.hf"),
            label: "HuggingFace token".to_owned(),
            which: Which::Hf,
        }),
        Arc::new(CredsCheck {
            id: CheckId::from("creds.together"),
            label: "together.ai credential".to_owned(),
            which: Which::Provider("together"),
        }),
        Arc::new(PortCheck {
            id: CheckId::from("ports.proxy"),
            label: "proxy port is free or ours".to_owned(),
            control: false,
        }),
        Arc::new(PortCheck {
            id: CheckId::from("ports.control"),
            label: "control port is free or ours".to_owned(),
            control: true,
        }),
        Arc::new(BuildsDiscovered),
        Arc::new(BuildsFlags),
        Arc::new(DevicesEnumerated),
        Arc::new(ModelsDiscovered),
        Arc::new(StateWritable),
        Arc::new(LegacyMigration),
    ]
}

// ---------------------------------------------------------------------------------------
// helpers shared by the local checks
// ---------------------------------------------------------------------------------------

/// Build a result. `ms` is filled in by the runner, which is the only honest timer.
fn result(
    id: &CheckId,
    label: &str,
    status: CheckStatus,
    detail: impl Into<String>,
    fix: Option<&str>,
) -> CheckResult {
    CheckResult {
        id: id.clone(),
        label: label.to_owned(),
        status,
        ms: 0,
        detail: detail.into(),
        fix: fix.map(str::to_owned),
    }
}

/// Describe where a credential came from, without ever touching the credential.
fn describe_source(source: &CredentialSource) -> String {
    match source {
        CredentialSource::None => "no credential needed".to_owned(),
        CredentialSource::Env { var } => format!("from ${var}"),
        CredentialSource::File { path } => format!("from {path}"),
        CredentialSource::Managed { store } => format!("from {store}"),
        CredentialSource::Instance => "minted per instance".to_owned(),
    }
}

// ---------------------------------------------------------------------------------------
// creds.*
// ---------------------------------------------------------------------------------------

/// Which credential chain a [`CredsCheck`] walks.
enum Which {
    /// `secret::resolve_vast`.
    Vast,
    /// `secret::resolve_hf`.
    Hf,
    /// `secret::resolve_provider`, by managed-provider id.
    Provider(&'static str),
}

/// Walks one credential chain and reports **where** the key came from, never the key.
struct CredsCheck {
    id: CheckId,
    label: String,
    which: Which,
}

#[async_trait]
impl Check for CredsCheck {
    fn id(&self) -> CheckId {
        self.id.clone()
    }
    fn label(&self) -> &str {
        &self.label
    }
    fn needs(&self) -> CheckNeeds {
        CheckNeeds::Local
    }
    async fn run(&self, ctx: &CheckCtx) -> CheckResult {
        let found = match &self.which {
            Which::Vast => secret::resolve_vast(&ctx.cfg, &ctx.paths),
            Which::Hf => secret::resolve_hf(&ctx.cfg, &ctx.paths),
            Which::Provider(id) => match ProviderId::parse(id) {
                Ok(pid) => secret::resolve_provider(&ctx.cfg, &ctx.paths, &pid),
                Err(e) => {
                    return result(
                        &self.id,
                        &self.label,
                        CheckStatus::Fail,
                        format!("`{id}` is not a valid provider id: {e}"),
                        None,
                    )
                }
            },
        };

        match found {
            Ok(Some(c)) => result(
                &self.id,
                &self.label,
                CheckStatus::Pass,
                format!("present, {}", describe_source(&c.source)),
                None,
            ),
            // No key is a normal state for a provider nobody uses, so: Skipped, with the
            // one command that fixes it if it was not intentional.
            // The verb is `provider set`, and there is no `provider key`: the fix line this
            // replaces named a subcommand `apexrouter` does not have, so following it
            // printed a clap usage error rather than storing a key.
            Ok(None) => result(
                &self.id,
                &self.label,
                CheckStatus::Skipped,
                "no credential configured",
                Some(&format!(
                    "`apexrouter provider set {} --key-stdin`, or an env var, or a key file",
                    self.id.as_str().rsplit('.').next().unwrap_or("<id>")
                )),
            ),
            // A *present but unreadable* file is a real defect, and that is what an error
            // out of the chain means.
            Err(e) => result(
                &self.id,
                &self.label,
                CheckStatus::Fail,
                format!("credential chain failed: {e}"),
                Some("check the permissions on the key file our config names"),
            ),
        }
    }
}

// ---------------------------------------------------------------------------------------
// ports.*
// ---------------------------------------------------------------------------------------

/// Can we bind the listener we are configured to bind?
struct PortCheck {
    id: CheckId,
    label: String,
    /// `true` for the control plane, `false` for the data plane.
    control: bool,
}

#[async_trait]
impl Check for PortCheck {
    fn id(&self) -> CheckId {
        self.id.clone()
    }
    fn label(&self) -> &str {
        &self.label
    }
    fn needs(&self) -> CheckNeeds {
        CheckNeeds::Local
    }
    async fn run(&self, ctx: &CheckCtx) -> CheckResult {
        let addr = if self.control {
            ctx.cfg.control_bind()
        } else {
            ctx.cfg.proxy_bind()
        };
        match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => {
                drop(l);
                result(
                    &self.id,
                    &self.label,
                    CheckStatus::Pass,
                    format!("{addr} is free"),
                    None,
                )
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => result(
                &self.id,
                &self.label,
                // Warn, not Fail: the usual holder is our own daemon, which is the
                // desired state. Identifying the holder needs `/proc`, which is
                // `apexrouter status`'s job, not a check's.
                CheckStatus::Warn,
                format!("{addr} is already bound — by our daemon, or by something else"),
                Some(
                    "`apexrouter status` says whether it is ours; otherwise stop endpoint_proxy.py",
                ),
            ),
            Err(e) => result(
                &self.id,
                &self.label,
                CheckStatus::Fail,
                format!("cannot bind {addr}: {e}"),
                Some("fix the `[server]` bind address in config.toml"),
            ),
        }
    }
}

// ---------------------------------------------------------------------------------------
// builds.* and devices.*
// ---------------------------------------------------------------------------------------

/// `ctx.rig`, or the reason there is nothing to look at.
///
/// Both surfaces that run this registry now put a [`RigSnapshot`] in the context — the
/// daemon from its cache, `apexrouter doctor` by scanning — so reaching this arm means the
/// **scan itself failed**, not that nobody has run one. The old fix line said "run
/// `apexrouter rig` first", which was advice that could not work: nothing was injecting the
/// snapshot, so running it changed nothing. What `apexrouter rig` is actually good for here
/// is showing the scan's own error.
macro_rules! rig_or_skip {
    ($self:expr, $ctx:expr) => {
        match $ctx.rig.as_ref() {
            Some(r) => r,
            None => {
                return result(
                    &$self.id(),
                    $self.label(),
                    CheckStatus::Skipped,
                    "the rig could not be scanned",
                    Some(
                        "`apexrouter rig` runs the same scan and prints where it looked; \
                         `[endpoints] build_roots` is what it searched",
                    ),
                )
            }
        }
    };
}

/// Did we find any `llama-server` at all?
struct BuildsDiscovered;

#[async_trait]
impl Check for BuildsDiscovered {
    fn id(&self) -> CheckId {
        CheckId::from("builds.discovered")
    }
    fn label(&self) -> &str {
        "llama.cpp builds discovered"
    }
    fn needs(&self) -> CheckNeeds {
        CheckNeeds::Local
    }
    async fn run(&self, ctx: &CheckCtx) -> CheckResult {
        let rig = rig_or_skip!(self, ctx);
        if rig.builds.is_empty() {
            return result(
                &self.id(),
                self.label(),
                CheckStatus::Warn,
                "no llama-server binary found",
                Some("add the directory holding `build*/bin/llama-server` to `[endpoints] build_roots`"),
            );
        }
        let names: Vec<&str> = rig.builds.iter().map(|b| b.id.as_str()).collect();
        result(
            &self.id(),
            self.label(),
            CheckStatus::Pass,
            format!("{} build(s): {}", rig.builds.len(), names.join(", ")),
            None,
        )
    }
}

/// Did the `--help` probe actually read anything out of each binary?
struct BuildsFlags;

#[async_trait]
impl Check for BuildsFlags {
    fn id(&self) -> CheckId {
        CheckId::from("builds.flags")
    }
    fn label(&self) -> &str {
        "flag support probed per binary"
    }
    fn needs(&self) -> CheckNeeds {
        CheckNeeds::Local
    }
    async fn run(&self, ctx: &CheckCtx) -> CheckResult {
        let rig = rig_or_skip!(self, ctx);
        if rig.builds.is_empty() {
            return result(
                &self.id(),
                self.label(),
                CheckStatus::Skipped,
                "no builds to probe",
                None,
            );
        }
        let blind: Vec<&str> = rig
            .builds
            .iter()
            .filter(|b| b.flags.flags.is_empty() || b.flags.help_lines == 0)
            .map(|b| b.id.as_str())
            .collect();
        if blind.is_empty() {
            return result(
                &self.id(),
                self.label(),
                CheckStatus::Pass,
                format!("all {} build(s) advertised their flags", rig.builds.len()),
                None,
            );
        }
        result(
            &self.id(),
            self.label(),
            // The argv builder emits nothing it has not seen, so an unprobed binary is
            // usable but under-configured. That is a Warn.
            CheckStatus::Warn,
            format!("no flags read from: {}", blind.join(", ")),
            Some("run `<build>/bin/llama-server --help` by hand — a RUNPATH problem shows up here"),
        )
    }
}

/// Did any build enumerate a real device?
struct DevicesEnumerated;

#[async_trait]
impl Check for DevicesEnumerated {
    fn id(&self) -> CheckId {
        CheckId::from("devices.enumerated")
    }
    fn label(&self) -> &str {
        "compute devices enumerated"
    }
    fn needs(&self) -> CheckNeeds {
        CheckNeeds::Local
    }
    async fn run(&self, ctx: &CheckCtx) -> CheckResult {
        let rig = rig_or_skip!(self, ctx);
        let real = rig.gpus.iter().filter(|g| !g.is_software).count();
        let software = rig.gpus.len() - real;
        if real > 0 {
            return result(
                &self.id(),
                self.label(),
                CheckStatus::Pass,
                format!("{real} device(s), {software} software rasteriser(s) ignored"),
                None,
            );
        }
        result(
            &self.id(),
            self.label(),
            // CPU-only is slow, not broken.
            CheckStatus::Warn,
            if software > 0 {
                format!("only software rasterisers ({software}) — this would run on llvmpipe")
            } else {
                "no compute device enumerated — CPU only".to_owned()
            },
            Some("check the driver, then `<build>/bin/llama-server --list-devices`"),
        )
    }
}

// ---------------------------------------------------------------------------------------
// models.*
// ---------------------------------------------------------------------------------------

/// Are there weights on disk under `[endpoints] model_roots`?
struct ModelsDiscovered;

#[async_trait]
impl Check for ModelsDiscovered {
    fn id(&self) -> CheckId {
        CheckId::from("models.discovered")
    }
    fn label(&self) -> &str {
        "local weights discovered"
    }
    fn needs(&self) -> CheckNeeds {
        CheckNeeds::Local
    }
    async fn run(&self, ctx: &CheckCtx) -> CheckResult {
        match discover::discover_models(&ctx.cfg.endpoints).await {
            Ok(models) if models.is_empty() => result(
                &self.id(),
                self.label(),
                // An empty models directory is normal on a machine that only proxies
                // managed providers.
                CheckStatus::Warn,
                format!(
                    "no GGUF found under: {}",
                    ctx.cfg.endpoints.model_roots.join(", ")
                ),
                Some("add a directory to `[endpoints] model_roots`, or pull one with `apexrouter hf get`"),
            ),
            Ok(models) => {
                let bytes: u64 = models.iter().map(|m| m.total_bytes).sum();
                result(
                    &self.id(),
                    self.label(),
                    CheckStatus::Pass,
                    format!("{} model(s), {} GiB on disk", models.len(), gib(bytes)),
                    None,
                )
            }
            Err(e) => result(
                &self.id(),
                self.label(),
                CheckStatus::Fail,
                format!("discovery failed: {e}"),
                Some("check that every `[endpoints] model_roots` entry is readable"),
            ),
        }
    }
}

/// Bytes as GiB with one decimal, for a human-readable row.
fn gib(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

// ---------------------------------------------------------------------------------------
// state.writable
// ---------------------------------------------------------------------------------------

/// Can we actually write the state directory? Everything durable depends on it.
struct StateWritable;

#[async_trait]
impl Check for StateWritable {
    fn id(&self) -> CheckId {
        CheckId::from("state.writable")
    }
    fn label(&self) -> &str {
        "state directory is writable"
    }
    fn needs(&self) -> CheckNeeds {
        CheckNeeds::Local
    }
    async fn run(&self, ctx: &CheckCtx) -> CheckResult {
        let dir = ctx.paths.state().to_path_buf();
        // Blocking file I/O on a runtime thread, deliberately: it is three syscalls
        // against a local directory, and moving it to a blocking pool would cost more
        // than it saves.
        match probe_writable(&dir) {
            Ok(()) => result(
                &self.id(),
                self.label(),
                CheckStatus::Pass,
                format!("{} is writable", dir.display()),
                None,
            ),
            Err(e) => result(
                &self.id(),
                self.label(),
                CheckStatus::Fail,
                format!("{} is not writable: {e}", dir.display()),
                Some("fix the permissions, or point `$APEXROUTER_HOME` somewhere writable"),
            ),
        }
    }
}

/// Create the directory if need be, write a probe file, remove it again.
///
/// The probe name carries the pid so two daemons probing at once cannot collide.
fn probe_writable(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let probe = dir.join(format!(".write-probe-{}", std::process::id()));
    std::fs::write(&probe, b"apexrouter")?;
    std::fs::remove_file(&probe)
}

// ---------------------------------------------------------------------------------------
// legacy.migration
// ---------------------------------------------------------------------------------------

/// Is there a previous-generation install sitting there un-imported?
struct LegacyMigration;

#[async_trait]
impl Check for LegacyMigration {
    fn id(&self) -> CheckId {
        CheckId::from("legacy.migration")
    }
    fn label(&self) -> &str {
        "legacy LocalRouter state"
    }
    fn needs(&self) -> CheckNeeds {
        CheckNeeds::Local
    }
    async fn run(&self, ctx: &CheckCtx) -> CheckResult {
        let legacy = ctx.paths.legacy();
        let mut found: Vec<String> = Vec::new();
        if legacy.vastai_gguf.is_dir() {
            found.push(legacy.vastai_gguf.display().to_string());
        }
        if let Some(dir) = legacy.localrouter_dir.as_ref() {
            found.push(dir.display().to_string());
        }

        if found.is_empty() {
            return result(
                &self.id(),
                self.label(),
                CheckStatus::Pass,
                "nothing to migrate",
                None,
            );
        }
        // Whether it was *already* imported is `migrate plan`'s answer, not a check's:
        // saying so here would mean re-deriving the plan on every `doctor`.
        result(
            &self.id(),
            self.label(),
            CheckStatus::Warn,
            format!("legacy state present: {}", found.join(", ")),
            Some("see what it would do: `apexrouter migrate --dry-run`"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::mpsc;

    // ---- test doubles ------------------------------------------------------------------

    /// A check that sleeps, then passes.
    struct Slow {
        id: &'static str,
        ms: u64,
        ran: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Check for Slow {
        fn id(&self) -> CheckId {
            CheckId::from(self.id)
        }
        fn label(&self) -> &str {
            "slow"
        }
        fn needs(&self) -> CheckNeeds {
            CheckNeeds::Local
        }
        async fn run(&self, _ctx: &CheckCtx) -> CheckResult {
            tokio::time::sleep(Duration::from_millis(self.ms)).await;
            self.ran.fetch_add(1, Ordering::SeqCst);
            result(&self.id(), self.label(), CheckStatus::Pass, "slept", None)
        }
    }

    /// A check that panics.
    struct Exploding;

    #[async_trait]
    impl Check for Exploding {
        fn id(&self) -> CheckId {
            CheckId::from("boom.panics")
        }
        fn label(&self) -> &str {
            "explodes"
        }
        fn needs(&self) -> CheckNeeds {
            CheckNeeds::Local
        }
        async fn run(&self, _ctx: &CheckCtx) -> CheckResult {
            panic!("kaboom");
        }
    }

    /// A check that never finishes.
    struct Hangs;

    #[async_trait]
    impl Check for Hangs {
        fn id(&self) -> CheckId {
            CheckId::from("hang.forever")
        }
        fn label(&self) -> &str {
            "hangs"
        }
        fn needs(&self) -> CheckNeeds {
            CheckNeeds::Local
        }
        async fn run(&self, _ctx: &CheckCtx) -> CheckResult {
            std::future::pending::<()>().await;
            unreachable!("pending never resolves")
        }
    }

    /// A check that lies about its own id, to prove the runner stamps the real one.
    struct Liar;

    #[async_trait]
    impl Check for Liar {
        fn id(&self) -> CheckId {
            CheckId::from("truth.one")
        }
        fn label(&self) -> &str {
            "liar"
        }
        fn needs(&self) -> CheckNeeds {
            CheckNeeds::Local
        }
        async fn run(&self, _ctx: &CheckCtx) -> CheckResult {
            CheckResult {
                id: CheckId::from("some.other.id"),
                label: String::new(),
                status: CheckStatus::Pass,
                ms: 99_999,
                detail: "wrong id and a made-up duration".into(),
                fix: None,
            }
        }
    }

    /// A check that needs something the context does not have.
    struct NeedsDaemon;

    #[async_trait]
    impl Check for NeedsDaemon {
        fn id(&self) -> CheckId {
            CheckId::from("daemon.only")
        }
        fn label(&self) -> &str {
            "needs a daemon"
        }
        fn needs(&self) -> CheckNeeds {
            CheckNeeds::Daemon
        }
        async fn run(&self, _ctx: &CheckCtx) -> CheckResult {
            panic!("must never run without a daemon");
        }
    }

    // ---- fixtures ----------------------------------------------------------------------

    fn ctx() -> CheckCtx {
        ctx_with(Config::default())
    }

    fn ctx_with(cfg: Config) -> CheckCtx {
        CheckCtx {
            // Resolution reads the ambient environment and touches nothing: no test here
            // writes through `paths`, so this cannot race a sibling unit's tests.
            paths: Paths::resolve().expect("paths"),
            cfg: Arc::new(cfg),
            http: reqwest::Client::new(),
            rig: None,
            proxy_url: None,
            instance: None,
            ext: HashMap::new(),
        }
    }

    /// The default config with one listener moved. The **control** listener, deliberately:
    /// `proxy_bind` honours `$PROXY_PORT`, which another unit's tests mutate.
    fn cfg_with_control_bind(bind: &str) -> Config {
        Config {
            server: crate::config::ServerCfg {
                control_bind: bind.to_owned(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn registry(checks: Vec<Arc<dyn Check>>) -> Registry {
        let mut r = Registry::new();
        for c in checks {
            r.register(c);
        }
        r
    }

    /// The ids a filter selects, in the order `run` returns them.
    async fn selected_ids(reg: &Registry, only: Option<&str>) -> Vec<String> {
        let (tx, _rx) = mpsc::channel(8);
        reg.run(&ctx(), only, tx)
            .await
            .into_iter()
            .map(|r| r.id.0)
            .collect()
    }

    /// Drain a channel into a vec. The runner is done, so the sender is gone.
    async fn drain(mut rx: mpsc::Receiver<CheckResult>) -> Vec<CheckResult> {
        let mut out = Vec::new();
        while let Some(r) = rx.recv().await {
            out.push(r);
        }
        out
    }

    // ---- the acceptance paragraph ------------------------------------------------------

    #[tokio::test]
    async fn three_two_hundred_millisecond_checks_finish_in_under_four_hundred() {
        let ran = Arc::new(AtomicUsize::new(0));
        let reg = registry(vec![
            Arc::new(Slow {
                id: "slow.one",
                ms: 200,
                ran: ran.clone(),
            }),
            Arc::new(Slow {
                id: "slow.two",
                ms: 200,
                ran: ran.clone(),
            }),
            Arc::new(Slow {
                id: "slow.three",
                ms: 200,
                ran: ran.clone(),
            }),
        ]);
        let (tx, rx) = mpsc::channel(8);

        let started = Instant::now();
        let out = reg.run(&ctx(), None, tx).await;
        let elapsed = started.elapsed();

        assert_eq!(out.len(), 3, "every check reported");
        assert_eq!(ran.load(Ordering::SeqCst), 3);
        assert!(
            elapsed < Duration::from_millis(400),
            "600 ms of sequential work took {elapsed:?} — the runner is not concurrent"
        );
        assert_eq!(drain(rx).await.len(), 3, "each result was streamed");
    }

    #[tokio::test]
    async fn results_stream_as_each_lands_rather_than_at_the_end() {
        let ran = Arc::new(AtomicUsize::new(0));
        let reg = registry(vec![
            Arc::new(Slow {
                id: "slow.late",
                ms: 300,
                ran: ran.clone(),
            }),
            Arc::new(Slow {
                id: "slow.early",
                ms: 10,
                ran: ran.clone(),
            }),
        ]);
        let (tx, mut rx) = mpsc::channel(8);

        let ctx = ctx();
        let run = reg.run(&ctx, None, tx);
        tokio::pin!(run);

        // The fast check's result must arrive while the slow one is still running.
        let first = tokio::select! {
            r = rx.recv() => r.expect("a result before the run finished"),
            _ = &mut run => panic!("the run finished before streaming anything"),
        };
        assert_eq!(first.id.as_str(), "slow.early");

        let out = run.await;
        // …while the returned vec is in registration order, not completion order.
        assert_eq!(
            out.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["slow.late", "slow.early"]
        );
    }

    #[tokio::test]
    async fn only_filters_by_id_namespace_and_fragment() {
        let ran = Arc::new(AtomicUsize::new(0));
        let reg = registry(vec![
            Arc::new(Slow {
                id: "creds.vast",
                ms: 0,
                ran: ran.clone(),
            }),
            Arc::new(Slow {
                id: "creds.hf",
                ms: 0,
                ran: ran.clone(),
            }),
            Arc::new(Slow {
                id: "together.ratelimits",
                ms: 0,
                ran: ran.clone(),
            }),
        ]);

        assert_eq!(
            selected_ids(&reg, Some("creds.vast")).await,
            vec!["creds.vast"],
            "exact"
        );
        assert_eq!(
            selected_ids(&reg, Some("creds")).await,
            vec!["creds.vast", "creds.hf"],
            "namespace"
        );
        assert_eq!(
            selected_ids(&reg, Some("rate-limits")).await,
            vec!["together.ratelimits"],
            "separators are ignored, so what the operator sees in the docs works"
        );
        assert_eq!(selected_ids(&reg, Some("nothing-like-this")).await.len(), 0);
        assert_eq!(
            selected_ids(&reg, Some("")).await.len(),
            3,
            "an empty filter selects all"
        );
        assert_eq!(selected_ids(&reg, None).await.len(), 3);
    }

    #[tokio::test]
    async fn a_panicking_check_fails_and_never_poisons_the_run() {
        let ran = Arc::new(AtomicUsize::new(0));
        let reg = registry(vec![
            Arc::new(Exploding),
            Arc::new(Slow {
                id: "slow.survivor",
                ms: 0,
                ran: ran.clone(),
            }),
        ]);
        let (tx, rx) = mpsc::channel(8);

        let out = reg.run(&ctx(), None, tx).await;

        assert_eq!(out.len(), 2, "the sibling check still reported");
        assert_eq!(out[0].status, CheckStatus::Fail);
        assert_eq!(out[0].id.as_str(), "boom.panics");
        assert!(
            out[0].detail.contains("kaboom"),
            "detail: {}",
            out[0].detail
        );
        assert!(out[0].fix.is_some(), "a panic is a bug and says so");
        assert_eq!(out[1].status, CheckStatus::Pass);
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        assert_eq!(drain(rx).await.len(), 2, "both were streamed");
    }

    #[tokio::test(start_paused = true)]
    async fn a_hanging_check_fails_on_its_timeout() {
        let reg = registry(vec![Arc::new(Hangs)]);
        let (tx, _rx) = mpsc::channel(8);

        let out = reg.run(&ctx(), None, tx).await;

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].status, CheckStatus::Fail);
        assert!(
            out[0].detail.contains("timed out"),
            "detail: {}",
            out[0].detail
        );
    }

    #[tokio::test]
    async fn the_runner_stamps_id_label_and_duration() {
        let reg = registry(vec![Arc::new(Liar)]);
        let (tx, _rx) = mpsc::channel(8);

        let out = reg.run(&ctx(), None, tx).await;

        assert_eq!(out[0].id.as_str(), "truth.one", "the trait's id wins");
        assert_eq!(out[0].label, "liar", "an empty label is filled in");
        assert!(
            out[0].ms < 99_999,
            "the runner times the check, not the check"
        );
    }

    #[tokio::test]
    async fn an_unmet_precondition_is_skipped_not_failed() {
        let reg = registry(vec![Arc::new(NeedsDaemon)]);
        let (tx, _rx) = mpsc::channel(8);

        // `NeedsDaemon::run` panics if it is ever polled, so Skipped here also proves the
        // check body was not entered.
        let out = reg.run(&ctx(), None, tx).await;

        assert_eq!(out[0].status, CheckStatus::Skipped);
        assert!(out[0].detail.contains("no daemon"));
    }

    #[tokio::test]
    async fn a_closed_receiver_is_not_an_error() {
        let ran = Arc::new(AtomicUsize::new(0));
        let reg = registry(vec![Arc::new(Slow {
            id: "slow.one",
            ms: 0,
            ran: ran.clone(),
        })]);
        let (tx, rx) = mpsc::channel(1);
        drop(rx);

        assert_eq!(reg.run(&ctx(), None, tx).await.len(), 1);
    }

    #[tokio::test]
    async fn ids_lists_every_registered_check_in_order() {
        let reg = registry(local_checks());
        let ids: Vec<String> = reg.ids().into_iter().map(|i| i.0).collect();
        assert_eq!(
            ids,
            vec![
                "creds.vast",
                "creds.hf",
                "creds.together",
                "ports.proxy",
                "ports.control",
                "builds.discovered",
                "builds.flags",
                "devices.enumerated",
                "models.discovered",
                "state.writable",
                "legacy.migration",
            ]
        );
    }

    // ---- the local checks themselves ---------------------------------------------------

    #[test]
    fn every_local_check_is_local_and_labelled() {
        for c in local_checks() {
            assert_eq!(c.needs(), CheckNeeds::Local, "{}", c.id());
            assert!(!c.label().is_empty(), "{}", c.id());
            assert!(
                c.id().as_str().contains('.'),
                "ids are dotted: {}",
                c.id().as_str()
            );
        }
    }

    /// Run exactly one of the real local checks.
    async fn run_local(id: &str, ctx: &CheckCtx) -> CheckResult {
        let (tx, _rx) = mpsc::channel(8);
        let mut out = registry(local_checks()).run(ctx, Some(id), tx).await;
        assert_eq!(out.len(), 1, "`{id}` selected {} checks", out.len());
        out.remove(0)
    }

    #[tokio::test]
    async fn ports_control_passes_when_free_and_warns_when_taken() {
        // Port 0 asks the kernel for a free one; bind it, then read back the real port so
        // the "taken" half of this test cannot race another process.
        let held = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = held.local_addr().expect("addr");

        let cfg = cfg_with_control_bind(&addr.to_string());
        let taken = run_local("ports.control", &ctx_with(cfg.clone())).await;
        assert_eq!(taken.status, CheckStatus::Warn, "{}", taken.detail);
        assert!(taken.fix.is_some());

        drop(held);
        let free = run_local("ports.control", &ctx_with(cfg)).await;
        assert_eq!(free.status, CheckStatus::Pass, "{}", free.detail);
    }

    #[tokio::test]
    async fn ports_control_fails_on_an_address_that_cannot_be_bound() {
        // Not one of this host's addresses: EADDRNOTAVAIL, which is a real defect.
        let cfg = cfg_with_control_bind("192.0.2.1:2739");
        let out = run_local("ports.control", &ctx_with(cfg)).await;
        assert_eq!(out.status, CheckStatus::Fail, "{}", out.detail);
    }

    #[test]
    fn state_writable_probes_and_leaves_nothing_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("state/apexrouter");

        probe_writable(&nested).expect("writable");

        assert!(nested.is_dir(), "the state directory is created if missing");
        let left: Vec<_> = std::fs::read_dir(&nested)
            .expect("read_dir")
            .filter_map(std::result::Result::ok)
            .collect();
        assert!(left.is_empty(), "the probe file was removed");
    }

    #[test]
    fn state_writable_reports_an_unwritable_directory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).expect("mkdir");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).expect("chmod");

        let probed = probe_writable(&locked.join("state"));

        // Restore before asserting, so a failure still cleans up.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        assert!(probed.is_err(), "an unwritable parent must be reported");
    }

    #[tokio::test]
    async fn builds_and_devices_skip_without_a_rig_and_report_with_one() {
        use apexrouter_protocol::{BuildId, FlagSupport, Gpu, GpuBackend, LlamaBuild};

        for id in ["builds.discovered", "builds.flags", "devices.enumerated"] {
            let out = run_local(id, &ctx()).await;
            assert_eq!(out.status, CheckStatus::Skipped, "{id}: {}", out.detail);
        }

        let flags = FlagSupport {
            flags: std::collections::BTreeSet::from(["--flash-attn".to_owned()]),
            help_lines: 200,
            ..FlagSupport::default()
        };
        let rig = RigSnapshot {
            gpus: vec![Gpu {
                device: "Vulkan0".into(),
                index: 0,
                name: "a real one".into(),
                backend: GpuBackend::Vulkan,
                vram_total_mb: 8192,
                vram_free_mb: 8192,
                pci_bus_id: None,
                driver: None,
                is_software: false,
                seen_by_builds: vec![],
                held_by: vec![],
                reserved_mb: 0,
            }],
            builds: vec![LlamaBuild {
                id: BuildId::parse("build-vulkan").expect("id"),
                server_path: "/nowhere/bin/llama-server".into(),
                label: "vulkan".into(),
                build_info: None,
                backends: vec![GpuBackend::Vulkan],
                devices: vec!["Vulkan0".into()],
                flags,
                probed_at_unix: 0,
            }],
            ..RigSnapshot::default()
        };

        let mut with_rig = ctx();
        with_rig.rig = Some(Arc::new(rig.clone()));
        for id in ["builds.discovered", "builds.flags", "devices.enumerated"] {
            let out = run_local(id, &with_rig).await;
            assert_eq!(out.status, CheckStatus::Pass, "{id}: {}", out.detail);
        }
        assert!(run_local("builds.discovered", &with_rig)
            .await
            .detail
            .contains("build-vulkan"));

        // An unprobed binary and a software-only device are warnings, not failures.
        let mut degraded = rig;
        degraded.builds[0].flags = FlagSupport::default();
        degraded.gpus[0].is_software = true;
        let mut ctx_degraded = ctx();
        ctx_degraded.rig = Some(Arc::new(degraded));
        for id in ["builds.flags", "devices.enumerated"] {
            let out = run_local(id, &ctx_degraded).await;
            assert_eq!(out.status, CheckStatus::Warn, "{id}: {}", out.detail);
            assert!(out.fix.is_some(), "{id}");
        }

        // No builds at all: discovery warns, and there is nothing to probe.
        let mut ctx_empty = ctx();
        ctx_empty.rig = Some(Arc::new(RigSnapshot::default()));
        assert_eq!(
            run_local("builds.discovered", &ctx_empty).await.status,
            CheckStatus::Warn
        );
        assert_eq!(
            run_local("builds.flags", &ctx_empty).await.status,
            CheckStatus::Skipped
        );
        assert_eq!(
            run_local("devices.enumerated", &ctx_empty).await.status,
            CheckStatus::Warn
        );
    }

    #[test]
    fn a_credential_source_is_described_without_the_credential() {
        assert_eq!(
            describe_source(&CredentialSource::Env {
                var: "VAST_API_KEY".into()
            }),
            "from $VAST_API_KEY"
        );
        assert_eq!(
            describe_source(&CredentialSource::File {
                path: "/home/a/.config/vastai/vast_api_key".into()
            }),
            "from /home/a/.config/vastai/vast_api_key"
        );
        assert_eq!(
            describe_source(&CredentialSource::Managed {
                store: "credentials.toml".into()
            }),
            "from credentials.toml"
        );
    }

    #[test]
    fn only_matching_is_case_and_separator_insensitive() {
        assert!(matches_only("creds.vast", None));
        assert!(matches_only("creds.vast", Some("creds.vast")));
        assert!(matches_only("creds.vast", Some("CREDS")));
        assert!(matches_only("together.ratelimits", Some("rate_limits")));
        assert!(matches_only("state.writable", Some("  ")));
        assert!(!matches_only("creds.vast", Some("credsx")));
    }

    #[test]
    fn a_duration_longer_than_u32_millis_saturates() {
        assert_eq!(millis(Duration::from_millis(1500)), 1500);
        assert_eq!(millis(Duration::from_secs(60 * 60 * 24 * 365)), u32::MAX);
    }
}
