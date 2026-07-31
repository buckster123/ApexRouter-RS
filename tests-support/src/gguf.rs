//! A synthetic GGUF header the real `discover::read_gguf_meta` and the real `fit()` can
//! read.
//!
//! No weights: the file is a header plus `weights_bytes` of zeroes, because `fit()` sizes
//! the weights from `metadata().len()` and reads everything else out of the KV block. A
//! sparse file would be cheaper still, but a test that asserts on a fit needs the bytes to
//! be there when the solver stats them.

use std::path::Path;

/// The model shape to write. Defaults describe a 9B-ish dense llama: 36 layers, 32k train
/// context, GQA 32/8 — close enough to Carnice-9b that a fit against it is recognisable.
#[derive(Clone, Debug)]
pub struct GgufSpec {
    /// `general.architecture`.
    pub arch: String,
    /// `<arch>.block_count`.
    pub n_layer: u32,
    /// `<arch>.context_length`.
    pub n_ctx_train: u32,
    /// `<arch>.embedding_length`.
    pub n_embd: u32,
    /// `<arch>.attention.head_count`.
    pub n_head: u32,
    /// `<arch>.attention.head_count_kv`.
    pub n_head_kv: u32,
    /// `<arch>.expert_count`, for an MoE.
    pub n_expert: Option<u32>,
    /// How many bytes of pretend weights follow the header. This is what `fit()` reads as
    /// the weight size, so it is the knob that decides whether a plan fits.
    pub weights_bytes: usize,
}

impl Default for GgufSpec {
    fn default() -> Self {
        GgufSpec {
            arch: "llama".to_owned(),
            n_layer: 36,
            n_ctx_train: 32_768,
            n_embd: 4096,
            n_head: 32,
            n_head_kv: 8,
            n_expert: None,
            weights_bytes: 4096,
        }
    }
}

impl GgufSpec {
    /// A model of roughly this many mebibytes on disk.
    pub fn sized_mb(mut self, mb: usize) -> GgufSpec {
        self.weights_bytes = mb * 1024 * 1024;
        self
    }

    /// A different layer count.
    pub fn layers(mut self, n: u32) -> GgufSpec {
        self.n_layer = n;
        self
    }

    /// A different training context.
    pub fn ctx_train(mut self, n: u32) -> GgufSpec {
        self.n_ctx_train = n;
        self
    }
}

/// Write a GGUF v3 header at `path`, creating parent directories.
///
/// # Errors
/// Any I/O failure, rendered.
pub fn write(path: &Path, spec: &GgufSpec) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }

    let mut kv: Vec<(String, Kv)> = vec![
        (
            "general.architecture".to_owned(),
            Kv::Str(spec.arch.clone()),
        ),
        (format!("{}.block_count", spec.arch), Kv::U32(spec.n_layer)),
        (
            format!("{}.context_length", spec.arch),
            Kv::U32(spec.n_ctx_train),
        ),
        (
            format!("{}.embedding_length", spec.arch),
            Kv::U32(spec.n_embd),
        ),
        (
            format!("{}.attention.head_count", spec.arch),
            Kv::U32(spec.n_head),
        ),
        (
            format!("{}.attention.head_count_kv", spec.arch),
            Kv::U32(spec.n_head_kv),
        ),
    ];
    if let Some(n) = spec.n_expert {
        kv.push((format!("{}.expert_count", spec.arch), Kv::U32(n)));
    }

    let mut b: Vec<u8> = Vec::with_capacity(512 + spec.weights_bytes);
    b.extend_from_slice(b"GGUF");
    b.extend_from_slice(&3u32.to_le_bytes()); // version
    b.extend_from_slice(&0u64.to_le_bytes()); // tensor count
    b.extend_from_slice(&(kv.len() as u64).to_le_bytes());
    for (key, value) in kv {
        put_str(&mut b, &key);
        match value {
            Kv::U32(v) => {
                b.extend_from_slice(&4u32.to_le_bytes()); // GGUF_TYPE_UINT32
                b.extend_from_slice(&v.to_le_bytes());
            }
            Kv::Str(s) => {
                b.extend_from_slice(&8u32.to_le_bytes()); // GGUF_TYPE_STRING
                put_str(&mut b, &s);
            }
        }
    }
    b.resize(b.len() + spec.weights_bytes, 0);
    std::fs::write(path, b).map_err(|e| format!("{}: {e}", path.display()))
}

/// The two value types this writer needs.
enum Kv {
    U32(u32),
    Str(String),
}

/// GGUF strings are a `u64` length followed by the bytes.
fn put_str(b: &mut Vec<u8>, s: &str) {
    b.extend_from_slice(&(s.len() as u64).to_le_bytes());
    b.extend_from_slice(s.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_starts_with_the_magic_and_the_file_is_the_size_asked_for() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sub").join("Fake-9b-Q6_K.gguf");
        write(&path, &GgufSpec::default().sized_mb(1)).expect("write");
        let raw = std::fs::read(&path).expect("read");
        assert_eq!(&raw[..4], b"GGUF");
        assert!(raw.len() > 1024 * 1024);
    }
}
