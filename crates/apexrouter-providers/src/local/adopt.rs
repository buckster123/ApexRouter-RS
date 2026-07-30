//! OWNER: unit P-01 (providers/src/local/**). Do not edit outside that unit.
//!
//! Re-adopting children across a daemon restart.
//!
//! `Adoption::Foreign` is **never signalled**. `POST /v1/endpoints/{id}/adopt` exists, but
//! it requires `/props` (or `/v1/models`) to match the spec's model path before it will
//! record `adopted: true`.

use apexrouter_core::error::{Error, Result};
use apexrouter_core::proc::{self, Adoption};
use apexrouter_core::upstream;
use apexrouter_protocol::{EndpointRecord, EndpointSpec, ProcFacts};
use std::time::Duration;

/// Re-derive ownership of one persisted endpoint from its recorded facts.
///
/// This is [`apexrouter_core::proc::adopt`] with the supervisor's rule attached: the answer
/// is only ever consumed through [`signallable`], so an `Adoption` that is not `Adopted`
/// can never reach a `kill(2)`. Identity is pid ∧ `start_time_ticks` ∧ `boot_id` ∧ exe ∧
/// cmdline hash — `boot_id` because start ticks are measured since boot and mean nothing
/// across one, and the cmdline hash because a re-exec with different flags is a different
/// server even on the same pid.
///
/// A record with no `proc` — a `Node` or `Managed` endpoint, which has no process — is
/// [`Adoption::Vanished`]: there is nothing to own.
pub fn adopt(rec: &EndpointRecord) -> Adoption {
    proc::adopt(rec)
}

/// The facts we are allowed to signal, or `None`.
///
/// The single gate every stop path goes through. `Foreign` and `Ambiguous` return `None`
/// because something that is *not ours* holding our port is a report, not a target: the
/// alternative is a supervisor that kills the operator's hand-started `llama-server`
/// because a stale record happened to name its pid.
pub fn signallable(adoption: &Adoption) -> Option<&ProcFacts> {
    match adoption {
        Adoption::Adopted(facts) => Some(facts),
        Adoption::Foreign { .. } | Adoption::Ambiguous { .. } | Adoption::Vanished => None,
    }
}

/// Does the process listening on `base_url` actually serve the model this spec names?
///
/// The gate on `POST /v1/endpoints/{id}/adopt`. `/props` carries the loaded model's path
/// and is checked first; a build started without `--props` falls back to `/v1/models`,
/// where the `-a` alias has to appear. Anything else is `false` — adopting the wrong
/// process is worse than refusing to adopt the right one.
///
/// # Errors
/// Returns [`Error::Invalid`] when the spec is not one this supervisor owns.
pub async fn verify_serving(
    http: &reqwest::Client,
    base_url: &str,
    spec: &EndpointSpec,
    timeout: Duration,
) -> Result<bool> {
    let (model_path, alias) = match spec {
        EndpointSpec::LocalLlama(s) => (Some(s.model_path.clone()), s.alias_flag.clone()),
        EndpointSpec::LocalVllm(s) => (None, s.model_id.clone()),
        other => {
            return Err(Error::Invalid {
                what: "endpoint spec".to_owned(),
                why: format!(
                    "{:?} is not a local endpoint; the local supervisor cannot adopt it",
                    other.kind()
                ),
            })
        }
    };

    let probe = upstream::probe(http, base_url, None, timeout).await;
    if !probe.healthy {
        return Ok(false);
    }

    if let (Some(want), Some(got)) = (model_path.as_deref(), probe.model_path.as_deref()) {
        if same_model_file(want, got) {
            return Ok(true);
        }
    }
    if !alias.is_empty() && probe.models.iter().any(|m| m.id == alias) {
        return Ok(true);
    }
    Ok(false)
}

/// Compare two model paths the way an operator would.
///
/// llama.cpp reports the path it was given, which may be relative to a cwd we do not share,
/// and a moved weights directory changes the prefix without changing the file. So the full
/// paths are compared first and the file names second; a bare file-name match is enough,
/// because two different GGUFs with the same name is a self-inflicted problem while
/// refusing to adopt a correctly-running server leaks 6 GB of VRAM until somebody notices.
fn same_model_file(want: &str, got: &str) -> bool {
    if want.is_empty() || got.is_empty() {
        return false;
    }
    if want == got {
        return true;
    }
    let name = |p: &str| p.rsplit('/').next().unwrap_or(p).to_owned();
    name(want) == name(got)
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_protocol::{
        BackendId, CredentialSource, DesiredState, LocalLlamaSpec, NglPlan, NodeSpec, Protocol,
        SamplingMode, SplitPlan,
    };

    fn spec() -> EndpointSpec {
        EndpointSpec::LocalLlama(LocalLlamaSpec {
            build: "build-vulkan".parse().expect("build id"),
            model_path: "/home/andre/models/carnice-9b/Carnice-9b-Q6_K.gguf".to_owned(),
            mmproj: None,
            alias_flag: "carnice-9b".to_owned(),
            host: "127.0.0.1".to_owned(),
            port: Some(8100),
            ctx: None,
            parallel: None,
            kv_type: None,
            ngl: NglPlan::Auto,
            split: SplitPlan::default(),
            mode: SamplingMode::Raw,
            flash_attn: None,
            api_key: None,
            extra_args: Vec::new(),
        })
    }

    fn record(facts: Option<ProcFacts>) -> EndpointRecord {
        EndpointRecord {
            id: BackendId::parse("local-carnice").expect("id"),
            spec: spec(),
            desired: DesiredState::Running,
            proc: facts,
            port: Some(8100),
            log_path: None,
            started_at_unix: 0,
            fit: None,
            adopted: false,
            alias_bindings: Vec::new(),
        }
    }

    /// Identity for this test binary, captured the way a spawn would capture it.
    fn own_facts() -> ProcFacts {
        let pid = std::process::id();
        let argv = proc::cmdline(pid).expect("cmdline");
        proc::identify(pid, &argv, "").expect("identify self")
    }

    #[test]
    fn a_record_with_no_process_has_nothing_to_adopt() {
        assert!(matches!(adopt(&record(None)), Adoption::Vanished));
    }

    #[test]
    fn a_matching_identity_is_adopted_and_may_be_signalled() {
        let outcome = adopt(&record(Some(own_facts())));
        assert!(
            matches!(outcome, Adoption::Adopted(_)),
            "identical facts must adopt, got {outcome:?}"
        );
        assert!(signallable(&outcome).is_some());
    }

    #[test]
    fn a_start_tick_mismatch_is_never_adopted_and_is_never_signalled() {
        let mut facts = own_facts();
        // The pid is real and alive; the process that owned these ticks is not this one.
        facts.start_time_ticks = facts.start_time_ticks.wrapping_add(1);
        let outcome = adopt(&record(Some(facts)));
        assert!(
            !matches!(outcome, Adoption::Adopted(_)),
            "a start-tick mismatch must never adopt, got {outcome:?}"
        );
        assert!(signallable(&outcome).is_none());
    }

    #[test]
    fn a_reboot_invalidates_every_recorded_identity() {
        let mut facts = own_facts();
        facts.boot_id = "00000000-0000-0000-0000-000000000000".to_owned();
        let outcome = adopt(&record(Some(facts)));
        assert!(!matches!(outcome, Adoption::Adopted(_)));
        assert!(signallable(&outcome).is_none());
    }

    #[test]
    fn foreign_and_ambiguous_are_never_signallable() {
        for a in [
            Adoption::Foreign {
                pid: 4242,
                why: "someone else's llama-server".to_owned(),
            },
            Adoption::Ambiguous {
                pid: 4242,
                why: "hidepid".to_owned(),
            },
            Adoption::Vanished,
        ] {
            assert!(signallable(&a).is_none(), "{a:?} must not be signalled");
        }
    }

    #[tokio::test]
    async fn a_non_local_spec_cannot_be_adopted_by_the_local_supervisor() {
        let node = EndpointSpec::Node(NodeSpec {
            base_url: "http://127.0.0.1:1".to_owned(),
            credential: CredentialSource::None,
            label: "lan".to_owned(),
            declared_models: Vec::new(),
            protocol: Protocol::OpenAi,
        });
        let http = reqwest::Client::new();
        let err = verify_serving(
            &http,
            "http://127.0.0.1:1",
            &node,
            Duration::from_millis(50),
        )
        .await
        .expect_err("must refuse");
        assert!(matches!(err, Error::Invalid { .. }));
    }

    #[tokio::test]
    async fn an_unreachable_upstream_is_not_adopted() {
        let http = reqwest::Client::new();
        // Port 1 is reserved and never listening.
        let ok = verify_serving(
            &http,
            "http://127.0.0.1:1",
            &spec(),
            Duration::from_millis(200),
        )
        .await
        .expect("no error");
        assert!(!ok);
    }

    #[test]
    fn model_paths_match_on_the_file_name_but_not_across_different_files() {
        assert!(same_model_file("/a/b/m.gguf", "/a/b/m.gguf"));
        assert!(same_model_file("/a/b/m.gguf", "/mnt/weights/m.gguf"));
        assert!(!same_model_file("/a/b/m.gguf", "/a/b/other.gguf"));
        assert!(!same_model_file("", "/a/b/m.gguf"));
    }
}
