//! OWNER: unit P-01 (providers/src/local/**). Do not edit outside that unit.
//!
//! Spawning, health-gating, log rotation and stopping a local `llama-server`.
//!
//! * `setsid` + facts-on-disk **before** the spawn function returns, so a crash, a
//!   `systemctl --user restart` or a `cargo install` never evicts a model that took 90
//!   seconds and 6 GB to load.
//! * The health gate has a real wall-clock deadline that **resets on observed progress** —
//!   a 503 `{"status":"loading model"}` or a recognised load line means it is alive and
//!   working.
//! * Log rotation uses **copytruncate** semantics: an adopted child holds an fd to that
//!   inode, so renaming would send its output into a deleted file. Children we did not spawn
//!   are never rotated.

use apexrouter_core::config::EndpointsCfg;
use apexrouter_core::discover;
use apexrouter_core::error::{Error, Result};
use apexrouter_core::proc::{liveness, Liveness};
use apexrouter_core::upstream::{self, UpstreamProbe};
use apexrouter_protocol::{BootPhase, Gpu, ProcFacts, RigSnapshot};
use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How much of a log file the tail reader will pull into memory. A `llama-server` crash
/// message and the twenty lines around it are always inside this.
const TAIL_WINDOW_BYTES: u64 = 256 * 1024;

/// How long to keep asking `waitpid` for a child that has been signalled.
const REAP_BUDGET: Duration = Duration::from_millis(2_000);

// ---------------------------------------------------------------------------------------
// Port reservations
// ---------------------------------------------------------------------------------------

/// A port held out of the allocator until the health gate settles.
///
/// The bind probe alone is not enough: between "this port answered without `EADDRINUSE`"
/// and "the child has actually bound it" there is a window of tens of seconds, and two
/// concurrent launches would otherwise both pick 8100. The lease closes that window — the
/// port stays reserved for as long as this value is alive, and is released on `Drop`
/// whether the launch succeeded, failed or unwound.
pub struct PortLease {
    port: u16,
    held: Arc<Mutex<HashSet<u16>>>,
}

impl PortLease {
    /// The reserved port.
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl fmt::Debug for PortLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PortLease")
            .field("port", &self.port)
            .finish()
    }
}

impl Drop for PortLease {
    fn drop(&mut self) {
        match self.held.lock() {
            Ok(mut held) => {
                held.remove(&self.port);
            }
            Err(poisoned) => {
                poisoned.into_inner().remove(&self.port);
            }
        }
    }
}

/// Why a port could not be reserved.
///
/// The caller turns this into a [`LaunchError::PortInUse`](super::LaunchError::PortInUse)
/// once it has looked up *who* holds it, because naming the holder is what makes the
/// message actionable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortDenied {
    /// The explicitly requested port is bound, or already leased by another launch.
    Taken(u16),
    /// Every port in `[endpoints] port_range` is bound or leased.
    RangeExhausted(u16, u16),
}

/// Reserve a port under the shared reservation set.
///
/// `want` is honoured exactly when it is free; there is no silent substitution, because an
/// operator who typed `--port 8100` and got 8101 has been lied to. `None` allocates the
/// first port of `range` that is neither bound nor already leased.
///
/// # Errors
/// [`PortDenied::Taken`] when `want` is unavailable, [`PortDenied::RangeExhausted`] when
/// the whole pool is.
pub fn reserve_port(
    held: &Arc<Mutex<HashSet<u16>>>,
    range: (u16, u16),
    want: Option<u16>,
) -> std::result::Result<PortLease, PortDenied> {
    // A poisoned lock still describes a valid reservation set: the only writers are this
    // function and `Drop`, neither of which can leave it half-updated.
    let mut set = match held.lock() {
        Ok(s) => s,
        Err(poisoned) => poisoned.into_inner(),
    };
    let taken: Vec<u16> = set.iter().copied().collect();

    let port = match want {
        Some(p) => {
            if taken.contains(&p) || !apexrouter_core::proc::port_free(p) {
                return Err(PortDenied::Taken(p));
            }
            p
        }
        None => apexrouter_core::proc::alloc_port(range, &taken)
            .ok_or(PortDenied::RangeExhausted(range.0, range.1))?,
    };
    set.insert(port);
    Ok(PortLease {
        port,
        held: Arc::clone(held),
    })
}

// ---------------------------------------------------------------------------------------
// Logs
// ---------------------------------------------------------------------------------------

/// The last `n` lines of a log file.
///
/// Never fails: a log we cannot read is an empty tail, not a second error stacked on top of
/// the one the operator is already looking at.
pub fn log_tail(path: &Path, n: usize) -> Vec<String> {
    if n == 0 {
        return Vec::new();
    }
    let Ok(mut f) = fs::File::open(path) else {
        return Vec::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let from = len.saturating_sub(TAIL_WINDOW_BYTES);
    if f.seek(SeekFrom::Start(from)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_owned)
        .collect();
    // A window that started mid-file almost certainly started mid-line.
    if from > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    let skip = lines.len().saturating_sub(n);
    lines.split_off(skip)
}

/// Rotate a log **in place**, with copytruncate semantics.
///
/// An adopted child holds an `O_APPEND` fd to this inode; renaming the file would send the
/// rest of its output into a deleted inode and the operator would watch an empty file
/// forever. So the bytes are copied out to `<log>.1` and the original is truncated to zero
/// while keeping its inode. `O_APPEND` writers seek to the (new) end on every write, so no
/// NUL hole appears.
///
/// Returns whether a rotation happened. A `max_bytes` of 0 disables rotation.
///
/// # Errors
/// Returns [`Error::Io`] when the copy or the truncate fails.
pub fn rotate_log(path: &Path, max_bytes: u64) -> Result<bool> {
    if max_bytes == 0 {
        return Ok(false);
    }
    let len = match fs::metadata(path) {
        Ok(m) => m.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(Error::Io {
                path: path.display().to_string(),
                source,
            })
        }
    };
    if len < max_bytes {
        return Ok(false);
    }

    let previous = PathBuf::from(format!("{}.1", path.display()));
    fs::copy(path, &previous).map_err(|source| Error::Io {
        path: previous.display().to_string(),
        source,
    })?;
    let f = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|source| Error::Io {
            path: path.display().to_string(),
            source,
        })?;
    f.set_len(0).map_err(|source| Error::Io {
        path: path.display().to_string(),
        source,
    })?;
    Ok(true)
}

// ---------------------------------------------------------------------------------------
// Boot progress
// ---------------------------------------------------------------------------------------

/// Map one `llama-server` log line onto the boot state machine.
///
/// Only lines that mean *forward motion* return `Some`. That matters more than it looks:
/// the return value is what resets the health gate's wall-clock deadline, so ordinary
/// chatter must not buy the child another full deadline. `Failed` is returned for the load
/// errors llama.cpp prints before exiting, so the gate can stop waiting for a process that
/// has already given up.
pub fn phase_from_line(line: &str) -> Option<BootPhase> {
    let l = line.trim();
    if l.is_empty() {
        return None;
    }
    let lower = l.to_ascii_lowercase();

    if lower.contains("server is listening") || lower.contains("starting the main loop") {
        return Some(BootPhase::Healthy);
    }
    if lower.contains("failed to load model")
        || lower.contains("error loading model")
        || lower.contains("unable to load model")
    {
        return Some(BootPhase::Failed {
            reason: l.to_owned(),
        });
    }
    if lower.starts_with("load_tensors")
        || lower.starts_with("llama_context")
        || lower.starts_with("llama_kv_cache")
        || lower.starts_with("llama_model_loader")
        || lower.contains("loading model")
        || lower.contains("srv    load_model")
    {
        return Some(BootPhase::Loading {
            pct: load_pct(&lower),
        });
    }
    None
}

/// `load_tensors: ... 47%` → `Some(47.0)`. llama.cpp does not always print one.
fn load_pct(lower: &str) -> Option<f32> {
    let idx = lower.find('%')?;
    let digits: String = lower[..idx]
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let digits: String = digits.chars().rev().collect();
    digits
        .parse::<f32>()
        .ok()
        .filter(|p| (0.0..=100.0).contains(p))
}

// ---------------------------------------------------------------------------------------
// The health gate
// ---------------------------------------------------------------------------------------

/// Everything the health gate watches.
#[derive(Clone)]
pub struct GateInput<'a> {
    /// The client used for `/health`. Pooling should be off: a gate that leaves an idle
    /// socket per attempt turns "ten starts in a loop" into an fd leak.
    pub http: &'a reqwest::Client,
    /// The child's base URL, **without** a trailing `/v1`.
    pub base_url: &'a str,
    /// Its log, tailed for progress markers.
    pub log: &'a Path,
    /// Its identity, re-verified on every tick so a crash is noticed immediately.
    pub facts: &'a ProcFacts,
    /// Wall clock allowed **since the last observed progress**, not since the start.
    pub deadline: Duration,
    /// How often to probe.
    pub interval: Duration,
}

/// How the gate ended. Two of the three outcomes are the caller's stop path.
#[derive(Debug)]
pub enum GateOutcome {
    /// `/health` answered 200. The probe is carried out so the caller need not repeat it.
    Ready(Box<UpstreamProbe>),
    /// The deadline expired with no progress. The caller must kill, unrecord and report.
    Timeout,
    /// The child is gone.
    Exited {
        /// Exit code, when `waitpid` still had one to collect. Negative values are the
        /// negated terminating signal.
        code: Option<i32>,
    },
}

/// Wait for a freshly spawned child to answer, streaming boot phases as it goes.
///
/// The deadline is real wall clock and it **resets on observed progress**: a 503
/// `{"status":"loading model"}`, or a new recognised line in the log, means the child is
/// alive and working and has earned another full budget. That is the whole difference
/// between this and the "60 second loop that can run 4 minutes and then orphans the child"
/// it replaces — the loop below can only end in `Ready`, `Timeout` or `Exited`.
///
/// `on_phase` is called once per *transition*, never per tick, so the WS stream does not
/// carry a redundant `BootProgress` every three seconds.
pub async fn health_gate(
    input: GateInput<'_>,
    on_phase: &(dyn Fn(BootPhase, Option<String>) + Send + Sync),
) -> GateOutcome {
    let mut last_progress = Instant::now();
    let mut log_at: u64 = 0;
    let mut phase = BootPhase::Loading { pct: None };
    on_phase(phase.clone(), None);

    loop {
        // ---- has it died? --------------------------------------------------------------
        // Checked first: a child that exited two seconds ago must not be waited on for the
        // rest of the deadline.
        match liveness(input.facts) {
            Liveness::Alive => {}
            Liveness::Zombie | Liveness::Dead => {
                let code = reap(input.facts.pid);
                return GateOutcome::Exited { code };
            }
            // `EPERM` under `hidepid`, an unparsable `stat`: we cannot see it, which is not
            // the same as it being dead. Keep probing over HTTP, which can still answer.
            Liveness::Unknown(e) => {
                tracing::debug!(pid = input.facts.pid, error = %e, "liveness unreadable");
            }
        }

        // ---- the readiness gate --------------------------------------------------------
        let probe = upstream::probe(input.http, input.base_url, None, input.interval).await;
        if probe.healthy {
            on_phase(BootPhase::Healthy, None);
            return GateOutcome::Ready(Box::new(probe));
        }
        if probe.loading {
            // Alive and working. This is progress, and it buys a full new budget.
            last_progress = Instant::now();
            if !matches!(phase, BootPhase::Loading { .. }) {
                phase = BootPhase::Loading { pct: None };
                on_phase(
                    phase.clone(),
                    Some("upstream reports loading model".to_owned()),
                );
            }
        }

        // ---- the log -------------------------------------------------------------------
        let (lines, new_at) = read_from(input.log, log_at);
        log_at = new_at;
        for line in lines {
            let Some(next) = phase_from_line(&line) else {
                continue;
            };
            last_progress = Instant::now();
            if next != phase {
                phase = next.clone();
                on_phase(next, Some(line));
            }
        }

        if last_progress.elapsed() >= input.deadline {
            return GateOutcome::Timeout;
        }
        tokio::time::sleep(input.interval).await;
    }
}

/// Read whatever was appended to `path` since byte `from`, and the new end offset.
///
/// A file that shrank (somebody rotated it under us) restarts from zero rather than
/// returning garbage. A trailing partial line is left for the next pass.
fn read_from(path: &Path, from: u64) -> (Vec<String>, u64) {
    let Ok(mut f) = fs::File::open(path) else {
        return (Vec::new(), from);
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = if len < from { 0 } else { from };
    if start == len {
        return (Vec::new(), len);
    }
    if f.seek(SeekFrom::Start(start)).is_err() {
        return (Vec::new(), start);
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return (Vec::new(), start);
    }
    let text = String::from_utf8_lossy(&buf);
    let end = text.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let lines: Vec<String> = text[..end]
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_owned)
        .collect();
    (lines, start + end as u64)
}

// ---------------------------------------------------------------------------------------
// Reaping
// ---------------------------------------------------------------------------------------

/// Reap a child we spawned, so a supervisor that runs for weeks does not accumulate
/// zombies.
///
/// `core::proc::spawn_detached` drops the `std::process::Child` on purpose — the child is
/// not tied to our lifetime — which leaves us as its parent and therefore responsible for
/// `waitpid` while we are alive. Once we exit, the child is reparented and reaped by init.
///
/// Returns the exit code when one was collected; a negative value is the negated
/// terminating signal. A pid that is not ours, or that has already been reaped, yields
/// `None` and is not an error.
pub fn reap(pid: u32) -> Option<i32> {
    let raw = i32::try_from(pid).ok()?;
    let pid = rustix::process::Pid::from_raw(raw)?;
    let deadline = Instant::now() + REAP_BUDGET;
    loop {
        match rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG) {
            Ok(Some(status)) => {
                return status
                    .exit_status()
                    .and_then(|c| i32::try_from(c).ok())
                    .or_else(|| {
                        status
                            .terminating_signal()
                            .and_then(|s| i32::try_from(s).ok())
                            .map(|s| -s)
                    });
            }
            // Not our child, or already reaped by somebody else.
            Err(_) => return None,
            // Still running: it was signalled a moment ago and has not finished unwinding.
            Ok(None) => {
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// The rig
// ---------------------------------------------------------------------------------------

/// Scan the machine: every `llama-server` build, every device each of them enumerates, and
/// the host's memory situation.
///
/// Devices are merged across builds by their `-dev` token and every build that could see
/// one is recorded in `seen_by_builds` — on a box with a Vulkan build and a ROCm build the
/// same GPU appears under two names, and pretending otherwise double-counts its VRAM.
///
/// # Errors
/// Propagates [`discover::discover_builds`]. A build that will not run is reported with no
/// devices rather than failing the whole scan.
pub async fn scan_rig(cfg: &EndpointsCfg, cache: &Path) -> Result<RigSnapshot> {
    let builds = discover::discover_builds(cfg, cache).await?;

    let mut gpus: Vec<Gpu> = Vec::new();
    for build in &builds {
        let found = discover::probe_devices(Path::new(&build.server_path))
            .await
            .unwrap_or_default();
        for mut gpu in found {
            match gpus.iter_mut().find(|g| g.device == gpu.device) {
                Some(existing) => {
                    if !existing.seen_by_builds.contains(&build.id) {
                        existing.seen_by_builds.push(build.id.clone());
                    }
                }
                None => {
                    if !gpu.seen_by_builds.contains(&build.id) {
                        gpu.seen_by_builds.push(build.id.clone());
                    }
                    gpus.push(gpu);
                }
            }
        }
    }

    let mem = meminfo();
    Ok(RigSnapshot {
        gpus,
        builds,
        ram_total_mb: mem.ram_total_mb,
        ram_free_mb: mem.ram_free_mb,
        swap_total_mb: mem.swap_total_mb,
        swap_used_mb: mem.swap_used_mb,
        cpu_threads: std::thread::available_parallelism()
            .map(|n| u32::try_from(n.get()).unwrap_or(u32::MAX))
            .unwrap_or(1),
        scanned_at_unix: chrono::Utc::now().timestamp(),
    })
}

/// What `/proc/meminfo` told us, in MiB.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct MemInfo {
    ram_total_mb: u64,
    ram_free_mb: u64,
    swap_total_mb: u64,
    swap_used_mb: u64,
}

/// Parse `/proc/meminfo`. `MemAvailable` is the honest "free" number — `MemFree` on a box
/// with a warm page cache reads as almost nothing and would make every fit fail.
fn meminfo() -> MemInfo {
    let Ok(raw) = fs::read_to_string("/proc/meminfo") else {
        return MemInfo::default();
    };
    let kib = |key: &str| -> u64 {
        raw.lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
    };
    let swap_total = kib("SwapTotal:");
    let swap_free = kib("SwapFree:");
    MemInfo {
        ram_total_mb: kib("MemTotal:") / 1024,
        ram_free_mb: kib("MemAvailable:") / 1024,
        swap_total_mb: swap_total / 1024,
        swap_used_mb: swap_total.saturating_sub(swap_free) / 1024,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // ---- port leases -------------------------------------------------------------------

    #[test]
    fn two_reservations_never_hand_out_the_same_port() {
        let held = Arc::new(Mutex::new(HashSet::new()));
        let a = reserve_port(&held, (39_140, 39_160), None).expect("first");
        let b = reserve_port(&held, (39_140, 39_160), None).expect("second");
        assert_ne!(a.port(), b.port());
        assert!(held.lock().expect("lock").contains(&a.port()));
    }

    #[test]
    fn a_lease_is_released_on_drop() {
        let held = Arc::new(Mutex::new(HashSet::new()));
        let port = {
            let lease = reserve_port(&held, (39_161, 39_170), None).expect("lease");
            lease.port()
        };
        assert!(!held.lock().expect("lock").contains(&port));
        let again = reserve_port(&held, (port, port), None).expect("again");
        assert_eq!(again.port(), port);
    }

    #[test]
    fn an_exhausted_range_is_reported_as_such_not_as_a_taken_port() {
        let held = Arc::new(Mutex::new(HashSet::new()));
        let lease = reserve_port(&held, (39_171, 39_171), None).expect("only port");
        assert_eq!(
            reserve_port(&held, (39_171, 39_171), None).err(),
            Some(PortDenied::RangeExhausted(39_171, 39_171))
        );
        assert_eq!(
            reserve_port(&held, (39_171, 39_171), Some(lease.port())).err(),
            Some(PortDenied::Taken(39_171))
        );
    }

    // ---- logs --------------------------------------------------------------------------

    #[test]
    fn log_tail_returns_the_last_n_lines_and_never_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("e.log");
        let mut f = fs::File::create(&log).expect("create");
        for i in 0..500 {
            writeln!(f, "line {i}").expect("write");
        }
        drop(f);

        assert_eq!(
            log_tail(&log, 3),
            vec![
                "line 497".to_owned(),
                "line 498".to_owned(),
                "line 499".to_owned()
            ]
        );
        assert!(log_tail(&dir.path().join("absent.log"), 5).is_empty());
        assert!(log_tail(&log, 0).is_empty());
    }

    #[test]
    fn rotation_is_copytruncate_so_an_appending_child_keeps_the_same_inode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("e.log");
        fs::write(&log, vec![b'x'; 4096]).expect("write");

        // A child holds an O_APPEND fd, exactly as a spawned llama-server does.
        let mut child_fd = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .expect("open");

        assert!(rotate_log(&log, 1024).expect("rotate"));
        assert_eq!(fs::metadata(&log).expect("meta").len(), 0);
        assert_eq!(
            fs::metadata(dir.path().join("e.log.1"))
                .expect("meta")
                .len(),
            4096
        );

        // The still-open fd writes into the SAME file, which is the whole point.
        writeln!(child_fd, "after rotation").expect("write");
        assert_eq!(
            fs::read_to_string(&log).expect("read"),
            "after rotation\n",
            "renaming instead of truncating would have lost this line into a deleted inode"
        );
    }

    #[test]
    fn rotation_does_nothing_below_the_threshold_or_when_disabled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("e.log");
        fs::write(&log, b"small").expect("write");
        assert!(!rotate_log(&log, 1024).expect("under"));
        assert!(!rotate_log(&log, 0).expect("disabled"));
        assert!(!rotate_log(&dir.path().join("absent.log"), 1).expect("absent"));
    }

    // ---- boot phases -------------------------------------------------------------------

    #[test]
    fn recognised_load_markers_advance_the_boot_machine() {
        assert!(matches!(
            phase_from_line("load_tensors: offloading 36 repeating layers to GPU"),
            Some(BootPhase::Loading { .. })
        ));
        assert!(matches!(
            phase_from_line("llama_context: KV self size = 594.00 MiB"),
            Some(BootPhase::Loading { .. })
        ));
        assert_eq!(
            phase_from_line("main: server is listening on http://127.0.0.1:8100"),
            Some(BootPhase::Healthy)
        );
        assert!(matches!(
            phase_from_line("common_init_from_params: failed to load model '/nope.gguf'"),
            Some(BootPhase::Failed { .. })
        ));
        // Chatter must NOT reset the deadline.
        assert_eq!(
            phase_from_line("ggml_vulkan: Found 1 Vulkan devices:"),
            None
        );
        assert_eq!(phase_from_line("   "), None);
    }

    #[test]
    fn a_percentage_is_lifted_out_of_a_load_line_when_there_is_one() {
        assert_eq!(
            phase_from_line("load_tensors: loading model tensors, this can take a while... 42%"),
            Some(BootPhase::Loading { pct: Some(42.0) })
        );
    }

    // ---- incremental log reads ----------------------------------------------------------

    #[test]
    fn incremental_log_reads_never_replay_and_never_split_a_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("e.log");
        fs::write(&log, "one\ntwo\npart").expect("write");

        let (lines, at) = read_from(&log, 0);
        assert_eq!(lines, vec!["one".to_owned(), "two".to_owned()]);
        assert_eq!(at, 8);

        let (lines, at2) = read_from(&log, at);
        assert!(lines.is_empty(), "the partial line is not acted on yet");
        assert_eq!(at2, at);

        fs::write(&log, "one\ntwo\npartial\n").expect("append");
        let (lines, _) = read_from(&log, at2);
        assert_eq!(lines, vec!["partial".to_owned()]);
    }

    #[test]
    fn a_truncated_log_restarts_from_zero_instead_of_reading_garbage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("e.log");
        fs::write(&log, "aaa\nbbb\nccc\n").expect("write");
        let (_, at) = read_from(&log, 0);
        assert_eq!(at, 12);
        // What `rotate_log` does to the inode, followed by the child's next append.
        fs::write(&log, "fresh\n").expect("truncate and append");
        let (lines, _) = read_from(&log, at);
        assert_eq!(lines, vec!["fresh".to_owned()]);
    }

    // ---- the rig ------------------------------------------------------------------------

    #[test]
    fn meminfo_reads_the_real_proc_file() {
        assert!(meminfo().ram_total_mb > 0, "/proc/meminfo has MemTotal");
    }
}
