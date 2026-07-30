//! OWNER: unit S-04 (server/src/api/{rig,fit,catalog,usage,requests,jobs}.rs,
//! server/src/jobs.rs). Do not edit outside that unit.
//!
//! `GET /v1/requests`, `GET /v1/requests/{id}`, `POST /v1/requests/{id}/cancel`.
//!
//! # Where the records come from
//!
//! `RouterInner` keeps its own bounded ring of finished [`RequestRecord`]s (R-08), but the
//! field is private and `BUILD-PLAN.md` §4 publishes no accessor for it — reported in this
//! unit's `signature_problems`. Rather than reach into another unit's file, this module keeps
//! its **own** ring, fed from the `RequestFinished` broadcast every surface already
//! subscribes to. The record is the same value; only the reader differs.
//!
//! That has one visible consequence and it is documented rather than hidden. R-08 only
//! serialises `RequestFinished` while `tx.receiver_count() > 0` — the rule that stops a
//! router at 50 rps from drowning its own dashboard. So a [`RequestLog`] must be **attached**
//! before it can see anything, and everything before the attach is invisible to it.
//! [`attach`] is idempotent and per-channel; a daemon should call it once at startup
//! (`crate::api::requests::attach(&state.tx)`), and the handlers call it lazily so a
//! control plane that never wires it up still answers with everything since the first query
//! instead of an empty list forever.
//!
//! `usage.jsonl` remains the durable record. This ring is a display buffer: bounded, in
//! memory, and gone on restart, exactly like the one it mirrors.

use crate::api::{ApiError, ApiResult};
use crate::state::AppState;
use apexrouter_protocol::{Alias, BackendId, Event, RequestId, RequestRecord};
use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::Json;
use serde::Deserialize;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::broadcast;

/// How many finished requests the ring keeps. Matches R-08's `RING_CAPACITY`, so the two
/// views of the same traffic go back equally far.
const RING_CAPACITY: usize = 1_000;

/// The default `?limit=`.
const DEFAULT_LIMIT: usize = 100;

/// `GET /v1/requests?limit=&alias=&backend=&since=`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct RequestsQuery {
    /// How many to return, newest first. Defaults to 100, capped at the ring.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Only requests that resolved this alias.
    #[serde(default)]
    pub alias: Option<String>,
    /// Only requests served by this backend.
    #[serde(default)]
    pub backend: Option<String>,
    /// Only requests that started at or after this window — the same spellings
    /// [`crate::api::usage::parse_since`] accepts.
    #[serde(default)]
    pub since: Option<String>,
}

/// A bounded ring of finished requests, fed from the event broadcast.
#[derive(Debug)]
pub struct RequestLog {
    /// Newest at the back.
    ring: Mutex<VecDeque<RequestRecord>>,
    /// How many records to keep.
    capacity: usize,
}

impl RequestLog {
    /// An empty log keeping `capacity` records (minimum 1).
    pub fn new(capacity: usize) -> Arc<RequestLog> {
        Arc::new(RequestLog {
            ring: Mutex::new(VecDeque::new()),
            capacity: capacity.max(1),
        })
    }

    /// Push one record, evicting the oldest when full.
    pub fn record(&self, r: RequestRecord) {
        let mut ring = self.lock();
        while ring.len() >= self.capacity {
            ring.pop_front();
        }
        ring.push_back(r);
    }

    /// How many records are held.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether the log has seen anything.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// One record by id.
    pub fn get(&self, id: RequestId) -> Option<RequestRecord> {
        self.lock().iter().rev().find(|r| r.id == id).cloned()
    }

    /// The most recent records, newest first, with every filter ANDed.
    ///
    /// A record with no alias never matches an alias filter, and a record with no backend
    /// never matches a backend filter — an absent value is not a wildcard.
    pub fn recent(
        &self,
        limit: usize,
        alias: Option<&Alias>,
        backend: Option<&BackendId>,
        since: Option<i64>,
    ) -> Vec<RequestRecord> {
        self.lock()
            .iter()
            .rev()
            .filter(|r| alias.is_none() || r.alias.as_ref() == alias)
            .filter(|r| backend.is_none() || r.backend.as_ref() == backend)
            .filter(|r| match since {
                Some(s) => r.started_unix >= s,
                None => true,
            })
            .take(limit)
            .cloned()
            .collect()
    }

    /// The ring, surviving a poisoned lock: this is a display buffer, and turning another
    /// task's panic into a second panic would cost the daemon.
    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<RequestRecord>> {
        self.ring.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Every attached log, keyed by the broadcast channel it listens to.
///
/// Keyed rather than a single global because one process can hold several `AppState`s — that
/// is what the test suite is — and two tests' traffic must not appear in each other's answers.
#[allow(clippy::type_complexity)]
fn logs() -> &'static Mutex<Vec<(broadcast::Sender<Event>, Arc<RequestLog>)>> {
    static L: OnceLock<Mutex<Vec<(broadcast::Sender<Event>, Arc<RequestLog>)>>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(Vec::new()))
}

/// Subscribe a [`RequestLog`] to `tx`, or return the one already listening to it.
///
/// Idempotent per channel. Holding the subscription alive is deliberate: it is what makes
/// R-08 emit `RequestFinished` at all, so calling this once at startup is what makes
/// `GET /v1/requests` answer with the whole session rather than "everything since you first
/// asked".
pub fn attach(tx: &broadcast::Sender<Event>) -> Arc<RequestLog> {
    let mut all = logs().lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, log)) = all.iter().find(|(s, _)| s.same_channel(tx)) {
        return Arc::clone(log);
    }
    let log = RequestLog::new(RING_CAPACITY);
    all.push((tx.clone(), Arc::clone(&log)));
    drop(all);

    let mut rx = tx.subscribe();
    let sink = Arc::clone(&log);
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(Event::RequestFinished { record }) => sink.record(*record),
                Ok(_) => {}
                // Lagged means the ring already missed records; the next `recv` resumes.
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::debug!(missed = n, "the request log lagged the broadcast");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    log
}

/// The `/v1/requests*` routes.
pub fn router() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route("/v1/requests", get(list_requests))
        .route("/v1/requests/{id}", get(get_request))
        .route("/v1/requests/{id}/cancel", post(cancel_request))
}

/// `GET /v1/requests` — recent finished requests, newest first.
pub async fn list_requests(
    State(s): State<Arc<AppState>>,
    Query(q): Query<RequestsQuery>,
) -> ApiResult<Vec<RequestRecord>> {
    let log = attach(&s.tx);
    let alias = q
        .alias
        .as_deref()
        .map(|a| {
            Alias::parse(a)
                .map_err(|e| ApiError::bad_request("bad_id", e.to_string()).with_param("alias"))
        })
        .transpose()?;
    let backend = q
        .backend
        .as_deref()
        .map(|b| {
            BackendId::parse(b)
                .map_err(|e| ApiError::bad_request("bad_id", e.to_string()).with_param("backend"))
        })
        .transpose()?;
    let since = crate::api::usage::parse_since(q.since.as_deref())?;
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, RING_CAPACITY);
    Ok(Json(log.recent(
        limit,
        alias.as_ref(),
        backend.as_ref(),
        since,
    )))
}

/// `GET /v1/requests/{id}` — one record.
pub async fn get_request(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<RequestRecord> {
    let log = attach(&s.tx);
    let parsed = parse_request_id(&id)?;
    log.get(parsed)
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("no record of request {id}")).with_param("id"))
}

/// `POST /v1/requests/{id}/cancel` — **not in mk1**.
///
/// Cancelling an in-flight relay needs a handle on the running task, and the request path
/// (R-08) keeps none: its per-request state is a stack frame and an `InFlightGuard`, neither
/// of which is addressable from another task. Answering `202` and doing nothing would be
/// worse than refusing — an operator watching a runaway 4000-token generation needs to know
/// it is still running, so they reach for `endpoint stop` instead.
pub async fn cancel_request(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<RequestRecord> {
    let parsed = parse_request_id(&id)?;
    let log = attach(&s.tx);
    if let Some(done) = log.get(parsed) {
        return Err(ApiError::conflict(format!(
            "request {id} already finished with status {} after {} ms",
            done.status, done.total_ms
        ))
        .with_param("id"));
    }
    Err(ApiError::not_implemented(
        "cancelling an in-flight request is not implemented in mk1: the relay owns no \
         cancellation handle. Stop the backend (`apexrouter endpoint stop <id>`) or \
         disconnect the client, which the relay does notice.",
    )
    .with_param("id"))
}

/// Parse a `RequestId` out of a path segment.
fn parse_request_id(raw: &str) -> Result<RequestId, ApiError> {
    raw.parse::<ulid::Ulid>().map(RequestId).map_err(|_| {
        ApiError::bad_request(
            "bad_id",
            format!("`{raw}` is not a request id; ids are ULIDs, as returned in X-Request-Id"),
        )
        .with_param("id")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::testkit::{app, test_config};
    use apexrouter_protocol::{CostEstimate, Protocol, RouteReason};
    use std::time::Duration;

    fn rec(alias: Option<&str>, backend: Option<&str>, started_unix: i64) -> RequestRecord {
        RequestRecord {
            id: RequestId::new(),
            started_unix,
            alias: alias.map(|a| Alias::parse(a).expect("alias")),
            backend: backend.map(|b| BackendId::parse(b).expect("backend")),
            upstream_model: Some("m".to_owned()),
            route_reason: RouteReason::Alias,
            ingress: Protocol::OpenAi,
            method: "POST".to_owned(),
            path: "/v1/chat/completions".to_owned(),
            status: 200,
            attempts: 1,
            streamed: false,
            aborted: false,
            ttft_ms: Some(40),
            total_ms: 120,
            prompt_tokens: None,
            completion_tokens: None,
            cached_tokens: None,
            tok_per_s: None,
            cost: CostEstimate::Unknown,
            error: None,
        }
    }

    #[test]
    fn the_ring_is_bounded_and_newest_first() {
        let log = RequestLog::new(3);
        let mut ids = Vec::new();
        for _ in 0..5 {
            let r = rec(Some("auto"), Some("local"), 0);
            ids.push(r.id);
            log.record(r);
        }
        assert_eq!(log.len(), 3, "the oldest two were evicted");
        let got: Vec<RequestId> = log
            .recent(10, None, None, None)
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(got, vec![ids[4], ids[3], ids[2]]);
    }

    #[test]
    fn filters_are_anded_and_an_absent_field_is_not_a_wildcard() {
        let log = RequestLog::new(16);
        log.record(rec(Some("auto"), Some("local"), 100));
        log.record(rec(Some("big"), Some("remote"), 200));
        log.record(rec(None, None, 300));

        let auto = Alias::parse("auto").expect("alias");
        let local = BackendId::parse("local").expect("id");
        assert_eq!(log.recent(10, Some(&auto), None, None).len(), 1);
        assert_eq!(log.recent(10, None, Some(&local), None).len(), 1);
        assert_eq!(log.recent(10, Some(&auto), Some(&local), None).len(), 1);
        let remote = BackendId::parse("remote").expect("id");
        assert_eq!(
            log.recent(10, Some(&auto), Some(&remote), None).len(),
            0,
            "filters AND, they do not OR"
        );
        assert_eq!(
            log.recent(10, None, None, Some(200)).len(),
            2,
            "since is inclusive"
        );
    }

    #[test]
    fn a_record_is_findable_by_id() {
        let log = RequestLog::new(8);
        let r = rec(Some("auto"), Some("local"), 0);
        let id = r.id;
        log.record(r);
        assert!(log.get(id).is_some());
        assert!(log.get(RequestId::new()).is_none());
    }

    #[test]
    fn a_malformed_request_id_is_a_400_not_a_500() {
        let e = parse_request_id("not-a-ulid").expect_err("refused");
        assert_eq!(e.status, axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(e.body.param.as_deref(), Some("id"));
    }

    /// The load-bearing claim of this module: a record broadcast by the request path lands in
    /// the log without the router crate having to publish its private ring.
    #[tokio::test]
    async fn an_attached_log_captures_broadcast_records() {
        let state = app(test_config());
        let log = attach(&state.tx);
        assert!(
            Arc::ptr_eq(&log, &attach(&state.tx)),
            "attach is idempotent per channel"
        );

        let r = rec(Some("auto"), Some("local"), 0);
        let id = r.id;
        state
            .tx
            .send(Event::RequestFinished {
                record: Box::new(r),
            })
            .expect("a subscriber exists, so the send succeeds");

        for _ in 0..200 {
            if log.get(id).is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            log.get(id).is_some(),
            "the broadcast record reached the log"
        );
    }

    #[tokio::test]
    async fn cancelling_a_finished_request_says_so_and_cancelling_a_live_one_refuses() {
        let state = app(test_config());
        let log = attach(&state.tx);
        let r = rec(Some("auto"), Some("local"), 0);
        let id = r.id;
        log.record(r);

        let e = cancel_request(State(Arc::clone(&state)), Path(id.to_string()))
            .await
            .expect_err("a finished request cannot be cancelled");
        assert_eq!(e.status, axum::http::StatusCode::CONFLICT);

        let unknown = RequestId::new();
        let e = cancel_request(State(state), Path(unknown.to_string()))
            .await
            .expect_err("mk1 cannot cancel in flight");
        assert_eq!(e.status, axum::http::StatusCode::NOT_IMPLEMENTED);
    }
}
