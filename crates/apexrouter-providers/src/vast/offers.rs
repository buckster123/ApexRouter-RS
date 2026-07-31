//! OWNER: unit P-03 (providers/src/vast/offers.rs). Do not edit outside that unit.
//!
//! Turning a saved [`SearchProfile`] into a live market query.
//!
//! Any relaxation appends a string to `OfferSearchResult::relaxations` — e.g.
//! `"widened: geo dropped, reliability 0.99 -> 0.97"` — that **every surface renders as an
//! explicit banner**. Geo is a client-side match on the **tail** of `geolocation`, and
//! `gpu_name_vocabulary` comes from a live broad search, never a constant: `00c` proves
//! those strings change.
//!
//! # One search path
//!
//! [`search_unified`] is the *only* way an offer reaches any surface. "Auto — cheapest" is
//! [`cheapest`], which is `result.offers.first()` — literally the top row of the browser
//! table, because the rows are the same `Vec` produced by the same query. The ancestor's
//! documented bug (the browser searched `reliability>0.97 / inet_down>300` while
//! `vast_up.sh` rented at `>0.99 / >500`, so "auto" rented a box the operator never saw) is
//! unreachable from here: there is no second threshold set to disagree with.
//!
//! # The client-side predicate
//!
//! Everything the server was asked for is re-checked locally by [`offer_matches`]. `00c`
//! only verified the `eq` and `in` operators, so a range constraint may or may not survive
//! the trip; re-checking means the price ceiling is enforced by *us* whatever the API did
//! with it, and it means the candidate set is a pure function of the [`OfferQuery`] rather
//! than of the server's mood. An optional quality field the API omitted is treated as
//! unknown and does **not** exclude the row — except `geolocation` under a non-`Any` geo
//! filter, where "unknown location" cannot satisfy "must be in the EU".
//!
//! # What is never relaxed
//!
//! `max_dph`, `gpu_names`, the GPU-count range, `min_disk_gb` and `min_cuda` survive every
//! widening. The price ceiling is a money guard, the GPU names and count are the operator's
//! intent, and disk/CUDA gate whether the image can run at all. Widening drops the geo
//! filter and lowers the reliability and bandwidth floors — exactly the ancestor's stage-2
//! behaviour, except that here it is announced.

use super::api::VastApi;
use apexrouter_core::error::Result;
use apexrouter_protocol::{GeoFilter, Offer, OfferQuery, OfferSearchResult, SearchProfile};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

/// Rows returned when neither the profile nor the caller says otherwise.
pub const DEFAULT_OFFER_LIMIT: u32 = 50;

/// Ordering used when neither the profile nor the caller says otherwise: cheapest first, so
/// `offers[0]` is both the top browser row and what "auto — cheapest" rents.
pub const DEFAULT_ORDER_FIELD: &str = "dph_total";

/// The reliability floor a widened search falls back to (the ancestor's stage-2 value).
pub const WIDENED_MIN_RELIABILITY: f64 = 0.97;

/// The inbound-bandwidth floor, Mbps, a widened search falls back to.
pub const WIDENED_MIN_INET_DOWN: f64 = 300.0;

/// How many rows the vocabulary sweep asks for. Big enough to see the long tail of card
/// names, small enough to stay one cheap read-only call.
pub const VOCABULARY_SAMPLE_LIMIT: u32 = 1000;

/// Per-call overrides layered on top of a saved profile (`--gpu`, `--num-gpus`, `--geo`,
/// `--max-price`).
///
/// Every field is `Option`: `None` means "whatever the profile said". This is the shape
/// behind `apexrouter vast offers [--profile P] [--gpu G] [--num-gpus N] [--geo G]
/// [--max-price F]`, the `apexrouter_vast_offers` MCP tool and the web UI's offer browser —
/// all three funnel into [`profile_to_query`], which is why they cannot disagree.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct QueryOverrides {
    /// Replace the profile's `gpu_names` wholesale (`--gpu "RTX 3090"`).
    #[serde(default)]
    pub gpu_names: Option<Vec<String>>,
    /// Minimum GPU count.
    #[serde(default)]
    pub num_gpus_min: Option<u32>,
    /// Maximum GPU count.
    #[serde(default)]
    pub num_gpus_max: Option<u32>,
    /// Ceiling on `dph_total`, dollars per hour (`--max-price`).
    #[serde(default)]
    pub max_dph: Option<f64>,
    /// Floor on `reliability2`, 0..1.
    #[serde(default)]
    pub min_reliability: Option<f64>,
    /// Floor on `inet_down`, Mbps.
    #[serde(default)]
    pub min_inet_down: Option<f64>,
    /// Floor on disk, GB.
    #[serde(default)]
    pub min_disk_gb: Option<u32>,
    /// Floor on `cuda_max_good`.
    #[serde(default)]
    pub min_cuda: Option<f64>,
    /// Geography filter (`--geo EU`).
    #[serde(default)]
    pub geo: Option<GeoFilter>,
    /// Restrict to verified hosts. `00c`: accepted by the API, not echoed on the offer.
    #[serde(default)]
    pub verified: Option<bool>,
    /// Row cap.
    #[serde(default)]
    pub limit: Option<u32>,
    /// `[(field, "asc"|"desc")]`.
    #[serde(default)]
    pub order: Option<Vec<(String, String)>>,
    /// Free-form passthrough, merged over the profile's own `extra`.
    #[serde(default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl QueryOverrides {
    /// Nothing overridden — the profile as saved.
    pub fn none() -> Self {
        Self::default()
    }

    /// Pin an exact GPU count, the way `--num-gpus 2` means "two, not two-to-four".
    pub fn with_num_gpus(mut self, n: u32) -> Self {
        self.num_gpus_min = Some(n);
        self.num_gpus_max = Some(n);
        self
    }

    /// Replace the GPU-name list (`--gpu "RTX 3090"`).
    pub fn with_gpu_names<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.gpu_names = Some(names.into_iter().map(Into::into).collect());
        self
    }

    /// Replace the geography filter.
    pub fn with_geo(mut self, geo: GeoFilter) -> Self {
        self.geo = Some(geo);
        self
    }

    /// Set the hourly price ceiling, dollars.
    pub fn with_max_dph(mut self, dph: f64) -> Self {
        self.max_dph = Some(dph);
        self
    }

    /// Set the row cap.
    pub fn with_limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }
}

/// Profile + overrides → the one query shape.
///
/// Pure and total: no I/O, no failure mode. A profile floor of zero (`min_reliability: 0.0`,
/// `min_inet_down: 0`, `min_disk_gb: 0`) becomes `None` — "no constraint" — rather than a
/// tautological one, so the widening logic does not report relaxing something nobody asked
/// for. `f32` profile floors are rounded to six decimals on the way to `f64` so that a saved
/// `0.99` does not become `0.9900000095367432` and quietly exclude an offer whose
/// `reliability2` is exactly `0.99`.
pub fn profile_to_query(p: &SearchProfile, overrides: &QueryOverrides) -> OfferQuery {
    let gpu_names = overrides
        .gpu_names
        .clone()
        .unwrap_or_else(|| p.gpu_names.clone());

    let mut num_gpus_min = overrides.num_gpus_min.unwrap_or(p.num_gpus_min).max(1);
    let mut num_gpus_max = overrides.num_gpus_max.unwrap_or(p.num_gpus_max).max(1);
    if num_gpus_min > num_gpus_max {
        std::mem::swap(&mut num_gpus_min, &mut num_gpus_max);
    }

    let max_dph = overrides
        .max_dph
        .or_else(|| p.max_dph.map(|m| m.as_usd()))
        .filter(|d| d.is_finite() && *d > 0.0);

    let min_reliability = overrides
        .min_reliability
        .or_else(|| positive(round6(f64::from(p.min_reliability))));
    let min_inet_down = overrides
        .min_inet_down
        .or_else(|| positive(f64::from(p.min_inet_down)));
    let min_disk_gb = overrides.min_disk_gb.or(if p.min_disk_gb > 0 {
        Some(p.min_disk_gb)
    } else {
        None
    });
    let min_cuda = overrides
        .min_cuda
        .or_else(|| p.min_cuda.map(|c| round6(f64::from(c))));

    let mut extra = p.extra.clone();
    for (k, v) in &overrides.extra {
        extra.insert(k.clone(), v.clone());
    }

    OfferQuery {
        gpu_names,
        num_gpus_min,
        num_gpus_max,
        max_dph,
        min_reliability,
        min_inet_down,
        min_disk_gb,
        min_cuda,
        geo: overrides.geo.clone().unwrap_or_else(|| p.geo.clone()),
        verified: overrides.verified,
        limit: overrides.limit.unwrap_or(DEFAULT_OFFER_LIMIT).max(1),
        order: overrides
            .order
            .clone()
            .unwrap_or_else(|| vec![(DEFAULT_ORDER_FIELD.to_string(), "asc".to_string())]),
        extra,
    }
}

/// **One** search path, used by both `--auto` and the browser table.
///
/// Runs the profile's query, applies [`offer_matches`] to what came back, and — only if that
/// leaves nothing — runs the widened query described by [`widen`] and records what it gave
/// up in `relaxations`. Rows are ordered per `OfferQuery::order` and truncated to
/// `OfferQuery::limit`, so `offers.first()` is the cheapest surviving offer and is exactly
/// what [`cheapest`] hands to the rent path.
///
/// The returned `gpu_name_vocabulary` is whatever card names this search actually saw, taken
/// from the unfiltered rows of every stage — live, never compiled in. The dropdown wants the
/// broader sweep in [`gpu_name_vocabulary`].
///
/// # Errors
///
/// Propagates the transport/decoding failure from [`VastApi::search`]. An empty market is
/// `Ok` with no offers, not an error.
pub async fn search_unified(
    api: &dyn VastApi,
    p: &SearchProfile,
    o: &QueryOverrides,
) -> Result<OfferSearchResult> {
    let strict = profile_to_query(p, o);

    let raw = api.search(&strict).await?;
    let mut vocabulary = raw.gpu_name_vocabulary.clone();
    collect_gpu_names(&raw.offers, &mut vocabulary);

    let mut relaxations: Vec<String> = raw.relaxations.clone();
    let mut kept = retain_matching(&strict, raw.offers);
    let mut effective = strict.clone();

    if kept.is_empty() {
        if let Some((wide, note)) = widen(&strict) {
            tracing::debug!(relaxation = %note, "vast offer search found nothing, widening");
            let raw_wide = api.search(&wide).await?;
            collect_gpu_names(&raw_wide.offers, &mut vocabulary);
            relaxations.extend(raw_wide.relaxations.iter().cloned());
            relaxations.push(note);
            kept = retain_matching(&wide, raw_wide.offers);
            effective = wide;
        }
    }

    sort_offers(&effective, &mut kept);
    kept.truncate(effective.limit as usize);

    dedupe_strings(&mut vocabulary);

    Ok(OfferSearchResult {
        offers: kept,
        relaxations,
        queried_at_unix: now_unix(),
        gpu_name_vocabulary: vocabulary,
    })
}

/// The live `gpu_name` vocabulary for the dropdown, from a broad search.
///
/// Deliberately not a constant: `00c` lists the strings that were live on 2026-07-30 and
/// says in as many words that they change with the market. One unconstrained read-only
/// search, distinct names, sorted. Free — offer search costs nothing.
///
/// # Errors
///
/// Propagates the transport/decoding failure from [`VastApi::search`].
pub async fn gpu_name_vocabulary(api: &dyn VastApi) -> Result<Vec<String>> {
    let broad = OfferQuery {
        gpu_names: Vec::new(),
        num_gpus_min: 1,
        num_gpus_max: 8,
        max_dph: None,
        min_reliability: None,
        min_inet_down: None,
        min_disk_gb: None,
        min_cuda: None,
        geo: GeoFilter::Any,
        verified: None,
        limit: VOCABULARY_SAMPLE_LIMIT,
        order: Vec::new(),
        extra: serde_json::Map::new(),
    };
    let res = api.search(&broad).await?;
    let mut names = res.gpu_name_vocabulary.clone();
    collect_gpu_names(&res.offers, &mut names);
    dedupe_strings(&mut names);
    Ok(names)
}

/// "Auto — cheapest matching offer": the first row of the very list every surface rendered.
///
/// This function exists so no caller is tempted to re-derive "cheapest" from its own search.
pub fn cheapest(result: &OfferSearchResult) -> Option<&Offer> {
    result.offers.first()
}

/// Does this offer satisfy the query, checked locally?
///
/// Applied to every row of every stage, so the candidate set is a pure function of the
/// query. An absent optional field is unknown and does not exclude the row; an absent
/// `geolocation` cannot satisfy a non-`Any` geo filter. A row the market marks `rented` or
/// not `rentable` is always excluded — that is not a constraint, it is arithmetic.
///
/// `verified` is not re-checked: `00c` records that the API accepts the filter but does not
/// echo the field back on the offer, so there is nothing local to test.
pub fn offer_matches(q: &OfferQuery, o: &Offer) -> bool {
    if o.rented == Some(true) || o.rentable == Some(false) {
        return false;
    }
    if !q.gpu_names.is_empty()
        && !q
            .gpu_names
            .iter()
            .any(|n| n.trim().eq_ignore_ascii_case(o.gpu_name.trim()))
    {
        return false;
    }
    if o.num_gpus < q.num_gpus_min || o.num_gpus > q.num_gpus_max {
        return false;
    }
    if let Some(max) = q.max_dph {
        // Money: a non-finite price is never "under the cap".
        if !o.dph_total.is_finite() || o.dph_total > max {
            return false;
        }
    }
    if !at_least(o.reliability2, q.min_reliability) {
        return false;
    }
    if !at_least(o.inet_down, q.min_inet_down) {
        return false;
    }
    if !at_least(o.disk_space, q.min_disk_gb.map(f64::from)) {
        return false;
    }
    if !at_least(o.cuda_max_good, q.min_cuda) {
        return false;
    }
    if !matches!(q.geo, GeoFilter::Any) {
        match o.geolocation.as_deref() {
            Some(g) if q.geo.matches(g) => {}
            _ => return false,
        }
    }
    true
}

/// The widened query and the sentence that describes it, or `None` when there is nothing
/// left to give up.
///
/// Drops the geo filter and lowers the reliability and bandwidth floors to
/// [`WIDENED_MIN_RELIABILITY`] / [`WIDENED_MIN_INET_DOWN`]. The price ceiling, the GPU names,
/// the GPU count, the disk floor and the CUDA floor are never touched. The sentence reads
/// `"widened: geo dropped, reliability 0.99 -> 0.97"`.
pub fn widen(q: &OfferQuery) -> Option<(OfferQuery, String)> {
    let mut w = q.clone();
    let mut notes: Vec<String> = Vec::new();

    if !matches!(w.geo, GeoFilter::Any) {
        notes.push("geo dropped".to_string());
        w.geo = GeoFilter::Any;
    }
    if let Some(r) = w.min_reliability {
        if r > WIDENED_MIN_RELIABILITY {
            notes.push(format!(
                "reliability {r:.2} -> {WIDENED_MIN_RELIABILITY:.2}"
            ));
            w.min_reliability = Some(WIDENED_MIN_RELIABILITY);
        }
    }
    if let Some(d) = w.min_inet_down {
        if d > WIDENED_MIN_INET_DOWN {
            notes.push(format!(
                "inet_down {d:.0} -> {WIDENED_MIN_INET_DOWN:.0} Mbps"
            ));
            w.min_inet_down = Some(WIDENED_MIN_INET_DOWN);
        }
    }

    if notes.is_empty() {
        None
    } else {
        Some((w, format!("widened: {}", notes.join(", "))))
    }
}

// ---------------------------------------------------------------------------------------
// internals
// ---------------------------------------------------------------------------------------

/// `Some(v)` when `v` is a usable positive floor, `None` when it is zero/negative/NaN.
fn positive(v: f64) -> Option<f64> {
    if v.is_finite() && v > 0.0 {
        Some(v)
    } else {
        None
    }
}

/// Round an `f32`-derived `f64` to six decimals, so `0.99f32` compares as `0.99`.
fn round6(v: f64) -> f64 {
    if v.is_finite() {
        (v * 1_000_000.0).round() / 1_000_000.0
    } else {
        v
    }
}

/// Floor check where an absent value means "unknown", which never excludes.
fn at_least(value: Option<f64>, floor: Option<f64>) -> bool {
    match (value, floor) {
        (_, None) => true,
        (None, Some(_)) => true,
        (Some(v), Some(f)) => v.is_finite() && v >= f,
    }
}

fn retain_matching(q: &OfferQuery, offers: Vec<Offer>) -> Vec<Offer> {
    offers.into_iter().filter(|o| offer_matches(q, o)).collect()
}

fn collect_gpu_names(offers: &[Offer], into: &mut Vec<String>) {
    for o in offers {
        let n = o.gpu_name.trim();
        if !n.is_empty() {
            into.push(n.to_string());
        }
    }
}

fn dedupe_strings(v: &mut Vec<String>) {
    v.sort();
    v.dedup();
}

/// The numeric field a sort key names, when we know how to read it locally.
fn sort_key(field: &str, o: &Offer) -> Option<f64> {
    match field {
        "dph_total" => Some(o.dph_total),
        "dph_base" => o.dph_base,
        "reliability2" => o.reliability2,
        "inet_down" => o.inet_down,
        "inet_up" => o.inet_up,
        "dlperf" => o.dlperf,
        "dlperf_per_dphtotal" => o.dlperf_per_dphtotal,
        "gpu_total_ram" => Some(o.gpu_total_ram as f64),
        "num_gpus" => Some(f64::from(o.num_gpus)),
        _ => None,
    }
}

/// Re-impose the query's ordering locally so the top row is the top row on every surface.
/// An order key we cannot read locally leaves the server's order alone.
fn sort_offers(q: &OfferQuery, offers: &mut [Offer]) {
    let Some((field, dir)) = q.order.first() else {
        return;
    };
    if offers.iter().any(|o| sort_key(field, o).is_none()) {
        return;
    }
    let desc = dir.eq_ignore_ascii_case("desc");
    offers.sort_by(|a, b| {
        let (ka, kb) = (sort_key(field, a), sort_key(field, b));
        let ord = match (ka, kb) {
            (Some(x), Some(y)) => x.total_cmp(&y),
            _ => Ordering::Equal,
        };
        if desc {
            ord.reverse()
        } else {
            ord
        }
    });
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_core::error::Error;
    use apexrouter_protocol::{
        ContainerLaunch, ImageType, InstanceId, Money, ProfileId, VastAccount, VastInstance,
    };
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::Mutex;

    // ---- doubles ----------------------------------------------------------------------
    //
    // Hermetic by construction: no socket is opened anywhere in this module, and the money
    // methods panic rather than pretend, so a future edit that reaches for `create` from a
    // test fails loudly instead of silently.

    #[derive(Default)]
    struct FakeApi {
        /// One `Vec<Offer>` per search, in order. The last is reused if we run out.
        pages: Vec<Vec<Offer>>,
        seen: Mutex<Vec<OfferQuery>>,
    }

    impl FakeApi {
        fn with(pages: Vec<Vec<Offer>>) -> Self {
            FakeApi {
                pages,
                seen: Mutex::new(Vec::new()),
            }
        }
        fn queries(&self) -> Vec<OfferQuery> {
            self.seen.lock().expect("lock").clone()
        }
        fn calls(&self) -> usize {
            self.seen.lock().expect("lock").len()
        }
    }

    #[async_trait]
    impl VastApi for FakeApi {
        async fn account(&self) -> Result<VastAccount> {
            Ok(VastAccount::default())
        }
        async fn search(&self, q: &OfferQuery) -> Result<OfferSearchResult> {
            let mut seen = self.seen.lock().expect("lock");
            let idx = seen.len().min(self.pages.len().saturating_sub(1));
            seen.push(q.clone());
            Ok(OfferSearchResult {
                offers: self.pages.get(idx).cloned().unwrap_or_default(),
                ..Default::default()
            })
        }
        async fn create(
            &self,
            _offer_id: u64,
            _launch: &ContainerLaunch,
            _label: &str,
        ) -> Result<InstanceId> {
            panic!("P-03 tests never touch the money path");
        }
        async fn instances(&self) -> Result<Vec<VastInstance>> {
            Ok(Vec::new())
        }
        async fn instance(&self, _id: InstanceId) -> Result<Option<VastInstance>> {
            Ok(None)
        }
        async fn destroy(&self, _id: InstanceId) -> Result<()> {
            panic!("P-03 tests never destroy an instance");
        }
        async fn logs(&self, _id: InstanceId, _tail: u32) -> Result<Vec<String>> {
            Err(Error::NotFound("logs".into()))
        }
        async fn exec(&self, _id: InstanceId, _cmd: &str) -> Result<String> {
            Err(Error::NotFound("exec".into()))
        }
    }

    fn offer(v: serde_json::Value) -> Offer {
        serde_json::from_value(v).expect("offer fixture")
    }

    /// A realistic row: the 2× RTX 3090 in Czechia that `00c` recorded live.
    fn cz_3090() -> Offer {
        offer(json!({
            "id": 43731729, "gpu_name": "RTX 3090", "num_gpus": 2,
            "gpu_ram": 24576, "gpu_total_ram": 49152, "dph_total": 0.305,
            "reliability2": 0.9897, "inet_down": 561.8, "disk_space": 383.0,
            "cuda_max_good": 13.2, "geolocation": "Czechia, CZ", "rentable": true
        }))
    }

    fn us_h100() -> Offer {
        offer(json!({
            "id": 5001, "gpu_name": "H100 SXM", "num_gpus": 2,
            "gpu_ram": 81559, "gpu_total_ram": 163118, "dph_total": 3.344,
            "reliability2": 0.9951, "inet_down": 2000.0, "disk_space": 500.0,
            "cuda_max_good": 13.1, "geolocation": "Montana, US", "rentable": true
        }))
    }

    fn profile() -> SearchProfile {
        SearchProfile {
            id: ProfileId::parse("rtx3090-2-4").expect("id"),
            label: "RTX 3090 ×2–4".to_string(),
            gpu_names: vec!["RTX 3090".to_string()],
            num_gpus_min: 2,
            num_gpus_max: 4,
            max_dph: Some(Money::from_usd(0.90)),
            min_reliability: 0.98,
            min_inet_down: 300,
            min_disk_gb: 80,
            min_cuda: Some(12.0),
            geo: GeoFilter::Any,
            image_type: ImageType::Prebuilt,
            extra: serde_json::Map::new(),
        }
    }

    // ---- profile_to_query -------------------------------------------------------------

    #[test]
    fn profile_to_query_carries_every_field() {
        let q = profile_to_query(&profile(), &QueryOverrides::none());
        assert_eq!(q.gpu_names, vec!["RTX 3090".to_string()]);
        assert_eq!((q.num_gpus_min, q.num_gpus_max), (2, 4));
        assert_eq!(q.max_dph, Some(0.90));
        assert_eq!(q.min_reliability, Some(0.98));
        assert_eq!(q.min_inet_down, Some(300.0));
        assert_eq!(q.min_disk_gb, Some(80));
        assert_eq!(q.min_cuda, Some(12.0));
        assert_eq!(q.geo, GeoFilter::Any);
        assert_eq!(q.limit, DEFAULT_OFFER_LIMIT);
        assert_eq!(
            q.order,
            vec![("dph_total".to_string(), "asc".to_string())],
            "cheapest first, so offers[0] is what auto rents"
        );
    }

    #[test]
    fn f32_floors_do_not_become_long_decimals() {
        let mut p = profile();
        p.min_reliability = 0.99;
        p.min_cuda = Some(12.4);
        let q = profile_to_query(&p, &QueryOverrides::none());
        assert_eq!(q.min_reliability, Some(0.99));
        assert_eq!(q.min_cuda, Some(12.4));
        // and the strict filter must not exclude an offer that is exactly at the floor
        let o = offer(json!({
            "id": 1, "gpu_name": "RTX 3090", "num_gpus": 2, "gpu_ram": 24576,
            "gpu_total_ram": 49152, "dph_total": 0.30, "reliability2": 0.99,
            "inet_down": 400.0, "disk_space": 100.0, "cuda_max_good": 12.4
        }));
        assert!(offer_matches(&q, &o));
    }

    #[test]
    fn overrides_beat_the_profile() {
        let o = QueryOverrides::none()
            .with_gpu_names(["H100 SXM"])
            .with_num_gpus(2)
            .with_geo(GeoFilter::Us)
            .with_max_dph(3.50)
            .with_limit(12);
        let q = profile_to_query(&profile(), &o);
        assert_eq!(q.gpu_names, vec!["H100 SXM".to_string()]);
        assert_eq!((q.num_gpus_min, q.num_gpus_max), (2, 2));
        assert_eq!(q.geo, GeoFilter::Us);
        assert_eq!(q.max_dph, Some(3.50));
        assert_eq!(q.limit, 12);
    }

    #[test]
    fn zero_floors_become_no_constraint_and_counts_are_sane() {
        let mut p = profile();
        p.min_reliability = 0.0;
        p.min_inet_down = 0;
        p.min_disk_gb = 0;
        p.num_gpus_min = 4;
        p.num_gpus_max = 1;
        let q = profile_to_query(&p, &QueryOverrides::none());
        assert_eq!(q.min_reliability, None);
        assert_eq!(q.min_inet_down, None);
        assert_eq!(q.min_disk_gb, None);
        assert_eq!((q.num_gpus_min, q.num_gpus_max), (1, 4), "min/max swapped");
    }

    #[test]
    fn extra_passthrough_merges_override_over_profile() {
        let mut p = profile();
        p.extra.insert("static_ip".into(), json!({"eq": true}));
        p.extra.insert("verified".into(), json!({"eq": false}));
        let mut o = QueryOverrides::none();
        o.extra.insert("verified".into(), json!({"eq": true}));
        let q = profile_to_query(&p, &o);
        assert_eq!(q.extra.get("static_ip"), Some(&json!({"eq": true})));
        assert_eq!(q.extra.get("verified"), Some(&json!({"eq": true})));
    }

    // ---- one search path --------------------------------------------------------------

    #[tokio::test]
    async fn auto_cheapest_and_the_browser_see_the_same_rows() {
        // The ancestor's bug: the browser searched one threshold set, `vast_up.sh` another.
        // Here both surfaces call search_unified, so "auto" is offers[0] of the same Vec.
        let cheap = offer(json!({
            "id": 1, "gpu_name": "RTX 3090", "num_gpus": 2, "gpu_ram": 24576,
            "gpu_total_ram": 49152, "dph_total": 0.28, "reliability2": 0.985,
            "inet_down": 900.0, "disk_space": 200.0, "cuda_max_good": 12.8,
            "geolocation": "Sweden, SE"
        }));
        let api = FakeApi::with(vec![vec![cz_3090(), cheap.clone()]]);

        let browser = search_unified(&api, &profile(), &QueryOverrides::none())
            .await
            .expect("search");
        let auto = search_unified(&api, &profile(), &QueryOverrides::none())
            .await
            .expect("search");

        assert_eq!(browser.offers, auto.offers);
        assert_eq!(cheapest(&auto).map(|o| o.id), Some(cheap.id));
        assert_eq!(
            browser.offers.first().map(|o| o.id),
            cheapest(&auto).map(|o| o.id),
            "auto rents the top row of the table the operator saw"
        );
        let qs = api.queries();
        assert_eq!(qs[0], qs[1], "one query builder, one candidate set");
        assert_eq!(qs[0], profile_to_query(&profile(), &QueryOverrides::none()));
    }

    #[tokio::test]
    async fn offers_are_sorted_cheapest_first_and_truncated_to_limit() {
        let mk = |id: u64, dph: f64| {
            offer(json!({
                "id": id, "gpu_name": "RTX 3090", "num_gpus": 2, "gpu_ram": 24576,
                "gpu_total_ram": 49152, "dph_total": dph, "reliability2": 0.99,
                "inet_down": 800.0, "disk_space": 300.0, "cuda_max_good": 12.8
            }))
        };
        let api = FakeApi::with(vec![vec![mk(1, 0.55), mk(2, 0.31), mk(3, 0.42)]]);
        let r = search_unified(&api, &profile(), &QueryOverrides::none().with_limit(2))
            .await
            .expect("search");
        assert_eq!(
            r.offers.iter().map(|o| o.id).collect::<Vec<_>>(),
            vec![2, 3]
        );
    }

    // ---- geo ---------------------------------------------------------------------------

    #[test]
    fn geo_matches_the_tail_of_geolocation() {
        let mut q = profile_to_query(&profile(), &QueryOverrides::none().with_geo(GeoFilter::Eu));
        assert!(
            offer_matches(&q, &cz_3090()),
            "\"Czechia, CZ\" is in the EU"
        );

        // the whole string is never compared: a bare code still matches
        let bare = offer(json!({
            "id": 2, "gpu_name": "RTX 3090", "num_gpus": 2, "gpu_ram": 24576,
            "gpu_total_ram": 49152, "dph_total": 0.31, "geolocation": "CZ"
        }));
        assert!(offer_matches(&q, &bare));

        // ...and a country *name* with no code tail does not
        let named = offer(json!({
            "id": 3, "gpu_name": "RTX 3090", "num_gpus": 2, "gpu_ram": 24576,
            "gpu_total_ram": 49152, "dph_total": 0.31, "geolocation": "Czechia"
        }));
        assert!(!offer_matches(&q, &named));

        // an unknown location cannot satisfy "must be in the EU"
        let nowhere = offer(json!({
            "id": 4, "gpu_name": "RTX 3090", "num_gpus": 2, "gpu_ram": 24576,
            "gpu_total_ram": 49152, "dph_total": 0.31
        }));
        assert!(!offer_matches(&q, &nowhere));
        q.geo = GeoFilter::Any;
        assert!(offer_matches(&q, &nowhere), "Any accepts an unknown geo");

        let nordic = profile_to_query(
            &profile(),
            &QueryOverrides::none().with_geo(GeoFilter::EuNordic),
        );
        assert!(!offer_matches(&nordic, &cz_3090()));
    }

    // ---- relaxation --------------------------------------------------------------------

    #[tokio::test]
    async fn a_relaxation_is_recorded_verbatim() {
        let mut p = profile();
        p.geo = GeoFilter::EuNordic;
        p.min_reliability = 0.99;
        p.min_inet_down = 300;
        p.max_dph = Some(Money::from_usd(0.90));

        // strict stage: only a Czech box, which the Nordic filter rejects
        // widened stage: the same box, now acceptable
        let api = FakeApi::with(vec![vec![cz_3090()], vec![cz_3090()]]);
        let r = search_unified(&api, &p, &QueryOverrides::none())
            .await
            .expect("search");

        assert_eq!(api.calls(), 2, "strict, then widened");
        assert_eq!(
            r.relaxations,
            vec!["widened: geo dropped, reliability 0.99 -> 0.97".to_string()],
            "the exact sentence every surface renders as a banner"
        );
        assert_eq!(r.offers.len(), 1);

        let qs = api.queries();
        assert_eq!(qs[1].geo, GeoFilter::Any);
        assert_eq!(qs[1].min_reliability, Some(0.97));
        assert_eq!(qs[1].max_dph, qs[0].max_dph, "the price cap never widens");
        assert_eq!(qs[1].gpu_names, qs[0].gpu_names);
        assert_eq!(qs[1].min_disk_gb, qs[0].min_disk_gb);
        assert_eq!(qs[1].min_cuda, qs[0].min_cuda);
    }

    #[test]
    fn widening_names_the_bandwidth_floor_too() {
        let mut p = profile();
        p.geo = GeoFilter::Us;
        p.min_reliability = 0.99;
        p.min_inet_down = 500;
        let q = profile_to_query(&p, &QueryOverrides::none());
        let (w, note) = widen(&q).expect("something to relax");
        assert_eq!(
            note,
            "widened: geo dropped, reliability 0.99 -> 0.97, inet_down 500 -> 300 Mbps"
        );
        assert_eq!(w.min_inet_down, Some(300.0));
    }

    #[tokio::test]
    async fn nothing_to_relax_means_one_call_and_no_banner() {
        let mut p = profile();
        p.geo = GeoFilter::Any;
        p.min_reliability = 0.90;
        p.min_inet_down = 100;
        let api = FakeApi::with(vec![vec![]]);
        let r = search_unified(&api, &p, &QueryOverrides::none())
            .await
            .expect("search");
        assert_eq!(api.calls(), 1);
        assert!(r.relaxations.is_empty());
        assert!(r.offers.is_empty(), "an empty market is Ok, not an error");
    }

    #[tokio::test]
    async fn a_hit_on_the_strict_query_never_widens() {
        let mut p = profile();
        p.geo = GeoFilter::Eu;
        let api = FakeApi::with(vec![vec![cz_3090()]]);
        let r = search_unified(&api, &p, &QueryOverrides::none())
            .await
            .expect("search");
        assert_eq!(api.calls(), 1);
        assert!(r.relaxations.is_empty());
    }

    // ---- money -------------------------------------------------------------------------

    #[tokio::test]
    async fn the_price_ceiling_is_enforced_locally() {
        // The server returning an over-cap row (or ignoring the range operator entirely)
        // must not put a $3.34/hr box in front of "auto — cheapest".
        let mut p = profile();
        p.gpu_names = vec!["RTX 3090".into(), "H100 SXM".into()];
        p.max_dph = Some(Money::from_usd(0.90));
        let api = FakeApi::with(vec![vec![us_h100(), cz_3090()]]);
        let r = search_unified(&api, &p, &QueryOverrides::none())
            .await
            .expect("search");
        assert_eq!(
            r.offers.iter().map(|o| o.id).collect::<Vec<_>>(),
            vec![cz_3090().id]
        );
        assert_eq!(cheapest(&r).map(|o| o.dph_total), Some(0.305));
    }

    #[test]
    fn an_unpriceable_offer_never_passes_a_cap() {
        let q = profile_to_query(&profile(), &QueryOverrides::none());
        let mut o = cz_3090();
        o.dph_total = f64::NAN;
        assert!(!offer_matches(&q, &o));
    }

    #[test]
    fn rented_and_unrentable_rows_are_dropped() {
        let q = profile_to_query(&profile(), &QueryOverrides::none());
        let mut rented = cz_3090();
        rented.rented = Some(true);
        assert!(!offer_matches(&q, &rented));
        let mut closed = cz_3090();
        closed.rentable = Some(false);
        assert!(!offer_matches(&q, &closed));
    }

    #[test]
    fn an_unknown_optional_field_does_not_exclude() {
        let q = profile_to_query(&profile(), &QueryOverrides::none());
        let sparse = offer(json!({
            "id": 9, "gpu_name": "RTX 3090", "num_gpus": 2, "gpu_ram": 24576,
            "gpu_total_ram": 49152, "dph_total": 0.30
        }));
        assert!(offer_matches(&q, &sparse));
    }

    #[test]
    fn gpu_count_range_is_respected() {
        let q = profile_to_query(&profile(), &QueryOverrides::none());
        let mut one = cz_3090();
        one.num_gpus = 1;
        assert!(!offer_matches(&q, &one));
        let mut eight = cz_3090();
        eight.num_gpus = 8;
        assert!(!offer_matches(&q, &eight));
    }

    // ---- vocabulary ---------------------------------------------------------------------

    #[tokio::test]
    async fn vocabulary_is_live_never_a_constant() {
        // Cards that exist in no table in this repo: if these come back, the vocabulary is
        // being read off the market rather than off a compiled-in list.
        let future = |id: u64, name: &str| {
            offer(json!({
                "id": id, "gpu_name": name, "num_gpus": 1, "gpu_ram": 32768,
                "gpu_total_ram": 32768, "dph_total": 1.0
            }))
        };
        let api = FakeApi::with(vec![vec![
            future(1, "RTX 6090"),
            future(2, "B300 SXM"),
            future(3, "RTX 6090"),
        ]]);
        let v = gpu_name_vocabulary(&api).await.expect("vocabulary");
        assert_eq!(v, vec!["B300 SXM".to_string(), "RTX 6090".to_string()]);

        let q = &api.queries()[0];
        assert!(q.gpu_names.is_empty(), "a broad sweep constrains nothing");
        assert_eq!(q.max_dph, None);
        assert_eq!(q.geo, GeoFilter::Any);
        assert_eq!(q.limit, VOCABULARY_SAMPLE_LIMIT);
    }

    #[tokio::test]
    async fn search_reports_the_vocabulary_it_saw_including_filtered_rows() {
        let api = FakeApi::with(vec![vec![us_h100(), cz_3090()]]);
        let r = search_unified(&api, &profile(), &QueryOverrides::none())
            .await
            .expect("search");
        assert_eq!(r.offers.len(), 1, "the H100 is off-profile");
        assert_eq!(
            r.gpu_name_vocabulary,
            vec!["H100 SXM".to_string(), "RTX 3090".to_string()],
            "the dropdown still learns about a card the filter rejected"
        );
        assert!(r.queried_at_unix > 1_700_000_000);
    }
}
