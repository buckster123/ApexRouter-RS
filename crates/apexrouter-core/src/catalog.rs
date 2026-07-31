//! OWNER: unit C-15 (core/catalog.rs). Do not edit outside that unit.
//!
//! Recipes and search profiles, persisted at `$STATE/catalog.toml` through **`toml_edit`**
//! so a hand-written comment survives a GUI edit.
//!
//! Ids are **generated**, never typed by the user. Staleness — model file gone, build
//! removed, profile deleted — is reported as a `Warning` with a `fix` string, never an
//! `Error`: a recipe pointing at a model you deleted is normal.
//!
//! # How the round-trip works
//!
//! [`save`] never re-renders the document from scratch. It parses whatever is on disk into a
//! `toml_edit::DocumentMut`, serialises the in-memory [`Catalog`] into a second document, and
//! merges the second into the first **entry by entry, keyed on the generated `id`** — keeping
//! each existing entry's decor, which is where `toml_edit` stores the comments and blank
//! lines a human wrote. A field that has become `None` is removed, an entry that has been
//! deleted is dropped, a new entry is appended. Everything else on disk is left alone.
//!
//! # Locking
//!
//! `catalog.toml` is `$STATE`, so every public function here goes through the same
//! `state.lock` as [`crate::store::Store`]: [`load`] under `LOCK_SH`, everything that writes
//! under one `LOCK_EX` spanning the whole read-modify-write. That lock is **not** reentrant,
//! which is why the mutating functions call the private unlocked helpers rather than
//! [`load`]/[`save`].

use crate::error::{Error, Result};
use crate::paths::Paths;
use crate::store::Store;
use apexrouter_protocol::{
    EndpointRecord, EndpointSpec, GeoFilter, ImageType, LocalModel, ManagedSpec, Money, ProfileId,
    Provenance2, ProviderId, Recipe, RecipeId, RecipeKind, RigSnapshot, SearchProfile, Severity,
    ValidationIssue, ValidationReport,
};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Mode for `catalog.toml`. Owner-only, like everything else under `$STATE`.
const FILE_MODE: u32 = 0o600;

/// The profile id a recipe saved from a running vast instance gets.
///
/// An `EndpointRecord` for a rental records the *container*, not the market query that found
/// the box, so "save this running thing as a recipe" genuinely cannot know which
/// [`SearchProfile`] to re-run. [`validate_recipe`] reports the placeholder as a `Warning`
/// with a fix, exactly like a profile that was deleted.
const UNASSIGNED_PROFILE: &str = "unassigned";

/// Everything in `catalog.toml`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Catalog {
    /// Saved launch plans.
    pub recipes: Vec<Recipe>,
    /// Saved market queries.
    pub profiles: Vec<SearchProfile>,
}

/// The serde projection of [`Catalog`]. Private on purpose: the on-disk shape is this
/// module's business, and `Catalog` deliberately carries no serde derives.
#[derive(Debug, Default, Serialize, Deserialize)]
struct CatalogFile {
    /// `[[recipes]]`.
    #[serde(default)]
    recipes: Vec<Recipe>,
    /// `[[profiles]]`.
    #[serde(default)]
    profiles: Vec<SearchProfile>,
}

/// Read `catalog.toml`. A missing file yields an empty catalog.
///
/// # Errors
/// [`Error::Io`] if the file exists but cannot be read, [`Error::Toml`] if it does not parse.
pub fn load(paths: &Paths) -> Result<Catalog> {
    let store = Store::new(paths.clone());
    store.with_state_lock_shared(|| read_file(&paths.catalog_file()))
}

/// Write `catalog.toml` through `toml_edit`, atomically, preserving comments.
///
/// # Errors
/// [`Error::Invalid`] if the file on disk exists but is not parseable TOML — we refuse to
/// clobber a document we do not understand — or if a value cannot be represented in TOML (a
/// JSON `null` in `SearchProfile::extra`); [`Error::Io`] on a write failure.
pub fn save(paths: &Paths, c: &Catalog) -> Result<()> {
    let store = Store::new(paths.clone());
    store.with_state_lock_exclusive(|| write_file(&store, &paths.catalog_file(), c))
}

/// Insert or replace one recipe, generating an id when it is new.
///
/// The incoming id is honoured **only** when it names a recipe that already exists. Anything
/// else — a blank draft from the GUI, an id a user typed, an id imported from `recipes.toml`
/// — gets a fresh id derived from the label and made unique against the catalog.
/// `created_at_unix` survives an update; `updated_at_unix` is stamped on every write.
///
/// # Errors
/// Anything the read-modify-write can fail with.
pub fn upsert_recipe(paths: &Paths, r: Recipe) -> Result<Recipe> {
    let store = Store::new(paths.clone());
    let file = paths.catalog_file();
    store.with_state_lock_exclusive(|| {
        let mut c = read_file(&file)?;
        let now = now_unix();
        let saved = match c.recipes.iter().position(|x| x.id == r.id) {
            Some(i) => {
                let mut updated = r;
                updated.id = c.recipes[i].id.clone();
                updated.created_at_unix = first_seen(c.recipes[i].created_at_unix, now);
                updated.updated_at_unix = now;
                c.recipes[i] = updated.clone();
                updated
            }
            None => {
                let taken: Vec<&str> = c.recipes.iter().map(|x| x.id.as_str()).collect();
                let mut fresh = r;
                fresh.id = unique_id(&fresh.label, "recipe", &taken, RecipeId::parse);
                fresh.created_at_unix = first_seen(fresh.created_at_unix, now);
                fresh.updated_at_unix = now;
                c.recipes.push(fresh.clone());
                fresh
            }
        };
        write_file(&store, &file, &c)?;
        Ok(saved)
    })
}

/// Delete one recipe.
///
/// # Errors
/// [`Error::NotFound`] when no recipe has that id, plus anything the read-modify-write can
/// fail with.
pub fn remove_recipe(paths: &Paths, id: &RecipeId) -> Result<()> {
    let store = Store::new(paths.clone());
    let file = paths.catalog_file();
    store.with_state_lock_exclusive(|| {
        let mut c = read_file(&file)?;
        let before = c.recipes.len();
        c.recipes.retain(|r| &r.id != id);
        if c.recipes.len() == before {
            return Err(Error::NotFound(format!("recipe {id}")));
        }
        write_file(&store, &file, &c)
    })
}

/// Insert or replace one search profile, generating an id when it is new.
///
/// Same id rule as [`upsert_recipe`]: an id that names nothing is replaced by a generated one.
///
/// # Errors
/// Anything the read-modify-write can fail with.
pub fn upsert_profile(paths: &Paths, p: SearchProfile) -> Result<SearchProfile> {
    let store = Store::new(paths.clone());
    let file = paths.catalog_file();
    store.with_state_lock_exclusive(|| {
        let mut c = read_file(&file)?;
        let saved = match c.profiles.iter().position(|x| x.id == p.id) {
            Some(i) => {
                let mut updated = p;
                updated.id = c.profiles[i].id.clone();
                c.profiles[i] = updated.clone();
                updated
            }
            None => {
                let taken: Vec<&str> = c.profiles.iter().map(|x| x.id.as_str()).collect();
                let mut fresh = p;
                fresh.id = unique_id(&fresh.label, "profile", &taken, ProfileId::parse);
                c.profiles.push(fresh.clone());
                fresh
            }
        };
        write_file(&store, &file, &c)?;
        Ok(saved)
    })
}

/// Delete one search profile.
///
/// # Errors
/// [`Error::NotFound`] when no profile has that id, plus anything the read-modify-write can
/// fail with.
pub fn remove_profile(paths: &Paths, id: &ProfileId) -> Result<()> {
    let store = Store::new(paths.clone());
    let file = paths.catalog_file();
    store.with_state_lock_exclusive(|| {
        let mut c = read_file(&file)?;
        let before = c.profiles.len();
        c.profiles.retain(|p| &p.id != id);
        if c.profiles.len() == before {
            return Err(Error::NotFound(format!("profile {id}")));
        }
        write_file(&store, &file, &c)
    })
}

/// Check a recipe against what actually exists on this machine right now.
///
/// **Staleness is never an `Error`.** A recipe whose weights were deleted, whose llama.cpp
/// build directory was removed, or whose search profile no longer exists is a `Warning` with
/// a `fix` string, because all three are ordinary consequences of using the machine. Only a
/// recipe that could not be launched *however* the world looked — no label, no model path, a
/// container with no image, a credential inlined into `onstart` — is an `Error`.
///
/// The "profile deleted" check reads the catalog through [`Paths::resolve`], which is the
/// single resolution mechanism (`--home` is pushed into the environment before anything
/// loads). When there is no `catalog.toml` at all the check is skipped rather than guessed at.
pub fn validate_recipe(r: &Recipe, rig: &RigSnapshot, models: &[LocalModel]) -> ValidationReport {
    let mut issues: Vec<ValidationIssue> = Vec::new();

    if r.label.trim().is_empty() {
        issues.push(issue(
            "label",
            Severity::Error,
            "the recipe has no label",
            Some("give it a name: `apexrouter recipe edit <id> --label ...`"),
        ));
    }

    match &r.kind {
        RecipeKind::Local(s) => {
            if s.model_path.trim().is_empty() {
                issues.push(issue(
                    "kind.model_path",
                    Severity::Error,
                    "no model path",
                    Some("pick a model: `apexrouter models ls`"),
                ));
            } else if let Some(m) = find_model(models, &s.model_path) {
                if let Some(size) = r.provenance.size_bytes {
                    let shard = m.shards.first().map(|sh| sh.bytes);
                    if size != m.total_bytes && Some(size) != shard {
                        issues.push(issue(
                            "provenance.size_bytes",
                            Severity::Warning,
                            &format!(
                                "the weights changed size since discovery: {size} bytes then, \
                                 {} bytes now",
                                m.total_bytes
                            ),
                            Some(
                                "re-run discovery so the fit plan is computed against the file \
                                 that is actually there: `apexrouter rig --refresh`",
                            ),
                        ));
                    }
                }
            } else if !Path::new(&s.model_path).exists() {
                issues.push(issue(
                    "kind.model_path",
                    Severity::Warning,
                    &format!("model file gone: {}", s.model_path),
                    Some("pick another model, or re-download it: `apexrouter models ls`"),
                ));
            }

            if let Some(mm) = &s.mmproj {
                if find_mmproj(models, mm).is_none() && !Path::new(mm).exists() {
                    issues.push(issue(
                        "kind.mmproj",
                        Severity::Warning,
                        &format!("model file gone: the vision projector {mm} is not there"),
                        Some("clear the projector, or put it back next to the weights"),
                    ));
                }
            }

            if !rig.builds.iter().any(|b| b.id == s.build) {
                issues.push(issue(
                    "kind.build",
                    Severity::Warning,
                    &format!(
                        "build removed: no llama.cpp build `{}` on this machine",
                        s.build
                    ),
                    Some("choose one of the discovered builds: `apexrouter rig`"),
                ));
            }

            check_devices(&mut issues, rig, &s.split.devices, "kind.split.devices");
        }

        RecipeKind::LocalVllm(s) => {
            if s.model_id.trim().is_empty() {
                issues.push(issue(
                    "kind.model_id",
                    Severity::Error,
                    "no model id",
                    Some("set the HuggingFace model id vLLM should serve"),
                ));
            }
            if s.bin.trim().is_empty() {
                issues.push(issue(
                    "kind.bin",
                    Severity::Error,
                    "no vllm binary",
                    Some("set the path to the `vllm` executable"),
                ));
            } else if !Path::new(&s.bin).exists() {
                issues.push(issue(
                    "kind.bin",
                    Severity::Warning,
                    &format!("build removed: {} is not there any more", s.bin),
                    Some("point the recipe at the vllm you have now, or reinstall it"),
                ));
            }
            check_devices(&mut issues, rig, &s.devices, "kind.devices");
        }

        RecipeKind::Vast {
            profile, launch, ..
        } => {
            match known_profile_ids() {
                Some(known) if profile.as_str() == UNASSIGNED_PROFILE => issues.push(issue(
                    "kind.profile",
                    Severity::Warning,
                    "profile deleted: this recipe has no search profile yet",
                    Some(&format!(
                        "attach one of the {} saved profiles: `apexrouter profile ls`",
                        known.len()
                    )),
                )),
                Some(known) if !known.iter().any(|k| k == profile) => issues.push(issue(
                    "kind.profile",
                    Severity::Warning,
                    &format!("profile deleted: no search profile `{profile}` in the catalog"),
                    Some("attach another one: `apexrouter profile ls`"),
                )),
                _ => {}
            }
            if launch.image.trim().is_empty() {
                issues.push(issue(
                    "kind.launch.image",
                    Severity::Error,
                    "no container image",
                    Some("set `[docker]` in config.toml, or pick an image type"),
                ));
            }
            if launch.disk_gb == 0 {
                issues.push(issue(
                    "kind.launch.disk_gb",
                    Severity::Error,
                    "a container with no disk cannot download weights",
                    Some("ask for the model size plus about 20 GB"),
                ));
            }
            if launch.onstart.contains("HF_TOKEN") {
                issues.push(issue(
                    "kind.launch.onstart",
                    Severity::Error,
                    "a credential is inlined into `onstart`, which vast persists and echoes \
                     back from `show instance`",
                    Some(
                        "move it into the env map, which is where the container contract puts \
                         `HF_TOKEN`",
                    ),
                ));
            }
            if launch.expose_public {
                issues.push(issue(
                    "kind.launch.expose_public",
                    Severity::Warning,
                    "a vast direct port is plaintext HTTP on a shared public IP",
                    Some(
                        "keep the tunnel-only posture, or mint a per-instance api key before \
                         launching",
                    ),
                ));
            }
        }

        RecipeKind::Managed(s) => {
            if s.base_url.trim().is_empty() {
                issues.push(issue(
                    "kind.base_url",
                    Severity::Error,
                    "no base url",
                    Some("set the provider's base url, without a trailing `/v1`"),
                ));
            } else if s.base_url.trim_end_matches('/').ends_with("/v1") {
                issues.push(issue(
                    "kind.base_url",
                    Severity::Warning,
                    "the base url carries a trailing `/v1`, which the router appends itself",
                    Some("store the origin only, e.g. `https://api.together.xyz`"),
                ));
            }
        }
    }

    ValidationReport {
        ok: !issues.iter().any(|i| i.severity == Severity::Error),
        issues,
    }
}

/// The seed profiles: 3090×2-4, 3090×4 (perf), H100×1, H100×2.
///
/// Uses the **exact** `gpu_name` strings verified in `docs/port/00c` — `"RTX 3090"`,
/// `"H100 SXM"`, `"H100 NVL"`, `"H100 PCIE"` — and `num_gpus_min/max` ranges, never one
/// profile per GPU count.
///
/// The three H100 spellings live in one profile precisely because the market decides which of
/// them is cheap on any given day; the label says which is fastest for tensor parallel.
///
/// Seed these with [`save`], which writes ids verbatim. [`upsert_profile`] would regenerate
/// them from the label, which is right for something a human just drew in the GUI and wrong
/// for a seed a `RecipeKind::Vast` is going to reference by id.
pub fn default_profiles() -> Vec<SearchProfile> {
    let h100 = || {
        vec![
            "H100 SXM".to_string(),
            "H100 NVL".to_string(),
            "H100 PCIE".to_string(),
        ]
    };
    vec![
        SearchProfile {
            id: id_from("rtx3090-2-4", ProfileId::parse),
            label: "RTX 3090 ×2–4".to_string(),
            gpu_names: vec!["RTX 3090".to_string()],
            num_gpus_min: 2,
            num_gpus_max: 4,
            max_dph: Some(Money::from_usd(0.90)),
            min_reliability: 0.98,
            min_inet_down: 300,
            min_disk_gb: 80,
            min_cuda: Some(12.0),
            geo: GeoFilter::Any,
            image_type: ImageType::Prebuilt,
            extra: serde_json::Map::new(),
        },
        SearchProfile {
            id: id_from("rtx3090-4-perf", ProfileId::parse),
            label: "RTX 3090 ×4 (perf: fast link, high reliability)".to_string(),
            gpu_names: vec!["RTX 3090".to_string()],
            num_gpus_min: 4,
            num_gpus_max: 4,
            max_dph: Some(Money::from_usd(1.60)),
            min_reliability: 0.99,
            min_inet_down: 700,
            min_disk_gb: 200,
            min_cuda: Some(12.4),
            geo: GeoFilter::Any,
            image_type: ImageType::Prebuilt,
            extra: serde_json::Map::new(),
        },
        SearchProfile {
            id: id_from("h100-1", ProfileId::parse),
            label: "H100 ×1 (SXM/NVL/PCIE — SXM is the fast one)".to_string(),
            gpu_names: h100(),
            num_gpus_min: 1,
            num_gpus_max: 1,
            max_dph: Some(Money::from_usd(2.50)),
            min_reliability: 0.98,
            min_inet_down: 500,
            min_disk_gb: 200,
            min_cuda: Some(12.4),
            geo: GeoFilter::Any,
            image_type: ImageType::Prebuilt,
            extra: serde_json::Map::new(),
        },
        SearchProfile {
            id: id_from("h100-2", ProfileId::parse),
            label: "H100 ×2 (SXM/NVL/PCIE — PCIE is the slow one for tensor parallel)".to_string(),
            gpu_names: h100(),
            num_gpus_min: 2,
            num_gpus_max: 2,
            max_dph: Some(Money::from_usd(4.50)),
            min_reliability: 0.98,
            min_inet_down: 500,
            min_disk_gb: 300,
            min_cuda: Some(12.4),
            geo: GeoFilter::Any,
            image_type: ImageType::Prebuilt,
            extra: serde_json::Map::new(),
        },
    ]
}

/// "Save this running thing as a recipe."
///
/// The id here is only a proposal: [`upsert_recipe`] regenerates it unless it happens to name
/// a recipe that already exists.
///
/// Two mappings are lossy, deliberately:
///
/// * A **`Node`** endpoint — a plain OpenAI-compatible URL somebody registered — becomes a
///   [`RecipeKind::Managed`], because that is the same thing minus a lifecycle. The provider
///   id is derived from the host.
/// * A **`Vast`** endpoint carries the container it is running but not the market query that
///   found the box, so the profile is left unassigned and [`validate_recipe`] asks for one.
pub fn recipe_from_endpoint(rec: &EndpointRecord, label: &str) -> Recipe {
    let now = now_unix();
    let kind = match &rec.spec {
        EndpointSpec::LocalLlama(s) => RecipeKind::Local(s.clone()),
        EndpointSpec::LocalVllm(s) => RecipeKind::LocalVllm(s.clone()),
        EndpointSpec::Vast(s) => RecipeKind::Vast {
            profile: id_from(UNASSIGNED_PROFILE, ProfileId::parse),
            launch: s.launch.clone(),
            fit: rec.fit.clone(),
        },
        EndpointSpec::Node(s) => RecipeKind::Managed(ManagedSpec {
            provider: id_from(host_of(&s.base_url), ProviderId::parse),
            base_url: s.base_url.clone(),
            credential: s.credential.clone(),
            model_id: s.declared_models.first().cloned(),
            protocol: s.protocol,
        }),
        EndpointSpec::Managed(s) => RecipeKind::Managed(s.clone()),
    };

    // The size at save time, so `validate_recipe` can say "the weights changed under you".
    let size_bytes = match &rec.spec {
        EndpointSpec::LocalLlama(s) => std::fs::metadata(&s.model_path).ok().map(|m| m.len()),
        _ => None,
    };

    Recipe {
        id: id_from(label, RecipeId::parse),
        label: label.to_string(),
        description: Some(format!("saved from the running endpoint `{}`", rec.id)),
        kind,
        provenance: Provenance2 {
            discovered_at_unix: first_seen(rec.started_at_unix, now),
            size_bytes,
            source: format!("endpoint {}", rec.id),
            fit: rec.fit.clone(),
        },
        created_at_unix: now,
        updated_at_unix: now,
    }
}

// ---------------------------------------------------------------------------------------
// the on-disk round-trip
// ---------------------------------------------------------------------------------------

/// [`load`] without the lock, for callers that already hold it.
fn read_file(path: &Path) -> Result<Catalog> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Catalog::default()),
        Err(source) => {
            return Err(Error::Io {
                path: path.display().to_string(),
                source,
            })
        }
    };
    let file: CatalogFile = toml::from_str(&text)?;
    Ok(Catalog {
        recipes: file.recipes,
        profiles: file.profiles,
    })
}

/// [`save`] without the lock, for callers that already hold it.
fn write_file(store: &Store, path: &Path, c: &Catalog) -> Result<()> {
    let base_text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(Error::Io {
                path: path.display().to_string(),
                source,
            })
        }
    };
    let mut base: toml_edit::DocumentMut =
        base_text
            .parse()
            .map_err(|e: toml_edit::TomlError| Error::Invalid {
                what: format!("catalog file {}", path.display()),
                why: format!("refusing to overwrite a document that does not parse: {e}"),
            })?;

    let file = CatalogFile {
        recipes: c.recipes.clone(),
        profiles: c.profiles.clone(),
    };
    let fresh_text = toml::to_string_pretty(&file)?;
    let fresh: toml_edit::DocumentMut =
        fresh_text
            .parse()
            .map_err(|e: toml_edit::TomlError| Error::Invalid {
                what: "serialised catalog".to_string(),
                why: e.to_string(),
            })?;

    // One position counter across both lists, so `[[recipes]]` always renders before
    // `[[profiles]]` however the entries were reordered in memory.
    let mut position = 0usize;
    let fresh_table = fresh.as_table();
    merge_list(base.as_table_mut(), fresh_table, "recipes", &mut position);
    merge_list(base.as_table_mut(), fresh_table, "profiles", &mut position);

    store.write_atomic(path, base.to_string().as_bytes(), FILE_MODE)
}

/// Merge one array-of-tables from `src` into `dst`, matching entries on their `id` so an
/// entry's decor — the comments and blank lines a human wrote around it — survives.
///
/// Entries end up in `src` order, which is the order of the in-memory `Vec`.
fn merge_list(dst: &mut toml_edit::Table, src: &toml_edit::Table, key: &str, position: &mut usize) {
    let Some(toml_edit::Item::ArrayOfTables(src_list)) = src.get(key) else {
        // An empty `Vec` serialises as `key = []`. Drop the key entirely rather than writing
        // that: a bare `profiles = []` at the top of the file would collect the document's
        // opening comment as its own decor, and then lose it the moment the list stopped
        // being empty and became `[[profiles]]` headers instead.
        dst.remove(key);
        return;
    };

    let previous: Vec<toml_edit::Table> = dst
        .get(key)
        .and_then(|i| i.as_array_of_tables())
        .map(|a| a.iter().cloned().collect())
        .unwrap_or_default();

    let mut merged = toml_edit::ArrayOfTables::new();
    for src_t in src_list.iter() {
        let id = src_t.get("id").and_then(|i| i.as_str());
        let prev = id.and_then(|id| {
            previous
                .iter()
                .find(|t| t.get("id").and_then(|i| i.as_str()) == Some(id))
        });
        let mut table = match prev {
            Some(p) => {
                let mut t = p.clone();
                overwrite_table(&mut t, src_t);
                t
            }
            None => src_t.clone(),
        };
        renumber(&mut table, position);
        merged.push(table);
    }
    dst.insert(key, toml_edit::Item::ArrayOfTables(merged));
}

/// Give a table and everything nested inside it consecutive positions, depth first.
///
/// `toml_edit` renders tables in *position* order across the whole document, not in tree
/// order, so an entry cloned out of one document and pushed into another would otherwise have
/// its `[recipes.kind]` land under somebody else's `[[recipes]]` header — which is a
/// different recipe, or a duplicate-key parse error. Numbering every entry depth-first as it
/// is merged is what keeps the rendered document meaning what the tree means.
///
/// A sub-table with no keys of its own is made implicit while we are here, so an empty
/// `extra` map does not render as a bare `[profiles.extra]` header.
fn renumber(table: &mut toml_edit::Table, position: &mut usize) {
    table.set_position(*position);
    *position += 1;
    if table.is_empty() {
        table.set_implicit(true);
    }
    for (_, item) in table.iter_mut() {
        match item {
            toml_edit::Item::Table(t) => renumber(t, position),
            toml_edit::Item::ArrayOfTables(a) => {
                for t in a.iter_mut() {
                    renumber(t, position);
                }
            }
            _ => {}
        }
    }
}

/// Make `dst` say exactly what `src` says, while keeping `dst`'s decor.
///
/// Keys `src` does not have are removed — that is how a field which became `None` stops being
/// written — and sub-tables recurse, so a comment inside `[recipes.kind]` survives too.
fn overwrite_table(dst: &mut toml_edit::Table, src: &toml_edit::Table) {
    for (key, src_item) in src.iter() {
        match (dst.get_mut(key).and_then(|i| i.as_table_mut()), src_item) {
            (Some(dst_t), toml_edit::Item::Table(src_t)) => overwrite_table(dst_t, src_t),
            _ => {
                // A comment written *above* `label = "…"` is the decor of the key, and one
                // written after it on the same line is the decor of the value. Carry both:
                // `Table::insert` would install a brand-new `Key` and drop the first.
                let decor = dst
                    .get(key)
                    .and_then(|i| i.as_value())
                    .map(|v| v.decor().clone());
                let existing_key = dst.key(key).cloned();
                let mut item = src_item.clone();
                if let (Some(decor), Some(v)) = (decor, item.as_value_mut()) {
                    *v.decor_mut() = decor;
                }
                match existing_key {
                    Some(k) => {
                        dst.insert_formatted(&k, item);
                    }
                    None => {
                        dst.insert(key, item);
                    }
                }
            }
        }
    }
    let keep: Vec<String> = src.iter().map(|(k, _)| k.to_string()).collect();
    dst.retain(|k, _| keep.iter().any(|s| s == k));
}

// ---------------------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------------------

/// Unix seconds, now.
fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Keep a plausible stamp, or stamp one now. Zero or negative means the caller never set it —
/// a GUI draft, an import — and "1970" is not a useful answer.
fn first_seen(existing: i64, now: i64) -> i64 {
    if existing > 0 {
        existing
    } else {
        now
    }
}

/// One [`ValidationIssue`], with the fix folded back into a single actionable line.
fn issue(field: &str, severity: Severity, message: &str, fix: Option<&str>) -> ValidationIssue {
    ValidationIssue {
        field: field.to_string(),
        severity,
        message: collapse_ws(message),
        fix: fix.map(collapse_ws),
    }
}

/// Fold the line breaks a wrapped string literal picked up back into one line.
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Warn about every `-dev` token the rig no longer enumerates.
fn check_devices(
    issues: &mut Vec<ValidationIssue>,
    rig: &RigSnapshot,
    devices: &[String],
    field: &str,
) {
    for d in devices {
        if !rig.gpus.iter().any(|g| &g.device == d) {
            issues.push(issue(
                field,
                Severity::Warning,
                &format!("build removed: no device `{d}` on this machine any more"),
                Some(
                    "re-pick the devices, or clear them and let llama.cpp choose: \
                     `apexrouter rig`",
                ),
            ));
        }
    }
}

/// Find the discovered model a `-m` path belongs to, shards included.
fn find_model<'a>(models: &'a [LocalModel], path: &str) -> Option<&'a LocalModel> {
    models
        .iter()
        .find(|m| m.shards.iter().any(|s| s.path == path))
}

/// Find the discovered model a vision projector belongs to.
fn find_mmproj<'a>(models: &'a [LocalModel], path: &str) -> Option<&'a LocalModel> {
    models
        .iter()
        .find(|m| m.mmproj.iter().any(|s| s.path == path))
}

/// The profile ids currently in the catalog, or `None` when there is no catalog to read.
///
/// Best effort by design: "I could not read a catalog" must never become "your profile was
/// deleted". The file is stat'ed before any lock is taken, so validating a recipe never
/// creates `$STATE` as a side effect.
fn known_profile_ids() -> Option<Vec<ProfileId>> {
    let paths = Paths::resolve().ok()?;
    let file = paths.catalog_file();
    if !file.exists() {
        return None;
    }
    let c = read_file(&file).ok()?;
    Some(c.profiles.into_iter().map(|p| p.id).collect())
}

/// Host part of a URL, without scheme, port or path.
fn host_of(url: &str) -> &str {
    let no_scheme = url.rsplit("//").next().unwrap_or(url);
    let host = no_scheme.split('/').next().unwrap_or(no_scheme);
    host.split(':').next().unwrap_or(host)
}

/// Lowercase, keep `[a-z0-9._-]`, collapse everything else to `-`, guarantee a leading
/// `[a-z0-9]`, never return an empty string and never exceed 64 bytes.
///
/// The postcondition is exactly the id charset `^[a-z0-9][a-z0-9._-]{0,63}$`, which is what
/// makes [`id_from`] total.
fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(64));
    let mut last_dash = false;
    for ch in s.chars() {
        let c = ch.to_ascii_lowercase();
        let keep = c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_' || c == '-';
        if keep {
            if c == '-' {
                if last_dash {
                    continue;
                }
                last_dash = true;
            } else {
                last_dash = false;
            }
            out.push(c);
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out
        .chars()
        .next()
        .is_some_and(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit()))
    {
        out.remove(0);
    }
    out.truncate(64);
    while out.ends_with('-') || out.ends_with('.') || out.ends_with('_') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("item");
    }
    out
}

/// Build a validated id from arbitrary human text.
///
/// Total by construction: [`slugify`]'s postcondition **is** the id charset, so the `Err` arm
/// cannot be reached. It is spelled out rather than papered over, because silently
/// substituting some other id would be worse than the crash —
/// `slugify_always_produces_a_parseable_id` hammers the postcondition with every input class
/// that could break it.
fn id_from<T, E>(raw: &str, parse: impl Fn(&str) -> std::result::Result<T, E>) -> T {
    let slug = slugify(raw);
    match parse(&slug) {
        Ok(v) => v,
        Err(_) => unreachable!("slugify produced {slug:?}, which is outside the id charset"),
    }
}

/// A generated id no existing entry is using: `label`, slugified, then `-2`, `-3`, …
///
/// `fallback` is used when the label slugifies to nothing meaningful, so a nameless draft
/// still gets `recipe`, `recipe-2`, `recipe-3`.
fn unique_id<T, E>(
    label: &str,
    fallback: &str,
    taken: &[&str],
    parse: impl Fn(&str) -> std::result::Result<T, E> + Copy,
) -> T {
    let base = {
        let s = slugify(label);
        if s == "item" {
            slugify(fallback)
        } else {
            s
        }
    };
    if !taken.contains(&base.as_str()) {
        return id_from(&base, parse);
    }
    // Terminates: `taken` is finite, so at most `taken.len() + 1` suffixes can collide.
    let mut n = 2usize;
    loop {
        let suffix = format!("-{n}");
        let head = base.len().min(64usize.saturating_sub(suffix.len()));
        let trimmed = base.get(..head).unwrap_or(base.as_str());
        let candidate = format!("{trimmed}{suffix}");
        if !taken.contains(&candidate.as_str()) {
            return id_from(&candidate, parse);
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_protocol::{
        BackendId, BuildId, ContainerLaunch, ContainerRuntime, CredentialSource, DesiredState,
        FitPlan, FitVerdict, GgufMeta, Gpu, GpuBackend, InstanceId, KvType, LlamaBuild,
        LocalLlamaSpec, LocalVllmSpec, ModelShard, NglPlan, NodeSpec, Protocol, SamplingMode,
        SplitMode, SplitPlan, TriState, VastSpec,
    };
    use std::collections::BTreeMap;
    use std::sync::{Mutex, OnceLock};

    /// `Paths::resolve()` reads process-global environment, so tests that redirect it
    /// serialise here.
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// A `Paths` rooted in a fresh tempdir. Nothing here ever touches the real `$HOME`.
    fn test_paths() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = {
            let guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
            let prev = std::env::var_os("APEXROUTER_HOME");
            std::env::set_var("APEXROUTER_HOME", dir.path());
            let p = Paths::resolve();
            match prev {
                Some(v) => std::env::set_var("APEXROUTER_HOME", v),
                None => std::env::remove_var("APEXROUTER_HOME"),
            }
            drop(guard);
            p.expect("Paths::resolve")
        };
        assert!(paths.state().starts_with(dir.path()));
        (dir, paths)
    }

    /// Run `f` with `$APEXROUTER_HOME` pointed at `state`, so the functions that resolve
    /// paths from the environment see the test's catalog and not the real one.
    fn with_home<R>(state: &Path, f: impl FnOnce() -> R) -> R {
        let guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("APEXROUTER_HOME");
        std::env::set_var("APEXROUTER_HOME", state);
        let out = f();
        match prev {
            Some(v) => std::env::set_var("APEXROUTER_HOME", v),
            None => std::env::remove_var("APEXROUTER_HOME"),
        }
        drop(guard);
        out
    }

    fn local_spec(model_path: &str) -> LocalLlamaSpec {
        LocalLlamaSpec {
            build: BuildId::parse("build-vulkan").expect("id"),
            model_path: model_path.to_string(),
            mmproj: None,
            alias_flag: "Carnice-9b-Q6_K".into(),
            host: "127.0.0.1".into(),
            port: Some(8100),
            ctx: Some(16_384),
            parallel: Some(4),
            kv_type: Some(KvType::Q8_0),
            ngl: NglPlan::All,
            split: SplitPlan {
                devices: vec!["Vulkan0".into()],
                mode: SplitMode::Layer,
                main_gpu: None,
                tensor_split: None,
            },
            mode: SamplingMode::Thinking,
            flash_attn: Some(TriState::Auto),
            api_key: None,
            extra_args: vec!["--no-warmup".into()],
        }
    }

    fn recipe(label: &str, model_path: &str) -> Recipe {
        Recipe {
            id: RecipeId::parse("typed-by-a-human").expect("id"),
            label: label.to_string(),
            description: Some("a hand-made draft".into()),
            kind: RecipeKind::Local(local_spec(model_path)),
            provenance: Provenance2 {
                discovered_at_unix: 1_785_412_331,
                size_bytes: Some(7_600_000_000),
                source: model_path.to_string(),
                fit: None,
            },
            created_at_unix: 1_785_412_331,
            updated_at_unix: 1_785_412_331,
        }
    }

    fn launch() -> ContainerLaunch {
        let mut env = BTreeMap::new();
        env.insert("MODEL_REPO".into(), "unsloth/Qwen3-Coder".into());
        ContainerLaunch {
            runtime: ContainerRuntime::LlamaCpp,
            image: "ghcr.io/buckster123/vastai-gguf:prebuilt".into(),
            image_type: ImageType::Prebuilt,
            disk_gb: 80,
            env,
            onstart: "bash /app/launch.sh > /var/log/launch.log 2>&1 &".into(),
            host: "127.0.0.1".into(),
            port: 8000,
            expose_public: false,
        }
    }

    fn rig_with_build() -> RigSnapshot {
        RigSnapshot {
            gpus: vec![Gpu {
                device: "Vulkan0".into(),
                index: 0,
                name: "AMD Radeon 840M Graphics (RADV KRACKAN1)".into(),
                backend: GpuBackend::Vulkan,
                vram_total_mb: 16_384,
                vram_free_mb: 12_000,
                pci_bus_id: None,
                driver: None,
                is_software: false,
                seen_by_builds: vec![],
                held_by: vec![],
                reserved_mb: 0,
            }],
            builds: vec![LlamaBuild {
                id: BuildId::parse("build-vulkan").expect("id"),
                server_path: "/home/andre/llama.cpp/build-vulkan/bin/llama-server".into(),
                label: "vulkan".into(),
                build_info: Some("b9199 (39cf5d619)".into()),
                backends: vec![GpuBackend::Vulkan],
                devices: vec!["Vulkan0".into()],
                flags: Default::default(),
                probed_at_unix: 1_785_412_331,
            }],
            ram_total_mb: 22_000,
            ram_free_mb: 9_000,
            swap_total_mb: 8_000,
            swap_used_mb: 1_000,
            cpu_threads: 12,
            scanned_at_unix: 1_785_412_331,
        }
    }

    fn model(path: &str, bytes: u64) -> LocalModel {
        LocalModel {
            id: "carnice-9b".into(),
            name: "Carnice-9b-Q6_K".into(),
            dir: "/models".into(),
            shards: vec![ModelShard {
                path: path.to_string(),
                bytes,
            }],
            total_bytes: bytes,
            mmproj: vec![],
            quant: Some("Q6_K".into()),
            gguf: Some(GgufMeta::default()),
            discovered_at_unix: 1_785_412_331,
        }
    }

    fn endpoint(spec: EndpointSpec) -> EndpointRecord {
        EndpointRecord {
            id: BackendId::parse("local-carnice").expect("id"),
            spec,
            desired: DesiredState::Running,
            proc: None,
            port: Some(8100),
            log_path: None,
            started_at_unix: 1_785_412_331,
            fit: None,
            adopted: false,
            alias_bindings: vec![],
        }
    }

    // -- the acceptance test: comments survive a GUI edit ---------------------------------

    #[test]
    fn a_hand_written_comment_survives_a_gui_edit() {
        let (_dir, paths) = test_paths();
        let saved =
            upsert_recipe(&paths, recipe("Carnice 9B", "/models/carnice.gguf")).expect("upsert");

        // Somebody opens the file and writes in it, as people do.
        let text = std::fs::read_to_string(paths.catalog_file()).expect("read");
        let commented = format!(
            "# the catalog. hand-written, and staying that way.\n\n{}",
            text.replace("[[recipes]]", "# this one is the daily driver\n[[recipes]]")
                .replace("label = ", "# renamed 2026-07-30\nlabel = ")
        );
        std::fs::write(paths.catalog_file(), &commented).expect("write");

        // …then edits it from the GUI: rename it, add a second recipe, add a profile.
        let mut edited = load(&paths).expect("load").recipes.remove(0);
        assert_eq!(
            edited.id, saved.id,
            "the generated id is stable across a load"
        );
        edited.label = "Carnice 9B (daily)".into();
        upsert_recipe(&paths, edited).expect("rename");
        upsert_recipe(&paths, recipe("Qwen 30B", "/models/qwen.gguf")).expect("second");
        upsert_profile(&paths, default_profiles().remove(0)).expect("profile");

        let after = std::fs::read_to_string(paths.catalog_file()).expect("read");
        assert!(
            after.contains("# the catalog. hand-written, and staying that way."),
            "the document comment was lost:\n{after}"
        );
        assert!(
            after.contains("# this one is the daily driver"),
            "the entry comment was lost:\n{after}"
        );
        assert!(
            after.contains("# renamed 2026-07-30"),
            "the key comment was lost:\n{after}"
        );
        assert!(
            after.contains("Carnice 9B (daily)"),
            "the edit was lost:\n{after}"
        );

        let back = load(&paths).expect("load");
        assert_eq!(back.recipes.len(), 2);
        assert_eq!(back.recipes[0].label, "Carnice 9B (daily)");
        assert_eq!(back.recipes[0].id, saved.id);
        assert_eq!(back.profiles.len(), 1);
    }

    #[test]
    fn an_unknown_key_and_its_comment_survive_a_write() {
        let (_dir, paths) = test_paths();
        std::fs::create_dir_all(paths.state()).expect("mkdir");
        std::fs::write(
            paths.catalog_file(),
            "# written by a newer apexrouter\nschema_version = 7\n",
        )
        .expect("write");
        upsert_recipe(&paths, recipe("a", "/models/a.gguf")).expect("upsert");
        let text = std::fs::read_to_string(paths.catalog_file()).expect("read");
        assert!(text.contains("schema_version = 7"), "{text}");
        assert!(text.contains("# written by a newer apexrouter"), "{text}");
    }

    // -- generated ids -------------------------------------------------------------------

    #[test]
    fn ids_are_generated_never_typed_and_are_unique() {
        let (_dir, paths) = test_paths();
        let a = upsert_recipe(&paths, recipe("Carnice 9B", "/models/a.gguf")).expect("a");
        let b = upsert_recipe(&paths, recipe("Carnice 9B", "/models/b.gguf")).expect("b");
        let c = upsert_recipe(&paths, recipe("Carnice 9B", "/models/c.gguf")).expect("c");

        // The typed id never survives.
        assert_ne!(a.id.as_str(), "typed-by-a-human");
        assert_eq!(a.id.as_str(), "carnice-9b");
        assert_eq!(b.id.as_str(), "carnice-9b-2");
        assert_eq!(c.id.as_str(), "carnice-9b-3");

        let ids: Vec<String> = load(&paths)
            .expect("load")
            .recipes
            .iter()
            .map(|r| r.id.as_str().to_string())
            .collect();
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "ids collided: {ids:?}");

        // An update keeps the id and the creation stamp, and moves `updated_at`.
        let mut again = a.clone();
        again.label = "renamed".into();
        again.updated_at_unix = 0;
        let updated = upsert_recipe(&paths, again).expect("update");
        assert_eq!(updated.id, a.id);
        assert_eq!(updated.created_at_unix, a.created_at_unix);
        assert!(updated.updated_at_unix >= a.created_at_unix);
        assert_eq!(load(&paths).expect("load").recipes.len(), 3);
    }

    #[test]
    fn profile_ids_are_generated_too() {
        let (_dir, paths) = test_paths();
        let mut p = default_profiles().remove(0);
        p.id = ProfileId::parse("whatever-the-user-typed").expect("id");
        let saved = upsert_profile(&paths, p.clone()).expect("save");
        assert_eq!(saved.id.as_str(), "rtx-3090-2-4");
        let second = upsert_profile(&paths, p).expect("save");
        assert_eq!(second.id.as_str(), "rtx-3090-2-4-2");
    }

    #[test]
    fn a_nameless_draft_still_gets_an_id() {
        let (_dir, paths) = test_paths();
        let mut draft = recipe("", "/models/a.gguf");
        draft.label = "   ".into();
        let saved = upsert_recipe(&paths, draft).expect("draft");
        assert_eq!(saved.id.as_str(), "recipe");
    }

    #[test]
    fn slugify_always_produces_a_parseable_id() {
        let long_ascii = "x".repeat(300);
        let long_unicode = "é".repeat(300);
        for raw in [
            "",
            "   ",
            "///",
            "----",
            "...",
            "___",
            "Ünïcødé ✨ модель",
            "H100 ×2 (SXM/NVL/PCIE — PCIE is slower)",
            "9",
            "-9-",
            long_ascii.as_str(),
            long_unicode.as_str(),
        ] {
            let slug = slugify(raw);
            assert!(
                RecipeId::parse(&slug).is_ok(),
                "slugify({raw:?}) = {slug:?} is not a valid id"
            );
            assert!(ProfileId::parse(&slug).is_ok());
            assert!(ProviderId::parse(&slug).is_ok());
        }
    }

    // -- validation ----------------------------------------------------------------------

    #[test]
    fn staleness_is_a_warning_with_a_fix_never_an_error() {
        let mut r = recipe("Carnice 9B", "/models/gone.gguf");
        r.kind = RecipeKind::Local(LocalLlamaSpec {
            build: BuildId::parse("build-that-was-deleted").expect("id"),
            mmproj: Some("/models/gone-mmproj.gguf".into()),
            split: SplitPlan {
                devices: vec!["CUDA7".into()],
                ..SplitPlan::default()
            },
            ..local_spec("/models/gone.gguf")
        });

        let report = validate_recipe(&r, &rig_with_build(), &[]);
        assert!(
            report.ok,
            "staleness must never make a recipe invalid: {report:?}"
        );
        assert!(report
            .issues
            .iter()
            .all(|i| i.severity == Severity::Warning));
        assert!(report.issues.iter().all(|i| i.fix.is_some()), "{report:?}");

        let says = |needle: &str| report.issues.iter().any(|i| i.message.contains(needle));
        assert!(says("model file gone"), "{report:?}");
        assert!(says("build removed"), "{report:?}");
        assert_eq!(report.issues.len(), 4, "{report:?}");
    }

    #[test]
    fn a_deleted_profile_is_a_warning_on_a_vast_recipe() {
        let (_dir, paths) = test_paths();
        upsert_profile(&paths, default_profiles().remove(0)).expect("profile");

        let mut r = recipe("rented", "/models/a.gguf");
        r.kind = RecipeKind::Vast {
            profile: ProfileId::parse("h100-2").expect("id"),
            launch: launch(),
            fit: None,
        };
        let report = with_home(paths.state(), || {
            validate_recipe(&r, &rig_with_build(), &[])
        });

        assert!(report.ok, "{report:?}");
        let found = report.issues.iter().find(|i| i.field == "kind.profile");
        let Some(found) = found else {
            panic!("no profile issue in {report:?}");
        };
        assert_eq!(found.severity, Severity::Warning);
        assert!(found.message.contains("profile deleted"), "{found:?}");
        assert!(found.fix.is_some());
    }

    #[test]
    fn an_unassigned_profile_is_a_warning_and_a_missing_catalog_is_silent() {
        let (_dir, paths) = test_paths();
        let mut r = recipe("rented", "/models/a.gguf");
        r.kind = RecipeKind::Vast {
            profile: ProfileId::parse(UNASSIGNED_PROFILE).expect("id"),
            launch: launch(),
            fit: None,
        };

        // No catalog on disk at all: we do not know, so we say nothing.
        let silent = with_home(paths.state(), || {
            validate_recipe(&r, &rig_with_build(), &[])
        });
        assert!(silent.issues.is_empty(), "{silent:?}");

        upsert_profile(&paths, default_profiles().remove(0)).expect("profile");
        let report = with_home(paths.state(), || {
            validate_recipe(&r, &rig_with_build(), &[])
        });
        assert!(report.ok);
        assert_eq!(report.issues.len(), 1, "{report:?}");
        assert_eq!(report.issues[0].severity, Severity::Warning);
        assert!(report.issues[0].message.contains("profile deleted"));
    }

    #[test]
    fn a_valid_local_recipe_has_nothing_to_say() {
        let r = recipe("Carnice 9B", "/models/carnice.gguf");
        let models = [model("/models/carnice.gguf", 7_600_000_000)];
        let report = validate_recipe(&r, &rig_with_build(), &models);
        assert!(report.ok);
        assert!(report.issues.is_empty(), "{report:?}");
    }

    #[test]
    fn weights_that_changed_size_are_a_warning() {
        let r = recipe("Carnice 9B", "/models/carnice.gguf");
        let models = [model("/models/carnice.gguf", 42)];
        let report = validate_recipe(&r, &rig_with_build(), &models);
        assert!(report.ok);
        assert_eq!(report.issues.len(), 1, "{report:?}");
        assert_eq!(report.issues[0].severity, Severity::Warning);
        assert_eq!(report.issues[0].field, "provenance.size_bytes");
        assert!(report.issues[0].fix.is_some());
    }

    #[test]
    fn structural_nonsense_is_an_error() {
        let mut r = recipe("", "");
        r.kind = RecipeKind::Local(local_spec(""));
        let report = validate_recipe(&r, &rig_with_build(), &[]);
        assert!(!report.ok);
        assert_eq!(
            report
                .issues
                .iter()
                .filter(|i| i.severity == Severity::Error)
                .count(),
            2,
            "{report:?}"
        );
    }

    #[test]
    fn a_credential_inlined_into_onstart_is_an_error() {
        let mut r = recipe("rented", "/models/a.gguf");
        let mut l = launch();
        l.onstart = "HF_TOKEN=hf_secret bash /app/launch.sh".into();
        r.kind = RecipeKind::Vast {
            profile: ProfileId::parse(UNASSIGNED_PROFILE).expect("id"),
            launch: l,
            fit: None,
        };
        let report = validate_recipe(&r, &rig_with_build(), &[]);
        assert!(!report.ok, "{report:?}");
    }

    #[test]
    fn a_managed_recipe_with_a_v1_suffix_is_a_warning() {
        let mut r = recipe("together", "/models/a.gguf");
        r.kind = RecipeKind::Managed(ManagedSpec {
            provider: ProviderId::parse("together").expect("id"),
            base_url: "https://api.together.xyz/v1".into(),
            credential: CredentialSource::None,
            model_id: None,
            protocol: Protocol::OpenAi,
        });
        let report = validate_recipe(&r, &rig_with_build(), &[]);
        assert!(report.ok);
        assert_eq!(report.issues.len(), 1, "{report:?}");
        assert_eq!(report.issues[0].severity, Severity::Warning);
    }

    // -- default profiles ----------------------------------------------------------------

    #[test]
    fn default_profiles_use_the_exact_gpu_name_strings_and_ranges() {
        let p = default_profiles();
        assert_eq!(p.len(), 4);

        let ids: Vec<&str> = p.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids, ["rtx3090-2-4", "rtx3090-4-perf", "h100-1", "h100-2"]);

        // The exact strings verified in docs/port/00c, and nothing else.
        let vocabulary = ["RTX 3090", "H100 SXM", "H100 NVL", "H100 PCIE"];
        for prof in &p {
            assert!(!prof.gpu_names.is_empty(), "{prof:?}");
            for name in &prof.gpu_names {
                assert!(
                    vocabulary.contains(&name.as_str()),
                    "{name:?} is not one of the verified gpu_name strings"
                );
            }
            assert!(prof.num_gpus_min >= 1);
            assert!(prof.num_gpus_max >= prof.num_gpus_min);
            assert!(prof.min_reliability >= 0.98);
            assert!(prof.max_dph.is_some_and(|m| m > Money::ZERO));
            assert!(prof.min_disk_gb >= 80);
            assert!(prof.min_inet_down >= 300);
            assert_eq!(prof.geo, GeoFilter::Any);
        }

        // The 3090 tier spans 2–4 in ONE profile, never one profile per count.
        assert_eq!((p[0].num_gpus_min, p[0].num_gpus_max), (2, 4));
        assert_eq!(p[0].gpu_names, ["RTX 3090"]);
        // The H100 tiers carry all three spellings, because the market picks which is cheap.
        assert_eq!(p[2].gpu_names.len(), 3);
        assert_eq!(p[3].gpu_names.len(), 3);
    }

    // -- save this running thing as a recipe ---------------------------------------------

    #[test]
    fn recipe_from_endpoint_maps_every_spec() {
        let local = recipe_from_endpoint(
            &endpoint(EndpointSpec::LocalLlama(local_spec("/models/a.gguf"))),
            "Daily driver",
        );
        assert_eq!(local.label, "Daily driver");
        assert_eq!(local.id.as_str(), "daily-driver");
        assert_eq!(local.provenance.source, "endpoint local-carnice");
        assert_eq!(local.provenance.discovered_at_unix, 1_785_412_331);
        assert!(matches!(local.kind, RecipeKind::Local(_)));

        let vllm = recipe_from_endpoint(
            &endpoint(EndpointSpec::LocalVllm(LocalVllmSpec {
                bin: "/usr/bin/vllm".into(),
                model_id: "Qwen/Qwen3-8B".into(),
                tp: Some(2),
                ctx: Some(32_768),
                quantization: None,
                kv_cache_dtype: None,
                enforce_eager: false,
                reasoning_parser: None,
                gpu_util: Some(0.92),
                max_num_seqs: None,
                trust_remote: false,
                chunked_prefill: true,
                host: "127.0.0.1".into(),
                port: None,
                devices: vec![],
                extra_args: vec![],
            })),
            "vllm",
        );
        assert!(matches!(vllm.kind, RecipeKind::LocalVllm(_)));

        // A rental keeps its container but cannot know the query that found the box.
        let rented = recipe_from_endpoint(
            &endpoint(EndpointSpec::Vast(VastSpec {
                instance_id: InstanceId(43_731_729),
                runtime: ContainerRuntime::LlamaCpp,
                launch: launch(),
                tunnel: None,
            })),
            "4×3090",
        );
        match &rented.kind {
            RecipeKind::Vast {
                profile, launch, ..
            } => {
                assert_eq!(profile.as_str(), UNASSIGNED_PROFILE);
                assert_eq!(launch.port, 8000);
            }
            other => panic!("wrong kind: {other:?}"),
        }

        // A Node is a Managed provider minus a lifecycle.
        let node = recipe_from_endpoint(
            &endpoint(EndpointSpec::Node(NodeSpec {
                base_url: "https://api.together.xyz".into(),
                credential: CredentialSource::None,
                label: "together".into(),
                declared_models: vec!["deepseek-ai/DeepSeek-V4-Pro".into()],
                protocol: Protocol::OpenAi,
            })),
            "together node",
        );
        match &node.kind {
            RecipeKind::Managed(m) => {
                assert_eq!(m.provider.as_str(), "api.together.xyz");
                assert_eq!(m.base_url, "https://api.together.xyz");
                assert_eq!(m.model_id.as_deref(), Some("deepseek-ai/DeepSeek-V4-Pro"));
            }
            other => panic!("wrong kind: {other:?}"),
        }

        let managed = recipe_from_endpoint(
            &endpoint(EndpointSpec::Managed(ManagedSpec {
                provider: ProviderId::parse("together").expect("id"),
                base_url: "https://api.together.xyz".into(),
                credential: CredentialSource::None,
                model_id: None,
                protocol: Protocol::OpenAi,
            })),
            "together",
        );
        assert!(matches!(managed.kind, RecipeKind::Managed(_)));
    }

    #[test]
    fn a_recipe_saved_from_an_endpoint_is_storable_as_is() {
        let (_dir, paths) = test_paths();
        let r = recipe_from_endpoint(
            &endpoint(EndpointSpec::LocalLlama(local_spec("/models/a.gguf"))),
            "Daily driver",
        );
        let saved = upsert_recipe(&paths, r).expect("save");
        assert_eq!(saved.id.as_str(), "daily-driver");
        assert_eq!(load(&paths).expect("load").recipes, vec![saved]);
    }

    // -- persistence ---------------------------------------------------------------------

    #[test]
    fn a_missing_file_is_an_empty_catalog() {
        let (_dir, paths) = test_paths();
        let c = load(&paths).expect("load");
        assert_eq!(c, Catalog::default());
        assert!(
            !paths.catalog_file().exists(),
            "load must not create the file"
        );
    }

    #[test]
    fn every_field_survives_a_round_trip() {
        let (_dir, paths) = test_paths();
        let mut r = recipe("Carnice 9B", "/models/carnice.gguf");
        r.id = RecipeId::parse("carnice-9b").expect("id");
        r.provenance.fit = Some(FitPlan {
            ctx: 16_384,
            parallel: 4,
            kv_type: KvType::Q8_0,
            ngl: NglPlan::Layers(48),
            split: SplitPlan {
                devices: vec!["Vulkan0".into(), "Vulkan1".into()],
                mode: SplitMode::Row,
                main_gpu: Some(0),
                tensor_split: Some(vec![0.5, 0.5]),
            },
            weights_mb: 7_200,
            kv_mb: 1_100,
            compute_mb: 400,
            headroom_mb: 900,
            per_device_mb: vec![("Vulkan0".into(), 4_400), ("Vulkan1".into(), 4_300)],
            verdict: FitVerdict::Fits { headroom_mb: 900 },
            why: vec!["weights 7.03 GiB".into()],
        });
        let mut vast = recipe("rented", "/models/x.gguf");
        vast.id = RecipeId::parse("rented").expect("id");
        vast.kind = RecipeKind::Vast {
            profile: ProfileId::parse("h100-2").expect("id"),
            launch: launch(),
            fit: None,
        };
        let mut profile = default_profiles().remove(0);
        profile
            .extra
            .insert("cuda_max_good".into(), serde_json::json!({ "gte": 12.4 }));
        let c = Catalog {
            recipes: vec![r, vast],
            profiles: {
                let mut v = default_profiles();
                v[0] = profile;
                v
            },
        };

        save(&paths, &c).expect("save");
        assert_eq!(load(&paths).expect("load"), c);

        // Idempotent: saving what we just loaded changes not one byte.
        let first = std::fs::read_to_string(paths.catalog_file()).expect("read");
        save(&paths, &load(&paths).expect("load")).expect("save again");
        assert_eq!(
            std::fs::read_to_string(paths.catalog_file()).expect("read"),
            first
        );
    }

    #[test]
    fn the_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, paths) = test_paths();
        save(&paths, &Catalog::default()).expect("save");
        let mode = std::fs::metadata(paths.catalog_file())
            .expect("metadata")
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, FILE_MODE);
    }

    #[test]
    fn a_field_that_becomes_none_stops_being_written() {
        let (_dir, paths) = test_paths();
        let saved = upsert_recipe(&paths, recipe("Carnice 9B", "/models/a.gguf")).expect("save");
        assert!(std::fs::read_to_string(paths.catalog_file())
            .expect("read")
            .contains("description"));

        let mut edited = saved;
        edited.description = None;
        edited.provenance.size_bytes = None;
        upsert_recipe(&paths, edited).expect("edit");

        let text = std::fs::read_to_string(paths.catalog_file()).expect("read");
        assert!(!text.contains("description"), "{text}");
        assert!(!text.contains("size_bytes"), "{text}");
        assert!(load(&paths).expect("load").recipes[0].description.is_none());
    }

    #[test]
    fn removal_drops_the_entry_and_an_unknown_id_is_reported() {
        let (_dir, paths) = test_paths();
        let a = upsert_recipe(&paths, recipe("alpha", "/models/a.gguf")).expect("a");
        let b = upsert_recipe(&paths, recipe("beta", "/models/b.gguf")).expect("b");
        let p = upsert_profile(&paths, default_profiles().remove(2)).expect("p");

        remove_recipe(&paths, &a.id).expect("remove");
        let left = load(&paths).expect("load");
        assert_eq!(left.recipes.len(), 1);
        assert_eq!(left.recipes[0].id, b.id);
        let text = std::fs::read_to_string(paths.catalog_file()).expect("read");
        assert!(!text.contains("alpha"), "{text}");

        assert!(matches!(
            remove_recipe(&paths, &a.id),
            Err(Error::NotFound(_))
        ));
        remove_profile(&paths, &p.id).expect("remove profile");
        assert!(load(&paths).expect("load").profiles.is_empty());
        assert!(matches!(
            remove_profile(&paths, &p.id),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn a_document_that_does_not_parse_is_never_clobbered() {
        let (_dir, paths) = test_paths();
        std::fs::create_dir_all(paths.state()).expect("mkdir");
        std::fs::write(paths.catalog_file(), "this is not toml = = =\n").expect("write");
        assert!(matches!(load(&paths), Err(Error::Toml(_))));
        assert!(matches!(
            save(&paths, &Catalog::default()),
            Err(Error::Invalid { .. })
        ));
        assert_eq!(
            std::fs::read_to_string(paths.catalog_file()).expect("read"),
            "this is not toml = = =\n"
        );
    }
}
