//! OWNER: unit P-03 (providers/src/vast/offers.rs). Do not edit outside that unit.
//!
//! Turning a saved [`SearchProfile`] into a live market query.
//!
//! Any relaxation appends a string to `OfferSearchResult::relaxations` — e.g.
//! `"widened: geo dropped, reliability 0.99 -> 0.97"` — that **every surface renders as an
//! explicit banner**. Geo is a client-side match on the **tail** of `geolocation`, and
//! `gpu_name_vocabulary` comes from a live broad search, never a constant: `00c` proves
//! those strings change.

use super::api::VastApi;
use apexrouter_core::error::Result;
use apexrouter_protocol::{OfferQuery, OfferSearchResult, SearchProfile};

/// Per-call overrides layered on top of a saved profile (`--gpu`, `--num-gpus`, `--geo`,
/// `--max-price`).
#[derive(Clone, Debug, Default)]
pub struct QueryOverrides {/* P-03 */}

/// Profile + overrides → the one query shape.
pub fn profile_to_query(p: &SearchProfile, overrides: &QueryOverrides) -> OfferQuery {
    todo!("P-03: profile_to_query")
}

/// **One** search path, used by both `--auto` and the browser table.
pub async fn search_unified(
    api: &dyn VastApi,
    p: &SearchProfile,
    o: &QueryOverrides,
) -> Result<OfferSearchResult> {
    todo!("P-03: search_unified")
}

/// The live `gpu_name` vocabulary for the dropdown, from a broad search.
pub async fn gpu_name_vocabulary(api: &dyn VastApi) -> Result<Vec<String>> {
    todo!("P-03: gpu_name_vocabulary")
}
