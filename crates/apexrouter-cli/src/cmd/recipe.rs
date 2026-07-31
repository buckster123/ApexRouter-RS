//! OWNER: unit S-08 (cli/src/cmd/{vast,tunnel,hf,provider,recipe,profile,usage,smoke,doctor,compare,backend,swap,up,approvals,token,open,env,migrate}.rs). Do not edit outside that unit.
//!
//! `apexrouter recipe ls | show | new | edit | rm | validate | run`.
//!
//! A recipe is a saved launch plan — the CLI half of "dynamic recipe building in the GUI".
//! It replaces LocalRouter's 54 hand-solved `vast_gguf` strings with a document that says
//! what it launches and where its facts came from, so **staleness is detectable**:
//! `validate` reports a model file that has since been deleted as a `Warning` with a fix,
//! never as corruption.
//!
//! `ls` and `show` read `$STATE/catalog.toml` with no daemon, because a catalogue is a file.
//! `new`, `edit`, `rm` and `run` go through the daemon, which owns the write path and — for
//! `run` — is the only thing that may start a process or rent a box.
//!
//! `edit` is JSON in `$VISUAL`/`$EDITOR` rather than a wizard: a `RecipeKind` is a tagged
//! union with a whole `LocalLlamaSpec` or `ContainerLaunch` inside it, and a form that
//! covers every field is the GUI's job, not a terminal's.

use crate::cli::RecipeCmd;
use crate::cmd::Ctx;
use crate::daemon::{Need, Serving};
use crate::render;
use apexrouter_protocol::{
    EndpointRecord, JobRecord, Recipe, RecipeKind, ServedBy, Severity, ValidationReport,
};

/// Run `apexrouter recipe …`.
///
/// # Errors
/// A catalogue that will not parse, a daemon that will not answer, or an unknown id.
pub async fn run(ctx: &Ctx, cmd: &RecipeCmd) -> anyhow::Result<()> {
    match cmd {
        RecipeCmd::Ls(args) => {
            let serving = ctx.serving(Need::ReadState).await?;
            let recipes = load(ctx, &serving).await?;
            if args.json {
                return render::print_json(
                    serving.served_by(),
                    render::now_unix(),
                    serving.is_offline(),
                    &recipes,
                );
            }
            if serving.is_offline() {
                render::print_offline_notice();
            }
            render::print_table(
                &["RECIPE", "KIND", "LABEL", "TARGET", "SOURCE"],
                recipes.iter().map(row).collect(),
            );
            Ok(())
        }
        RecipeCmd::Show { id, json } => {
            let serving = ctx.serving(Need::ReadState).await?;
            let r = find(&load(ctx, &serving).await?, id)?;
            if *json {
                return render::print_json(
                    serving.served_by(),
                    render::now_unix(),
                    serving.is_offline(),
                    &r,
                );
            }
            if serving.is_offline() {
                render::print_offline_notice();
            }
            print_recipe(&r);
            Ok(())
        }
        RecipeCmd::New {
            from_endpoint,
            edit,
            json,
        } => {
            let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
            let r: Recipe = client
                .post(
                    &format!("/v1/recipes/from-endpoint/{from_endpoint}"),
                    &serde_json::json!({}),
                )
                .await?;
            if *json {
                return render::print_json(ServedBy::Daemon, render::now_unix(), false, &r);
            }
            render::print_line(&format!("{}  {}", r.id.as_str(), r.label));
            if *edit {
                return edit_document(ctx, r.id.as_str()).await;
            }
            Ok(())
        }
        RecipeCmd::Edit { id } => edit_document(ctx, id).await,
        RecipeCmd::Rm { id } => {
            let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
            client.delete(&format!("/v1/recipes/{id}")).await?;
            render::print_line(&format!("removed {id}"));
            Ok(())
        }
        RecipeCmd::Validate { id, json } => {
            let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
            let report: ValidationReport = client
                .post(
                    &format!("/v1/recipes/{id}/validate"),
                    &serde_json::json!({}),
                )
                .await?;
            if *json {
                return render::print_json(ServedBy::Daemon, render::now_unix(), false, &report);
            }
            print_report(id, &report);
            Ok(())
        }
        RecipeCmd::Run {
            id,
            alias,
            no_wait,
            yes,
            json,
        } => instantiate(ctx, id, alias.as_deref(), *no_wait, *yes, *json).await,
    }
}

/// Every recipe: from the daemon when there is one, from `$STATE/catalog.toml` when not.
///
/// # Errors
/// A catalogue that will not parse, or a daemon that will not answer.
pub async fn load(ctx: &Ctx, serving: &Serving) -> anyhow::Result<Vec<Recipe>> {
    match serving {
        Serving::Daemon(c) => Ok(c.get::<Vec<Recipe>>("/v1/recipes").await?),
        _ => Ok(apexrouter_core::catalog::load(&ctx.paths)?.recipes),
    }
}

/// One recipe by id, or a message naming what there is.
///
/// # Errors
/// When no recipe has that id.
pub fn find(recipes: &[Recipe], id: &str) -> anyhow::Result<Recipe> {
    recipes
        .iter()
        .find(|r| r.id.as_str() == id)
        .cloned()
        .ok_or_else(|| {
            let known: Vec<&str> = recipes.iter().map(|r| r.id.as_str()).collect();
            anyhow::anyhow!(
                "no recipe `{id}`{}",
                if known.is_empty() {
                    " — the catalogue is empty".to_string()
                } else {
                    format!(" — there is: {}", known.join(", "))
                }
            )
        })
}

/// `recipe run <id>` — and the shared body behind `apexrouter up <recipe>`.
///
/// A `Vast` recipe rents hardware, so it requires `--yes` and prints the ceiling it is
/// about to act under. The approval itself is minted server-side; this is the gate that
/// stops a mistyped tab-completion from starting a bill.
///
/// # Errors
/// A missing `--yes` on a renting recipe, or a daemon that refuses the instantiation.
pub async fn instantiate(
    ctx: &Ctx,
    id: &str,
    alias: Option<&str>,
    no_wait: bool,
    yes: bool,
    json: bool,
) -> anyhow::Result<()> {
    let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
    let recipes: Vec<Recipe> = client.get("/v1/recipes").await?;
    let recipe = find(&recipes, id)?;

    if spends_money(&recipe.kind) && !yes {
        anyhow::bail!(
            "`{id}` rents hardware on vast.ai — re-run with --yes. It is a \
             {} recipe, and instantiating it starts an hourly bill.",
            kind_label(&recipe.kind)
        );
    }

    let mut query: Vec<String> = Vec::new();
    if let Some(a) = alias {
        query.push(format!("alias={a}"));
    }
    if no_wait {
        query.push("no_wait=true".to_string());
    }
    let path = match query.is_empty() {
        true => format!("/v1/recipes/{id}/instantiate"),
        false => format!("/v1/recipes/{id}/instantiate?{}", query.join("&")),
    };

    let raw: serde_json::Value = client.post(&path, &serde_json::json!({})).await?;
    if json {
        return render::print_json(ServedBy::Daemon, render::now_unix(), false, &raw);
    }
    // `?no_wait` answers with a JobRecord; everything else answers with the record itself.
    if let Ok(rec) = serde_json::from_value::<EndpointRecord>(raw.clone()) {
        render::print_line(&format!(
            "{}  port {}  {}",
            rec.id.as_str(),
            render::dash(rec.port),
            render::variant(&rec.desired)
        ));
        return Ok(());
    }
    let job: JobRecord = serde_json::from_value(raw)?;
    render::print_line(&format!("job {} ({})", job.id, render::variant(&job.state)));
    Ok(())
}

/// Fetch, edit as JSON, `PUT` back. The daemon validates on the way in.
///
/// # Errors
/// An editor that fails, JSON that will not parse, or a daemon that refuses the document.
async fn edit_document(ctx: &Ctx, id: &str) -> anyhow::Result<()> {
    let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
    let before: Recipe = client.get(&format!("/v1/recipes/{id}")).await?;
    let edited: Recipe = crate::cmd::profile::edit_json(&before, &format!("recipe-{id}"))?;
    let after: Recipe = client.put(&format!("/v1/recipes/{id}"), &edited).await?;
    render::print_line(&format!("{} saved", after.id.as_str()));
    Ok(())
}

/// Does instantiating this recipe start a bill?
pub fn spends_money(k: &RecipeKind) -> bool {
    matches!(k, RecipeKind::Vast { .. })
}

/// The `KIND` column.
pub fn kind_label(k: &RecipeKind) -> &'static str {
    match k {
        RecipeKind::Local(_) => "local",
        RecipeKind::LocalVllm(_) => "local_vllm",
        RecipeKind::Vast { .. } => "vast",
        RecipeKind::Managed(_) => "managed",
    }
}

/// What a recipe launches, in one cell.
fn target_label(k: &RecipeKind) -> String {
    match k {
        RecipeKind::Local(s) => s
            .model_path
            .rsplit('/')
            .next()
            .unwrap_or(&s.model_path)
            .to_string(),
        RecipeKind::LocalVllm(s) => s.model_id.clone(),
        RecipeKind::Vast {
            profile, launch, ..
        } => {
            format!("{} · {}", profile.as_str(), launch.image)
        }
        RecipeKind::Managed(s) => s
            .model_id
            .clone()
            .unwrap_or_else(|| s.provider.as_str().to_string()),
    }
}

/// One row of the recipe table.
fn row(r: &Recipe) -> Vec<String> {
    vec![
        r.id.as_str().to_string(),
        kind_label(&r.kind).to_string(),
        r.label.clone(),
        target_label(&r.kind),
        r.provenance.source.clone(),
    ]
}

/// The detail view.
fn print_recipe(r: &Recipe) {
    render::print_line(&format!("{}  {}", r.id.as_str(), r.label));
    render::print_line(&format!("  kind      {}", kind_label(&r.kind)));
    render::print_line(&format!("  target    {}", target_label(&r.kind)));
    render::print_line(&format!("  source    {}", r.provenance.source));
    if let Some(b) = r.provenance.size_bytes {
        render::print_line(&format!("  size      {}", render::human_bytes(b)));
    }
    if let Some(f) = &r.provenance.fit {
        render::print_line(&format!(
            "  fit       ctx {} · {} slot(s) · kv {}",
            f.ctx,
            f.parallel,
            f.kv_type.as_flag()
        ));
    }
    if let Some(d) = &r.description {
        render::print_line(&format!("  note      {d}"));
    }
    if spends_money(&r.kind) {
        render::print_line("  money     instantiating this rents hardware; `run` needs --yes");
    }
}

/// A validation report, one actionable line per issue.
pub fn print_report(id: &str, report: &ValidationReport) {
    render::print_line(&format!(
        "{id}  {}",
        if report.ok { "ok" } else { "NOT OK" }
    ));
    for i in &report.issues {
        render::print_line(&format!(
            "  {:8} {}  {}{}",
            severity(i.severity),
            i.field,
            i.message,
            i.fix
                .as_ref()
                .map(|f| format!(" — {f}"))
                .unwrap_or_default()
        ));
    }
}

/// Severity in its serde spelling.
fn severity(s: Severity) -> String {
    render::variant(&s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_protocol::{ProfileId, RecipeId};

    fn recipe(kind: RecipeKind) -> Recipe {
        Recipe {
            id: RecipeId::parse("carnice").expect("id"),
            label: "Carnice 9B".to_string(),
            description: None,
            kind,
            provenance: apexrouter_protocol::Provenance2 {
                discovered_at_unix: 0,
                size_bytes: None,
                source: "/home/andre/models".to_string(),
                fit: None,
            },
            created_at_unix: 0,
            updated_at_unix: 0,
        }
    }

    fn vast_kind() -> RecipeKind {
        RecipeKind::Vast {
            profile: ProfileId::parse("two-3090s").expect("id"),
            launch: apexrouter_protocol::ContainerLaunch {
                runtime: apexrouter_protocol::ContainerRuntime::LlamaCpp,
                image: "apexrouter/llama:prebuilt".to_string(),
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

    #[test]
    fn only_a_renting_recipe_counts_as_spending() {
        assert!(spends_money(&vast_kind()));
        assert!(!spends_money(&RecipeKind::Managed(
            apexrouter_protocol::ManagedSpec {
                provider: apexrouter_protocol::ProviderId::parse("together").expect("id"),
                base_url: "https://api.together.ai".to_string(),
                credential: apexrouter_protocol::CredentialSource::None,
                model_id: Some("m".to_string()),
                protocol: apexrouter_protocol::Protocol::OpenAi,
            }
        )));
    }

    #[test]
    fn the_target_cell_names_what_will_actually_be_launched() {
        let r = recipe(vast_kind());
        assert!(target_label(&r.kind).contains("two-3090s"), "{:?}", r.kind);
        assert_eq!(kind_label(&r.kind), "vast");
    }

    #[test]
    fn a_missing_recipe_lists_what_the_catalogue_holds() {
        let e = find(&[], "nope").expect_err("must fail");
        assert!(e.to_string().contains("catalogue is empty"), "{e}");
        let e = find(&[recipe(vast_kind())], "nope").expect_err("must fail");
        assert!(e.to_string().contains("carnice"), "{e}");
    }
}
