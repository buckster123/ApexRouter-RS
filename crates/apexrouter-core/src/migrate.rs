//! OWNER: unit C-16 (core/migrate.rs). Do not edit outside that unit.
//!
//! Migration from `~/.vastai-gguf` and the LocalRouter checkout.
//!
//! `--dry-run` writes **nothing at all** — proven by comparing a directory hash before and
//! after. The 54 `vast_gguf` recipes are deliberately **not** imported: they are a frozen
//! function superseded by `fit()`, and the plan says exactly that, per row.
//!
//! Type traps handled on import: `max_price` is a quoted **string**; `enforce_eager` is the
//! string `"true"`/`"false"` (parse `true|1|yes` case-insensitively, everything else false);
//! `provider` is absent on 54 of 71 rows and defaults to `vast_gguf`; `ctx` is the **total**
//! pool shared across `parallel` slots; `vram_gb` is **per GPU** and must be multiplied by
//! `num_gpus`.
//!
//! Two rules shape everything below.
//!
//! 1. **Nothing legacy is ever mutated, and [`plan`] writes nothing at all.** It opens no
//!    file for writing, creates no directory, and does not so much as open the ledger
//!    (which `Ledger::open` would create). A test hashes the whole tree either side of it.
//! 2. **A credential is never copied.** A key found in `~/.vastai-gguf/config.toml` is
//!    imported as a *reference* — an env var name or a file path — and the plaintext never
//!    reaches [`MigrationPlan`], the config file, or a log line. That is why
//!    [`LegacyActiveEndpoint`] records `api_key_present: bool` rather than the key itself:
//!    the plan is printed, and a struct that can hold key material eventually prints it.

use crate::config::{Config, DockerCfg, KnownFork, ProviderCfg};
use crate::error::{Error, Result};
use crate::ledger::Ledger;
use crate::paths::Paths;
use apexrouter_protocol::{
    BuildId, CostEstimate, CredentialSource, GeoFilter, ImageType, InstanceId, KvType, LedgerRow,
    LedgerState, LocalLlamaSpec, ManagedSpec, MigrationAction, MigrationItem, MigrationPlan,
    MigrationReport, Money, NglPlan, ProfileId, Protocol, Provenance2, ProviderId, Recipe,
    RecipeId, RecipeKind, SamplingMode, SearchProfile, SplitPlan,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// The provider a `recipes.toml` row belongs to when it names none. 54 of the 71 rows.
const DEFAULT_LEGACY_PROVIDER: &str = "vast_gguf";

/// LocalRouter's own default Together base URL, used when the legacy `config.toml` is not
/// available to [`import_recipes_toml`], which sees only one file. [`plan`] overrides it
/// with the configured value, **verbatim** — `api.together.xyz` is never rewritten to `.ai`.
const LEGACY_TOGETHER_BASE_URL: &str = "https://api.together.ai/v1";

// ---------------------------------------------------------------------------------------
// plan / apply
// ---------------------------------------------------------------------------------------

/// Enumerate every legacy artefact and say what we would do with each.
///
/// This is the `--dry-run` path. It writes nothing, creates nothing and locks nothing.
///
/// # Errors
/// [`Error::Io`] only when a directory that exists cannot be read at all. A single
/// unparseable file is reported as an item, never as a failure — legacy state is other
/// people's data and half of it is stale by design.
pub fn plan(paths: &Paths, cfg: &Config) -> Result<MigrationPlan> {
    let survey = survey(paths, cfg)?;
    Ok(MigrationPlan {
        items: survey.planned.into_iter().map(|p| p.item).collect(),
        source_paths: survey.source_paths,
    })
}

/// Execute a plan. Import-only; never destructive to the legacy tree.
///
/// The `plan` argument is *honoured*, not decoration: an artefact whose item was downgraded
/// to [`MigrationAction::Skip`] is not written, which is what lets a caller render the plan,
/// let a human strike rows out, and apply the remainder.
///
/// Re-running is safe. Providers, forks, recipes and profiles are inserted only when absent,
/// and a ledger row already carrying this function's import marker is not appended twice.
///
/// # Errors
/// [`Error::Io`] or [`Error::Toml`] from writing the config file, the catalog or the ledger.
pub fn apply(paths: &Paths, cfg: &Config, plan: &MigrationPlan) -> Result<MigrationReport> {
    let allowed: HashSet<(&str, &str)> = plan
        .items
        .iter()
        .filter(|i| i.action != MigrationAction::Skip)
        .map(|i| (i.what.as_str(), i.from.as_str()))
        .collect();

    let survey = survey(paths, cfg)?;
    let mut report = MigrationReport {
        warnings: survey.warnings.clone(),
        ..MigrationReport::default()
    };

    let mut next_cfg = cfg.clone();
    let mut cfg_dirty = false;
    let mut recipes: Vec<Recipe> = Vec::new();
    let mut profiles: Vec<SearchProfile> = Vec::new();
    let mut ledger_rows: Vec<LedgerRow> = Vec::new();

    for planned in survey.planned {
        let selected = allowed.contains(&(planned.item.what.as_str(), planned.item.from.as_str()));
        if !selected || planned.item.action == MigrationAction::Skip {
            report.skipped = report.skipped.saturating_add(1);
            continue;
        }
        if planned.item.action == MigrationAction::Warn {
            report
                .warnings
                .push(format!("{}: {}", planned.item.from, planned.item.detail));
        }
        match planned.payload {
            Payload::Nothing => {}
            Payload::Provider { id, cfg: pcfg } => {
                next_cfg.providers.entry(id).or_insert(pcfg);
                cfg_dirty = true;
            }
            Payload::Docker(d) => {
                next_cfg.docker = d;
                cfg_dirty = true;
            }
            Payload::MirrorUsageLog => {
                next_cfg.compat.mirror_usage_log = true;
                cfg_dirty = true;
            }
            Payload::Fork { name, fork } => {
                next_cfg.known_forks.entry(name).or_insert(fork);
                cfg_dirty = true;
            }
            Payload::Recipe(r) => recipes.push(*r),
            Payload::Profile(p) => profiles.push(*p),
            Payload::Ledger(row) => ledger_rows.push(*row),
        }
        report.imported = report.imported.saturating_add(1);
    }

    if cfg_dirty {
        next_cfg.save(paths)?;
    }

    // The catalog file is only rewritten when there is something to put in it: a migration
    // that found no recipes must not reformat a hand-edited `catalog.toml`.
    if !recipes.is_empty() || !profiles.is_empty() {
        let mut catalog = crate::catalog::load(paths)?;
        let have_recipes: HashSet<RecipeId> =
            catalog.recipes.iter().map(|r| r.id.clone()).collect();
        let have_profiles: HashSet<ProfileId> =
            catalog.profiles.iter().map(|p| p.id.clone()).collect();
        catalog.recipes.extend(
            recipes
                .into_iter()
                .filter(|r| !have_recipes.contains(&r.id)),
        );
        catalog.profiles.extend(
            profiles
                .into_iter()
                .filter(|p| !have_profiles.contains(&p.id)),
        );
        crate::catalog::save(paths, &catalog)?;
    }

    if !ledger_rows.is_empty() {
        let ledger = Ledger::open(paths)?;
        let seen: HashSet<(Option<u64>, Option<String>)> = ledger
            .rows()?
            .into_iter()
            .map(|r| (r.instance_id.map(|i| i.0), r.note))
            .collect();
        for row in ledger_rows {
            if seen.contains(&(row.instance_id.map(|i| i.0), row.note.clone())) {
                continue;
            }
            ledger.append(&row)?;
        }
    }

    Ok(report)
}

// ---------------------------------------------------------------------------------------
// .active_endpoint
// ---------------------------------------------------------------------------------------

/// Read `.active_endpoint` in **all four** shapes it has had.
///
/// The four, all of them written by code still on this machine:
///
/// | # | writer | provider | timestamp key | `pid` |
/// |---|---|---|---|---|
/// | 1 | `localrouter/providers.py` | `together` | `activated_at` | no |
/// | 2 | `endpoint_proxy.py` `/switch` | `together` | `switched_at` | no |
/// | 3 | `localrouter/local_endpoint.py` | `local` | `activated_at` | yes |
/// | 4 | `endpoint_proxy.py` `/switch` | `local` | `switched_at` | no |
///
/// A missing or empty file is `Ok(None)`: nothing was active, which is the normal state.
///
/// # Errors
/// [`Error::Io`] when the file exists but cannot be read, [`Error::Json`] when it is not
/// JSON at all.
pub fn read_legacy_active_endpoint(path: &Path) -> Result<Option<LegacyActiveEndpoint>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(Error::Io {
                path: path.display().to_string(),
                source,
            })
        }
    };
    if text.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(&text)?))
}

/// The legacy "what is active" file, tolerant of every shape it has taken.
///
/// `activated_at` and `switched_at` are the same field under two names, and `pid` is
/// sometimes absent — hence the serde aliases and the `Option`.
///
/// **`api_key` is deliberately reduced to a bool on the way in.** Shape 3 embeds a plaintext
/// local API key; this struct is `Serialize` and its contents reach operator-facing output,
/// so the only safe representation of that field is "there was one".
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LegacyActiveEndpoint {
    /// `"together"`, `"local"`, occasionally `"vast-gguf"`.
    pub provider: String,
    /// Shapes 3 and 4: the `local_instances/<name>.json` stem.
    pub name: Option<String>,
    /// Shapes 1 and 2: the upstream model id.
    pub model_id: Option<String>,
    /// Shapes 1 and 2. Used **verbatim** wherever it is used at all.
    pub base_url: Option<String>,
    /// Shapes 1 and 2: `<base_url>/chat/completions`, denormalised by the writer.
    pub endpoint: Option<String>,
    /// Shapes 3 and 4.
    pub host: Option<String>,
    /// Shapes 3 and 4.
    pub port: Option<u16>,
    /// Shape 3 only. The process is long gone by the time we read this.
    pub pid: Option<i32>,
    /// Shapes 3 and 4. Often points at a model that no longer exists.
    pub model_path: Option<String>,
    /// Injected by `get_active_endpoint()` in memory; occasionally lands on disk.
    pub status: Option<String>,
    /// The vast shape, produced in memory by `get_active_endpoint()`.
    pub instance_id: Option<u64>,
    /// The one timestamp, under whichever of its two names the writer used. Kept as the raw
    /// string: it is `%Y-%m-%dT%H:%M:%SZ`, and shape 1 writes **local** time with a lying
    /// `Z`, so it is parsed leniently and never re-emitted as if it were RFC 3339.
    #[serde(alias = "switched_at")]
    pub activated_at: Option<String>,
    /// Whether the file carried a plaintext `api_key`. **The key itself is discarded at the
    /// deserializer**, so it cannot leak through this struct's `Serialize`.
    #[serde(
        rename = "api_key",
        deserialize_with = "de_key_present",
        serialize_with = "ser_key_present"
    )]
    pub api_key_present: bool,
}

/// Collapse any `api_key` value to "was there one", discarding the key material.
fn de_key_present<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<bool, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(match v {
        serde_json::Value::Null => false,
        serde_json::Value::String(s) => !s.trim().is_empty(),
        _ => true,
    })
}

/// Emit the presence flag, never a key.
fn ser_key_present<S: Serializer>(v: &bool, s: S) -> std::result::Result<S::Ok, S::Error> {
    s.serialize_bool(*v)
}

// ---------------------------------------------------------------------------------------
// local_instances/*.json
// ---------------------------------------------------------------------------------------

/// Read `~/.vastai-gguf/local_instances/*.json`.
///
/// **Paths are validated on load** — a saved instance pointing at a model that no longer
/// exists is normal, and must show up as an importable-but-stale row rather than an error.
/// A file that will not parse is logged and skipped, for the same reason.
///
/// The result is sorted by `name`, so a plan is byte-stable across runs.
///
/// # Errors
/// [`Error::Io`] when the directory exists but cannot be listed. A missing directory is an
/// empty result.
pub fn read_legacy_instances(dir: &Path) -> Result<Vec<LegacyLocalInstance>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(Error::Io {
                path: dir.display().to_string(),
                source,
            })
        }
    };

    let mut out: Vec<LegacyLocalInstance> = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "unreadable legacy instance entry");
                continue;
            }
        };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "unreadable legacy instance");
                continue;
            }
        };
        let mut inst: LegacyLocalInstance = match serde_json::from_str(&text) {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "unparseable legacy instance");
                continue;
            }
        };
        if inst.name.trim().is_empty() {
            inst.name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unnamed")
                .to_owned();
        }
        inst.source_file = path.display().to_string();
        inst.model_exists = inst
            .model_path
            .as_deref()
            .is_some_and(|p| expand_tilde(p).is_file());
        inst.binary_exists = inst
            .binary
            .as_deref()
            .is_some_and(|p| expand_tilde(p).is_file());
        out.push(inst);
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// One row of `~/.vastai-gguf/local_instances/`.
///
/// `model_exists` / `binary_exists` / `source_file` are **computed at load**, not fields of
/// the legacy file: "does the thing it names still exist" is the single question that decides
/// whether this row imports cleanly or imports stale, and answering it once here keeps every
/// surface from re-deriving it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LegacyLocalInstance {
    /// The `<name>.json` stem, and LocalRouter's handle for the instance.
    pub name: String,
    /// The pid it had when it was written. Long dead by now, normally.
    pub pid: Option<i32>,
    /// The port it bound.
    pub port: Option<u16>,
    /// The host it bound. `127.0.0.1` in every real file.
    pub host: Option<String>,
    /// Absolute path of the `llama-server` it ran.
    pub binary: Option<String>,
    /// The GGUF, often written in `~/…` form and often gone.
    pub model_path: Option<String>,
    /// `"vulkan"`, `"cuda"`, `"cpu"`, …
    pub backend: Option<String>,
    /// `%Y-%m-%dT%H:%M:%SZ`, parsed leniently.
    pub started_at: Option<String>,
    /// `"running"` / `"stopped"`.
    pub status: Option<String>,
    /// `%Y-%m-%dT%H:%M:%SZ`, parsed leniently.
    pub stopped_at: Option<String>,
    /// Computed at load: does `model_path` still resolve to a file?
    pub model_exists: bool,
    /// Computed at load: does `binary` still resolve to a file?
    pub binary_exists: bool,
    /// Computed at load: which file this row came from.
    pub source_file: String,
}

// ---------------------------------------------------------------------------------------
// recipes.toml
// ---------------------------------------------------------------------------------------

/// Import `recipes.toml`: the `[docker]` image map, the 7 `llama_cpp_repo`/`ref` mappings
/// as `known_forks` (genuinely undiscoverable knowledge), the GPU tiers as `SearchProfile`
/// seeds, the 3 `local` recipes and the 7 `together` recipes.
///
/// The trailing `Vec<String>` is the per-row skip reasons, including one for each of the 54
/// `vast_gguf` rows.
///
/// The five-tuple is the published signature: one legacy file yields five unrelated kinds of
/// thing, and naming a struct for a one-call-site import would be ceremony.
///
/// # Errors
/// [`Error::Io`] if the file cannot be read, [`Error::Toml`] if it is not TOML.
#[allow(clippy::type_complexity)]
pub fn import_recipes_toml(
    path: &Path,
) -> Result<(
    Vec<Recipe>,
    Vec<SearchProfile>,
    Vec<KnownFork>,
    DockerCfg,
    Vec<String>,
)> {
    let imported = import_recipes_detail(path)?;
    let skips = imported
        .skips
        .iter()
        .map(|s| format!("{}: {}", s.name, s.reason))
        .collect();
    Ok((
        imported.recipes.into_iter().map(|(_, r)| r).collect(),
        imported.profiles.into_iter().map(|(_, p)| p).collect(),
        imported.forks.into_iter().map(|(_, _, f)| f).collect(),
        imported.docker,
        skips,
    ))
}

/// One deliberately-not-imported row.
struct SkipRow {
    /// The legacy row's `name`, or the `[docker]` key.
    name: String,
    /// Why, in the operator's language.
    reason: String,
}

/// Everything [`import_recipes_toml`] produces, with the identity of each row kept so
/// [`plan`] can address one item per row.
struct ImportedRecipes {
    /// `(legacy row name, recipe)`.
    recipes: Vec<(String, Recipe)>,
    /// `(legacy tier key, profile)`.
    profiles: Vec<(String, SearchProfile)>,
    /// `(legacy row name, `known_forks` key, fork)`.
    forks: Vec<(String, String, KnownFork)>,
    /// The `[docker]` image map, defaults filled in for keys the file omits.
    docker: DockerCfg,
    /// Per-row skip reasons.
    skips: Vec<SkipRow>,
    /// How each tier's per-GPU `vram_gb` multiplies out, keyed by tier key. Rendered into
    /// the plan so the arithmetic is visible rather than assumed.
    tier_notes: BTreeMap<String, String>,
    /// `[docker]` keys we do not model.
    extra_docker_keys: Vec<String>,
}

/// The whole of [`import_recipes_toml`], with row identity preserved.
fn import_recipes_detail(path: &Path) -> Result<ImportedRecipes> {
    let text = std::fs::read_to_string(path).map_err(|source| Error::Io {
        path: path.display().to_string(),
        source,
    })?;
    let file: LegacyRecipesFile = toml::from_str(&text)?;
    let now = now_unix();

    // ---- [docker] ------------------------------------------------------------------
    let mut docker = DockerCfg::default();
    let mut extra_docker_keys = Vec::new();
    for (k, v) in &file.docker {
        match k.as_str() {
            "prebuilt" => docker.prebuilt = v.clone(),
            "builder" => docker.builder = v.clone(),
            "vllm" => docker.vllm = v.clone(),
            other => extra_docker_keys.push(other.to_owned()),
        }
    }

    // ---- [gpu_tiers.*] -> SearchProfile seeds ---------------------------------------
    let mut ids = HashSet::new();
    let mut profiles = Vec::new();
    let mut tier_notes = BTreeMap::new();
    for (key, tier) in &file.gpu_tiers {
        let id = ProfileId::parse(&unique_slug(key, &mut ids))?;
        let num_gpus = tier.num_gpus.unwrap_or(1).max(1);
        // `vram_gb` is PER GPU. The pooled figure — the only one a fit decision may use —
        // is this multiplication, and the plan prints it so the trap stays visible.
        let per_gpu = tier.vram_gb.unwrap_or(0);
        let pooled = u64::from(per_gpu) * u64::from(num_gpus);
        let gpu_names: Vec<String> = tier.vast_names.iter().map(|n| vast_gpu_name(n)).collect();
        tier_notes.insert(
            key.clone(),
            format!(
                "{num_gpus}× {} — vram_gb {per_gpu} is PER GPU, so {pooled} GB pooled; \
                 legacy vast names {:?} rewritten to the live vocabulary {:?}.",
                gpu_names.join(" / "),
                tier.vast_names,
                gpu_names
            ),
        );
        profiles.push((
            key.clone(),
            SearchProfile {
                id,
                label: tier
                    .label
                    .clone()
                    .unwrap_or_else(|| format!("imported tier {key}")),
                gpu_names,
                num_gpus_min: num_gpus,
                num_gpus_max: num_gpus,
                // `max_price` is a quoted STRING in every legacy row.
                max_dph: flex_f64(tier.max_price.as_ref()).map(Money::from_usd),
                min_reliability: 0.95,
                min_inet_down: 100,
                min_disk_gb: tier.min_disk_gb.unwrap_or(60),
                min_cuda: None,
                geo: GeoFilter::Any,
                image_type: image_type_of(tier.image_type.as_deref()),
                extra: serde_json::Map::new(),
            },
        ));
    }

    // ---- [[recipes]] ----------------------------------------------------------------
    let mut recipes = Vec::new();
    let mut forks = Vec::new();
    let mut skips = Vec::new();
    let mut fork_ids = HashSet::new();
    for row in &file.recipes {
        // Genuinely undiscoverable knowledge, harvested regardless of what the row itself
        // becomes: "this model needs that fork" is the one fact `fit()` can never re-derive.
        if let (Some(repo), Some(fork_ref)) = (&row.llama_cpp_repo, &row.llama_cpp_ref) {
            let key = unique_slug(
                row.model_repo.as_deref().unwrap_or(row.name.as_str()),
                &mut fork_ids,
            );
            forks.push((
                row.name.clone(),
                key,
                KnownFork {
                    match_repo: row
                        .model_repo
                        .clone()
                        .map_or_else(|| "*".to_owned(), |r| format!("{r}*")),
                    llama_cpp_repo: repo.clone(),
                    llama_cpp_ref: fork_ref.clone(),
                },
            ));
        }

        let provider = row.provider.as_deref().unwrap_or(DEFAULT_LEGACY_PROVIDER);
        match provider {
            "local" => match local_recipe(row, now, &mut ids) {
                Ok(r) => recipes.push((row.name.clone(), r)),
                Err(why) => skips.push(SkipRow {
                    name: row.name.clone(),
                    reason: why,
                }),
            },
            "together" => match managed_recipe(row, provider, now, &mut ids) {
                Ok(r) => recipes.push((row.name.clone(), r)),
                Err(why) => skips.push(SkipRow {
                    name: row.name.clone(),
                    reason: why,
                }),
            },
            "vllm" => skips.push(SkipRow {
                name: row.name.clone(),
                reason: format!(
                    "frozen vllm row: {} on tier '{}' (kv_cache_dtype={}, enforce_eager={}, \
                     reasoning_parser={}). A vLLM rental is a recipe plus a live search \
                     profile now; the fixed tier is superseded by `fit()`.",
                    row.model_id.as_deref().unwrap_or("?"),
                    row.gpu.as_deref().unwrap_or("?"),
                    row.kv_cache_dtype.as_deref().unwrap_or("default"),
                    // `enforce_eager` is the STRING "true"/"false" in every legacy row.
                    flex_bool(row.enforce_eager.as_ref()),
                    row.reasoning_parser.as_deref().unwrap_or("none"),
                ),
            }),
            _ => skips.push(SkipRow {
                name: row.name.clone(),
                reason: vast_gguf_skip_reason(row, &file.gpu_tiers),
            }),
        }
    }

    for k in &extra_docker_keys {
        skips.push(SkipRow {
            name: format!("docker.{k}"),
            reason: format!(
                "`[docker] {k}` names an image family ApexRouter does not publish; the three \
                 it does — prebuilt, builder, vllm — were imported."
            ),
        });
    }

    Ok(ImportedRecipes {
        recipes,
        profiles,
        forks,
        docker,
        skips,
        tier_notes,
        extra_docker_keys,
    })
}

/// The per-row reason a `vast_gguf` row is not imported.
///
/// It names the tier's arithmetic explicitly — `vram_gb` per GPU × `num_gpus`, and `ctx` as
/// the **total** pool shared across `parallel` slots — because those two are exactly what a
/// human re-deriving the decision gets wrong.
fn vast_gguf_skip_reason(row: &LegacyRecipeRow, tiers: &BTreeMap<String, LegacyTier>) -> String {
    let tier = row.gpu.as_deref().and_then(|g| tiers.get(g));
    let num_gpus = tier.and_then(|t| t.num_gpus).unwrap_or(1).max(1);
    let per_gpu = tier.and_then(|t| t.vram_gb).unwrap_or(0);
    let pooled = u64::from(per_gpu) * u64::from(num_gpus);
    let parallel = row.parallel.unwrap_or(1).max(1);
    let ctx = row.ctx.unwrap_or(0);
    format!(
        "frozen {DEFAULT_LEGACY_PROVIDER} row: {} {} on tier '{}' ({num_gpus}× {per_gpu} GB per \
         GPU = {pooled} GB pooled), ctx {ctx} is the TOTAL pool shared across {parallel} slot(s) \
         (~{} each). Not imported: this table is a hand-computed function of model size, quant \
         and VRAM that `fit()` now computes live against the offer you actually rent.",
        row.model_repo.as_deref().unwrap_or("?"),
        row.model_quant.as_deref().unwrap_or("?"),
        row.gpu.as_deref().unwrap_or("?"),
        ctx / u64::from(parallel),
    )
}

/// Build a `Local` recipe from a legacy `provider = "local"` row.
fn local_recipe(
    row: &LegacyRecipeRow,
    now: i64,
    ids: &mut HashSet<String>,
) -> std::result::Result<Recipe, String> {
    let model_path = row
        .model_path
        .as_deref()
        .filter(|p| !p.trim().is_empty())
        .ok_or_else(|| "local row has no `model_path`".to_owned())?;
    // The legacy row names a backend, never a build directory. `build-<backend>` is the
    // convention on this machine, and `catalog::validate_recipe` reports a miss as a
    // Warning with a fix — which is the right place for that to surface, not here.
    let backend = row.backend.as_deref().unwrap_or("vulkan");
    let build = BuildId::parse(&slugify(&format!("build-{backend}")))
        .map_err(|e| format!("cannot derive a build id from backend {backend:?}: {e}"))?;
    let id = RecipeId::parse(&unique_slug(&row.name, ids))
        .map_err(|e| format!("cannot derive a recipe id from {:?}: {e}", row.name))?;
    let spec = LocalLlamaSpec {
        build,
        model_path: expand_tilde(model_path).display().to_string(),
        mmproj: row
            .mmproj
            .as_ref()
            .map(|m| expand_tilde(m).display().to_string()),
        alias_flag: row.name.clone(),
        host: "127.0.0.1".to_owned(),
        port: row.port,
        ctx: row.ctx.map(clamp_u32),
        parallel: row.parallel,
        kv_type: kv_type_of(row.kv_type.as_deref()),
        ngl: ngl_of(row.n_gpu_layers),
        split: SplitPlan::default(),
        mode: sampling_of(row.mode.as_deref()),
        flash_attn: None,
        api_key: None,
        extra_args: Vec::new(),
    };
    Ok(Recipe {
        id,
        label: row.label.clone().unwrap_or_else(|| row.name.clone()),
        description: row.description.clone(),
        kind: RecipeKind::Local(spec),
        provenance: Provenance2 {
            discovered_at_unix: now,
            size_bytes: None,
            source: format!("imported from recipes.toml row `{}`", row.name),
            fit: None,
        },
        created_at_unix: now,
        updated_at_unix: now,
    })
}

/// Build a `Managed` recipe from a legacy `provider = "together"` row.
fn managed_recipe(
    row: &LegacyRecipeRow,
    provider: &str,
    now: i64,
    ids: &mut HashSet<String>,
) -> std::result::Result<Recipe, String> {
    let provider_id = ProviderId::parse(&slugify(provider))
        .map_err(|e| format!("cannot derive a provider id from {provider:?}: {e}"))?;
    let id = RecipeId::parse(&unique_slug(&row.name, ids))
        .map_err(|e| format!("cannot derive a recipe id from {:?}: {e}", row.name))?;
    let spec = ManagedSpec {
        provider: provider_id,
        base_url: strip_v1(LEGACY_TOGETHER_BASE_URL),
        credential: CredentialSource::Env {
            var: env_var_name(provider),
        },
        model_id: row.model_id.clone(),
        protocol: Protocol::OpenAi,
    };
    let price_note = match (row.price_input, row.price_output) {
        (Some(i), Some(o)) => format!(
            " Legacy prices (${i}/${o} per Mtok) are NOT imported: pricing is a live table fed \
             by the provider, not a constant frozen into a recipe."
        ),
        _ => String::new(),
    };
    Ok(Recipe {
        id,
        label: row.label.clone().unwrap_or_else(|| row.name.clone()),
        description: Some(format!(
            "{}{price_note}",
            row.description.clone().unwrap_or_default()
        )),
        kind: RecipeKind::Managed(spec),
        provenance: Provenance2 {
            discovered_at_unix: now,
            size_bytes: None,
            source: format!("imported from recipes.toml row `{}`", row.name),
            fit: None,
        },
        created_at_unix: now,
        updated_at_unix: now,
    })
}

// ---------------------------------------------------------------------------------------
// the survey: one read-only pass over every legacy artefact
// ---------------------------------------------------------------------------------------

/// What [`apply`] would write for one planned item.
enum Payload {
    /// Informational only.
    Nothing,
    /// A `[providers.<id>]` entry — a credential *reference*, never a key.
    Provider {
        /// The provider id.
        id: String,
        /// Its config entry.
        cfg: ProviderCfg,
    },
    /// The `[docker]` image map.
    Docker(DockerCfg),
    /// `[compat] mirror_usage_log = true` — opt in to writing the legacy usage mirror.
    MirrorUsageLog,
    /// A `[known_forks.<name>]` entry.
    Fork {
        /// The table key.
        name: String,
        /// The mapping.
        fork: KnownFork,
    },
    /// A catalog recipe.
    Recipe(Box<Recipe>),
    /// A catalog search profile.
    Profile(Box<SearchProfile>),
    /// A ledger row.
    Ledger(Box<LedgerRow>),
}

/// One artefact: what the operator sees, and what would be written for it.
struct Planned {
    /// The operator-facing row.
    item: MigrationItem,
    /// What `apply` writes for it.
    payload: Payload,
}

/// The whole survey.
struct Survey {
    /// Every artefact found.
    planned: Vec<Planned>,
    /// Directories read.
    source_paths: Vec<String>,
    /// Things the operator must read regardless of which rows they keep.
    warnings: Vec<String>,
}

/// Enumerate every legacy artefact exactly once. Read-only.
fn survey(paths: &Paths, cfg: &Config) -> Result<Survey> {
    let legacy = paths.legacy();
    let vg = legacy.vastai_gguf.clone();
    let lr = legacy.localrouter_dir.clone();

    let mut planned: Vec<Planned> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut source_paths: Vec<String> = Vec::new();
    if vg.is_dir() {
        source_paths.push(vg.display().to_string());
    }
    if let Some(d) = &lr {
        source_paths.push(d.display().to_string());
    }

    survey_providers(&vg, cfg, &mut planned)?;
    survey_usage_log(&vg, cfg, &mut planned);
    survey_local_logs(&vg, &mut planned);
    survey_instances(&vg, &mut planned)?;
    survey_pinned_provider(&vg, cfg, &mut planned)?;
    survey_credentials(paths, &mut planned);

    if let Some(dir) = &lr {
        survey_active_endpoint(dir, &mut planned)?;
        survey_instance_ids(dir, &mut planned, &mut warnings)?;
        survey_hf_pin(dir, &mut planned)?;
        survey_recipes(dir, cfg, &mut planned, &mut warnings)?;
    }

    Ok(Survey {
        planned,
        source_paths,
        warnings,
    })
}

/// `~/.vastai-gguf/config.toml` — `[providers.*]`, imported as credential *references*.
fn survey_providers(vg: &Path, cfg: &Config, out: &mut Vec<Planned>) -> Result<()> {
    let path = vg.join("config.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(Error::Io {
                path: path.display().to_string(),
                source,
            })
        }
    };
    let doc: toml::Value = match toml::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            out.push(informational(
                "provider",
                &path.display().to_string(),
                MigrationAction::Warn,
                format!("not valid TOML ({e}); left alone — it is not ours to fix"),
            ));
            return Ok(());
        }
    };
    let Some(providers) = doc.get("providers").and_then(toml::Value::as_table) else {
        return Ok(());
    };

    for (id, entry) in providers {
        let base_url = entry
            .get("base_url")
            .and_then(toml::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let has_key = entry
            .get("api_key")
            .and_then(toml::Value::as_str)
            .is_some_and(|k| !k.trim().is_empty());
        let from = format!("{}#providers.{id}", path.display());

        if let Some(existing) = cfg.providers.get(id) {
            out.push(informational(
                "provider",
                &from,
                MigrationAction::Skip,
                format!(
                    "`[providers.{id}]` is already configured (base_url {}); the legacy file \
                     stays readable as step 3 of the credential chain.",
                    existing.base_url
                ),
            ));
            continue;
        }

        let key_note = if has_key {
            format!(
                "A plaintext key is present in the legacy file and is NOT copied: the import \
                 records the reference `${}` instead.",
                env_var_name(id)
            )
        } else {
            "No key is present in the legacy file.".to_owned()
        };
        out.push(Planned {
            item: MigrationItem {
                what: "provider".to_owned(),
                from: from.clone(),
                action: MigrationAction::Import,
                detail: format!(
                    "base_url `{base_url}` is imported VERBATIM and never rewritten. {key_note} \
                     The legacy file remains readable as step 3 of the credential chain, so \
                     nothing breaks if you drop the reference later."
                ),
            },
            payload: Payload::Provider {
                id: id.clone(),
                cfg: ProviderCfg {
                    base_url,
                    api_key_env: Some(env_var_name(id)),
                    api_key_file: None,
                },
            },
        });
    }
    Ok(())
}

/// `~/.vastai-gguf/usage.log` — read in place forever, never copied.
fn survey_usage_log(vg: &Path, cfg: &Config, out: &mut Vec<Planned>) {
    let path = vg.join("usage.log");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let rows = text.lines().filter(|l| !l.trim().is_empty()).count();
    let from = path.display().to_string();
    out.push(informational(
        "usage log",
        &from,
        MigrationAction::Skip,
        format!(
            "{rows} row(s), merged into every usage aggregate IN PLACE while \
             `[compat] read_legacy_state` is {} — there is nothing to copy, and copying would \
             double-count every row.",
            cfg.compat.read_legacy_state
        ),
    ));
    survey_usage_mirror(&path, cfg, out);
}

/// Offer `[compat] mirror_usage_log`, which is **off** unless a human asks for it.
///
/// This is the one place where turning it on is the obvious thing to want: a migration is
/// exactly the transition period during which the old LocalRouter TUI's usage view should
/// keep filling up. Everywhere else, appending to another tool's state file because our
/// daemon started is a surprise, which is why the default is `false` — so the capability is
/// *offered* here rather than buried in a config comment nobody reads.
///
/// The row is `Warn`, not `Import`: keeping it writes outside our own state directory from
/// then on, and a plan that a human strikes rows out of should say so out loud.
fn survey_usage_mirror(usage_log: &Path, cfg: &Config, out: &mut Vec<Planned>) {
    let from = format!("{}#[compat] mirror_usage_log", usage_log.display());
    if cfg.compat.mirror_usage_log {
        out.push(informational(
            "usage mirror",
            &from,
            MigrationAction::Skip,
            "already enabled: every new usage row is appended to this file as well as to \
             ApexRouter's own log. Nothing to do."
                .to_owned(),
        ));
        return;
    }
    out.push(Planned {
        item: MigrationItem {
            what: "usage mirror".to_owned(),
            from,
            action: MigrationAction::Warn,
            detail: format!(
                "OPTIONAL, and OFF by default: keep this row to set `[compat] \
                 mirror_usage_log = true`, so every new usage row is ALSO appended to {} in \
                 the legacy field set and the old LocalRouter TUI's usage view keeps working \
                 during the transition. Keeping it means ApexRouter writes into another \
                 tool's state directory from then on; strike it out to leave that file \
                 untouched. ApexRouter's own usage log is written either way, and the legacy \
                 rows already there are read regardless.",
                usage_log.display()
            ),
        },
        payload: Payload::MirrorUsageLog,
    });
}

/// `~/.vastai-gguf/local_logs/` — offered as history, never moved.
fn survey_local_logs(vg: &Path, out: &mut Vec<Planned>) {
    let dir = vg.join("local_logs");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let n = entries.filter(std::result::Result::is_ok).count();
    out.push(informational(
        "legacy logs",
        &dir.display().to_string(),
        MigrationAction::Skip,
        format!("{n} file(s), offered in the logs view as historical; not copied or rotated."),
    ));
}

/// `~/.vastai-gguf/local_instances/*.json` — importable local recipes, stale or not.
fn survey_instances(vg: &Path, out: &mut Vec<Planned>) -> Result<()> {
    let dir = vg.join("local_instances");
    let now = now_unix();
    let mut ids = HashSet::new();
    for inst in read_legacy_instances(&dir)? {
        let from = inst.source_file.clone();
        let model = inst.model_path.clone().unwrap_or_default();
        let build_name = inst
            .binary
            .as_deref()
            .and_then(build_id_from_binary)
            .unwrap_or_else(|| format!("build-{}", inst.backend.as_deref().unwrap_or("cpu")));
        let build = match BuildId::parse(&slugify(&build_name)) {
            Ok(b) => b,
            Err(e) => {
                out.push(informational(
                    "local instance",
                    &from,
                    MigrationAction::Skip,
                    format!("no usable build id from binary {:?}: {e}", inst.binary),
                ));
                continue;
            }
        };
        let id = match RecipeId::parse(&unique_slug(&inst.name, &mut ids)) {
            Ok(i) => i,
            Err(e) => {
                out.push(informational(
                    "local instance",
                    &from,
                    MigrationAction::Skip,
                    format!("no usable recipe id from name {:?}: {e}", inst.name),
                ));
                continue;
            }
        };
        let action = if inst.model_exists && inst.binary_exists {
            MigrationAction::Import
        } else {
            MigrationAction::Warn
        };
        let detail = format!(
            "saved local endpoint `{}` → recipe. model `{model}` {}; build `{build}` {}. A saved \
             instance pointing at something you deleted is normal, not an error — \
             `apexrouter recipe validate` reports it as a Warning with a fix.",
            inst.name,
            exists_phrase(inst.model_exists),
            exists_phrase(inst.binary_exists),
        );
        let spec = LocalLlamaSpec {
            build,
            model_path: expand_tilde(&model).display().to_string(),
            mmproj: None,
            alias_flag: inst.name.clone(),
            host: inst.host.clone().unwrap_or_else(|| "127.0.0.1".to_owned()),
            port: inst.port,
            ctx: None,
            parallel: None,
            kv_type: None,
            ngl: NglPlan::Auto,
            split: SplitPlan::default(),
            mode: SamplingMode::Thinking,
            flash_attn: None,
            api_key: None,
            extra_args: Vec::new(),
        };
        out.push(Planned {
            item: MigrationItem {
                what: "local instance".to_owned(),
                from,
                action,
                detail,
            },
            payload: Payload::Recipe(Box::new(Recipe {
                id,
                label: inst.name.clone(),
                description: Some(format!(
                    "imported from LocalRouter's saved instance (started {}, {})",
                    inst.started_at.as_deref().unwrap_or("unknown"),
                    inst.status.as_deref().unwrap_or("unknown status"),
                )),
                kind: RecipeKind::Local(spec),
                provenance: Provenance2 {
                    discovered_at_unix: parse_legacy_time(inst.started_at.as_deref())
                        .unwrap_or(now),
                    size_bytes: None,
                    source: inst.source_file.clone(),
                    fit: None,
                },
                created_at_unix: now,
                updated_at_unix: now,
            })),
        });
    }
    Ok(())
}

/// `~/.vastai-gguf/.pinned_provider` — imported once as a managed recipe.
fn survey_pinned_provider(vg: &Path, cfg: &Config, out: &mut Vec<Planned>) -> Result<()> {
    let path = vg.join(".pinned_provider");
    let Some(pin) = read_json_trimmed::<LegacyPinnedProvider>(&path)? else {
        return Ok(());
    };
    let provider = if pin.provider.trim().is_empty() {
        "together".to_owned()
    } else {
        pin.provider.clone()
    };
    let mut ids = HashSet::new();
    let id = match RecipeId::parse(&unique_slug(
        &format!("{provider}-{}", pin.model_id),
        &mut ids,
    )) {
        Ok(i) => i,
        Err(e) => {
            out.push(informational(
                "pinned provider",
                &path.display().to_string(),
                MigrationAction::Skip,
                format!("no usable recipe id: {e}"),
            ));
            return Ok(());
        }
    };
    let provider_id = match ProviderId::parse(&slugify(&provider)) {
        Ok(p) => p,
        Err(e) => {
            out.push(informational(
                "pinned provider",
                &path.display().to_string(),
                MigrationAction::Skip,
                format!("no usable provider id from {provider:?}: {e}"),
            ));
            return Ok(());
        }
    };
    // Verbatim: whatever URL the configured provider carries, else the legacy file's own.
    let base_url = strip_v1(
        cfg.providers
            .get(&provider)
            .map(|p| p.base_url.as_str())
            .filter(|u| !u.trim().is_empty())
            .unwrap_or(pin.base_url.as_str()),
    );
    let now = now_unix();
    out.push(Planned {
        item: MigrationItem {
            what: "pinned provider".to_owned(),
            from: path.display().to_string(),
            action: MigrationAction::Import,
            detail: format!(
                "pinned `{}` on `{provider}` at `{base_url}` → managed recipe. The base URL is \
                 used verbatim and the credential is a reference, never a copied key.",
                pin.model_id
            ),
        },
        payload: Payload::Recipe(Box::new(Recipe {
            id,
            label: format!("{provider}: {}", pin.model_id),
            description: Some("imported from ~/.vastai-gguf/.pinned_provider".to_owned()),
            kind: RecipeKind::Managed(ManagedSpec {
                provider: provider_id,
                base_url,
                credential: credential_for(cfg, &provider),
                model_id: Some(pin.model_id.clone()),
                protocol: Protocol::OpenAi,
            }),
            provenance: Provenance2 {
                discovered_at_unix: now,
                size_bytes: None,
                source: path.display().to_string(),
                fit: None,
            },
            created_at_unix: now,
            updated_at_unix: now,
        })),
    });
    Ok(())
}

/// The two third-party credential files: read where they are, never copied.
fn survey_credentials(paths: &Paths, out: &mut Vec<Planned>) {
    for (what, p) in [
        ("vast credential", paths.legacy().vast_key.clone()),
        ("hf credential", paths.legacy().hf_token.clone()),
    ] {
        if !p.is_file() {
            continue;
        }
        out.push(informational(
            what,
            &p.display().to_string(),
            MigrationAction::Skip,
            "present, and read IN PLACE at its owner's conventional path. Never copied into \
             ApexRouter's state: only a key you type is ever written, and it goes to \
             `credentials.toml` at mode 0600."
                .to_owned(),
        ));
    }
}

/// `<LocalRouter>/.active_endpoint`, all four shapes.
fn survey_active_endpoint(lr: &Path, out: &mut Vec<Planned>) -> Result<()> {
    let path = lr.join(".active_endpoint");
    let Some(ep) = read_legacy_active_endpoint(&path)? else {
        return Ok(());
    };
    let what = if ep.provider == "local" {
        format!(
            "local `{}` on {}:{}",
            ep.name.as_deref().unwrap_or("?"),
            ep.host.as_deref().unwrap_or("127.0.0.1"),
            ep.port.unwrap_or(0)
        )
    } else {
        format!(
            "`{}` at `{}`",
            ep.model_id.as_deref().unwrap_or("?"),
            ep.base_url.as_deref().unwrap_or("?")
        )
    };
    out.push(informational(
        "active endpoint",
        &path.display().to_string(),
        MigrationAction::Skip,
        format!(
            "LocalRouter last activated {what} on provider `{}` at {}{}{}. Informational only: \
             the default route is set explicitly with `apexrouter route set default <alias>`, \
             never inherited from a stale file.",
            ep.provider,
            ep.activated_at.as_deref().unwrap_or("an unknown time"),
            ep.pid
                .map(|p| format!(" (pid {p}, long gone)"))
                .unwrap_or_default(),
            if ep.api_key_present {
                " — it carried a plaintext api_key, discarded on read and absent from this plan"
            } else {
                ""
            },
        ),
    ));
    Ok(())
}

/// `<LocalRouter>/.last_instance` and `.instance_history` — money that may still be running.
fn survey_instance_ids(
    lr: &Path,
    out: &mut Vec<Planned>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let last_path = lr.join(".last_instance");
    let last = read_trimmed(&last_path)?.and_then(|s| s.parse::<u64>().ok());
    let now = now_unix();

    if let Some(id) = last {
        warnings.push(format!(
            "`.last_instance` still names vast instance {id}. LocalRouter deletes that file when \
             it destroys an instance, so this one may STILL BE BILLING. It is imported as a \
             Confirmed ledger row so `apexrouter vast ls` keeps it visible until you verify it."
        ));
        out.push(Planned {
            item: MigrationItem {
                what: "vast instance".to_owned(),
                from: last_path.display().to_string(),
                action: MigrationAction::Warn,
                detail: format!(
                    "instance {id} → ledger row `Confirmed` (not `Reconciled`: we have not asked \
                     vast whether it is alive). It stays in `ledger.active()` until a destroy is \
                     verified, which is exactly the point of the ledger."
                ),
            },
            payload: Payload::Ledger(Box::new(LedgerRow {
                seq: 0,
                at_unix: now,
                instance_id: Some(InstanceId(id)),
                state: LedgerState::Confirmed,
                offer_id: None,
                profile: None,
                gpu: None,
                num_gpus: None,
                dph: None,
                approved_max_dph: None,
                approval_source: Some("migrate".to_owned()),
                destroyed_at_unix: None,
                est_cost: CostEstimate::Unknown,
                note: Some(format!("imported from {}", last_path.display())),
            })),
        });
    }

    let hist_path = lr.join(".instance_history");
    for (at, id) in read_instance_history(&hist_path)? {
        if Some(id) == last {
            continue;
        }
        let at_unix = at.unwrap_or(now);
        out.push(Planned {
            item: MigrationItem {
                what: "vast instance".to_owned(),
                from: format!("{}#{id}", hist_path.display()),
                action: MigrationAction::Warn,
                detail: format!(
                    "instance {id}, created {}, superseded by a later launch → ledger row \
                     `Destroyed`. LocalRouter never recorded the destruction, so the \
                     destroyed-at timestamp is ASSUMED, not observed, and the row says so.",
                    at.map_or_else(|| "?".to_owned(), |t| t.to_string())
                ),
            },
            payload: Payload::Ledger(Box::new(LedgerRow {
                seq: 0,
                at_unix,
                instance_id: Some(InstanceId(id)),
                state: LedgerState::Destroyed,
                offer_id: None,
                profile: None,
                gpu: None,
                num_gpus: None,
                dph: None,
                approved_max_dph: None,
                approval_source: Some("migrate".to_owned()),
                destroyed_at_unix: Some(at_unix),
                est_cost: CostEstimate::Unknown,
                note: Some(format!(
                    "imported from {}; destruction assumed, never observed",
                    hist_path.display()
                )),
            })),
        });
    }
    Ok(())
}

/// `<LocalRouter>/.hf_pin` — a wizard default, not durable state.
fn survey_hf_pin(lr: &Path, out: &mut Vec<Planned>) -> Result<()> {
    let path = lr.join(".hf_pin");
    let Some(pin) = read_json_trimmed::<LegacyHfPin>(&path)? else {
        return Ok(());
    };
    out.push(informational(
        "hf pin",
        &path.display().to_string(),
        MigrationAction::Skip,
        format!(
            "MODEL_REPO={} MODEL_QUANT={} ({}{}). A transient wizard default, not durable state \
             — re-pin it with `apexrouter hf get {} --quant {}`.",
            pin.model_repo,
            pin.model_quant,
            pin.filename,
            if pin.size.is_empty() {
                String::new()
            } else {
                format!(", {}", pin.size)
            },
            pin.model_repo,
            pin.model_quant,
        ),
    ));
    Ok(())
}

/// `<LocalRouter>/recipes.toml` — the big one.
fn survey_recipes(
    lr: &Path,
    cfg: &Config,
    out: &mut Vec<Planned>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let path = lr.join("recipes.toml");
    if !path.is_file() {
        return Ok(());
    }
    let imported = import_recipes_detail(&path)?;
    let p = path.display().to_string();

    let docker_from = format!("{p}#docker");
    if imported.docker == cfg.docker {
        out.push(informational(
            "docker images",
            &docker_from,
            MigrationAction::Skip,
            "identical to the configured `[docker]` map; nothing to do.".to_owned(),
        ));
    } else {
        out.push(Planned {
            item: MigrationItem {
                what: "docker images".to_owned(),
                from: docker_from,
                action: MigrationAction::Import,
                detail: format!(
                    "prebuilt={} builder={} vllm={}{}",
                    imported.docker.prebuilt,
                    imported.docker.builder,
                    imported.docker.vllm,
                    if imported.extra_docker_keys.is_empty() {
                        String::new()
                    } else {
                        format!(" (ignored: {})", imported.extra_docker_keys.join(", "))
                    }
                ),
            },
            payload: Payload::Docker(imported.docker.clone()),
        });
    }

    for (row_name, key, fork) in imported.forks {
        let from = format!("{p}#recipes.{row_name}.llama_cpp_repo");
        if cfg.known_forks.values().any(|k| *k == fork) {
            out.push(informational(
                "known_fork",
                &from,
                MigrationAction::Skip,
                format!(
                    "`{}` @ `{}` for `{}` is already configured.",
                    fork.llama_cpp_repo, fork.llama_cpp_ref, fork.match_repo
                ),
            ));
            continue;
        }
        out.push(Planned {
            item: MigrationItem {
                what: "known_fork".to_owned(),
                from,
                action: MigrationAction::Import,
                detail: format!(
                    "`{}` needs `{}` @ `{}`. Genuinely undiscoverable knowledge — no amount of \
                     probing derives it — and a hit forces the builder image (+12–18 min cold \
                     start).",
                    fork.match_repo, fork.llama_cpp_repo, fork.llama_cpp_ref
                ),
            },
            payload: Payload::Fork { name: key, fork },
        });
    }

    let n_profiles = imported.profiles.len();
    for (tier_key, profile) in imported.profiles {
        out.push(Planned {
            item: MigrationItem {
                what: "search profile".to_owned(),
                from: format!("{p}#gpu_tiers.{tier_key}"),
                action: MigrationAction::Warn,
                detail: format!(
                    "→ profile `{}` ({}). {} Seeded as a QUERY TEMPLATE, not a fixed tier: \
                     `gpu_name` strings change under you, so widen `num_gpus_min`/`num_gpus_max` \
                     rather than keeping one profile per GPU count.",
                    profile.id,
                    profile.label,
                    imported
                        .tier_notes
                        .get(&tier_key)
                        .cloned()
                        .unwrap_or_default(),
                ),
            },
            payload: Payload::Profile(Box::new(profile)),
        });
    }
    if n_profiles > 0 {
        warnings.push(format!(
            "{n_profiles} search profiles were seeded from fixed legacy GPU tiers. One tier per \
             GPU count is exactly what `num_gpus_min`/`num_gpus_max` ranges replace — merge them \
             once you have looked at a live search."
        ));
    }

    for (row_name, mut recipe) in imported.recipes {
        // The one fact `import_recipes_toml` cannot know from one file: the configured
        // provider's base URL and credential reference. Applied here, verbatim.
        if let RecipeKind::Managed(spec) = &mut recipe.kind {
            let id = spec.provider.to_string();
            if let Some(pc) = cfg.providers.get(&id) {
                if !pc.base_url.trim().is_empty() {
                    spec.base_url = strip_v1(&pc.base_url);
                }
            }
            spec.credential = credential_for(cfg, &id);
        }
        let stale = match &recipe.kind {
            RecipeKind::Local(s) => !Path::new(&s.model_path).is_file(),
            _ => false,
        };
        let detail = match &recipe.kind {
            RecipeKind::Local(s) => format!(
                "local recipe `{}` → build `{}`, model `{}`{}. The build id is derived from the \
                 legacy `backend` key, which names a backend and not a build directory — \
                 `apexrouter recipe validate` will say so if it is wrong.",
                recipe.id,
                s.build,
                s.model_path,
                if stale { " (MODEL IS GONE)" } else { "" }
            ),
            RecipeKind::Managed(s) => format!(
                "managed recipe `{}` → `{}` at `{}` (verbatim), credential by reference. Legacy \
                 per-token prices are not imported.",
                recipe.id,
                s.model_id.as_deref().unwrap_or("?"),
                s.base_url
            ),
            _ => format!("recipe `{}`", recipe.id),
        };
        out.push(Planned {
            item: MigrationItem {
                what: "recipe".to_owned(),
                from: format!("{p}#recipes.{row_name}"),
                action: if stale {
                    MigrationAction::Warn
                } else {
                    MigrationAction::Import
                },
                detail,
            },
            payload: Payload::Recipe(Box::new(recipe)),
        });
    }

    for skip in imported.skips {
        out.push(informational(
            "recipe",
            &format!("{p}#recipes.{}", skip.name),
            MigrationAction::Skip,
            skip.reason,
        ));
    }
    Ok(())
}

/// A planned item that writes nothing.
fn informational(what: &str, from: &str, action: MigrationAction, detail: String) -> Planned {
    Planned {
        item: MigrationItem {
            what: what.to_owned(),
            from: from.to_owned(),
            action,
            detail,
        },
        payload: Payload::Nothing,
    }
}

/// "still exists" / "NO LONGER EXISTS", for a plan row a human reads.
fn exists_phrase(exists: bool) -> &'static str {
    if exists {
        "still exists"
    } else {
        "NO LONGER EXISTS"
    }
}

// ---------------------------------------------------------------------------------------
// the legacy scalar files
// ---------------------------------------------------------------------------------------

/// Read a one-value legacy file, **trimming the trailing newline** `echo` put there.
///
/// `vast_up.sh` writes `echo "${INST_ID}" > .last_instance`, so every one of these files ends
/// in `\n` (and on a Windows-edited checkout, `\r\n`). An untrimmed read turns
/// `"1234\n".parse::<u64>()` into an error and a possibly-billing instance into a ghost.
fn read_trimmed(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(t) => {
            let t = t.trim().to_owned();
            Ok((!t.is_empty()).then_some(t))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::Io {
            path: path.display().to_string(),
            source,
        }),
    }
}

/// Read a JSON legacy file, tolerating the trailing newline and an empty file.
fn read_json_trimmed<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let Some(text) = read_trimmed(path)? else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_str(&text)?))
}

/// Read `.instance_history`: `printf '%s\t%s\n'` of an RFC-3339-ish timestamp and an id.
///
/// Blank lines, a trailing newline and a line that is only an id (older writers) all parse.
/// A line that yields no id at all is logged and skipped rather than failing the file.
fn read_instance_history(path: &Path) -> Result<Vec<(Option<i64>, u64)>> {
    let Some(text) = read_trimmed(path)? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split('\t').map(str::trim).filter(|f| !f.is_empty());
        let (at, id) = match (fields.next(), fields.next()) {
            (Some(a), Some(b)) => (parse_legacy_time(Some(a)), b.parse::<u64>().ok()),
            (Some(a), None) => (None, a.parse::<u64>().ok()),
            _ => (None, None),
        };
        match id {
            Some(id) => out.push((at, id)),
            None => {
                tracing::warn!(path = %path.display(), line, "unparseable instance history line");
            }
        }
    }
    Ok(out)
}

/// `~/.vastai-gguf/.pinned_provider`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct LegacyPinnedProvider {
    /// `"together"`.
    provider: String,
    /// The upstream model id.
    model_id: String,
    /// Used verbatim.
    base_url: String,
}

/// `<LocalRouter>/.hf_pin`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct LegacyHfPin {
    /// HF repo id.
    #[serde(rename = "MODEL_REPO")]
    model_repo: String,
    /// Quant tag.
    #[serde(rename = "MODEL_QUANT")]
    model_quant: String,
    /// The exact file.
    filename: String,
    /// Human size string.
    size: String,
}

// ---------------------------------------------------------------------------------------
// recipes.toml shapes
// ---------------------------------------------------------------------------------------

/// The whole legacy `recipes.toml`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct LegacyRecipesFile {
    /// The `[docker]` image map, keys unconstrained.
    docker: BTreeMap<String, String>,
    /// `[gpu_tiers.<key>]`.
    gpu_tiers: BTreeMap<String, LegacyTier>,
    /// `[[recipes]]`.
    recipes: Vec<LegacyRecipeRow>,
}

/// `[gpu_tiers.<key>]`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct LegacyTier {
    /// Underscored vast names, e.g. `"H100_SXM"`.
    vast_names: Vec<String>,
    /// Human label, with a stale price baked into it.
    label: Option<String>,
    /// **A quoted string** in every legacy row, e.g. `"3.50"`.
    max_price: Option<toml::Value>,
    /// Disk floor.
    min_disk_gb: Option<u32>,
    /// `"prebuilt"` / `"builder"` / `"vllm"`.
    image_type: Option<String>,
    /// **PER GPU.** Multiply by `num_gpus` for the pooled figure.
    vram_gb: Option<u32>,
    /// Absent on the single-GPU tiers.
    num_gpus: Option<u32>,
}

/// One `[[recipes]]` row. Every field optional: 54 of 71 rows omit `provider` alone.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct LegacyRecipeRow {
    /// The row's handle.
    name: String,
    /// Absent on 54 of 71 rows, where it means `vast_gguf`.
    provider: Option<String>,
    /// Human label.
    label: Option<String>,
    /// Free text.
    description: Option<String>,
    /// The `gpu_tiers` key.
    gpu: Option<String>,
    /// HF repo of the GGUF.
    model_repo: Option<String>,
    /// Quant tag.
    model_quant: Option<String>,
    /// Managed/vLLM model id.
    model_id: Option<String>,
    /// Local GGUF path, often `~/…`.
    model_path: Option<String>,
    /// Vision projector.
    mmproj: Option<String>,
    /// **The TOTAL context pool**, shared across `parallel` slots.
    ctx: Option<u64>,
    /// Slot count.
    parallel: Option<u32>,
    /// llama.cpp `-ctk`/`-ctv`.
    kv_type: Option<String>,
    /// vLLM's own KV dtype.
    kv_cache_dtype: Option<String>,
    /// Image family.
    image_type: Option<String>,
    /// Fork repo.
    llama_cpp_repo: Option<String>,
    /// Fork ref.
    llama_cpp_ref: Option<String>,
    /// Local port.
    port: Option<u16>,
    /// `999` means "all".
    n_gpu_layers: Option<i64>,
    /// `"vulkan"`, `"cuda"`, …
    backend: Option<String>,
    /// Sampling preset name.
    mode: Option<String>,
    /// **The string `"true"`/`"false"`**, not a bool.
    enforce_eager: Option<toml::Value>,
    /// vLLM reasoning parser.
    reasoning_parser: Option<String>,
    /// Legacy $/Mtok. Not imported.
    price_input: Option<f64>,
    /// Legacy $/Mtok. Not imported.
    price_output: Option<f64>,
}

// ---------------------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------------------

/// Parse a legacy `enforce_eager`: the **string** `"true"`/`"false"` on every real row.
///
/// `true`, `1` and `yes` are true, case-insensitively; a real bool is honoured; anything else
/// — including a typo — is false, which is the safe direction for a flag whose only effect is
/// to disable CUDA graphs.
fn flex_bool(v: Option<&toml::Value>) -> bool {
    match v {
        Some(toml::Value::Boolean(b)) => *b,
        Some(toml::Value::Integer(i)) => *i != 0,
        Some(toml::Value::String(s)) => {
            matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes")
        }
        _ => false,
    }
}

/// Parse a legacy `max_price`: a **quoted string** on every real row, tolerated as a number.
fn flex_f64(v: Option<&toml::Value>) -> Option<f64> {
    match v {
        Some(toml::Value::Float(f)) => Some(*f),
        Some(toml::Value::Integer(i)) => Some(*i as f64),
        Some(toml::Value::String(s)) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// Legacy `vast_names` are underscored (`"H100_SXM"`); the live vocabulary verified in
/// `docs/port/00c` is spaced (`"H100 SXM"`). Rewrite the separator, never the name.
fn vast_gpu_name(legacy: &str) -> String {
    legacy.trim().replace('_', " ")
}

/// `"prebuilt"` / `"builder"` / `"vllm"`, defaulting to prebuilt.
fn image_type_of(s: Option<&str>) -> ImageType {
    match s.map(str::trim).unwrap_or_default() {
        "builder" => ImageType::Builder,
        "vllm" => ImageType::Vllm,
        _ => ImageType::Prebuilt,
    }
}

/// The `-ctk`/`-ctv` spelling, which is exactly [`KvType`]'s serde spelling.
fn kv_type_of(s: Option<&str>) -> Option<KvType> {
    let s = s?.trim().to_ascii_lowercase();
    serde_json::from_value(serde_json::Value::String(s)).ok()
}

/// `999` — LocalRouter's idiom for "all" — becomes [`NglPlan::All`].
fn ngl_of(n: Option<i64>) -> NglPlan {
    match n {
        None => NglPlan::Auto,
        Some(n) if n >= 999 => NglPlan::All,
        Some(n) if n <= 0 => NglPlan::Layers(0),
        Some(n) => NglPlan::Layers(clamp_u32(n.unsigned_abs())),
    }
}

/// The legacy `mode` string, defaulting to `thinking` exactly as `launch.sh` does.
fn sampling_of(s: Option<&str>) -> SamplingMode {
    match s.map(str::trim).unwrap_or_default() {
        "coding" => SamplingMode::Coding,
        "nonthinking" | "non-thinking" => SamplingMode::Nonthinking,
        "raw" => SamplingMode::Raw,
        _ => SamplingMode::Thinking,
    }
}

/// Saturating `u64` → `u32`. Legacy `ctx` values are large but never near `u32::MAX`.
fn clamp_u32(v: u64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

/// `/home/andre/llama.cpp/build-vulkan/bin/llama-server` → `build-vulkan`.
///
/// A [`BuildId`] *is* the build-directory name, so this is a lookup, not a guess.
fn build_id_from_binary(bin: &str) -> Option<String> {
    let p = expand_tilde(bin);
    let bin_dir = p.parent()?; // …/bin
    let build_dir = bin_dir.parent()?; // …/build-vulkan
    build_dir.file_name()?.to_str().map(str::to_owned)
}

/// Strip exactly one trailing `/v1`, never anything else.
///
/// [`ManagedSpec::base_url`] is stored without it. The **host** is never touched:
/// `api.together.xyz` stays `api.together.xyz`.
fn strip_v1(url: &str) -> String {
    let t = url.trim().trim_end_matches('/');
    t.strip_suffix("/v1").unwrap_or(t).to_owned()
}

/// `together` → `TOGETHER_API_KEY`. The conventional env var for a provider id.
fn env_var_name(id: &str) -> String {
    let mut s: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    s.push_str("_API_KEY");
    s
}

/// Where a provider's key lives, as a **reference**. Never key material.
fn credential_for(cfg: &Config, id: &str) -> CredentialSource {
    let fallback = || CredentialSource::Env {
        var: env_var_name(id),
    };
    match cfg.providers.get(id) {
        Some(p) => match (&p.api_key_env, &p.api_key_file) {
            (Some(var), _) if !var.trim().is_empty() => CredentialSource::Env { var: var.clone() },
            (_, Some(path)) if !path.trim().is_empty() => {
                CredentialSource::File { path: path.clone() }
            }
            _ => fallback(),
        },
        None => fallback(),
    }
}

/// Reduce any string to the id charset `^[a-z0-9][a-z0-9._-]{0,63}$`.
///
/// Ids are **generated**, never typed: this is the generator for an imported row, and a
/// legacy `name` is its input, not its authority.
fn slugify(s: &str) -> String {
    let mut out = String::new();
    for ch in s.chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_' || c == '-' {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    trim_id_tail(&mut out);
    // Nothing survived the charset: name it, rather than emitting a one-character id that
    // looks like truncation damage.
    if out.is_empty() {
        return "imported".to_owned();
    }
    if !out.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit()) {
        out.insert_str(0, "r-");
    }
    out.truncate(64);
    trim_id_tail(&mut out);
    out
}

/// Drop trailing separators, which read as truncation damage in an id.
fn trim_id_tail(s: &mut String) {
    while s.ends_with('-') || s.ends_with('.') {
        s.pop();
    }
}

/// [`slugify`], made unique within one import by a numeric suffix.
fn unique_slug(s: &str, used: &mut HashSet<String>) -> String {
    let base = slugify(s);
    if used.insert(base.clone()) {
        return base;
    }
    for n in 2..10_000u32 {
        let suffix = format!("-{n}");
        let mut cand = base.clone();
        if cand.len() + suffix.len() > 64 {
            cand.truncate(64 - suffix.len());
        }
        cand.push_str(&suffix);
        if used.insert(cand.clone()) {
            return cand;
        }
    }
    base
}

/// `~` and `~/…` against `$HOME`; any other form is returned unchanged.
fn expand_tilde(s: &str) -> PathBuf {
    if s == "~" {
        if let Some(h) = dirs::home_dir() {
            return h;
        }
    } else if let Some(rest) = s.strip_prefix("~/") {
        if let Some(h) = dirs::home_dir() {
            return h.join(rest);
        }
    }
    PathBuf::from(s)
}

/// Parse a legacy timestamp leniently.
///
/// LocalRouter writes `%Y-%m-%dT%H:%M:%SZ` — and `providers.py` writes **local** time with a
/// lying `Z`. The offset is unrecoverable, so the `Z` is believed and the result is only ever
/// used for ordering, never for billing.
fn parse_legacy_time(s: Option<&str>) -> Option<i64> {
    let s = s?.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp());
    }
    let naive = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%SZ")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S"))
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S"))
        .ok()?;
    Some(naive.and_utc().timestamp())
}

/// Wall clock, unix seconds.
fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::ffi::OsString;
    use std::fs;
    use std::sync::{Mutex, OnceLock};

    // -----------------------------------------------------------------------------------
    // harness
    // -----------------------------------------------------------------------------------

    /// `Paths::resolve()` reads the process-global environment, so the tests that redirect
    /// it serialise here and put every variable back before the lock is released.
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// The variables that decide where `Paths` lands. Saved and restored as a set.
    const ENV_KEYS: [&str; 7] = [
        "HOME",
        "APEXROUTER_HOME",
        "APEXROUTER_LOCALROUTER_DIR",
        "APEXROUTER_CONFIG",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
        "XDG_CONFIG_HOME",
    ];

    /// A `Paths` rooted in `home`, which is a tempdir laid out as `$HOME`.
    ///
    /// `$APEXROUTER_HOME` puts state inside it and `$APEXROUTER_LOCALROUTER_DIR` points the
    /// LocalRouter probe at our fixture, so nothing here can read or write the real
    /// `~/.vastai-gguf` or the real checkout.
    fn test_paths(home: &Path, localrouter: Option<&Path>) -> Paths {
        let guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let saved: Vec<(&str, Option<OsString>)> =
            ENV_KEYS.iter().map(|k| (*k, std::env::var_os(k))).collect();
        for k in ENV_KEYS {
            std::env::remove_var(k);
        }
        std::env::set_var("HOME", home);
        std::env::set_var("APEXROUTER_HOME", home.join("state"));
        std::env::set_var(
            "APEXROUTER_LOCALROUTER_DIR",
            localrouter.map_or_else(|| home.join("no-localrouter"), Path::to_path_buf),
        );
        let p = Paths::resolve();
        for (k, v) in saved {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        drop(guard);

        let p = p.expect("Paths::resolve");
        assert!(
            p.state().starts_with(home),
            "state {} escaped the tempdir {}",
            p.state().display(),
            home.display()
        );
        p
    }

    /// Content hash of a whole directory tree: every relative path and every byte.
    fn tree_hash(root: &Path) -> String {
        fn walk(dir: &Path, root: &Path, acc: &mut Vec<(String, Vec<u8>)>) {
            let Ok(rd) = fs::read_dir(dir) else { return };
            let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
            entries.sort();
            for p in entries {
                let rel = p.strip_prefix(root).unwrap_or(&p).display().to_string();
                if p.is_dir() {
                    acc.push((format!("d:{rel}"), Vec::new()));
                    walk(&p, root, acc);
                } else {
                    acc.push((format!("f:{rel}"), fs::read(&p).unwrap_or_default()));
                }
            }
        }
        let mut acc = Vec::new();
        walk(root, root, &mut acc);
        let mut h = Sha256::new();
        for (name, bytes) in acc {
            h.update(name.as_bytes());
            h.update(b"\0");
            h.update(&bytes);
            h.update(b"\0");
        }
        format!("{:x}", h.finalize())
    }

    fn write(path: &Path, body: &str) {
        if let Some(d) = path.parent() {
            fs::create_dir_all(d).expect("mkdir");
        }
        fs::write(path, body).expect("write");
    }

    /// A `recipes.toml` with exactly the legacy shape: 54 provider-less `vast_gguf` rows,
    /// 7 `vllm`, 7 `together`, 3 `local`, 7 fork mappings, 19 tiers, 4 `[docker]` keys.
    fn fixture_recipes_toml(model_path: &str) -> String {
        let mut s = String::from(
            "[docker]\n\
             prebuilt = \"ghcr.io/x:prebuilt\"\n\
             builder = \"ghcr.io/x:builder\"\n\
             prebuilt_legacy = \"ghcr.io/x:old\"\n\
             vllm = \"ghcr.io/x:vllm\"\n\n",
        );
        // 19 tiers; the first 9 are multi-GPU and carry `num_gpus`.
        for i in 0..19 {
            s.push_str(&format!(
                "[gpu_tiers.tier{i}]\n\
                 vast_names = [\"H100_SXM\", \"H100_SXM5\"]\n\
                 label = \"tier {i}\"\n\
                 max_price = \"3.50\"\n\
                 min_disk_gb = 100\n\
                 image_type = \"builder\"\n\
                 vram_gb = 80\n"
            ));
            if i < 9 {
                s.push_str(&format!("num_gpus = {}\n", i + 2));
            }
            s.push('\n');
        }
        // 54 rows with NO `provider` key; 7 of them carry a fork mapping.
        for i in 0..54 {
            s.push_str(&format!(
                "[[recipes]]\n\
                 name = \"vast-row-{i}\"\n\
                 label = \"vast row {i}\"\n\
                 gpu = \"tier0\"\n\
                 model_repo = \"Preyazz/DeepSeek-V4-Flash-GGUF\"\n\
                 model_quant = \"Q4_K_M\"\n\
                 ctx = 262144\n\
                 parallel = 2\n\
                 kv_type = \"q8_0\"\n\
                 description = \"row {i}\"\n"
            ));
            if i < 7 {
                s.push_str(
                    "llama_cpp_repo = \"fairydreaming/llama.cpp\"\n\
                     llama_cpp_ref = \"deepseek-dsa\"\n",
                );
            }
            s.push('\n');
        }
        // 7 vllm rows, `enforce_eager` as the STRING "true"/"false".
        for i in 0..7 {
            let eager = if i % 2 == 0 { "true" } else { "false" };
            s.push_str(&format!(
                "[[recipes]]\n\
                 name = \"vllm-row-{i}\"\n\
                 provider = \"vllm\"\n\
                 label = \"vllm row {i}\"\n\
                 gpu = \"tier1\"\n\
                 model_id = \"deepseek-ai/DeepSeek-V4-Pro\"\n\
                 ctx = 393216\n\
                 image_type = \"vllm\"\n\
                 kv_cache_dtype = \"fp8\"\n\
                 enforce_eager = \"{eager}\"\n\
                 reasoning_parser = \"deepseek_r1\"\n\
                 description = \"vllm {i}\"\n\n"
            ));
        }
        // 7 together rows.
        for i in 0..7 {
            s.push_str(&format!(
                "[[recipes]]\n\
                 name = \"together-row-{i}\"\n\
                 provider = \"together\"\n\
                 label = \"together row {i}\"\n\
                 model_id = \"meta-llama/Llama-3.1-8B-Instruct-Turbo\"\n\
                 ctx = 131072\n\
                 price_input = 0.18\n\
                 price_output = 0.18\n\
                 description = \"together {i}\"\n\n"
            ));
        }
        // 3 local rows; the first points at a model that exists.
        for i in 0..3 {
            let mp = if i == 0 {
                model_path
            } else {
                "~/models/gone.gguf"
            };
            s.push_str(&format!(
                "[[recipes]]\n\
                 name = \"local-row-{i}\"\n\
                 provider = \"local\"\n\
                 label = \"local row {i}\"\n\
                 model_path = \"{mp}\"\n\
                 port = 810{i}\n\
                 ctx = 32768\n\
                 parallel = 1\n\
                 kv_type = \"q8_0\"\n\
                 n_gpu_layers = 999\n\
                 backend = \"vulkan\"\n\
                 mode = \"coding\"\n\
                 description = \"local {i}\"\n\n"
            ));
        }
        s.push_str("[local]\n");
        s
    }

    // -----------------------------------------------------------------------------------
    // .active_endpoint — all four shapes
    // -----------------------------------------------------------------------------------

    #[test]
    fn all_four_active_endpoint_shapes_deserialise() {
        let dir = tempfile::tempdir().expect("tempdir");

        // 1 — together, `activated_at`, no pid  (localrouter/providers.py)
        let p1 = dir.path().join("s1.json");
        write(
            &p1,
            r#"{"provider":"together","model_id":"meta-llama/Llama-3.1-8B-Instruct-Turbo",
                "base_url":"https://api.together.xyz/v1",
                "endpoint":"https://api.together.xyz/v1/chat/completions",
                "activated_at":"2026-05-02T19:36:00Z"}"#,
        );
        let a = read_legacy_active_endpoint(&p1)
            .expect("read")
            .expect("some");
        assert_eq!(a.provider, "together");
        assert_eq!(a.activated_at.as_deref(), Some("2026-05-02T19:36:00Z"));
        assert_eq!(a.pid, None);
        assert!(!a.api_key_present);
        // The host survives byte-for-byte: `.xyz` is never rewritten to `.ai`.
        assert_eq!(a.base_url.as_deref(), Some("https://api.together.xyz/v1"));

        // 2 — together, `switched_at`  (endpoint_proxy.py /switch)
        let p2 = dir.path().join("s2.json");
        write(
            &p2,
            r#"{"provider":"together","model_id":"Qwen/QwQ-32B-Preview",
                "base_url":"https://api.together.ai/v1",
                "endpoint":"https://api.together.ai/v1/chat/completions",
                "switched_at":"2026-05-03T00:00:01Z"}"#,
        );
        let b = read_legacy_active_endpoint(&p2)
            .expect("read")
            .expect("some");
        assert_eq!(
            b.activated_at.as_deref(),
            Some("2026-05-03T00:00:01Z"),
            "`switched_at` must alias onto `activated_at`"
        );
        assert_eq!(b.model_id.as_deref(), Some("Qwen/QwQ-32B-Preview"));

        // 3 — local, `activated_at`, WITH pid and a plaintext api_key
        //     (localrouter/local_endpoint.py)
        let p3 = dir.path().join("s3.json");
        write(
            &p3,
            r#"{"provider":"local","name":"local-qwen35-9b","host":"127.0.0.1","port":8100,
                "pid":649035,"model_path":"~/models/Qwen3.5-9B-Q4_K_M.gguf",
                "activated_at":"2026-05-03T00:34:36Z","api_key":"sk-super-secret-value"}"#,
        );
        let c = read_legacy_active_endpoint(&p3)
            .expect("read")
            .expect("some");
        assert_eq!(c.provider, "local");
        assert_eq!(c.pid, Some(649_035));
        assert_eq!(c.port, Some(8100));
        assert!(c.api_key_present);
        let json = serde_json::to_string(&c).expect("ser");
        assert!(
            !json.contains("sk-super-secret-value"),
            "key material must never survive into a serialisable plan: {json}"
        );
        assert!(json.contains("\"api_key\":true"));

        // 4 — local, `switched_at`, NO pid  (endpoint_proxy.py /switch)
        let p4 = dir.path().join("s4.json");
        write(
            &p4,
            r#"{"provider":"local","name":"local-carnice-9b","host":"127.0.0.1","port":8102,
                "model_path":"~/models/carnice-9b/Carnice-9b-Q6_K.gguf",
                "switched_at":"2026-05-05T15:51:00Z"}"#,
        );
        let d = read_legacy_active_endpoint(&p4)
            .expect("read")
            .expect("some");
        assert_eq!(d.provider, "local");
        assert_eq!(d.pid, None);
        assert_eq!(d.name.as_deref(), Some("local-carnice-9b"));
        assert_eq!(d.activated_at.as_deref(), Some("2026-05-05T15:51:00Z"));

        // Absent and empty are not errors.
        assert_eq!(
            read_legacy_active_endpoint(&dir.path().join("nope.json")).expect("read"),
            None
        );
        let empty = dir.path().join("empty.json");
        write(&empty, "\n");
        assert_eq!(read_legacy_active_endpoint(&empty).expect("read"), None);
    }

    #[test]
    fn active_endpoint_tolerates_unknown_and_extra_fields() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("x.json");
        write(
            &p,
            "{\"provider\":\"vast-gguf\",\"instance_id\":31337,\"status\":\"running\",\
             \"something_new\":{\"a\":1},\"switched_at\":\"2026-05-05T15:51:00Z\"}\n",
        );
        let a = read_legacy_active_endpoint(&p)
            .expect("read")
            .expect("some");
        assert_eq!(a.instance_id, Some(31337));
        assert_eq!(a.status.as_deref(), Some("running"));
    }

    // -----------------------------------------------------------------------------------
    // trailing newlines
    // -----------------------------------------------------------------------------------

    #[test]
    fn legacy_scalar_files_parse_with_trailing_newlines_trimmed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        // `.last_instance` — `echo` leaves a newline; an untrimmed parse loses the instance.
        write(&root.join(".last_instance"), "27638194\n");
        let last = read_trimmed(&root.join(".last_instance")).expect("read");
        assert_eq!(last.as_deref(), Some("27638194"));
        assert_eq!(
            last.as_deref().and_then(|s| s.parse::<u64>().ok()),
            Some(27_638_194)
        );

        // CRLF and surrounding blank lines too.
        write(&root.join(".last_instance"), "\r\n  27638194  \r\n\r\n");
        assert_eq!(
            read_trimmed(&root.join(".last_instance"))
                .expect("read")
                .as_deref(),
            Some("27638194")
        );

        // `.instance_history` — `printf '%s\t%s\n'`, plus a trailing newline.
        write(
            &root.join(".instance_history"),
            "2026-05-01T10:00:00Z\t111\n2026-05-02T11:30:00Z\t222\n\n",
        );
        let hist = read_instance_history(&root.join(".instance_history")).expect("read");
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[0].1, 111);
        assert_eq!(hist[1].1, 222);
        assert!(hist[0].0.is_some(), "the timestamp column must parse");
        assert!(hist[0].0 < hist[1].0);

        // `.hf_pin` — JSON with a trailing newline.
        write(
            &root.join(".hf_pin"),
            "{\"MODEL_REPO\":\"unsloth/Qwen3.6-27B-GGUF\",\"MODEL_QUANT\":\"UD-Q8_K_XL\",\
             \"filename\":\"q8.gguf\",\"size\":\"29 GB\"}\n",
        );
        let pin: LegacyHfPin = read_json_trimmed(&root.join(".hf_pin"))
            .expect("read")
            .expect("some");
        assert_eq!(pin.model_repo, "unsloth/Qwen3.6-27B-GGUF");
        assert_eq!(pin.model_quant, "UD-Q8_K_XL");

        // `.pinned_provider` — the live file on this machine has no trailing newline at all.
        write(
            &root.join(".pinned_provider"),
            "{\"provider\": \"together\", \"model_id\": \"deepseek-ai/DeepSeek-V4-Pro\", \
             \"base_url\": \"https://api.together.ai/v1\"}",
        );
        let pp: LegacyPinnedProvider = read_json_trimmed(&root.join(".pinned_provider"))
            .expect("read")
            .expect("some");
        assert_eq!(pp.model_id, "deepseek-ai/DeepSeek-V4-Pro");
        assert_eq!(pp.base_url, "https://api.together.ai/v1");

        // Absent files are `None`, not errors.
        assert_eq!(read_trimmed(&root.join("nope")).expect("read"), None);
        assert!(read_instance_history(&root.join("nope"))
            .expect("read")
            .is_empty());
    }

    // -----------------------------------------------------------------------------------
    // local_instances
    // -----------------------------------------------------------------------------------

    #[test]
    fn legacy_instances_validate_their_paths_on_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let live_model = root.join("models/live.gguf");
        write(&live_model, "GGUF");
        let live_bin = root.join("llama.cpp/build-vulkan/bin/llama-server");
        write(&live_bin, "#!/bin/true");

        let inst_dir = root.join("local_instances");
        write(
            &inst_dir.join("b-stale.json"),
            &format!(
                r#"{{"name":"b-stale","pid":649035,"port":8100,"host":"127.0.0.1",
                    "binary":"{}","model_path":"{}/models/deleted.gguf","backend":"vulkan",
                    "started_at":"2026-05-03T00:34:36Z","status":"stopped",
                    "stopped_at":"2026-05-03T00:38:32Z"}}"#,
                live_bin.display(),
                root.display()
            ),
        );
        write(
            &inst_dir.join("a-live.json"),
            &format!(
                r#"{{"name":"a-live","port":8102,"host":"127.0.0.1","binary":"{}",
                    "model_path":"{}","backend":"vulkan","status":"running"}}"#,
                live_bin.display(),
                live_model.display()
            ),
        );
        // A file that will not parse must not fail the whole load.
        write(&inst_dir.join("c-broken.json"), "{ this is not json");
        // A non-JSON file is ignored outright.
        write(&inst_dir.join("notes.txt"), "hello");

        let rows = read_legacy_instances(&inst_dir).expect("read");
        assert_eq!(rows.len(), 2, "the broken row is skipped, never fatal");
        assert_eq!(rows[0].name, "a-live", "rows are sorted by name");
        assert!(rows[0].model_exists);
        assert!(rows[0].binary_exists);
        assert_eq!(rows[1].name, "b-stale");
        assert!(
            !rows[1].model_exists,
            "a saved instance pointing at a deleted model is stale, not an error"
        );
        assert!(rows[1].binary_exists);
        assert_eq!(rows[1].pid, Some(649_035));
        assert!(rows[1].source_file.ends_with("b-stale.json"));

        // A missing directory is empty, not an error.
        assert!(read_legacy_instances(&root.join("nope"))
            .expect("read")
            .is_empty());
    }

    // -----------------------------------------------------------------------------------
    // recipes.toml
    // -----------------------------------------------------------------------------------

    #[test]
    fn recipes_toml_import_handles_every_type_trap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let model = dir.path().join("models/here.gguf");
        write(&model, "GGUF");
        let path = dir.path().join("recipes.toml");
        write(&path, &fixture_recipes_toml(&model.display().to_string()));

        let (recipes, profiles, forks, docker, skips) = import_recipes_toml(&path).expect("import");

        // `[docker]`: the three families we publish; the fourth key is reported, not silently
        // dropped.
        assert_eq!(docker.prebuilt, "ghcr.io/x:prebuilt");
        assert_eq!(docker.builder, "ghcr.io/x:builder");
        assert_eq!(docker.vllm, "ghcr.io/x:vllm");
        assert!(skips.iter().any(|s| s.contains("prebuilt_legacy")));

        // `max_price` is a QUOTED STRING and must still land as money.
        assert_eq!(profiles.len(), 19);
        assert_eq!(
            profiles[0].max_dph,
            Some(Money::from_usd(3.50)),
            "`max_price = \"3.50\"` is a string in every legacy row"
        );
        // Legacy vast names are underscored; the live vocabulary is spaced.
        assert_eq!(profiles[0].gpu_names, vec!["H100 SXM", "H100 SXM5"]);
        // `num_gpus` becomes a range, and defaults to 1 when the tier omits it.
        let multi = profiles
            .iter()
            .find(|p| p.num_gpus_min > 1)
            .expect("a multi-gpu tier");
        assert_eq!(multi.num_gpus_min, multi.num_gpus_max);
        assert!(profiles.iter().any(|p| p.num_gpus_min == 1));

        // `vram_gb` is PER GPU: the pooled figure must be the multiplication.
        let detail = import_recipes_detail(&path).expect("detail");
        let tier0 = detail.tier_notes.get("tier0").expect("tier0 note");
        assert!(
            tier0.contains("PER GPU") && tier0.contains("160 GB pooled"),
            "80 GB per GPU × 2 GPUs is 160 GB pooled, not 80: {tier0}"
        );

        // `provider` absent means `vast_gguf`: 54 rows, one reason each.
        let vast_rows: Vec<&String> = skips
            .iter()
            .filter(|s| s.contains(DEFAULT_LEGACY_PROVIDER))
            .collect();
        assert_eq!(
            vast_rows.len(),
            54,
            "every provider-less row is skipped with its own reason"
        );
        assert!(
            vast_rows[0].contains("160 GB pooled"),
            "the per-row reason states the pooled VRAM: {}",
            vast_rows[0]
        );
        assert!(
            vast_rows[0].contains("TOTAL pool shared across 2 slot(s)"),
            "the per-row reason states that ctx is the total pool: {}",
            vast_rows[0]
        );
        assert!(vast_rows[0].contains("fit()"));

        // `enforce_eager` is the STRING "true"/"false".
        let eager: Vec<&String> = skips.iter().filter(|s| s.contains("vllm-row-")).collect();
        assert_eq!(eager.len(), 7);
        assert!(eager.iter().any(|s| s.contains("enforce_eager=true")));
        assert!(eager.iter().any(|s| s.contains("enforce_eager=false")));

        // The 7 fork mappings.
        assert_eq!(forks.len(), 7);
        assert!(forks
            .iter()
            .all(|f| f.llama_cpp_repo == "fairydreaming/llama.cpp"
                && f.llama_cpp_ref == "deepseek-dsa"
                && f.match_repo.ends_with('*')));

        // 3 local + 7 together = 10 recipes, and nothing else.
        assert_eq!(recipes.len(), 10);
        let local: Vec<&Recipe> = recipes
            .iter()
            .filter(|r| matches!(r.kind, RecipeKind::Local(_)))
            .collect();
        let managed: Vec<&Recipe> = recipes
            .iter()
            .filter(|r| matches!(r.kind, RecipeKind::Managed(_)))
            .collect();
        assert_eq!(local.len(), 3);
        assert_eq!(managed.len(), 7);

        // Local rows: `n_gpu_layers = 999` is "all", `~` is expanded, `mode` is honoured.
        let RecipeKind::Local(spec) = &local[0].kind else {
            panic!("expected a local recipe");
        };
        assert_eq!(spec.ngl, NglPlan::All);
        assert_eq!(spec.mode, SamplingMode::Coding);
        assert_eq!(spec.kv_type, Some(KvType::Q8_0));
        assert_eq!(spec.build.as_str(), "build-vulkan");
        assert_eq!(spec.ctx, Some(32_768));
        assert!(
            !spec.model_path.starts_with('~'),
            "the raw `~` form is never stored: {}",
            spec.model_path
        );

        // Managed rows: base_url without `/v1`, credential by REFERENCE, no prices.
        let RecipeKind::Managed(m) = &managed[0].kind else {
            panic!("expected a managed recipe");
        };
        assert_eq!(m.base_url, "https://api.together.ai");
        assert_eq!(
            m.credential,
            CredentialSource::Env {
                var: "TOGETHER_API_KEY".to_owned()
            }
        );
        assert!(managed[0]
            .description
            .as_deref()
            .unwrap_or_default()
            .contains("NOT imported"));

        // Ids are generated and unique.
        let mut ids: Vec<&str> = recipes.iter().map(|r| r.id.as_str()).collect();
        ids.sort_unstable();
        let n = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), n, "generated recipe ids must be unique");
    }

    /// Ground truth: the real checkout, when it is on this machine.
    #[test]
    fn the_real_recipes_toml_yields_exactly_54_vast_gguf_skips() {
        let Some(home) = dirs::home_dir() else { return };
        let path = home.join("Projects/Inference/tools/LocalRouter/recipes.toml");
        if !path.is_file() {
            return;
        }
        let (recipes, profiles, forks, docker, skips) = import_recipes_toml(&path).expect("import");
        assert_eq!(
            skips
                .iter()
                .filter(|s| s.contains(DEFAULT_LEGACY_PROVIDER))
                .count(),
            54,
            "the real file has 54 provider-less rows"
        );
        assert_eq!(forks.len(), 7, "the real file has 7 fork mappings");
        assert_eq!(profiles.len(), 19, "the real file has 19 gpu tiers");
        assert_eq!(recipes.len(), 10, "3 local + 7 together");
        assert!(docker.prebuilt.contains("vastai-gguf"));
    }

    #[test]
    fn the_type_trap_parsers_are_exactly_as_specified() {
        // `enforce_eager`: `true|1|yes` case-insensitively, everything else false.
        for s in ["true", "TRUE", " True ", "1", "yes", "YES"] {
            assert!(flex_bool(Some(&toml::Value::String(s.into()))), "{s:?}");
        }
        for s in ["false", "FALSE", "no", "0", "", "maybe", "truthy"] {
            assert!(!flex_bool(Some(&toml::Value::String(s.into()))), "{s:?}");
        }
        assert!(flex_bool(Some(&toml::Value::Boolean(true))));
        assert!(!flex_bool(None));

        // `max_price`: a quoted string, tolerated as a number.
        assert_eq!(
            flex_f64(Some(&toml::Value::String("0.55".into()))),
            Some(0.55)
        );
        assert_eq!(flex_f64(Some(&toml::Value::Float(1.6))), Some(1.6));
        assert_eq!(flex_f64(Some(&toml::Value::Integer(3))), Some(3.0));
        assert_eq!(flex_f64(Some(&toml::Value::String("free".into()))), None);
        assert_eq!(flex_f64(None), None);

        // `/v1` is stripped; the host is never rewritten.
        assert_eq!(
            strip_v1("https://api.together.xyz/v1"),
            "https://api.together.xyz"
        );
        assert_eq!(
            strip_v1("https://api.together.ai/v1/"),
            "https://api.together.ai"
        );
        assert_eq!(strip_v1("http://127.0.0.1:8100"), "http://127.0.0.1:8100");

        // Ids are generated from legacy names, never trusted from them.
        assert_eq!(
            slugify("DSv4-Flash Q2_K (2×H100)"),
            "dsv4-flash-q2_k-2-h100"
        );
        assert_eq!(slugify("///"), "imported");
        assert_eq!(slugify("-leading"), "r--leading");
        let mut used = HashSet::new();
        assert_eq!(unique_slug("same", &mut used), "same");
        assert_eq!(unique_slug("same", &mut used), "same-2");

        // A build id IS the build-directory name.
        assert_eq!(
            build_id_from_binary("/home/andre/llama.cpp/build-vulkan/bin/llama-server").as_deref(),
            Some("build-vulkan")
        );

        // `-ngl` policy.
        assert_eq!(ngl_of(None), NglPlan::Auto);
        assert_eq!(ngl_of(Some(999)), NglPlan::All);
        assert_eq!(ngl_of(Some(32)), NglPlan::Layers(32));

        // Legacy timestamps, including the local-time-with-a-lying-Z form.
        assert_eq!(parse_legacy_time(Some("1970-01-01T00:00:10Z")), Some(10));
        assert_eq!(parse_legacy_time(Some("not a time")), None);
        assert_eq!(parse_legacy_time(None), None);
    }

    // -----------------------------------------------------------------------------------
    // plan / apply
    // -----------------------------------------------------------------------------------

    /// Lay out a complete legacy world under `home`, returning the LocalRouter dir.
    fn fixture_world(home: &Path) -> PathBuf {
        let vg = home.join(".vastai-gguf");
        write(
            &vg.join("config.toml"),
            "[providers.together]\n\
             base_url  = \"https://api.together.xyz/v1\"\n\
             api_key   = \"tgp_v1_REAL_SECRET_KEY_VALUE\"\n",
        );
        write(
            &vg.join("usage.log"),
            "{\"timestamp\":\"2026-05-02T20:12:00Z\",\"provider\":\"together\"}\n",
        );
        write(&vg.join("local_logs/one.log"), "hello\n");
        write(
            &vg.join(".pinned_provider"),
            "{\"provider\": \"together\", \"model_id\": \"deepseek-ai/DeepSeek-V4-Pro\", \
             \"base_url\": \"https://api.together.ai/v1\"}",
        );
        let model = home.join("models/carnice-9b/Carnice-9b-Q6_K.gguf");
        write(&model, "GGUF");
        let bin = home.join("llama.cpp/build-vulkan/bin/llama-server");
        write(&bin, "#!/bin/true");
        write(
            &vg.join("local_instances/local-qwen35-9b.json"),
            &format!(
                r#"{{"name":"local-qwen35-9b","pid":649035,"port":8100,"host":"127.0.0.1",
                    "binary":"{}","model_path":"~/models/Qwen3.5-9B-Q4_K_M.gguf",
                    "backend":"vulkan","started_at":"2026-05-03T00:34:36Z","status":"stopped"}}"#,
                bin.display()
            ),
        );

        let lr = home.join("LocalRouterFixture");
        write(&lr.join("endpoint_proxy.py"), "# legacy\n");
        write(
            &lr.join(".active_endpoint"),
            "{\"provider\":\"local\",\"name\":\"local-qwen35-9b\",\"host\":\"127.0.0.1\",\
             \"port\":8100,\"pid\":649035,\"activated_at\":\"2026-05-03T00:34:36Z\",\
             \"api_key\":\"sk-local-secret\"}\n",
        );
        write(&lr.join(".last_instance"), "27638194\n");
        write(
            &lr.join(".instance_history"),
            "2026-05-01T10:00:00Z\t27000001\n2026-05-03T10:00:00Z\t27638194\n",
        );
        write(
            &lr.join(".hf_pin"),
            "{\"MODEL_REPO\":\"unsloth/Qwen3.6-27B-GGUF\",\"MODEL_QUANT\":\"UD-Q8_K_XL\",\
             \"filename\":\"q8.gguf\",\"size\":\"29 GB\"}\n",
        );
        write(
            &lr.join("recipes.toml"),
            &fixture_recipes_toml(&model.display().to_string()),
        );
        lr
    }

    #[test]
    fn dry_run_writes_nothing_at_all() {
        let home = tempfile::tempdir().expect("tempdir");
        let lr = fixture_world(home.path());
        let paths = test_paths(home.path(), Some(&lr));
        let cfg = Config::default();

        let before = tree_hash(home.path());
        let plan = super::plan(&paths, &cfg).expect("plan");
        assert_eq!(
            before,
            tree_hash(home.path()),
            "`migrate --dry-run` must not create, touch or rewrite a single byte — not the \
             state dir, not the ledger, not the legacy tree"
        );
        // …including a second run, in case the first created something lazily.
        let _ = super::plan(&paths, &cfg).expect("plan again");
        assert_eq!(before, tree_hash(home.path()));

        assert!(!plan.items.is_empty());
        assert_eq!(plan.source_paths.len(), 2);
        assert!(plan
            .source_paths
            .iter()
            .any(|p| p.ends_with("LocalRouterFixture")));

        // Every one of the 54 frozen rows is its own row in the plan, with its own reason.
        let skipped_vast: Vec<&MigrationItem> = plan
            .items
            .iter()
            .filter(|i| {
                i.action == MigrationAction::Skip && i.detail.contains(DEFAULT_LEGACY_PROVIDER)
            })
            .collect();
        assert_eq!(skipped_vast.len(), 54);
        assert!(skipped_vast.iter().all(|i| i.what == "recipe"
            && i.from.contains("recipes.toml#recipes.vast-row-")
            && !i.detail.is_empty()));

        // The plan is printed: no key material may appear anywhere in it.
        let rendered = serde_json::to_string(&plan).expect("ser");
        assert!(!rendered.contains("tgp_v1_REAL_SECRET_KEY_VALUE"));
        assert!(!rendered.contains("sk-local-secret"));

        // The forks, the docker map and the tier seeds all show up.
        assert_eq!(
            plan.items.iter().filter(|i| i.what == "known_fork").count(),
            7
        );
        assert_eq!(
            plan.items
                .iter()
                .filter(|i| i.what == "search profile")
                .count(),
            19
        );
        assert_eq!(
            plan.items
                .iter()
                .filter(|i| i.what == "docker images")
                .count(),
            1
        );
        // One instance id from `.last_instance`, one distinct row from history.
        assert_eq!(
            plan.items
                .iter()
                .filter(|i| i.what == "vast instance")
                .count(),
            2
        );
        // The stale saved instance imports as a Warn, not an Error.
        let inst = plan
            .items
            .iter()
            .find(|i| i.what == "local instance")
            .expect("the saved instance");
        assert_eq!(inst.action, MigrationAction::Warn);
        assert!(inst.detail.contains("NO LONGER EXISTS"));
    }

    #[test]
    fn apply_imports_credentials_as_references_and_seeds_the_ledger() {
        let home = tempfile::tempdir().expect("tempdir");
        // A world with NO recipes.toml and NO .pinned_provider: this test is about the
        // config and the ledger, and must not depend on the catalog writer.
        let vg = home.path().join(".vastai-gguf");
        write(
            &vg.join("config.toml"),
            "[providers.legacyco]\n\
             base_url  = \"https://api.legacy.xyz/v1\"\n\
             api_key   = \"tgp_v1_REAL_SECRET_KEY_VALUE\"\n",
        );
        let lr = home.path().join("LocalRouterFixture");
        write(&lr.join("endpoint_proxy.py"), "# legacy\n");
        write(&lr.join(".last_instance"), "27638194\n");
        write(
            &lr.join(".instance_history"),
            "2026-05-01T10:00:00Z\t27000001\n2026-05-03T10:00:00Z\t27638194\n",
        );

        let paths = test_paths(home.path(), Some(&lr));
        let cfg = Config::default();
        let plan = super::plan(&paths, &cfg).expect("plan");
        let report = apply(&paths, &cfg, &plan).expect("apply");
        assert!(report.imported > 0);

        // The key is NOT copied; the reference is.
        let written = fs::read_to_string(paths.config_file()).expect("config");
        assert!(
            !written.contains("tgp_v1_REAL_SECRET_KEY_VALUE"),
            "a borrowed credential must never be copied into our config"
        );
        assert!(
            written.contains("https://api.legacy.xyz/v1"),
            "the legacy base_url is used verbatim"
        );
        assert!(written.contains("LEGACYCO_API_KEY"));

        // `.last_instance` stays visible as possibly-billing; history does not.
        let ledger = Ledger::open(&paths).expect("ledger");
        let active = ledger.active().expect("active");
        assert_eq!(active.len(), 1, "only the un-destroyed instance is active");
        assert_eq!(active[0].instance_id.map(|i| i.0), Some(27_638_194));
        assert_eq!(active[0].state, LedgerState::Confirmed);
        assert_eq!(ledger.rows().expect("rows").len(), 2);
        assert!(report
            .warnings
            .iter()
            .any(|w| w.contains("may STILL BE BILLING")));

        // Re-applying is idempotent: no duplicate ledger rows, no duplicate providers.
        let plan2 = super::plan(&paths, &cfg).expect("plan");
        let _ = apply(&paths, &cfg, &plan2).expect("apply again");
        assert_eq!(ledger.rows().expect("rows").len(), 2);
    }

    #[test]
    fn a_row_struck_out_of_the_plan_is_never_written() {
        let home = tempfile::tempdir().expect("tempdir");
        let vg = home.path().join(".vastai-gguf");
        write(
            &vg.join("config.toml"),
            "[providers.legacyco]\nbase_url = \"https://api.legacy.xyz/v1\"\n",
        );
        let lr = home.path().join("LocalRouterFixture");
        write(&lr.join("endpoint_proxy.py"), "# legacy\n");
        write(&lr.join(".last_instance"), "27638194\n");

        let paths = test_paths(home.path(), Some(&lr));
        let cfg = Config::default();

        // Strike every row out — exactly what a human unticking the whole list produces.
        let mut plan = super::plan(&paths, &cfg).expect("plan");
        for item in &mut plan.items {
            item.action = MigrationAction::Skip;
        }
        let report = apply(&paths, &cfg, &plan).expect("apply");
        assert_eq!(report.imported, 0);
        assert!(report.skipped > 0);
        assert!(
            !paths.config_file().exists(),
            "nothing selected means nothing written"
        );
        assert!(!paths.ledger().exists());
    }

    /// FINDING B: `[compat] mirror_usage_log` is off by default, so migration — the one
    /// place where wanting it is obvious — has to *offer* it rather than leave it buried.
    #[test]
    fn migration_offers_the_usage_mirror_and_only_writes_it_when_kept() {
        let home = tempfile::tempdir().expect("tempdir");
        let vg = home.path().join(".vastai-gguf");
        write(
            &vg.join("usage.log"),
            "{\"timestamp\":\"2026-05-02T20:12:00Z\",\"provider\":\"together\"}\n",
        );
        let paths = test_paths(home.path(), None);
        let cfg = Config::default();
        assert!(!cfg.compat.mirror_usage_log, "the default is off");

        let plan = super::plan(&paths, &cfg).expect("plan");
        let offer = plan
            .items
            .iter()
            .find(|i| i.what == "usage mirror")
            .expect("migration must offer the mirror");
        assert_eq!(offer.action, MigrationAction::Warn);
        assert!(offer.detail.contains("mirror_usage_log"));
        assert!(
            offer.detail.contains("OFF by default"),
            "the offer must say what it is turning on: {}",
            offer.detail
        );

        // Struck out, nothing changes — the daemon still leaves the legacy file alone.
        let mut struck = plan.clone();
        for item in &mut struck.items {
            item.action = MigrationAction::Skip;
        }
        apply(&paths, &cfg, &struck).expect("apply");
        assert!(
            !paths.config_file().exists(),
            "nothing selected, nothing written"
        );

        // Kept, the config says so — and only then.
        apply(&paths, &cfg, &plan).expect("apply");
        let written = fs::read_to_string(paths.config_file()).expect("config");
        assert!(
            written.contains("mirror_usage_log = true"),
            "keeping the row enables it: {written}"
        );
        let reloaded = Config::load_from(Some(&paths.config_file()), None).expect("load");
        assert!(reloaded.compat.mirror_usage_log);

        // With it already on, the row becomes an informational skip, not a second offer.
        let again = super::plan(&paths, &reloaded).expect("plan");
        let row = again
            .items
            .iter()
            .find(|i| i.what == "usage mirror")
            .expect("the row stays visible");
        assert_eq!(row.action, MigrationAction::Skip);
        assert!(row.detail.contains("already enabled"));
    }

    #[test]
    fn an_empty_machine_yields_an_empty_plan() {
        let home = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(home.path(), None);
        let plan = super::plan(&paths, &Config::default()).expect("plan");
        assert!(plan.items.is_empty());
        assert!(plan.source_paths.is_empty());
    }
}
