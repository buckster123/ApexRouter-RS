//! OWNER: unit S-08 (cli/src/cmd/{vast,tunnel,hf,provider,recipe,profile,usage,smoke,doctor,compare,backend,swap,up,approvals,token,open,env,migrate}.rs). Do not edit outside that unit.
//!
//! `apexrouter provider ls | show | set | test | models`.
//!
//! # The key never travels except when a human typed it
//!
//! `--key-env` and `--key-file` are *references*: they name where the credential lives and
//! nothing else crosses the wire, which is why importing a legacy config never copies a
//! secret. `--key-stdin` is the one path that carries key material, and it exists because
//! §9.2 says a key the user **types** may be written — to `$STATE/credentials.toml` at
//! `0600`, never to `config.toml`.
//!
//! `--key-stdin` reads stdin, not an argument, deliberately: a key on the command line is
//! in `/proc/*/cmdline` and in the shell history of every operator who ever pastes it.
//! Nothing here ever prints a key back; `ls` and `show` print the credential's **source**.
//!
//! Every verb is `Mutate`. `GET /v1/providers` resolves the real credential chain, so it is
//! the daemon's job — and the hermeticity guard exists because a test that calls it against
//! a real config would reach a paid endpoint.

use crate::cli::{ProviderCmd, ProviderSetArgs};
use crate::cmd::{backend, Ctx};
use crate::daemon::Need;
use crate::render;
use apexrouter_protocol::{CheckResult, ProviderStatus, ServedBy, UpstreamModel};
use std::io::Read;

/// Run `apexrouter provider …`.
///
/// # Errors
/// A daemon that will not answer, an unknown provider id, or an empty `--key-stdin`.
pub async fn run(ctx: &Ctx, cmd: &ProviderCmd) -> anyhow::Result<()> {
    let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
    match cmd {
        ProviderCmd::Ls(args) => {
            let rows: Vec<ProviderStatus> = client.get("/v1/providers").await?;
            if args.json {
                return render::print_json(ServedBy::Daemon, render::now_unix(), false, &rows);
            }
            render::print_table(
                &[
                    "PROVIDER",
                    "CREDENTIAL",
                    "PRESENT",
                    "MODELS",
                    "BASE URL",
                    "LAST ERROR",
                ],
                rows.iter().map(row).collect(),
            );
            Ok(())
        }
        ProviderCmd::Show { id, json } => {
            let p: ProviderStatus = client.get(&format!("/v1/providers/{id}")).await?;
            if *json {
                return render::print_json(ServedBy::Daemon, render::now_unix(), false, &p);
            }
            print_provider(&p);
            Ok(())
        }
        ProviderCmd::Set(args) => {
            let patch = patch(args)?;
            let after: ProviderStatus = client
                .put(&format!("/v1/providers/{}", args.id), &patch)
                .await?;
            if args.json {
                return render::print_json(ServedBy::Daemon, render::now_unix(), false, &after);
            }
            print_provider(&after);
            Ok(())
        }
        ProviderCmd::Test {
            id,
            completion,
            model,
            json,
        } => {
            let mut query: Vec<String> = Vec::new();
            if *completion {
                query.push("completion=1".to_string());
            }
            if let Some(m) = model {
                query.push(format!("model={m}"));
            }
            let path = match query.is_empty() {
                true => format!("/v1/providers/{id}/test"),
                false => format!("/v1/providers/{id}/test?{}", query.join("&")),
            };
            let results: Vec<CheckResult> = client.post(&path, &serde_json::json!({})).await?;
            if *json {
                return render::print_json(ServedBy::Daemon, render::now_unix(), false, &results);
            }
            crate::cmd::doctor::print_results(&results);
            Ok(())
        }
        ProviderCmd::Models { id, org, json } => {
            let all: Vec<UpstreamModel> = client.get(&format!("/v1/providers/{id}/models")).await?;
            let rows = filter_org(&all, org.as_deref());
            if *json {
                return render::print_json(ServedBy::Daemon, render::now_unix(), false, &rows);
            }
            print_models(&rows);
            Ok(())
        }
    }
}

/// Turn `provider set` flags into the `PUT /v1/providers/{id}` body.
///
/// The four fields are not mutually exclusive on the wire — a GUI setting a key and a base
/// URL in one save should not need two calls — but they land in two different files, and
/// only the ones present are written.
///
/// # Errors
/// An empty `--key-stdin`, or stdin that cannot be read.
fn patch(args: &ProviderSetArgs) -> anyhow::Result<serde_json::Value> {
    let mut body = serde_json::Map::new();
    if let Some(u) = &args.base_url {
        body.insert("base_url".into(), serde_json::Value::from(u.trim()));
    }
    if let Some(v) = &args.key_env {
        body.insert("api_key_env".into(), serde_json::Value::from(v.trim()));
    }
    if let Some(p) = &args.key_file {
        body.insert(
            "api_key_file".into(),
            serde_json::Value::from(p.display().to_string()),
        );
    }
    if args.key_stdin {
        body.insert("api_key".into(), serde_json::Value::from(read_key()?));
    }
    if body.is_empty() {
        anyhow::bail!("nothing to set — pass --base-url, --key-env, --key-file or --key-stdin");
    }
    Ok(serde_json::Value::Object(body))
}

/// Read a key from stdin, trimming the newline a `echo`/heredoc leaves behind.
///
/// Never an argument: a key in argv is in `/proc/*/cmdline` for anything on the box to read.
///
/// # Errors
/// An unreadable stdin, or an empty key — which would otherwise store a credential that
/// authenticates as nobody and fails at the least convenient moment.
fn read_key() -> anyhow::Result<String> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    let key = buf.trim().to_string();
    if key.is_empty() {
        anyhow::bail!("--key-stdin read an empty key from stdin");
    }
    Ok(key)
}

/// Keep only the models whose id is in one org — the `owner/model` prefix HuggingFace-style
/// ids carry, matched case-insensitively because operators type what they see.
pub fn filter_org(models: &[UpstreamModel], org: Option<&str>) -> Vec<UpstreamModel> {
    let Some(org) = org.map(str::trim).filter(|o| !o.is_empty()) else {
        return models.to_vec();
    };
    let want = org.to_lowercase();
    models
        .iter()
        .filter(|m| {
            m.id.split_once('/')
                .map(|(o, _)| o.to_lowercase() == want)
                .unwrap_or(false)
        })
        .cloned()
        .collect()
}

/// One row of the provider table. The credential's **source**, never its value.
fn row(p: &ProviderStatus) -> Vec<String> {
    vec![
        p.id.as_str().to_string(),
        backend::cred_label(&p.credential),
        if p.credential_present { "yes" } else { "no" }.to_string(),
        p.models_cached.to_string(),
        p.base_url.clone(),
        p.last_error.clone().unwrap_or_default(),
    ]
}

/// The detail view.
fn print_provider(p: &ProviderStatus) {
    render::print_line(p.id.as_str());
    render::print_line(&format!("  base url  {}", p.base_url));
    render::print_line(&format!(
        "  cred      {} ({})",
        backend::cred_label(&p.credential),
        if p.credential_present {
            "present"
        } else {
            "ABSENT"
        }
    ));
    render::print_line(&format!("  models    {} cached", p.models_cached));
    if let Some(t) = p.last_ok_unix {
        render::print_line(&format!(
            "  last ok   {} ago",
            render::human_secs(render::now_unix() - t)
        ));
    }
    if let Some(r) = &p.rate_limit {
        render::print_line(&format!(
            "  ratelimit {}/{}{}",
            render::dash(r.remaining),
            render::dash(r.limit),
            r.reset_unix
                .map(|t| format!(
                    " · resets in {}",
                    render::human_secs(t - render::now_unix())
                ))
                .unwrap_or_default()
        ));
    }
    if let Some(e) = &p.last_error {
        render::print_line(&format!("  error     {e}"));
    }
}

/// The catalogue, grouped by org so a 200-model list is readable.
fn print_models(models: &[UpstreamModel]) {
    let mut current = String::new();
    let mut sorted = models.to_vec();
    sorted.sort_by(|a, b| a.id.cmp(&b.id));
    for m in &sorted {
        let org = m.id.split_once('/').map(|(o, _)| o).unwrap_or("");
        if org != current {
            current = org.to_string();
            render::print_blank();
            render::print_line(if current.is_empty() {
                "(no org)"
            } else {
                &current
            });
        }
        render::print_line(&format!(
            "  {}{}{}{}",
            m.id,
            m.ctx.map(|c| format!("  ctx {c}")).unwrap_or_default(),
            if m.vision { "  vision" } else { "" },
            if m.tools { "  tools" } else { "" }
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn set_args() -> ProviderSetArgs {
        ProviderSetArgs {
            id: "together".to_string(),
            base_url: None,
            key_env: None,
            key_file: None,
            key_stdin: false,
            json: false,
        }
    }

    fn model(id: &str) -> UpstreamModel {
        UpstreamModel {
            id: id.to_string(),
            ctx: None,
            vision: false,
            tools: false,
        }
    }

    #[test]
    fn a_key_reference_travels_as_a_reference_and_never_as_a_value() {
        let mut a = set_args();
        a.key_env = Some("TOGETHER_API_KEY".to_string());
        a.key_file = Some(PathBuf::from("/home/andre/.config/vastai/vast_api_key"));
        let body = patch(&a).expect("patch");
        assert_eq!(body["api_key_env"], "TOGETHER_API_KEY");
        assert_eq!(
            body["api_key_file"],
            "/home/andre/.config/vastai/vast_api_key"
        );
        assert!(
            body.get("api_key").is_none(),
            "no key material without --key-stdin: {body}"
        );
    }

    #[test]
    fn a_set_with_no_fields_says_what_to_pass_rather_than_writing_an_empty_patch() {
        let e = patch(&set_args()).expect_err("must fail");
        assert!(e.to_string().contains("--key-stdin"), "{e}");
    }

    #[test]
    fn the_base_url_is_trimmed_but_never_rewritten() {
        let mut a = set_args();
        // `api.together.xyz` is a legacy spelling and stays exactly that.
        a.base_url = Some("  https://api.together.xyz  ".to_string());
        let body = patch(&a).expect("patch");
        assert_eq!(body["base_url"], "https://api.together.xyz");
    }

    #[test]
    fn org_filtering_matches_the_prefix_case_insensitively() {
        let all = vec![
            model("Qwen/Qwen3-32B"),
            model("meta-llama/Llama-3.3-70B"),
            model("bare-id"),
        ];
        assert_eq!(filter_org(&all, Some("qwen")).len(), 1);
        assert_eq!(filter_org(&all, Some("QWEN"))[0].id, "Qwen/Qwen3-32B");
        assert_eq!(filter_org(&all, None).len(), 3);
        assert_eq!(filter_org(&all, Some("  ")).len(), 3);
        assert!(filter_org(&all, Some("nobody")).is_empty());
    }
}
