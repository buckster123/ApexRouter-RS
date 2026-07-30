//! HuggingFace search and download types.
//!
//! Sizes come from `paths-info`, which is authoritative, never from `siblings` (which often
//! omits them). This is what closes the discovery→launch dead-end: an HF row can become a
//! local endpoint without leaving the app.

use crate::ids::JobId;
use serde::{Deserialize, Serialize};

/// One repository from `GET /api/models?filter=gguf&search=`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HfModel {
    /// `"unsloth/Qwen3-Coder-30B-A3B-Instruct-GGUF"`.
    pub id: String,
    /// Namespace, when the id has one.
    #[serde(default)]
    pub author: Option<String>,
    /// Download count.
    #[serde(default)]
    pub downloads: Option<u64>,
    /// Like count.
    #[serde(default)]
    pub likes: Option<u64>,
    /// Gated repos need access granted; the UI must show the request-access URL rather than
    /// claiming "not found".
    pub gated: bool,
    /// ISO timestamp as HF sends it.
    #[serde(default)]
    pub last_modified: Option<String>,
    /// Repo tags.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// One file in a repo, with its authoritative size.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HfFile {
    /// Path within the repo.
    pub rfilename: String,
    /// Bytes, from `paths-info`.
    #[serde(default)]
    pub size: Option<u64>,
    /// Matched by `(UD-Q\d+[^.\s_-]*|Q\d+_K_[A-Z]+|Q\d+_\d+)`.
    #[serde(default)]
    pub quant: Option<String>,
    /// Matched as a filename **token**, not a path substring — a directory named `vocab-x`
    /// must not hide its contents.
    pub is_mmproj: bool,
    /// `(index, total)` parsed from `-00001-of-000NN`.
    #[serde(default)]
    pub shard_of: Option<(u32, u32)>,
}

/// Files grouped into one downloadable unit: a quant plus its shards plus its projector.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HfFileGroup {
    /// What to show in the list.
    pub label: String,
    /// The quantisation, when one was detected.
    #[serde(default)]
    pub quant: Option<String>,
    /// Sum of every file's size — the number the fit solver and the disk check use.
    pub total_bytes: u64,
    /// The weight shards, in order.
    #[serde(default)]
    pub files: Vec<HfFile>,
    /// Vision projectors that pair with this group.
    #[serde(default)]
    pub mmproj: Vec<HfFile>,
}

/// Streamed download progress.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DownloadProgress {
    /// The job this belongs to.
    pub job: JobId,
    /// Repo id.
    pub repo: String,
    /// The file currently transferring.
    pub file: String,
    /// Bytes done for this file.
    pub bytes_done: u64,
    /// Total bytes, when known.
    #[serde(default)]
    pub bytes_total: Option<u64>,
    /// Observed rate.
    pub mbps: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hf_types_round_trip() {
        let m = HfModel {
            id: "unsloth/Qwen3-Coder-30B-A3B-Instruct-GGUF".into(),
            author: Some("unsloth".into()),
            downloads: Some(120_000),
            likes: Some(312),
            gated: false,
            last_modified: Some("2026-06-01T12:00:00.000Z".into()),
            tags: vec!["gguf".into(), "text-generation".into()],
        };
        let s = serde_json::to_string(&m).expect("ser");
        assert_eq!(serde_json::from_str::<HfModel>(&s).expect("de"), m);

        let g = HfFileGroup {
            label: "UD-Q4_K_XL (2 shards)".into(),
            quant: Some("UD-Q4_K_XL".into()),
            total_bytes: 18_000_000_000,
            files: vec![HfFile {
                rfilename: "UD-Q4_K_XL/model-00001-of-00002.gguf".into(),
                size: Some(9_000_000_000),
                quant: Some("UD-Q4_K_XL".into()),
                is_mmproj: false,
                shard_of: Some((1, 2)),
            }],
            mmproj: vec![HfFile {
                rfilename: "mmproj-F16.gguf".into(),
                size: Some(600_000_000),
                quant: None,
                is_mmproj: true,
                shard_of: None,
            }],
        };
        let s = serde_json::to_string(&g).expect("ser");
        assert_eq!(serde_json::from_str::<HfFileGroup>(&s).expect("de"), g);

        let p = DownloadProgress {
            job: JobId::new(),
            repo: "unsloth/Qwen3-Coder-30B-A3B-Instruct-GGUF".into(),
            file: "UD-Q4_K_XL/model-00001-of-00002.gguf".into(),
            bytes_done: 1_000_000,
            bytes_total: Some(9_000_000_000),
            mbps: 88.5,
        };
        let s = serde_json::to_string(&p).expect("ser");
        assert_eq!(serde_json::from_str::<DownloadProgress>(&s).expect("de"), p);
    }

    #[test]
    fn shard_tuple_is_a_json_array() {
        let f = HfFile {
            rfilename: "m-00001-of-00003.gguf".into(),
            size: None,
            quant: None,
            is_mmproj: false,
            shard_of: Some((1, 3)),
        };
        let s = serde_json::to_string(&f).expect("ser");
        assert!(s.contains("[1,3]"), "{s}");
        assert_eq!(serde_json::from_str::<HfFile>(&s).expect("de"), f);
    }
}
