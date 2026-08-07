//! OWNER: unit S-06 (cli/src/{main,cli,daemon,render}.rs, cli/src/cmd/{mod,status,serve,config,rig,models,fit,endpoint,route,switch,url,version,completions}.rs). Do not edit outside that unit.
//!
//! `apexrouter status [--json] [--watch]`. Class `ReadState`: with no daemon it serves from
//! `$STATE` under `LOCK_SH` and tags the output `served_by: "offline"`.
//!
//! The offline snapshot carries **facts** — endpoint records, the routing table, backends,
//! tunnels, recipes — and leaves every poller-derived field (live health, throughput, free
//! VRAM, spend) at its zero value with `stale: true`. Inventing a tok/s figure for a daemon
//! that is not running would be a lie, and `stale` is the field that says so without prose.

use crate::cli::StatusArgs;
use crate::cmd::{url, Ctx};
use crate::daemon::{Need, Serving};
use crate::render;
use apexrouter_core::catalog;
use apexrouter_core::store::Store;
use apexrouter_protocol::{
    Alias, Backend, CostEstimate, EndpointRecord, Health, ModelRoute, Money, ProxyStatus,
    RigSnapshot, ServedBy, Snapshot, Totals,
};
use std::time::Duration;

/// Run `apexrouter status`.
///
/// # Errors
/// A `$STATE` read failure, or a daemon that will not answer.
pub async fn run(ctx: &Ctx, args: &StatusArgs) -> anyhow::Result<()> {
    if !args.watch {
        return once(ctx, args).await;
    }
    let interval = Duration::from_secs(args.interval.max(1));
    loop {
        // Home the cursor and clear, but only for a terminal: `--watch | tee` must not be
        // filled with escape sequences.
        render::clear_screen();
        once(ctx, args).await?;
        tokio::time::sleep(interval).await;
    }
}

/// One rendering of the current picture.
///
/// # Errors
/// A `$STATE` read failure, or a daemon that will not answer.
async fn once(ctx: &Ctx, args: &StatusArgs) -> anyhow::Result<()> {
    let serving = ctx.serving(Need::ReadState).await?;
    let snap = match &serving {
        Serving::Daemon(c) => c.snapshot().await?,
        Serving::Offline(store) => offline_snapshot(ctx, store)?,
        Serving::None(_) => offline_snapshot(ctx, &Store::new(ctx.paths.clone()))?,
    };

    if args.json {
        return render::print_json(snap.served_by, snap.as_of_unix, snap.stale, &snap);
    }
    print_human(&snap);
    Ok(())
}

/// Assemble a [`Snapshot`] from `$STATE` alone, under `LOCK_SH`.
///
/// # Errors
/// A `$STATE` read failure, or a `default_alias` that is not a valid alias.
pub fn offline_snapshot(ctx: &Ctx, store: &Store) -> anyhow::Result<Snapshot> {
    let (endpoints, routes, backends, tunnels, services, studio) =
        store.with_state_lock_shared(|| {
            let endpoints = store.list_endpoints()?;
            let routes = store.load_routes()?;
            let backends = store.load_backends()?;
            let tunnels = store.load_tunnels()?;
            let services = store.load_services()?;
            let studio = store.load_studio()?;
            Ok((endpoints, routes, backends, tunnels, services, studio))
        })?;
    let cat = catalog::load(&ctx.paths).unwrap_or_default();

    let (proxy_url, _) = url::proxy_base(ctx)?;
    let (control_url, _) = url::control_base(ctx)?;
    let default_alias = Alias::parse(routes.default_alias.as_str()).map_err(|e| {
        anyhow::anyhow!(
            "the default alias in {} is not a valid alias: {e}",
            ctx.paths.routes_file().display()
        )
    })?;

    Ok(Snapshot {
        product: apexrouter_protocol::PRODUCT.to_string(),
        version: apexrouter_protocol::VERSION.to_string(),
        served_by: ServedBy::Offline,
        as_of_unix: render::now_unix(),
        // Everything a poller would have filled in is absent, and this is how a script
        // finds out without parsing prose.
        stale: true,
        proxy: ProxyStatus {
            base_url: format!("{proxy_url}/v1"),
            control_url,
            uptime_secs: 0.0,
            inflight: 0,
            req_per_min: 0.0,
            tok_per_s: 0.0,
            default_alias,
            table_valid: true,
            table_error: None,
        },
        backends,
        routes: routes.routes,
        endpoints,
        rig: RigSnapshot::default(),
        instances: Vec::new(),
        tunnels,
        services,
        studio,
        providers: Vec::new(),
        recipes: cat.recipes,
        profiles: cat.profiles,
        totals: Totals {
            spend_24h: CostEstimate::Unknown,
            spend_7d: CostEstimate::Unknown,
            tokens_24h: 0,
            vast_credit: None,
            burn_rate_usd_hr: Money::ZERO,
            burn_down_hours: None,
        },
        alerts: Vec::new(),
        jobs: Vec::new(),
    })
}

/// The human rendering: one header line, the offline notice when it applies, then tables.
pub fn print_human(snap: &Snapshot) {
    render::print_line(&format!(
        "{} {}  ·  {}",
        snap.product, snap.version, snap.proxy.base_url
    ));
    if snap.served_by == ServedBy::Offline {
        render::print_offline_notice();
    } else {
        render::print_line(&format!(
            "up {}  ·  {} in flight  ·  {:.1} req/min  ·  {:.1} tok/s",
            render::human_secs(snap.proxy.uptime_secs as i64),
            snap.proxy.inflight,
            snap.proxy.req_per_min,
            snap.proxy.tok_per_s
        ));
    }
    if !snap.proxy.table_valid {
        render::print_line(&format!(
            "WARNING  the routing table on disk did not compile; the previous one is still \
             serving: {}",
            snap.proxy.table_error.as_deref().unwrap_or("no detail")
        ));
    }
    for a in &snap.alerts {
        render::print_line(&format!(
            "{:<8} {}{}",
            format!("{:?}", a.level).to_uppercase(),
            a.message,
            a.action
                .as_ref()
                .map(|x| format!(" — {x}"))
                .unwrap_or_default()
        ));
    }

    render::print_blank();
    render::print_table(
        &["ALIAS", "DEFAULT", "STRATEGY", "TARGETS"],
        snap.routes.iter().map(route_row).collect(),
    );

    render::print_blank();
    render::print_table(
        &["ENDPOINT", "STATE", "PORT", "PID", "ALIASES", "UPTIME"],
        snap.endpoints.iter().map(endpoint_row).collect(),
    );

    if !snap.backends.is_empty() {
        render::print_blank();
        render::print_table(
            &["BACKEND", "KIND", "HEALTH", "BASE URL", "TAGS"],
            snap.backends.iter().map(backend_row).collect(),
        );
    }
}

/// One row of the alias table.
fn route_row(r: &ModelRoute) -> Vec<String> {
    vec![
        r.alias.as_str().to_string(),
        if r.is_default {
            "yes".to_string()
        } else {
            String::new()
        },
        render::variant(&r.strategy),
        r.targets
            .iter()
            .map(crate::cmd::route::target_label)
            .collect::<Vec<_>>()
            .join(" -> "),
    ]
}

/// One row of the endpoint table.
fn endpoint_row(e: &EndpointRecord) -> Vec<String> {
    vec![
        e.id.as_str().to_string(),
        render::variant(&e.desired),
        render::dash(e.port),
        render::dash(e.proc.as_ref().map(|p| p.pid)),
        e.alias_bindings
            .iter()
            .map(|a| a.as_str())
            .collect::<Vec<_>>()
            .join(","),
        if e.started_at_unix > 0 {
            render::human_secs(render::now_unix() - e.started_at_unix)
        } else {
            String::new()
        },
    ]
}

/// One row of the backend table.
fn backend_row(b: &Backend) -> Vec<String> {
    vec![
        b.id.as_str().to_string(),
        render::variant(&b.kind),
        health_label(&b.health),
        b.base_url.clone(),
        b.tags.join(","),
    ]
}

/// A health value as one word plus its most useful number.
pub fn health_label(h: &Health) -> String {
    match h {
        Health::Unknown => "unknown".to_string(),
        Health::Starting { phase, .. } => {
            format!("starting/{}", format!("{phase:?}").to_lowercase())
        }
        Health::Ready {
            slots_busy,
            slots_total,
            ..
        } => format!("ready {slots_busy}/{slots_total}"),
        Health::Degraded {
            consecutive_failures,
            ..
        } => format!("degraded ({consecutive_failures} fails)"),
        Health::Down { .. } => "down".to_string(),
        Health::Draining { in_flight } => format!("draining ({in_flight})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use crate::daemon::testenv;
    use apexrouter_protocol::{
        BackendId, BuildId, DesiredState, EndpointSpec, LocalLlamaSpec, NglPlan, RouteFile,
        RouteTarget, SamplingMode, SplitPlan, Strategy,
    };
    use clap::Parser;

    fn record(id: &str) -> EndpointRecord {
        EndpointRecord {
            id: BackendId::parse(id).expect("id"),
            spec: EndpointSpec::LocalLlama(LocalLlamaSpec {
                build: BuildId::parse("build-vulkan").expect("build"),
                model_path: "/home/andre/models/carnice-9b/Carnice-9b-Q6_K.gguf".to_string(),
                mmproj: None,
                alias_flag: "carnice".to_string(),
                host: "127.0.0.1".to_string(),
                port: Some(8100),
                ctx: Some(32_768),
                parallel: Some(1),
                kv_type: None,
                ngl: NglPlan::All,
                split: SplitPlan::default(),
                mode: SamplingMode::Thinking,
                flash_attn: None,
                api_key: None,
                extra_args: Vec::new(),
            }),
            desired: DesiredState::Running,
            proc: None,
            port: Some(8100),
            log_path: None,
            started_at_unix: 0,
            fit: None,
            adopted: false,
            alias_bindings: vec![Alias::parse("auto").expect("alias")],
        }
    }

    /// The acceptance case: nothing is running, and `status` still answers — from `$STATE`,
    /// tagged `served_by: "offline"`, with `stale: true`.
    #[test]
    fn status_on_a_machine_where_nothing_runs_is_served_offline() {
        let _guard = testenv::lock();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("APEXROUTER_HOME", dir.path());
        std::env::remove_var("APEXROUTER_URL");

        let cli = Cli::parse_from(["apexrouter", "status", "--json"]);
        let ctx = Ctx::load(&cli).expect("ctx");
        ctx.paths.ensure_layout().expect("layout");
        let store = Store::new(ctx.paths.clone());

        // Facts on disk: one endpoint record and one route.
        store.put_endpoint(&record("local-carnice")).expect("write");
        store
            .save_routes(&RouteFile {
                schema_version: 1,
                default_alias: Alias::parse("auto").expect("alias"),
                routes: vec![ModelRoute {
                    alias: Alias::parse("auto").expect("alias"),
                    targets: vec![RouteTarget {
                        backend: apexrouter_protocol::BackendSelector::Id(
                            BackendId::parse("local-carnice").expect("id"),
                        ),
                        model: None,
                        weight: 1,
                    }],
                    strategy: Strategy::FirstHealthy,
                    filter: Default::default(),
                    retry: Default::default(),
                    is_default: true,
                    description: None,
                }],
            })
            .expect("routes");

        let snap = offline_snapshot(&ctx, &store).expect("snapshot");
        assert_eq!(snap.served_by, ServedBy::Offline);
        assert!(snap.stale, "poller-derived fields are absent, and say so");
        assert_eq!(snap.endpoints.len(), 1);
        assert_eq!(snap.routes.len(), 1);
        assert_eq!(snap.proxy.base_url, "http://127.0.0.1:8888/v1");
        assert_eq!(snap.proxy.default_alias.as_str(), "auto");

        // ... and that is what the `--json` envelope reports.
        let env = render::envelope_value(snap.served_by, snap.as_of_unix, snap.stale, &snap)
            .expect("env");
        assert_eq!(env["served_by"], serde_json::Value::from("offline"));
        assert_eq!(env["stale"], serde_json::Value::from(true));

        // The human rendering must not panic on an empty rig or empty backends.
        print_human(&snap);
        std::env::remove_var("APEXROUTER_HOME");
    }

    #[test]
    fn health_renders_as_one_word_plus_its_number() {
        assert_eq!(
            health_label(&Health::Ready {
                since_unix: 0,
                slots_busy: 1,
                slots_total: 4,
                tps_p50: None
            }),
            "ready 1/4"
        );
        assert_eq!(
            health_label(&Health::Draining { in_flight: 2 }),
            "draining (2)"
        );
        assert_eq!(health_label(&Health::Unknown), "unknown");
    }
}
