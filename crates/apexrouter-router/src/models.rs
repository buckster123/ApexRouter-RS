//! OWNER: unit R-06 (router/src/errors.rs, router/src/models.rs). Do not edit outside that
//! unit.
//!
//! The aggregated model list, served from the table with **no upstream hop**.
//!
//! **The OpenAI list shape is the default and stays byte-exact**: ApexOS-RS's
//! `agentd/crates/gateway/src/compute.rs` sweeps the LAN probing `GET /v1/models` for
//! exactly that shape, and being byte-exact there is what makes ApexRouter auto-discoverable
//! as an ApexOS compute node. Extras live under a single `apexrouter` key so strict clients
//! ignore them.
//!
//! Every row is therefore exactly five keys — `id`, `object`, `created`, `owned_by` and the
//! one `apexrouter` object. Nothing this module produces is `await`ed, opens a socket, or
//! reads a file: both entry points are plain synchronous functions over the compiled table,
//! which is what "no upstream hop" means in practice.
//!
//! ## Reading the table
//!
//! `ARCHITECTURE.md` §6.1 says the list is "aggregated across aliases + **every enabled
//! backend**", so this module enumerates `RoutingTable`'s `by_alias` and `by_id` maps
//! directly. Both are `pub(crate)` (unit R-01 widened them from private so `resolve.rs`
//! could read them at all), and this module lives in the same crate.
//!
//! Reading them rather than probing through `resolve()` is what makes two documented
//! behaviours true rather than aspirational: a backend reachable by **no** alias still
//! contributes its model rows, and a route's `Strategy` is reported instead of `null`.
//! [`view`] is the only function that touches R-01's internals.

use crate::registry::LiveBackend;
use crate::resolve::{RequestClass, RouteError, UnknownModelPolicy};
use crate::table::RoutingTable;
use apexrouter_protocol::{Backend, BackendLimits, Health, RouteReason, Strategy, UpstreamModel};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

/// A model string no identifier can spell: [`apexrouter_protocol::BackendId`] and
/// [`apexrouter_protocol::Alias`] are `^[a-z0-9][a-z0-9._-]{0,63}$`, so a control character
/// cannot appear in an alias, in a pin, or in `LEGACY_MODEL_NAMES`. Under
/// [`UnknownModelPolicy::Reject`] this is guaranteed to fall through to rule 6, whose error
/// carries the alias set. Retained because [`one_model`] uses the same guarantee.
const ALIAS_PROBE: &str = "\u{1}apexrouter-alias-enumeration-probe\u{1}";

/// What one alias looks like from outside the table.
struct AliasView {
    /// The alias, as clients spell it.
    alias: String,
    /// How the route orders its candidates.
    strategy: Strategy,
    /// Whether anything behind it is routable right now.
    healthy: bool,
    /// The backend ids it can dispatch to, in declared order.
    targets: Vec<String>,
}

/// Everything `/v1/models` needs out of the compiled table.
///
/// **This is the whole of this unit's dependency on R-01's internals.**
struct TableView {
    /// Every alias the table knows, sorted so the list is deterministic.
    aliases: Vec<AliasView>,
    /// **Every enabled backend**, aliased or not, keyed by id so the list is deterministic.
    backends: BTreeMap<String, Arc<LiveBackend>>,
}

fn view(t: &RoutingTable) -> TableView {
    let mut aliases: Vec<AliasView> = t
        .by_alias
        .values()
        .map(|route| {
            let mut targets: Vec<String> = Vec::with_capacity(route.targets.len());
            let mut healthy = false;
            for target in &route.targets {
                if target.backend.meta.load().health.is_routable() {
                    healthy = true;
                }
                let id = target.backend.id.as_str().to_owned();
                if !targets.contains(&id) {
                    targets.push(id);
                }
            }
            AliasView {
                alias: route.alias.as_str().to_owned(),
                strategy: route.strategy,
                // The route exists; nothing behind it can serve. It stays in the list,
                // marked unhealthy — a missing row would read as "misconfigured", and the
                // whole point of the aggregate is that a client can see what is down.
                healthy,
                targets,
            }
        })
        .collect();
    aliases.sort_by(|a, b| a.alias.cmp(&b.alias));

    // `by_id` holds the ENABLED backends, which is exactly §6.1's "every enabled backend" —
    // including one no route points at, so a freshly adopted endpoint is visible before
    // anybody has written it into `routes.toml`.
    let backends = t
        .by_id
        .iter()
        .map(|(id, b)| (id.as_str().to_owned(), Arc::clone(b)))
        .collect();

    TableView { aliases, backends }
}

/// Unix seconds. `created` is an OpenAI-shape requirement, not a fact we hold.
fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The `state` tag of [`Health`], as the wire spells it.
fn health_status(h: &Health) -> &'static str {
    match h {
        Health::Unknown => "unknown",
        Health::Starting { .. } => "starting",
        Health::Ready { .. } => "ready",
        Health::Degraded { .. } => "degraded",
        Health::Down { .. } => "down",
        Health::Draining { .. } => "draining",
    }
}

/// `"busy/total"`, and only when the backend is actually serving. A backend that is starting
/// or down has no meaningful busy count, and inventing `0/4` would put a number on the wire
/// that nothing measured.
fn slots_value(h: &Health) -> Value {
    match h {
        Health::Ready {
            slots_busy,
            slots_total,
            ..
        } => json!(format!("{slots_busy}/{slots_total}")),
        _ => Value::Null,
    }
}

/// Median observed throughput, when the prober has one.
fn tps_value(h: &Health) -> Value {
    match h {
        Health::Ready {
            tps_p50: Some(tps), ..
        } => json!(tps),
        _ => Value::Null,
    }
}

/// Blended `$/Mtok`, or `null`.
///
/// `null` covers both "no price model" and [`apexrouter_protocol::CostEstimate::Unknown`] —
/// a rented box with no throughput hint has no honest per-token number, and printing one
/// would be the invented ordering `Strategy::Cheapest` is rejected for.
fn price_value(b: &Backend) -> Value {
    let tps = match &b.health {
        Health::Ready { tps_p50, .. } => *tps_p50,
        _ => None,
    };
    match b
        .price
        .as_ref()
        .map(|p| p.per_mtok(tps))
        .and_then(|e| e.usd())
    {
        Some(m) => json!(m.as_usd()),
        None => Value::Null,
    }
}

/// Context length: the model's own, else the backend's.
fn ctx_value(m: Option<&UpstreamModel>, limits: &BackendLimits) -> Value {
    match m.and_then(|m| m.ctx).or(limits.ctx) {
        Some(ctx) => json!(ctx),
        None => Value::Null,
    }
}

/// One alias row.
fn alias_row(
    id: &str,
    created: i64,
    strategy: Option<Strategy>,
    healthy: bool,
    targets: &[String],
) -> Value {
    json!({
        "id": id,
        "object": "model",
        "created": created,
        "owned_by": "apexrouter",
        "apexrouter": {
            "kind": "alias",
            // `null` only when the name is a legacy/fallback spelling rather than a declared
            // route — there is no route whose strategy it could be.
            "strategy": strategy_value(strategy),
            "healthy": healthy,
            "targets": targets,
        }
    })
}

/// `Strategy` as the wire spells it (`"first_healthy"`, …), or `null`.
fn strategy_value(s: Option<Strategy>) -> Value {
    match s {
        // A fieldless `#[serde(rename_all = "snake_case")]` enum cannot fail to serialise;
        // `null` is nevertheless the honest fallback rather than a panic.
        Some(s) => serde_json::to_value(s).unwrap_or(Value::Null),
        None => Value::Null,
    }
}

/// One `"<backend_id>/<model>"` row.
///
/// `id` is echoed exactly as the client asked for it, so `GET /v1/models/{id}` and the entry
/// in `GET /v1/models` are the same string.
fn backend_model_row(id: &str, b: &Backend, m: Option<&UpstreamModel>) -> Value {
    let created = match b.health {
        Health::Ready { since_unix, .. } => since_unix,
        _ => now_unix(),
    };
    json!({
        "id": id,
        "object": "model",
        "created": created,
        "owned_by": b.id.as_str(),
        "apexrouter": {
            "kind": "backend_model",
            "status": health_status(&b.health),
            "ctx": ctx_value(m, &b.limits),
            "slots": slots_value(&b.health),
            "vision": m.is_some_and(|m| m.vision),
            "price": price_value(b),
            "tok_per_s_p50": tps_value(&b.health),
        }
    })
}

/// The OpenAI list envelope. Two keys, in OpenAI's order.
fn model_list(data: Vec<Value>) -> Value {
    json!({ "object": "list", "data": data })
}

/// Every alias plus every enabled backend's models, in the OpenAI list shape.
///
/// Served entirely from the compiled table: no socket is opened, so a fleet with a dead
/// rented box still answers `GET /v1/models` instantly, with that box's row marked `down`.
/// A disabled backend contributes nothing — it is not a routing candidate, and advertising
/// it would make the list a lie.
pub fn aggregate_models(t: &RoutingTable) -> serde_json::Value {
    let created = now_unix();
    let v = view(t);
    let mut data: Vec<Value> = Vec::with_capacity(v.aliases.len() + v.backends.len());
    let mut seen: Vec<String> = Vec::new();

    for a in &v.aliases {
        if seen.iter().any(|s| s == &a.alias) {
            continue;
        }
        seen.push(a.alias.clone());
        data.push(alias_row(
            &a.alias,
            created,
            Some(a.strategy),
            a.healthy,
            &a.targets,
        ));
    }

    for (id, lb) in &v.backends {
        let meta = lb.meta.load();
        if !meta.enabled {
            continue;
        }
        for m in &meta.models {
            let row_id = format!("{id}/{}", m.id);
            if seen.iter().any(|s| s == &row_id) {
                continue;
            }
            seen.push(row_id.clone());
            data.push(backend_model_row(&row_id, &meta, Some(m)));
        }
    }

    model_list(data)
}

/// One entry: an alias, or `"<backend_id>/<model>"`.
///
/// The lookup is `resolve()` itself, so `/v1/models/{id}` answers for exactly the strings
/// `/v1/chat/completions` would accept — including a bare upstream model id (rule 3) and the
/// legacy names (rule 5). `None` is a 404: the caller renders it with
/// [`crate::errors::openai_error`] as `model_not_found`.
pub fn one_model(t: &RoutingTable, id: &str) -> Option<serde_json::Value> {
    if id.is_empty() {
        return None;
    }

    match t.resolve(Some(id), RequestClass::Chat, UnknownModelPolicy::Reject) {
        Ok(plan) => match plan.reason {
            RouteReason::Alias | RouteReason::DefaultFallback | RouteReason::LegacyModelName => {
                let mut targets: Vec<String> = Vec::with_capacity(plan.candidates.len());
                let mut healthy = false;
                for c in &plan.candidates {
                    let bid = c.backend.id.as_str().to_string();
                    if c.backend.meta.load().health.is_routable() {
                        healthy = true;
                    }
                    if !targets.contains(&bid) {
                        targets.push(bid);
                    }
                }
                Some(alias_row(
                    id,
                    now_unix(),
                    strategy_of(t, id),
                    healthy,
                    &targets,
                ))
            }
            RouteReason::ExplicitPin
            | RouteReason::UpstreamIdMatch
            | RouteReason::ImplicitMulti => {
                let c = plan.candidates.first()?;
                let meta = c.backend.meta.load();
                let m = meta.models.iter().find(|m| m.id == c.upstream_model);
                Some(backend_model_row(id, &meta, m))
            }
        },
        // The alias exists but has nothing to serve: that is a row, not a 404. Only rule 6
        // — a name the table has never heard of — is missing.
        Err(RouteError::NoHealthy { .. }) | Err(RouteError::FilteredOut { .. }) => {
            Some(alias_row(id, now_unix(), strategy_of(t, id), false, &[]))
        }
        Err(RouteError::NoRoute { .. }) => None,
    }
}

/// The declared strategy of the route spelled `id`, if `id` names a route at all.
///
/// A legacy name (`""`, `"x"`, `"default"`) resolves through the default alias but is not
/// itself a route, so it reports `None` rather than borrowing the default's strategy.
fn strategy_of(t: &RoutingTable, id: &str) -> Option<Strategy> {
    t.by_alias
        .iter()
        .find(|(a, _)| a.as_str() == id)
        .map(|(_, r)| r.strategy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_protocol::{
        BackendId, BackendKind, CredentialSource, Money, PriceModel, Protocol, Provenance,
    };

    fn backend(health: Health, price: Option<PriceModel>) -> Backend {
        Backend {
            id: BackendId::parse("local-carnice").expect("id"),
            kind: BackendKind::LocalLlama,
            protocol: Protocol::OpenAi,
            label: "Carnice 9B (Vulkan)".into(),
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
                queue_depth: 32,
                ctx: Some(16_384),
                slots_total: Some(4),
            },
            price,
            health,
            provenance: Provenance::Spawned,
            endpoint: None,
            enabled: true,
            devices: vec!["Vulkan0".into()],
            last_error: None,
        }
    }

    fn ready() -> Health {
        Health::Ready {
            since_unix: 1_780_000_000,
            slots_busy: 1,
            slots_total: 4,
            tps_p50: Some(4.1),
        }
    }

    /// The OpenAI row shape, plus **one** extras key.
    fn assert_row_shape(row: &Value) {
        let obj = row.as_object().expect("row is an object");
        let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            ["id", "object", "created", "owned_by", "apexrouter"],
            "row: {row}"
        );
        assert_eq!(obj["object"], "model");
        assert!(obj["id"].is_string());
        assert!(obj["created"].is_i64());
        assert!(obj["owned_by"].is_string());
        assert!(obj["apexrouter"].is_object(), "extras are one object");

        // Nothing non-OpenAI escaped the `apexrouter` key.
        let extras: Vec<&str> = keys
            .iter()
            .copied()
            .filter(|k| !matches!(*k, "id" | "object" | "created" | "owned_by"))
            .collect();
        assert_eq!(extras, ["apexrouter"], "exactly one extras key: {row}");
    }

    #[test]
    fn an_alias_row_puts_every_extra_under_one_apexrouter_key() {
        let targets = vec!["local-carnice".to_string(), "together".to_string()];
        let row = alias_row(
            "auto",
            1_780_000_000,
            Some(Strategy::FirstHealthy),
            true,
            &targets,
        );
        assert_row_shape(&row);
        assert_eq!(row["id"], "auto");
        assert_eq!(row["owned_by"], "apexrouter");
        assert_eq!(row["created"], 1_780_000_000_i64);

        let x = &row["apexrouter"];
        assert_eq!(x["kind"], "alias");
        assert_eq!(x["healthy"], true);
        assert_eq!(x["targets"], json!(["local-carnice", "together"]));
        assert_eq!(
            x["strategy"], "first_healthy",
            "the §6.1 example carries the route's declared strategy"
        );
        // A legacy spelling is not a route, so it has no strategy to report.
        let legacy = alias_row("x", 1, None, false, &[]);
        assert!(legacy["apexrouter"]["strategy"].is_null());
    }

    #[test]
    fn a_backend_model_row_reports_what_the_prober_measured() {
        let b = backend(ready(), Some(PriceModel::Free));
        let m = b.models.first().cloned().expect("model");
        let row = backend_model_row("local-carnice/Carnice-9b-Q6_K", &b, Some(&m));
        assert_row_shape(&row);
        assert_eq!(row["id"], "local-carnice/Carnice-9b-Q6_K");
        assert_eq!(row["owned_by"], "local-carnice");
        assert_eq!(row["created"], 1_780_000_000_i64);

        let x = &row["apexrouter"];
        assert_eq!(x["kind"], "backend_model");
        assert_eq!(x["status"], "ready");
        assert_eq!(x["ctx"], 32_768, "the model's ctx wins over the backend's");
        assert_eq!(x["slots"], "1/4");
        assert_eq!(x["vision"], false);
        assert_eq!(x["price"], 0.0);
        // Compared as the `f32` it is: an `f64` literal would not be the same number.
        assert_eq!(x["tok_per_s_p50"], json!(4.1_f32));
    }

    #[test]
    fn a_backend_that_is_not_ready_invents_no_numbers() {
        let b = backend(
            Health::Down {
                reason: "connection refused".into(),
                retry_at_unix: 9,
            },
            None,
        );
        let row = backend_model_row("local-carnice/Carnice-9b-Q6_K", &b, None);
        assert_row_shape(&row);
        let x = &row["apexrouter"];
        assert_eq!(x["status"], "down");
        assert!(x["slots"].is_null(), "no busy count exists for a dead box");
        assert!(x["tok_per_s_p50"].is_null());
        assert!(x["price"].is_null());
        assert_eq!(x["ctx"], 16_384, "falls back to the backend's own ctx");
        assert_eq!(x["vision"], false);
    }

    #[test]
    fn health_states_spell_themselves_the_way_the_wire_does() {
        for (h, want) in [
            (Health::Unknown, "unknown"),
            (
                Health::Starting {
                    phase: apexrouter_protocol::BootPhase::Loading { pct: Some(10.0) },
                    since_unix: 1,
                    detail: None,
                },
                "starting",
            ),
            (ready(), "ready"),
            (
                Health::Degraded {
                    reason: "timeouts".into(),
                    consecutive_failures: 3,
                },
                "degraded",
            ),
            (
                Health::Down {
                    reason: "gone".into(),
                    retry_at_unix: 0,
                },
                "down",
            ),
            (Health::Draining { in_flight: 2 }, "draining"),
        ] {
            assert_eq!(health_status(&h), want);
            // The status token is also what `Health`'s own serde tag emits.
            let tagged = serde_json::to_value(&h).expect("ser");
            assert_eq!(tagged["state"], want);
        }
    }

    #[test]
    fn a_rented_box_with_no_throughput_hint_has_no_price() {
        let b = backend(
            Health::Starting {
                phase: apexrouter_protocol::BootPhase::Provisioning,
                since_unix: 1,
                detail: None,
            },
            Some(PriceModel::PerHour {
                dph: Money::from_usd(3.34),
            }),
        );
        assert!(
            price_value(&b).is_null(),
            "an invented $/Mtok is exactly what Cheapest is rejected for"
        );

        // The same box, once it is serving and has an EWMA, does have one.
        let mut serving = b.clone();
        serving.health = ready();
        assert!(price_value(&serving).as_f64().unwrap_or(0.0) > 0.0);
    }

    #[test]
    fn the_list_envelope_is_openai_shaped_and_every_row_hides_its_extras() {
        let b = backend(ready(), Some(PriceModel::Free));
        let m = b.models.first().cloned().expect("model");
        let list = model_list(vec![
            alias_row(
                "auto",
                1,
                Some(Strategy::FirstHealthy),
                true,
                &["local-carnice".to_string()],
            ),
            backend_model_row("local-carnice/Carnice-9b-Q6_K", &b, Some(&m)),
        ]);

        let obj = list.as_object().expect("list object");
        let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        assert_eq!(keys, ["object", "data"], "the LAN sweep matches this shape");
        assert_eq!(obj["object"], "list");

        let data = obj["data"].as_array().expect("data array");
        assert_eq!(data.len(), 2);
        for row in data {
            assert_row_shape(row);
        }

        // A strict client that drops unknown keys sees a valid OpenAI list.
        let stripped: Vec<Value> = data
            .iter()
            .map(|r| {
                json!({
                    "id": r["id"],
                    "object": r["object"],
                    "created": r["created"],
                    "owned_by": r["owned_by"],
                })
            })
            .collect();
        assert_eq!(stripped.len(), 2);
        assert_eq!(stripped[0]["id"], "auto");
    }

    #[test]
    fn aggregation_makes_no_upstream_hop() {
        // Both entry points are plain synchronous functions of the table: there is no
        // `.await`, no client and no socket in their signatures, so "served from the table"
        // is a type-level fact rather than a promise in a doc comment.
        fn takes_sync_list(_: fn(&RoutingTable) -> Value) {}
        fn takes_sync_one(_: fn(&RoutingTable, &str) -> Option<Value>) {}
        takes_sync_list(aggregate_models);
        takes_sync_one(one_model);
    }

    #[test]
    fn the_alias_probe_cannot_collide_with_a_real_id() {
        // Ids are `^[a-z0-9][a-z0-9._-]{0,63}$`; the probe is neither parseable as one nor a
        // legacy name nor a pin.
        assert!(BackendId::parse(ALIAS_PROBE).is_err());
        assert!(apexrouter_protocol::Alias::parse(ALIAS_PROBE).is_err());
        assert!(!ALIAS_PROBE.contains('/'));
        assert!(!apexrouter_protocol::LEGACY_MODEL_NAMES.contains(&ALIAS_PROBE));
    }
}
