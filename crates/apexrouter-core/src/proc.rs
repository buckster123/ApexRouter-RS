//! OWNER: unit C-06 (core/proc.rs). Do not edit outside that unit.
//!
//! Process identity, liveness and detached spawn. **Children outlive the manager.**
//!
//! Three traps this module exists to close:
//!
//! * `/proc/<pid>/stat` field 22 must be parsed **after the last `)`** — `comm` can contain
//!   spaces and parentheses, and naive whitespace splitting silently reads the wrong number.
//! * A signal is only ever sent after re-verifying identity, so a reused PID is never killed.
//! * `EPERM` is a [`Liveness::Unknown`] the caller must match, never a panic.

use crate::error::{Error, Result};
use apexrouter_protocol::EndpointRecord;
use apexrouter_protocol::ProcFacts;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::ErrorKind;
use std::net::TcpListener;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// `/proc/<pid>/stat` index of the `state` field, counted from the first field **after** the
/// closing `)` of `comm`. `state` is field 3 (1-based), so its index here is `3 - 3`.
const STAT_IDX_STATE: usize = 0;
/// Index of `ppid` (field 4) among the fields after `comm`.
const STAT_IDX_PPID: usize = 1;
/// Index of `starttime` (field 22) among the fields after `comm`.
const STAT_IDX_STARTTIME: usize = 19;

/// Path of `/proc/sys/kernel/random/boot_id`.
const BOOT_ID_PATH: &str = "/proc/sys/kernel/random/boot_id";

/// Is a pid alive, and in what sense?
#[derive(Debug)]
pub enum Liveness {
    /// Running.
    Alive,
    /// Exited but not reaped. Its parent is init, which will reap it.
    Zombie,
    /// Gone.
    Dead,
    /// We could not tell — typically `EPERM`. **Must be matched**, never assumed dead.
    Unknown(std::io::Error),
}

/// What a record's identity check concluded.
#[derive(Debug)]
pub enum Adoption {
    /// pid ∧ start_ticks ∧ boot_id ∧ exe ∧ cmdline all agree. We own it; we may signal it.
    Adopted(ProcFacts),
    /// Something else holds the port or the pid. **NEVER signalled.**
    Foreign {
        /// Whose it is.
        pid: u32,
        /// Why we concluded that.
        why: String,
    },
    /// Gone. `desired == Running` makes it a failure; `desired == Stopped` makes it tidy-up.
    Vanished,
    /// Partial match. Also never signalled.
    Ambiguous {
        /// The pid in question.
        pid: u32,
        /// Why it is ambiguous.
        why: String,
    },
}

/// Which signal to send. Deliberately tiny: we only ever send these two.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Signal {
    /// Ask politely.
    Term,
    /// Insist.
    Kill,
}

/// `/proc/sys/kernel/random/boot_id`. Part of process identity because `start_time_ticks`
/// is measured since boot and is not comparable across one.
pub fn boot_id() -> Result<String> {
    let raw = fs::read_to_string(BOOT_ID_PATH).map_err(|e| io_err(BOOT_ID_PATH, e))?;
    Ok(raw.trim().to_string())
}

/// `/proc/<pid>/stat` field 22, parsed **after the last `)`**.
pub fn start_time_ticks(pid: u32) -> Result<u64> {
    let path = proc_path(pid, "stat");
    let raw = fs::read_to_string(&path).map_err(|e| io_err(&path, e))?;
    let fields = fields_after_comm(&raw).ok_or_else(|| unparsable(&path))?;
    fields
        .get(STAT_IDX_STARTTIME)
        .and_then(|f| f.parse::<u64>().ok())
        .ok_or_else(|| unparsable(&path))
}

/// `/proc/<pid>/cmdline`, split on NUL.
pub fn cmdline(pid: u32) -> Result<Vec<String>> {
    let path = proc_path(pid, "cmdline");
    let raw = fs::read(&path).map_err(|e| io_err(&path, e))?;
    Ok(split_nul(&raw))
}

/// Resolved `/proc/<pid>/exe`, with a trailing `" (deleted)"` stripped — rebuilding
/// `build-vulkan` under a running server must not un-adopt it.
pub fn exe_path(pid: u32) -> Result<String> {
    let path = proc_path(pid, "exe");
    let target = fs::read_link(&path).map_err(|e| io_err(&path, e))?;
    Ok(strip_deleted(&target.to_string_lossy()))
}

/// Is the process these facts describe still the one we think it is?
///
/// A different `boot_id`, a different `start_time_ticks` or a missing `/proc` entry all mean
/// [`Liveness::Dead`]: the pid may well be in use, but not by *our* process. Anything that
/// merely stops us from looking — `EPERM` under `hidepid`, an unparsable `stat` — is
/// [`Liveness::Unknown`], which the caller must handle rather than treat as dead.
pub fn liveness(f: &ProcFacts) -> Liveness {
    match boot_id() {
        Ok(current) if current != f.boot_id => return Liveness::Dead,
        Ok(_) => {}
        Err(e) => return Liveness::Unknown(as_io(e)),
    }
    let path = proc_path(f.pid, "stat");
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) => return classify_proc_error(e),
    };
    let fields = match fields_after_comm(&raw) {
        Some(fields) => fields,
        None => return Liveness::Unknown(std::io::Error::other(format!("unparsable {path}"))),
    };
    match fields
        .get(STAT_IDX_STARTTIME)
        .and_then(|s| s.parse::<u64>().ok())
    {
        Some(ticks) if ticks == f.start_time_ticks => {}
        Some(_) => return Liveness::Dead,
        None => return Liveness::Unknown(std::io::Error::other(format!("unparsable {path}"))),
    }
    match fields.get(STAT_IDX_STATE).and_then(|s| s.chars().next()) {
        Some('Z') => Liveness::Zombie,
        Some('X') | Some('x') => Liveness::Dead,
        Some(_) => Liveness::Alive,
        None => Liveness::Unknown(std::io::Error::other(format!("unparsable {path}"))),
    }
}

/// Capture identity for a pid we just spawned or are about to adopt.
///
/// `argv` must be the **full** vector including `argv[0]`, because that is what
/// `/proc/<pid>/cmdline` reports and what [`adopt`] later re-hashes.
///
/// `exe` is the binary the caller believes it launched; it is canonicalised and stored as
/// given. `/proc/<pid>/exe` is deliberately **not** consulted when the caller knows: between
/// `fork()` and `execve()` that link still points at the *manager's* binary, so reading it
/// immediately after a spawn would record the wrong path. Pass an empty `exe` — when adopting
/// a pid whose binary we do not know — to read it from `/proc` instead.
pub fn identify(pid: u32, argv: &[String], exe: &str) -> Result<ProcFacts> {
    let exe = if exe.is_empty() {
        exe_path(pid)?
    } else {
        let stripped = strip_deleted(exe);
        fs::canonicalize(&stripped)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or(stripped)
    };
    Ok(ProcFacts {
        pid,
        start_time_ticks: start_time_ticks(pid)?,
        boot_id: boot_id()?,
        exe,
        cmdline_sha256: hash_argv(argv),
    })
}

/// Full adoption check for a persisted record.
///
/// The decision ladder, strictest first: no facts or a stale `boot_id` is
/// [`Adoption::Vanished`]; a live pid whose `start_time_ticks` differ is
/// [`Adoption::Foreign`] (pid reuse — never signalled); an unreadable `/proc` or a changed
/// argv is [`Adoption::Ambiguous`]. `exe` is **advisory only**: a mismatch is recorded in the
/// refreshed facts but does not un-adopt, so rebuilding the binary under a running server is
/// survivable.
pub fn adopt(rec: &EndpointRecord) -> Adoption {
    let facts = match rec.proc.as_ref() {
        Some(facts) => facts,
        None => return Adoption::Vanished,
    };
    let pid = facts.pid;

    match boot_id() {
        Ok(current) if current != facts.boot_id => return Adoption::Vanished,
        Ok(_) => {}
        Err(e) => {
            return Adoption::Ambiguous {
                pid,
                why: format!("cannot read boot_id: {e}"),
            }
        }
    }

    let path = proc_path(pid, "stat");
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == ErrorKind::NotFound => return Adoption::Vanished,
        Err(e) => {
            return Adoption::Ambiguous {
                pid,
                why: format!("cannot read {path}: {e}"),
            }
        }
    };
    let fields = match fields_after_comm(&raw) {
        Some(fields) => fields,
        None => {
            return Adoption::Ambiguous {
                pid,
                why: format!("unparsable {path}"),
            }
        }
    };
    match fields.get(STAT_IDX_STATE).and_then(|s| s.chars().next()) {
        Some('Z') | Some('X') | Some('x') => return Adoption::Vanished,
        Some(_) => {}
        None => {
            return Adoption::Ambiguous {
                pid,
                why: format!("unparsable {path}"),
            }
        }
    }
    match fields
        .get(STAT_IDX_STARTTIME)
        .and_then(|s| s.parse::<u64>().ok())
    {
        Some(ticks) if ticks == facts.start_time_ticks => {}
        Some(ticks) => {
            return Adoption::Foreign {
                pid,
                why: format!(
                    "pid {pid} started at {ticks} ticks, the record says {}: the pid was reused",
                    facts.start_time_ticks
                ),
            }
        }
        None => {
            return Adoption::Ambiguous {
                pid,
                why: format!("unparsable {path}"),
            }
        }
    }

    let argv = match cmdline(pid) {
        Ok(argv) => argv,
        Err(e) => {
            return Adoption::Ambiguous {
                pid,
                why: format!("cannot read cmdline: {e}"),
            }
        }
    };
    let hash = hash_argv(&argv);
    if hash != facts.cmdline_sha256 {
        return Adoption::Ambiguous {
            pid,
            why: format!(
                "pid {pid} matches on identity but its argv hashes to {hash}, \
                 the record says {}: it re-exec'd or rewrote argv",
                facts.cmdline_sha256
            ),
        };
    }

    Adoption::Adopted(ProcFacts {
        pid,
        start_time_ticks: facts.start_time_ticks,
        boot_id: facts.boot_id.clone(),
        exe: exe_path(pid).unwrap_or_else(|_| facts.exe.clone()),
        cmdline_sha256: hash,
    })
}

/// Bind-probe on `127.0.0.1`. The caller holds the reservation under a per-endpoint lock
/// until the child's health gate passes, so two concurrent launches cannot both win a port.
pub fn port_free(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// First free port in a range, skipping ports already promised.
pub fn alloc_port(range: (u16, u16), taken: &[u16]) -> Option<u16> {
    let (lo, hi) = range;
    if lo > hi {
        return None;
    }
    (lo..=hi).find(|p| !taken.contains(p) && port_free(*p))
}

/// Everything needed to launch a detached child.
#[derive(Debug)]
pub struct SpawnRequest<'a> {
    /// The binary.
    pub program: &'a Path,
    /// argv, excluding argv[0].
    pub args: &'a [String],
    /// Environment additions, including `LD_LIBRARY_PATH`.
    pub env: &'a [(String, String)],
    /// Working directory. `$STATE`, and never load-bearing.
    pub cwd: &'a Path,
    /// Where stdout and stderr go. Opened `O_APPEND`; **never truncated**, because that
    /// would destroy the crash log of the previous run.
    pub log: &'a Path,
    /// Give the child its own session and process group.
    pub setsid: bool,
}

/// The result of a detached spawn.
#[derive(Debug)]
pub struct SpawnedChild {
    /// The child's pid.
    pub pid: u32,
    /// Its identity, captured **before** the spawn function returns.
    pub facts: ProcFacts,
}

/// `setsid` + `Stdio::from(owned File)` + `O_APPEND`.
///
/// The owned `File` makes the parent's fd leak inexpressible; `setsid` means the server does
/// not die with the terminal, and survives `systemctl --user restart apexrouterd`.
///
/// The `std::process::Child` handle is dropped on purpose: this function's contract is a pid
/// plus facts, and the child is *not* tied to the caller's lifetime. A daemon that wants to
/// avoid zombies while it is still alive reaps by pid in its own background task; once the
/// daemon exits, the child is reparented to init (or to the session's child subreaper) and is
/// reaped there.
pub fn spawn_detached(req: SpawnRequest<'_>) -> Result<SpawnedChild> {
    if let Some(parent) = req.log.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| io_err(parent.to_string_lossy(), e))?;
        }
    }
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(req.log)
        .map_err(|e| io_err(req.log.to_string_lossy(), e))?;
    let log_err = log
        .try_clone()
        .map_err(|e| io_err(req.log.to_string_lossy(), e))?;

    let mut cmd = Command::new(req.program);
    cmd.args(req.args)
        .current_dir(req.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    for (k, v) in req.env {
        cmd.env(k, v);
    }
    if req.setsid {
        // SAFETY: the closure runs between fork() and exec() in the child. It performs a
        // single `setsid(2)` syscall and allocates nothing, so it touches no lock the fork
        // could have left held.
        unsafe {
            cmd.pre_exec(|| rustix::process::setsid().map(|_| ()).map_err(Into::into));
        }
    }

    let child = cmd
        .spawn()
        .map_err(|e| io_err(req.program.to_string_lossy(), e))?;
    let pid = child.id();

    let program = req.program.to_string_lossy().into_owned();
    let mut argv = Vec::with_capacity(req.args.len() + 1);
    argv.push(program.clone());
    argv.extend(req.args.iter().cloned());

    let facts = identify(pid, &argv, &program)?;
    Ok(SpawnedChild { pid, facts })
}

/// Re-verify identity **first**, then signal. A mismatch is an error and sends nothing.
pub fn signal_verified(f: &ProcFacts, sig: Signal) -> Result<()> {
    match liveness(f) {
        Liveness::Alive => {}
        // Already exited and waiting to be reaped: there is nothing to signal, and saying so
        // is not an error.
        Liveness::Zombie => return Ok(()),
        Liveness::Dead => {
            return Err(Error::NotFound(format!(
                "pid {} is no longer the process these facts describe — no signal sent",
                f.pid
            )))
        }
        Liveness::Unknown(source) => {
            return Err(Error::Io {
                path: proc_path(f.pid, "stat"),
                source,
            })
        }
    }
    let pid =
        rustix::process::Pid::from_raw(i32::try_from(f.pid).unwrap_or(-1)).ok_or_else(|| {
            Error::Invalid {
                what: "pid".to_string(),
                why: format!("{} is not a signallable pid", f.pid),
            }
        })?;
    let sig = match sig {
        Signal::Term => rustix::process::Signal::Term,
        Signal::Kill => rustix::process::Signal::Kill,
    };
    rustix::process::kill_process(pid, sig).map_err(|e| Error::Io {
        path: format!("kill({})", f.pid),
        source: e.into(),
    })
}

/// `SIGTERM`, wait, `SIGKILL`, wait. Identity-verified at every step.
pub fn stop_graceful(f: &ProcFacts, term_wait: Duration, kill_wait: Duration) -> Result<()> {
    if matches!(liveness(f), Liveness::Dead | Liveness::Zombie) {
        return Ok(());
    }
    signal_verified(f, Signal::Term)?;
    if wait_until_gone(f, term_wait) {
        return Ok(());
    }
    signal_verified(f, Signal::Kill)?;
    if wait_until_gone(f, kill_wait) {
        return Ok(());
    }
    Err(Error::Timeout {
        ms: u64::try_from((term_wait + kill_wait).as_millis()).unwrap_or(u64::MAX),
    })
}

// ---------------------------------------------------------------------------------------
// internals
// ---------------------------------------------------------------------------------------

/// `/proc/<pid>/<leaf>`.
fn proc_path(pid: u32, leaf: &str) -> String {
    format!("/proc/{pid}/{leaf}")
}

/// Wrap an [`std::io::Error`] with the path it happened to.
fn io_err(path: impl AsRef<str>, source: std::io::Error) -> Error {
    Error::Io {
        path: path.as_ref().to_string(),
        source,
    }
}

/// The error a malformed `/proc` file deserves.
fn unparsable(path: &str) -> Error {
    Error::Invalid {
        what: path.to_string(),
        why: "not in the expected /proc stat layout".to_string(),
    }
}

/// Flatten a crate error back into an [`std::io::Error`] so it can ride in
/// [`Liveness::Unknown`].
fn as_io(e: Error) -> std::io::Error {
    match e {
        Error::Io { source, .. } | Error::RawIo(source) => source,
        other => std::io::Error::other(other.to_string()),
    }
}

/// `NotFound` is the only `/proc` error that means "dead". Everything else — `EPERM` under
/// `hidepid=2` above all — is [`Liveness::Unknown`] and must never be assumed to be death.
fn classify_proc_error(e: std::io::Error) -> Liveness {
    match e.kind() {
        ErrorKind::NotFound => Liveness::Dead,
        _ => Liveness::Unknown(e),
    }
}

/// The whitespace-separated fields of a `/proc/<pid>/stat` line **after the last `)`**.
///
/// `comm` is the process name in parentheses and it is neither escaped nor length-limited in
/// its content: a binary called `sl e)ep` produces `1234 (sl e)ep) S 1 …`. Splitting the whole
/// line on whitespace therefore mis-indexes every field after `comm`. Index 0 of the returned
/// vector is field 3 (`state`).
fn fields_after_comm(raw: &str) -> Option<Vec<&str>> {
    let close = raw.rfind(')')?;
    let fields: Vec<&str> = raw[close + 1..].split_whitespace().collect();
    if fields.is_empty() {
        None
    } else {
        Some(fields)
    }
}

/// Split a NUL-delimited `/proc` blob, dropping only the terminator's empty tail so an
/// intentionally empty argv element survives.
fn split_nul(raw: &[u8]) -> Vec<String> {
    let mut parts: Vec<&[u8]> = raw.split(|b| *b == 0).collect();
    if parts.last().is_some_and(|p| p.is_empty()) {
        parts.pop();
    }
    parts
        .into_iter()
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect()
}

/// Drop the `" (deleted)"` the kernel appends to `/proc/<pid>/exe` after the binary is
/// replaced. Rebuilding `build-vulkan` under a running `llama-server` must not un-adopt it.
fn strip_deleted(path: &str) -> String {
    path.strip_suffix(" (deleted)").unwrap_or(path).to_string()
}

/// SHA-256 over the argv vector joined by NUL — exactly the bytes `/proc/<pid>/cmdline`
/// reports, minus its terminator.
fn hash_argv(argv: &[String]) -> String {
    let mut h = Sha256::new();
    for (i, a) in argv.iter().enumerate() {
        if i > 0 {
            h.update([0u8]);
        }
        h.update(a.as_bytes());
    }
    format!("{:x}", h.finalize())
}

/// Poll liveness until the process is gone or the budget expires. `true` = gone.
fn wait_until_gone(f: &ProcFacts, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    let step = Duration::from_millis(20);
    loop {
        if matches!(liveness(f), Liveness::Dead | Liveness::Zombie) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        std::thread::sleep(step.min(deadline - now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_protocol::{BackendId, DesiredState, EndpointSpec, NodeSpec};
    use std::process::Child;

    /// A synthetic `/proc/<pid>/stat` whose `comm` contains both a space and a `)`.
    const TRICKY_STAT: &str = "4242 (sl e)ep) S 1 4242 4242 0 -1 4194304 210 0 0 0 1 2 3 4 \
                               20 0 1 0 987654321 12345 6 18446744073709551615 1 1 0\n";

    fn self_facts() -> ProcFacts {
        let pid = std::process::id();
        let argv: Vec<String> = std::env::args().collect();
        identify(pid, &argv, "/proc/self/exe").expect("identify self")
    }

    fn ppid(pid: u32) -> Option<u32> {
        let raw = fs::read_to_string(proc_path(pid, "stat")).ok()?;
        fields_after_comm(&raw)?
            .get(STAT_IDX_PPID)
            .and_then(|s| s.parse().ok())
    }

    fn uptime_ticks() -> u64 {
        let raw = fs::read_to_string("/proc/uptime").expect("read /proc/uptime");
        let secs: f64 = raw
            .split_whitespace()
            .next()
            .expect("uptime field")
            .parse()
            .expect("uptime is a float");
        // 100 Hz is USER_HZ on every Linux target we support; the test only needs an
        // upper bound, so a fixed value is fine.
        (secs * 100.0) as u64
    }

    /// A binary reachable under a name containing a space and a `)`, so the child's `comm`
    /// is `(sl e)ep)` and a naive `/proc/<pid>/stat` split mis-indexes every field after it.
    ///
    /// It is a **symlink to this very test binary**, not a copy of `/bin/sleep`, for two
    /// reasons: `/bin/sleep` on this machine is a uutils multi-call binary that dispatches on
    /// `argv[0]` and exits immediately under any other name; and writing an executable and
    /// exec'ing it from a multi-threaded process races into `ETXTBSY`. `comm` is taken from
    /// the basename the kernel was handed, so a symlink is enough to make it hostile.
    fn tricky_binary(dir: &Path) -> std::path::PathBuf {
        let link = dir.join("sl e)ep");
        let target = std::env::current_exe().expect("current_exe");
        std::os::unix::fs::symlink(target, &link).expect("symlink the test binary");
        link
    }

    /// argv (excluding argv[0]) that makes [`tricky_binary`] run [`helper_sleep`].
    fn sleeper_args() -> Vec<String> {
        vec![
            "--exact".to_string(),
            "proc::tests::helper_sleep".to_string(),
        ]
    }

    /// The env that tells [`helper_sleep`] how long to stay up.
    fn sleeper_env(ms: u64) -> Vec<(String, String)> {
        vec![("APEXROUTER_TEST_SLEEP_MS".to_string(), ms.to_string())]
    }

    /// Start a long-lived child whose `comm` is hostile. Returns its path and the handle.
    fn spawn_tricky(dir: &Path, ms: u64) -> (std::path::PathBuf, Child) {
        let bin = tricky_binary(dir);
        let child = Command::new(&bin)
            .args(sleeper_args())
            .env("APEXROUTER_TEST_SLEEP_MS", ms.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the tricky binary");
        (bin, child)
    }

    /// The sleeping half of [`spawn_tricky`]. Under a plain `cargo test` the env var is
    /// absent and this is a no-op.
    #[test]
    fn helper_sleep() {
        let Ok(ms) = std::env::var("APEXROUTER_TEST_SLEEP_MS") else {
            return;
        };
        std::thread::sleep(Duration::from_millis(ms.parse().unwrap_or(0)));
        std::process::exit(0);
    }

    /// `/proc/<pid>/cmdline` is a torn read while the child is still inside `execve` — the
    /// old and new argv areas overlap. Poll until it has settled at `want` elements.
    fn settled_cmdline(pid: u32, want: usize) -> Vec<String> {
        for _ in 0..200 {
            match cmdline(pid) {
                Ok(argv) if argv.len() == want => return argv,
                _ => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        cmdline(pid).unwrap_or_default()
    }

    /// `/proc/<pid>/stat` has the same window as `cmdline`: between `fork()` and the end of
    /// `execve()` the child is still a clone of us, so its `comm` is the *parent thread's*
    /// name (`proc::tests::st…`) and its state is `D` while the new image loads. `comm` only
    /// becomes the basename we exec'd once the new image is installed. Poll for it, exactly
    /// as [`settled_cmdline`] polls for argv, or this test fails a few percent of the time.
    fn settled_stat(pid: u32, want_comm: &str) -> String {
        for _ in 0..200 {
            match fs::read_to_string(proc_path(pid, "stat")) {
                Ok(raw) if raw.contains(want_comm) => return raw,
                _ => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        fs::read_to_string(proc_path(pid, "stat")).unwrap_or_default()
    }

    struct Reaper(Child);
    impl Drop for Reaper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn fields_after_comm_survives_parens_and_spaces_in_comm() {
        let f = fields_after_comm(TRICKY_STAT).expect("parsed");
        assert_eq!(f[STAT_IDX_STATE], "S");
        assert_eq!(f[STAT_IDX_PPID], "1");
        assert_eq!(f[STAT_IDX_STARTTIME], "987654321");
        // The naive parse everybody writes first would land three fields early.
        let naive: Vec<&str> = TRICKY_STAT.split_whitespace().collect();
        assert_ne!(naive[21], "987654321");
    }

    #[test]
    fn start_time_ticks_parses_a_comm_containing_a_paren() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (_bin, child) = spawn_tricky(dir.path(), 60_000);
        let pid = child.id();
        let _reaper = Reaper(child);

        let raw = settled_stat(pid, "(sl e)ep)");
        assert!(raw.contains("(sl e)ep)"), "comm was not hostile: {raw}");

        let ticks = start_time_ticks(pid).expect("start_time_ticks");
        // A process that started after us and before now. The naive whitespace parse yields
        // `num_threads` (1) or `itrealvalue` (0) here, both far below this floor.
        let ours = start_time_ticks(std::process::id()).expect("our own start ticks");
        assert!(
            ticks >= ours,
            "child ({ticks}) must not predate this process ({ours})"
        );
        assert!(
            ticks <= uptime_ticks(),
            "child ({ticks}) must not postdate boot + uptime"
        );
    }

    #[test]
    fn cmdline_and_exe_of_a_tricky_child_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (bin, child) = spawn_tricky(dir.path(), 60_000);
        let pid = child.id();
        let _reaper = Reaper(child);

        let argv = settled_cmdline(pid, 3);
        assert_eq!(argv.len(), 3, "argv was {argv:?}");
        assert_eq!(argv[0], bin.to_string_lossy());
        assert_eq!(argv[1..], sleeper_args()[..]);
        // `/proc/<pid>/exe` resolves symlinks, so it names the real binary, not the hostile
        // path we exec'd — which is exactly why `exe` is advisory only.
        assert_eq!(
            exe_path(pid).expect("exe"),
            std::env::current_exe()
                .expect("current_exe")
                .to_string_lossy()
        );
    }

    #[test]
    fn split_nul_keeps_interior_empties_and_drops_the_terminator() {
        assert_eq!(split_nul(b"a\0\0b\0"), vec!["a", "", "b"]);
        assert_eq!(split_nul(b""), Vec::<String>::new());
    }

    #[test]
    fn strip_deleted_only_touches_the_suffix() {
        assert_eq!(
            strip_deleted("/x/llama-server (deleted)"),
            "/x/llama-server"
        );
        assert_eq!(strip_deleted("/x/llama-server"), "/x/llama-server");
        assert_eq!(strip_deleted("/x/a (deleted)/b"), "/x/a (deleted)/b");
    }

    #[test]
    fn hash_argv_is_nul_joined_and_order_sensitive() {
        assert_ne!(
            hash_argv(&["a".into(), "b".into()]),
            hash_argv(&["b".into(), "a".into()])
        );
        // "ab" must not collide with ["a","b"] — that is what the NUL separator buys.
        assert_ne!(
            hash_argv(&["ab".into()]),
            hash_argv(&["a".into(), "b".into()])
        );
    }

    #[test]
    fn liveness_of_ourselves_is_alive() {
        assert!(matches!(liveness(&self_facts()), Liveness::Alive));
    }

    #[test]
    fn liveness_is_dead_when_start_ticks_or_boot_id_disagree() {
        let mut f = self_facts();
        f.start_time_ticks = f.start_time_ticks.wrapping_add(1);
        assert!(matches!(liveness(&f), Liveness::Dead));

        let mut f = self_facts();
        f.boot_id = "00000000-0000-0000-0000-000000000000".to_string();
        assert!(matches!(liveness(&f), Liveness::Dead));

        let mut f = self_facts();
        f.pid = u32::MAX - 1; // no such pid; /proc lookup is ENOENT
        assert!(matches!(liveness(&f), Liveness::Dead));
    }

    #[test]
    fn eperm_is_unknown_never_a_panic() {
        // EPERM/EACCES on /proc (hidepid=2) must not be mistaken for death.
        let eperm = std::io::Error::from_raw_os_error(1);
        assert!(matches!(classify_proc_error(eperm), Liveness::Unknown(_)));
        let eacces = std::io::Error::from(ErrorKind::PermissionDenied);
        assert!(matches!(classify_proc_error(eacces), Liveness::Unknown(_)));
        // and only ENOENT is death
        let enoent = std::io::Error::from(ErrorKind::NotFound);
        assert!(matches!(classify_proc_error(enoent), Liveness::Dead));
    }

    #[test]
    fn signal_verified_refuses_a_mismatched_identity_and_sends_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (bin, child) = spawn_tricky(dir.path(), 60_000);
        let pid = child.id();
        let _reaper = Reaper(child);

        let argv = cmdline(pid).expect("cmdline");
        let good = identify(pid, &argv, &bin.to_string_lossy()).expect("identify");

        let mut bad = good.clone();
        bad.start_time_ticks = good.start_time_ticks.wrapping_add(1);
        assert!(signal_verified(&bad, Signal::Kill).is_err());
        // No signal was sent: the real identity is still alive.
        assert!(matches!(liveness(&good), Liveness::Alive));

        let mut bad = good.clone();
        bad.boot_id = "not-this-boot".to_string();
        assert!(signal_verified(&bad, Signal::Kill).is_err());
        assert!(matches!(liveness(&good), Liveness::Alive));

        // The matching identity does stop it.
        stop_graceful(
            &good,
            Duration::from_millis(500),
            Duration::from_millis(500),
        )
        .expect("stop_graceful");
        assert!(matches!(liveness(&good), Liveness::Dead | Liveness::Zombie));
    }

    #[test]
    fn stop_graceful_on_an_already_gone_process_is_ok() {
        let mut f = self_facts();
        f.pid = u32::MAX - 1;
        assert!(stop_graceful(&f, Duration::from_millis(10), Duration::from_millis(10)).is_ok());
    }

    fn record_with(proc: Option<ProcFacts>) -> EndpointRecord {
        EndpointRecord {
            id: BackendId::parse("test-endpoint").expect("id"),
            spec: EndpointSpec::Node(NodeSpec {
                base_url: "http://127.0.0.1:9".to_string(),
                credential: apexrouter_protocol::CredentialSource::None,
                label: "test".to_string(),
                declared_models: vec![],
                protocol: Default::default(),
            }),
            desired: DesiredState::Running,
            proc,
            port: None,
            log_path: None,
            started_at_unix: 0,
            fit: None,
            adopted: false,
            alias_bindings: vec![],
        }
    }

    #[test]
    fn adopt_classifies_every_case() {
        // no facts at all
        assert!(matches!(adopt(&record_with(None)), Adoption::Vanished));

        // a live, exact match
        let good = self_facts();
        assert!(matches!(
            adopt(&record_with(Some(good.clone()))),
            Adoption::Adopted(_)
        ));

        // pid reuse: same pid, different start time
        let mut reused = good.clone();
        reused.start_time_ticks = good.start_time_ticks.wrapping_add(1);
        assert!(matches!(
            adopt(&record_with(Some(reused))),
            Adoption::Foreign { .. }
        ));

        // rebooted since the record was written
        let mut rebooted = good.clone();
        rebooted.boot_id = "00000000-0000-0000-0000-000000000000".to_string();
        assert!(matches!(
            adopt(&record_with(Some(rebooted))),
            Adoption::Vanished
        ));

        // gone
        let mut gone = good.clone();
        gone.pid = u32::MAX - 1;
        assert!(matches!(
            adopt(&record_with(Some(gone))),
            Adoption::Vanished
        ));

        // same process, different argv
        let mut rewritten = good.clone();
        rewritten.cmdline_sha256 = hash_argv(&["something".into(), "else".into()]);
        assert!(matches!(
            adopt(&record_with(Some(rewritten))),
            Adoption::Ambiguous { .. }
        ));

        // a rebuilt binary (exe differs) is advisory only and still adopts
        let mut rebuilt = good.clone();
        rebuilt.exe = "/gone/llama-server".to_string();
        assert!(matches!(
            adopt(&record_with(Some(rebuilt))),
            Adoption::Adopted(_)
        ));
    }

    #[test]
    fn port_alloc_skips_taken_and_bound_ports() {
        let held = TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral bind");
        let held_port = held.local_addr().expect("local_addr").port();
        assert!(!port_free(held_port));

        let free = TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral bind");
        let free_port = free.local_addr().expect("local_addr").port();
        drop(free);
        assert!(port_free(free_port));

        assert_eq!(alloc_port((free_port, free_port), &[]), Some(free_port));
        assert_eq!(alloc_port((free_port, free_port), &[free_port]), None);
        assert_eq!(alloc_port((held_port, held_port), &[]), None);
        assert_eq!(alloc_port((9000, 8000), &[]), None);
    }

    #[test]
    fn spawn_detached_appends_to_the_log_and_never_truncates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("logs").join("child.log");
        fs::create_dir_all(dir.path().join("logs")).expect("mkdir");
        fs::write(&log, b"previous crash tail\n").expect("seed log");

        let bin = tricky_binary(dir.path());
        let args = sleeper_args();
        let env = sleeper_env(200);
        let spawned = spawn_detached(SpawnRequest {
            program: &bin,
            args: &args,
            env: &env,
            cwd: dir.path(),
            log: &log,
            setsid: true,
        })
        .expect("spawn_detached");

        assert_eq!(spawned.facts.pid, spawned.pid);
        assert_eq!(
            spawned.facts.exe,
            std::env::current_exe()
                .expect("current_exe")
                .to_string_lossy()
        );
        let mut expected = vec![bin.to_string_lossy().into_owned()];
        expected.extend(args.iter().cloned());
        assert_eq!(spawned.facts.cmdline_sha256, hash_argv(&expected));
        assert!(!spawned.facts.boot_id.is_empty());
        // The facts describe the live child, not a guess.
        assert!(matches!(liveness(&spawned.facts), Liveness::Alive));

        // the seeded content is still there
        let after = fs::read_to_string(&log).expect("read log");
        assert!(after.starts_with("previous crash tail\n"));

        let _ = signal_verified(&spawned.facts, Signal::Kill);
    }

    #[test]
    fn spawn_detached_gives_the_child_its_own_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("child.log");
        let bin = tricky_binary(dir.path());
        let args = sleeper_args();
        let env = sleeper_env(60_000);
        let spawned = spawn_detached(SpawnRequest {
            program: &bin,
            args: &args,
            env: &env,
            cwd: dir.path(),
            log: &log,
            setsid: true,
        })
        .expect("spawn_detached");

        let raw = fs::read_to_string(proc_path(spawned.pid, "stat")).expect("stat");
        assert!(raw.contains("(sl e)ep)"), "comm was not hostile: {raw}");
        let f = fields_after_comm(&raw).expect("fields");
        // fields 5 (pgrp) and 6 (session) -> indices 2 and 3
        assert_eq!(f[2], spawned.pid.to_string(), "child leads its own group");
        assert_eq!(f[3], spawned.pid.to_string(), "child leads its own session");

        let _ = signal_verified(&spawned.facts, Signal::Kill);
    }

    /// Helper half of [`a_detached_child_outlives_the_process_that_spawned_it`]. Under a plain
    /// `cargo test` the env var is absent and this is a no-op.
    #[test]
    fn helper_spawn_then_exit() {
        let Ok(out) = std::env::var("APEXROUTER_TEST_SPAWN_OUT") else {
            return;
        };
        let dir = std::path::PathBuf::from(&out)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir);
        let bin = tricky_binary(&dir);
        let args = sleeper_args();
        let env = sleeper_env(60_000);
        let spawned = spawn_detached(SpawnRequest {
            program: &bin,
            args: &args,
            env: &env,
            cwd: &dir,
            log: &dir.join("child.log"),
            setsid: true,
        })
        .expect("spawn_detached");
        fs::write(&out, spawned.pid.to_string()).expect("write pid");
        // Leave without waiting: that is the whole point.
        std::process::exit(0);
    }

    #[test]
    fn a_detached_child_outlives_the_process_that_spawned_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("pid");
        let exe = std::env::current_exe().expect("current_exe");
        let parent = Command::new(&exe)
            .args(["--exact", "proc::tests::helper_spawn_then_exit"])
            .env("APEXROUTER_TEST_SPAWN_OUT", &out)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn helper");
        let parent_pid = parent.id();
        let mut parent = parent;
        let status = parent.wait().expect("wait helper");
        assert!(status.success(), "helper exited {status}");

        let pid: u32 = fs::read_to_string(&out)
            .expect("helper wrote a pid")
            .trim()
            .parse()
            .expect("pid parses");

        // The parent is gone; the child is not. Its ppid has moved to a reaper — pid 1 on a
        // plain system, or the session's `PR_SET_CHILD_SUBREAPER` holder (`systemd --user`)
        // when one exists. Either way it is no longer the process that spawned it.
        let mut reparented = None;
        for _ in 0..100 {
            match ppid(pid) {
                Some(p) if p != parent_pid => {
                    reparented = Some(p);
                    break;
                }
                _ => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        let new_ppid = reparented.unwrap_or_else(|| panic!("pid {pid} was never reparented"));
        assert_ne!(new_ppid, parent_pid);
        assert_ne!(new_ppid, std::process::id());

        let raw = fs::read_to_string(proc_path(pid, "stat")).expect("child still has a stat");
        let f = fields_after_comm(&raw).expect("fields");
        assert_ne!(f[STAT_IDX_STATE], "Z", "the child is alive, not a zombie");

        // tidy up: it is not our child, so signal it by verified identity.
        let facts = identify(pid, &cmdline(pid).expect("cmdline"), "").expect("identify");
        let _ = signal_verified(&facts, Signal::Kill);
    }
}
