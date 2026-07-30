//! OWNER: unit R-09 (router/src/compat.rs). Do not edit outside that unit.
//!
//! The three byte-compatible legacy routes on the proxy listener: `/health`, `/providers`
//! and `POST /switch`.
//!
//! Three documented silent no-ops are fixed here: an `api_key` in a `together` body is now
//! persisted as a `CredentialRef`; `local` now copies the instance's key; and a malformed
//! instance JSON returns a JSON `400`, not an HTML 500. Provider probes run **concurrently**
//! with a 3 s cap, where the Python ran them serially for ~8 s, and Together is detected
//! from the **full credential chain**, not just `$TOGETHER_API_KEY`.
//!
//! `POST /switch` is a mutation and gets the mutation gate: unauthenticated `/switch` with
//! an arbitrary `base_url` plus an injected key is a **credential-exfiltration primitive**,
//! not merely SSRF, so any supplied URL is validated against `[compat] allow_switch_hosts`.

use crate::registry::BackendRegistry;
use crate::Router;
use apexrouter_core::config::CompatCfg;
use apexrouter_core::error::Result;
use apexrouter_protocol::ModelRoute;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::Json;
use bytes::Bytes;
use serde_json::Value;

/// A superset of the LocalRouter shape and the house shape:
/// `{"ok":true,"product":"apexrouter","version":…,"provider":…,"uptime":…}`. Always 200;
/// never probes a backend.
pub async fn legacy_health(State(r): State<Router>) -> Json<Value> {
    todo!("R-09: legacy_health")
}

/// The **exact** legacy JSON shape (`active`, `target`, `providers{}`, `local_instances[]`)
/// plus additive `endpoints[]` and `routes[]`.
pub async fn legacy_providers(State(r): State<Router>) -> Json<Value> {
    todo!("R-09: legacy_providers")
}

/// The legacy switch verb, retargeting `default_alias`. Extended with
/// `{"provider":"endpoint","id":…}` and `{"alias":…}`.
pub async fn legacy_switch(State(r): State<Router>, headers: HeaderMap, body: Bytes) -> Response {
    todo!("R-09: legacy_switch")
}

/// Mirror the default alias into `.active_endpoint` in the legacy shape, atomically.
/// Off by default (`[compat] active_endpoint_path = ""`).
pub fn mirror_active_endpoint(
    cfg: &CompatCfg,
    route: &ModelRoute,
    reg: &BackendRegistry,
) -> Result<()> {
    todo!("R-09: mirror_active_endpoint")
}
