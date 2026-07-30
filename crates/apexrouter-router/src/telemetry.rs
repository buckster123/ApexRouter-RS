//! OWNER: unit R-07 (router/src/telemetry.rs). Do not edit outside that unit.
//!
//! The request ring, the broadcast and `/metrics`.
//!
//! `RequestStarted`/`RequestFinished` are only **serialised** when
//! `tx.receiver_count() > 0`, and `UsageTick` is coalesced to 1 Hz — a router at 50 rps must
//! not drown its own dashboard.
//!
//! llama.cpp's `/slots` is read internally and **never proxied outward**: it echoes prompts.
//!
//! Three things are deliberately separate here:
//!
//! * the **ring** is bounded and lossy — it answers "what happened lately";
//! * the **counters** are unbounded and monotonic — a Prometheus counter may never go
//!   backwards because a record aged out of the ring;
//! * the **1 Hz coalescer** is a compare-and-swap on a millisecond stamp, so a hundred
//!   concurrent `tick()` callers still produce at most one `UsageTick` per second.

use crate::registry::BackendRegistry;
use apexrouter_protocol::{
    Alias, Backend, BackendId, BackendKind, CostEstimate, CredentialSource, Event, Money,
    RequestRecord, RigSnapshot, UsageBucket, UsageSummary,
};
use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;
use tokio::sync::broadcast;

/// Upper edges of the `apexrouter_ttft_seconds` histogram, in seconds.
///
/// Chosen for what this router actually serves: a warm local `llama-server` answers in tens
/// of milliseconds, a cold rented box can take half a minute to produce its first token, and
/// anything past two minutes is a defect rather than a latency.
const TTFT_BUCKETS: [f64; 12] = [
    0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 30.0, 60.0, 120.0,
];

/// The window `tick()` summarises, in seconds.
const TICK_WINDOW_SECS: i64 = 60;

/// Minimum spacing between two `UsageTick`s, in milliseconds.
const TICK_INTERVAL_MS: i64 = 1_000;

/// Label used when a record has no backend, or a backend the registry no longer knows.
const UNKNOWN: &str = "unknown";

/// Monotonic, unbounded counters. Never truncated by the ring.
#[derive(Default)]
struct Counters {
    /// `(alias, backend, status) -> count`.
    requests: BTreeMap<(String, String, u16), u64>,
    /// Cumulative histogram buckets, aligned with [`TTFT_BUCKETS`].
    ttft_buckets: [u64; TTFT_BUCKETS.len()],
    /// Sum of every observed TTFT, in seconds.
    ttft_sum: f64,
    /// How many TTFTs were observed.
    ttft_count: u64,
    /// Prompt tokens, reported or estimated.
    tokens_prompt: u64,
    /// Completion tokens, reported or estimated.
    tokens_completion: u64,
    /// Cache-hit prompt tokens (`timings.cache_n`).
    tokens_cached: u64,
    /// EWMA of observed throughput. `None` until an upstream reports one.
    tps_ewma: Option<f64>,
    /// Accumulated micro-USD per backend id. Resolved to a provider at render time, so a
    /// backend that is renamed or removed cannot retroactively rewrite history.
    cost_micros: BTreeMap<String, i64>,
}

/// One backend's live numbers, read once so every gauge in a single `/metrics` body sees the
/// same instant rather than drifting between families.
struct BackendGauges {
    /// Which backend.
    id: BackendId,
    /// Its description, as of this read.
    meta: std::sync::Arc<Backend>,
    /// Requests the router has outstanding against it.
    inflight: u32,
    /// Permits nobody is holding right now.
    available: u32,
    /// False while draining.
    accepting: bool,
}

impl BackendGauges {
    /// Requests waiting for a permit.
    ///
    /// A `Semaphore` cannot report its own total, so the pool size comes from the description
    /// the registry sized it from. When neither number is known the fallback assumes every
    /// in-flight request already holds a permit, which reports zero queueing rather than
    /// inventing some.
    fn queued(&self) -> u64 {
        let capacity = self
            .meta
            .limits
            .slots_total
            .filter(|s| *s > 0)
            .or(Some(self.meta.limits.max_concurrent).filter(|c| *c > 0))
            .map_or(self.available.saturating_add(self.inflight), |c| {
                c.max(self.available)
            });
        let held = capacity.saturating_sub(self.available);
        u64::from(self.inflight.saturating_sub(held))
    }
}

/// One group key's running totals inside a [`Telemetry::window`] pass.
#[derive(Default)]
struct Bucket {
    /// Folded cost. `None` until the first record seeds it — `CostEstimate::add` demotes
    /// `Unknown + x`, and an empty accumulator is not a missing price.
    cost: Option<CostEstimate>,
    /// Prompt tokens.
    prompt: u64,
    /// Completion tokens.
    completion: u64,
    /// How many requests landed in this bucket.
    requests: u64,
    /// Every observed throughput, for the median.
    tps: Vec<f32>,
}

/// The rolling request record and the metrics view over it.
pub struct Telemetry {
    /// Bounded ring of recent requests.
    ring: Mutex<VecDeque<RequestRecord>>,
    /// The broadcast every surface subscribes to.
    tx: broadcast::Sender<Event>,
    /// How many records the ring keeps.
    ring_capacity: usize,
    /// Monotonic counters and histograms for `/metrics`.
    counters: Mutex<Counters>,
    /// Origin for the coalescer's millisecond clock.
    started: Instant,
    /// When the last `UsageTick` went out, ms since `started`; `i64::MIN` means never.
    last_tick_ms: AtomicI64,
    /// How many events were actually serialised onto the broadcast. Proves the
    /// `receiver_count() > 0` gate.
    broadcasts: AtomicU64,
}

impl Telemetry {
    /// Build a telemetry sink that keeps `ring_capacity` recent records (minimum 1).
    ///
    /// NOTE FOR THE ORCHESTRATOR: `BUILD-PLAN.md` §4 R-07 publishes no constructor, and the
    /// struct's fields are private, so without this the type is unconstructible outside this
    /// module. Reported in `signature_problems` rather than silently assumed.
    pub fn new(tx: broadcast::Sender<Event>, ring_capacity: usize) -> Telemetry {
        let cap = ring_capacity.max(1);
        Telemetry {
            ring: Mutex::new(VecDeque::with_capacity(cap)),
            tx,
            ring_capacity: cap,
            counters: Mutex::new(Counters::default()),
            started: Instant::now(),
            last_tick_ms: AtomicI64::new(i64::MIN),
            broadcasts: AtomicU64::new(0),
        }
    }

    /// Record a finished request and broadcast it, if anybody is listening.
    ///
    /// The counters are updated unconditionally; the `Event` is only built — and the record
    /// only cloned — when `tx.receiver_count() > 0`.
    pub fn record(&self, r: RequestRecord) {
        self.observe(&r);
        if self.tx.receiver_count() > 0 {
            let ev = Event::RequestFinished {
                record: Box::new(r.clone()),
            };
            if self.tx.send(ev).is_ok() {
                self.broadcasts.fetch_add(1, Ordering::Relaxed);
            }
        }
        let mut ring = self.lock_ring();
        while ring.len() >= self.ring_capacity {
            ring.pop_front();
        }
        ring.push_back(r);
    }

    /// The most recent records, optionally filtered.
    ///
    /// Newest first. Filters are ANDed; a record with no alias (or no backend) never matches
    /// an alias (or backend) filter.
    pub fn recent(
        &self,
        limit: usize,
        alias: Option<&Alias>,
        backend: Option<&BackendId>,
    ) -> Vec<RequestRecord> {
        let ring = self.lock_ring();
        ring.iter()
            .rev()
            .filter(|r| alias.is_none() || r.alias.as_ref() == alias)
            .filter(|r| backend.is_none() || r.backend.as_ref() == backend)
            .take(limit)
            .cloned()
            .collect()
    }

    /// Prometheus text exposition: `apexrouter_requests_total{alias,backend,status}`,
    /// `apexrouter_ttft_seconds`, `apexrouter_tokens_total{kind}`,
    /// `apexrouter_tokens_per_second`, `apexrouter_backend_up{backend}`,
    /// `apexrouter_inflight{backend}`, `apexrouter_queue_depth`,
    /// `apexrouter_cost_usd_total{provider}`, `apexrouter_vram_free_mb{device}`.
    ///
    /// Every family is always declared, even when it has no series yet, so a dashboard does
    /// not have to special-case a freshly started daemon.
    pub fn prometheus(&self, reg: &BackendRegistry, rig: Option<&RigSnapshot>) -> String {
        let live = reg.all();
        let metas: Vec<BackendGauges> = live
            .iter()
            .map(|b| BackendGauges {
                id: b.id.clone(),
                meta: b.meta.load_full(),
                inflight: b.inflight.load(Ordering::Relaxed),
                available: u32::try_from(b.sem.available_permits()).unwrap_or(u32::MAX),
                accepting: b.accepting.load(Ordering::Relaxed),
            })
            .collect();

        let c = self.lock_counters();
        let mut out = String::with_capacity(4096);

        family(
            &mut out,
            "apexrouter_requests_total",
            "counter",
            "Proxied requests, by alias, backend and final status.",
        );
        for ((alias, backend, status), n) in &c.requests {
            let _ = writeln!(
                out,
                "apexrouter_requests_total{{alias=\"{}\",backend=\"{}\",status=\"{}\"}} {n}",
                esc(alias),
                esc(backend),
                status
            );
        }

        family(
            &mut out,
            "apexrouter_ttft_seconds",
            "histogram",
            "Time to first byte of an upstream response, in seconds.",
        );
        for (i, edge) in TTFT_BUCKETS.iter().enumerate() {
            let _ = writeln!(
                out,
                "apexrouter_ttft_seconds_bucket{{le=\"{}\"}} {}",
                num(*edge),
                c.ttft_buckets[i]
            );
        }
        let _ = writeln!(
            out,
            "apexrouter_ttft_seconds_bucket{{le=\"+Inf\"}} {}",
            c.ttft_count
        );
        let _ = writeln!(out, "apexrouter_ttft_seconds_sum {}", num(c.ttft_sum));
        let _ = writeln!(out, "apexrouter_ttft_seconds_count {}", c.ttft_count);

        family(
            &mut out,
            "apexrouter_tokens_total",
            "counter",
            "Tokens seen, by kind. Estimated counts are included; honesty lives on the record.",
        );
        let _ = writeln!(
            out,
            "apexrouter_tokens_total{{kind=\"prompt\"}} {}",
            c.tokens_prompt
        );
        let _ = writeln!(
            out,
            "apexrouter_tokens_total{{kind=\"completion\"}} {}",
            c.tokens_completion
        );
        let _ = writeln!(
            out,
            "apexrouter_tokens_total{{kind=\"cached\"}} {}",
            c.tokens_cached
        );

        family(
            &mut out,
            "apexrouter_tokens_per_second",
            "gauge",
            "EWMA of upstream-reported generation throughput. NaN until one is reported.",
        );
        let _ = writeln!(
            out,
            "apexrouter_tokens_per_second {}",
            num(c.tps_ewma.unwrap_or(f64::NAN))
        );

        family(
            &mut out,
            "apexrouter_backend_up",
            "gauge",
            "1 when a backend is routable (health Ready and still accepting), else 0.",
        );
        for b in &metas {
            let up = u8::from(b.meta.enabled && b.accepting && b.meta.health.is_routable());
            let _ = writeln!(
                out,
                "apexrouter_backend_up{{backend=\"{}\"}} {up}",
                esc(b.id.as_str())
            );
        }

        family(
            &mut out,
            "apexrouter_inflight",
            "gauge",
            "Requests the router currently has outstanding against a backend.",
        );
        for b in &metas {
            let _ = writeln!(
                out,
                "apexrouter_inflight{{backend=\"{}\"}} {}",
                esc(b.id.as_str()),
                b.inflight
            );
        }

        family(
            &mut out,
            "apexrouter_queue_depth",
            "gauge",
            "Requests waiting for a backend permit: in-flight minus permits currently held.",
        );
        let queued: u64 = metas.iter().map(BackendGauges::queued).sum();
        let _ = writeln!(out, "apexrouter_queue_depth {queued}");

        family(
            &mut out,
            "apexrouter_cost_usd_total",
            "counter",
            "Accumulated request cost in USD, by provider. Estimates are included.",
        );
        let mut provider_of: BTreeMap<&str, String> = BTreeMap::new();
        for b in &metas {
            provider_of.insert(b.id.as_str(), provider_label(&b.meta));
        }
        let mut by_provider: BTreeMap<String, i64> = BTreeMap::new();
        for (backend, micros) in &c.cost_micros {
            let p = provider_of
                .get(backend.as_str())
                .cloned()
                .unwrap_or_else(|| UNKNOWN.to_owned());
            let slot = by_provider.entry(p).or_insert(0);
            *slot = slot.saturating_add(*micros);
        }
        for (provider, micros) in &by_provider {
            let _ = writeln!(
                out,
                "apexrouter_cost_usd_total{{provider=\"{}\"}} {}",
                esc(provider),
                num(Money(*micros).as_usd())
            );
        }

        family(
            &mut out,
            "apexrouter_vram_free_mb",
            "gauge",
            "Free VRAM per device, MiB, as of the last rig scan.",
        );
        for gpu in rig.map(|r| r.gpus.as_slice()).unwrap_or(&[]) {
            let _ = writeln!(
                out,
                "apexrouter_vram_free_mb{{device=\"{}\"}} {}",
                esc(&gpu.device),
                gpu.vram_free_mb
            );
        }

        out
    }

    /// The rolling window, at most once a second.
    ///
    /// Returns `None` when it is too soon. When it does fire it also broadcasts
    /// [`Event::UsageTick`] — but only when somebody is subscribed.
    pub fn tick(&self) -> Option<UsageSummary> {
        let now = i64::try_from(self.started.elapsed().as_millis()).unwrap_or(i64::MAX);
        let last = self.last_tick_ms.load(Ordering::Acquire);
        if last != i64::MIN && now.saturating_sub(last) < TICK_INTERVAL_MS {
            return None;
        }
        if self
            .last_tick_ms
            .compare_exchange(last, now, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            // Another thread ticked in the meantime. One tick per second, not one per caller.
            return None;
        }
        let summary = self.window(TICK_WINDOW_SECS);
        if self.tx.receiver_count() > 0 {
            let ev = Event::UsageTick {
                window: Box::new(summary.clone()),
            };
            if self.tx.send(ev).is_ok() {
                self.broadcasts.fetch_add(1, Ordering::Relaxed);
            }
        }
        Some(summary)
    }

    /// Summarise every record in the ring newer than `secs` seconds ago.
    fn window(&self, secs: i64) -> UsageSummary {
        let cutoff = chrono::Utc::now().timestamp() - secs;
        let ring = self.lock_ring();

        // `CostEstimate::add` demotes `Unknown + x` to `Approximate`, which is right for a
        // real missing price and wrong for an empty accumulator — so the accumulator starts
        // as `None` and the first record seeds it. A window of purely `Metered` rows stays
        // `Metered`.
        let mut total_cost: Option<CostEstimate> = None;
        let mut total_prompt: u64 = 0;
        let mut total_completion: u64 = 0;
        let mut rows: u64 = 0;
        let mut buckets: BTreeMap<String, Bucket> = BTreeMap::new();

        for r in ring.iter().filter(|r| r.started_unix >= cutoff) {
            let prompt = u64::from(r.prompt_tokens.map(|t| t.value()).unwrap_or(0));
            let completion = u64::from(r.completion_tokens.map(|t| t.value()).unwrap_or(0));
            rows += 1;
            total_prompt = total_prompt.saturating_add(prompt);
            total_completion = total_completion.saturating_add(completion);
            total_cost = Some(match total_cost.take() {
                Some(acc) => acc.add(r.cost.clone()),
                None => r.cost.clone(),
            });

            let key = match (&r.backend, &r.alias) {
                (Some(b), _) => b.as_str().to_owned(),
                (None, Some(a)) => a.as_str().to_owned(),
                (None, None) => UNKNOWN.to_owned(),
            };
            let slot = buckets.entry(key).or_default();
            slot.cost = Some(match slot.cost.take() {
                Some(acc) => acc.add(r.cost.clone()),
                None => r.cost.clone(),
            });
            slot.prompt = slot.prompt.saturating_add(prompt);
            slot.completion = slot.completion.saturating_add(completion);
            slot.requests += 1;
            if let Some(tps) = r.tok_per_s {
                slot.tps.push(tps);
            }
        }

        let by = buckets
            .into_iter()
            .map(|(key, b)| UsageBucket {
                key,
                cost: b.cost.unwrap_or(CostEstimate::Unknown),
                prompt_tokens: b.prompt,
                completion_tokens: b.completion,
                requests: b.requests,
                tok_per_s_p50: median(b.tps),
            })
            .collect();

        UsageSummary {
            window: format!("{secs}s"),
            by,
            total_cost: total_cost.unwrap_or(CostEstimate::Unknown),
            total_prompt,
            total_completion,
            rows,
        }
    }

    /// Fold one finished request into the monotonic counters.
    fn observe(&self, r: &RequestRecord) {
        let alias = r.alias.as_ref().map_or(UNKNOWN, |a| a.as_str()).to_owned();
        let backend_key = r
            .backend
            .as_ref()
            .map_or(UNKNOWN, |b| b.as_str())
            .to_owned();
        let mut c = self.lock_counters();

        *c.requests
            .entry((alias, backend_key.clone(), r.status))
            .or_insert(0) += 1;

        if let Some(ms) = r.ttft_ms {
            let secs = f64::from(ms) / 1000.0;
            c.ttft_count += 1;
            c.ttft_sum += secs;
            for (i, edge) in TTFT_BUCKETS.iter().enumerate() {
                if secs <= *edge {
                    c.ttft_buckets[i] += 1;
                }
            }
        }

        if let Some(t) = r.prompt_tokens {
            c.tokens_prompt = c.tokens_prompt.saturating_add(u64::from(t.value()));
        }
        if let Some(t) = r.completion_tokens {
            c.tokens_completion = c.tokens_completion.saturating_add(u64::from(t.value()));
        }
        if let Some(n) = r.cached_tokens {
            c.tokens_cached = c.tokens_cached.saturating_add(u64::from(n));
        }
        if let Some(tps) = r.tok_per_s {
            let tps = f64::from(tps);
            if tps.is_finite() && tps > 0.0 {
                c.tps_ewma = Some(match c.tps_ewma {
                    Some(prev) => prev * 0.8 + tps * 0.2,
                    None => tps,
                });
            }
        }
        if let Some(usd) = r.cost.usd() {
            let slot = c.cost_micros.entry(backend_key).or_insert(0);
            *slot = slot.saturating_add(usd.0);
        }
    }

    /// Lock the ring, recovering from a poisoned mutex rather than propagating a panic — a
    /// telemetry lock must never take the request path down with it.
    fn lock_ring(&self) -> std::sync::MutexGuard<'_, VecDeque<RequestRecord>> {
        self.ring.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Lock the counters, recovering from poisoning for the same reason.
    fn lock_counters(&self) -> std::sync::MutexGuard<'_, Counters> {
        self.counters.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// How many events were serialised onto the broadcast so far.
    #[cfg(test)]
    fn broadcasts(&self) -> u64 {
        self.broadcasts.load(Ordering::Relaxed)
    }

    /// Pretend the last tick happened `ms` milliseconds earlier, so the 1 Hz coalescer can be
    /// exercised without sleeping.
    #[cfg(test)]
    fn rewind_tick(&self, ms: i64) {
        let last = self.last_tick_ms.load(Ordering::Acquire);
        if last != i64::MIN {
            self.last_tick_ms
                .store(last.saturating_sub(ms), Ordering::Release);
        }
    }
}

/// Write the `# HELP` / `# TYPE` preamble of one metric family.
fn family(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

/// Escape a Prometheus label value: backslash, double quote and newline.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(ch),
        }
    }
    out
}

/// Render a float the way the exposition format wants it: `NaN`, `+Inf`, `-Inf` or a plain
/// decimal with no exponent surprises.
fn num(v: f64) -> String {
    if v.is_nan() {
        "NaN".to_owned()
    } else if v == f64::INFINITY {
        "+Inf".to_owned()
    } else if v == f64::NEG_INFINITY {
        "-Inf".to_owned()
    } else {
        let s = format!("{v:.6}");
        let s = s.trim_end_matches('0').trim_end_matches('.').to_owned();
        if s.is_empty() || s == "-" {
            "0".to_owned()
        } else {
            s
        }
    }
}

/// Median of an unordered sample, `None` when empty.
fn median(mut v: Vec<f32>) -> Option<f32> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(v[v.len() / 2])
}

/// Which provider a backend's spend belongs to. A managed backend names its credential
/// store (`together`, …); everything else is named by its kind.
fn provider_label(b: &Backend) -> String {
    match (&b.kind, &b.credential) {
        (BackendKind::Managed, CredentialSource::Managed { store }) if !store.is_empty() => {
            store.clone()
        }
        (BackendKind::LocalLlama, _) => "local_llama".to_owned(),
        (BackendKind::LocalVllm, _) => "local_vllm".to_owned(),
        (BackendKind::VastLlama, _) => "vast_llama".to_owned(),
        (BackendKind::VastVllm, _) => "vast_vllm".to_owned(),
        (BackendKind::Managed, _) => "managed".to_owned(),
        (BackendKind::Node, _) => "node".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_core::config::RouterCfg;
    use apexrouter_protocol::{
        Health, PriceSource, Protocol, Provenance, RequestId, RouteReason, TokenCount,
    };
    use std::collections::BTreeSet;

    fn alias(s: &str) -> Alias {
        Alias::parse(s).expect("alias")
    }

    fn backend_id(s: &str) -> BackendId {
        BackendId::parse(s).expect("backend id")
    }

    fn rec(a: Option<&str>, b: Option<&str>, status: u16) -> RequestRecord {
        RequestRecord {
            id: RequestId::new(),
            started_unix: chrono::Utc::now().timestamp(),
            alias: a.map(alias),
            backend: b.map(backend_id),
            upstream_model: Some("carnice-9b".into()),
            route_reason: RouteReason::Alias,
            ingress: Protocol::OpenAi,
            method: "POST".into(),
            path: "/v1/chat/completions".into(),
            status,
            attempts: 1,
            streamed: false,
            aborted: false,
            ttft_ms: Some(120),
            total_ms: 900,
            prompt_tokens: Some(TokenCount::Reported(10)),
            completion_tokens: Some(TokenCount::Estimated(20)),
            cached_tokens: Some(4),
            tok_per_s: Some(30.0),
            cost: CostEstimate::Metered {
                usd: Money(1_500),
                source: PriceSource::ConfigTable,
            },
            error: None,
        }
    }

    fn rig() -> RigSnapshot {
        use apexrouter_protocol::{Gpu, GpuBackend};
        RigSnapshot {
            gpus: vec![Gpu {
                device: "Vulkan0".into(),
                index: 0,
                name: "AMD Radeon 840M Graphics (RADV KRACKAN1)".into(),
                backend: GpuBackend::Vulkan,
                vram_total_mb: 8192,
                vram_free_mb: 6144,
                driver: Some("radv".into()),
                is_software: false,
                seen_by_builds: vec![],
                held_by: vec![],
                reserved_mb: 0,
            }],
            builds: Vec::new(),
            ram_total_mb: 24_000,
            ram_free_mb: 9_000,
            swap_total_mb: 8_000,
            swap_used_mb: 1_000,
            cpu_threads: 12,
            scanned_at_unix: 0,
        }
    }

    fn backend(id: &str, kind: BackendKind, credential: CredentialSource) -> Backend {
        Backend {
            id: backend_id(id),
            kind,
            protocol: Protocol::OpenAi,
            label: id.to_owned(),
            base_url: "http://127.0.0.1:8100".into(),
            credential,
            tags: vec![],
            models: vec![],
            limits: Default::default(),
            price: None,
            health: Health::Unknown,
            provenance: Provenance::Manual,
            endpoint: None,
            enabled: true,
            devices: vec![],
            last_error: None,
        }
    }

    /// A registry with one saturated, healthy local backend (4 slots, all permits taken, 6
    /// requests outstanding → 2 of them queued) and one managed backend that is down.
    fn registry() -> BackendRegistry {
        let cfg = RouterCfg::default();
        let reg = BackendRegistry::new();

        let mut a = backend("local-a", BackendKind::LocalLlama, CredentialSource::None);
        a.health = Health::Ready {
            since_unix: 0,
            slots_busy: 4,
            slots_total: 4,
            tps_p50: Some(30.0),
        };
        a.limits.max_concurrent = 4;
        a.limits.slots_total = Some(4);
        let live = reg.upsert(a, &cfg);
        live.inflight.store(6, Ordering::Relaxed);
        live.sem.try_acquire_many(4).expect("four permits").forget();

        let mut b = backend(
            "local-b",
            BackendKind::Managed,
            CredentialSource::Managed {
                store: "together".into(),
            },
        );
        b.health = Health::Down {
            reason: "connection refused".into(),
            retry_at_unix: 0,
        };
        reg.upsert(b, &cfg);
        reg
    }

    // ---- a standard Prometheus text-exposition validator -------------------------------

    #[derive(PartialEq, Eq, Clone, Copy, Debug)]
    enum Kind {
        Counter,
        Gauge,
        Histogram,
        Summary,
        Untyped,
    }

    fn parse_kind(s: &str) -> Option<Kind> {
        match s {
            "counter" => Some(Kind::Counter),
            "gauge" => Some(Kind::Gauge),
            "histogram" => Some(Kind::Histogram),
            "summary" => Some(Kind::Summary),
            "untyped" => Some(Kind::Untyped),
            _ => None,
        }
    }

    fn valid_metric_name(s: &str) -> bool {
        let mut cs = s.chars();
        match cs.next() {
            Some(c) if c.is_ascii_alphabetic() || c == '_' || c == ':' => {}
            _ => return false,
        }
        cs.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
    }

    fn valid_label_name(s: &str) -> bool {
        let mut cs = s.chars();
        match cs.next() {
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
            _ => return false,
        }
        cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
    }

    fn valid_value(s: &str) -> bool {
        matches!(s, "NaN" | "+Inf" | "-Inf" | "Inf") || s.parse::<f64>().is_ok()
    }

    /// A parsed sample line: metric name, label pairs, and everything after the labels.
    type Sample = (String, Vec<(String, String)>, String);

    /// Split `name{a="1",b="2"} 3` into the name, the label pairs and the value text.
    fn split_sample(line: &str) -> Result<Sample, String> {
        let (head, rest) = match line.find('{') {
            None => {
                let mut it = line.splitn(2, ' ');
                let name = it.next().unwrap_or_default().to_owned();
                let value = it.next().ok_or("sample has no value")?.to_owned();
                return Ok((name, vec![], value));
            }
            Some(i) => line.split_at(i),
        };
        let close = rest.find('}').ok_or("unterminated label set")?;
        let labels_src = &rest[1..close];
        let tail = rest[close + 1..].trim_start();
        if tail.is_empty() {
            return Err("sample has no value".into());
        }
        let mut labels = Vec::new();
        if !labels_src.is_empty() {
            for pair in labels_src.split(',') {
                let (k, v) = pair.split_once('=').ok_or("label is not k=v")?;
                let v = v.trim();
                if !v.starts_with('"') || !v.ends_with('"') || v.len() < 2 {
                    return Err(format!("label value is not quoted: {v}"));
                }
                labels.push((k.trim().to_owned(), v[1..v.len() - 1].to_owned()));
            }
        }
        Ok((head.to_owned(), labels, tail.to_owned()))
    }

    /// Validate a body against the Prometheus text exposition format: comments, one HELP and
    /// one TYPE per family, families contiguous, every sample declared, legal names, legal
    /// label sets, legal values.
    fn validate_exposition(body: &str) -> Result<BTreeSet<String>, String> {
        if !body.ends_with('\n') {
            return Err("body does not end with a newline".into());
        }
        if body.contains('\r') {
            return Err("body contains a carriage return".into());
        }
        let mut types: BTreeMap<String, Kind> = BTreeMap::new();
        let mut helps: BTreeSet<String> = BTreeSet::new();
        let mut closed: BTreeSet<String> = BTreeSet::new();
        let mut current: Option<String> = None;

        for (n, line) in body.lines().enumerate() {
            let ln = n + 1;
            if line.trim().is_empty() {
                continue;
            }
            if let Some(comment) = line.strip_prefix("# ") {
                let mut it = comment.splitn(3, ' ');
                match (it.next(), it.next(), it.next()) {
                    (Some("HELP"), Some(name), text) => {
                        if !valid_metric_name(name) {
                            return Err(format!("line {ln}: bad metric name {name}"));
                        }
                        if !helps.insert(name.to_owned()) {
                            return Err(format!("line {ln}: duplicate HELP for {name}"));
                        }
                        if text.unwrap_or("").trim().is_empty() {
                            return Err(format!("line {ln}: empty HELP for {name}"));
                        }
                    }
                    (Some("TYPE"), Some(name), kind) => {
                        let kind = kind.unwrap_or("").trim();
                        let kind = parse_kind(kind)
                            .ok_or_else(|| format!("line {ln}: bad TYPE {kind}"))?;
                        if !helps.contains(name) {
                            return Err(format!("line {ln}: TYPE {name} precedes its HELP"));
                        }
                        if types.insert(name.to_owned(), kind).is_some() {
                            return Err(format!("line {ln}: duplicate TYPE for {name}"));
                        }
                    }
                    _ => {}
                }
                continue;
            }
            if line.starts_with('#') {
                continue;
            }

            let (name, labels, value) =
                split_sample(line).map_err(|e| format!("line {ln}: {e}"))?;
            if !valid_metric_name(&name) {
                return Err(format!("line {ln}: bad metric name {name}"));
            }
            let mut seen: BTreeSet<String> = BTreeSet::new();
            for (k, _) in &labels {
                if !valid_label_name(k) {
                    return Err(format!("line {ln}: bad label name {k}"));
                }
                if !seen.insert(k.clone()) {
                    return Err(format!("line {ln}: duplicate label {k}"));
                }
            }
            let mut parts = value.split_whitespace();
            let v = parts.next().unwrap_or_default();
            if !valid_value(v) {
                return Err(format!("line {ln}: bad value {v}"));
            }
            if let Some(ts) = parts.next() {
                if ts.parse::<i64>().is_err() {
                    return Err(format!("line {ln}: bad timestamp {ts}"));
                }
            }
            if parts.next().is_some() {
                return Err(format!("line {ln}: trailing junk"));
            }

            // Resolve histogram/summary suffixes back to their family.
            let family = ["_bucket", "_sum", "_count"]
                .iter()
                .find_map(|suf| {
                    name.strip_suffix(*suf).filter(|base| {
                        matches!(
                            types.get(*base),
                            Some(Kind::Histogram) | Some(Kind::Summary)
                        )
                    })
                })
                .map_or_else(|| name.clone(), str::to_owned);

            if !types.contains_key(&family) {
                return Err(format!("line {ln}: sample {name} has no TYPE"));
            }
            if current.as_deref() != Some(family.as_str()) {
                if closed.contains(&family) {
                    return Err(format!("line {ln}: family {family} is not contiguous"));
                }
                if let Some(prev) = current.take() {
                    closed.insert(prev);
                }
                current = Some(family);
            }
        }
        Ok(types.keys().cloned().collect())
    }

    // ---- tests -------------------------------------------------------------------------

    #[test]
    fn the_ring_is_bounded_and_recent_is_newest_first() {
        let (tx, _rx) = broadcast::channel(16);
        let t = Telemetry::new(tx, 3);
        for i in 0..5u16 {
            t.record(rec(Some("auto"), Some("local-a"), 200 + i));
        }
        let recent = t.recent(10, None, None);
        assert_eq!(recent.len(), 3, "ring must be bounded at its capacity");
        assert_eq!(recent[0].status, 204, "newest first");
        assert_eq!(recent[2].status, 202);
        assert_eq!(t.recent(1, None, None).len(), 1, "limit is honoured");
    }

    #[test]
    fn recent_filters_by_alias_and_backend() {
        let (tx, _rx) = broadcast::channel(16);
        let t = Telemetry::new(tx, 16);
        t.record(rec(Some("auto"), Some("local-a"), 200));
        t.record(rec(Some("big"), Some("local-b"), 200));
        t.record(rec(None, None, 500));

        assert_eq!(t.recent(10, Some(&alias("auto")), None).len(), 1);
        assert_eq!(t.recent(10, None, Some(&backend_id("local-b"))).len(), 1);
        assert_eq!(
            t.recent(10, Some(&alias("auto")), Some(&backend_id("local-b")))
                .len(),
            0,
            "filters are ANDed"
        );
        assert_eq!(
            t.recent(10, Some(&alias("auto")), None)[0].backend,
            Some(backend_id("local-a"))
        );
    }

    #[test]
    fn request_finished_is_only_serialised_when_somebody_is_listening() {
        let (tx, rx) = broadcast::channel(16);
        drop(rx);
        let t = Telemetry::new(tx.clone(), 16);

        t.record(rec(Some("auto"), Some("local-a"), 200));
        assert_eq!(
            t.broadcasts(),
            0,
            "no receivers: the event must never be built"
        );
        assert_eq!(t.recent(10, None, None).len(), 1, "the ring still records");

        let mut rx = tx.subscribe();
        t.record(rec(Some("auto"), Some("local-a"), 201));
        assert_eq!(t.broadcasts(), 1, "one receiver: exactly one event");
        match rx.try_recv() {
            Ok(Event::RequestFinished { record }) => assert_eq!(record.status, 201),
            other => panic!("expected RequestFinished, got {other:?}"),
        }

        drop(rx);
        t.record(rec(Some("auto"), Some("local-a"), 202));
        assert_eq!(
            t.broadcasts(),
            1,
            "last receiver gone: back to not serialising"
        );
    }

    #[test]
    fn usage_tick_is_coalesced_to_one_hertz() {
        let (tx, mut rx) = broadcast::channel(16);
        let t = Telemetry::new(tx, 16);
        t.record(rec(Some("auto"), Some("local-a"), 200));
        assert!(
            matches!(rx.try_recv(), Ok(Event::RequestFinished { .. })),
            "the record itself is broadcast first"
        );

        let first = t.tick().expect("the first tick always fires");
        assert_eq!(first.window, "60s");
        assert_eq!(first.rows, 1);
        assert_eq!(first.total_prompt, 10);
        assert_eq!(first.total_completion, 20);
        assert_eq!(first.by.len(), 1);
        assert_eq!(first.by[0].key, "local-a");
        assert_eq!(first.by[0].requests, 1);
        assert_eq!(first.by[0].tok_per_s_p50, Some(30.0));
        assert!(matches!(rx.try_recv(), Ok(Event::UsageTick { .. })));

        assert!(t.tick().is_none(), "a second tick inside 1 s is suppressed");
        assert!(t.tick().is_none());
        assert!(rx.try_recv().is_err(), "and nothing else was broadcast");

        t.rewind_tick(1_500);
        assert!(t.tick().is_some(), "after a second it fires again");
        assert!(matches!(rx.try_recv(), Ok(Event::UsageTick { .. })));
    }

    #[test]
    fn a_window_older_than_the_tick_window_is_excluded() {
        let (tx, _rx) = broadcast::channel(16);
        let t = Telemetry::new(tx, 16);
        let mut old = rec(Some("auto"), Some("local-a"), 200);
        old.started_unix = chrono::Utc::now().timestamp() - 600;
        t.record(old);
        t.record(rec(Some("auto"), Some("local-a"), 200));
        let s = t.tick().expect("first tick");
        assert_eq!(s.rows, 1, "only the record inside the window counts");
    }

    #[test]
    fn prometheus_body_is_valid_exposition_and_names_every_metric() {
        let (tx, _rx) = broadcast::channel(16);
        let t = Telemetry::new(tx, 16);
        t.record(rec(Some("auto"), Some("local-a"), 200));
        t.record(rec(Some("auto"), Some("local-a"), 200));
        t.record(rec(Some("big"), Some("local-b"), 502));
        t.record(rec(None, None, 503));

        let rig = rig();
        let body = t.prometheus(&registry(), Some(&rig));
        let families = match validate_exposition(&body) {
            Ok(f) => f,
            Err(e) => panic!("invalid exposition format: {e}\n---\n{body}"),
        };

        for want in [
            "apexrouter_requests_total",
            "apexrouter_ttft_seconds",
            "apexrouter_tokens_total",
            "apexrouter_tokens_per_second",
            "apexrouter_backend_up",
            "apexrouter_inflight",
            "apexrouter_queue_depth",
            "apexrouter_cost_usd_total",
            "apexrouter_vram_free_mb",
        ] {
            assert!(families.contains(want), "§4.5 metric {want} is missing");
        }

        assert!(body.contains(
            "apexrouter_requests_total{alias=\"auto\",backend=\"local-a\",status=\"200\"} 2"
        ));
        assert!(body.contains(
            "apexrouter_requests_total{alias=\"unknown\",backend=\"unknown\",status=\"503\"} 1"
        ));
        assert!(body.contains("apexrouter_tokens_total{kind=\"prompt\"} 40"));
        assert!(body.contains("apexrouter_tokens_total{kind=\"completion\"} 80"));
        assert!(body.contains("apexrouter_tokens_total{kind=\"cached\"} 16"));
        assert!(body.contains("apexrouter_ttft_seconds_count 4"));
        assert!(body.contains("apexrouter_ttft_seconds_bucket{le=\"+Inf\"} 4"));
        assert!(body.contains("apexrouter_vram_free_mb{device=\"Vulkan0\"} 6144"));

        assert!(body.contains("apexrouter_backend_up{backend=\"local-a\"} 1"));
        assert!(
            body.contains("apexrouter_backend_up{backend=\"local-b\"} 0"),
            "a backend that is down is not up\n{body}"
        );
        assert!(body.contains("apexrouter_inflight{backend=\"local-a\"} 6"));
        assert!(body.contains("apexrouter_inflight{backend=\"local-b\"} 0"));
        assert!(
            body.contains("apexrouter_queue_depth 2"),
            "6 in flight against 4 slots, all taken, is 2 waiting\n{body}"
        );

        assert!(body.contains("apexrouter_cost_usd_total{provider=\"local_llama\"} 0.003"));
        assert!(
            body.contains("apexrouter_cost_usd_total{provider=\"together\"} 0.0015"),
            "a managed backend spends under its credential store's name\n{body}"
        );
        assert!(
            body.contains("apexrouter_cost_usd_total{provider=\"unknown\"} 0.0015"),
            "spend with no backend is still reported\n{body}"
        );
    }

    #[test]
    fn the_histogram_is_cumulative_and_ends_at_the_count() {
        let (tx, _rx) = broadcast::channel(16);
        let t = Telemetry::new(tx, 16);
        for ms in [10u32, 300, 4_000, 90_000] {
            let mut r = rec(Some("auto"), Some("local-a"), 200);
            r.ttft_ms = Some(ms);
            t.record(r);
        }
        let body = t.prometheus(&BackendRegistry::new(), None);
        let mut prev = 0u64;
        for line in body.lines().filter(|l| l.contains("_bucket{le=")) {
            let n: u64 = line
                .rsplit(' ')
                .next()
                .and_then(|v| v.parse().ok())
                .expect("bucket count");
            assert!(n >= prev, "buckets must be cumulative: {line}");
            prev = n;
        }
        assert_eq!(prev, 4, "the +Inf bucket equals the observation count");
        assert!(body.contains("apexrouter_ttft_seconds_bucket{le=\"0.05\"} 1"));
        assert!(body.contains("apexrouter_ttft_seconds_bucket{le=\"0.5\"} 2"));
        assert!(body.contains("apexrouter_ttft_seconds_bucket{le=\"5\"} 3"));
    }

    #[test]
    fn counters_are_monotonic_across_ring_eviction() {
        let (tx, _rx) = broadcast::channel(16);
        let t = Telemetry::new(tx, 1);
        for _ in 0..7 {
            t.record(rec(Some("auto"), Some("local-a"), 200));
        }
        assert_eq!(t.recent(10, None, None).len(), 1, "the ring dropped six");
        let body = t.prometheus(&BackendRegistry::new(), None);
        assert!(
            body.contains(
                "apexrouter_requests_total{alias=\"auto\",backend=\"local-a\",status=\"200\"} 7"
            ),
            "a counter must not go backwards when a record ages out\n{body}"
        );
    }

    #[test]
    fn label_values_are_escaped_and_floats_are_exposition_legal() {
        assert_eq!(esc("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
        assert_eq!(num(f64::NAN), "NaN");
        assert_eq!(num(f64::INFINITY), "+Inf");
        assert_eq!(num(f64::NEG_INFINITY), "-Inf");
        assert_eq!(num(0.0), "0");
        assert_eq!(num(1.5), "1.5");
        assert_eq!(num(0.000_001_5), "0.000002");
    }

    #[test]
    fn the_validator_rejects_bodies_it_should_reject() {
        assert!(validate_exposition("x 1\n").is_err(), "undeclared family");
        assert!(
            validate_exposition("# HELP x h\n# TYPE x counter\nx nope\n").is_err(),
            "non-numeric value"
        );
        assert!(
            validate_exposition("# HELP x h\n# TYPE x counter\nx 1").is_err(),
            "missing trailing newline"
        );
        assert!(
            validate_exposition(
                "# HELP x h\n# TYPE x counter\nx 1\n# HELP y h\n# TYPE y gauge\ny 1\nx 2\n"
            )
            .is_err(),
            "non-contiguous family"
        );
        assert!(
            validate_exposition("# HELP x h\n# TYPE x counter\nx{a=\"1\",a=\"2\"} 1\n").is_err(),
            "duplicate label"
        );
        assert!(
            validate_exposition("# HELP x h\n# TYPE x counter\nx{a=\"1\"} 1\n").is_ok(),
            "a well-formed body must pass"
        );
        assert!(
            validate_exposition(
                "# HELP x h\n# TYPE x histogram\nx_bucket{le=\"+Inf\"} 1\nx_sum 0.5\nx_count 1\n"
            )
            .is_ok(),
            "histogram suffixes belong to their family"
        );
    }

    #[test]
    fn provider_labels_name_managed_stores_and_kinds() {
        let mk = |kind, credential| Backend {
            id: backend_id("b"),
            kind,
            protocol: Protocol::OpenAi,
            label: "b".into(),
            base_url: "http://127.0.0.1:8100".into(),
            credential,
            tags: vec![],
            models: vec![],
            limits: Default::default(),
            price: None,
            health: Health::Unknown,
            provenance: Provenance::Manual,
            endpoint: None,
            enabled: true,
            devices: vec![],
            last_error: None,
        };
        assert_eq!(
            provider_label(&mk(
                BackendKind::Managed,
                CredentialSource::Managed {
                    store: "together".into()
                }
            )),
            "together"
        );
        assert_eq!(
            provider_label(&mk(BackendKind::Managed, CredentialSource::None)),
            "managed"
        );
        assert_eq!(
            provider_label(&mk(BackendKind::LocalLlama, CredentialSource::None)),
            "local_llama"
        );
        assert_eq!(
            provider_label(&mk(BackendKind::VastLlama, CredentialSource::Instance)),
            "vast_llama"
        );
    }
}
