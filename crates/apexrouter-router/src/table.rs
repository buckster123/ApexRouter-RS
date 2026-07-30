//! OWNER: unit R-01 (router/src/table.rs, router/src/registry.rs). Do not edit outside that
//! unit.
//!
//! The compiled routing table.
//!
//! Reload is parse → compile → validate → `ArcSwap::store`. **A failed compile keeps the
//! running table**, raises an `Alert`, and shows red in both GUIs and in `apexrouter
//! status`. Compile-time validation rejects: a dangling target, a duplicate alias, an alias
//! that shadows a live upstream id (unless `allow_shadow`), an unsatisfiable `require_tags`,
//! and `Strategy::Cheapest` on a route where no target has a price model *and* no
//! `tps_hint` — that would be an invented ordering.

use crate::registry::{BackendRegistry, LiveBackend};
use apexrouter_core::config::Config;
use apexrouter_protocol::{Alias, BackendId, RouteFile, ValidationReport};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// One alias, resolved down to the backends it can actually reach.
pub struct CompiledRoute {/* R-01 */}

/// The read-only structure the request path loads on every request.
pub struct RoutingTable {
    /// Rule 1.
    by_alias: HashMap<Alias, CompiledRoute>,
    /// Rules 3 and 4. **This is what makes every existing client work unchanged.**
    by_upstream_id: HashMap<String, Vec<Arc<LiveBackend>>>,
    /// Rule 2, the explicit pin.
    by_id: HashMap<BackendId, Arc<LiveBackend>>,
    /// Where rule 5 and `unknown_model = "fallback"` land.
    default_alias: Alias,
    /// `""`, `"x"`, `"auto"`, `"default"` — why `smoke.sh` keeps working.
    legacy_model_names: HashSet<String>,
    /// Bumped on every recompile, so a stale reader is detectable.
    generation: u64,
}

/// Compiles a `RouteFile` plus a registry into a `RoutingTable`.
pub struct TableBuilder;

impl TableBuilder {
    /// Clone `Arc`s out of the registry so live state is carried across, and validate.
    ///
    /// Returns a `ValidationReport` rather than an opaque error, so the caller can keep the
    /// old table serving and render exactly what was wrong.
    pub fn compile(
        cfg: &Config,
        routes: &RouteFile,
        reg: &BackendRegistry,
    ) -> std::result::Result<RoutingTable, ValidationReport> {
        todo!("R-01: TableBuilder::compile")
    }
}

impl RoutingTable {
    /// Which generation this table is. Bumped on every successful compile.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The alias legacy and unknown model names fall through to.
    pub fn default_alias(&self) -> &Alias {
        &self.default_alias
    }
}
