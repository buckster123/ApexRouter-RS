//! OWNER: unit S-06 (cli/src/{main,cli,daemon,render}.rs, cli/src/cmd/{mod,status,serve,config,rig,models,fit,endpoint,route,switch,url,version,completions}.rs). Do not edit outside that unit.
//!
//! `apexrouter models ls | show <id>`.
//!
//! Also the home of [`resolve_model`], the **documented** positional resolution order that
//! `fit` and `endpoint start` share: exact id → exact name → case-insensitive exact →
//! unique case-insensitive prefix → path on disk. An ambiguous prefix lists its candidates
//! rather than guessing.

use crate::cli::ModelsCmd;
use crate::cmd::Ctx;
use crate::daemon::{Need, Serving};
use crate::render;
use apexrouter_core::discover;
use apexrouter_protocol::{LocalModel, ModelShard};
use std::path::Path;

/// Run `apexrouter models …`.
///
/// # Errors
/// Discovery failure, a daemon that will not answer, or an unresolvable model.
pub async fn run(ctx: &Ctx, cmd: &ModelsCmd) -> anyhow::Result<()> {
    let serving = ctx.serving(Need::ReadState).await?;
    let served_by = serving.served_by();
    let models = load(ctx, &serving).await?;

    match cmd {
        ModelsCmd::Ls(args) => {
            if args.json {
                return render::print_json(served_by, render::now_unix(), false, &models);
            }
            if serving.is_offline() {
                render::print_offline_notice();
            }
            let rows = models
                .iter()
                .map(|m| {
                    vec![
                        m.id.clone(),
                        m.name.clone(),
                        m.quant.clone().unwrap_or_default(),
                        render::human_bytes(m.total_bytes),
                        m.shards.len().to_string(),
                        m.gguf
                            .as_ref()
                            .map(|g| g.n_ctx_train.to_string())
                            .unwrap_or_default(),
                        if m.is_vision() {
                            "yes".to_string()
                        } else {
                            String::new()
                        },
                        m.dir.clone(),
                    ]
                })
                .collect();
            render::print_table(
                &[
                    "ID",
                    "NAME",
                    "QUANT",
                    "SIZE",
                    "SHARDS",
                    "CTX_TRAIN",
                    "VISION",
                    "DIR",
                ],
                rows,
            );
            Ok(())
        }
        ModelsCmd::Show { model, json } => {
            let m = resolve_model(&models, model)?;
            if *json {
                return render::print_json(served_by, render::now_unix(), false, &m);
            }
            if serving.is_offline() {
                render::print_offline_notice();
            }
            render::print_line(&format!("{}  ({})", m.name, m.id));
            render::print_line(&format!("  dir      {}", m.dir));
            render::print_line(&format!(
                "  size     {} in {} shard(s)",
                render::human_bytes(m.total_bytes),
                m.shards.len()
            ));
            render::print_line(&format!(
                "  quant    {}",
                m.quant.clone().unwrap_or_else(|| "-".to_string())
            ));
            match &m.gguf {
                Some(g) => {
                    render::print_line(&format!(
                        "  arch     {} · {} layers · {} kv heads · ctx_train {}",
                        g.arch, g.n_layer, g.n_head_kv, g.n_ctx_train
                    ));
                    if let Some(full) = g.full_attn_layers {
                        render::print_line(&format!(
                            "  attn     {full} of {} layers carry KV",
                            g.n_layer
                        ));
                    }
                    if let Some(n) = g.n_expert {
                        render::print_line(&format!("  experts  {n}"));
                    }
                }
                None => render::print_line("  arch     (GGUF header unreadable)"),
            }
            for s in &m.mmproj {
                render::print_line(&format!("  mmproj   {}", s.path));
            }
            for s in &m.shards {
                render::print_line(&format!("  shard    {}", s.path));
            }
            Ok(())
        }
    }
}

/// Every local model: from the daemon when there is one, from a live scan when there is not.
///
/// # Errors
/// A daemon that will not answer, or a discovery failure.
pub async fn load(ctx: &Ctx, serving: &Serving) -> anyhow::Result<Vec<LocalModel>> {
    match serving {
        Serving::Daemon(c) => Ok(c.get::<Vec<LocalModel>>("/v1/models/local").await?),
        _ => Ok(discover::discover_models(&ctx.cfg.endpoints).await?),
    }
}

/// Resolve a positional model argument, in the order ARCHITECTURE §7 documents.
///
/// # Errors
/// A "nothing matches" naming where discovery looked, or an "ambiguous" listing every
/// candidate — never a silent pick.
pub fn resolve_model(models: &[LocalModel], q: &str) -> anyhow::Result<LocalModel> {
    if let Some(m) = models.iter().find(|m| m.id == q || m.name == q) {
        return Ok(m.clone());
    }
    let lower = q.to_lowercase();
    let ci_exact: Vec<&LocalModel> = models
        .iter()
        .filter(|m| m.id.to_lowercase() == lower || m.name.to_lowercase() == lower)
        .collect();
    if ci_exact.len() == 1 {
        return Ok(ci_exact[0].clone());
    }

    let prefix: Vec<&LocalModel> = models
        .iter()
        .filter(|m| {
            m.id.to_lowercase().starts_with(&lower) || m.name.to_lowercase().starts_with(&lower)
        })
        .collect();
    match prefix.len() {
        1 => return Ok(prefix[0].clone()),
        0 => {}
        _ => {
            let names: Vec<&str> = prefix.iter().map(|m| m.name.as_str()).collect();
            anyhow::bail!(
                "`{q}` is ambiguous — it matches {}. Name one of them exactly.",
                names.join(", ")
            );
        }
    }

    let path = Path::new(q);
    if path.is_file() {
        return from_path(path);
    }

    anyhow::bail!(
        "no local model matches `{q}` — `apexrouter models ls` lists what discovery found, \
         and [endpoints] model_roots is where it looked"
    )
}

/// A one-file [`LocalModel`] for a path the operator named directly, so a GGUF outside
/// `model_roots` is usable without editing config.
///
/// # Errors
/// When the file cannot be stat'd. An unreadable GGUF header is `gguf: None`, not an
/// error — `fit` is what complains about that, and it says why.
fn from_path(path: &Path) -> anyhow::Result<LocalModel> {
    let bytes = std::fs::metadata(path)?.len();
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let dir = path
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    Ok(LocalModel {
        id: name.to_lowercase().replace([' ', '_'], "-"),
        name,
        dir,
        shards: vec![ModelShard {
            path: path.display().to_string(),
            bytes,
        }],
        total_bytes: bytes,
        mmproj: Vec::new(),
        quant: None,
        gguf: discover::read_gguf_meta(path).ok(),
        discovered_at_unix: render::now_unix(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_protocol::GgufMeta;

    fn model(id: &str, name: &str) -> LocalModel {
        LocalModel {
            id: id.to_string(),
            name: name.to_string(),
            dir: "/home/andre/models".to_string(),
            shards: vec![ModelShard {
                path: format!("/home/andre/models/{name}.gguf"),
                bytes: 6_900_000_000,
            }],
            total_bytes: 6_900_000_000,
            mmproj: Vec::new(),
            quant: Some("Q6_K".to_string()),
            gguf: Some(GgufMeta::default()),
            discovered_at_unix: 0,
        }
    }

    #[test]
    fn exact_id_and_exact_name_both_resolve() {
        let ms = vec![model("carnice-9b-q6-k", "Carnice-9b-Q6_K")];
        assert_eq!(
            resolve_model(&ms, "carnice-9b-q6-k").expect("id").name,
            "Carnice-9b-Q6_K"
        );
        assert_eq!(
            resolve_model(&ms, "Carnice-9b-Q6_K").expect("name").id,
            "carnice-9b-q6-k"
        );
    }

    #[test]
    fn a_unique_case_insensitive_prefix_resolves() {
        let ms = vec![model("carnice-9b-q6-k", "Carnice-9b-Q6_K")];
        assert_eq!(resolve_model(&ms, "carnice").expect("prefix").id, ms[0].id);
        assert_eq!(
            resolve_model(&ms, "CARNICE-9B").expect("prefix").id,
            ms[0].id
        );
    }

    #[test]
    fn an_ambiguous_prefix_lists_the_candidates_instead_of_guessing() {
        let ms = vec![
            model("carnice-9b-q6-k", "Carnice-9b-Q6_K"),
            model("carnice-9b-q4-k", "Carnice-9b-Q4_K"),
        ];
        let e = resolve_model(&ms, "carnice").expect_err("must not guess");
        let msg = e.to_string();
        assert!(msg.contains("ambiguous"), "{msg}");
        assert!(
            msg.contains("Carnice-9b-Q6_K") && msg.contains("Carnice-9b-Q4_K"),
            "{msg}"
        );
    }

    #[test]
    fn nothing_matching_says_where_discovery_looked() {
        let e = resolve_model(&[], "nope").expect_err("must fail");
        assert!(e.to_string().contains("model_roots"), "{e}");
    }

    #[test]
    fn a_path_on_disk_resolves_even_outside_the_model_roots() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("Weird-Q4_K_M.gguf");
        std::fs::write(&p, b"not really a gguf").expect("write");
        let m = resolve_model(&[], &p.display().to_string()).expect("path");
        assert_eq!(m.total_bytes, 17);
        assert_eq!(m.name, "Weird-Q4_K_M");
        assert!(
            m.gguf.is_none(),
            "an unreadable header is None, not an error"
        );
    }
}
