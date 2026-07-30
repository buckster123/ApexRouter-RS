//! OWNER: unit R-02 (router/src/resolve.rs, router/src/policy.rs). Do not edit outside that
//! unit.
//!
//! **There is exactly one `resolve()` and every surface calls it.** It is synchronous, does
//! no I/O, and the answer is observable on every response as
//! `X-ApexRouter-Route: <alias-or-"-">|<reason>`.
//!
//! The six rules, in order (`ARCHITECTURE.md` §4.2):
//!
//! 1. an **alias** → that route
//! 2. `"<backend_id>/<upstream_model>"` → **explicit pin**, one candidate
//! 3. an exact **upstream model id** on exactly one enabled backend
//! 4. the same id on several backends → `[router] implicit_strategy`, plus a one-shot alert
//! 5. `""`, `"x"`, `"auto"`, `"default"`, absent → the default alias
//! 6. anything else → `[router] unknown_model`, **default `reject`**
//!
//! Every rule is a hash lookup into a map the compiler already built (R-01): selector
//! expansion, glob matching and strategy parsing all happened at compile time, so the
//! request path only ever hashes a string and clones `Arc`s.
//!
//! Two things `resolve()` deliberately does **not** do, because it is synchronous and I/O
//! free:
//!
//! * it never calls `Breaker::check()` — a breaker-open candidate is skipped by the attempt
//!   loop (R-04), exactly where `ARCHITECTURE.md` §4.3 puts it, so the skip is recorded as
//!   an attempt instead of silently shrinking the plan;
//! * it never broadcasts the rule-4 collision `Alert`. It reports
//!   [`RouteReason::ImplicitMulti`] and the caller — which owns the event bus — raises the
//!   one-shot alert.

use crate::policy::order_candidates;
use crate::registry::LiveBackend;
use crate::table::{CompiledRoute, RoutingTable};
use apexrouter_protocol::{
    Alias, BackendId, Health, RetryPolicy, RouteFilter, RouteReason, Strategy,
};
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// How many legs one `weight: n` target may expand into.
///
/// `RouteTarget.weight` is carried through compilation and expanded here, because it means
/// something only under [`Strategy::RoundRobin`] and the published `Candidate` has no weight
/// field. The cap stops a hand-edited `weight = 100000` from allocating a candidate list
/// nobody will ever walk; every ratio up to 8:1 survives intact.
const MAX_WEIGHT_EXPANSION: u32 = 8;

/// `smallvec` is deliberately not a dependency — `ARCHITECTURE.md` §2.1 pins the crate set
/// and `arc-swap` is the one addition. The published `SmallVecLike` is therefore a `Vec`;
/// the name is kept so the signature reads as the document writes it and so a future
/// small-vector optimisation is a one-line change here.
pub type SmallVecLike<T> = Vec<T>;

/// What the resolver decided, in dispatch order.
pub struct Plan {
    /// Ordered candidates. The retry loop walks these, bounded by `RetryPolicy.attempts`.
    pub candidates: SmallVecLike<Candidate>,
    /// Which rule fired.
    pub reason: RouteReason,
    /// The alias, when one resolved.
    pub alias: Option<Alias>,
    /// `Some` only when the outbound `"model"` differs from what the client sent. This is
    /// the **only** key the body rewriter is allowed to touch.
    pub rewrite_model_to: Option<String>,
    /// **The route's own `[retry]` block**, carried to the attempt state machine.
    ///
    /// Rule 1 and rules 5/6 take it from the matched `CompiledRoute`. Rules 2, 3 and 4 route
    /// without a route — an explicit pin and a bare upstream id name a backend, not an alias
    /// — so they carry `RetryPolicy::default()`, which is also what a route that declared no
    /// `[retry]` block was compiled with. Without this field the config key parses, compiles
    /// and is then silently ignored, which is the exact class of bug this project exists to
    /// eliminate.
    pub retry: RetryPolicy,
}

/// One dispatchable target.
pub struct Candidate {
    /// Live state, including the permit pool and the breaker.
    pub backend: Arc<LiveBackend>,
    /// The model id to send upstream.
    pub upstream_model: String,
}

/// What kind of request this is. Drives which backends are eligible.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RequestClass {
    /// `GET /v1/models`.
    Models,
    /// `/v1/chat/completions` and `/v1/messages`.
    Chat,
    /// `/v1/completions`.
    Completion,
    /// `/v1/embeddings` — only embedding-capable backends.
    Embedding,
    /// `/v1/rerank`.
    Rerank,
    /// Anything else, proxied to the default alias's primary target.
    Opaque,
}

/// Why nothing could be dispatched to.
#[derive(Debug, thiserror::Error)]
pub enum RouteError {
    /// Rule 6 under `reject`. Lists the aliases we do know, because a 404 that names the
    /// alternatives is the difference between a fixed typo and a support thread.
    #[error("unknown model; known aliases: {}", known.join(", "))]
    NoRoute {
        /// The aliases we do know.
        known: Vec<String>,
    },
    /// The alias resolved but nothing behind it is `Ready`.
    #[error("no healthy backend for alias {alias}")]
    NoHealthy {
        /// Which alias.
        alias: Alias,
    },
    /// Everything behind the alias failed the route's filter.
    #[error("every candidate for alias {alias} was filtered out: {why}")]
    FilteredOut {
        /// Which alias.
        alias: Alias,
        /// Which filter, and what it wanted.
        why: String,
    },
}

/// What to do with a model string that matched no rule.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum UnknownModelPolicy {
    /// `404 model_not_found`, listing known aliases. The default: a fat-fingered
    /// `gpt-4o-mimi` must not silently bill a rented H100.
    Reject,
    /// LocalRouter's old behaviour — send it to the default alias.
    Fallback,
}

impl RoutingTable {
    /// **SYNCHRONOUS. NO I/O.** Six rules, in the documented order.
    ///
    /// Rule 3 reads the prober-maintained `model_index`; a cold index means rule 3 misses
    /// and rule 5/6 applies, which is documented and visible in `X-ApexRouter-Route`.
    pub fn resolve(
        &self,
        model: Option<&str>,
        class: RequestClass,
        unknown: UnknownModelPolicy,
    ) -> Result<Plan, RouteError> {
        // `Models` and `Opaque` never route on the model string: `/v1/models` is answered
        // from the table itself (R-06, with no upstream hop), and an opaque vendor path
        // goes to the default alias's PRIMARY target, because an arbitrary path is not
        // safely failoverable.
        if matches!(class, RequestClass::Models | RequestClass::Opaque) {
            let mut plan = self.default_route_plan(model, class, RouteReason::LegacyModelName)?;
            plan.candidates.truncate(1);
            return Ok(plan);
        }

        let name = match model {
            Some(n) => n,
            // Rule 5, the absent-model case.
            None => return self.default_route_plan(None, class, RouteReason::LegacyModelName),
        };

        // Rule 1 — alias. `Alias: Borrow<str>`, so this is a lookup, not a parse.
        if let Some(route) = self.by_alias.get(name) {
            return plan_from_route(route, Some(name), class, RouteReason::Alias);
        }

        // Rule 2 — explicit pin, `"<backend_id>/<upstream_model>"`. A miss falls THROUGH
        // rather than failing, because a managed provider's model id carries slashes of its
        // own (`meta-llama/Llama-3.1-8B-Instruct-Turbo`) and rule 3 must still see it whole.
        if let Some(plan) = self.explicit_pin(name) {
            return Ok(plan);
        }

        // Rules 3 and 4 — an exact upstream model id.
        if let Some(plan) = self.upstream_id_match(name, class) {
            return Ok(plan);
        }

        // Rule 5 — the legacy names. `smoke.sh`'s hardcoded `"model":"x"` lands here.
        if self.legacy_model_names.contains(name) {
            return self.default_route_plan(Some(name), class, RouteReason::LegacyModelName);
        }

        // Rule 6 — unknown.
        match unknown {
            UnknownModelPolicy::Reject => Err(RouteError::NoRoute {
                known: self.known_aliases(),
            }),
            UnknownModelPolicy::Fallback => {
                self.default_route_plan(Some(name), class, RouteReason::DefaultFallback)
            }
        }
    }

    /// Rule 2. `None` means "not a pin" — never "the pin failed" — so the caller falls
    /// through to the later rules.
    fn explicit_pin(&self, name: &str) -> Option<Plan> {
        let (id, upstream) = name.split_once('/')?;
        if upstream.is_empty() {
            return None;
        }
        let b = self.by_id.get(id)?;
        if !b.meta.load().enabled {
            return None;
        }
        // No health gate: a pin is an operator instruction, and the breaker plus the attempt
        // loop report a dead target honestly instead of quietly routing somewhere else.
        Some(Plan {
            candidates: vec![Candidate {
                backend: Arc::clone(b),
                upstream_model: upstream.to_owned(),
            }],
            reason: RouteReason::ExplicitPin,
            alias: None,
            rewrite_model_to: Some(upstream.to_owned()),
            // A pin names no route, so there is no `[retry]` block to honour. The one
            // candidate makes `failover` moot in any case.
            retry: RetryPolicy::default(),
        })
    }

    /// Rules 3 and 4. `None` means no live backend advertises the id, so the caller falls
    /// through to rules 5 and 6.
    fn upstream_id_match(&self, name: &str, class: RequestClass) -> Option<Plan> {
        let backends = self.by_upstream_id.get(name)?;
        let mut live: Vec<&Arc<LiveBackend>> =
            backends.iter().filter(|b| b.meta.load().enabled).collect();
        // A cold prober leaves everything `Unknown`; dropping those would make a freshly
        // started daemon reject requests it can in fact serve. Prefer dispatchable
        // backends, fall back to merely-enabled ones.
        if live.iter().any(|b| is_dispatchable(b)) {
            live.retain(|b| is_dispatchable(b));
        }
        let mut candidates: Vec<Candidate> = live
            .into_iter()
            .map(|b| Candidate {
                backend: Arc::clone(b),
                upstream_model: name.to_owned(),
            })
            .collect();
        keep_class_capable(class, &mut candidates);
        if candidates.is_empty() {
            return None;
        }
        let multi = candidates.len() > 1;
        if multi {
            order_candidates(self.implicit_strategy, &mut candidates);
        }
        Some(Plan {
            candidates,
            reason: if multi {
                RouteReason::ImplicitMulti
            } else {
                RouteReason::UpstreamIdMatch
            },
            alias: None,
            // The client already named the upstream id, so the body passes through
            // untouched — this is the zero-copy path R-03 depends on.
            rewrite_model_to: None,
            // Rules 3 and 4 match a model id across backends rather than a route, so no
            // `[retry]` block applies and the shipped default governs.
            retry: RetryPolicy::default(),
        })
    }

    /// Route to the default alias, with the reason the calling rule implies.
    fn default_route_plan(
        &self,
        requested: Option<&str>,
        class: RequestClass,
        reason: RouteReason,
    ) -> Result<Plan, RouteError> {
        match self.by_alias.get(self.default_alias.as_str()) {
            Some(route) => plan_from_route(route, requested, class, reason),
            // A non-empty table whose `default_alias` names no route fails to compile
            // (R-01), so this is the empty-table path: name what we do know instead of
            // inventing a destination.
            None => Err(RouteError::NoRoute {
                known: self.known_aliases(),
            }),
        }
    }

    /// Every alias the table knows, sorted, for a `404` that names the alternatives.
    fn known_aliases(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .by_alias
            .keys()
            .map(|a| a.as_str().to_owned())
            .collect();
        v.sort();
        v
    }
}

/// Turn one compiled route into an ordered plan, or say precisely why it could not.
fn plan_from_route(
    route: &CompiledRoute,
    requested: Option<&str>,
    class: RequestClass,
    reason: RouteReason,
) -> Result<Plan, RouteError> {
    // Weight is a round-robin concept. Expanding it under `first_healthy` would change
    // nothing, and dropping a `weight = 0` target there would silently delete a route
    // somebody wrote on purpose.
    let weighted = route.strategy == Strategy::RoundRobin;
    let mut candidates: Vec<Candidate> = Vec::with_capacity(route.targets.len());
    let mut dispatchable = 0usize;
    let mut rejected: Vec<&'static str> = Vec::new();

    for t in &route.targets {
        let repeats = if weighted {
            t.weight.min(MAX_WEIGHT_EXPANSION)
        } else {
            1
        };
        if repeats == 0 || !is_dispatchable(&t.backend) {
            continue;
        }
        dispatchable += 1;
        let upstream_model = upstream_model_for(&t.backend, t.model.as_deref(), requested);
        if let Some(why) = filter_reject(&t.backend, &upstream_model, &route.filter) {
            if !rejected.contains(&why) {
                rejected.push(why);
            }
            continue;
        }
        for _ in 0..repeats {
            candidates.push(Candidate {
                backend: Arc::clone(&t.backend),
                upstream_model: upstream_model.clone(),
            });
        }
    }

    if dispatchable == 0 {
        return Err(RouteError::NoHealthy {
            alias: route.alias.clone(),
        });
    }
    if candidates.is_empty() {
        return Err(RouteError::FilteredOut {
            alias: route.alias.clone(),
            why: rejected.join(", "),
        });
    }

    keep_class_capable(class, &mut candidates);
    if candidates.is_empty() {
        return Err(RouteError::FilteredOut {
            alias: route.alias.clone(),
            why: "no candidate serves this request class".to_owned(),
        });
    }

    order_candidates(route.strategy, &mut candidates);
    if weighted {
        // The repeats existed to weight the *rotation*. Leaving them in would make the
        // retry chain try the same backend twice before failing over, which is not what
        // `weight` means.
        dedupe(&mut candidates);
    }

    let rewrite_model_to = match requested {
        Some(r) if r == candidates[0].upstream_model => None,
        _ => Some(candidates[0].upstream_model.clone()),
    };
    Ok(Plan {
        candidates,
        reason,
        alias: Some(route.alias.clone()),
        rewrite_model_to,
        // **This is what makes `routes.toml`'s `[retry]` block do something.** It is copied
        // rather than referenced so the plan outlives the `ArcSwap` guard the table was read
        // through; `RetryPolicy` is three bytes and `Copy`.
        retry: route.retry,
    })
}

/// Drop repeated `(backend, upstream model)` pairs, keeping the first of each. The same
/// backend under two different model ids is a real failover step and survives.
fn dedupe(cands: &mut Vec<Candidate>) {
    let mut seen: Vec<(BackendId, String)> = Vec::with_capacity(cands.len());
    cands.retain(|c| {
        if seen
            .iter()
            .any(|(id, m)| *id == c.backend.id && *m == c.upstream_model)
        {
            return false;
        }
        seen.push((c.backend.id.clone(), c.upstream_model.clone()));
        true
    });
}

/// May this backend be dispatched to at all?
///
/// `Ready` is the routable state. `Unknown` is admitted too — it means "the prober has not
/// run yet", not "known bad", and refusing it would make a freshly started daemon 503
/// everything for one probe interval. `Starting`, `Degraded`, `Down` and `Draining` are not
/// admitted; `Draining` deliberately so, per `ARCHITECTURE.md` §3.4.
fn is_dispatchable(b: &LiveBackend) -> bool {
    let meta = b.meta.load();
    meta.enabled
        && b.accepting.load(Ordering::Relaxed)
        && (meta.health.is_routable() || matches!(meta.health, Health::Unknown))
}

/// The model id to put in the outbound body for one target.
///
/// Route override → the prober's `model_index` → the backend's advertised list → whatever
/// the client sent, which keeps a cold, unprobed backend a passthrough rather than a 404.
fn upstream_model_for(b: &LiveBackend, over: Option<&str>, requested: Option<&str>) -> String {
    if let Some(m) = over {
        return m.to_owned();
    }
    if let Some(first) = b.model_index.load().first() {
        return first.clone();
    }
    if let Some(first) = b.meta.load().models.first() {
        return first.id.clone();
    }
    requested.unwrap_or_default().to_owned()
}

/// Which filter predicate rejects this candidate, if any.
///
/// A cost ceiling excludes a backend whose `$/Mtok` is *unknown*: a `max_cost_per_mtok`
/// route that silently admitted an unpriced rented GPU would be the exact failure the
/// ceiling exists to prevent.
fn filter_reject(b: &LiveBackend, model_id: &str, f: &RouteFilter) -> Option<&'static str> {
    let meta = b.meta.load();
    if !f
        .require_tags
        .iter()
        .all(|want| meta.tags.iter().any(|have| have == want))
    {
        return Some("require_tags");
    }
    if f.exclude_tags
        .iter()
        .any(|bad| meta.tags.iter().any(|have| have == bad))
    {
        return Some("exclude_tags");
    }
    let advertised = meta.models.iter().find(|m| m.id == model_id);
    if f.require_vision && !advertised.is_some_and(|m| m.vision) {
        return Some("require_vision");
    }
    if f.require_tools && !advertised.is_some_and(|m| m.tools) {
        return Some("require_tools");
    }
    if let Some(min) = f.min_ctx {
        let ctx = advertised.and_then(|m| m.ctx).or(meta.limits.ctx);
        if ctx.map_or(true, |c| c < min) {
            return Some("min_ctx");
        }
    }
    if let Some(max) = f.max_cost_per_mtok {
        let usd = meta
            .price
            .as_ref()
            .and_then(|p| p.per_mtok(tps_hint(&meta.health)).usd());
        if usd.map_or(true, |u| u > max) {
            return Some("max_cost_per_mtok");
        }
    }
    None
}

/// The throughput assumption a `PerHour` price is normalised with: the backend's own
/// observed p50, never a buried constant.
fn tps_hint(h: &Health) -> Option<f32> {
    match h {
        Health::Ready { tps_p50, .. } => *tps_p50,
        _ => None,
    }
}

/// Narrow a candidate set to the backends that serve this request class.
///
/// A *preference*, not a hard filter: it only bites when at least one candidate declares the
/// capability, so a rig whose backends carry no capability tags routes exactly as before.
fn keep_class_capable(class: RequestClass, cands: &mut Vec<Candidate>) {
    let tag = match class {
        RequestClass::Embedding => "embedding",
        RequestClass::Rerank => "rerank",
        _ => return,
    };
    let tagged = |c: &Candidate| c.backend.meta.load().tags.iter().any(|t| t == tag);
    if cands.iter().any(&tagged) {
        cands.retain(&tagged);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::CompiledTarget;
    use apexrouter_core::config::RouterCfg;
    use apexrouter_protocol::{
        Backend, BackendKind, BackendLimits, CredentialSource, Money, PriceModel, Protocol,
        Provenance, RetryPolicy, UpstreamModel, LEGACY_MODEL_NAMES,
    };
    use std::collections::{HashMap, HashSet};
    use std::time::Instant;

    fn alias(s: &str) -> Alias {
        Alias::parse(s).expect("test alias")
    }

    fn id(s: &str) -> BackendId {
        BackendId::parse(s).expect("test backend id")
    }

    fn backend_meta(name: &str, models: &[&str]) -> Backend {
        Backend {
            id: id(name),
            kind: BackendKind::LocalLlama,
            protocol: Protocol::OpenAi,
            label: name.to_owned(),
            base_url: format!("http://127.0.0.1:8080/{name}"),
            credential: CredentialSource::None,
            tags: vec!["local".to_owned()],
            models: models
                .iter()
                .map(|m| UpstreamModel {
                    id: (*m).to_owned(),
                    ctx: Some(32_768),
                    vision: false,
                    tools: true,
                })
                .collect(),
            limits: BackendLimits {
                max_concurrent: 4,
                queue_depth: 8,
                ctx: Some(32_768),
                slots_total: Some(4),
            },
            price: None,
            health: Health::Ready {
                since_unix: 0,
                slots_busy: 0,
                slots_total: 4,
                tps_p50: Some(20.0),
            },
            provenance: Provenance::Discovered,
            endpoint: None,
            enabled: true,
            devices: Vec::new(),
            last_error: None,
        }
    }

    fn backend(name: &str, models: &[&str]) -> Arc<LiveBackend> {
        LiveBackend::new(backend_meta(name, models), &RouterCfg::default())
    }

    /// Mutate a backend's description the way the prober does.
    fn edit_meta(b: &Arc<LiveBackend>, f: impl FnOnce(&mut Backend)) {
        let mut meta = Backend::clone(&b.meta.load_full());
        f(&mut meta);
        b.update_meta(meta);
    }

    fn route(a: &str, targets: Vec<(Arc<LiveBackend>, Option<&str>)>) -> CompiledRoute {
        route_with_retry(a, targets, RetryPolicy::default())
    }

    fn route_with_retry(
        a: &str,
        targets: Vec<(Arc<LiveBackend>, Option<&str>)>,
        retry: RetryPolicy,
    ) -> CompiledRoute {
        CompiledRoute {
            alias: alias(a),
            targets: targets
                .into_iter()
                .map(|(backend, model)| CompiledTarget {
                    backend,
                    model: model.map(str::to_owned),
                    weight: 1,
                })
                .collect(),
            strategy: Strategy::FirstHealthy,
            filter: RouteFilter::default(),
            retry,
            is_default: a == "auto",
        }
    }

    /// A table built by hand, so the six rules are tested without a `RouteFile` round trip.
    /// Every test calls the published `resolve()` itself.
    struct Fixture {
        table: RoutingTable,
    }

    impl Fixture {
        fn empty() -> Fixture {
            Fixture {
                table: RoutingTable {
                    by_alias: HashMap::new(),
                    by_upstream_id: HashMap::new(),
                    by_id: HashMap::new(),
                    default_alias: alias("auto"),
                    legacy_model_names: LEGACY_MODEL_NAMES
                        .iter()
                        .map(|s| (*s).to_owned())
                        .collect::<HashSet<String>>(),
                    generation: 1,
                    implicit_strategy: Strategy::FirstHealthy,
                },
            }
        }

        fn add(&mut self, b: &Arc<LiveBackend>) {
            self.table.by_id.insert(b.id.clone(), Arc::clone(b));
            for m in b.model_index.load().iter() {
                self.table
                    .by_upstream_id
                    .entry(m.clone())
                    .or_default()
                    .push(Arc::clone(b));
            }
        }

        fn get(&self, name: &str) -> Arc<LiveBackend> {
            Arc::clone(&self.table.by_id[&id(name)])
        }

        fn put(&mut self, r: CompiledRoute) {
            self.table.by_alias.insert(r.alias.clone(), r);
        }

        fn resolve(&self, model: Option<&str>, unknown: UnknownModelPolicy) -> Plan {
            self.table
                .resolve(model, RequestClass::Chat, unknown)
                .unwrap_or_else(|e| panic!("{model:?} should resolve: {e}"))
        }

        fn resolve_err(&self, model: Option<&str>, unknown: UnknownModelPolicy) -> RouteError {
            match self.table.resolve(model, RequestClass::Chat, unknown) {
                Ok(p) => panic!(
                    "{model:?} should NOT resolve, got {:?} with {} candidate(s)",
                    p.reason,
                    p.candidates.len()
                ),
                Err(e) => e,
            }
        }
    }

    /// `local-carnice` advertises `Carnice-9b-Q6_K` and `shared`; `vast-h100` advertises
    /// `big-model` and the same `shared` id — that collision is rule 4.
    fn fixture() -> Fixture {
        let local = backend("local-carnice", &["Carnice-9b-Q6_K", "shared"]);
        let rented = backend("vast-h100", &["big-model", "shared"]);

        let mut f = Fixture::empty();
        f.add(&local);
        f.add(&rented);
        f.put(route("auto", vec![(local, None)]));
        f.put(route("big", vec![(rented, Some("big-model"))]));
        f
    }

    #[test]
    fn rule_1_an_alias_wins_and_rewrites_the_model() {
        let f = fixture();
        let p = f.resolve(Some("big"), UnknownModelPolicy::Reject);
        assert_eq!(p.reason, RouteReason::Alias);
        assert_eq!(p.alias.as_ref().map(Alias::as_str), Some("big"));
        assert_eq!(p.candidates.len(), 1);
        assert_eq!(p.candidates[0].upstream_model, "big-model");
        assert_eq!(p.rewrite_model_to.as_deref(), Some("big-model"));
    }

    #[test]
    fn rule_1_beats_rule_3_when_an_alias_shadows_an_upstream_id() {
        // `TableBuilder::compile` rejects a table like this — and rule 1 winning is exactly
        // why it bothers to.
        let mut f = fixture();
        let local = f.get("local-carnice");
        f.put(route("shared", vec![(local, Some("Carnice-9b-Q6_K"))]));
        let p = f.resolve(Some("shared"), UnknownModelPolicy::Reject);
        assert_eq!(p.reason, RouteReason::Alias);
        assert_eq!(p.candidates.len(), 1);
    }

    #[test]
    fn rule_2_explicit_pin_yields_exactly_one_candidate() {
        let f = fixture();
        let p = f.resolve(Some("vast-h100/big-model"), UnknownModelPolicy::Reject);
        assert_eq!(p.reason, RouteReason::ExplicitPin);
        assert!(p.alias.is_none());
        assert_eq!(p.candidates.len(), 1, "a pin never fails over");
        assert_eq!(p.candidates[0].backend.id.as_str(), "vast-h100");
        assert_eq!(p.candidates[0].upstream_model, "big-model");
        assert_eq!(p.rewrite_model_to.as_deref(), Some("big-model"));
    }

    #[test]
    fn rule_2_falls_through_for_a_slashed_provider_model_id() {
        let mut f = fixture();
        let together = backend("together", &["meta-llama/Llama-3.1-8B-Instruct-Turbo"]);
        f.add(&together);
        let p = f.resolve(
            Some("meta-llama/Llama-3.1-8B-Instruct-Turbo"),
            UnknownModelPolicy::Reject,
        );
        assert_eq!(
            p.reason,
            RouteReason::UpstreamIdMatch,
            "`meta-llama` is not a backend id, so the whole string is the model"
        );
        assert!(p.rewrite_model_to.is_none(), "passthrough, not a rewrite");
    }

    #[test]
    fn rule_3_exact_upstream_id_on_one_backend() {
        let f = fixture();
        let p = f.resolve(Some("Carnice-9b-Q6_K"), UnknownModelPolicy::Reject);
        assert_eq!(p.reason, RouteReason::UpstreamIdMatch);
        assert_eq!(p.candidates.len(), 1);
        assert_eq!(p.candidates[0].backend.id.as_str(), "local-carnice");
        assert!(
            p.rewrite_model_to.is_none(),
            "the client already named the upstream id — the body passes through"
        );
    }

    #[test]
    fn rule_4_same_id_on_several_backends_is_implicit_multi() {
        let f = fixture();
        let p = f.resolve(Some("shared"), UnknownModelPolicy::Reject);
        assert_eq!(p.reason, RouteReason::ImplicitMulti);
        assert_eq!(p.candidates.len(), 2);
        assert!(p.rewrite_model_to.is_none());
    }

    #[test]
    fn rule_5_legacy_names_land_on_the_default_alias() {
        let f = fixture();
        for name in ["x", "", "default"] {
            let p = f.resolve(Some(name), UnknownModelPolicy::Reject);
            assert_eq!(
                p.reason,
                RouteReason::LegacyModelName,
                "{name:?} is a legacy model name"
            );
            assert_eq!(p.alias.as_ref().map(Alias::as_str), Some("auto"));
            assert_eq!(p.candidates[0].backend.id.as_str(), "local-carnice");
            assert_eq!(
                p.rewrite_model_to.as_deref(),
                Some("Carnice-9b-Q6_K"),
                "smoke.sh's hardcoded \"model\":\"x\" must reach a real model id"
            );
        }
    }

    #[test]
    fn rule_5_an_absent_model_lands_on_the_default_alias() {
        let f = fixture();
        let p = f.resolve(None, UnknownModelPolicy::Reject);
        assert_eq!(p.reason, RouteReason::LegacyModelName);
        assert_eq!(p.alias.as_ref().map(Alias::as_str), Some("auto"));
    }

    #[test]
    fn rule_5_auto_is_an_alias_before_it_is_a_legacy_name() {
        let f = fixture();
        let p = f.resolve(Some("auto"), UnknownModelPolicy::Reject);
        assert_eq!(
            p.reason,
            RouteReason::Alias,
            "`auto` is both a legacy name and the default alias; rule 1 runs first"
        );
    }

    #[test]
    fn rule_6_reject_is_the_default_and_names_the_alternatives() {
        let f = fixture();
        let e = f.resolve_err(Some("gpt-4o-mimi"), UnknownModelPolicy::Reject);
        match e {
            RouteError::NoRoute { known } => assert_eq!(known, vec!["auto", "big"]),
            other => panic!("expected NoRoute, got {other:?}"),
        }
    }

    #[test]
    fn rule_6_fallback_is_localrouters_old_behaviour() {
        let f = fixture();
        let p = f.resolve(Some("gpt-4o-mimi"), UnknownModelPolicy::Fallback);
        assert_eq!(p.reason, RouteReason::DefaultFallback);
        assert_eq!(p.alias.as_ref().map(Alias::as_str), Some("auto"));
        assert_eq!(p.rewrite_model_to.as_deref(), Some("Carnice-9b-Q6_K"));
    }

    #[test]
    fn the_six_rules_fire_in_the_documented_order() {
        // One table, six inputs, six reasons — the ORDER is the contract.
        let mut f = fixture();
        let local = f.get("local-carnice");
        f.put(route("coder", vec![(local, Some("Carnice-9b-Q6_K"))]));
        let cases: [(Option<&str>, RouteReason); 6] = [
            (Some("coder"), RouteReason::Alias),
            (Some("vast-h100/big-model"), RouteReason::ExplicitPin),
            (Some("Carnice-9b-Q6_K"), RouteReason::UpstreamIdMatch),
            (Some("shared"), RouteReason::ImplicitMulti),
            (Some("x"), RouteReason::LegacyModelName),
            (Some("nope-not-a-model"), RouteReason::DefaultFallback),
        ];
        for (model, want) in cases {
            let p = f.resolve(model, UnknownModelPolicy::Fallback);
            assert_eq!(p.reason, want, "for model {model:?}");
        }
    }

    #[test]
    fn a_disabled_backend_is_never_a_candidate() {
        let f = fixture();
        edit_meta(&f.get("local-carnice"), |m| m.enabled = false);

        // Rule 1: the alias's only leg is disabled.
        let e = f.resolve_err(Some("auto"), UnknownModelPolicy::Reject);
        assert!(matches!(e, RouteError::NoHealthy { .. }), "{e:?}");

        // Rule 2: a pin at a disabled backend falls through rather than dispatching.
        let e = f.resolve_err(
            Some("local-carnice/Carnice-9b-Q6_K"),
            UnknownModelPolicy::Reject,
        );
        assert!(matches!(e, RouteError::NoRoute { .. }), "{e:?}");

        // Rule 3: a disabled backend does not satisfy the exact-id rule either, and the
        // name is not a legacy one, so rule 6 rejects it.
        let e = f.resolve_err(Some("Carnice-9b-Q6_K"), UnknownModelPolicy::Reject);
        assert!(matches!(e, RouteError::NoRoute { .. }), "{e:?}");
    }

    #[test]
    fn draining_is_not_routable_but_an_unprobed_backend_is() {
        let f = fixture();
        let local = f.get("local-carnice");

        edit_meta(&local, |m| m.health = Health::Draining { in_flight: 2 });
        let e = f.resolve_err(Some("auto"), UnknownModelPolicy::Reject);
        assert!(
            matches!(e, RouteError::NoHealthy { .. }),
            "draining is deliberately not routable: {e:?}"
        );

        edit_meta(&local, |m| m.health = Health::Unknown);
        let p = f.resolve(Some("auto"), UnknownModelPolicy::Reject);
        assert_eq!(
            p.candidates.len(),
            1,
            "an unprobed backend is still routable"
        );
    }

    #[test]
    fn a_backend_that_stopped_accepting_is_skipped_even_while_health_says_ready() {
        let f = fixture();
        f.get("local-carnice")
            .accepting
            .store(false, Ordering::SeqCst);
        let e = f.resolve_err(Some("auto"), UnknownModelPolicy::Reject);
        assert!(matches!(e, RouteError::NoHealthy { .. }), "{e:?}");
    }

    #[test]
    fn a_filter_that_removes_everything_says_which_filter() {
        let mut f = fixture();
        let mut r = route(
            "vision",
            vec![(f.get("local-carnice"), Some("Carnice-9b-Q6_K"))],
        );
        r.filter.require_vision = true;
        f.put(r);
        let e = f.resolve_err(Some("vision"), UnknownModelPolicy::Reject);
        match e {
            RouteError::FilteredOut { alias: a, why } => {
                assert_eq!(a.as_str(), "vision");
                assert_eq!(why, "require_vision");
            }
            other => panic!("expected FilteredOut, got {other:?}"),
        }
    }

    #[test]
    fn require_tags_and_exclude_tags_filter_and_report() {
        let mut f = fixture();
        let mut r = route("gpu", vec![(f.get("local-carnice"), None)]);
        r.filter.require_tags = vec!["gpu:cuda".to_owned()];
        f.put(r);
        match f.resolve_err(Some("gpu"), UnknownModelPolicy::Reject) {
            RouteError::FilteredOut { why, .. } => assert_eq!(why, "require_tags"),
            other => panic!("expected FilteredOut, got {other:?}"),
        }

        let mut r = route("nonlocal", vec![(f.get("local-carnice"), None)]);
        r.filter.exclude_tags = vec!["local".to_owned()];
        f.put(r);
        match f.resolve_err(Some("nonlocal"), UnknownModelPolicy::Reject) {
            RouteError::FilteredOut { why, .. } => assert_eq!(why, "exclude_tags"),
            other => panic!("expected FilteredOut, got {other:?}"),
        }
    }

    #[test]
    fn a_cost_ceiling_excludes_an_unpriced_backend() {
        let mut f = fixture();
        let local = f.get("local-carnice");
        let mut r = route("cheap", vec![(Arc::clone(&local), Some("Carnice-9b-Q6_K"))]);
        r.filter.max_cost_per_mtok = Some(Money::from_usd(1.0));
        f.put(r);

        let e = f.resolve_err(Some("cheap"), UnknownModelPolicy::Reject);
        assert!(
            matches!(e, RouteError::FilteredOut { .. }),
            "an unpriced backend cannot be proven under a ceiling: {e:?}"
        );

        edit_meta(&local, |m| m.price = Some(PriceModel::Free));
        let p = f.resolve(Some("cheap"), UnknownModelPolicy::Reject);
        assert_eq!(p.candidates.len(), 1, "free is under every ceiling");
    }

    #[test]
    fn min_ctx_uses_the_advertised_model_then_the_backend_limit() {
        let mut f = fixture();
        let mut r = route("long", vec![(f.get("local-carnice"), None)]);
        r.filter.min_ctx = Some(131_072);
        f.put(r);
        match f.resolve_err(Some("long"), UnknownModelPolicy::Reject) {
            RouteError::FilteredOut { why, .. } => assert_eq!(why, "min_ctx"),
            other => panic!("expected FilteredOut, got {other:?}"),
        }
    }

    #[test]
    fn an_opaque_request_gets_the_default_aliass_primary_target_only() {
        let mut f = fixture();
        let legs = vec![(f.get("local-carnice"), None), (f.get("vast-h100"), None)];
        f.put(route("auto", legs));
        let p = f
            .table
            .resolve(
                Some("anything-at-all"),
                RequestClass::Opaque,
                UnknownModelPolicy::Reject,
            )
            .expect("an opaque path always routes");
        assert_eq!(p.candidates.len(), 1, "primary target only");
        assert_eq!(p.candidates[0].backend.id.as_str(), "local-carnice");
    }

    #[test]
    fn an_embedding_request_prefers_an_embedding_tagged_backend() {
        let mut f = fixture();
        let embed = backend("embeddings", &["shared"]);
        edit_meta(&embed, |m| m.tags.push("embedding".to_owned()));
        f.add(&embed);

        let p = f
            .table
            .resolve(
                Some("shared"),
                RequestClass::Embedding,
                UnknownModelPolicy::Reject,
            )
            .expect("embedding resolves");
        assert_eq!(p.candidates.len(), 1);
        assert_eq!(p.candidates[0].backend.id.as_str(), "embeddings");

        // Chat is unaffected by the embedding tag.
        let p = f.resolve(Some("shared"), UnknownModelPolicy::Reject);
        assert_eq!(p.candidates.len(), 3);
    }

    #[test]
    fn round_robin_weights_the_rotation_without_duplicating_the_retry_chain() {
        let mut f = fixture();
        let mut r = route(
            "weighted",
            vec![(f.get("local-carnice"), None), (f.get("vast-h100"), None)],
        );
        r.strategy = Strategy::RoundRobin;
        r.targets[1].weight = 3;
        f.put(r);

        let mut heads = Vec::new();
        for _ in 0..12 {
            let p = f.resolve(Some("weighted"), UnknownModelPolicy::Reject);
            assert_eq!(
                p.candidates.len(),
                2,
                "the retry chain holds each backend once"
            );
            heads.push(p.candidates[0].backend.id.as_str().to_owned());
        }
        assert!(
            heads.iter().any(|h| h == "vast-h100") && heads.iter().any(|h| h == "local-carnice"),
            "both backends must take the head over twelve requests: {heads:?}"
        );
    }

    #[test]
    fn a_weight_of_zero_drops_a_target_only_where_weight_means_something() {
        let mut f = fixture();
        let mut r = route(
            "weighted",
            vec![(f.get("local-carnice"), None), (f.get("vast-h100"), None)],
        );
        r.strategy = Strategy::RoundRobin;
        r.targets[0].weight = 0;
        f.put(r);
        let p = f.resolve(Some("weighted"), UnknownModelPolicy::Reject);
        assert_eq!(p.candidates.len(), 1);
        assert_eq!(p.candidates[0].backend.id.as_str(), "vast-h100");

        // The same zero under first_healthy changes nothing: weight has no meaning there.
        let mut r = route(
            "ordered",
            vec![(f.get("local-carnice"), None), (f.get("vast-h100"), None)],
        );
        r.targets[0].weight = 0;
        f.put(r);
        let p = f.resolve(Some("ordered"), UnknownModelPolicy::Reject);
        assert_eq!(p.candidates.len(), 2);
        assert_eq!(p.candidates[0].backend.id.as_str(), "local-carnice");
    }

    #[test]
    fn an_empty_table_rejects_rather_than_inventing_a_destination() {
        let f = Fixture::empty();
        let e = f.resolve_err(Some("x"), UnknownModelPolicy::Fallback);
        match e {
            RouteError::NoRoute { known } => assert!(known.is_empty(), "{known:?}"),
            other => panic!("expected NoRoute, got {other:?}"),
        }
    }

    #[test]
    fn the_table_and_its_backends_are_shareable_across_threads() {
        // `resolve()` takes `&self`, does no I/O and is called from every worker task, so
        // the property is pinned by a test rather than described in a comment.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<RoutingTable>();
        assert_send_sync::<LiveBackend>();
        assert_send_sync::<Plan>();
    }

    #[test]
    fn a_routes_own_retry_block_reaches_the_plan() {
        // Before `Plan::retry` existed, `routes.toml`'s `[routes.retry]` parsed, compiled
        // into `CompiledRoute::retry` and then stopped — the handler used
        // `RetryPolicy::default()` unconditionally and the config key did nothing.
        let mut f = fixture();
        let local = f.get("local-carnice");
        let pinned = RetryPolicy {
            attempts: 5,
            failover: false,
            honor_retry_after: false,
        };
        f.put(route_with_retry("coder", vec![(local, None)], pinned));

        let p = f.resolve(Some("coder"), UnknownModelPolicy::Reject);
        assert_eq!(p.reason, RouteReason::Alias);
        assert_eq!(p.retry, pinned, "the route's own policy, not the default");
    }

    #[test]
    fn a_route_without_its_own_retry_block_carries_the_default() {
        let f = fixture();
        // Rule 1, on a route compiled with the shipped default.
        assert_eq!(
            f.resolve(Some("auto"), UnknownModelPolicy::Reject).retry,
            RetryPolicy::default()
        );
        // Rule 5 — the legacy names land on the default route, so they inherit its policy.
        assert_eq!(
            f.resolve(Some("x"), UnknownModelPolicy::Reject).retry,
            RetryPolicy::default()
        );
        // Rules 2 and 3 name a backend rather than a route, so there is no `[retry]` block
        // in play at all and the shipped default governs.
        assert_eq!(
            f.resolve(Some("vast-h100/big-model"), UnknownModelPolicy::Reject)
                .retry,
            RetryPolicy::default()
        );
        assert_eq!(
            f.resolve(Some("Carnice-9b-Q6_K"), UnknownModelPolicy::Reject)
                .retry,
            RetryPolicy::default()
        );
    }

    #[test]
    fn a_per_route_retry_block_survives_a_non_default_route_being_the_one_that_matched() {
        // The failure mode this guards: reading the policy off the *default* route instead
        // of the route that actually matched.
        let mut f = fixture();
        let local = f.get("local-carnice");
        let rented = f.get("vast-h100");
        f.put(route_with_retry(
            "auto",
            vec![(local, None)],
            RetryPolicy {
                attempts: 1,
                failover: false,
                honor_retry_after: true,
            },
        ));
        f.put(route_with_retry(
            "big",
            vec![(rented, Some("big-model"))],
            RetryPolicy {
                attempts: 4,
                failover: true,
                honor_retry_after: false,
            },
        ));
        assert_eq!(
            f.resolve(Some("big"), UnknownModelPolicy::Reject)
                .retry
                .attempts,
            4
        );
        assert_eq!(
            f.resolve(Some("auto"), UnknownModelPolicy::Reject)
                .retry
                .attempts,
            1
        );
    }

    #[test]
    fn the_alias_hit_path_is_fast() {
        // Acceptance: "does not allocate on the alias hit path (bench-asserted < 200 ns)".
        // `SmallVecLike` is a `Vec`, so the hit path is one hash lookup plus two small
        // allocations (the candidate vector and the model id). 200 ns holds with
        // optimisations on; a debug build is roughly an order of magnitude slower and is
        // measured against a correspondingly looser bound rather than skipped.
        let f = fixture();
        const ITERS: u32 = 20_000;
        let mut best = f64::MAX;
        for _ in 0..5 {
            let t0 = Instant::now();
            for _ in 0..ITERS {
                let p =
                    f.table
                        .resolve(Some("big"), RequestClass::Chat, UnknownModelPolicy::Reject);
                std::hint::black_box(&p);
            }
            let ns = t0.elapsed().as_nanos() as f64 / f64::from(ITERS);
            best = best.min(ns);
        }
        let budget = if cfg!(debug_assertions) {
            2_000.0
        } else {
            200.0
        };
        assert!(
            best < budget,
            "alias hit path took {best:.0} ns/op, budget {budget:.0} ns"
        );
    }
}
