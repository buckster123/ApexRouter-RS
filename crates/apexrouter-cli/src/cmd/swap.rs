//! OWNER: unit S-08 (cli/src/cmd/{vast,tunnel,hf,provider,recipe,profile,usage,smoke,doctor,compare,backend,swap,up,approvals,token,open,env,migrate}.rs). Do not edit outside that unit.
//!
//! `apexrouter swap <alias> --to <model|recipe|backend-id> [--mode hot|sequential]` — an RPC to `POST /v1/routes/{alias}/swap`, never a file write plus a hope that the watcher noticed.
//!
//! **One verb, and the mode is chosen for you** (`ARCHITECTURE.md` §4.7). `hot` brings the
//! new backend up before the old one goes, which needs VRAM for both; `sequential` frees the
//! old one first, which is the only option when it does not fit. Omitting `--mode` lets the
//! daemon decide from `fit()`, because the daemon is the thing that knows what is resident —
//! guessing here and being wrong means either an OOM or a needless gap in service.
//!
//! `--to` resolves in the same documented order [`crate::cmd::up`] uses, minus the "path on
//! disk" step: a registered **backend id** first (nothing to start, so the swap is pure
//! re-pointing), then a **recipe**, then a **model**. A `Vast` recipe is refused here on
//! purpose: swapping is a routing operation, and one that quietly starts an hourly bill is
//! not one — `apexrouter recipe run --yes` is where that decision is made out loud.
//!
//! **Resolution stops at naming the target; deciding whether it can serve is the daemon's.**
//! `--to <model>` is sent as an `EndpointSpec` even when something is already running those
//! weights, because only the daemon knows what is resident and whether it is healthy — it
//! re-points at the running copy instead of starting a second one that would OOM the box.
//! Guessing that here would put a second answer to "what is active" in the tree, which is
//! the thing invariant 1 exists to forbid.
//!
//! A refusal is unwrapped from the daemon's `ErrorEnvelope` before it is printed, so the
//! sentence the operator sees is the daemon's — including the command that fixes it. The
//! failure this replaces was a silent storm of `503`s whose remedy appeared nowhere.

use crate::cli::SwapArgs;
use crate::cmd::{route, up, Ctx};
use crate::daemon::Need;
use crate::render;
use apexrouter_client::NodeClient;
use apexrouter_protocol::{
    Backend, EndpointSpec, Recipe, RecipeKind, ServedBy, SwapMode, SwapReport,
};

/// Run `apexrouter swap`.
///
/// # Errors
/// An invalid alias, an unresolvable `--to`, a `Vast` recipe, or a daemon that refuses.
pub async fn run(ctx: &Ctx, args: &SwapArgs) -> anyhow::Result<()> {
    let alias = route::parse_alias(&args.alias)?;
    let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
    let to = resolve(ctx, &client, &args.to).await?;

    let body = match args.mode {
        Some(m) => serde_json::json!({ "to": to, "mode": SwapMode::from(m) }),
        None => serde_json::json!({ "to": to }),
    };
    let report: SwapReport = client
        .post(&format!("/v1/routes/{}/swap", alias.as_str()), &body)
        .await
        .map_err(refusal)?;

    if args.json {
        return render::print_json(ServedBy::Daemon, render::now_unix(), false, &report);
    }
    render::print_line(&format!(
        "{}  {}  ->  {}  ({} · drained {} ms · {} ms total{})",
        report.alias.as_str(),
        report
            .from
            .as_ref()
            .map(|b| b.as_str().to_string())
            .unwrap_or_else(|| "(nothing)".to_string()),
        report.to.as_str(),
        render::variant(&report.mode),
        report.drained_ms,
        report.total_ms,
        if report.parked > 0 {
            format!(" · {} request(s) parked", report.parked)
        } else {
            String::new()
        }
    ));
    Ok(())
}

/// Unwrap a daemon refusal so the operator reads the sentence, not the envelope around it.
///
/// A swap is refused with `{"error":{"kind":…,"message":…}}` and the message is where the
/// recovery lives — "…Fix it with: `apexrouter backend enable dead-b`". Printed raw, that
/// arrives as `409 from /v1/routes/auto/swap: {"error":{"kind":…` and the remedy is buried in
/// JSON on a terminal. `kind` is kept as a prefix so `--json` mode and a human read the same
/// discriminator; anything that is not an envelope is passed through untouched, because a
/// mangled error is worse than a verbose one.
fn refusal(e: apexrouter_client::Error) -> anyhow::Error {
    if let apexrouter_client::Error::Status { body, .. } = &e {
        if let Ok(env) = serde_json::from_str::<apexrouter_protocol::ErrorEnvelope>(body) {
            return anyhow::anyhow!("{}: {}", env.error.kind, env.error.message);
        }
    }
    anyhow::Error::new(e)
}

/// Turn `--to` into a `SwapTarget`: a bare backend id, or an `EndpointSpec` to start first.
///
/// The untagged `SwapTarget` on the wire is `BackendId | EndpointSpec`, so a bare JSON
/// string means "this already exists" and an object means "start this".
///
/// # Errors
/// A `--to` that matches nothing, or a recipe that rents hardware.
async fn resolve(ctx: &Ctx, client: &NodeClient, to: &str) -> anyhow::Result<serde_json::Value> {
    // 1. A backend that is already registered. Nothing to start.
    let backends: Vec<Backend> = client.get("/v1/backends").await?;
    if let Some(b) = backends.iter().find(|b| b.id.as_str() == to) {
        return Ok(serde_json::Value::from(b.id.as_str()));
    }

    // 2. A recipe, by exact id.
    let recipes: Vec<Recipe> = client.get("/v1/recipes").await?;
    if let Some(r) = recipes.iter().find(|r| r.id.as_str() == to) {
        return Ok(serde_json::to_value(spec_of(r)?)?);
    }

    // 3. A model, resolved the way `up` and `endpoint start` resolve one.
    match up::local_spec(ctx, client, to, None, None, None, None).await {
        Ok(spec) => Ok(serde_json::to_value(spec)?),
        Err(e) => {
            let known: Vec<&str> = backends.iter().map(|b| b.id.as_str()).collect();
            Err(anyhow::anyhow!(
                "`{to}` is not a registered backend ({}), not a recipe, and not a model: {e}",
                if known.is_empty() {
                    "the table is empty".to_string()
                } else {
                    known.join(", ")
                }
            ))
        }
    }
}

/// The [`EndpointSpec`] a recipe instantiates, when swapping to it is a routing decision
/// rather than a spending one.
///
/// # Errors
/// A `Vast` recipe. Renting is deliberate spend and belongs behind `--yes`, so it is
/// refused here with the verb that does ask.
pub fn spec_of(r: &Recipe) -> anyhow::Result<EndpointSpec> {
    match &r.kind {
        RecipeKind::Local(s) => Ok(EndpointSpec::LocalLlama(s.clone())),
        RecipeKind::LocalVllm(s) => Ok(EndpointSpec::LocalVllm(s.clone())),
        RecipeKind::Managed(s) => Ok(EndpointSpec::Managed(s.clone())),
        RecipeKind::VastStudio { .. } => anyhow::bail!(
            "a VastStudio recipe is multi-service — use `apexrouter studio up`, not swap"
        ),
        RecipeKind::Vast { .. } => anyhow::bail!(
            "`{}` is a vast.ai recipe: swapping to it would start an hourly bill without \
             asking. Rent it explicitly with `apexrouter recipe run {} --yes`, then \
             `apexrouter swap <alias> --to <the new backend id>`.",
            r.id.as_str(),
            r.id.as_str()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_protocol::{Alias, BackendId, ProfileId, Provenance2, RecipeId};

    fn recipe_with(kind: RecipeKind) -> Recipe {
        Recipe {
            id: RecipeId::parse("carnice").expect("id"),
            label: "Carnice".to_string(),
            description: None,
            kind,
            provenance: Provenance2::default(),
            created_at_unix: 0,
            updated_at_unix: 0,
        }
    }

    fn vast_kind() -> RecipeKind {
        RecipeKind::Vast {
            profile: ProfileId::parse("two-3090s").expect("id"),
            launch: apexrouter_protocol::ContainerLaunch {
                runtime: apexrouter_protocol::ContainerRuntime::LlamaCpp,
                image: "img".to_string(),
                image_type: apexrouter_protocol::ImageType::Prebuilt,
                disk_gb: 120,
                env: Default::default(),
                onstart: String::new(),
                host: "127.0.0.1".to_string(),
                port: 8000,
                expose_public: false,
            },
            fit: None,
        }
    }

    fn vllm_kind() -> RecipeKind {
        RecipeKind::LocalVllm(apexrouter_protocol::LocalVllmSpec {
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
        })
    }

    #[test]
    fn a_renting_recipe_is_refused_and_names_the_verb_that_asks() {
        let e = spec_of(&recipe_with(vast_kind())).expect_err("must refuse");
        let msg = e.to_string();
        assert!(msg.contains("--yes"), "{msg}");
        assert!(msg.contains("recipe run"), "{msg}");
    }

    #[test]
    fn a_local_recipe_becomes_the_spec_it_launches() {
        let spec = spec_of(&recipe_with(vllm_kind())).expect("spec");
        assert!(matches!(spec, EndpointSpec::LocalVllm(_)));
    }

    #[test]
    fn a_refusal_reaches_the_operator_as_a_sentence_with_the_remedy_in_it() {
        // What the daemon answers when the target cannot serve. The recovery is IN the
        // message, and printing the envelope raw buries it in JSON on a terminal.
        let e = refusal(apexrouter_client::Error::Status {
            status: 409,
            path: "/v1/routes/auto/swap".to_string(),
            body: r#"{"error":{"kind":"backend_not_ready","message":"not pointing auto at dead-b: it is disabled. Fix it with: apexrouter backend enable dead-b"}}"#
                .to_string(),
        });
        let msg = e.to_string();
        assert!(msg.starts_with("backend_not_ready: "), "{msg}");
        assert!(msg.contains("apexrouter backend enable dead-b"), "{msg}");
        assert!(
            !msg.contains('{'),
            "the envelope is unwrapped, not quoted: {msg}"
        );
    }

    #[test]
    fn a_body_that_is_not_an_envelope_is_passed_through_untouched() {
        // An HTML error page from something that is not our daemon must stay debuggable; a
        // mangled error is worse than a verbose one.
        let e = refusal(apexrouter_client::Error::Status {
            status: 502,
            path: "/v1/routes/auto/swap".to_string(),
            body: "<title>502 Bad Gateway</title>".to_string(),
        });
        let msg = e.to_string();
        assert!(msg.contains("502 Bad Gateway"), "{msg}");
    }

    #[test]
    fn a_swap_from_nothing_renders_as_nothing_rather_than_a_blank_column() {
        let report = SwapReport {
            alias: Alias::parse("auto").expect("alias"),
            mode: SwapMode::Sequential,
            from: None,
            to: BackendId::parse("local-carnice").expect("id"),
            parked: 0,
            drained_ms: 0,
            total_ms: 12,
        };
        let from = report
            .from
            .as_ref()
            .map(|b| b.as_str().to_string())
            .unwrap_or_else(|| "(nothing)".to_string());
        assert_eq!(from, "(nothing)");
        assert_eq!(render::variant(&report.mode), "sequential");
    }
}
