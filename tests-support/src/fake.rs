//! The on-disk half: a directory that looks exactly like a llama.cpp build tree, with the
//! fake `llama-server` inside it.
//!
//! `core::discover::builds` globs `build*/bin/llama-server` and labels the build after the
//! directory two levels up, so the layout is not decorative:
//!
//! ```text
//! <tempdir>/                     <- put THIS in `[endpoints] build_roots`
//! ├── build-fake/bin/llama-server
//! ├── records/                   <- launch records land here, keyed by port
//! ├── models/
//! └── behavior                   <- optional default behaviour for every launch
//! ```

use crate::behavior::Behavior;
use crate::gguf::{self, GgufSpec};
use crate::record::Records;
use apexrouter_protocol::{BuildId, FlagSupport, Gpu, GpuBackend, LlamaBuild, RigSnapshot};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// The build directory name, and therefore the [`BuildId`] discovery derives.
pub const BUILD_DIR: &str = "build-fake";

/// The subdirectory launch records are written to.
pub const RECORDS_DIR: &str = "records";

/// A file in the build root whose contents are a default behaviour spec.
pub const BEHAVIOR_FILE: &str = "behavior";

/// Override the binary lookup entirely.
pub const BIN_ENV: &str = "APEX_FAKE_LLAMA_BIN";

/// Every flag the fake advertises in `--help`.
///
/// This is exactly the set `core::argv::plan_local` can emit, so nothing is dropped with a
/// "does not advertise" warning and the argv under test is the full one. Plus `--fit` and
/// `--models-dir`, which set `has_fit` and `has_router_mode`.
pub const ADVERTISED_FLAGS: &[&str] = &[
    "-h",
    "--help",
    "--version",
    "--list-devices",
    "-m",
    "--model",
    "--host",
    "--port",
    "-a",
    "--alias",
    "-dev",
    "--device",
    "-sm",
    "--split-mode",
    "-mg",
    "--main-gpu",
    "--tensor-split",
    "-c",
    "--ctx-size",
    "-np",
    "--parallel",
    "-ctk",
    "--cache-type-k",
    "-ctv",
    "--cache-type-v",
    "-ngl",
    "--n-gpu-layers",
    "-fa",
    "--flash-attn",
    "--jinja",
    "--no-jinja",
    "--metrics",
    "--props",
    "--slots",
    "--no-slots",
    "--mmproj",
    "--api-key",
    "--api-key-file",
    "--temp",
    "--top-p",
    "--top-k",
    "--min-p",
    "--presence-penalty",
    "--chat-template-kwargs",
    "--fit",
    "--models-dir",
    "--reasoning-format",
    "--reasoning-budget",
];

/// A temporary llama.cpp-shaped build tree holding the fake server.
///
/// Dropping it removes the tree, the records and any GGUF fixtures written into it.
pub struct FakeBuild {
    dir: tempfile::TempDir,
    server: PathBuf,
    records: Records,
}

impl FakeBuild {
    /// Lay out a build tree in a fresh temporary directory.
    ///
    /// # Panics
    /// When the fake binary cannot be found or built, or the tree cannot be written. Both
    /// are setup failures a test can do nothing about, and the message names the fix.
    pub fn new() -> FakeBuild {
        match FakeBuild::try_new() {
            Ok(b) => b,
            Err(e) => panic!("{e}"),
        }
    }

    /// As [`FakeBuild::new`], reporting instead of panicking.
    ///
    /// # Errors
    /// A rendered message naming the command to run by hand.
    pub fn try_new() -> Result<FakeBuild, String> {
        let source = binary_path()?;
        let dir = tempfile::Builder::new()
            .prefix("apex-fake-")
            .tempdir()
            .map_err(|e| format!("tempdir: {e}"))?;

        let bin_dir = dir.path().join(BUILD_DIR).join("bin");
        std::fs::create_dir_all(&bin_dir).map_err(|e| format!("{}: {e}", bin_dir.display()))?;
        let server = bin_dir.join("llama-server");
        // A link or a copy, never a symlink: `push_candidate` deduplicates candidates by
        // *canonical* path, so two build trees symlinked at one binary would collapse into
        // a single discovered build. A hard link is indistinguishable from a copy to
        // `canonicalize` and costs nothing; it only works when `$TMPDIR` and `target/`
        // share a filesystem, so the copy is the fallback.
        std::fs::hard_link(&source, &server)
            .or_else(|_| std::fs::copy(&source, &server).map(|_| ()))
            .map_err(|e| format!("link {} -> {}: {e}", source.display(), server.display()))?;
        make_executable(&server)?;

        // The fake resolves its record destination from `$APEX_FAKE_LLAMA_RECORD` before it
        // falls back to the build root, so when that variable is set this side must follow
        // it too. Otherwise `records()` would read an empty directory and the failure would
        // look like "the child never started".
        let records = match std::env::var_os("APEX_FAKE_LLAMA_RECORD") {
            Some(dir) => Records::at(PathBuf::from(dir)),
            None => Records::at(dir.path().join(RECORDS_DIR)),
        };
        std::fs::create_dir_all(dir.path().join("models"))
            .map_err(|e| format!("models dir: {e}"))?;

        Ok(FakeBuild {
            dir,
            server,
            records,
        })
    }

    /// The build **root** — the path to add to `[endpoints] build_roots`.
    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    /// The `llama-server` itself, for a [`LlamaBuild`] built by hand.
    pub fn server_path(&self) -> &Path {
        &self.server
    }

    /// The id discovery will give this build: `build-fake`.
    pub fn build_id(&self) -> BuildId {
        BuildId::parse(BUILD_DIR).expect("build-fake is a valid slug")
    }

    /// Launch records written by every child started out of this tree.
    pub fn records(&self) -> &Records {
        &self.records
    }

    /// A default behaviour applied to every launch from this tree, for the cases where
    /// `extra_args` is out of reach (driving the CLI, say). Argv and `$APEX_FAKE_LLAMA_BEHAVIOR`
    /// both win over it.
    ///
    /// # Errors
    /// If the file cannot be written.
    pub fn set_default_behavior(&self, spec: &str) -> Result<(), String> {
        let path = self.dir.path().join(BEHAVIOR_FILE);
        std::fs::write(&path, spec).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Write a synthetic GGUF into the tree and return its path.
    ///
    /// # Panics
    /// When the file cannot be written.
    pub fn model(&self, name: &str, spec: &GgufSpec) -> PathBuf {
        let path = self.dir.path().join("models").join(name);
        if let Err(e) = gguf::write(&path, spec) {
            panic!("{e}");
        }
        path
    }

    /// The [`LlamaBuild`] this tree would be discovered as — without running discovery.
    ///
    /// Hand it to `LocalProvisioner::set_rig` (through [`FakeBuild::rig`]) and the
    /// supervisor never shells out to `--help` or `--list-devices`, which makes a
    /// supervisor test both faster and independent of what is installed on the box.
    pub fn llama_build(&self) -> LlamaBuild {
        LlamaBuild {
            id: self.build_id(),
            server_path: self.server.display().to_string(),
            label: BUILD_DIR.to_owned(),
            build_info: Some(crate::server::BUILD_INFO.to_owned()),
            backends: vec![GpuBackend::Vulkan, GpuBackend::Cpu],
            devices: vec!["Vulkan0".to_owned()],
            flags: flag_support(),
            probed_at_unix: 0,
        }
    }

    /// A one-GPU rig snapshot around [`FakeBuild::llama_build`].
    pub fn rig(&self, vram_total_mb: u64, vram_free_mb: u64) -> RigSnapshot {
        let build = self.llama_build();
        RigSnapshot {
            gpus: vec![Gpu {
                device: "Vulkan0".to_owned(),
                index: 0,
                name: "AMD Radeon 840M Graphics (RADV KRACKAN1)".to_owned(),
                backend: GpuBackend::Vulkan,
                vram_total_mb,
                vram_free_mb: vram_free_mb.min(vram_total_mb),
                pci_bus_id: None,
                driver: Some("radv".to_owned()),
                is_software: false,
                seen_by_builds: vec![build.id.clone()],
                held_by: Vec::new(),
                reserved_mb: 0,
            }],
            builds: vec![build],
            ram_total_mb: 22_000,
            ram_free_mb: 10_000,
            swap_total_mb: 8_000,
            swap_used_mb: 0,
            cpu_threads: 12,
            scanned_at_unix: 0,
        }
    }

    /// Keep the tree after the test ends, and say where it is. For debugging a failure
    /// whose evidence is in the log file.
    pub fn keep(self) -> PathBuf {
        let path = self.dir.keep();
        eprintln!("apex-fake: build tree kept at {}", path.display());
        path
    }
}

impl Default for FakeBuild {
    fn default() -> Self {
        FakeBuild::new()
    }
}

/// The [`FlagSupport`] a b9199 `--help` produces, matching [`ADVERTISED_FLAGS`].
pub fn flag_support() -> FlagSupport {
    let flags: BTreeSet<String> = ADVERTISED_FLAGS.iter().map(|s| (*s).to_owned()).collect();
    FlagSupport {
        jinja_default_on: true,
        fa_tristate: true,
        has_fit: flags.contains("--fit"),
        has_router_mode: flags.contains("--models-dir"),
        help_lines: u32::try_from(help_text().lines().count()).unwrap_or(0),
        flags,
    }
}

/// Where the fake `llama-server` binary is.
///
/// Resolved once per process, in this order:
///
/// 1. `$APEX_FAKE_LLAMA_BIN`.
/// 2. Next to the running test binary — `target/debug/llama-server`, which is what
///    `cargo test --workspace` and `cargo build -p apexrouter-tests-support --bin
///    llama-server` both produce.
/// 3. Built on demand, `--offline`, into `target/apex-tests-support/`. A **separate**
///    target directory on purpose: `cargo test` holds a lock on its own, and a nested
///    build sharing it would wait for a lock its own parent is holding.
///
/// # Errors
/// A message naming the one command that fixes it.
pub fn binary_path() -> Result<PathBuf, String> {
    static RESOLVED: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    RESOLVED.get_or_init(resolve_binary).clone()
}

fn resolve_binary() -> Result<PathBuf, String> {
    if let Some(p) = std::env::var_os(BIN_ENV) {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!(
            "{BIN_ENV} points at {}, which is not a file",
            path.display()
        ));
    }
    if let Some(found) = near_current_exe() {
        return Ok(found);
    }
    build_on_demand()
}

/// `target/debug/llama-server` and friends, relative to the running test binary.
fn near_current_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let mut dir = exe.parent()?;
    for _ in 0..4 {
        for candidate in [dir.join("llama-server"), dir.join("examples/llama-server")] {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        dir = dir.parent()?;
    }
    None
}

/// `cargo build --offline --bin llama-server`, into a target directory of its own.
fn build_on_demand() -> Result<PathBuf, String> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let manifest = manifest_dir.join("Cargo.toml");
    let target = manifest_dir
        .parent()
        .unwrap_or(&manifest_dir)
        .join("target")
        .join("apex-tests-support");
    let built = target.join("debug").join("llama-server");
    if built.is_file() {
        return Ok(built);
    }

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let out = Command::new(&cargo)
        .args([
            "build",
            "--quiet",
            "--offline",
            "--bin",
            "llama-server",
            "--manifest-path",
        ])
        .arg(&manifest)
        .env("CARGO_TARGET_DIR", &target)
        .output()
        .map_err(|e| format!("could not run `{cargo} build`: {e}"))?;

    if built.is_file() {
        return Ok(built);
    }
    Err(format!(
        "the fake llama-server is not built and could not be built on demand.\n\
         Run this once:\n    cargo build -p apexrouter-tests-support --bin llama-server\n\
         (or set {BIN_ENV} to a built copy)\n\
         cargo said: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

/// `chmod +x`, which a `fs::copy` preserves but a future `fs::write` would not.
fn make_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("chmod {}: {e}", path.display()))
}

// -------------------------------------------------------------------------------------
// The three things the binary prints instead of serving
// -------------------------------------------------------------------------------------

/// `--help`, in the flush-left b9199 layout `core::discover::builds::parse_help` expects.
///
/// The flag names must start at column 0: an indented line is read as a continuation of
/// the previous entry's description, which is how the real parser tells a wrapped
/// description from a new flag.
pub fn help_text() -> String {
    let mut out = String::from(
        "----- common params -----\n\
         \n",
    );
    let describe = |flag: &str| -> &'static str {
        match flag {
            "-fa" | "--flash-attn" => "[on|off|auto] set Flash Attention use (default: auto)",
            "--jinja" => "use jinja template for chat (default: enabled)",
            "--no-jinja" => "disable jinja template for chat (default: enabled)",
            "-c" | "--ctx-size" => "N size of the prompt context (default: 0, 0 = from model)",
            "-np" | "--parallel" => "N number of parallel sequences (default: -1)",
            "--fit" => "[on|off] automatically size the context to fit (default: on)",
            "--models-dir" => "PATH directory of models to serve on demand",
            _ => "a flag this fake advertises so the argv builder emits it",
        }
    };
    for flag in ADVERTISED_FLAGS {
        out.push_str(&format!("{flag:<34}{}\n", describe(flag)));
    }
    out
}

/// `--list-devices`, in the table `parse_devices` reads.
pub fn devices_text() -> String {
    "ggml_vulkan: Found 1 Vulkan devices:\n\
     Available devices:\n  \
     Vulkan0: AMD Radeon 840M Graphics (RADV KRACKAN1) (20992 MiB, 19518 MiB free)\n"
        .to_owned()
}

/// `--version`, normalised by `parse_build_info` to `b9199 (39cf5d619)`.
pub fn version_text() -> String {
    "version: 9199 (39cf5d619)\nbuilt with GNU 15.2.0 for x86_64-linux-gnu\n".to_owned()
}

/// The behaviour for one launch: argv, then the environment, then the build root's file.
pub fn behavior_for(argv: &[String], argv0: &str) -> Behavior {
    let mut b = Behavior::default();
    if let Some(root) = build_root_of(argv0) {
        if let Ok(spec) = std::fs::read_to_string(root.join(BEHAVIOR_FILE)) {
            b.apply_spec(spec.trim());
        }
    }
    if let Ok(spec) = std::env::var("APEX_FAKE_LLAMA_BEHAVIOR") {
        b.apply_spec(&spec);
    }
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--apex-behavior" => {
                if let Some(spec) = argv.get(i + 1) {
                    b.apply_spec(spec);
                }
                i += 2;
            }
            // The two spellings the incumbent Python stand-in used, so a test copied from
            // `providers/src/local/tests.rs` behaves the same way here.
            "--fake-exit-early" => {
                b.refuse_start = true;
                i += 1;
            }
            "--fake-never-healthy" => {
                b.stall = true;
                i += 1;
            }
            _ => i += 1,
        }
    }
    b
}

/// Where to write the launch record: `--apex-record`, then the environment, then
/// `<build root>/records` when that directory exists.
pub fn record_dest(argv: &[String], argv0: &str) -> Option<PathBuf> {
    let mut i = 0;
    while i < argv.len() {
        if argv[i] == "--apex-record" {
            return argv.get(i + 1).map(PathBuf::from);
        }
        i += 1;
    }
    if let Some(p) = std::env::var_os("APEX_FAKE_LLAMA_RECORD") {
        return Some(PathBuf::from(p));
    }
    let dir = build_root_of(argv0)?.join(RECORDS_DIR);
    dir.is_dir().then_some(dir)
}

/// `<root>/build-fake/bin/llama-server` -> `<root>`.
fn build_root_of(argv0: &str) -> Option<PathBuf> {
    Path::new(argv0)
        .parent() // bin
        .and_then(Path::parent) // build-fake
        .and_then(Path::parent) // the root
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_help_text_parses_into_the_flag_support_we_claim() {
        // The same assertion the real discovery makes, without depending on it: every
        // advertised flag must be at column 0 so it is read as a flag entry.
        let text = help_text();
        for flag in ADVERTISED_FLAGS {
            assert!(
                text.lines().any(|l| l.starts_with(flag)),
                "{flag} is not flush-left in --help"
            );
        }
        assert!(text.contains("on|off|auto"), "-fa must look tristate");
        assert!(
            text.contains("default: enabled"),
            "--jinja must be default-on"
        );
    }

    #[test]
    fn the_device_table_is_indented_under_its_header() {
        let text = devices_text();
        let header = text
            .lines()
            .position(|l| {
                l.trim()
                    .to_ascii_lowercase()
                    .starts_with("available devices")
            })
            .expect("header");
        let row = text.lines().nth(header + 1).expect("row");
        assert!(
            row.starts_with(char::is_whitespace),
            "rows must be indented"
        );
        assert!(row.contains("Vulkan0:"));
        assert!(row.trim_end().ends_with("free)"));
    }

    #[test]
    fn the_record_destination_prefers_argv_over_the_environment() {
        let argv: Vec<String> = ["--apex-record", "/tmp/somewhere/rec.json"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        assert_eq!(
            record_dest(&argv, "/x/build-fake/bin/llama-server"),
            Some(PathBuf::from("/tmp/somewhere/rec.json"))
        );
    }

    #[test]
    fn behaviour_comes_off_argv_including_the_incumbent_spellings() {
        let argv: Vec<String> = ["--apex-behavior", "load_ms=50,reasoning"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let b = behavior_for(&argv, "llama-server");
        assert_eq!(b.load_ms, 50);
        assert!(b.reasoning);

        let argv = vec!["--fake-exit-early".to_owned()];
        assert!(behavior_for(&argv, "llama-server").refuse_start);
        let argv = vec!["--fake-never-healthy".to_owned()];
        assert!(behavior_for(&argv, "llama-server").stall);
    }
}
