//! OWNER: unit S-05 (server/src/{ws,assets,prober,watcher}.rs). Do not edit outside that
//! unit.
//!
//! The config watcher.
//!
//! It watches **`$CONFIG` and `$STATE/routes.json` only** — **never** a directory containing
//! endpoint logs, which children write to continuously; a recursive state-dir watch fires
//! ten times a second and there is a regression test that writes 1000 log lines and asserts
//! zero reloads.
//!
//! `notify` + a 250 ms debounce + a 10 s poll fallback, alongside `SIGHUP` and
//! `POST /v1/reload`. **A failed compile keeps the running table** and raises an `Alert`.
//!
//! # Two mechanisms, because one is not enough
//!
//! `notify` alone cannot be trusted for these two files. Every writer that matters —
//! `Store::write_atomic`, `Config::save`, `vim`, `sed -i` — writes a temp file and
//! `rename`s it into place, which replaces the inode the kernel watch is attached to. The
//! next edit is then invisible. So the watch is re-armed after every detected change *and*
//! a 10 s poll compares a SHA-256 of each file's contents. The poll is what makes the
//! watcher correct; `notify` is what makes it feel instant.
//!
//! Content hashes, not mtimes: a file that is written twice inside one filesystem timestamp
//! tick is a real thing on a laptop, and an mtime that goes backwards (a restored backup, a
//! `git checkout`) must still count as a change.

use crate::state::AppState;
use apexrouter_core::paths::Paths;
use apexrouter_protocol::{AlertLevel, Event};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

/// How long a burst of filesystem events is coalesced before anything is reparsed.
///
/// An atomic write is at least a create, a write and a rename; an editor adds a backup file
/// and a `chmod`. Reloading once per burst instead of once per event is the difference
/// between one recompile and five.
pub const DEBOUNCE: Duration = Duration::from_millis(250);

/// How often the files are re-hashed regardless of what `notify` said.
pub const POLL_INTERVAL: Duration = Duration::from_secs(10);

/// Run for the daemon's lifetime, reloading config and routes on change.
pub async fn config_watcher(state: Arc<AppState>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let watcher = match arm(&state.paths, tx) {
        Ok(w) => Some(w),
        Err(e) => {
            // Not fatal, and not silent: the daemon keeps working, edits just take up to
            // `POLL_INTERVAL` to land instead of being instant.
            tracing::warn!(error = %e, "filesystem notifications unavailable; polling only");
            None
        }
    };
    // The watcher is moved into the loop and held for its lifetime: dropping a `notify`
    // watcher cancels every watch it holds.
    watch_loop(state, rx, watcher, DEBOUNCE, POLL_INTERVAL).await;
}

// ----------------------------------------------------------------------------------------
// what is watched
// ----------------------------------------------------------------------------------------

/// The two files, and nothing else.
pub fn watched_files(paths: &Paths) -> [PathBuf; 2] {
    [paths.config_file(), paths.routes_file()]
}

/// Every path handed to `notify`, all **non-recursive**.
///
/// The two files themselves, plus each one's parent directory — because a `rename` into
/// place is delivered to the *directory*, and a watch on a file that has just been replaced
/// is a watch on an inode nobody will ever write to again.
///
/// A parent directory is refused if it is one of the churning ones. `$STATE/logs` is where
/// children append continuously; `$STATE/endpoints` and `$STATE/jobs` are rewritten by the
/// supervisor and the job runner. None of them can ever be a parent of these two files, and
/// the check is here so that stops being an argument and becomes a test.
pub fn watch_targets(paths: &Paths) -> Vec<PathBuf> {
    let forbidden = [
        paths.logs_dir(),
        paths.endpoints_dir(),
        paths.jobs_dir(),
        paths.approvals_dir(),
    ];
    let mut out: Vec<PathBuf> = Vec::new();
    for file in watched_files(paths) {
        if let Some(parent) = file.parent() {
            let parent = parent.to_path_buf();
            if !forbidden.contains(&parent) && !out.contains(&parent) {
                out.push(parent);
            }
        }
        if !out.contains(&file) {
            out.push(file);
        }
    }
    out
}

/// Install the watches. The returned watcher must be kept alive.
///
/// A path that does not exist yet is not an error: `config.toml` is optional (a zero-config
/// install is a supported install) and its parent directory watch will see it appear.
fn arm(paths: &Paths, tx: UnboundedSender<()>) -> notify::Result<RecommendedWatcher> {
    let interesting = watched_files(paths);
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else {
            return;
        };
        // **The filter that makes the regression test pass.** A parent-directory watch on
        // `$STATE` also delivers `usage.jsonl` and `ledger.jsonl` appends; only the two
        // files we actually care about get past here, so a busy router reloads nothing.
        if ev.paths.iter().any(|p| interesting.iter().any(|w| w == p)) {
            let _ = tx.send(());
        }
    })?;
    let mut armed = 0usize;
    for target in watch_targets(paths) {
        match watcher.watch(&target, RecursiveMode::NonRecursive) {
            Ok(()) => armed += 1,
            // A missing file or directory is expected and normal.
            Err(e) => tracing::debug!(path = %target.display(), error = %e, "not watched"),
        }
    }
    if armed == 0 {
        tracing::debug!("no watch could be armed; the poll fallback carries the load");
    }
    Ok(watcher)
}

// ----------------------------------------------------------------------------------------
// the loop
// ----------------------------------------------------------------------------------------

/// What woke the loop up.
enum Wake {
    /// `notify` said something changed.
    Event,
    /// The poll deadline expired.
    Poll,
    /// The watcher went away; from here on it is polling only.
    WatcherGone,
}

/// The loop, with its two timings injected so tests do not have to wait ten seconds and can
/// exercise the poll fallback on its own by passing no watcher at all.
pub(crate) async fn watch_loop(
    state: Arc<AppState>,
    mut rx: UnboundedReceiver<()>,
    mut watcher: Option<RecommendedWatcher>,
    debounce: Duration,
    poll: Duration,
) {
    let files = watched_files(&state.paths);
    let mut seen = Fingerprints::read(&files);
    let mut watching = watcher.is_some();

    loop {
        let wake = tokio::select! {
            got = rx.recv(), if watching => match got {
                Some(()) => Wake::Event,
                None => Wake::WatcherGone,
            },
            () = tokio::time::sleep(poll) => Wake::Poll,
        };

        match wake {
            Wake::WatcherGone => {
                watching = false;
                watcher = None;
                continue;
            }
            Wake::Event => coalesce(&mut rx, debounce).await,
            Wake::Poll => {}
        }

        let now = Fingerprints::read(&files);
        let changed = now.changed_since(&seen);
        if changed == Changed::Nothing {
            continue;
        }
        seen = now;
        rearm(&state.paths, watcher.as_mut());
        apply(&state, changed).await;
    }
}

/// Swallow the rest of a burst, so one save is one reload.
async fn coalesce(rx: &mut UnboundedReceiver<()>, debounce: Duration) {
    while tokio::time::timeout(debounce, rx.recv()).await.is_ok() {
        // Another event inside the window: keep waiting. `Err` is the timeout, which is the
        // burst having ended.
    }
}

/// Re-attach the file watches after a change.
///
/// An atomic write replaces the inode, and a kernel watch follows the inode rather than the
/// name — so without this, the *second* edit of `routes.json` would only ever be found by
/// the poll. Watching an already-watched path is idempotent in `notify`, and a path that is
/// still missing is not an error worth raising.
fn rearm(paths: &Paths, watcher: Option<&mut RecommendedWatcher>) {
    let Some(w) = watcher else {
        return;
    };
    for target in watch_targets(paths) {
        if let Err(e) = w.watch(&target, RecursiveMode::NonRecursive) {
            tracing::debug!(path = %target.display(), error = %e, "could not re-arm a watch");
        }
    }
}

/// Which of the two files moved.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Changed {
    /// Neither.
    Nothing,
    /// `routes.json` only: recompile, do not reparse config.
    RoutesOnly,
    /// `config.toml`, with or without the routes: a full reload.
    Config,
}

/// A content hash of each watched file. `None` means the file is not there, which is a
/// perfectly good state for both of them and is distinct from "there and empty".
#[derive(Clone, Debug, PartialEq, Eq)]
struct Fingerprints {
    /// `config.toml`.
    config: Option<[u8; 32]>,
    /// `routes.json`.
    routes: Option<[u8; 32]>,
}

impl Fingerprints {
    /// Hash both files. Two small reads; an unreadable file hashes as absent, which makes
    /// the next successful read a change rather than a silent no-op.
    fn read(files: &[PathBuf; 2]) -> Fingerprints {
        Fingerprints {
            config: digest(&files[0]),
            routes: digest(&files[1]),
        }
    }

    /// What changed relative to `previous`.
    fn changed_since(&self, previous: &Fingerprints) -> Changed {
        if self.config != previous.config {
            Changed::Config
        } else if self.routes != previous.routes {
            Changed::RoutesOnly
        } else {
            Changed::Nothing
        }
    }
}

/// SHA-256 of a file's contents, or `None` when it cannot be read.
fn digest(path: &Path) -> Option<[u8; 32]> {
    let bytes = std::fs::read(path).ok()?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Some(h.finalize().into())
}

// ----------------------------------------------------------------------------------------
// the reload
// ----------------------------------------------------------------------------------------

/// Reparse and re-arm, through the one shared reload path.
///
/// **A failed compile keeps the running table** — that is `api::recompile`'s contract, not
/// something this module re-implements — and raises an `Alert` so it is visible in both GUIs
/// and in `apexrouter status` rather than only in a log nobody is tailing.
async fn apply(state: &Arc<AppState>, changed: Changed) {
    match changed {
        Changed::Nothing => {}
        Changed::Config => {
            tracing::info!(path = %state.paths.config_file().display(), "config changed; reloading");
            // The one reload path, shared with `SIGHUP` and `POST /v1/reload`.
            match crate::api::snapshot::reload(state) {
                Ok(report) if report.ok => {
                    sync_ui_dir(state);
                }
                Ok(report) => {
                    sync_ui_dir(state);
                    alert(
                        state,
                        AlertLevel::Serious,
                        format!(
                            "the routing table did not compile: {}; the previous table is still serving",
                            crate::api::render_issues(&report)
                        ),
                        "routes.invalid",
                    );
                }
                Err(e) => alert(
                    state,
                    AlertLevel::Serious,
                    format!("{}; the running configuration is unchanged", e.body.message),
                    "config.invalid",
                ),
            }
        }
        Changed::RoutesOnly => {
            tracing::info!(path = %state.paths.routes_file().display(), "routes changed; recompiling");
            if let Err(report) = crate::api::recompile(state) {
                alert(
                    state,
                    AlertLevel::Serious,
                    format!(
                        "the routing table did not compile: {}; the previous table is still serving",
                        crate::api::render_issues(&report)
                    ),
                    "routes.invalid",
                );
            }
        }
    }
}

/// Open or close the `[server] ui_dir` live-reload hatch to match the config just loaded.
fn sync_ui_dir(state: &Arc<AppState>) {
    let cfg = state.cfg.load();
    let dir = cfg.server.ui_dir.trim();
    crate::assets::set_ui_dir(if dir.is_empty() {
        None
    } else {
        Some(PathBuf::from(dir))
    });
}

/// Raise one alert. The id is stable, so repeats coalesce in a UI instead of stacking.
fn alert(state: &Arc<AppState>, level: AlertLevel, message: String, id: &str) {
    tracing::warn!(alert = id, "{message}");
    let _ = state.tx.send(Event::Alert {
        level,
        message,
        action: Some("reload".to_owned()),
        id: id.to_owned(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ws::testkit::{harness, Harness};
    use std::time::Duration as Dur;

    /// A short debounce and a short poll, so a test is a second rather than a minute.
    const FAST_DEBOUNCE: Dur = Dur::from_millis(20);
    /// The poll fallback, sped up by two orders of magnitude.
    const FAST_POLL: Dur = Dur::from_millis(50);

    /// Spawn the loop with fast timings and no `notify` watcher at all, so what is under
    /// test is purely the poll fallback.
    ///
    /// It returns only once the loop has taken its baseline fingerprints. Without that the
    /// tests would be a race they usually lose: `#[tokio::test]` is single-threaded, so a
    /// spawned task does not run until the test awaits, and a file written before the
    /// baseline is read is not a change at all.
    async fn spawn_polling(h: &Harness) -> tokio::task::JoinHandle<()> {
        let (_tx, rx) = mpsc::unbounded_channel();
        let state = Arc::clone(&h.state);
        let task =
            tokio::spawn(
                async move { watch_loop(state, rx, None, FAST_DEBOUNCE, FAST_POLL).await },
            );
        tokio::time::sleep(FAST_POLL / 2).await;
        task
    }

    /// Wait for the first event that matters, or give up.
    async fn wait_for(
        rx: &mut tokio::sync::broadcast::Receiver<Event>,
        want: fn(&Event) -> bool,
        within: Dur,
    ) -> Option<Event> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return None;
            }
            match tokio::time::timeout(left, rx.recv()).await {
                Ok(Ok(ev)) if want(&ev) => return Some(ev),
                Ok(Ok(_)) => {}
                Ok(Err(_)) | Err(_) => return None,
            }
        }
    }

    fn is_table_changed(ev: &Event) -> bool {
        matches!(ev, Event::RouteTableChanged { .. })
    }

    fn is_alert(ev: &Event) -> bool {
        matches!(ev, Event::Alert { .. })
    }

    // ------------------------------------------------------------------------------------
    // what is watched
    // ------------------------------------------------------------------------------------

    /// THE regression test the acceptance names: a thousand endpoint log lines, zero
    /// reloads. A recursive `$STATE` watch would fire on every one of them.
    #[tokio::test]
    async fn a_thousand_log_lines_cause_zero_reloads() {
        let h = harness();
        let mut rx = h.subscribe();
        let state = Arc::clone(&h.state);
        // The **real** watcher, with the real `notify` wiring, because the thing under test
        // is which paths it armed.
        let task = tokio::spawn(async move { config_watcher(state).await });
        tokio::time::sleep(Dur::from_millis(150)).await;

        let log = h.state.paths.logs_dir().join("local-carnice.log");
        std::fs::create_dir_all(h.state.paths.logs_dir()).expect("logs dir");
        {
            use std::io::Write as _;
            let mut f = std::fs::File::create(&log).expect("log file");
            for i in 0..1000 {
                writeln!(f, "slot update_slots: id  0 | task {i} | prompt processing")
                    .expect("write");
                if i % 100 == 0 {
                    f.flush().expect("flush");
                }
            }
            f.flush().expect("flush");
        }
        // Also churn the two other files a busy daemon rewrites constantly.
        std::fs::create_dir_all(h.state.paths.endpoints_dir()).expect("endpoints dir");
        for i in 0..50 {
            std::fs::write(
                h.state.paths.endpoints_dir().join(format!("e{i}.json")),
                b"{}",
            )
            .expect("write");
        }

        // Well past the 250 ms debounce, nowhere near the 10 s poll.
        let seen = wait_for(&mut rx, |_| true, Dur::from_millis(1200)).await;
        assert!(
            seen.is_none(),
            "writing endpoint logs must not reload anything, got {seen:?}"
        );
        task.abort();
    }

    #[tokio::test]
    async fn the_watch_targets_are_the_two_files_and_never_a_churning_directory() {
        let h = harness();
        let paths = &h.state.paths;
        let targets = watch_targets(paths);

        assert!(targets.contains(&paths.config_file()));
        assert!(targets.contains(&paths.routes_file()));
        for forbidden in [
            paths.logs_dir(),
            paths.endpoints_dir(),
            paths.jobs_dir(),
            paths.approvals_dir(),
        ] {
            assert!(
                !targets.contains(&forbidden),
                "{} must never be watched",
                forbidden.display()
            );
        }
        // Only the two files themselves, plus at most their two parents.
        assert!(targets.len() <= 4, "{targets:?}");
    }

    // ------------------------------------------------------------------------------------
    // the reload
    // ------------------------------------------------------------------------------------

    #[tokio::test]
    async fn a_new_routes_file_recompiles_the_table() {
        let h = harness();
        let mut rx = h.subscribe();
        let task = spawn_polling(&h).await;

        std::fs::write(
            h.state.paths.routes_file(),
            br#"{"schema_version":1,"default_alias":"auto","routes":[]}"#,
        )
        .expect("write");

        let ev = wait_for(&mut rx, is_table_changed, Dur::from_secs(5)).await;
        match ev {
            Some(Event::RouteTableChanged { valid, error, .. }) => {
                assert!(valid, "{error:?}");
            }
            other => panic!("expected a recompile, got {other:?}"),
        }
        task.abort();
    }

    #[tokio::test]
    async fn a_routes_file_that_will_not_parse_keeps_the_running_table_and_alerts() {
        let h = harness();
        let before = h.state.router.table().generation();
        let mut rx = h.subscribe();
        let task = spawn_polling(&h).await;

        std::fs::write(h.state.paths.routes_file(), b"{ this is not json").expect("write");

        let ev = wait_for(&mut rx, is_alert, Dur::from_secs(5)).await;
        match ev {
            Some(Event::Alert { level, id, .. }) => {
                assert_eq!(id, "routes.invalid");
                assert_eq!(level, AlertLevel::Serious);
            }
            other => panic!("expected an alert, got {other:?}"),
        }
        assert_eq!(
            h.state.router.table().generation(),
            before,
            "the running table must survive a file that does not parse"
        );
        task.abort();
    }

    #[tokio::test]
    async fn a_config_edit_is_reloaded_and_reaches_the_supervisor() {
        let h = harness();
        let mut rx = h.subscribe();
        let task = spawn_polling(&h).await;

        std::fs::write(
            h.state.paths.config_file(),
            b"[supervisor]\nhealth_interval_ms = 7777\n",
        )
        .expect("write");

        let ev = wait_for(&mut rx, is_table_changed, Dur::from_secs(5)).await;
        assert!(ev.is_some(), "a config change must re-arm the table");
        assert_eq!(
            h.state.cfg.load().supervisor.health_interval_ms,
            7777,
            "the hot config must have been swapped"
        );
        task.abort();
    }

    #[tokio::test]
    async fn a_config_that_will_not_parse_leaves_the_running_config_alone() {
        let h = harness();
        let before = h.state.cfg.load().supervisor.health_interval_ms;
        let mut rx = h.subscribe();
        let task = spawn_polling(&h).await;

        std::fs::write(h.state.paths.config_file(), b"[supervisor\nbroken = ").expect("write");

        let ev = wait_for(&mut rx, is_alert, Dur::from_secs(5)).await;
        match ev {
            Some(Event::Alert { id, .. }) => assert_eq!(id, "config.invalid"),
            other => panic!("expected a config alert, got {other:?}"),
        }
        assert_eq!(
            h.state.cfg.load().supervisor.health_interval_ms,
            before,
            "a config that does not parse must not be applied"
        );
        task.abort();
    }

    /// A reload opens and closes the `[server] ui_dir` live-reload hatch.
    ///
    /// Synchronous on purpose: `ui_dir` is a process global that the `assets` tests also
    /// write, so the two share one lock, and a `std::sync` guard must not be held across an
    /// `await`.
    #[test]
    fn the_ui_dir_hatch_follows_the_config() {
        let _g = crate::assets::ui_dir_test_lock().lock().expect("lock");
        let mut cfg = crate::ws::testkit::test_config();
        let h = crate::ws::testkit::harness_with(cfg.clone());
        let ui = h.dir.path().join("ui");
        std::fs::create_dir_all(&ui).expect("ui dir");

        cfg.server.ui_dir = ui.display().to_string();
        h.state.cfg.store(Arc::new(cfg.clone()));
        sync_ui_dir(&h.state);
        assert_eq!(crate::assets::ui_dir(), Some(ui));

        // `ui_dir = ""` is how the hatch is closed again.
        cfg.server.ui_dir = String::new();
        h.state.cfg.store(Arc::new(cfg));
        sync_ui_dir(&h.state);
        assert_eq!(crate::assets::ui_dir(), None);
    }

    /// The poll is the correctness mechanism, so it is tested without `notify` at all: the
    /// loop above is spawned with `watching = false`.
    #[tokio::test]
    async fn the_poll_fallback_finds_a_change_with_no_filesystem_events_at_all() {
        let h = harness();
        let mut rx = h.subscribe();
        let task = spawn_polling(&h).await;

        std::fs::write(
            h.state.paths.routes_file(),
            br#"{"schema_version":1,"default_alias":"auto","routes":[]}"#,
        )
        .expect("write");

        assert!(
            wait_for(&mut rx, is_table_changed, Dur::from_secs(5))
                .await
                .is_some(),
            "the poll must find a change even with no watcher"
        );
        task.abort();
    }

    /// Nothing changed, nothing reloads — no matter how many times the loop wakes up.
    #[tokio::test]
    async fn an_unchanged_pair_of_files_never_reloads() {
        let h = harness();
        let mut rx = h.subscribe();
        let task = spawn_polling(&h).await;

        std::fs::write(
            h.state.paths.routes_file(),
            br#"{"schema_version":1,"default_alias":"auto","routes":[]}"#,
        )
        .expect("write");

        // The file appearing is one change, and one reload.
        let first = wait_for(&mut rx, is_table_changed, Dur::from_secs(5)).await;
        assert!(first.is_some(), "the file appearing should reload once");
        // Many poll intervals later, with nothing written, nothing else has happened.
        let second = wait_for(&mut rx, is_table_changed, Dur::from_millis(600)).await;
        assert!(second.is_none(), "a stable file must not reload again");
        task.abort();
    }

    // ------------------------------------------------------------------------------------
    // fingerprints
    // ------------------------------------------------------------------------------------

    #[test]
    fn fingerprints_distinguish_absent_empty_and_changed() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let files = [
            dir.path().join("config.toml"),
            dir.path().join("routes.json"),
        ];

        let absent = Fingerprints::read(&files);
        assert_eq!(absent.config, None);
        assert_eq!(absent.changed_since(&absent), Changed::Nothing);

        std::fs::write(&files[0], b"").expect("write");
        let empty = Fingerprints::read(&files);
        assert_ne!(empty.config, None, "an empty file is not an absent file");
        assert_eq!(empty.changed_since(&absent), Changed::Config);

        std::fs::write(&files[1], b"{}").expect("write");
        let both = Fingerprints::read(&files);
        assert_eq!(both.changed_since(&empty), Changed::RoutesOnly);

        // Same bytes written again: not a change, whatever the mtime says.
        std::fs::write(&files[1], b"{}").expect("write");
        assert_eq!(
            Fingerprints::read(&files).changed_since(&both),
            Changed::Nothing
        );

        // A config change outranks a simultaneous routes change: the full reload does both.
        std::fs::write(&files[0], b"[server]\n").expect("write");
        std::fs::write(&files[1], b"{\"a\":1}").expect("write");
        assert_eq!(
            Fingerprints::read(&files).changed_since(&both),
            Changed::Config
        );
    }

    #[test]
    fn a_deleted_file_is_a_change_back_to_absent() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let files = [
            dir.path().join("config.toml"),
            dir.path().join("routes.json"),
        ];
        std::fs::write(&files[0], b"[server]\n").expect("write");
        let present = Fingerprints::read(&files);
        std::fs::remove_file(&files[0]).expect("remove");
        assert_eq!(
            Fingerprints::read(&files).changed_since(&present),
            Changed::Config
        );
    }
}
