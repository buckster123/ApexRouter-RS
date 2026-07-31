//! OWNER: unit C-14 (core/usage.rs, core/pricing.rs). Do not edit outside that unit.
//!
//! The usage log. Legacy field names are preserved **exactly**, so `cost.py` keeps working,
//! and `[compat] mirror_usage_log` — **off by default** — appends every row to
//! `~/.vastai-gguf/usage.log` too.
//!
//! Reading is deliberately lenient: unknown keys survive via `flatten`, `epoch` is optional,
//! and the legacy `%Y-%m-%dT%H:%M:%SZ` local-time-with-a-lying-`Z` timestamps parse. **No
//! row can ever fail a load.** Writing is strict: RFC 3339 UTC, one `write()` per row.
//!
//! ## The lying `Z`, and why `epoch` wins
//!
//! LocalRouter's `cost.py` wrote `datetime.now().strftime("%Y-%m-%dT%H:%M:%SZ")` — *local*
//! time with a `Z` glued on. The real `~/.vastai-gguf/usage.log` proves it: the first row
//! says `2026-05-02T20:11:21Z` and carries `epoch: 1777745481.53`, which is `18:11:21` UTC.
//! Two hours of CEST, asserted as UTC.
//!
//! So, in order:
//!
//! 1. `epoch` is unambiguous and **always wins** when a row has one. Every row we write
//!    carries one, precisely so this ambiguity dies with the legacy file.
//! 2. A `Z` timestamp with **sub-second precision** is one of ours: real UTC.
//! 3. A `Z` timestamp at whole-second precision is `cost.py`'s, and its `Z` is a lie — it is
//!    read as local time. A zone-less or space-separated stamp is read the same way.
//!
//! That discriminator is the only one available: our rows and `cost.py`'s rows are otherwise
//! textually identical, and guessing "UTC" would silently shift a whole day of legacy rows
//! out of a `24h` window on any machine that is not on UTC.

use crate::config::CompatCfg;
use crate::error::{Error, Result};
use crate::paths::Paths;
use apexrouter_protocol::{
    CostEstimate, Money, PriceSource, UsageBucket, UsageRecord, UsageSummary,
};
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, SecondsFormat, TimeZone, Utc};
use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Owner-only, like everything else under `$STATE`.
const FILE_MODE: u32 = 0o600;

/// The seven fields `cost.py` knows about, in the order it wrote them. The mirror carries
/// exactly these and nothing else.
const LEGACY_FIELDS: [&str; 7] = [
    "timestamp",
    "epoch",
    "provider",
    "model_id",
    "prompt_tokens",
    "completion_tokens",
    "cost_usd",
];

/// An `O_APPEND` handle on `usage.jsonl`, plus the optional legacy mirror.
///
/// Cloneable and `Send + Sync`: the router holds one for the whole process and appends from
/// whichever task finished a request.
#[derive(Clone, Debug)]
pub struct UsageWriter {
    /* C-14 */
    /// `$STATE/usage.jsonl`.
    path: PathBuf,
    /// Opened once, `O_APPEND`, so rotation by `copytruncate` keeps this fd valid.
    file: Arc<File>,
    /// `~/.vastai-gguf/usage.log`, when `[compat] mirror_usage_log` is on and it exists.
    mirror: Option<PathBuf>,
}

impl UsageWriter {
    /// Open (creating if needed) the usage log and, when configured, the legacy mirror.
    ///
    /// **The mirror is off unless it was asked for.** `[compat] mirror_usage_log` defaults to
    /// `false`, because `~/.vastai-gguf` belongs to the old Python tool and merely starting
    /// this daemon must not append to a file we do not own; `apexrouter migrate` offers to
    /// switch it on, which is the transition case it exists for. Even when it *is* on, the
    /// mirror is opened only while `~/.vastai-gguf` already exists — a machine that never ran
    /// LocalRouter must not grow that directory either.
    ///
    /// # Errors
    /// Returns [`Error::Io`] when `$STATE` cannot be created or `usage.jsonl` cannot be
    /// opened for append. Failing here, at startup, is the point: the alternative is finding
    /// out at the instant a row is due.
    pub fn open(paths: &Paths, cfg: &CompatCfg) -> Result<UsageWriter> {
        let legacy_dir = &paths.legacy().vastai_gguf;
        let mirror = if cfg.mirror_usage_log && legacy_dir.is_dir() {
            Some(legacy_dir.join("usage.log"))
        } else {
            None
        };
        UsageWriter::at(paths.usage_log(), mirror)
    }

    /// The real constructor, path-only so it is exercisable without a whole [`Paths`].
    fn at(path: PathBuf, mirror: Option<PathBuf>) -> Result<UsageWriter> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|source| Error::Io {
                path: dir.display().to_string(),
                source,
            })?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(FILE_MODE)
            .open(&path)
            .map_err(|source| Error::Io {
                path: path.display().to_string(),
                source,
            })?;
        Ok(UsageWriter {
            path,
            file: Arc::new(file),
            mirror,
        })
    }

    /// One `write()` per row, so a concurrent `tail -f` never sees a torn line.
    ///
    /// The row is canonicalised first: `timestamp` becomes RFC 3339 UTC at millisecond
    /// precision and `epoch` is filled in, whatever the caller supplied. Both are compat
    /// surfaces rather than caller data — see the module docs on the lying `Z`.
    ///
    /// A failing **mirror** is logged and swallowed: `~/.vastai-gguf` is a courtesy to
    /// `cost.py` and must never fail a request that has already happened.
    ///
    /// # Errors
    /// Returns [`Error::Json`] if the row will not serialise, or [`Error::Io`] if the append
    /// to `$STATE/usage.jsonl` fails.
    pub fn append(&self, rec: &UsageRecord) -> Result<()> {
        let row = canonicalise(rec);
        let mut line = serde_json::to_string(&row)?;
        line.push('\n');

        // One buffer, one `write_all` into an O_APPEND fd: no seek, no interleaving.
        let mut file: &File = &self.file;
        file.write_all(line.as_bytes())
            .map_err(|source| self.io(source))?;

        if let Some(mirror) = &self.mirror {
            if let Err(e) = append_legacy_mirror(mirror, &row) {
                tracing::warn!(path = %mirror.display(), error = %e, "usage mirror append failed");
            }
        }
        Ok(())
    }

    /// Rotate with **copytruncate** semantics, never truncate-on-start.
    ///
    /// The current file is copied to `usage.jsonl.1` and then truncated **in place**, so the
    /// `O_APPEND` fd this writer (and any `tail -f`) holds keeps pointing at the live inode.
    /// A rename would send every subsequent row into the rotated file. `max_mb == 0` disables
    /// rotation.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the size cannot be read, the copy fails, or the truncate
    /// fails.
    pub fn rotate_if_needed(&self, max_mb: u64) -> Result<()> {
        if max_mb == 0 {
            return Ok(());
        }
        let limit = max_mb.saturating_mul(1024 * 1024);
        if self.len()? <= limit {
            return Ok(());
        }

        // Exclusive across "check the size, copy, truncate", so two writers cannot both
        // rotate and lose a generation.
        rustix::fs::flock(&*self.file, rustix::fs::FlockOperation::LockExclusive)
            .map_err(|e| self.io(std::io::Error::from(e)))?;
        let result = self.rotate_locked(limit);
        // The fd outlives this call, so the lock must be dropped by hand.
        let unlocked = rustix::fs::flock(&*self.file, rustix::fs::FlockOperation::Unlock)
            .map_err(|e| self.io(std::io::Error::from(e)));
        result.and(unlocked)
    }

    /// The copy-then-truncate half of [`rotate_if_needed`](Self::rotate_if_needed), with the
    /// lock already held.
    fn rotate_locked(&self, limit: u64) -> Result<()> {
        // Re-check under the lock: whoever we queued behind may have just rotated.
        if self.len()? <= limit {
            return Ok(());
        }
        let rotated = rotated_path(&self.path);
        std::fs::copy(&self.path, &rotated).map_err(|source| Error::Io {
            path: rotated.display().to_string(),
            source,
        })?;
        self.file.set_len(0).map_err(|source| self.io(source))?;
        Ok(())
    }

    /// Current size of the live file.
    fn len(&self) -> Result<u64> {
        Ok(self
            .file
            .metadata()
            .map_err(|source| self.io(source))?
            .len())
    }

    /// Attach this writer's path to an I/O failure.
    fn io(&self, source: std::io::Error) -> Error {
        Error::Io {
            path: self.path.display().to_string(),
            source,
        }
    }
}

/// `usage.jsonl` → `usage.jsonl.1`, without eating an existing extension.
fn rotated_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".1");
    PathBuf::from(s)
}

/// Rewrite the two compat fields so every row we write is unambiguous.
///
/// `epoch` wins over the string, the string is re-rendered as RFC 3339 UTC with milliseconds,
/// and an unparseable or missing timestamp becomes "now" rather than a row that no window can
/// ever contain.
fn canonicalise(rec: &UsageRecord) -> UsageRecord {
    let dt = rec
        .epoch
        .and_then(datetime_from_epoch)
        .or_else(|| {
            parse_lenient_timestamp(&rec.timestamp).and_then(|s| Utc.timestamp_opt(s, 0).single())
        })
        .unwrap_or_else(Utc::now);

    let mut out = rec.clone();
    out.timestamp = dt.to_rfc3339_opts(SecondsFormat::Millis, true);
    out.epoch = Some(dt.timestamp_millis() as f64 / 1000.0);
    out
}

/// Fractional unix seconds → a UTC instant, rejecting NaN, infinities and out-of-range values.
fn datetime_from_epoch(epoch: f64) -> Option<DateTime<Utc>> {
    if !epoch.is_finite() {
        return None;
    }
    let millis = (epoch * 1000.0).round();
    if millis < i64::MIN as f64 || millis > i64::MAX as f64 {
        return None;
    }
    Utc.timestamp_millis_opt(millis as i64).single()
}

/// Append the **legacy field set only** to `~/.vastai-gguf/usage.log`.
///
/// `cost.py` reads this file with a fixed set of keys; sending it our additive fields would
/// be a gratuitous risk for zero benefit, so the mirror carries exactly the seven columns the
/// old writer produced, in the old order.
fn append_legacy_mirror(path: &Path, row: &UsageRecord) -> Result<()> {
    let mut map = serde_json::Map::new();
    map.insert("timestamp".into(), row.timestamp.clone().into());
    if let Some(epoch) = row.epoch.and_then(serde_json::Number::from_f64) {
        map.insert("epoch".into(), serde_json::Value::Number(epoch));
    }
    map.insert("provider".into(), row.provider.clone().into());
    map.insert("model_id".into(), row.model_id.clone().into());
    map.insert("prompt_tokens".into(), row.prompt_tokens.into());
    map.insert("completion_tokens".into(), row.completion_tokens.into());
    map.insert(
        "cost_usd".into(),
        serde_json::Number::from_f64(row.cost_usd)
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
    );

    let mut line = serde_json::to_string(&serde_json::Value::Object(map))?;
    line.push('\n');

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| Error::Io {
            path: path.display().to_string(),
            source,
        })?;
    file.write_all(line.as_bytes()).map_err(|source| Error::Io {
        path: path.display().to_string(),
        source,
    })
}

/// Every row we can find: `$STATE/usage.jsonl` plus, when `read_legacy_state`,
/// `~/.vastai-gguf/usage.log`.
///
/// Rows the mirror already duplicated are **not** counted twice: a legacy row whose seven
/// legacy fields match a row in `usage.jsonl` is dropped, one legacy row per matching new
/// row, so two genuinely identical requests still count as two.
///
/// The result is sorted oldest-first; rows whose time cannot be determined sort first.
///
/// # Errors
/// Returns [`Error::Io`] when a file exists but cannot be read. A file that is *absent* is
/// not an error, and no individual row can ever fail — see [`read_rows_counted`].
pub fn read_all(paths: &Paths, cfg: &CompatCfg) -> Result<Vec<UsageRecord>> {
    let legacy = cfg
        .read_legacy_state
        .then(|| paths.legacy().vastai_gguf.join("usage.log"));
    read_merged(&paths.usage_log(), legacy.as_deref())
}

/// [`read_all`] with the two paths supplied directly.
fn read_merged(new_path: &Path, legacy_path: Option<&Path>) -> Result<Vec<UsageRecord>> {
    let mut rows = read_rows(new_path)?;

    if let Some(legacy_path) = legacy_path {
        // A multiset, not a set: identical rows that are genuinely two requests must survive.
        let mut mirrored: HashMap<MirrorKey, usize> = HashMap::new();
        for row in &rows {
            *mirrored.entry(mirror_key(row)).or_insert(0) += 1;
        }
        for row in read_rows(legacy_path)? {
            match mirrored.get_mut(&mirror_key(&row)) {
                Some(n) if *n > 0 => *n -= 1,
                _ => rows.push(row),
            }
        }
    }

    rows.sort_by_key(|r| row_unix(r).unwrap_or(i64::MIN));
    Ok(rows)
}

/// The seven legacy fields, as an equality key. `f64` by bits, because a mirrored row is a
/// re-serialisation of the same number, never a recomputation of it.
type MirrorKey = (String, String, String, u32, u32, u64);

/// Build the dedup key for one row.
fn mirror_key(r: &UsageRecord) -> MirrorKey {
    (
        r.timestamp.clone(),
        r.provider.clone(),
        r.model_id.clone(),
        r.prompt_tokens,
        r.completion_tokens,
        r.cost_usd.to_bits(),
    )
}

/// Read one JSONL file. A missing file is an empty vector.
fn read_rows(path: &Path) -> Result<Vec<UsageRecord>> {
    Ok(read_rows_counted(path)?.0)
}

/// [`read_rows`] plus the number of lines that were not JSON objects at all.
///
/// Anything that *is* a JSON object becomes a row: the strict deserialiser is tried first,
/// and whatever it rejects (a float where a token count belongs, a stringified number, a
/// missing column) is salvaged field by field. That is the whole point — a strict reader
/// refusing real data is the bug this file exists to prevent.
fn read_rows_counted(path: &Path) -> Result<(Vec<UsageRecord>, usize)> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(source) => {
            return Err(Error::Io {
                path: path.display().to_string(),
                source,
            })
        }
    };

    // Lossy on purpose: one mangled byte in a 30 MB log must not cost us the other rows.
    let text = String::from_utf8_lossy(&bytes);
    let mut out = Vec::new();
    let mut unreadable = 0usize;

    for (n, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<UsageRecord>(line) {
            Ok(row) => out.push(row),
            Err(strict) => match serde_json::from_str::<serde_json::Value>(line) {
                Ok(serde_json::Value::Object(map)) => out.push(salvage(map)),
                _ => {
                    unreadable += 1;
                    tracing::warn!(
                        path = %path.display(), line = n + 1, error = %strict,
                        "usage row is not a JSON object; skipped"
                    );
                }
            },
        }
    }
    Ok((out, unreadable))
}

/// Coerce a JSON object that the strict deserialiser rejected into a row.
///
/// Nothing is discarded: every key we do not recognise stays in `extra`, exactly as the
/// `flatten` path would have kept it.
fn salvage(mut map: serde_json::Map<String, serde_json::Value>) -> UsageRecord {
    let timestamp = map
        .remove("timestamp")
        .and_then(|v| as_string(&v))
        .unwrap_or_default();
    let epoch = map.remove("epoch").and_then(|v| as_f64(&v));
    let provider = map
        .remove("provider")
        .and_then(|v| as_string(&v))
        .unwrap_or_default();
    let model_id = map
        .remove("model_id")
        .and_then(|v| as_string(&v))
        .unwrap_or_default();
    let prompt_tokens = map
        .remove("prompt_tokens")
        .and_then(|v| as_u32(&v))
        .unwrap_or(0);
    let completion_tokens = map
        .remove("completion_tokens")
        .and_then(|v| as_u32(&v))
        .unwrap_or(0);
    let cost_usd = map
        .remove("cost_usd")
        .and_then(|v| as_f64(&v))
        .unwrap_or(0.0);
    let request_id = map.remove("request_id").and_then(|v| as_string(&v));
    let backend = map.remove("backend").and_then(|v| as_string(&v));
    let alias = map.remove("alias").and_then(|v| as_string(&v));
    let ttft_ms = map.remove("ttft_ms").and_then(|v| as_u32(&v));
    let tok_per_s = map
        .remove("tok_per_s")
        .and_then(|v| as_f64(&v))
        .map(|f| f as f32);
    let stream = map.remove("stream").and_then(|v| as_bool(&v));
    let estimated = map.remove("estimated").and_then(|v| as_bool(&v));

    UsageRecord {
        timestamp,
        epoch,
        provider,
        model_id,
        prompt_tokens,
        completion_tokens,
        cost_usd,
        request_id,
        backend,
        alias,
        ttft_ms,
        tok_per_s,
        stream,
        estimated,
        extra: map,
    }
}

/// A string, or a number/bool rendered as one.
fn as_string(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// A number, or a number that arrived quoted.
fn as_f64(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// A token count, tolerating floats and quoted numbers, clamped into `u32`.
fn as_u32(v: &serde_json::Value) -> Option<u32> {
    let f = as_f64(v)?;
    if !f.is_finite() || f < 0.0 {
        return Some(0);
    }
    Some(f.round().min(f64::from(u32::MAX)) as u32)
}

/// A bool, tolerating `"true"` / `"false"` / `1` / `0`.
fn as_bool(v: &serde_json::Value) -> Option<bool> {
    match v {
        serde_json::Value::Bool(b) => Some(*b),
        serde_json::Value::Number(n) => n.as_f64().map(|f| f != 0.0),
        serde_json::Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Some(true),
            "false" | "0" | "no" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// When a row happened, in unix seconds. `epoch` first — it cannot lie about its zone.
fn row_unix(r: &UsageRecord) -> Option<i64> {
    r.epoch
        .and_then(datetime_from_epoch)
        .map(|d| d.timestamp())
        .or_else(|| parse_lenient_timestamp(&r.timestamp))
}

/// Aggregate over a real window (`24h`, `7d`, `all`) and a grouping.
///
/// `since` is a unix-seconds cutoff, inclusive. A row whose time cannot be determined at all
/// is counted in `all` and excluded from every bounded window — it cannot be *shown* to be
/// inside one, and a window that quietly includes undated rows is not a window.
///
/// Cost is folded with [`CostEstimate::add`], so one estimated row visibly demotes the total
/// instead of the total quietly claiming to be metered.
pub fn aggregate(rows: &[UsageRecord], since: Option<i64>, by: GroupBy) -> UsageSummary {
    /// One group under construction.
    #[derive(Default)]
    struct Acc {
        cost: Option<CostEstimate>,
        prompt: u64,
        completion: u64,
        requests: u64,
        tps: Vec<f32>,
    }

    let mut groups: BTreeMap<String, Acc> = BTreeMap::new();
    let mut total_prompt = 0u64;
    let mut total_completion = 0u64;
    let mut counted = 0u64;

    for row in rows {
        let at = row_unix(row);
        if let Some(cutoff) = since {
            match at {
                Some(t) if t >= cutoff => {}
                _ => continue,
            }
        }

        let acc = groups.entry(group_key(row, at, by)).or_default();
        let cost = row_cost(row);
        acc.cost = Some(match acc.cost.take() {
            Some(prev) => prev.add(cost),
            None => cost,
        });
        acc.prompt = acc.prompt.saturating_add(u64::from(row.prompt_tokens));
        acc.completion = acc
            .completion
            .saturating_add(u64::from(row.completion_tokens));
        acc.requests += 1;
        if let Some(tps) = row.tok_per_s.filter(|t| t.is_finite() && *t > 0.0) {
            acc.tps.push(tps);
        }

        total_prompt = total_prompt.saturating_add(u64::from(row.prompt_tokens));
        total_completion = total_completion.saturating_add(u64::from(row.completion_tokens));
        counted += 1;
    }

    let mut by_bucket: Vec<UsageBucket> = groups
        .into_iter()
        .map(|(key, acc)| UsageBucket {
            key,
            cost: acc.cost.unwrap_or(CostEstimate::Unknown),
            prompt_tokens: acc.prompt,
            completion_tokens: acc.completion,
            requests: acc.requests,
            tok_per_s_p50: median(acc.tps),
        })
        .collect();

    // Days read chronologically; every other grouping reads biggest-spend first.
    if by != GroupBy::Day {
        by_bucket.sort_by(|a, b| {
            b.cost
                .usd()
                .unwrap_or(Money::ZERO)
                .cmp(&a.cost.usd().unwrap_or(Money::ZERO))
                .then_with(|| a.key.cmp(&b.key))
        });
    }

    let total_cost = by_bucket
        .iter()
        .map(|b| b.cost.clone())
        .reduce(CostEstimate::add)
        .unwrap_or(CostEstimate::Metered {
            usd: Money::ZERO,
            source: PriceSource::ConfigTable,
        });

    UsageSummary {
        window: window_label(since),
        by: by_bucket,
        total_cost,
        total_prompt,
        total_completion,
        rows: counted,
    }
}

/// What an absent grouping field renders as. Never blank: a blank key looks like a bug.
const UNKNOWN_KEY: &str = "unknown";

/// The bucket one row belongs to.
fn group_key(row: &UsageRecord, at: Option<i64>, by: GroupBy) -> String {
    match by {
        GroupBy::Provider => non_empty(&row.provider),
        GroupBy::Model => non_empty(&row.model_id),
        GroupBy::Backend => row
            .backend
            .as_deref()
            .map_or_else(|| UNKNOWN_KEY.to_owned(), non_empty),
        GroupBy::Alias => row
            .alias
            .as_deref()
            .map_or_else(|| UNKNOWN_KEY.to_owned(), non_empty),
        GroupBy::Day => at
            .and_then(|t| Utc.timestamp_opt(t, 0).single())
            .map_or_else(
                || UNKNOWN_KEY.to_owned(),
                |d| d.format("%Y-%m-%d").to_string(),
            ),
    }
}

/// `""` → `"unknown"`.
fn non_empty(s: &str) -> String {
    if s.trim().is_empty() {
        UNKNOWN_KEY.to_owned()
    } else {
        s.to_owned()
    }
}

/// What one row's `cost_usd` is worth as an honest estimate.
///
/// The source is [`PriceSource::ConfigTable`]: the number was computed from a price table
/// when the row was written, not billed to us by a provider. A row that was flagged
/// `estimated` at write time stays a guess forever, and says why.
fn row_cost(row: &UsageRecord) -> CostEstimate {
    let usd = Money::from_usd(row.cost_usd);
    if row.estimated == Some(true) {
        CostEstimate::Approximate {
            usd,
            source: PriceSource::ConfigTable,
            assumption: "token counts were estimated, not reported by the upstream".to_owned(),
        }
    } else {
        CostEstimate::Metered {
            usd,
            source: PriceSource::ConfigTable,
        }
    }
}

/// Median throughput, or `None` when nothing in the bucket reported one.
fn median(mut v: Vec<f32>) -> Option<f32> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = v.len() / 2;
    if v.len() % 2 == 1 {
        Some(v[mid])
    } else {
        Some((v[mid - 1] + v[mid]) / 2.0)
    }
}

/// Name the window the way the user asked for it: `"24h"`, `"7d"`, `"all"`.
///
/// The cutoff is a unix instant by the time it reaches [`aggregate`], so the label is
/// recovered by measuring it against now, with a minute of slack for the round trip. Anything
/// that is not one of the named windows renders as its cutoff, which is still true.
fn window_label(since: Option<i64>) -> String {
    /// The windows the CLI, the MCP tool and both GUIs offer.
    const NAMED: [(i64, &str); 6] = [
        (3_600, "1h"),
        (21_600, "6h"),
        (86_400, "24h"),
        (604_800, "7d"),
        (2_592_000, "30d"),
        (7_776_000, "90d"),
    ];
    let Some(since) = since else {
        return "all".to_owned();
    };
    let span = Utc::now().timestamp() - since;
    for (secs, name) in NAMED {
        if (span - secs).abs() <= 60 {
            return name.to_owned();
        }
    }
    Utc.timestamp_opt(since, 0).single().map_or_else(
        || format!("since {since}"),
        |d| format!("since {}", d.to_rfc3339_opts(SecondsFormat::Secs, true)),
    )
}

/// How to bucket a [`UsageSummary`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GroupBy {
    /// By provider id.
    Provider,
    /// By upstream model id.
    Model,
    /// By backend id.
    Backend,
    /// By alias.
    Alias,
    /// By calendar day, UTC.
    Day,
}

/// Parse both the RFC 3339 UTC we write and the legacy local-time-with-a-lying-`Z` we read.
///
/// Accepted, in order:
///
/// * a bare number — fractional unix seconds;
/// * `…Z` **with** a fractional part — one of ours, real UTC;
/// * `…Z` at whole-second precision — `cost.py`'s, read as **local** time (module docs);
/// * RFC 3339 with a real numeric offset (`+02:00`);
/// * zone-less `YYYY-MM-DDTHH:MM[:SS[.fff]]`, `T` or space separated — local time;
/// * `YYYY-MM-DD` — local midnight.
///
/// Returns `None` only when the string is none of those. During a DST fold the earlier of the
/// two possible instants is chosen; a nonexistent local time yields `None`.
pub fn parse_lenient_timestamp(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    // A bare epoch. Guarded by "no date punctuation" so `2026-05-02` is never read as a
    // subtraction and `2026-05-02T10:00:00` never reaches `parse::<f64>`.
    if !s.contains(['-', ':', 'T', 't', ' ', '/']) {
        if let Ok(epoch) = s.parse::<f64>() {
            return datetime_from_epoch(epoch).map(|d| d.timestamp());
        }
    }

    if s.ends_with('Z') || s.ends_with('z') {
        let naive = parse_naive(&s[..s.len() - 1])?;
        // Sub-second precision means we wrote it, and our `Z` is honest. Whole seconds is
        // `cost.py`'s lying `Z`, which is local time.
        return if s.contains('.') {
            Some(naive.and_utc().timestamp())
        } else {
            local_to_unix(naive)
        };
    }

    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp());
    }
    parse_naive(s).and_then(local_to_unix)
}

/// Every zone-less shape the legacy writers produced.
fn parse_naive(s: &str) -> Option<NaiveDateTime> {
    let s = s.trim().replace(' ', "T");
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(&s, fmt) {
            return Some(dt);
        }
    }
    NaiveDate::parse_from_str(&s, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
}

/// A local wall-clock reading → unix seconds, taking the earlier instant of a DST fold.
fn local_to_unix(naive: NaiveDateTime) -> Option<i64> {
    Local
        .from_local_datetime(&naive)
        .earliest()
        .map(|d| d.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real `~/.vastai-gguf/usage.log` from the ground-truth machine, verbatim.
    ///
    /// Embedded so the regression holds on a machine that never ran LocalRouter; the test
    /// *also* reads the real file when it is there.
    const REAL_LEGACY_LOG: &str = concat!(
        r#"{"timestamp": "2026-05-02T20:11:21Z", "epoch": 1777745481.5262182, "provider": "together", "model_id": "meta-llama/Llama-3.1-8B-Instruct-Turbo", "prompt_tokens": 100, "completion_tokens": 50, "cost_usd": 2.7e-05}"#,
        "\n",
        r#"{"timestamp": "2026-05-02T20:11:21Z", "epoch": 1777745481.5263166, "provider": "vast-gguf", "model_id": "Qwen3.6-27B-Q8.gguf", "prompt_tokens": 100, "completion_tokens": 50, "cost_usd": 0.000766}"#,
        "\n",
        r#"{"timestamp": "2026-05-02T20:11:49Z", "provider": "together", "model_id": "Qwen/Qwen2.5-72B-Instruct-Turbo", "prompt_tokens": 131072, "completion_tokens": 500, "cost_usd": 0.115783}"#,
        "\n",
        r#"{"timestamp": "2026-05-02T20:12:11Z", "epoch": 1777745531.4937491, "provider": "together", "model_id": "Qwen/Qwen2.5-72B-Instruct-Turbo", "prompt_tokens": 131072, "completion_tokens": 500, "cost_usd": 0.115783}"#,
        "\n",
    );

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    /// Every environment variable [`Paths::resolve`] consults, so a test can neutralise the
    /// lot and leave the process exactly as it found it.
    const ENV_KEYS: [&str; 6] = [
        "HOME",
        "APEXROUTER_HOME",
        "APEXROUTER_LOCALROUTER_DIR",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
        "XDG_CONFIG_HOME",
    ];

    /// `std::env` is process-global; the tests here that redirect it serialise on this.
    fn env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    /// A [`Paths`] whose state root **and** whose legacy `~/.vastai-gguf` both live inside
    /// `home`, a tempdir standing in for `$HOME`.
    ///
    /// Both are asserted before the value is handed back: if another test's environment
    /// raced us, this fails here rather than anywhere near the real `~/.vastai-gguf`.
    fn test_paths(home: &Path) -> Paths {
        let guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let saved: Vec<(&str, Option<std::ffi::OsString>)> =
            ENV_KEYS.iter().map(|k| (*k, std::env::var_os(k))).collect();
        for k in ENV_KEYS {
            std::env::remove_var(k);
        }
        std::env::set_var("HOME", home);
        std::env::set_var("APEXROUTER_HOME", home.join("state"));
        std::env::set_var("APEXROUTER_LOCALROUTER_DIR", home.join("no-localrouter"));
        let resolved = Paths::resolve();
        for (k, v) in saved {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        drop(guard);

        let paths = resolved.expect("Paths::resolve");
        assert!(
            paths.state().starts_with(home),
            "state {} escaped the tempdir {}",
            paths.state().display(),
            home.display()
        );
        assert!(
            paths.legacy().vastai_gguf.starts_with(home),
            "legacy dir {} escaped the tempdir {}",
            paths.legacy().vastai_gguf.display(),
            home.display()
        );
        paths
    }

    /// Every file under `root` that is **not** inside `skip`, as path → bytes.
    fn tree_outside(root: &Path, skip: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut out = BTreeMap::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            if dir.starts_with(skip) {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Ok(bytes) = std::fs::read(&path) {
                    out.insert(path, bytes);
                }
            }
        }
        out
    }

    fn row(provider: &str, model: &str, cost: f64) -> UsageRecord {
        UsageRecord {
            timestamp: "2026-07-30T10:00:00.000Z".into(),
            epoch: Some(1_785_405_600.0),
            provider: provider.into(),
            model_id: model.into(),
            prompt_tokens: 100,
            completion_tokens: 50,
            cost_usd: cost,
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

    // ---- the named regression from BUILD-PLAN §6.3 ----------------------------------------

    /// Guards against a strict deserializer rejecting real data.
    #[test]
    fn real_legacy_log_has_zero_failed_rows() {
        let dir = tmp();
        let path = dir.path().join("usage.log");
        std::fs::write(&path, REAL_LEGACY_LOG).expect("write fixture");

        let (rows, unreadable) = read_rows_counted(&path).expect("read");
        assert_eq!(unreadable, 0, "no line may be unreadable");
        assert_eq!(rows.len(), 4, "every real row must load");

        // The legacy `epoch` survives, and its absence on row 3 is not an error.
        assert_eq!(rows[0].epoch, Some(1_777_745_481.526_218_2));
        assert_eq!(rows[2].epoch, None);
        assert_eq!(rows[1].provider, "vast-gguf");
        assert_eq!(rows[3].prompt_tokens, 131_072);

        // And the real file on this machine, when it is here.
        let real = dirs::home_dir()
            .map(|h| h.join(".vastai-gguf/usage.log"))
            .filter(|p| p.is_file());
        if let Some(real) = real {
            let text = std::fs::read_to_string(&real).expect("read real log");
            let expected = text.lines().filter(|l| !l.trim().is_empty()).count();
            let (rows, unreadable) = read_rows_counted(&real).expect("read real log");
            assert_eq!(unreadable, 0, "the real log must have zero failed rows");
            assert_eq!(rows.len(), expected, "every real row must load");
        }
    }

    #[test]
    fn an_unknown_key_survives_a_round_trip() {
        let line = r#"{"timestamp": "2026-05-02T20:11:21Z", "provider": "together", "model_id": "m", "prompt_tokens": 1, "completion_tokens": 2, "cost_usd": 0.5, "who_added_this": {"deep": [1,2]}}"#;
        let row: UsageRecord = serde_json::from_str(line).expect("lenient by construction");
        assert_eq!(
            row.extra.get("who_added_this").map(ToString::to_string),
            Some(r#"{"deep":[1,2]}"#.to_owned())
        );
        let back = serde_json::to_string(&row).expect("ser");
        assert!(back.contains("who_added_this"), "{back}");
    }

    #[test]
    fn a_row_the_strict_reader_rejects_is_still_salvaged() {
        let dir = tmp();
        let path = dir.path().join("usage.log");
        std::fs::write(
            &path,
            concat!(
                r#"{"timestamp":"2026-05-02T20:11:21Z","provider":"together","model_id":"m","prompt_tokens":"100","completion_tokens":50.0,"cost_usd":"0.25","estimated":"true","junk":7}"#,
                "\n",
                "not json at all\n",
                "\n",
            ),
        )
        .expect("write");

        let (rows, unreadable) = read_rows_counted(&path).expect("read");
        assert_eq!(unreadable, 1, "only the non-JSON line is unreadable");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].prompt_tokens, 100);
        assert_eq!(rows[0].completion_tokens, 50);
        assert!((rows[0].cost_usd - 0.25).abs() < 1e-9);
        assert_eq!(rows[0].estimated, Some(true));
        assert!(rows[0].extra.contains_key("junk"));
    }

    // ---- the on-disk shape ------------------------------------------------------------------

    #[test]
    fn the_written_row_has_the_legacy_field_names_in_the_legacy_order() {
        let dir = tmp();
        let w = UsageWriter::at(dir.path().join("usage.jsonl"), None).expect("open");
        w.append(&row("together", "m", 0.25)).expect("append");

        let text = std::fs::read_to_string(dir.path().join("usage.jsonl")).expect("read");
        let v: serde_json::Value = serde_json::from_str(text.trim()).expect("parse");
        let obj = v.as_object().expect("object");
        let keys: Vec<&str> = obj.keys().take(7).map(String::as_str).collect();
        assert_eq!(keys, LEGACY_FIELDS, "legacy columns, legacy order");
        // cost.py reads these by name; they must be the same names, not renamed ones.
        assert!(obj["cost_usd"].is_number());
        assert!(obj["model_id"].is_string());
        assert!(obj["prompt_tokens"].is_number());
    }

    #[test]
    fn timestamps_written_are_rfc_3339_utc() {
        let dir = tmp();
        let w = UsageWriter::at(dir.path().join("usage.jsonl"), None).expect("open");
        // A legacy local-time-with-a-lying-Z going in comes out canonical.
        let mut r = row("together", "m", 0.25);
        r.timestamp = "2026-05-02T20:11:21Z".into();
        r.epoch = Some(1_777_745_481.526);
        w.append(&r).expect("append");

        let text = std::fs::read_to_string(dir.path().join("usage.jsonl")).expect("read");
        let v: serde_json::Value = serde_json::from_str(text.trim()).expect("parse");
        let ts = v["timestamp"].as_str().expect("timestamp");
        assert_eq!(
            ts, "2026-05-02T18:11:21.526Z",
            "epoch wins over the lying Z"
        );
        let parsed = DateTime::parse_from_rfc3339(ts).expect("strict RFC 3339");
        assert_eq!(parsed.timestamp(), 1_777_745_481);
        assert_eq!(v["epoch"].as_f64(), Some(1_777_745_481.526));
    }

    #[test]
    fn rows_accumulate_and_every_row_is_newline_terminated() {
        let dir = tmp();
        let path = dir.path().join("usage.jsonl");
        let w = UsageWriter::at(path.clone(), None).expect("open");
        for i in 0..5 {
            w.append(&row("together", &format!("m{i}"), 0.1))
                .expect("append");
        }
        let text = std::fs::read_to_string(&path).expect("read");
        assert_eq!(text.lines().count(), 5);
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn the_mirror_carries_only_the_legacy_field_set() {
        let dir = tmp();
        std::fs::create_dir_all(dir.path().join("legacy")).expect("mkdir");
        let mirror = dir.path().join("legacy/usage.log");
        let w =
            UsageWriter::at(dir.path().join("usage.jsonl"), Some(mirror.clone())).expect("open");

        let mut r = row("vast-gguf", "Qwen3.6-27B-Q8.gguf", 0.000_766);
        r.alias = Some("auto".into());
        r.backend = Some("local-carnice".into());
        r.tok_per_s = Some(4.2);
        w.append(&r).expect("append");

        let text = std::fs::read_to_string(&mirror).expect("read mirror");
        let v: serde_json::Value = serde_json::from_str(text.trim()).expect("parse");
        let keys: Vec<&str> = v
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, LEGACY_FIELDS, "exactly the legacy columns");
        assert_eq!(v["provider"], "vast-gguf", "the legacy spelling stays");
    }

    /// FINDING B from MK1-CORE ACCEPTANCE: a default launch appended 15 rows to Andre's real
    /// `~/.vastai-gguf/usage.log`. Writing into another tool's state directory is opt-in.
    #[test]
    fn the_legacy_mirror_is_off_by_default_and_writes_nothing_outside_state() {
        assert!(
            !CompatCfg::default().mirror_usage_log,
            "`[compat] mirror_usage_log` must default to false"
        );

        let dir = tmp();
        let home = dir.path();
        let legacy = home.join(".vastai-gguf");
        std::fs::create_dir_all(&legacy).expect("legacy dir");
        std::fs::write(legacy.join("usage.log"), REAL_LEGACY_LOG).expect("legacy fixture");

        let paths = test_paths(home);
        let before = tree_outside(home, paths.state());
        assert!(
            before.contains_key(&legacy.join("usage.log")),
            "the fixture must be in the snapshot, or this test proves nothing"
        );

        let w = UsageWriter::open(&paths, &CompatCfg::default()).expect("open");
        assert!(
            w.mirror.is_none(),
            "the default must not even open another tool's file"
        );
        for i in 0..3 {
            w.append(&row("together", &format!("m{i}"), 0.01))
                .expect("append");
        }

        assert_eq!(
            tree_outside(home, paths.state()),
            before,
            "nothing outside $STATE may be created or modified"
        );

        // The rows are not lost — they are in our own log, which is inside $STATE.
        assert!(paths.usage_log().starts_with(paths.state()));
        let ours = std::fs::read_to_string(paths.usage_log()).expect("read ours");
        assert_eq!(ours.lines().count(), 3);

        // Positive control: opted in, the legacy file does grow — so the assertion above is
        // testing the toggle and not merely a path that never worked.
        let opted_in = CompatCfg {
            mirror_usage_log: true,
            ..CompatCfg::default()
        };
        let w = UsageWriter::open(&paths, &opted_in).expect("open");
        assert_eq!(
            w.mirror.as_deref(),
            Some(legacy.join("usage.log").as_path())
        );
        w.append(&row("together", "m", 0.01)).expect("append");
        let mirrored = std::fs::read_to_string(legacy.join("usage.log")).expect("read mirror");
        assert_eq!(
            mirrored.lines().count(),
            REAL_LEGACY_LOG.lines().count() + 1,
            "opting in appends exactly one row"
        );
    }

    #[test]
    fn rotation_is_copytruncate_and_keeps_the_inode() {
        use std::os::unix::fs::MetadataExt;

        let dir = tmp();
        let path = dir.path().join("usage.jsonl");
        let w = UsageWriter::at(path.clone(), None).expect("open");
        w.append(&row("together", "before", 0.01)).expect("append");
        let inode = std::fs::metadata(&path).expect("stat").ino();

        // 0 disables rotation, and a file under the limit is left alone.
        std::fs::write(&path, vec![b'x'; 2 * 1024 * 1024]).expect("grow");
        w.rotate_if_needed(0).expect("disabled");
        w.rotate_if_needed(8).expect("under the limit");
        assert_eq!(
            std::fs::metadata(&path).expect("stat").len(),
            2 * 1024 * 1024
        );
        assert!(!rotated_path(&path).exists(), "nothing rotated yet");

        // Over the limit: copy, then truncate in place.
        w.rotate_if_needed(1).expect("rotate");
        assert_eq!(
            std::fs::metadata(&path).expect("stat").len(),
            0,
            "truncated"
        );
        assert_eq!(
            std::fs::metadata(&path).expect("stat").ino(),
            inode,
            "copytruncate keeps the inode a tail -f is holding"
        );
        assert_eq!(
            std::fs::metadata(rotated_path(&path))
                .expect("stat .1")
                .len(),
            2 * 1024 * 1024,
            "the rotated copy has the old bytes"
        );

        // The O_APPEND fd still works, and the next row lands at the new offset 0.
        w.append(&row("together", "after", 0.01)).expect("append");
        let text = std::fs::read_to_string(&path).expect("read");
        assert_eq!(text.lines().count(), 1);
    }

    // ---- reading and merging ------------------------------------------------------------

    #[test]
    fn mirrored_rows_are_not_counted_twice() {
        let dir = tmp();
        let new = dir.path().join("usage.jsonl");
        let legacy = dir.path().join("usage.log");
        let w = UsageWriter::at(new.clone(), Some(legacy.clone())).expect("open");

        w.append(&row("together", "m", 0.25)).expect("append");
        w.append(&row("together", "m", 0.25)).expect("append twice");

        // Plus one row only the old world knows about.
        let mut legacy_text = std::fs::read_to_string(&legacy).expect("read");
        legacy_text.push_str(
            r#"{"timestamp": "2026-05-02T20:11:21Z", "epoch": 1777745481.5, "provider": "together", "model_id": "old", "prompt_tokens": 1, "completion_tokens": 1, "cost_usd": 0.5}"#,
        );
        legacy_text.push('\n');
        std::fs::write(&legacy, legacy_text).expect("write");

        let rows = read_merged(&new, Some(&legacy)).expect("merge");
        assert_eq!(rows.len(), 3, "two new rows plus the one legacy-only row");
        assert_eq!(rows.iter().filter(|r| r.model_id == "m").count(), 2);
        assert_eq!(rows.iter().filter(|r| r.model_id == "old").count(), 1);
        // Oldest first.
        assert_eq!(rows[0].model_id, "old");

        // And without the legacy file the answer is just the new rows.
        assert_eq!(read_merged(&new, None).expect("merge").len(), 2);
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let dir = tmp();
        let rows = read_merged(
            &dir.path().join("nope.jsonl"),
            Some(&dir.path().join("also-nope.log")),
        )
        .expect("no files is fine");
        assert!(rows.is_empty());
    }

    // ---- timestamps ----------------------------------------------------------------------

    #[test]
    fn lenient_timestamps_cover_every_shape_on_disk() {
        // Ours: sub-second `Z` is honest UTC.
        assert_eq!(
            parse_lenient_timestamp("2026-05-02T18:11:21.526Z"),
            Some(1_777_745_481)
        );
        // A real offset is taken at face value.
        assert_eq!(
            parse_lenient_timestamp("2026-05-02T20:11:21+02:00"),
            Some(1_777_745_481)
        );
        // A bare epoch.
        assert_eq!(
            parse_lenient_timestamp("1777745481.5262182"),
            Some(1_777_745_481)
        );
        // cost.py's whole-second `Z` is local time wearing a UTC badge.
        let naive = NaiveDateTime::parse_from_str("2026-05-02T20:11:21", "%Y-%m-%dT%H:%M:%S")
            .expect("naive");
        let as_local = local_to_unix(naive);
        assert!(as_local.is_some());
        assert_eq!(parse_lenient_timestamp("2026-05-02T20:11:21Z"), as_local);
        // Zone-less, space-separated and date-only all parse as local too.
        assert_eq!(parse_lenient_timestamp("2026-05-02 20:11:21"), as_local);
        assert_eq!(parse_lenient_timestamp("2026-05-02T20:11:21"), as_local);
        assert!(parse_lenient_timestamp("2026-05-02").is_some());
        // Nothing else does.
        assert_eq!(parse_lenient_timestamp(""), None);
        assert_eq!(parse_lenient_timestamp("yesterday"), None);
        assert_eq!(parse_lenient_timestamp("2026-13-45T99:99:99Z"), None);
    }

    #[test]
    fn on_the_real_log_epoch_wins_over_the_lying_z() {
        let dir = tmp();
        let path = dir.path().join("u.log");
        std::fs::write(&path, REAL_LEGACY_LOG).expect("write");
        let (rows, _) = read_rows_counted(&path).expect("read");

        let epoch = rows[0].epoch.expect("row 0 has an epoch") as i64;
        assert_eq!(row_unix(&rows[0]), Some(epoch), "epoch is what readers use");

        // The string reading differs from the epoch by a whole zone offset, never by drift:
        // on the machine that wrote it (CEST) the two agree exactly.
        let from_string = parse_lenient_timestamp(&rows[0].timestamp).expect("parses");
        assert_eq!(
            (from_string - epoch).rem_euclid(900),
            0,
            "the difference is a zone offset, not arbitrary drift"
        );

        // Row 3 has no epoch at all, so it falls back to the string.
        assert_eq!(
            row_unix(&rows[2]),
            parse_lenient_timestamp(&rows[2].timestamp)
        );
    }

    // ---- aggregation ---------------------------------------------------------------------

    fn at(secs_ago: i64, provider: &str, cost: f64) -> UsageRecord {
        let mut r = row(provider, "m", cost);
        r.epoch = Some((Utc::now().timestamp() - secs_ago) as f64);
        r.timestamp = String::new();
        r
    }

    #[test]
    fn aggregation_windows_are_real() {
        let now = Utc::now().timestamp();
        let rows = vec![
            at(60, "together", 1.0),
            at(3 * 86_400, "together", 2.0),
            at(30 * 86_400, "vast-gguf", 4.0),
        ];

        let day = aggregate(&rows, Some(now - 86_400), GroupBy::Provider);
        assert_eq!(day.window, "24h");
        assert_eq!(day.rows, 1);
        assert_eq!(day.total_cost.usd(), Some(Money::from_usd(1.0)));

        let week = aggregate(&rows, Some(now - 7 * 86_400), GroupBy::Provider);
        assert_eq!(week.window, "7d");
        assert_eq!(week.rows, 2);
        assert_eq!(week.total_cost.usd(), Some(Money::from_usd(3.0)));

        let all = aggregate(&rows, None, GroupBy::Provider);
        assert_eq!(all.window, "all");
        assert_eq!(all.rows, 3);
        assert_eq!(all.total_cost.usd(), Some(Money::from_usd(7.0)));
        assert_eq!(all.total_prompt, 300);
        assert_eq!(all.total_completion, 150);
    }

    #[test]
    fn an_undated_row_is_in_all_and_in_no_bounded_window() {
        let mut undated = row("together", "m", 1.0);
        undated.epoch = None;
        undated.timestamp = "some day".into();
        let rows = vec![undated, at(60, "together", 1.0)];

        assert_eq!(aggregate(&rows, None, GroupBy::Provider).rows, 2);
        assert_eq!(
            aggregate(
                &rows,
                Some(Utc::now().timestamp() - 86_400),
                GroupBy::Provider
            )
            .rows,
            1
        );
    }

    #[test]
    fn grouping_buckets_by_every_key() {
        let mut a = at(60, "together", 1.0);
        a.alias = Some("auto".into());
        a.backend = Some("together-1".into());
        a.tok_per_s = Some(10.0);
        let mut b = at(60, "vast-gguf", 3.0);
        b.model_id = "other".into();
        b.tok_per_s = Some(30.0);
        let mut c = at(60, "vast-gguf", 2.0);
        c.model_id = "other".into();
        c.tok_per_s = Some(50.0);
        let rows = vec![a, b, c];

        let by_provider = aggregate(&rows, None, GroupBy::Provider);
        assert_eq!(by_provider.by.len(), 2);
        // Biggest spend first.
        assert_eq!(by_provider.by[0].key, "vast-gguf");
        assert_eq!(by_provider.by[0].requests, 2);
        assert_eq!(by_provider.by[0].tok_per_s_p50, Some(40.0));
        assert_eq!(by_provider.by[1].key, "together");

        assert_eq!(aggregate(&rows, None, GroupBy::Model).by.len(), 2);
        // An absent alias/backend renders as "unknown", never as "".
        let by_alias = aggregate(&rows, None, GroupBy::Alias);
        assert!(by_alias.by.iter().any(|b| b.key == "unknown"));
        assert!(by_alias.by.iter().any(|b| b.key == "auto"));
        let by_backend = aggregate(&rows, None, GroupBy::Backend);
        assert!(by_backend.by.iter().any(|b| b.key == "together-1"));

        let by_day = aggregate(&rows, None, GroupBy::Day);
        assert_eq!(by_day.by.len(), 1);
        assert_eq!(by_day.by[0].key.len(), 10, "YYYY-MM-DD");
    }

    #[test]
    fn one_estimated_row_demotes_the_total() {
        let mut est = at(60, "together", 1.0);
        est.estimated = Some(true);
        let rows = vec![est, at(60, "vast-gguf", 1.0)];

        let s = aggregate(&rows, None, GroupBy::Provider);
        assert!(s.total_cost.is_guess(), "{:?}", s.total_cost);
        match s.total_cost {
            CostEstimate::Approximate { assumption, .. } => {
                assert!(!assumption.is_empty(), "the assumption must be legible");
            }
            other => panic!("expected Approximate, got {other:?}"),
        }

        // With no estimated rows the total stays metered.
        let clean = aggregate(&[at(60, "together", 1.0)], None, GroupBy::Provider);
        assert!(!clean.total_cost.is_guess());
        // And an empty window is an honest zero, not an unknown.
        let empty = aggregate(&[], None, GroupBy::Provider);
        assert_eq!(empty.rows, 0);
        assert_eq!(empty.total_cost.usd(), Some(Money::ZERO));
        assert!(!empty.total_cost.is_guess());
    }
}
