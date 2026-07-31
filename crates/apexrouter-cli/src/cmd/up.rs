//! OWNER: unit S-08 (cli/src/cmd/{vast,tunnel,hf,provider,recipe,profile,usage,smoke,doctor,compare,backend,swap,up,approvals,token,open,env,migrate}.rs). Do not edit outside that unit.
//!
//! `apexrouter up <model|recipe> [--alias A] [--yes]` — the one-command happy path. Resolves its positional in a **documented** order: exact recipe id -> exact model id -> unique case-insensitive prefix -> path on disk; an ambiguous prefix errors with the candidates listed.
//!
//! # The resolution order, and why it is written down
//!
//! ```text
//! 1. exact recipe id
//! 2. exact model id (or exact model name)
//! 3. unique case-insensitive prefix of a model id/name
//! 4. a path on disk
//! ```
//!
//! Recipes come first because a recipe is a decision somebody already made — sizing, build,
//! devices, sampling — and a model whose id happens to collide with one must not silently
//! win. **An ambiguous prefix is an error listing every candidate**, never a guess: picking
//! one of `Carnice-9b-Q6_K` and `Carnice-9b-Q4_K` for a human is how the wrong quant ends up
//! resident for a week.
//!
//! `--yes` is required only when the resolved thing spends money — a `Vast` recipe. Nothing
//! local needs it, because starting a local process is free and stopping it is one verb.

use crate::cli::{split_list, UpArgs};
use crate::cmd::{models, recipe, url, Ctx};
use crate::daemon::Need;
use crate::render;
use apexrouter_client::NodeClient;
use apexrouter_protocol::{
    EndpointRecord, EndpointSpec, GpuBackend, JobRecord, LocalLlamaSpec, LocalModel, NglPlan,
    Recipe, RigSnapshot, SamplingMode, ServedBy, SplitMode, SplitPlan,
};

/// What `up`'s positional turned out to name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// An exact recipe id — step 1.
    Recipe(String),
    /// A model, by id, name, unique prefix or path — steps 2 to 4.
    Model(String),
}

/// Run `apexrouter up`.
///
/// # Errors
/// An ambiguous or unmatched positional, a renting recipe without `--yes`, or a daemon that
/// refuses to start the thing.
pub async fn run(ctx: &Ctx, args: &UpArgs) -> anyhow::Result<()> {
    let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
    let recipes: Vec<Recipe> = client.get("/v1/recipes").await?;
    let models: Vec<LocalModel> = client.get("/v1/models/local").await?;

    match resolve(&recipes, &models, &args.what)? {
        Resolved::Recipe(id) => {
            recipe::instantiate(
                ctx,
                &id,
                args.alias.as_deref(),
                args.no_wait,
                args.yes,
                args.json,
            )
            .await
        }
        Resolved::Model(model) => start_model(ctx, &client, &model, args).await,
    }
}

/// The documented resolution order, as one pure function.
///
/// # Errors
/// Nothing matched — naming where discovery looked — or a prefix matched more than one
/// model, in which case every candidate is listed and nothing is chosen.
pub fn resolve(recipes: &[Recipe], models: &[LocalModel], what: &str) -> anyhow::Result<Resolved> {
    // 1. exact recipe id.
    if recipes.iter().any(|r| r.id.as_str() == what) {
        return Ok(Resolved::Recipe(what.to_string()));
    }

    // 2. exact model id or name.
    if models.iter().any(|m| m.id == what || m.name == what) {
        return Ok(Resolved::Model(what.to_string()));
    }

    // 3. unique case-insensitive prefix. Ambiguity is an error, with the candidates.
    let lower = what.to_lowercase();
    let hits: Vec<&LocalModel> = models
        .iter()
        .filter(|m| {
            m.id.to_lowercase().starts_with(&lower) || m.name.to_lowercase().starts_with(&lower)
        })
        .collect();
    match hits.len() {
        1 => return Ok(Resolved::Model(hits[0].id.clone())),
        0 => {}
        _ => {
            let names: Vec<&str> = hits.iter().map(|m| m.name.as_str()).collect();
            anyhow::bail!(
                "`{what}` is ambiguous — it matches {}. Name one of them exactly.",
                names.join(", ")
            );
        }
    }

    // 4. a path on disk.
    if std::path::Path::new(what).is_file() {
        return Ok(Resolved::Model(what.to_string()));
    }

    let recipe_ids: Vec<&str> = recipes.iter().map(|r| r.id.as_str()).collect();
    anyhow::bail!(
        "`{what}` is not a recipe{}, not a local model, and not a file — \
         `apexrouter recipe ls` and `apexrouter models ls` list both, and \
         [endpoints] model_roots is where discovery looked",
        if recipe_ids.is_empty() {
            String::new()
        } else {
            format!(" (there is: {})", recipe_ids.join(", "))
        }
    )
}

/// Start a local `llama-server` for a resolved model, bind the alias, print the URL.
///
/// # Errors
/// An unresolvable model, no usable build, or a daemon that refuses the spec.
async fn start_model(
    ctx: &Ctx,
    client: &NodeClient,
    model: &str,
    args: &UpArgs,
) -> anyhow::Result<()> {
    let spec = local_spec(
        ctx,
        client,
        model,
        args.devices.as_deref(),
        args.ctx,
        args.parallel,
        args.mode.map(Into::into),
    )
    .await?;

    let mut query: Vec<String> = Vec::new();
    // `up` binds an alias by default — that is the "one command" part. Without an explicit
    // `--alias` the default alias is what an agent's `"model"` field already says.
    let alias = match &args.alias {
        Some(a) => a.clone(),
        None => default_alias(ctx, client).await,
    };
    query.push(format!("alias={alias}"));
    if args.no_wait {
        query.push("no_wait=true".to_string());
    }
    if args.force {
        query.push("force=true".to_string());
    }
    let path = format!("/v1/endpoints?{}", query.join("&"));

    let raw: serde_json::Value = client.post(&path, &spec).await?;
    if args.json {
        return render::print_json(ServedBy::Daemon, render::now_unix(), false, &raw);
    }
    if let Ok(rec) = serde_json::from_value::<EndpointRecord>(raw.clone()) {
        render::print_line(&format!(
            "{}  port {}  alias {}",
            rec.id.as_str(),
            render::dash(rec.port),
            alias
        ));
        let (base, _) = url::proxy_base(ctx)?;
        render::print_line(&format!("{base}/v1"));
        return Ok(());
    }
    let job: JobRecord = serde_json::from_value(raw)?;
    render::print_line(&format!(
        "job {} ({}) — `apexrouter endpoint ls` when it finishes",
        job.id,
        render::variant(&job.state)
    ));
    Ok(())
}

/// The [`EndpointSpec`] for a local model, with the build chosen the way `endpoint start`
/// chooses one.
///
/// Published because [`crate::cmd::swap`] needs exactly this: `--to <model>` has to become a
/// spec the daemon can start, and two implementations of "which build?" would eventually
/// disagree about which one can see `Vulkan0`.
///
/// # Errors
/// An unresolvable model, a model with no shard, or a machine with no llama.cpp build.
pub async fn local_spec(
    ctx: &Ctx,
    client: &NodeClient,
    model: &str,
    devices: Option<&str>,
    ctx_size: Option<u32>,
    parallel: Option<u32>,
    mode: Option<SamplingMode>,
) -> anyhow::Result<EndpointSpec> {
    let all: Vec<LocalModel> = client.get("/v1/models/local").await?;
    let m = models::resolve_model(&all, model)?;
    let rig: RigSnapshot = client.get("/v1/rig").await?;

    let devices = devices.map(split_list).unwrap_or_default();
    let want = devices.first().and_then(|d| backend_of(d));
    let choice = apexrouter_core::discover::choose_build(&rig.builds, want).ok_or_else(|| {
        anyhow::anyhow!(
            "no llama.cpp build was discovered — `apexrouter rig` shows where it looked, \
             and [endpoints] build_roots is what it searched"
        )
    })?;

    let model_path = m
        .primary_path()
        .ok_or_else(|| anyhow::anyhow!("{} has no shard to load", m.name))?
        .to_string();

    Ok(EndpointSpec::LocalLlama(LocalLlamaSpec {
        build: choice.chosen,
        model_path,
        mmproj: m.mmproj.first().map(|s| s.path.clone()),
        alias_flag: m.name.clone(),
        host: "127.0.0.1".to_string(),
        port: None,
        ctx: ctx_size,
        parallel,
        kv_type: None,
        // `Auto` is the point of `up`: the fit solver decides how many layers fit, rather
        // than an operator guessing and discovering the answer as an OOM.
        ngl: NglPlan::Auto,
        split: SplitPlan {
            devices,
            mode: SplitMode::Layer,
            main_gpu: None,
            tensor_split: None,
        },
        mode: mode.unwrap_or_else(|| sampling_mode(&ctx.cfg.endpoints.default_mode)),
        flash_attn: None,
        api_key: None,
        extra_args: Vec::new(),
    }))
}

/// The alias `up` binds when none was named: whatever the table calls default.
///
/// A daemon that will not answer falls back to `"auto"`, which is what the shipped config
/// and every LocalRouter transcript use — a wrong guess here costs one `route default`, and
/// failing the whole start over a name would cost the operator the point of the verb.
async fn default_alias(_ctx: &Ctx, client: &NodeClient) -> String {
    match client
        .get::<serde_json::Value>("/v1/snapshot")
        .await
        .ok()
        .and_then(|v| {
            v.get("default_alias")
                .and_then(|a| a.as_str())
                .map(str::to_string)
        }) {
        Some(a) if !a.is_empty() => a,
        _ => "auto".to_string(),
    }
}

/// The compute backend a `-dev` token belongs to, by its documented prefix.
fn backend_of(device: &str) -> Option<GpuBackend> {
    let d = device.to_lowercase();
    if d.starts_with("vulkan") {
        Some(GpuBackend::Vulkan)
    } else if d.starts_with("cuda") {
        Some(GpuBackend::Cuda)
    } else if d.starts_with("rocm") {
        Some(GpuBackend::Rocm)
    } else if d.starts_with("hip") {
        Some(GpuBackend::Hip)
    } else if d.starts_with("sycl") {
        Some(GpuBackend::Sycl)
    } else if d.starts_with("metal") {
        Some(GpuBackend::Metal)
    } else if d.starts_with("cpu") {
        Some(GpuBackend::Cpu)
    } else {
        None
    }
}

/// `[endpoints] default_mode` as the enum. An unknown value is `Thinking`, the documented
/// default, rather than a panic.
fn sampling_mode(s: &str) -> SamplingMode {
    match s.to_lowercase().as_str() {
        "coding" => SamplingMode::Coding,
        "nonthinking" => SamplingMode::Nonthinking,
        "raw" => SamplingMode::Raw,
        _ => SamplingMode::Thinking,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_protocol::{ModelShard, Provenance2, RecipeId, RecipeKind};

    fn model(id: &str, name: &str) -> LocalModel {
        LocalModel {
            id: id.to_string(),
            name: name.to_string(),
            dir: "/home/andre/models".to_string(),
            shards: vec![ModelShard {
                path: format!("/home/andre/models/{name}.gguf"),
                bytes: 1,
            }],
            total_bytes: 1,
            mmproj: Vec::new(),
            quant: None,
            gguf: None,
            discovered_at_unix: 0,
        }
    }

    fn recipe(id: &str) -> Recipe {
        Recipe {
            id: RecipeId::parse(id).expect("id"),
            label: id.to_string(),
            description: None,
            kind: RecipeKind::LocalVllm(apexrouter_protocol::LocalVllmSpec {
                bin: "vllm".to_string(),
                model_id: "Qwen/Qwen3".to_string(),
                tp: None,
                ctx: None,
                quantization: None,
                kv_cache_dtype: None,
                enforce_eager: false,
                reasoning_parser: None,
                gpu_util: None,
                max_num_seqs: None,
                trust_remote: false,
                chunked_prefill: true,
                host: "127.0.0.1".to_string(),
                port: None,
                devices: Vec::new(),
                extra_args: Vec::new(),
            }),
            provenance: Provenance2::default(),
            created_at_unix: 0,
            updated_at_unix: 0,
        }
    }

    #[test]
    fn an_exact_recipe_id_wins_over_a_model_of_the_same_name() {
        let recipes = vec![recipe("carnice")];
        let models = vec![model("carnice", "carnice")];
        assert_eq!(
            resolve(&recipes, &models, "carnice").expect("resolved"),
            Resolved::Recipe("carnice".to_string())
        );
    }

    #[test]
    fn an_exact_model_id_or_name_resolves_when_no_recipe_claims_it() {
        let models = vec![model("carnice-9b-q6-k", "Carnice-9b-Q6_K")];
        assert_eq!(
            resolve(&[], &models, "Carnice-9b-Q6_K").expect("by name"),
            Resolved::Model("Carnice-9b-Q6_K".to_string())
        );
        assert_eq!(
            resolve(&[], &models, "carnice-9b-q6-k").expect("by id"),
            Resolved::Model("carnice-9b-q6-k".to_string())
        );
    }

    #[test]
    fn a_unique_case_insensitive_prefix_resolves_to_the_model_id() {
        let models = vec![model("carnice-9b-q6-k", "Carnice-9b-Q6_K")];
        assert_eq!(
            resolve(&[], &models, "CARNICE").expect("prefix"),
            Resolved::Model("carnice-9b-q6-k".to_string())
        );
    }

    #[test]
    fn an_ambiguous_prefix_lists_every_candidate_and_chooses_nothing() {
        let models = vec![
            model("carnice-9b-q6-k", "Carnice-9b-Q6_K"),
            model("carnice-9b-q4-k", "Carnice-9b-Q4_K"),
        ];
        let e = resolve(&[], &models, "carnice").expect_err("must not guess");
        let msg = e.to_string();
        assert!(msg.contains("ambiguous"), "{msg}");
        assert!(msg.contains("Carnice-9b-Q6_K"), "{msg}");
        assert!(msg.contains("Carnice-9b-Q4_K"), "{msg}");
    }

    #[test]
    fn a_path_on_disk_resolves_after_every_id_form_has_failed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("Weird-Q4_K_M.gguf");
        std::fs::write(&p, b"gguf").expect("write");
        let path = p.display().to_string();
        assert_eq!(
            resolve(&[], &[], &path).expect("path"),
            Resolved::Model(path.clone())
        );
    }

    #[test]
    fn nothing_matching_names_both_catalogues_and_where_discovery_looked() {
        let e = resolve(&[recipe("other")], &[], "nope").expect_err("must fail");
        let msg = e.to_string();
        assert!(msg.contains("model_roots"), "{msg}");
        assert!(msg.contains("other"), "the recipes that DO exist: {msg}");
    }

    #[test]
    fn a_device_token_names_its_backend() {
        assert_eq!(backend_of("Vulkan0"), Some(GpuBackend::Vulkan));
        assert_eq!(backend_of("ROCm0"), Some(GpuBackend::Rocm));
        assert_eq!(backend_of("nonsense"), None);
    }

    #[test]
    fn the_default_sampling_mode_survives_a_typo_in_config() {
        assert_eq!(sampling_mode("coding"), SamplingMode::Coding);
        assert_eq!(sampling_mode("NONSENSE"), SamplingMode::Thinking);
    }
}
