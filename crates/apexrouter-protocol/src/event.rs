//! The WebSocket protocol, the snapshot every surface renders, and the `--json` envelope.
//!
//! `GET /ws` subscribes to the broadcast **before** sending the snapshot, re-sends a full
//! snapshot on `RecvError::Lagged`, and coalesces `UsageTick` to 1 Hz — a router at 50 rps
//! must not drown its own dashboard.

use crate::backend::{Backend, BootPhase};
use crate::catalog::{Recipe, SearchProfile};
use crate::check::CheckResult as CheckResultBody;
use crate::endpoint::{EndpointRecord, TunnelStatus};
use crate::ids::{Alias, BackendId, InstanceId, JobId, RequestId, ServiceId};
use crate::money::{CostEstimate, Money};
use crate::provider::ProviderStatus;
use crate::rig::RigSnapshot;
use crate::route::ModelRoute;
use crate::studio::{ServiceRecord, StudioRecord};
use crate::telemetry::{RequestRecord, UsageSummary};
use crate::vast::VastInstance;
use serde::{Deserialize, Serialize};

/// Everything the daemon broadcasts. `PartialEq` so a no-op change is never sent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// The full picture. Sent on connect and after a `Lagged`.
    Snapshot(Box<Snapshot>),
    /// One backend's description changed.
    BackendChanged {
        /// The new description.
        backend: Box<Backend>,
    },
    /// A backend went away.
    BackendRemoved {
        /// Which one.
        id: BackendId,
    },
    /// The table recompiled — or failed to, in which case the previous one is still serving.
    RouteTableChanged {
        /// The routes now in effect.
        routes: Vec<ModelRoute>,
        /// Whether the on-disk table compiled.
        valid: bool,
        /// The parse or validation error, when it did not.
        #[serde(default)]
        error: Option<String>,
    },
    /// A rig rescan landed.
    RigChanged {
        /// The new snapshot.
        rig: Box<RigSnapshot>,
    },
    /// A request began. Only serialised when somebody is listening.
    RequestStarted {
        /// Which request.
        id: RequestId,
        /// Which alias resolved.
        #[serde(default)]
        alias: Option<Alias>,
        /// Which backend it went to.
        #[serde(default)]
        backend: Option<BackendId>,
    },
    /// A request ended, successfully or not.
    RequestFinished {
        /// The complete record.
        record: Box<RequestRecord>,
    },
    /// A boot state machine advanced. Drives the live boot view in both GUIs.
    BootProgress {
        /// Which backend is coming up.
        backend: BackendId,
        /// Where it is now.
        phase: BootPhase,
        /// The log line that caused the transition.
        #[serde(default)]
        line: Option<String>,
    },
    /// A tailed log line.
    LogLine {
        /// Where it came from.
        source: LogSource,
        /// The line, verbatim.
        line: String,
    },
    /// The rented fleet changed, or credit was re-read.
    VastFleetChanged {
        /// Every live instance.
        instances: Vec<VastInstance>,
        /// Credit remaining, when we could read it.
        #[serde(default)]
        credit: Option<f64>,
    },
    /// Rolling usage. Coalesced to 1 Hz.
    UsageTick {
        /// The window.
        window: Box<UsageSummary>,
    },
    /// A background job changed state.
    JobChanged {
        /// The record.
        job: Box<JobRecord>,
    },
    /// One check landed. Streamed as each finishes, not batched at the end.
    CheckResult {
        /// The result.
        result: CheckResultBody,
    },
    /// Something the operator needs to see.
    Alert {
        /// How loud.
        level: AlertLevel,
        /// What happened.
        message: String,
        /// What can be done about it, as a verb the UI can wire to a button.
        #[serde(default)]
        action: Option<String>,
        /// Stable id, so repeats coalesce instead of stacking.
        id: String,
    },
    /// A studio service record changed (S3 / S16). Facts only — liveness is on the status surface.
    ServiceChanged {
        /// The new description.
        service: Box<ServiceRecord>,
    },
    /// A studio service went away.
    ServiceRemoved {
        /// Which one.
        id: ServiceId,
    },
    /// The active studio manifest changed (or cleared).
    StudioChanged {
        /// The new manifest, or `None` when no studio is recorded.
        #[serde(default)]
        studio: Option<Box<StudioRecord>>,
    },
}

/// How loud an alert is. `Critical` is reserved for money leaking.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertLevel {
    /// Worth knowing.
    Info,
    /// Worth looking at.
    Warning,
    /// Worth acting on soon.
    Serious,
    /// Acting on now — e.g. a box billing with no local record.
    Critical,
}

/// Where a log line came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "src", rename_all = "snake_case")]
pub enum LogSource {
    /// A supervised endpoint.
    Endpoint {
        /// Which one.
        id: BackendId,
    },
    /// A rented instance.
    Instance {
        /// Which one.
        id: InstanceId,
    },
    /// The daemon itself.
    Daemon,
}

/// A standing alert, as carried in a [`Snapshot`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Alert {
    /// Stable id, so repeats coalesce.
    pub id: String,
    /// How loud.
    pub level: AlertLevel,
    /// What happened.
    pub message: String,
    /// What can be done about it.
    #[serde(default)]
    pub action: Option<String>,
    /// When it was raised, unix seconds.
    pub at_unix: i64,
}

/// The full picture, as both GUIs and `apexrouter status` render it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// `"apexrouter"`.
    pub product: String,
    /// Crate version.
    pub version: String,
    /// Whether a daemon answered, or this came off disk.
    pub served_by: ServedBy,
    /// When it was assembled, unix seconds.
    pub as_of_unix: i64,
    /// True when poller-derived fields could not be refreshed.
    pub stale: bool,
    /// The data plane's own state.
    pub proxy: ProxyStatus,
    /// Every backend.
    #[serde(default)]
    pub backends: Vec<Backend>,
    /// Every route.
    #[serde(default)]
    pub routes: Vec<ModelRoute>,
    /// Every endpoint record.
    #[serde(default)]
    pub endpoints: Vec<EndpointRecord>,
    /// GPUs, builds, RAM, swap.
    pub rig: RigSnapshot,
    /// Rented boxes.
    #[serde(default)]
    pub instances: Vec<VastInstance>,
    /// SSH tunnels.
    #[serde(default)]
    pub tunnels: Vec<TunnelStatus>,
    /// Studio service records (Comfy lanes + co-resident facts). Empty pre-studio.
    #[serde(default)]
    pub services: Vec<ServiceRecord>,
    /// Active studio manifest, when one is recorded.
    #[serde(default)]
    pub studio: Option<StudioRecord>,
    /// Managed providers.
    #[serde(default)]
    pub providers: Vec<ProviderStatus>,
    /// Saved launch plans.
    #[serde(default)]
    pub recipes: Vec<Recipe>,
    /// Saved market queries.
    #[serde(default)]
    pub profiles: Vec<SearchProfile>,
    /// Spend, tokens, credit, burn-down.
    pub totals: Totals,
    /// Standing alerts.
    #[serde(default)]
    pub alerts: Vec<Alert>,
    /// Background jobs.
    #[serde(default)]
    pub jobs: Vec<JobRecord>,
}

/// Where an answer came from. On **every** `--json` envelope, so a script can tell without
/// parsing prose.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServedBy {
    /// A running daemon answered.
    Daemon,
    /// Read from `$STATE` under a shared lock, with no daemon running.
    Offline,
}

/// The data plane's own state — the top bar of every surface.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProxyStatus {
    /// What to put in `OPENAI_BASE_URL`. The one string the user copies.
    pub base_url: String,
    /// Where the control plane is, discovered from the lock file's owner record.
    pub control_url: String,
    /// Daemon uptime.
    pub uptime_secs: f64,
    /// Requests in flight right now.
    pub inflight: u32,
    /// Rolling rate.
    pub req_per_min: f32,
    /// Aggregate throughput.
    pub tok_per_s: f32,
    /// Where an unrecognised or legacy model name lands.
    pub default_alias: Alias,
    /// Whether the on-disk table compiled. `false` means the previous table is still serving.
    pub table_valid: bool,
    /// The compile error, when there is one.
    #[serde(default)]
    pub table_error: Option<String>,
}

/// Money and tokens at a glance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Totals {
    /// Spend in the last 24 hours.
    pub spend_24h: CostEstimate,
    /// Spend in the last 7 days.
    pub spend_7d: CostEstimate,
    /// Tokens in the last 24 hours.
    pub tokens_24h: u64,
    /// vast.ai credit remaining.
    #[serde(default)]
    pub vast_credit: Option<f64>,
    /// Current hourly burn across every rented box.
    pub burn_rate_usd_hr: Money,
    /// Hours of credit left at the current burn. `None` when nothing is burning.
    #[serde(default)]
    pub burn_down_hours: Option<f32>,
}

/// One background operation behind `?no_wait=true`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JobRecord {
    /// The id returned immediately.
    pub id: JobId,
    /// `"hf.download"`, `"vast.rent"`, `"endpoint.start"`, …
    pub kind: String,
    /// Where it is. **Never** `Pending` forever: every error path, including a `JoinError`
    /// from a panicking task, flips it to `Failed`.
    pub state: JobState,
    /// Progress, when derivable.
    #[serde(default)]
    pub pct: Option<f32>,
    /// What it is doing right now.
    #[serde(default)]
    pub message: Option<String>,
    /// Unix seconds.
    pub started_unix: i64,
    /// Unix seconds, once finished.
    #[serde(default)]
    pub finished_unix: Option<i64>,
    /// The protocol type the operation produced, as JSON.
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    /// Why it failed.
    #[serde(default)]
    pub error: Option<String>,
}

/// A job's lifecycle.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Accepted, not started.
    Pending,
    /// Running.
    Running,
    /// Done.
    Succeeded,
    /// Failed, with `error` populated.
    Failed,
    /// Cancelled by a human.
    Cancelled,
}

/// The envelope EVERY `--json` CLI output and every control-plane `GET` is wrapped in.
///
/// `served_by`, `as_of_unix` and `stale` ride alongside the payload so a script can tell
/// where its answer came from without parsing prose.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Envelope<T> {
    /// Daemon or offline.
    pub served_by: ServedBy,
    /// When the payload was assembled, unix seconds.
    pub as_of_unix: i64,
    /// True when poller-derived fields could not be refreshed.
    pub stale: bool,
    /// The payload, flattened alongside the envelope keys. `T` must serialise as a map.
    #[serde(flatten)]
    pub data: T,
}

/// The `--json` failure shape, printed on stdout with exit code 1.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorEnvelope {
    /// The error.
    pub error: ErrorBody,
}

/// A machine-readable failure. `kind` is the discriminator, so no exit codes had to be
/// invented.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    /// `"model_not_found"`, `"upstream_unavailable"`, `"port_in_use"`, …
    pub kind: String,
    /// Human-readable.
    pub message: String,
    /// Which parameter, when the failure is about one.
    #[serde(default)]
    pub param: Option<String>,
    /// A finer code, when there is one.
    #[serde(default)]
    pub code: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{BackendKind, BackendLimits, CredentialSource, Health, Provenance};
    use crate::check::{CheckId, CheckStatus};
    use crate::telemetry::UsageSummary;

    fn backend() -> Backend {
        Backend {
            id: BackendId::parse("local-carnice").expect("id"),
            kind: BackendKind::LocalLlama,
            protocol: Default::default(),
            label: "Carnice".into(),
            base_url: "http://127.0.0.1:8100".into(),
            credential: CredentialSource::None,
            tags: vec![],
            models: vec![],
            limits: BackendLimits::default(),
            price: None,
            health: Health::Unknown,
            provenance: Provenance::Spawned,
            endpoint: None,
            enabled: true,
            devices: vec![],
            last_error: None,
        }
    }

    fn proxy_status() -> ProxyStatus {
        ProxyStatus {
            base_url: "http://127.0.0.1:8888".into(),
            control_url: "http://127.0.0.1:2739".into(),
            uptime_secs: 12.5,
            inflight: 0,
            req_per_min: 0.0,
            tok_per_s: 0.0,
            default_alias: Alias::parse("auto").expect("alias"),
            table_valid: true,
            table_error: None,
        }
    }

    fn totals() -> Totals {
        Totals {
            spend_24h: CostEstimate::Unknown,
            spend_7d: CostEstimate::Unknown,
            tokens_24h: 0,
            vast_credit: Some(7.73),
            burn_rate_usd_hr: Money::ZERO,
            burn_down_hours: None,
        }
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            product: crate::PRODUCT.into(),
            version: crate::VERSION.into(),
            served_by: ServedBy::Daemon,
            as_of_unix: 1_785_412_331,
            stale: false,
            proxy: proxy_status(),
            backends: vec![backend()],
            routes: vec![],
            endpoints: vec![],
            rig: RigSnapshot::default(),
            instances: vec![],
            tunnels: vec![],
            services: vec![],
            studio: None,
            providers: vec![],
            recipes: vec![],
            profiles: vec![],
            totals: totals(),
            alerts: vec![Alert {
                id: "vast.orphan.1".into(),
                level: AlertLevel::Critical,
                message: "an instance is billing with no local record".into(),
                action: Some("destroy".into()),
                at_unix: 1_785_412_331,
            }],
            jobs: vec![],
        }
    }

    fn job() -> JobRecord {
        JobRecord {
            id: JobId::new(),
            kind: "hf.download".into(),
            state: JobState::Running,
            pct: Some(42.0),
            message: Some("model-00001-of-00002.gguf".into()),
            started_unix: 1_785_412_331,
            finished_unix: None,
            result: None,
            error: None,
        }
    }

    /// Every `Event` variant must survive the WebSocket unchanged: the internally tagged
    /// representation is the one place a newtype payload silently stops round-tripping.
    #[test]
    fn every_event_variant_round_trips() {
        let events = vec![
            Event::Snapshot(Box::new(snapshot())),
            Event::BackendChanged {
                backend: Box::new(backend()),
            },
            Event::BackendRemoved {
                id: BackendId::parse("local-carnice").expect("id"),
            },
            Event::RouteTableChanged {
                routes: vec![],
                valid: false,
                error: Some("duplicate alias \"auto\"".into()),
            },
            Event::RigChanged {
                rig: Box::new(RigSnapshot::default()),
            },
            Event::RequestStarted {
                id: RequestId::new(),
                alias: Some(Alias::parse("auto").expect("alias")),
                backend: None,
            },
            Event::RequestFinished {
                record: Box::new(sample_record()),
            },
            Event::BootProgress {
                backend: BackendId::parse("local-carnice").expect("id"),
                phase: BootPhase::Loading { pct: Some(10.0) },
                line: Some("load_tensors: ...".into()),
            },
            Event::LogLine {
                source: LogSource::Endpoint {
                    id: BackendId::parse("local-carnice").expect("id"),
                },
                line: "main: server is listening".into(),
            },
            Event::VastFleetChanged {
                instances: vec![],
                credit: Some(7.73),
            },
            Event::UsageTick {
                window: Box::new(UsageSummary {
                    window: "24h".into(),
                    by: vec![],
                    total_cost: CostEstimate::Unknown,
                    total_prompt: 0,
                    total_completion: 0,
                    rows: 0,
                }),
            },
            Event::JobChanged {
                job: Box::new(job()),
            },
            Event::CheckResult {
                result: CheckResultBody {
                    id: CheckId::from("creds.vast"),
                    label: "vast api key".into(),
                    status: CheckStatus::Pass,
                    ms: 1,
                    detail: "found".into(),
                    fix: None,
                },
            },
            Event::Alert {
                level: AlertLevel::Warning,
                message: "duplicate upstream model id after a rental".into(),
                action: None,
                id: "route.collision.1".into(),
            },
            Event::ServiceChanged {
                service: Box::new(crate::studio::ServiceRecord {
                    id: crate::ids::ServiceId::parse("studio-video").expect("id"),
                    instance_id: InstanceId(140_330),
                    name: "video".into(),
                    runtime: crate::studio::ServiceRuntime::ComfyUi,
                    remote_port: 8188,
                    local_port: crate::STUDIO_VIDEO_PORT,
                    health: crate::studio::ServiceHealthProbe::ComfySystemStats,
                    devices: vec!["0".into()],
                    reserved_mb: 23_000,
                    desired: crate::endpoint::DesiredState::Running,
                    started_at_unix: 1_785_412_331,
                }),
            },
            Event::ServiceRemoved {
                id: crate::ids::ServiceId::parse("studio-video").expect("id"),
            },
            Event::StudioChanged {
                studio: Some(Box::new(crate::studio::StudioRecord {
                    instance_id: InstanceId(140_330),
                    machine_id: Some(140_330),
                    recipe_id: None,
                    profile_id: None,
                    service_ids: vec![],
                    endpoint_ids: vec![],
                    created_at_unix: 1_785_412_331,
                    updated_at_unix: 1_785_412_331,
                })),
            },
        ];
        for e in &events {
            let s = serde_json::to_string(e).expect("ser");
            assert!(s.starts_with("{\"type\":\""), "internally tagged: {s}");
            assert_eq!(&serde_json::from_str::<Event>(&s).expect("de"), e);
        }
    }

    fn sample_record() -> RequestRecord {
        RequestRecord {
            id: RequestId::new(),
            started_unix: 1_785_412_331,
            alias: None,
            backend: None,
            upstream_model: None,
            route_reason: crate::route::RouteReason::LegacyModelName,
            ingress: Default::default(),
            method: "POST".into(),
            path: "/v1/chat/completions".into(),
            status: 200,
            attempts: 1,
            streamed: false,
            aborted: false,
            ttft_ms: None,
            total_ms: 12,
            prompt_tokens: None,
            completion_tokens: None,
            cached_tokens: None,
            tok_per_s: None,
            cost: CostEstimate::Unknown,
            error: None,
        }
    }

    #[test]
    fn snapshot_round_trips_whole() {
        let s0 = snapshot();
        let s = serde_json::to_string(&s0).expect("ser");
        assert_eq!(serde_json::from_str::<Snapshot>(&s).expect("de"), s0);
    }

    #[test]
    fn log_source_round_trips_every_variant() {
        for src in [
            LogSource::Endpoint {
                id: BackendId::parse("local-carnice").expect("id"),
            },
            LogSource::Instance {
                id: InstanceId(28_675_431),
            },
            LogSource::Daemon,
        ] {
            let s = serde_json::to_string(&src).expect("ser");
            assert_eq!(serde_json::from_str::<LogSource>(&s).expect("de"), src);
        }
        assert_eq!(
            serde_json::to_string(&LogSource::Daemon).expect("ser"),
            r#"{"src":"daemon"}"#
        );
    }

    #[test]
    fn envelope_flattens_alongside_its_payload() {
        let e = Envelope {
            served_by: ServedBy::Offline,
            as_of_unix: 1_785_412_331,
            stale: true,
            data: proxy_status(),
        };
        let s = serde_json::to_string(&e).expect("ser");
        assert!(s.contains("\"served_by\":\"offline\""), "{s}");
        assert!(s.contains("\"base_url\""), "{s}");
        assert_eq!(
            serde_json::from_str::<Envelope<ProxyStatus>>(&s).expect("de"),
            e
        );
    }

    #[test]
    fn error_envelope_round_trips() {
        let e = ErrorEnvelope {
            error: ErrorBody {
                kind: "model_not_found".into(),
                message: "unknown model \"gpt-4o-mimi\"; known aliases: auto, coder".into(),
                param: Some("model".into()),
                code: None,
            },
        };
        let s = serde_json::to_string(&e).expect("ser");
        assert_eq!(serde_json::from_str::<ErrorEnvelope>(&s).expect("de"), e);
    }

    #[test]
    fn job_and_alert_levels_round_trip() {
        let j = job();
        let s = serde_json::to_string(&j).expect("ser");
        assert_eq!(serde_json::from_str::<JobRecord>(&s).expect("de"), j);

        for st in [
            JobState::Pending,
            JobState::Running,
            JobState::Succeeded,
            JobState::Failed,
            JobState::Cancelled,
        ] {
            let x = serde_json::to_string(&st).expect("ser");
            assert_eq!(serde_json::from_str::<JobState>(&x).expect("de"), st);
        }
        for lv in [
            AlertLevel::Info,
            AlertLevel::Warning,
            AlertLevel::Serious,
            AlertLevel::Critical,
        ] {
            let x = serde_json::to_string(&lv).expect("ser");
            assert_eq!(serde_json::from_str::<AlertLevel>(&x).expect("de"), lv);
        }
        assert!(AlertLevel::Critical > AlertLevel::Info);
    }
}
