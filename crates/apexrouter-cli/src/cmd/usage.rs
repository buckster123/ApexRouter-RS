//! OWNER: unit S-08 (cli/src/cmd/{vast,tunnel,hf,provider,recipe,profile,usage,smoke,doctor,compare,backend,swap,up,approvals,token,open,env,migrate}.rs). Do not edit outside that unit.
//!
//! `apexrouter usage [--since 24h|7d|all] [--by provider|model|backend|alias|day] [--json]`.
//!
//! A `ReadState` verb, and one of the ones where that matters most: the rows are two
//! append-only files (`$STATE/usage.jsonl` and, while `[compat] read_legacy_state` is on,
//! `~/.vastai-gguf/usage.log`), so "what did last week cost?" is answerable with nothing
//! running. Offline the CLI reads and aggregates them itself with exactly the two functions
//! the daemon calls — `core::usage::{read_all, aggregate}` — so the two paths cannot drift
//! into disagreeing about a number a human is going to act on.
//!
//! Cost is a [`CostEstimate`], never an `f64`: one approximate row visibly demotes the total
//! instead of the total quietly claiming to be metered.

use crate::cli::UsageArgs;
use crate::cmd::Ctx;
use crate::daemon::{Need, Serving};
use crate::render;
use apexrouter_core::usage::{aggregate, read_all};
use apexrouter_protocol::{CostEstimate, UsageSummary};

/// Run `apexrouter usage`.
///
/// # Errors
/// A usage file that exists but cannot be read, or a daemon that will not answer.
pub async fn run(ctx: &Ctx, args: &UsageArgs) -> anyhow::Result<()> {
    let serving = ctx.serving(Need::ReadState).await?;
    let summary = load(ctx, &serving, args).await?;

    if args.json {
        return render::print_json(
            serving.served_by(),
            render::now_unix(),
            serving.is_offline(),
            &summary,
        );
    }
    if serving.is_offline() {
        render::print_offline_notice();
    }
    print_summary(&summary, args);
    Ok(())
}

/// The summary: from the daemon when there is one, computed here when there is not.
///
/// # Errors
/// An unreadable usage file, or a daemon that will not answer.
pub async fn load(ctx: &Ctx, serving: &Serving, args: &UsageArgs) -> anyhow::Result<UsageSummary> {
    if let Serving::Daemon(c) = serving {
        let path = format!(
            "/v1/usage?since={}&by={}",
            urlencode(&args.since),
            args.by.as_query()
        );
        return Ok(c.get::<UsageSummary>(&path).await?);
    }
    // The blocking read is fine here: this process has nothing else to do, and the answer
    // is the whole point of the invocation.
    let rows = read_all(&ctx.paths, &ctx.cfg.compat)?;
    Ok(aggregate(&rows, parse_since(&args.since)?, args.by.into()))
}

/// Parse `--since` into a unix-seconds cutoff, the way `GET /v1/usage` does.
///
/// Accepted: `all` / `forever` / `0` / empty (no cutoff), `<n><s|m|h|d|w>` relative to now,
/// or anything `core::usage::parse_lenient_timestamp` accepts.
///
/// # Errors
/// A window that is none of those. Silently treating it as `all` would answer a question
/// nobody asked, and the answer would look plausible.
pub fn parse_since(spec: &str) -> anyhow::Result<Option<i64>> {
    let raw = spec.trim();
    if raw.is_empty()
        || raw == "0"
        || raw.eq_ignore_ascii_case("all")
        || raw.eq_ignore_ascii_case("forever")
    {
        return Ok(None);
    }
    if let Some(secs) = relative_secs(raw) {
        return Ok(Some(render::now_unix() - secs));
    }
    apexrouter_core::usage::parse_lenient_timestamp(raw)
        .map(Some)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "`{raw}` is not a window — use `all`, a duration like `30m`/`24h`/`7d`/`4w`, \
                 or a timestamp"
            )
        })
}

/// `<n><unit>` in seconds, or `None` when it is not that shape.
fn relative_secs(raw: &str) -> Option<i64> {
    let (digits, unit) = raw.split_at(raw.len().checked_sub(1)?);
    let n: i64 = digits.parse().ok()?;
    let mult = match unit {
        "s" | "S" => 1,
        "m" | "M" => 60,
        "h" | "H" => 3_600,
        "d" | "D" => 86_400,
        "w" | "W" => 604_800,
        _ => return None,
    };
    n.checked_mul(mult)
}

/// Percent-encode a window before it reaches a query string. A window is usually `24h`;
/// this is what keeps a timestamp with a `:` in it from arriving mangled.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' => out.push(ch),
            _ => {
                let mut buf = [0u8; 4];
                for b in ch.encode_utf8(&mut buf).as_bytes() {
                    out.push_str(&format!("%{b:02X}"));
                }
            }
        }
    }
    out
}

/// The table, plus the one total line a human is actually looking for.
fn print_summary(s: &UsageSummary, args: &UsageArgs) {
    render::print_line(&format!(
        "{} · by {} · {} row(s)",
        s.window,
        args.by.as_query(),
        s.rows
    ));
    render::print_table(
        &[
            "KEY",
            "COST",
            "PROMPT",
            "COMPLETION",
            "REQUESTS",
            "TOK/S p50",
        ],
        s.by.iter()
            .map(|b| {
                vec![
                    b.key.clone(),
                    money(&b.cost),
                    b.prompt_tokens.to_string(),
                    b.completion_tokens.to_string(),
                    b.requests.to_string(),
                    b.tok_per_s_p50
                        .map(|v| format!("{v:.1}"))
                        .unwrap_or_default(),
                ]
            })
            .collect(),
    );
    render::print_line(&format!(
        "total  {}  ({} prompt + {} completion tokens)",
        money(&s.total_cost),
        s.total_prompt,
        s.total_completion
    ));
}

/// A [`CostEstimate`] as a cell, with its provenance visible rather than rounded away.
///
/// `Metered` prints bare, `Approximate` carries a `~`, and `Unknown` is `?` — never `$0.00`,
/// which is a claim rather than an answer.
pub fn money(c: &CostEstimate) -> String {
    match c {
        CostEstimate::Metered { usd, .. } => format!("${:.4}", usd.as_usd()),
        CostEstimate::Approximate { usd, .. } => format!("~${:.4}", usd.as_usd()),
        CostEstimate::Unknown => "?".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_protocol::{Money, PriceSource};

    #[test]
    fn all_and_its_synonyms_mean_no_cutoff() {
        for s in ["all", "ALL", "forever", "0", "  "] {
            assert_eq!(parse_since(s).expect(s), None, "{s}");
        }
    }

    #[test]
    fn a_duration_is_now_minus_that_many_seconds() {
        let now = render::now_unix();
        let cut = parse_since("24h").expect("24h").expect("some");
        assert!((now - cut - 86_400).abs() <= 2, "{cut}");
        assert_eq!(relative_secs("7d"), Some(604_800));
        assert_eq!(relative_secs("4w"), Some(2_419_200));
        assert_eq!(relative_secs("30m"), Some(1_800));
    }

    #[test]
    fn an_unparseable_window_is_an_error_not_a_silent_all() {
        let e = parse_since("last tuesday").expect_err("must fail");
        assert!(e.to_string().contains("not a window"), "{e}");
    }

    #[test]
    fn cost_renders_its_provenance_and_never_invents_a_zero() {
        assert_eq!(
            money(&CostEstimate::Metered {
                usd: Money::from_usd(1.5),
                source: PriceSource::ProviderApi,
            }),
            "$1.5000"
        );
        assert_eq!(
            money(&CostEstimate::Approximate {
                usd: Money::from_usd(1.5),
                source: PriceSource::ConfigTable,
                assumption: "tok/s hint".to_string(),
            }),
            "~$1.5000"
        );
        assert_eq!(money(&CostEstimate::Unknown), "?");
    }

    #[test]
    fn a_window_reaches_the_query_string_encoded() {
        assert_eq!(urlencode("24h"), "24h");
        assert_eq!(
            urlencode("2026-07-30T10:00:00Z"),
            "2026-07-30T10%3A00%3A00Z"
        );
    }
}
