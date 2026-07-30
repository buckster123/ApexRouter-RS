//! OWNER: unit R-01 (router/src/table.rs, router/src/registry.rs). Do not edit outside that
//! unit.
//!
//! Live per-backend state.
//!
//! **The compiled table holds `Arc<LiveBackend>` pointers, so rebuilding the table never
//! resets live state.** `upsert` on an existing id preserves the `Semaphore`, the breaker,
//! the EWMA and the in-flight count — the regression test starts three in-flight requests,
//! recompiles the table, and asserts the permit count is unchanged.

use crate::breaker::Breaker;
use crate::limits::TokenBucket;
use apexrouter_core::config::RouterCfg;
use apexrouter_protocol::{Backend, BackendId};
use arc_swap::ArcSwap;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use tokio::sync::Semaphore;

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
}
