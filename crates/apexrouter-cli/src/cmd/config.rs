//! OWNER: unit S-06 (cli/src/{main,cli,daemon,render}.rs, cli/src/cmd/{mod,status,serve,config,rig,models,fit,endpoint,route,switch,url,version,completions}.rs). Do not edit outside that unit.
//!
//! `apexrouter config init | show [--json] | path | edit`. Class `Pure`.
//!
//! `init` and `edit` are offline **writers**: they take `apexrouterd.lock` exclusively,
//! which *proves* no daemon is running rather than asking one politely. `show` and `path`
//! read nothing but the config itself.
//!
//! `show --json` prints the JSON envelope and **nothing else**; `show` without it prints
//! TOML, which is the format the file is actually in.

use crate::cli::ConfigCmd;
use crate::cmd::Ctx;
use crate::render;
use apexrouter_core::config::Config;
use apexrouter_core::lockfile;
use apexrouter_protocol::ServedBy;
use std::process::Command;

/// Run `apexrouter config …`.
///
/// # Errors
/// A daemon holding the lock, an existing file without `--force`, or an editor that fails.
pub fn run(ctx: &Ctx, cmd: &ConfigCmd) -> anyhow::Result<()> {
    match cmd {
        ConfigCmd::Init { force } => {
            ctx.paths.ensure_layout()?;
            // Proof, not courtesy: if a daemon holds this, writing the config underneath it
            // would be a change nobody has loaded.
            let _lock = lockfile::acquire_offline_exclusive(&ctx.paths)?;
            let path = Config::init_file(&ctx.paths, *force)?;
            render::print_line(&format!("wrote {}", path.display()));
            Ok(())
        }
        ConfigCmd::Show(args) => {
            if args.json {
                return render::print_json(
                    ServedBy::Offline,
                    render::now_unix(),
                    false,
                    &ctx.cfg.serializable(),
                );
            }
            // The CLI links no TOML serializer — `toml` is not one of its dependencies, and
            // adding one to print a config the daemon already serialises would be a second
            // rendering of the same document. Humans get the effective values as JSON with
            // the path to edit; `--json` gets the envelope and nothing else.
            render::print_line(&format!(
                "# effective configuration — edit {}",
                ctx.paths.config_file().display()
            ));
            render::print_line(&serde_json::to_string_pretty(&ctx.cfg.serializable())?);
            Ok(())
        }
        ConfigCmd::Path(args) => {
            let path = ctx.paths.config_file();
            if args.json {
                return render::print_json(
                    ServedBy::Offline,
                    render::now_unix(),
                    false,
                    &serde_json::json!({
                        "config": path.display().to_string(),
                        "exists": path.exists(),
                        "state": ctx.paths.state().display().to_string(),
                        "cache": ctx.paths.cache().display().to_string(),
                    }),
                );
            }
            render::print_line(&path.display().to_string());
            Ok(())
        }
        ConfigCmd::Edit => {
            ctx.paths.ensure_layout()?;
            let _lock = lockfile::acquire_offline_exclusive(&ctx.paths)?;
            let path = ctx.paths.config_file();
            if !path.exists() {
                Config::init_file(&ctx.paths, false)?;
            }
            let editor = editor();
            // An argv vector, never a shell string: a config path with a space in it must
            // not become two arguments, and house rule 5 forbids `sh -c` outright.
            let status = Command::new(&editor).arg(&path).status().map_err(|e| {
                anyhow::anyhow!("could not run `{editor}` (set $VISUAL or $EDITOR): {e}")
            })?;
            if !status.success() {
                anyhow::bail!("{editor} exited with {status}");
            }
            // Re-read, so a syntax error is reported now rather than at the next start.
            Config::load()?;
            render::print_line(&format!("{} parses", path.display()));
            Ok(())
        }
    }
}

/// `$VISUAL`, then `$EDITOR`, then `vi` — the POSIX order.
fn editor() -> String {
    for key in ["VISUAL", "EDITOR"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return v;
            }
        }
    }
    "vi".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, JsonFlag};
    use crate::daemon::testenv;
    use clap::Parser;

    fn ctx_at(dir: &std::path::Path) -> Ctx {
        std::env::set_var("APEXROUTER_HOME", dir);
        std::env::remove_var("APEXROUTER_CONFIG");
        let cli = Cli::parse_from(["apexrouter", "config", "path"]);
        Ctx::load(&cli).expect("ctx")
    }

    #[test]
    fn init_writes_the_example_and_refuses_to_clobber_it() {
        let _guard = testenv::lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ctx_at(dir.path());

        run(&ctx, &ConfigCmd::Init { force: false }).expect("init");
        let path = ctx.paths.config_file();
        assert!(path.exists());
        let first = std::fs::read_to_string(&path).expect("read");
        assert!(first.contains("[server]"), "the example config was written");

        let e = run(&ctx, &ConfigCmd::Init { force: false }).expect_err("must refuse");
        assert!(e.to_string().contains("--force"), "{e}");

        run(&ctx, &ConfigCmd::Init { force: true }).expect("forced init");
        std::env::remove_var("APEXROUTER_HOME");
    }

    #[test]
    fn init_refuses_while_a_daemon_holds_the_lock() {
        let _guard = testenv::lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ctx_at(dir.path());
        ctx.paths.ensure_layout().expect("layout");
        let _held = apexrouter_core::lockfile::DaemonLock::acquire(&ctx.paths).expect("lock");

        let e = run(&ctx, &ConfigCmd::Init { force: false }).expect_err("must refuse");
        assert!(e.to_string().contains("daemon stopped"), "{e}");
        std::env::remove_var("APEXROUTER_HOME");
    }

    /// The acceptance case: `config show --json` puts the envelope on stdout and nothing
    /// else. Asserted on the value, since capturing the process's stdout in-process is not
    /// something a unit test can do honestly.
    #[test]
    fn show_json_is_exactly_the_envelope() {
        let _guard = testenv::lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ctx_at(dir.path());

        let v = render::envelope_value(
            ServedBy::Offline,
            render::now_unix(),
            false,
            &ctx.cfg.serializable(),
        )
        .expect("envelope");
        assert_eq!(v["served_by"], serde_json::Value::from("offline"));
        assert!(v.get("server").is_some(), "the config is flattened in: {v}");
        assert!(v["server"]["proxy_bind"].is_string());

        // Both renderings must succeed on a default config.
        run(&ctx, &ConfigCmd::Show(JsonFlag { json: true })).expect("json");
        run(&ctx, &ConfigCmd::Show(JsonFlag { json: false })).expect("toml");
        run(&ctx, &ConfigCmd::Path(JsonFlag { json: true })).expect("path json");
        std::env::remove_var("APEXROUTER_HOME");
    }

    #[test]
    fn the_editor_follows_the_posix_order() {
        let _guard = testenv::lock();
        std::env::remove_var("VISUAL");
        std::env::remove_var("EDITOR");
        assert_eq!(editor(), "vi");
        std::env::set_var("EDITOR", "nano");
        assert_eq!(editor(), "nano");
        std::env::set_var("VISUAL", "hx");
        assert_eq!(editor(), "hx");
        std::env::remove_var("VISUAL");
        std::env::remove_var("EDITOR");
    }
}
