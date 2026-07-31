//! OWNER: unit P-02 (providers/src/vast/{api,query,mod}.rs). Do not edit outside that unit.
//!
//! Query construction. There is **one** builder, so "auto — cheapest" and the browser table
//! cannot ever run different searches — which is precisely the documented bug where
//! ApexRouter's ancestor rented from a stricter candidate set than the user had seen.
//!
//! Every operator used here was confirmed with a live read-only `PUT /api/v0/search/asks/`
//! against Andre's account (2026-07-30), not merely read in a doc:
//!
//! * `{"eq": …}` for a single `gpu_name`, `{"in": [ … ]}` for several;
//! * `{"gte": …}` / `{"lte": …}` on `num_gpus`, `dph_total`, `reliability2`, `inet_down`,
//!   `disk_space` and `cuda_max_good` — an impossible bound really does return
//!   `{"offers": []}`, so these filters are applied server-side rather than silently ignored;
//! * `"type": "ask"` and `"order": [["dph_total", "asc"]]`.
//!
//! **`geo` is deliberately never sent.** The wire field is a display string (`"Czechia, CZ"`,
//! and sometimes `", US"` with no city at all), so geography is a client-side match on its
//! tail — done by P-03, which must be able to report dropping it as a relaxation.

use apexrouter_protocol::OfferQuery;
use serde_json::{Map, Value};

/// `type` selects on-demand asks; the bid market has its own `is_bid`/`min_bid` shape.
const QUERY_TYPE: &str = "ask";
/// What "auto — cheapest" means, and therefore what the browser shows by default.
const DEFAULT_ORDER: (&str, &str) = ("dph_total", "asc");

/// Build the `{"q": …}` body for `PUT /api/v0/search/asks/`, exactly as verified in
/// `docs/port/00c`.
///
/// A fixture test asserts the produced body shape, because getting this wrong is how you
/// rent a box the operator never saw.
///
/// Shape rules:
///
/// * `rentable` is **always** `{"eq": true}` — a search that can return an unrentable row is
///   a search whose top result is a lie.
/// * `num_gpus_min == num_gpus_max` collapses to `{"eq": n}`; otherwise the bounds that are
///   set become `{"gte": …, "lte": …}`. A zero bound means "unset", never "zero GPUs".
/// * a zero `limit` is omitted so the server default applies, rather than asking for zero
///   rows.
/// * [`OfferQuery::order`] wins when it is set; otherwise cheapest-first.
/// * [`OfferQuery::extra`] is merged **last**, so an operator can override anything above it
///   — including with an operator vast adds after this was written.
pub fn build_query(q: &OfferQuery) -> Value {
    let mut body = Map::new();

    if let Some(gpu) = gpu_name_clause(&q.gpu_names) {
        body.insert("gpu_name".to_owned(), gpu);
    }
    if let Some(n) = num_gpus_clause(q.num_gpus_min, q.num_gpus_max) {
        body.insert("num_gpus".to_owned(), n);
    }
    body.insert("rentable".to_owned(), op("eq", Value::Bool(true)));
    if let Some(v) = q.verified {
        body.insert("verified".to_owned(), op("eq", Value::Bool(v)));
    }
    insert_bound(&mut body, "dph_total", "lte", q.max_dph);
    insert_bound(&mut body, "reliability2", "gte", q.min_reliability);
    insert_bound(&mut body, "inet_down", "gte", q.min_inet_down);
    insert_bound(&mut body, "disk_space", "gte", q.min_disk_gb.map(f64::from));
    insert_bound(&mut body, "cuda_max_good", "gte", q.min_cuda);

    body.insert("type".to_owned(), Value::String(QUERY_TYPE.to_owned()));
    body.insert("order".to_owned(), order_clause(&q.order));
    if q.limit > 0 {
        body.insert("limit".to_owned(), Value::from(q.limit));
    }

    for (k, v) in &q.extra {
        body.insert(k.clone(), v.clone());
    }

    let mut envelope = Map::new();
    envelope.insert("q".to_owned(), Value::Object(body));
    Value::Object(envelope)
}

/// `{"<operator>": <value>}` — the one shape every vast filter takes.
fn op(operator: &str, value: Value) -> Value {
    let mut m = Map::new();
    m.insert(operator.to_owned(), value);
    Value::Object(m)
}

/// One name is an `eq`; several are an `in`; none is no constraint at all.
fn gpu_name_clause(names: &[String]) -> Option<Value> {
    let names: Vec<&String> = names.iter().filter(|n| !n.trim().is_empty()).collect();
    match names.len() {
        0 => None,
        1 => Some(op("eq", Value::String(names[0].clone()))),
        _ => Some(op(
            "in",
            Value::Array(
                names
                    .into_iter()
                    .map(|n| Value::String(n.clone()))
                    .collect(),
            ),
        )),
    }
}

/// A GPU-count clause, collapsing an exact range to `eq`. A zero bound is "unset".
fn num_gpus_clause(min: u32, max: u32) -> Option<Value> {
    if min > 0 && min == max {
        return Some(op("eq", Value::from(min)));
    }
    let mut m = Map::new();
    if min > 0 {
        m.insert("gte".to_owned(), Value::from(min));
    }
    if max > 0 && max >= min {
        m.insert("lte".to_owned(), Value::from(max));
    }
    if m.is_empty() {
        None
    } else {
        Some(Value::Object(m))
    }
}

/// Add `field: {op: value}` when the bound is set and finite.
fn insert_bound(body: &mut Map<String, Value>, field: &str, operator: &str, v: Option<f64>) {
    let Some(v) = v.filter(|v| v.is_finite()) else {
        return;
    };
    if let Some(n) = serde_json::Number::from_f64(v) {
        body.insert(field.to_owned(), op(operator, Value::Number(n)));
    }
}

/// `[[field, "asc"|"desc"], …]`. An unrecognised direction becomes `asc` rather than being
/// sent verbatim, because vast ignores what it does not understand and we would then be
/// ordering by something other than what the surface displayed.
fn order_clause(order: &[(String, String)]) -> Value {
    let pairs: Vec<Value> = order
        .iter()
        .filter(|(f, _)| !f.trim().is_empty())
        .map(|(field, dir)| {
            let dir = if dir.eq_ignore_ascii_case("desc") {
                "desc"
            } else {
                "asc"
            };
            Value::Array(vec![
                Value::String(field.clone()),
                Value::String(dir.to_owned()),
            ])
        })
        .collect();
    if pairs.is_empty() {
        Value::Array(vec![Value::Array(vec![
            Value::String(DEFAULT_ORDER.0.to_owned()),
            Value::String(DEFAULT_ORDER.1.to_owned()),
        ])])
    } else {
        Value::Array(pairs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_protocol::GeoFilter;
    use serde_json::json;

    fn query() -> OfferQuery {
        OfferQuery {
            gpu_names: vec!["RTX 3090".into()],
            num_gpus_min: 2,
            num_gpus_max: 2,
            max_dph: None,
            min_reliability: None,
            min_inet_down: None,
            min_disk_gb: None,
            min_cuda: None,
            geo: GeoFilter::Any,
            verified: Some(true),
            limit: 3,
            order: Vec::new(),
            extra: Map::new(),
        }
    }

    /// The body from `docs/port/00c` §"Offer search", byte for byte in JSON terms.
    #[test]
    fn body_is_exactly_the_shape_verified_in_00c() {
        assert_eq!(
            build_query(&query()),
            json!({
                "q": {
                    "gpu_name": {"eq": "RTX 3090"},
                    "num_gpus": {"eq": 2},
                    "rentable": {"eq": true},
                    "verified": {"eq": true},
                    "type": "ask",
                    "order": [["dph_total", "asc"]],
                    "limit": 3
                }
            })
        );
    }

    #[test]
    fn several_gpu_names_become_an_in_list_and_a_range_becomes_gte_lte() {
        let mut q = query();
        q.gpu_names = vec!["RTX 3090".into(), "RTX 4090".into(), "  ".into()];
        q.num_gpus_min = 2;
        q.num_gpus_max = 4;
        let b = build_query(&q);
        assert_eq!(b["q"]["gpu_name"], json!({"in": ["RTX 3090", "RTX 4090"]}));
        assert_eq!(b["q"]["num_gpus"], json!({"gte": 2, "lte": 4}));
    }

    #[test]
    fn every_numeric_bound_is_sent_with_the_operator_verified_live() {
        let mut q = query();
        q.max_dph = Some(0.6);
        q.min_reliability = Some(0.98);
        q.min_inet_down = Some(500.0);
        q.min_disk_gb = Some(200);
        q.min_cuda = Some(12.4);
        let b = build_query(&q);
        assert_eq!(b["q"]["dph_total"], json!({"lte": 0.6}));
        // `reliability2` — the field the offer itself carries. A live search with
        // `{"gte": 0.9999}` returns zero rows, so this really is filtered server-side.
        assert_eq!(b["q"]["reliability2"], json!({"gte": 0.98}));
        assert_eq!(b["q"]["inet_down"], json!({"gte": 500.0}));
        assert_eq!(b["q"]["disk_space"], json!({"gte": 200.0}));
        assert_eq!(b["q"]["cuda_max_good"], json!({"gte": 12.4}));
    }

    /// Geography is a client-side tail match. If it ever reached the wire, a relaxation
    /// P-03 reported as "geo dropped" would not actually have widened the search.
    #[test]
    fn geo_never_reaches_the_wire() {
        let mut q = query();
        q.geo = GeoFilter::Codes(vec!["CZ".into()]);
        let b = build_query(&q);
        let obj = b["q"].as_object().expect("q is an object");
        assert!(!obj.contains_key("geolocation"), "{b}");
        assert!(!obj.contains_key("geo"), "{b}");
        assert!(!obj.contains_key("geolocode"), "{b}");
    }

    #[test]
    fn unset_bounds_are_omitted_and_a_zero_limit_lets_the_server_decide() {
        let mut q = query();
        q.verified = None;
        q.limit = 0;
        let b = build_query(&q);
        let obj = b["q"].as_object().expect("q is an object");
        for absent in [
            "dph_total",
            "reliability2",
            "inet_down",
            "disk_space",
            "cuda_max_good",
            "verified",
            "limit",
        ] {
            assert!(!obj.contains_key(absent), "{absent} should be absent: {b}");
        }
        // rentable is never optional.
        assert_eq!(obj["rentable"], json!({"eq": true}));
    }

    #[test]
    fn order_defaults_to_cheapest_first_and_normalises_the_direction() {
        let mut q = query();
        q.order = vec![
            ("dlperf_per_dphtotal".into(), "DESC".into()),
            ("dph_total".into(), "sideways".into()),
        ];
        assert_eq!(
            build_query(&q)["q"]["order"],
            json!([["dlperf_per_dphtotal", "desc"], ["dph_total", "asc"]])
        );
        q.order.clear();
        assert_eq!(build_query(&q)["q"]["order"], json!([["dph_total", "asc"]]));
    }

    #[test]
    fn extra_is_merged_last_so_an_operator_can_override_anything() {
        let mut q = query();
        q.extra.insert("cluster_id".to_owned(), json!({"eq": 7}));
        q.extra.insert("limit".to_owned(), json!(64));
        let b = build_query(&q);
        assert_eq!(b["q"]["cluster_id"], json!({"eq": 7}));
        assert_eq!(b["q"]["limit"], json!(64));
    }

    /// No GPU constraint at all is a legal broad search — it is how P-03 builds the live
    /// `gpu_name` vocabulary.
    #[test]
    fn a_broad_search_carries_no_gpu_constraint() {
        let mut q = query();
        q.gpu_names.clear();
        q.num_gpus_min = 0;
        q.num_gpus_max = 0;
        let b = build_query(&q);
        let obj = b["q"].as_object().expect("q is an object");
        assert!(!obj.contains_key("gpu_name"), "{b}");
        assert!(!obj.contains_key("num_gpus"), "{b}");
    }
}
