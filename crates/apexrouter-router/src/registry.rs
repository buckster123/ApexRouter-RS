//! OWNER: unit R-01 (router/src/table.rs, router/src/registry.rs). Do not edit outside that
//! unit.
//!
//! Live per-backend state, **and the warm queue a sequential swap parks on**.
//!
//! **The compiled table holds `Arc<LiveBackend>` pointers, so rebuilding the table never
//! resets live state.** `upsert` on an existing id preserves the `Semaphore`, the breaker,
//! the EWMA and the in-flight count — the regression test starts three in-flight requests,
//! recompiles the table, and asserts the permit count is unchanged.
//!
//! # The warm queue (`ARCHITECTURE.md` §4.7)
//!
//! A **Sequential** swap stops A before B exists, because on a memory-constrained box two
//! 7 GB models cannot coexist. Everything that arrives in between used to get a `503` —
//! measured at 18 of them across a 72 ms window against a *fake* server, which in production
//! is the minutes a 7 GB GGUF takes to load. [`WarmRegistry`] closes that window: the swap
//! opens a [`WarmWindow`] on the alias before it drains anything, arriving requests
//! [`park`](WarmSlot::park) on a `tokio::sync::Notify` behind a bounded queue, and closing the
//! window wakes them to re-resolve against B.
//!
//! ## `warm_timeout` is patience, not a total budget
//!
//! The deadline is **re-armed on progress** ([`WarmWindow::rearm`]), for the same reason the
//! health gate's deadline is: both are waiting on one event — the replacement becoming able
//! to serve — so a park that gives up while the load is demonstrably progressing converts a
//! survivable wait into the outage. Measured before the re-arm existed: a 3000 ms window
//! against a swap that ran 12,038 ms `503`'d its four parked requests at 2977 ms, and the
//! alias then produced **74,550** `no_healthy_backend` responses over the remaining nine
//! seconds. A bound on patience is still needed for the case where progress genuinely stops,
//! which is what the deadline now measures: wall clock **since the last sign of life**.
//!
//! Four properties this file is responsible for:
//!
//! * **Invariant 2 — the request path never touches the filesystem.** Everything here is an
//!   atomic, a `Notify` and one `RwLock<HashMap>` that is never held across an `await`.
//! * **Invariant 3 — health is computed on read.** "Is this alias warming?" is not a stored
//!   flag; it is [`WarmSlot::until_ms`], a *deadline*, compared against a monotonic clock. A
//!   window whose owner died reads closed the moment it expires, so the worst a leaked one
//!   can do is bound itself. There is no string anywhere that can disagree with reality. A
//!   re-arm keeps that property: it moves the deadline, it never sets a flag.
//! * **A parked request is still accounted for.** The depth is decremented by an RAII guard
//!   inside the parking future, so a client that hangs up mid-park releases its slot; R-08
//!   wraps that with the `RequestFinished { aborted: true }` §4.3 requires.
//! * **A re-arm never wakes anybody.** Waking a parked request means sending it back to
//!   re-resolve, and during a re-arm there is by definition still nothing for it to resolve
//!   to. Only [`WarmWindow::close`] and a *new* window wake waiters.

use crate::breaker::Breaker;
use crate::limits::TokenBucket;
use apexrouter_core::config::RouterCfg;
use apexrouter_protocol::{AlertLevel, Alias, Backend, BackendId, Event};
use arc_swap::ArcSwap;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, Notify, Semaphore};

/// Smoothing factor for [`LatencyEwma`]. 0.2 keeps roughly the last five samples visible,
/// which is the shortest window in which one slow request does not look like a trend.
const EWMA_ALPHA: f64 = 0.2;

/// One backend, created once and mutated in place for the daemon's lifetime.
pub struct LiveBackend {
    /// Stable id.
    pub id: BackendId,
    /// The serialisable description.
    pub meta: ArcSwap<Backend>,
    /// Sized from `/props.total_slots`, falling back to `/slots`, falling back to config.
    pub sem: Arc<Semaphore>,
    /// Atomics only. Requires `min_volume` observations before it can open, so a single
    /// 200 ms blip on a 1 rps rig does not create a 30 s outage.
    pub breaker: Breaker,
    /// Per-backend retry budget, so a struggling backend cannot be amplified into a storm.
    pub retry_bucket: TokenBucket,
    /// The router's **own** in-flight counter — `/slots` 501s on `--no-slots` builds.
    pub inflight: AtomicU32,
    /// False while draining.
    pub accepting: AtomicBool,
    /// Rolling latency, for `LeastBusy` and the p50 columns.
    pub latency: LatencyEwma,
    /// Maintained by the health prober; read by `resolve()` rule 3.
    pub model_index: ArcSwap<Vec<String>>,
}

/// Permits for a backend: what the upstream reports, else what config asked for, else the
/// global cap. Never zero — a zero-permit backend would stall every request forever.
fn desired_permits(b: &Backend, cfg: &RouterCfg) -> u32 {
    if let Some(slots) = b.limits.slots_total.filter(|s| *s > 0) {
        return slots;
    }
    if b.limits.max_concurrent > 0 {
        return b.limits.max_concurrent;
    }
    cfg.max_inflight.max(1)
}

/// Recover a poisoned lock instead of panicking. A panic elsewhere must not take the routing
/// table down with it, and everything behind these locks is a plain number or a map.
fn unpoison<T>(r: Result<T, std::sync::PoisonError<T>>) -> T {
    r.unwrap_or_else(|p| p.into_inner())
}

impl LiveBackend {
    /// Create live state for a backend seen for the first time.
    pub fn new(b: Backend, cfg: &RouterCfg) -> Arc<LiveBackend> {
        let permits = desired_permits(&b, cfg);
        let id = b.id.clone();
        let accepting = b.enabled;
        let models: Vec<String> = b.models.iter().map(|m| m.id.clone()).collect();
        Arc::new(LiveBackend {
            id,
            meta: ArcSwap::from_pointee(b),
            sem: Arc::new(Semaphore::new(permits as usize)),
            breaker: Breaker::default(),
            retry_bucket: TokenBucket::default(),
            inflight: AtomicU32::new(0),
            accepting: AtomicBool::new(accepting),
            latency: LatencyEwma::default(),
            model_index: ArcSwap::from_pointee(models),
        })
    }

    /// Replace the description without touching any live state.
    ///
    /// The permit pool, breaker, retry bucket, in-flight gauge, drain flag, latency EWMA and
    /// the prober's model index all survive: this replaces the *description* and nothing else.
    pub fn update_meta(&self, b: Backend) {
        self.meta.store(Arc::new(b));
    }

    /// Grow or shrink the concurrency permit pool in place.
    ///
    /// The pool's *total* size is derived as `available + inflight`, because `Semaphore` does
    /// not report its own total and `LiveBackend`'s published field list has no counter to
    /// hold one. That identity holds because `InFlightGuard` takes exactly one permit per
    /// request and bumps `inflight` with it.
    ///
    /// Growing is immediate. Shrinking can only give back permits that are **free right
    /// now** — a permit held by an in-flight request is never revoked — so a shrink under
    /// load lands partially and the next call finishes it.
    pub fn resize_semaphore(&self, permits: u32) {
        let target = permits.max(1) as usize;
        let held = self.inflight.load(Ordering::Acquire) as usize;
        let current = self.sem.available_permits().saturating_add(held);
        match target.cmp(&current) {
            std::cmp::Ordering::Equal => {}
            std::cmp::Ordering::Greater => self.sem.add_permits(target - current),
            std::cmp::Ordering::Less => {
                self.sem.forget_permits(current - target);
            }
        }
    }

    /// Replace the model index the prober maintains.
    pub fn set_models(&self, m: Vec<String>) {
        self.model_index.store(Arc::new(m));
    }
}

/// An exponentially weighted moving average of observed latency.
///
/// Atomics, no lock: written on every finished request, read by `LeastBusy` and by every
/// snapshot. Until a sample lands it reports `None` rather than zero, because "fast" and
/// "never measured" must not render the same.
#[derive(Debug, Default)]
pub struct LatencyEwma {
    /// `f64::to_bits` of the current average, in milliseconds.
    bits: AtomicU64,
    /// How many samples have landed.
    samples: AtomicU64,
}

impl LatencyEwma {
    /// Fold one observation, in milliseconds, into the average.
    ///
    /// A non-finite or negative sample is ignored rather than poisoning the average.
    pub fn record(&self, ms: f64) {
        if !ms.is_finite() || ms < 0.0 {
            return;
        }
        let mut current = self.bits.load(Ordering::Relaxed);
        loop {
            let next = if self.samples.load(Ordering::Relaxed) == 0 {
                ms
            } else {
                let prev = f64::from_bits(current);
                prev + EWMA_ALPHA * (ms - prev)
            };
            match self.bits.compare_exchange_weak(
                current,
                next.to_bits(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    self.samples.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                Err(observed) => current = observed,
            }
        }
    }

    /// The current average in milliseconds, or `None` when nothing has been observed.
    pub fn ms(&self) -> Option<f64> {
        if self.samples.load(Ordering::Relaxed) == 0 {
            return None;
        }
        Some(f64::from_bits(self.bits.load(Ordering::Relaxed)))
    }
}

/// Every live backend, by id. Survives table recompiles.
#[derive(Default)]
pub struct BackendRegistry {
    /// The one lock. Held for map operations only — never across an `await`.
    inner: RwLock<HashMap<BackendId, Arc<LiveBackend>>>,
}

impl BackendRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        BackendRegistry::default()
    }

    /// Insert or update. **REUSES live state** when the id is already known.
    ///
    /// The returned `Arc` is the same pointer every earlier caller holds, which is what makes
    /// a table recompile invisible to the request path. A changed permit count resizes the
    /// pool **in place**; resizing is not replacing, so held permits and queued waiters are
    /// undisturbed. The drain flag and the prober's model index are live state and are left
    /// exactly as they were.
    pub fn upsert(&self, b: Backend, cfg: &RouterCfg) -> Arc<LiveBackend> {
        let mut g = unpoison(self.inner.write());
        if let Some(existing) = g.get(&b.id) {
            let live = Arc::clone(existing);
            let permits = desired_permits(&b, cfg);
            live.update_meta(b);
            live.resize_semaphore(permits);
            return live;
        }
        let live = LiveBackend::new(b, cfg);
        g.insert(live.id.clone(), Arc::clone(&live));
        live
    }

    /// Forget a backend, returning its live state so the caller can drain it.
    pub fn remove(&self, id: &BackendId) -> Option<Arc<LiveBackend>> {
        unpoison(self.inner.write()).remove(id)
    }

    /// Look one up.
    pub fn get(&self, id: &BackendId) -> Option<Arc<LiveBackend>> {
        unpoison(self.inner.read()).get(id).map(Arc::clone)
    }

    /// Every live backend.
    ///
    /// Sorted by id, so a compiled table — and therefore the order candidates are tried in —
    /// never depends on `HashMap` iteration order.
    pub fn all(&self) -> Vec<Arc<LiveBackend>> {
        let mut v: Vec<Arc<LiveBackend>> = unpoison(self.inner.read()).values().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    /// Every backend's serialisable description, for a `Snapshot`.
    pub fn snapshot(&self) -> Vec<Backend> {
        self.all()
            .into_iter()
            .map(|b| Backend::clone(&b.meta.load_full()))
            .collect()
    }
}

// ==========================================================================================
// the warm queue — ARCHITECTURE.md §4.7
// ==========================================================================================

/// `warm_queue_max`'s **fallback**, used when the configuration does not name one: 32, per
/// `ARCHITECTURE.md` §4.7.
///
/// Not the bound itself. Nothing in this module reads this constant to decide anything —
/// [`WarmRegistry::open`] takes the bound as an argument and `server/src/api/routes.rs`
/// supplies it — so the day `[router] warm_queue_max` lands in `core::config::RouterCfg` the
/// only line that changes is the one that computes the argument.
///
/// **The key is still missing from `RouterCfg`, and that is a reported collision rather than
/// a decision:** `core/src/config.rs` belongs to unit C-02 and this unit does not write other
/// units' files. See `warm_queue_max` in `server/src/api/routes.rs` for the exact one-line
/// change waiting for it.
pub const DEFAULT_WARM_QUEUE_MAX: u32 = 32;

/// How long a `503` produced by a full warm queue asks the client to wait, at most.
const MAX_RETRY_AFTER_SECS: u32 = 30;

/// How long a `503` produced by an *expired* warm window asks the client to wait.
///
/// Fixed and short: the window is over, so the very next request either finds the alias
/// serving or fails for a reason that is no longer "wait, something is starting".
const EXPIRED_RETRY_AFTER_SECS: u32 = 5;

/// How a park ended. Every arm is a decision the caller can act on with no further state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parked {
    /// The window closed. The alias has something to serve from again — **re-resolve**,
    /// because the whole point is that it now points somewhere else.
    Rearmed {
        /// How long this request waited, for the response header and the record.
        waited_ms: u32,
        /// The queue depth this request saw when it parked.
        depth: u32,
    },
    /// `warm_timeout` expired with the alias still warming. An OpenAI-shaped `503` with
    /// `Retry-After`.
    TimedOut {
        /// How long this request waited.
        waited_ms: u32,
        /// What to put in `Retry-After`.
        retry_after_secs: u32,
    },
    /// `warm_queue_max` requests are already parked, so this one is refused **immediately**
    /// rather than deepening a queue that is already the wrong answer.
    Overflow {
        /// How many were parked when this request arrived.
        depth: u32,
        /// The bound it hit.
        max: u32,
        /// What to put in `Retry-After`.
        retry_after_secs: u32,
    },
}

/// One alias's parking state. Created on first use and then reused for the daemon's life.
///
/// The `Notify` is the primitive §4.7 names. It is used with
/// [`Notified::enable`](tokio::sync::futures::Notified::enable) before the deadline is
/// re-read, so a window that closes between the check and the registration wakes the parked
/// request instead of leaving it to time out — `notify_waiters` stores no permit, and that
/// lost wakeup is the classic way this primitive is got wrong.
pub struct WarmSlot {
    /// Which alias is warming. Read only to render the broadcast message.
    alias: Alias,
    /// Woken by [`WarmWindow::close`] — and by its `Drop`, so a swap task that panics still
    /// releases everything parked behind it.
    notify: Notify,
    /// How much patience **one sign of life** buys, in milliseconds.
    ///
    /// `warm_timeout` is not a total budget for the swap; it is how long the window is
    /// willing to wait *since it last saw the thing it waits for make progress*. Stored so
    /// [`WarmWindow::rearm`] can restate the deadline without the caller having to carry the
    /// number around and risk restating a different one.
    budget_ms: AtomicU64,
    /// How many times this window has been re-armed. Logged by the swap, and the difference
    /// between "the load was slow" and "the park expired because nothing was happening".
    rearms: AtomicU32,
    /// Requests parked **right now**. This is the number both GUIs render.
    parked: AtomicU32,
    /// High-water mark for the open window; becomes `SwapReport::parked`.
    peak: AtomicU32,
    /// `warm_queue_max` for the open window.
    max: AtomicU32,
    /// Monotonic base for [`WarmSlot::now_ms`]. A wall clock would let one NTP step either
    /// expire every park at once or hold them all past the deadline.
    base: Instant,
    /// Milliseconds after `base` at which the window expires. **`0` means closed.**
    ///
    /// This is the entirety of "is this alias warming?", and it is a deadline rather than a
    /// flag on purpose (invariant 3): a flag left set by a task that died is a stored status
    /// that disagrees with reality, whereas a deadline in the past simply reads closed.
    until_ms: AtomicU64,
    /// Bumped on **every** open and every close.
    ///
    /// Without it, a swap that closes its window and a second swap that opens one in the same
    /// breath would leave the first swap's parked requests waiting: they would wake, re-read
    /// the deadline, find a window open again and settle back down — held across two
    /// `warm_timeout`s for a swap that ended long ago. A parked request is owed a re-resolve
    /// at every transition, not merely at the ones that end in a closed window.
    epoch: AtomicU64,
    /// Where "warming, N parked" goes.
    events: broadcast::Sender<Event>,
}

/// The state of one alias's warm window, **derived from its deadline every time it is asked**.
///
/// `Closed` and `Expired` are deliberately different answers. Both mean "stop parking", but a
/// closed window means the swap finished and the alias points somewhere that can serve, while
/// an expired one means the swap over-ran its own arithmetic and the client is owed a `503`.
/// Collapsing them would turn every timeout into a rearm and send the request straight back
/// into the failure it parked to avoid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Window {
    /// Open, with this much left.
    Open(Duration),
    /// The swap closed it: the alias can serve again.
    Closed,
    /// `warm_timeout` passed with the window still open.
    Expired,
}

impl WarmSlot {
    /// Milliseconds since this slot's monotonic base.
    fn now_ms(&self) -> u64 {
        u64::try_from(self.base.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Is a warm window open on this alias right now? **Computed, never stored.**
    pub fn is_open(&self) -> bool {
        matches!(self.window(), Window::Open(_))
    }

    /// The window's state, derived from its deadline and a monotonic clock.
    fn window(&self) -> Window {
        let until = self.until_ms.load(Ordering::Acquire);
        if until == 0 {
            return Window::Closed;
        }
        match until.checked_sub(self.now_ms()) {
            Some(left) if left > 0 => Window::Open(Duration::from_millis(left)),
            _ => Window::Expired,
        }
    }

    /// How many requests are parked here right now.
    pub fn parked(&self) -> u32 {
        self.parked.load(Ordering::Acquire)
    }

    /// The deepest the queue got during the open window — `SwapReport::parked`.
    pub fn peak(&self) -> u32 {
        self.peak.load(Ordering::Acquire)
    }

    /// How many times the open window's deadline has been pushed out by observed progress.
    pub fn rearms(&self) -> u32 {
        self.rearms.load(Ordering::Acquire)
    }

    /// Park until the alias can serve again, the window expires, or the queue is full.
    ///
    /// **Cancel-safe by construction.** The depth is owned by a stack-local RAII guard, so a
    /// client that hangs up — which drops this future — gives its slot back and updates the
    /// broadcast exactly as an orderly wake does. Nothing is held across the `await` but
    /// atomics.
    pub async fn park(self: &Arc<Self>) -> Parked {
        let started = Instant::now();
        let max = self.max.load(Ordering::Acquire).max(1);
        // The window this request is parking behind. Any transition away from it — closed,
        // or closed and immediately reopened by the next swap — releases this request.
        let epoch = self.epoch.load(Ordering::Acquire);

        // Claim a place in the bounded queue, or overflow. One CAS, so two requests cannot
        // both see `max - 1` and both park.
        let depth = match self
            .parked
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < max).then_some(n + 1)
            }) {
            Ok(previous) => previous + 1,
            Err(full) => {
                return Parked::Overflow {
                    depth: full,
                    max,
                    retry_after_secs: self.retry_after_secs(),
                }
            }
        };
        // From here every exit runs this guard's `Drop`, including a dropped future.
        let _slot = DepthGuard {
            slot: Arc::clone(self),
        };
        self.peak.fetch_max(depth, Ordering::AcqRel);
        self.announce(depth);

        loop {
            let left = match self.state_for(epoch) {
                Ok(left) => left,
                Err(done) => return done(millis(started.elapsed()), depth),
            };
            // Register with the `Notify` BEFORE re-reading the state. `notify_waiters` stores
            // no permit, so a window that closes in the gap would otherwise be a lost wakeup
            // and this request would sit here for the whole `warm_timeout`.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Err(done) = self.state_for(epoch) {
                return done(millis(started.elapsed()), depth);
            }
            // Either the window changed (woken) or `left` elapsed. Which of the two it was is
            // decided by re-reading the state at the top, not by which arm this returned, so
            // a spurious wake simply waits again on a smaller `left`. The loop always shrinks.
            let _ = tokio::time::timeout(left, notified).await;
        }
    }

    /// How much longer to wait, or the outcome this park has already reached.
    ///
    /// `Err` carries the constructor rather than the value so the two call sites above cannot
    /// disagree about which outcome corresponds to which state — the mistake that would turn
    /// every expiry into a rearm and send the request straight back into the failure it
    /// parked to avoid.
    #[allow(clippy::type_complexity)]
    fn state_for(&self, epoch: u64) -> Result<Duration, fn(u32, u32) -> Parked> {
        if self.epoch.load(Ordering::Acquire) != epoch {
            return Err(|waited_ms, depth| Parked::Rearmed { waited_ms, depth });
        }
        match self.window() {
            Window::Open(left) => Ok(left),
            Window::Closed => Err(|waited_ms, depth| Parked::Rearmed { waited_ms, depth }),
            Window::Expired => Err(|waited_ms, _| Parked::TimedOut {
                waited_ms,
                retry_after_secs: EXPIRED_RETRY_AFTER_SECS,
            }),
        }
    }

    /// `Retry-After` for a refusal raised while the window is still open: what is left of it,
    /// clamped so a ten-minute model load does not become a ten-minute `Retry-After`.
    fn retry_after_secs(&self) -> u32 {
        let left = match self.window() {
            Window::Open(left) => left.as_secs(),
            Window::Closed | Window::Expired => 0,
        };
        u32::try_from(left)
            .unwrap_or(MAX_RETRY_AFTER_SECS)
            .clamp(1, MAX_RETRY_AFTER_SECS)
    }

    /// Broadcast the depth, so both GUIs can render "warming, N parked".
    ///
    /// The id is stable per alias, so the web UI replaces the row rather than stacking one
    /// per parked request; `Info`, because a swap that is working is not a problem. Skipped
    /// entirely when nobody is subscribed — serialising a message nobody reads is pure cost
    /// on a path that only runs while the product is already degraded.
    fn announce(&self, parked: u32) {
        if self.events.receiver_count() == 0 {
            return;
        }
        let _ = self.events.send(Event::Alert {
            level: AlertLevel::Info,
            message: if parked == 0 {
                format!("{}: warm again, 0 parked", self.alias)
            } else {
                format!("{}: warming, {parked} parked", self.alias)
            },
            action: None,
            id: format!("router.warming.{}", self.alias),
        });
    }
}

/// Owns one request's place in the queue. Its `Drop` is what makes a park cancel-safe.
struct DepthGuard {
    slot: Arc<WarmSlot>,
}

impl Drop for DepthGuard {
    fn drop(&mut self) {
        let now = self
            .slot
            .parked
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                Some(n.saturating_sub(1))
            })
            .unwrap_or(0)
            .saturating_sub(1);
        self.slot.announce(now);
    }
}

/// The open warm window, held by the swap for exactly as long as the alias cannot serve.
///
/// Dropping it closes the window and wakes everything parked behind it, which is why every
/// failure path of a sequential swap — including a panic — ends with the parked requests
/// re-resolving rather than waiting out `warm_timeout`.
pub struct WarmWindow {
    slot: Arc<WarmSlot>,
    open: Arc<AtomicU32>,
    /// The epoch this window's own `open` produced. A later swap of the same alias bumps it,
    /// after which this window is **stale** and may no longer speak for the alias.
    epoch: u64,
    closed: bool,
}

impl WarmWindow {
    /// The deepest the queue has been so far.
    pub fn peak(&self) -> u32 {
        self.slot.peak()
    }

    /// How many are parked right now.
    pub fn parked(&self) -> u32 {
        self.slot.parked()
    }

    /// How many times this window has been re-armed.
    pub fn rearms(&self) -> u32 {
        self.slot.rearms()
    }

    /// **The thing being waited for is still working — give it the budget again.**
    ///
    /// `warm_timeout` used to be a fixed slice of wall clock, and that made a park a race
    /// against a load rather than a wait for one: measured, a 3000 ms window against a swap
    /// that ran 12,038 ms `503`'d its four parked requests at 2977 ms and the alias then
    /// answered 74,550 requests with `no_healthy_backend` for the remaining nine seconds.
    /// That is the outage `ARCHITECTURE.md` §4.7 exists to prevent, merely postponed — and it
    /// is worse than waiting, because the load was demonstrably progressing the whole time.
    ///
    /// The health gate has always resolved this the same way: its deadline is "wall clock
    /// **since the last observed progress**", not since the start. The gate and the warm queue
    /// are waiting on the same event, so this makes them wait on the same liveness signal —
    /// each sign of life restates the deadline at `now + budget`.
    ///
    /// Deliberately **not** a wake-up. Bumping the epoch or calling `notify_waiters` would
    /// release every parked request into the outage that is still in progress, which is the
    /// exact failure a park exists to avoid; a parked request re-reads the deadline on its own
    /// next tick and simply waits longer. Nothing here is on the request path either way.
    ///
    /// Patience can be **extended, never resurrected**. `false` means the re-arm was refused,
    /// and there are exactly three ways to earn that:
    ///
    /// * the window is closed — the requests behind it have already been released;
    /// * it has been superseded by a later swap of the same alias, and a stale window may no
    ///   longer speak for it;
    /// * it already expired, so everything parked has already been refused with
    ///   `warm_timeout`. Reviving it would mean a swap that lost the argument once quietly
    ///   parking the *next* client too.
    ///
    /// A caller ticking far faster than the budget — which is what the pacemaker does, one
    /// re-arm per health probe — can only see the third case if the daemon itself was wedged
    /// for a whole budget, which is precisely when refusing is right.
    pub fn rearm(&self) -> bool {
        if self.closed || self.slot.epoch.load(Ordering::Acquire) != self.epoch {
            return false;
        }
        let now = self.slot.now_ms();
        let until = now
            .saturating_add(self.slot.budget_ms.load(Ordering::Acquire))
            .max(1);
        let armed = self
            .slot
            .until_ms
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                // `0` is closed and `<= now` is expired. `max` because a re-arm must never
                // *shorten* a window: `open` and `rearm` read the clock at different moments,
                // and pulling a deadline in is the defect this method exists to remove.
                (current != 0 && current > now).then(|| current.max(until))
            })
            .is_ok();
        if armed {
            self.slot.rearms.fetch_add(1, Ordering::AcqRel);
        }
        armed
    }

    /// Close the window and wake every parked request, returning the peak depth for
    /// `SwapReport::parked`.
    pub fn close(mut self) -> u32 {
        self.shut();
        self.slot.peak()
    }

    /// The idempotent half of [`WarmWindow::close`], shared with `Drop`.
    fn shut(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        // **Only the current window may clear the deadline.** Two sequential swaps of the same
        // alias overlap only if an operator issues them concurrently, but when they do, the
        // first one's `Drop` must not cancel the second one's window: a deadline of zero reads
        // `Closed`, and the second swap's parked requests would be released into the outage it
        // is still in the middle of. A stale window still wakes waiters and still gives back
        // its place in the open count — it simply stops speaking for the alias.
        if self.slot.epoch.load(Ordering::Acquire) == self.epoch {
            self.slot.until_ms.store(0, Ordering::Release);
            self.slot.epoch.fetch_add(1, Ordering::AcqRel);
        }
        // After the deadline is cleared and the epoch is bumped, both release-ordered: a
        // waiter woken here re-reads them and must not see the state it parked under.
        self.slot.notify.notify_waiters();
        let _ = self
            .open
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                Some(n.saturating_sub(1))
            });
    }
}

impl Drop for WarmWindow {
    fn drop(&mut self) {
        self.shut();
    }
}

/// Every alias's warm queue, by alias. Lives on `RouterInner` beside the backend registry.
pub struct WarmRegistry {
    /// The one lock. Held for a map lookup and nothing else — never across an `await`.
    slots: RwLock<HashMap<Alias, Arc<WarmSlot>>>,
    /// How many windows are open right now.
    ///
    /// **This is the only thing the request path reads when nothing is warming**: one relaxed
    /// load on an already-failing dispatch, and nothing at all on the happy path.
    open: Arc<AtomicU32>,
    /// Handed to each slot so depth changes reach the WS event stream.
    events: broadcast::Sender<Event>,
}

impl WarmRegistry {
    /// An empty registry publishing to `events`.
    pub fn new(events: broadcast::Sender<Event>) -> WarmRegistry {
        WarmRegistry {
            slots: RwLock::new(HashMap::new()),
            open: Arc::new(AtomicU32::new(0)),
            events,
        }
    }

    /// Is **anything** warming? The request path's cheap negative.
    pub fn any_open(&self) -> bool {
        self.open.load(Ordering::Relaxed) > 0
    }

    /// The slot for `alias`, **only when a window is open on it**.
    ///
    /// Returning `None` for a closed window is what keeps the caller honest: a request may
    /// park only because a swap said the alias is coming back, never because some earlier
    /// swap once touched it.
    pub fn parking_for(&self, alias: &Alias) -> Option<Arc<WarmSlot>> {
        if !self.any_open() {
            return None;
        }
        let slot = unpoison(self.slots.read()).get(alias).map(Arc::clone)?;
        slot.is_open().then_some(slot)
    }

    /// How many requests are parked on `alias` right now. `0` when nothing is.
    pub fn parked(&self, alias: &Alias) -> u32 {
        unpoison(self.slots.read())
            .get(alias)
            .map(|s| s.parked())
            .unwrap_or(0)
    }

    /// How many times the last window opened on `alias` was re-armed by observed progress.
    ///
    /// Survives the window that produced it — [`WarmRegistry::open`] zeroes it, `close` does
    /// not — because the question an operator asks is always after the fact: "that swap took
    /// twelve seconds and nobody got a `503`; was that patience or luck?".
    pub fn rearms(&self, alias: &Alias) -> u32 {
        unpoison(self.slots.read())
            .get(alias)
            .map(|s| s.rearms())
            .unwrap_or(0)
    }

    /// Open a warm window on `alias` for `timeout`, admitting at most `max` parked requests.
    ///
    /// `timeout` is `warm_timeout` and **is not an independent number** (§4.7): the caller
    /// derives it from the budget of the thing being started, because a 90 s park against a
    /// 180 s load is an arithmetic guarantee of failure. It is also not a *total* budget — it
    /// is the patience one sign of life buys, and [`WarmWindow::rearm`] restates it every time
    /// the caller sees the thing it is waiting for make progress. The returned guard closes
    /// the window however the caller's function exits.
    pub fn open(&self, alias: &Alias, timeout: Duration, max: u32) -> WarmWindow {
        let slot = self.slot(alias);
        slot.max.store(max.max(1), Ordering::Release);
        slot.peak.store(slot.parked(), Ordering::Release);
        slot.rearms.store(0, Ordering::Release);
        let budget = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
        slot.budget_ms.store(budget, Ordering::Release);
        let until = slot.now_ms().saturating_add(budget).max(1);
        slot.until_ms.store(until, Ordering::Release);
        let epoch = slot.epoch.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        // Anything parked behind the *previous* window is owed a re-resolve now, not when
        // this new one happens to end.
        slot.notify.notify_waiters();
        self.open.fetch_add(1, Ordering::AcqRel);
        WarmWindow {
            slot,
            open: Arc::clone(&self.open),
            epoch,
            closed: false,
        }
    }

    /// The slot for `alias`, created on first use.
    fn slot(&self, alias: &Alias) -> Arc<WarmSlot> {
        if let Some(existing) = unpoison(self.slots.read()).get(alias) {
            return Arc::clone(existing);
        }
        let mut g = unpoison(self.slots.write());
        Arc::clone(g.entry(alias.clone()).or_insert_with(|| {
            Arc::new(WarmSlot {
                alias: alias.clone(),
                notify: Notify::new(),
                budget_ms: AtomicU64::new(0),
                rearms: AtomicU32::new(0),
                parked: AtomicU32::new(0),
                peak: AtomicU32::new(0),
                max: AtomicU32::new(DEFAULT_WARM_QUEUE_MAX),
                base: Instant::now(),
                until_ms: AtomicU64::new(0),
                epoch: AtomicU64::new(0),
                events: self.events.clone(),
            })
        }))
    }
}

/// Milliseconds, saturating rather than wrapping.
fn millis(d: Duration) -> u32 {
    u32::try_from(d.as_millis()).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_protocol::{
        BackendKind, BackendLimits, CredentialSource, Health, Protocol, Provenance, UpstreamModel,
    };

    fn backend(id: &str, slots: Option<u32>) -> Backend {
        Backend {
            id: BackendId::parse(id).expect("id"),
            kind: BackendKind::LocalLlama,
            protocol: Protocol::OpenAi,
            label: id.to_owned(),
            base_url: "http://127.0.0.1:8100".into(),
            credential: CredentialSource::None,
            tags: vec!["local".into()],
            models: vec![UpstreamModel {
                id: "Carnice-9b-Q6_K".into(),
                ctx: Some(32_768),
                vision: false,
                tools: true,
            }],
            limits: BackendLimits {
                max_concurrent: 2,
                queue_depth: 8,
                ctx: Some(32_768),
                slots_total: slots,
            },
            price: None,
            health: Health::Unknown,
            provenance: Provenance::Spawned,
            endpoint: None,
            enabled: true,
            devices: vec![],
            last_error: None,
        }
    }

    #[test]
    fn permit_count_comes_from_slots_then_config_then_the_global_cap() {
        let cfg = RouterCfg::default();

        let from_slots = LiveBackend::new(backend("a", Some(7)), &cfg);
        assert_eq!(from_slots.sem.available_permits(), 7);

        let from_limits = LiveBackend::new(backend("b", None), &cfg);
        assert_eq!(from_limits.sem.available_permits(), 2);

        let mut bare = backend("c", None);
        bare.limits.max_concurrent = 0;
        let fallback = LiveBackend::new(bare, &cfg);
        assert_eq!(fallback.sem.available_permits(), cfg.max_inflight as usize);
    }

    #[test]
    fn new_seeds_the_model_index_and_the_drain_flag() {
        let cfg = RouterCfg::default();
        let live = LiveBackend::new(backend("a", Some(1)), &cfg);
        assert_eq!(live.model_index.load().as_slice(), ["Carnice-9b-Q6_K"]);
        assert!(live.accepting.load(Ordering::Relaxed));

        let mut disabled = backend("b", Some(1));
        disabled.enabled = false;
        let live = LiveBackend::new(disabled, &cfg);
        assert!(!live.accepting.load(Ordering::Relaxed));
    }

    #[test]
    fn update_meta_replaces_the_description_only() {
        let cfg = RouterCfg::default();
        let live = LiveBackend::new(backend("a", Some(4)), &cfg);
        live.set_models(vec!["probed-model".into()]);
        live.latency.record(120.0);
        live.inflight.fetch_add(2, Ordering::SeqCst);
        live.accepting.store(false, Ordering::SeqCst);

        let mut next = backend("a", Some(4));
        next.label = "renamed".into();
        live.update_meta(next);

        assert_eq!(live.meta.load().label, "renamed");
        assert_eq!(live.model_index.load().as_slice(), ["probed-model"]);
        assert_eq!(live.latency.ms(), Some(120.0));
        assert_eq!(live.inflight.load(Ordering::SeqCst), 2);
        assert!(!live.accepting.load(Ordering::SeqCst));
    }

    #[test]
    fn resize_semaphore_grows_and_shrinks_in_place() {
        let cfg = RouterCfg::default();
        let live = LiveBackend::new(backend("a", Some(4)), &cfg);
        let sem = Arc::clone(&live.sem);

        live.resize_semaphore(8);
        assert_eq!(live.sem.available_permits(), 8);
        live.resize_semaphore(3);
        assert_eq!(live.sem.available_permits(), 3);
        // The same pool object throughout — a resize never orphans a waiter.
        assert!(Arc::ptr_eq(&sem, &live.sem));

        // Zero would stall the backend forever, so it is clamped to one.
        live.resize_semaphore(0);
        assert_eq!(live.sem.available_permits(), 1);
    }

    #[test]
    fn shrinking_never_revokes_a_permit_that_is_in_use() {
        let cfg = RouterCfg::default();
        let live = LiveBackend::new(backend("a", Some(4)), &cfg);
        let held = Arc::clone(&live.sem)
            .try_acquire_many_owned(3)
            .expect("3 of 4 permits");
        // What `InFlightGuard` does for every permit it takes.
        live.inflight.store(3, Ordering::Release);

        live.resize_semaphore(1);
        // Only the one free permit could be forgotten.
        assert_eq!(live.sem.available_permits(), 0);
        drop(held);
        live.inflight.store(0, Ordering::Release);
        assert_eq!(live.sem.available_permits(), 3);
        // A later call finishes the shrink now that the permits are back.
        live.resize_semaphore(1);
        assert_eq!(live.sem.available_permits(), 1);
    }

    #[test]
    fn upsert_preserves_live_state_across_a_recompile() {
        // THE regression test: three in-flight requests, a recompile, unchanged permits.
        let cfg = RouterCfg::default();
        let reg = BackendRegistry::new();
        let first = reg.upsert(backend("local-carnice", Some(4)), &cfg);

        let _in_flight = Arc::clone(&first.sem)
            .try_acquire_many_owned(3)
            .expect("3 in-flight requests");
        first.inflight.store(3, Ordering::SeqCst);
        first.latency.record(88.0);
        first.set_models(vec!["probed-model".into()]);
        assert_eq!(first.sem.available_permits(), 1);

        // Reconciliation re-upserts every backend on every recompile.
        let mut same = backend("local-carnice", Some(4));
        same.label = "re-discovered".into();
        let again = reg.upsert(same, &cfg);

        assert!(Arc::ptr_eq(&first, &again), "the Arc must be reused");
        assert!(
            Arc::ptr_eq(&first.sem, &again.sem),
            "the Semaphore survives"
        );
        assert_eq!(again.sem.available_permits(), 1, "permit count unchanged");
        assert_eq!(again.inflight.load(Ordering::SeqCst), 3);
        assert_eq!(again.latency.ms(), Some(88.0));
        assert_eq!(again.model_index.load().as_slice(), ["probed-model"]);
        assert_eq!(again.meta.load().label, "re-discovered");
    }

    #[test]
    fn upsert_resizes_in_place_when_the_upstream_reports_more_slots() {
        let cfg = RouterCfg::default();
        let reg = BackendRegistry::new();
        let live = reg.upsert(backend("a", Some(2)), &cfg);
        let held = Arc::clone(&live.sem)
            .try_acquire_owned()
            .expect("one permit");
        live.inflight.store(1, Ordering::Release);
        assert_eq!(live.sem.available_permits(), 1);

        let again = reg.upsert(backend("a", Some(6)), &cfg);
        assert!(Arc::ptr_eq(&live.sem, &again.sem));
        assert_eq!(
            again.sem.available_permits(),
            5,
            "grew by four, kept the held one"
        );
        drop(held);
        assert_eq!(again.sem.available_permits(), 6);
    }

    #[test]
    fn get_remove_all_and_snapshot() {
        let cfg = RouterCfg::default();
        let reg = BackendRegistry::new();
        reg.upsert(backend("zeta", Some(1)), &cfg);
        reg.upsert(backend("alpha", Some(1)), &cfg);

        let ids: Vec<String> = reg.all().iter().map(|b| b.id.to_string()).collect();
        assert_eq!(ids, ["alpha", "zeta"], "all() is sorted by id");
        assert_eq!(reg.snapshot().len(), 2);
        assert_eq!(reg.snapshot()[0].id.as_str(), "alpha");

        let alpha = BackendId::parse("alpha").expect("id");
        assert!(reg.get(&alpha).is_some());
        let removed = reg.remove(&alpha).expect("removed");
        assert_eq!(removed.id, alpha);
        assert!(reg.get(&alpha).is_none());
        assert!(reg.remove(&alpha).is_none());
        assert_eq!(reg.all().len(), 1);
    }

    #[test]
    fn latency_ewma_reports_nothing_until_it_has_a_sample() {
        let e = LatencyEwma::default();
        assert_eq!(e.ms(), None);
        e.record(100.0);
        assert_eq!(e.ms(), Some(100.0));
        e.record(200.0);
        let after = e.ms().expect("a sample");
        assert!(after > 100.0 && after < 200.0, "{after} should be smoothed");
        // Garbage in is ignored, not averaged in.
        e.record(f64::NAN);
        e.record(-5.0);
        assert_eq!(e.ms(), Some(after));
    }

    // ======================================================================================
    // the warm queue — ARCHITECTURE.md §4.7
    // ======================================================================================
    //
    // Real time, never `start_paused`: this primitive reads a `std::time::Instant`, which
    // tokio's clock does not mock, so a paused runtime would auto-advance the `timeout` while
    // the deadline stood still. Every duration below is therefore small and every assertion
    // is one-sided.

    fn alias(s: &str) -> Alias {
        Alias::parse(s).expect("alias")
    }

    fn warm() -> (WarmRegistry, broadcast::Receiver<Event>) {
        let (tx, rx) = broadcast::channel(256);
        (WarmRegistry::new(tx), rx)
    }

    #[tokio::test]
    async fn nothing_may_park_until_a_swap_opens_a_window() {
        let (reg, _rx) = warm();
        let auto = alias("auto");
        assert!(!reg.any_open());
        assert!(
            reg.parking_for(&auto).is_none(),
            "parking without a swap is a promise nobody is keeping"
        );
        assert_eq!(reg.parked(&auto), 0);

        let window = reg.open(&auto, Duration::from_secs(30), 32);
        assert!(reg.any_open());
        assert!(reg.parking_for(&auto).is_some());
        // A different alias is untouched: a swap of `auto` must not park `code`.
        assert!(reg.parking_for(&alias("code")).is_none());

        window.close();
        assert!(!reg.any_open());
        assert!(reg.parking_for(&auto).is_none());
    }

    #[tokio::test]
    async fn closing_the_window_releases_every_parked_request() {
        let (reg, _rx) = warm();
        let auto = alias("auto");
        let window = reg.open(&auto, Duration::from_secs(30), 32);
        let slot = reg.parking_for(&auto).expect("open");

        let parkers: Vec<_> = (0..5)
            .map(|_| {
                let s = Arc::clone(&slot);
                tokio::spawn(async move { s.park().await })
            })
            .collect();

        // Wait for all five to actually be parked before closing, so this tests the wake and
        // not a race in which they park after the window is already shut.
        while slot.parked() < 5 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert_eq!(window.parked(), 5);
        assert_eq!(window.peak(), 5);

        assert_eq!(
            window.close(),
            5,
            "the peak is what SwapReport::parked wants"
        );
        for p in parkers {
            match p.await.expect("park task") {
                Parked::Rearmed { depth, .. } => assert!((1..=5).contains(&depth)),
                other => panic!("a closed window must rearm, not {other:?}"),
            }
        }
        assert_eq!(slot.parked(), 0, "every slot came back");
    }

    #[tokio::test]
    async fn a_window_dropped_rather_than_closed_still_wakes_everyone() {
        // The panic path, and every `?` in the swap: nothing may be left waiting out
        // `warm_timeout` because the code that promised to close the window never got there.
        let (reg, _rx) = warm();
        let auto = alias("auto");
        let window = reg.open(&auto, Duration::from_secs(30), 32);
        let slot = reg.parking_for(&auto).expect("open");

        let parked = tokio::spawn({
            let s = Arc::clone(&slot);
            async move { s.park().await }
        });
        while slot.parked() < 1 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        drop(window);
        assert!(matches!(
            parked.await.expect("park task"),
            Parked::Rearmed { .. }
        ));
        assert!(!reg.any_open(), "a dropped window is a closed window");
    }

    #[tokio::test]
    async fn the_queue_is_bounded_and_the_overflow_is_refused_at_once() {
        let (reg, _rx) = warm();
        let auto = alias("auto");
        let _window = reg.open(&auto, Duration::from_secs(30), 2);
        let slot = reg.parking_for(&auto).expect("open");

        let held: Vec<_> = (0..2)
            .map(|_| {
                let s = Arc::clone(&slot);
                tokio::spawn(async move { s.park().await })
            })
            .collect();
        while slot.parked() < 2 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        // The third does not wait: deepening a queue that is already the wrong answer only
        // moves the failure later.
        match slot.park().await {
            Parked::Overflow {
                depth,
                max,
                retry_after_secs,
            } => {
                assert_eq!((depth, max), (2, 2));
                assert!(
                    (1..=30).contains(&retry_after_secs),
                    "Retry-After must be usable: {retry_after_secs}"
                );
            }
            other => panic!("expected Overflow, got {other:?}"),
        }
        assert_eq!(slot.parked(), 2, "the refusal took no slot");
        for h in held {
            h.abort();
            let _ = h.await;
        }
    }

    #[tokio::test]
    async fn an_expired_window_times_the_park_out_rather_than_rearming_it() {
        // Closed and expired are different answers. Conflating them would send a timed-out
        // request straight back into the failure it parked to avoid.
        let (reg, _rx) = warm();
        let auto = alias("auto");
        let _window = reg.open(&auto, Duration::from_millis(60), 32);
        let slot = reg.parking_for(&auto).expect("open");

        match slot.park().await {
            Parked::TimedOut {
                waited_ms,
                retry_after_secs,
            } => {
                assert!(waited_ms >= 50, "it really waited: {waited_ms} ms");
                assert!(retry_after_secs >= 1);
            }
            other => panic!("expected TimedOut, got {other:?}"),
        }
        assert_eq!(slot.parked(), 0);
        // Derived, not stored: nothing closed this window, it simply passed its deadline.
        assert!(!slot.is_open());
        assert!(reg.parking_for(&auto).is_none());
    }

    #[tokio::test]
    async fn a_client_that_gives_up_while_parked_gives_its_slot_back() {
        // Cancel-safety. R-08 hangs the `RequestFinished { aborted: true }` off the same
        // drop; what this asserts is that the depth cannot leak, because a leaked depth
        // permanently shrinks the queue for every later swap.
        let (reg, _rx) = warm();
        let auto = alias("auto");
        let window = reg.open(&auto, Duration::from_secs(30), 1);
        let slot = reg.parking_for(&auto).expect("open");

        let abandoned = tokio::spawn({
            let s = Arc::clone(&slot);
            async move { s.park().await }
        });
        while slot.parked() < 1 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        abandoned.abort();
        let _ = abandoned.await;

        while slot.parked() > 0 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert_eq!(slot.parked(), 0, "the dropped future released its place");

        // …and the one-deep queue admits somebody else, rather than being permanently full
        // because a client hung up in it.
        let next = tokio::spawn({
            let s = Arc::clone(&slot);
            async move { s.park().await }
        });
        while slot.parked() < 1 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        window.close();
        assert!(matches!(
            next.await.expect("park task"),
            Parked::Rearmed { .. }
        ));
    }

    #[tokio::test]
    async fn the_depth_reaches_the_event_stream_so_both_guis_can_render_it() {
        let (reg, mut rx) = warm();
        let auto = alias("auto");
        let window = reg.open(&auto, Duration::from_secs(30), 32);
        let slot = reg.parking_for(&auto).expect("open");

        let parked = tokio::spawn({
            let s = Arc::clone(&slot);
            async move { s.park().await }
        });
        while slot.parked() < 1 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        window.close();
        let _ = parked.await;

        let mut seen: Vec<String> = Vec::new();
        while let Ok(Event::Alert { id, message, .. }) = rx.try_recv() {
            assert_eq!(id, "router.warming.auto", "one stable id, so rows coalesce");
            seen.push(message);
        }
        assert!(
            seen.iter().any(|m| m == "auto: warming, 1 parked"),
            "both GUIs need the depth: {seen:?}"
        );
        assert!(
            seen.iter().any(|m| m.contains("0 parked")),
            "and the row has to stop saying 1: {seen:?}"
        );
    }

    #[tokio::test]
    async fn a_second_swap_of_the_same_alias_releases_the_firsts_parked_requests() {
        // Two overlapping sequential swaps of one alias. The request parked behind the first
        // is owed a re-resolve the moment the second opens — holding it across both windows
        // would be two `warm_timeout`s for a swap that ended long ago.
        let (reg, _rx) = warm();
        let auto = alias("auto");
        let first = reg.open(&auto, Duration::from_secs(30), 32);
        let slot = reg.parking_for(&auto).expect("open");
        let parked = tokio::spawn({
            let s = Arc::clone(&slot);
            async move { s.park().await }
        });
        while slot.parked() < 1 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        let second = reg.open(&auto, Duration::from_secs(30), 32);
        assert!(matches!(
            parked.await.expect("park task"),
            Parked::Rearmed { .. }
        ));

        // …and the first window, now stale, must not cancel the second one on its way out.
        drop(first);
        assert!(
            slot.is_open(),
            "a superseded window closed the swap that superseded it"
        );
        assert!(reg.parking_for(&auto).is_some());
        second.close();
        assert!(!slot.is_open());
        assert!(!reg.any_open(), "both windows gave back their place");
    }

    #[tokio::test]
    async fn reopening_reuses_the_slot_and_resets_the_peak() {
        let (reg, _rx) = warm();
        let auto = alias("auto");
        let first = reg.open(&auto, Duration::from_secs(30), 32);
        let slot = reg.parking_for(&auto).expect("open");
        let parked = tokio::spawn({
            let s = Arc::clone(&slot);
            async move { s.park().await }
        });
        while slot.parked() < 1 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert_eq!(first.close(), 1);
        let _ = parked.await;

        let second = reg.open(&auto, Duration::from_secs(30), 32);
        assert!(
            Arc::ptr_eq(&slot, &reg.parking_for(&auto).expect("open")),
            "the same slot, so a waiter from the first window is not orphaned"
        );
        assert_eq!(
            second.peak(),
            0,
            "the next swap reports its own depth, not the last one's"
        );
    }

    // --------------------------------------------------------------------------------------
    // re-arming — the deadline is patience, not a stopwatch
    // --------------------------------------------------------------------------------------

    /// A request parked behind a 120 ms window is still parked 300 ms later, because the
    /// thing it waits for kept saying it was alive — and it leaves `Rearmed`, not `TimedOut`.
    ///
    /// This is the whole of the fix at the smallest scale that can show it: without the
    /// re-arm the park ends in `Parked::TimedOut` at 120 ms, which upstairs is a `503` and,
    /// once the window is gone, a `no_healthy_backend` storm for as long as the load runs on.
    #[tokio::test]
    async fn a_window_re_armed_by_progress_outlives_its_own_budget() {
        let (reg, _rx) = warm();
        let auto = alias("auto");
        let window = reg.open(&auto, Duration::from_millis(120), 32);
        let slot = reg.parking_for(&auto).expect("open");

        let parked = tokio::spawn({
            let s = Arc::clone(&slot);
            async move { s.park().await }
        });
        while slot.parked() < 1 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        // Six ticks at 50 ms: 300 ms of wall clock against a 120 ms budget.
        for _ in 0..6 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(window.rearm(), "a live window must accept a re-arm");
        }
        assert!(
            slot.is_open(),
            "the window expired while it was being told the load was still working"
        );
        assert_eq!(slot.parked(), 1, "and the request is still waiting");
        assert_eq!(window.rearms(), 6);

        // The launch finishes: the request is released to re-resolve, not refused.
        window.close();
        assert!(matches!(
            parked.await.expect("park task"),
            Parked::Rearmed { .. }
        ));
        assert_eq!(
            reg.rearms(&auto),
            6,
            "the count outlives the window, because the question is always asked afterwards"
        );
    }

    /// The bound the ruling insisted on keeping: when progress stops, so does the patience.
    #[tokio::test]
    async fn a_window_that_stops_being_re_armed_still_expires() {
        let (reg, _rx) = warm();
        let auto = alias("auto");
        let window = reg.open(&auto, Duration::from_millis(80), 32);
        let slot = reg.parking_for(&auto).expect("open");
        assert!(window.rearm(), "one sign of life");

        let outcome = slot.park().await;
        assert!(
            matches!(outcome, Parked::TimedOut { .. }),
            "patience without progress is still bounded: {outcome:?}"
        );
        assert!(!slot.is_open());
        // A re-arm cannot resurrect a window whose requests have already been refused.
        assert!(!window.rearm(), "an expired window is not re-openable");
    }

    /// A closed window, and a window superseded by the next swap of the same alias, both
    /// refuse to re-arm — otherwise a pacemaker that outlived its swap could hold requests
    /// behind a window nobody is going to close.
    #[tokio::test]
    async fn a_stale_or_closed_window_refuses_to_re_arm() {
        let (reg, _rx) = warm();
        let auto = alias("auto");

        let first = reg.open(&auto, Duration::from_secs(30), 32);
        assert!(first.rearm());
        let second = reg.open(&auto, Duration::from_secs(30), 32);
        assert!(
            !first.rearm(),
            "a superseded window may no longer speak for the alias"
        );
        assert!(second.rearm());

        let deadline_before = {
            let slot = reg.parking_for(&auto).expect("open");
            slot.until_ms.load(Ordering::Acquire)
        };
        drop(first);
        let slot = reg
            .parking_for(&auto)
            .expect("the second window is still open");
        assert_eq!(
            slot.until_ms.load(Ordering::Acquire),
            deadline_before,
            "a stale window's drop must not touch the live window's deadline"
        );

        second.close();
        let closed = reg.slot(&auto);
        assert!(!closed.is_open());
    }

    /// Two swaps in a row each get their own re-arm count, so "was that patience or luck?"
    /// is answered about the swap being asked about.
    #[tokio::test]
    async fn opening_a_window_resets_the_re_arm_count() {
        let (reg, _rx) = warm();
        let auto = alias("auto");
        let first = reg.open(&auto, Duration::from_millis(500), 32);
        assert!(first.rearm());
        assert!(first.rearm());
        assert_eq!(reg.rearms(&auto), 2);
        first.close();

        let second = reg.open(&auto, Duration::from_millis(500), 32);
        assert_eq!(reg.rearms(&auto), 0, "the next swap starts from zero");
        assert!(second.rearm());
        assert_eq!(second.rearms(), 1);
    }
}
