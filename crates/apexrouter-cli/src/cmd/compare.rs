//! OWNER: unit S-08 (cli/src/cmd/{vast,tunnel,hf,provider,recipe,profile,usage,smoke,doctor,compare,backend,swap,up,approvals,token,open,env,migrate}.rs). Do not edit outside that unit.
//!
//! `apexrouter compare --alias A --alias B --prompt P [--max-tokens N] [--json]`.
//!
//! This is what `Strategy::Mirror` and `Strategy::Fastest` were actually *for*, shipped as
//! an explicit verb instead of a routing mode that silently doubles every bill
//! (`ARCHITECTURE.md` §12). One `POST /v1/compare`, which runs every alias **concurrently**
//! against the live routing table and answers with one `CompareRow` each.
//!
//! Blocking is the default and it is deliberate: the comparison *is* the answer, so a job id
//! would be a worse result than a wait. Cost rides back as a `CostEstimate`, so a row whose
//! upstream published no usage says `?` rather than `$0.00`.

use crate::cli::CompareArgs;
use crate::cmd::{route, Ctx};
use crate::daemon::Need;
use crate::render;
use apexrouter_protocol::{CompareRow, ServedBy};

/// Run `apexrouter compare`.
///
/// # Errors
/// An invalid alias, a daemon that will not answer, or an alias with no route.
pub async fn run(ctx: &Ctx, args: &CompareArgs) -> anyhow::Result<()> {
    let aliases = parse_aliases(&args.aliases)?;
    if args.prompt.trim().is_empty() {
        anyhow::bail!("--prompt must not be empty: every alias gets the same one");
    }
    let client = ctx.serving(Need::Mutate).await?.into_daemon()?;

    let body = serde_json::json!({
        "aliases": aliases,
        "prompt": args.prompt,
        "max_tokens": args.max_tokens,
    });
    let rows: Vec<CompareRow> = client.post("/v1/compare", &body).await?;

    if args.json {
        return render::print_json(ServedBy::Daemon, render::now_unix(), false, &rows);
    }
    print_rows(&rows);
    Ok(())
}

/// Validate every `--alias` up front, so a typo in the fourth one does not surface only
/// after the first three have already generated tokens.
///
/// # Errors
/// An alias that does not match the id charset, or a list with nothing in it.
pub fn parse_aliases(raw: &[String]) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::with_capacity(raw.len());
    for a in raw {
        out.push(route::parse_alias(a)?.as_str().to_string());
    }
    if out.is_empty() {
        anyhow::bail!("name at least one --alias");
    }
    Ok(out)
}

/// The table, then each preview in full underneath it.
///
/// Two blocks rather than one wide table: a 200-character preview in a cell destroys the
/// column alignment that makes the numbers comparable, and the numbers are why anyone ran
/// this.
fn print_rows(rows: &[CompareRow]) {
    render::print_table(
        &[
            "ALIAS", "BACKEND", "MODEL", "OK", "MS", "TTFT", "TOK/S", "TOKENS", "COST",
        ],
        rows.iter().map(row).collect(),
    );
    for r in rows {
        render::print_blank();
        render::print_line(&format!("--- {} ---", r.alias.as_str()));
        match &r.error {
            Some(e) => render::print_line(&format!("error: {e}")),
            None => render::print_line(&r.preview),
        }
    }
}

/// One row of the comparison table.
fn row(r: &CompareRow) -> Vec<String> {
    vec![
        r.alias.as_str().to_string(),
        r.backend
            .as_ref()
            .map(|b| b.as_str().to_string())
            .unwrap_or_default(),
        r.model.clone(),
        if r.ok { "pass" } else { "FAIL" }.to_string(),
        r.ms.to_string(),
        render::dash(r.ttft_ms),
        r.tok_per_s.map(|v| format!("{v:.1}")).unwrap_or_default(),
        tokens(r),
        crate::cmd::usage::money(&r.cost),
    ]
}

/// `prompt+completion` as one cell, keeping the honesty marker `TokenCount` carries.
///
/// An estimated count is prefixed `~`: a tokenizer we do not have is the difference between
/// a measurement and a guess, and a guess must not render as a fact.
fn tokens(r: &CompareRow) -> String {
    use apexrouter_protocol::TokenCount;
    let one = |t: &Option<TokenCount>| match t {
        Some(TokenCount::Reported(n)) => n.to_string(),
        Some(TokenCount::Estimated(n)) => format!("~{n}"),
        None => "-".to_string(),
    };
    format!("{}+{}", one(&r.prompt_tokens), one(&r.completion_tokens))
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_protocol::{Alias, CostEstimate, TokenCount};

    fn compare_row() -> CompareRow {
        CompareRow {
            alias: Alias::parse("auto").expect("alias"),
            backend: None,
            model: "Carnice-9b".to_string(),
            ok: true,
            ms: 900,
            ttft_ms: Some(120),
            tok_per_s: Some(9.71),
            prompt_tokens: Some(TokenCount::Reported(12)),
            completion_tokens: Some(TokenCount::Estimated(64)),
            cost: CostEstimate::Unknown,
            preview: "hello".to_string(),
            error: None,
        }
    }

    #[test]
    fn every_alias_is_validated_before_a_single_token_is_generated() {
        let ok = parse_aliases(&["auto".to_string(), "fast".to_string()]).expect("valid");
        assert_eq!(ok, ["auto", "fast"]);
        let e = parse_aliases(&["auto".to_string(), "Not An Alias".to_string()])
            .expect_err("must fail");
        assert!(e.to_string().contains("not a valid alias"), "{e}");
        assert!(parse_aliases(&[]).is_err());
    }

    #[test]
    fn an_estimated_token_count_is_marked_as_one() {
        assert_eq!(tokens(&compare_row()), "12+~64");
    }

    #[test]
    fn a_row_that_failed_says_fail_rather_than_showing_a_blank() {
        let mut r = compare_row();
        r.ok = false;
        assert_eq!(row(&r)[3], "FAIL");
        assert_eq!(row(&compare_row())[3], "pass");
        assert_eq!(row(&compare_row())[8], "?", "unknown cost is never $0.00");
    }
}
