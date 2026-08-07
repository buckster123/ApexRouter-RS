//! OWNER: unit C-08 (core/ledger.rs, core/money.rs). Do not edit outside that unit.
//!
//! [`SpendApproval`] — **a value you cannot fabricate.**
//!
//! It lives here rather than in `apexrouter-protocol` precisely so no surface can build one
//! with a struct literal: the fields are private, the type is `#[non_exhaustive]`, and
//! [`SpendApproval::confirm`] is the only constructor. There is no function signature in
//! `providers::vast` that reaches a billing call without one of these as a parameter.

use crate::config::VastCfg;
use apexrouter_protocol::{JobId, Money};

/// Proof that a human (or a configured policy) authorised spending, up to a ceiling.
///
/// ```compile_fail
/// # use apexrouter_core::money::{SpendApproval, ApprovalSource};
/// # use apexrouter_protocol::Money;
/// // The fields are private and the type is #[non_exhaustive]: there is no literal path.
/// let forged = SpendApproval { max_usd_per_hour: Money(999_000_000), confirmed_at: 0,
///                              source: ApprovalSource::Api };
/// ```
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct SpendApproval {
    max_usd_per_hour: Money,
    confirmed_at: i64,
    source: ApprovalSource,
}

/// Which surface asked. `Mcp` carries whether a human has cleared it, because an agent
/// filling in a big number is exactly the case the ceiling exists for.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ApprovalSource {
    /// `apexrouter vast rent --yes`.
    Cli,
    /// The embedded web UI.
    WebUi,
    /// The Slint app.
    SlintUi,
    /// An MCP tool call.
    Mcp {
        /// Whether a human has already granted the pending approval.
        human_cleared: bool,
    },
    /// A direct REST call.
    Api,
}

/// Why an approval was refused. Each variant is renderable as an actionable message.
#[derive(Debug, thiserror::Error)]
pub enum ApprovalError {
    /// The request exceeded `[providers.vast] max_usd_per_hour_ceiling`.
    #[error("requested {requested}/hr exceeds the configured ceiling of {ceiling}/hr")]
    AboveCeiling {
        /// What was asked for.
        requested: Money,
        /// The hard daemon-side cap.
        ceiling: Money,
    },
    /// `require_human_confirm` is on and the source was an agent.
    #[error("a human must grant approval {pending}: run `apexrouter approvals grant {pending}`")]
    HumanConfirmationRequired {
        /// The approval to grant.
        pending: JobId,
    },
    /// There is not enough credit to pay for it.
    #[error("credit is {credit} but this needs about {needed}")]
    InsufficientCredit {
        /// Credit remaining.
        credit: Money,
        /// What the operation needs.
        needed: Money,
    },
}

impl SpendApproval {
    /// The ONLY constructor. Enforces `cfg.max_usd_per_hour_ceiling` and
    /// `cfg.require_human_confirm`.
    ///
    /// The checks run in this order, cheapest and most absolute first:
    ///
    /// 1. **The ceiling.** `requested > cfg.max_usd_per_hour_ceiling` is
    ///    [`ApprovalError::AboveCeiling`]. This is the daemon-side hard cap: an agent that
    ///    fills in a big number still cannot spend more than the human configured. A
    ///    negative rate is meaningless and is normalised to [`Money::ZERO`] first, so it can
    ///    never sneak past a comparison.
    /// 2. **Human confirmation.** With `cfg.require_human_confirm`, only a surface that is
    ///    already a human — [`ApprovalSource::Cli`], [`ApprovalSource::WebUi`],
    ///    [`ApprovalSource::SlintUi`] — or an MCP call a human has already cleared
    ///    ([`ApprovalSource::Mcp`] with `human_cleared: true`) passes. An uncleared agent
    ///    **and a bare API call** (the path an agent takes when it omits `?source=mcp`) get
    ///    [`ApprovalError::HumanConfirmationRequired`] carrying a fresh [`JobId`] — the id
    ///    the human clears with `apexrouter approvals grant <id>`. Gating only MCP was how
    ///    an agent skipped the flag by posting to the same HTTP route without the query.
    /// 3. **Credit.** When the caller could read the account's credit, one hour at the
    ///    approved rate must fit inside it, otherwise
    ///    [`ApprovalError::InsufficientCredit`]. `credit: None` means "we could not ask" —
    ///    being offline never invents a refusal.
    pub fn confirm(
        requested: Money,
        source: ApprovalSource,
        cfg: &VastCfg,
        credit: Option<f64>,
    ) -> std::result::Result<SpendApproval, ApprovalError> {
        let requested = if requested < Money::ZERO {
            Money::ZERO
        } else {
            requested
        };

        let ceiling = Money::from_usd(cfg.max_usd_per_hour_ceiling);
        if requested > ceiling {
            return Err(ApprovalError::AboveCeiling { requested, ceiling });
        }

        if cfg.require_human_confirm && !source_is_human_cleared(source) {
            return Err(ApprovalError::HumanConfirmationRequired {
                pending: JobId::new(),
            });
        }

        if let Some(credit) = credit {
            let credit = Money::from_usd(credit);
            if credit < requested {
                return Err(ApprovalError::InsufficientCredit {
                    credit,
                    needed: requested,
                });
            }
        }

        Ok(SpendApproval {
            max_usd_per_hour: requested,
            confirmed_at: chrono::Utc::now().timestamp(),
            source,
        })
    }

    /// The approved ceiling.
    pub fn max_usd_per_hour(&self) -> Money {
        self.max_usd_per_hour
    }

    /// When it was granted, unix seconds.
    pub fn confirmed_at(&self) -> i64 {
        self.confirmed_at
    }

    /// Which surface granted it. Recorded on the ledger row.
    pub fn source(&self) -> ApprovalSource {
        self.source
    }
}

/// Surfaces that may spend when `[providers.vast] require_human_confirm` is on.
///
/// A human on the CLI / web UI / Slint app, or an MCP call that already carries a granted
/// approval id. Bare [`ApprovalSource::Api`] and uncleared MCP do **not** pass — that is
/// what stops an agent from omitting `?source=mcp` and walking through the same HTTP route.
fn source_is_human_cleared(source: ApprovalSource) -> bool {
    matches!(
        source,
        ApprovalSource::Cli
            | ApprovalSource::WebUi
            | ApprovalSource::SlintUi
            | ApprovalSource::Mcp {
                human_cleared: true
            }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped default is a $4.00/hr ceiling with no human gate.
    fn cfg() -> VastCfg {
        VastCfg::default()
    }

    #[test]
    fn a_four_dollar_ceiling_rejects_a_nine_dollar_request() {
        let err = SpendApproval::confirm(Money::from_usd(9.00), ApprovalSource::Cli, &cfg(), None)
            .expect_err("a $9.00 request must not clear a $4.00 ceiling");
        match err {
            ApprovalError::AboveCeiling { requested, ceiling } => {
                assert_eq!(requested, Money::from_usd(9.00));
                assert_eq!(ceiling, Money::from_usd(4.00));
            }
            other => panic!("expected AboveCeiling, got {other:?}"),
        }
    }

    #[test]
    fn a_request_at_or_under_the_ceiling_is_approved() {
        for usd in [0.0, 0.34, 3.99, 4.00] {
            let a = SpendApproval::confirm(Money::from_usd(usd), ApprovalSource::Cli, &cfg(), None)
                .expect("under the ceiling");
            assert_eq!(a.max_usd_per_hour(), Money::from_usd(usd));
            assert_eq!(a.source(), ApprovalSource::Cli);
            assert!(a.confirmed_at() > 1_700_000_000, "{}", a.confirmed_at());
        }
    }

    #[test]
    fn a_negative_rate_is_normalised_to_zero_rather_than_trusted() {
        let a = SpendApproval::confirm(Money(-9_000_000), ApprovalSource::Api, &cfg(), None)
            .expect("a negative rate is not above any sane ceiling");
        assert_eq!(a.max_usd_per_hour(), Money::ZERO);
    }

    #[test]
    fn mcp_without_human_clearance_requires_confirmation() {
        let mut cfg = cfg();
        cfg.require_human_confirm = true;
        let err = SpendApproval::confirm(
            Money::from_usd(0.34),
            ApprovalSource::Mcp {
                human_cleared: false,
            },
            &cfg,
            None,
        )
        .expect_err("an uncleared agent request must be gated");
        assert!(
            matches!(err, ApprovalError::HumanConfirmationRequired { .. }),
            "expected HumanConfirmationRequired, got {err:?}"
        );
        assert!(
            err.to_string().contains("approvals grant"),
            "the message must tell the human what to run: {err}"
        );
    }

    #[test]
    fn a_cleared_agent_and_every_human_surface_pass_the_confirm_gate() {
        let mut cfg = cfg();
        cfg.require_human_confirm = true;
        for source in [
            ApprovalSource::Cli,
            ApprovalSource::WebUi,
            ApprovalSource::SlintUi,
            ApprovalSource::Mcp {
                human_cleared: true,
            },
        ] {
            let a = SpendApproval::confirm(Money::from_usd(0.34), source, &cfg, None)
                .unwrap_or_else(|e| panic!("{source:?} must pass the gate: {e}"));
            assert_eq!(a.source(), source);
        }
    }

    #[test]
    fn a_bare_api_call_requires_confirmation_when_the_gate_is_on() {
        // An agent that omits `?source=mcp` lands as Api. That used to skip the human
        // gate entirely; the gate is now about the surface, not the query string the
        // caller chose to write.
        let mut cfg = cfg();
        cfg.require_human_confirm = true;
        let err = SpendApproval::confirm(Money::from_usd(0.34), ApprovalSource::Api, &cfg, None)
            .expect_err("a bare API call is not a human");
        assert!(
            matches!(err, ApprovalError::HumanConfirmationRequired { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn an_uncleared_agent_passes_when_the_gate_is_off() {
        let a = SpendApproval::confirm(
            Money::from_usd(0.34),
            ApprovalSource::Mcp {
                human_cleared: false,
            },
            &cfg(),
            None,
        )
        .expect("the gate is off by default");
        assert_eq!(a.max_usd_per_hour(), Money::from_usd(0.34));
    }

    #[test]
    fn the_ceiling_is_checked_before_the_human_gate() {
        // Otherwise a human would be asked to clear something the daemon would refuse anyway.
        let mut cfg = cfg();
        cfg.require_human_confirm = true;
        let err = SpendApproval::confirm(
            Money::from_usd(9.00),
            ApprovalSource::Mcp {
                human_cleared: false,
            },
            &cfg,
            None,
        )
        .expect_err("above the ceiling");
        assert!(
            matches!(err, ApprovalError::AboveCeiling { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn credit_that_cannot_cover_one_hour_refuses() {
        let err = SpendApproval::confirm(
            Money::from_usd(3.34),
            ApprovalSource::Cli,
            &cfg(),
            Some(1.10),
        )
        .expect_err("$1.10 of credit does not buy an hour at $3.34");
        match err {
            ApprovalError::InsufficientCredit { credit, needed } => {
                assert_eq!(credit, Money::from_usd(1.10));
                assert_eq!(needed, Money::from_usd(3.34));
            }
            other => panic!("expected InsufficientCredit, got {other:?}"),
        }

        // Exactly one hour of credit is enough, and an unreadable balance never invents a
        // refusal — being offline must not block a rental the human already approved.
        assert!(SpendApproval::confirm(
            Money::from_usd(3.34),
            ApprovalSource::Cli,
            &cfg(),
            Some(3.34)
        )
        .is_ok());
        assert!(
            SpendApproval::confirm(Money::from_usd(3.34), ApprovalSource::Cli, &cfg(), None)
                .is_ok()
        );
    }

    #[test]
    fn a_zero_ceiling_refuses_every_paid_request() {
        let mut cfg = cfg();
        cfg.max_usd_per_hour_ceiling = 0.0;
        assert!(matches!(
            SpendApproval::confirm(Money::from_usd(0.01), ApprovalSource::Cli, &cfg, None),
            Err(ApprovalError::AboveCeiling { .. })
        ));
    }
}
