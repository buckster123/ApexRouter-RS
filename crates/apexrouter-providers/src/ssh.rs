//! OWNER: unit P-05 (providers/src/ssh.rs). Do not edit outside that unit.
//!
//! The tunnel supervisor. It **owns the `ssh` `Child`** — `pgrep` appears nowhere, because
//! `pgrep ssh` can kill an unrelated connection.
//!
//! The exact flag set (`ARCHITECTURE.md` §4.9):
//! `-N -L <local>:127.0.0.1:8000 -p <port> root@<host> -o ExitOnForwardFailure=yes
//! -o ServerAliveInterval=30 -o ServerAliveCountMax=3 -o StrictHostKeyChecking=accept-new
//! -o UserKnownHostsFile=$STATE/ssh/known_hosts -o ControlMaster=auto
//! -o ControlPath=$STATE/ssh/cm-<instance-id> -o ControlPersist=5m`.
//!
//! Two things the source proposals dropped and this does not:
//!
//! 1. **A reconnect supervisor.** `ExitOnForwardFailure` makes ssh *exit* on a dead link; it
//!    does not re-establish it. A laptop wifi blip must not leave a $3.34/hr box unreachable
//!    until a human notices. Bounded backoff 1 s → ×2 → cap 30 s, then a `Serious` alert.
//! 2. **Teardown does `ssh -O exit` and unlinks the ControlPath.** Killing the `-N -L` child
//!    leaves the ControlMaster alive for `ControlPersist` minutes against a destroyed box.
//!
//! Ownership is by **identity, not by handle**: `core::proc::spawn_detached` gives the ssh
//! child its own session so a `systemctl --user restart apexrouterd` does not drop every
//! forward, and drops the `std::process::Child` on purpose. What we keep is the
//! [`ProcFacts`] triple (pid ∧ `start_time_ticks` ∧ `boot_id`) plus the argv hash, which is
//! what makes re-adoption after a daemon restart safe and pid reuse detectable. Nothing is
//! ever signalled without re-verifying that triple first.

use apexrouter_core::config::Config;
use apexrouter_core::error::{Error, Result};
use apexrouter_core::exec;
use apexrouter_core::proc::{self, Liveness, Signal, SpawnRequest};
use apexrouter_core::store::Store;
use apexrouter_core::Paths;
use apexrouter_protocol::{AlertLevel, Event, InstanceId, LogSource, ProcFacts};
use apexrouter_protocol::{TunnelSpec, TunnelStatus};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

use crate::local::supervisor::{log_tail, reap};

/// The address the remote end of the forward is dialled on, **inside** the container.
/// Tunnel-only posture: `HOST=127.0.0.1` is forced at create time, so the container's own
/// loopback is the only thing listening.
const REMOTE_BIND: &str = "127.0.0.1";

/// How long a `SIGTERM` gets before the `SIGKILL`.
const TERM_WAIT: Duration = Duration::from_millis(1_500);
/// How long the `SIGKILL` gets before we stop waiting.
const KILL_WAIT: Duration = Duration::from_millis(1_500);
/// How long `ssh -O exit` gets. It talks to a unix socket, so this is generous.
const CONTROL_EXIT_TIMEOUT: Duration = Duration::from_secs(10);
/// Default budget for "the forward is actually listening".
const DEFAULT_UP_TIMEOUT: Duration = Duration::from_secs(20);
/// Default liveness poll for [`TunnelSupervisor::supervise`].
const DEFAULT_POLL: Duration = Duration::from_secs(2);
/// First reconnect delay. Doubles per consecutive failure.
const DEFAULT_BACKOFF_BASE: Duration = Duration::from_secs(1);
/// Reconnect delay ceiling (`ARCHITECTURE.md` §4.9).
const BACKOFF_CAP: Duration = Duration::from_secs(30);
/// How often the up-gate re-checks the local port.
const UP_POLL: Duration = Duration::from_millis(50);
/// The restart-budget window.
const RESTART_WINDOW: Duration = Duration::from_secs(3_600);
/// Lines of the ssh log carried on a failure.
const FAIL_TAIL_LINES: usize = 10;

// ---------------------------------------------------------------------------------------
// argv
// ---------------------------------------------------------------------------------------

/// The exact argv handed to `ssh`, minus `argv[0]`, split out so it can be asserted on
/// without a network.
///
/// This is the flag set of `ARCHITECTURE.md` §4.9 and nothing else. The dedicated
/// `known_hosts` exists because vast recycles `sshN.vast.ai` hostnames; `ControlMaster` is a
/// measured ~500 ms → RTT win for agentic tool loops, and is what `ssh -O exit` later talks
/// to.
///
/// `ssh_host` may already carry a user (`root@ssh5.vast.ai`), in which case it is used
/// verbatim rather than being prefixed twice.
pub fn tunnel_argv(spec: &TunnelSpec, known_hosts: &Path, control_path: &Path) -> Vec<String> {
    vec![
        "-N".to_owned(),
        "-L".to_owned(),
        forward_token(spec),
        "-p".to_owned(),
        spec.ssh_port.to_string(),
        destination(&spec.ssh_host),
        "-o".to_owned(),
        "ExitOnForwardFailure=yes".to_owned(),
        "-o".to_owned(),
        "ServerAliveInterval=30".to_owned(),
        "-o".to_owned(),
        "ServerAliveCountMax=3".to_owned(),
        "-o".to_owned(),
        "StrictHostKeyChecking=accept-new".to_owned(),
        "-o".to_owned(),
        format!("UserKnownHostsFile={}", known_hosts.display()),
        "-o".to_owned(),
        "ControlMaster=auto".to_owned(),
        "-o".to_owned(),
        format!("ControlPath={}", control_path.display()),
        "-o".to_owned(),
        "ControlPersist=5m".to_owned(),
    ]
}

/// `<local>:127.0.0.1:<remote>` — the `-L` argument, and the token adoption looks for in a
/// live `/proc/<pid>/cmdline`.
fn forward_token(spec: &TunnelSpec) -> String {
    format!(
        "{}:{REMOTE_BIND}:{}",
        spec.local_port,
        spec.remote_port_or_default()
    )
}

/// `root@<host>`, unless the host already names a user.
fn destination(host: &str) -> String {
    if host.contains('@') {
        host.to_owned()
    } else {
        format!("root@{host}")
    }
}

/// Small extension so a record written with `remote_port: 0` still means "the container's
/// llama-server", which is 8000 in every published image.
trait RemotePort {
    /// The remote port, defaulting to 8000.
    fn remote_port_or_default(&self) -> u16;
}

impl RemotePort for TunnelSpec {
    fn remote_port_or_default(&self) -> u16 {
        if self.remote_port == 0 {
            8000
        } else {
            self.remote_port
        }
    }
}

// ---------------------------------------------------------------------------------------
// The supervisor
// ---------------------------------------------------------------------------------------

/// Per-instance reconnect bookkeeping. Lives inside [`TunnelSupervisor::supervise`]'s loop,
/// because it describes the *attempt*, not the tunnel: everything durable is in
/// `$STATE/tunnels.json`.
#[derive(Debug, Default)]
struct Attempt {
    /// Consecutive failures, which is what the backoff doubles on.
    failures: u32,
    /// Earliest instant at which the next attempt may run.
    next_at: Option<Instant>,
    /// When each attempt in the rolling hour happened.
    window: VecDeque<Instant>,
    /// The budget is spent and the `Serious` alert has been raised once.
    exhausted: bool,
}

/// Owns every `ssh` child and persists `TunnelStatus` so a daemon restart re-adopts rather
/// than colliding on the local port.
pub struct TunnelSupervisor {
    paths: Paths,
    store: Store,
    cfg: RwLock<Config>,
    tx: broadcast::Sender<Event>,
    /// The resolved `ssh` binary. A test points this at a stand-in.
    ssh: PathBuf,
    /// Serialises the read-modify-write of `$STATE/tunnels.json`. Never held across an
    /// `.await`.
    io: Mutex<()>,
    /// Liveness poll interval for [`TunnelSupervisor::supervise`].
    poll_interval: Duration,
    /// First reconnect delay; doubles per consecutive failure, capped at 30 s.
    backoff_base: Duration,
    /// How long a fresh forward gets to bind its local port.
    up_timeout: Duration,
    /// Set by [`TunnelSupervisor::stop`], read by the supervise loop.
    stopped: AtomicBool,
}

impl TunnelSupervisor {
    /// Build a supervisor over a resolved path set.
    ///
    /// The `Store` is derived from `paths` rather than passed in, because a supervisor whose
    /// state root disagreed with the daemon's would write tunnel records nobody re-adopts.
    /// Owns no children until [`TunnelSupervisor::up`] is called, and — like the local
    /// supervisor — does not kill them when it exits either: `[vast] tunnels_on_shutdown`
    /// decides that, and its default is `adopt`.
    pub fn new(paths: Paths, cfg: Config, tx: broadcast::Sender<Event>) -> Self {
        TunnelSupervisor {
            store: Store::new(paths.clone()),
            paths,
            cfg: RwLock::new(cfg),
            tx,
            ssh: resolve_ssh(),
            io: Mutex::new(()),
            poll_interval: DEFAULT_POLL,
            backoff_base: DEFAULT_BACKOFF_BASE,
            up_timeout: DEFAULT_UP_TIMEOUT,
            stopped: AtomicBool::new(false),
        }
    }

    /// Use a specific `ssh` binary instead of the first one on `PATH`.
    #[must_use]
    pub fn with_ssh_binary(mut self, ssh: PathBuf) -> Self {
        self.ssh = ssh;
        self
    }

    /// Override the three timings: the liveness poll, the first backoff step, and how long a
    /// fresh forward gets to bind its local port.
    #[must_use]
    pub fn with_timings(mut self, poll: Duration, backoff_base: Duration, up: Duration) -> Self {
        self.poll_interval = poll;
        self.backoff_base = backoff_base;
        self.up_timeout = up;
        self
    }

    /// Replace the configuration after a `SIGHUP` reload. Running forwards are untouched.
    pub fn set_config(&self, cfg: Config) {
        match self.cfg.write() {
            Ok(mut slot) => *slot = cfg,
            Err(poisoned) => *poisoned.into_inner() = cfg,
        }
    }

    /// Every persisted tunnel record, as `GET /v1/tunnels` renders it.
    ///
    /// # Errors
    /// Propagates an unreadable or unparsable `$STATE/tunnels.json`.
    pub fn status(&self) -> Result<Vec<TunnelStatus>> {
        self.store.load_tunnels()
    }

    /// First free local port in `[vast] tunnel_port_range`, skipping ports already promised
    /// to a persisted tunnel.
    ///
    /// # Errors
    /// [`Error::PortInUse`] naming the pool when every port in it is taken.
    pub fn allocate_port(&self) -> Result<u16> {
        let records = self.store.load_tunnels()?;
        self.pick_port(&records)
    }

    /// Bring one forward up.
    ///
    /// Idempotent: an instance whose recorded child is still alive and still forwarding the
    /// same port gets its existing record back, which is what stops a re-`up` after a daemon
    /// restart from colliding on the local port. `local_port: 0` allocates from
    /// `[vast] tunnel_port_range`.
    ///
    /// # Errors
    /// [`Error::PortInUse`] when somebody else — one of ours or a stranger — holds the local
    /// port, [`Error::Timeout`] when ssh never bound it, and [`Error::Other`] carrying the
    /// log tail when ssh exited instead.
    pub async fn up(&self, spec: TunnelSpec) -> Result<TunnelStatus> {
        self.paths.ensure_layout()?;
        let mut spec = spec;
        let records = self.store.load_tunnels()?;

        // Already ours and already up? Then this call is a no-op, which is what stops a
        // re-`up` after a restart from fighting its own child for the local port.
        let ours = records
            .iter()
            .find(|t| t.spec.instance_id == spec.instance_id)
            .cloned();
        let live = ours.as_ref().is_some_and(|t| {
            t.proc
                .as_ref()
                .is_some_and(|f| is_live(f) && confirms_forward(f.pid, &t.spec) != Some(false))
        });
        if live {
            if let Some(existing) = ours {
                return Ok(existing);
            }
        }

        if spec.local_port == 0 {
            spec.local_port = self.pick_port(&records)?;
        }

        // Somebody else's record on the same local port is a collision we can name.
        let holder = records
            .iter()
            .find(|t| {
                t.spec.local_port == spec.local_port
                    && t.spec.instance_id != spec.instance_id
                    && t.proc.as_ref().is_some_and(is_live)
            })
            .map(|t| t.spec.instance_id);
        if let Some(id) = holder {
            return Err(Error::PortInUse {
                port: spec.local_port,
                held_by: Some(format!("tunnel for instance {id}")),
            });
        }
        if !proc::port_free(spec.local_port) {
            return Err(Error::PortInUse {
                port: spec.local_port,
                held_by: None,
            });
        }

        let control_path = self.paths.control_path(spec.instance_id);
        // Our previous child is gone, so any master it left behind is against a link that
        // no longer works. Close it and unlink the socket before spawning, or the new ssh
        // multiplexes onto a corpse.
        if control_path.exists() {
            let old = ours.as_ref().map_or(&spec, |t| &t.spec);
            self.control_exit(old, &control_path).await;
            unlink(&control_path);
        }
        let argv = tunnel_argv(&spec, &self.paths.known_hosts(), &control_path);
        let log = self.log_file(spec.instance_id);
        let child = proc::spawn_detached(SpawnRequest {
            program: &self.ssh,
            args: &argv,
            env: &[],
            cwd: self.paths.state(),
            log: &log,
            setsid: true,
        })?;

        self.await_forward(&spec, &child.facts, &log).await?;

        let carried = records
            .iter()
            .find(|t| t.spec.instance_id == spec.instance_id)
            .map(|t| t.restarts)
            .unwrap_or(0);
        let status = TunnelStatus {
            spec,
            up: true,
            proc: Some(child.facts),
            since_unix: Some(now_unix()),
            restarts: carried,
            last_error: None,
        };
        self.put(status.clone())?;
        self.say(
            status.spec.instance_id,
            format!(
                "tunnel up: 127.0.0.1:{} -> {}:{} (pid {})",
                status.spec.local_port,
                status.spec.ssh_host,
                status.spec.remote_port_or_default(),
                child.pid
            ),
        );
        Ok(status)
    }

    /// Kill the child, run `ssh -O exit`, and unlink the ControlPath.
    ///
    /// All three, in that order, and none of them conditional on the previous one: killing
    /// the `-N -L` child on its own leaves the ControlMaster alive for `ControlPersist`
    /// minutes against a box that may already be destroyed, and a stale socket file makes
    /// the *next* `up` reuse a master that cannot connect.
    ///
    /// Removing an unknown instance is not an error — `down` is what you run when you are
    /// not sure.
    ///
    /// # Errors
    /// Propagates a failure to rewrite `$STATE/tunnels.json`.
    pub async fn down(&self, id: InstanceId) -> Result<()> {
        let records = self.store.load_tunnels()?;
        let record = records.iter().find(|t| t.spec.instance_id == id).cloned();

        if let Some(facts) = record.as_ref().and_then(|r| r.proc.clone()) {
            self.terminate(&facts).await;
        }

        let control_path = self.paths.control_path(id);
        if let Some(rec) = record.as_ref() {
            self.control_exit(&rec.spec, &control_path).await;
        }
        unlink(&control_path);

        self.forget(id)?;
        if record.is_some() {
            self.say(id, format!("tunnel down: instance {id}"));
        }
        Ok(())
    }

    /// Re-adopt persisted tunnels at startup.
    ///
    /// A record whose identity triple still matches a live process — and whose
    /// `/proc/<pid>/cmdline` still carries our `-L` forward — is adopted as `up`. Anything
    /// else is marked down and left for [`TunnelSupervisor::supervise`] to reconnect; it is
    /// never signalled, because a pid that is no longer ours belongs to somebody else.
    ///
    /// # Errors
    /// Propagates an unreadable `$STATE/tunnels.json`.
    pub async fn adopt_all(&self) -> Result<Vec<TunnelStatus>> {
        let records = self.store.load_tunnels()?;
        let mut out = Vec::with_capacity(records.len());

        for mut rec in records {
            let verdict = match rec.proc.as_ref() {
                None => Some("no recorded process".to_owned()),
                Some(facts) if !is_live(facts) => {
                    Some(format!("pid {} is no longer the ssh we spawned", facts.pid))
                }
                Some(facts) => match confirms_forward(facts.pid, &rec.spec) {
                    Some(false) => Some(format!(
                        "pid {} no longer forwards {}",
                        facts.pid,
                        forward_token(&rec.spec)
                    )),
                    _ => None,
                },
            };

            match verdict {
                None => {
                    rec.up = true;
                    rec.last_error = None;
                }
                Some(why) => {
                    rec.up = false;
                    rec.last_error = Some(why);
                }
            }
            out.push(rec);
        }

        self.save_all(&out)?;
        Ok(out)
    }

    /// The bounded-retry reconnect loop. Runs for the daemon's lifetime.
    ///
    /// Every persisted record is a *desire*: `down` removes the row, so anything still in
    /// `$STATE/tunnels.json` whose child has gone is reconnected — 1 s → ×2 → cap 30 s,
    /// `[supervisor] max_restarts_per_hour` attempts in a rolling hour, then one `Serious`
    /// alert and no more retries until somebody brings it up by hand.
    pub async fn supervise(self: Arc<Self>) {
        let mut state: HashMap<u64, Attempt> = HashMap::new();
        while !self.stopped.load(Ordering::Acquire) {
            if let Err(e) = self.tick(&mut state).await {
                tracing::warn!(error = %e, "tunnel supervisor tick failed");
            }
            tokio::time::sleep(self.poll_interval).await;
        }
    }

    /// Ask [`TunnelSupervisor::supervise`] to return after its current tick.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
    }

    /// Honour `[vast] tunnels_on_shutdown` on the way out.
    ///
    /// `adopt` (the default) leaves every forward up, so a `systemctl --user restart` does
    /// not make a paid box unreachable; `down` tears them all down. Instances themselves are
    /// **never** touched here — a crash must not delete a paid box.
    ///
    /// # Errors
    /// Propagates a failure to rewrite `$STATE/tunnels.json`.
    pub async fn on_shutdown(&self) -> Result<()> {
        self.stop();
        if !self.cfg_read(|c| c.vast.tunnels_on_shutdown.eq_ignore_ascii_case("down")) {
            return Ok(());
        }
        for rec in self.store.load_tunnels()? {
            self.down(rec.spec.instance_id).await?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------------------
    // Internals
    // -----------------------------------------------------------------------------------

    /// One pass over every persisted record.
    async fn tick(&self, state: &mut HashMap<u64, Attempt>) -> Result<()> {
        let records = self.store.load_tunnels()?;
        let known: HashSet<u64> = records.iter().map(|r| r.spec.instance_id.0).collect();
        state.retain(|id, _| known.contains(id));

        for rec in records {
            let id = rec.spec.instance_id;
            if rec.proc.as_ref().is_some_and(is_live) {
                state.remove(&id.0);
                if !rec.up {
                    self.update(id, |t| {
                        t.up = true;
                        t.last_error = None;
                    })?;
                }
                continue;
            }

            let budget = self.cfg_read(|c| c.supervisor.max_restarts_per_hour);
            let now = Instant::now();

            // What this pass does, decided while the entry is borrowed and acted on after.
            let attempt = {
                let entry = state.entry(id.0).or_default();
                if entry.exhausted || entry.next_at.is_some_and(|at| now < at) {
                    continue;
                }
                entry
                    .window
                    .retain(|at| now.duration_since(*at) < RESTART_WINDOW);
                if u32::try_from(entry.window.len()).unwrap_or(u32::MAX) >= budget {
                    entry.exhausted = true;
                    None
                } else {
                    entry.window.push_back(now);
                    Some(entry.failures.saturating_add(1))
                }
            };

            let Some(attempt) = attempt else {
                let why = format!(
                    "tunnel for instance {id} has exhausted its budget of \
                     {budget} reconnects per hour and is staying down"
                );
                self.update(id, |t| {
                    t.up = false;
                    t.last_error = Some(why.clone());
                })?;
                self.alert(
                    AlertLevel::Serious,
                    format!("tunnel.{id}.restarts"),
                    why,
                    Some("tunnel up".to_owned()),
                );
                continue;
            };

            self.say(
                id,
                format!("tunnel for instance {id} is down; reconnect attempt {attempt}"),
            );
            let outcome = self.up(rec.spec.clone()).await;
            let entry = state.entry(id.0).or_default();
            match outcome {
                Ok(_) => {
                    entry.failures = 0;
                    entry.next_at = None;
                    self.update(id, |t| t.restarts = t.restarts.saturating_add(1))?;
                    self.say(id, format!("tunnel for instance {id} reconnected"));
                }
                Err(e) => {
                    entry.failures = attempt;
                    let delay = self.backoff(attempt);
                    entry.next_at = Some(Instant::now() + delay);
                    let why = e.to_string();
                    self.update(id, |t| {
                        t.up = false;
                        t.last_error = Some(why.clone());
                    })?;
                    self.say(
                        id,
                        format!("tunnel reconnect failed ({why}); retrying in {delay:?}"),
                    );
                }
            }
        }
        Ok(())
    }

    /// 1 s → ×2 → cap 30 s.
    fn backoff(&self, failures: u32) -> Duration {
        let shift = failures.saturating_sub(1).min(16);
        self.backoff_base
            .saturating_mul(1u32 << shift)
            .min(BACKOFF_CAP)
    }

    /// Wait until the forward is actually listening, the child dies, or the budget expires.
    async fn await_forward(&self, spec: &TunnelSpec, facts: &ProcFacts, log: &Path) -> Result<()> {
        let deadline = Instant::now() + self.up_timeout;
        loop {
            if !proc::port_free(spec.local_port) {
                return Ok(());
            }
            if !is_live(facts) {
                let tail = log_tail(log, FAIL_TAIL_LINES).join("\n");
                return Err(Error::Other(format!(
                    "ssh exited before the forward on 127.0.0.1:{} came up{}{tail}",
                    spec.local_port,
                    if tail.is_empty() { "" } else { ": " }
                )));
            }
            if Instant::now() >= deadline {
                self.terminate(facts).await;
                return Err(Error::Timeout {
                    ms: u64::try_from(self.up_timeout.as_millis()).unwrap_or(u64::MAX),
                });
            }
            tokio::time::sleep(UP_POLL).await;
        }
    }

    /// `SIGTERM`, then `SIGKILL`, each re-verifying identity first, then reap.
    async fn terminate(&self, facts: &ProcFacts) {
        if matches!(proc::liveness(facts), Liveness::Dead) {
            return;
        }
        if let Err(e) = proc::signal_verified(facts, Signal::Term) {
            tracing::debug!(pid = facts.pid, error = %e, "ssh child was already gone");
        }
        if self.wait_gone(facts, TERM_WAIT).await {
            reap(facts.pid);
            return;
        }
        if let Err(e) = proc::signal_verified(facts, Signal::Kill) {
            tracing::debug!(pid = facts.pid, error = %e, "ssh child was already gone");
        }
        self.wait_gone(facts, KILL_WAIT).await;
        reap(facts.pid);
    }

    /// `true` once the process these facts describe is no longer running.
    async fn wait_gone(&self, facts: &ProcFacts, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        loop {
            match proc::liveness(facts) {
                Liveness::Dead | Liveness::Zombie => return true,
                _ => {}
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// `ssh -O exit`, so the ControlMaster does not outlive the forward.
    async fn control_exit(&self, spec: &TunnelSpec, control_path: &Path) {
        if !control_path.exists() {
            return;
        }
        let port = spec.ssh_port.to_string();
        let control = format!("ControlPath={}", control_path.display());
        let dest = destination(&spec.ssh_host);
        let argv = ["-O", "exit", "-o", control.as_str(), "-p", &port, &dest];
        match exec::run(&self.ssh, &argv, CONTROL_EXIT_TIMEOUT).await {
            Ok(out) if out.status == 0 => {}
            Ok(out) => tracing::debug!(
                status = out.status,
                stderr = %out.stderr.trim(),
                "ssh -O exit reported no master to close"
            ),
            Err(e) => tracing::warn!(error = %e, "could not run ssh -O exit"),
        }
    }

    /// First port in `[vast] tunnel_port_range` that is neither bound nor already promised.
    fn pick_port(&self, records: &[TunnelStatus]) -> Result<u16> {
        let range = self.cfg_read(|c| c.vast.tunnel_port_range);
        let taken: Vec<u16> = records.iter().map(|t| t.spec.local_port).collect();
        proc::alloc_port(range, &taken).ok_or(Error::PortInUse {
            port: range.0,
            held_by: Some(format!(
                "the tunnel port pool {}-{} is full",
                range.0, range.1
            )),
        })
    }

    /// `$STATE/logs/tunnel-<instance-id>.log`.
    fn log_file(&self, id: InstanceId) -> PathBuf {
        self.paths.logs_dir().join(format!("tunnel-{}.log", id.0))
    }

    /// Read something out of the config without holding the lock any longer than that.
    fn cfg_read<T>(&self, f: impl FnOnce(&Config) -> T) -> T {
        match self.cfg.read() {
            Ok(cfg) => f(&cfg),
            Err(poisoned) => f(&poisoned.into_inner()),
        }
    }

    /// Insert or replace one record.
    fn put(&self, status: TunnelStatus) -> Result<()> {
        let _guard = self.io_guard();
        let mut all = self.store.load_tunnels()?;
        match all
            .iter_mut()
            .find(|t| t.spec.instance_id == status.spec.instance_id)
        {
            Some(slot) => *slot = status,
            None => all.push(status),
        }
        self.store.save_tunnels(&all)
    }

    /// Mutate one record in place, if it is still there.
    fn update(&self, id: InstanceId, f: impl FnOnce(&mut TunnelStatus)) -> Result<()> {
        let _guard = self.io_guard();
        let mut all = self.store.load_tunnels()?;
        let Some(slot) = all.iter_mut().find(|t| t.spec.instance_id == id) else {
            return Ok(());
        };
        f(slot);
        self.store.save_tunnels(&all)
    }

    /// Drop one record.
    fn forget(&self, id: InstanceId) -> Result<()> {
        let _guard = self.io_guard();
        let mut all = self.store.load_tunnels()?;
        let before = all.len();
        all.retain(|t| t.spec.instance_id != id);
        if all.len() == before {
            return Ok(());
        }
        self.store.save_tunnels(&all)
    }

    /// Replace the whole file, as `adopt_all` does once.
    fn save_all(&self, all: &[TunnelStatus]) -> Result<()> {
        let _guard = self.io_guard();
        self.store.save_tunnels(all)
    }

    /// The state-file mutex. A poisoned lock still describes a valid file: every writer
    /// completes its read-modify-write inside one closure.
    fn io_guard(&self) -> std::sync::MutexGuard<'_, ()> {
        match self.io.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Broadcast one line against the instance the tunnel serves.
    fn say(&self, id: InstanceId, line: String) {
        tracing::info!(instance = id.0, "{line}");
        let _ = self.tx.send(Event::LogLine {
            source: LogSource::Instance { id },
            line,
        });
    }

    /// Broadcast an alert. The id is stable so repeats coalesce instead of stacking.
    fn alert(&self, level: AlertLevel, id: String, message: String, action: Option<String>) {
        tracing::warn!(alert = %id, "{message}");
        let _ = self.tx.send(Event::Alert {
            level,
            message,
            action,
            id,
        });
    }
}

// ---------------------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------------------

/// Is the process these facts describe still running?
///
/// A zombie is reaped here and reported dead: it is a child we spawned that has exited and
/// is waiting for our `waitpid`. [`Liveness::Unknown`] — `EPERM` under `hidepid`, an
/// unparsable `stat` — is deliberately reported **alive**: the expensive mistake is starting
/// a second ssh against the same local port, not leaving a live one alone.
fn is_live(facts: &ProcFacts) -> bool {
    match proc::liveness(facts) {
        Liveness::Alive => true,
        Liveness::Zombie => {
            reap(facts.pid);
            false
        }
        Liveness::Dead => false,
        Liveness::Unknown(e) => {
            tracing::warn!(pid = facts.pid, error = %e, "cannot tell if the ssh child is alive");
            true
        }
    }
}

/// Does this pid still carry our `-L` forward?
///
/// `Some(true)` yes, `Some(false)` it re-exec'd into something else, `None` we could not
/// read `/proc/<pid>/cmdline` and are not going to guess.
fn confirms_forward(pid: u32, spec: &TunnelSpec) -> Option<bool> {
    let argv = proc::cmdline(pid).ok()?;
    let token = forward_token(spec);
    Some(argv.contains(&token))
}

/// The first `ssh` on `PATH`, or the bare name for `execvp` to resolve.
///
/// Resolved eagerly so [`ProcFacts::exe`] records an absolute path, which is what makes a
/// re-adopted record self-describing.
fn resolve_ssh() -> PathBuf {
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("ssh");
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::from("ssh")
}

/// Remove a file that may not be there. A ControlPath we could not unlink is a warning, not
/// a failure: the teardown that matters — the child and the master — has already happened.
fn unlink(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(
            path = %path.display(),
            error = %e,
            "could not unlink the ControlPath"
        ),
    }
}

/// Unix seconds, clamped rather than panicking on a pre-1970 clock.
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::OnceLock;

    // -----------------------------------------------------------------------------------
    // The stand-in ssh
    // -----------------------------------------------------------------------------------

    /// Same argv shape as OpenSSH, no network.
    ///
    /// * `-L a:127.0.0.1:b` binds `a` on loopback and holds it, which is exactly the signal
    ///   `up` gates on. A bind failure exits non-zero, which is what
    ///   `ExitOnForwardFailure=yes` does against a dead link.
    /// * `-O exit` appends to `<ControlPath>.exit`, so a test can prove teardown ran it.
    /// * a destination containing `fail-host` exits 255, standing in for an unreachable box.
    const FAKE_SSH: &str = r#"#!/usr/bin/env python3
import os, socket, sys, time

argv = sys.argv[1:]

def opt(prefix):
    for i, a in enumerate(argv):
        if a == '-o' and i + 1 < len(argv) and argv[i + 1].startswith(prefix):
            return argv[i + 1][len(prefix):]
    return None

cp = opt('ControlPath=')

if '-O' in argv:
    i = argv.index('-O')
    action = argv[i + 1] if i + 1 < len(argv) else 'none'
    if cp:
        with open(cp + '.' + action, 'a') as f:
            f.write('called\n')
    sys.exit(0)

dest = [a for a in argv if '@' in a]
if dest and 'fail-host' in dest[0]:
    sys.stderr.write('ssh: connect to host ' + dest[0] + ' port 22: No route to host\n')
    sys.exit(255)

fwd = argv[argv.index('-L') + 1]
local = int(fwd.split(':')[0])

s = socket.socket()
try:
    s.bind(('127.0.0.1', local))
except OSError as e:
    sys.stderr.write('bind [127.0.0.1]:%d: %s\n' % (local, e))
    sys.exit(255)
s.listen(16)

if cp:
    open(cp, 'w').close()

print('forwarding ' + fwd, flush=True)
while True:
    time.sleep(3600)
"#;

    // -----------------------------------------------------------------------------------
    // Harness
    // -----------------------------------------------------------------------------------

    /// `Paths::resolve` reads the process environment, which is global. One lock, and the
    /// variable is restored before it is released.
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// A `Paths` rooted at `dir`.
    ///
    /// The variable is process-global and another unit's test module holds its own lock over
    /// the same variable, so the result is re-checked and the resolution retried rather than
    /// trusted: a `Paths` pointing at somebody else's temp root would fail this module's
    /// tests for a reason that has nothing to do with tunnels.
    fn paths_at(dir: &Path) -> Paths {
        for _ in 0..50 {
            let guard = match env_lock().lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            let previous = std::env::var_os("APEXROUTER_HOME");
            std::env::set_var("APEXROUTER_HOME", dir);
            let resolved = Paths::resolve();
            match previous {
                Some(v) => std::env::set_var("APEXROUTER_HOME", v),
                None => std::env::remove_var("APEXROUTER_HOME"),
            }
            drop(guard);

            if let Ok(paths) = resolved {
                if paths.state() == dir {
                    paths.ensure_layout().expect("layout");
                    return paths;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("could not resolve a Paths rooted at {}", dir.display());
    }

    /// A loopback port nothing is listening on right now.
    fn free_port() -> u16 {
        let l = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral");
        let port = l.local_addr().expect("local addr").port();
        drop(l);
        port
    }

    /// A small port pool starting somewhere the kernel just told us was free, so two of
    /// these tests running in parallel do not fight over 8800.
    fn pool() -> (u16, u16) {
        let base = free_port();
        (base, base.saturating_add(20))
    }

    struct Harness {
        _dir: tempfile::TempDir,
        paths: Paths,
        sup: Arc<TunnelSupervisor>,
        rx: broadcast::Receiver<Event>,
    }

    impl Harness {
        fn new() -> Harness {
            Harness::tuned(|_| {})
        }

        fn tuned(tune: impl FnOnce(&mut Config)) -> Harness {
            let dir = tempfile::tempdir().expect("tempdir");
            let paths = paths_at(&dir.path().join("state"));

            let bin = dir.path().join("bin");
            std::fs::create_dir_all(&bin).expect("bin dir");
            let ssh = bin.join("ssh");
            std::fs::write(&ssh, FAKE_SSH).expect("write fake ssh");
            std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).expect("chmod");

            let mut cfg = Config::default();
            cfg.vast.tunnel_port_range = pool();
            tune(&mut cfg);

            let (tx, rx) = broadcast::channel(256);
            let sup = TunnelSupervisor::new(paths.clone(), cfg, tx)
                .with_ssh_binary(ssh)
                .with_timings(
                    Duration::from_millis(50),
                    Duration::from_millis(20),
                    Duration::from_secs(10),
                );
            Harness {
                _dir: dir,
                paths,
                sup: Arc::new(sup),
                rx,
            }
        }

        fn spec(&self, instance: u64, local: u16) -> TunnelSpec {
            TunnelSpec {
                instance_id: InstanceId(instance),
                local_port: local,
                remote_port: 8000,
                ssh_host: "ssh5.vast.ai".to_owned(),
                ssh_port: 41231,
            }
        }

        /// A second supervisor over the same state root — a daemon restart.
        fn restarted(&self) -> Arc<TunnelSupervisor> {
            let mut cfg = Config::default();
            cfg.vast.tunnel_port_range = pool();
            let (tx, _rx) = broadcast::channel(16);
            Arc::new(
                TunnelSupervisor::new(self.paths.clone(), cfg, tx)
                    .with_ssh_binary(self.sup.ssh.clone())
                    .with_timings(
                        Duration::from_millis(50),
                        Duration::from_millis(20),
                        Duration::from_secs(10),
                    ),
            )
        }

        fn records(&self) -> Vec<TunnelStatus> {
            self.sup.status().expect("status")
        }

        /// Every event broadcast so far.
        fn drain(&mut self) -> Vec<Event> {
            let mut out = Vec::new();
            while let Ok(ev) = self.rx.try_recv() {
                out.push(ev);
            }
            out
        }
    }

    impl Drop for Harness {
        /// The forwards are detached on purpose, so the test binary must clean up after
        /// itself or a stand-in ssh outlives `cargo test` holding a loopback port.
        fn drop(&mut self) {
            self.sup.stop();
            for rec in self.sup.status().unwrap_or_default() {
                if let Some(facts) = rec.proc {
                    let _ = proc::signal_verified(&facts, Signal::Kill);
                    reap(facts.pid);
                }
            }
        }
    }

    /// Poll until `f` holds, or fail the test.
    async fn until(label: &str, f: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("timed out waiting for {label}");
    }

    // -----------------------------------------------------------------------------------
    // argv
    // -----------------------------------------------------------------------------------

    #[test]
    fn the_flag_set_is_exactly_architecture_4_9() {
        let spec = TunnelSpec {
            instance_id: InstanceId(28_675_431),
            local_port: 8800,
            remote_port: 8000,
            ssh_host: "ssh5.vast.ai".to_owned(),
            ssh_port: 41231,
        };
        let argv = tunnel_argv(
            &spec,
            Path::new("/state/ssh/known_hosts"),
            Path::new("/state/ssh/cm-28675431"),
        );
        assert_eq!(
            argv,
            vec![
                "-N",
                "-L",
                "8800:127.0.0.1:8000",
                "-p",
                "41231",
                "root@ssh5.vast.ai",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "ServerAliveInterval=30",
                "-o",
                "ServerAliveCountMax=3",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "UserKnownHostsFile=/state/ssh/known_hosts",
                "-o",
                "ControlMaster=auto",
                "-o",
                "ControlPath=/state/ssh/cm-28675431",
                "-o",
                "ControlPersist=5m",
            ]
        );
    }

    #[test]
    fn a_host_that_already_names_a_user_is_not_prefixed_twice() {
        assert_eq!(destination("ssh5.vast.ai"), "root@ssh5.vast.ai");
        assert_eq!(destination("root@ssh5.vast.ai"), "root@ssh5.vast.ai");
    }

    /// The two things `ARCHITECTURE.md` forbids here, asserted against this module's own
    /// source. Comment lines are excluded: the prose is allowed to *name* what the code may
    /// not do, and the needles are assembled at compile time so this test is not a hit on
    /// itself.
    #[test]
    fn this_module_never_shells_out_and_never_greps_for_a_pid() {
        let by_name = concat!("pg", "rep");
        let shell = concat!("sh", " -c");
        let code: String = include_str!("ssh.rs")
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<&str>>()
            .join("\n");
        assert!(
            !code.contains(by_name),
            "{by_name} must appear nowhere: it can kill an unrelated ssh"
        );
        assert!(
            !code.contains(shell),
            "no shell, anywhere: argv vectors only"
        );
    }

    // -----------------------------------------------------------------------------------
    // up / down
    // -----------------------------------------------------------------------------------

    #[tokio::test]
    async fn up_spawns_the_forward_and_persists_the_record() {
        let mut h = Harness::new();
        let port = free_port();
        let status = h.sup.up(h.spec(1, port)).await.expect("up");

        assert!(status.up);
        assert_eq!(status.restarts, 0);
        assert_eq!(status.last_error, None);
        let facts = status.proc.clone().expect("proc facts");
        assert!(is_live(&facts));
        assert!(!proc::port_free(port), "the forward must hold its port");

        let on_disk = h.records();
        assert_eq!(on_disk.len(), 1);
        assert_eq!(on_disk[0].proc.as_ref().map(|f| f.pid), Some(facts.pid));
        assert!(h.drain().iter().any(
            |e| matches!(e, Event::LogLine { source: LogSource::Instance { id }, .. } if id.0 == 1)
        ));

        h.sup.down(InstanceId(1)).await.expect("down");
    }

    #[tokio::test]
    async fn up_is_idempotent_while_the_child_is_alive() {
        let h = Harness::new();
        let port = free_port();
        let first = h.sup.up(h.spec(1, port)).await.expect("first up");
        let second = h.sup.up(h.spec(1, port)).await.expect("second up");

        assert_eq!(
            first.proc.as_ref().map(|f| f.pid),
            second.proc.as_ref().map(|f| f.pid),
            "a live tunnel must not be respawned"
        );
        assert_eq!(h.records().len(), 1);
        h.sup.down(InstanceId(1)).await.expect("down");
    }

    #[tokio::test]
    async fn a_zero_local_port_is_allocated_from_the_pool() {
        let h = Harness::new();
        let range = h.sup.cfg_read(|c| c.vast.tunnel_port_range);
        let mut spec = h.spec(7, 0);
        spec.local_port = 0;
        let status = h.sup.up(spec).await.expect("up");
        assert!(
            (range.0..=range.1).contains(&status.spec.local_port),
            "{} is outside {range:?}",
            status.spec.local_port
        );
        h.sup.down(InstanceId(7)).await.expect("down");
    }

    #[tokio::test]
    async fn a_second_instance_cannot_take_a_live_local_port() {
        let h = Harness::new();
        let port = free_port();
        h.sup.up(h.spec(1, port)).await.expect("up");

        let err = h.sup.up(h.spec(2, port)).await.expect_err("collision");
        match err {
            Error::PortInUse { port: p, held_by } => {
                assert_eq!(p, port);
                assert_eq!(held_by, Some("tunnel for instance 1".to_owned()));
            }
            other => panic!("expected PortInUse, got {other}"),
        }
        h.sup.down(InstanceId(1)).await.expect("down");
    }

    #[tokio::test]
    async fn down_kills_the_child_exits_the_master_and_unlinks_the_control_path() {
        let h = Harness::new();
        let port = free_port();
        let status = h.sup.up(h.spec(1, port)).await.expect("up");
        let facts = status.proc.clone().expect("facts");
        let control_path = h.paths.control_path(InstanceId(1));
        until("the ControlPath to appear", || control_path.exists()).await;

        h.sup.down(InstanceId(1)).await.expect("down");

        assert!(
            !control_path.exists(),
            "the ControlPath socket must be unlinked"
        );
        let marker = control_path.with_extension("exit");
        assert!(
            marker.exists(),
            "teardown must run `ssh -O exit`, marker {} missing",
            marker.display()
        );
        assert!(!is_live(&facts), "the ssh child must be gone");
        assert!(h.records().is_empty(), "the record must be forgotten");
        until("the local port to be released", || proc::port_free(port)).await;
    }

    #[tokio::test]
    async fn down_on_an_unknown_instance_is_not_an_error() {
        let h = Harness::new();
        h.sup.down(InstanceId(4_242)).await.expect("down");
    }

    // -----------------------------------------------------------------------------------
    // adoption
    // -----------------------------------------------------------------------------------

    #[tokio::test]
    async fn a_daemon_restart_re_adopts_rather_than_colliding() {
        let h = Harness::new();
        let port = free_port();
        let first = h.sup.up(h.spec(1, port)).await.expect("up");
        let pid = first.proc.as_ref().map(|f| f.pid);

        let restarted = h.restarted();
        let adopted = restarted.adopt_all().await.expect("adopt");
        assert_eq!(adopted.len(), 1);
        assert!(adopted[0].up, "a live forward must be re-adopted");
        assert_eq!(adopted[0].proc.as_ref().map(|f| f.pid), pid);

        // And `up` on the restarted daemon returns the same child instead of fighting for
        // the local port.
        let again = restarted.up(h.spec(1, port)).await.expect("re-up");
        assert_eq!(again.proc.as_ref().map(|f| f.pid), pid);

        restarted.down(InstanceId(1)).await.expect("down");
    }

    #[tokio::test]
    async fn a_dead_record_is_adopted_as_down_and_never_signalled() {
        let h = Harness::new();
        let port = free_port();
        let status = h.sup.up(h.spec(1, port)).await.expect("up");
        let facts = status.proc.clone().expect("facts");
        proc::signal_verified(&facts, Signal::Kill).expect("kill");
        until("the child to die", || !is_live(&facts)).await;

        let adopted = h.sup.adopt_all().await.expect("adopt");
        assert_eq!(adopted.len(), 1);
        assert!(!adopted[0].up);
        assert!(adopted[0]
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("no longer the ssh")));

        h.sup.down(InstanceId(1)).await.expect("down");
    }

    // -----------------------------------------------------------------------------------
    // the reconnect loop
    // -----------------------------------------------------------------------------------

    #[tokio::test]
    async fn a_killed_tunnel_is_reconnected() {
        let h = Harness::new();
        let port = free_port();
        let first = h.sup.up(h.spec(1, port)).await.expect("up");
        let dead_pid = first.proc.as_ref().map(|f| f.pid).expect("pid");

        let sup = Arc::clone(&h.sup);
        let loop_handle = tokio::spawn(sup.supervise());

        let facts = first.proc.clone().expect("facts");
        proc::signal_verified(&facts, Signal::Kill).expect("kill");

        let watch = Arc::clone(&h.sup);
        until("the tunnel to come back on a new pid", || {
            watch.status().unwrap_or_default().first().is_some_and(|t| {
                t.up && t.proc.as_ref().is_some_and(|f| f.pid != dead_pid) && t.restarts >= 1
            })
        })
        .await;

        h.sup.stop();
        let _ = loop_handle.await;
        h.sup.down(InstanceId(1)).await.expect("down");
    }

    #[tokio::test]
    async fn an_exhausted_restart_budget_raises_a_serious_alert() {
        let mut h = Harness::tuned(|c| c.supervisor.max_restarts_per_hour = 2);
        // A record whose host can never be reached: every reconnect attempt fails fast.
        let mut spec = h.spec(9, free_port());
        spec.ssh_host = "fail-host.vast.ai".to_owned();
        let store = Store::new(h.paths.clone());
        store
            .save_tunnels(&[TunnelStatus {
                spec,
                up: false,
                proc: None,
                since_unix: None,
                restarts: 0,
                last_error: None,
            }])
            .expect("seed");

        let sup = Arc::clone(&h.sup);
        let loop_handle = tokio::spawn(sup.supervise());

        let watch = Arc::clone(&h.sup);
        until("the budget to be exhausted", || {
            watch
                .status()
                .unwrap_or_default()
                .first()
                .and_then(|t| t.last_error.clone())
                .is_some_and(|e| e.contains("budget"))
        })
        .await;

        h.sup.stop();
        let _ = loop_handle.await;

        let alerts: Vec<Event> = h
            .drain()
            .into_iter()
            .filter(|e| matches!(e, Event::Alert { level, .. } if *level == AlertLevel::Serious))
            .collect();
        assert!(
            alerts.iter().any(|e| matches!(
                e,
                Event::Alert { id, action, .. } if id == "tunnel.9.restarts" && action.is_some()
            )),
            "expected one Serious alert, got {alerts:?}"
        );
        assert!(
            !h.records().first().is_some_and(|t| t.up),
            "an exhausted tunnel stays down"
        );
    }

    #[test]
    fn the_backoff_doubles_and_is_capped_at_thirty_seconds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths_at(&dir.path().join("state"));
        let (tx, _rx) = broadcast::channel(4);
        let sup = TunnelSupervisor::new(paths, Config::default(), tx);
        assert_eq!(sup.backoff(1), Duration::from_secs(1));
        assert_eq!(sup.backoff(2), Duration::from_secs(2));
        assert_eq!(sup.backoff(3), Duration::from_secs(4));
        assert_eq!(sup.backoff(6), Duration::from_secs(30));
        assert_eq!(sup.backoff(30), BACKOFF_CAP);
    }

    #[tokio::test]
    async fn shutdown_adopts_by_default_and_tears_down_when_told_to() {
        let h = Harness::new();
        let port = free_port();
        h.sup.up(h.spec(1, port)).await.expect("up");

        h.sup.on_shutdown().await.expect("adopt shutdown");
        assert_eq!(h.records().len(), 1, "the default is adopt");
        assert!(!proc::port_free(port), "the forward is still up");

        let mut cfg = Config::default();
        cfg.vast.tunnels_on_shutdown = "down".to_owned();
        h.sup.set_config(cfg);
        h.sup.on_shutdown().await.expect("down shutdown");
        assert!(h.records().is_empty());
        until("the local port to be released", || proc::port_free(port)).await;
    }
}
