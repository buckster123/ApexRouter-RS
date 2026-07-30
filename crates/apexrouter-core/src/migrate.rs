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

use crate::config::{Config, DockerCfg, KnownFork};
use crate::error::Result;
use crate::paths::Paths;
use apexrouter_protocol::{MigrationPlan, MigrationReport, Recipe, SearchProfile};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Enumerate every legacy artefact and say what we would do with each.
pub fn plan(paths: &Paths, cfg: &Config) -> Result<MigrationPlan> {
    todo!("C-16: plan")
}

/// Execute a plan. Import-only; never destructive to the legacy tree.
pub fn apply(paths: &Paths, cfg: &Config, plan: &MigrationPlan) -> Result<MigrationReport> {
    todo!("C-16: apply")
}

/// Read `.active_endpoint` in **all four** shapes it has had.
pub fn read_legacy_active_endpoint(path: &Path) -> Result<Option<LegacyActiveEndpoint>> {
    todo!("C-16: read_legacy_active_endpoint")
}

/// The legacy "what is active" file, tolerant of every shape it has taken.
///
/// `activated_at` and `switched_at` are the same field under two names, and `pid` is
/// sometimes absent — hence the serde aliases and the `Option`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LegacyActiveEndpoint {/* C-16: all FOUR shapes via serde aliases activated_at|switched_at */}

/// Read `~/.vastai-gguf/local_instances/*.json`.
///
/// **Paths are validated on load** — a saved instance pointing at a model that no longer
/// exists is normal, and must show up as an importable-but-stale row rather than an error.
pub fn read_legacy_instances(dir: &Path) -> Result<Vec<LegacyLocalInstance>> {
    todo!("C-16: read_legacy_instances")
}

/// One row of `~/.vastai-gguf/local_instances/`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LegacyLocalInstance {/* C-16 */}

/// Import `recipes.toml`: the `[docker]` image map, the 7 `llama_cpp_repo`/`ref` mappings
/// as `known_forks` (genuinely undiscoverable knowledge), the GPU tiers as `SearchProfile`
/// seeds, the 3 `local` recipes and the 7 `together` recipes.
///
/// The trailing `Vec<String>` is the per-row skip reasons, including one for each of the 54
/// `vast_gguf` rows.
///
/// The five-tuple is the published signature: one legacy file yields five unrelated kinds of
/// thing, and naming a struct for a one-call-site import would be ceremony.
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
    todo!("C-16: import_recipes_toml")
}
