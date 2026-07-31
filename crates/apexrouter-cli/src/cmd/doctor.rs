//! OWNER: unit S-08 (cli/src/cmd/{vast,tunnel,hf,provider,recipe,profile,usage,smoke,doctor,compare,backend,swap,up,approvals,token,open,env,migrate}.rs). Do not edit outside that unit.
//!
//! `apexrouter doctor [--only <check>] [--json]`.
//!
//! # Why the checks run *here* rather than through the daemon
//!
//! §7 classifies `doctor` as `ReadState`: it must answer with nothing running, because the
//! commonest reason to run it is that nothing *is* running. The daemon's own
//! `GET /v1/diagnose` streams SSE (`check` events, then one `done`), and `NodeClient`'s
//! `sse()` decodes into the WebSocket `Event` enum, not into a bare `CheckResult` — so
//! consuming it from here would be decoding the wrong type down a path that does not exist
//! when the answer is most wanted.
//!
//! Instead this runs the registry in-process: `core::checks::local_checks()` plus
//! `providers::checks::provider_checks()`, exactly the two lists the daemon registers, with
//! `proxy_url` filled in from the owner record when a daemon *is* up so `proxy.roundtrip`
//! still means something. Checks run concurrently with per-check timeouts and a panic
//! guard, so `--only rate-limits` is instant instead of waiting through four SSH probes.
//!
//! A check that cannot be meaningful here reports `Skipped`, never `Fail`: being offline,
//! having no vast credential, or not owning a rented box are not defects.

use crate::cli::DoctorArgs;
use crate::cmd::{url, Ctx};
use crate::daemon::Need;
use crate::render;
use apexrouter_core::checks::{CheckCtx, Registry};
use apexrouter_protocol::{CheckResult, CheckStatus};
use std::collections::HashMap;
use std::sync::Arc;

/// Run `apexrouter doctor`.
///
/// # Errors
/// A `$STATE` resolution failure. A *failing check* is not an error: it is the answer, and
/// it is on stdout with a fix line.
pub async fn run(ctx: &Ctx, args: &DoctorArgs) -> anyhow::Result<()> {
    let serving = ctx.serving(Need::ReadState).await?;
    let results = diagnose(ctx, args.only.as_deref()).await;

    if args.json {
        return render::print_json(
            serving.served_by(),
            render::now_unix(),
            serving.is_offline(),
            &results,
        );
    }
    if serving.is_offline() {
        render::print_offline_notice();
    }
    print_results(&results);
    Ok(())
}

/// Run the registry and return every result in **registration** order, so two runs on the
/// same machine produce the same `--json` and a diff between them means something.
///
/// `only` matches loosely, because operators type what they see: an exact id
/// (`together.ratelimits`), a namespace (`creds`) or any fragment, with separators ignored
/// on both sides.
pub async fn diagnose(ctx: &Ctx, only: Option<&str>) -> Vec<CheckResult> {
    let mut registry = Registry::new();
    for c in apexrouter_core::checks::local_checks() {
        registry.register(c);
    }
    for c in apexrouter_providers::checks::provider_checks() {
        registry.register(c);
    }

    let check_ctx = CheckCtx {
        paths: ctx.paths.clone(),
        cfg: Arc::new(ctx.cfg.clone()),
        // `reqwest::Client: Default`; naming the type would make `reqwest` a dependency of
        // this crate for one value the check registry immediately takes ownership of.
        http: Default::default(),
        rig: None,
        // A daemon that is up is the honest `proxy_url`; one that is not is `None`, and a
        // check that needs a daemon then reports `Skipped` rather than a fabricated `Fail`.
        proxy_url: url::proxy_base(ctx).ok().map(|(base, _)| base),
        instance: None,
        ext: HashMap::new(),
    };

    // The registry documents a dropped receiver as normal: `doctor` renders the returned
    // Vec, so there is no stream to attach.
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    drop(rx);
    let results = registry.run(&check_ctx, only, tx).await;

    // A filter that matched nothing is said out loud, exactly as `GET /v1/diagnose` says it:
    // an empty table and "nothing is broken" reads as a clean bill of health for checks that
    // never ran.
    if results.is_empty() {
        if let Some(pattern) = only.map(str::trim).filter(|p| !p.is_empty()) {
            return vec![nothing_selected(pattern, &registry.ids())];
        }
    }
    results
}

/// The honest answer when `--only` selected no check at all.
fn nothing_selected(pattern: &str, ids: &[apexrouter_protocol::CheckId]) -> CheckResult {
    apexrouter_protocol::CheckResult {
        id: apexrouter_protocol::CheckId::from("doctor.selection"),
        label: "check selection".to_owned(),
        status: CheckStatus::Skipped,
        ms: 0,
        detail: format!("`--only {pattern}` matched no registered check"),
        fix: Some(format!(
            "the registry is: {}",
            ids.iter()
                .map(|i| i.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// The rows, then the one-line verdict a human is looking for.
///
/// The fix goes on its own indented line rather than in a column: fixes are sentences, and
/// a sentence in a table cell destroys the alignment that makes the statuses scannable.
pub fn print_results(results: &[CheckResult]) {
    render::print_table(
        &["CHECK", "STATUS", "MS", "DETAIL"],
        results
            .iter()
            .map(|r| {
                vec![
                    r.id.as_str().to_string(),
                    status(r.status),
                    r.ms.to_string(),
                    r.detail.clone(),
                ]
            })
            .collect(),
    );
    for r in results {
        if let Some(fix) = &r.fix {
            render::print_line(&format!("  fix {}: {fix}", r.id.as_str()));
        }
    }
    render::print_blank();
    render::print_line(&verdict(results));
}

/// The tally line: how many of each status, and whether anything is actually broken.
pub fn verdict(results: &[CheckResult]) -> String {
    let count = |s: CheckStatus| results.iter().filter(|r| r.status == s).count();
    let fail = count(CheckStatus::Fail);
    format!(
        "{} pass · {} warn · {} fail · {} skipped — {}",
        count(CheckStatus::Pass),
        count(CheckStatus::Warn),
        fail,
        count(CheckStatus::Skipped),
        if fail == 0 {
            "nothing is broken"
        } else {
            "see the fix lines above"
        }
    )
}

/// A status in its serde spelling, which is what `--json` carries too.
fn status(s: CheckStatus) -> String {
    render::variant(&s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_protocol::CheckId;

    fn result(id: &str, status: CheckStatus) -> CheckResult {
        CheckResult {
            id: CheckId::from(id),
            label: id.to_string(),
            status,
            ms: 3,
            detail: "detail".to_string(),
            fix: None,
        }
    }

    #[test]
    fn statuses_render_in_the_wire_spelling() {
        assert_eq!(status(CheckStatus::Pass), "pass");
        assert_eq!(status(CheckStatus::Skipped), "skipped");
    }

    #[test]
    fn the_verdict_counts_every_status_and_only_fail_is_broken() {
        let rows = vec![
            result("a", CheckStatus::Pass),
            result("b", CheckStatus::Warn),
            result("c", CheckStatus::Skipped),
        ];
        let v = verdict(&rows);
        assert!(v.contains("1 pass"), "{v}");
        assert!(v.contains("1 warn"), "{v}");
        assert!(v.contains("1 skipped"), "{v}");
        assert!(
            v.contains("nothing is broken"),
            "a warning is not a failure: {v}"
        );

        let mut broken = rows;
        broken.push(result("d", CheckStatus::Fail));
        assert!(
            verdict(&broken).contains("fix lines"),
            "{}",
            verdict(&broken)
        );
    }

    #[test]
    fn a_filter_that_matched_nothing_says_so_rather_than_showing_a_clean_bill() {
        let ids = vec![CheckId::from("creds.vast"), CheckId::from("ports.proxy")];
        let r = nothing_selected("zzz", &ids);
        assert_eq!(r.status, CheckStatus::Skipped);
        assert!(r.detail.contains("zzz"), "{}", r.detail);
        let fix = r.fix.expect("the registry is the fix");
        assert!(
            fix.contains("creds.vast") && fix.contains("ports.proxy"),
            "{fix}"
        );
    }

    #[test]
    fn an_empty_registry_still_produces_an_honest_verdict() {
        let v = verdict(&[]);
        assert!(v.contains("0 pass"), "{v}");
        assert!(v.contains("nothing is broken"), "{v}");
    }
}
