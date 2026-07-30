//! OWNER: unit S-06 (cli/src/{main,cli,daemon,render}.rs, cli/src/cmd/{mod,status,serve,config,rig,models,fit,endpoint,route,switch,url,version,completions}.rs). Do not edit outside that unit.
//!
//! `apexrouter switch <together|local <name>|vast-gguf|endpoint <id>|alias <a>>` — kept for
//! muscle memory.
//!
//! It is **sugar over `route set <default alias>`**, not a second mechanism: the legacy TUI's
//! "switch provider" is exactly "re-point the alias every client already uses", and having
//! it write the same route through the same endpoint is what stops the two from disagreeing.

use crate::cli::SwitchCmd;
use crate::cmd::{route, Ctx};
use crate::daemon::Need;
use crate::render;
use apexrouter_client::NodeClient;
use apexrouter_protocol::{
    Alias, Backend, BackendId, BackendSelector, EndpointRecord, ModelRoute, RouteTarget, ServedBy,
};

/// Run `apexrouter switch …`.
///
/// # Errors
/// A daemon that will not answer, or a target that resolves to nothing.
pub async fn run(ctx: &Ctx, cmd: &SwitchCmd) -> anyhow::Result<()> {
    let client = ctx.serving(Need::Mutate).await?.into_daemon()?;

    // `switch alias <a>` changes which alias is the default; everything else changes where
    // the default alias points.
    if let SwitchCmd::Alias { alias, json } = cmd {
        let alias = route::parse_alias(alias)?;
        let _: serde_json::Value = client
            .post(
                "/v1/routes/default",
                &serde_json::json!({ "alias": alias.as_str() }),
            )
            .await?;
        if *json {
            return render::print_json(
                ServedBy::Daemon,
                render::now_unix(),
                false,
                &serde_json::json!({ "default_alias": alias.as_str() }),
            );
        }
        render::print_line(&format!("default alias is now {}", alias.as_str()));
        return Ok(());
    }

    let routes: Vec<ModelRoute> = client.get("/v1/routes").await?;
    let alias = default_alias(&routes, &ctx.cfg.router.default_alias)?;
    let selector = selector_for(&client, cmd).await?;

    let mut route = routes
        .iter()
        .find(|r| r.alias == alias)
        .cloned()
        .unwrap_or(ModelRoute {
            alias: alias.clone(),
            targets: Vec::new(),
            strategy: apexrouter_protocol::Strategy::FirstHealthy,
            filter: apexrouter_protocol::RouteFilter::default(),
            retry: apexrouter_protocol::RetryPolicy::default(),
            is_default: true,
            description: None,
        });
    let before = route::targets_label(&route);
    route.targets = vec![RouteTarget {
        backend: selector,
        model: None,
        weight: 1,
    }];

    let after: ModelRoute = client
        .put(&format!("/v1/routes/{}", alias.as_str()), &route)
        .await?;
    if json_flag(cmd) {
        return render::print_json(ServedBy::Daemon, render::now_unix(), false, &after);
    }
    render::print_line(&format!(
        "{}  {}  ->  {}",
        alias.as_str(),
        before,
        route::targets_label(&after)
    ));
    Ok(())
}

/// Which alias `switch` re-points: the table's default, falling back to `[router]
/// default_alias`.
///
/// # Errors
/// When the configured default alias is not a valid alias.
fn default_alias(routes: &[ModelRoute], configured: &str) -> anyhow::Result<Alias> {
    match routes.iter().find(|r| r.is_default) {
        Some(r) => Ok(r.alias.clone()),
        None => route::parse_alias(configured),
    }
}

/// Turn the muscle-memory verb into a backend selector.
///
/// # Errors
/// When nothing on this machine matches.
async fn selector_for(client: &NodeClient, cmd: &SwitchCmd) -> anyhow::Result<BackendSelector> {
    match cmd {
        SwitchCmd::Together(_) => {
            let backends: Vec<Backend> = client.get("/v1/backends").await?;
            match backends.iter().find(|b| b.id.as_str() == "together") {
                Some(b) => Ok(BackendSelector::Id(b.id.clone())),
                // No registered `together` backend: select by tag, which is what a managed
                // provider registered under another id will carry.
                None => Ok(BackendSelector::Tag("together".to_string())),
            }
        }
        SwitchCmd::VastGguf(_) => Ok(BackendSelector::Tag("vast".to_string())),
        SwitchCmd::Endpoint { id, .. } => {
            Ok(BackendSelector::Id(BackendId::parse(id).map_err(|e| {
                anyhow::anyhow!("`{id}` is not a backend id: {e}")
            })?))
        }
        SwitchCmd::Local { name, .. } => {
            let endpoints: Vec<EndpointRecord> = client.get("/v1/endpoints").await?;
            let id = match_local(&endpoints, name)?;
            Ok(BackendSelector::Id(id))
        }
        // Handled before this function is reached.
        SwitchCmd::Alias { .. } => Err(anyhow::anyhow!("switch alias is handled by the caller")),
    }
}

/// Find the one local endpoint a name refers to.
///
/// # Errors
/// Nothing matching, or more than one match — which lists the candidates.
fn match_local(endpoints: &[EndpointRecord], name: &str) -> anyhow::Result<BackendId> {
    let lower = name.to_lowercase();
    let hits: Vec<&EndpointRecord> = endpoints
        .iter()
        .filter(|e| {
            e.id.as_str().to_lowercase().contains(&lower)
                || model_of(e)
                    .map(|m| m.to_lowercase().contains(&lower))
                    .unwrap_or(false)
        })
        .collect();
    match hits.len() {
        1 => Ok(hits[0].id.clone()),
        0 => anyhow::bail!(
            "no local endpoint matches `{name}` — `apexrouter endpoint ls` lists what there is"
        ),
        _ => {
            let ids: Vec<&str> = hits.iter().map(|e| e.id.as_str()).collect();
            anyhow::bail!(
                "`{name}` is ambiguous — it matches {}. Use `apexrouter switch endpoint <id>`.",
                ids.join(", ")
            )
        }
    }
}

/// The model an endpoint serves, when its spec names one.
fn model_of(e: &EndpointRecord) -> Option<&str> {
    match &e.spec {
        apexrouter_protocol::EndpointSpec::LocalLlama(s) => Some(&s.model_path),
        apexrouter_protocol::EndpointSpec::LocalVllm(s) => Some(&s.model_id),
        _ => None,
    }
}

/// `--json` on whichever leaf this is.
fn json_flag(cmd: &SwitchCmd) -> bool {
    match cmd {
        SwitchCmd::Together(a) | SwitchCmd::VastGguf(a) => a.json,
        SwitchCmd::Local { json, .. }
        | SwitchCmd::Endpoint { json, .. }
        | SwitchCmd::Alias { json, .. } => *json,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_protocol::{
        BuildId, DesiredState, EndpointSpec, LocalLlamaSpec, NglPlan, RetryPolicy, RouteFilter,
        SamplingMode, SplitPlan, Strategy,
    };

    fn endpoint(id: &str, model: &str) -> EndpointRecord {
        EndpointRecord {
            id: BackendId::parse(id).expect("id"),
            spec: EndpointSpec::LocalLlama(LocalLlamaSpec {
                build: BuildId::parse("build-vulkan").expect("build"),
                model_path: model.to_string(),
                mmproj: None,
                alias_flag: "m".to_string(),
                host: "127.0.0.1".to_string(),
                port: None,
                ctx: None,
                parallel: None,
                kv_type: None,
                ngl: NglPlan::Auto,
                split: SplitPlan::default(),
                mode: SamplingMode::Thinking,
                flash_attn: None,
                api_key: None,
                extra_args: Vec::new(),
            }),
            desired: DesiredState::Running,
            proc: None,
            port: None,
            log_path: None,
            started_at_unix: 0,
            fit: None,
            adopted: false,
            alias_bindings: Vec::new(),
        }
    }

    fn route(alias: &str, is_default: bool) -> ModelRoute {
        ModelRoute {
            alias: Alias::parse(alias).expect("alias"),
            targets: Vec::new(),
            strategy: Strategy::FirstHealthy,
            filter: RouteFilter::default(),
            retry: RetryPolicy::default(),
            is_default,
            description: None,
        }
    }

    #[test]
    fn the_tables_default_wins_over_the_configured_one() {
        let routes = vec![route("auto", false), route("fast", true)];
        assert_eq!(
            default_alias(&routes, "auto").expect("alias").as_str(),
            "fast"
        );
        assert_eq!(default_alias(&[], "auto").expect("alias").as_str(), "auto");
    }

    #[test]
    fn a_local_name_matches_the_id_or_the_model_path() {
        let eps = vec![endpoint(
            "local-carnice",
            "/home/andre/models/carnice-9b/Carnice-9b-Q6_K.gguf",
        )];
        assert_eq!(
            match_local(&eps, "carnice").expect("match").as_str(),
            "local-carnice"
        );
        assert_eq!(
            match_local(&eps, "Q6_K").expect("match").as_str(),
            "local-carnice"
        );
        assert!(match_local(&eps, "llama").is_err());
    }

    #[test]
    fn an_ambiguous_local_name_lists_the_candidates() {
        let eps = vec![
            endpoint("local-carnice-a", "/m/Carnice-Q6_K.gguf"),
            endpoint("local-carnice-b", "/m/Carnice-Q4_K.gguf"),
        ];
        let e = match_local(&eps, "carnice").expect_err("must not guess");
        assert!(e.to_string().contains("ambiguous"), "{e}");
        assert!(e.to_string().contains("local-carnice-a"), "{e}");
    }
}
