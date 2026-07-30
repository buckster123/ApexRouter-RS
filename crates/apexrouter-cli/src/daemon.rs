//! OWNER: unit S-06 (cli/src/{main,cli,daemon,render}.rs and the core `cmd/*` modules).
//! Do not edit outside that unit.
//!
//! Daemon resolution — the answer to "daemon-first is annoying".
//!
//! | Class | Commands | Daemon down → |
//! |---|---|---|
//! | `Pure` | `version`, `config path/show`, `fit`, `completions` | runs; no daemon involved |
//! | `ReadState` | `status`, `rig`, `models ls`, `endpoint ls`, `route ls`, `usage`, `doctor`, … | serves from `$STATE` under `LOCK_SH`, tagged `served_by: "offline"` |
//! | `Mutate` | everything else | **autostart** (default), poll `/health` for 5 s, proceed |
//!
//! "Is a daemon running?" is answered by **one syscall** — `flock` on
//! `$STATE/apexrouterd.lock` — never by an HTTP call that has to time out first. That is
//! what makes `apexrouter status` instant on a machine where nothing is running.

use apexrouter_client::NodeClient;
use apexrouter_core::config::Config;
use apexrouter_core::lockfile::{self, DaemonProbe, OwnerRecord};
use apexrouter_core::paths::Paths;
use apexrouter_core::proc::{self, SpawnRequest};
use apexrouter_core::store::Store;
use apexrouter_protocol::ServedBy;
use std::time::{Duration, Instant};

/// How long a `Mutate` verb waits for an autostarted daemon to publish its owner record and
/// answer `/health`. ARCHITECTURE §7 says five seconds.
const AUTOSTART_DEADLINE: Duration = Duration::from_secs(5);
/// How long to wait out a daemon that holds the lock but has not written its owner record
/// yet — startup step 2 happens before step 3, so this window is real, if tiny.
const OWNER_RECORD_GRACE: Duration = Duration::from_millis(1_500);
/// Poll interval while waiting for either of the above.
const POLL: Duration = Duration::from_millis(75);

/// What a subcommand needs in order to answer.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Need {
    /// Nothing but this process.
    Pure,
    /// A picture of the world, which `$STATE` can supply when no daemon is running.
    ReadState,
    /// A daemon, because the operation changes something.
    Mutate,
}

/// Where this invocation's answers will come from.
pub enum Serving {
    /// A daemon answered.
    Daemon(NodeClient),
    /// Read from `$STATE` under `LOCK_SH`. Output is tagged `served_by: "offline"`.
    Offline(Store),
    /// Neither was possible, and here is why.
    None(anyhow::Error),
}

impl Serving {
    /// What goes in the `--json` envelope's `served_by`.
    pub fn served_by(&self) -> ServedBy {
        match self {
            Serving::Daemon(_) => ServedBy::Daemon,
            Serving::Offline(_) | Serving::None(_) => ServedBy::Offline,
        }
    }

    /// True when the answer will come off disk rather than from a running daemon.
    pub fn is_offline(&self) -> bool {
        matches!(self, Serving::Offline(_))
    }

    /// The daemon, or the reason there is not one.
    ///
    /// This is how a `Mutate` verb turns [`Serving::None`] into the clean error that
    /// `--no-autostart` promises.
    ///
    /// # Errors
    /// The carried reason for [`Serving::None`]; a "needs the daemon" error for
    /// [`Serving::Offline`].
    pub fn into_daemon(self) -> anyhow::Result<NodeClient> {
        match self {
            Serving::Daemon(c) => Ok(c),
            Serving::None(e) => Err(e),
            Serving::Offline(_) => Err(anyhow::anyhow!(
                "this command needs a running daemon: start one with `apexrouter serve --detach`"
            )),
        }
    }
}

/// Resolve how this invocation will be served, autostarting when the need is `Mutate` and
/// `autostart` is on. Two racing autostarts converge on one daemon.
///
/// # Errors
/// Only for a failure that makes *both* paths impossible — a `$STATE` directory that
/// cannot be created, say. "No daemon and the command needs one" is
/// `Ok(`[`Serving::None`]`)`, so the caller decides whether that is fatal.
pub async fn resolve_serving(
    need: Need,
    cfg: &Config,
    paths: &Paths,
    autostart: bool,
) -> anyhow::Result<Serving> {
    if need == Need::Pure {
        return Ok(Serving::Offline(Store::new(paths.clone())));
    }
    paths.ensure_layout()?;

    if let Some(client) = existing_daemon(cfg, paths)? {
        return Ok(Serving::Daemon(client));
    }

    match need {
        Need::Pure | Need::ReadState => Ok(Serving::Offline(Store::new(paths.clone()))),
        Need::Mutate => {
            if !autostart {
                return Ok(Serving::None(anyhow::anyhow!(
                    "apexrouterd is not running and --no-autostart was given: \
                     start it with `apexrouter serve --detach`"
                )));
            }
            if !cfg.server.autostart {
                return Ok(Serving::None(anyhow::anyhow!(
                    "apexrouterd is not running and [server] autostart = false: \
                     start it with `apexrouter serve --detach`"
                )));
            }
            match autostart_daemon(cfg, paths).await {
                Ok(client) => Ok(Serving::Daemon(client)),
                Err(e) => Ok(Serving::None(e)),
            }
        }
    }
}

/// A client for a daemon that is **already** running, or `None`.
///
/// `$APEXROUTER_URL` wins and is trusted: it is how a CLI on one box talks to a daemon on
/// another, where no local lock file can possibly answer the question.
///
/// # Errors
/// Propagates a lock-file I/O failure that is not "no daemon".
fn existing_daemon(cfg: &Config, paths: &Paths) -> anyhow::Result<Option<NodeClient>> {
    if let Some(url) = env_nonempty("APEXROUTER_URL") {
        return Ok(Some(NodeClient::new(trim_url(&url), token_for(cfg))));
    }
    match owner_record(paths, OWNER_RECORD_GRACE)? {
        Some(rec) => Ok(Some(client_for(&rec, cfg))),
        None => Ok(None),
    }
}

/// The daemon's owner record, waiting out the "locked but not yet written" window.
///
/// # Errors
/// Propagates an I/O failure on the lock file. A daemon that never publishes its record
/// inside `grace` is reported as absent, not as an error: the honest answer for a starter
/// that died between startup steps 2 and 3.
pub fn owner_record(paths: &Paths, grace: Duration) -> anyhow::Result<Option<OwnerRecord>> {
    let deadline = Instant::now() + grace;
    loop {
        match lockfile::probe(paths) {
            Ok(DaemonProbe::Owned(rec)) => return Ok(Some(rec)),
            Ok(DaemonProbe::Free) => return Ok(None),
            // Locked, no readable record yet: a daemon is mid-startup. Keep looking.
            Err(apexrouter_core::error::Error::Invalid { .. }) => {
                if Instant::now() >= deadline {
                    return Ok(None);
                }
                std::thread::sleep(POLL);
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// A client aimed at the control plane named by an owner record.
pub fn client_for(rec: &OwnerRecord, cfg: &Config) -> NodeClient {
    NodeClient::new(trim_url(&rec.control_url), token_for(cfg))
}

/// The bearer token, if the operator configured one.
///
/// The env var named by `[server] token_env` wins; `$APEXROUTER_TOKEN` is the fallback, so
/// a renamed var never leaves the CLI unable to talk to its own daemon.
pub fn token_for(cfg: &Config) -> Option<String> {
    env_nonempty(&cfg.server.token_env).or_else(|| env_nonempty("APEXROUTER_TOKEN"))
}

/// An env var, treating the empty string as unset.
fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Drop a trailing slash so path joins never produce `//v1`.
fn trim_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

/// Start a daemon and wait for it to be usable.
///
/// # Errors
/// When the spawn fails, or when no daemon owns the lock and answers `/health` inside
/// [`AUTOSTART_DEADLINE`]. The message points at `$STATE/logs/apexrouterd.log`, which is
/// where the detached child's stderr went.
async fn autostart_daemon(cfg: &Config, paths: &Paths) -> anyhow::Result<NodeClient> {
    let spawn_paths = paths.clone();
    let spawn = move || spawn_daemon(&spawn_paths);
    let rec = ensure_owner(paths, &spawn, AUTOSTART_DEADLINE, POLL).await?;
    let client = client_for(&rec, cfg);
    wait_healthy(&client, AUTOSTART_DEADLINE).await?;
    tracing::debug!(pid = rec.pid, control = %rec.control_url, "autostarted apexrouterd");
    Ok(client)
}

/// Wait until **somebody** owns the daemon lock, spawning one starter of our own if nobody
/// does.
///
/// This is where "two racing autostarts converge on one daemon" is decided, and it is
/// decided by the kernel: both CLIs may spawn a `serve`, but only one of those acquires
/// `flock(LOCK_EX|LOCK_NB)` — the loser exits immediately with a `Conflict` naming the
/// winner — and both CLIs then read the *same* owner record and talk to the *same* daemon.
/// No pid file, no "is it up yet" heuristic, no double start.
///
/// `spawn` is injected so that convergence can be tested without a real daemon binary.
///
/// # Errors
/// The spawn's own failure, or a timeout naming the daemon log.
pub async fn ensure_owner(
    paths: &Paths,
    spawn: &(dyn Fn() -> anyhow::Result<()> + Sync),
    deadline: Duration,
    poll: Duration,
) -> anyhow::Result<OwnerRecord> {
    let until = Instant::now() + deadline;
    let mut spawned = false;
    loop {
        match lockfile::probe(paths) {
            Ok(DaemonProbe::Owned(rec)) => return Ok(rec),
            Ok(DaemonProbe::Free) if !spawned => {
                spawn()?;
                spawned = true;
            }
            // Free-but-already-spawned, or locked with no record yet: wait, do not start a
            // second daemon.
            Ok(DaemonProbe::Free) | Err(apexrouter_core::error::Error::Invalid { .. }) => {}
            Err(e) => return Err(e.into()),
        }
        if Instant::now() >= until {
            return Err(anyhow::anyhow!(
                "apexrouterd did not come up within {}s — see {}",
                deadline.as_secs(),
                paths.logs_dir().join("apexrouterd.log").display()
            ));
        }
        tokio::time::sleep(poll).await;
    }
}

/// `apexrouter serve --foreground`, detached: its own session, stdio in
/// `$STATE/logs/apexrouterd.log`, and no tie to this process's lifetime.
///
/// `--config`/`--home` are **not** re-passed: [`crate::cli::Cli::apply_env`] has already
/// pushed them into this process's environment, which the child inherits. Env vars stay
/// the single resolution mechanism (ARCHITECTURE §5.1).
///
/// # Errors
/// When the current executable cannot be located, or the child cannot be spawned.
pub fn spawn_daemon(paths: &Paths) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let args = vec!["serve".to_string(), "--foreground".to_string()];
    let log = paths.logs_dir().join("apexrouterd.log");
    let child = proc::spawn_detached(SpawnRequest {
        program: &exe,
        args: &args,
        env: &[],
        cwd: paths.state(),
        log: &log,
        setsid: true,
    })?;
    tracing::info!(pid = child.pid, log = %log.display(), "starting apexrouterd");
    Ok(())
}

/// Poll `/health` until the daemon answers or the deadline expires.
///
/// # Errors
/// A timeout carrying the last transport error, so "it never bound" and "it answers 500"
/// are distinguishable.
pub async fn wait_healthy(client: &NodeClient, deadline: Duration) -> anyhow::Result<()> {
    let until = Instant::now() + deadline;
    let mut last: Option<String> = None;
    loop {
        match client.health().await {
            Ok(_) => return Ok(()),
            Err(e) => last = Some(e.to_string()),
        }
        if Instant::now() >= until {
            return Err(anyhow::anyhow!(
                "apexrouterd is not answering /health after {}s{}",
                deadline.as_secs(),
                last.map(|e| format!(": {e}")).unwrap_or_default()
            ));
        }
        tokio::time::sleep(POLL).await;
    }
}

#[cfg(test)]
pub(crate) mod testenv {
    //! `Paths::resolve()` reads the process environment, which `cargo test`'s thread pool
    //! shares. Every test in this binary that points `$APEXROUTER_HOME` at a temp dir takes
    //! this lock first.

    use std::sync::{Mutex, MutexGuard};

    /// The process-wide environment lock.
    pub static ENV: Mutex<()> = Mutex::new(());

    /// Take the lock, ignoring poisoning: a panicking test must not wedge the rest.
    pub fn lock() -> MutexGuard<'static, ()> {
        ENV.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    // The environment lock is a `std::sync::Mutex` held across `.await` on purpose: these
    // tests coordinate `$APEXROUTER_HOME`, which is process-global, and an async mutex
    // would not exclude the *synchronous* tests in the other modules that take the same
    // lock. Nothing here contends under load; it is one test binary.
    #![allow(clippy::await_holding_lock)]

    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Child, Command, Stdio};

    /// Env var carrying the state home into the helper subprocess.
    const HELPER_HOME: &str = "APEXROUTER_TEST_DAEMON_HOME";
    /// What the helper prints once it owns the lock and has written its record.
    const HELPER_MARKER: &str = "apexrouter-test-daemon-pid=";

    /// A `Paths` rooted at `dir`, resolved the way the binary resolves it.
    fn paths_at(dir: &std::path::Path) -> Paths {
        std::env::set_var("APEXROUTER_HOME", dir);
        std::env::remove_var("APEXROUTER_URL");
        let p = Paths::resolve().expect("paths");
        p.ensure_layout().expect("layout");
        p
    }

    /// Undo what [`paths_at`] set, so the next test starts clean.
    fn clear_env() {
        std::env::remove_var("APEXROUTER_HOME");
        std::env::remove_var("APEXROUTER_URL");
    }

    #[tokio::test]
    async fn read_state_with_nothing_running_is_served_offline() {
        let _guard = testenv::lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths_at(dir.path());

        let s = resolve_serving(Need::ReadState, &Config::default(), &paths, true)
            .await
            .expect("resolve");
        assert!(
            s.is_offline(),
            "nothing is running, so this must be offline"
        );
        assert_eq!(s.served_by(), ServedBy::Offline);
        clear_env();
    }

    #[tokio::test]
    async fn pure_never_touches_the_lock_file() {
        let _guard = testenv::lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths_at(dir.path());

        let s = resolve_serving(Need::Pure, &Config::default(), &paths, true)
            .await
            .expect("resolve");
        assert!(s.is_offline());
        assert!(
            !paths.daemon_lock().exists(),
            "a Pure verb must not so much as create the daemon lock"
        );
        clear_env();
    }

    #[tokio::test]
    async fn mutate_without_autostart_fails_cleanly_and_says_what_to_do() {
        let _guard = testenv::lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths_at(dir.path());

        let s = resolve_serving(Need::Mutate, &Config::default(), &paths, false)
            .await
            .expect("resolve");
        let err = match s {
            Serving::None(e) => e,
            _ => panic!("expected Serving::None with --no-autostart"),
        };
        let msg = err.to_string();
        assert!(msg.contains("not running"), "{msg}");
        assert!(msg.contains("apexrouter serve"), "{msg}");

        // ... and that is exactly what `into_daemon()` hands the caller.
        let again = resolve_serving(Need::Mutate, &Config::default(), &paths, false)
            .await
            .expect("resolve");
        assert!(again.into_daemon().is_err());
        clear_env();
    }

    #[tokio::test]
    async fn mutate_with_autostart_disabled_in_config_names_the_key() {
        let _guard = testenv::lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths_at(dir.path());
        let mut cfg = Config::default();
        cfg.server.autostart = false;

        match resolve_serving(Need::Mutate, &cfg, &paths, true)
            .await
            .expect("resolve")
        {
            Serving::None(e) => assert!(e.to_string().contains("autostart = false"), "{e}"),
            _ => panic!("expected Serving::None"),
        }
        clear_env();
    }

    #[tokio::test]
    async fn a_running_daemon_is_discovered_from_its_owner_record() {
        let _guard = testenv::lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths_at(dir.path());
        let mut helper = spawn_helper(dir.path());
        let pid = read_helper_pid(&mut helper);

        let rec = owner_record(&paths, Duration::from_secs(5))
            .expect("probe")
            .expect("a daemon owns the lock");
        assert_eq!(rec.pid, pid);
        assert!(rec.control_url.starts_with("http://127.0.0.1:"), "{rec:?}");

        // A ReadState verb now resolves to the daemon rather than to `$STATE`.
        let s = resolve_serving(Need::ReadState, &Config::default(), &paths, false)
            .await
            .expect("resolve");
        assert_eq!(s.served_by(), ServedBy::Daemon);

        let _ = helper.kill();
        let _ = helper.wait();
        clear_env();
    }

    /// The acceptance case: two `Mutate` verbs racing to autostart converge on ONE daemon.
    ///
    /// Both call [`ensure_owner`] with a spawn that launches a stand-in daemon; the kernel
    /// picks the winner via `flock`, the loser exits, and both callers come back with the
    /// same owner record.
    #[tokio::test]
    async fn two_racing_autostarts_converge_on_one_daemon() {
        let _guard = testenv::lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths_at(dir.path());
        let home = dir.path().to_path_buf();

        let children: std::sync::Mutex<Vec<Child>> = std::sync::Mutex::new(Vec::new());
        let spawn = || -> anyhow::Result<()> {
            let child = spawn_helper(&home);
            children.lock().expect("children").push(child);
            Ok(())
        };

        let a = ensure_owner(&paths, &spawn, Duration::from_secs(15), POLL);
        let b = ensure_owner(&paths, &spawn, Duration::from_secs(15), POLL);
        let (ra, rb) = tokio::join!(a, b);
        let (ra, rb) = (ra.expect("first racer"), rb.expect("second racer"));

        assert_eq!(ra, rb, "the two racers must land on the same daemon");

        let mut kids = children.into_inner().expect("children");
        assert!(
            kids.iter().any(|c| c.id() == ra.pid),
            "the owner must be one of the daemons we spawned"
        );
        for c in &mut kids {
            let _ = c.kill();
            let _ = c.wait();
        }
        clear_env();
    }

    #[test]
    fn token_prefers_the_configured_env_var_name() {
        let _guard = testenv::lock();
        let mut cfg = Config::default();
        cfg.server.token_env = "APEXROUTER_TEST_TOKEN".to_string();
        std::env::remove_var("APEXROUTER_TOKEN");
        std::env::set_var("APEXROUTER_TEST_TOKEN", "  sekrit  ");
        assert_eq!(token_for(&cfg).as_deref(), Some("sekrit"));

        std::env::set_var("APEXROUTER_TEST_TOKEN", "");
        std::env::set_var("APEXROUTER_TOKEN", "fallback");
        assert_eq!(token_for(&cfg).as_deref(), Some("fallback"));
        std::env::remove_var("APEXROUTER_TOKEN");
        std::env::remove_var("APEXROUTER_TEST_TOKEN");
    }

    #[test]
    fn trailing_slashes_never_become_double_slashes() {
        assert_eq!(trim_url("http://127.0.0.1:2739/"), "http://127.0.0.1:2739");
        assert_eq!(trim_url("  http://x:1  "), "http://x:1");
    }

    // ---- the stand-in daemon -----------------------------------------------------------

    /// Launch this test binary as a stand-in daemon: it takes the real daemon lock, binds a
    /// loopback port, writes a real owner record, and waits to be killed.
    fn spawn_helper(home: &std::path::Path) -> Child {
        let exe = std::env::current_exe().expect("current exe");
        Command::new(exe)
            .args([
                "--exact",
                "daemon::tests::daemon_helper",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(HELPER_HOME, home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn helper")
    }

    /// Read the pid the helper announces once it owns the lock.
    fn read_helper_pid(child: &mut Child) -> u32 {
        let stdout = child.stdout.take().expect("piped stdout");
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).expect("read helper");
            assert!(n > 0, "the helper exited without announcing itself");
            if let Some(at) = line.find(HELPER_MARKER) {
                let tail = &line[at + HELPER_MARKER.len()..];
                let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
                return digits.parse().expect("helper pid");
            }
        }
    }

    /// Not a test. The stand-in daemon body for the two tests above: everything a real
    /// `serve` does to announce itself, and nothing else.
    #[test]
    #[ignore = "helper subprocess; driven by the autostart tests"]
    fn daemon_helper() {
        let Ok(home) = std::env::var(HELPER_HOME) else {
            return;
        };
        std::env::set_var("APEXROUTER_HOME", &home);
        let paths = Paths::resolve().expect("paths");
        paths.ensure_layout().expect("layout");

        // Losing the flock is the whole point of the race: exit quietly.
        let Ok(mut lock) = apexrouter_core::lockfile::DaemonLock::acquire(&paths) else {
            return;
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        lock.write_owner(&OwnerRecord {
            pid: std::process::id(),
            start_time_ticks: proc::start_time_ticks(std::process::id()).expect("ticks"),
            boot_id: proc::boot_id().expect("boot id"),
            version: apexrouter_protocol::VERSION.to_string(),
            proxy_url: "http://127.0.0.1:8888".to_string(),
            control_url: format!("http://127.0.0.1:{port}"),
            started_at_unix: chrono::Utc::now().timestamp(),
        })
        .expect("write owner");

        println!("\n{HELPER_MARKER}{}", std::process::id());
        std::io::stdout().flush().expect("flush");
        std::thread::sleep(Duration::from_secs(60));
        drop(listener);
        drop(lock);
    }
}
