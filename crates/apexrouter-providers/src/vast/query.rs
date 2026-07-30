//! OWNER: unit P-02 (providers/src/vast/{api,query,mod}.rs). Do not edit outside that unit.
//!
//! Query construction. There is **one** builder, so "auto — cheapest" and the browser table
//! cannot ever run different searches — which is precisely the documented bug where
//! ApexRouter's ancestor rented from a stricter candidate set than the user had seen.

use apexrouter_protocol::OfferQuery;

/// Build the `{"q": …}` body for `PUT /api/v0/search/asks/`, exactly as verified in
/// `docs/port/00c`.
///
/// A fixture test asserts the produced body shape, because getting this wrong is how you
/// rent a box the operator never saw.
pub fn build_query(q: &OfferQuery) -> serde_json::Value {
    todo!("P-02: build_query")
}
