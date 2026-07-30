//! OWNER: unit S-06 (cli/src/{main,cli,daemon,render}.rs, cli/src/cmd/{mod,status,serve,config,rig,models,fit,endpoint,route,switch,url,version,completions}.rs). Do not edit outside that unit.
//!
//! `apexrouter rig [--json]` — GPUs (free/total, who holds them), builds, RAM, swap.
//!
//! Free and total are printed **side by side and never subtracted**: a ROCm device on this
//! box reports free (12877 MiB) greater than total (11397 MiB) because of GTT accounting,
//! and a "used = total − free" column would underflow into a 4-billion-MiB lie.
//!
//! A build that will not run — `build-rocm` with a missing `libhipblas.so.3` — is listed
//! with no backends and no devices, because "installed and broken" is exactly what the
//! operator needs to see.

use crate::cli::RigArgs;
use crate::cmd::Ctx;
use crate::daemon::{Need, Serving};
use crate::render;
use apexrouter_protocol::{GpuBackend, RigSnapshot};

/// The `-dev` prefix a compute backend uses, lower-cased, with the open variant printed as
/// whatever llama.cpp called it rather than as `Other("…")`.
fn backend_name(b: &GpuBackend) -> String {
    match b {
        GpuBackend::Other(s) => s.to_lowercase(),
        other => render::variant(other),
    }
}

/// Run `apexrouter rig`.
///
/// # Errors
/// A scan failure, or a daemon that will not answer.
pub async fn run(ctx: &Ctx, args: &RigArgs) -> anyhow::Result<()> {
    let serving = ctx.serving(Need::ReadState).await?;
    let served_by = serving.served_by();
    let rig = load(ctx, &serving, args.rescan).await?;

    if args.json {
        return render::print_json(served_by, rig.scanned_at_unix, false, &rig);
    }
    if serving.is_offline() {
        render::print_offline_notice();
    }

    let rows = rig
        .gpus
        .iter()
        .map(|g| {
            vec![
                g.device.clone(),
                g.name.clone(),
                backend_name(&g.backend),
                render::human_mib(g.vram_free_mb),
                render::human_mib(g.vram_total_mb),
                render::human_mib(g.reserved_mb),
                if g.is_software {
                    "software".to_string()
                } else {
                    String::new()
                },
                g.seen_by_builds
                    .iter()
                    .map(|b| b.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
                g.held_by
                    .iter()
                    .map(|b| b.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
            ]
        })
        .collect();
    render::print_table(
        &[
            "DEVICE", "NAME", "BACKEND", "FREE", "TOTAL", "RESERVED", "KIND", "SEEN BY", "HELD BY",
        ],
        rows,
    );

    render::print_blank();
    let rows = rig
        .builds
        .iter()
        .map(|b| {
            vec![
                b.id.as_str().to_string(),
                b.build_info.clone().unwrap_or_default(),
                b.backends
                    .iter()
                    .map(backend_name)
                    .collect::<Vec<_>>()
                    .join(","),
                b.devices.join(","),
                b.flags.help_lines.to_string(),
                b.server_path.clone(),
            ]
        })
        .collect();
    render::print_table(
        &["BUILD", "VERSION", "BACKENDS", "DEVICES", "HELP", "PATH"],
        rows,
    );

    render::print_blank();
    render::print_line(&format!(
        "host  ram {} free of {}  ·  swap {} used of {}  ·  {} cpu threads",
        render::human_mib(rig.ram_free_mb),
        render::human_mib(rig.ram_total_mb),
        render::human_mib(rig.swap_used_mb),
        render::human_mib(rig.swap_total_mb),
        rig.cpu_threads
    ));
    Ok(())
}

/// The rig, from the daemon when there is one and from a live scan when there is not.
///
/// # Errors
/// A scan failure, or a daemon that will not answer.
pub async fn load(ctx: &Ctx, serving: &Serving, rescan: bool) -> anyhow::Result<RigSnapshot> {
    match serving {
        Serving::Daemon(c) if rescan => Ok(c
            .post::<serde_json::Value, RigSnapshot>("/v1/rig/rescan", &serde_json::json!({}))
            .await?),
        Serving::Daemon(c) => Ok(c.get::<RigSnapshot>("/v1/rig").await?),
        // No daemon: scan here and now. Discovery is a `--list-devices` probe per build,
        // which is exactly what the daemon would have done.
        _ => Ok(apexrouter_providers::local::supervisor::scan_rig(
            &ctx.cfg.endpoints,
            ctx.paths.cache(),
        )
        .await?),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_protocol::{Gpu, GpuBackend};

    fn gpu(free: u64, total: u64) -> Gpu {
        Gpu {
            device: "ROCm0".to_string(),
            index: 0,
            name: "AMD Radeon".to_string(),
            backend: GpuBackend::Rocm,
            vram_total_mb: total,
            vram_free_mb: free,
            driver: None,
            is_software: false,
            seen_by_builds: Vec::new(),
            held_by: Vec::new(),
            reserved_mb: 0,
        }
    }

    /// The GTT-accounting trap from the ground-truth corrections: free > total is real on
    /// this machine, and nothing in the render path may subtract them.
    #[test]
    fn free_greater_than_total_never_underflows_a_used_column() {
        let g = gpu(12_877, 11_397);
        // What the table prints, verbatim.
        let free = render::human_mib(g.vram_free_mb);
        let total = render::human_mib(g.vram_total_mb);
        assert_eq!(free, "12.6 GiB");
        assert_eq!(total, "11.1 GiB");
        // And the guard that matters: any "used" figure is saturating, never wrapping.
        assert_eq!(g.vram_total_mb.saturating_sub(g.vram_free_mb), 0);
    }
}
