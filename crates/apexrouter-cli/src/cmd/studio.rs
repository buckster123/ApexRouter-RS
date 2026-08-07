//! OWNER: studio phase 7 (cli/src/cmd/studio.rs).
//!
//! `apexrouter studio up | down | status [--json]`
//!
//! The one verb (STUDIO.md S1): up resolves wake → converge → rent. Down parks only.
//! Money paths require `--yes` and a ceiling; status is free.

use crate::cli::StudioCmd;
use crate::cmd::Ctx;
use crate::daemon::Need;
use crate::render;
use apexrouter_protocol::{JobRecord, ServedBy, StudioStatus, StudioUpPath, StudioUpRequest};

/// Default hourly ceiling offered when the operator omits `--max-hourly`.
/// Still subject to the daemon-side hard ceiling.
const DEFAULT_MAX_HOURLY: f64 = 1.50;

/// Run `apexrouter studio …`.
///
/// # Errors
/// A daemon that will not answer, a money verb without `--yes`, or a control-plane refusal.
pub async fn run(ctx: &Ctx, cmd: &StudioCmd) -> anyhow::Result<()> {
    match cmd {
        StudioCmd::Status { json } => {
            let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
            let st: StudioStatus = client.get("/v1/studio").await?;
            if *json {
                return render::print_json(ServedBy::Daemon, render::now_unix(), false, &st);
            }
            print_status(&st);
            Ok(())
        }
        StudioCmd::Up {
            yes,
            max_hourly,
            recipe,
            machine_id,
            offer_id,
            json,
        } => {
            let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
            // Probe path first so we can refuse --yes-less with the right message.
            let preview: StudioStatus = client.get("/v1/studio").await?;
            if !*yes {
                anyhow::bail!("{}", up_needs_yes(&preview));
            }
            let body = StudioUpRequest {
                recipe_id: recipe
                    .as_deref()
                    .map(apexrouter_protocol::RecipeId::parse)
                    .transpose()
                    .map_err(|e| anyhow::anyhow!(e))?,
                confirm: true,
                max_usd_per_hour: max_hourly.unwrap_or(DEFAULT_MAX_HOURLY),
                machine_id: *machine_id,
                offer_id: *offer_id,
            };
            let raw: serde_json::Value = client
                .post("/v1/studio/up?source=cli", &body)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            if *json {
                return render::print_json(ServedBy::Daemon, render::now_unix(), false, &raw);
            }
            if let Ok(job) = serde_json::from_value::<JobRecord>(raw.clone()) {
                render::print_line(&format!(
                    "studio up started (job {} · {}) — follow with `apexrouter studio status`",
                    job.id, job.kind
                ));
            } else {
                render::print_line(&format!("studio up: {raw}"));
            }
            Ok(())
        }
        StudioCmd::Down { json } => {
            let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
            let raw: serde_json::Value = client
                .post("/v1/studio/down", &serde_json::json!({}))
                .await?;
            if *json {
                return render::print_json(ServedBy::Daemon, render::now_unix(), false, &raw);
            }
            let id = raw
                .get("instance_id")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let weekly = raw
                .get("weekly_disk_usd")
                .and_then(serde_json::Value::as_f64);
            render::print_line(&format!(
                "studio parked (instance {id}){}",
                weekly
                    .map(|w| format!(" — disk held at ~${w:.2}/week"))
                    .unwrap_or_default()
            ));
            render::print_line("wake with: apexrouter studio up --yes");
            Ok(())
        }
    }
}

fn up_needs_yes(st: &StudioStatus) -> String {
    match st.next_up_path {
        StudioUpPath::Wake => format!("{} — re-run with --yes [--max-hourly N]", st.summary),
        StudioUpPath::Rent => format!(
            "{} — re-run with --yes [--max-hourly N] (daemon ceiling still applies)",
            st.summary
        ),
        StudioUpPath::Converge => {
            // Converge is free of new billing but still goes through the daemon job.
            // Allow without --yes? S1 says one verb; converge does not spend. Still require
            // Mutate. We'll allow without --yes for converge only by re-checking on server
            // — actually our CLI always requires --yes for up for simplicity.
            format!("{} — re-run with --yes to converge tunnels", st.summary)
        }
    }
}

fn print_status(st: &StudioStatus) {
    render::print_line(&st.summary);
    render::print_line(&format!("next up path: {:?}", st.next_up_path));
    if let Some(studio) = &st.studio {
        render::print_line(&format!(
            "instance {}  machine {}  recipe {}",
            studio.instance_id,
            studio
                .machine_id
                .map(|m| m.to_string())
                .unwrap_or_else(|| "-".into()),
            studio
                .recipe_id
                .as_ref()
                .map(|r| r.as_str().to_string())
                .unwrap_or_else(|| "-".into())
        ));
    }
    if let Some(phase) = &st.instance_phase {
        render::print_line(&format!("phase: {phase:?}"));
    }
    if let Some(dph) = st.dph_total {
        render::print_line(&format!("dph: ${dph:.4}/hr"));
    }
    if !st.services.is_empty() {
        render::print_table(
            &[
                "SERVICE",
                "RUNTIME",
                "LOCAL",
                "REMOTE",
                "RESERVED_MB",
                "DESIRED",
            ],
            st.services
                .iter()
                .map(|s| {
                    vec![
                        s.name.clone(),
                        format!("{:?}", s.runtime),
                        s.local_port.to_string(),
                        s.remote_port.to_string(),
                        s.reserved_mb.to_string(),
                        format!("{:?}", s.desired),
                    ]
                })
                .collect(),
        );
    }
    if !st.tunnels.is_empty() {
        render::print_table(
            &["UP", "LOCAL", "REMOTE", "RESTARTS"],
            st.tunnels
                .iter()
                .map(|t| {
                    vec![
                        if t.up { "yes" } else { "no" }.into(),
                        t.spec.local_port.to_string(),
                        t.spec.remote_port.to_string(),
                        t.restarts.to_string(),
                    ]
                })
                .collect(),
        );
    }
    for ss in &st.service_status {
        render::print_line(&format!(
            "  probe {}: {:?}{}",
            ss.record.name,
            ss.liveness,
            ss.observed_vram_mb
                .map(|m| format!(" ({m} MiB)"))
                .unwrap_or_default()
        ));
    }
}
