//! OWNER: unit S-04 (server/src/api/{rig,fit,catalog,usage,requests,jobs}.rs,
//! server/src/jobs.rs). Do not edit outside that unit.
//!
//! `GET /v1/jobs`, `GET /v1/jobs/{id}`, `POST /v1/jobs/{id}/cancel`.
//!
//! The thin REST face of [`crate::jobs::JobRegistry`]. Every `?no_wait=true` route in the
//! control plane — `POST /v1/endpoints`, `POST /v1/hf/downloads`, `POST /v1/vast/instances`,
//! `POST /v1/compare`, `POST /v1/recipes/{id}/instantiate` — returns a `JobRecord` whose `id`
//! is polled here, so this is the one place an agent has to know about to follow anything
//! long-running.

use crate::api::{ApiError, ApiResult};
use crate::state::AppState;
use apexrouter_protocol::{JobId, JobRecord};
use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::Json;
use std::sync::Arc;

/// The `/v1/jobs*` routes.
pub fn router() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route("/v1/jobs", get(list_jobs))
        .route("/v1/jobs/{id}", get(get_job))
        .route("/v1/jobs/{id}/cancel", post(cancel_job))
}

/// `GET /v1/jobs` — every job, newest first, live and recently finished.
pub async fn list_jobs(State(s): State<Arc<AppState>>) -> ApiResult<Vec<JobRecord>> {
    s.jobs.ensure_wired(&s.tx, &s.paths);
    Ok(Json(s.jobs.all()))
}

/// `GET /v1/jobs/{id}` — one job's record. This is the poll target.
pub async fn get_job(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<JobRecord> {
    s.jobs.ensure_wired(&s.tx, &s.paths);
    let job = parse_job_id(&id)?;
    s.jobs
        .get(job)
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("no job {id}")).with_param("id"))
}

/// `POST /v1/jobs/{id}/cancel` — mark it `Cancelled` and abort the task.
///
/// A job that has already finished is a `409` rather than a silent success: "cancelled" and
/// "finished a second before you asked" are different outcomes and the caller is entitled to
/// know which one happened.
pub async fn cancel_job(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<JobRecord> {
    s.jobs.ensure_wired(&s.tx, &s.paths);
    let job = parse_job_id(&id)?;
    let existing = s
        .jobs
        .get(job)
        .ok_or_else(|| ApiError::not_found(format!("no job {id}")).with_param("id"))?;
    s.jobs.cancel(job).map(Json).ok_or_else(|| {
        ApiError::conflict(format!(
            "job {id} had already finished as {:?}",
            existing.state
        ))
        .with_param("id")
    })
}

/// Parse a `JobId` out of a path segment.
fn parse_job_id(raw: &str) -> Result<JobId, ApiError> {
    raw.parse::<ulid::Ulid>().map(JobId).map_err(|_| {
        ApiError::bad_request(
            "bad_id",
            format!("`{raw}` is not a job id; ids are ULIDs, as returned by every ?no_wait route"),
        )
        .with_param("id")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::testkit::{app, test_config};
    use apexrouter_protocol::JobState;
    use std::time::Duration;

    #[tokio::test]
    async fn jobs_are_listed_fetched_and_cancelled_through_the_api() {
        let state = app(test_config());
        let rec = state.jobs.spawn::<_, ()>("test.forever", async {
            loop {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });

        let Json(all) = list_jobs(State(Arc::clone(&state))).await.expect("list");
        assert_eq!(all.len(), 1);

        let Json(one) = get_job(State(Arc::clone(&state)), Path(rec.id.to_string()))
            .await
            .expect("get");
        assert_eq!(one.id, rec.id);

        let Json(cancelled) = cancel_job(State(Arc::clone(&state)), Path(rec.id.to_string()))
            .await
            .expect("cancel");
        assert_eq!(cancelled.state, JobState::Cancelled);

        let again = cancel_job(State(state), Path(rec.id.to_string()))
            .await
            .expect_err("a finished job cannot be cancelled twice");
        assert_eq!(again.status, axum::http::StatusCode::CONFLICT);
    }

    /// A daemon killed with `SIGKILL` cannot run its own error paths. The next process
    /// runs them for it, so a row left `Running` on disk is `Failed` — never `Pending`
    /// forever — the first time the control plane touches the registry.
    #[tokio::test]
    async fn a_row_a_dead_daemon_left_open_is_failed_on_the_next_start() {
        let state = app(test_config());
        state.jobs.attach_store(&state.store);
        let rec = state.jobs.create("endpoint.start");
        let on_disk = state.paths.jobs_dir().join(format!("{}.json", rec.id));
        assert!(
            on_disk.exists(),
            "a job row is persisted when it is created"
        );

        // A second daemon over the same state directory: nothing in memory, one open row on
        // disk.
        let next = crate::jobs::JobRegistry::new();
        let closed = next.attach_store(&state.store);
        assert_eq!(closed, 1, "exactly one stale row was closed");
        let row = next.get(rec.id).expect("the row was re-read");
        assert_eq!(row.state, JobState::Failed);
        assert!(row.finished_unix.is_some());
        assert!(
            row.error.as_deref().unwrap_or_default().contains("daemon"),
            "the reason names what happened: {:?}",
            row.error
        );

        // and the close survived to disk, so a third start does not re-close it
        let third = crate::jobs::JobRegistry::new();
        assert_eq!(third.attach_store(&state.store), 0);
    }

    #[tokio::test]
    async fn an_unknown_job_is_a_404_and_a_malformed_id_is_a_400() {
        let state = app(test_config());
        let missing = get_job(State(Arc::clone(&state)), Path(JobId::new().to_string()))
            .await
            .expect_err("no such job");
        assert_eq!(missing.status, axum::http::StatusCode::NOT_FOUND);

        let bad = get_job(State(state), Path("nope".to_owned()))
            .await
            .expect_err("not a ulid");
        assert_eq!(bad.status, axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(bad.body.param.as_deref(), Some("id"));
    }
}
