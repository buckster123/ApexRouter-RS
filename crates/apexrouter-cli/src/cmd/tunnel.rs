//! OWNER: unit S-08 (cli/src/cmd/{vast,tunnel,hf,provider,recipe,profile,usage,smoke,doctor,compare,backend,swap,up,approvals,token,open,env,migrate}.rs). Do not edit outside that unit.
//!
//! `apexrouter tunnel up <instance-id> | down [<id>] | status [--json]`.
//!
//! A tunnel is **supervised**, not spawned and forgotten (`ARCHITECTURE.md` §4.9): the
//! daemon owns the `ssh` child, records its `ProcFacts` in `$STATE/tunnels.json`, re-adopts
//! it on restart, and reconnects with bounded backoff when a wifi blip takes the link down.
//! That is why `up` and `down` are daemon RPCs and never an `ssh` this process spawns — a
//! CLI that exits would take the tunnel with it, and a `$3.34/hr` box would go unreachable.
//!
//! `status` reads `$STATE/tunnels.json` when no daemon is running, because a record of a
//! tunnel that *was* up is exactly what you want when nothing is.
//!
//! Local ports come from a pool starting at 8800: multiple rentals is the normal case.

use crate::cli::TunnelCmd;
use crate::cmd::Ctx;
use crate::daemon::{Need, Serving};
use crate::render;
use apexrouter_protocol::{ServedBy, TunnelStatus};

/// Run `apexrouter tunnel …`.
///
/// # Errors
/// A `$STATE` read failure, a daemon that will not answer, or an unknown instance.
pub async fn run(ctx: &Ctx, cmd: &TunnelCmd) -> anyhow::Result<()> {
    match cmd {
        TunnelCmd::Status(args) => {
            let serving = ctx.serving(Need::ReadState).await?;
            let tunnels = load(ctx, &serving).await?;
            if args.json {
                return render::print_json(
                    serving.served_by(),
                    render::now_unix(),
                    serving.is_offline(),
                    &tunnels,
                );
            }
            if serving.is_offline() {
                render::print_offline_notice();
            }
            render::print_table(
                &[
                    "INSTANCE", "UP", "LOCAL", "REMOTE", "SSH", "PID", "RESTARTS", "AGE", "ERROR",
                ],
                tunnels.iter().map(row).collect(),
            );
            if tunnels.is_empty() {
                render::print_line("(no tunnels)");
            }
            Ok(())
        }
        TunnelCmd::Up { instance_id, json } => {
            let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
            let t: TunnelStatus = client
                .post(
                    &format!("/v1/vast/instances/{instance_id}/tunnel"),
                    &serde_json::json!({}),
                )
                .await?;
            if *json {
                return render::print_json(ServedBy::Daemon, render::now_unix(), false, &t);
            }
            render::print_line(&local_url(&t));
            Ok(())
        }
        TunnelCmd::Down { id } => {
            let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
            let ids = match id {
                Some(one) => vec![*one],
                // `down` with no id closes them all, and the list comes from the daemon so a
                // stale `$STATE` copy cannot make this a no-op that claims success.
                None => client
                    .get::<Vec<TunnelStatus>>("/v1/tunnels")
                    .await?
                    .iter()
                    .map(|t| t.spec.instance_id.0)
                    .collect(),
            };
            if ids.is_empty() {
                render::print_line("(no tunnels to close)");
                return Ok(());
            }
            for id in ids {
                client
                    .delete(&format!("/v1/vast/instances/{id}/tunnel"))
                    .await?;
                render::print_line(&format!("closed the tunnel to instance {id}"));
            }
            Ok(())
        }
    }
}

/// Every tunnel: from the daemon when there is one, from `$STATE/tunnels.json` when not.
///
/// # Errors
/// A `$STATE` read failure, or a daemon that will not answer.
pub async fn load(ctx: &Ctx, serving: &Serving) -> anyhow::Result<Vec<TunnelStatus>> {
    match serving {
        Serving::Daemon(c) => Ok(c.get::<Vec<TunnelStatus>>("/v1/tunnels").await?),
        Serving::Offline(store) => Ok(store.with_state_lock_shared(|| store.load_tunnels())?),
        Serving::None(_) => {
            Ok(apexrouter_core::store::Store::new(ctx.paths.clone()).load_tunnels()?)
        }
    }
}

/// The URL a client should use for a tunnelled instance — always loopback, which is the
/// whole point of the tunnel-only default posture (§9.5).
pub fn local_url(t: &TunnelStatus) -> String {
    format!("http://127.0.0.1:{}", t.spec.local_port)
}

/// One row of the tunnel table.
fn row(t: &TunnelStatus) -> Vec<String> {
    vec![
        t.spec.instance_id.0.to_string(),
        if t.up { "yes" } else { "no" }.to_string(),
        t.spec.local_port.to_string(),
        t.spec.remote_port.to_string(),
        format!("{}:{}", t.spec.ssh_host, t.spec.ssh_port),
        render::dash(t.proc.as_ref().map(|p| p.pid)),
        t.restarts.to_string(),
        t.since_unix
            .map(|s| render::human_secs(render::now_unix() - s))
            .unwrap_or_default(),
        t.last_error.clone().unwrap_or_default(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_protocol::{InstanceId, TunnelSpec};

    fn tunnel(up: bool) -> TunnelStatus {
        TunnelStatus {
            spec: TunnelSpec {
                instance_id: InstanceId(12345),
                local_port: 8800,
                remote_port: 8000,
                ssh_host: "root@ssh5.vast.ai".to_string(),
                ssh_port: 41234,
            },
            up,
            proc: None,
            since_unix: None,
            restarts: 2,
            last_error: None,
        }
    }

    #[test]
    fn the_client_url_is_always_loopback() {
        assert_eq!(local_url(&tunnel(true)), "http://127.0.0.1:8800");
    }

    #[test]
    fn a_down_tunnel_still_renders_its_whole_record() {
        let r = row(&tunnel(false));
        assert_eq!(r[0], "12345");
        assert_eq!(r[1], "no");
        assert_eq!(r[2], "8800");
        assert_eq!(r[4], "root@ssh5.vast.ai:41234");
        assert_eq!(r[6], "2", "restart count is how a flapping link is spotted");
    }
}
