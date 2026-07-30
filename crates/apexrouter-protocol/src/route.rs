//! Routes — the product.
//!
//! An alias is a stable string a client puts in `"model"`; it points at an ordered chain of
//! live backends. Model aliasing is the feature that makes the whole thing worth running:
//! an agent hardcodes `OPENAI_BASE_URL` and `model: "auto"` once and never touches either
//! again while the thing behind it changes from an iGPU to a rented 2×H100 and back.

use crate::ids::{Alias, BackendId};
use crate::money::Money;
use serde::{Deserialize, Serialize};

/// One alias and everything that governs how it dispatches.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelRoute {
    /// The string a client puts in `"model"`.
    pub alias: Alias,
    /// Ordered. Order matters for every strategy except `RoundRobin`.
    #[serde(default)]
    pub targets: Vec<RouteTarget>,
    /// How to pick among healthy targets.
    pub strategy: Strategy,
    /// Candidates failing any filter are never dispatched to.
    pub filter: RouteFilter,
    /// Retry and failover policy.
    pub retry: RetryPolicy,
    /// Exactly one route in a table may be the default.
    pub is_default: bool,
    /// Free text for the UI.
    #[serde(default)]
    pub description: Option<String>,
}

/// One entry in a route's chain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteTarget {
    /// Which backend(s) this entry names.
    pub backend: BackendSelector,
    /// The upstream model id to send. `None` uses the backend's own advertised id.
    #[serde(default)]
    pub model: Option<String>,
    /// Used by `RoundRobin`.
    pub weight: u32,
}

/// How a target names backends.
///
/// Serde note: the contract spells this `#[serde(tag = "sel")]`, but an internally tagged
/// enum cannot carry a bare string newtype payload. It is adjacently tagged with
/// `content = "value"`, so a selector is `{"sel":"id","value":"local-carnice"}` /
/// `{"sel":"tag","value":"local"}` / `{"sel":"glob","value":"vast-*"}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "sel", rename_all = "snake_case", content = "value")]
pub enum BackendSelector {
    /// Exactly this backend.
    Id(BackendId),
    /// Every backend carrying this tag.
    Tag(String),
    /// Every backend whose id matches this glob, e.g. `"vast-*"`.
    Glob(String),
}

/// mk1 ships EXACTLY the strategies it implements — no config value can reach a `todo!()`.
///
/// `Mirror`, `Fastest` and sticky sessions are deliberately absent from the enum, not
/// merely unimplemented. Batch comparison ships as `POST /v1/compare` instead.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    /// First healthy target in order. The default, and the one that never surprises anybody.
    FirstHealthy,
    /// Weighted round robin.
    RoundRobin,
    /// Fewest in-flight requests, by the router's own counter.
    LeastBusy,
    /// Lowest `$/Mtok`. **Rejected at compile time** when no target has a price model and no
    /// throughput hint — the ordering would be an invented number.
    Cheapest,
}

/// Hard requirements a candidate must satisfy.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteFilter {
    /// Every tag here must be present.
    #[serde(default)]
    pub require_tags: Vec<String>,
    /// Any tag here disqualifies.
    #[serde(default)]
    pub exclude_tags: Vec<String>,
    /// Ceiling on the blended `$/Mtok`.
    #[serde(default)]
    pub max_cost_per_mtok: Option<Money>,
    /// Minimum advertised context.
    #[serde(default)]
    pub min_ctx: Option<u32>,
    /// Only vision-capable models.
    pub require_vision: bool,
    /// Only tool-calling models.
    pub require_tools: bool,
}

/// Retry and failover.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// TOTAL attempts, including the first. There is never a retry after the first
    /// upstream byte — that invariant is expressed as a type, not a comment.
    pub attempts: u8,
    /// May a retry go to a *different* backend?
    pub failover: bool,
    /// Respect an upstream `Retry-After` before trying again.
    pub honor_retry_after: bool,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            attempts: 2,
            failover: true,
            honor_retry_after: true,
        }
    }
}

/// Why the resolver chose what it chose. Emitted on **every** response as the second half of
/// `X-ApexRouter-Route: <alias-or-"-">|<reason>`, so routing is observable rather than
/// inferred.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteReason {
    /// Rule 1: the model string is an alias.
    Alias,
    /// Rule 2: `"<backend_id>/<upstream_model>"`.
    ExplicitPin,
    /// Rule 3: it exactly matches one enabled backend's advertised model id.
    UpstreamIdMatch,
    /// Rule 4: the same id on several backends; `[router] implicit_strategy` decided.
    ImplicitMulti,
    /// Rule 6 under `unknown_model = "fallback"`.
    DefaultFallback,
    /// Rule 5: `""`, `"x"`, `"auto"`, `"default"` or absent.
    LegacyModelName,
}

impl RouteReason {
    /// The token that goes in the `X-ApexRouter-Route` header.
    pub fn as_str(&self) -> &'static str {
        match self {
            RouteReason::Alias => "alias",
            RouteReason::ExplicitPin => "explicit_pin",
            RouteReason::UpstreamIdMatch => "upstream_id_match",
            RouteReason::ImplicitMulti => "implicit_multi",
            RouteReason::DefaultFallback => "default_fallback",
            RouteReason::LegacyModelName => "legacy_model_name",
        }
    }
}

/// How a swap is performed. Chosen by `fit()`, overridable.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwapMode {
    /// Start B alongside A, repoint, drain A, stop A. Zero downtime; needs the VRAM.
    Hot,
    /// Drain and stop A, start B, park arriving requests on a `Notify`.
    Sequential,
}

/// The on-disk routing table: `$STATE/routes.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RouteFile {
    /// Bumped when the shape changes. Currently `1`.
    pub schema_version: u32,
    /// Where rule 5 and `unknown_model = "fallback"` land.
    pub default_alias: Alias,
    /// Every route.
    #[serde(default)]
    pub routes: Vec<ModelRoute>,
}

/// The result of validating a table, a recipe or a profile.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationReport {
    /// True when nothing at `Severity::Error` was found.
    pub ok: bool,
    /// Everything found, in the order found.
    #[serde(default)]
    pub issues: Vec<ValidationIssue>,
}

/// One problem, with the fix spelled out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationIssue {
    /// Dotted path to the offending field.
    pub field: String,
    /// How bad it is.
    pub severity: Severity,
    /// What is wrong, in words.
    pub message: String,
    /// What to do about it. Rendered as an actionable line, never as prose.
    #[serde(default)]
    pub fix: Option<String>,
}

/// Issue severity. Staleness is a `Warning`, never an `Error` — a recipe pointing at a model
/// you deleted is normal, not corruption.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Worth knowing.
    Info,
    /// Worth fixing.
    Warning,
    /// Blocks the operation.
    Error,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route() -> ModelRoute {
        ModelRoute {
            alias: Alias::parse("auto").expect("alias"),
            targets: vec![
                RouteTarget {
                    backend: BackendSelector::Id(BackendId::parse("local-carnice").expect("id")),
                    model: Some("Carnice-9b-Q6_K".into()),
                    weight: 1,
                },
                RouteTarget {
                    backend: BackendSelector::Glob("vast-*".into()),
                    model: None,
                    weight: 2,
                },
            ],
            strategy: Strategy::FirstHealthy,
            filter: RouteFilter {
                require_tags: vec!["tools".into()],
                exclude_tags: vec!["experimental".into()],
                max_cost_per_mtok: Some(Money::from_usd(1.5)),
                min_ctx: Some(8_192),
                require_vision: false,
                require_tools: true,
            },
            retry: RetryPolicy::default(),
            is_default: true,
            description: Some("whatever is healthy".into()),
        }
    }

    #[test]
    fn backend_selector_round_trips_every_variant() {
        for sel in [
            BackendSelector::Id(BackendId::parse("local-carnice").expect("id")),
            BackendSelector::Tag("local".into()),
            BackendSelector::Glob("vast-*".into()),
        ] {
            let s = serde_json::to_string(&sel).expect("ser");
            assert_eq!(
                serde_json::from_str::<BackendSelector>(&s).expect("de"),
                sel
            );
        }
        assert_eq!(
            serde_json::to_string(&BackendSelector::Tag("local".into())).expect("ser"),
            r#"{"sel":"tag","value":"local"}"#
        );
    }

    #[test]
    fn route_file_round_trips() {
        let f = RouteFile {
            schema_version: 1,
            default_alias: Alias::parse("auto").expect("alias"),
            routes: vec![route()],
        };
        let s = serde_json::to_string(&f).expect("ser");
        assert_eq!(serde_json::from_str::<RouteFile>(&s).expect("de"), f);
    }

    #[test]
    fn retry_policy_default_is_the_documented_one() {
        let d = RetryPolicy::default();
        assert_eq!(d.attempts, 2);
        assert!(d.failover);
        assert!(d.honor_retry_after);
    }

    #[test]
    fn route_reason_tokens_are_stable_header_values() {
        let pairs = [
            (RouteReason::Alias, "alias"),
            (RouteReason::ExplicitPin, "explicit_pin"),
            (RouteReason::UpstreamIdMatch, "upstream_id_match"),
            (RouteReason::ImplicitMulti, "implicit_multi"),
            (RouteReason::DefaultFallback, "default_fallback"),
            (RouteReason::LegacyModelName, "legacy_model_name"),
        ];
        for (r, token) in pairs {
            assert_eq!(r.as_str(), token);
            // The header token and the wire spelling must not drift apart.
            assert_eq!(
                serde_json::to_string(&r).expect("ser"),
                format!("\"{token}\"")
            );
            assert_eq!(
                serde_json::from_str::<RouteReason>(&format!("\"{token}\"")).expect("de"),
                r
            );
        }
    }

    #[test]
    fn strategy_and_swap_mode_and_severity_round_trip() {
        for s in [
            Strategy::FirstHealthy,
            Strategy::RoundRobin,
            Strategy::LeastBusy,
            Strategy::Cheapest,
        ] {
            let j = serde_json::to_string(&s).expect("ser");
            assert_eq!(serde_json::from_str::<Strategy>(&j).expect("de"), s);
        }
        assert_eq!(
            serde_json::to_string(&Strategy::FirstHealthy).expect("ser"),
            "\"first_healthy\""
        );
        for m in [SwapMode::Hot, SwapMode::Sequential] {
            let j = serde_json::to_string(&m).expect("ser");
            assert_eq!(serde_json::from_str::<SwapMode>(&j).expect("de"), m);
        }
        for sev in [Severity::Info, Severity::Warning, Severity::Error] {
            let j = serde_json::to_string(&sev).expect("ser");
            assert_eq!(serde_json::from_str::<Severity>(&j).expect("de"), sev);
        }
    }

    #[test]
    fn validation_report_round_trips() {
        let r = ValidationReport {
            ok: false,
            issues: vec![ValidationIssue {
                field: "routes[0].targets[1].backend".into(),
                severity: Severity::Error,
                message: "no backend matches glob \"vast-*\"".into(),
                fix: Some("rent one, or drop the target".into()),
            }],
        };
        let s = serde_json::to_string(&r).expect("ser");
        assert_eq!(serde_json::from_str::<ValidationReport>(&s).expect("de"), r);
    }
}
