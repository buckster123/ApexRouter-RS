//! OWNER: unit S-07 (server/src/api/{vast,hf,providers,checks,compare}.rs). Do not edit outside that unit.
//!
//! The `/v1/vast/*` set. `POST /v1/vast/instances` returns **409** without `{confirm:true, max_usd_per_hour}`, and the 409 body carries the cost preview and the current credit.
