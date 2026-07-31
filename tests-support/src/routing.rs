//! Routing-table fixtures: a [`Backend`] pointing at a fake, and a [`ModelRoute`] pointing
//! at that backend.
//!
//! Only `apexrouter-protocol` types, on purpose. Compiling the table is two lines the
//! caller writes in its own crate (`registry.upsert` + `TableBuilder::compile`), and doing
//! it here would make every consumer of this crate compile the router.

use apexrouter_protocol::{
    Alias, Backend, BackendId, BackendKind, BackendLimits, BackendSelector, CredentialSource,
    Health, ModelRoute, PriceModel, Protocol, Provenance, RetryPolicy, RouteFile, RouteFilter,
    RouteTarget, Strategy, UpstreamModel,
};

/// A healthy, free, four-slot `Node` backend at `base_url`, advertising `models`.
///
/// `base_url` must **not** carry a trailing `/v1`: that is the stored form, and the relay
/// joins segments itself.
///
/// # Panics
/// When `id` is not a valid [`BackendId`] slug (`^[a-z0-9][a-z0-9._-]{0,63}$`).
pub fn backend(id: &str, base_url: &str, models: &[&str]) -> Backend {
    Backend {
        id: BackendId::parse(id).unwrap_or_else(|e| panic!("backend id `{id}`: {e}")),
        kind: BackendKind::Node,
        protocol: Protocol::OpenAi,
        label: id.to_owned(),
        base_url: base_url.trim_end_matches('/').to_owned(),
        credential: CredentialSource::None,
        tags: vec!["fake".to_owned()],
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
        price: Some(PriceModel::Free),
        health: Health::Ready {
            since_unix: 0,
            slots_busy: 0,
            slots_total: 4,
            tps_p50: None,
        },
        provenance: Provenance::Manual,
        endpoint: None,
        enabled: true,
        devices: Vec::new(),
        last_error: None,
    }
}

/// A `FirstHealthy` route from `alias` to `targets`, in order, with the shipped default
/// retry policy and no model rewrite.
///
/// # Panics
/// When `alias` or a target is not a valid slug.
pub fn route(alias: &str, targets: &[&str]) -> ModelRoute {
    route_to(alias, targets, None)
}

/// As [`route`], rewriting the outbound `"model"` to `model` on every target — the case a
/// swap test needs, because the alias the client sends is not the id the upstream knows.
///
/// # Panics
/// When `alias` or a target is not a valid slug.
pub fn route_to(alias: &str, targets: &[&str], model: Option<&str>) -> ModelRoute {
    ModelRoute {
        alias: Alias::parse(alias).unwrap_or_else(|e| panic!("alias `{alias}`: {e}")),
        targets: targets
            .iter()
            .map(|t| RouteTarget {
                backend: BackendSelector::Id(
                    BackendId::parse(t).unwrap_or_else(|e| panic!("target `{t}`: {e}")),
                ),
                model: model.map(str::to_owned),
                weight: 1,
            })
            .collect(),
        strategy: Strategy::FirstHealthy,
        filter: RouteFilter::default(),
        retry: RetryPolicy::default(),
        is_default: false,
        description: None,
    }
}

/// A [`RouteFile`] whose `default_alias` is the first route's alias, with that route marked
/// default — the shape `TableBuilder::compile` expects.
///
/// # Panics
/// When `routes` is empty, or `default` is not a valid alias.
pub fn route_file(default: &str, mut routes: Vec<ModelRoute>) -> RouteFile {
    assert!(!routes.is_empty(), "a route file needs at least one route");
    let default_alias =
        Alias::parse(default).unwrap_or_else(|e| panic!("default alias `{default}`: {e}"));
    for r in &mut routes {
        r.is_default = r.alias == default_alias;
    }
    RouteFile {
        schema_version: 1,
        default_alias,
        routes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_backend_is_ready_free_and_stored_without_a_trailing_v1() {
        let b = backend("stub-a", "http://127.0.0.1:9001/", &["stub-model"]);
        assert_eq!(b.base_url, "http://127.0.0.1:9001");
        assert!(matches!(b.health, Health::Ready { .. }));
        assert_eq!(b.price, Some(PriceModel::Free));
        assert_eq!(b.models.len(), 1);
        assert!(b.enabled);
    }

    #[test]
    fn exactly_the_named_route_is_the_default() {
        let file = route_file("auto", vec![route("auto", &["a"]), route("slow", &["b"])]);
        assert_eq!(file.default_alias.as_str(), "auto");
        assert!(file.routes[0].is_default);
        assert!(!file.routes[1].is_default);
    }

    #[test]
    fn a_route_can_rewrite_the_outbound_model() {
        let r = route_to("auto", &["a"], Some("carnice-9b"));
        assert_eq!(r.targets[0].model.as_deref(), Some("carnice-9b"));
        assert_eq!(route("auto", &["a"]).targets[0].model, None);
    }
}
