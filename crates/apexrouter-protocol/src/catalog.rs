//! Recipes, search profiles and the container contract.
//!
//! `recipes.toml`'s 71 hand-written entries are replaced by **discovery + saved drafts**. A
//! [`Recipe`] is the saved *result* of a discovery session, with provenance so staleness is
//! detectable; a [`SearchProfile`] is a *query template* over the live vast.ai market, never
//! a fixed tier — `00c` proves `gpu_name` strings change under you.

use crate::endpoint::{LocalLlamaSpec, LocalVllmSpec, ManagedSpec};
use crate::fit::FitPlan;
use crate::ids::{ProfileId, RecipeId};
use crate::money::Money;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A saved launch plan.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Recipe {
    /// Generated, never typed by the user.
    pub id: RecipeId,
    /// Human label.
    pub label: String,
    /// Free text.
    #[serde(default)]
    pub description: Option<String>,
    /// What it launches.
    pub kind: RecipeKind,
    /// Where it came from, so staleness is detectable.
    pub provenance: Provenance2,
    /// Unix seconds.
    pub created_at_unix: i64,
    /// Unix seconds.
    pub updated_at_unix: i64,
}

/// What a recipe launches.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecipeKind {
    /// A local `llama-server`.
    Local(LocalLlamaSpec),
    /// A local vLLM.
    LocalVllm(LocalVllmSpec),
    /// A rental: which market query, what to run, and what we expect to fit.
    Vast {
        /// Which search profile to run.
        profile: ProfileId,
        /// The container contract.
        launch: ContainerLaunch,
        /// What we expect to fit on the pooled VRAM.
        #[serde(default)]
        fit: Option<FitPlan>,
    },
    /// A managed provider.
    Managed(ManagedSpec),
}

/// Where a recipe's facts came from, so "this is stale" is answerable without guessing.
///
/// Named `Provenance2` because [`crate::backend::Provenance`] already means something else:
/// how a *backend* came to exist.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Provenance2 {
    /// When the underlying artefact was discovered, unix seconds.
    pub discovered_at_unix: i64,
    /// Size of the weights at discovery time.
    #[serde(default)]
    pub size_bytes: Option<u64>,
    /// Free text: a path, an HF repo, `"imported from recipes.toml"`.
    pub source: String,
    /// The fit plan computed at discovery time.
    #[serde(default)]
    pub fit: Option<FitPlan>,
}

/// A vast.ai search profile. **Never** a hardcoded GPU enum.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchProfile {
    /// Generated.
    pub id: ProfileId,
    /// Human label.
    pub label: String,
    /// Exact strings from the live vocabulary, e.g. `["RTX 3090"]`, `["H100 SXM"]`.
    #[serde(default)]
    pub gpu_names: Vec<String>,
    /// Minimum GPU count.
    pub num_gpus_min: u32,
    /// Maximum GPU count. One profile spans a range, never one profile per count.
    pub num_gpus_max: u32,
    /// Ceiling on `dph_total`.
    #[serde(default)]
    pub max_dph: Option<Money>,
    /// Minimum `reliability2`.
    pub min_reliability: f32,
    /// Minimum inbound bandwidth, Mbps — a slow box turns a 30 GB pull into an hour.
    pub min_inet_down: u32,
    /// Minimum disk.
    pub min_disk_gb: u32,
    /// Minimum `cuda_max_good`.
    #[serde(default)]
    pub min_cuda: Option<f32>,
    /// Geography filter, matched client-side on the tail of `geolocation`.
    pub geo: GeoFilter,
    /// Which of our published images to run.
    pub image_type: ImageType,
    /// Free-form passthrough into the query body, for anything vast adds later.
    #[serde(default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Where in the world. Matched against the **tail** of `geolocation` (`"Czechia, CZ"`),
/// never by string equality on the whole field.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GeoFilter {
    /// Anywhere.
    Any,
    /// The Nordics.
    EuNordic,
    /// The EU.
    Eu,
    /// The United States.
    Us,
    /// An explicit ISO-3166 alpha-2 list.
    Codes(Vec<String>),
}

impl GeoFilter {
    /// The country codes this filter accepts. Empty means "no constraint".
    pub fn codes(&self) -> Vec<&str> {
        match self {
            GeoFilter::Any => Vec::new(),
            GeoFilter::EuNordic => vec!["SE", "NO", "DK", "FI", "IS"],
            GeoFilter::Eu => vec![
                "AT", "BE", "BG", "HR", "CY", "CZ", "DK", "EE", "FI", "FR", "DE", "GR", "HU", "IE",
                "IT", "LV", "LT", "LU", "MT", "NL", "PL", "PT", "RO", "SK", "SI", "ES", "SE",
            ],
            GeoFilter::Us => vec!["US"],
            GeoFilter::Codes(v) => v.iter().map(String::as_str).collect(),
        }
    }

    /// Does a vast `geolocation` string satisfy this filter?
    ///
    /// The field looks like `"Czechia, CZ"`, so we take the tail after the last comma and
    /// compare case-insensitively. A field with no comma is treated as already being a code.
    pub fn matches(&self, geolocation: &str) -> bool {
        if matches!(self, GeoFilter::Any) {
            return true;
        }
        let tail = geolocation.rsplit(',').next().unwrap_or(geolocation).trim();
        if tail.is_empty() {
            return false;
        }
        self.codes().iter().any(|c| c.eq_ignore_ascii_case(tail))
    }
}

/// Which published image to run. `Builder` costs 12–18 minutes of cold start and is forced
/// by a `known_forks` hit.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageType {
    /// Prebuilt llama.cpp.
    Prebuilt,
    /// Builds llama.cpp from a named repo/ref.
    Builder,
    /// vLLM.
    Vllm,
}

/// **THE CONTAINER CONTRACT.** This is what makes a rented box actually serve tokens.
///
/// The `env` map is produced by the same `core::argv` builder that produces the local argv,
/// so the `--top-k 20` divergence between two hand-maintained tables cannot recur.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerLaunch {
    /// What runs inside.
    pub runtime: ContainerRuntime,
    /// Resolved from `[docker]` by `image_type`.
    pub image: String,
    /// Which image family.
    pub image_type: ImageType,
    /// Disk to request, GB.
    pub disk_gb: u32,
    /// The variable contract. `HF_TOKEN` lives here and **never** in `onstart`, which vast
    /// persists and echoes back in `show instance`.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// `"bash /app/launch.sh > /var/log/launch.log 2>&1 &"`.
    pub onstart: String,
    /// ALWAYS `127.0.0.1` — tunnel-only posture — unless `expose_public`.
    pub host: String,
    /// Port inside the container. 8000.
    pub port: u16,
    /// `false` by default; `true` REQUIRES a freshly minted per-instance api key, because a
    /// vast direct port is plaintext HTTP on a shared public IP.
    pub expose_public: bool,
}

/// What runs inside a rented container.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerRuntime {
    /// `llama-server`.
    LlamaCpp,
    /// vLLM.
    Vllm,
}

/// Exactly what we are about to send to vast, before we send it. Rendered before every
/// confirm, in every surface.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerEnvPreview {
    /// The env map, ordered for display.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// The onstart string.
    pub onstart: String,
    /// The resolved image.
    pub image: String,
    /// Overrides the image's own `ENTRYPOINT` (which would otherwise start a second server
    /// on port 8000 and contend with `onstart`). Normally `["sleep", "infinity"]`.
    #[serde(default)]
    pub args_override: Vec<String>,
    /// e.g. `"custom fork → builder image → +12–18 min cold start"`.
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// What `apexrouter migrate --dry-run` proposes to do, per artefact.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationPlan {
    /// One entry per legacy artefact found.
    #[serde(default)]
    pub items: Vec<MigrationItem>,
    /// Which directories were read.
    #[serde(default)]
    pub source_paths: Vec<String>,
}

/// One legacy artefact and what we would do with it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationItem {
    /// `"recipe"`, `"usage row"`, `"known_fork"`, …
    pub what: String,
    /// Where it came from.
    pub from: String,
    /// Import, skip or warn.
    pub action: MigrationAction,
    /// The reason, per row. The 54 `vast_gguf` recipes each get one.
    pub detail: String,
}

/// What migration would do with one artefact.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationAction {
    /// Bring it across.
    Import,
    /// Deliberately leave it behind, with a reason.
    Skip,
    /// Bring it across but flag something about it.
    Warn,
}

/// What `apexrouter migrate --apply` actually did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationReport {
    /// How many artefacts were imported.
    pub imported: u32,
    /// How many were skipped.
    pub skipped: u32,
    /// Anything the operator should read.
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::CredentialSource;
    use crate::ids::ProviderId;

    #[test]
    fn geo_filter_matches_on_the_tail_never_the_whole_string() {
        assert!(GeoFilter::Any.matches("Czechia, CZ"));
        assert!(GeoFilter::Eu.matches("Czechia, CZ"));
        assert!(GeoFilter::Eu.matches("  Sweden ,  se "));
        assert!(!GeoFilter::Eu.matches("United States, US"));
        assert!(GeoFilter::Us.matches("United States, US"));
        assert!(GeoFilter::EuNordic.matches("Norway, NO"));
        assert!(!GeoFilter::EuNordic.matches("Poland, PL"));
        assert!(GeoFilter::Codes(vec!["IS".into()]).matches("Iceland, IS"));
        // A bare code with no comma still works.
        assert!(GeoFilter::Us.matches("US"));
        assert!(!GeoFilter::Us.matches(""));
    }

    #[test]
    fn geo_filter_round_trips_every_variant() {
        for g in [
            GeoFilter::Any,
            GeoFilter::EuNordic,
            GeoFilter::Eu,
            GeoFilter::Us,
            GeoFilter::Codes(vec!["CZ".into(), "PL".into()]),
        ] {
            let s = serde_json::to_string(&g).expect("ser");
            assert_eq!(serde_json::from_str::<GeoFilter>(&s).expect("de"), g);
        }
        assert_eq!(
            serde_json::to_string(&GeoFilter::EuNordic).expect("ser"),
            "\"eu_nordic\""
        );
    }

    #[test]
    fn container_launch_round_trips() {
        let mut env = BTreeMap::new();
        env.insert("MODEL_REPO".into(), "unsloth/Qwen3-Coder".into());
        env.insert("HOST".into(), "127.0.0.1".into());
        let c = ContainerLaunch {
            runtime: ContainerRuntime::LlamaCpp,
            image: "ghcr.io/buckster123/vastai-gguf:prebuilt".into(),
            image_type: ImageType::Prebuilt,
            disk_gb: 80,
            env,
            onstart: "bash /app/launch.sh > /var/log/launch.log 2>&1 &".into(),
            host: "127.0.0.1".into(),
            port: 8000,
            expose_public: false,
        };
        let s = serde_json::to_string(&c).expect("ser");
        assert_eq!(serde_json::from_str::<ContainerLaunch>(&s).expect("de"), c);
    }

    #[test]
    fn recipe_kind_round_trips_every_variant() {
        let kinds = [
            RecipeKind::Managed(ManagedSpec {
                provider: ProviderId::parse("together").expect("id"),
                base_url: "https://api.together.xyz".into(),
                credential: CredentialSource::Env {
                    var: "TOGETHER_API_KEY".into(),
                },
                model_id: None,
                protocol: Default::default(),
            }),
            RecipeKind::Vast {
                profile: ProfileId::parse("rtx3090").expect("id"),
                launch: ContainerLaunch {
                    runtime: ContainerRuntime::Vllm,
                    image: "ghcr.io/buckster123/vastai-gguf:vllm".into(),
                    image_type: ImageType::Vllm,
                    disk_gb: 120,
                    env: BTreeMap::new(),
                    onstart: "bash /app/launch_vllm.sh &".into(),
                    host: "127.0.0.1".into(),
                    port: 8000,
                    expose_public: false,
                },
                fit: None,
            },
        ];
        for k in &kinds {
            let s = serde_json::to_string(k).expect("ser");
            assert_eq!(&serde_json::from_str::<RecipeKind>(&s).expect("de"), k);
        }
    }

    #[test]
    fn search_profile_and_migration_types_round_trip() {
        let p = SearchProfile {
            id: ProfileId::parse("rtx3090").expect("id"),
            label: "RTX 3090 x2-4".into(),
            gpu_names: vec!["RTX 3090".into()],
            num_gpus_min: 2,
            num_gpus_max: 4,
            max_dph: Some(Money::from_usd(0.40)),
            min_reliability: 0.98,
            min_inet_down: 500,
            min_disk_gb: 80,
            min_cuda: Some(12.0),
            geo: GeoFilter::Eu,
            image_type: ImageType::Prebuilt,
            extra: serde_json::Map::new(),
        };
        let s = serde_json::to_string(&p).expect("ser");
        assert_eq!(serde_json::from_str::<SearchProfile>(&s).expect("de"), p);

        let plan = MigrationPlan {
            items: vec![MigrationItem {
                what: "recipe".into(),
                from: "recipes.toml [vast_gguf.qwen3-coder-2x3090]".into(),
                action: MigrationAction::Skip,
                detail: "a frozen hand-solved fit, superseded by fit()".into(),
            }],
            source_paths: vec!["/home/andre/.vastai-gguf".into()],
        };
        let s = serde_json::to_string(&plan).expect("ser");
        assert_eq!(serde_json::from_str::<MigrationPlan>(&s).expect("de"), plan);

        let rep = MigrationReport {
            imported: 17,
            skipped: 54,
            warnings: vec!["together key left in place as a reference".into()],
        };
        let s = serde_json::to_string(&rep).expect("ser");
        assert_eq!(
            serde_json::from_str::<MigrationReport>(&s).expect("de"),
            rep
        );
    }

    #[test]
    fn container_env_preview_round_trips() {
        let p = ContainerEnvPreview {
            env: vec![("HOST".into(), "127.0.0.1".into())],
            onstart: "bash /app/launch.sh &".into(),
            image: "ghcr.io/buckster123/vastai-gguf:builder".into(),
            args_override: vec!["sleep".into(), "infinity".into()],
            warnings: vec!["custom fork -> builder image -> +12-18 min cold start".into()],
        };
        let s = serde_json::to_string(&p).expect("ser");
        assert_eq!(
            serde_json::from_str::<ContainerEnvPreview>(&s).expect("de"),
            p
        );
    }
}
