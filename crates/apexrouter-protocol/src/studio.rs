//! Studio lifecycle types — the multi-service rented box (LLM + Comfy lanes).
//!
//! Charter: `docs/STUDIO.md` S2–S5. **ComfyUI lanes are ServiceRecords, never Backends**
//! (S2). Records hold **facts only** (S3); liveness is computed on read by a prober that is
//! not in this crate. Recipe shapes are additive: [`RecipeKind::VastStudio`] sits beside
//! [`RecipeKind::Vast`], and [`ContainerLaunch`] stays one-image/one-port for single-service
//! rentals.

use crate::catalog::ImageType;
use crate::endpoint::DesiredState;
use crate::fit::FitPlan;
use crate::ids::{Alias, BackendId, InstanceId, ProfileId, RecipeId, ServiceId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Recipe side — what we plan to launch
// ---------------------------------------------------------------------------

/// What a studio service process speaks. Deliberately **not** an extension of
/// [`crate::catalog::ContainerRuntime`]: that enum is Stage-0 published and one-image/one-
/// port; ComfyUI is a different animal (S3).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceRuntime {
    /// `llama-server` OpenAI path.
    LlamaCpp,
    /// vLLM OpenAI path.
    Vllm,
    /// ComfyUI — image or video lane. Never a Backend.
    ComfyUi,
}

/// How to probe a service for readiness. Spec only — the answer is never persisted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServiceHealthProbe {
    /// `GET /v1/models` through the tunnel → 2xx.
    OpenAiModels,
    /// `GET /system_stats` through the tunnel → 2xx (ComfyUI).
    ComfySystemStats,
    /// Escape hatch for a future probe shape.
    HttpGet {
        /// Path, e.g. `"/health"`.
        path: String,
        /// Optional substring that must appear in the body.
        #[serde(default)]
        body_contains: Option<String>,
    },
}

/// Whether this service joins the OpenAI routing table.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServiceRouting {
    /// OpenAI-compatible — registers as Backend + alias through the proxy.
    OpenAi {
        /// Alias clients put in `model`. When `None`, derived from the service name.
        #[serde(default)]
        alias: Option<Alias>,
    },
    /// Lifecycle + tunnel only. Comfy lanes live here (S2).
    LocalOnly,
}

/// One service inside a studio recipe — the planning shape.
///
/// `local_port` of 8811/8812 is the S5 promise; ordinary leases leave it `None` and the
/// allocator picks outside the 8810–8819 slice.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServiceSpec {
    /// Short name: `"llm"`, `"video"`, `"image"`.
    pub name: String,
    /// What process runs.
    pub runtime: ServiceRuntime,
    /// In-container listen port (8000 / 8188 / 8189).
    pub port: u16,
    /// Device tokens (`"0"`, `"1"`, …) for CUDA_VISIBLE_DEVICES and friends.
    #[serde(default)]
    pub devices: Vec<String>,
    /// Static VRAM reservation for this lane, MiB (S7 — fit() is not taught torch).
    pub reserved_mb: u32,
    /// Per-service env additions, merged into the container env at rent time.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Readiness probe *spec*.
    pub health: ServiceHealthProbe,
    /// OpenAI table membership.
    pub routing: ServiceRouting,
    /// What we expect the LLM slot to fit, when this is an OpenAI lane.
    #[serde(default)]
    pub fit: Option<FitPlan>,
    /// Fixed local tunnel port, or `None` for an ordinary lease outside 8810–8819.
    #[serde(default)]
    pub local_port: Option<u16>,
}

/// The multi-service container contract for a studio box.
///
/// Sibling of [`crate::catalog::ContainerLaunch`]. One image, many services, fixed local
/// ports for the creative lanes. `onstart` runs the idempotent `studio.sh` (S6).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StudioLaunch {
    /// Resolved from `[docker].studio` by `ImageType::Studio`.
    pub image: String,
    /// Always [`ImageType::Studio`] for a studio recipe; kept explicit for serde honesty.
    pub image_type: ImageType,
    /// Disk to request, GB — weights live here across park/wake.
    pub disk_gb: u32,
    /// Shared env map. `HF_TOKEN` lives here, never in `onstart`.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// `"bash /app/studio.sh > /var/log/studio.log 2>&1 &"`.
    pub onstart: String,
    /// ALWAYS `127.0.0.1` — tunnel-only posture.
    pub host: String,
    /// `false` by default; public exposure requires a freshly minted key.
    pub expose_public: bool,
    /// The services this box is supposed to run.
    pub services: Vec<ServiceSpec>,
}

// ---------------------------------------------------------------------------
// State side — facts only
// ---------------------------------------------------------------------------

/// One non-Backend (or co-resident) service on a rented studio box. Persisted at
/// `$STATE/services.json`. **Facts only** — no `status: "running"` string.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServiceRecord {
    /// Stable id (`studio-video`, `studio-image`, …).
    pub id: ServiceId,
    /// Which vast contract owns the box.
    pub instance_id: InstanceId,
    /// Short name matching the recipe (`"video"`, `"image"`, `"llm"`).
    pub name: String,
    /// What process is supposed to be running.
    pub runtime: ServiceRuntime,
    /// In-container port.
    pub remote_port: u16,
    /// Local tunnel port (8811/8812 fixed, or a lease).
    pub local_port: u16,
    /// Probe *spec* — the prober computes the answer on read.
    pub health: ServiceHealthProbe,
    /// Device tokens reserved for this lane.
    #[serde(default)]
    pub devices: Vec<String>,
    /// Static VRAM reservation, MiB.
    pub reserved_mb: u32,
    /// Expectation IS state.
    pub desired: DesiredState,
    /// When this record was written, unix seconds.
    pub started_at_unix: i64,
}

/// The manifest that makes "the whole studio" a defined thing for park/wake/status.
/// Persisted at `$STATE/studio.json`. One active studio per state dir for mk1.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StudioRecord {
    /// The vast contract.
    pub instance_id: InstanceId,
    /// Physical host pin when rented from a favorite (★140330).
    #[serde(default)]
    pub machine_id: Option<u64>,
    /// The recipe that produced this posture, when known.
    #[serde(default)]
    pub recipe_id: Option<RecipeId>,
    /// Search profile used on the rent path.
    #[serde(default)]
    pub profile_id: Option<ProfileId>,
    /// Every service on the box (Comfy + OpenAI slots that are ServiceRecords).
    #[serde(default)]
    pub service_ids: Vec<ServiceId>,
    /// OpenAI-routed endpoints (Backend ids) that belong to this studio.
    #[serde(default)]
    pub endpoint_ids: Vec<BackendId>,
    /// When the studio was first brought up, unix seconds.
    pub created_at_unix: i64,
    /// Last converge / wake touch, unix seconds.
    pub updated_at_unix: i64,
}

// ---------------------------------------------------------------------------
// Computed status (never persisted — invariant 3)
// ---------------------------------------------------------------------------

/// What the svc_prober last observed for one ServiceRecord. **Computed on read.**
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ServiceLiveness {
    /// Prober has not spoken yet.
    Unknown,
    /// TCP/HTTP in flight or connection refused that may still be loading.
    Starting {
        /// Free-text detail (e.g. `"connection refused"`).
        #[serde(default)]
        detail: Option<String>,
    },
    /// Probe path answered 2xx.
    Ready,
    /// Process expectation is Running but the probe is dead.
    Down {
        /// Why.
        detail: String,
    },
    /// Observed VRAM exceeds the static reservation (S7 alert path).
    ExceedsReservation {
        /// What `/system_stats` reported, MiB.
        observed_mb: u32,
        /// What the ServiceRecord reserved, MiB.
        reserved_mb: u32,
    },
}

/// Live view of one service: facts + computed liveness. Not written to disk.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServiceStatus {
    /// The persisted facts.
    pub record: ServiceRecord,
    /// What the prober last saw.
    pub liveness: ServiceLiveness,
    /// Observed VRAM when the probe can report it (Comfy `/system_stats`).
    #[serde(default)]
    pub observed_vram_mb: Option<u32>,
    /// When the last probe finished, unix seconds. `0` = never.
    pub last_probe_unix: i64,
}

/// Per-device remainder after Comfy static reservations (S7). Pure planning output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StudioDeviceBudget {
    /// Device token (`"0"`, `"1"`, …).
    pub device: String,
    /// Capacity we are planning against, MiB (usually card total or free-at-rent).
    pub capacity_mb: u64,
    /// Σ `ServiceSpec.reserved_mb` for lanes on this device, MiB.
    pub reserved_mb: u64,
    /// Global headroom held back on every device, MiB.
    pub headroom_mb: u64,
    /// `capacity − reserved − headroom`, saturating. What `fit()` may spend for the llm.
    pub free_for_llm_mb: u64,
}

/// Full studio VRAM plan: per-device remainders + notes. Never a sum across devices.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StudioBudget {
    /// One row per device that appears in any service's `devices` list (or explicit caps).
    #[serde(default)]
    pub devices: Vec<StudioDeviceBudget>,
    /// Human-readable arithmetic, same spirit as `FitPlan::why`.
    #[serde(default)]
    pub notes: Vec<String>,
}

// ---------------------------------------------------------------------------
// Wire types for the studio verb (phase 7)
// ---------------------------------------------------------------------------

/// `POST /v1/studio/up` body.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StudioUpRequest {
    /// Recipe id. Default `studio-96gb`.
    #[serde(default)]
    pub recipe_id: Option<RecipeId>,
    /// Without this, the call returns a cost preview and creates/wakes nothing.
    pub confirm: bool,
    /// Requested ceiling for rent/wake. Still subject to the daemon-side hard ceiling.
    pub max_usd_per_hour: f64,
    /// Override the recipe's machine pin (★ favorite).
    #[serde(default)]
    pub machine_id: Option<u64>,
    /// Pin a specific offer instead of searching (advanced).
    #[serde(default)]
    pub offer_id: Option<u64>,
}

/// Which path `studio up` took (or will take).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StudioUpPath {
    /// `$STATE/studio.json` names a parked instance → wake + restore tunnels.
    Wake,
    /// Instance is already running → re-establish tunnels + readiness.
    Converge,
    /// No studio (or box gone) → rent a new one.
    Rent,
}

/// Snapshot of the active studio for `GET /v1/studio` and CLI status.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StudioStatus {
    /// The manifest, if any.
    #[serde(default)]
    pub studio: Option<StudioRecord>,
    /// Every ServiceRecord on disk for this studio.
    #[serde(default)]
    pub services: Vec<ServiceRecord>,
    /// Computed liveness (empty when the prober has not spoken).
    #[serde(default)]
    pub service_status: Vec<ServiceStatus>,
    /// Tunnels belonging to the studio instance.
    #[serde(default)]
    pub tunnels: Vec<crate::endpoint::TunnelStatus>,
    /// Live instance phase when the fleet cache has it.
    #[serde(default)]
    pub instance_phase: Option<crate::backend::BootPhase>,
    /// Live dph when known.
    #[serde(default)]
    pub dph_total: Option<f64>,
    /// Which path `up` would take right now.
    pub next_up_path: StudioUpPath,
    /// Free-text summary for humans.
    pub summary: String,
}

/// Seed helper: the R3-measured 96 GB dual-4090 posture (S2 table), without weights.
///
/// Ports: llm remote 8000 / local lease; video 8188→**8811**; image 8189→**8812**.
/// Reservations: video 23_000 MiB, image 32_000 MiB. Operators re-measure on recipe change.
pub fn studio_96gb_services() -> Vec<ServiceSpec> {
    use crate::ids::Alias;
    vec![
        ServiceSpec {
            name: "llm".into(),
            runtime: ServiceRuntime::LlamaCpp,
            port: 8000,
            devices: vec!["0".into()],
            reserved_mb: 0, // fit()-solved against remainder after video reservation
            env: BTreeMap::new(),
            health: ServiceHealthProbe::OpenAiModels,
            routing: ServiceRouting::OpenAi {
                alias: Some(Alias::parse("studio-llm").expect("alias")),
            },
            fit: None,
            local_port: None,
        },
        ServiceSpec {
            name: "video".into(),
            runtime: ServiceRuntime::ComfyUi,
            port: 8188,
            devices: vec!["0".into()],
            reserved_mb: 23_000,
            env: BTreeMap::new(),
            health: ServiceHealthProbe::ComfySystemStats,
            routing: ServiceRouting::LocalOnly,
            fit: None,
            local_port: Some(crate::STUDIO_VIDEO_PORT),
        },
        ServiceSpec {
            name: "image".into(),
            runtime: ServiceRuntime::ComfyUi,
            port: 8189,
            devices: vec!["1".into()],
            reserved_mb: 32_000,
            env: BTreeMap::new(),
            health: ServiceHealthProbe::ComfySystemStats,
            routing: ServiceRouting::LocalOnly,
            fit: None,
            local_port: Some(crate::STUDIO_IMAGE_PORT),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DEFAULT_STUDIO_PORT_RANGE, STUDIO_IMAGE_PORT, STUDIO_VIDEO_PORT};

    #[test]
    fn studio_ports_are_the_s5_promise() {
        assert_eq!(DEFAULT_STUDIO_PORT_RANGE, (8810, 8819));
        assert_eq!(STUDIO_VIDEO_PORT, 8811);
        assert_eq!(STUDIO_IMAGE_PORT, 8812);
        assert!(STUDIO_VIDEO_PORT >= DEFAULT_STUDIO_PORT_RANGE.0);
        assert!(STUDIO_IMAGE_PORT <= DEFAULT_STUDIO_PORT_RANGE.1);
    }

    #[test]
    fn service_and_studio_records_round_trip() {
        let svc = ServiceRecord {
            id: ServiceId::parse("studio-video").expect("id"),
            instance_id: InstanceId(14_033_000),
            name: "video".into(),
            runtime: ServiceRuntime::ComfyUi,
            remote_port: 8188,
            local_port: STUDIO_VIDEO_PORT,
            health: ServiceHealthProbe::ComfySystemStats,
            devices: vec!["0".into()],
            reserved_mb: 23_000,
            desired: DesiredState::Running,
            started_at_unix: 1_780_000_000,
        };
        let s = serde_json::to_string(&svc).expect("ser");
        assert_eq!(serde_json::from_str::<ServiceRecord>(&s).expect("de"), svc);

        let studio = StudioRecord {
            instance_id: InstanceId(14_033_000),
            machine_id: Some(140_330),
            recipe_id: Some(RecipeId::parse("studio-96gb").expect("id")),
            profile_id: Some(ProfileId::parse("studio-96gb").expect("id")),
            service_ids: vec![
                ServiceId::parse("studio-llm").expect("id"),
                ServiceId::parse("studio-video").expect("id"),
                ServiceId::parse("studio-image").expect("id"),
            ],
            endpoint_ids: vec![BackendId::parse("vast-studio-llm").expect("id")],
            created_at_unix: 1_780_000_000,
            updated_at_unix: 1_780_000_100,
        };
        let s = serde_json::to_string(&studio).expect("ser");
        assert_eq!(
            serde_json::from_str::<StudioRecord>(&s).expect("de"),
            studio
        );
    }

    #[test]
    fn pre_studio_state_dir_still_deserializes_empty() {
        // Absent / empty services.json → empty vec; absent studio.json → None at the store.
        let empty: Vec<ServiceRecord> = serde_json::from_str("[]").expect("empty services");
        assert!(empty.is_empty());
        // Extra fields on a record must not kill an older peer reading a newer file.
        let with_extra = r#"{
            "id": "studio-video",
            "instance_id": 1,
            "name": "video",
            "runtime": "comfy_ui",
            "remote_port": 8188,
            "local_port": 8811,
            "health": {"kind": "comfy_system_stats"},
            "reserved_mb": 23000,
            "desired": "running",
            "started_at_unix": 0,
            "future_field": true
        }"#;
        let rec: ServiceRecord = serde_json::from_str(with_extra).expect("tolerant");
        assert_eq!(rec.local_port, 8811);
    }

    #[test]
    fn studio_96gb_seed_matches_s2_ports_and_reservations() {
        let svcs = studio_96gb_services();
        assert_eq!(svcs.len(), 3);
        let video = svcs.iter().find(|s| s.name == "video").expect("video");
        let image = svcs.iter().find(|s| s.name == "image").expect("image");
        let llm = svcs.iter().find(|s| s.name == "llm").expect("llm");
        assert_eq!(video.local_port, Some(STUDIO_VIDEO_PORT));
        assert_eq!(image.local_port, Some(STUDIO_IMAGE_PORT));
        assert_eq!(video.reserved_mb, 23_000);
        assert_eq!(image.reserved_mb, 32_000);
        assert!(matches!(llm.routing, ServiceRouting::OpenAi { .. }));
        assert!(matches!(video.routing, ServiceRouting::LocalOnly));
        assert!(matches!(image.routing, ServiceRouting::LocalOnly));
    }

    #[test]
    fn service_runtime_never_is_container_runtime() {
        // Compile-time-ish guard: the two enums are distinct types.
        let a = ServiceRuntime::ComfyUi;
        let _ = serde_json::to_string(&a).expect("ser");
        assert_eq!(
            serde_json::to_string(&ServiceRuntime::ComfyUi).expect("ser"),
            "\"comfy_ui\""
        );
    }
}
