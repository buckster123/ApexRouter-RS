//! OWNER: studio phase 6 (server/src/svc_prober.rs).
//!
//! Probes **ServiceRecords** (Comfy lanes + co-resident facts). Never touches the routing
//! table — ComfyUI is not a Backend (STUDIO.md S2). Liveness is computed on read from
//! HTTP through the local tunnel port; nothing is written into the ServiceRecord.
//!
//! Probe shapes (from [`ServiceHealthProbe`]):
//! * `ComfySystemStats` → `GET http://127.0.0.1:{local}/system_stats`
//! * `OpenAiModels`     → `GET http://127.0.0.1:{local}/v1/models`
//! * `HttpGet`          → `GET` the declared path
//!
//! When Comfy reports VRAM above the static reservation, emit a `Serious` alert (S7). The
//! barrier / studio verb (phase 7) consumes the same status cache.

use crate::state::AppState;
use apexrouter_protocol::{
    AlertLevel, Event, ServiceHealthProbe, ServiceId, ServiceLiveness, ServiceRecord, ServiceStatus,
};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// Floor on the probe interval (same spirit as the OpenAI prober).
const MIN_INTERVAL: Duration = Duration::from_millis(500);
/// Cap on one probe RTT.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// In-process cache of last probe results. Facts stay on disk; this is computed state.
#[derive(Default)]
pub struct ServiceStatusCache {
    inner: RwLock<HashMap<ServiceId, ServiceStatus>>,
}

impl ServiceStatusCache {
    /// Snapshot every known status (for `GET /v1/services` later).
    pub fn all(&self) -> Vec<ServiceStatus> {
        self.inner
            .read()
            .map(|g| g.values().cloned().collect())
            .unwrap_or_default()
    }

    /// One service, if we have probed it.
    pub fn get(&self, id: &ServiceId) -> Option<ServiceStatus> {
        self.inner.read().ok().and_then(|g| g.get(id).cloned())
    }

    fn put(&self, status: ServiceStatus) {
        if let Ok(mut g) = self.inner.write() {
            g.insert(status.record.id.clone(), status);
        }
    }
}

/// Run for the daemon's lifetime. No-op when `services.json` is empty.
pub async fn service_prober(state: Arc<AppState>) {
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(1))
        .timeout(PROBE_TIMEOUT)
        .pool_max_idle_per_host(2)
        .build()
        .unwrap_or_default();

    loop {
        let interval =
            Duration::from_millis(state.cfg.load().supervisor.health_interval_ms).max(MIN_INTERVAL);
        if let Err(e) = probe_round(&state, &http).await {
            tracing::debug!(error = %e, "svc_prober round failed");
        }
        tokio::time::sleep(interval).await;
    }
}

/// One pass over every ServiceRecord. Crate-visible for tests.
pub(crate) async fn probe_round(
    state: &Arc<AppState>,
    http: &reqwest::Client,
) -> anyhow::Result<()> {
    let records = state.store.load_services().unwrap_or_default();
    if records.is_empty() {
        return Ok(());
    }
    let now = chrono::Utc::now().timestamp();
    for rec in records {
        let status = probe_one(http, &rec, now).await;
        if let ServiceLiveness::ExceedsReservation {
            observed_mb,
            reserved_mb,
        } = &status.liveness
        {
            state.alert(
                AlertLevel::Serious,
                &format!("studio.vram.{}", rec.id),
                format!(
                    "service `{}` observed {observed_mb} MiB VRAM > reserved {reserved_mb} MiB \
                     — the co-resident llm may OOM; re-measure the recipe (S7)",
                    rec.name
                ),
            );
        }
        // Broadcast only on change so a 1 Hz prober does not drown the bus.
        let prev = state.service_status.get(&rec.id);
        let changed = prev.as_ref().map(|p| p.liveness != status.liveness) != Some(false);
        state.service_status.put(status.clone());
        if changed {
            let _ = state.tx.send(Event::ServiceChanged {
                service: Box::new(rec),
            });
        }
    }
    Ok(())
}

async fn probe_one(http: &reqwest::Client, rec: &ServiceRecord, now: i64) -> ServiceStatus {
    let url = match &rec.health {
        ServiceHealthProbe::ComfySystemStats => {
            format!("http://127.0.0.1:{}/system_stats", rec.local_port)
        }
        ServiceHealthProbe::OpenAiModels => {
            format!("http://127.0.0.1:{}/v1/models", rec.local_port)
        }
        ServiceHealthProbe::HttpGet { path, .. } => {
            let path = if path.starts_with('/') {
                path.clone()
            } else {
                format!("/{path}")
            };
            format!("http://127.0.0.1:{}{path}", rec.local_port)
        }
    };

    let response = http.get(&url).send().await;
    let (liveness, observed) = match response {
        Ok(resp) if resp.status().is_success() => {
            let body = resp.text().await.unwrap_or_default();
            let vram = parse_comfy_vram_mb(&body);
            let live = match (&rec.health, vram) {
                (ServiceHealthProbe::ComfySystemStats, Some(obs))
                    if rec.reserved_mb > 0 && obs > rec.reserved_mb =>
                {
                    ServiceLiveness::ExceedsReservation {
                        observed_mb: obs,
                        reserved_mb: rec.reserved_mb,
                    }
                }
                _ => ServiceLiveness::Ready,
            };
            // Optional body_contains check for HttpGet.
            if let ServiceHealthProbe::HttpGet {
                body_contains: Some(needle),
                ..
            } = &rec.health
            {
                if !body.contains(needle.as_str()) {
                    (
                        ServiceLiveness::Down {
                            detail: format!("body missing expected substring {needle:?}"),
                        },
                        vram,
                    )
                } else {
                    (live, vram)
                }
            } else {
                (live, vram)
            }
        }
        Ok(resp) => (
            ServiceLiveness::Starting {
                detail: Some(format!("http {}", resp.status().as_u16())),
            },
            None,
        ),
        Err(e) => {
            let detail = e.to_string();
            // Connection refused during Comfy torch import is progress, not death (S8).
            if detail.contains("Connection refused") || detail.contains("error trying to connect") {
                (
                    ServiceLiveness::Starting {
                        detail: Some(detail),
                    },
                    None,
                )
            } else {
                (ServiceLiveness::Down { detail }, None)
            }
        }
    };

    ServiceStatus {
        record: rec.clone(),
        liveness,
        observed_vram_mb: observed,
        last_probe_unix: now,
    }
}

/// Best-effort parse of ComfyUI `/system_stats` for a device VRAM figure, MiB.
///
/// Comfy returns JSON with `devices[].vram_total` / `vram_free` in **bytes**. We report
/// used = total − free when both are present.
fn parse_comfy_vram_mb(body: &str) -> Option<u32> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let devices = v.get("devices")?.as_array()?;
    let mut used: u64 = 0;
    let mut any = false;
    for d in devices {
        let total = d.get("vram_total").and_then(|x| x.as_u64()).unwrap_or(0);
        let free = d.get("vram_free").and_then(|x| x.as_u64()).unwrap_or(0);
        if total > 0 {
            used = used.saturating_add(total.saturating_sub(free));
            any = true;
        }
    }
    if !any {
        return None;
    }
    Some((used / (1024 * 1024)) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_comfy_system_stats_vram() {
        let body = r#"{
            "system": {"os": "linux"},
            "devices": [
                {"name": "cuda:0", "type": "cuda", "vram_total": 51539607552, "vram_free": 28991029248}
            ]
        }"#;
        // used = 51539607552 - 28991029248 = 22548578304 ≈ 21504 MiB
        let mb = parse_comfy_vram_mb(body).expect("vram");
        assert!((21_000..22_000).contains(&mb), "got {mb}");
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse_comfy_vram_mb("not json").is_none());
        assert!(parse_comfy_vram_mb("{}").is_none());
    }
}
