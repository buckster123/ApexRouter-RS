//! OWNER: unit C-14 (core/usage.rs, core/pricing.rs). Do not edit outside that unit.
//!
//! The usage log. Legacy field names are preserved **exactly**, so `cost.py` keeps working,
//! and `[compat] mirror_usage_log` appends every row to `~/.vastai-gguf/usage.log` too.
//!
//! Reading is deliberately lenient: unknown keys survive via `flatten`, `epoch` is optional,
//! and the legacy `%Y-%m-%dT%H:%M:%SZ` local-time-with-a-lying-`Z` timestamps parse. **No
//! row can ever fail a load.** Writing is strict: RFC 3339 UTC, one `write()` per row.

use crate::config::CompatCfg;
use crate::error::Result;
use crate::paths::Paths;
use apexrouter_protocol::{UsageRecord, UsageSummary};

/// An `O_APPEND` handle on `usage.jsonl`, plus the optional legacy mirror.
#[derive(Debug)]
pub struct UsageWriter {/* C-14 */}

impl UsageWriter {
    /// Open (creating if needed) the usage log and, when configured, the legacy mirror.
    pub fn open(paths: &Paths, cfg: &CompatCfg) -> Result<UsageWriter> {
        todo!("C-14: UsageWriter::open")
    }

    /// One `write()` per row, so a concurrent `tail -f` never sees a torn line.
    pub fn append(&self, rec: &UsageRecord) -> Result<()> {
        todo!("C-14: UsageWriter::append")
    }

    /// Rotate with **copytruncate** semantics, never truncate-on-start.
    pub fn rotate_if_needed(&self, max_mb: u64) -> Result<()> {
        todo!("C-14: UsageWriter::rotate_if_needed")
    }
}

/// Every row we can find: `$STATE/usage.jsonl` plus, when `read_legacy_state`,
/// `~/.vastai-gguf/usage.log`.
pub fn read_all(paths: &Paths, cfg: &CompatCfg) -> Result<Vec<UsageRecord>> {
    todo!("C-14: read_all")
}

/// Aggregate over a real window (`24h`, `7d`, `all`) and a grouping.
pub fn aggregate(rows: &[UsageRecord], since: Option<i64>, by: GroupBy) -> UsageSummary {
    todo!("C-14: aggregate")
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
pub fn parse_lenient_timestamp(s: &str) -> Option<i64> {
    todo!("C-14: parse_lenient_timestamp")
}
