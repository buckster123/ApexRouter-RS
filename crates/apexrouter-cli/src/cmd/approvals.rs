//! OWNER: unit S-08 (cli/src/cmd/{vast,tunnel,hf,provider,recipe,profile,usage,smoke,doctor,compare,backend,swap,up,approvals,token,open,env,migrate}.rs). Do not edit outside that unit.
//!
//! `apexrouter approvals ls | grant <id> | deny <id>` — clears a pending human confirmation.
//!
//! An [`ApprovalRequest`] is what a surface that *cannot* ask a human leaves behind: the MCP
//! `apexrouter_vast_rent` tool without `confirm`, or `POST /v1/vast/instances` without
//! `{confirm, max_usd_per_hour}`, both answer with the cost preview rather than renting. The
//! row then waits here.
//!
//! **`grant` is the money decision**, so it requires `--yes` and prints `$/hr`, the estimated
//! total and the **current credit** before it acts — the same three numbers `vast rent`
//! prints, from the same `ApprovalRequest`, because an approval granted without seeing the
//! burn-down is exactly the mistake this whole subsystem exists to prevent.
//!
//! `deny` needs no confirmation: refusing to spend is never the dangerous direction.

use crate::cli::ApprovalsCmd;
use crate::cmd::Ctx;
use crate::daemon::Need;
use crate::render;
use apexrouter_protocol::{ApprovalRequest, ServedBy};

/// Run `apexrouter approvals …`.
///
/// # Errors
/// A daemon that will not answer, an unknown approval id, or a `grant` without `--yes`.
pub async fn run(ctx: &Ctx, cmd: &ApprovalsCmd) -> anyhow::Result<()> {
    let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
    match cmd {
        ApprovalsCmd::Ls(args) => {
            let rows: Vec<ApprovalRequest> = client.get("/v1/approvals").await?;
            if args.json {
                return render::print_json(ServedBy::Daemon, render::now_unix(), false, &rows);
            }
            render::print_table(
                &[
                    "ID",
                    "WHAT",
                    "MAX $/HR",
                    "EST TOTAL",
                    "CREDIT",
                    "SOURCE",
                    "AGE",
                ],
                rows.iter().map(row).collect(),
            );
            if rows.is_empty() {
                render::print_line("(nothing is waiting for a decision)");
            }
            Ok(())
        }
        ApprovalsCmd::Grant { id, yes } => {
            let pending: Vec<ApprovalRequest> = client.get("/v1/approvals").await?;
            let req = find(&pending, id)?;
            print_cost(&req);
            if !*yes {
                anyhow::bail!(
                    "granting `{id}` starts an hourly bill — re-run with --yes once the \
                     numbers above are what you expect"
                );
            }
            let _: serde_json::Value = client
                .post(
                    &format!("/v1/approvals/{id}/grant"),
                    &serde_json::json!({ "confirm": true }),
                )
                .await?;
            render::print_line(&format!("granted {id}"));
            Ok(())
        }
        ApprovalsCmd::Deny { id } => {
            let _: serde_json::Value = client
                .post(&format!("/v1/approvals/{id}/deny"), &serde_json::json!({}))
                .await?;
            render::print_line(&format!("denied {id}"));
            Ok(())
        }
    }
}

/// One pending approval by id, or a message naming what is waiting.
///
/// # Errors
/// When nothing pending carries that id — including when nothing is pending at all, which
/// is the commonest way to reach here (the approval was already granted, denied or expired).
pub fn find(pending: &[ApprovalRequest], id: &str) -> anyhow::Result<ApprovalRequest> {
    pending
        .iter()
        .find(|a| a.id.to_string() == id)
        .cloned()
        .ok_or_else(|| {
            let known: Vec<String> = pending.iter().map(|a| a.id.to_string()).collect();
            anyhow::anyhow!(
                "no approval `{id}` is waiting{}",
                if known.is_empty() {
                    " — nothing is pending".to_string()
                } else {
                    format!(" — waiting: {}", known.join(", "))
                }
            )
        })
}

/// The three numbers a human must see before saying yes, plus the burn-down.
///
/// Credit is `Option<f64>` on purpose: `None` means *we could not ask*, which is itself
/// worth showing rather than rendering a confident `$0.00`.
pub fn print_cost(a: &ApprovalRequest) {
    render::print_line(&a.what);
    render::print_line(&format!("  $/hr (max)   ${:.4}", a.max_usd_per_hour));
    render::print_line(&format!("  est total    ${:.2}", a.est_total_usd));
    match a.credit {
        Some(c) => {
            render::print_line(&format!("  credit       ${c:.2}"));
            if let Some(h) = burn_down_hours(c, a.max_usd_per_hour) {
                render::print_line(&format!("  burn-down    {h:.1} hours at this ceiling"));
            }
        }
        None => render::print_line("  credit       (could not be read — ask vast.ai directly)"),
    }
    render::print_line(&format!("  requested by {}", a.source));
}

/// How long the remaining credit lasts at a rate. `None` when the rate is not positive.
pub fn burn_down_hours(credit: f64, dph: f64) -> Option<f64> {
    if dph > 0.0 && dph.is_finite() && credit.is_finite() {
        Some(credit / dph)
    } else {
        None
    }
}

/// One row of the approvals table.
fn row(a: &ApprovalRequest) -> Vec<String> {
    vec![
        a.id.to_string(),
        a.what.clone(),
        format!("{:.4}", a.max_usd_per_hour),
        format!("{:.2}", a.est_total_usd),
        a.credit
            .map(|c| format!("{c:.2}"))
            .unwrap_or_else(|| "?".to_string()),
        a.source.clone(),
        render::human_secs(render::now_unix() - a.requested_at_unix),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Built from JSON rather than a struct literal: `JobId` wraps a `Ulid`, and `ulid` is
    /// not a dependency of this crate — the wire shape is the contract anyway.
    fn approval(credit: Option<f64>) -> ApprovalRequest {
        serde_json::from_value(serde_json::json!({
            "id": "00000000000000000000000000",
            "what": "2x RTX 3090 · offer 12345",
            "max_usd_per_hour": 0.60,
            "est_total_usd": 1.20,
            "credit": credit,
            "requested_at_unix": render::now_unix() - 30,
            "source": "mcp",
        }))
        .expect("ApprovalRequest")
    }

    #[test]
    fn unreadable_credit_renders_as_a_question_mark_never_as_zero() {
        assert_eq!(row(&approval(None))[4], "?");
        assert_eq!(row(&approval(Some(7.73)))[4], "7.73");
    }

    #[test]
    fn burn_down_is_credit_over_rate_and_never_divides_by_zero() {
        assert_eq!(burn_down_hours(7.73, 0.60), Some(7.73 / 0.60));
        assert_eq!(burn_down_hours(7.73, 0.0), None);
        assert_eq!(burn_down_hours(7.73, f64::NAN), None);
    }

    #[test]
    fn a_missing_approval_says_whether_anything_is_pending_at_all() {
        let e = find(&[], "01J").expect_err("must fail");
        assert!(e.to_string().contains("nothing is pending"), "{e}");
        let e = find(&[approval(None)], "01J").expect_err("must fail");
        assert!(e.to_string().contains("waiting:"), "{e}");
    }
}
