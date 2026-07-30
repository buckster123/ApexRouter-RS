//! OWNER: unit R-01 (router/src/table.rs, router/src/registry.rs). Do not edit outside that
//! unit.
//!
//! The compiled routing table.
//!
//! Reload is parse → compile → validate → `ArcSwap::store`. **A failed compile keeps the
//! running table**, raises an `Alert`, and shows red in both GUIs and in `apexrouter
//! status`. Compile-time validation rejects: a dangling target, a duplicate alias, an alias
//! that shadows a live upstream id (unless `allow_shadow`), an unsatisfiable `require_tags`,
//! and `Strategy::Cheapest` on a route where no target has a price model *and* no
//! `tps_hint` — that would be an invented ordering.
//!
//! NOTE for a later stage: no published type carries an `allow_shadow` flag — neither
//! `ModelRoute` nor `RouterCfg` has the field — so the shadow rule is currently enforced with
//! no escape hatch. Adding the flag is a protocol change and belongs to whoever owns those
//! types, not to R-01.

use crate::registry::{BackendRegistry, LiveBackend};
use apexrouter_core::config::Config;
use apexrouter_protocol::{
    Alias, Backend, BackendId, BackendSelector, CostEstimate, Health, RetryPolicy, RouteFile,
    RouteFilter, Severity, Strategy, ValidationIssue, ValidationReport, LEGACY_MODEL_NAMES,
};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Every successful compile takes the next number. Process-wide, because "is this reader
/// holding a stale table?" is only answerable if the counter never repeats.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// One expanded target: exactly one live backend, plus what the route says to send it.
///
/// Selectors are expanded here, at compile time, so `resolve()` does no matching work. The
/// `weight` is carried rather than pre-expanded, because it only means anything under
/// `RoundRobin` — R-02 expands it there and nowhere else.
pub(crate) struct CompiledTarget {
    /// Live state, including the permit pool and the breaker.
    pub(crate) backend: Arc<LiveBackend>,
    /// The upstream model id this route overrides to, or `None` to send the backend's own.
    pub(crate) model: Option<String>,
    /// Used by `RoundRobin`.
    pub(crate) weight: u32,
}

/// One alias, resolved down to the backends it can actually reach.
pub struct CompiledRoute {
    /// The alias this route answers to.
    pub(crate) alias: Alias,
    /// Ordered, with every selector already expanded to live backends.
    pub(crate) targets: Vec<CompiledTarget>,
    /// How to order candidates.
    pub(crate) strategy: Strategy,
    /// Applied per request, because health, context and price move between recompiles.
    pub(crate) filter: RouteFilter,
    /// Retry and failover.
    pub(crate) retry: RetryPolicy,
    /// Whether this route declared itself the default.
    pub(crate) is_default: bool,
}

/// The read-only structure the request path loads on every request.
pub struct RoutingTable {
    /// Rule 1.
    pub(crate) by_alias: HashMap<Alias, CompiledRoute>,
    /// Rules 3 and 4. **This is what makes every existing client work unchanged.**
    pub(crate) by_upstream_id: HashMap<String, Vec<Arc<LiveBackend>>>,
    /// Rule 2, the explicit pin.
    pub(crate) by_id: HashMap<BackendId, Arc<LiveBackend>>,
    /// Where rule 5 and `unknown_model = "fallback"` land.
    pub(crate) default_alias: Alias,
    /// `""`, `"x"`, `"auto"`, `"default"` — why `smoke.sh` keeps working.
    pub(crate) legacy_model_names: HashSet<String>,
    /// Bumped on every recompile, so a stale reader is detectable.
    pub(crate) generation: u64,
    /// `[router] implicit_strategy`, baked in at compile time because `resolve()` is given no
    /// config and must still answer rule 4.
    pub(crate) implicit_strategy: Strategy,
}

/// Compiles a `RouteFile` plus a registry into a `RoutingTable`.
pub struct TableBuilder;

/// One error, with the fix spelled out. Every issue this module raises blocks the compile.
fn error(field: impl Into<String>, message: impl Into<String>, fix: &str) -> ValidationIssue {
    ValidationIssue {
        field: field.into(),
        severity: Severity::Error,
        message: message.into(),
        fix: Some(fix.to_owned()),
    }
}

/// `*` and `?` only. Written by hand because `glob` is not a dependency of this crate, and
/// because a `BackendId` is ASCII by construction so bytes are safe to walk.
fn glob_match(pattern: &str, s: &str) -> bool {
    let (p, t) = (pattern.as_bytes(), s.as_bytes());
    let (mut pi, mut ti) = (0usize, 0usize);
    // Where to resume if the current `*` turns out to have eaten too little.
    let (mut star, mut resume) = (None, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            resume = ti;
            pi += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            resume += 1;
            ti = resume;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// The wire spelling of a strategy, as `[router] implicit_strategy` writes it.
fn parse_strategy(s: &str) -> Option<Strategy> {
    match s {
        "first_healthy" => Some(Strategy::FirstHealthy),
        "round_robin" => Some(Strategy::RoundRobin),
        "least_busy" => Some(Strategy::LeastBusy),
        "cheapest" => Some(Strategy::Cheapest),
        _ => None,
    }
}

/// The throughput a backend has actually shown, which is the only honest `tps_hint`.
fn tps_hint(b: &Backend) -> Option<f32> {
    match &b.health {
        Health::Ready { tps_p50, .. } => *tps_p50,
        _ => None,
    }
}

/// True when this backend can be ordered by price without inventing a number.
fn has_usable_price(b: &Backend) -> bool {
    match &b.price {
        None => false,
        Some(p) => !matches!(p.per_mtok(tps_hint(b)), CostEstimate::Unknown),
    }
}

/// Which registered backends a selector names. Disabled ones are included: a target pointing
/// at a backend somebody switched off is *disabled*, not *dangling*, and the two want
/// different messages.
fn select<'a>(sel: &BackendSelector, live: &'a [Arc<LiveBackend>]) -> Vec<&'a Arc<LiveBackend>> {
    match sel {
        BackendSelector::Id(id) => live.iter().filter(|b| &b.id == id).collect(),
        BackendSelector::Tag(tag) => live
            .iter()
            .filter(|b| b.meta.load().tags.iter().any(|t| t == tag))
            .collect(),
        BackendSelector::Glob(g) => live
            .iter()
            .filter(|b| glob_match(g, b.id.as_str()))
            .collect(),
    }
}

/// How a selector reads in an error message.
fn describe(sel: &BackendSelector) -> String {
    match sel {
        BackendSelector::Id(id) => format!("id \"{id}\""),
        BackendSelector::Tag(t) => format!("tag \"{t}\""),
        BackendSelector::Glob(g) => format!("glob \"{g}\""),
    }
}

impl TableBuilder {
    /// Clone `Arc`s out of the registry so live state is carried across, and validate.
    ///
    /// Returns a `ValidationReport` rather than an opaque error, so the caller can keep the
    /// old table serving and render exactly what was wrong.
    ///
    /// What is rejected, and why each one is worth a failed reload:
    ///
    /// * a **dangling target** — a selector no registered backend matches;
    /// * a **duplicate alias** — the second one would silently never fire;
    /// * an **alias that shadows a live upstream id** — rule 1 beats rule 3, so the model id
    ///   a client has always sent would quietly start going somewhere else;
    /// * an **unsatisfiable `require_tags`** — the route can never dispatch;
    /// * **`Strategy::Cheapest` with no priced target** — the ordering would be invented.
    ///
    /// An empty `routes` list compiles: rules 2, 3 and 4 still work off the registry alone,
    /// which is what makes a zero-config first start serve rather than refuse.
    pub fn compile(
        cfg: &Config,
        routes: &RouteFile,
        reg: &BackendRegistry,
    ) -> std::result::Result<RoutingTable, ValidationReport> {
        let mut issues: Vec<ValidationIssue> = Vec::new();

        // Sorted by id, so every map and every candidate list below is deterministic.
        let live = reg.all();
        let enabled: Vec<Arc<LiveBackend>> = live
            .iter()
            .filter(|b| b.meta.load().enabled)
            .map(Arc::clone)
            .collect();

        let by_id: HashMap<BackendId, Arc<LiveBackend>> = enabled
            .iter()
            .map(|b| (b.id.clone(), Arc::clone(b)))
            .collect();

        // Declared models and probed models both count: a cold index must not make rule 3
        // miss for a backend whose record already tells us what it serves.
        let mut by_upstream_id: HashMap<String, Vec<Arc<LiveBackend>>> = HashMap::new();
        for b in &enabled {
            let mut ids: Vec<String> = b.meta.load().models.iter().map(|m| m.id.clone()).collect();
            ids.extend(b.model_index.load().iter().cloned());
            ids.sort();
            ids.dedup();
            for id in ids {
                by_upstream_id.entry(id).or_default().push(Arc::clone(b));
            }
        }

        let implicit_strategy = match parse_strategy(&cfg.router.implicit_strategy) {
            Some(s) => s,
            None => {
                issues.push(error(
                    "router.implicit_strategy",
                    format!(
                        "\"{}\" is not a strategy this build implements",
                        cfg.router.implicit_strategy
                    ),
                    "use one of: first_healthy, round_robin, least_busy, cheapest",
                ));
                Strategy::FirstHealthy
            }
        };

        let mut by_alias: HashMap<Alias, CompiledRoute> = HashMap::new();
        let mut defaults: Vec<&str> = Vec::new();

        for (i, r) in routes.routes.iter().enumerate() {
            if by_alias.contains_key(&r.alias) {
                issues.push(error(
                    format!("routes[{i}].alias"),
                    format!("alias \"{}\" is declared more than once", r.alias),
                    "delete one of the two routes, or rename it",
                ));
                continue;
            }
            if by_upstream_id.contains_key(r.alias.as_str()) {
                issues.push(error(
                    format!("routes[{i}].alias"),
                    format!(
                        "alias \"{}\" shadows the upstream model id of a live backend; \
                         rule 1 beats rule 3, so requests for that model id would silently \
                         change destination",
                        r.alias
                    ),
                    "rename the alias, or disable the backend advertising that model id",
                ));
            }
            if r.is_default {
                defaults.push(r.alias.as_str());
            }
            if r.targets.is_empty() {
                issues.push(error(
                    format!("routes[{i}].targets"),
                    format!(
                        "route \"{}\" has no targets and can never dispatch",
                        r.alias
                    ),
                    "add a target, or delete the route",
                ));
            }

            // Every selector expanded to the live backends it names, in registry order.
            let mut targets: Vec<CompiledTarget> = Vec::new();
            for (j, t) in r.targets.iter().enumerate() {
                let matched = select(&t.backend, &live);
                if matched.is_empty() {
                    issues.push(error(
                        format!("routes[{i}].targets[{j}].backend"),
                        format!("no backend matches {}", describe(&t.backend)),
                        "register the backend first, or point the target somewhere live",
                    ));
                    continue;
                }
                for b in matched {
                    if !b.meta.load().enabled {
                        continue;
                    }
                    targets.push(CompiledTarget {
                        backend: Arc::clone(b),
                        model: t.model.clone(),
                        weight: t.weight,
                    });
                }
            }

            if !targets.is_empty() {
                if !r.filter.require_tags.is_empty()
                    && !targets.iter().any(|t| {
                        let tags = &t.backend.meta.load().tags;
                        r.filter
                            .require_tags
                            .iter()
                            .all(|want| tags.iter().any(|have| have == want))
                    })
                {
                    issues.push(error(
                        format!("routes[{i}].filter.require_tags"),
                        format!(
                            "no target of \"{}\" carries every tag in [{}]",
                            r.alias,
                            r.filter.require_tags.join(", ")
                        ),
                        "drop a tag, tag a backend, or add a target that has them",
                    ));
                }

                if !r.filter.exclude_tags.is_empty()
                    && targets.iter().all(|t| {
                        let tags = &t.backend.meta.load().tags;
                        r.filter
                            .exclude_tags
                            .iter()
                            .any(|bad| tags.iter().any(|have| have == bad))
                    })
                {
                    issues.push(error(
                        format!("routes[{i}].filter.exclude_tags"),
                        format!(
                            "every target of \"{}\" is excluded by [{}]",
                            r.alias,
                            r.filter.exclude_tags.join(", ")
                        ),
                        "drop an exclusion, or add a target that is not excluded",
                    ));
                }

                if r.strategy == Strategy::Cheapest
                    && !targets
                        .iter()
                        .any(|t| has_usable_price(&t.backend.meta.load()))
                {
                    issues.push(error(
                        format!("routes[{i}].strategy"),
                        format!(
                            "\"{}\" is cheapest, but no target has a price model and no \
                             per-hour target has an observed throughput to normalise with",
                            r.alias
                        ),
                        "price a target, wait for a tps_p50 observation, or use first_healthy",
                    ));
                }
            }

            by_alias.insert(
                r.alias.clone(),
                CompiledRoute {
                    alias: r.alias.clone(),
                    targets,
                    strategy: r.strategy,
                    filter: r.filter.clone(),
                    retry: r.retry,
                    is_default: r.is_default,
                },
            );
        }

        if defaults.len() > 1 {
            issues.push(error(
                "routes",
                format!(
                    "{} routes claim to be the default: {}",
                    defaults.len(),
                    defaults.join(", ")
                ),
                "set is_default on exactly one route",
            ));
        }

        // An empty table is legal — nothing can dangle. A non-empty one whose default alias
        // names no route is not: rule 5 would have nowhere to land.
        if !routes.routes.is_empty() && !by_alias.contains_key(&routes.default_alias) {
            issues.push(error(
                "default_alias",
                format!(
                    "default_alias \"{}\" names no route in this table",
                    routes.default_alias
                ),
                "point default_alias at an existing alias, or add that route",
            ));
        }

        if issues.iter().any(|i| i.severity == Severity::Error) {
            return Err(ValidationReport { ok: false, issues });
        }

        Ok(RoutingTable {
            by_alias,
            by_upstream_id,
            by_id,
            default_alias: routes.default_alias.clone(),
            legacy_model_names: LEGACY_MODEL_NAMES.iter().map(|s| (*s).to_owned()).collect(),
            generation: GENERATION.fetch_add(1, Ordering::Relaxed) + 1,
            implicit_strategy,
        })
    }
}

impl RoutingTable {
    /// Which generation this table is. Bumped on every successful compile.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The alias legacy and unknown model names fall through to.
    pub fn default_alias(&self) -> &Alias {
        &self.default_alias
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_core::config::RouterCfg;
    use apexrouter_protocol::{
        BackendKind, BackendLimits, CredentialSource, ModelRoute, Money, PriceModel, Protocol,
        Provenance, RouteTarget, UpstreamModel,
    };

    fn backend(id: &str) -> Backend {
        Backend {
            id: BackendId::parse(id).expect("id"),
            kind: BackendKind::LocalLlama,
            protocol: Protocol::OpenAi,
            label: id.to_owned(),
            base_url: "http://127.0.0.1:8100".into(),
            credential: CredentialSource::None,
            tags: vec!["local".into()],
            models: vec![UpstreamModel {
                id: "Carnice-9b-Q6_K".into(),
                ctx: Some(32_768),
                vision: false,
                tools: true,
            }],
            limits: BackendLimits {
                max_concurrent: 4,
                queue_depth: 8,
                ctx: Some(32_768),
                slots_total: Some(4),
            },
            price: None,
            health: Health::Unknown,
            provenance: Provenance::Spawned,
            endpoint: None,
            enabled: true,
            devices: vec![],
            last_error: None,
        }
    }

    fn route(alias: &str, sel: BackendSelector) -> ModelRoute {
        ModelRoute {
            alias: Alias::parse(alias).expect("alias"),
            targets: vec![RouteTarget {
                backend: sel,
                model: None,
                weight: 1,
            }],
            strategy: Strategy::FirstHealthy,
            filter: RouteFilter::default(),
            retry: RetryPolicy::default(),
            is_default: alias == "auto",
            description: None,
        }
    }

    fn id(s: &str) -> BackendId {
        BackendId::parse(s).expect("id")
    }

    fn file(routes: Vec<ModelRoute>) -> RouteFile {
        RouteFile {
            schema_version: 1,
            default_alias: Alias::parse("auto").expect("alias"),
            routes,
        }
    }

    fn registry_with(backends: Vec<Backend>) -> BackendRegistry {
        let cfg = RouterCfg::default();
        let reg = BackendRegistry::new();
        for b in backends {
            reg.upsert(b, &cfg);
        }
        reg
    }

    fn issues(r: &ValidationReport) -> String {
        r.issues
            .iter()
            .map(|i| format!("{}: {}", i.field, i.message))
            .collect::<Vec<_>>()
            .join(" | ")
    }

    #[test]
    fn compiles_the_indexes_every_rule_reads() {
        let reg = registry_with(vec![backend("local-carnice")]);
        let routes = file(vec![route(
            "auto",
            BackendSelector::Id(id("local-carnice")),
        )]);
        let t = match TableBuilder::compile(&Config::default(), &routes, &reg) {
            Ok(t) => t,
            Err(r) => panic!("should compile: {}", issues(&r)),
        };

        assert_eq!(t.by_alias.len(), 1);
        assert_eq!(
            t.by_alias[&Alias::parse("auto").expect("alias")]
                .targets
                .len(),
            1
        );
        assert_eq!(t.by_id.len(), 1);
        assert_eq!(t.by_upstream_id["Carnice-9b-Q6_K"].len(), 1);
        assert_eq!(t.default_alias().as_str(), "auto");
        assert_eq!(t.implicit_strategy, Strategy::FirstHealthy);
        for legacy in ["", "x", "auto", "default"] {
            assert!(t.legacy_model_names.contains(legacy), "{legacy}");
        }
    }

    #[test]
    fn the_table_holds_the_registrys_own_arcs() {
        // This is the whole structural point: a recompile hands out the same live state.
        let reg = registry_with(vec![backend("local-carnice")]);
        let live = reg.get(&id("local-carnice")).expect("registered");
        let routes = file(vec![route(
            "auto",
            BackendSelector::Id(id("local-carnice")),
        )]);
        let t = TableBuilder::compile(&Config::default(), &routes, &reg).expect("compiles");

        let in_table = &t.by_alias[&Alias::parse("auto").expect("alias")].targets[0].backend;
        assert!(Arc::ptr_eq(&live, in_table));
        assert!(Arc::ptr_eq(&live, &t.by_id[&id("local-carnice")]));
        assert!(Arc::ptr_eq(&live, &t.by_upstream_id["Carnice-9b-Q6_K"][0]));
    }

    #[test]
    fn generation_is_bumped_on_every_successful_compile() {
        let reg = registry_with(vec![backend("local-carnice")]);
        let routes = file(vec![route(
            "auto",
            BackendSelector::Id(id("local-carnice")),
        )]);
        let cfg = Config::default();
        let a = TableBuilder::compile(&cfg, &routes, &reg).expect("compiles");
        let b = TableBuilder::compile(&cfg, &routes, &reg).expect("compiles");
        assert!(b.generation() > a.generation());
    }

    #[test]
    fn an_empty_route_list_compiles_so_a_first_start_still_serves() {
        let reg = registry_with(vec![backend("local-carnice")]);
        let t = TableBuilder::compile(&Config::default(), &file(vec![]), &reg).expect("compiles");
        assert!(t.by_alias.is_empty());
        // Rules 2, 3 and 4 still have everything they need.
        assert_eq!(t.by_id.len(), 1);
        assert_eq!(t.by_upstream_id.len(), 1);
    }

    #[test]
    fn rejects_a_dangling_target_by_id_tag_and_glob() {
        let reg = registry_with(vec![backend("local-carnice")]);
        for sel in [
            BackendSelector::Id(id("not-there")),
            BackendSelector::Tag("rented".into()),
            BackendSelector::Glob("vast-*".into()),
        ] {
            let routes = file(vec![route("auto", sel.clone())]);
            let report = TableBuilder::compile(&Config::default(), &routes, &reg)
                .err()
                .unwrap_or_else(|| panic!("{sel:?} should be dangling"));
            assert!(!report.ok);
            assert_eq!(report.issues.len(), 1, "{}", issues(&report));
            assert_eq!(report.issues[0].field, "routes[0].targets[0].backend");
            assert!(report.issues[0].message.contains("no backend matches"));
        }
    }

    #[test]
    fn a_disabled_backend_is_disabled_not_dangling() {
        let mut off = backend("local-carnice");
        off.enabled = false;
        let reg = registry_with(vec![off]);
        let routes = file(vec![route(
            "auto",
            BackendSelector::Id(id("local-carnice")),
        )]);
        let t = TableBuilder::compile(&Config::default(), &routes, &reg).expect("compiles");
        // It compiles, but a disabled backend is never a candidate.
        assert!(t.by_alias[&Alias::parse("auto").expect("alias")]
            .targets
            .is_empty());
        assert!(t.by_id.is_empty());
        assert!(t.by_upstream_id.is_empty());
    }

    #[test]
    fn rejects_a_duplicate_alias() {
        let reg = registry_with(vec![backend("local-carnice")]);
        let routes = file(vec![
            route("auto", BackendSelector::Id(id("local-carnice"))),
            route("auto", BackendSelector::Id(id("local-carnice"))),
        ]);
        let report = TableBuilder::compile(&Config::default(), &routes, &reg)
            .err()
            .expect("duplicate alias");
        assert_eq!(report.issues[0].field, "routes[1].alias");
        assert!(report.issues[0].message.contains("more than once"));
    }

    #[test]
    fn rejects_an_alias_that_shadows_a_live_upstream_id() {
        // The alias charset is lowercase, so give the backend a model id an alias can equal.
        let mut b = backend("local-carnice");
        b.models[0].id = "carnice".into();
        let reg = registry_with(vec![b]);
        let routes = RouteFile {
            schema_version: 1,
            default_alias: Alias::parse("carnice").expect("alias"),
            routes: vec![route("carnice", BackendSelector::Id(id("local-carnice")))],
        };
        let report = TableBuilder::compile(&Config::default(), &routes, &reg)
            .err()
            .expect("shadowing alias");
        assert_eq!(report.issues[0].field, "routes[0].alias");
        assert!(
            report.issues[0].message.contains("shadows"),
            "{}",
            issues(&report)
        );
    }

    #[test]
    fn a_probed_model_id_shadows_just_as_a_declared_one_does() {
        let mut b = backend("local-carnice");
        b.models.clear();
        let reg = registry_with(vec![b]);
        reg.get(&id("local-carnice"))
            .expect("registered")
            .set_models(vec!["carnice".into()]);
        let routes = RouteFile {
            schema_version: 1,
            default_alias: Alias::parse("carnice").expect("alias"),
            routes: vec![route("carnice", BackendSelector::Id(id("local-carnice")))],
        };
        let report = TableBuilder::compile(&Config::default(), &routes, &reg)
            .err()
            .expect("shadowing alias");
        assert!(report.issues[0].message.contains("shadows"));
    }

    #[test]
    fn rejects_an_unsatisfiable_require_tags() {
        let reg = registry_with(vec![backend("local-carnice")]);
        let mut r = route("auto", BackendSelector::Id(id("local-carnice")));
        r.filter.require_tags = vec!["local".into(), "vision".into()];
        let report = TableBuilder::compile(&Config::default(), &file(vec![r]), &reg)
            .err()
            .expect("unsatisfiable require_tags");
        assert_eq!(report.issues[0].field, "routes[0].filter.require_tags");

        // The satisfiable case still compiles.
        let reg = registry_with(vec![backend("local-carnice")]);
        let mut r = route("auto", BackendSelector::Id(id("local-carnice")));
        r.filter.require_tags = vec!["local".into()];
        TableBuilder::compile(&Config::default(), &file(vec![r]), &reg).expect("compiles");
    }

    #[test]
    fn rejects_an_exclude_tags_that_removes_every_target() {
        let reg = registry_with(vec![backend("local-carnice")]);
        let mut r = route("auto", BackendSelector::Id(id("local-carnice")));
        r.filter.exclude_tags = vec!["local".into()];
        let report = TableBuilder::compile(&Config::default(), &file(vec![r]), &reg)
            .err()
            .expect("everything excluded");
        assert_eq!(report.issues[0].field, "routes[0].filter.exclude_tags");
    }

    #[test]
    fn rejects_cheapest_when_nothing_can_be_priced() {
        let reg = registry_with(vec![backend("local-carnice")]);
        let mut r = route("auto", BackendSelector::Id(id("local-carnice")));
        r.strategy = Strategy::Cheapest;
        let report = TableBuilder::compile(&Config::default(), &file(vec![r.clone()]), &reg)
            .err()
            .expect("cheapest with no price");
        assert_eq!(report.issues[0].field, "routes[0].strategy");

        // A per-hour box with no observed throughput is still unpriceable: normalising it
        // would invent a tok/s number, which is exactly what cost.py did.
        let mut hourly = backend("local-carnice");
        hourly.price = Some(PriceModel::PerHour {
            dph: Money::from_usd(3.34),
        });
        let reg = registry_with(vec![hourly.clone()]);
        assert!(TableBuilder::compile(&Config::default(), &file(vec![r.clone()]), &reg).is_err());

        // With an observed tps_p50 it normalises, so the ordering is real.
        hourly.health = Health::Ready {
            since_unix: 1,
            slots_busy: 0,
            slots_total: 4,
            tps_p50: Some(40.0),
        };
        let reg = registry_with(vec![hourly]);
        TableBuilder::compile(&Config::default(), &file(vec![r.clone()]), &reg).expect("compiles");

        // A per-token price needs no assumption at all.
        let mut priced = backend("local-carnice");
        priced.price = Some(PriceModel::PerToken {
            input: Money::from_usd(0.2),
            output: Money::from_usd(0.6),
        });
        let reg = registry_with(vec![priced]);
        TableBuilder::compile(&Config::default(), &file(vec![r]), &reg).expect("compiles");
    }

    #[test]
    fn rejects_a_default_alias_that_names_no_route() {
        let reg = registry_with(vec![backend("local-carnice")]);
        let routes = RouteFile {
            schema_version: 1,
            default_alias: Alias::parse("nope").expect("alias"),
            routes: vec![route("auto", BackendSelector::Id(id("local-carnice")))],
        };
        let report = TableBuilder::compile(&Config::default(), &routes, &reg)
            .err()
            .expect("dangling default alias");
        assert_eq!(report.issues[0].field, "default_alias");
    }

    #[test]
    fn rejects_a_route_with_no_targets_and_two_defaults() {
        let reg = registry_with(vec![backend("local-carnice")]);
        let mut empty = route("auto", BackendSelector::Id(id("local-carnice")));
        empty.targets.clear();
        let mut second = route("coder", BackendSelector::Id(id("local-carnice")));
        second.is_default = true;
        let report = TableBuilder::compile(&Config::default(), &file(vec![empty, second]), &reg)
            .err()
            .expect("two problems");
        let fields: Vec<&str> = report.issues.iter().map(|i| i.field.as_str()).collect();
        assert!(fields.contains(&"routes[0].targets"), "{fields:?}");
        assert!(fields.contains(&"routes"), "{fields:?}");
    }

    #[test]
    fn rejects_an_implicit_strategy_config_cannot_mean() {
        let reg = registry_with(vec![backend("local-carnice")]);
        let mut cfg = Config::default();
        cfg.router.implicit_strategy = "fastest".into();
        let routes = file(vec![route(
            "auto",
            BackendSelector::Id(id("local-carnice")),
        )]);
        let report = TableBuilder::compile(&cfg, &routes, &reg)
            .err()
            .expect("no such strategy");
        assert_eq!(report.issues[0].field, "router.implicit_strategy");
    }

    #[test]
    fn a_failed_compile_leaves_the_old_table_serving() {
        let reg = registry_with(vec![backend("local-carnice")]);
        let good = file(vec![route(
            "auto",
            BackendSelector::Id(id("local-carnice")),
        )]);
        let running = TableBuilder::compile(&Config::default(), &good, &reg).expect("compiles");
        let generation = running.generation();

        let bad = file(vec![route("auto", BackendSelector::Id(id("gone")))]);
        let report = TableBuilder::compile(&Config::default(), &bad, &reg)
            .err()
            .expect("dangling");
        assert!(!report.ok);
        assert!(!report.issues.is_empty());

        // Nothing about the running table moved: the caller simply never stored a new one.
        assert_eq!(running.generation(), generation);
        assert_eq!(running.default_alias().as_str(), "auto");
        assert_eq!(running.by_alias.len(), 1);
    }

    #[test]
    fn several_backends_serving_one_model_id_all_land_in_by_upstream_id() {
        let reg = registry_with(vec![backend("a-local"), backend("b-vast")]);
        let routes = file(vec![route("auto", BackendSelector::Glob("*".into()))]);
        let t = TableBuilder::compile(&Config::default(), &routes, &reg).expect("compiles");
        let both = &t.by_upstream_id["Carnice-9b-Q6_K"];
        assert_eq!(both.len(), 2);
        assert_eq!(both[0].id.as_str(), "a-local", "sorted by id");
        assert_eq!(both[1].id.as_str(), "b-vast");
        assert_eq!(
            t.by_alias[&Alias::parse("auto").expect("alias")]
                .targets
                .len(),
            2
        );
    }

    #[test]
    fn glob_matches_the_shapes_a_backend_id_can_take() {
        assert!(glob_match("vast-*", "vast-h100"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("a*c*e", "abcde"));
        assert!(glob_match("local-carnice", "local-carnice"));
        assert!(glob_match("local-?arnice", "local-carnice"));
        assert!(!glob_match("vast-*", "local-carnice"));
        assert!(!glob_match("local-carnic", "local-carnice"));
        assert!(!glob_match("local-?", "local-carnice"));
        assert!(glob_match("*-carnice", "local-carnice"));
    }

    #[test]
    fn a_target_model_override_is_carried_into_the_compiled_route() {
        let reg = registry_with(vec![backend("local-carnice")]);
        let mut r = route("auto", BackendSelector::Id(id("local-carnice")));
        r.targets[0].model = Some("Carnice-9b-Q6_K".into());
        r.targets[0].weight = 7;
        let t = TableBuilder::compile(&Config::default(), &file(vec![r]), &reg).expect("compiles");
        let compiled = &t.by_alias[&Alias::parse("auto").expect("alias")];
        assert_eq!(compiled.alias.as_str(), "auto");
        assert_eq!(compiled.retry, RetryPolicy::default());
        assert_eq!(compiled.strategy, Strategy::FirstHealthy);
        assert!(compiled.is_default);
        assert_eq!(compiled.targets.len(), 1);
        assert_eq!(
            compiled.targets[0].model.as_deref(),
            Some("Carnice-9b-Q6_K")
        );
        // The weight is carried, not expanded: it means something only under RoundRobin,
        // and expanding it here would silently reorder a first_healthy route.
        assert_eq!(compiled.targets[0].weight, 7);
    }

    #[test]
    fn a_tag_target_expands_to_every_backend_wearing_it() {
        let mut rented = backend("vast-h100");
        rented.tags = vec!["rented".into()];
        let reg = registry_with(vec![backend("local-carnice"), rented]);
        let mut r = route("auto", BackendSelector::Tag("local".into()));
        r.targets.push(RouteTarget {
            backend: BackendSelector::Tag("rented".into()),
            model: Some("big-model".into()),
            weight: 3,
        });
        let t = TableBuilder::compile(&Config::default(), &file(vec![r]), &reg).expect("compiles");
        let compiled = &t.by_alias[&Alias::parse("auto").expect("alias")];
        assert_eq!(compiled.targets.len(), 2);
        assert_eq!(compiled.targets[0].backend.id.as_str(), "local-carnice");
        assert_eq!(compiled.targets[0].model, None);
        assert_eq!(compiled.targets[1].backend.id.as_str(), "vast-h100");
        assert_eq!(compiled.targets[1].model.as_deref(), Some("big-model"));
        assert_eq!(compiled.targets[1].weight, 3);
    }

    #[test]
    fn the_implicit_strategy_config_names_is_baked_into_the_table() {
        // `resolve()` is given no config, so rule 4's strategy has to travel in the table.
        let reg = registry_with(vec![backend("local-carnice")]);
        let routes = file(vec![route(
            "auto",
            BackendSelector::Id(id("local-carnice")),
        )]);
        let mut cfg = Config::default();
        cfg.router.implicit_strategy = "least_busy".into();
        let t = TableBuilder::compile(&cfg, &routes, &reg).expect("compiles");
        assert_eq!(t.implicit_strategy, Strategy::LeastBusy);
    }
}
