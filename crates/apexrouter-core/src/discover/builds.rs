//! OWNER: unit C-09 (core/discover/builds.rs, core/discover/mod.rs). Do not edit outside
//! that unit.
//!
//! Build and device discovery, and the three measured defects it closes:
//!
//! * A **fixed candidate list** misses `build-mtp` and `build-zaya1`. We glob
//!   `build*/bin/llama-server` under every configured root and `$PATH`, and label by
//!   build-dir name.
//! * **Backend detection by grepping `--help`** reports `cuda` on an AMD box. We use
//!   `llama-server --list-devices`, and fall back to inspecting sibling `libggml-*.so`.
//! * A **substring heuristic** silently chooses HIP when Vulkan was asked for.
//!   [`choose_build`] returns a `BinaryChoiceInfo` whose `exact: false` is a **visible**
//!   value the UI renders as a warning.

use crate::config::EndpointsCfg;
use crate::error::Result;
use apexrouter_protocol::{BinaryChoiceInfo, FlagSupport, Gpu, GpuBackend, LlamaBuild};
use std::path::Path;

/// Glob `build*/bin/llama-server` under every configured root and `$PATH`.
pub async fn discover_builds(cfg: &EndpointsCfg, cache: &Path) -> Result<Vec<LlamaBuild>> {
    todo!("C-09: discover_builds")
}

/// `llama-server --list-devices`. **Never** a `--help` grep. `llvmpipe` is marked
/// `is_software` and excluded from default selection.
pub async fn probe_devices(server: &Path) -> Result<Vec<Gpu>> {
    todo!("C-09: probe_devices")
}

/// `llama-server --help`, cached in `$CACHE` keyed by `(path, mtime, size)`.
///
/// Never a hardcoded whitelist: b9199 already moved `-fa` to `on|off|auto`, made `--jinja`
/// default-on and deprecated `--webui`.
pub async fn probe_flags(server: &Path, cache: &Path) -> Result<FlagSupport> {
    todo!("C-09: probe_flags")
}

/// Pick a build for a wanted backend, reporting a fallback rather than performing one
/// silently.
pub fn choose_build(builds: &[LlamaBuild], want: Option<GpuBackend>) -> Option<BinaryChoiceInfo> {
    todo!("C-09: choose_build")
}
