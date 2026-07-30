//! OWNER: unit C-14 (core/usage.rs, core/pricing.rs). Do not edit outside that unit.
//!
//! The price table is **data fed by providers at runtime** — it never calls one, because
//! `core -> providers` would be a cycle.
//!
//! A `PerHour` price yields `CostEstimate::Approximate` with the throughput assumption **in
//! the string**, never a bare number. LocalRouter's `cost.py` buried a 100 tok/s constant;
//! this is the type that makes that impossible.

use apexrouter_protocol::{CostEstimate, InstanceId, PriceModel, ProviderId, TokenCount};

/// Everything we currently believe about prices, from every source.
#[derive(Debug, Default)]
pub struct PriceTable {/* C-14 */}

impl PriceTable {
    /// Replace one provider's model prices. Called by the provider layer after a catalogue
    /// fetch.
    pub fn set_provider_models(&mut self, id: &ProviderId, models: &[(String, PriceModel)]) {
        todo!("C-14: PriceTable::set_provider_models")
    }

    /// Record what one rented instance is costing per hour.
    pub fn set_instance_dph(&mut self, id: InstanceId, dph: f64) {
        todo!("C-14: PriceTable::set_instance_dph")
    }

    /// Estimate one request's cost, degrading to `Unknown` rather than inventing a number.
    pub fn estimate(
        &self,
        provider: &str,
        model: &str,
        prompt: TokenCount,
        completion: TokenCount,
        tps_hint: Option<f32>,
    ) -> CostEstimate {
        todo!("C-14: PriceTable::estimate")
    }
}
