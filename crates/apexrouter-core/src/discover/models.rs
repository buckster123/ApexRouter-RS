//! OWNER: unit C-10 (core/discover/models.rs, core/discover/gguf.rs). Do not edit outside
//! that unit.
//!
//! Local weight discovery. Recurses into per-model subdirectories, follows symlinks,
//! honours `ignore_globs`, groups `-00001-of-000NN` shards into **one** `LocalModel` with a
//! summed size, pairs `mmproj-*.gguf` into `LocalModel::mmproj`, and matches `mmproj`/
//! `vocab` as **filename tokens, not path substrings** — a directory named `vocab-x` must
//! not hide its contents.

use crate::config::EndpointsCfg;
use crate::error::Result;
use apexrouter_protocol::LocalModel;

/// Walk `model_roots` and return one entry per logical model, **smallest first**.
///
/// A models directory with nothing in it is normal, not an error.
pub async fn discover_models(cfg: &EndpointsCfg) -> Result<Vec<LocalModel>> {
    todo!("C-10: discover_models")
}
