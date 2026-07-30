//! The check registry's wire types. One `trait Check` in `apexrouter_core::checks` backs
//! `doctor`, `diagnose` and the four native smoke probes; these are what it emits.
//!
//! Checks run **concurrently** with per-check timeouts and stream as each lands, so
//! `diagnose --only rate-limits` is instant instead of waiting through four sequential SSH
//! probes.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A dotted check id: `"creds.vast"`, `"ports.proxy"`, `"smoke.throughput"`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CheckId(pub String);

impl CheckId {
    /// Borrow the id.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CheckId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for CheckId {
    fn from(s: &str) -> Self {
        CheckId(s.to_owned())
    }
}

impl From<String> for CheckId {
    fn from(s: String) -> Self {
        CheckId(s)
    }
}

/// What one check found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    /// Which check.
    pub id: CheckId,
    /// Human label for the row.
    pub label: String,
    /// Pass, warn, fail or skipped.
    pub status: CheckStatus,
    /// How long it took.
    pub ms: u32,
    /// What it found, in words.
    pub detail: String,
    /// What to do about it. An actionable line, never prose.
    #[serde(default)]
    pub fix: Option<String>,
}

/// Check outcome. A check that panics yields `Fail`; it never poisons the run.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// Fine.
    Pass,
    /// Works, but worth knowing about.
    Warn,
    /// Broken.
    Fail,
    /// Not applicable here (offline, no credential, wrong machine).
    Skipped,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_id_is_a_bare_string_on_the_wire() {
        let id = CheckId::from("creds.vast");
        assert_eq!(serde_json::to_string(&id).expect("ser"), "\"creds.vast\"");
        assert_eq!(
            serde_json::from_str::<CheckId>("\"creds.vast\"").expect("de"),
            id
        );
        assert_eq!(id.to_string(), "creds.vast");
        assert_eq!(id.as_str(), "creds.vast");
    }

    #[test]
    fn check_result_round_trips_every_status() {
        for status in [
            CheckStatus::Pass,
            CheckStatus::Warn,
            CheckStatus::Fail,
            CheckStatus::Skipped,
        ] {
            let r = CheckResult {
                id: CheckId::from("ports.proxy"),
                label: "proxy port 8888 is free or ours".into(),
                status,
                ms: 3,
                detail: "bound by apexrouter".into(),
                fix: Some("stop endpoint_proxy.py first".into()),
            };
            let s = serde_json::to_string(&r).expect("ser");
            assert_eq!(serde_json::from_str::<CheckResult>(&s).expect("de"), r);
        }
        assert_eq!(
            serde_json::to_string(&CheckStatus::Skipped).expect("ser"),
            "\"skipped\""
        );
    }
}
