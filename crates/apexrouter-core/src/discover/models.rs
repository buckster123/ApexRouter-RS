//! OWNER: unit C-10 (core/discover/models.rs, core/discover/gguf.rs). Do not edit outside
//! that unit.
//!
//! Local weight discovery. Recurses into per-model subdirectories, follows symlinks,
//! honours `ignore_globs`, groups `-00001-of-000NN` shards into **one** `LocalModel` with a
//! summed size, pairs `mmproj-*.gguf` into `LocalModel::mmproj`, and matches `mmproj`/
//! `vocab` as **filename tokens, not path substrings** — a directory named `vocab-x` must
//! not hide its contents.
//!
//! Two deliberate reversals of the LocalRouter behaviour this replaces
//! (`docs/port/03-local-endpoint.md` §2.4):
//!
//! * results are sorted **smallest first**. The old code sorted largest-first on the
//!   theory that bigger is more interesting; on a 24 GB shared-memory box the smallest
//!   thing that fits is the interesting one.
//! * a `PermissionError` costs us one *subdirectory*, not the whole root. The old code
//!   wrapped the entire walk in one `try`, so a single unreadable folder silently emptied
//!   the model list.

use crate::config::EndpointsCfg;
use crate::discover::gguf::read_gguf_meta;
use crate::error::Result;
use apexrouter_protocol::{LocalModel, ModelShard};
use glob::{MatchOptions, Pattern};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// How deep a scan will recurse before assuming something pathological.
const MAX_DEPTH: usize = 24;

/// Directory names that never hold a usable model. `.cache` is where a half-finished
/// `huggingface-cli download` leaves its blobs — `~/models/qwen36-35b-a3b/` on this
/// machine is exactly that, and it is not a model.
const ALWAYS_PRUNED_DIRS: &[&str] = &[".cache"];

/// Walk `model_roots` and return one entry per logical model, **smallest first**.
///
/// A models directory with nothing in it is normal, not an error.
///
/// # Errors
///
/// Only if the blocking scan task cannot be joined. A missing root, an unreadable
/// subdirectory, a file that is not GGUF and a root holding no weights at all are all
/// ordinary outcomes that produce a (possibly empty) list.
pub async fn discover_models(cfg: &EndpointsCfg) -> Result<Vec<LocalModel>> {
    let cfg = cfg.clone();
    match tokio::task::spawn_blocking(move || discover_models_blocking(&cfg)).await {
        Ok(v) => Ok(v),
        Err(e) => Err(crate::error::Error::Other(format!(
            "model discovery task failed: {e}"
        ))),
    }
}

/// The whole scan, synchronously. `discover_models` is this on a blocking thread.
fn discover_models_blocking(cfg: &EndpointsCfg) -> Vec<LocalModel> {
    let ignores = compile_globs(&cfg.ignore_globs);
    let mut files: Vec<GgufFile> = Vec::new();
    let mut seen_dirs: HashSet<PathBuf> = HashSet::new();
    let mut seen_files: HashSet<PathBuf> = HashSet::new();

    for root in expand_roots(&cfg.model_roots) {
        scan_dir(
            &root,
            &ignores,
            0,
            &mut seen_dirs,
            &mut seen_files,
            &mut files,
        );
    }

    group(files)
}

/// One `.gguf` on disk, before grouping.
struct GgufFile {
    /// Canonical path.
    path: PathBuf,
    /// Directory holding it.
    dir: PathBuf,
    /// Filename with the `.gguf` extension removed, original case.
    stem: String,
    /// Size in bytes.
    bytes: u64,
}

// ---------------------------------------------------------------------------------------
// roots and ignores
// ---------------------------------------------------------------------------------------

/// Expand `~`, then any glob metacharacters, into concrete directories.
///
/// Glob roots are supported because the useful layout on this machine is a symlink farm:
/// `~/Projects/Inference/stacks/*/models` is one root, not seven.
fn expand_roots(roots: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for raw in roots {
        let expanded = expand_tilde(raw);
        let as_str = expanded.to_string_lossy().into_owned();
        if as_str.contains('*') || as_str.contains('?') || as_str.contains('[') {
            match glob::glob(&as_str) {
                Ok(paths) => out.extend(paths.filter_map(std::result::Result::ok)),
                Err(e) => {
                    tracing::warn!(root = %as_str, error = %e, "bad model_roots glob, skipped")
                }
            }
        } else {
            out.push(expanded);
        }
    }
    out
}

/// `~` and `~/…` against `$HOME`. Any other form is returned unchanged.
fn expand_tilde(s: &str) -> PathBuf {
    if s == "~" {
        if let Some(h) = dirs::home_dir() {
            return h;
        }
    } else if let Some(rest) = s.strip_prefix("~/") {
        if let Some(h) = dirs::home_dir() {
            return h.join(rest);
        }
    }
    PathBuf::from(s)
}

/// Compile `ignore_globs`. A pattern that will not parse is warned about and dropped —
/// one typo in the config must not blind the whole scan.
fn compile_globs(globs: &[String]) -> Vec<Pattern> {
    globs
        .iter()
        .filter_map(|g| match Pattern::new(&expand_tilde(g).to_string_lossy()) {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::warn!(glob = %g, error = %e, "bad ignore_globs pattern, skipped");
                None
            }
        })
        .collect()
}

/// Does any ignore glob match this path? `**` is a whole path component, so
/// `**/.cache/**` cannot be fooled by a filename that merely contains `.cache`.
fn ignored(path: &Path, ignores: &[Pattern]) -> bool {
    let opts = MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: false,
    };
    ignores.iter().any(|p| p.matches_path_with(path, opts))
}

// ---------------------------------------------------------------------------------------
// the walk
// ---------------------------------------------------------------------------------------

/// Recurse one directory, following symlinks, collecting `.gguf` files.
///
/// Loops are broken by canonicalising every directory before descending and refusing to
/// visit one twice; `resources/models/*` here is a symlink farm by design, so simply not
/// following links is not an option.
fn scan_dir(
    dir: &Path,
    ignores: &[Pattern],
    depth: usize,
    seen_dirs: &mut HashSet<PathBuf>,
    seen_files: &mut HashSet<PathBuf>,
    out: &mut Vec<GgufFile>,
) {
    if depth > MAX_DEPTH {
        tracing::debug!(dir = %dir.display(), "model scan hit the depth limit");
        return;
    }
    let Ok(real) = dir.canonicalize() else {
        return; // missing root or dangling link: normal, not an error
    };
    if !seen_dirs.insert(real.clone()) {
        return; // already walked, via a symlink or a second root
    }
    let entries = match std::fs::read_dir(&real) {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!(dir = %real.display(), error = %e, "unreadable directory, skipped");
            return;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if ignored(&path, ignores) {
            continue;
        }
        // `metadata()` follows symlinks; `file_type()` on the entry does not.
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if ALWAYS_PRUNED_DIRS.contains(&name.as_str()) {
                continue;
            }
            scan_dir(&path, ignores, depth + 1, seen_dirs, seen_files, out);
        } else if meta.is_file() && is_gguf(&path) {
            let Ok(canon) = path.canonicalize() else {
                continue;
            };
            if !seen_files.insert(canon.clone()) {
                continue; // the same file reached twice through different links
            }
            let Some(stem) = canon.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
                continue;
            };
            let dir = canon.parent().map(Path::to_path_buf).unwrap_or_default();
            out.push(GgufFile {
                path: canon,
                dir,
                stem,
                bytes: meta.len(),
            });
        }
    }
}

/// Case-insensitive `.gguf` extension test.
fn is_gguf(path: &Path) -> bool {
    path.extension()
        .map(|e| e.eq_ignore_ascii_case("gguf"))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------------------
// filename classification
// ---------------------------------------------------------------------------------------

/// Split a filename stem into lowercase tokens on `-`, `_`, `.` and space.
///
/// Tokens, never substrings: this is the difference between skipping
/// `ggml-vocab-llama.gguf` and hiding everything under a directory called
/// `vocab-experiments/`.
fn tokens(stem: &str) -> Vec<String> {
    stem.split(['-', '_', '.', ' '])
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Is this file a vision projector rather than a model?
fn is_projector(stem: &str) -> bool {
    tokens(stem).iter().any(|t| t == "mmproj")
}

/// Is this file a bare vocabulary dump rather than a model?
fn is_vocab(stem: &str) -> bool {
    tokens(stem).iter().any(|t| t == "vocab")
}

/// Split `<base>-00001-of-00003` into `("<base>", 1)`. `None` when it is not a shard.
///
/// The suffix is fixed-width by the llama.cpp convention `-%05d-of-%05d`. A stem that is
/// *only* the suffix has no base to group on and is left alone as its own model.
fn shard_of(stem: &str) -> Option<(&str, u32)> {
    const TAIL: usize = 15; // "-00001-of-00003"
    if stem.len() <= TAIL || !stem.is_char_boundary(stem.len() - TAIL) {
        return None;
    }
    let (base, tail) = stem.split_at(stem.len() - TAIL);
    let b = tail.as_bytes();
    if b[0] != b'-' || &b[6..10] != b"-of-" {
        return None;
    }
    if !b[1..6].iter().all(u8::is_ascii_digit) || !b[10..15].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let index: u32 = tail.get(1..6)?.parse().ok()?;
    Some((base, index))
}

/// The quantisation token in a filename, leftmost match, alternatives tried in order.
///
/// The published pattern is `(UD-Q\d+[^.\s_-]*|Q\d+_K_[A-Z]+|Q\d+_\d+)`. Three deliberate
/// widenings, each of which only ever turns a non-match or a truncated match into the
/// whole token:
///
/// * bare `Q\d+_K`, because the only real model on this machine is `Carnice-9b-Q6_K.gguf`
///   and the published alternation matches nothing in it;
/// * `IQ\d+_[A-Z0-9]+`, for the `IQ4_XS`-style quants in wide circulation;
/// * the `UD-` class keeps underscores. `[^.\s_-]*` stops at the first `_`, so
///   `…-UD-Q4_K_XL.gguf` yields `UD-Q4` — a prefix of the real token, and a worse handle
///   for it.
fn quant_token(stem: &str) -> Option<String> {
    (0..stem.len()).find_map(|i| quant_at(stem, i))
}

/// Try every alternative at one offset.
fn quant_at(stem: &str, i: usize) -> Option<String> {
    let rest = stem.as_bytes().get(i..)?;

    // UD-Q\d+…  — the Unsloth dynamic quants, e.g. UD-Q4_K_XL.
    if rest.starts_with(b"UD-Q") {
        let mut j = 4;
        while rest.get(j).is_some_and(u8::is_ascii_digit) {
            j += 1;
        }
        if j > 4 {
            while rest
                .get(j)
                .is_some_and(|c| !matches!(c, b'.' | b'-' | b' ' | b'\t' | b'\n' | b'\r'))
            {
                j += 1;
            }
            return stem.get(i..i + j).map(str::to_owned);
        }
    }

    // Q\d+_K[_A-Z]*  |  Q\d+_\d+  |  IQ\d+_[A-Z0-9]+
    let digits_at = if rest.starts_with(b"IQ") {
        2usize
    } else if rest.starts_with(b"Q") {
        1usize
    } else {
        return None;
    };
    let mut j = digits_at;
    while rest.get(j).is_some_and(u8::is_ascii_digit) {
        j += 1;
    }
    if j == digits_at || rest.get(j) != Some(&b'_') {
        return None;
    }
    j += 1;
    let start_of_suffix = j;
    while rest
        .get(j)
        .is_some_and(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    {
        j += 1;
        // `Q4_K_M`, `Q4_K_XL`: one more `_`-separated uppercase run is part of the token.
        if rest.get(j) == Some(&b'_') && rest.get(j + 1).is_some_and(u8::is_ascii_uppercase) {
            j += 1;
        }
    }
    if j == start_of_suffix {
        return None;
    }
    stem.get(i..i + j).map(str::to_owned)
}

// ---------------------------------------------------------------------------------------
// grouping
// ---------------------------------------------------------------------------------------

/// Collapse the flat file list into logical models: shards summed, projectors paired,
/// headers read, smallest first, ids assigned last so they follow the final order.
fn group(files: Vec<GgufFile>) -> Vec<LocalModel> {
    let mut projectors: HashMap<PathBuf, Vec<(String, ModelShard)>> = HashMap::new();
    // BTreeMap so the shard order inside a group is the filename order.
    let mut groups: BTreeMap<(PathBuf, String), Vec<(u32, GgufFile)>> = BTreeMap::new();

    for f in files {
        if is_vocab(&f.stem) {
            continue;
        }
        if is_projector(&f.stem) {
            projectors.entry(f.dir.clone()).or_default().push((
                f.stem.clone(),
                ModelShard {
                    path: f.path.display().to_string(),
                    bytes: f.bytes,
                },
            ));
            continue;
        }
        let (base, index) = match shard_of(&f.stem) {
            Some((base, i)) => (base.to_owned(), i),
            None => (f.stem.clone(), 0),
        };
        groups
            .entry((f.dir.clone(), base))
            .or_default()
            .push((index, f));
    }

    let now = chrono::Utc::now().timestamp();
    let mut models: Vec<LocalModel> = Vec::with_capacity(groups.len());

    for ((dir, base), mut parts) in groups {
        parts.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.path.cmp(&b.1.path)));
        let total_bytes = parts.iter().map(|(_, f)| f.bytes).sum();
        let shards: Vec<ModelShard> = parts
            .iter()
            .map(|(_, f)| ModelShard {
                path: f.path.display().to_string(),
                bytes: f.bytes,
            })
            .collect();
        let gguf = shards
            .first()
            .and_then(|s| read_gguf_meta(Path::new(&s.path)).ok());
        let mmproj = projectors
            .get(&dir)
            .map(|ps| pair_projectors(&base, ps))
            .unwrap_or_default();

        models.push(LocalModel {
            id: String::new(), // assigned below, after sorting
            name: base.clone(),
            dir: dir.display().to_string(),
            shards,
            total_bytes,
            mmproj,
            quant: quant_token(&base),
            gguf,
            discovered_at_unix: now,
        });
    }

    // Smallest first: on a memory-constrained box the smallest thing that fits is the
    // interesting one. Name and directory break ties so the order is reproducible.
    models.sort_by(|a, b| {
        a.total_bytes
            .cmp(&b.total_bytes)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.dir.cmp(&b.dir))
    });

    let mut used: HashSet<String> = HashSet::new();
    for m in &mut models {
        m.id = unique_slug(&m.dir, &m.name, &mut used);
    }
    models
}

/// Which projectors in this directory belong to this model, best match first.
///
/// The prefix is what precedes the `mmproj` token: `Bonsai-27B-dspark-mmproj-f16` pairs
/// with `Bonsai-27B-dspark-Q4_1` and not with `Bonsai-27B-Q2_0`, while a bare
/// `mmproj-f16.gguf` has an empty prefix and pairs with everything in the directory.
fn pair_projectors(model_stem: &str, projectors: &[(String, ModelShard)]) -> Vec<ModelShard> {
    let model = model_stem.to_lowercase();
    let mut matches: Vec<(usize, &ModelShard)> = projectors
        .iter()
        .filter_map(|(stem, shard)| {
            let prefix = projector_prefix(stem);
            model.starts_with(&prefix).then_some((prefix.len(), shard))
        })
        .collect();
    // Longest prefix first: `mmproj.first()` is the one `--mmproj` should be given.
    matches.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.path.cmp(&b.1.path)));
    matches.into_iter().map(|(_, s)| s.clone()).collect()
}

/// The lowercase model-name prefix a projector filename claims, `""` when it claims none.
fn projector_prefix(stem: &str) -> String {
    let lower = stem.to_lowercase();
    let mut cut = lower.len();
    let mut at = 0usize;
    while let Some(found) = lower.get(at..).and_then(|s| s.find("mmproj")) {
        let idx = at + found;
        let before_ok =
            idx == 0 || matches!(lower.as_bytes().get(idx - 1), Some(b'-' | b'_' | b'.'));
        let after = idx + "mmproj".len();
        let after_ok =
            after == lower.len() || matches!(lower.as_bytes().get(after), Some(b'-' | b'_' | b'.'));
        if before_ok && after_ok {
            cut = idx;
            break;
        }
        at = idx + 1;
    }
    lower
        .get(..cut)
        .unwrap_or("")
        .trim_end_matches(['-', '_', '.'])
        .to_owned()
}

/// A stable, unique slug from the directory and the base filename.
fn unique_slug(dir: &str, name: &str, used: &mut HashSet<String>) -> String {
    let dir_part = Path::new(dir)
        .file_name()
        .map(|n| slugify(&n.to_string_lossy()))
        .unwrap_or_default();
    let name_part = slugify(name);
    let base = if dir_part.is_empty() || name_part.contains(&dir_part) {
        name_part
    } else {
        format!("{dir_part}-{name_part}")
    };
    let base = if base.is_empty() {
        "model".to_owned()
    } else {
        base
    };
    if used.insert(base.clone()) {
        return base;
    }
    for n in 2u32.. {
        let candidate = format!("{base}-{n}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    base
}

/// Lowercase, `[a-z0-9]` kept, everything else collapsed to a single `-`.
fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn cfg_for(roots: &[&Path]) -> EndpointsCfg {
        EndpointsCfg {
            model_roots: roots.iter().map(|p| p.display().to_string()).collect(),
            ..EndpointsCfg::default()
        }
    }

    /// A file of `bytes` length, cheaply (sparse, no 7 GB of zeroes actually written).
    fn touch(path: &Path, bytes: u64) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        let f = fs::File::create(path).expect("create");
        f.set_len(bytes).expect("set_len");
    }

    // ---- filename classification -----------------------------------------------------

    #[test]
    fn mmproj_and_vocab_are_filename_tokens_not_substrings() {
        assert!(is_projector("mmproj-F16"));
        assert!(is_projector("Ternary-Bonsai-27B-mmproj-f16"));
        assert!(!is_projector("my-mmprojector-model-Q4_K_M"));
        assert!(is_vocab("ggml-vocab-llama"));
        assert!(!is_vocab("vocabulary-tuned-9b-Q4_K_M"));
    }

    #[test]
    fn shards_are_recognised_by_their_fixed_width_suffix() {
        assert_eq!(
            shard_of("Qwen3-235B-Q4_K_M-00002-of-00005"),
            Some(("Qwen3-235B-Q4_K_M", 2))
        );
        assert_eq!(shard_of("Carnice-9b-Q6_K"), None);
        assert_eq!(shard_of("model-1-of-3"), None, "the width is fixed at five");
        assert_eq!(
            shard_of("-00001-of-00002"),
            None,
            "no base name left to group on"
        );
    }

    #[test]
    fn the_quant_token_is_pulled_out_of_the_filename() {
        assert_eq!(quant_token("Carnice-9b-Q6_K").as_deref(), Some("Q6_K"));
        assert_eq!(quant_token("Qwen3.5-9B-Q4_K_M").as_deref(), Some("Q4_K_M"));
        assert_eq!(
            quant_token("Qwen3-30B-A3B-UD-Q4_K_XL").as_deref(),
            Some("UD-Q4_K_XL")
        );
        assert_eq!(
            quant_token("Ternary-Bonsai-27B-Q2_0").as_deref(),
            Some("Q2_0")
        );
        assert_eq!(quant_token("phi-4-IQ4_XS").as_deref(), Some("IQ4_XS"));
        assert_eq!(quant_token("some-model-f16"), None);
    }

    #[test]
    fn slugs_are_stable_and_unique() {
        let mut used = HashSet::new();
        assert_eq!(
            unique_slug("/home/a/models/carnice-9b", "Carnice-9b-Q6_K", &mut used),
            "carnice-9b-q6-k"
        );
        assert_eq!(
            unique_slug("/home/a/models/other", "Carnice-9b-Q6_K", &mut used),
            "other-carnice-9b-q6-k"
        );
        assert_eq!(
            unique_slug("/home/a/models/other", "Carnice-9b-Q6_K", &mut used),
            "other-carnice-9b-q6-k-2",
            "a repeat gets a suffix rather than colliding"
        );
    }

    // ---- the scan --------------------------------------------------------------------

    #[tokio::test]
    async fn an_empty_or_missing_models_dir_is_not_an_error() {
        let d = tempfile::tempdir().expect("tempdir");
        let empty = d.path().join("models");
        fs::create_dir_all(&empty).expect("mkdir");
        let missing = d.path().join("nowhere");

        let got = discover_models(&cfg_for(&[&empty, &missing]))
            .await
            .expect("an empty root is normal");
        assert!(got.is_empty());
    }

    #[tokio::test]
    async fn models_live_in_per_model_subdirectories_and_dot_cache_is_ignored() {
        let d = tempfile::tempdir().expect("tempdir");
        let root = d.path().join("models");
        touch(&root.join("carnice-9b/Carnice-9b-Q6_K.gguf"), 7_359_259_424);
        touch(
            &root.join("carnice-9b/.cache/huggingface/download/blob.gguf"),
            4_096,
        );
        touch(&root.join("qwen36-35b-a3b/.cache/huggingface/x.gguf"), 512);
        touch(&root.join("results/bench.json"), 10);

        let got = discover_models(&cfg_for(&[&root])).await.expect("scan");
        assert_eq!(got.len(), 1, "got {got:#?}");
        assert_eq!(got[0].name, "Carnice-9b-Q6_K");
        assert_eq!(got[0].total_bytes, 7_359_259_424);
        assert_eq!(got[0].quant.as_deref(), Some("Q6_K"));
        assert!(got[0].primary_path().unwrap_or("").ends_with("Q6_K.gguf"));
        assert!(!got[0].is_vision());
        assert!(
            got[0].gguf.is_none(),
            "an empty file has no readable header"
        );
    }

    #[tokio::test]
    async fn shards_group_into_one_model_with_a_summed_size() {
        let d = tempfile::tempdir().expect("tempdir");
        let root = d.path().join("models");
        let big = root.join("qwen3-235b");
        touch(&big.join("Qwen3-235B-Q4_K_M-00001-of-00003.gguf"), 1_000);
        touch(&big.join("Qwen3-235B-Q4_K_M-00002-of-00003.gguf"), 2_000);
        touch(&big.join("Qwen3-235B-Q4_K_M-00003-of-00003.gguf"), 3_000);

        let got = discover_models(&cfg_for(&[&root])).await.expect("scan");
        assert_eq!(got.len(), 1, "three shards are one model: {got:#?}");
        let m = &got[0];
        assert_eq!(m.name, "Qwen3-235B-Q4_K_M");
        assert_eq!(m.shards.len(), 3);
        assert_eq!(m.total_bytes, 6_000);
        assert!(
            m.primary_path()
                .unwrap_or("")
                .ends_with("00001-of-00003.gguf"),
            "-m gets the first shard: {:?}",
            m.primary_path()
        );
        assert_eq!(m.quant.as_deref(), Some("Q4_K_M"));
    }

    #[tokio::test]
    async fn projectors_pair_with_their_own_model_and_are_not_models_themselves() {
        let d = tempfile::tempdir().expect("tempdir");
        let root = d.path().join("models");
        let dir = root.join("ternary-bonsai-27b");
        touch(&dir.join("Ternary-Bonsai-27B-Q2_0.gguf"), 7_200);
        touch(&dir.join("Ternary-Bonsai-27B-dspark-Q4_1.gguf"), 1_900);
        touch(&dir.join("Ternary-Bonsai-27B-mmproj-f16.gguf"), 100);
        touch(&dir.join("Ternary-Bonsai-27B-dspark-mmproj-f16.gguf"), 90);
        touch(&dir.join("ggml-vocab-bonsai.gguf"), 5);

        let got = discover_models(&cfg_for(&[&root])).await.expect("scan");
        assert_eq!(
            got.len(),
            2,
            "projectors and vocab are not models: {got:#?}"
        );

        let dspark = got
            .iter()
            .find(|m| m.name.contains("dspark"))
            .expect("dspark model");
        assert!(dspark.is_vision());
        assert!(
            dspark.mmproj[0].path.contains("dspark-mmproj"),
            "the specific projector wins: {:?}",
            dspark.mmproj
        );

        let plain = got
            .iter()
            .find(|m| m.name == "Ternary-Bonsai-27B-Q2_0")
            .expect("plain model");
        assert_eq!(plain.mmproj.len(), 1);
        assert!(plain.mmproj[0].path.contains("27B-mmproj"));
    }

    #[tokio::test]
    async fn a_bare_projector_pairs_with_every_model_in_its_directory() {
        let d = tempfile::tempdir().expect("tempdir");
        let root = d.path().join("models");
        let dir = root.join("vision");
        touch(&dir.join("gemma-3-12b-Q4_K_M.gguf"), 8_000);
        touch(&dir.join("mmproj-F16.gguf"), 800);

        let got = discover_models(&cfg_for(&[&root])).await.expect("scan");
        assert_eq!(got.len(), 1);
        assert!(got[0].is_vision());
    }

    #[tokio::test]
    async fn a_directory_named_vocab_x_does_not_hide_its_contents() {
        let d = tempfile::tempdir().expect("tempdir");
        let root = d.path().join("models");
        touch(&root.join("vocab-experiments/tuned-9b-Q4_K_M.gguf"), 4_000);
        touch(&root.join("mmproj-tests/plain-3b-Q8_0.gguf"), 3_000);

        let got = discover_models(&cfg_for(&[&root])).await.expect("scan");
        let names: Vec<&str> = got.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["plain-3b-Q8_0", "tuned-9b-Q4_K_M"], "{got:#?}");
    }

    #[tokio::test]
    async fn results_are_sorted_smallest_first() {
        let d = tempfile::tempdir().expect("tempdir");
        let root = d.path().join("models");
        touch(&root.join("a/big-Q8_0.gguf"), 9_000);
        touch(&root.join("b/small-Q4_K_M.gguf"), 1_000);
        touch(&root.join("c/middle-Q5_K_M.gguf"), 5_000);

        let got = discover_models(&cfg_for(&[&root])).await.expect("scan");
        let sizes: Vec<u64> = got.iter().map(|m| m.total_bytes).collect();
        assert_eq!(sizes, vec![1_000, 5_000, 9_000]);
        assert!(got.iter().all(|m| !m.id.is_empty()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn directory_symlinks_are_followed_and_loops_do_not_hang() {
        let d = tempfile::tempdir().expect("tempdir");
        let root = d.path().join("models");
        let real = d.path().join("elsewhere/ternary-gguf/27B");
        touch(&real.join("Ternary-Bonsai-27B-Q2_0.gguf"), 7_200);
        fs::create_dir_all(&root).expect("mkdir");
        std::os::unix::fs::symlink(&real, root.join("ternary-bonsai-27b")).expect("symlink");
        // …and a loop back to the root, which must not recurse forever.
        std::os::unix::fs::symlink(&root, real.join("loop")).expect("symlink");

        let got = discover_models(&cfg_for(&[&root])).await.expect("scan");
        assert_eq!(got.len(), 1, "{got:#?}");
        assert_eq!(got[0].name, "Ternary-Bonsai-27B-Q2_0");
    }

    #[tokio::test]
    async fn the_same_file_reached_twice_is_reported_once() {
        let d = tempfile::tempdir().expect("tempdir");
        let root = d.path().join("models");
        touch(&root.join("m/model-Q4_K_M.gguf"), 1_234);

        // The same root twice, plus its own parent.
        let got = discover_models(&cfg_for(&[&root, &root, d.path()]))
            .await
            .expect("scan");
        assert_eq!(got.len(), 1, "{got:#?}");
    }

    #[tokio::test]
    async fn ignore_globs_are_honoured() {
        let d = tempfile::tempdir().expect("tempdir");
        let root = d.path().join("models");
        touch(&root.join("keep/model-Q4_K_M.gguf"), 1_000);
        touch(&root.join("drafts/wip-Q4_K_M.gguf"), 2_000);

        let mut cfg = cfg_for(&[&root]);
        cfg.ignore_globs = vec![format!("{}/drafts/**", root.display())];
        let got = discover_models(&cfg).await.expect("scan");
        assert_eq!(got.len(), 1, "{got:#?}");
        assert_eq!(got[0].name, "model-Q4_K_M");
    }

    #[tokio::test]
    async fn glob_roots_expand() {
        let d = tempfile::tempdir().expect("tempdir");
        let stacks = d.path().join("stacks");
        touch(&stacks.join("one/models/a-Q4_K_M.gguf"), 1_000);
        touch(&stacks.join("two/models/b-Q4_K_M.gguf"), 2_000);

        let root = format!("{}/*/models", stacks.display());
        let cfg = EndpointsCfg {
            model_roots: vec![root],
            ..EndpointsCfg::default()
        };
        let got = discover_models(&cfg).await.expect("scan");
        assert_eq!(got.len(), 2, "{got:#?}");
    }

    #[tokio::test]
    async fn the_gguf_header_is_read_into_the_model() {
        // A minimal but real GGUF v3 header, so discovery populates `gguf`.
        let mut body: Vec<u8> = Vec::new();
        let mut kv_str = |k: &str, v: &str| {
            body.extend_from_slice(&(k.len() as u64).to_le_bytes());
            body.extend_from_slice(k.as_bytes());
            body.extend_from_slice(&8u32.to_le_bytes());
            body.extend_from_slice(&(v.len() as u64).to_le_bytes());
            body.extend_from_slice(v.as_bytes());
        };
        kv_str("general.architecture", "llama");
        let mut kv_u32 = |k: &str, v: u32| {
            body.extend_from_slice(&(k.len() as u64).to_le_bytes());
            body.extend_from_slice(k.as_bytes());
            body.extend_from_slice(&4u32.to_le_bytes());
            body.extend_from_slice(&v.to_le_bytes());
        };
        kv_u32("llama.block_count", 28);
        kv_u32("llama.context_length", 8192);
        kv_u32("llama.attention.head_count_kv", 8);
        kv_u32("llama.attention.key_length", 128);
        kv_u32("llama.attention.value_length", 128);

        let mut file = Vec::new();
        file.extend_from_slice(b"GGUF");
        file.extend_from_slice(&3u32.to_le_bytes());
        file.extend_from_slice(&1u64.to_le_bytes());
        file.extend_from_slice(&6u64.to_le_bytes());
        file.extend_from_slice(&body);

        let d = tempfile::tempdir().expect("tempdir");
        let root = d.path().join("models");
        let path = root.join("tiny/tiny-Q4_K_M.gguf");
        fs::create_dir_all(path.parent().unwrap_or(&root)).expect("mkdir");
        fs::write(&path, &file).expect("write");

        let got = discover_models(&cfg_for(&[&root])).await.expect("scan");
        assert_eq!(got.len(), 1);
        let meta = got[0].gguf.as_ref().expect("header read");
        assert_eq!(meta.arch, "llama");
        assert_eq!(meta.n_layer, 28);
        assert_eq!(meta.n_ctx_train, 8192);
        assert_eq!(meta.n_head_kv, 8);
    }

    /// The real machine (`docs/port/00-machine-ground-truth.md`): `~/models` holds exactly
    /// one complete GGUF, inside a per-model subdirectory, next to a `.cache` full of an
    /// abandoned download. Skipped elsewhere.
    #[tokio::test]
    async fn the_real_models_dir_yields_carnice() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let root = home.join("models");
        if !root.join("carnice-9b/Carnice-9b-Q6_K.gguf").is_file() {
            return;
        }
        let got = discover_models(&cfg_for(&[&root])).await.expect("scan");
        let carnice = got
            .iter()
            .find(|m| m.name == "Carnice-9b-Q6_K")
            .unwrap_or_else(|| panic!("Carnice not found in {got:#?}"));
        assert_eq!(carnice.total_bytes, 7_359_259_424);
        assert_eq!(carnice.quant.as_deref(), Some("Q6_K"));
        assert_eq!(carnice.shards.len(), 1);
        assert!(!carnice.is_vision());
        let meta = carnice.gguf.as_ref().expect("real header");
        assert_eq!(meta.arch, "qwen35");
        assert_eq!(meta.n_layer, 32);
        assert!(
            got.iter().all(|m| !m.dir.contains("/.cache")),
            "a .cache directory is never a model source: {got:#?}"
        );
    }
}
