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

use crate::table::RoutingTable;

/// Every alias plus every enabled backend's models, in the OpenAI list shape.
pub fn aggregate_models(t: &RoutingTable) -> serde_json::Value {
    todo!("R-06: aggregate_models")
}

/// One entry: an alias, or `"<backend_id>/<model>"`.
pub fn one_model(t: &RoutingTable, id: &str) -> Option<serde_json::Value> {
    todo!("R-06: one_model")
}
