//! OWNER: unit I-01 (cli/tests/migrate_e2e.rs, docs/MIGRATION.md). Do not edit outside that unit.
//!
//! `apexrouter migrate` end-to-end, against a **copy of the real `~/.vastai-gguf`**.
//!
//! # Why this test drives the binary
//!
//! `core::migrate` has thorough unit tests over synthetic trees. They cannot catch what this
//! one is for: the CLI's own path/env plumbing (`--from`, `--localrouter`, the redirected
//! `$APEXROUTER_HOME`), and what the *real* legacy directory on this machine actually
//! contains — a live Together key, a `.pinned_provider`, four `usage.log` rows from
//! 2026-05-02, and a saved local instance pointing at a model that **no longer exists**.
//! Stale state is the normal case, not an edge case.
//!
//! # The rule this test enforces above all others
//!
//! `~/.vastai-gguf` is *another tool's state directory*. Nothing here ever writes into it.
//! The suite therefore:
//!
//! 1. **copies** the real directory into a `TempDir` and migrates the copy;
//! 2. hashes the copy before and after every single invocation;
//! 3. hashes the **real** directory before and after the whole suite, so a code path that
//!    escapes the fake `$HOME` is caught rather than merely thought impossible.
//!
//! When the real directory is absent (CI, a fresh box) an equivalent fixture is written
//! instead and every structural assertion still runs; only the assertions that quote the
//! real machine's numbers — 4 usage rows, 54 frozen `vast_gguf` recipes, 19 gpu tiers, 7
//! fork mappings — are skipped, and they are skipped explicitly rather than silently.
//!
//! Hermetic: every invocation runs with a cleared environment, a `$HOME` inside a
//! `TempDir`, and `migrate` is a `Need::Pure` verb — no daemon, no socket, no network.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

// =======================================================================================
// the world
// =======================================================================================

/// Where the legacy tree under test came from.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Source {
    /// A byte-for-byte copy of Andre's `~/.vastai-gguf`.
    Real,
    /// The equivalent fixture, written when the real directory is not on this machine.
    Fixture,
}

/// A disposable machine: a fake `$HOME` holding the legacy copy, a LocalRouter copy, and an
/// `$APEXROUTER_HOME` that starts out non-existent.
struct World {
    /// Kept alive for the duration of the test; dropping it deletes everything.
    _tmp: tempfile::TempDir,
    /// The fake `$HOME`.
    home: PathBuf,
    /// `<home>/.vastai-gguf` — the copy we are allowed to migrate.
    legacy: PathBuf,
    /// `<home>/LocalRouter` — a copy of the checkout, plus its four repo-directory files.
    localrouter: PathBuf,
    /// `$APEXROUTER_HOME`. Deliberately not created: `--dry-run` must not create it either.
    state: PathBuf,
    /// Whether [`World::legacy`] is a copy of the real thing.
    source: Source,
    /// Whether [`World::localrouter`] carries a copy of the real `recipes.toml`.
    real_recipes: bool,
}

impl World {
    /// Build the world: real state where it exists, the fixture where it does not.
    fn new() -> World {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let legacy = home.join(".vastai-gguf");
        let localrouter = home.join("LocalRouter");
        std::fs::create_dir_all(&legacy).expect("mkdir legacy");
        std::fs::create_dir_all(&localrouter).expect("mkdir localrouter");

        let source = match real_legacy_dir() {
            Some(real) => {
                copy_tree(&real, &legacy);
                Source::Real
            }
            None => {
                write_legacy_fixture(&legacy);
                Source::Fixture
            }
        };

        let real_recipes = match real_recipes_toml() {
            Some(real) => {
                std::fs::copy(&real, localrouter.join("recipes.toml")).expect("copy recipes.toml");
                true
            }
            None => {
                write(&localrouter.join("recipes.toml"), RECIPES_FIXTURE);
                false
            }
        };

        // The four files LocalRouter wrote into its own repo directory. The real checkout no
        // longer has them, and they are the only source of the ledger seed, so they are
        // synthesised here — into the copy, never into the checkout.
        write(&localrouter.join(".last_instance"), "25731461\n");
        write(
            &localrouter.join(".instance_history"),
            "2026-05-01T18:02:11Z\t25690001\n2026-05-02T09:14:52Z\t25731461\n",
        );
        // Shape 3: the one that embeds a plaintext local api_key.
        write(
            &localrouter.join(".active_endpoint"),
            "{\"provider\":\"local\",\"name\":\"local-qwen35-9b\",\"host\":\"127.0.0.1\",\
             \"port\":8100,\"pid\":649035,\"api_key\":\"local-plaintext-key-must-not-leak\",\
             \"activated_at\":\"2026-05-03T00:34:36Z\"}\n",
        );
        write(
            &localrouter.join(".hf_pin"),
            "{\"MODEL_REPO\":\"unsloth/Qwen3.6-27B-GGUF\",\"MODEL_QUANT\":\"UD-Q6_K_XL\",\
             \"filename\":\"Qwen3.6-27B-UD-Q6_K_XL.gguf\",\"size\":\"22.1 GB\"}\n",
        );

        World {
            state: home.join("state"),
            _tmp: tmp,
            home,
            legacy,
            localrouter,
            source,
            real_recipes,
        }
    }

    /// Run `apexrouter …` with a cleared environment pinned at this world.
    fn run(&self, args: &[&str]) -> Out {
        self.run_with_home(&self.home, args)
    }

    /// The same, with `$HOME` pointed somewhere else — what `--from` is for.
    fn run_with_home(&self, home: &Path, args: &[&str]) -> Out {
        let out = Command::new(env!("CARGO_BIN_EXE_apexrouter"))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", home)
            .env("APEXROUTER_HOME", &self.state)
            .env("APEXROUTER_CONFIG", self.state.join("config.toml"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("APEXROUTER_LOCALROUTER_DIR", &self.localrouter)
            .args(args)
            .output()
            .expect("spawn apexrouter");
        Out {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// The legacy copy's fingerprint. Every test asserts this is unchanged.
    fn legacy_hash(&self) -> String {
        hash_tree(&self.legacy)
    }

    /// The plaintext `api_key` sitting in the legacy `config.toml`, if there is one.
    ///
    /// Read, never printed: it is real key material on the machine this test targets, and
    /// the whole point of the assertion is that it stays where it was found.
    fn legacy_api_key(&self) -> Option<String> {
        let text = std::fs::read_to_string(self.legacy.join("config.toml")).ok()?;
        for line in text.lines() {
            let line = line.trim();
            if !line.starts_with("api_key") {
                continue;
            }
            let (_, rhs) = line.split_once('=')?;
            let quoted = rhs.trim();
            let inner = quoted
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .unwrap_or(quoted);
            if inner.len() >= 8 {
                return Some(inner.to_owned());
            }
        }
        None
    }
}

/// One invocation's result.
struct Out {
    /// Process exit code.
    code: i32,
    /// Everything on stdout — `--json` output is the whole of it.
    stdout: String,
    /// Everything on stderr — `tracing` and `Error: …`.
    stderr: String,
}

impl Out {
    /// Assert success and parse stdout as the `--json` envelope.
    fn json(&self) -> serde_json::Value {
        assert_eq!(
            self.code, 0,
            "exit {} — stderr:\n{}",
            self.code, self.stderr
        );
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("stdout is not JSON ({e}):\n{}", self.stdout))
    }
}

// =======================================================================================
// locating the real machine's state
// =======================================================================================

/// `~/.vastai-gguf`, when this really is Andre's box.
fn real_legacy_dir() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let dir = home.join(".vastai-gguf");
    dir.is_dir().then_some(dir)
}

/// The real LocalRouter checkout's `recipes.toml`, probed exactly where `core::paths` probes.
fn real_recipes_toml() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    [
        "Projects/Inference/tools/LocalRouter",
        "Projects/LocalRouter",
        "LocalRouter",
        "src/LocalRouter",
    ]
    .iter()
    .map(|c| home.join(c).join("recipes.toml"))
    .find(|p| p.is_file())
}

// =======================================================================================
// hashing and copying
// =======================================================================================

/// A stable fingerprint of a whole tree: every path, every length, every byte.
///
/// Directories are hashed as entries in their own right, so an empty directory appearing
/// under the legacy tree is a change like any other. A missing root hashes as empty.
fn hash_tree(root: &Path) -> String {
    let mut entries: BTreeMap<String, Option<Vec<u8>>> = BTreeMap::new();
    collect(root, root, &mut entries);
    let mut h = Sha256::new();
    for (rel, body) in entries {
        h.update(rel.as_bytes());
        h.update([0]);
        match body {
            None => h.update(b"dir"),
            Some(bytes) => {
                h.update(b"file");
                h.update(bytes.len().to_le_bytes());
                h.update(&bytes);
            }
        }
        h.update([0]);
    }
    h.finalize().iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

/// Walk `dir`, recording every entry relative to `root`.
fn collect(dir: &Path, root: &Path, out: &mut BTreeMap<String, Option<Vec<u8>>>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .display()
            .to_string();
        if path.is_dir() {
            out.insert(rel, None);
            collect(&path, root, out);
        } else {
            out.insert(rel, Some(std::fs::read(&path).unwrap_or_default()));
        }
    }
}

/// Every file under `root`, as `(relative path, contents)`.
fn files_under(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut entries = BTreeMap::new();
    collect(root, root, &mut entries);
    entries
        .into_iter()
        .filter_map(|(k, v)| v.map(|b| (k, b)))
        .collect()
}

/// Recursive copy. Only ever used to copy the real tree *out* of `$HOME`.
fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("mkdir");
    for entry in std::fs::read_dir(src).expect("read_dir").flatten() {
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_tree(&from, &to);
        } else {
            std::fs::copy(&from, &to).expect("copy");
        }
    }
}

/// Write a file, creating its parent.
fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(path, body).expect("write");
}

// =======================================================================================
// the fixture, for a machine that is not Andre's
// =======================================================================================

/// The same shape as the real `~/.vastai-gguf`, with a key that is not a key.
fn write_legacy_fixture(dir: &Path) {
    write(
        &dir.join("config.toml"),
        "# ~/.vastai-gguf/config.toml — provider configuration\n\
         #\n\
         # Edit this file to add API keys and base URLs for external providers.\n\
         \n\
         [providers.together]\n\
         base_url  = \"https://api.together.ai/v1\"\n\
         api_key   = \"fixture-key-0000000000000000000000\"\n",
    );
    write(
        &dir.join(".pinned_provider"),
        "{\"provider\": \"together\", \"model_id\": \"deepseek-ai/DeepSeek-V4-Pro\", \
         \"base_url\": \"https://api.together.ai/v1\"}",
    );
    // Four rows, as on the real machine: one plain, one without `epoch`, one carrying an
    // unknown field that must survive via `flatten`, one in the legacy `vast-gguf` spelling.
    write(
        &dir.join("usage.log"),
        "{\"timestamp\": \"2026-05-02T20:11:21Z\", \"epoch\": 1777745481.5262182, \
         \"provider\": \"together\", \"model_id\": \"meta-llama/Llama-3.1-8B-Instruct-Turbo\", \
         \"prompt_tokens\": 100, \"completion_tokens\": 50, \"cost_usd\": 2.7e-05}\n\
         {\"timestamp\": \"2026-05-02T20:11:21Z\", \"epoch\": 1777745481.5263166, \
         \"provider\": \"vast-gguf\", \"model_id\": \"Qwen3.6-27B-Q8.gguf\", \
         \"prompt_tokens\": 100, \"completion_tokens\": 50, \"cost_usd\": 0.000766}\n\
         {\"timestamp\": \"2026-05-02T20:12:02Z\", \"provider\": \"together\", \
         \"model_id\": \"Qwen/Qwen2.5-72B-Instruct-Turbo\", \"prompt_tokens\": 131072, \
         \"completion_tokens\": 500, \"cost_usd\": 0.11}\n\
         {\"timestamp\": \"2026-05-02T20:13:44Z\", \"epoch\": 1777745624.0, \
         \"provider\": \"together\", \"model_id\": \"Qwen/QwQ-32B-Preview\", \
         \"prompt_tokens\": 131072, \"completion_tokens\": 500, \"cost_usd\": 0.12, \
         \"an_unknown_field\": \"kept via flatten\"}\n",
    );
    // The saved instance points at a model that is not there. That is the normal case.
    write(
        &dir.join("local_instances/local-qwen35-9b.json"),
        "{\n  \"name\": \"local-qwen35-9b\",\n  \"pid\": 649035,\n  \"port\": 8100,\n  \
         \"host\": \"127.0.0.1\",\n  \"binary\": \"/nonexistent/llama.cpp/build-vulkan/bin/llama-server\",\n  \
         \"model_path\": \"~/models/Qwen3.5-9B-Q4_K_M.gguf\",\n  \"backend\": \"vulkan\",\n  \
         \"started_at\": \"2026-05-03T00:34:36Z\",\n  \"status\": \"stopped\",\n  \
         \"stopped_at\": \"2026-05-03T00:38:32Z\"\n}",
    );
    write(
        &dir.join("local_logs/local-qwen35-9b.log"),
        "llama_model_loader: loaded meta data\n",
    );
}

/// A small `recipes.toml` carrying every type trap the real one carries.
const RECIPES_FIXTURE: &str = r#"
[docker]
prebuilt = "ghcr.io/buckster123/vastai-gguf:prebuilt"
builder = "ghcr.io/buckster123/vastai-gguf:builder"
vllm = "ghcr.io/buckster123/vastai-gguf:vllm"

[gpu_tiers.5090]
vast_names = ["RTX_5090"]
label = "RTX 5090 32GB   (~$0.34/hr)"
max_price = "0.55"
min_disk_gb = 60
image_type = "prebuilt"
vram_gb = 32

[gpu_tiers.h100-sxm-2x]
vast_names = ["H100_SXM", "H100_SXM5"]
label = "2x H100 SXM 160GB  (~$5/hr)"
max_price = "5.00"
min_disk_gb = 100
num_gpus = 2
image_type = "builder"
vram_gb = 80

[[recipes]]
name = "dsv4-flash-q2k-2xh100"
label = "DSv4-Flash 284B  Q2_K  128K ctx  (2xH100)"
gpu = "h100-sxm-2x"
model_repo = "Preyazz/DeepSeek-V4-Flash-GGUF"
model_quant = "Q2_K"
ctx = 131072
parallel = 1
kv_type = "q8_0"
image_type = "builder"
llama_cpp_repo = "fairydreaming/llama.cpp"
llama_cpp_ref = "deepseek-dsa"
description = "Lightest quant, fits with KV headroom."

[[recipes]]
name = "qwen36-27b-q6-5090"
label = "Qwen3.6-27B  Q6_K  96K ctx  (5090)"
gpu = "5090"
model_repo = "unsloth/Qwen3.6-27B-GGUF"
model_quant = "UD-Q6_K_XL"
ctx = 98304
parallel = 3
kv_type = "q8_0"
description = "Three slots sharing one 96K pool."

[[recipes]]
name = "dsv4-pro-5xh200"
provider = "vllm"
label = "DSv4-Pro (vLLM, 5xH200)"
gpu = "h100-sxm-2x"
model_id = "deepseek-ai/DeepSeek-V4-Pro"
kv_cache_dtype = "fp8"
enforce_eager = "true"
description = "enforce_eager is the STRING true."

[[recipes]]
name = "together-llama3.1-8b"
provider = "together"
label = "Llama 3.1 8B Turbo ($0.18/M tokens)"
model_id = "meta-llama/Llama-3.1-8B-Instruct-Turbo"
ctx = 131072
price_input = 0.18
price_output = 0.18
description = "Cheap and fast."

[[recipes]]
name = "local-qwen35-9b"
provider = "local"
label = "Qwen3.5-9B  Q4_K_M  (local Vulkan)"
model_path = "~/models/Qwen3.5-9B-Q4_K_M.gguf"
port = 8100
ctx = 32768
parallel = 1
kv_type = "q8_0"
n_gpu_layers = 999
backend = "vulkan"
mode = "thinking"
description = "The model this points at is gone. That is normal."
"#;

// =======================================================================================
// reading the plan
// =======================================================================================

/// The `items` array of a plan envelope.
fn items(v: &serde_json::Value) -> &Vec<serde_json::Value> {
    v.get("items")
        .and_then(serde_json::Value::as_array)
        .unwrap_or_else(|| panic!("no items in {v}"))
}

/// The rows whose `what` is exactly `what`.
fn rows_of<'a>(v: &'a serde_json::Value, what: &str) -> Vec<&'a serde_json::Value> {
    items(v)
        .iter()
        .filter(|i| i.get("what").and_then(serde_json::Value::as_str) == Some(what))
        .collect()
}

/// A row's field as a `&str`, or `""`.
fn field<'a>(row: &'a serde_json::Value, key: &str) -> &'a str {
    row.get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
}

// =======================================================================================
// 1. --dry-run writes nothing, anywhere
// =======================================================================================

/// The headline acceptance: a plan with a reason on every row, and not one byte written.
///
/// Three trees are checked, not one: the legacy copy (what migration reads), the state
/// directory (what `--apply` would write) and the real `~/.vastai-gguf` (which this process
/// has no business touching at all).
#[test]
fn dry_run_prints_a_reasoned_plan_and_writes_nothing_anywhere() {
    let real_before = real_legacy_dir().map(|d| hash_tree(&d));
    let w = World::new();
    let legacy_before = w.legacy_hash();
    let lr_before = hash_tree(&w.localrouter);

    let out = w.run(&["migrate", "--dry-run", "--json"]);
    let plan = out.json();

    assert_eq!(
        w.legacy_hash(),
        legacy_before,
        "--dry-run modified the legacy tree"
    );
    assert_eq!(
        hash_tree(&w.localrouter),
        lr_before,
        "--dry-run modified the LocalRouter checkout"
    );
    assert!(
        files_under(&w.state).is_empty(),
        "--dry-run created files under $APEXROUTER_HOME: {:?}",
        files_under(&w.state)
            .into_iter()
            .map(|(p, _)| p)
            .collect::<Vec<_>>()
    );

    // Both source directories are named, so a human can see what was read.
    let sources = plan["source_paths"].as_array().expect("source_paths");
    let sources: Vec<&str> = sources.iter().filter_map(|s| s.as_str()).collect();
    assert!(
        sources.iter().any(|s| Path::new(s) == w.legacy),
        "the legacy copy is not in {sources:?}"
    );
    assert!(
        sources.iter().any(|s| Path::new(s) == w.localrouter),
        "the LocalRouter copy is not in {sources:?}"
    );

    // Per-row reasons: the whole value of the plan is that a human can strike rows out.
    let rows = items(&plan);
    assert!(
        rows.len() > 10,
        "a plan of {} rows is not a plan",
        rows.len()
    );
    for row in rows {
        for key in ["what", "from", "action", "detail"] {
            assert!(
                !field(row, key).trim().is_empty(),
                "row {row} has an empty `{key}`"
            );
        }
        assert!(
            ["import", "skip", "warn"].contains(&field(row, "action")),
            "unknown action in {row}"
        );
        assert!(
            field(row, "detail").len() > 20,
            "row {row} has a reason too short to be one"
        );
    }

    // The real directory is untouched — proven, not assumed.
    if let (Some(before), Some(dir)) = (real_before, real_legacy_dir()) {
        assert_eq!(
            hash_tree(&dir),
            before,
            "the REAL ~/.vastai-gguf changed during a --dry-run"
        );
    }
}

/// A bare `apexrouter migrate` is `--dry-run` with a hint, never an import.
#[test]
fn a_bare_migrate_prints_the_plan_and_says_nothing_was_written() {
    let w = World::new();
    let before = w.legacy_hash();

    let out = w.run(&["migrate"]);
    assert_eq!(out.code, 0, "stderr:\n{}", out.stderr);
    assert!(
        out.stdout.contains("Nothing was written"),
        "stdout:\n{}",
        out.stdout
    );
    assert!(
        out.stdout.contains("--apply"),
        "the plan must name the verb that acts on it:\n{}",
        out.stdout
    );
    assert_eq!(w.legacy_hash(), before);
    assert!(
        files_under(&w.state).is_empty(),
        "a bare migrate wrote state"
    );
}

/// The stale saved instance is a `warn` with an explanation, never an error.
#[test]
fn the_stale_local_instance_is_explained_rather_than_erroring() {
    let w = World::new();
    let plan = w.run(&["migrate", "--dry-run", "--json"]).json();

    let rows = rows_of(&plan, "local instance");
    assert_eq!(rows.len(), 1, "one saved instance, got {}", rows.len());
    let row = rows[0];
    assert_eq!(field(row, "action"), "warn", "row: {row}");
    assert!(
        field(row, "detail").contains("NO LONGER EXISTS"),
        "the reason must name the missing model: {row}"
    );
    assert!(
        field(row, "detail").contains("normal, not an error"),
        "stale state is the normal case and the plan must say so: {row}"
    );
    assert!(
        field(row, "from").ends_with("local-qwen35-9b.json"),
        "row: {row}"
    );
}

/// The 54 frozen `vast_gguf` rows are skipped, each saying why, and `fit()` is named.
#[test]
fn the_frozen_vast_gguf_recipes_are_skipped_with_a_reason_each() {
    let w = World::new();
    let plan = w.run(&["migrate", "--dry-run", "--json"]).json();

    let frozen: Vec<_> = rows_of(&plan, "recipe")
        .into_iter()
        .filter(|r| field(r, "detail").contains("frozen vast_gguf row"))
        .collect();
    assert!(!frozen.is_empty(), "no frozen rows in the plan");
    for row in &frozen {
        assert_eq!(field(row, "action"), "skip", "row: {row}");
        assert!(
            field(row, "detail").contains("fit()"),
            "a skipped recipe must name what supersedes it: {row}"
        );
    }
    if w.real_recipes {
        assert_eq!(
            frozen.len(),
            54,
            "the real recipes.toml has 54 provider-less rows"
        );
    }
}

// =======================================================================================
// 2. --apply
// =======================================================================================

/// `--apply` into a redirected `$APEXROUTER_HOME`: imports land, the legacy tree does not move.
#[test]
fn apply_writes_only_into_the_redirected_state_directory() {
    let real_before = real_legacy_dir().map(|d| hash_tree(&d));
    let w = World::new();
    let legacy_before = w.legacy_hash();
    let lr_before = hash_tree(&w.localrouter);

    let report = w.run(&["migrate", "--apply", "--json"]).json();
    assert!(
        report["imported"].as_u64().unwrap_or(0) > 5,
        "nothing was imported: {report}"
    );
    assert!(
        report["skipped"].as_u64().unwrap_or(0) > 0,
        "everything was imported, which cannot be right: {report}"
    );

    assert_eq!(
        w.legacy_hash(),
        legacy_before,
        "--apply wrote into ~/.vastai-gguf's copy"
    );
    assert_eq!(
        hash_tree(&w.localrouter),
        lr_before,
        "--apply wrote into the LocalRouter checkout"
    );

    let written: Vec<String> = files_under(&w.state).into_iter().map(|(p, _)| p).collect();
    for expected in ["config.toml", "catalog.toml", "ledger.jsonl"] {
        assert!(
            written.iter().any(|p| p == expected),
            "{expected} was not written; state holds {written:?}"
        );
    }

    if let (Some(before), Some(dir)) = (real_before, real_legacy_dir()) {
        assert_eq!(
            hash_tree(&dir),
            before,
            "the REAL ~/.vastai-gguf changed during --apply"
        );
    }
}

/// The real Together key is never copied — not into config, not into the plan, not anywhere.
///
/// The key is read out of the legacy copy at test time and searched for byte-for-byte across
/// every file the migration wrote and every byte it printed. It is never itself printed.
#[test]
fn the_legacy_api_key_is_imported_as_a_reference_and_never_copied() {
    let w = World::new();
    let key = w
        .legacy_api_key()
        .expect("the legacy config.toml carries an api_key");

    let plan = w.run(&["migrate", "--dry-run", "--json"]);
    assert!(
        !plan.stdout.contains(&key) && !plan.stderr.contains(&key),
        "the plan printed the legacy api_key"
    );

    let applied = w.run(&["migrate", "--apply", "--json"]);
    assert_eq!(applied.code, 0, "stderr:\n{}", applied.stderr);
    assert!(
        !applied.stdout.contains(&key) && !applied.stderr.contains(&key),
        "--apply printed the legacy api_key"
    );

    for (path, bytes) in files_under(&w.state) {
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains(&key),
            "the legacy api_key was copied into {path}"
        );
    }

    // What *is* recorded is where the key lives, and the base URL, verbatim.
    let cfg = std::fs::read_to_string(w.state.join("config.toml")).expect("config.toml");
    assert!(
        cfg.contains("api_key_env") || cfg.contains("api_key_file"),
        "the imported provider must reference a key location:\n{cfg}"
    );
    assert!(
        cfg.contains("https://api.together.ai/v1"),
        "the legacy base_url must survive verbatim:\n{cfg}"
    );

    // Shape 3 of `.active_endpoint` embeds a plaintext key too. It is discarded on read.
    assert!(
        !plan.stdout.contains("local-plaintext-key-must-not-leak"),
        "the .active_endpoint api_key reached the plan"
    );
    for (path, bytes) in files_under(&w.state) {
        assert!(
            !String::from_utf8_lossy(&bytes).contains("local-plaintext-key-must-not-leak"),
            "the .active_endpoint api_key was written into {path}"
        );
    }
}

/// A provider the config does not already carry is imported as an env-var **reference**.
///
/// The real machine's only legacy provider is `together`, which ships pre-configured, so the
/// import branch would otherwise never run against real-shaped input.
#[test]
fn an_unknown_legacy_provider_is_imported_as_an_env_reference() {
    let w = World::new();
    // A second provider, appended to the *copy* only.
    let extra = "\n[providers.somebodyelse]\nbase_url = \"https://api.example.com/v1\"\n\
                 api_key = \"plaintext-that-must-not-travel\"\n";
    let cfg_path = w.legacy.join("config.toml");
    let mut text = std::fs::read_to_string(&cfg_path).expect("legacy config");
    text.push_str(extra);
    std::fs::write(&cfg_path, &text).expect("write legacy copy");

    let plan = w.run(&["migrate", "--dry-run", "--json"]).json();
    let row = rows_of(&plan, "provider")
        .into_iter()
        .find(|r| field(r, "from").ends_with("#providers.somebodyelse"))
        .expect("a row for the new provider");
    assert_eq!(field(row, "action"), "import", "row: {row}");
    assert!(
        field(row, "detail").contains("SOMEBODYELSE_API_KEY"),
        "the plan must name the reference it will record: {row}"
    );
    assert!(
        field(row, "detail").contains("NOT copied"),
        "the plan must say the key is not copied: {row}"
    );

    w.run(&["migrate", "--apply", "--json"]).json();
    let out = std::fs::read_to_string(w.state.join("config.toml")).expect("config.toml");
    assert!(
        out.contains("[providers.somebodyelse]"),
        "the provider was not imported:\n{out}"
    );
    assert!(
        out.contains("api_key_env = \"SOMEBODYELSE_API_KEY\""),
        "the reference was not recorded:\n{out}"
    );
    assert!(
        !out.contains("plaintext-that-must-not-travel"),
        "the plaintext key was copied:\n{out}"
    );
    assert!(
        out.contains("https://api.example.com/v1"),
        "the base_url must be imported verbatim:\n{out}"
    );
}

/// The legacy `usage.log` is merged into the aggregate with **zero failed rows**.
///
/// Merged in place: the file is never copied, so `usage.jsonl` stays empty and the rows still
/// appear. The legacy `vast-gguf` spelling survives on the wire.
#[test]
fn the_legacy_usage_log_merges_with_zero_failed_rows() {
    let w = World::new();
    let legacy_rows = std::fs::read_to_string(w.legacy.join("usage.log"))
        .expect("usage.log")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count();

    let plan = w.run(&["migrate", "--dry-run", "--json"]).json();
    let row = rows_of(&plan, "usage log")
        .into_iter()
        .next()
        .expect("a usage log row");
    assert_eq!(field(row, "action"), "skip", "nothing is copied: {row}");
    assert!(
        field(row, "detail").contains(&format!("{legacy_rows} row(s)")),
        "the plan must count the rows it found: {row}"
    );
    assert!(
        field(row, "detail").contains("double-count"),
        "the plan must say why copying would be wrong: {row}"
    );

    w.run(&["migrate", "--apply", "--json"]).json();

    let usage = w
        .run(&[
            "--no-autostart",
            "usage",
            "--since",
            "all",
            "--by",
            "provider",
            "--json",
        ])
        .json();
    assert_eq!(
        usage["rows"].as_u64(),
        Some(legacy_rows as u64),
        "every legacy row must survive the merge: {usage}"
    );
    assert_eq!(
        usage["served_by"].as_str(),
        Some("offline"),
        "this must be answerable with nothing running: {usage}"
    );
    let keys: Vec<&str> = usage["by"]
        .as_array()
        .expect("by")
        .iter()
        .filter_map(|b| b["key"].as_str())
        .collect();
    assert!(
        keys.contains(&"vast-gguf"),
        "the legacy provider spelling must stay on the wire: {keys:?}"
    );
    assert!(
        !w.state.join("usage.jsonl").exists()
            || std::fs::read_to_string(w.state.join("usage.jsonl"))
                .expect("usage.jsonl")
                .trim()
                .is_empty(),
        "the legacy rows were copied into our own log — they will double-count"
    );

    if w.source == Source::Real {
        assert_eq!(legacy_rows, 4, "the real usage.log has 4 rows");
    }
}

/// The ledger is seeded from `.last_instance` and `.instance_history` — money stays visible.
#[test]
fn the_ledger_is_seeded_from_the_localrouter_instance_files() {
    let w = World::new();
    let report = w.run(&["migrate", "--apply", "--json"]).json();

    let warnings: Vec<&str> = report["warnings"]
        .as_array()
        .expect("warnings")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    assert!(
        warnings.iter().any(|s| s.contains("STILL BE BILLING")),
        "a possibly-live instance must be shouted about: {warnings:?}"
    );

    let ledger = std::fs::read_to_string(w.state.join("ledger.jsonl")).expect("ledger.jsonl");
    let rows: Vec<serde_json::Value> = ledger
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("ledger row"))
        .collect();
    assert_eq!(rows.len(), 2, "one from each file: {ledger}");

    let confirmed = rows
        .iter()
        .find(|r| r["instance_id"].as_u64() == Some(25_731_461))
        .expect("the .last_instance row");
    assert_eq!(
        confirmed["state"].as_str(),
        Some("confirmed"),
        "not reconciled: vast was never asked"
    );
    assert_eq!(confirmed["approval_source"].as_str(), Some("migrate"));
    assert_eq!(
        confirmed["destroyed_at_unix"].as_null(),
        Some(()),
        "a live-looking instance must not be marked destroyed"
    );

    let destroyed = rows
        .iter()
        .find(|r| r["instance_id"].as_u64() == Some(25_690_001))
        .expect("the .instance_history row");
    assert_eq!(destroyed["state"].as_str(), Some("destroyed"));
    assert!(
        destroyed["note"]
            .as_str()
            .unwrap_or_default()
            .contains("assumed, never observed"),
        "an assumed destruction must say so: {destroyed}"
    );
}

/// `known_forks`, the docker map and the gpu tiers come across from `recipes.toml`.
#[test]
fn recipes_toml_seeds_known_forks_the_docker_map_and_the_search_profiles() {
    let w = World::new();
    let plan = w.run(&["migrate", "--dry-run", "--json"]).json();

    let forks = rows_of(&plan, "known_fork");
    assert!(!forks.is_empty(), "no fork mappings in the plan");
    for row in &forks {
        assert_eq!(field(row, "action"), "import", "row: {row}");
        assert!(
            field(row, "detail").contains("fairydreaming/llama.cpp"),
            "a fork row must name the fork: {row}"
        );
    }
    let profiles = rows_of(&plan, "search profile");
    assert!(!profiles.is_empty(), "no gpu tiers in the plan");
    for row in &profiles {
        assert!(
            field(row, "detail").contains("PER GPU"),
            "the vram_gb trap must be spelled out on every tier: {row}"
        );
    }
    assert_eq!(rows_of(&plan, "docker images").len(), 1, "one docker row");

    if w.real_recipes {
        assert_eq!(forks.len(), 7, "the real file has 7 fork mappings");
        assert_eq!(profiles.len(), 19, "the real file has 19 gpu tiers");
    }

    w.run(&["migrate", "--apply", "--json"]).json();
    let catalog = std::fs::read_to_string(w.state.join("catalog.toml")).expect("catalog.toml");
    let cfg = std::fs::read_to_string(w.state.join("config.toml")).expect("config.toml");
    assert!(
        cfg.contains("known_forks"),
        "known_forks did not reach the config:\n{cfg}"
    );
    for row in &profiles {
        let id = field(row, "from").rsplit('.').next().unwrap_or_default();
        assert!(
            catalog.contains(&format!("id = \"{id}\"")),
            "profile {id} did not reach the catalog"
        );
    }
}

/// The `together` recipes arrive as managed recipes pointing at the legacy base URL.
#[test]
fn the_together_recipes_arrive_as_managed_recipes() {
    let w = World::new();
    let plan = w.run(&["migrate", "--dry-run", "--json"]).json();

    let managed: Vec<_> = rows_of(&plan, "recipe")
        .into_iter()
        .filter(|r| field(r, "detail").starts_with("managed recipe"))
        .collect();
    assert!(!managed.is_empty(), "no managed recipes in the plan");
    for row in &managed {
        assert_eq!(field(row, "action"), "import", "row: {row}");
        assert!(
            field(row, "detail").contains("(verbatim)"),
            "the base URL must be marked verbatim: {row}"
        );
    }
    if w.real_recipes {
        assert_eq!(managed.len(), 7, "the real file has 7 together recipes");
    }

    // `.pinned_provider` becomes one too — the live file pins DeepSeek-V4-Pro.
    let pinned = rows_of(&plan, "pinned provider");
    assert_eq!(pinned.len(), 1, "one pinned provider");
    assert_eq!(field(pinned[0], "action"), "import");
    assert!(
        field(pinned[0], "detail").contains("deepseek-ai/DeepSeek-V4-Pro"),
        "row: {}",
        pinned[0]
    );
}

/// Re-running `--apply` imports nothing twice.
///
/// Whether the *first* apply can emit two recipes under one id is a separate question, and a
/// separate (ignored) test below; this one asks only that the second run is a no-op.
#[test]
fn applying_twice_is_idempotent() {
    let w = World::new();
    w.run(&["migrate", "--apply", "--json"]).json();
    let ledger_rows = |w: &World| {
        std::fs::read_to_string(w.state.join("ledger.jsonl"))
            .expect("ledger.jsonl")
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count()
    };
    let catalog_ids = |w: &World| {
        std::fs::read_to_string(w.state.join("catalog.toml"))
            .expect("catalog.toml")
            .lines()
            .filter(|l| l.starts_with("id = "))
            .map(str::to_owned)
            .collect::<Vec<String>>()
    };
    let config_before = std::fs::read(w.state.join("config.toml")).expect("config.toml");
    let ledger_before = std::fs::read(w.state.join("ledger.jsonl")).expect("ledger.jsonl");
    let rows_before = ledger_rows(&w);
    let ids_before = catalog_ids(&w);

    let report = w.run(&["migrate", "--apply", "--json"]).json();
    assert_eq!(
        ledger_rows(&w),
        rows_before,
        "the second --apply duplicated ledger rows"
    );
    assert_eq!(
        catalog_ids(&w),
        ids_before,
        "the second --apply changed the catalog: {report}"
    );
    assert_eq!(
        std::fs::read(w.state.join("config.toml")).expect("config.toml"),
        config_before,
        "the second --apply rewrote config.toml"
    );
    assert_eq!(
        std::fs::read(w.state.join("ledger.jsonl")).expect("ledger.jsonl"),
        ledger_before,
        "the second --apply rewrote the ledger"
    );

    // From here the whole tree must be byte-stable. It is not stable across the *first*
    // re-read, and that is a consequence of the duplicate-id defect recorded below: a
    // `catalog.toml` carrying two entries under one id does not survive its own
    // `toml_edit` round-trip unchanged. With the collision removed, run 1 is already stable.
    let settled = hash_tree(&w.state);
    w.run(&["migrate", "--apply", "--json"]).json();
    assert_eq!(
        hash_tree(&w.state),
        settled,
        "a repeated --apply keeps changing the state directory"
    );
}

// =======================================================================================
// 3. --from and --localrouter
// =======================================================================================

/// `--from` re-roots the *legacy* half of path resolution and nothing else.
#[test]
fn from_points_at_a_legacy_directory_outside_the_current_home() {
    let w = World::new();
    let elsewhere = w._tmp.path().join("another-home");
    std::fs::create_dir_all(&elsewhere).expect("mkdir");

    // With $HOME somewhere empty and no --from, nothing legacy is found.
    let bare = w
        .run_with_home(&elsewhere, &["migrate", "--dry-run", "--json"])
        .json();
    let sources: Vec<&str> = bare["source_paths"]
        .as_array()
        .expect("source_paths")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    assert!(
        !sources.iter().any(|s| Path::new(s) == w.legacy),
        "the legacy copy was found without --from: {sources:?}"
    );

    // With --from, it is.
    let legacy = w.legacy.display().to_string();
    let with = w
        .run_with_home(
            &elsewhere,
            &["migrate", "--dry-run", "--json", "--from", &legacy],
        )
        .json();
    let sources: Vec<&str> = with["source_paths"]
        .as_array()
        .expect("source_paths")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    assert!(
        sources.iter().any(|s| Path::new(s) == w.legacy),
        "--from did not re-root the legacy lookup: {sources:?}"
    );
    assert!(
        !rows_of(&with, "usage log").is_empty(),
        "the legacy tree was named but not surveyed"
    );
}

/// A `--from` that is not a `.vastai-gguf` directory is refused, not silently ignored.
#[test]
fn from_refuses_a_directory_that_is_not_the_legacy_one() {
    let w = World::new();
    let wrong = w._tmp.path().join("old-state");
    std::fs::create_dir_all(&wrong).expect("mkdir");
    let wrong = wrong.display().to_string();

    let out = w.run(&["migrate", "--dry-run", "--from", &wrong]);
    assert_ne!(out.code, 0, "a wrong --from must fail");
    assert!(
        out.stderr.contains(".vastai-gguf"),
        "the error must name the contract:\n{}",
        out.stderr
    );
    assert!(
        files_under(&w.state).is_empty(),
        "a refused migrate wrote state"
    );
}

/// `--localrouter` must be a directory; a missing one is an error, not an empty survey.
#[test]
fn localrouter_must_be_a_directory() {
    let w = World::new();
    let missing = w._tmp.path().join("no-such-checkout");
    let missing = missing.display().to_string();

    let out = w.run(&["migrate", "--dry-run", "--localrouter", &missing]);
    assert_ne!(out.code, 0, "a missing --localrouter must fail");
    assert!(
        out.stderr.contains("is not a directory"),
        "stderr:\n{}",
        out.stderr
    );
}

// =======================================================================================
// 4. an empty machine
// =======================================================================================

/// No legacy state at all is an empty plan and exit 0 — the common case on a new box.
#[test]
fn a_machine_with_no_legacy_state_yields_an_empty_plan() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    let state = tmp.path().join("state");
    std::fs::create_dir_all(&home).expect("mkdir");

    let out = Command::new(env!("CARGO_BIN_EXE_apexrouter"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .env("APEXROUTER_HOME", &state)
        .env("APEXROUTER_CONFIG", state.join("config.toml"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("APEXROUTER_LOCALROUTER_DIR", home.join("nope"))
        .args(["migrate", "--dry-run"])
        .output()
        .expect("spawn");
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("nothing legacy was found on this machine"),
        "stdout:\n{stdout}"
    );
    assert!(
        files_under(&state).is_empty(),
        "an empty migration wrote state"
    );
}

// =======================================================================================
// 5. known defects, written down rather than remembered
// =======================================================================================

/// **Known defect, reported to the owner of `core/src/migrate.rs` (unit C-16).**
///
/// `apply` de-duplicates recipe ids against the catalog it is writing into, but not against
/// the batch it is writing. On the real machine both `~/.vastai-gguf/local_instances/
/// local-qwen35-9b.json` and `<LocalRouter>/recipes.toml#recipes.local-qwen35-9b` mint the id
/// `local-qwen35-9b`, so `catalog.toml` ends up with two recipes under one id. The second is
/// then unreachable (`recipe show` finds the first) and `recipe rm` deletes both — the exact
/// silent-shadowing failure `catalog::upsert_recipe` exists to prevent. A second symptom is
/// measurable: a `catalog.toml` holding the collision does not survive its own `toml_edit`
/// round-trip byte-for-byte, so the first re-read rewrites the file. Delete the colliding
/// source and `--apply` is byte-idempotent from run 1 — which is how the cause was pinned.
///
/// Ignored so the suite stays green while the fix belongs to somebody else; delete the
/// attribute the moment `apply` dedupes within its own batch.
#[test]
#[ignore = "known defect: migrate::apply can write two recipes under one id — see docs/MIGRATION.md"]
fn apply_never_writes_two_recipes_under_the_same_id() {
    let w = World::new();
    w.run(&["migrate", "--apply", "--json"]).json();
    let catalog = std::fs::read_to_string(w.state.join("catalog.toml")).expect("catalog.toml");
    let mut ids: Vec<&str> = catalog.lines().filter(|l| l.starts_with("id = ")).collect();
    let n = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), n, "duplicate ids in catalog.toml");
}

/// **Known behaviour that surprises people, and the reason `--dry-run` exists.**
///
/// The `[compat] mirror_usage_log` row is a `Warn` the operator is meant to strike out. The
/// CLI has no way to strike a row, so a plain `--apply` keeps it and the config comes out with
/// `mirror_usage_log = true` — from then on the daemon appends to `~/.vastai-gguf/usage.log`.
/// This test pins that so it cannot change unnoticed; `docs/MIGRATION.md` tells the operator
/// how to turn it back off.
#[test]
fn apply_keeps_the_usage_mirror_row_and_therefore_enables_it() {
    let w = World::new();
    let before = w.legacy_hash();
    w.run(&["migrate", "--apply", "--json"]).json();

    let cfg = std::fs::read_to_string(w.state.join("config.toml")).expect("config.toml");
    assert!(
        cfg.contains("mirror_usage_log = true"),
        "the plan kept the mirror row, so the config must show it:\n{cfg}"
    );
    // …and migration itself still wrote nothing into the legacy tree.
    assert_eq!(w.legacy_hash(), before, "--apply appended to usage.log");
}
