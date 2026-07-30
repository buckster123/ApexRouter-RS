//! OWNER: unit S-03 (server/src/api/{mod,snapshot,backends,routes,endpoints}.rs). Do not edit outside that unit.
//!
//! The `/v1/routes*` set, including `validate`, `test`, `swap` and `default`. `PUT /v1/routes` is atomic, and a compile failure leaves the previous table serving.
