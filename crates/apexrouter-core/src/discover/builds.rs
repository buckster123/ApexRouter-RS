//! OWNER: unit C-09 (core/discover/builds.rs, core/discover/mod.rs). Do not edit outside
//! that unit.
//!
//! Build and device discovery, and the three measured defects it closes:
//!
//! * A **fixed candidate list** misses `build-mtp` and `build-zaya1`. We glob
//!   `build*/bin/llama-server` under every configured root and `$PATH`, and label by
//!   build-dir name.
//! * **Backend detection by grepping `--help`** reports `cuda` on an AMD box. We use
//!   `llama-server --list-devices`, and fall back to inspecting sibling `libggml-*.so`.
//! * A **substring heuristic** silently chooses HIP when Vulkan was asked for.
//!   [`choose_build`] returns a `BinaryChoiceInfo` whose `exact: false` is a **visible**
//!   value the UI renders as a warning.
//!
//! …and the fourth, found by the MK1 acceptance gate: a backend may report **more free VRAM
//! than the card has**. `--list-devices` is the single point at which a device's memory
//! enters this program, so it is the single point at which that is made impossible —
//! [`VramReading`] is the only way this module builds a [`Gpu`]'s memory pair, and its one
//! constructor enforces `free <= total`. See its docs for the measured numbers.
//!
//! Two conventions the rest of the crate depends on:
//!
//! * Every probe runs the binary with `LD_LIBRARY_PATH = dirname(binary)`, because
//!   `build-vulkan`'s trailing-colon RUNPATH otherwise picks up a sibling build's `.so`
//!   and the probe then describes the wrong build.
//! * A software rasteriser (`llvmpipe` and friends) is **marked, never dropped** —
//!   `Gpu::is_software` is the published carrier of that fact, and `RigSnapshot::gpus`
//!   specifies devices as "marked, not hidden". Device *selection* takes the non-software
//!   ones unless software is explicitly asked for; discovery keeps the information.

use crate::config::EndpointsCfg;
use crate::error::{Error, Result};
use crate::exec;
use apexrouter_protocol::{BinaryChoiceInfo, BuildId, FlagSupport, Gpu, GpuBackend, LlamaBuild};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

/// `--help` is pure string formatting, but a cold binary on a loaded disk is not instant.
const HELP_TIMEOUT: Duration = Duration::from_secs(60);
/// `--list-devices` initialises the compute backend, which on a cold GPU is seconds.
const DEVICES_TIMEOUT: Duration = Duration::from_secs(90);
/// `--version` prints two lines and exits.
const VERSION_TIMEOUT: Duration = Duration::from_secs(30);

/// The binary we are looking for. A "server" that is not this one is not a llama.cpp build.
const SERVER_BIN: &str = "llama-server";

/// Substrings that identify a software rasteriser in a device name.
const SOFTWARE_MARKERS: &[&str] = &[
    "llvmpipe",
    "lavapipe",
    "swiftshader",
    "softpipe",
    "software rasterizer",
];

/// The line that introduces the `--list-devices` table, lowercased.
const DEVICE_HEADER: &str = "available devices";

/// One device's memory reading, with the impossible pair removed **at construction**.
///
/// `--list-devices` prints `(<total> MiB, <free> MiB free)` verbatim from the backend, and a
/// backend may report a `free` larger than the `total` it printed on the same line. Measured
/// on this box, same silicon, same minute:
///
/// ```text
///   ROCm0:   AMD Radeon 840M Graphics                    (11397 MiB, 18482 MiB free)
///   Vulkan0: AMD Radeon 840M Graphics (RADV KRACKAN1)    (20992 MiB, 20363 MiB free)
/// ```
///
/// HIP counts **GTT** — system memory the card may borrow — as free VRAM, against a `total`
/// that is only the carve-out. The excess is not usable VRAM, it is an accounting artefact,
/// and everything downstream of it is wrong: `total - free` underflows a `u64` to 17 EiB, and
/// a budget built from `free` promised 16115 MiB on a device that then failed a 7312 MiB
/// `cudaMalloc` for the KV cache — the MK1 acceptance gate's first launch line.
///
/// Clamping the *subtraction* only fixes the callers you remember. So the pair is not carried
/// as two `u64`s a caller must remember to compare: [`VramReading::observed`] is the only
/// constructor, it enforces `free <= total`, and it is the only way this module fills
/// [`Gpu::vram_total_mb`] and [`Gpu::vram_free_mb`]. The impossible reading is therefore not
/// guarded, it is unrepresentable — while [`VramReading::reported_free_mb`] keeps what the
/// driver actually said, so the correction can be logged rather than hidden.
///
/// `total_mb == 0` means the line carried no memory group at all (`WebGPU0: Something New`);
/// `--list-devices` prints both numbers or neither, so `free` is then 0 too and the clamp is
/// a no-op on every input this parser can produce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VramReading {
    total_mb: u64,
    free_mb: u64,
    reported_free_mb: u64,
}

impl VramReading {
    /// The only constructor: `free` above `total` is clamped to `total`, always.
    pub fn observed(total_mb: u64, reported_free_mb: u64) -> Self {
        Self {
            total_mb,
            free_mb: reported_free_mb.min(total_mb),
            reported_free_mb,
        }
    }

    /// Total memory the backend claims for this device, MiB.
    pub fn total_mb(&self) -> u64 {
        self.total_mb
    }

    /// Free memory, MiB. **Never** greater than [`VramReading::total_mb`].
    pub fn free_mb(&self) -> u64 {
        self.free_mb
    }

    /// What the backend actually printed, before the clamp — the evidence, kept.
    pub fn reported_free_mb(&self) -> u64 {
        self.reported_free_mb
    }

    /// True when the reading had to be corrected: the backend reported more free memory than
    /// the device has. AMD's GTT accounting is the live example.
    pub fn was_clamped(&self) -> bool {
        self.reported_free_mb > self.total_mb
    }
}

/// One cached `--help` probe. The key is `(path, mtime, size)`, so a rebuilt binary at the
/// same path invalidates itself and nobody has to remember to clear a cache.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct FlagCache {
    /// The binary this describes. Stored so a filename collision cannot silently answer
    /// for a different file.
    path: String,
    /// Modification time, nanoseconds since the epoch.
    mtime_ns: u128,
    /// Size in bytes.
    size: u64,
    /// The parsed result.
    flags: FlagSupport,
}

/// Glob `build*/bin/llama-server` under every configured root and `$PATH`.
///
/// Every candidate is probed: `--help` (cached by `(path, mtime, size)`), `--list-devices`
/// for the backends it can actually see, and `--version` for the build string. A binary
/// that will not run — `build-rocm` with a missing `libhipblas.so` is the live example — is
/// still reported, with empty `backends` and `devices`, because "installed and broken" is
/// precisely what the operator needs to see.
///
/// Results are deduplicated by canonical path and sorted by [`BuildId`], so two scans of an
/// unchanged machine produce identical output.
pub async fn discover_builds(cfg: &EndpointsCfg, cache: &Path) -> Result<Vec<LlamaBuild>> {
    let ignore: Vec<glob::Pattern> = cfg
        .ignore_globs
        .iter()
        .filter_map(|g| glob::Pattern::new(g).ok())
        .collect();

    let mut candidates: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();

    for root in &cfg.build_roots {
        for path in glob_root(&expand_tilde(root)) {
            push_candidate(path, &ignore, &mut candidates, &mut seen);
        }
    }
    for dir in path_dirs() {
        push_candidate(dir.join(SERVER_BIN), &ignore, &mut candidates, &mut seen);
    }

    let now = chrono::Utc::now().timestamp();
    let mut out: Vec<LlamaBuild> = Vec::with_capacity(candidates.len());
    let mut used_ids: HashSet<String> = HashSet::new();

    for path in candidates {
        let label = build_label(&path);
        let id = unique_id(&label, &mut used_ids)?;

        let flags = match probe_flags(&path, cache).await {
            Ok(f) => f,
            Err(e) => {
                tracing::debug!(path = %path.display(), error = %e, "llama-server --help failed");
                FlagSupport::default()
            }
        };
        let gpus = match probe_devices(&path).await {
            Ok(g) => g,
            Err(e) => {
                tracing::debug!(path = %path.display(), error = %e, "llama-server --list-devices failed");
                Vec::new()
            }
        };

        let mut backends: Vec<GpuBackend> = Vec::new();
        for g in &gpus {
            if !backends.contains(&g.backend) {
                backends.push(g.backend.clone());
            }
        }

        out.push(LlamaBuild {
            id,
            server_path: path.display().to_string(),
            label,
            build_info: probe_build_info(&path).await,
            backends,
            devices: gpus.into_iter().map(|g| g.device).collect(),
            flags,
            probed_at_unix: now,
        });
    }

    out.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    Ok(out)
}

/// `llama-server --list-devices`. **Never** a `--help` grep. `llvmpipe` is marked
/// `is_software` and excluded from default selection.
///
/// The device table is on stdout; backend chatter (`ggml_cuda_init: found 1 ROCm devices`)
/// goes to stderr and is deliberately not parsed — that line is exactly the one that
/// reports `cuda` on an AMD box.
///
/// A binary that enumerates nothing and exits non-zero is an `Err`; one that enumerates
/// nothing and exits cleanly returns an empty list, the honest answer for a CPU-only build.
///
/// Each returned [`Gpu`] carries the PCI address of the silicon behind it when
/// [`crate::discover::physical`] can align the enumeration with `/sys/bus/pci/devices`.
/// That is what lets two builds' views of one card be recognised as one card.
pub async fn probe_devices(server: &Path) -> Result<Vec<Gpu>> {
    let out = run_probe(server, &["--list-devices"], DEVICES_TIMEOUT).await?;
    let saw_header = out
        .stdout
        .lines()
        .any(|l| l.trim().to_ascii_lowercase().starts_with(DEVICE_HEADER));

    if !saw_header && (out.status != 0 || out.timed_out) {
        return Err(probe_failed(server, "--list-devices", &out));
    }
    let mut gpus = parse_devices(&out.stdout, &build_id_for(server)?);
    crate::discover::physical::attach_pci_ids(
        &mut gpus,
        &crate::discover::physical::scan_pci_gpus(),
    );
    Ok(gpus)
}

/// `llama-server --help`, cached in `$CACHE` keyed by `(path, mtime, size)`.
///
/// Never a hardcoded whitelist: b9199 already moved `-fa` to `on|off|auto`, made `--jinja`
/// default-on and deprecated `--webui`.
///
/// A cache that cannot be written is logged and ignored: an unwritable cache directory must
/// make discovery slow, not broken.
pub async fn probe_flags(server: &Path, cache: &Path) -> Result<FlagSupport> {
    let (mtime_ns, size) = stat_key(server)?;
    let entry_path = flag_cache_path(cache, server);

    if let Some(hit) = read_flag_cache(&entry_path) {
        if hit.path == server.display().to_string() && hit.mtime_ns == mtime_ns && hit.size == size
        {
            return Ok(hit.flags);
        }
    }

    let out = run_probe(server, &["--help"], HELP_TIMEOUT).await?;
    // b9199 prints help on stdout; older builds used stderr. Take whichever is fuller.
    let text = if out.stdout.lines().count() >= out.stderr.lines().count() {
        &out.stdout
    } else {
        &out.stderr
    };
    if text.trim().is_empty() {
        return Err(probe_failed(server, "--help", &out));
    }
    let flags = parse_help(text);

    write_flag_cache(
        &entry_path,
        &FlagCache {
            path: server.display().to_string(),
            mtime_ns,
            size,
            flags: flags.clone(),
        },
    );
    Ok(flags)
}

/// Pick a build for a wanted backend, reporting a fallback rather than performing one
/// silently.
///
/// With `want: None` the choice is `exact: true` — nothing specific was asked for, so
/// nothing was substituted — and `got` names the backend the chosen build actually
/// enumerated. With `want: Some(b)` and no build that enumerated `b`, the result is
/// `exact: false` with both `wanted` and `got` populated: the caller still gets a usable
/// build, and the UI has what it needs to say "you asked for Vulkan, this is HIP".
///
/// **`want: None` is the default launch** (`apexrouter up` with no `--devices`), so what it
/// prefers matters more than anything else in this module: see [`rank`] and
/// [`backend_confidence`] for the order and the reasoning. An explicit `want` is never
/// overruled by it — a named backend that exists is always an exact match.
///
/// `None` only when there are no builds at all.
pub fn choose_build(builds: &[LlamaBuild], want: Option<GpuBackend>) -> Option<BinaryChoiceInfo> {
    let Some(want) = want else {
        let b = best(builds.iter())?;
        return Some(BinaryChoiceInfo {
            chosen: b.id.clone(),
            exact: true,
            wanted: None,
            got: primary_backend(b),
        });
    };

    if let Some(b) = best(builds.iter().filter(|b| b.backends.contains(&want))) {
        return Some(BinaryChoiceInfo {
            chosen: b.id.clone(),
            exact: true,
            wanted: Some(want.clone()),
            got: Some(want),
        });
    }

    let b = best(builds.iter())?;
    Some(BinaryChoiceInfo {
        chosen: b.id.clone(),
        exact: false,
        wanted: Some(want),
        got: primary_backend(b),
    })
}

// ---------------------------------------------------------------------------
// candidate enumeration
// ---------------------------------------------------------------------------

/// Every `llama-server` under one root: `build*/bin/`, `build*/`, `bin/`, and the root
/// itself — so a root may be a tree of builds, a single build, or a plain `bin` directory.
fn glob_root(root: &Path) -> Vec<PathBuf> {
    const PATTERNS: &[&str] = &["build*/bin", "build*", "bin", ""];
    let mut out = Vec::new();
    for pat in PATTERNS {
        let joined = if pat.is_empty() {
            root.join(SERVER_BIN)
        } else {
            root.join(pat).join(SERVER_BIN)
        };
        let Some(pattern) = joined.to_str() else {
            continue;
        };
        match glob::glob(pattern) {
            Ok(paths) => out.extend(paths.filter_map(|p| p.ok())),
            Err(e) => tracing::debug!(pattern, error = %e, "bad build-root glob"),
        }
    }
    out
}

/// `$PATH`, split into directories.
fn path_dirs() -> Vec<PathBuf> {
    match std::env::var_os("PATH") {
        Some(p) => std::env::split_paths(&p).collect(),
        None => Vec::new(),
    }
}

/// Accept a candidate when it is an executable file, is not ignored, and has not already
/// been seen under another name.
fn push_candidate(
    path: PathBuf,
    ignore: &[glob::Pattern],
    out: &mut Vec<PathBuf>,
    seen: &mut HashSet<PathBuf>,
) {
    if !is_executable_file(&path) {
        return;
    }
    if ignore.iter().any(|p| p.matches_path(&path)) {
        return;
    }
    let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
    if seen.insert(canonical) {
        out.push(path);
    }
}

/// A regular file with at least one execute bit.
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(m) => m.is_file() && m.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

/// The build-dir name: the directory holding `bin/`, or the directory holding the binary
/// when there is no `bin/` level.
fn build_label(server: &Path) -> String {
    let parent = server.parent();
    let dir = match parent {
        Some(p) if p.file_name().map(|n| n == "bin").unwrap_or(false) => p.parent().or(parent),
        other => other,
    };
    dir.and_then(|d| d.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| SERVER_BIN.to_owned())
}

/// A [`BuildId`] for this binary from its build-dir name, so [`probe_devices`] can fill
/// `Gpu::seen_by_builds` without a second discovery pass.
fn build_id_for(server: &Path) -> Result<BuildId> {
    Ok(BuildId::parse(&slugify(&build_label(server)))?)
}

/// The build-dir name as a slug, made unique against the ids already handed out.
fn unique_id(label: &str, used: &mut HashSet<String>) -> Result<BuildId> {
    let base = slugify(label);
    let mut candidate = base.clone();
    let mut n = 1u32;
    while !used.insert(candidate.clone()) {
        n += 1;
        candidate = format!("{base}-{n}");
    }
    Ok(BuildId::parse(&candidate)?)
}

/// Lowercase, `[a-z0-9._-]`, first character alphanumeric, at most 64 bytes — the
/// [`BuildId`] charset.
fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let c = ch.to_ascii_lowercase();
        let ok = c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_' || c == '-';
        out.push(if ok { c } else { '-' });
    }
    while out
        .chars()
        .next()
        .map(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit()))
        .unwrap_or(false)
    {
        out.remove(0);
    }
    out.truncate(64);
    if out.is_empty() {
        SERVER_BIN.to_owned()
    } else {
        out
    }
}

/// `~` and `~/…` against `$HOME`; everything else verbatim.
fn expand_tilde(s: &str) -> PathBuf {
    if s == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(s));
    }
    match s.strip_prefix("~/") {
        Some(rest) => match dirs::home_dir() {
            Some(h) => h.join(rest),
            None => PathBuf::from(s),
        },
        None => PathBuf::from(s),
    }
}

// ---------------------------------------------------------------------------
// probing
// ---------------------------------------------------------------------------

/// Run the binary with `LD_LIBRARY_PATH` pinned to its own directory.
///
/// `build-vulkan`'s RUNPATH ends in a colon, which makes the loader search the current
/// directory and can bind a sibling build's `libggml-*.so`. Pinning the variable makes a
/// probe describe the build it was asked about.
async fn run_probe(server: &Path, args: &[&str], timeout: Duration) -> Result<exec::Output> {
    let dir = server
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    exec::run_env(server, args, &[("LD_LIBRARY_PATH", dir.as_str())], timeout).await
}

/// A probe that ran and failed, described by what the operator would have seen.
fn probe_failed(server: &Path, what: &str, out: &exec::Output) -> Error {
    let detail = out
        .stderr
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or(if out.timed_out {
            "timed out"
        } else {
            "no output"
        });
    Error::Invalid {
        what: format!("{} {what}", server.display()),
        why: detail.trim().to_owned(),
    }
}

/// `--version`, normalised to `"b9199 (39cf5d619)"`. `None` when the binary would not run.
async fn probe_build_info(server: &Path) -> Option<String> {
    let out = run_probe(server, &["--version"], VERSION_TIMEOUT)
        .await
        .ok()?;
    parse_build_info(&out.stdout).or_else(|| parse_build_info(&out.stderr))
}

/// The `version: 9199 (39cf5d619)` line, as `b9199 (39cf5d619)`.
fn parse_build_info(text: &str) -> Option<String> {
    let rest = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("version:"))?
        .trim();
    if rest.is_empty() {
        return None;
    }
    let already_prefixed = rest
        .strip_prefix('b')
        .and_then(|r| r.chars().next())
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false);
    Some(if already_prefixed {
        rest.to_owned()
    } else {
        format!("b{rest}")
    })
}

// ---------------------------------------------------------------------------
// --list-devices parsing
// ---------------------------------------------------------------------------

/// Parse the `Available devices:` table.
///
/// ```text
/// Available devices:
///   Vulkan0: AMD Radeon 840M Graphics (RADV KRACKAN1) (20992 MiB, 19066 MiB free)
/// ```
///
/// The trailing parenthesised group carries the memory numbers; a device name may itself
/// contain parentheses, so the group is taken from the end of the line, not the first `(`.
///
/// This is the **one** place a device's memory enters ApexRouter, so it is the one place the
/// `free > total` artefact is corrected: the numbers become a [`VramReading`] before they
/// become a [`Gpu`], and the correction is logged with both figures.
fn parse_devices(stdout: &str, seen_by: &BuildId) -> Vec<Gpu> {
    let mut out = Vec::new();
    let mut in_table = false;

    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.to_ascii_lowercase().starts_with(DEVICE_HEADER) {
            in_table = true;
            continue;
        }
        if !in_table || trimmed.is_empty() {
            continue;
        }
        // Table rows are indented; anything flush-left ends the table.
        if !line.starts_with(char::is_whitespace) {
            break;
        }
        let Some((token, rest)) = trimmed.split_once(':') else {
            continue;
        };
        let token = token.trim();
        if token.is_empty() || token.contains(char::is_whitespace) {
            continue;
        }
        let (name, vram) = split_device_memory(rest.trim());
        let (backend, index) = parse_device_token(token);
        if vram.was_clamped() {
            tracing::warn!(
                device = token,
                build = seen_by.as_str(),
                reported_free_mb = vram.reported_free_mb(),
                total_mb = vram.total_mb(),
                "device reports more free memory than it has (GTT/shared-memory accounting); \
                 clamped to total"
            );
        }
        let lower = name.to_ascii_lowercase();
        out.push(Gpu {
            device: token.to_owned(),
            index,
            name,
            backend,
            vram_total_mb: vram.total_mb(),
            vram_free_mb: vram.free_mb(),
            // Filled by `physical::attach_pci_ids`: `--list-devices` prints no bus id.
            pci_bus_id: None,
            driver: None,
            is_software: SOFTWARE_MARKERS.iter().any(|m| lower.contains(m)),
            seen_by_builds: vec![seen_by.clone()],
            held_by: Vec::new(),
            reserved_mb: 0,
        });
    }
    out
}

/// `NAME (20992 MiB, 19066 MiB free)` -> `("NAME", VramReading { 20992, 19066 })`.
///
/// The two numbers leave this function as a [`VramReading`] and never as a bare pair, so
/// there is no point between the parser and the [`Gpu`] at which `free > total` exists.
fn split_device_memory(rest: &str) -> (String, VramReading) {
    let unknown = |name: &str| (name.to_owned(), VramReading::observed(0, 0));
    let trimmed = rest.trim_end();
    let Some(open) = trimmed.rfind('(') else {
        return unknown(rest);
    };
    if !trimmed.ends_with(')') {
        return unknown(rest);
    }
    let inner = trimmed[open + 1..trimmed.len() - 1].trim();
    let mut parts = inner.split(',');
    let total = parts.next().and_then(parse_mib);
    let free = parts.next().and_then(parse_mib);
    match (total, free) {
        (Some(t), Some(f)) => (
            trimmed[..open].trim().to_owned(),
            VramReading::observed(t, f),
        ),
        // Not a memory group after all — some other parenthesised suffix. Keep the name.
        _ => unknown(rest),
    }
}

/// `"20992 MiB"` / `"19066 MiB free"` -> MiB.
fn parse_mib(s: &str) -> Option<u64> {
    let s = s.trim();
    let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() || !s[digits.len()..].trim_start().starts_with("MiB") {
        return None;
    }
    digits.parse().ok()
}

/// `"Vulkan0"` -> `(Vulkan, 0)`. An unknown prefix becomes `Other`, lowercased, rather than
/// being guessed at.
fn parse_device_token(token: &str) -> (GpuBackend, u32) {
    let digits_at = token
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_ascii_digit())
        .map(|(i, _)| i)
        .last()
        .unwrap_or(token.len());
    let (name, index) = token.split_at(digits_at);
    let index = index.parse().unwrap_or(0);
    let backend = match name.to_ascii_lowercase().as_str() {
        "vulkan" | "vk" => GpuBackend::Vulkan,
        "cuda" => GpuBackend::Cuda,
        "rocm" => GpuBackend::Rocm,
        "hip" => GpuBackend::Hip,
        "metal" | "mtl" => GpuBackend::Metal,
        "sycl" => GpuBackend::Sycl,
        "cpu" | "blas" => GpuBackend::Cpu,
        other => GpuBackend::Other(other.to_owned()),
    };
    (backend, index)
}

// ---------------------------------------------------------------------------
// --help parsing
// ---------------------------------------------------------------------------

/// Turn `--help` into a [`FlagSupport`]. Everything is read off the text; nothing is
/// inferred from a version number.
fn parse_help(text: &str) -> FlagSupport {
    let mut flags: BTreeSet<String> = BTreeSet::new();
    let mut jinja = String::new();
    let mut fa = String::new();
    let mut entry: Option<(Vec<String>, String)> = None;
    let mut lines = 0u32;

    for line in text.lines() {
        lines += 1;
        if line.starts_with(char::is_whitespace) {
            // A continuation of the current entry's description.
            if let Some((_, body)) = entry.as_mut() {
                body.push(' ');
                body.push_str(line.trim());
            }
            continue;
        }
        flush_entry(entry.take(), &mut flags, &mut jinja, &mut fa);
        let (names, rest) = head_flags(line);
        // An empty name list is a section header (`----- common params -----`) or a blank.
        if !names.is_empty() {
            entry = Some((names, rest));
        }
    }
    flush_entry(entry.take(), &mut flags, &mut jinja, &mut fa);

    FlagSupport {
        jinja_default_on: jinja_default_on(&jinja, &flags),
        fa_tristate: fa.contains("on|off|auto") || fa.contains("'on', 'off', or 'auto'"),
        has_fit: flags.contains("--fit"),
        has_router_mode: flags.contains("--models-dir"),
        help_lines: lines,
        flags,
    }
}

/// Fold one finished help entry into the accumulating flag set, keeping the text of the two
/// entries whose *wording* carries a fact we need.
fn flush_entry(
    entry: Option<(Vec<String>, String)>,
    flags: &mut BTreeSet<String>,
    jinja: &mut String,
    fa: &mut String,
) {
    let Some((names, text)) = entry else { return };
    if names.iter().any(|n| n == "--jinja" || n == "--no-jinja") {
        jinja.push_str(&text);
    }
    if names.iter().any(|n| n == "-fa" || n == "--flash-attn") {
        fa.push_str(&text);
    }
    flags.extend(names);
}

/// The flag names a help line opens with, and the remainder of the line.
///
/// Stops at the first token that is not a flag — the argument placeholder (`N`, `FNAME`,
/// `[on|off|auto]`, `{none,layer,row}`) or the start of the description.
fn head_flags(line: &str) -> (Vec<String>, String) {
    let mut names = Vec::new();
    let mut rest = line;
    loop {
        let trimmed = rest.trim_start();
        if trimmed.is_empty() {
            rest = trimmed;
            break;
        }
        let end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
        let token = trimmed[..end].trim_end_matches(',');
        if !is_flag(token) {
            rest = trimmed;
            break;
        }
        names.push(token.to_owned());
        rest = &trimmed[end..];
    }
    (names, rest.trim().to_owned())
}

/// One or two leading dashes then an alphanumeric. `-----` (a section rule), `lo-hi` and
/// `{none,linear}` are not flags.
fn is_flag(token: &str) -> bool {
    let body = token.trim_start_matches('-');
    let dashes = token.len() - body.len();
    (1..=2).contains(&dashes)
        && body
            .chars()
            .next()
            .map(|c| c.is_ascii_alphanumeric())
            .unwrap_or(false)
}

/// b9199+ enables jinja by default, which makes `--no-jinja` the meaningful flag. Read the
/// entry's own words; fall back to "the negative form exists, so the positive is default".
fn jinja_default_on(entry: &str, flags: &BTreeSet<String>) -> bool {
    let lower = entry.to_ascii_lowercase();
    if lower.contains("default: enabled") || lower.contains("default: true") {
        return true;
    }
    if lower.contains("default: disabled") || lower.contains("default: false") {
        return false;
    }
    flags.contains("--no-jinja")
}

// ---------------------------------------------------------------------------
// the flag cache
// ---------------------------------------------------------------------------

/// `(mtime as nanoseconds since the epoch, size in bytes)`.
fn stat_key(server: &Path) -> Result<(u128, u64)> {
    let meta = std::fs::metadata(server).map_err(|source| Error::Io {
        path: server.display().to_string(),
        source,
    })?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Ok((mtime, meta.len()))
}

/// `<cache>/builds/<sha256(path)>.json`. Hashed because a binary path is not a filename.
fn flag_cache_path(cache: &Path, server: &Path) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(server.display().to_string().as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(32);
    for byte in digest.iter().take(16) {
        // Writing into a String cannot fail; there is nothing to report.
        let _ = write!(hex, "{byte:02x}");
    }
    cache.join("builds").join(format!("{hex}.json"))
}

/// A cache hit, or `None` for any reason at all — a truncated or unreadable entry is a miss.
fn read_flag_cache(path: &Path) -> Option<FlagCache> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Best-effort write through a temporary file, so a concurrent reader never sees half an
/// entry. Failure is logged, never propagated.
fn write_flag_cache(path: &Path, entry: &FlagCache) {
    let Some(dir) = path.parent() else { return };
    if let Err(e) = std::fs::create_dir_all(dir) {
        tracing::debug!(dir = %dir.display(), error = %e, "flag cache dir");
        return;
    }
    let Ok(bytes) = serde_json::to_vec(entry) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = std::fs::write(&tmp, &bytes) {
        tracing::debug!(path = %tmp.display(), error = %e, "flag cache write");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        tracing::debug!(path = %path.display(), error = %e, "flag cache rename");
        let _ = std::fs::remove_file(&tmp);
    }
}

// ---------------------------------------------------------------------------
// build ranking
// ---------------------------------------------------------------------------

/// Prefer a build that enumerated an accelerator, then the backend whose enumeration
/// promises most about a launch, then the build that enumerated more devices, then the
/// lexicographically first id — so the answer never depends on directory order.
fn best<'a>(builds: impl Iterator<Item = &'a LlamaBuild>) -> Option<&'a LlamaBuild> {
    builds.max_by(|a, b| {
        rank(a)
            .cmp(&rank(b))
            .then_with(|| b.id.as_str().cmp(a.id.as_str()))
    })
}

/// `(has an accelerator, backend confidence, device count)`.
///
/// Confidence outranks the device count deliberately: `LlamaBuild::devices` counts every
/// token the build printed, software rasterisers included (they are *marked* on the `Gpu`,
/// not dropped, and a token list carries no such mark), so it is a noisy measure of "how much
/// of this machine can this build use" — while the backend decides whether the launch runs at
/// all.
fn rank(b: &LlamaBuild) -> (u8, u8, usize) {
    let accel = u8::from(b.backends.iter().any(|x| !matches!(x, GpuBackend::Cpu)));
    let confidence = b.backends.iter().map(backend_confidence).max().unwrap_or(0);
    (accel, confidence, b.devices.len())
}

/// How much a backend's **enumeration** promises about the launch that follows.
///
/// This is a tiebreak for a *default* — what to run when the operator named neither a build
/// nor a device. Before this existed the tiebreak between two builds that each saw one
/// accelerator was the **directory name**, which is a coin flip: on the development box
/// `~/llama.cpp/build` (ROCm) beat `~/llama.cpp/build-vulkan` because `"build"` sorts first,
/// and the ROCm launch died in `cudaMalloc` where the Vulkan build of the same model runs.
///
/// The ranking is not about speed, and it is not about a vendor. It is about how much
/// enumerating a device implies about being able to *use* it:
///
/// * **The vendor's own runtime for hardware the vendor ships it for** (CUDA, Metal). If it
///   enumerates, it is the first-class path and the fast one. Highest.
/// * **A portable, conformance-gated API** (Vulkan). A device appears only through an ICD
///   that implements the compute pipeline the runtime asked for, on any vendor's silicon.
///   Broad, and rarely a lie.
/// * **A vendor runtime whose support matrix is narrower than the hardware it binds**
///   (ROCm/HIP, SYCL). These enumerate whatever the runtime loads — supported or not — and
///   when the device is outside the matrix they fail *late*: at allocation, at dispatch, or
///   by reporting memory figures that are not memory (this box's ROCm view of the 840M claims
///   more free VRAM than the card has; see [`VramReading`]). A late failure is the worst
///   thing a default can do, because the manager has already promised a fit.
///
/// Nothing here is specific to one machine, and nothing here removes a choice: `--build`,
/// `--devices ROCm0` and `choose_build(builds, Some(Rocm))` all still select that build
/// exactly, and [`choose_build`] reports what it settled for either way. A faster specialist
/// is one flag away; a default that OOMs is a bug report.
fn backend_confidence(b: &GpuBackend) -> u8 {
    match b {
        GpuBackend::Cuda | GpuBackend::Metal => 3,
        GpuBackend::Vulkan => 2,
        // An accelerator we know can enumerate more than it can run…
        GpuBackend::Rocm | GpuBackend::Hip | GpuBackend::Sycl => 1,
        // …and one llama.cpp grew that this build has never heard of: still an accelerator,
        // but nothing is known about it, so it ranks with the ones that can disappoint.
        GpuBackend::Other(_) => 1,
        GpuBackend::Cpu => 0,
    }
}

/// The backend a build is "for": its first accelerator, else whatever it did enumerate.
fn primary_backend(b: &LlamaBuild) -> Option<GpuBackend> {
    b.backends
        .iter()
        .find(|x| !matches!(x, GpuBackend::Cpu))
        .or_else(|| b.backends.first())
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    // -- fixtures ---------------------------------------------------------

    fn write_exec(path: &Path, body: &str) {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).unwrap();
        }
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        f.flush().unwrap();
        drop(f);
        let mut perm = std::fs::metadata(path).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(path, perm).unwrap();
    }

    /// A stand-in `llama-server` that answers the three probes.
    const FAKE_SERVER: &str = r#"#!/bin/sh
case "$1" in
  --list-devices)
    printf 'Available devices:\n'
    printf '  Vulkan0: Fake Accelerator (RADV TEST) (20992 MiB, 19066 MiB free)\n'
    printf '  Vulkan1: llvmpipe (LLVM 19.1.0, 256 bits) (2048 MiB, 2048 MiB free)\n'
    ;;
  --version)
    printf 'version: 9199 (39cf5d619)\n' 1>&2
    printf 'built with GNU 15.2.0 for Linux x86_64\n' 1>&2
    ;;
  *)
    printf -- '-m,    --model FNAME                    model path\n'
    printf -- '-fa,   --flash-attn [on|off|auto]       set Flash Attention use\n'
    printf -- '--jinja, --no-jinja                     use jinja for chat (default: enabled)\n'
    printf -- '-fit,  --fit [on|off]                   fit to device memory\n'
    printf -- '--models-dir PATH                       router server model directory\n'
    ;;
esac
"#;

    fn cfg_with_root(root: &Path) -> EndpointsCfg {
        EndpointsCfg {
            build_roots: vec![root.display().to_string()],
            ..EndpointsCfg::default()
        }
    }

    fn llama_cpp_root() -> Option<PathBuf> {
        let root = dirs::home_dir()?.join("llama.cpp");
        root.is_dir().then_some(root)
    }

    fn a_build(id: &str, backends: Vec<GpuBackend>, devices: Vec<&str>) -> LlamaBuild {
        LlamaBuild {
            id: BuildId::parse(id).unwrap(),
            server_path: format!("/opt/{id}/bin/llama-server"),
            label: id.to_owned(),
            build_info: None,
            backends,
            devices: devices.into_iter().map(str::to_owned).collect(),
            flags: FlagSupport::default(),
            probed_at_unix: 0,
        }
    }

    // -- discovery --------------------------------------------------------

    #[tokio::test]
    async fn globs_build_dirs_and_labels_them_by_directory_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("llama.cpp");
        write_exec(&root.join("build-fake/bin/llama-server"), FAKE_SERVER);
        write_exec(&root.join("build-zaya1/bin/llama-server"), FAKE_SERVER);
        // Neither of these may show up: wrong directory shape, and not executable.
        write_exec(&root.join("scripts/tools/llama-server"), FAKE_SERVER);
        std::fs::create_dir_all(root.join("build-broken/bin")).unwrap();
        std::fs::write(root.join("build-broken/bin/llama-server"), "no +x").unwrap();

        let builds = discover_builds(&cfg_with_root(&root), &dir.path().join("cache"))
            .await
            .unwrap();
        let ids: Vec<&str> = builds.iter().map(|b| b.id.as_str()).collect();

        assert!(ids.contains(&"build-fake"), "{ids:?}");
        assert!(ids.contains(&"build-zaya1"), "{ids:?}");
        assert!(!ids.contains(&"scripts"), "{ids:?}");
        assert!(!ids.contains(&"tools"), "{ids:?}");
        assert!(!ids.contains(&"build-broken"), "{ids:?}");

        let fake = builds
            .iter()
            .find(|b| b.id.as_str() == "build-fake")
            .unwrap();
        assert_eq!(fake.label, "build-fake");
        assert!(fake.server_path.ends_with("build-fake/bin/llama-server"));
        assert_eq!(fake.backends, vec![GpuBackend::Vulkan]);
        assert_eq!(fake.devices, vec!["Vulkan0", "Vulkan1"]);
        assert_eq!(fake.build_info.as_deref(), Some("b9199 (39cf5d619)"));
        assert!(fake.flags.has("--model") && fake.flags.has("-m"));
        assert!(fake.flags.fa_tristate && fake.flags.jinja_default_on);
        assert!(fake.probed_at_unix > 0);
    }

    #[tokio::test]
    async fn a_binary_that_will_not_run_is_still_reported() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("llama.cpp");
        write_exec(
            &root.join("build-rocm/bin/llama-server"),
            "#!/bin/sh\nprintf 'libhipblas.so.3: cannot open shared object file\\n' 1>&2\nexit 127\n",
        );

        let builds = discover_builds(&cfg_with_root(&root), &dir.path().join("cache"))
            .await
            .unwrap();
        let mine: Vec<&LlamaBuild> = builds
            .iter()
            .filter(|b| b.id.as_str() == "build-rocm")
            .collect();
        assert_eq!(mine.len(), 1, "{builds:?}");
        assert!(mine[0].backends.is_empty());
        assert!(mine[0].devices.is_empty());
        assert!(mine[0].flags.flags.is_empty());
    }

    #[tokio::test]
    async fn ignore_globs_are_honoured_and_results_are_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("llama.cpp");
        write_exec(&root.join("build-zzz/bin/llama-server"), FAKE_SERVER);
        write_exec(&root.join("build-aaa/bin/llama-server"), FAKE_SERVER);
        write_exec(&root.join("build-skip/bin/llama-server"), FAKE_SERVER);

        let mut cfg = cfg_with_root(&root);
        cfg.ignore_globs = vec![format!("{}/build-skip/**", root.display())];

        let builds = discover_builds(&cfg, &dir.path().join("cache"))
            .await
            .unwrap();
        let ids: Vec<&str> = builds
            .iter()
            .map(|b| b.id.as_str())
            .filter(|id| id.starts_with("build-"))
            .collect();
        assert_eq!(ids, vec!["build-aaa", "build-zzz"], "{ids:?}");
    }

    // -- devices ----------------------------------------------------------

    #[tokio::test]
    async fn probe_devices_marks_llvmpipe_so_default_selection_excludes_it() {
        let dir = tempfile::tempdir().unwrap();
        let server = dir.path().join("build-fake/bin/llama-server");
        write_exec(&server, FAKE_SERVER);

        let gpus = probe_devices(&server).await.unwrap();
        assert_eq!(gpus.len(), 2, "{gpus:?}");
        assert!(!gpus[0].is_software);
        assert!(gpus[1].is_software, "llvmpipe must be marked software");

        // The selection every caller performs unless software is explicitly requested.
        let selectable: Vec<&str> = gpus
            .iter()
            .filter(|g| !g.is_software)
            .map(|g| g.device.as_str())
            .collect();
        assert_eq!(selectable, vec!["Vulkan0"]);

        assert_eq!(gpus[0].backend, GpuBackend::Vulkan);
        assert_eq!(gpus[0].index, 0);
        assert_eq!(gpus[0].name, "Fake Accelerator (RADV TEST)");
        assert_eq!(gpus[0].vram_total_mb, 20_992);
        assert_eq!(gpus[0].vram_free_mb, 19_066);
        assert_eq!(gpus[1].index, 1);
        assert_eq!(
            gpus[0].seen_by_builds,
            vec![BuildId::parse("build-fake").unwrap()]
        );
        assert!(gpus[0].held_by.is_empty() && gpus[0].reserved_mb == 0);
    }

    #[tokio::test]
    async fn a_binary_that_enumerates_nothing_and_fails_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let server = dir.path().join("build-broken/bin/llama-server");
        write_exec(&server, "#!/bin/sh\nprintf 'boom\\n' 1>&2\nexit 127\n");
        let err = probe_devices(&server).await.unwrap_err();
        assert!(matches!(err, Error::Invalid { .. }), "{err:?}");
        assert!(err.to_string().contains("boom"), "{err}");
    }

    #[test]
    fn device_table_parsing_handles_names_with_parentheses_and_unknown_backends() {
        let id = BuildId::parse("build-x").unwrap();
        let text = "\
ggml_cuda_init: found 1 ROCm devices
Available devices:
  Vulkan0: AMD Radeon 840M Graphics (RADV KRACKAN1) (20992 MiB, 19066 MiB free)
  CUDA1: NVIDIA GeForce RTX 4090 (24564 MiB, 24000 MiB free)
  WebGPU0: Something New
";
        let gpus = parse_devices(text, &id);
        assert_eq!(gpus.len(), 3, "{gpus:?}");
        assert_eq!(gpus[0].name, "AMD Radeon 840M Graphics (RADV KRACKAN1)");
        assert_eq!(gpus[0].backend, GpuBackend::Vulkan);
        assert_eq!(gpus[1].backend, GpuBackend::Cuda);
        assert_eq!(gpus[1].index, 1);
        assert_eq!(gpus[2].backend, GpuBackend::Other("webgpu".to_owned()));
        assert_eq!(gpus[2].name, "Something New");
        assert_eq!(gpus[2].vram_total_mb, 0);
        // The chatter above the table is never mistaken for a device.
        assert!(!gpus.iter().any(|g| g.device.contains("ggml")));
    }

    #[test]
    fn an_empty_device_table_is_not_an_error() {
        let id = BuildId::parse("build-cpu").unwrap();
        assert!(parse_devices("Available devices:\n", &id).is_empty());
        assert!(parse_devices("", &id).is_empty());
    }

    // -- the GTT trap ------------------------------------------------------
    //
    // MK1 acceptance, `apexrouter up Carnice-9b-Q6_K --alias auto`: the ROCm view of this
    // laptop's single 840M reported 17139 MiB free against an 11397 MiB total, the budget
    // came out at 16115 MiB, `fit` said "fits, 3.8 GiB spare", and llama-server then died
    // with `cudaMalloc failed: out of memory` allocating 7312 MiB of KV.

    #[test]
    fn a_reading_can_never_say_more_is_free_than_the_device_has() {
        // The gate's numbers, exactly.
        let rocm = VramReading::observed(11_397, 17_139);
        assert_eq!(rocm.free_mb(), 11_397, "free is clamped to total");
        assert_eq!(rocm.total_mb(), 11_397);
        assert!(rocm.was_clamped());
        assert_eq!(
            rocm.reported_free_mb(),
            17_139,
            "the driver's own figure is kept, so the correction can be reported"
        );

        // A coherent reading is left completely alone.
        let vulkan = VramReading::observed(20_992, 20_363);
        assert_eq!((vulkan.total_mb(), vulkan.free_mb()), (20_992, 20_363));
        assert!(!vulkan.was_clamped());
        assert_eq!(vulkan.reported_free_mb(), 20_363);

        // And the invariant holds for every pair a driver could print, which is the point of
        // there being exactly one constructor.
        for (total, free) in [
            (0u64, 0u64),
            (0, u64::MAX),
            (11_397, 12_821),
            (11_397, 17_139),
            (u64::MAX, 0),
            (7, 7),
            (81_559, 80_000),
        ] {
            let r = VramReading::observed(total, free);
            assert!(r.free_mb() <= r.total_mb(), "{total} / {free}");
            assert_eq!(r.total_mb(), total, "a total is never invented");
            assert_eq!(r.free_mb(), free.min(total));
        }
    }

    #[test]
    fn a_device_that_reports_more_free_than_total_is_clamped_at_ingest() {
        let id = BuildId::parse("build").unwrap();
        // The live line from `~/llama.cpp/build/bin/llama-server --list-devices`.
        let gpus = parse_devices(
            "Available devices:\n  ROCm0: AMD Radeon 840M Graphics (11397 MiB, 17139 MiB free)\n",
            &id,
        );
        assert_eq!(gpus.len(), 1, "{gpus:?}");
        assert_eq!(gpus[0].backend, GpuBackend::Rocm);
        assert_eq!(gpus[0].vram_total_mb, 11_397);
        assert_eq!(
            gpus[0].vram_free_mb, 11_397,
            "no consumer may ever see the 17139 MiB"
        );
        // The honesty types downstream can now answer at all, and answer sanely.
        assert!(!gpus[0].reports_gtt_overcommit());
        assert_eq!(gpus[0].vram_used_mb(), Some(0));
        assert!(gpus[0].vram_free_mb <= gpus[0].vram_total_mb);
    }

    // -- flags ------------------------------------------------------------

    #[test]
    fn help_parsing_reads_the_tristate_and_the_jinja_default_off_the_text() {
        let help = "\
----- common params -----

-h,    --help, --usage                  print usage and exit
-t,    --threads N                      number of CPU threads (default: -1)
                                        (env: LLAMA_ARG_THREADS)
-Cr,   --cpu-range lo-hi                range of CPUs for affinity
--rope-scaling {none,linear,yarn}       RoPE frequency scaling method
-fa,   --flash-attn [on|off|auto]       set Flash Attention use (default: 'auto')
--jinja, --no-jinja                     whether to use jinja template engine for chat
                                        (default: enabled)
-fit,  --fit [on|off]                   whether to adjust unset arguments
--models-dir PATH                       directory containing models for the router server
";
        let f = parse_help(help);
        assert!(f.has("-h") && f.has("--help") && f.has("--usage"));
        assert!(f.has("-t") && f.has("--threads"));
        assert!(f.has("--rope-scaling") && f.has("-Cr") && f.has("--cpu-range"));
        assert!(f.fa_tristate);
        assert!(
            f.jinja_default_on,
            "the default is on the continuation line"
        );
        assert!(f.has_fit);
        assert!(f.has_router_mode);
        assert_eq!(f.help_lines, help.lines().count() as u32);
        // A section rule, an argument placeholder and a range are not flags.
        assert!(!f.has("-----"));
        assert!(!f.has("lo-hi"));
        assert!(!f.has("N"));
        assert!(!f.has("{none,linear,yarn}"));
        assert!(!f.has("-1"));
    }

    #[test]
    fn a_disabled_jinja_default_is_read_as_disabled() {
        let f = parse_help("--jinja, --no-jinja   use jinja (default: disabled)\n");
        assert!(!f.jinja_default_on);
        let f = parse_help("--jinja   use jinja\n");
        assert!(
            !f.jinja_default_on,
            "no --no-jinja means no default-on claim"
        );
        let f = parse_help("--no-jinja   disable jinja\n");
        assert!(f.jinja_default_on, "the negative form implies the default");
    }

    #[tokio::test]
    async fn probe_flags_cache_key_is_path_mtime_size() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let calls = dir.path().join("calls");
        let server = dir.path().join("build-fake/bin/llama-server");
        let body = |extra: &str| {
            format!(
                "#!/bin/sh\nprintf 'x\\n' >> {calls}\nprintf -- '-m,    --model FNAME   model path\\n'\n{extra}",
                calls = calls.display()
            )
        };
        write_exec(&server, &body(""));

        let first = probe_flags(&server, &cache).await.unwrap();
        assert!(first.has("-m") && first.has("--model"));
        let second = probe_flags(&server, &cache).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(
            std::fs::read_to_string(&calls).unwrap().lines().count(),
            1,
            "the second call must be served from cache"
        );

        // Same path, new content: size and mtime both change, so the cache misses.
        write_exec(&server, &body("printf -- '--brand-new-flag   new\\n'\n"));
        let third = probe_flags(&server, &cache).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(&calls).unwrap().lines().count(),
            2,
            "a rebuilt binary must invalidate its own cache entry"
        );
        assert!(third.has("--brand-new-flag"));

        // The entry on disk carries all three components of the key.
        let entry: FlagCache =
            serde_json::from_slice(&std::fs::read(flag_cache_path(&cache, &server)).unwrap())
                .unwrap();
        let (mtime, size) = stat_key(&server).unwrap();
        assert_eq!(entry.path, server.display().to_string());
        assert_eq!(entry.mtime_ns, mtime);
        assert_eq!(entry.size, size);
    }

    #[tokio::test]
    async fn a_cache_entry_naming_another_path_is_never_reused() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let server = dir.path().join("a/bin/llama-server");
        write_exec(&server, "#!/bin/sh\nprintf -- '-m FNAME  model\\n'\n");
        probe_flags(&server, &cache).await.unwrap();

        // Forge an entry under this binary's cache filename that names a different path.
        let entry_path = flag_cache_path(&cache, &server);
        let mut forged: FlagCache =
            serde_json::from_slice(&std::fs::read(&entry_path).unwrap()).unwrap();
        forged.path = "/somewhere/else/llama-server".to_owned();
        forged.flags.flags.insert("--forged".to_owned());
        std::fs::write(&entry_path, serde_json::to_vec(&forged).unwrap()).unwrap();

        let flags = probe_flags(&server, &cache).await.unwrap();
        assert!(!flags.has("--forged"));
    }

    // -- choose_build -----------------------------------------------------

    #[test]
    fn choose_build_reports_a_fallback_instead_of_substituting() {
        let builds = vec![
            a_build("build-hip", vec![GpuBackend::Hip], vec!["ROCm0"]),
            a_build("build-cpu", vec![GpuBackend::Cpu], vec![]),
        ];
        let c = choose_build(&builds, Some(GpuBackend::Vulkan)).unwrap();
        assert!(!c.exact, "a substitution must be visible");
        assert_eq!(c.chosen.as_str(), "build-hip");
        assert_eq!(c.wanted, Some(GpuBackend::Vulkan));
        assert_eq!(c.got, Some(GpuBackend::Hip));
    }

    #[test]
    fn choose_build_is_exact_when_the_backend_is_present() {
        let builds = vec![
            a_build("build-hip", vec![GpuBackend::Hip], vec!["ROCm0"]),
            a_build(
                "build-vulkan",
                vec![GpuBackend::Vulkan],
                vec!["Vulkan0", "Vulkan1"],
            ),
        ];
        let c = choose_build(&builds, Some(GpuBackend::Vulkan)).unwrap();
        assert!(c.exact);
        assert_eq!(c.chosen.as_str(), "build-vulkan");
        assert_eq!(c.wanted, Some(GpuBackend::Vulkan));
        assert_eq!(c.got, Some(GpuBackend::Vulkan));
    }

    #[test]
    fn choose_build_prefers_an_accelerator_and_is_deterministic() {
        let builds = vec![
            a_build("build-cpu", vec![GpuBackend::Cpu], vec![]),
            a_build("build-zzz", vec![GpuBackend::Vulkan], vec!["Vulkan0"]),
            a_build("build-aaa", vec![GpuBackend::Vulkan], vec!["Vulkan0"]),
        ];
        let c = choose_build(&builds, None).unwrap();
        assert!(
            c.exact,
            "nothing specific was asked for, nothing substituted"
        );
        assert_eq!(c.chosen.as_str(), "build-aaa");
        assert_eq!(c.wanted, None);
        assert_eq!(c.got, Some(GpuBackend::Vulkan));

        // Reordering the input cannot change the answer.
        let mut reversed = builds.clone();
        reversed.reverse();
        assert_eq!(choose_build(&reversed, None), Some(c));
    }

    /// The rig this laptop really is, and the launch the plan opens with:
    /// `apexrouter up <model> --alias auto`, no `--devices`, so `want` is `None`.
    ///
    /// Five builds, two of which see the same single card through ROCm and two through
    /// Vulkan. The old tiebreak was the directory name, so `build` — ROCm, and the one that
    /// OOMs — won because `"build"` sorts before `"build-vulkan"`.
    #[test]
    fn the_default_choice_is_not_decided_by_the_directory_name() {
        let builds = vec![
            a_build("build", vec![GpuBackend::Rocm], vec!["ROCm0"]),
            a_build("build-mtp", vec![GpuBackend::Rocm], vec!["ROCm0"]),
            // Installed and broken: enumerates nothing, so it is never a default.
            a_build("build-rocm", vec![], vec![]),
            a_build("build-vulkan", vec![GpuBackend::Vulkan], vec!["Vulkan0"]),
            a_build("build-zaya1", vec![GpuBackend::Vulkan], vec!["Vulkan0"]),
        ];
        let c = choose_build(&builds, None).expect("a choice");
        assert_eq!(c.chosen.as_str(), "build-vulkan", "{c:?}");
        assert_eq!(c.got, Some(GpuBackend::Vulkan));
        assert!(c.exact, "nothing was asked for, so nothing was substituted");

        // Still order-independent: the answer is a ranking, not an accident of the glob.
        let mut reversed = builds.clone();
        reversed.reverse();
        assert_eq!(choose_build(&reversed, None), Some(c));

        // And asking for the specialist still gets exactly the specialist.
        let rocm = choose_build(&builds, Some(GpuBackend::Rocm)).expect("a choice");
        assert!(rocm.exact);
        assert_eq!(rocm.chosen.as_str(), "build", "{rocm:?}");
        assert_eq!(rocm.got, Some(GpuBackend::Rocm));
    }

    #[test]
    fn the_default_prefers_the_vendor_path_where_the_vendor_ships_one() {
        // An NVIDIA box: CUDA is both the fast path and the safe one.
        let nvidia = vec![
            a_build("build-vulkan", vec![GpuBackend::Vulkan], vec!["Vulkan0"]),
            a_build("build-cuda", vec![GpuBackend::Cuda], vec!["CUDA0"]),
        ];
        assert_eq!(
            choose_build(&nvidia, None)
                .expect("a choice")
                .chosen
                .as_str(),
            "build-cuda"
        );

        // A box where the only accelerator is the narrow one: it is still chosen, because a
        // default that refuses to run is not a default.
        let rocm_only = vec![
            a_build("build-cpu", vec![GpuBackend::Cpu], vec!["CPU0"]),
            a_build("build-hip", vec![GpuBackend::Hip], vec!["ROCm0"]),
        ];
        let c = choose_build(&rocm_only, None).expect("a choice");
        assert_eq!(c.chosen.as_str(), "build-hip");
        assert_eq!(c.got, Some(GpuBackend::Hip));

        // Within one backend the richer build still wins, and a software rasteriser in the
        // token list cannot buy a weaker backend the default.
        let same_backend = vec![
            a_build("build-a", vec![GpuBackend::Cuda], vec!["CUDA0"]),
            a_build("build-b", vec![GpuBackend::Cuda], vec!["CUDA0", "CUDA1"]),
        ];
        assert_eq!(
            choose_build(&same_backend, None)
                .expect("a choice")
                .chosen
                .as_str(),
            "build-b"
        );
        let padded = vec![
            a_build("build-cuda", vec![GpuBackend::Cuda], vec!["CUDA0"]),
            a_build(
                "build-vulkan",
                vec![GpuBackend::Vulkan],
                vec!["Vulkan0", "Vulkan1"],
            ),
        ];
        assert_eq!(
            choose_build(&padded, None)
                .expect("a choice")
                .chosen
                .as_str(),
            "build-cuda"
        );
    }

    #[test]
    fn choose_build_on_an_empty_machine_is_none() {
        assert_eq!(choose_build(&[], None), None);
        assert_eq!(choose_build(&[], Some(GpuBackend::Vulkan)), None);
    }

    // -- naming -----------------------------------------------------------

    #[test]
    fn build_labels_come_from_the_build_directory() {
        assert_eq!(
            build_label(Path::new("/home/a/llama.cpp/build-mtp/bin/llama-server")),
            "build-mtp"
        );
        assert_eq!(
            build_label(Path::new("/home/a/llama.cpp/build-zaya1/llama-server")),
            "build-zaya1"
        );
        assert_eq!(
            build_label(Path::new("/usr/local/bin/llama-server")),
            "local"
        );
        assert_eq!(slugify("Build Vulkan!"), "build-vulkan-");
        assert_eq!(slugify("---"), "llama-server");
        assert_eq!(
            build_id_for(Path::new("/x/build-vulkan/bin/llama-server"))
                .unwrap()
                .as_str(),
            "build-vulkan"
        );
    }

    #[test]
    fn duplicate_directory_names_get_distinct_ids() {
        let mut used = HashSet::new();
        let mut next = || unique_id("build-vulkan", &mut used).unwrap();
        assert_eq!(next().as_str(), "build-vulkan");
        assert_eq!(next().as_str(), "build-vulkan-2");
        assert_eq!(next().as_str(), "build-vulkan-3");
    }

    #[test]
    fn build_info_is_normalised() {
        assert_eq!(
            parse_build_info("version: 9199 (39cf5d619)\nbuilt with GNU 15.2.0\n").as_deref(),
            Some("b9199 (39cf5d619)")
        );
        assert_eq!(
            parse_build_info("version: b9199 (39cf5d619)\n").as_deref(),
            Some("b9199 (39cf5d619)")
        );
        assert_eq!(parse_build_info("no version here\n"), None);
    }

    // -- the real machine -------------------------------------------------
    //
    // The acceptance tests for unit C-09. They are skipped, not failed, on a box without
    // llama.cpp: CI is not the rig.

    #[tokio::test]
    async fn real_machine_finds_every_build_including_mtp_and_zaya1() {
        let Some(root) = llama_cpp_root() else {
            eprintln!("SKIP: no ~/llama.cpp on this box");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let builds = discover_builds(&cfg_with_root(&root), &dir.path().join("cache"))
            .await
            .unwrap();
        let ids: Vec<&str> = builds.iter().map(|b| b.id.as_str()).collect();

        for expected in [
            "build",
            "build-mtp",
            "build-rocm",
            "build-vulkan",
            "build-zaya1",
        ] {
            assert!(ids.contains(&expected), "missing {expected}: {ids:?}");
        }
        assert!(ids.len() >= 5, "{ids:?}");
        for b in &builds {
            assert!(b.server_path.ends_with("/bin/llama-server"), "{b:?}");
            assert_eq!(b.label, b.id.as_str());
        }
    }

    #[tokio::test]
    async fn real_machine_backend_comes_from_list_devices_not_from_the_help_text() {
        let Some(root) = llama_cpp_root() else {
            eprintln!("SKIP: no ~/llama.cpp on this box");
            return;
        };
        let server = root.join("build-vulkan/bin/llama-server");
        if !server.is_file() {
            return;
        }

        // The premise: `--help` says nothing at all about which backend this build is.
        let help = exec::run(&server, &["--help"], HELP_TIMEOUT).await.unwrap();
        let text = format!("{}{}", help.stdout, help.stderr).to_ascii_lowercase();
        assert!(text.lines().count() > 100, "help was truncated");
        for word in ["vulkan", "cuda", "hip", "rocm"] {
            assert!(
                !text.contains(word),
                "--help mentions {word}; backend detection must not grep it"
            );
        }

        // …and `--list-devices` says exactly which backend it is.
        let gpus = probe_devices(&server).await.unwrap();
        assert!(
            gpus.iter().any(|g| g.backend == GpuBackend::Vulkan),
            "{gpus:?}"
        );
        assert!(gpus.iter().all(|g| !g.device.is_empty()));

        let dir = tempfile::tempdir().unwrap();
        let flags = probe_flags(&server, &dir.path().join("cache"))
            .await
            .unwrap();
        assert!(flags.has("--model") && flags.has("--port") && flags.has("-fa"));
        assert!(flags.help_lines > 100);
    }

    /// The default launch, on whatever machine this is: when the box offers a build on a
    /// backend whose enumeration promises more, the default must not be one that promises
    /// less. Phrased over [`backend_confidence`] rather than over a backend name, so it says
    /// the same thing on a CUDA box, an Apple box, and this laptop — where it fails on the
    /// pre-fix ranking, which picked `~/llama.cpp/build` (ROCm) over `build-vulkan`.
    #[tokio::test]
    async fn real_machine_default_build_is_not_the_late_failing_backend() {
        let Some(root) = llama_cpp_root() else {
            eprintln!("SKIP: no ~/llama.cpp on this box");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let builds = discover_builds(&cfg_with_root(&root), &dir.path().join("cache"))
            .await
            .unwrap();
        let Some(choice) = choose_build(&builds, None) else {
            eprintln!("SKIP: no builds on this box");
            return;
        };
        let best_available = builds
            .iter()
            .flat_map(|b| b.backends.iter())
            .map(backend_confidence)
            .max()
            .unwrap_or(0);
        let chosen = builds.iter().find(|b| b.id == choice.chosen).unwrap();
        let got = chosen
            .backends
            .iter()
            .map(backend_confidence)
            .max()
            .unwrap_or(0);
        eprintln!(
            "\n=== default build on this machine ===\n  chosen={} backends={:?} confidence={got} (best available {best_available})",
            choice.chosen, chosen.backends
        );
        assert_eq!(
            got, best_available,
            "the default launch must use the backend whose enumeration promises most; \
             chose {} ({:?})",
            choice.chosen, chosen.backends
        );
    }

    #[tokio::test]
    async fn real_machine_choose_build_picks_a_vulkan_build_for_vulkan() {
        let Some(root) = llama_cpp_root() else {
            eprintln!("SKIP: no ~/llama.cpp on this box");
            return;
        };
        if !root.join("build-vulkan/bin/llama-server").is_file() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let builds = discover_builds(&cfg_with_root(&root), &dir.path().join("cache"))
            .await
            .unwrap();
        let c = choose_build(&builds, Some(GpuBackend::Vulkan)).unwrap();
        assert!(c.exact, "{c:?}");
        assert_eq!(c.got, Some(GpuBackend::Vulkan));
        let chosen = builds.iter().find(|b| b.id == c.chosen).unwrap();
        assert!(chosen.backends.contains(&GpuBackend::Vulkan), "{chosen:?}");
    }
}
