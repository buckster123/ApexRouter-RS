//! OWNER: unit R-07 (router/src/telemetry.rs). Do not edit outside that unit.
//!
//! The request ring, the broadcast and `/metrics`.
//!
//! `RequestStarted`/`RequestFinished` are only **serialised** when
//! `tx.receiver_count() > 0`, and `UsageTick` is coalesced to 1 Hz — a router at 50 rps must
//! not drown its own dashboard.
//!
//! llama.cpp's `/slots` is read internally and **never proxied outward**: it echoes prompts.

use crate::registry::BackendRegistry;
use apexrouter_protocol::{Alias, BackendId, Event, RequestRecord, RigSnapshot, UsageSummary};
use std::collections::VecDeque;
use tokio::sync::{broadcast, Mutex};

/// The rolling request record and the metrics view over it.
pub struct Telemetry {
    /// Bounded ring of recent requests.
    ring: Mutex<VecDeque<RequestRecord>>,
    /// The broadcast every surface subscribes to.
    tx: broadcast::Sender<Event>,
    /* R-07: counters, histograms, the 1 Hz coalescer */
}

impl Telemetry {
    /// Record a finished request and broadcast it, if anybody is listening.
    pub fn record(&self, r: RequestRecord) {
        todo!("R-07: Telemetry::record")
    }

    /// The most recent records, optionally filtered.
    pub fn recent(
        &self,
        limit: usize,
        alias: Option<&Alias>,
        backend: Option<&BackendId>,
    ) -> Vec<RequestRecord> {
        todo!("R-07: Telemetry::recent")
    }

    /// Prometheus text exposition: `apexrouter_requests_total{alias,backend,status}`,
    /// `apexrouter_ttft_seconds`, `apexrouter_tokens_total{kind}`,
    /// `apexrouter_tokens_per_second`, `apexrouter_backend_up{backend}`,
    /// `apexrouter_inflight{backend}`, `apexrouter_queue_depth`,
    /// `apexrouter_cost_usd_total{provider}`, `apexrouter_vram_free_mb{device}`.
    pub fn prometheus(&self, reg: &BackendRegistry, rig: Option<&RigSnapshot>) -> String {
        todo!("R-07: Telemetry::prometheus")
    }

    /// The rolling window, at most once a second.
    pub fn tick(&self) -> Option<UsageSummary> {
        todo!("R-07: Telemetry::tick")
    }
}
