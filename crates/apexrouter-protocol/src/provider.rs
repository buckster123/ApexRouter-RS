//! Managed providers, as the control plane reports them.
//!
//! `GET /v1/providers` returns `{source: "env:TOGETHER_API_KEY", present: true}` — the
//! **source**, never the value. That is the whole point of this module's shape.

use crate::backend::CredentialSource;
use crate::ids::ProviderId;
use serde::{Deserialize, Serialize};

/// One configured provider and what we know about its credential and catalogue.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderStatus {
    /// `together`, `vast-gguf`, …
    pub id: ProviderId,
    /// INVARIANT: stored without a trailing `/v1`. A legacy `api.together.xyz` is used
    /// as-is and never rewritten to `.ai`.
    pub base_url: String,
    /// **Where** the credential lives. Never the credential.
    pub credential: CredentialSource,
    /// Whether resolving that source actually produced something.
    pub credential_present: bool,
    /// How many models we have cached for it.
    pub models_cached: u32,
    /// Last successful call, unix seconds.
    #[serde(default)]
    pub last_ok_unix: Option<i64>,
    /// Last failure, in words.
    #[serde(default)]
    pub last_error: Option<String>,
    /// Rate-limit headers, when the provider sends any.
    #[serde(default)]
    pub rate_limit: Option<RateLimitInfo>,
}

/// Rate-limit state read from response headers.
///
/// `remaining` is **not** relied upon for decisions — providers disagree about whether it
/// counts requests or tokens. `reset_unix` is what a backoff actually uses.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimitInfo {
    /// The ceiling, when advertised.
    #[serde(default)]
    pub limit: Option<u64>,
    /// What is left, when advertised.
    #[serde(default)]
    pub remaining: Option<u64>,
    /// When the window resets, unix seconds.
    #[serde(default)]
    pub reset_unix: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_status_round_trips_and_names_the_source_not_the_key() {
        let p = ProviderStatus {
            id: ProviderId::parse("together").expect("id"),
            base_url: "https://api.together.xyz".into(),
            credential: CredentialSource::Env {
                var: "TOGETHER_API_KEY".into(),
            },
            credential_present: true,
            models_cached: 118,
            last_ok_unix: Some(1_785_412_331),
            last_error: None,
            rate_limit: Some(RateLimitInfo {
                limit: Some(600),
                remaining: Some(599),
                reset_unix: Some(1_785_412_391),
            }),
        };
        let s = serde_json::to_string(&p).expect("ser");
        assert!(
            s.contains("TOGETHER_API_KEY"),
            "the variable NAME is public"
        );
        assert_eq!(serde_json::from_str::<ProviderStatus>(&s).expect("de"), p);
    }

    #[test]
    fn rate_limit_info_defaults_to_all_unknown() {
        let r: RateLimitInfo = serde_json::from_str("{}").expect("de");
        assert_eq!(r, RateLimitInfo::default());
    }
}
