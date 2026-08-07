//! ApexRouter-RS core: paths, config, secrets, atomic store and locks, process identity,
//! the argv builder, discovery, the fit solver, upstream probing, pricing, usage, the
//! ledger, catalog, migration and the `Check` registry.
//!
//! **No axum, no provisioning, and no dependency on `apexrouter-providers`** — that edge
//! would be a cycle. Consequences, stated so nobody re-introduces it:
//!
//! * HTTP probing of an OpenAI-compatible upstream lives in [`upstream`], not in a provider.
//! * [`pricing`] is a *data* table fed by providers at runtime; it never calls one.
//! * [`checks`] defines `trait Check` and the concurrent runner; checks needing
//!   vast/together/ssh live in `apexrouter_providers::checks` and are registered by
//!   `apexrouter-server` at startup.
//! * `Secret`/`CredentialRef` live here, and `apexrouter-protocol` never carries key
//!   material — it carries `CredentialSource`, a description.
//!
//! Stage 0 skeleton: every module below is a stub owned by a later work unit. See
//! `docs/BUILD-PLAN.md` §5 for the ownership index.

#![allow(unused)]

pub mod argv;
pub mod catalog;
pub mod checks;
pub mod config;
pub mod discover;
pub mod error;
pub mod exec;
pub mod fit;
pub mod ledger;
pub mod lockfile;
pub mod migrate;
pub mod money;
pub mod paths;
pub mod pricing;
pub mod proc;
pub mod secret;
pub mod store;
pub mod studio;
pub mod upstream;
pub mod usage;

pub use config::Config;
pub use error::{Error, Result};
pub use paths::Paths;
pub use secret::{CredentialRef, ResolvedCredential, Secret};
pub use store::Store;

/// Re-exported so a downstream crate needs one `use` line, not two.
pub use apexrouter_protocol as protocol;
