//! OWNER: unit S-08 (cli/src/cmd/{vast,tunnel,hf,provider,recipe,profile,usage,smoke,doctor,compare,backend,swap,up,approvals,token,open,env,migrate}.rs). Do not edit outside that unit.
//!
//! `apexrouter migrate [--dry-run] [--from ~/.vastai-gguf] [--localrouter PATH]`. `--dry-run` writes nothing at all.
//!
//! # Plan-only is the default
//!
//! `--dry-run` is in the documented surface and `--apply` is what the I-01 acceptance runs,
//! so both exist — and **neither** is the default. A bare `apexrouter migrate` prints the
//! plan and stops. Reading somebody else's state directory and rewriting your own config on
//! the strength of a verb with no adverb is not a defensible default, and the plan is the
//! thing a human is supposed to read anyway: every row carries `what`, `from`, an action and
//! a reason, precisely so rows can be struck out before `--apply`.
//!
//! # Why this runs without a daemon
//!
//! `apply` writes `config.toml`, `catalog.toml` and the ledger. Autostarting a daemon first
//! would leave it holding a copy of all three that goes stale the instant the import lands
//! — the same reasoning that makes `config init` a `Pure` verb.
//!
//! # `--from` and `--localrouter`
//!
//! `LegacyPaths` resolves `~/.vastai-gguf` from `$HOME` and the LocalRouter checkout from
//! `$APEXROUTER_LOCALROUTER_DIR`, and env vars are the single resolution mechanism
//! (§5.1). So `--localrouter` sets that variable, and `--from` re-roots `$HOME` for the
//! *legacy* half of the resolution only — with `$APEXROUTER_HOME`, `$APEXROUTER_CONFIG` and
//! `$XDG_CACHE_HOME` re-pinned to what this process already resolved, so our own state does
//! not move underneath us. A `--from` whose basename is not `.vastai-gguf` is refused rather
//! than silently ignored: that directory name is the whole of the legacy contract.

use crate::cli::MigrateArgs;
use crate::cmd::Ctx;
use crate::render;
use apexrouter_core::migrate;
use apexrouter_core::paths::Paths;
use apexrouter_protocol::{MigrationAction, MigrationPlan, MigrationReport, ServedBy};
use std::path::Path;

/// The directory name the legacy tool uses. `--from` must end in it.
const LEGACY_DIR_NAME: &str = ".vastai-gguf";

/// Run `apexrouter migrate`.
///
/// # Errors
/// An unreadable legacy tree, a `--from` that is not a `.vastai-gguf` directory, or a
/// failure writing one of the three files `--apply` touches.
pub fn run(ctx: &Ctx, args: &MigrateArgs) -> anyhow::Result<()> {
    let paths = redirect(ctx, args)?;
    let plan = migrate::plan(&paths, &ctx.cfg)?;

    if !args.apply {
        if args.json {
            return render::print_json(ServedBy::Offline, render::now_unix(), false, &plan);
        }
        print_plan(&plan);
        render::print_blank();
        render::print_line(
            "Nothing was written. Re-run with --apply to import the rows marked `import`.",
        );
        return Ok(());
    }

    let report: MigrationReport = migrate::apply(&paths, &ctx.cfg, &plan)?;
    if args.json {
        return render::print_json(ServedBy::Offline, render::now_unix(), false, &report);
    }
    print_plan(&plan);
    render::print_blank();
    render::print_line(&format!(
        "imported {} · skipped {}",
        report.imported, report.skipped
    ));
    for w in &report.warnings {
        render::print_line(&format!("warning: {w}"));
    }
    Ok(())
}

/// Apply `--from`/`--localrouter` and re-resolve, or return what the process already had.
///
/// # Errors
/// A `--from` that is not a directory named `.vastai-gguf`, a `--localrouter` that is not a
/// directory, or a re-resolution that cannot find a home.
fn redirect(ctx: &Ctx, args: &MigrateArgs) -> anyhow::Result<Paths> {
    if args.from.is_none() && args.localrouter.is_none() {
        return Ok(ctx.paths.clone());
    }

    if let Some(dir) = &args.localrouter {
        if !dir.is_dir() {
            anyhow::bail!("--localrouter {} is not a directory", dir.display());
        }
        std::env::set_var("APEXROUTER_LOCALROUTER_DIR", dir);
    }

    if let Some(from) = &args.from {
        let home = legacy_home(from)?;
        // Pin everything of ours to what is already resolved, so moving $HOME for the
        // legacy lookup cannot move our state, our config or our cache.
        std::env::set_var("APEXROUTER_HOME", ctx.paths.state());
        std::env::set_var("APEXROUTER_CONFIG", ctx.paths.config_file());
        if let Some(cache_home) = ctx.paths.cache().parent() {
            std::env::set_var("XDG_CACHE_HOME", cache_home);
        }
        std::env::set_var("HOME", home);
    }
    Ok(Paths::resolve()?)
}

/// The `$HOME` that makes `LegacyPaths` resolve `--from` — its parent directory.
///
/// # Errors
/// When `from` is not a directory, is not named `.vastai-gguf`, or has no parent.
fn legacy_home(from: &Path) -> anyhow::Result<std::path::PathBuf> {
    if !from.is_dir() {
        anyhow::bail!("--from {} is not a directory", from.display());
    }
    if from.file_name().and_then(|n| n.to_str()) != Some(LEGACY_DIR_NAME) {
        anyhow::bail!(
            "--from must name a `{LEGACY_DIR_NAME}` directory, not {} — the legacy tool's \
             state is found by that name and nothing else reads as it",
            from.display()
        );
    }
    from.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow::anyhow!("--from {} has no parent directory", from.display()))
}

/// The plan: where it looked, then one row per artefact with its reason.
fn print_plan(plan: &MigrationPlan) {
    for p in &plan.source_paths {
        render::print_line(&format!("source  {p}"));
    }
    if plan.source_paths.is_empty() {
        render::print_line("source  (nothing legacy was found on this machine)");
    }
    render::print_blank();
    render::print_table(
        &["ACTION", "WHAT", "FROM", "WHY"],
        plan.items
            .iter()
            .map(|i| {
                vec![
                    action(i.action),
                    i.what.clone(),
                    i.from.clone(),
                    i.detail.clone(),
                ]
            })
            .collect(),
    );
}

/// The action column, in its serde spelling.
fn action(a: MigrationAction) -> String {
    render::variant(&a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_must_name_the_legacy_directory_by_its_real_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let wrong = dir.path().join("old-state");
        std::fs::create_dir(&wrong).expect("mkdir");
        let e = legacy_home(&wrong).expect_err("must fail");
        assert!(e.to_string().contains(".vastai-gguf"), "{e}");

        let right = dir.path().join(LEGACY_DIR_NAME);
        std::fs::create_dir(&right).expect("mkdir");
        assert_eq!(legacy_home(&right).expect("home"), dir.path());
    }

    #[test]
    fn from_a_file_or_a_missing_path_is_refused_rather_than_surveyed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join(LEGACY_DIR_NAME);
        std::fs::write(&f, b"not a directory").expect("write");
        assert!(legacy_home(&f).is_err());
        assert!(legacy_home(&dir.path().join("nope")).is_err());
    }

    #[test]
    fn the_action_column_uses_the_serde_spelling() {
        assert_eq!(action(MigrationAction::Import), "import");
        assert_eq!(action(MigrationAction::Skip), "skip");
        assert_eq!(action(MigrationAction::Warn), "warn");
    }
}
