//! A **Backend** is a live upstream in the routing table — OpenAI-compatible unless it
//! declares otherwise. Every endpoint produces a backend; not every backend has an endpoint
//! (a LAN node or a managed provider has no lifecycle).
//!
//! `Health` is **computed on read**, never persisted (invariant 3). It appears here because
//! it is part of the wire shape every surface renders, not because it is a fact on disk.

use crate::endpoint::EndpointRef;
use crate::ids::BackendId;
use crate::money::{CostEstimate, Money, PriceSource};
use serde::{Deserialize, Serialize};

/// The wire dialect a listener accepts or an upstream speaks. `ARCHITECTURE.md` §3.4 is the
/// matrix; this enum is one axis of it.
///
/// It appears in exactly two places on the request path: the ingress records which dialect
/// the *client* spoke, and the resolved [`Backend`] declares which dialect the *upstream*
/// speaks. The pair names the cell.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    /// OpenAI-compatible. The canonical surface.
    OpenAi,
    /// Anthropic Messages. Ingress is translated; `OpenAi -> Anthropic` is permanently a 501.
    Anthropic,
}

// Written out rather than derived because `ARCHITECTURE.md` §3.4 and `BUILD-PLAN.md` §3.6
// both quote this impl verbatim, and a later agent diffing the contract against the source
// should find exactly what the contract says. The derive would be byte-identical in effect.
#[allow(clippy::derivable_impls)]
impl Default for Protocol {
    fn default() -> Self {
        Protocol::OpenAi
    }
}

impl Protocol {
    /// The wire spelling, which is also what `X-ApexRouter-Protocol` carries:
    /// `"open_ai"` or `"anthropic"`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Protocol::OpenAi => "open_ai",
            Protocol::Anthropic => "anthropic",
        }
    }
}

/// What kind of thing is behind this backend. Drives which provisioner owns its lifecycle.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    /// A locally supervised `llama-server`.
    LocalLlama,
    /// A locally supervised vLLM.
    LocalVllm,
    /// `llama-server` on a rented vast.ai box.
    VastLlama,
    /// vLLM on a rented vast.ai box.
    VastVllm,
    /// A managed provider (together.ai and friends).
    Managed,
    /// A plain OpenAI-compatible URL somebody registered. No lifecycle.
    Node,
}

/// One live upstream. The serialisable description; the router holds live state beside it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Backend {
    /// Stable id, used in routes, logs and `X-ApexRouter-Backend`.
    pub id: BackendId,
    /// What kind of thing it is.
    pub kind: BackendKind,
    /// The dialect this upstream speaks. `OpenAi` unless the record says otherwise.
    #[serde(default)]
    pub protocol: Protocol,
    /// Human label.
    pub label: String,
    /// INVARIANT: stored **without** a trailing `/v1`. The relay joins segments itself.
    pub base_url: String,
    /// A *description* of where the credential lives. Never key material.
    pub credential: CredentialSource,
    /// Free-form: `"local"`, `"tools"`, `"vision"`, `"cheap"`, `"gpu:vulkan"`, `"rented"`.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Models this upstream advertises. Maintained by the health prober.
    #[serde(default)]
    pub models: Vec<UpstreamModel>,
    /// Concurrency and context limits.
    pub limits: BackendLimits,
    /// Pricing, when we know any.
    #[serde(default)]
    pub price: Option<PriceModel>,
    /// Computed on read. Never persisted.
    pub health: Health,
    /// How this backend came to exist.
    pub provenance: Provenance,
    /// `Some(..)` when ApexRouter can start and stop it.
    #[serde(default)]
    pub endpoint: Option<EndpointRef>,
    /// A disabled backend is never a routing candidate.
    pub enabled: bool,
    /// Device tokens this backend occupies, for the rig strip's `held_by`.
    #[serde(default)]
    pub devices: Vec<String>,
    /// The last error observed, for the card's footer.
    #[serde(default)]
    pub last_error: Option<String>,
}

/// One model id an upstream advertises, with the capabilities we could determine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpstreamModel {
    /// The exact id to put in an outbound `"model"`.
    pub id: String,
    /// Context length, when the upstream tells us.
    #[serde(default)]
    pub ctx: Option<u32>,
    /// Accepts image content blocks.
    pub vision: bool,
    /// Accepts `tools` / emits `tool_calls`.
    pub tools: bool,
}

/// Concurrency and size limits for one backend.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendLimits {
    /// Semaphore size. Sized from `/props.total_slots`, then `/slots`, then config.
    pub max_concurrent: u32,
    /// How many requests may wait for a permit before we 503.
    pub queue_depth: u32,
    /// Context length, when known.
    #[serde(default)]
    pub ctx: Option<u32>,
    /// Slot count as the upstream reports it.
    #[serde(default)]
    pub slots_total: Option<u32>,
}

/// How an upstream charges.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PriceModel {
    /// Per 1M tokens.
    PerToken {
        /// Input price per 1M tokens.
        input: Money,
        /// Output price per 1M tokens.
        output: Money,
    },
    /// Per hour of wall clock, e.g. a rented GPU.
    PerHour {
        /// Dollars per hour.
        dph: Money,
    },
    /// Free (a local model, or a provider tier that costs nothing).
    Free,
}

impl PriceModel {
    /// Normalise to a single `$/Mtok` figure.
    ///
    /// Both non-trivial cases need an assumption, and the assumption is **returned with the
    /// number** so the UI can label it — never buried like `cost.py`'s hardcoded 100 tok/s.
    /// `PerHour` without a `tps_hint` is [`CostEstimate::Unknown`], which is exactly what
    /// makes `Strategy::Cheapest` rejectable at compile time instead of silently inventing
    /// an ordering.
    pub fn per_mtok(&self, tps_hint: Option<f32>) -> CostEstimate {
        match self {
            PriceModel::Free => CostEstimate::Metered {
                usd: Money::ZERO,
                source: PriceSource::ConfigTable,
            },
            PriceModel::PerToken { input, output } => CostEstimate::Approximate {
                usd: Money((input.0.saturating_add(output.0)) / 2),
                source: PriceSource::ProviderApi,
                assumption: format!(
                    "blended $/Mtok = (input {input} + output {output}) / 2, a 50/50 token mix"
                ),
            },
            PriceModel::PerHour { dph } => match tps_hint {
                Some(tps) if tps > 0.0 => {
                    let tokens_per_hour = f64::from(tps) * 3600.0;
                    let mtok_per_hour = tokens_per_hour / 1_000_000.0;
                    CostEstimate::Approximate {
                        usd: dph.mul_f64(1.0 / mtok_per_hour),
                        source: PriceSource::VastOffer,
                        assumption: format!("{dph}/hr at {tps:.1} tok/s"),
                    }
                }
                _ => CostEstimate::Unknown,
            },
        }
    }
}

/// Computed liveness. **Never** written to disk — invariant 3.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Health {
    /// Not probed yet.
    Unknown,
    /// Coming up. `phase` drives the boot view in both GUIs.
    Starting {
        /// Where in the boot machine it is.
        phase: BootPhase,
        /// When this phase began, unix seconds.
        since_unix: i64,
        /// A log line or status message worth showing.
        #[serde(default)]
        detail: Option<String>,
    },
    /// Serving. The only routable state.
    Ready {
        /// When it became ready, unix seconds.
        since_unix: i64,
        /// Busy slots, as last probed.
        slots_busy: u32,
        /// Total slots.
        slots_total: u32,
        /// Median observed throughput.
        #[serde(default)]
        tps_p50: Option<f32>,
    },
    /// Answering, but failing often enough to be demoted.
    Degraded {
        /// Why.
        reason: String,
        /// How many consecutive probes failed.
        consecutive_failures: u32,
    },
    /// Not answering.
    Down {
        /// Why.
        reason: String,
        /// When the breaker will admit a probe, unix seconds.
        retry_at_unix: i64,
    },
    /// Finishing in-flight work, accepting nothing new.
    Draining {
        /// Requests still running against it.
        in_flight: u32,
    },
}

impl Health {
    /// Only `Ready` may be dispatched to. Draining deliberately is not.
    pub fn is_routable(&self) -> bool {
        matches!(self, Health::Ready { .. })
    }
}

/// The boot state machine, shared by a local spawn and a rented box.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum BootPhase {
    /// A ledger row exists; nothing has been billed yet.
    Reserved,
    /// The provider is allocating.
    Provisioning,
    /// Pulling a container image.
    Pulling,
    /// Building llama.cpp from a fork (`known_forks` forces this; +12–18 min).
    Compiling,
    /// Fetching weights.
    Downloading {
        /// Percent complete, when derivable.
        #[serde(default)]
        pct: Option<f32>,
        /// Observed download rate.
        #[serde(default)]
        mbps: Option<f32>,
    },
    /// Loading weights into VRAM.
    Loading {
        /// Percent complete, when derivable.
        #[serde(default)]
        pct: Option<f32>,
    },
    /// Serving.
    Healthy,
    /// Stopped on purpose: the GPUs are released, the disk is held and still billing, and a
    /// wake can bring it back. **Not** `Destroyed` — the box exists and costs money.
    Parked,
    /// Gave up. Carries the reason, and the caller carries the log tail.
    Failed {
        /// Why.
        reason: String,
    },
    /// Torn down.
    Destroyed,
}

impl BootPhase {
    /// True when the boot machine will make no further transitions, however it ended.
    ///
    /// `Parked` is terminal: a parked box makes no progress until an explicit wake, and
    /// anything waiting on its boot should stop waiting.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            BootPhase::Healthy
                | BootPhase::Parked
                | BootPhase::Failed { .. }
                | BootPhase::Destroyed
        )
    }
}

/// How a backend came to exist. Shown on every card, because "who made this" is the first
/// question when something unexpected is holding a GPU.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// Found by a scan.
    Discovered,
    /// We spawned it.
    Spawned,
    /// We rented it.
    Rented,
    /// A human registered it.
    Manual,
    /// It was already running and we verified its identity.
    Adopted,
    /// It came from a legacy state file during migration.
    Imported,
}

/// **A description of where a credential lives.** Never key material — that is
/// `apexrouter_core::secret::Secret`, and it never crosses this crate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CredentialSource {
    /// No credential needed.
    None,
    /// An environment variable, by name.
    Env {
        /// The variable name.
        var: String,
    },
    /// A file on disk, by path.
    File {
        /// The path.
        path: String,
    },
    /// Our own `credentials.toml`, or another named store.
    Managed {
        /// The store name.
        store: String,
    },
    /// Minted per instance, e.g. an exposed rented box's `llama-server --api-key-file`.
    Instance,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::EndpointRef;

    fn sample() -> Backend {
        Backend {
            id: BackendId::parse("local-carnice").expect("id"),
            kind: BackendKind::LocalLlama,
            protocol: Protocol::OpenAi,
            label: "Carnice 9B (Vulkan)".into(),
            base_url: "http://127.0.0.1:8100".into(),
            credential: CredentialSource::None,
            tags: vec!["local".into(), "gpu:vulkan".into()],
            models: vec![UpstreamModel {
                id: "Carnice-9b-Q6_K".into(),
                ctx: Some(32_768),
                vision: false,
                tools: true,
            }],
            limits: BackendLimits {
                max_concurrent: 4,
                queue_depth: 32,
                ctx: Some(32_768),
                slots_total: Some(4),
            },
            price: Some(PriceModel::Free),
            health: Health::Ready {
                since_unix: 1_785_412_331,
                slots_busy: 1,
                slots_total: 4,
                tps_p50: Some(4.1),
            },
            provenance: Provenance::Spawned,
            endpoint: Some(EndpointRef {
                id: BackendId::parse("local-carnice").expect("id"),
                kind: BackendKind::LocalLlama,
            }),
            enabled: true,
            devices: vec!["Vulkan0".into()],
            last_error: None,
        }
    }

    #[test]
    fn backend_round_trips() {
        let b = sample();
        let s = serde_json::to_string(&b).expect("ser");
        assert_eq!(serde_json::from_str::<Backend>(&s).expect("de"), b);
    }

    #[test]
    fn protocol_defaults_to_openai_and_spells_itself_for_the_header() {
        #[derive(Deserialize)]
        struct Probe {
            #[serde(default)]
            protocol: Protocol,
        }
        let p: Probe = serde_json::from_str("{}").expect("de");
        assert_eq!(p.protocol, Protocol::OpenAi);
        assert_eq!(Protocol::OpenAi.as_str(), "open_ai");
        assert_eq!(Protocol::Anthropic.as_str(), "anthropic");
        assert_eq!(
            serde_json::to_string(&Protocol::OpenAi).expect("ser"),
            "\"open_ai\""
        );
    }

    #[test]
    fn health_round_trips_every_variant_and_only_ready_routes() {
        let cases = [
            Health::Unknown,
            Health::Starting {
                phase: BootPhase::Loading { pct: Some(42.0) },
                since_unix: 1,
                detail: Some("load_tensors".into()),
            },
            Health::Ready {
                since_unix: 2,
                slots_busy: 0,
                slots_total: 4,
                tps_p50: None,
            },
            Health::Degraded {
                reason: "timeouts".into(),
                consecutive_failures: 3,
            },
            Health::Down {
                reason: "connection refused".into(),
                retry_at_unix: 9,
            },
            Health::Draining { in_flight: 2 },
        ];
        for h in &cases {
            let s = serde_json::to_string(h).expect("ser");
            assert_eq!(&serde_json::from_str::<Health>(&s).expect("de"), h);
        }
        assert_eq!(cases.iter().filter(|h| h.is_routable()).count(), 1);
    }

    #[test]
    fn boot_phase_round_trips_and_knows_its_terminals() {
        let cases = [
            BootPhase::Reserved,
            BootPhase::Provisioning,
            BootPhase::Pulling,
            BootPhase::Compiling,
            BootPhase::Downloading {
                pct: Some(12.5),
                mbps: Some(88.0),
            },
            BootPhase::Loading { pct: None },
            BootPhase::Healthy,
            BootPhase::Failed {
                reason: "exited".into(),
            },
            BootPhase::Destroyed,
        ];
        for p in &cases {
            let s = serde_json::to_string(p).expect("ser");
            assert_eq!(&serde_json::from_str::<BootPhase>(&s).expect("de"), p);
        }
        assert!(!BootPhase::Downloading {
            pct: None,
            mbps: None
        }
        .is_terminal());
        assert!(BootPhase::Healthy.is_terminal());
        assert!(BootPhase::Failed { reason: "x".into() }.is_terminal());
        assert!(BootPhase::Destroyed.is_terminal());
    }

    #[test]
    fn credential_source_round_trips_and_carries_no_key_material() {
        for c in [
            CredentialSource::None,
            CredentialSource::Env {
                var: "TOGETHER_API_KEY".into(),
            },
            CredentialSource::File {
                path: "~/.config/vastai/vast_api_key".into(),
            },
            CredentialSource::Managed {
                store: "credentials.toml".into(),
            },
            CredentialSource::Instance,
        ] {
            let s = serde_json::to_string(&c).expect("ser");
            assert_eq!(serde_json::from_str::<CredentialSource>(&s).expect("de"), c);
        }
        assert_eq!(
            serde_json::to_string(&CredentialSource::Env {
                var: "TOGETHER_API_KEY".into()
            })
            .expect("ser"),
            r#"{"kind":"env","var":"TOGETHER_API_KEY"}"#
        );
    }

    #[test]
    fn price_model_per_mtok_always_states_its_assumption() {
        assert_eq!(
            PriceModel::Free.per_mtok(None),
            CostEstimate::Metered {
                usd: Money::ZERO,
                source: PriceSource::ConfigTable
            }
        );

        // A per-hour box with no throughput hint is Unknown, never an invented ordering.
        assert_eq!(
            PriceModel::PerHour {
                dph: Money::from_usd(3.34)
            }
            .per_mtok(None),
            CostEstimate::Unknown
        );

        let hourly = PriceModel::PerHour {
            dph: Money::from_usd(3.34),
        }
        .per_mtok(Some(100.0));
        match hourly {
            CostEstimate::Approximate {
                ref assumption,
                source,
                ..
            } => {
                assert_eq!(source, PriceSource::VastOffer);
                assert!(assumption.contains("100.0 tok/s"), "{assumption}");
            }
            other => panic!("expected Approximate, got {other:?}"),
        }

        let per_token = PriceModel::PerToken {
            input: Money::from_usd(0.20),
            output: Money::from_usd(0.60),
        }
        .per_mtok(None);
        assert!(per_token.is_guess());
        assert_eq!(per_token.usd(), Some(Money::from_usd(0.40)));
    }

    #[test]
    fn price_model_round_trips() {
        for p in [
            PriceModel::PerToken {
                input: Money(200_000),
                output: Money(600_000),
            },
            PriceModel::PerHour {
                dph: Money(340_000),
            },
            PriceModel::Free,
        ] {
            let s = serde_json::to_string(&p).expect("ser");
            assert_eq!(serde_json::from_str::<PriceModel>(&s).expect("de"), p);
        }
    }
}
