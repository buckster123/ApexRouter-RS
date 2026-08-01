//! OWNER: unit P-02 (providers/src/vast/{api,query,mod}.rs). Do not edit outside that unit —
//! `offers.rs` is P-03 and `{rent,boot,stall}.rs` are P-04.
//!
//! vast.ai. **Nothing that costs money happens without a `SpendApproval`**, the ledger row
//! is written before the billing call, and **instances are never auto-destroyed on daemon
//! shutdown, at any setting.** A crash must not delete a paid box.
//!
//! The layering, so nothing reaches around it: [`api`] is the transport and knows nothing
//! about profiles, relaxation or money; `offers` turns a saved profile into the **one**
//! [`build_query`] shape; `rent`/`boot`/`stall` own the money path and the watchdog. Every
//! one of them takes `&dyn VastApi`, so [`FixtureVast`] can stand in for the market and no
//! test ever spends a cent.

pub mod api;
pub mod boot;
pub mod offers;
pub mod query;
pub mod rent;
pub mod stall;

pub use api::{FixtureCall, FixtureCreate, FixtureVast, VastApi, VastApiHttp};
pub use boot::watch_boot;
pub use offers::{
    constraint_failures, gpu_name_vocabulary, offer_matches, profile_to_query, search_unified,
    QueryOverrides,
};
pub use query::build_query;
pub use rent::{park, rent, rented_backend, wake, weekly_disk_usd, VastProvisioner};
pub use stall::{restart_download, sample_download};
