//! vast.ai: the market, the fleet and the money ledger.
//!
//! Two rules shape every type here. First, **unknown fields are preserved** via
//! `#[serde(flatten)] extra` — the API adds columns and we must not lose them. Second,
//! **nothing that costs money happens without a `SpendApproval`**, and the ledger row is
//! written before the billing call; `SpendApproval` itself lives in `apexrouter-core` so no
//! surface can fabricate one with a struct literal.

use crate::backend::BootPhase;
use crate::catalog::{ContainerLaunch, GeoFilter};
use crate::ids::{Alias, InstanceId, JobId, ProfileId};
use crate::money::CostEstimate;
use serde::{Deserialize, Serialize};

/// One rentable machine, as the market lists it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Offer {
    /// Offer id.
    pub id: u64,
    /// The ask contract this offer belongs to.
    #[serde(default)]
    pub ask_contract_id: Option<u64>,
    /// Physical machine id — several offers can share one.
    #[serde(default)]
    pub machine_id: Option<u64>,
    /// e.g. `"RTX 3090"`, `"H100 SXM"`. An exact string from the live vocabulary.
    pub gpu_name: String,
    /// How many GPUs in this offer.
    pub num_gpus: u32,
    /// VRAM per GPU, MiB.
    pub gpu_ram: u64,
    /// Pooled VRAM, MiB. This is what `fit()` gets.
    pub gpu_total_ram: u64,
    /// All-in dollars per hour.
    pub dph_total: f64,
    /// Base dollars per hour, before storage and bandwidth.
    #[serde(default)]
    pub dph_base: Option<f64>,
    /// Storage cost component.
    #[serde(default)]
    pub storage_cost: Option<f64>,
    /// Inbound bandwidth cost component.
    #[serde(default)]
    pub inet_down_cost: Option<f64>,
    /// Outbound bandwidth cost component.
    #[serde(default)]
    pub inet_up_cost: Option<f64>,
    /// Host RAM, MiB.
    #[serde(default)]
    pub cpu_ram: Option<u64>,
    /// Effective CPU cores allocated to this offer.
    #[serde(default)]
    pub cpu_cores_effective: Option<f64>,
    /// Host CPU model, e.g. `"AMD EPYC 9354"` — an i5 on a "verified" board is still a
    /// lemon, and this is how you see it before renting (GARDEN.md host doctrine).
    #[serde(default)]
    pub cpu_name: Option<String>,
    /// Host motherboard model, the other half of the host-class read.
    #[serde(default)]
    pub mobo_name: Option<String>,
    /// The hosting account behind the machine.
    #[serde(default)]
    pub host_id: Option<u64>,
    /// Disk, GB.
    #[serde(default)]
    pub disk_space: Option<f64>,
    /// Highest CUDA version the driver supports.
    #[serde(default)]
    pub cuda_max_good: Option<f64>,
    /// Driver version string.
    #[serde(default)]
    pub driver_version: Option<String>,
    /// `"Czechia, CZ"` — parse the tail, never string-equal the whole thing.
    #[serde(default)]
    pub geolocation: Option<String>,
    /// Inbound Mbps. A slow box turns a 30 GB pull into an hour of billing.
    #[serde(default)]
    pub inet_down: Option<f64>,
    /// Outbound Mbps.
    #[serde(default)]
    pub inet_up: Option<f64>,
    /// Host reliability, 0..1.
    #[serde(default)]
    pub reliability2: Option<f64>,
    /// How many direct ports the host will map.
    #[serde(default)]
    pub direct_port_count: Option<u32>,
    /// Whether the host has a static IP.
    #[serde(default)]
    pub static_ip: Option<bool>,
    /// Currently rented.
    #[serde(default)]
    pub rented: Option<bool>,
    /// Currently rentable.
    #[serde(default)]
    pub rentable: Option<bool>,
    /// Deep-learning performance score.
    #[serde(default)]
    pub dlperf: Option<f64>,
    /// Performance per dollar.
    #[serde(default)]
    pub dlperf_per_dphtotal: Option<f64>,
    /// How long the host commits to keep it available, seconds.
    #[serde(default)]
    pub duration: Option<f64>,
    /// When the host's commitment ends, unix seconds.
    #[serde(default)]
    pub end_date: Option<f64>,
    /// Everything else the API sent. Never dropped.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Offer {
    /// Pooled VRAM in MiB — what a model actually has to fit into.
    ///
    /// Prefers the API's own pooled figure; falls back to per-GPU × count, which is what the
    /// field means when the API omits the total.
    pub fn pooled_vram_mb(&self) -> u64 {
        if self.gpu_total_ram > 0 {
            self.gpu_total_ram
        } else {
            self.gpu_ram.saturating_mul(u64::from(self.num_gpus))
        }
    }

    /// The country code at the tail of `geolocation`, trimmed. `None` when absent or blank.
    pub fn geo_code(&self) -> Option<&str> {
        let g = self.geolocation.as_deref()?;
        let tail = g.rsplit(',').next().unwrap_or(g).trim();
        if tail.is_empty() {
            None
        } else {
            Some(tail)
        }
    }
}

/// One market query. **There is exactly one of these**, built by one builder, so "auto —
/// cheapest" can never rent from a stricter candidate set than the browser displayed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OfferQuery {
    /// Exact `gpu_name` strings.
    #[serde(default)]
    pub gpu_names: Vec<String>,
    /// Minimum GPU count.
    pub num_gpus_min: u32,
    /// Maximum GPU count.
    pub num_gpus_max: u32,
    /// Ceiling on `dph_total`.
    #[serde(default)]
    pub max_dph: Option<f64>,
    /// Floor on `reliability2`.
    #[serde(default)]
    pub min_reliability: Option<f64>,
    /// Floor on `inet_down`, Mbps.
    #[serde(default)]
    pub min_inet_down: Option<f64>,
    /// Floor on disk, GB.
    #[serde(default)]
    pub min_disk_gb: Option<u32>,
    /// Floor on `cuda_max_good`.
    #[serde(default)]
    pub min_cuda: Option<f64>,
    /// Client-side geography filter.
    pub geo: GeoFilter,
    /// Restrict to verified hosts.
    #[serde(default)]
    pub verified: Option<bool>,
    /// Row cap.
    pub limit: u32,
    /// `[(field, "asc"|"desc")]`.
    #[serde(default)]
    pub order: Vec<(String, String)>,
    /// Free-form passthrough merged into the `{"q": …}` body.
    #[serde(default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// What one search returned, **including how the query was relaxed to return it**.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OfferSearchResult {
    /// The rows.
    #[serde(default)]
    pub offers: Vec<Offer>,
    /// e.g. `"widened: geo dropped, reliability 0.99 -> 0.97"`. Every surface renders these
    /// as an explicit banner — a relaxation the user did not see is how you rent the wrong box.
    #[serde(default)]
    pub relaxations: Vec<String>,
    /// Unix seconds.
    pub queried_at_unix: i64,
    /// The live `gpu_name` vocabulary, for the dropdown. Never a compiled-in constant.
    #[serde(default)]
    pub gpu_name_vocabulary: Vec<String>,
}

/// The account, as `GET /users/current/` reports it.
///
/// **There is deliberately no `api_key` field**, even though the API echoes one.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VastAccount {
    /// Account id.
    pub id: u64,
    /// Credit remaining, dollars.
    pub credit: f64,
    /// Balance, dollars.
    #[serde(default)]
    pub balance: Option<f64>,
    /// Whether vast will let us bill.
    #[serde(default)]
    pub can_pay: Option<bool>,
    /// Whether billing is configured.
    #[serde(default)]
    pub has_billing: Option<bool>,
}

/// One rented box.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VastInstance {
    /// The contract id. The create call returns it as `new_contract`, not `id`.
    pub id: InstanceId,
    /// `"created"`, `"loading"`, `"running"`, `"stopped"`, `"exited"`, `"offline"`, …
    #[serde(default)]
    pub actual_status: Option<String>,
    /// The state the contract is asked to be in (`"running"` | `"stopped"`), when vast
    /// reports it. `actual_status: "stopped"` with `intended_status: "stopped"` is a parked
    /// box; the same status with `intended_status: "running"` is a box that failed to start.
    #[serde(default)]
    pub intended_status: Option<String>,
    /// A human-readable status line, when vast supplies one. **This is the truth channel**:
    /// a box can sit in `loading` forever while `status_msg` carries the fatal container
    /// error (GARDEN-RUNS.md, the R1 lemon).
    #[serde(default)]
    pub status_msg: Option<String>,
    /// The physical machine this contract runs on. Stable across ask re-mints, which is why
    /// favorites key on it and not on offer ids.
    #[serde(default)]
    pub machine_id: Option<u64>,
    /// Storage price, dollars per GB per month — what a parked box keeps costing.
    #[serde(default)]
    pub storage_cost: Option<f64>,
    /// `sshN.vast.ai`. Recycled, which is why we keep a dedicated `known_hosts`.
    #[serde(default)]
    pub ssh_host: Option<String>,
    /// SSH port.
    #[serde(default)]
    pub ssh_port: Option<u16>,
    /// The host's public address.
    #[serde(default)]
    pub public_ipaddr: Option<String>,
    /// The docs say `int[]`; the CLI reads a Docker map. Kept raw and read tolerantly by
    /// [`VastInstance::external_port`].
    #[serde(default)]
    pub ports: serde_json::Value,
    /// First mapped direct port.
    #[serde(default)]
    pub direct_port_start: Option<i32>,
    /// Last mapped direct port.
    #[serde(default)]
    pub direct_port_end: Option<i32>,
    /// GPU model.
    #[serde(default)]
    pub gpu_name: Option<String>,
    /// GPU count.
    #[serde(default)]
    pub num_gpus: Option<u32>,
    /// GPU utilisation, percent.
    #[serde(default)]
    pub gpu_util: Option<f64>,
    /// Dollars per hour we are being billed.
    #[serde(default)]
    pub dph_total: Option<f64>,
    /// `"Czechia, CZ"`.
    #[serde(default)]
    pub geolocation: Option<String>,
    /// The label we set at create time.
    #[serde(default)]
    pub label: Option<String>,
    /// When billing started, unix seconds.
    #[serde(default)]
    pub start_date: Option<f64>,
    /// Disk used, GB.
    #[serde(default)]
    pub disk_util: Option<f64>,
    /// Disk allocated, GB.
    #[serde(default)]
    pub disk_space: Option<f64>,
    /// Inbound Mbps.
    #[serde(default)]
    pub inet_down: Option<f64>,
    /// Everything else the API sent.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl VastInstance {
    /// Map vast's status vocabulary onto our boot machine.
    ///
    /// Anything unrecognised is `Provisioning`, not `Failed`: a status we have never seen is
    /// not evidence that a box we are paying for is dead.
    pub fn phase(&self) -> BootPhase {
        let status = self
            .actual_status
            .as_deref()
            .map(|s| s.trim().to_ascii_lowercase());
        match status.as_deref() {
            None => BootPhase::Reserved,
            Some("created") | Some("scheduling") | Some("starting") => BootPhase::Provisioning,
            Some("loading") | Some("pulling") => BootPhase::Pulling,
            Some("running") => BootPhase::Healthy,
            // A stopped box is NOT destroyed: it holds its disk, bills for it, and can be
            // woken. Reading it as `Destroyed` is how a fleet view lies about money.
            Some("stopped") | Some("inactive") => BootPhase::Parked,
            Some(terminal @ ("exited" | "offline" | "unknown")) => BootPhase::Failed {
                reason: self
                    .status_msg
                    .clone()
                    .unwrap_or_else(|| format!("instance is {terminal}")),
            },
            Some(_) => BootPhase::Provisioning,
        }
    }

    /// The `status_msg`, flattened to one line and trimmed, or `None` when it is empty.
    ///
    /// **The one place the status line is cleaned**, so `vast ls`, `vast watch`, the boot
    /// watchdog and both GUIs render the same words.
    pub fn status_note(&self) -> Option<String> {
        let msg = self.status_msg.as_deref()?;
        let flat = msg.split_whitespace().collect::<Vec<_>>().join(" ");
        if flat.is_empty() {
            None
        } else {
            Some(flat)
        }
    }

    /// Whether the status line reads like a host-side failure.
    ///
    /// A heuristic, deliberately conservative: it gates *saying more* (surfacing the line,
    /// counting towards a stuck-boot verdict), never *destroying anything*. The R1 lemon sat
    /// in `loading` forever while `status_msg` said `error creating task for container` —
    /// this predicate is what lets a watcher call that dead instead of pending.
    pub fn status_looks_fatal(&self) -> bool {
        let Some(note) = self.status_note() else {
            return false;
        };
        let lower = note.to_ascii_lowercase();
        ["error", "fail", "unable", "cannot", "no such", "denied"]
            .iter()
            .any(|marker| lower.contains(marker))
    }

    /// `exited | offline | unknown` are TERMINAL — they never reach running, and a watchdog
    /// that keeps polling one of them burns money for nothing.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.actual_status
                .as_deref()
                .map(|s| s.trim().to_ascii_lowercase())
                .as_deref(),
            Some("exited") | Some("offline") | Some("unknown")
        )
    }

    /// Where an internal container port surfaced, tolerating both documented shapes of the
    /// `ports` field.
    ///
    /// The Docker map form is `{"8000/tcp":[{"HostIp":"0.0.0.0","HostPort":"41234"}]}`; a
    /// `HostIp` of `0.0.0.0` or empty means "the instance's public address". The array form
    /// carries no mapping, so the internal port is returned unchanged.
    pub fn external_port(&self, internal: u16) -> Option<(String, u16)> {
        let public = self.public_ipaddr.clone();
        match &self.ports {
            serde_json::Value::Object(map) => {
                let key_tcp = format!("{internal}/tcp");
                let key_bare = internal.to_string();
                let entry = map.get(&key_tcp).or_else(|| map.get(&key_bare))?;
                let first = match entry {
                    serde_json::Value::Array(a) => a.first()?,
                    other => other,
                };
                let obj = first.as_object()?;
                let host_port = obj.get("HostPort").and_then(|v| match v {
                    serde_json::Value::String(s) => s.parse::<u16>().ok(),
                    serde_json::Value::Number(n) => n.as_u64().and_then(|n| u16::try_from(n).ok()),
                    _ => None,
                })?;
                let host_ip = obj.get("HostIp").and_then(|v| v.as_str()).unwrap_or("");
                let host = if host_ip.is_empty() || host_ip == "0.0.0.0" {
                    public?
                } else {
                    host_ip.to_owned()
                };
                Some((host, host_port))
            }
            serde_json::Value::Array(a) => {
                let listed = a
                    .iter()
                    .filter_map(|v| v.as_u64())
                    .any(|p| p == u64::from(internal));
                if listed {
                    Some((public?, internal))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Seconds since billing started. `None` when vast has not told us `start_date`.
    pub fn uptime_secs(&self) -> Option<f64> {
        let start = self.start_date?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs_f64();
        Some((now - start).max(0.0))
    }
}

/// One append-only row in `$STATE/ledger.jsonl`. **"Active" is a query, not a file.**
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LedgerRow {
    /// Monotonic sequence within the file.
    pub seq: u64,
    /// Unix seconds.
    pub at_unix: i64,
    /// `None` on a `Reserved` row written before the create call returned.
    #[serde(default)]
    pub instance_id: Option<InstanceId>,
    /// What happened.
    pub state: LedgerState,
    /// Which offer.
    #[serde(default)]
    pub offer_id: Option<u64>,
    /// Which search profile produced it.
    #[serde(default)]
    pub profile: Option<ProfileId>,
    /// GPU model.
    #[serde(default)]
    pub gpu: Option<String>,
    /// GPU count.
    #[serde(default)]
    pub num_gpus: Option<u32>,
    /// Dollars per hour.
    #[serde(default)]
    pub dph: Option<f64>,
    /// The ceiling the human approved, so an over-spend is provable after the fact.
    #[serde(default)]
    pub approved_max_dph: Option<f64>,
    /// `"cli"`, `"web_ui"`, `"mcp"`, …
    #[serde(default)]
    pub approval_source: Option<String>,
    /// Unix seconds, once destruction is verified.
    #[serde(default)]
    pub destroyed_at_unix: Option<i64>,
    /// Accrued cost, with its provenance.
    pub est_cost: CostEstimate,
    /// Free text.
    #[serde(default)]
    pub note: Option<String>,
}

/// The ledger's state machine.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerState {
    /// Written **before** the billing call. A SIGKILL skips `Drop`, which is why this row —
    /// not the `Drop` handler — is the real protection.
    Reserved,
    /// The create call returned an instance id.
    Confirmed,
    /// Observed running.
    Running,
    /// A destroy was issued.
    DestroyRequested,
    /// Destruction verified.
    Destroyed,
    /// A reservation that never got an id. Raises a Critical alert with a Destroy action.
    OrphanSuspect,
    /// Startup reconciliation matched this row against the live fleet.
    Reconciled,
    /// Stopped on purpose: GPUs released, disk held and **still billing** storage. A parked
    /// instance stays in `Ledger::active()` — parking is not forgetting.
    Parked,
}

/// A remembered verdict on a physical host.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FavoriteVerdict {
    /// Proven good: rent it again.
    Star,
    /// Burned: never auto-pick it, warn when named explicitly.
    Skull,
}

/// One row of `$STATE/favorites.json`.
///
/// Keyed on **`machine_id`**, never on an offer id: several hosts re-mint ask ids per
/// search snapshot (GARDEN-RUNS.md, R4), so an offer id names a listing, not a machine.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FavoriteHost {
    /// The physical machine.
    pub machine_id: u64,
    /// Star or skull.
    pub verdict: FavoriteVerdict,
    /// Why, in the operator's words (`"flawless through R1/R1b"`, `"containerd disk death"`).
    #[serde(default)]
    pub note: Option<String>,
    /// GPU model at the time of the verdict, for recognisability.
    #[serde(default)]
    pub gpu_name: Option<String>,
    /// Location at the time of the verdict.
    #[serde(default)]
    pub geolocation: Option<String>,
    /// Unix seconds.
    pub added_at_unix: i64,
}

/// What the surfaces send to rent a box.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RentRequest {
    /// Search profile, when renting `--auto`.
    #[serde(default)]
    pub profile: Option<ProfileId>,
    /// A specific offer, when the user picked one.
    #[serde(default)]
    pub offer_id: Option<u64>,
    /// The container contract.
    pub launch: ContainerLaunch,
    /// Without this, the call returns a cost preview and creates nothing.
    pub confirm: bool,
    /// The requested ceiling. Still subject to the daemon-side hard ceiling.
    pub max_usd_per_hour: f64,
    /// Bring up an `ssh -L` tunnel as soon as it is reachable.
    pub auto_tunnel: bool,
    /// Bind this alias to it once it is healthy.
    #[serde(default)]
    pub bind_alias: Option<Alias>,
}

/// A pending human confirmation, written to `$STATE/approvals/<id>.json` when
/// `require_human_confirm` is on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    /// The job the approval unblocks.
    pub id: JobId,
    /// What is being asked for, in words.
    pub what: String,
    /// The requested ceiling.
    pub max_usd_per_hour: f64,
    /// The estimated total, so a human sees the bill and not just the rate.
    pub est_total_usd: f64,
    /// Credit remaining, when we could read it.
    #[serde(default)]
    pub credit: Option<f64>,
    /// Unix seconds.
    pub requested_at_unix: i64,
    /// `"cli"`, `"mcp"`, `"web_ui"`, …
    pub source: String,
}

/// One sample of a rented box's download progress, from a 4-second `/proc/net/dev` delta.
#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DownloadHealth {
    /// Unix seconds.
    pub sampled_at_unix: i64,
    /// Bytes received on eth0 over the sample window.
    pub rx_bytes_4s: u64,
    /// Derived rate.
    pub mbps: f32,
    /// The verdict the UI banners on.
    pub verdict: StallVerdict,
}

/// Download verdict. `< 1000 bytes` over the window is stalled; `< 50 Mbps` is slow.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StallVerdict {
    /// Moving at a sensible rate.
    Active,
    /// Moving, but slowly enough to be worth flagging.
    Slow,
    /// Not moving. Offer the one-click restart.
    Stalled,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::money::{Money, PriceSource};

    fn offer_json() -> &'static str {
        r#"{
            "id": 12345,
            "ask_contract_id": 999,
            "machine_id": 42,
            "gpu_name": "RTX 3090",
            "num_gpus": 2,
            "gpu_ram": 24576,
            "gpu_total_ram": 49152,
            "dph_total": 0.3012,
            "geolocation": "Czechia, CZ",
            "reliability2": 0.9871,
            "a_field_vast_added_last_tuesday": 7
        }"#
    }

    #[test]
    fn offer_preserves_unknown_fields_and_pools_vram() {
        let o: Offer = serde_json::from_str(offer_json()).expect("offer must load");
        assert_eq!(o.pooled_vram_mb(), 49_152);
        assert_eq!(o.geo_code(), Some("CZ"));
        assert!(o.extra.contains_key("a_field_vast_added_last_tuesday"));

        let s = serde_json::to_string(&o).expect("ser");
        assert!(s.contains("a_field_vast_added_last_tuesday"));
        assert_eq!(serde_json::from_str::<Offer>(&s).expect("de"), o);

        // Pooled falls back to per-GPU x count when the API omits the total.
        let mut o2 = o.clone();
        o2.gpu_total_ram = 0;
        assert_eq!(o2.pooled_vram_mb(), 24_576 * 2);
    }

    #[test]
    fn instance_phase_and_terminality_follow_the_status_vocabulary() {
        let inst = |status: Option<&str>| VastInstance {
            id: InstanceId(1),
            actual_status: status.map(str::to_owned),
            intended_status: None,
            status_msg: None,
            machine_id: None,
            storage_cost: None,
            ssh_host: None,
            ssh_port: None,
            public_ipaddr: None,
            ports: serde_json::Value::Null,
            direct_port_start: None,
            direct_port_end: None,
            gpu_name: None,
            num_gpus: None,
            gpu_util: None,
            dph_total: None,
            geolocation: None,
            label: None,
            start_date: None,
            disk_util: None,
            disk_space: None,
            inet_down: None,
            extra: serde_json::Map::new(),
        };
        assert_eq!(inst(None).phase(), BootPhase::Reserved);
        assert_eq!(inst(Some("created")).phase(), BootPhase::Provisioning);
        assert_eq!(inst(Some("loading")).phase(), BootPhase::Pulling);
        assert_eq!(inst(Some("running")).phase(), BootPhase::Healthy);
        // Stopped is parked, never destroyed: the disk is held and still billing.
        assert_eq!(inst(Some("stopped")).phase(), BootPhase::Parked);
        assert_eq!(inst(Some("inactive")).phase(), BootPhase::Parked);
        assert!(matches!(
            inst(Some("exited")).phase(),
            BootPhase::Failed { .. }
        ));
        // An unrecognised status is never treated as failure.
        assert_eq!(
            inst(Some("some-new-status")).phase(),
            BootPhase::Provisioning
        );

        for terminal in ["exited", "offline", "unknown", " EXITED "] {
            assert!(inst(Some(terminal)).is_terminal(), "{terminal}");
        }
        for live in ["running", "loading", "created", "stopped"] {
            assert!(!inst(Some(live)).is_terminal(), "{live}");
        }
    }

    #[test]
    fn the_status_note_is_flattened_and_the_fatal_heuristic_reads_it() {
        let with_msg = |msg: Option<&str>| -> VastInstance {
            serde_json::from_value(serde_json::json!({
                "id": 1,
                "actual_status": "loading",
                "status_msg": msg,
            }))
            .expect("de")
        };
        assert_eq!(with_msg(None).status_note(), None);
        assert_eq!(with_msg(Some("  \n ")).status_note(), None);
        assert_eq!(
            with_msg(Some("Error response from daemon:\n  failed to create task")).status_note(),
            Some("Error response from daemon: failed to create task".to_owned())
        );

        // The R1 lemon's actual words must read as fatal.
        assert!(with_msg(Some(
            "error creating task for container: read /var/lib/containerd/io.containerd.meta.db: \
             input/output error"
        ))
        .status_looks_fatal());
        assert!(with_msg(Some("Unable to find image locally")).status_looks_fatal());
        // Routine progress must not.
        assert!(!with_msg(Some("Successfully pulled image")).status_looks_fatal());
        assert!(!with_msg(Some("Extracting layers 4/9")).status_looks_fatal());
        assert!(!with_msg(None).status_looks_fatal());
    }

    #[test]
    fn external_port_reads_both_documented_ports_shapes() {
        let base = r#"{
            "id": 1,
            "public_ipaddr": "203.0.113.7",
            "ports": {"8000/tcp": [{"HostIp": "0.0.0.0", "HostPort": "41234"}]}
        }"#;
        let i: VastInstance = serde_json::from_str(base).expect("de");
        assert_eq!(
            i.external_port(8000),
            Some(("203.0.113.7".to_owned(), 41_234))
        );
        assert_eq!(i.external_port(9999), None);

        let arr = r#"{"id": 1, "public_ipaddr": "203.0.113.7", "ports": [8000, 8080]}"#;
        let i: VastInstance = serde_json::from_str(arr).expect("de");
        assert_eq!(
            i.external_port(8000),
            Some(("203.0.113.7".to_owned(), 8_000))
        );
        assert_eq!(i.external_port(1234), None);

        let none = r#"{"id": 1}"#;
        let i: VastInstance = serde_json::from_str(none).expect("de");
        assert_eq!(i.external_port(8000), None);
        assert_eq!(i.uptime_secs(), None);
    }

    #[test]
    fn instance_round_trips_with_unknown_fields() {
        let raw = r#"{
            "id": 28675431,
            "actual_status": "running",
            "ssh_host": "ssh4.vast.ai",
            "ssh_port": 22,
            "ports": {"8000/tcp": [{"HostIp": "0.0.0.0", "HostPort": "41234"}]},
            "dph_total": 0.3012,
            "start_date": 1785412331.0,
            "brand_new_column": "kept"
        }"#;
        let i: VastInstance = serde_json::from_str(raw).expect("de");
        let s = serde_json::to_string(&i).expect("ser");
        assert!(s.contains("brand_new_column"));
        assert_eq!(serde_json::from_str::<VastInstance>(&s).expect("de"), i);
        assert!(i.uptime_secs().expect("start_date present") >= 0.0);
    }

    #[test]
    fn ledger_row_and_state_round_trip() {
        let row = LedgerRow {
            seq: 7,
            at_unix: 1_785_412_331,
            instance_id: None,
            state: LedgerState::Reserved,
            offer_id: Some(12_345),
            profile: Some(ProfileId::parse("rtx3090").expect("id")),
            gpu: Some("RTX 3090".into()),
            num_gpus: Some(2),
            dph: Some(0.3012),
            approved_max_dph: Some(0.40),
            approval_source: Some("cli".into()),
            destroyed_at_unix: None,
            est_cost: CostEstimate::Approximate {
                usd: Money::from_usd(0.30),
                source: PriceSource::VastOffer,
                assumption: "one hour minimum".into(),
            },
            note: None,
        };
        let s = serde_json::to_string(&row).expect("ser");
        assert_eq!(serde_json::from_str::<LedgerRow>(&s).expect("de"), row);

        for st in [
            LedgerState::Reserved,
            LedgerState::Confirmed,
            LedgerState::Running,
            LedgerState::DestroyRequested,
            LedgerState::Destroyed,
            LedgerState::OrphanSuspect,
            LedgerState::Reconciled,
        ] {
            let j = serde_json::to_string(&st).expect("ser");
            assert_eq!(serde_json::from_str::<LedgerState>(&j).expect("de"), st);
        }
        assert_eq!(
            serde_json::to_string(&LedgerState::OrphanSuspect).expect("ser"),
            "\"orphan_suspect\""
        );
    }

    #[test]
    fn account_has_no_api_key_field() {
        // The API echoes an api_key. We deliberately do not model it, so it can never be
        // serialised into a snapshot, a log line or a --json envelope.
        let raw = r#"{"id": 1, "credit": 7.73, "api_key": "SECRET-DO-NOT-KEEP"}"#;
        let a: VastAccount = serde_json::from_str(raw).expect("de");
        let s = serde_json::to_string(&a).expect("ser");
        assert!(!s.contains("api_key"), "{s}");
        assert!(!s.contains("SECRET"), "{s}");
        assert!((a.credit - 7.73).abs() < 1e-9);
    }

    #[test]
    fn query_result_and_money_request_types_round_trip() {
        let q = OfferQuery {
            gpu_names: vec!["RTX 3090".into()],
            num_gpus_min: 2,
            num_gpus_max: 4,
            max_dph: Some(0.40),
            min_reliability: Some(0.98),
            min_inet_down: Some(500.0),
            min_disk_gb: Some(80),
            min_cuda: Some(12.0),
            geo: GeoFilter::Eu,
            verified: Some(true),
            limit: 64,
            order: vec![("dph_total".into(), "asc".into())],
            extra: serde_json::Map::new(),
        };
        let s = serde_json::to_string(&q).expect("ser");
        assert_eq!(serde_json::from_str::<OfferQuery>(&s).expect("de"), q);

        let r = OfferSearchResult {
            offers: vec![serde_json::from_str(offer_json()).expect("offer")],
            relaxations: vec!["widened: geo dropped, reliability 0.99 -> 0.97".into()],
            queried_at_unix: 1_785_412_331,
            gpu_name_vocabulary: vec!["RTX 3090".into(), "H100 SXM".into()],
        };
        let s = serde_json::to_string(&r).expect("ser");
        assert_eq!(
            serde_json::from_str::<OfferSearchResult>(&s).expect("de"),
            r
        );

        let dh = DownloadHealth {
            sampled_at_unix: 1_785_412_331,
            rx_bytes_4s: 12,
            mbps: 0.0,
            verdict: StallVerdict::Stalled,
        };
        let s = serde_json::to_string(&dh).expect("ser");
        assert_eq!(serde_json::from_str::<DownloadHealth>(&s).expect("de"), dh);

        let ap = ApprovalRequest {
            id: JobId::new(),
            what: "rent 2x RTX 3090".into(),
            max_usd_per_hour: 0.40,
            est_total_usd: 0.40,
            credit: Some(7.73),
            requested_at_unix: 1_785_412_331,
            source: "mcp".into(),
        };
        let s = serde_json::to_string(&ap).expect("ser");
        assert_eq!(serde_json::from_str::<ApprovalRequest>(&s).expect("de"), ap);

        let rr = RentRequest {
            profile: Some(ProfileId::parse("rtx3090").expect("id")),
            offer_id: None,
            launch: ContainerLaunch {
                runtime: crate::catalog::ContainerRuntime::LlamaCpp,
                image: "ghcr.io/buckster123/vastai-gguf:prebuilt".into(),
                image_type: crate::catalog::ImageType::Prebuilt,
                disk_gb: 80,
                env: Default::default(),
                onstart: "bash /app/launch.sh > /var/log/launch.log 2>&1 &".into(),
                host: "127.0.0.1".into(),
                port: 8000,
                expose_public: false,
            },
            confirm: false,
            max_usd_per_hour: 0.40,
            auto_tunnel: true,
            bind_alias: Some(Alias::parse("big").expect("alias")),
        };
        let s = serde_json::to_string(&rr).expect("ser");
        assert_eq!(serde_json::from_str::<RentRequest>(&s).expect("de"), rr);
    }
}
