//! OWNER: unit S-06 (cli/src/{main,cli,daemon,render}.rs, cli/src/cmd/{mod,status,serve,config,rig,models,fit,endpoint,route,switch,url,version,completions}.rs). Do not edit outside that unit.
//!
//! `apexrouter route ls | show | set | rm | default | test`. `route set` prints the
//! before -> after target diff.
//!
//! Target syntax, once, here: `<backend>[:<model>]`, `tag:<tag>[:<model>]`,
//! `glob:<pattern>[:<model>]`. A [`BackendId`] cannot contain `:` — it is validated against
//! `^[a-z0-9][a-z0-9._-]{0,63}$` — so the split is unambiguous rather than heuristic.

use crate::cli::RouteCmd;
use crate::cmd::Ctx;
use crate::daemon::{Need, Serving};
use crate::render;
use apexrouter_protocol::{
    Alias, BackendId, BackendSelector, ModelRoute, RetryPolicy, RouteFilter, RouteTarget,
    SmokeProbe, Strategy,
};

/// Run `apexrouter route …`.
///
/// # Errors
/// A `$STATE` read failure, a daemon that will not answer, or an invalid alias/target.
pub async fn run(ctx: &Ctx, cmd: &RouteCmd) -> anyhow::Result<()> {
    match cmd {
        RouteCmd::Ls(args) => {
            let serving = ctx.serving(Need::ReadState).await?;
            let routes = load(ctx, &serving).await?;
            if args.json {
                return render::print_json(
                    serving.served_by(),
                    render::now_unix(),
                    serving.is_offline(),
                    &routes,
                );
            }
            if serving.is_offline() {
                render::print_offline_notice();
            }
            render::print_table(
                &["ALIAS", "DEFAULT", "STRATEGY", "RETRIES", "TARGETS"],
                routes.iter().map(row).collect(),
            );
            Ok(())
        }
        RouteCmd::Show { alias, json } => {
            let serving = ctx.serving(Need::ReadState).await?;
            let routes = load(ctx, &serving).await?;
            let r = routes
                .iter()
                .find(|r| r.alias.as_str() == alias)
                .ok_or_else(|| anyhow::anyhow!("no route named `{alias}`"))?;
            if *json {
                return render::print_json(
                    serving.served_by(),
                    render::now_unix(),
                    serving.is_offline(),
                    r,
                );
            }
            if serving.is_offline() {
                render::print_offline_notice();
            }
            print_route(r);
            Ok(())
        }
        RouteCmd::Set(args) => {
            let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
            let alias = parse_alias(&args.alias)?;
            let existing: Vec<ModelRoute> = client.get("/v1/routes").await?;
            let before = existing.iter().find(|r| r.alias == alias).cloned();

            let targets = args
                .targets
                .iter()
                .map(|t| parse_target(t))
                .collect::<anyhow::Result<Vec<_>>>()?;
            let route = merge(before.clone(), alias.clone(), targets, args)?;

            let after: ModelRoute = client
                .put(&format!("/v1/routes/{}", alias.as_str()), &route)
                .await?;
            if args.json {
                return render::print_json(
                    apexrouter_protocol::ServedBy::Daemon,
                    render::now_unix(),
                    false,
                    &after,
                );
            }
            render::print_line(&format!(
                "{}  {}  ->  {}",
                alias.as_str(),
                before
                    .as_ref()
                    .map(targets_label)
                    .unwrap_or_else(|| "(new)".to_string()),
                targets_label(&after)
            ));
            Ok(())
        }
        RouteCmd::Rm { alias } => {
            let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
            client
                .delete(&format!("/v1/routes/{}", parse_alias(alias)?.as_str()))
                .await?;
            render::print_line(&format!("removed {alias}"));
            Ok(())
        }
        RouteCmd::Default { alias } => {
            let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
            let alias = parse_alias(alias)?;
            let _: serde_json::Value = client
                .post(
                    "/v1/routes/default",
                    &serde_json::json!({ "alias": alias.as_str() }),
                )
                .await?;
            render::print_line(&format!("default alias is now {}", alias.as_str()));
            Ok(())
        }
        RouteCmd::Test { alias, json } => {
            let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
            let alias = parse_alias(alias)?;
            let probe: SmokeProbe = client
                .post(
                    &format!("/v1/routes/{}/test", alias.as_str()),
                    &serde_json::json!({}),
                )
                .await?;
            if *json {
                return render::print_json(
                    apexrouter_protocol::ServedBy::Daemon,
                    render::now_unix(),
                    false,
                    &probe,
                );
            }
            render::print_line(&format!(
                "{}  {}  {} ms  ttft {}  {} tok/s  {}",
                alias.as_str(),
                if probe.ok { "pass" } else { "FAIL" },
                probe.ms,
                render::dash(probe.ttft_ms),
                render::dash(probe.tok_per_s),
                probe.detail
            ));
            Ok(())
        }
    }
}

/// Every route: from the daemon when there is one, from `$STATE` when there is not.
///
/// # Errors
/// A `$STATE` read failure, or a daemon that will not answer.
pub async fn load(ctx: &Ctx, serving: &Serving) -> anyhow::Result<Vec<ModelRoute>> {
    match serving {
        Serving::Daemon(c) => Ok(c.get::<Vec<ModelRoute>>("/v1/routes").await?),
        Serving::Offline(store) => Ok(store.with_state_lock_shared(|| store.load_routes())?.routes),
        Serving::None(_) => {
            let store = apexrouter_core::store::Store::new(ctx.paths.clone());
            Ok(store.load_routes()?.routes)
        }
    }
}

/// Fold the operator's flags onto whatever the route already was.
///
/// An absent flag means "leave it alone", which is what makes
/// `route set auto --target x` safe to run against a route that already has a filter.
///
/// # Errors
/// An invalid `--require-tag` or ratio.
fn merge(
    before: Option<ModelRoute>,
    alias: Alias,
    targets: Vec<RouteTarget>,
    args: &crate::cli::RouteSetArgs,
) -> anyhow::Result<ModelRoute> {
    let mut route = before.unwrap_or(ModelRoute {
        alias: alias.clone(),
        targets: Vec::new(),
        strategy: Strategy::FirstHealthy,
        filter: RouteFilter::default(),
        retry: RetryPolicy::default(),
        is_default: false,
        description: None,
    });
    route.alias = alias;
    route.targets = targets;
    if let Some(s) = args.strategy {
        route.strategy = s.into();
    }
    if let Some(f) = args.failover() {
        route.retry.failover = f;
    }
    if let Some(n) = args.retries {
        route.retry.attempts = n.max(1);
    }
    if !args.require_tags.is_empty() {
        route.filter.require_tags = args.require_tags.clone();
    }
    if let Some(c) = args.max_cost {
        route.filter.max_cost_per_mtok = Some(apexrouter_protocol::Money::from_usd(c));
    }
    if let Some(n) = args.min_ctx {
        route.filter.min_ctx = Some(n);
    }
    Ok(route)
}

/// Parse an alias, with the charset rule in the error rather than in a manual.
///
/// # Errors
/// When the alias does not match `^[a-z0-9][a-z0-9._-]{0,63}$`.
pub fn parse_alias(s: &str) -> anyhow::Result<Alias> {
    Alias::parse(s).map_err(|e| anyhow::anyhow!("`{s}` is not a valid alias: {e}"))
}

/// Parse one `--target`.
///
/// # Errors
/// An unparseable selector or backend id.
pub fn parse_target(s: &str) -> anyhow::Result<RouteTarget> {
    let mut parts = s.splitn(3, ':');
    let head = parts.next().unwrap_or_default();
    let (backend, model) = match head {
        "tag" => {
            let tag = parts
                .next()
                .filter(|t| !t.is_empty())
                .ok_or_else(|| anyhow::anyhow!("`{s}` needs a tag: tag:<tag>[:<model>]"))?;
            (BackendSelector::Tag(tag.to_string()), parts.next())
        }
        "glob" => {
            let pat = parts
                .next()
                .filter(|t| !t.is_empty())
                .ok_or_else(|| anyhow::anyhow!("`{s}` needs a pattern: glob:<pat>[:<model>]"))?;
            (BackendSelector::Glob(pat.to_string()), parts.next())
        }
        id => {
            let backend = BackendId::parse(id)
                .map_err(|e| anyhow::anyhow!("`{id}` is not a valid backend id: {e}"))?;
            // Only two segments are meaningful here: `<backend>:<model>`. A model id may
            // itself contain `:`, so the remainder is taken whole.
            let rest: Vec<&str> = parts.collect();
            let model = if rest.is_empty() {
                None
            } else {
                Some(rest.join(":"))
            };
            return Ok(RouteTarget {
                backend: BackendSelector::Id(backend),
                model: model.filter(|m| !m.is_empty()),
                weight: 1,
            });
        }
    };
    Ok(RouteTarget {
        backend,
        model: model.map(str::to_string).filter(|m| !m.is_empty()),
        weight: 1,
    })
}

/// One target, as the tables and the diff print it.
pub fn target_label(t: &RouteTarget) -> String {
    let sel = match &t.backend {
        BackendSelector::Id(id) => id.as_str().to_string(),
        BackendSelector::Tag(tag) => format!("tag:{tag}"),
        BackendSelector::Glob(g) => format!("glob:{g}"),
    };
    match &t.model {
        Some(m) => format!("{sel}:{m}"),
        None => sel,
    }
}

/// A route's whole chain on one line.
pub fn targets_label(r: &ModelRoute) -> String {
    if r.targets.is_empty() {
        return "(none)".to_string();
    }
    r.targets
        .iter()
        .map(target_label)
        .collect::<Vec<_>>()
        .join(" -> ")
}

/// One row of the route table.
fn row(r: &ModelRoute) -> Vec<String> {
    vec![
        r.alias.as_str().to_string(),
        if r.is_default {
            "yes".to_string()
        } else {
            String::new()
        },
        render::variant(&r.strategy),
        format!(
            "{}{}",
            r.retry.attempts,
            if r.retry.failover { " +failover" } else { "" }
        ),
        targets_label(r),
    ]
}

/// The detail view.
fn print_route(r: &ModelRoute) {
    render::print_line(&format!(
        "{}{}",
        r.alias.as_str(),
        if r.is_default { "  (default)" } else { "" }
    ));
    render::print_line(&format!("  strategy  {}", render::variant(&r.strategy)));
    render::print_line(&format!(
        "  retry     {} attempt(s), failover {}, honour Retry-After {}",
        r.retry.attempts, r.retry.failover, r.retry.honor_retry_after
    ));
    for t in &r.targets {
        render::print_line(&format!("  target    {}", target_label(t)));
    }
    if !r.filter.require_tags.is_empty() {
        render::print_line(&format!("  require   {}", r.filter.require_tags.join(", ")));
    }
    if let Some(c) = r.filter.min_ctx {
        render::print_line(&format!("  min ctx   {c}"));
    }
    if let Some(m) = r.filter.max_cost_per_mtok {
        render::print_line(&format!("  max cost  ${:.4}/Mtok", m.as_usd()));
    }
    if let Some(d) = &r.description {
        render::print_line(&format!("  note      {d}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_backend_id_is_an_id_selector() {
        let t = parse_target("local-carnice").expect("target");
        assert_eq!(
            t.backend,
            BackendSelector::Id(BackendId::parse("local-carnice").expect("id"))
        );
        assert_eq!(t.model, None);
        assert_eq!(target_label(&t), "local-carnice");
    }

    #[test]
    fn a_backend_and_model_split_on_the_first_colon() {
        let t = parse_target("together:meta-llama/Llama-3.3-70B").expect("target");
        assert_eq!(t.model.as_deref(), Some("meta-llama/Llama-3.3-70B"));
        assert_eq!(target_label(&t), "together:meta-llama/Llama-3.3-70B");
    }

    #[test]
    fn tag_and_glob_selectors_keep_their_optional_model() {
        let t = parse_target("tag:rented").expect("target");
        assert_eq!(t.backend, BackendSelector::Tag("rented".to_string()));
        let t = parse_target("glob:vast-*:qwen").expect("target");
        assert_eq!(t.backend, BackendSelector::Glob("vast-*".to_string()));
        assert_eq!(t.model.as_deref(), Some("qwen"));
        assert_eq!(target_label(&t), "glob:vast-*:qwen");
    }

    #[test]
    fn an_invalid_backend_id_says_so_rather_than_becoming_a_tag() {
        let e = parse_target("Not An Id").expect_err("must fail");
        assert!(e.to_string().contains("not a valid backend id"), "{e}");
        assert!(parse_target("tag:").is_err(), "an empty tag is not a tag");
    }

    #[test]
    fn merge_leaves_untouched_fields_alone() {
        let before = ModelRoute {
            alias: Alias::parse("auto").expect("alias"),
            targets: vec![parse_target("old").expect("t")],
            strategy: Strategy::LeastBusy,
            filter: RouteFilter {
                min_ctx: Some(8_192),
                ..RouteFilter::default()
            },
            retry: RetryPolicy {
                attempts: 5,
                failover: false,
                honor_retry_after: true,
            },
            is_default: true,
            description: Some("keep me".to_string()),
        };
        let args = crate::cli::RouteSetArgs {
            alias: "auto".to_string(),
            targets: vec!["new".to_string()],
            strategy: None,
            failover: false,
            no_failover: false,
            retries: None,
            require_tags: Vec::new(),
            max_cost: None,
            min_ctx: None,
            json: false,
        };
        let after = merge(
            Some(before.clone()),
            Alias::parse("auto").expect("alias"),
            vec![parse_target("new").expect("t")],
            &args,
        )
        .expect("merge");

        assert_eq!(targets_label(&after), "new");
        assert_eq!(after.strategy, Strategy::LeastBusy);
        assert_eq!(after.retry.attempts, 5);
        assert!(!after.retry.failover);
        assert_eq!(after.filter.min_ctx, Some(8_192));
        assert!(
            after.is_default,
            "a set must not silently un-default a route"
        );
        assert_eq!(after.description.as_deref(), Some("keep me"));
    }
}
