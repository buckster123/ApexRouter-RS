//! OWNER: unit P-02 (providers/src/vast/{api,query,mod}.rs). Do not edit outside that unit —
//! `offers.rs` is P-03 and `{rent,boot,stall}.rs` are P-04.
//!
//! vast.ai. **Nothing that costs money happens without a `SpendApproval`**, the ledger row
//! is written before the billing call, and **instances are never auto-destroyed on daemon
//! shutdown, at any setting.** A crash must not delete a paid box.

pub mod api;
pub mod boot;
pub mod offers;
pub mod query;
pub mod rent;
pub mod stall;

pub use api::{FixtureVast, VastApi, VastApiHttp};
pub use boot::watch_boot;
pub use offers::{gpu_name_vocabulary, profile_to_query, search_unified, QueryOverrides};
pub use query::build_query;
pub use rent::{rent, VastProvisioner};
pub use stall::{restart_download, sample_download};
