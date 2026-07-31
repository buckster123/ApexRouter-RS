//! OWNER: unit I-01 (server/src/api/migrate.rs). Do not edit outside that unit.
//!
//! `POST /v1/migrate` — the CLI's `apexrouter migrate`, over the control plane.
//!
//! The body decides everything: `dry_run: true` computes the plan and writes **nothing**;
//! `dry_run: false` applies it. `skip` strikes rows out first, with
//! `core::migrate::strike`'s matching — the same one implementation the CLI uses, so a
//! pattern behaves identically on both surfaces. A pattern that matches no row is a `400`,
//! never a silently unhonoured intent.
//!
//! # The daemon is running while this writes — why that is sound here
//!
//! The CLI verb is `Need::Pure` because *autostarting* a daemon to then write underneath
//! it would be absurd. This route is the other arrangement: the daemon is already up, and
//! `apply` writes `config.toml` — which the S-05 watcher reloads on content hash — and
//! `catalog.toml`, which every `/v1/recipes*` handler reads from disk per request. Nothing
//! serves from a snapshot that survives the import, and the request path (invariant 2)
//! reads neither file.

use super::{ApiError, ApiResult};
use crate::state::AppState;
use apexrouter_core::migrate;
use apexrouter_protocol::{MigrationPlan, MigrationReport};
use axum::extract::State;
use axum::routing::post;
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;

/// The `/v1/migrate` route.
pub fn router() -> axum::Router<Arc<AppState>> {
    axum::Router::new().route("/v1/migrate", post(run))
}

/// `{dry_run, skip[]}`. `dry_run` is required on purpose: a body that does not say is a
/// caller that has not decided, and the difference is "writes nothing" versus "writes
/// three files".
#[derive(Clone, Debug, Deserialize)]
pub struct MigrateRequest {
    /// `true` returns the [`MigrationPlan`] and writes nothing at all.
    pub dry_run: bool,
    /// Strike patterns, applied to the plan before anything else happens.
    #[serde(default)]
    pub skip: Vec<String>,
}

/// A plan under `dry_run`, a report otherwise — the OpenAPI `oneOf`.
#[derive(Debug, serde::Serialize)]
#[serde(untagged)]
pub enum MigrateResponse {
    /// What would happen.
    Plan(MigrationPlan),
    /// What happened.
    Report(MigrationReport),
}

/// `POST /v1/migrate`.
pub async fn run(
    State(s): State<Arc<AppState>>,
    Json(req): Json<MigrateRequest>,
) -> ApiResult<MigrateResponse> {
    let paths = s.paths.clone();
    let cfg = s.cfg.load_full();
    // `plan` walks the legacy tree and `apply` is `toml_edit` under file writes — both
    // are filesystem work and belong off the runtime's workers.
    let out = tokio::task::spawn_blocking(move || -> apexrouter_core::error::Result<_> {
        let mut plan = migrate::plan(&paths, &cfg)?;
        migrate::strike(&mut plan, &req.skip)?;
        if req.dry_run {
            return Ok(MigrateResponse::Plan(plan));
        }
        let report = migrate::apply(&paths, &cfg, &plan)?;
        Ok(MigrateResponse::Report(report))
    })
    .await
    .map_err(|e| ApiError::internal(format!("the migrate task failed: {e}")))?
    .map_err(ApiError::from)?;
    Ok(Json(out))
}
