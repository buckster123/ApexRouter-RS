//! OWNER: unit S-06 (cli/src/{main,cli,daemon,render}.rs, cli/src/cmd/{mod,status,serve,config,rig,models,fit,endpoint,route,switch,url,version,completions}.rs). Do not edit outside that unit.
//!
//! `apexrouter serve` — the daemon entrypoint. `--foreground`, `--stop`, `--no-ui`,
//! `--allow-remote --token-env VAR`.
//!
//! Foreground is the default, because that is what a systemd unit and a `cargo run` both
//! want; `--detach` is the explicit "put it in the background", and it is the same code path
//! a `Mutate` verb's autostart takes.
//!
//! Startup order is not negotiable (ARCHITECTURE §1.3): resolve paths, **take the lock**,
//! load config, then hand off to `apexrouter_server::serve`, which reconciles before it
//! binds. Taking the lock *here* is what makes "apexrouter is already running: pid N owns …"
//! the first thing a second starter sees, rather than an `EADDRINUSE` two seconds later.

use crate::cli::ServeArgs;
use crate::cmd::Ctx;
use crate::daemon;
use crate::render;
use apexrouter_core::config::Config;
use apexrouter_core::lockfile::DaemonLock;
use apexrouter_core::proc::{self, Signal};
use apexrouter_protocol::ProcFacts;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

/// How long `--stop` waits for the daemon to release its lock after `SIGTERM`. Longer than
/// the default `drain_timeout_secs` (30), because a drain that is still finishing a stream
/// is a daemon doing its job.
const STOP_DEADLINE: Duration = Duration::from_secs(45);

/// Run `apexrouter serve`.
///
/// # Errors
/// A lock held by another process, a bind that would be non-loopback without a token, or
/// whatever the server itself returns.
pub async fn run(ctx: &Ctx, args: &ServeArgs) -> anyhow::Result<()> {
    if args.stop {
        return stop(ctx).await;
    }
    let cfg = overlay(ctx.cfg.clone(), args)?;
    if args.detach {
        return detach(ctx).await;
    }
    foreground(ctx, cfg).await
}

/// Fold the command-line overrides onto the loaded config, and refuse the one combination
/// that is a security hole.
///
/// # Errors
/// A non-loopback bind with no token configured (ARCHITECTURE §9.1). The message names the
/// variable to set, because "refuses to start" without the fix is just an obstacle.
fn overlay(mut cfg: Config, args: &ServeArgs) -> anyhow::Result<Config> {
    if let Some(b) = &args.proxy_bind {
        cfg.server.proxy_bind = b.clone();
    }
    if let Some(b) = &args.control_bind {
        cfg.server.control_bind = b.clone();
    }
    if let Some(v) = &args.token_env {
        cfg.server.token_env = v.clone();
    }
    if args.no_ui {
        // `""` means "the embedded ui-web"; a sentinel path means "serve nothing", and the
        // server treats a missing directory as no UI.
        cfg.server.ui_dir = "/nonexistent/apexrouter-ui-disabled".to_string();
    }

    let token = std::env::var(&cfg.server.token_env)
        .ok()
        .filter(|v| !v.is_empty());
    for (what, addr) in [
        ("proxy_bind", cfg.proxy_bind()),
        ("control_bind", cfg.control_bind()),
    ] {
        if is_remote(&addr) {
            if !args.allow_remote {
                anyhow::bail!(
                    "{what} = {addr} is not loopback; pass --allow-remote to mean it, and set a \
                     token in ${}",
                    cfg.server.token_env
                );
            }
            if token.is_none() {
                anyhow::bail!(
                    "{what} = {addr} is not loopback and ${} is unset — a LAN-visible \
                     ApexRouter without a bearer token is an open proxy. Set it, or bind \
                     127.0.0.1.",
                    cfg.server.token_env
                );
            }
        }
    }
    Ok(cfg)
}

/// Is this bind reachable from off the machine?
fn is_remote(addr: &SocketAddr) -> bool {
    !addr.ip().is_loopback()
}

/// Become the daemon.
///
/// # Errors
/// A lock held by another process, or whatever `apexrouter_server::serve` returns.
async fn foreground(ctx: &Ctx, cfg: Config) -> anyhow::Result<()> {
    ctx.paths.ensure_layout()?;
    // Step 2 of the documented startup order, before anything binds: whoever holds this
    // lock is the daemon, and the failure names them.
    let lock = DaemonLock::acquire(&ctx.paths)?;
    tracing::info!(
        proxy = %cfg.proxy_bind(),
        control = %cfg.control_bind(),
        "apexrouterd starting"
    );
    apexrouter_server::serve(ctx.paths.clone(), cfg, lock).await
}

/// Start a daemon in the background and wait until it answers.
///
/// # Errors
/// A spawn failure, or a daemon that never comes up — the message points at its log.
async fn detach(ctx: &Ctx) -> anyhow::Result<()> {
    ctx.paths.ensure_layout()?;
    if let Some(rec) = daemon::owner_record(&ctx.paths, Duration::ZERO)? {
        anyhow::bail!(
            "apexrouter is already running: pid {} owns {} (control {})",
            rec.pid,
            ctx.paths.daemon_lock().display(),
            rec.control_url
        );
    }
    let paths = ctx.paths.clone();
    let spawn = move || daemon::spawn_daemon(&paths);
    let rec = daemon::ensure_owner(
        &ctx.paths,
        &spawn,
        Duration::from_secs(10),
        Duration::from_millis(75),
    )
    .await?;
    let client = daemon::client_for(&rec, &ctx.cfg);
    daemon::wait_healthy(&client, Duration::from_secs(10)).await?;
    render::print_line(&format!(
        "apexrouterd is up: pid {}, proxy {}, control {}",
        rec.pid, rec.proxy_url, rec.control_url
    ));
    Ok(())
}

/// Stop the running daemon and wait for it to let go of the lock.
///
/// `SIGTERM` rather than `POST /v1/shutdown`: the signal is the same graceful path
/// (ARCHITECTURE §1.6 — drain, then exit), it needs no token, and it works when the control
/// listener is wedged. Identity is verified from the owner record before anything is
/// signalled, so a reused pid is never killed.
///
/// # Errors
/// When no daemon is running, or it does not exit within [`STOP_DEADLINE`].
async fn stop(ctx: &Ctx) -> anyhow::Result<()> {
    let Some(rec) = daemon::owner_record(&ctx.paths, Duration::from_millis(500))? else {
        anyhow::bail!("apexrouterd is not running");
    };
    let facts = ProcFacts {
        pid: rec.pid,
        start_time_ticks: rec.start_time_ticks,
        boot_id: rec.boot_id.clone(),
        // Advisory only, and unknown from a lock file: `liveness` compares the boot id and
        // the start ticks, which is what makes this signal safe.
        exe: String::new(),
        cmdline_sha256: String::new(),
    };
    proc::signal_verified(&facts, Signal::Term)?;
    render::print_line(&format!("sent SIGTERM to pid {}, draining…", rec.pid));

    let until = Instant::now() + STOP_DEADLINE;
    loop {
        if daemon::owner_record(&ctx.paths, Duration::ZERO)?.is_none() {
            render::print_line("apexrouterd stopped");
            return Ok(());
        }
        if Instant::now() >= until {
            anyhow::bail!(
                "pid {} still holds {} after {}s — it is draining, or wedged; \
                 `kill -9 {}` is the last resort",
                rec.pid,
                ctx.paths.daemon_lock().display(),
                STOP_DEADLINE.as_secs(),
                rec.pid
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ServeArgs;

    fn args() -> ServeArgs {
        ServeArgs::default()
    }

    #[test]
    fn a_loopback_bind_needs_nothing() {
        let cfg = overlay(Config::default(), &args()).expect("loopback is fine");
        assert_eq!(cfg.proxy_bind().to_string(), "127.0.0.1:8888");
    }

    #[test]
    fn a_non_loopback_bind_without_allow_remote_refuses_to_start() {
        let mut a = args();
        a.proxy_bind = Some("0.0.0.0:8888".to_string());
        let e = overlay(Config::default(), &a).expect_err("must refuse");
        assert!(e.to_string().contains("--allow-remote"), "{e}");
    }

    #[test]
    fn a_non_loopback_bind_without_a_token_refuses_and_names_the_variable() {
        let _guard = crate::daemon::testenv::lock();
        std::env::remove_var("APEXROUTER_TOKEN");
        let mut a = args();
        a.control_bind = Some("192.168.1.9:2739".to_string());
        a.allow_remote = true;
        let e = overlay(Config::default(), &a).expect_err("must refuse");
        let msg = e.to_string();
        assert!(msg.contains("APEXROUTER_TOKEN"), "{msg}");
        assert!(msg.contains("open proxy"), "{msg}");
    }

    #[test]
    fn a_non_loopback_bind_with_a_token_is_allowed() {
        let _guard = crate::daemon::testenv::lock();
        std::env::set_var("APEXROUTER_TOKEN", "s3kr1t");
        let mut a = args();
        a.control_bind = Some("192.168.1.9:2739".to_string());
        a.allow_remote = true;
        assert!(overlay(Config::default(), &a).is_ok());
        std::env::remove_var("APEXROUTER_TOKEN");
    }

    #[test]
    fn overrides_land_where_they_are_read_from() {
        let mut a = args();
        a.proxy_bind = Some("127.0.0.1:9999".to_string());
        a.control_bind = Some("127.0.0.1:9998".to_string());
        a.no_ui = true;
        let cfg = overlay(Config::default(), &a).expect("cfg");
        assert_eq!(cfg.proxy_bind().port(), 9999);
        assert_eq!(cfg.control_bind().port(), 9998);
        assert!(
            !cfg.server.ui_dir.is_empty(),
            "--no-ui must not mean 'embedded'"
        );
    }

    #[test]
    fn loopback_detection_matches_the_security_posture() {
        assert!(!is_remote(&"127.0.0.1:1".parse().expect("addr")));
        assert!(!is_remote(&"[::1]:1".parse().expect("addr")));
        assert!(is_remote(&"0.0.0.0:1".parse().expect("addr")));
        assert!(is_remote(&"192.168.1.9:1".parse().expect("addr")));
    }
}
