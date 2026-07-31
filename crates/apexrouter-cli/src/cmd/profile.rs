//! OWNER: unit S-08 (cli/src/cmd/{vast,tunnel,hf,provider,recipe,profile,usage,smoke,doctor,compare,backend,swap,up,approvals,token,open,env,migrate}.rs). Do not edit outside that unit.
//!
//! `apexrouter profile ls | show | new | edit | rm`.
//!
//! A search profile is **one** `OfferQuery`, and that is the whole point: the documented
//! LocalRouter bug where "auto — cheapest" rented from a stricter candidate set than the
//! browser had displayed dies because there is exactly one query builder and every surface
//! feeds it the same profile (`ARCHITECTURE.md` §4.8).
//!
//! `gpu_names` are **exact strings from the live vocabulary** (`apexrouter vast gpu-names`),
//! never a hardcoded enum: vast adds GPUs faster than we ship, and a profile naming a card
//! the market does not spell that way silently matches nothing.
//!
//! This module also owns [`edit_json`], the round-trip `recipe edit` shares: serialise,
//! open `$VISUAL`/`$EDITOR` on a temp file, re-parse. It lives here rather than in a new
//! file because S-08 owns no module whose job is "editor plumbing".

use crate::cli::{parse_geo, split_list, ProfileCmd, ProfileNewArgs};
use crate::cmd::Ctx;
use crate::daemon::{Need, Serving};
use crate::render;
use apexrouter_protocol::{GeoFilter, ImageType, Money, ProfileId, SearchProfile, ServedBy};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::process::Command;

/// Run `apexrouter profile …`.
///
/// # Errors
/// A catalogue that will not parse, a daemon that will not answer, or an unknown id.
pub async fn run(ctx: &Ctx, cmd: &ProfileCmd) -> anyhow::Result<()> {
    match cmd {
        ProfileCmd::Ls(args) => {
            let serving = ctx.serving(Need::ReadState).await?;
            let profiles = load(ctx, &serving).await?;
            if args.json {
                return render::print_json(
                    serving.served_by(),
                    render::now_unix(),
                    serving.is_offline(),
                    &profiles,
                );
            }
            if serving.is_offline() {
                render::print_offline_notice();
            }
            render::print_table(
                &[
                    "PROFILE", "LABEL", "GPUS", "COUNT", "MAX $/HR", "GEO", "IMAGE",
                ],
                profiles.iter().map(row).collect(),
            );
            Ok(())
        }
        ProfileCmd::Show { id, json } => {
            let serving = ctx.serving(Need::ReadState).await?;
            let p = find(&load(ctx, &serving).await?, id)?;
            if *json {
                return render::print_json(
                    serving.served_by(),
                    render::now_unix(),
                    serving.is_offline(),
                    &p,
                );
            }
            if serving.is_offline() {
                render::print_offline_notice();
            }
            print_profile(&p);
            Ok(())
        }
        ProfileCmd::New(args) => {
            let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
            let profile = build(args)?;
            let saved: SearchProfile = client.post("/v1/profiles", &profile).await?;
            if args.json {
                return render::print_json(ServedBy::Daemon, render::now_unix(), false, &saved);
            }
            render::print_line(&format!("{}  {}", saved.id.as_str(), saved.label));
            Ok(())
        }
        ProfileCmd::Edit { id } => {
            let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
            let before: SearchProfile = client.get(&format!("/v1/profiles/{id}")).await?;
            let edited: SearchProfile = edit_json(&before, &format!("profile-{id}"))?;
            let after: SearchProfile = client.put(&format!("/v1/profiles/{id}"), &edited).await?;
            render::print_line(&format!("{} saved", after.id.as_str()));
            Ok(())
        }
        ProfileCmd::Rm { id } => {
            let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
            client.delete(&format!("/v1/profiles/{id}")).await?;
            render::print_line(&format!("removed {id}"));
            Ok(())
        }
    }
}

/// Every profile: from the daemon when there is one, from `$STATE/catalog.toml` when not.
///
/// # Errors
/// A catalogue that will not parse, or a daemon that will not answer.
pub async fn load(ctx: &Ctx, serving: &Serving) -> anyhow::Result<Vec<SearchProfile>> {
    match serving {
        Serving::Daemon(c) => Ok(c.get::<Vec<SearchProfile>>("/v1/profiles").await?),
        _ => Ok(apexrouter_core::catalog::load(&ctx.paths)?.profiles),
    }
}

/// One profile by id, or a message naming what there is.
///
/// # Errors
/// When no profile has that id.
pub fn find(profiles: &[SearchProfile], id: &str) -> anyhow::Result<SearchProfile> {
    profiles
        .iter()
        .find(|p| p.id.as_str() == id)
        .cloned()
        .ok_or_else(|| {
            let known: Vec<&str> = profiles.iter().map(|p| p.id.as_str()).collect();
            anyhow::anyhow!(
                "no search profile `{id}`{}",
                if known.is_empty() {
                    " — there are none; `apexrouter profile new` makes one".to_string()
                } else {
                    format!(" — there is: {}", known.join(", "))
                }
            )
        })
}

/// Turn `profile new` flags into a [`SearchProfile`].
///
/// # Errors
/// An unusable `--geo`, or a label that yields no valid id.
fn build(args: &ProfileNewArgs) -> anyhow::Result<SearchProfile> {
    let id = ProfileId::parse(&slug(&args.label))
        .map_err(|e| anyhow::anyhow!("`{}` yields no usable profile id: {e}", args.label))?;
    if args.num_gpus_max < args.num_gpus_min {
        anyhow::bail!(
            "--num-gpus-max {} is below --num-gpus-min {}",
            args.num_gpus_max,
            args.num_gpus_min
        );
    }
    Ok(SearchProfile {
        id,
        label: args.label.clone(),
        gpu_names: args.gpu.as_deref().map(split_list).unwrap_or_default(),
        num_gpus_min: args.num_gpus_min,
        num_gpus_max: args.num_gpus_max,
        max_dph: args.max_price.map(Money::from_usd),
        min_reliability: args.min_reliability,
        min_inet_down: args.min_inet_down,
        min_disk_gb: args.min_disk_gb,
        min_cuda: None,
        geo: args
            .geo
            .as_deref()
            .map(parse_geo)
            .transpose()?
            .unwrap_or(GeoFilter::Any),
        image_type: ImageType::Prebuilt,
        extra: serde_json::Map::new(),
    })
}

/// A label as an id: lowercase, non-alphanumerics collapsed to `-`.
pub fn slug(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut dash = false;
    for ch in label.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            dash = false;
        } else if !out.is_empty() && !dash {
            out.push('-');
            dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// Serialise `v` as pretty JSON, open it in `$VISUAL`/`$EDITOR`, and parse what comes back.
///
/// The temp file lives under `$TMPDIR`, not in the repo and not in `$STATE`: it is neither
/// our durable state nor anything a reader should find later. A parse failure keeps the
/// file and names it, so an hour of editing is never thrown away by a missing comma.
///
/// # Errors
/// An editor that cannot be run or exits non-zero, or JSON that will not parse back into
/// `T` — with the path to the edited buffer in the message.
pub fn edit_json<T: Serialize + DeserializeOwned>(v: &T, stem: &str) -> anyhow::Result<T> {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("apexrouter-{stem}-{}.json", std::process::id()));
    std::fs::write(&path, serde_json::to_string_pretty(v)?)?;

    let editor = editor();
    // An argv vector, never a shell string: house rule 5, and a `$TMPDIR` with a space in
    // it must not become two arguments.
    let status = Command::new(&editor)
        .arg(&path)
        .status()
        .map_err(|e| anyhow::anyhow!("could not run `{editor}` (set $VISUAL or $EDITOR): {e}"))?;
    if !status.success() {
        anyhow::bail!(
            "{editor} exited with {status}; {} was left alone",
            path.display()
        );
    }

    let text = std::fs::read_to_string(&path)?;
    match serde_json::from_str::<T>(&text) {
        Ok(parsed) => {
            let _ = std::fs::remove_file(&path);
            Ok(parsed)
        }
        Err(e) => Err(anyhow::anyhow!(
            "the edited document does not parse ({e}); your edits are still in {}",
            path.display()
        )),
    }
}

/// `$VISUAL`, then `$EDITOR`, then `vi` — the POSIX order.
fn editor() -> String {
    for key in ["VISUAL", "EDITOR"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return v;
            }
        }
    }
    "vi".to_string()
}

/// One row of the profile table.
fn row(p: &SearchProfile) -> Vec<String> {
    vec![
        p.id.as_str().to_string(),
        p.label.clone(),
        p.gpu_names.join(","),
        if p.num_gpus_min == p.num_gpus_max {
            p.num_gpus_min.to_string()
        } else {
            format!("{}-{}", p.num_gpus_min, p.num_gpus_max)
        },
        p.max_dph
            .map(|m| format!("{:.4}", m.as_usd()))
            .unwrap_or_default(),
        geo_label(&p.geo),
        render::variant(&p.image_type),
    ]
}

/// The detail view.
fn print_profile(p: &SearchProfile) {
    render::print_line(&format!("{}  {}", p.id.as_str(), p.label));
    render::print_line(&format!(
        "  gpus      {}",
        if p.gpu_names.is_empty() {
            "(any)".to_string()
        } else {
            p.gpu_names.join(", ")
        }
    ));
    render::print_line(&format!(
        "  count     {}..{}",
        p.num_gpus_min, p.num_gpus_max
    ));
    render::print_line(&format!(
        "  max $/hr  {}",
        p.max_dph
            .map(|m| format!("{:.4}", m.as_usd()))
            .unwrap_or_else(|| "(none)".to_string())
    ));
    render::print_line(&format!("  geo       {}", geo_label(&p.geo)));
    render::print_line(&format!(
        "  floors    reliability {:.2} · inet_down {} Mbps · disk {} GB{}",
        p.min_reliability,
        p.min_inet_down,
        p.min_disk_gb,
        p.min_cuda
            .map(|c| format!(" · cuda {c:.1}"))
            .unwrap_or_default()
    ));
    render::print_line(&format!("  image     {}", render::variant(&p.image_type)));
}

/// A geo filter in one cell.
pub fn geo_label(g: &GeoFilter) -> String {
    match g {
        GeoFilter::Any => "any".to_string(),
        GeoFilter::Eu => "eu".to_string(),
        GeoFilter::EuNordic => "eu-nordic".to_string(),
        GeoFilter::Us => "us".to_string(),
        GeoFilter::Codes(c) => c.join(","),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(label: &str) -> ProfileNewArgs {
        ProfileNewArgs {
            label: label.to_string(),
            gpu: Some("RTX 3090, H100 SXM".to_string()),
            num_gpus_min: 2,
            num_gpus_max: 2,
            max_price: Some(0.75),
            geo: Some("EU".to_string()),
            min_reliability: 0.98,
            min_inet_down: 200,
            min_disk_gb: 60,
            json: false,
        }
    }

    #[test]
    fn a_label_becomes_a_usable_id() {
        assert_eq!(slug("Two 3090s (EU)"), "two-3090s-eu");
        assert_eq!(slug("H100 SXM"), "h100-sxm");
        assert_eq!(slug("---"), "");
    }

    #[test]
    fn gpu_names_stay_exactly_as_the_market_spells_them() {
        let p = build(&args("Two 3090s")).expect("profile");
        assert_eq!(p.gpu_names, ["RTX 3090", "H100 SXM"]);
        assert_eq!(p.id.as_str(), "two-3090s");
        assert_eq!(p.geo, GeoFilter::Eu);
        assert_eq!(p.max_dph.map(|m| m.as_usd()), Some(0.75));
    }

    #[test]
    fn an_inverted_gpu_count_range_is_refused_rather_than_matching_nothing() {
        let mut a = args("Two 3090s");
        a.num_gpus_min = 4;
        a.num_gpus_max = 2;
        let e = build(&a).expect_err("must fail");
        assert!(e.to_string().contains("below"), "{e}");
    }

    #[test]
    fn a_label_with_no_alphanumerics_is_refused_rather_than_yielding_an_empty_id() {
        let e = build(&args("!!!")).expect_err("must fail");
        assert!(e.to_string().contains("no usable profile id"), "{e}");
    }

    #[test]
    fn geo_renders_both_the_named_groups_and_a_code_list() {
        assert_eq!(geo_label(&GeoFilter::Any), "any");
        assert_eq!(geo_label(&GeoFilter::EuNordic), "eu-nordic");
        assert_eq!(
            geo_label(&GeoFilter::Codes(vec!["CZ".into(), "PL".into()])),
            "CZ,PL"
        );
    }

    #[test]
    fn a_missing_profile_points_at_the_verb_that_makes_one() {
        let e = find(&[], "nope").expect_err("must fail");
        assert!(e.to_string().contains("profile new"), "{e}");
    }
}
