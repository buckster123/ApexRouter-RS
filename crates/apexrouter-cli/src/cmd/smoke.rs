//! OWNER: unit S-08 (cli/src/cmd/{vast,tunnel,hf,provider,recipe,profile,usage,smoke,doctor,compare,backend,swap,up,approvals,token,open,env,migrate}.rs). Do not edit outside that unit.
//!
//! `apexrouter smoke [--alias A | --base-url URL] [--json]` — four probes with TTFT and tok/s read from the `timings` object.
//!
//! `smoke.sh`, reimplemented natively, with the two improvements P-08 built into
//! `providers::smoke`: TTFT and tok/s come from the upstream's **`timings` object**, not a
//! stopwatch, and the probe sends **the resolved route's model id** rather than the
//! hardcoded `"model":"x"` that 400s on every managed provider.
//!
//! # Where the probes run
//!
//! In this process. `POST /v1/smoke` answers with SSE and `NodeClient` has no POST-SSE verb
//! (nor a way to decode a bare `CheckResult` frame), so driving the daemon's copy from here
//! would mean re-implementing an HTTP client to reach a re-implementation of four probes
//! this binary already links. The daemon is still asked the one question only it can answer
//! — *what does this alias resolve to right now?* — and the probes then run against that
//! answer.
//!
//! A `--base-url` needs no daemon at all, which is the case an operator is usually in when
//! something is wrong.

use crate::cli::SmokeArgs;
use crate::cmd::{route, Ctx};
use crate::daemon::Need;
use crate::render;
use apexrouter_core::secret::resolve_provider;
use apexrouter_protocol::{Backend, ModelRoute, ProviderId, ServedBy, SmokeProbe};

/// Run `apexrouter smoke`.
///
/// # Errors
/// Neither target given, an alias with no route, or a daemon that will not answer. A
/// *failing probe* is not an error: it is the answer, and it is on stdout.
pub async fn run(ctx: &Ctx, args: &SmokeArgs) -> anyhow::Result<()> {
    let target = resolve(ctx, args).await?;
    render::print_line(&format!("{}  {}", target.label(), target.base_url));

    // The registry documents a dropped receiver as normal: the probes are rendered from the
    // returned Vec, in run order, which is the order that makes `warmup` before
    // `throughput` meaningful.
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    drop(rx);
    let http = Default::default();
    let probes = apexrouter_providers::smoke::run(&http, &target, tx).await;

    if args.json {
        return render::print_json(ServedBy::Offline, render::now_unix(), false, &probes);
    }
    print_probes(&probes);
    Ok(())
}

/// Turn `--alias`/`--base-url` into a concrete upstream, model id and credential.
///
/// An alias is resolved through the daemon, because the routing table is the only thing
/// that knows which backend an alias points at *now*; a base URL is used verbatim (with
/// `/v1` stripped, since `join_v1` puts exactly one back on).
///
/// # Errors
/// Neither flag given, an invalid alias, an alias with no targets, or a daemon that will
/// not answer.
async fn resolve(
    ctx: &Ctx,
    args: &SmokeArgs,
) -> anyhow::Result<apexrouter_providers::smoke::SmokeTarget> {
    use apexrouter_providers::smoke::SmokeTarget;

    if let Some(url) = args.base_url.as_deref() {
        let url = url.trim();
        if url.is_empty() {
            anyhow::bail!("--base-url must not be empty");
        }
        return Ok(SmokeTarget::new(
            strip_v1(url),
            args.model.clone().unwrap_or_default(),
        ));
    }

    let Some(alias) = args.alias.as_deref() else {
        anyhow::bail!("one of --alias or --base-url is required");
    };
    let alias = route::parse_alias(alias)?;
    let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
    let routes: Vec<ModelRoute> = client.get("/v1/routes").await?;
    let backends: Vec<Backend> = client.get("/v1/backends").await?;

    let (b, model) = first_target(&routes, &backends, alias.as_str())?;
    let model = args.model.clone().unwrap_or(model);
    // A managed backend's id **is** its provider id, so the credential chain resolves
    // without the operator naming the provider. The secret never leaves this process.
    let cred = ProviderId::parse(b.id.as_str())
        .ok()
        .and_then(|p| resolve_provider(&ctx.cfg, &ctx.paths, &p).ok().flatten())
        .map(|r| r.secret);
    Ok(SmokeTarget::new(strip_v1(&b.base_url), model).with_cred(cred))
}

/// The first routable target of an alias, as `(backend, model id)`.
///
/// "First" rather than "whatever the strategy would pick": a smoke test is a question about
/// a specific upstream, and a round-robin answer that differs between two runs is not one.
///
/// # Errors
/// An unknown alias, an alias with no targets, or a target naming a backend that is not in
/// the table — each with what *is* there in the message.
pub fn first_target(
    routes: &[ModelRoute],
    backends: &[Backend],
    alias: &str,
) -> anyhow::Result<(Backend, String)> {
    let route = routes
        .iter()
        .find(|r| r.alias.as_str() == alias)
        .ok_or_else(|| {
            let known: Vec<&str> = routes.iter().map(|r| r.alias.as_str()).collect();
            anyhow::anyhow!(
                "no route named `{alias}`{}",
                if known.is_empty() {
                    String::new()
                } else {
                    format!(" — there is: {}", known.join(", "))
                }
            )
        })?;

    for t in &route.targets {
        let Some(b) = select(backends, t) else {
            continue;
        };
        let model = t
            .model
            .clone()
            .or_else(|| b.models.first().map(|m| m.id.clone()))
            .unwrap_or_default();
        return Ok((b, model));
    }
    anyhow::bail!(
        "`{alias}` has no target that names a backend in the table — \
         `apexrouter route show {alias}` and `apexrouter backend ls` show both sides"
    )
}

/// The backend one target selects, by id, tag or glob.
fn select(backends: &[Backend], t: &apexrouter_protocol::RouteTarget) -> Option<Backend> {
    use apexrouter_protocol::BackendSelector;
    match &t.backend {
        BackendSelector::Id(id) => backends.iter().find(|b| b.id == *id).cloned(),
        BackendSelector::Tag(tag) => backends
            .iter()
            .find(|b| b.tags.iter().any(|have| have == tag))
            .cloned(),
        BackendSelector::Glob(pat) => backends
            .iter()
            .find(|b| glob_matches(pat, b.id.as_str()))
            .cloned(),
    }
}

/// `*`-only glob matching, which is the whole of the documented pattern language.
fn glob_matches(pattern: &str, s: &str) -> bool {
    let mut at = 0usize;
    let parts: Vec<&str> = pattern.split('*').collect();
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        match s[at..].find(part) {
            Some(pos) => {
                // A leading literal must match at the start; a trailing one at the end.
                if i == 0 && pos != 0 {
                    return false;
                }
                at += pos + part.len();
            }
            None => return false,
        }
    }
    if !pattern.ends_with('*') {
        if let Some(last) = parts.last().filter(|p| !p.is_empty()) {
            return s.ends_with(last);
        }
    }
    true
}

/// Strip a trailing `/v1` (and any trailing slash) — the base-URL invariant, kept here so
/// the CLI and `POST /v1/smoke` normalise a pasted URL identically.
pub fn strip_v1(url: &str) -> String {
    let mut s = url.trim().trim_end_matches('/');
    while let Some(stripped) = s.strip_suffix("/v1") {
        s = stripped.trim_end_matches('/');
    }
    s.to_string()
}

/// The four probes as a table. An upstream that publishes no `timings` shows `-` for TTFT
/// and tok/s — the honest answer, since wall-clock division across a network path is not a
/// throughput measurement.
fn print_probes(probes: &[SmokeProbe]) {
    render::print_table(
        &["PROBE", "OK", "MS", "TTFT", "TOK/S", "TOKENS", "DETAIL"],
        probes
            .iter()
            .map(|p| {
                vec![
                    p.name.clone(),
                    if p.ok { "pass" } else { "FAIL" }.to_string(),
                    p.ms.to_string(),
                    render::dash(p.ttft_ms),
                    p.tok_per_s.map(|v| format!("{v:.2}")).unwrap_or_default(),
                    render::dash(p.tokens),
                    p.detail.clone(),
                ]
            })
            .collect(),
    );
    let failed = probes.iter().filter(|p| !p.ok).count();
    render::print_blank();
    render::print_line(&format!(
        "{}/{} probes passed",
        probes.len() - failed,
        probes.len()
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_protocol::{
        Alias, BackendId, BackendKind, BackendLimits, BackendSelector, CredentialSource, Health,
        Protocol, Provenance, RetryPolicy, RouteFilter, RouteTarget, Strategy, UpstreamModel,
    };

    fn backend(id: &str, tags: Vec<&str>) -> Backend {
        Backend {
            id: BackendId::parse(id).expect("id"),
            kind: BackendKind::LocalLlama,
            protocol: Protocol::OpenAi,
            label: id.to_string(),
            base_url: format!("http://127.0.0.1:8100/{id}"),
            credential: CredentialSource::None,
            tags: tags.into_iter().map(str::to_string).collect(),
            models: vec![UpstreamModel {
                id: "Carnice-9b".to_string(),
                ctx: None,
                vision: false,
                tools: false,
            }],
            limits: BackendLimits::default(),
            price: None,
            health: Health::Unknown,
            provenance: Provenance::Manual,
            endpoint: None,
            enabled: true,
            devices: Vec::new(),
            last_error: None,
        }
    }

    fn route(alias: &str, targets: Vec<RouteTarget>) -> ModelRoute {
        ModelRoute {
            alias: Alias::parse(alias).expect("alias"),
            targets,
            strategy: Strategy::FirstHealthy,
            filter: RouteFilter::default(),
            retry: RetryPolicy::default(),
            is_default: false,
            description: None,
        }
    }

    fn target(sel: BackendSelector, model: Option<&str>) -> RouteTarget {
        RouteTarget {
            backend: sel,
            model: model.map(str::to_string),
            weight: 1,
        }
    }

    #[test]
    fn a_pasted_url_is_normalised_the_same_way_the_daemon_normalises_it() {
        assert_eq!(
            strip_v1("http://127.0.0.1:8100/v1"),
            "http://127.0.0.1:8100"
        );
        assert_eq!(
            strip_v1("http://127.0.0.1:8100/v1/"),
            "http://127.0.0.1:8100"
        );
        assert_eq!(strip_v1("  http://x/  "), "http://x");
        assert_eq!(strip_v1("http://x/v1/v1"), "http://x");
    }

    #[test]
    fn an_alias_resolves_to_its_first_target_and_that_target_s_model() {
        let backends = vec![backend("local-carnice", vec!["local"])];
        let routes = vec![route(
            "auto",
            vec![target(
                BackendSelector::Id(BackendId::parse("local-carnice").expect("id")),
                Some("explicit-model"),
            )],
        )];
        let (b, m) = first_target(&routes, &backends, "auto").expect("resolved");
        assert_eq!(b.id.as_str(), "local-carnice");
        assert_eq!(m, "explicit-model");
    }

    #[test]
    fn a_target_with_no_model_borrows_the_backend_s_first_one_never_the_string_x() {
        let backends = vec![backend("local-carnice", vec!["local"])];
        let routes = vec![route(
            "auto",
            vec![target(
                BackendSelector::Id(BackendId::parse("local-carnice").expect("id")),
                None,
            )],
        )];
        let (_, m) = first_target(&routes, &backends, "auto").expect("resolved");
        assert_eq!(m, "Carnice-9b");
        assert_ne!(m, "x", "smoke.sh's hardcoded model is what this replaces");
    }

    #[test]
    fn a_dead_target_is_skipped_so_a_stale_chain_still_smokes_something() {
        let backends = vec![backend("second", vec!["local"])];
        let routes = vec![route(
            "auto",
            vec![
                target(
                    BackendSelector::Id(BackendId::parse("gone").expect("id")),
                    None,
                ),
                target(BackendSelector::Tag("local".to_string()), None),
            ],
        )];
        let (b, _) = first_target(&routes, &backends, "auto").expect("resolved");
        assert_eq!(b.id.as_str(), "second");
    }

    #[test]
    fn an_alias_with_nothing_behind_it_names_both_sides_of_the_problem() {
        let e = first_target(&[route("auto", Vec::new())], &[], "auto").expect_err("must fail");
        assert!(e.to_string().contains("backend ls"), "{e}");
        let e = first_target(&[], &[], "nope").expect_err("must fail");
        assert!(e.to_string().contains("no route named"), "{e}");
    }

    #[test]
    fn globs_match_the_way_the_route_targets_document() {
        assert!(glob_matches("vast-*", "vast-12345"));
        assert!(!glob_matches("vast-*", "local-vast-12345"));
        assert!(glob_matches("*-carnice", "local-carnice"));
        assert!(glob_matches("*", "anything"));
        assert!(!glob_matches("*-carnice", "local-qwen"));
    }
}
