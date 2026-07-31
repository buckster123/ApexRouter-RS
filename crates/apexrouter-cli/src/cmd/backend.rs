//! OWNER: unit S-08 (cli/src/cmd/{vast,tunnel,hf,provider,recipe,profile,usage,smoke,doctor,compare,backend,swap,up,approvals,token,open,env,migrate}.rs). Do not edit outside that unit.
//!
//! `apexrouter backend ls | show | add | enable | disable | drain | probe | rm`.
//!
//! A *backend* is a live upstream in the routing table — the thing a `RouteTarget` selects.
//! It is not an endpoint (a child process we supervise) and not a provider (a credential
//! and a base URL in config); those have their own nouns, and conflating them is how
//! LocalRouter ended up with three menus for one concept.
//!
//! `ls` and `show` answer from `$STATE/backends.json` with no daemon, because "what is in
//! my table" is a fact on disk. Everything that changes routing goes through the daemon,
//! which is the only thing that may recompile a table.
//!
//! `add` is two calls on purpose: `POST /v1/backends` takes a [`NodeSpec`] and returns the
//! id it minted, and tags are a `PATCH` on that id. The server assigns the id (`node-<host>`,
//! uniquified) precisely so two boxes on the same host do not overwrite each other, so the
//! CLI cannot know it in advance.

use crate::cli::{split_list, BackendAddArgs, BackendCmd};
use crate::cmd::Ctx;
use crate::daemon::{Need, Serving};
use crate::render;
use apexrouter_protocol::{Backend, CredentialSource, NodeSpec, Protocol, ServedBy};

/// Run `apexrouter backend …`.
///
/// # Errors
/// A `$STATE` read failure, a daemon that will not answer, or an unknown backend id.
pub async fn run(ctx: &Ctx, cmd: &BackendCmd) -> anyhow::Result<()> {
    match cmd {
        BackendCmd::Ls(args) => {
            let serving = ctx.serving(Need::ReadState).await?;
            let backends = load(ctx, &serving).await?;
            if args.json {
                return render::print_json(
                    serving.served_by(),
                    render::now_unix(),
                    serving.is_offline(),
                    &backends,
                );
            }
            if serving.is_offline() {
                render::print_offline_notice();
            }
            render::print_table(
                &[
                    "BACKEND", "KIND", "HEALTH", "ENABLED", "MODELS", "TAGS", "URL",
                ],
                backends.iter().map(row).collect(),
            );
            Ok(())
        }
        BackendCmd::Show { id, json } => {
            let serving = ctx.serving(Need::ReadState).await?;
            let backends = load(ctx, &serving).await?;
            let b = find(&backends, id)?;
            if *json {
                return render::print_json(
                    serving.served_by(),
                    render::now_unix(),
                    serving.is_offline(),
                    &b,
                );
            }
            if serving.is_offline() {
                render::print_offline_notice();
            }
            print_backend(&b);
            Ok(())
        }
        BackendCmd::Add(args) => add(ctx, args).await,
        BackendCmd::Enable { id } => simple(ctx, id, "enable", "enabled").await,
        BackendCmd::Disable { id } => simple(ctx, id, "disable", "disabled").await,
        BackendCmd::Drain { id } => simple(ctx, id, "drain", "draining").await,
        BackendCmd::Probe { id, json } => {
            let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
            let b: Backend = client
                .post(&format!("/v1/backends/{id}/probe"), &serde_json::json!({}))
                .await?;
            if *json {
                return render::print_json(ServedBy::Daemon, render::now_unix(), false, &b);
            }
            render::print_line(&format!(
                "{}  {}  {} model(s){}",
                b.id.as_str(),
                render::variant(&b.health),
                b.models.len(),
                b.last_error
                    .as_ref()
                    .map(|e| format!("  — {e}"))
                    .unwrap_or_default()
            ));
            Ok(())
        }
        BackendCmd::Rm { id } => {
            let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
            client.delete(&format!("/v1/backends/{id}")).await?;
            render::print_line(&format!("removed {id}"));
            Ok(())
        }
    }
}

/// Every backend: from the daemon when there is one, from `$STATE` when there is not.
///
/// # Errors
/// A `$STATE` read failure, or a daemon that will not answer.
pub async fn load(ctx: &Ctx, serving: &Serving) -> anyhow::Result<Vec<Backend>> {
    match serving {
        Serving::Daemon(c) => Ok(c.get::<Vec<Backend>>("/v1/backends").await?),
        Serving::Offline(store) => Ok(store.with_state_lock_shared(|| store.load_backends())?),
        Serving::None(_) => {
            Ok(apexrouter_core::store::Store::new(ctx.paths.clone()).load_backends()?)
        }
    }
}

/// One backend by id, or a message naming what there is.
///
/// # Errors
/// When no backend has that id.
pub fn find(backends: &[Backend], id: &str) -> anyhow::Result<Backend> {
    backends
        .iter()
        .find(|b| b.id.as_str() == id)
        .cloned()
        .ok_or_else(|| {
            let known: Vec<&str> = backends.iter().map(|b| b.id.as_str()).collect();
            anyhow::anyhow!(
                "no backend `{id}`{}",
                if known.is_empty() {
                    " — the table is empty".to_string()
                } else {
                    format!(" — there is: {}", known.join(", "))
                }
            )
        })
}

/// `apexrouter backend add <url> …`.
///
/// # Errors
/// A daemon that will not answer, or a URL it refuses.
async fn add(ctx: &Ctx, args: &BackendAddArgs) -> anyhow::Result<()> {
    let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
    let spec = NodeSpec {
        base_url: args.url.trim().to_string(),
        credential: credential(args.key_env.as_deref()),
        label: args.label.clone().unwrap_or_default(),
        declared_models: args.models.as_deref().map(split_list).unwrap_or_default(),
        protocol: Protocol::OpenAi,
    };
    let backend: Backend = client.post("/v1/backends", &spec).await?;

    // Tags are a *patch* on the id the server minted (`PATCH /v1/backends/{id}`), and
    // `NodeClient` publishes GET/POST/PUT/DELETE only — there is no `patch` verb to call.
    // Registering and then silently dropping the tags would leave a `tag:` route target
    // matching nothing for a reason nobody could see, so the gap is reported instead.
    if !args.tags.is_empty() {
        render::print_line(&format!(
            "note: tags were NOT applied — `apexrouter-client` has no PATCH verb in this \
             build. Set them with: curl -X PATCH {}/v1/backends/{} -H 'content-type: \
             application/json' -d '{}'",
            client.base(),
            backend.id.as_str(),
            serde_json::json!({ "tags": merged_tags(&backend, &args.tags) })
        ));
    }

    if args.json {
        return render::print_json(ServedBy::Daemon, render::now_unix(), false, &backend);
    }
    render::print_line(&format!(
        "{}  {}  {}",
        backend.id.as_str(),
        backend.base_url,
        render::variant(&backend.health)
    ));
    Ok(())
}

/// The tag set a `PATCH` would install: what the registration produced, plus what the
/// operator asked for, without duplicates and in the order they were written.
fn merged_tags(backend: &Backend, wanted: &[String]) -> Vec<String> {
    let mut tags = backend.tags.clone();
    for t in wanted {
        if !tags.iter().any(|have| have == t) {
            tags.push(t.clone());
        }
    }
    tags
}

/// The four one-word verbs, all `POST /v1/backends/{id}/<verb>`.
///
/// # Errors
/// A daemon that will not answer, or an unknown id.
async fn simple(ctx: &Ctx, id: &str, verb: &str, past: &str) -> anyhow::Result<()> {
    let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
    let _: serde_json::Value = client
        .post(&format!("/v1/backends/{id}/{verb}"), &serde_json::json!({}))
        .await?;
    render::print_line(&format!("{id} {past}"));
    Ok(())
}

/// Where this backend's credential lives — a *description*, never the value (§9.2).
fn credential(key_env: Option<&str>) -> CredentialSource {
    match key_env.map(str::trim).filter(|v| !v.is_empty()) {
        Some(var) => CredentialSource::Env {
            var: var.to_string(),
        },
        None => CredentialSource::None,
    }
}

/// One row of the backend table.
fn row(b: &Backend) -> Vec<String> {
    vec![
        b.id.as_str().to_string(),
        render::variant(&b.kind),
        render::variant(&b.health),
        if b.enabled { "yes" } else { "no" }.to_string(),
        b.models
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>()
            .join(","),
        b.tags.join(","),
        b.base_url.clone(),
    ]
}

/// The detail view.
fn print_backend(b: &Backend) {
    render::print_line(&format!("{}  {}", b.id.as_str(), b.label));
    render::print_line(&format!("  kind      {}", render::variant(&b.kind)));
    render::print_line(&format!("  protocol  {}", b.protocol.as_str()));
    render::print_line(&format!("  url       {}", b.base_url));
    render::print_line(&format!("  health    {}", render::variant(&b.health)));
    render::print_line(&format!(
        "  enabled   {}",
        if b.enabled { "yes" } else { "no" }
    ));
    render::print_line(&format!("  cred      {}", cred_label(&b.credential)));
    render::print_line(&format!(
        "  limits    {} concurrent · queue {}{}",
        b.limits.max_concurrent,
        b.limits.queue_depth,
        b.limits
            .ctx
            .map(|c| format!(" · ctx {c}"))
            .unwrap_or_default()
    ));
    if !b.tags.is_empty() {
        render::print_line(&format!("  tags      {}", b.tags.join(", ")));
    }
    for m in &b.models {
        render::print_line(&format!(
            "  model     {}{}{}{}",
            m.id,
            m.ctx.map(|c| format!(" · ctx {c}")).unwrap_or_default(),
            if m.vision { " · vision" } else { "" },
            if m.tools { " · tools" } else { "" }
        ));
    }
    if !b.devices.is_empty() {
        render::print_line(&format!("  devices   {}", b.devices.join(", ")));
    }
    if let Some(e) = &b.last_error {
        render::print_line(&format!("  error     {e}"));
    }
}

/// A credential's **source**, in one cell. Never the value.
pub fn cred_label(c: &CredentialSource) -> String {
    match c {
        CredentialSource::None => "none".to_string(),
        CredentialSource::Env { var } => format!("env:{var}"),
        CredentialSource::File { path } => format!("file:{path}"),
        CredentialSource::Managed { store } => format!("managed:{store}"),
        CredentialSource::Instance => "instance".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_env_becomes_a_source_description_not_a_value() {
        assert_eq!(
            credential(Some("TOGETHER_API_KEY")),
            CredentialSource::Env {
                var: "TOGETHER_API_KEY".to_string()
            }
        );
        assert_eq!(credential(None), CredentialSource::None);
        assert_eq!(credential(Some("   ")), CredentialSource::None);
    }

    #[test]
    fn a_credential_label_shows_where_it_lives_and_never_what_it_is() {
        assert_eq!(
            cred_label(&CredentialSource::Env {
                var: "K".to_string()
            }),
            "env:K"
        );
        assert_eq!(
            cred_label(&CredentialSource::File {
                path: "/x".to_string()
            }),
            "file:/x"
        );
        assert_eq!(cred_label(&CredentialSource::Instance), "instance");
    }

    #[test]
    fn a_missing_backend_lists_what_the_table_holds() {
        let e = find(&[], "nope").expect_err("must fail");
        assert!(e.to_string().contains("table is empty"), "{e}");
    }
}
