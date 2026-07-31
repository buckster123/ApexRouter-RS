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
//!
//! # What has to be *in* the context, or the check may as well not exist
//!
//! Six of nineteen checks used to be unrunnable, and none of them said so honestly:
//! `builds.discovered`, `builds.flags` and `devices.enumerated` skipped with "no rig scan
//! yet" and told the operator to run `apexrouter rig` — which changed nothing, because
//! nothing ever put a [`apexrouter_protocol::RigSnapshot`] into the `CheckCtx`. `vast.credit`
//! and `vast.orphans` skipped with "no vast.ai client" on the line below a passing
//! `creds.vast`, because nothing ever put one into `ext` either.
//!
//! So this function fills both. The rig comes from a **discovery scan** — `--help`,
//! `--version` and `--list-devices` against each `llama-server`, which are metadata calls
//! that read no weights — and it is skipped entirely unless the `--only` selection could
//! want it, so `doctor --only rate-limits` stays instant. `ext` comes from
//! [`apexrouter_providers::checks::provider_ext`], which builds a vast client when a
//! credential resolves and hands back an empty map when none does.
//!
//! A `doctor --json` that reports zero failures has to mean the checks ran.

use crate::cli::DoctorArgs;
use crate::cmd::Ctx;
use crate::daemon::{self, Need};
use crate::render;
use apexrouter_core::checks::{CheckCtx, Registry};
use apexrouter_protocol::{CheckResult, CheckStatus};
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

    let cfg = Arc::new(ctx.cfg.clone());
    let check_ctx = CheckCtx {
        paths: ctx.paths.clone(),
        cfg: Arc::clone(&cfg),
        // `reqwest::Client: Default`; naming the type would make `reqwest` a dependency of
        // this crate for one value the check registry immediately takes ownership of.
        http: Default::default(),
        rig: rig_for(ctx, only).await,
        // A daemon that is up is the honest `proxy_url`; one that is not is `None`, and a
        // check that needs a daemon then reports `Skipped` rather than a fabricated `Fail`.
        //
        // Read from the **owner record**, not `url::proxy_base`: that helper falls back to
        // the configured bind address when no daemon owns the lock, so it is never `None` —
        // which turned `CheckNeeds::Daemon` into a decoration and made `proxy.roundtrip`
        // report `fail: unreachable` on every machine that simply was not serving yet. The
        // owner record is `Some` only while a live daemon holds the lock.
        proxy_url: daemon::owner_record(&ctx.paths, std::time::Duration::ZERO)
            .ok()
            .flatten()
            .map(|rec| rec.proxy_url.trim_end_matches('/').to_owned()),
        instance: None,
        // The vast client, when a credential resolves. Without this the two fleet checks
        // can never run; with it they run read-only calls and nothing else.
        ext: apexrouter_providers::checks::provider_ext(&cfg, &ctx.paths),
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

/// The check ids that are meaningless without a [`RigSnapshot`], and are the only reason
/// `doctor` would pay for a discovery scan.
const RIG_CHECKS: [&str; 3] = ["builds.discovered", "builds.flags", "devices.enumerated"];

/// The rig, scanned here, or `None` when the selection cannot want it and when the scan
/// fails.
///
/// A scan runs `llama-server --help`, `--version` and `--list-devices` per discovered
/// binary. Those are **metadata calls that load no weights** — the flag probe is cached
/// under `$CACHE` — so this is seconds at worst and nothing at all on the second run.
///
/// A failed scan is `None` and the three checks skip, which is the same honest outcome as
/// before; the difference is that on a working machine they now run.
async fn rig_for(ctx: &Ctx, only: Option<&str>) -> Option<Arc<apexrouter_protocol::RigSnapshot>> {
    if !wants_rig(only) {
        return None;
    }
    match apexrouter_providers::local::supervisor::scan_rig(&ctx.cfg.endpoints, ctx.paths.cache())
        .await
    {
        Ok(rig) => Some(Arc::new(rig)),
        Err(e) => {
            tracing::debug!(error = %e, "the rig scan failed; the rig checks will skip");
            None
        }
    }
}

/// Could this `--only` selection reach a check that reads the rig?
///
/// Deliberately **wider** than `Registry`'s own filter, which is private to `core`: this
/// matches when either side contains the other, so the worst a mismatch can do is scan a rig
/// nobody looks at. Being narrower would put the "check that can never run" defect straight
/// back, so the asymmetry is the point.
fn wants_rig(only: Option<&str>) -> bool {
    let Some(pattern) = only.map(normalise).filter(|p| !p.is_empty()) else {
        return true;
    };
    RIG_CHECKS
        .iter()
        .map(|id| normalise(id))
        .any(|id| id.contains(&pattern) || pattern.contains(&id))
}

/// Lower-case, with everything that is not a letter or a digit removed — `core`'s own rule
/// for `--only`, so `--only rig-devices` and `devices.enumerated` still meet.
fn normalise(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
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
    fn the_rig_is_scanned_for_every_selection_that_could_read_it() {
        // The whole run, and the two namespaces the rig checks live in.
        assert!(wants_rig(None));
        assert!(wants_rig(Some("")));
        assert!(wants_rig(Some("   ")));
        assert!(wants_rig(Some("builds")));
        assert!(wants_rig(Some("devices")));
        assert!(wants_rig(Some("devices.enumerated")));
        // Separators and case are ignored on both sides, as `--only` matching is.
        assert!(wants_rig(Some("DEVICES_ENUMERATED")));
        assert!(wants_rig(Some("builds-flags")));

        // …and nothing else pays for a Vulkan enumeration.
        assert!(!wants_rig(Some("rate-limits")));
        assert!(!wants_rig(Some("creds")));
        assert!(!wants_rig(Some("ports.proxy")));
        assert!(!wants_rig(Some("vast")));
    }

    #[test]
    fn every_rig_check_this_gate_names_is_a_check_that_exists() {
        // A typo here would silently reintroduce "the check can never run": the gate would
        // decline to scan for a selection that does in fact reach a rig check.
        let ids: Vec<String> = apexrouter_core::checks::local_checks()
            .iter()
            .map(|c| c.id().as_str().to_owned())
            .collect();
        for want in RIG_CHECKS {
            assert!(ids.iter().any(|id| id == want), "{want} is not registered");
        }
    }

    #[test]
    fn an_empty_registry_still_produces_an_honest_verdict() {
        let v = verdict(&[]);
        assert!(v.contains("0 pass"), "{v}");
        assert!(v.contains("nothing is broken"), "{v}");
    }
}
