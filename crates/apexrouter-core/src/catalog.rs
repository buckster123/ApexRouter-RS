//! OWNER: unit C-15 (core/catalog.rs). Do not edit outside that unit.
//!
//! Recipes and search profiles, persisted at `$STATE/catalog.toml` through **`toml_edit`**
//! so a hand-written comment survives a GUI edit.
//!
//! Ids are **generated**, never typed by the user. Staleness — model file gone, build
//! removed, profile deleted — is reported as a `Warning` with a `fix` string, never an
//! `Error`: a recipe pointing at a model you deleted is normal.

use crate::error::Result;
use crate::paths::Paths;
use apexrouter_protocol::{
    EndpointRecord, LocalModel, ProfileId, Recipe, RecipeId, RigSnapshot, SearchProfile,
    ValidationReport,
};

/// Everything in `catalog.toml`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Catalog {
    /// Saved launch plans.
    pub recipes: Vec<Recipe>,
    /// Saved market queries.
    pub profiles: Vec<SearchProfile>,
}

/// Read `catalog.toml`. A missing file yields an empty catalog.
pub fn load(paths: &Paths) -> Result<Catalog> {
    todo!("C-15: load")
}

/// Write `catalog.toml` through `toml_edit`, atomically, preserving comments.
pub fn save(paths: &Paths, c: &Catalog) -> Result<()> {
    todo!("C-15: save")
}

/// Insert or replace one recipe, generating an id when it is new.
pub fn upsert_recipe(paths: &Paths, r: Recipe) -> Result<Recipe> {
    todo!("C-15: upsert_recipe")
}

/// Delete one recipe.
pub fn remove_recipe(paths: &Paths, id: &RecipeId) -> Result<()> {
    todo!("C-15: remove_recipe")
}

/// Insert or replace one search profile, generating an id when it is new.
pub fn upsert_profile(paths: &Paths, p: SearchProfile) -> Result<SearchProfile> {
    todo!("C-15: upsert_profile")
}

/// Delete one search profile.
pub fn remove_profile(paths: &Paths, id: &ProfileId) -> Result<()> {
    todo!("C-15: remove_profile")
}

/// Check a recipe against what actually exists on this machine right now.
pub fn validate_recipe(r: &Recipe, rig: &RigSnapshot, models: &[LocalModel]) -> ValidationReport {
    todo!("C-15: validate_recipe")
}

/// The seed profiles: 3090×2-4, 3090×4 (perf), H100×1, H100×2.
///
/// Uses the **exact** `gpu_name` strings verified in `docs/port/00c` — `"RTX 3090"`,
/// `"H100 SXM"`, `"H100 NVL"`, `"H100 PCIE"` — and `num_gpus_min/max` ranges, never one
/// profile per GPU count.
pub fn default_profiles() -> Vec<SearchProfile> {
    todo!("C-15: default_profiles")
}

/// "Save this running thing as a recipe."
pub fn recipe_from_endpoint(rec: &EndpointRecord, label: &str) -> Recipe {
    todo!("C-15: recipe_from_endpoint")
}
