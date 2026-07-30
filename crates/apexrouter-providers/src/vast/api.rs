//! OWNER: unit P-02 (providers/src/vast/{api,query,mod}.rs). Do not edit outside that unit.
//!
//! The REST client, behind a trait so the money path can be tested for free.
//!
//! Verified facts this must honour (`docs/port/00c`):
//!
//! * offer search is `PUT /api/v0/search/asks/` with a `{"q": …}` body;
//! * the create response's instance id is **`new_contract`**, not `id`;
//! * logs are a **two-phase `result_url`** poll with **no Bearer on the result fetch**;
//!   a first-fetch 403/404 is normal and the backoff goes to ~30 s;
//! * vast publishes no rate-limit headers, so 429 handling is exponential backoff with
//!   jitter, capped at 30 s.

use apexrouter_core::error::Result;
use apexrouter_core::secret::Secret;
use apexrouter_protocol::{
    ContainerLaunch, InstanceId, OfferQuery, OfferSearchResult, VastAccount, VastInstance,
};
use async_trait::async_trait;

/// The live client.
pub struct VastApiHttp {
    /* P-02 */
    http: reqwest::Client,
    base: String,
    cred: Secret<String>,
}

/// Everything we ask vast.ai to do.
#[async_trait]
pub trait VastApi: Send + Sync {
    /// `GET /users/current/`. Free, and the one live call in the test suite.
    async fn account(&self) -> Result<VastAccount>;
    /// `PUT /api/v0/search/asks/` with a `{"q": …}` body.
    async fn search(&self, q: &OfferQuery) -> Result<OfferSearchResult>;
    /// `PUT /api/v0/asks/{offer_id}/`. **Reads the id from `new_contract`.**
    async fn create(
        &self,
        offer_id: u64,
        launch: &ContainerLaunch,
        label: &str,
    ) -> Result<InstanceId>;
    /// The whole fleet.
    async fn instances(&self) -> Result<Vec<VastInstance>>;
    /// One instance, or `None` when it is gone.
    async fn instance(&self, id: InstanceId) -> Result<Option<VastInstance>>;
    /// `DELETE`. The caller **verifies before forgetting**.
    async fn destroy(&self, id: InstanceId) -> Result<()>;
    /// `PUT /api/v0/instances/request_logs/{id}/`, then the two-phase `result_url` poll.
    async fn logs(&self, id: InstanceId, tail: u32) -> Result<Vec<String>>;
    /// Run something on the box through vast's exec endpoint.
    async fn exec(&self, id: InstanceId, cmd: &str) -> Result<String>;
}

/// Replays recorded JSON — **this is what lets the money path be tested for free.** No test
/// ever creates an instance.
pub struct FixtureVast {/* P-02 */}
