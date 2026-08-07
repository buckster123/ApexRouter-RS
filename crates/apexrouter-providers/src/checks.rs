//! OWNER: unit P-08 (providers/src/{checks,smoke,compare}.rs). Do not edit outside that
//! unit.
//!
//! Provider-specific `Check`s: `ssh.controlmaster`, `ssh.binary`, `vast.credit`,
//! `vast.orphans`, `together.ratelimits`, `net.stall`, `proxy.roundtrip`,
//! `endpoint.orphans`, plus the four deep SSH probes and the RX sample used by
//! `GET /v1/vast/instances/{id}/diagnose`.
//!
//! (`creds.{vast,hf,together}` are in [`apexrouter_core::checks::local_checks`], which
//! resolves them without a client; registering them again here would double every row.)
//!
//! These live here rather than in `core::checks` because they need vast/together/ssh
//! clients; `apexrouter-server` registers them at startup, and `CheckCtx::ext` is how they
//! get their clients without `core` ever depending on this crate.
//!
//! [`provider_ext`] is the other half of that sentence, and it was missing: registering a
//! check that reads `ext` is worthless unless something *writes* `ext`. Nothing did, so
//! `vast.credit` and `vast.orphans` skipped on every machine — including one whose
//! `creds.vast` row passed on the line above. Both `apexrouter doctor` (no daemon) and
//! `GET /v1/diagnose` (daemon) now fill the map before they run the registry.
//!
//! Nothing in this file can spend money. The vast calls are `GET /users/current/` and
//! `GET /instances/`, both read-only, and a check that finds an orphan **reports** it — the
//! Destroy action belongs to a surface that can ask for confirmation first.

use crate::local::adopt;
use crate::smoke::{first_chars, redact};
use crate::vast::api::VastApi;
use apexrouter_core::checks::{Check, CheckCtx, CheckNeeds};
use apexrouter_core::config::Config;
use apexrouter_core::error::Result;
use apexrouter_core::exec::{self, Output, SshOpts};
use apexrouter_core::ledger::Ledger;
use apexrouter_core::paths::Paths;
use apexrouter_core::proc::Adoption;
use apexrouter_core::secret::{self, Secret};
use apexrouter_core::store::Store;
use apexrouter_core::upstream::join_v1;
use apexrouter_protocol::{
    CheckId, CheckResult, CheckStatus, DesiredState, DownloadHealth, LedgerRow, ProviderId,
    StallVerdict, VastAccount, VastInstance,
};
use async_trait::async_trait;
use std::any::Any;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

// ----------------------------------------------------------------------------------------
// what the server injects, and how
// ----------------------------------------------------------------------------------------

/// `CheckCtx::ext` key for [`VastExt`].
pub const EXT_VAST: &str = "vast";
/// `CheckCtx::ext` key for [`SshExt`].
pub const EXT_SSH: &str = "ssh";
/// `CheckCtx::ext` key for [`TogetherExt`].
pub const EXT_TOGETHER: &str = "together";

/// The vast.ai client, injected by `apexrouter-server` at startup.
///
/// A newtype rather than the bare trait object because an `Arc<dyn Any>` can only be
/// downcast to a *sized* type.
pub struct VastExt(pub Arc<dyn VastApi>);

/// Which box the ssh-flavoured checks are about, and how to reach it.
pub struct SshExt {
    /// ssh destination, verbatim — `root@ssh5.vast.ai`.
    pub host: String,
    /// ssh port.
    pub port: u16,
    /// Connection options, including the ControlPath the tunnel supervisor uses.
    pub opts: SshOpts,
}

/// Where together.ai is and what to authenticate with, when the caller already knows.
///
/// Absent, `together.ratelimits` resolves both from config and the credential chain, which
/// is what the daemon does. Present, it is a test seam **and** the way an operator's
/// `--base-url` override reaches the check.
pub struct TogetherExt {
    /// Base URL, used verbatim: `api.together.xyz` is never rewritten to `.ai`.
    pub base_url: String,
    /// Bearer credential, when there is one.
    pub cred: Option<Secret<String>>,
}

/// Fetch one injected client, or `None` when the daemon did not register it.
pub fn ext<T: Any + Send + Sync>(ctx: &CheckCtx, key: &str) -> Option<Arc<T>> {
    ctx.ext.get(key)?.clone().downcast::<T>().ok()
}

/// The type every `CheckCtx::ext` map is.
pub type ExtMap = std::collections::HashMap<String, Arc<dyn Any + Send + Sync>>;

/// Put a vast client into an `ext` map under the key `vast.credit` and `vast.orphans` read.
///
/// The daemon already holds a client; it should hand that one over rather than build a
/// second connection pool. Anything a caller already put under [`EXT_VAST`] wins, because
/// an explicitly injected client is a test seam.
pub fn insert_vast(ext: &mut ExtMap, api: Arc<dyn VastApi>) {
    ext.entry(EXT_VAST.to_owned())
        .or_insert_with(|| Arc::new(VastExt(api)));
}

/// Every provider client that can be built from configuration alone, for the surfaces that
/// run the registry **without** a daemon — `apexrouter doctor` is the whole reason this
/// exists.
///
/// Before this, nothing anywhere put a `VastExt` into a `CheckCtx`, so `vast.credit` and
/// `vast.orphans` skipped with "no vast.ai client" on every machine, including one whose
/// `creds.vast` row passed immediately above them. A check that can never run is worse than
/// no check, because `doctor --json` then reports zero failures while having asked nothing.
///
/// **Nothing here spends money and nothing here dials anything.** Building a client resolves
/// a credential and constructs a connection pool; the first byte on the wire is the
/// read-only `GET /users/current/` a check makes later. A machine with no vast credential
/// gets an empty map and an honest `Skipped`.
///
/// `ssh.*` and `net.stall` are deliberately absent: they are *per instance*, and the
/// instance is chosen by the caller, not by configuration.
pub fn provider_ext(cfg: &Config, paths: &Paths) -> ExtMap {
    let mut ext = ExtMap::new();
    match crate::vast::VastApiHttp::from_config(cfg, paths) {
        Ok(api) => insert_vast(&mut ext, Arc::new(api)),
        // Not an error and not worth a warning: a machine that does not rent GPUs has no
        // vast key, and the two checks say so themselves.
        Err(e) => tracing::debug!(reason = %e, "no vast client for the check registry"),
    }
    ext
}

// ----------------------------------------------------------------------------------------
// the registries
// ----------------------------------------------------------------------------------------

/// The checks that need a provider client, a daemon or the network.
///
/// Registration order is display order, and it continues
/// [`apexrouter_core::checks::local_checks`]'s: this machine first, then the things that
/// cost money, then the things that carry traffic.
pub fn provider_checks() -> Vec<Arc<dyn Check>> {
    vec![
        Arc::new(SshBinary),
        Arc::new(SshControlMaster),
        Arc::new(VastCredit),
        Arc::new(VastOrphans),
        Arc::new(TogetherRateLimits),
        Arc::new(ProxyRoundtrip),
        Arc::new(EndpointOrphans),
        Arc::new(NetStall),
    ]
}

/// The deep diagnostics behind `GET /v1/vast/instances/{id}/diagnose`: the four SSH probes
/// and the RX sample, as one registry that runs them **concurrently**.
///
/// LocalRouter ran the four sequentially with 15 + 10 + 12 + 12 second timeouts and then
/// sampled RX for another four, so the panel took the better part of a minute to draw. Run
/// together they land in the slowest one's time.
pub fn deep_checks() -> Vec<Arc<dyn Check>> {
    vec![
        Arc::new(RemoteProbe {
            id: "instance.processes",
            label: "top processes",
            argv: "ps -eo pid,etime,pcpu,pmem,cmd --sort=-pcpu | head -15",
            timeout: Duration::from_secs(15),
            kind: ProbeKind::Processes,
        }),
        Arc::new(RemoteProbe {
            id: "instance.disk",
            label: "workspace disk",
            argv: "df -h /workspace 2>/dev/null || df -h /",
            timeout: Duration::from_secs(10),
            kind: ProbeKind::Disk,
        }),
        Arc::new(RemoteProbe {
            id: "instance.models",
            label: "model files",
            argv: "find /workspace/models -type f \\( -name '*.gguf' -o -name '*.incomplete' \\) \
                   -exec ls -lh {} \\; 2>/dev/null | head -20",
            timeout: Duration::from_secs(12),
            kind: ProbeKind::Models,
        }),
        Arc::new(RemoteProbe {
            id: "instance.log",
            label: "launch log",
            argv: "tail -30 /var/log/launch.log 2>/dev/null || echo '(no log yet)'",
            timeout: Duration::from_secs(12),
            kind: ProbeKind::Log,
        }),
        Arc::new(NetStall),
    ]
}

// ----------------------------------------------------------------------------------------
// the RX sample
// ----------------------------------------------------------------------------------------

/// The default sample window. Four seconds is long enough to see a stalled download and
/// short enough to sit inside a diagnose panel.
pub const RX_WINDOW_SECS: u32 = 4;

/// Below this many bytes over the window the download is stalled, and the restart is worth
/// offering.
pub const RX_STALLED_BYTES: u64 = 1000;

/// Below this the download is moving, but slowly enough to be worth flagging.
pub const RX_SLOW_MBPS: f32 = 50.0;

/// Classify an RX delta. Pure, so the thresholds are testable without a rented box.
///
/// `mbps` is **megabits** per second — `bytes * 8 / window / 1e6` — which is the unit the
/// hosting industry quotes and the one the stall banner has always used.
pub fn download_health(rx_bytes: u64, window_secs: u32) -> DownloadHealth {
    let window = window_secs.max(1);
    let mbps = ((rx_bytes as f64 * 8.0) / (f64::from(window) * 1_000_000.0)) as f32;
    let verdict = if rx_bytes < RX_STALLED_BYTES {
        StallVerdict::Stalled
    } else if mbps < RX_SLOW_MBPS {
        StallVerdict::Slow
    } else {
        StallVerdict::Active
    };
    DownloadHealth {
        sampled_at_unix: chrono::Utc::now().timestamp(),
        rx_bytes_4s: rx_bytes,
        mbps,
        verdict,
    }
}

/// Sample eth0's inbound byte counter over `window_secs`, over ssh.
///
/// The remote side is one `/proc/net/dev` read, a sleep and a second read; the ssh budget is
/// the window plus fifteen seconds, because connection setup is not part of the sample.
///
/// A non-zero exit or a non-integer answer is an `Err`, never a fabricated zero: "we could
/// not measure" and "nothing arrived" must not look alike, since one of them offers to
/// restart a download.
///
/// # Errors
/// Returns [`apexrouter_core::error::Error`] when ssh cannot run, times out, exits non-zero
/// or answers with something that is not a byte count.
pub async fn rx_sample(
    host: &str,
    port: u16,
    opts: &SshOpts,
    window_secs: u32,
) -> Result<DownloadHealth> {
    let window = window_secs.max(1);
    let script = format!(
        "RX1=$(cat /proc/net/dev | awk '/eth0/{{print $2}}'); sleep {window}; \
         RX2=$(cat /proc/net/dev | awk '/eth0/{{print $2}}'); echo $((RX2-RX1))"
    );
    let budget = Duration::from_secs(u64::from(window) + 15);
    let out = exec::ssh(host, port, opts, &[&script], budget).await?;
    let bytes = parse_rx(&out).ok_or_else(|| apexrouter_core::error::Error::Invalid {
        what: "RX sample".to_owned(),
        why: format!(
            "it did not answer with a byte count (rc {}{}): {}",
            out.status,
            if out.timed_out { ", timed out" } else { "" },
            first_chars(&out.stderr, 120)
        ),
    })?;
    Ok(download_health(bytes, window))
}

/// The byte count an [`rx_sample`] answered with, or `None` when it did not answer with one.
fn parse_rx(out: &Output) -> Option<u64> {
    if out.timed_out || out.status != 0 {
        return None;
    }
    out.stdout.trim().lines().next_back()?.trim().parse().ok()
}

// ----------------------------------------------------------------------------------------
// ssh.binary
// ----------------------------------------------------------------------------------------

/// Is there an `ssh` to run at all?
struct SshBinary;

#[async_trait]
impl Check for SshBinary {
    fn id(&self) -> CheckId {
        CheckId::from("ssh.binary")
    }
    fn label(&self) -> &str {
        "ssh client is installed"
    }
    fn needs(&self) -> CheckNeeds {
        CheckNeeds::Local
    }
    async fn run(&self, _ctx: &CheckCtx) -> CheckResult {
        // `ssh -V` prints its banner to stderr and exits 0 — one of the few that does.
        match exec::run(Path::new("ssh"), &["-V"], Duration::from_secs(5)).await {
            Ok(out) if out.status == 0 => result(self, CheckStatus::Pass, banner(&out), None),
            Ok(out) => result(
                self,
                CheckStatus::Fail,
                format!("`ssh -V` exited {}: {}", out.status, banner(&out)),
                Some("install openssh-client"),
            ),
            Err(e) => result(
                self,
                CheckStatus::Fail,
                format!("`ssh` would not start: {e}"),
                Some("install openssh-client — every tunnel and every deep probe needs it"),
            ),
        }
    }
}

/// The first line a command printed, from wherever it landed.
fn banner(out: &Output) -> String {
    let text = if out.stderr.trim().is_empty() {
        &out.stdout
    } else {
        &out.stderr
    };
    first_chars(text.lines().next().unwrap_or_default(), 120)
}

// ----------------------------------------------------------------------------------------
// ssh.controlmaster
// ----------------------------------------------------------------------------------------

/// Is a ControlMaster actually multiplexing, or is every command paying a fresh handshake?
///
/// Worth a measured ~500 ms → RTT win per remote command, which is the difference between
/// an agentic tool loop that feels local and one that does not.
struct SshControlMaster;

#[async_trait]
impl Check for SshControlMaster {
    fn id(&self) -> CheckId {
        CheckId::from("ssh.controlmaster")
    }
    fn label(&self) -> &str {
        "ssh ControlMaster is multiplexing"
    }
    fn needs(&self) -> CheckNeeds {
        CheckNeeds::Local
    }
    async fn run(&self, ctx: &CheckCtx) -> CheckResult {
        let Some(t) = ext::<SshExt>(ctx, EXT_SSH) else {
            return result(
                self,
                CheckStatus::Skipped,
                "no instance selected — nothing to multiplex to",
                None,
            );
        };
        let Some(cp) = t.opts.control_path.as_ref() else {
            return result(
                self,
                CheckStatus::Warn,
                "no ControlPath configured — every remote command pays a fresh handshake",
                Some("bring the tunnel up with `apexrouter tunnel up <instance-id>`"),
            );
        };
        if !cp.exists() {
            return result(
                self,
                CheckStatus::Warn,
                format!("no master socket at {}", cp.display()),
                Some("`apexrouter tunnel up <instance-id>` opens one"),
            );
        }

        let port = t.port.to_string();
        let control = format!("ControlPath={}", cp.display());
        let argv = [
            "-O",
            "check",
            "-p",
            port.as_str(),
            "-o",
            "BatchMode=yes",
            "-o",
            control.as_str(),
            t.host.as_str(),
        ];
        match exec::run(Path::new("ssh"), &argv, Duration::from_secs(5)).await {
            Ok(out) if out.status == 0 => result(
                self,
                CheckStatus::Pass,
                format!("master running at {}", cp.display()),
                None,
            ),
            Ok(out) => result(
                self,
                CheckStatus::Warn,
                format!("the socket at {} is stale: {}", cp.display(), banner(&out)),
                Some("`apexrouter tunnel down <id>` then `tunnel up <id>` replaces it"),
            ),
            Err(e) => result(
                self,
                CheckStatus::Fail,
                format!("`ssh -O check` would not run: {e}"),
                Some("install openssh-client"),
            ),
        }
    }
}

// ----------------------------------------------------------------------------------------
// vast.credit
// ----------------------------------------------------------------------------------------

/// How much credit is left, and how long the current burn leaves before it runs out.
struct VastCredit;

#[async_trait]
impl Check for VastCredit {
    fn id(&self) -> CheckId {
        CheckId::from("vast.credit")
    }
    fn label(&self) -> &str {
        "vast.ai credit"
    }
    fn needs(&self) -> CheckNeeds {
        CheckNeeds::Network
    }
    async fn run(&self, ctx: &CheckCtx) -> CheckResult {
        let Some(api) = ext::<VastExt>(ctx, EXT_VAST) else {
            return skipped_no_vast(self);
        };
        let account = match api.0.account().await {
            Ok(a) => a,
            Err(e) => {
                return result(
                    self,
                    CheckStatus::Warn,
                    format!("vast.ai did not answer: {}", redact(&e.to_string())),
                    Some("check `creds.vast` and the network — this is not proof of anything"),
                )
            }
        };
        // A failed fleet read must not turn a good balance into a bad verdict.
        let burn = api
            .0
            .instances()
            .await
            .ok()
            .map(|live| burn_per_hour(&live));
        let (status, detail, fix) = credit_verdict(&account, burn);
        result(self, status, detail, fix.as_deref())
    }
}

/// Dollars per hour every live instance is costing right now.
pub fn burn_per_hour(live: &[VastInstance]) -> f64 {
    live.iter()
        // A parked box bills storage, not GPUs; a destroyed one bills nothing. Folding
        // either into the hourly burn overstates the number the operator watches most.
        .filter(|i| {
            !matches!(
                i.phase(),
                apexrouter_protocol::BootPhase::Parked | apexrouter_protocol::BootPhase::Destroyed
            )
        })
        .filter_map(|i| i.dph_total)
        .filter(|d| d.is_finite())
        .sum()
}

/// Turn a balance and a burn rate into a verdict. Pure, so the thresholds are testable.
///
/// `Fail` at zero or when vast says it cannot bill us; `Warn` under an hour of runway or
/// under a dollar, because "it died mid-download" is the expensive failure this prevents.
pub fn credit_verdict(
    account: &VastAccount,
    burn_per_hour: Option<f64>,
) -> (CheckStatus, String, Option<String>) {
    let credit = account.credit;
    let mut detail = format!("${credit:.2} credit");
    if let Some(burn) = burn_per_hour.filter(|b| *b > 0.0) {
        detail.push_str(&format!(
            "; burning ${burn:.2}/hr → ~{:.1} h left",
            credit / burn
        ));
    }

    if account.can_pay == Some(false) {
        return (
            CheckStatus::Fail,
            format!("{detail}; vast.ai reports billing is not available"),
            Some("add a payment method at https://console.vast.ai/billing".to_owned()),
        );
    }
    if credit <= 0.0 {
        return (
            CheckStatus::Fail,
            detail,
            Some("top up before renting — a create call will be refused".to_owned()),
        );
    }
    let hours_left = burn_per_hour
        .filter(|b| *b > 0.0)
        .map_or(f64::INFINITY, |b| credit / b);
    if hours_left < 1.0 {
        return (
            CheckStatus::Warn,
            format!("{detail} — under an hour of runway"),
            Some("top up, or destroy what you are not using: `apexrouter vast ls`".to_owned()),
        );
    }
    if credit < 1.0 {
        return (
            CheckStatus::Warn,
            detail,
            Some("top up before the next rental".to_owned()),
        );
    }
    (CheckStatus::Pass, detail, None)
}

// ----------------------------------------------------------------------------------------
// vast.orphans
// ----------------------------------------------------------------------------------------

/// Is anything billing that the ledger does not know about?
struct VastOrphans;

#[async_trait]
impl Check for VastOrphans {
    fn id(&self) -> CheckId {
        CheckId::from("vast.orphans")
    }
    fn label(&self) -> &str {
        "no untracked vast.ai instances"
    }
    fn needs(&self) -> CheckNeeds {
        CheckNeeds::Network
    }
    async fn run(&self, ctx: &CheckCtx) -> CheckResult {
        // The client first, so a machine with no vast credential never opens the ledger.
        let Some(api) = ext::<VastExt>(ctx, EXT_VAST) else {
            return skipped_no_vast(self);
        };
        let live = match api.0.instances().await {
            Ok(l) => l,
            Err(e) => {
                return result(
                    self,
                    CheckStatus::Warn,
                    format!("vast.ai did not answer: {}", redact(&e.to_string())),
                    Some("an unreachable API is not proof that nothing is billing"),
                )
            }
        };
        let active =
            match Ledger::open(&ctx.paths).and_then(|l| l.active()) {
                Ok(rows) => rows,
                Err(e) => return result(
                    self,
                    CheckStatus::Warn,
                    format!("the ledger could not be read: {e}"),
                    Some(
                        "`apexrouter doctor --only state.writable` says whether $STATE is writable",
                    ),
                ),
            };
        let (status, detail, fix) = orphan_report(&active, &live).verdict();
        result(self, status, detail, fix.as_deref())
    }
}

/// What the ledger and the fleet disagree about.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OrphanReport {
    /// Billing right now with no ledger row: **we are paying for these and did not know**.
    pub untracked: Vec<u64>,
    /// A ledger row for an instance vast.ai no longer lists: stale bookkeeping, not money.
    pub stale: Vec<u64>,
    /// `Reserved` or `OrphanSuspect` rows that never got an instance id — a create call we
    /// cannot prove either way.
    pub unlinked: usize,
}

impl OrphanReport {
    /// The row this becomes.
    pub fn verdict(&self) -> (CheckStatus, String, Option<String>) {
        if !self.untracked.is_empty() {
            return (
                CheckStatus::Fail,
                format!("billing with no ledger row: {}", join_ids(&self.untracked)),
                Some(
                    "reconcile, then destroy what you did not mean to keep: \
                     `apexrouter vast ls --orphans`"
                        .to_owned(),
                ),
            );
        }
        if self.unlinked > 0 {
            return (
                CheckStatus::Fail,
                format!(
                    "{} reservation(s) whose create call was never confirmed",
                    self.unlinked
                ),
                Some(
                    "`apexrouter vast ls --orphans` lists them; after checking the console, \
                     `apexrouter vast reconcile --yes` clears bookkeeping only"
                        .to_owned(),
                ),
            );
        }
        if !self.stale.is_empty() {
            return (
                CheckStatus::Warn,
                format!(
                    "{} ledger row(s) for instances vast.ai no longer lists: {}",
                    self.stale.len(),
                    join_ids(&self.stale)
                ),
                Some(format!(
                    "`apexrouter vast ls --orphans` then `apexrouter vast forget {} --yes` \
                     (or `vast reconcile --yes` for all stale) — bookkeeping only, no destroy",
                    self.stale
                        .first()
                        .map(|id| id.to_string())
                        .unwrap_or_else(|| "<id>".to_owned())
                )),
            );
        }
        (
            CheckStatus::Pass,
            "the ledger and the fleet agree".to_owned(),
            None,
        )
    }
}

/// Compare what the ledger believes is alive with what vast.ai is actually running.
///
/// Pure on purpose: **no test may rent anything**, so the only way this logic can be
/// exercised is as a function over two lists.
pub fn orphan_report(active: &[LedgerRow], live: &[VastInstance]) -> OrphanReport {
    let known: BTreeSet<u64> = active
        .iter()
        .filter_map(|r| r.instance_id)
        .map(|i| i.0)
        .collect();
    let running: BTreeSet<u64> = live.iter().map(|i| i.id.0).collect();

    OrphanReport {
        untracked: running.difference(&known).copied().collect(),
        stale: known.difference(&running).copied().collect(),
        unlinked: active.iter().filter(|r| r.instance_id.is_none()).count(),
    }
}

/// `12345, 12346`.
fn join_ids(ids: &[u64]) -> String {
    ids.iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The one wording for "there is no vast client here".
///
/// Since [`provider_ext`] builds the client from configuration, this now means exactly one
/// thing — **no credential resolved** — so the fix line is the command that supplies one.
/// It used to mean "nobody injected a client", which no operator could act on.
fn skipped_no_vast(check: &dyn Check) -> CheckResult {
    result(
        check,
        CheckStatus::Skipped,
        "no vast.ai credential on this machine, so there is no fleet to check",
        Some(
            "`apexrouter provider set vast --key-stdin`, or export $VAST_API_KEY — \
             skip this if you do not rent GPUs",
        ),
    )
}

// ----------------------------------------------------------------------------------------
// together.ratelimits
// ----------------------------------------------------------------------------------------

/// What together.ai's rate-limit headers say right now.
struct TogetherRateLimits;

#[async_trait]
impl Check for TogetherRateLimits {
    fn id(&self) -> CheckId {
        CheckId::from("together.ratelimits")
    }
    fn label(&self) -> &str {
        "together.ai rate limits"
    }
    fn needs(&self) -> CheckNeeds {
        CheckNeeds::Network
    }
    async fn run(&self, ctx: &CheckCtx) -> CheckResult {
        let injected = ext::<TogetherExt>(ctx, EXT_TOGETHER);
        let (base_url, cred) = match injected.as_deref() {
            Some(t) => (t.base_url.clone(), t.cred.clone()),
            None => match resolve_together(ctx) {
                Some(pair) => pair,
                None => {
                    return result(
                        self,
                        CheckStatus::Skipped,
                        "together.ai is not configured",
                        None,
                    )
                }
            },
        };

        let mut rb = ctx
            .http
            .get(join_v1(&base_url, "/v1/models"))
            .timeout(Duration::from_secs(15))
            .header("accept", "*/*");
        if let Some(c) = cred.as_ref() {
            rb = rb.bearer_auth(c.expose());
        }
        match rb.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let header = |name: &str| {
                    resp.headers()
                        .get(name)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned)
                };
                let (s, detail, fix) = ratelimit_verdict(
                    status,
                    header("x-ratelimit-limit").as_deref(),
                    header("x-ratelimit-remaining").as_deref(),
                    header("x-ratelimit-reset").as_deref(),
                );
                result(self, s, detail, fix.as_deref())
            }
            Err(e) => result(
                self,
                CheckStatus::Fail,
                format!("together.ai is unreachable: {}", redact(&e.to_string())),
                Some("check the network, then `[providers.together] base_url`"),
            ),
        }
    }
}

/// `[providers.together] base_url` plus whatever the credential chain yields.
fn resolve_together(ctx: &CheckCtx) -> Option<(String, Option<Secret<String>>)> {
    let id = ProviderId::parse("together").ok()?;
    let base_url = ctx.cfg.provider(&id)?.base_url.clone();
    if base_url.trim().is_empty() {
        return None;
    }
    let cred = secret::resolve_provider(&ctx.cfg, &ctx.paths, &id)
        .ok()
        .flatten()
        .map(|r| r.secret);
    Some((base_url, cred))
}

/// Read the rate-limit posture off one answered request.
///
/// **`x-ratelimit-reset` is the header that matters and `x-ratelimit-remaining` is not
/// relied upon**: Together sends `reset` on a 429 and is inconsistent about `remaining`, so
/// a missing `remaining` is silence, never zero.
pub fn ratelimit_verdict(
    status: u16,
    limit: Option<&str>,
    remaining: Option<&str>,
    reset: Option<&str>,
) -> (CheckStatus, String, Option<String>) {
    match status {
        429 => (
            CheckStatus::Warn,
            match reset {
                Some(r) => format!("rate limited; x-ratelimit-reset: {r}"),
                None => "rate limited, and no x-ratelimit-reset was sent".to_owned(),
            },
            Some(
                "wait for the reset, or route elsewhere: `apexrouter route set auto <alias>`"
                    .to_owned(),
            ),
        ),
        401 | 403 => (
            CheckStatus::Fail,
            format!("the credential was rejected (HTTP {status})"),
            Some("`apexrouter provider set together --key-stdin`".to_owned()),
        ),
        s if (200..300).contains(&s) => {
            let detail = match (limit, remaining) {
                (Some(l), Some(r)) => format!("{r} of {l} remaining"),
                (Some(l), None) => format!("limit {l}; no remaining header"),
                (None, Some(r)) => format!("{r} remaining"),
                (None, None) => {
                    "not rate limited; the provider publishes no rate-limit headers".to_owned()
                }
            };
            (CheckStatus::Pass, detail, None)
        }
        s => (
            CheckStatus::Warn,
            format!("together.ai answered HTTP {s}"),
            Some("check `[providers.together] base_url` — it is used verbatim".to_owned()),
        ),
    }
}

// ----------------------------------------------------------------------------------------
// proxy.roundtrip
// ----------------------------------------------------------------------------------------

/// Does a request actually go through the proxy listener?
///
/// `GET /v1/models` on the **data plane** — the listener clients point at — rather than the
/// control plane, because "the daemon is up" and "the thing Claude Code talks to answers"
/// are different questions and only the second one matters to a user.
struct ProxyRoundtrip;

#[async_trait]
impl Check for ProxyRoundtrip {
    fn id(&self) -> CheckId {
        CheckId::from("proxy.roundtrip")
    }
    fn label(&self) -> &str {
        "a request round-trips through the proxy"
    }
    fn needs(&self) -> CheckNeeds {
        CheckNeeds::Daemon
    }
    async fn run(&self, ctx: &CheckCtx) -> CheckResult {
        let url = join_v1(&proxy_url(&ctx.cfg.server.proxy_bind), "/v1/models");
        match ctx
            .http
            .get(&url)
            .timeout(Duration::from_secs(10))
            .header("accept", "*/*")
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                result(self, CheckStatus::Pass, format!("{url} answered 200"), None)
            }
            Ok(resp) if resp.status().as_u16() == 401 => result(
                self,
                CheckStatus::Warn,
                format!("{url} answered 401"),
                Some("export the token named by `[server] token_env` and retry"),
            ),
            Ok(resp) => result(
                self,
                CheckStatus::Fail,
                format!("{url} answered {}", resp.status().as_u16()),
                Some("`apexrouter status` shows what the router has to route to"),
            ),
            Err(e) => result(
                self,
                CheckStatus::Fail,
                format!("{url} is unreachable: {}", redact(&e.to_string())),
                Some("`apexrouter serve` starts the daemon"),
            ),
        }
    }
}

/// A bind address as a URL a client can actually use: a wildcard bind is reached on
/// loopback, never on `0.0.0.0`.
fn proxy_url(bind: &str) -> String {
    let (host, port) = match bind.rsplit_once(':') {
        Some((h, p)) => (h.trim_matches(['[', ']']), p),
        None => ("127.0.0.1", bind),
    };
    match host {
        "0.0.0.0" | "::" | "" => format!("http://127.0.0.1:{port}"),
        h if h.contains(':') => format!("http://[{h}]:{port}"),
        h => format!("http://{h}:{port}"),
    }
}

// ----------------------------------------------------------------------------------------
// endpoint.orphans
// ----------------------------------------------------------------------------------------

/// Endpoint records whose process is gone, or is no longer ours.
///
/// `Foreign` is the interesting one: something *else* holds the pid a record names, and the
/// supervisor will never signal it — which is correct, and is exactly why an operator has to
/// be told rather than left wondering why `endpoint stop` does nothing.
struct EndpointOrphans;

#[async_trait]
impl Check for EndpointOrphans {
    fn id(&self) -> CheckId {
        CheckId::from("endpoint.orphans")
    }
    fn label(&self) -> &str {
        "endpoint records match live processes"
    }
    fn needs(&self) -> CheckNeeds {
        CheckNeeds::Local
    }
    async fn run(&self, ctx: &CheckCtx) -> CheckResult {
        let store = Store::new(ctx.paths.clone());
        let records =
            match store.list_endpoints() {
                Ok(r) => r,
                Err(e) => return result(
                    self,
                    CheckStatus::Warn,
                    format!("the endpoint records could not be read: {e}"),
                    Some(
                        "`apexrouter doctor --only state.writable` says whether $STATE is writable",
                    ),
                ),
            };

        let mut foreign = Vec::new();
        let mut vanished = Vec::new();
        for rec in records
            .iter()
            .filter(|r| r.desired == DesiredState::Running)
        {
            match adopt::adopt(rec) {
                Adoption::Adopted(_) => {}
                Adoption::Foreign { pid, .. } | Adoption::Ambiguous { pid, .. } => {
                    foreign.push(format!("{} (pid {pid})", rec.id));
                }
                Adoption::Vanished => vanished.push(rec.id.to_string()),
            }
        }

        if !foreign.is_empty() {
            return result(
                self,
                CheckStatus::Warn,
                format!(
                    "not ours any more, and never signalled: {}",
                    foreign.join(", ")
                ),
                Some("`apexrouter endpoint rm <id>` drops the record; the process is left alone"),
            );
        }
        if !vanished.is_empty() {
            return result(
                self,
                CheckStatus::Warn,
                format!("recorded as running, but gone: {}", vanished.join(", ")),
                // NOT `endpoint start <id>`: that verb takes a **model**, resolved through
                // `/v1/models/local`, and an endpoint id is not a model id — the advice
                // this replaces produced "no model matching `local-carnice-9b-q6-k`".
                // `restart` is the verb that relaunches a record from its own spec.
                Some(
                    "`apexrouter endpoint restart <id>` relaunches it from the recorded spec; \
                     `endpoint rm <id>` forgets it",
                ),
            );
        }
        result(
            self,
            CheckStatus::Pass,
            format!("{} endpoint record(s), all accounted for", records.len()),
            None,
        )
    }
}

// ----------------------------------------------------------------------------------------
// net.stall
// ----------------------------------------------------------------------------------------

/// Is the model download on the rented box actually moving?
struct NetStall;

#[async_trait]
impl Check for NetStall {
    fn id(&self) -> CheckId {
        CheckId::from("net.stall")
    }
    fn label(&self) -> &str {
        "download is moving"
    }
    fn needs(&self) -> CheckNeeds {
        CheckNeeds::Instance
    }
    async fn run(&self, ctx: &CheckCtx) -> CheckResult {
        let Some(t) = ext::<SshExt>(ctx, EXT_SSH) else {
            return result(
                self,
                CheckStatus::Skipped,
                "no ssh target for this instance",
                None,
            );
        };
        match rx_sample(&t.host, t.port, &t.opts, RX_WINDOW_SECS).await {
            Ok(health) => {
                let detail = format!(
                    "{} bytes in {RX_WINDOW_SECS} s ({:.1} Mbps)",
                    health.rx_bytes_4s, health.mbps
                );
                match health.verdict {
                    StallVerdict::Active => result(self, CheckStatus::Pass, detail, None),
                    StallVerdict::Slow => result(
                        self,
                        CheckStatus::Warn,
                        format!("{detail} — slow"),
                        Some("nothing to do but wait, unless it stays here"),
                    ),
                    StallVerdict::Stalled => result(
                        self,
                        CheckStatus::Fail,
                        format!("{detail} — stalled"),
                        Some("`apexrouter vast restart-download <id>`"),
                    ),
                }
            }
            Err(e) => result(
                self,
                CheckStatus::Warn,
                format!("could not sample eth0: {e}"),
                Some("check `ssh.controlmaster` — an unmeasurable link is not a stalled one"),
            ),
        }
    }
}

// ----------------------------------------------------------------------------------------
// the four deep SSH probes
// ----------------------------------------------------------------------------------------

/// How a remote probe's output becomes a verdict.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ProbeKind {
    /// `ps` — informational; the busiest process is the summary.
    Processes,
    /// `df -h` — `Warn` when the filesystem is nearly full.
    Disk,
    /// `ls -lh` over `*.gguf` / `*.incomplete`.
    Models,
    /// `tail /var/log/launch.log`.
    Log,
}

/// One remote command, run verbatim.
///
/// The command is passed to [`exec::ssh`] as **one** argument, which is the only way a
/// pipeline can survive: ssh hands whatever it is given to the remote login shell. No local
/// shell is involved anywhere in this crate.
struct RemoteProbe {
    id: &'static str,
    label: &'static str,
    argv: &'static str,
    timeout: Duration,
    kind: ProbeKind,
}

#[async_trait]
impl Check for RemoteProbe {
    fn id(&self) -> CheckId {
        CheckId::from(self.id)
    }
    fn label(&self) -> &str {
        self.label
    }
    fn needs(&self) -> CheckNeeds {
        CheckNeeds::Instance
    }
    async fn run(&self, ctx: &CheckCtx) -> CheckResult {
        let Some(t) = ext::<SshExt>(ctx, EXT_SSH) else {
            return result(
                self,
                CheckStatus::Skipped,
                "no ssh target for this instance",
                None,
            );
        };
        let out = match exec::ssh(&t.host, t.port, &t.opts, &[self.argv], self.timeout).await {
            Ok(out) => out,
            Err(e) => {
                return result(
                    self,
                    CheckStatus::Fail,
                    format!("ssh would not run: {e}"),
                    Some("check `ssh.binary`"),
                )
            }
        };
        if out.timed_out {
            return result(
                self,
                CheckStatus::Fail,
                format!("timed out after {} s", self.timeout.as_secs()),
                Some("the box may be wedged — `apexrouter vast log <id>`"),
            );
        }
        if out.status != 0 {
            return result(
                self,
                CheckStatus::Fail,
                format!("exited {}: {}", out.status, first_chars(&out.stderr, 160)),
                None,
            );
        }
        let (status, detail) = summarise(self.kind, &out.stdout);
        result(self, status, detail, None)
    }
}

/// Turn one probe's stdout into a verdict and a line a human can read.
fn summarise(kind: ProbeKind, stdout: &str) -> (CheckStatus, String) {
    match kind {
        ProbeKind::Processes => {
            let lines: Vec<&str> = stdout
                .lines()
                .skip(1)
                .filter(|l| !l.trim().is_empty())
                .collect();
            match lines.first() {
                Some(top) => (
                    CheckStatus::Pass,
                    format!(
                        "{} processes; busiest: {}",
                        lines.len(),
                        first_chars(top, 120)
                    ),
                ),
                None => (CheckStatus::Warn, "no processes listed".to_owned()),
            }
        }
        ProbeKind::Disk => match disk_use_percent(stdout) {
            Some((pct, line)) if pct >= 90 => (
                CheckStatus::Warn,
                format!("{pct}% full — a download will fail here: {line}"),
            ),
            Some((pct, line)) => (CheckStatus::Pass, format!("{pct}% used: {line}")),
            None => (
                CheckStatus::Warn,
                format!("could not read df output: {}", first_chars(stdout, 120)),
            ),
        },
        ProbeKind::Models => {
            let (complete, downloading) = model_file_counts(stdout);
            if complete == 0 && downloading == 0 {
                (
                    CheckStatus::Warn,
                    "no model files in /workspace/models yet".to_owned(),
                )
            } else {
                (
                    CheckStatus::Pass,
                    format!("{complete} complete, {downloading} downloading"),
                )
            }
        }
        ProbeKind::Log => match stdout.lines().rfind(|l| !l.trim().is_empty()) {
            Some(l) if l.contains("(no log yet)") => (
                CheckStatus::Warn,
                "no /var/log/launch.log yet — the container may still be pulling".to_owned(),
            ),
            Some(l) => (CheckStatus::Pass, first_chars(l, 160)),
            None => (CheckStatus::Warn, "the launch log is empty".to_owned()),
        },
    }
}

/// `df -h`'s use percentage and the row it came from.
fn disk_use_percent(stdout: &str) -> Option<(u32, String)> {
    for line in stdout.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let pct = fields
            .iter()
            .find_map(|f| f.strip_suffix('%').and_then(|n| n.parse::<u32>().ok()));
        if let Some(pct) = pct {
            return Some((pct, fields.join(" ")));
        }
    }
    None
}

/// The classification LocalRouter used, kept: an `.incomplete` file is a download in flight,
/// a `.gguf` is done.
fn model_file_counts(stdout: &str) -> (usize, usize) {
    let mut complete = 0;
    let mut downloading = 0;
    for line in stdout.lines() {
        if line.contains(".incomplete") {
            downloading += 1;
        } else if line.contains(".gguf") {
            complete += 1;
        }
    }
    (complete, downloading)
}

// ----------------------------------------------------------------------------------------
// shared
// ----------------------------------------------------------------------------------------

/// Build a result. `ms` is filled in by the runner, which is the only honest timer, and the
/// id is re-stamped by it too.
fn result(
    check: &dyn Check,
    status: CheckStatus,
    detail: impl Into<String>,
    fix: Option<&str>,
) -> CheckResult {
    CheckResult {
        id: check.id(),
        label: check.label().to_owned(),
        status,
        ms: 0,
        detail: detail.into(),
        fix: fix.map(str::to_owned),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_core::config::Config;
    use apexrouter_core::paths::Paths;
    use apexrouter_protocol::{
        ContainerLaunch, CostEstimate, InstanceId, LedgerState, OfferQuery, OfferSearchResult,
    };
    use std::collections::HashMap;
    use tokio::sync::mpsc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ---- fixtures ----------------------------------------------------------------------

    fn ctx() -> CheckCtx {
        CheckCtx {
            // Resolution reads the ambient environment and touches nothing; no test here
            // writes through `paths`.
            paths: Paths::resolve().expect("paths"),
            cfg: Arc::new(Config::default()),
            http: reqwest::Client::new(),
            rig: None,
            proxy_url: None,
            instance: None,
            ext: HashMap::new(),
        }
    }

    fn with_ext(mut ctx: CheckCtx, key: &str, v: Arc<dyn Any + Send + Sync>) -> CheckCtx {
        ctx.ext.insert(key.to_owned(), v);
        ctx
    }

    fn account(credit: f64) -> VastAccount {
        VastAccount {
            id: 1,
            credit,
            balance: None,
            can_pay: Some(true),
            has_billing: Some(true),
        }
    }

    fn instance(id: u64, dph: f64) -> VastInstance {
        let mut i: VastInstance =
            serde_json::from_value(serde_json::json!({ "id": id })).expect("instance");
        i.actual_status = Some("running".to_owned());
        i.dph_total = Some(dph);
        i
    }

    fn row(seq: u64, id: Option<u64>, state: LedgerState) -> LedgerRow {
        LedgerRow {
            seq,
            at_unix: 1_785_412_331,
            instance_id: id.map(InstanceId),
            state,
            offer_id: None,
            profile: None,
            gpu: None,
            num_gpus: None,
            dph: None,
            approved_max_dph: None,
            approval_source: None,
            destroyed_at_unix: None,
            est_cost: CostEstimate::Unknown,
            note: None,
        }
    }

    /// A vast client that answers from memory. **No test may reach vast.ai**, and nothing
    /// here can create, modify or destroy anything: the mutating methods are unreachable.
    struct FakeVast {
        account: VastAccount,
        instances: Vec<VastInstance>,
    }

    #[async_trait]
    impl VastApi for FakeVast {
        async fn account(&self) -> Result<VastAccount> {
            Ok(self.account.clone())
        }
        async fn search(&self, _q: &OfferQuery) -> Result<OfferSearchResult> {
            unreachable!("a check never searches offers")
        }
        async fn create(
            &self,
            _offer_id: u64,
            _launch: &ContainerLaunch,
            _label: &str,
        ) -> Result<InstanceId> {
            unreachable!("no test may create an instance")
        }
        async fn instances(&self) -> Result<Vec<VastInstance>> {
            Ok(self.instances.clone())
        }
        async fn instance(&self, id: InstanceId) -> Result<Option<VastInstance>> {
            Ok(self.instances.iter().find(|i| i.id == id).cloned())
        }
        async fn set_target_state(&self, _id: InstanceId, _running: bool) -> Result<()> {
            unreachable!("no test may park or wake an instance")
        }
        async fn destroy(&self, _id: InstanceId) -> Result<()> {
            unreachable!("no test may destroy an instance")
        }
        async fn logs(&self, _id: InstanceId, _tail: u32) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
        async fn exec(&self, _id: InstanceId, _cmd: &str) -> Result<String> {
            unreachable!("a check never execs through vast")
        }
    }

    fn vast_ext(account: VastAccount, instances: Vec<VastInstance>) -> Arc<dyn Any + Send + Sync> {
        Arc::new(VastExt(Arc::new(FakeVast { account, instances })))
    }

    // ---- the registries ----------------------------------------------------------------

    #[test]
    fn the_registries_do_not_collide_with_the_local_ones() {
        let local: BTreeSet<String> = apexrouter_core::checks::local_checks()
            .iter()
            .map(|c| c.id().to_string())
            .collect();
        let mine: Vec<String> = provider_checks()
            .iter()
            .chain(deep_checks().iter())
            .map(|c| c.id().to_string())
            .collect();
        for id in &mine {
            assert!(!local.contains(id), "{id} is already a core check");
        }
        // `net.stall` is deliberately in both of my lists; nothing else repeats.
        let unique: BTreeSet<&String> = mine.iter().collect();
        assert_eq!(unique.len(), mine.len() - 1);

        for want in [
            "ssh.binary",
            "ssh.controlmaster",
            "vast.credit",
            "vast.orphans",
            "together.ratelimits",
            "net.stall",
            "proxy.roundtrip",
            "endpoint.orphans",
        ] {
            assert!(mine.iter().any(|id| id == want), "{want} is missing");
        }
    }

    #[tokio::test]
    async fn the_deep_probes_skip_without_an_instance_and_touch_nothing() {
        let mut registry = apexrouter_core::checks::Registry::new();
        for c in deep_checks() {
            registry.register(c);
        }
        let (tx, _rx) = mpsc::channel(16);
        let out = registry.run(&ctx(), None, tx).await;
        assert_eq!(out.len(), 5);
        assert!(
            out.iter().all(|r| r.status == CheckStatus::Skipped),
            "{out:#?}"
        );
    }

    #[test]
    fn the_remote_commands_are_the_ones_localrouter_ran() {
        let ids: Vec<String> = deep_checks().iter().map(|c| c.id().to_string()).collect();
        assert_eq!(
            ids,
            vec![
                "instance.processes",
                "instance.disk",
                "instance.models",
                "instance.log",
                "net.stall"
            ]
        );
    }

    // ---- vast.credit -------------------------------------------------------------------

    #[tokio::test]
    async fn credit_reports_the_balance_and_the_runway() {
        let ctx = with_ext(
            ctx(),
            EXT_VAST,
            vast_ext(account(7.73), vec![instance(1, 3.34)]),
        );
        let r = VastCredit.run(&ctx).await;
        assert_eq!(r.status, CheckStatus::Pass);
        assert!(r.detail.contains("$7.73"), "{}", r.detail);
        assert!(r.detail.contains("2.3 h"), "{}", r.detail);
    }

    #[tokio::test]
    async fn no_vast_client_is_skipped_not_failed() {
        let ctx = ctx();
        for r in [VastCredit.run(&ctx).await, VastOrphans.run(&ctx).await] {
            assert_eq!(r.status, CheckStatus::Skipped);
            // A skip an operator can act on: the credential, not the injection.
            let fix = r
                .fix
                .expect("a skipped vast check names the credential command");
            assert!(fix.contains("provider set vast"), "{fix}");
        }
    }

    #[test]
    fn an_injected_client_is_the_one_the_checks_see_and_is_never_replaced() {
        let mut ext = ExtMap::new();
        insert_vast(
            &mut ext,
            Arc::new(FakeVast {
                account: account(1.0),
                instances: Vec::new(),
            }),
        );
        assert!(ext.contains_key(EXT_VAST));

        // A second insert must not evict the first: an explicitly injected client is a test
        // seam, and `provider_ext` running afterwards must not silently swap in a live one.
        let first = Arc::as_ptr(ext.get(EXT_VAST).expect("vast")) as *const ();
        insert_vast(
            &mut ext,
            Arc::new(FakeVast {
                account: account(2.0),
                instances: Vec::new(),
            }),
        );
        let second = Arc::as_ptr(ext.get(EXT_VAST).expect("vast")) as *const ();
        assert_eq!(first, second, "the first client survived");
    }

    #[tokio::test]
    async fn a_context_built_from_provider_ext_actually_runs_the_fleet_checks() {
        // HERMETICITY: `provider_ext` resolves a credential and builds a connection pool;
        // it dials nothing. The client it builds here points at a closed loopback port, and
        // the check below never runs against it — the assertion is that the *map* is filled,
        // which is the thing that was missing.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("vast_key"), "not-a-real-key\n").expect("key file");

        let mut cfg = Config::default();
        cfg.vast.base_url = "http://127.0.0.1:1/api/v0".to_owned();
        cfg.vast.api_key_file = dir.path().join("vast_key").display().to_string();

        let paths = Paths::resolve().expect("paths");
        let ext = provider_ext(&cfg, &paths);
        assert!(
            ext.contains_key(EXT_VAST),
            "a resolvable credential must yield a client"
        );

        // The point of the map: the two checks stop skipping. They will `Warn` here — the
        // base URL is a closed port, and "vast.ai did not answer" is the honest verdict —
        // but a `Warn` means the check *ran*, which is what could not happen before.
        let mut c = ctx();
        c.cfg = Arc::new(cfg);
        c.ext = ext;
        let r = VastCredit.run(&c).await;
        assert_eq!(r.status, CheckStatus::Warn, "{}", r.detail);
        assert!(r.detail.contains("did not answer"), "{}", r.detail);
    }

    #[test]
    fn credit_thresholds() {
        assert_eq!(credit_verdict(&account(7.73), None).0, CheckStatus::Pass);
        assert_eq!(
            credit_verdict(&account(7.73), Some(9.0)).0,
            CheckStatus::Warn,
            "under an hour of runway"
        );
        assert_eq!(credit_verdict(&account(0.40), None).0, CheckStatus::Warn);
        assert_eq!(credit_verdict(&account(0.0), None).0, CheckStatus::Fail);
        let mut broke = account(50.0);
        broke.can_pay = Some(false);
        assert_eq!(credit_verdict(&broke, None).0, CheckStatus::Fail);
    }

    // ---- vast.orphans ------------------------------------------------------------------

    #[test]
    fn an_instance_billing_with_no_ledger_row_is_a_failure() {
        let report = orphan_report(
            &[row(1, Some(100), LedgerState::Confirmed)],
            &[instance(100, 0.4), instance(200, 3.34)],
        );
        assert_eq!(report.untracked, vec![200]);
        assert!(report.stale.is_empty());
        let (status, detail, fix) = report.verdict();
        assert_eq!(status, CheckStatus::Fail);
        assert!(detail.contains("200"), "{detail}");
        assert!(fix.is_some());
    }

    #[test]
    fn a_ledger_row_for_a_gone_instance_is_only_a_warning() {
        let report = orphan_report(&[row(1, Some(100), LedgerState::Confirmed)], &[]);
        assert_eq!(report.stale, vec![100]);
        assert_eq!(report.verdict().0, CheckStatus::Warn);
    }

    #[test]
    fn a_reservation_that_was_never_confirmed_is_a_failure() {
        let report = orphan_report(&[row(1, None, LedgerState::Reserved)], &[]);
        assert_eq!(report.unlinked, 1);
        assert_eq!(report.verdict().0, CheckStatus::Fail);
    }

    #[test]
    fn agreement_is_a_pass() {
        let report = orphan_report(
            &[row(1, Some(100), LedgerState::Confirmed)],
            &[instance(100, 0.4)],
        );
        assert_eq!(report, OrphanReport::default());
        assert_eq!(report.verdict().0, CheckStatus::Pass);
    }

    // ---- together.ratelimits -----------------------------------------------------------

    #[tokio::test]
    async fn rate_limits_read_the_headers_off_a_live_answer() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-ratelimit-limit", "60")
                    .insert_header("x-ratelimit-remaining", "59")
                    .set_body_json(serde_json::json!([{"id": "m"}])),
            )
            .mount(&server)
            .await;

        let ctx = with_ext(
            ctx(),
            EXT_TOGETHER,
            Arc::new(TogetherExt {
                base_url: server.uri(),
                cred: Some(Secret::new("sk-test".to_owned())),
            }),
        );
        let r = TogetherRateLimits.run(&ctx).await;
        assert_eq!(r.status, CheckStatus::Pass);
        assert_eq!(r.detail, "59 of 60 remaining");
    }

    #[tokio::test]
    async fn a_429_reads_reset_and_does_not_rely_on_remaining() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(429).insert_header("x-ratelimit-reset", "1785412391"),
            )
            .mount(&server)
            .await;

        let ctx = with_ext(
            ctx(),
            EXT_TOGETHER,
            Arc::new(TogetherExt {
                base_url: server.uri(),
                cred: None,
            }),
        );
        let r = TogetherRateLimits.run(&ctx).await;
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(r.detail.contains("1785412391"), "{}", r.detail);
    }

    #[test]
    fn rate_limit_verdicts() {
        let (status, detail, _) = ratelimit_verdict(200, None, None, None);
        assert_eq!(status, CheckStatus::Pass);
        assert!(detail.contains("no rate-limit headers"), "{detail}");
        assert_eq!(
            ratelimit_verdict(401, None, None, None).0,
            CheckStatus::Fail
        );
        assert_eq!(
            ratelimit_verdict(503, None, None, None).0,
            CheckStatus::Warn
        );
    }

    #[tokio::test]
    async fn an_unconfigured_together_is_skipped() {
        let mut cfg = Config::default();
        cfg.providers.clear();
        let mut c = ctx();
        c.cfg = Arc::new(cfg);
        assert_eq!(
            TogetherRateLimits.run(&c).await.status,
            CheckStatus::Skipped
        );
    }

    // ---- proxy.roundtrip ---------------------------------------------------------------

    #[tokio::test]
    async fn the_proxy_roundtrip_hits_the_data_plane() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": []})))
            .mount(&server)
            .await;

        let mut cfg = Config::default();
        cfg.server.proxy_bind = server.address().to_string();
        let mut c = ctx();
        c.cfg = Arc::new(cfg);

        let r = ProxyRoundtrip.run(&c).await;
        assert_eq!(r.status, CheckStatus::Pass, "{}", r.detail);
    }

    #[test]
    fn a_wildcard_bind_is_reached_on_loopback() {
        assert_eq!(proxy_url("0.0.0.0:8888"), "http://127.0.0.1:8888");
        assert_eq!(proxy_url("127.0.0.1:8888"), "http://127.0.0.1:8888");
        assert_eq!(proxy_url("[::]:8888"), "http://127.0.0.1:8888");
    }

    // ---- ssh ---------------------------------------------------------------------------

    #[tokio::test]
    async fn the_control_master_check_skips_without_a_target() {
        assert_eq!(
            SshControlMaster.run(&ctx()).await.status,
            CheckStatus::Skipped
        );
    }

    #[tokio::test]
    async fn a_missing_control_socket_is_a_warning_not_a_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = with_ext(
            ctx(),
            EXT_SSH,
            Arc::new(SshExt {
                // Never dialled: the socket is absent, so the check answers before ssh runs.
                host: "root@127.0.0.1".to_owned(),
                port: 22,
                opts: SshOpts {
                    known_hosts: dir.path().join("known_hosts"),
                    control_path: Some(dir.path().join("cm-1")),
                    connect_timeout: 5,
                    extra: Vec::new(),
                },
            }),
        );
        let r = SshControlMaster.run(&ctx).await;
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(r.fix.is_some());
    }

    #[tokio::test]
    async fn the_ssh_binary_check_answers_on_this_machine() {
        let r = SshBinary.run(&ctx()).await;
        // Installed or not, the check must have an opinion — and a fix when it is bad news.
        assert!(matches!(r.status, CheckStatus::Pass | CheckStatus::Fail));
        if r.status == CheckStatus::Fail {
            assert!(r.fix.is_some());
        }
    }

    // ---- the RX sample -----------------------------------------------------------------

    #[test]
    fn the_stall_thresholds_are_the_measured_ones() {
        assert_eq!(download_health(12, 4).verdict, StallVerdict::Stalled);
        assert_eq!(download_health(999, 4).verdict, StallVerdict::Stalled);
        // 1 KB in 4 s is not stalled by the rule, but it is nobody's idea of fast.
        assert_eq!(download_health(1000, 4).verdict, StallVerdict::Slow);
        // 100 MB in 4 s = 200 Mbps.
        let fast = download_health(100_000_000, 4);
        assert_eq!(fast.verdict, StallVerdict::Active);
        assert!((fast.mbps - 200.0).abs() < 0.001, "{}", fast.mbps);
        // A zero window must not divide by zero.
        assert_eq!(download_health(0, 0).verdict, StallVerdict::Stalled);
    }

    #[test]
    fn a_failed_sample_is_not_a_zero() {
        let refused = Output {
            status: 255,
            stdout: String::new(),
            stderr: "ssh: connect to host port 22: Connection refused".to_owned(),
            timed_out: false,
        };
        assert_eq!(parse_rx(&refused), None);

        let timed_out = Output {
            status: 0,
            stdout: "412345".to_owned(),
            stderr: String::new(),
            timed_out: true,
        };
        assert_eq!(parse_rx(&timed_out), None);

        let good = Output {
            status: 0,
            stdout: "Warning: Permanently added 'ssh5.vast.ai'\n412345\n".to_owned(),
            stderr: String::new(),
            timed_out: false,
        };
        assert_eq!(parse_rx(&good), Some(412_345));
    }

    // ---- deep-probe summaries ----------------------------------------------------------

    #[test]
    fn df_output_is_read_for_its_use_percentage() {
        let df = "Filesystem      Size  Used Avail Use% Mounted on\n\
                  overlay         196G   88G  108G  45% /workspace\n";
        let (status, detail) = summarise(ProbeKind::Disk, df);
        assert_eq!(status, CheckStatus::Pass);
        assert!(detail.starts_with("45% used"), "{detail}");

        let full = "Filesystem      Size  Used Avail Use% Mounted on\n\
                    overlay         196G  190G  1.0G  99% /workspace\n";
        assert_eq!(summarise(ProbeKind::Disk, full).0, CheckStatus::Warn);
    }

    #[test]
    fn incomplete_files_are_counted_as_downloading() {
        let ls = "-rw-r--r-- 1 root root 4.7G Jul 30 12:00 /workspace/models/a-00001.gguf\n\
                  -rw-r--r-- 1 root root 4.7G Jul 30 12:00 /workspace/models/a-00002.gguf\n\
                  -rw-r--r-- 1 root root 1.2G Jul 30 12:01 /workspace/models/a-3.gguf.incomplete\n";
        let (status, detail) = summarise(ProbeKind::Models, ls);
        assert_eq!(status, CheckStatus::Pass);
        assert_eq!(detail, "2 complete, 1 downloading");
        assert_eq!(summarise(ProbeKind::Models, "").0, CheckStatus::Warn);
    }

    #[test]
    fn an_absent_launch_log_is_a_warning() {
        assert_eq!(
            summarise(ProbeKind::Log, "(no log yet)\n").0,
            CheckStatus::Warn
        );
        let (status, detail) = summarise(ProbeKind::Log, "loading model\nserver listening\n");
        assert_eq!(status, CheckStatus::Pass);
        assert_eq!(detail, "server listening");
    }

    #[test]
    fn the_process_probe_summarises_the_busiest() {
        let ps = "  PID     ELAPSED %CPU %MEM CMD\n \
                  1234    01:02:03 98.0  4.1 /app/llama-server -m /workspace/models/a.gguf\n \
                  1235    01:02:03  0.1  0.1 bash /app/launch.sh\n";
        let (status, detail) = summarise(ProbeKind::Processes, ps);
        assert_eq!(status, CheckStatus::Pass);
        assert!(detail.starts_with("2 processes"), "{detail}");
        assert!(detail.contains("llama-server"), "{detail}");
    }
}
