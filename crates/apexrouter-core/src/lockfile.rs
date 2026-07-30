//! OWNER: unit C-05 (core/lockfile.rs). Do not edit outside that unit.
//!
//! Daemon ownership: `flock` plus an owner record.
//!
//! "Is there an owner?" is **one syscall**. The kernel drops the lock on process death, so
//! there is no stale-owner class. The `File` is `O_CLOEXEC` (Rust's `File` is), so a
//! spawned `llama-server` can never inherit it and render the daemon permanently
//! unstartable — there is an explicit regression test for exactly that.
//!
//! Three rules govern who may touch `$STATE/apexrouterd.lock`:
//!
//! * the **daemon** takes it `LOCK_EX|LOCK_NB` for its whole lifetime ([`DaemonLock::acquire`])
//!   and writes the owner record into it ([`DaemonLock::write_owner`]);
//! * **offline writers** (`migrate --apply`, `config init`) take it the same way
//!   ([`acquire_offline_exclusive`]) and thereby *prove* no daemon is running;
//! * **offline readers** never take it — they coordinate on `$STATE/state.lock` with
//!   `LOCK_SH` (see `Store::with_state_lock_shared`). The single exception is [`probe`],
//!   which takes and immediately releases the lock because that momentary attempt *is* the
//!   liveness question, and asking it any other way would reintroduce stale pid files.
//!
//! Note that `flock` locks belong to the *open file description*, not to the process, so a
//! second `open()` of the same path inside the same process conflicts just like another
//! process would. That is what makes [`probe`] honest.

use crate::error::{Error, Result};
use crate::paths::Paths;
use rustix::fs::{flock, FlockOperation};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Mode the lock file is created with. It carries a URL and a pid, not key material, but
/// everything under `$STATE` is `0600` by house rule (ARCHITECTURE §5.3).
const LOCK_MODE: u32 = 0o600;

/// Held for the daemon's lifetime. Released by process exit.
#[derive(Debug)]
pub struct DaemonLock {
    /* C-05 */
    file: File,
}

/// What the lock file contains, so a second starter can name the first by pid and URL.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerRecord {
    /// The owning process.
    pub pid: u32,
    /// `/proc/<pid>/stat` field 22, so pid reuse is detectable.
    pub start_time_ticks: u64,
    /// `/proc/sys/kernel/random/boot_id`.
    pub boot_id: String,
    /// Which version is holding it.
    pub version: String,
    /// Where the data plane is.
    pub proxy_url: String,
    /// Where the control plane is. Every client discovers this from here or `$APEXROUTER_URL`.
    pub control_url: String,
    /// Unix seconds.
    pub started_at_unix: i64,
}

/// The answer to "is a daemon running?".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DaemonProbe {
    /// Somebody holds the lock; here is who.
    Owned(OwnerRecord),
    /// Nobody holds it.
    Free,
}

impl DaemonLock {
    /// `LOCK_EX|LOCK_NB`. The `File` is `O_CLOEXEC` so a spawned `llama-server` can NEVER
    /// inherit it.
    ///
    /// # Errors
    /// [`Error::Conflict`] when somebody already owns the lock — the message names the
    /// holder from its owner record when that record is readable. [`Error::Io`] when the
    /// lock file cannot be opened at all (usually because `Paths::ensure_layout()` has not
    /// run yet).
    pub fn acquire(paths: &Paths) -> Result<DaemonLock> {
        acquire_at(&paths.daemon_lock(), Holder::Daemon)
    }

    /// Write the owner record into the locked file.
    ///
    /// Truncates first, so a shorter record can never leave the tail of a longer one
    /// behind, and `fsync`s, so a reader that got here through `flock` sees the whole
    /// thing.
    ///
    /// # Errors
    /// [`Error::Json`] if the record will not serialise, [`Error::RawIo`] on a write or
    /// `fsync` failure.
    pub fn write_owner(&mut self, rec: &OwnerRecord) -> Result<()> {
        let mut bytes = serde_json::to_vec_pretty(rec)?;
        bytes.push(b'\n');
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&bytes)?;
        self.file.sync_all()?;
        Ok(())
    }
}

/// Try `LOCK_EX|LOCK_NB`, then read the record. **Read-only callers must not use this** —
/// see `Store::with_state_lock_shared`.
///
/// The lock, if this call manages to take it, is released before the function returns; the
/// window in which a concurrently starting daemon would see it held is a few microseconds
/// wide and bounded by two syscalls.
///
/// # Errors
/// [`Error::Invalid`] when the lock is held but the owner record is missing or corrupt —
/// which is a real, transient state, because the daemon locks (startup step 2) before it
/// writes the record (step 3). [`Error::Io`] on anything else.
pub fn probe(paths: &Paths) -> Result<DaemonProbe> {
    probe_at(&paths.daemon_lock())
}

/// Offline mutations (`migrate --apply`, `config init`) take this and thereby **prove** no
/// daemon is running.
///
/// Identical mechanics to [`DaemonLock::acquire`]; only the failure message differs, because
/// the operator's next move differs — stop the daemon, rather than find out who beat you to
/// the socket.
///
/// # Errors
/// [`Error::Conflict`] when a daemon (or another offline writer) holds the lock.
pub fn acquire_offline_exclusive(paths: &Paths) -> Result<DaemonLock> {
    acquire_at(&paths.daemon_lock(), Holder::Offline)
}

// ---------------------------------------------------------------------------------------
// internals — all of these take a path so they are testable without a whole `Paths`
// ---------------------------------------------------------------------------------------

/// Which caller lost the race, so the message can tell the operator what to do about it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Holder {
    Daemon,
    Offline,
}

/// Open (creating if needed) the lock file at `0600`, read+write.
///
/// Rust's `OpenOptions` sets `O_CLOEXEC` unconditionally on Linux; `lock_file_is_cloexec`
/// asserts it rather than trusting the docs.
fn open_lock_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(LOCK_MODE)
        .open(path)
        .map_err(|source| Error::Io {
            path: path.display().to_string(),
            source,
        })
}

/// `flock(LOCK_EX|LOCK_NB)`. `Ok(true)` = we hold it, `Ok(false)` = somebody else does.
fn try_lock_exclusive(file: &File, path: &Path) -> Result<bool> {
    match flock(file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(true),
        Err(Errno::WOULDBLOCK) => Ok(false),
        Err(e) => Err(Error::Io {
            path: path.display().to_string(),
            source: std::io::Error::from(e),
        }),
    }
}

/// Read whatever owner record the file holds. An empty or unparseable file is `None`, never
/// an error: the record is written *after* the lock is taken, so "locked but blank" is a
/// legitimate instant in every daemon's life.
fn read_owner(file: &mut File) -> Option<OwnerRecord> {
    if file.seek(SeekFrom::Start(0)).is_err() {
        return None;
    }
    let mut buf = String::new();
    if file.read_to_string(&mut buf).is_err() {
        return None;
    }
    serde_json::from_str(buf.trim()).ok()
}

/// The shared body of [`DaemonLock::acquire`] and [`acquire_offline_exclusive`].
fn acquire_at(path: &Path, who: Holder) -> Result<DaemonLock> {
    let mut file = open_lock_file(path)?;
    if try_lock_exclusive(&file, path)? {
        return Ok(DaemonLock { file });
    }
    let owner = read_owner(&mut file);
    let holder = match &owner {
        Some(r) => format!(
            "pid {} (version {}, control {}, proxy {})",
            r.pid, r.version, r.control_url, r.proxy_url
        ),
        None => "another process (it has not written its owner record yet)".to_string(),
    };
    Err(Error::Conflict(match who {
        Holder::Daemon => format!(
            "apexrouter is already running: {holder} owns {}",
            path.display()
        ),
        Holder::Offline => format!(
            "this offline change needs the daemon stopped: {holder} owns {}",
            path.display()
        ),
    }))
}

/// The body of [`probe`].
fn probe_at(path: &Path) -> Result<DaemonProbe> {
    // Deliberately does NOT create the file: a probe must leave no trace, and a missing
    // lock file is already a complete answer.
    let mut file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(DaemonProbe::Free),
        Err(source) => {
            return Err(Error::Io {
                path: path.display().to_string(),
                source,
            })
        }
    };
    if try_lock_exclusive(&file, path)? {
        // We took it, so nobody owns it. Dropping the File closes the fd, which is what
        // releases the flock — there is no window in which we hold it past this scope.
        drop(file);
        return Ok(DaemonProbe::Free);
    }
    match read_owner(&mut file) {
        Some(rec) => Ok(DaemonProbe::Owned(rec)),
        None => Err(Error::Invalid {
            what: "owner record".to_string(),
            why: format!(
                "{} is locked but holds no readable owner record; a daemon most likely \
                 started moments ago and has not written it yet",
                path.display()
            ),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// Env var carrying the lock path into the helper subprocess.
    const HELPER_ENV: &str = "APEXROUTER_TEST_LOCK_PATH";
    /// The line the helper prints once it holds the lock and has forked a child.
    const HELPER_MARKER: &str = "apexrouter-test-grandchild-pid=";

    fn rec(pid: u32) -> OwnerRecord {
        OwnerRecord {
            pid,
            start_time_ticks: 8_123_441,
            boot_id: "b6b7f4e0-0000-4000-8000-000000000000".to_string(),
            version: "0.1.0".to_string(),
            proxy_url: "http://127.0.0.1:8888".to_string(),
            control_url: "http://127.0.0.1:2739".to_string(),
            started_at_unix: 1_785_412_331,
        }
    }

    fn lock_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("apexrouterd.lock")
    }

    /// Poll `f` until it yields, or five seconds pass.
    ///
    /// `flock` is honest, but `fork()` is not polite: any other thread in this test binary
    /// that spawns a process momentarily duplicates **every** open fd — including a lock fd
    /// we are about to close — and the duplicate keeps the open file description, and hence
    /// the lock, alive. `O_CLOEXEC` disposes of it at `execve`, so the window is
    /// sub-millisecond, but libtest starts every test at once and `core` is full of units
    /// that spawn processes. Every "the lock was released" assertion therefore polls rather
    /// than racing. The daemon itself never does this: it takes the lock once, at startup,
    /// and holds it until the process dies.
    fn eventually<T>(mut f: impl FnMut() -> Option<T>) -> Option<T> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(v) = f() {
                return Some(v);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn acquire_creates_the_file_at_0600() {
        let dir = tempfile::tempdir().unwrap();
        let p = lock_path(&dir);
        assert!(!p.exists());
        let _lock = acquire_at(&p, Holder::Daemon).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "lock file must be 0600, got {mode:o}");
    }

    #[test]
    fn lock_file_is_cloexec() {
        // The whole no-stale-lock story rests on this bit. Assert the syscall, not the docs.
        let dir = tempfile::tempdir().unwrap();
        let lock = acquire_at(&lock_path(&dir), Holder::Daemon).unwrap();
        let flags = rustix::io::fcntl_getfd(&lock.file).unwrap();
        assert!(
            flags.contains(rustix::io::FdFlags::CLOEXEC),
            "the daemon lock fd must be O_CLOEXEC or a spawned llama-server inherits the lock"
        );
    }

    #[test]
    fn second_acquire_while_held_is_a_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let p = lock_path(&dir);
        let mut first = acquire_at(&p, Holder::Daemon).unwrap();
        first.write_owner(&rec(41233)).unwrap();

        let err = acquire_at(&p, Holder::Daemon).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("already running"), "{msg}");
        assert!(
            msg.contains("41233"),
            "the message must name the holder: {msg}"
        );
    }

    #[test]
    fn probe_reports_the_owner_then_free_after_release() {
        let dir = tempfile::tempdir().unwrap();
        let p = lock_path(&dir);

        // No file at all.
        assert_eq!(probe_at(&p).unwrap(), DaemonProbe::Free);
        assert!(!p.exists(), "probe must not create the lock file");

        let mut lock = acquire_at(&p, Holder::Daemon).unwrap();
        let written = rec(41233);
        lock.write_owner(&written).unwrap();
        assert_eq!(probe_at(&p).unwrap(), DaemonProbe::Owned(written));

        drop(lock);
        eventually(|| matches!(probe_at(&p), Ok(DaemonProbe::Free)).then_some(()))
            .expect("the lock outlived the DaemonLock that held it");
        // ... and probing did not leave the lock held.
        let _again =
            eventually(|| acquire_at(&p, Holder::Daemon).ok()).expect("probe() left the lock held");
    }

    #[test]
    fn probe_while_the_record_is_not_yet_written_is_an_error_not_a_lie() {
        let dir = tempfile::tempdir().unwrap();
        let p = lock_path(&dir);
        let _lock = acquire_at(&p, Holder::Daemon).unwrap(); // step 2, before step 3
        let err = probe_at(&p).unwrap_err();
        assert!(matches!(err, Error::Invalid { .. }), "{err}");
    }

    #[test]
    fn write_owner_truncates_rather_than_overwriting_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let p = lock_path(&dir);
        let mut lock = acquire_at(&p, Holder::Daemon).unwrap();

        let mut long = rec(41233);
        long.boot_id = "x".repeat(4096);
        lock.write_owner(&long).unwrap();
        let short = rec(7);
        lock.write_owner(&short).unwrap();

        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(
            !raw.contains(&"x".repeat(64)),
            "stale tail survived truncation"
        );
        let parsed: OwnerRecord = serde_json::from_str(raw.trim()).unwrap();
        assert_eq!(parsed, short);
    }

    #[test]
    fn offline_exclusive_and_daemon_lock_are_the_same_lock() {
        let dir = tempfile::tempdir().unwrap();
        let p = lock_path(&dir);
        let mut lock = acquire_at(&p, Holder::Daemon).unwrap();
        lock.write_owner(&rec(41233)).unwrap();

        let err = acquire_at(&p, Holder::Offline).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("daemon stopped"), "{msg}");
        assert!(msg.contains("41233"), "{msg}");

        drop(lock);
        // With no daemon, an offline writer gets it — that is the proof it wants.
        let _offline = eventually(|| acquire_at(&p, Holder::Offline).ok())
            .expect("an offline writer must get the lock once no daemon holds it");
    }

    /// The cheap half of the CLOEXEC regression: hold the lock, fork a long-lived child,
    /// release the lock, and re-acquire it while the child is still alive. If the child had
    /// inherited the fd, the open file description — and therefore the flock — would still
    /// be alive and this would fail.
    #[test]
    fn a_spawned_child_does_not_inherit_the_lock() {
        if !Path::new("/bin/sleep").exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let p = lock_path(&dir);

        let mut lock = acquire_at(&p, Holder::Daemon).unwrap();
        lock.write_owner(&rec(std::process::id())).unwrap();
        let mut child = Command::new("/bin/sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        drop(lock);

        // `spawn()` returns only after `execve` has succeeded, so this child's inherited
        // copies are already gone; five seconds is a courtesy to *other* test threads that
        // may be mid-fork, and is far shorter than the 30 s the child will live.
        let reacquired = eventually(|| acquire_at(&p, Holder::Daemon).ok());
        let alive = Path::new(&format!("/proc/{}", child.id())).exists();
        let _ = child.kill();
        let _ = child.wait();
        assert!(alive, "the child exited early; the test proved nothing");
        assert!(
            reacquired.is_some(),
            "a spawned child inherited the lock fd and kept it alive past its owner"
        );
    }

    /// The acceptance test for C-05: a **separate process** takes the lock, spawns a child
    /// of its own, and is then killed outright. The kernel must drop the lock at its death,
    /// and a fresh `acquire()` here must succeed even though its grandchild is still
    /// running. This is the regression that a leaked, non-CLOEXEC fd would break: the
    /// grandchild would keep the open file description — and the lock — alive forever, and
    /// the daemon would be permanently unstartable until that child died.
    #[test]
    fn parent_killed_releases_the_lock_even_with_a_surviving_child() {
        if !Path::new("/bin/sleep").exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let p = lock_path(&dir);

        let exe = std::env::current_exe().unwrap();
        let mut helper = Command::new(exe)
            .args([
                "--exact",
                "lockfile::tests::lock_holder_helper",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(HELPER_ENV, &p)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        // Read the grandchild's pid off the helper's stdout, on a thread so a wedged helper
        // times out instead of hanging the suite.
        let stdout = helper.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout)
                .lines()
                .map_while(std::result::Result::ok)
            {
                // libtest writes "test <name> ... " with no trailing newline before it runs
                // the test, so under --nocapture the marker lands mid-line. Search for it,
                // do not strip a prefix.
                if let Some(at) = line.find(HELPER_MARKER) {
                    let tail = &line[at + HELPER_MARKER.len()..];
                    let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
                    let _ = tx.send(digits.parse::<i32>().ok());
                    return;
                }
            }
            let _ = tx.send(None);
        });

        let grandchild = match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(Some(pid)) => pid,
            other => {
                let _ = helper.kill();
                let _ = helper.wait();
                panic!("the lock-holder helper never reported its child: {other:?}");
            }
        };

        // While the helper lives, we are locked out and can name it.
        let probed = probe_at(&p);
        let blocked = acquire_at(&p, Holder::Daemon);

        let helper_pid = helper.id();
        helper.kill().unwrap();
        helper.wait().unwrap();

        // The grandchild outlived its parent — that is the whole point of the test.
        let grandchild_alive = Path::new(&format!("/proc/{grandchild}")).exists();

        // The kernel drops the lock at process teardown; give it a bounded moment anyway.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut reacquired = acquire_at(&p, Holder::Daemon);
        while reacquired.is_err() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
            reacquired = acquire_at(&p, Holder::Daemon);
        }

        if let Some(pid) = rustix::process::Pid::from_raw(grandchild) {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::Kill);
        }

        match probed {
            Ok(DaemonProbe::Owned(r)) => assert_eq!(r.pid, helper_pid),
            other => panic!("probe should have named the helper, got {other:?}"),
        }
        assert!(
            blocked.is_err(),
            "acquire must fail while another process holds the lock"
        );
        assert!(
            grandchild_alive,
            "the grandchild died with its parent; the test proved nothing"
        );
        assert!(
            reacquired.is_ok(),
            "the lock survived its owner's death: {:?}",
            reacquired.err()
        );
    }

    /// Not a test. The subprocess body for
    /// [`parent_killed_releases_the_lock_even_with_a_surviving_child`]: take the lock, write
    /// an owner record, spawn a child that will outlive us, announce it, then wait to be
    /// killed. Runs only when the parent points `APEXROUTER_TEST_LOCK_PATH` at a lock file.
    #[test]
    #[ignore = "helper subprocess; driven by parent_killed_releases_the_lock_even_with_a_surviving_child"]
    fn lock_holder_helper() {
        let Ok(path) = std::env::var(HELPER_ENV) else {
            return;
        };
        let mut lock = acquire_at(Path::new(&path), Holder::Daemon).expect("helper must lock");
        lock.write_owner(&rec(std::process::id()))
            .expect("helper must write its owner record");

        // Deliberately never waited on: this process is about to be SIGKILLed and the child
        // must survive that, which is the entire point of the test above.
        #[allow(clippy::zombie_processes)]
        let child = Command::new("/bin/sleep")
            .arg("120")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("helper must spawn a child");

        println!("\n{HELPER_MARKER}{}", child.id());
        std::io::stdout().flush().expect("helper must flush");

        // Hold the lock (and this process) until the parent kills us.
        std::thread::sleep(Duration::from_secs(120));
        drop(lock);
    }
}
