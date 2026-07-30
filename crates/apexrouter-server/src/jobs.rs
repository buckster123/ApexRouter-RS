//! OWNER: unit S-04 (server/src/api/{rig,fit,catalog,usage,requests,jobs}.rs,
//! server/src/jobs.rs). Do not edit outside that unit.
//!
//! The `?no_wait=true` job runner.
//!
//! The one rule: a job is flipped to `Failed` on **every** error path, including a
//! `JoinError` from a panicking task. **Nothing sits `Pending` forever.**
//!
//! How that rule is actually enforced, because "we remembered to write it down in every
//! error branch" is not an enforcement mechanism:
//!
//! * [`JobRegistry::spawn_with`] runs the operation in one task and then **watches that
//!   task's `JoinHandle` from a second task**. Every way the first task can end — `Ok`,
//!   `Err`, a panic, an abort, a dropped future — surfaces at the watcher, which closes the
//!   row if the operation did not close it itself. There is no "forgot a branch" path,
//!   because the watcher does not look at branches; it looks at termination.
//! * A panic's payload is recovered from `JoinError::into_panic` and put in
//!   `JobRecord::error`, so the row says *what* panicked rather than "task failed".
//! * [`JobRegistry::attach_store`] re-reads `$STATE/jobs/*.json` at startup and fails every
//!   row a previous daemon left open. A process killed with `SIGKILL` cannot run its own
//!   error paths, so the next process runs them for it.
//!
//! The failure shape every handler returns is [`crate::api::ApiError`], published by S-03.

use apexrouter_core::paths::Paths;
use apexrouter_core::store::Store;
use apexrouter_protocol::{Event, JobId, JobRecord, JobState};
use serde::Serialize;
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::broadcast;
use tokio::task::AbortHandle;

/// How many finished jobs are kept before the oldest terminal rows are dropped.
///
/// Live rows are **never** dropped: a registry full of running jobs keeps every one of them,
/// because forgetting a running job is exactly the "sits `Pending` forever" failure seen from
/// the other side.
const RETAIN_FINISHED: usize = 256;

/// Unix seconds now.
fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// True for the three states a row can never leave.
fn is_terminal(s: JobState) -> bool {
    matches!(
        s,
        JobState::Succeeded | JobState::Failed | JobState::Cancelled
    )
}

/// Every background operation, live and recently finished.
///
/// Cheap to clone: the registry is a handle onto shared state, so a spawned task can hold one
/// for as long as it needs without borrowing `AppState`.
#[derive(Clone, Debug, Default)]
pub struct JobRegistry {
    /// The shared state. `Arc` so a spawned task can own a handle.
    inner: Arc<Inner>,
}

/// One job's row plus the machinery that can stop it.
#[derive(Debug)]
struct Entry {
    /// The protocol row, which is what every surface renders.
    rec: JobRecord,
    /// Set once the task exists; `None` for a row created but never spawned.
    abort: Option<AbortHandle>,
    /// Insertion order, so `all()` is stable when two jobs share a second.
    seq: u64,
}

/// The shared half of a [`JobRegistry`].
#[derive(Debug, Default)]
struct Inner {
    /// Every row, live and terminal.
    jobs: Mutex<HashMap<JobId, Entry>>,
    /// The broadcast, once a daemon has attached one.
    tx: RwLock<Option<broadcast::Sender<Event>>>,
    /// Where `$STATE/jobs/<id>.json` lives, once a daemon has attached a store.
    ///
    /// `Paths` rather than the `Store` itself, because `Store` is not `Clone` and a
    /// `RwLock` read guard cannot be held across the file write.
    paths: RwLock<Option<Paths>>,
    /// Monotonic insertion counter.
    seq: AtomicU64,
    /// Whether [`JobRegistry::ensure_wired`] has already run.
    wired: AtomicBool,
}

/// What a running job uses to report progress.
///
/// Handed to the closure by [`JobRegistry::spawn_with`]. Holding one does not keep the job
/// alive and dropping one does not end it — it is a reporting channel, nothing more.
#[derive(Clone, Debug)]
pub struct JobHandle {
    /// The registry's shared state.
    inner: Arc<Inner>,
    /// Which job.
    id: JobId,
}

impl JobHandle {
    /// Which job this handle reports for.
    pub fn id(&self) -> JobId {
        self.id
    }

    /// Report progress. `pct` is clamped to `0.0..=100.0`; `None` leaves it alone, and an
    /// empty `message` leaves the previous message alone.
    ///
    /// Deliberately not persisted: a row is written to `$STATE/jobs/` on every *state*
    /// transition, and a download reporting 400 progress ticks must not become 400 `fsync`s.
    pub fn progress(&self, pct: Option<f32>, message: impl Into<String>) {
        let msg = message.into();
        let updated = {
            let mut jobs = self.inner.lock_jobs();
            match jobs.get_mut(&self.id) {
                Some(e) if !is_terminal(e.rec.state) => {
                    if let Some(p) = pct {
                        e.rec.pct = Some(p.clamp(0.0, 100.0));
                    }
                    if !msg.is_empty() {
                        e.rec.message = Some(msg);
                    }
                    Some(e.rec.clone())
                }
                _ => None,
            }
        };
        if let Some(rec) = updated {
            self.inner.broadcast(&rec);
        }
    }
}

impl Inner {
    /// The job map, surviving a poisoned lock: a poisoned registry means another task
    /// panicked while holding it, and turning that into a second panic would take the daemon
    /// down over a display buffer.
    fn lock_jobs(&self) -> std::sync::MutexGuard<'_, HashMap<JobId, Entry>> {
        self.jobs.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The attached state root, if any.
    fn paths(&self) -> Option<Paths> {
        match self.paths.read() {
            Ok(s) => s.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }

    /// Send `JobChanged`, if anybody is listening.
    fn broadcast(&self, rec: &JobRecord) {
        let tx = match self.tx.read() {
            Ok(s) => s.clone(),
            Err(p) => p.into_inner().clone(),
        };
        if let Some(tx) = tx {
            if tx.receiver_count() > 0 {
                let _ = tx.send(Event::JobChanged {
                    job: Box::new(rec.clone()),
                });
            }
        }
    }

    /// Write `$STATE/jobs/<id>.json`. Best effort: a job whose row cannot be persisted is
    /// still a job that ran.
    fn persist(&self, rec: &JobRecord) {
        let Some(paths) = self.paths() else { return };
        let path = paths.jobs_dir().join(format!("{}.json", rec.id));
        if let Err(e) = Store::new(paths).write_json(&path, rec) {
            tracing::warn!(job = %rec.id, error = %e, "job row not persisted");
        }
    }

    /// Apply `f` to a live row, then broadcast and persist the result.
    ///
    /// Returns `None` when the row is gone or already terminal, which is what makes every
    /// close path idempotent: the first writer wins and a later one is a no-op.
    fn transition(&self, id: JobId, f: impl FnOnce(&mut JobRecord)) -> Option<JobRecord> {
        let updated = {
            let mut jobs = self.lock_jobs();
            let e = jobs.get_mut(&id)?;
            if is_terminal(e.rec.state) {
                return None;
            }
            f(&mut e.rec);
            e.rec.clone()
        };
        if is_terminal(updated.state) {
            self.prune();
        }
        self.broadcast(&updated);
        self.persist(&updated);
        Some(updated)
    }

    /// Drop the oldest terminal rows past [`RETAIN_FINISHED`], and their files.
    fn prune(&self) {
        let dropped: Vec<JobId> = {
            let mut jobs = self.lock_jobs();
            let mut finished: Vec<(u64, JobId)> = jobs
                .iter()
                .filter(|(_, e)| is_terminal(e.rec.state))
                .map(|(id, e)| (e.seq, *id))
                .collect();
            if finished.len() <= RETAIN_FINISHED {
                return;
            }
            finished.sort_unstable();
            let cut = finished.len() - RETAIN_FINISHED;
            let dropped: Vec<JobId> = finished.into_iter().take(cut).map(|(_, id)| id).collect();
            for id in &dropped {
                jobs.remove(id);
            }
            dropped
        };
        if let Some(paths) = self.paths() {
            for id in dropped {
                let _ = std::fs::remove_file(paths.jobs_dir().join(format!("{id}.json")));
            }
        }
    }
}

impl JobRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        JobRegistry::default()
    }

    /// An empty registry that broadcasts `JobChanged`.
    pub fn with_events(tx: broadcast::Sender<Event>) -> Self {
        let r = JobRegistry::default();
        r.attach_events(tx);
        r
    }

    /// Attach the broadcast and the state root **once**, if nothing has yet.
    ///
    /// `AppState::new` (S-01's file) constructs a bare registry and the plan publishes no
    /// wiring hook, so the first control-plane route that touches the registry performs it.
    /// Idempotent, and after the first call it is one relaxed atomic load — cheap enough to
    /// sit at the top of every handler that reaches `state.jobs`.
    ///
    /// A daemon that wants the crash-recovery sweep to happen *at startup* rather than at
    /// the first API call should call [`Self::attach_events`] and [`Self::attach_store`]
    /// directly from its own startup path; this then becomes a no-op.
    pub fn ensure_wired(&self, tx: &broadcast::Sender<Event>, paths: &Paths) {
        if self.inner.wired.swap(true, Ordering::AcqRel) {
            return;
        }
        self.attach_events(tx.clone());
        self.attach_store(&Store::new(paths.clone()));
    }

    /// Start broadcasting `JobChanged` on `tx`.
    pub fn attach_events(&self, tx: broadcast::Sender<Event>) {
        match self.inner.tx.write() {
            Ok(mut slot) => *slot = Some(tx),
            Err(p) => *p.into_inner() = Some(tx),
        }
        self.inner.wired.store(true, Ordering::Release);
    }

    /// Persist rows under `$STATE/jobs/`, and **close every row a previous daemon left
    /// open**.
    ///
    /// A process killed with `SIGKILL` cannot run its own error paths. Reading its rows back
    /// and failing the ones still `Pending`/`Running` is how "nothing sits `Pending` forever"
    /// survives a crash and not only a clean exit.
    ///
    /// Returns how many stale rows were failed. Called once, at startup, before the
    /// listeners bind — the directory walk is blocking I/O.
    pub fn attach_store(&self, store: &Store) -> usize {
        let paths = store.paths().clone();
        let dir = paths.jobs_dir();
        match self.inner.paths.write() {
            Ok(mut slot) => *slot = Some(paths),
            Err(p) => *p.into_inner() = Some(paths),
        }
        self.inner.wired.store(true, Ordering::Release);
        let mut closed = 0usize;
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return 0;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(std::ffi::OsStr::to_str) != Some("json") {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let Ok(mut rec) = serde_json::from_slice::<JobRecord>(&bytes) else {
                continue;
            };
            let stale = !is_terminal(rec.state);
            if stale {
                rec.state = JobState::Failed;
                rec.error = Some(
                    "the daemon exited while this job was running; its outcome is unknown"
                        .to_owned(),
                );
                rec.finished_unix = Some(now_unix());
                closed += 1;
            }
            let seq = self.inner.seq.fetch_add(1, Ordering::Relaxed);
            {
                let mut jobs = self.inner.lock_jobs();
                jobs.insert(
                    rec.id,
                    Entry {
                        rec: rec.clone(),
                        abort: None,
                        seq,
                    },
                );
            }
            if stale {
                self.inner.persist(&rec);
            }
        }
        self.inner.prune();
        closed
    }

    /// One job's record.
    pub fn get(&self, id: JobId) -> Option<JobRecord> {
        self.inner.lock_jobs().get(&id).map(|e| e.rec.clone())
    }

    /// Every job's record, newest first.
    pub fn all(&self) -> Vec<JobRecord> {
        let mut rows: Vec<(u64, JobRecord)> = {
            let jobs = self.inner.lock_jobs();
            jobs.values().map(|e| (e.seq, e.rec.clone())).collect()
        };
        rows.sort_unstable_by_key(|(seq, _)| std::cmp::Reverse(*seq));
        rows.into_iter().map(|(_, r)| r).collect()
    }

    /// Create a `Pending` row with no task behind it.
    ///
    /// Only useful when the caller drives the lifecycle itself; prefer [`Self::spawn`], which
    /// cannot leave a row open.
    pub fn create(&self, kind: impl Into<String>) -> JobRecord {
        let rec = JobRecord {
            id: JobId::new(),
            kind: kind.into(),
            state: JobState::Pending,
            pct: None,
            message: None,
            started_unix: now_unix(),
            finished_unix: None,
            result: None,
            error: None,
        };
        let seq = self.inner.seq.fetch_add(1, Ordering::Relaxed);
        {
            let mut jobs = self.inner.lock_jobs();
            jobs.insert(
                rec.id,
                Entry {
                    rec: rec.clone(),
                    abort: None,
                    seq,
                },
            );
        }
        self.inner.broadcast(&rec);
        self.inner.persist(&rec);
        rec
    }

    /// Mark a row `Running`. A no-op once it has moved on.
    pub fn start(&self, id: JobId) -> Option<JobRecord> {
        self.inner.transition(id, |r| {
            if r.state == JobState::Pending {
                r.state = JobState::Running;
            }
        })
    }

    /// Mark a row `Succeeded`, carrying the protocol type it produced.
    ///
    /// A `result` that will not serialise **fails** the job instead: a `Succeeded` row whose
    /// payload is missing is a lie, and the caller needs to learn its type is wrong.
    pub fn succeed<T: Serialize>(&self, id: JobId, result: &T) -> Option<JobRecord> {
        match serde_json::to_value(result) {
            Ok(v) => self.inner.transition(id, |r| {
                r.state = JobState::Succeeded;
                r.pct = Some(100.0);
                r.finished_unix = Some(now_unix());
                r.result = Some(v);
            }),
            Err(e) => self.fail(id, format!("result could not be serialised: {e}")),
        }
    }

    /// Mark a row `Failed` with a reason.
    pub fn fail(&self, id: JobId, error: impl Into<String>) -> Option<JobRecord> {
        let why = error.into();
        self.inner.transition(id, |r| {
            r.state = JobState::Failed;
            r.finished_unix = Some(now_unix());
            r.error = Some(why);
        })
    }

    /// Cancel a job: mark the row `Cancelled`, **then** abort the task.
    ///
    /// That order matters. `abort()` makes the task's `JoinHandle` resolve to a cancellation
    /// `JoinError`, and the watcher would otherwise record that as a failure; marking first
    /// means the watcher finds a terminal row and leaves it alone.
    ///
    /// Returns `None` when there is no such job, or it had already finished.
    pub fn cancel(&self, id: JobId) -> Option<JobRecord> {
        let rec = self.inner.transition(id, |r| {
            r.state = JobState::Cancelled;
            r.finished_unix = Some(now_unix());
            r.message = Some("cancelled".to_owned());
        })?;
        let abort = self
            .inner
            .lock_jobs()
            .get(&id)
            .and_then(|e| e.abort.clone());
        if let Some(a) = abort {
            a.abort();
        }
        Some(rec)
    }

    /// Run `fut` as a job and return its row immediately.
    ///
    /// The row is `Failed` on **every** way the future can end badly, including a panic.
    pub fn spawn<Fut, T>(&self, kind: impl Into<String>, fut: Fut) -> JobRecord
    where
        Fut: Future<Output = anyhow::Result<T>> + Send + 'static,
        T: Serialize + Send + 'static,
    {
        self.spawn_with(kind, move |_| fut)
    }

    /// [`Self::spawn`], with a [`JobHandle`] for progress reporting.
    ///
    /// Two tasks are spawned, not one. The first runs the operation; the second awaits the
    /// first's `JoinHandle` and closes the row if the operation did not. A panic inside the
    /// operation therefore lands as `Failed` with the panic message, which is the one error
    /// path a hand-written `match` always forgets.
    pub fn spawn_with<F, Fut, T>(&self, kind: impl Into<String>, f: F) -> JobRecord
    where
        F: FnOnce(JobHandle) -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<T>> + Send + 'static,
        T: Serialize + Send + 'static,
    {
        let rec = self.create(kind);
        let id = rec.id;

        let run = self.clone();
        let handle = JobHandle {
            inner: Arc::clone(&self.inner),
            id,
        };
        let task = tokio::spawn(async move {
            run.start(id);
            match f(handle).await {
                Ok(v) => {
                    run.succeed(id, &v);
                }
                Err(e) => {
                    run.fail(id, format!("{e:#}"));
                }
            }
        });

        let abort = task.abort_handle();
        {
            let mut jobs = self.inner.lock_jobs();
            if let Some(e) = jobs.get_mut(&id) {
                e.abort = Some(abort);
            }
        }

        // The watcher. Whatever happens to the task above, this closes the row.
        let watch = self.clone();
        tokio::spawn(async move {
            match task.await {
                // The body always closes the row itself; this catches a future refactor
                // that stops doing so, and is a no-op in the normal case.
                Ok(()) => {
                    watch.fail(id, "the job task finished without recording an outcome");
                }
                Err(join) if join.is_panic() => {
                    watch.fail(id, format!("job panicked: {}", panic_message(join)));
                }
                // Aborted. `cancel()` already wrote a terminal row, so this is a no-op
                // there and a safety net when the abort came from somewhere else.
                Err(_) => {
                    watch.fail(id, "the job task was aborted");
                }
            }
        });

        rec
    }
}

/// Recover a panic payload as text. `Box<dyn Any>` is `&str` or `String` in practice.
fn panic_message(join: tokio::task::JoinError) -> String {
    let payload = join.into_panic();
    if let Some(s) = payload.downcast_ref::<&str>() {
        return (*s).to_owned();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "panicked with a non-string payload".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Poll until `f` is true or the budget expires. Job closure is asynchronous by
    /// construction — the watcher is a second task — so asserting immediately would be
    /// asserting on the scheduler, not on the registry.
    async fn until(f: impl Fn() -> bool) -> bool {
        for _ in 0..400 {
            if f() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        f()
    }

    #[tokio::test]
    async fn a_successful_job_carries_its_result() {
        let jobs = JobRegistry::new();
        let rec = jobs.spawn("test.ok", async { Ok(serde_json::json!({"n": 7})) });
        assert_eq!(rec.state, JobState::Pending);
        let id = rec.id;
        assert!(until(|| jobs.get(id).map(|j| j.state) == Some(JobState::Succeeded)).await);
        let done = jobs.get(id).expect("row");
        assert_eq!(done.result, Some(serde_json::json!({"n": 7})));
        assert_eq!(done.pct, Some(100.0));
        assert!(done.finished_unix.is_some());
        assert!(done.error.is_none());
    }

    #[tokio::test]
    async fn an_erroring_job_is_failed_with_its_message() {
        let jobs = JobRegistry::new();
        let rec = jobs.spawn::<_, ()>("test.err", async {
            Err(anyhow::anyhow!("the upstream said no"))
        });
        let id = rec.id;
        assert!(until(|| jobs.get(id).map(|j| j.state) == Some(JobState::Failed)).await);
        let done = jobs.get(id).expect("row");
        assert!(
            done.error
                .as_deref()
                .unwrap_or_default()
                .contains("said no"),
            "{:?}",
            done.error
        );
    }

    /// The acceptance test named in `BUILD-PLAN.md` §4, unit S-04.
    #[tokio::test]
    async fn a_panicking_job_is_failed_not_left_pending() {
        let jobs = JobRegistry::new();
        let rec = jobs.spawn::<_, ()>("test.panic", async {
            panic!("deliberate panic inside a job");
        });
        let id = rec.id;
        assert!(
            until(|| jobs.get(id).map(|j| j.state) == Some(JobState::Failed)).await,
            "a panicking job must not sit Pending: {:?}",
            jobs.get(id)
        );
        let done = jobs.get(id).expect("row");
        assert_eq!(done.state, JobState::Failed);
        assert!(
            done.error
                .as_deref()
                .unwrap_or_default()
                .contains("deliberate panic inside a job"),
            "the panic message must reach the row: {:?}",
            done.error
        );
        assert!(done.finished_unix.is_some(), "a failed job is finished");
    }

    #[tokio::test]
    async fn a_job_that_never_returns_can_be_cancelled() {
        let jobs = JobRegistry::new();
        let rec = jobs.spawn::<_, ()>("test.forever", async {
            loop {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        let id = rec.id;
        assert!(until(|| jobs.get(id).map(|j| j.state) == Some(JobState::Running)).await);
        let cancelled = jobs.cancel(id).expect("cancelled");
        assert_eq!(cancelled.state, JobState::Cancelled);
        // The abort produces a cancellation `JoinError`; the watcher must not rewrite it.
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(jobs.get(id).map(|j| j.state), Some(JobState::Cancelled));
    }

    #[tokio::test]
    async fn a_finished_job_cannot_be_reopened() {
        let jobs = JobRegistry::new();
        let rec = jobs.create("test.manual");
        assert!(jobs.succeed(rec.id, &"done").is_some());
        assert!(jobs.fail(rec.id, "too late").is_none());
        assert_eq!(jobs.get(rec.id).map(|j| j.state), Some(JobState::Succeeded));
    }

    #[tokio::test]
    async fn progress_reaches_the_row_and_stops_at_the_end() {
        let jobs = JobRegistry::new();
        let rec = jobs.spawn_with("test.progress", |h| async move {
            h.progress(Some(42.0), "halfway");
            Ok::<_, anyhow::Error>(())
        });
        let id = rec.id;
        assert!(until(|| jobs.get(id).map(|j| j.state) == Some(JobState::Succeeded)).await);
        let done = jobs.get(id).expect("row");
        assert_eq!(done.message.as_deref(), Some("halfway"));
        assert_eq!(done.pct, Some(100.0));
    }

    #[tokio::test]
    async fn all_is_newest_first() {
        let jobs = JobRegistry::new();
        let a = jobs.create("a");
        let b = jobs.create("b");
        let c = jobs.create("c");
        let ids: Vec<JobId> = jobs.all().into_iter().map(|j| j.id).collect();
        assert_eq!(ids, vec![c.id, b.id, a.id]);
    }

    #[tokio::test]
    async fn a_job_change_reaches_the_broadcast() {
        let (tx, mut rx) = broadcast::channel(16);
        let jobs = JobRegistry::with_events(tx);
        let rec = jobs.create("test.event");
        let ev = rx.recv().await.expect("event");
        match ev {
            Event::JobChanged { job } => assert_eq!(job.id, rec.id),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_result_that_will_not_serialise_fails_the_job() {
        /// A type whose `Serialize` always errors.
        struct Bad;
        impl Serialize for Bad {
            fn serialize<S: serde::Serializer>(&self, _s: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("nope"))
            }
        }
        let jobs = JobRegistry::new();
        let rec = jobs.create("test.badresult");
        jobs.succeed(rec.id, &Bad);
        let done = jobs.get(rec.id).expect("row");
        assert_eq!(done.state, JobState::Failed);
    }
}
