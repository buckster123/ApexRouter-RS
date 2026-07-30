//! OWNER: unit S-03 (server/src/api/{mod,snapshot,backends,routes,endpoints}.rs). Do not
//! edit outside that unit — `{rig,fit,catalog,usage,requests,jobs}.rs` are S-04 and
//! `{vast,hf,providers,checks,compare}.rs` are S-07.
//!
//! The control-plane REST surface, all under `/v1/` (the proxy's `/v1` lives on a different
//! socket, so there is no collision). **Every response body is a protocol type** and every
//! mutation is `Origin`/`Host`-gated.
//!
//! `?no_wait=true` is the house pattern: return a `JobRecord` immediately and have the
//! spawned task flip the row to `Failed` on **every** error path, including a `JoinError`
//! from a panic, so nothing sits `Pending` forever.

pub mod backends;
pub mod catalog;
pub mod checks;
pub mod compare;
pub mod endpoints;
pub mod fit;
pub mod hf;
pub mod jobs;
pub mod providers;
pub mod requests;
pub mod rig;
pub mod routes;
pub mod snapshot;
pub mod usage;
pub mod vast;
