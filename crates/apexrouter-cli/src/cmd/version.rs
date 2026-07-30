//! OWNER: unit S-06 (cli/src/{main,cli,daemon,render}.rs, cli/src/cmd/{mod,status,serve,config,rig,models,fit,endpoint,route,switch,url,version,completions}.rs). Do not edit outside that unit.
//!
//! `apexrouter version`.
//!
//! It also reports the **running daemon's** version, read straight from the lock file's
//! owner record — no HTTP, no daemon required. "The CLI was upgraded and the daemon was not"
//! is the single most confusing state this product can be in, and this is where it shows.

use crate::cli::JsonFlag;
use crate::cmd::Ctx;
use crate::daemon;
use crate::render;
use apexrouter_protocol::ServedBy;
use serde::Serialize;
use std::time::Duration;

/// What `version` reports.
#[derive(Debug, Serialize)]
struct VersionInfo {
    /// Always `"apexrouter"`.
    product: &'static str,
    /// This binary's version.
    version: &'static str,
    /// The running daemon, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    daemon: Option<DaemonInfo>,
}

/// The daemon half, from the owner record.
#[derive(Debug, Serialize)]
struct DaemonInfo {
    /// The version it is running.
    version: String,
    /// Its pid.
    pid: u32,
    /// Where the data plane is.
    proxy_url: String,
    /// Where the control plane is.
    control_url: String,
    /// How long it has been up.
    uptime_secs: i64,
    /// True when the daemon is not this binary's version.
    mismatched: bool,
}

/// Run `apexrouter version`.
///
/// # Errors
/// A lock-file failure that is not "no daemon".
pub async fn run(ctx: &Ctx, args: &JsonFlag) -> anyhow::Result<()> {
    let daemon = daemon::owner_record(&ctx.paths, Duration::ZERO)?.map(|rec| DaemonInfo {
        mismatched: rec.version != apexrouter_protocol::VERSION,
        version: rec.version,
        pid: rec.pid,
        proxy_url: rec.proxy_url,
        control_url: rec.control_url,
        uptime_secs: (render::now_unix() - rec.started_at_unix).max(0),
    });
    let info = VersionInfo {
        product: apexrouter_protocol::PRODUCT,
        version: apexrouter_protocol::VERSION,
        daemon,
    };

    if args.json {
        return render::print_json(
            if info.daemon.is_some() {
                ServedBy::Daemon
            } else {
                ServedBy::Offline
            },
            render::now_unix(),
            false,
            &info,
        );
    }
    render::print_line(&format!("{} {}", info.product, info.version));
    match &info.daemon {
        Some(d) => {
            render::print_line(&format!(
                "daemon {} (pid {}, up {}) at {}",
                d.version,
                d.pid,
                render::human_secs(d.uptime_secs),
                d.control_url
            ));
            if d.mismatched {
                render::print_line(&format!(
                    "note: the running daemon is {} and this binary is {} — restart it with \
                     `apexrouter serve --stop && apexrouter serve --detach`",
                    d.version,
                    apexrouter_protocol::VERSION
                ));
            }
        }
        None => render::print_line("daemon not running"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_payload_names_the_product_and_omits_an_absent_daemon() {
        let info = VersionInfo {
            product: apexrouter_protocol::PRODUCT,
            version: apexrouter_protocol::VERSION,
            daemon: None,
        };
        let v = serde_json::to_value(&info).expect("json");
        assert_eq!(v["product"], serde_json::Value::from("apexrouter"));
        assert_eq!(
            v["version"],
            serde_json::Value::from(apexrouter_protocol::VERSION)
        );
        assert!(v.get("daemon").is_none(), "no daemon means no daemon key");
    }

    #[test]
    fn a_version_mismatch_is_a_field_not_a_guess() {
        let d = DaemonInfo {
            version: "0.0.9".to_string(),
            pid: 1,
            proxy_url: "http://127.0.0.1:8888".to_string(),
            control_url: "http://127.0.0.1:2739".to_string(),
            uptime_secs: 10,
            mismatched: "0.0.9" != apexrouter_protocol::VERSION,
        };
        assert!(d.mismatched);
    }
}
