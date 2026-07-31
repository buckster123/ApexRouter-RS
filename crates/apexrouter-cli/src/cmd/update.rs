//! OWNER: unit S-06 (cli/src/{main,cli,daemon,render}.rs, cli/src/cmd/{mod,status,serve,
//! config,rig,models,fit,endpoint,route,switch,url,version,completions,update}.rs). Do not
//! edit outside that unit.
//!
//! `apexrouter update [--no-pull]` — pull the checkout the installer recorded, then hand
//! over to that checkout's `install.sh`.
//!
//! # Why this delegates instead of building
//!
//! `install.sh` is the one implementation of "build the binaries, put them where this
//! machine keeps them, and prove the daemon that is serving runs the inode just written"
//! — its verify step is anchored to `/proc/<pid>/exe`, not to a port. It also restores
//! every install-time choice from `$STATE/install.conf` (prefix, GUI, service, jobs), so a
//! re-run *upgrades* the install instead of re-deciding it. A second implementation of all
//! that in Rust would only drift from the first. The one thing the installer deliberately
//! never does is touch the checkout's git state — `--from-source` means "no clone, no
//! pull" — so the pull is this verb's half of the job, and everything after the pull is
//! the installer's.
//!
//! # No `--json`
//!
//! The handover gives the terminal to `install.sh`, whose output is a human narrative
//! with prompts suppressed (`--yes`). Wrapping that in an envelope would mean parsing our
//! own installer's prose back out of a pipe; scripts that need a machine answer already
//! have `apexrouter version --json` before and after.

use crate::cli::UpdateArgs;
use crate::cmd::Ctx;
use crate::render;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The file the installer writes its resolved choices to, under `$STATE`.
const CONF_NAME: &str = "install.conf";

/// The key in it that names the checkout.
const SOURCE_KEY: &str = "APEXROUTER_INSTALL_SOURCE=";

/// The key that names where that checkout was cloned from, for the error path.
const REPO_KEY: &str = "APEXROUTER_INSTALL_REPO=";

/// Run `apexrouter update`.
///
/// # Errors
/// No recorded install, a recorded checkout that is gone, a pull that is not a
/// fast-forward, or an installer run that fails — each named, none silent.
pub fn run(ctx: &Ctx, args: &UpdateArgs) -> anyhow::Result<()> {
    let conf = ctx.paths.state().join(CONF_NAME);
    let recorded = read_conf(&conf)?;
    let source = recorded.source_dir()?;
    let installer = source.join("install.sh");
    if !installer.is_file() {
        anyhow::bail!(
            "{} names {} but there is no install.sh in it — reinstall per docs/INSTALL.md \
             and the record will be rewritten",
            conf.display(),
            source.display()
        );
    }

    if args.no_pull {
        render::print_line("--no-pull: building what the checkout already holds");
    } else if source.join(".git").is_dir() {
        let before = describe(&source);
        render::print_line(&format!("pulling {} ({before})", source.display()));
        // `--ff-only`: an update must never invent a merge commit in a checkout the
        // operator may also work in. A diverged checkout is theirs to resolve.
        let status = Command::new("git")
            .arg("-C")
            .arg(&source)
            .args(["pull", "--ff-only"])
            .status()
            .map_err(|e| anyhow::anyhow!("could not run git: {e}"))?;
        if !status.success() {
            anyhow::bail!(
                "git pull --ff-only failed in {} — resolve it there, or re-run with \
                 --no-pull to build what is already checked out",
                source.display()
            );
        }
        let after = describe(&source);
        let moved = if before == after {
            format!("already up to date at {after}")
        } else {
            format!("{before} -> {after}")
        };
        render::print_line(&moved);
    } else {
        render::print_line(&format!(
            "{} is not a git checkout — skipping the pull",
            source.display()
        ));
    }

    render::print_line(&format!("handing over to {}", installer.display()));
    // An argv vector, never a shell string (house rule 3). `--yes` because the operator
    // already answered every question at install time and install.conf remembers.
    let status = Command::new("bash")
        .arg(&installer)
        .arg("--yes")
        .current_dir(&source)
        .status()
        .map_err(|e| anyhow::anyhow!("could not run bash {}: {e}", installer.display()))?;
    if !status.success() {
        anyhow::bail!("install.sh exited with {status} — its stderr names the step");
    }
    Ok(())
}

/// What `install.conf` recorded, as far as this verb cares.
#[derive(Debug)]
struct Recorded {
    /// Where the record itself lives, for error messages.
    conf: PathBuf,
    /// `APEXROUTER_INSTALL_SOURCE`.
    source: Option<PathBuf>,
    /// `APEXROUTER_INSTALL_REPO`.
    repo: Option<String>,
}

impl Recorded {
    /// The checkout, which must still exist — with the recovery path named when it does
    /// not, because a `curl | bash` install that deleted its clone is a normal history.
    fn source_dir(&self) -> anyhow::Result<PathBuf> {
        let source = self.source.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "{} records no APEXROUTER_INSTALL_SOURCE — reinstall per docs/INSTALL.md",
                self.conf.display()
            )
        })?;
        if source.is_dir() {
            return Ok(source);
        }
        let hint = self
            .repo
            .as_deref()
            .unwrap_or("https://github.com/buckster123/ApexRouter-RS");
        anyhow::bail!(
            "the recorded checkout {} is gone. Re-clone and reinstall:\n  git clone {} && \
             cd {} && ./install.sh\n(the installer keeps your choices — they live in {})",
            source.display(),
            hint,
            hint.rsplit('/').next().unwrap_or("ApexRouter-RS"),
            self.conf.display()
        )
    }
}

/// Parse `install.conf`. Missing file is the one error here; missing keys are the
/// caller's, so the message can say what recovery looks like for each.
fn read_conf(conf: &Path) -> anyhow::Result<Recorded> {
    let text = std::fs::read_to_string(conf).map_err(|_| {
        anyhow::anyhow!(
            "no install record at {} — this binary was not placed by install.sh (a plain \
             `cargo build` has nothing to update; `git pull && cargo build --release` is \
             that workflow's whole update story)",
            conf.display()
        )
    })?;
    let mut rec = Recorded {
        conf: conf.to_path_buf(),
        source: None,
        repo: None,
    };
    for line in text.lines() {
        if let Some(v) = line.strip_prefix(SOURCE_KEY) {
            let v = v.trim();
            if !v.is_empty() {
                rec.source = Some(PathBuf::from(v));
            }
        } else if let Some(v) = line.strip_prefix(REPO_KEY) {
            let v = v.trim();
            if !v.is_empty() {
                rec.repo = Some(v.to_string());
            }
        }
    }
    Ok(rec)
}

/// `git describe --tags --always --dirty`, or `(unknown)` — decoration, never a gate.
fn describe(dir: &Path) -> String {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["describe", "--tags", "--always", "--dirty"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(unknown)".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_record_names_the_cargo_workflow_instead() {
        let dir = tempfile::tempdir().expect("tempdir");
        let e = read_conf(&dir.path().join(CONF_NAME)).expect_err("must refuse");
        assert!(e.to_string().contains("cargo build --release"), "{e}");
    }

    #[test]
    fn the_recorded_source_is_read_and_must_still_exist() {
        let dir = tempfile::tempdir().expect("tempdir");
        let conf = dir.path().join(CONF_NAME);
        let src = dir.path().join("src");
        std::fs::write(
            &conf,
            format!(
                "# comment\nAPEXROUTER_INSTALL_SOURCE={}\nAPEXROUTER_INSTALL_REPO=https://example.invalid/Repo\n",
                src.display()
            ),
        )
        .expect("write");

        let rec = read_conf(&conf).expect("parse");
        let e = rec.source_dir().expect_err("dir is gone");
        assert!(e.to_string().contains("example.invalid"), "{e}");

        std::fs::create_dir(&src).expect("mkdir");
        let rec = read_conf(&conf).expect("parse");
        assert_eq!(rec.source_dir().expect("dir"), src);
    }

    #[test]
    fn a_record_without_a_source_line_is_a_named_refusal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let conf = dir.path().join(CONF_NAME);
        std::fs::write(&conf, "APEXROUTER_INSTALL_PREFIX=/nowhere\n").expect("write");
        let e = read_conf(&conf)
            .expect("parse")
            .source_dir()
            .expect_err("must refuse");
        assert!(e.to_string().contains("APEXROUTER_INSTALL_SOURCE"), "{e}");
    }

    #[test]
    fn describe_on_a_non_repo_is_decoration_not_a_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(describe(dir.path()), "(unknown)");
    }
}
