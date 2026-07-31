//! OWNER: unit S-06 (cli/src/{main,cli,daemon,render}.rs, cli/src/cmd/{mod,status,serve,config,rig,models,fit,endpoint,route,switch,url,version,completions}.rs). Do not edit outside that unit.
//!
//! `apexrouter fit <model> …`. Class `Pure`: it runs with no daemon, because `fit()` is a
//! pure function.
//!
//! "Pure" is about the *solver*, not about the inputs: the budget is computed **live**
//! (a `--list-devices` probe plus the endpoint records already on disk), because a cached
//! free-VRAM number is the one input that is always wrong by the time it matters.
//!
//! `why` is printed in full. A number nobody can explain is a number nobody should trust.

use crate::cli::{split_list, FitArgs};
use crate::cmd::{models, Ctx};
use crate::daemon::{Need, Serving};
use crate::render;
use apexrouter_core::fit as solver;
use apexrouter_protocol::{
    FitInput, FitPlan, FitVerdict, LocalModel, NglPlan, ServedBy, SplitMode, SplitPlan,
};

/// Run `apexrouter fit`.
///
/// # Errors
/// An unresolvable model, an unreadable GGUF header, or a discovery failure.
pub async fn run(ctx: &Ctx, args: &FitArgs) -> anyhow::Result<()> {
    let serving = ctx.serving(Need::Pure).await?;
    let models = models::load(ctx, &serving).await?;
    let model = models::resolve_model(&models, &args.model)?;

    let rig =
        apexrouter_providers::local::supervisor::scan_rig(&ctx.cfg.endpoints, ctx.paths.cache())
            .await?;
    let devices = match args.devices.as_deref().map(split_list) {
        Some(d) if !d.is_empty() => d,
        _ => default_devices(&rig),
    };
    let running = match &serving {
        Serving::Offline(store) => store.list_endpoints().unwrap_or_default(),
        _ => Vec::new(),
    };
    // Scope: the build `endpoint start` would pick. `--devices` narrows within that
    // backend; it cannot widen across backends, because one process uses one backend.
    let budget = solver::budget_from_rig(
        &rig,
        solver::BackendScope::Auto,
        &devices,
        ctx.cfg.endpoints.vram_margin_mb,
        &running,
    );

    let gguf = model.gguf.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "the GGUF header of {} could not be read, so there is nothing to solve — \
             fit needs n_layer, n_head_kv and n_embd_head_k/v",
            model.name
        )
    })?;

    let split = SplitPlan {
        devices: if devices.is_empty() {
            budget.device_names()
        } else {
            devices
        },
        mode: args.split_mode.map(Into::into).unwrap_or(SplitMode::Layer),
        main_gpu: args.main_gpu,
        tensor_split: args.tensor_split.as_deref().map(parse_ratios).transpose()?,
    };

    let plan = solver::fit(&FitInput {
        weights_bytes: model.total_bytes,
        gguf,
        budget,
        want_ctx: args.ctx,
        want_parallel: args.parallel,
        want_kv: args.kv.map(Into::into),
        split,
        batch: args.batch,
    });

    if args.json {
        return render::print_json(ServedBy::Offline, render::now_unix(), false, &plan);
    }
    print_plan(&model, &plan);
    Ok(())
}

/// Which devices an unqualified `fit` may spend.
///
/// **One `llama-server` process uses exactly one build**, so only that build's devices are
/// spendable. This matters on the machine in `docs/port/00-machine-ground-truth.md`: the
/// single 840M appears as `ROCm0` to `build` and as `Vulkan0` to `build-vulkan`, and letting
/// the budget sum both would invent 20 GiB of VRAM that does not exist. Picking the build
/// `endpoint start` would pick keeps the answer and the launch in agreement.
///
/// An empty result is the honest CPU-only answer — the solver then judges against host RAM.
fn default_devices(rig: &apexrouter_protocol::RigSnapshot) -> Vec<String> {
    let chosen = apexrouter_core::discover::choose_build(&rig.builds, None)
        .and_then(|c| rig.builds.iter().find(|b| b.id == c.chosen));
    if let Some(b) = chosen {
        if !b.devices.is_empty() {
            return b.devices.clone();
        }
    }
    rig.builds
        .iter()
        .find(|b| !b.devices.is_empty())
        .map(|b| b.devices.clone())
        .unwrap_or_default()
}

/// `--tensor-split 3,1` into the ratio vector.
///
/// # Errors
/// A component that is not a number.
fn parse_ratios(v: &str) -> anyhow::Result<Vec<f32>> {
    split_list(v)
        .iter()
        .map(|s| {
            s.parse::<f32>()
                .map_err(|e| anyhow::anyhow!("--tensor-split component `{s}` is not a number: {e}"))
        })
        .collect()
}

/// The human rendering of a plan, `why` included.
pub fn print_plan(model: &LocalModel, plan: &FitPlan) {
    render::print_line(&format!(
        "{}  ->  {}",
        model.name,
        verdict_line(&plan.verdict)
    ));
    render::print_line(&format!(
        "  ctx      {} total across {} slot(s), kv {}",
        plan.ctx,
        plan.parallel,
        plan.kv_type.as_flag()
    ));
    render::print_line(&format!("  ngl      {}", ngl_line(&plan.ngl)));
    render::print_line(&format!(
        "  memory   weights {} · kv {} · compute {} · headroom {} MiB",
        render::human_mib(plan.weights_mb),
        render::human_mib(plan.kv_mb),
        render::human_mib(plan.compute_mb),
        plan.headroom_mb
    ));
    if !plan.split.devices.is_empty() {
        render::print_line(&format!("  devices  {}", plan.split.devices.join(", ")));
    }
    for (dev, mb) in &plan.per_device_mb {
        render::print_line(&format!("           {dev}: {}", render::human_mib(*mb)));
    }
    for line in &plan.why {
        render::print_line(&format!("  why      {line}"));
    }
}

/// The one-line verdict, with its number.
fn verdict_line(v: &FitVerdict) -> String {
    match v {
        FitVerdict::Fits { headroom_mb } => {
            format!("fits, {} spare", render::human_mib(*headroom_mb))
        }
        FitVerdict::Tight { headroom_mb } => {
            format!("tight, only {} spare", render::human_mib(*headroom_mb))
        }
        FitVerdict::NeedsOffload { layers_on_gpu } => {
            format!("needs offload, {layers_on_gpu} layer(s) on the GPU")
        }
        FitVerdict::WontFit { short_by_mb } => {
            format!("will not fit, short by {}", render::human_mib(*short_by_mb))
        }
    }
}

/// How the layer-offload policy renders.
fn ngl_line(n: &NglPlan) -> String {
    match n {
        NglPlan::Auto => "auto (no -ngl emitted; llama.cpp sizes it)".to_string(),
        NglPlan::All => "all (-ngl 999)".to_string(),
        NglPlan::Layers(n) => format!("{n} layers"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratios_parse_and_bad_ones_say_which_component() {
        assert_eq!(parse_ratios("3,1").expect("ratios"), vec![3.0, 1.0]);
        let e = parse_ratios("3,x").expect_err("must fail");
        assert!(e.to_string().contains('x'), "{e}");
    }

    #[test]
    fn every_verdict_renders_with_its_number() {
        assert!(verdict_line(&FitVerdict::Fits { headroom_mb: 2048 }).contains("2.0 GiB"));
        assert!(verdict_line(&FitVerdict::Tight { headroom_mb: 128 }).contains("128 MiB"));
        assert!(verdict_line(&FitVerdict::NeedsOffload { layers_on_gpu: 8 }).contains("8 layer(s)"));
        assert!(verdict_line(&FitVerdict::WontFit { short_by_mb: 4096 }).contains("4.0 GiB"));
    }

    /// The double-count trap: one physical GPU, two builds, two `-dev` names. An
    /// unqualified `fit` must budget for the build it would actually launch, not for the
    /// union.
    #[test]
    fn default_devices_come_from_one_build_never_from_every_build() {
        use apexrouter_protocol::{BuildId, FlagSupport, LlamaBuild, RigSnapshot};
        let build = |id: &str, devices: Vec<&str>| LlamaBuild {
            id: BuildId::parse(id).expect("id"),
            server_path: format!("/home/andre/llama.cpp/{id}/bin/llama-server"),
            label: id.to_string(),
            build_info: Some("b9199".to_string()),
            backends: Vec::new(),
            devices: devices.into_iter().map(str::to_string).collect(),
            flags: FlagSupport::default(),
            probed_at_unix: 0,
        };
        let rig = RigSnapshot {
            builds: vec![
                build("build", vec!["ROCm0"]),
                build("build-rocm", vec![]),
                build("build-vulkan", vec!["Vulkan0"]),
            ],
            ..RigSnapshot::default()
        };
        let d = default_devices(&rig);
        assert_eq!(d.len(), 1, "one build's devices, not the union: {d:?}");
        assert!(d[0] == "ROCm0" || d[0] == "Vulkan0", "{d:?}");

        // A rig with only a broken build has nothing to spend, and says so by being empty.
        let none = RigSnapshot {
            builds: vec![build("build-rocm", vec![])],
            ..RigSnapshot::default()
        };
        assert!(default_devices(&none).is_empty());
        assert!(default_devices(&RigSnapshot::default()).is_empty());
    }

    #[test]
    fn auto_ngl_says_it_emits_nothing() {
        assert!(ngl_line(&NglPlan::Auto).contains("no -ngl"));
        assert_eq!(ngl_line(&NglPlan::Layers(32)), "32 layers");
    }
}
