//! OWNER: unit S-04 (server/src/api/{rig,fit,catalog,usage,requests,jobs}.rs,
//! server/src/jobs.rs). Do not edit outside that unit.
//!
//! `GET /v1/usage?since=&by=`.
//!
//! The rows come from `apexrouter_core::usage::read_all`, which merges `$STATE/usage.jsonl`
//! with the legacy `~/.vastai-gguf/usage.log` and de-duplicates what the mirror already
//! wrote. Aggregation is `core::usage::aggregate`, which folds cost with
//! `CostEstimate::add` so one estimated row visibly demotes the total instead of the total
//! quietly claiming to be metered.
//!
//! Reading is **blocking** — two files, line by line, possibly megabytes — so it runs in
//! `spawn_blocking`. A usage query is not on the request path but it is on the *dashboard's*
//! path, and a 40 MB `usage.log` parsed on a runtime worker stalls every in-flight relay
//! sharing that worker.

use crate::api::{ApiError, ApiResult};
use crate::state::AppState;
use apexrouter_core::usage::{aggregate, parse_lenient_timestamp, read_all, GroupBy};
use apexrouter_protocol::UsageSummary;
use axum::extract::{Query, State};
use axum::routing::get;
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;

/// `GET /v1/usage?since=24h&by=provider|model|backend|alias|day`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct UsageQuery {
    /// `all`, a duration (`30m`, `24h`, `7d`, `4w`), or an absolute timestamp.
    #[serde(default)]
    pub since: Option<String>,
    /// How to bucket. Defaults to `provider`, which is what the legacy `cost.py` printed.
    #[serde(default)]
    pub by: Option<String>,
}

/// The `/v1/usage` route.
pub fn router() -> axum::Router<Arc<AppState>> {
    axum::Router::new().route("/v1/usage", get(get_usage))
}

/// `GET /v1/usage` — aggregate the append-only rows over a window.
pub async fn get_usage(
    State(s): State<Arc<AppState>>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<UsageSummary> {
    let since = parse_since(q.since.as_deref())?;
    let by = parse_group_by(q.by.as_deref())?;
    Ok(Json(summarise(&s, since, by).await?))
}

/// Read and aggregate, off the runtime's worker threads.
///
/// # Errors
/// `500` when a usage file exists but cannot be read. An *absent* file is not an error: a
/// machine that has served nothing has an empty summary, not a failure.
pub async fn summarise(
    state: &Arc<AppState>,
    since: Option<i64>,
    by: GroupBy,
) -> Result<UsageSummary, ApiError> {
    let paths = state.paths.clone();
    let compat = state.cfg.load().compat.clone();
    let rows = tokio::task::spawn_blocking(move || read_all(&paths, &compat))
        .await
        .map_err(|e| ApiError::internal(format!("the usage read task failed: {e}")))??;
    Ok(aggregate(&rows, since, by))
}

/// Parse `since` into a unix-seconds cutoff.
///
/// Accepted, in order:
///
/// * absent, or `all` / `forever` / `0` — no cutoff, which is what makes `all` a real window
///   rather than "a very long duration";
/// * `<n><unit>` with unit `s`, `m`, `h`, `d` or `w` — relative to now;
/// * anything [`parse_lenient_timestamp`] accepts, which includes a bare unix timestamp and
///   both the RFC 3339 we write and the local-time-with-a-lying-`Z` `cost.py` wrote.
///
/// A duration is `now - n`, computed once, so two buckets in one response cannot disagree
/// about when the window started.
///
/// # Errors
/// `400`, naming the parameter, when the string is none of those. Silently treating an
/// unparseable window as `all` would answer a question nobody asked.
pub fn parse_since(spec: Option<&str>) -> Result<Option<i64>, ApiError> {
    let raw = spec.unwrap_or("").trim();
    if raw.is_empty()
        || raw == "0"
        || raw.eq_ignore_ascii_case("all")
        || raw.eq_ignore_ascii_case("forever")
    {
        return Ok(None);
    }
    if let Some(secs) = parse_duration_secs(raw) {
        if secs == 0 {
            return Ok(None);
        }
        return Ok(Some(chrono::Utc::now().timestamp() - secs));
    }
    if let Some(at) = parse_lenient_timestamp(raw) {
        return Ok(Some(at));
    }
    Err(ApiError::bad_request(
        "invalid",
        format!(
            "`{raw}` is not a window; use `all`, a duration like `24h`, `7d` or `30m`, \
             or an absolute timestamp"
        ),
    )
    .with_param("since"))
}

/// `24h` -> 86400. `None` when the string is not `<digits><unit>`.
fn parse_duration_secs(raw: &str) -> Option<i64> {
    let (digits, unit) = raw.split_at(raw.find(|c: char| !c.is_ascii_digit())?);
    let n: i64 = digits.parse().ok()?;
    let mult = match unit.trim().to_ascii_lowercase().as_str() {
        "s" | "sec" | "secs" => 1,
        "m" | "min" | "mins" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3_600,
        "d" | "day" | "days" => 86_400,
        "w" | "wk" | "week" | "weeks" => 604_800,
        _ => return None,
    };
    n.checked_mul(mult)
}

/// Parse the `by=` grouping.
///
/// # Errors
/// `400` naming the five accepted values, rather than silently grouping by something else.
pub fn parse_group_by(spec: Option<&str>) -> Result<GroupBy, ApiError> {
    match spec
        .unwrap_or("provider")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "" | "provider" => Ok(GroupBy::Provider),
        "model" => Ok(GroupBy::Model),
        "backend" => Ok(GroupBy::Backend),
        "alias" => Ok(GroupBy::Alias),
        "day" => Ok(GroupBy::Day),
        other => Err(ApiError::bad_request(
            "invalid",
            format!("`{other}` is not a grouping; use provider, model, backend, alias or day"),
        )
        .with_param("by")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::testkit::{app, test_config};
    use apexrouter_core::usage::UsageWriter;
    use apexrouter_protocol::UsageRecord;

    fn row(provider: &str, ago_secs: i64, prompt: u32, completion: u32) -> UsageRecord {
        UsageRecord {
            timestamp: chrono::Utc::now().to_rfc3339(),
            epoch: Some((chrono::Utc::now().timestamp() - ago_secs) as f64),
            provider: provider.to_owned(),
            model_id: "m".to_owned(),
            prompt_tokens: prompt,
            completion_tokens: completion,
            cost_usd: 0.0,
            request_id: None,
            backend: None,
            alias: None,
            ttft_ms: None,
            tok_per_s: None,
            stream: None,
            estimated: None,
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn windows_parse_the_way_the_cli_spells_them() {
        assert_eq!(parse_since(None).expect("none"), None);
        assert_eq!(parse_since(Some("all")).expect("all"), None);
        assert_eq!(parse_since(Some("  ")).expect("blank"), None);
        assert_eq!(parse_since(Some("0")).expect("zero"), None);

        let now = chrono::Utc::now().timestamp();
        let day = parse_since(Some("24h")).expect("24h").expect("cutoff");
        assert!((now - 86_400 - day).abs() <= 2, "24h is one day back");
        let week = parse_since(Some("7d")).expect("7d").expect("cutoff");
        assert!((now - 604_800 - week).abs() <= 2);
        let half = parse_since(Some("30m")).expect("30m").expect("cutoff");
        assert!((now - 1_800 - half).abs() <= 2);
    }

    #[test]
    fn an_absolute_timestamp_is_a_window_too() {
        let at = parse_since(Some("2024-01-01T00:00:00Z"))
            .expect("rfc3339")
            .expect("cutoff");
        assert!(at > 1_700_000_000, "parsed as a real instant: {at}");
    }

    /// A window nobody can parse must not silently become "everything".
    #[test]
    fn an_unparseable_window_is_refused_not_widened() {
        let e = parse_since(Some("last tuesday")).expect_err("refused");
        assert_eq!(e.status, axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(e.body.param.as_deref(), Some("since"));
    }

    #[test]
    fn groupings_parse_and_anything_else_is_refused() {
        assert_eq!(parse_group_by(None).expect("default"), GroupBy::Provider);
        assert_eq!(parse_group_by(Some("day")).expect("day"), GroupBy::Day);
        assert_eq!(
            parse_group_by(Some("ALIAS")).expect("alias"),
            GroupBy::Alias
        );
        let e = parse_group_by(Some("colour")).expect_err("refused");
        assert_eq!(e.body.param.as_deref(), Some("by"));
    }

    #[tokio::test]
    async fn an_empty_machine_summarises_to_zero_rather_than_failing() {
        let state = app(test_config());
        let out = summarise(&state, None, GroupBy::Provider)
            .await
            .expect("summary");
        assert_eq!(out.rows, 0);
        assert!(out.by.is_empty());
    }

    #[tokio::test]
    async fn rows_outside_the_window_are_excluded() {
        let state = app(test_config());
        let cfg = state.cfg.load_full();
        let w = UsageWriter::open(&state.paths, &cfg.compat).expect("writer");
        w.append(&row("vast-gguf", 60, 10, 20)).expect("recent");
        w.append(&row("together", 3 * 86_400, 100, 200))
            .expect("old");

        let all = summarise(&state, None, GroupBy::Provider)
            .await
            .expect("all");
        assert_eq!(all.rows, 2);

        let day = parse_since(Some("24h")).expect("window");
        let recent = summarise(&state, day, GroupBy::Provider)
            .await
            .expect("24h");
        assert_eq!(recent.rows, 1, "the three-day-old row is outside 24h");
        assert_eq!(recent.by.len(), 1);
        assert_eq!(recent.by[0].key, "vast-gguf");
        assert_eq!(recent.total_prompt, 10);
        assert_eq!(recent.total_completion, 20);
    }
}
