//! OWNER: unit P-07 (providers/src/hf.rs). Do not edit outside that unit.
//!
//! HuggingFace. Six hand-rolled calls; no `hf-hub` crate, which would drag reqwest 0.13 in.
//!
//! Sizes come from `POST /api/models/{ns}/{repo}/paths-info/{rev}` — the **authoritative**
//! call — not from `siblings`, which often omits them. Gated repos are classified on
//! (status, header-if-present, body) with an **anonymous retry** to distinguish a bad token
//! from genuine gating, and always surface the request-access URL — never "not found".
//!
//! **This closes the discovery→launch dead-end: an HF row can become a local endpoint
//! without leaving the app.**

use apexrouter_core::error::Result;
use apexrouter_protocol::{DownloadProgress, HfFileGroup, HfModel};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

/// The HuggingFace client.
pub struct HfClient {/* P-07 */}

impl HfClient {
    /// `GET /api/models?filter=gguf&search=`, following an RFC 5988 `Link: rel=next`.
    pub async fn search(&self, q: &str, limit: u32) -> Result<Vec<HfModel>> {
        todo!("P-07: HfClient::search")
    }

    /// Files grouped by quant, with authoritative sizes from `paths-info` and shards summed.
    pub async fn files(&self, repo: &str) -> Result<Vec<HfFileGroup>> {
        todo!("P-07: HfClient::files")
    }

    /// Resumable, progress-streaming download into `~/models/<repo-basename>/`, with a size
    /// verification on completion.
    pub async fn download(
        &self,
        repo: &str,
        files: &[String],
        dest: &Path,
        tx: mpsc::Sender<DownloadProgress>,
    ) -> Result<Vec<PathBuf>> {
        todo!("P-07: HfClient::download")
    }
}
