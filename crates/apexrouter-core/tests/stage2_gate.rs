//! Stage 2 gate: the capability layer meets the real machine.
//!
//! Nine agents wrote `discover`, `gguf`, `fit`, `argv`, `upstream`, `usage`, `pricing`,
//! `catalog`, `migrate` and `checks` in parallel, each against a published signature. Every
//! assertion in this file spans a **seam** between two of them, or between one of them and
//! the physical box — the two things no single unit could check alone:
//!
//! * `discover` → `fit` → `argv` is the whole local launch path. A `LlamaBuild` that
//!   discovery produced, a `GgufMeta` that the header reader produced and a live VRAM
//!   budget go in; a command line goes out. If any unit's idea of the shared types drifted,
//!   it shows up here and nowhere else.
//! * Feature detection is only real if two builds on the same box disagree. This machine
//!   carries four working `llama-server`s at b8850, b9081, b9199 and b9219, so the flag sets
//!   genuinely differ and a hardcoded whitelist would be caught.
//! * The legacy reader is pointed at Andre's **real** `~/.vastai-gguf`. `migrate::plan` is
//!   documented to write nothing; here that is proven against live state that must survive.
//!
//! **Nothing here is allowed to modify the machine.** The legacy tree is hashed before and
//! after, the build-flag cache is redirected into a `tempdir`, and no test calls
//! `ensure_layout`, `Config::save`, `catalog::save` or `migrate::apply`.
//!
//! Every test degrades to a printed `SKIP` on a box without `~/llama.cpp`, `~/models` or
//! `~/.vastai-gguf`, so CI stays green off this laptop. Run with `--nocapture` to read the
//! numbers; they are the gate report.

use apexrouter_core::config::{CompatCfg, Config, EndpointsCfg};
use apexrouter_core::discover::{
    choose_build, discover_builds, discover_models, probe_devices, read_gguf_meta,
};
use apexrouter_core::protocol::{BuildId, LocalLlamaSpec, MigrationAction};
use apexrouter_core::protocol::{
    ContainerRuntime, FitInput, FitVerdict, GgufMeta, GpuBackend, KvType, LlamaBuild, LocalModel,
    NglPlan, RigSnapshot, SamplingMode, SplitPlan, TriState, VramBudget,
};
use apexrouter_core::{argv, fit, migrate, usage, Paths};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------------------

/// `$HOME`, or `None` on a machine that does not have one.
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// `~/llama.cpp` when it exists. Absent on CI, which is why every caller skips.
fn llama_root() -> Option<PathBuf> {
    let p = home()?.join("llama.cpp");
    p.is_dir().then_some(p)
}

/// `~/models` when it exists.
fn models_root() -> Option<PathBuf> {
    let p = home()?.join("models");
    p.is_dir().then_some(p)
}

/// The one real weight file this box carries, when it is present.
fn carnice() -> Option<PathBuf> {
    let p = models_root()?.join("carnice-9b/Carnice-9b-Q6_K.gguf");
    p.is_file().then_some(p)
}

/// An [`EndpointsCfg`] pointed at exactly one root, so a test asserts about a known tree.
fn cfg_with(model_roots: &[&str], build_roots: &[&str]) -> EndpointsCfg {
    EndpointsCfg {
        model_roots: model_roots.iter().map(|s| (*s).to_owned()).collect(),
        build_roots: build_roots.iter().map(|s| (*s).to_owned()).collect(),
        ..EndpointsCfg::default()
    }
}

/// Discover every build on this box, caching flags into a throwaway directory.
///
/// The cache is a `tempdir` rather than `Paths::cache()` on purpose: a gate run must not
/// warm, poison or invalidate the user's real cache.
async fn real_builds(tmp: &Path) -> Vec<LlamaBuild> {
    let cfg = cfg_with(&[], &["~/llama.cpp"]);
    discover_builds(&cfg, tmp)
        .await
        .expect("discover_builds on the real machine")
}

/// Find one build by its id, e.g. `"build-vulkan"`.
fn by_id<'a>(builds: &'a [LlamaBuild], id: &str) -> Option<&'a LlamaBuild> {
    builds.iter().find(|b| b.id.as_str() == id)
}

/// `MemAvailable` from `/proc/meminfo`, MiB. 0 when it cannot be read.
///
/// Deliberately not `sysinfo`: this number ends up in a `VramBudget` that decides whether a
/// 7 GiB model is allowed to launch, and reading the kernel's own answer keeps the gate
/// honest about what it measured.
fn mem_available_mb() -> u64 {
    let Ok(txt) = std::fs::read_to_string("/proc/meminfo") else {
        return 0;
    };
    for line in txt.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            let kb: u64 = rest
                .split_whitespace()
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            return kb / 1024;
        }
    }
    0
}

/// A content hash of every file under `dir`, keyed by relative path.
///
/// Used to prove a dry run touched nothing. Contents, sizes and the *set* of paths are all
/// covered; mtimes deliberately are not, because reading a file is allowed to update atime
/// and some filesystems fold that into ctime.
fn tree_digest(dir: &Path) -> Vec<(String, u64, [u8; 32])> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<(String, u64, [u8; 32])>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                walk(&path, base, out);
            } else if meta.is_file() {
                let bytes = std::fs::read(&path).unwrap_or_default();
                let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
                sha2::Digest::update(&mut hasher, &bytes);
                let digest: [u8; 32] = sha2::Digest::finalize(hasher).into();
                let rel = path
                    .strip_prefix(base)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .into_owned();
                out.push((rel, meta.len(), digest));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

// ---------------------------------------------------------------------------------------
// 1. build discovery
// ---------------------------------------------------------------------------------------

/// Every `build*/bin/llama-server` under `~/llama.cpp` is found, labelled by its build dir,
/// and a binary that cannot even start is reported rather than dropped.
#[tokio::test(flavor = "multi_thread")]
async fn real_machine_discovers_all_five_builds() {
    if llama_root().is_none() {
        eprintln!("SKIP: no ~/llama.cpp on this box");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let builds = real_builds(tmp.path()).await;

    eprintln!("\n=== builds discovered ({}) ===", builds.len());
    for b in &builds {
        eprintln!(
            "  {:<12} {:<20} backends={:<20} devices={:<12} flags={:<4} help_lines={} fa_tristate={} jinja_default_on={} has_fit={}",
            b.id.as_str(),
            b.build_info.as_deref().unwrap_or("(no --version)"),
            format!("{:?}", b.backends),
            format!("{:?}", b.devices),
            b.flags.flags.len(),
            b.flags.help_lines,
            b.flags.fa_tristate,
            b.flags.jinja_default_on,
            b.flags.has_fit,
        );
        eprintln!("               {}", b.server_path);
    }

    for want in [
        "build",
        "build-vulkan",
        "build-rocm",
        "build-mtp",
        "build-zaya1",
    ] {
        let b = by_id(&builds, want).unwrap_or_else(|| panic!("{want} was not discovered"));
        assert!(
            Path::new(&b.server_path).is_file(),
            "{want}: server_path must be an existing file, got {}",
            b.server_path
        );
        assert!(
            b.server_path.ends_with("/bin/llama-server"),
            "{want}: expected a bin/llama-server path, got {}",
            b.server_path
        );
    }
    assert!(
        builds.len() >= 5,
        "expected at least the five known builds, got {}",
        builds.len()
    );

    // A binary whose shared libraries are missing must still be listed, with empty probe
    // results, so a UI can show it as broken instead of pretending it does not exist.
    let rocm = by_id(&builds, "build-rocm").expect("build-rocm");
    assert!(
        rocm.backends.is_empty() && rocm.devices.is_empty(),
        "build-rocm cannot load libhipblas.so.3 on this box, so it must probe to nothing, got \
         backends={:?} devices={:?}",
        rocm.backends,
        rocm.devices
    );
}

/// The backend label comes from `--list-devices`, and the four builds that run disagree
/// about their flags — which is what makes feature detection load-bearing rather than
/// decorative.
#[tokio::test(flavor = "multi_thread")]
async fn real_machine_flags_are_probed_per_build_not_hardcoded() {
    if llama_root().is_none() {
        eprintln!("SKIP: no ~/llama.cpp on this box");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let builds = real_builds(tmp.path()).await;

    // Liveness is "the probe learned some flags", NOT `help_lines > 0`: a binary the dynamic
    // loader rejects still emits one line ("error while loading shared libraries: …"), so
    // build-rocm comes back with help_lines = 1 and an empty flag set. Any later unit that
    // wants "can this build run?" must ask `flags.flags.is_empty()`, not `help_lines`.
    let working: Vec<&LlamaBuild> = builds
        .iter()
        .filter(|b| !b.flags.flags.is_empty())
        .collect();
    assert!(
        working.len() >= 4,
        "expected four launchable builds, got {}",
        working.len()
    );
    let broken = by_id(&builds, "build-rocm").expect("build-rocm");
    assert!(
        broken.flags.flags.is_empty(),
        "a binary that cannot load its shared libraries learns no flags"
    );
    eprintln!(
        "\n=== a broken binary is reported, not dropped ===\n  build-rocm: flags={} help_lines={} build_info={:?}",
        broken.flags.flags.len(),
        broken.flags.help_lines,
        broken.build_info,
    );

    // Every working build must advertise the flags the argv builder emits unconditionally.
    for b in &working {
        for flag in ["-m", "--host", "--port", "-a", "-c", "-ngl", "-fa", "-sm"] {
            assert!(
                b.flags.has(flag),
                "{}: expected {flag} in the probed flag set",
                b.id
            );
        }
        assert!(b.flags.fa_tristate, "{}: -fa is [on|off|auto] here", b.id);
        assert!(
            b.flags.jinja_default_on,
            "{}: --jinja is default-enabled here",
            b.id
        );
        assert!(b.flags.has_fit, "{}: this build has --fit", b.id);
    }

    // The point of the whole exercise: two builds on ONE box do not agree.
    let old = by_id(&builds, "build").expect("build");
    let new = by_id(&builds, "build-vulkan").expect("build-vulkan");
    let only_new: BTreeSet<_> = new.flags.flags.difference(&old.flags.flags).collect();
    let only_old: BTreeSet<_> = old.flags.flags.difference(&new.flags.flags).collect();
    eprintln!(
        "\n=== feature detection is real ===\n  build       {} help lines, {} flags ({})\n  build-vulkan {} help lines, {} flags ({})",
        old.flags.help_lines,
        old.flags.flags.len(),
        old.build_info.as_deref().unwrap_or("?"),
        new.flags.help_lines,
        new.flags.flags.len(),
        new.build_info.as_deref().unwrap_or("?"),
    );
    eprintln!("  only in build-vulkan: {only_new:?}");
    eprintln!("  only in build:        {only_old:?}");
    assert_ne!(
        old.flags.flags, new.flags.flags,
        "two builds five hundred commits apart must not probe to the same flag set — that \
         would mean the flag list is compiled in"
    );

    // Backends are enumerated, never grepped out of help text.
    assert!(
        new.backends.contains(&GpuBackend::Vulkan),
        "build-vulkan enumerates Vulkan, got {:?}",
        new.backends
    );
    let mtp = by_id(&builds, "build-mtp").expect("build-mtp");
    assert!(
        mtp.backends.contains(&GpuBackend::Rocm) || mtp.backends.contains(&GpuBackend::Hip),
        "build-mtp enumerates ROCm on this box, got {:?}",
        mtp.backends
    );

    // choose_build must report a substitution rather than perform one silently.
    let want_cuda = choose_build(&builds, Some(GpuBackend::Cuda)).expect("a fallback choice");
    eprintln!(
        "\n=== choose_build(Cuda) on a machine with no CUDA ===\n  chosen={} exact={} wanted={:?} got={:?}",
        want_cuda.chosen, want_cuda.exact, want_cuda.wanted, want_cuda.got
    );
    assert!(!want_cuda.exact, "no CUDA here, so exact must be false");
    assert_eq!(want_cuda.wanted, Some(GpuBackend::Cuda));
    assert!(want_cuda.got.is_some(), "got must name what we settled for");
}

// ---------------------------------------------------------------------------------------
// 2. device discovery
// ---------------------------------------------------------------------------------------

/// `llama-server --list-devices` is parsed into `Gpu` rows, and a software rasteriser — if
/// this box still enumerates one — is marked so default selection drops it.
#[tokio::test(flavor = "multi_thread")]
async fn real_machine_enumerates_devices_and_marks_software_ones() {
    let Some(root) = llama_root() else {
        eprintln!("SKIP: no ~/llama.cpp on this box");
        return;
    };
    let vulkan = root.join("build-vulkan/bin/llama-server");
    if !vulkan.is_file() {
        eprintln!("SKIP: no build-vulkan/bin/llama-server");
        return;
    }

    let gpus = probe_devices(&vulkan).await.expect("probe_devices");
    eprintln!("\n=== devices, via build-vulkan --list-devices ===");
    for g in &gpus {
        eprintln!(
            "  {:<10} idx={} backend={:?} total={} MiB free={} MiB is_software={} name={:?}",
            g.device, g.index, g.backend, g.vram_total_mb, g.vram_free_mb, g.is_software, g.name
        );
    }

    let v0 = gpus
        .iter()
        .find(|g| g.device == "Vulkan0")
        .expect("Vulkan0 must be enumerated on this box");
    assert_eq!(v0.backend, GpuBackend::Vulkan);
    assert!(!v0.is_software, "the 840M is a real GPU");
    assert_eq!(
        v0.vram_total_mb, 20_992,
        "the 840M reports a 20992 MiB shared pool"
    );
    assert!(
        v0.vram_free_mb > 8_000,
        "free VRAM looked implausible: {} MiB",
        v0.vram_free_mb
    );
    assert!(
        v0.name.contains("AMD") || v0.name.contains("Radeon"),
        "unexpected device name {:?}",
        v0.name
    );

    // The invariant, whether or not this box currently enumerates llvmpipe: anything named
    // like a rasteriser is flagged, and the default selection filter drops exactly those.
    for g in &gpus {
        let looks_software = g.name.to_ascii_lowercase().contains("llvmpipe")
            || g.name.to_ascii_lowercase().contains("swiftshader")
            || g.name.to_ascii_lowercase().contains("lavapipe");
        assert_eq!(
            g.is_software, looks_software,
            "{} ({}) is_software must follow the device name",
            g.device, g.name
        );
    }
    let selectable: Vec<&str> = gpus
        .iter()
        .filter(|g| !g.is_software)
        .map(|g| g.device.as_str())
        .collect();
    eprintln!("  default selection: {selectable:?}");
    assert!(selectable.contains(&"Vulkan0"));

    // The ROCm-capable build enumerates a different backend from the same physical GPU.
    let plain = root.join("build/bin/llama-server");
    if plain.is_file() {
        let rocm = probe_devices(&plain).await.expect("probe_devices on build");
        eprintln!("=== devices, via build --list-devices ===");
        for g in &rocm {
            eprintln!(
                "  {:<10} backend={:?} total={} MiB free={} MiB",
                g.device, g.backend, g.vram_total_mb, g.vram_free_mb
            );
        }
        assert!(
            rocm.iter().any(|g| g.backend == GpuBackend::Rocm),
            "~/llama.cpp/build is a ROCm build on this machine, got {:?}",
            rocm.iter().map(|g| &g.backend).collect::<Vec<_>>()
        );
    }
}

/// **The GTT underflow, made impossible rather than merely avoided.**
///
/// ROCm on this box reports `free` (12821 MiB) greater than `total` (11397 MiB), so
/// `total - free` is an underflowed `u64` the size of the universe rather than a small
/// number. `Gpu::vram_used_mb() -> Option<u64>` is the one sanctioned way to ask, and it
/// returns `None` on that reading — a type that cannot express the lie.
///
/// This test is the enforcement: **no production line anywhere in the workspace may put
/// those two fields on either side of a subtraction.** It scans every `crates/*/src/**.rs`
/// down to its `#[cfg(test)]` boundary, and the only permitted hit is the `checked_sub`
/// inside `vram_used_mb` itself.
#[test]
fn nothing_in_the_workspace_subtracts_free_vram_from_total_vram() {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .join("crates");
    let mut files = Vec::new();
    walk(&root, &mut files);
    assert!(files.len() > 20, "the scan found nothing to scan");

    let mut offenders: Vec<String> = Vec::new();
    let mut sanctioned = 0usize;
    for file in &files {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        // Test modules live at the end of every file in this codebase and are allowed to
        // assert about the trap; production code is everything above the boundary.
        let production = match text.find("#[cfg(test)]") {
            Some(at) => &text[..at],
            None => &text[..],
        };
        for (n, line) in production.lines().enumerate() {
            if !(line.contains("vram_total_mb") && line.contains("vram_free_mb")) {
                continue;
            }
            if !(line.contains('-') || line.contains("sub")) {
                continue; // a comparison or a struct literal, not arithmetic
            }
            if line.contains("checked_sub") {
                sanctioned += 1;
                continue;
            }
            offenders.push(format!("{}:{}: {}", file.display(), n + 1, line.trim()));
        }
    }
    assert_eq!(
        sanctioned, 1,
        "exactly one sanctioned subtraction — `Gpu::vram_used_mb` — was expected"
    );
    assert!(
        offenders.is_empty(),
        "`total - free` underflows on this machine's ROCm device; use `Gpu::vram_used_mb()`:\n{}",
        offenders.join("\n")
    );
}

// ---------------------------------------------------------------------------------------
// 3. model discovery + the GGUF header
// ---------------------------------------------------------------------------------------

/// `~/models` is walked recursively, `.cache` is pruned, and the one real GGUF on the box is
/// described correctly — size, shard count, quant label and header.
#[tokio::test(flavor = "multi_thread")]
async fn real_machine_finds_carnice_and_reads_its_header() {
    let Some(root) = models_root() else {
        eprintln!("SKIP: no ~/models on this box");
        return;
    };
    let Some(gguf_path) = carnice() else {
        eprintln!("SKIP: ~/models/carnice-9b/Carnice-9b-Q6_K.gguf is not present");
        return;
    };

    let cfg = cfg_with(&["~/models"], &[]);
    let models = discover_models(&cfg).await.expect("discover_models");

    eprintln!("\n=== models under {} ===", root.display());
    for m in &models {
        eprintln!(
            "  {:<24} {:<10} {} shard(s) {} bytes ({:.2} GiB) mmproj={} arch={:?}",
            m.id,
            m.quant.as_deref().unwrap_or("-"),
            m.shards.len(),
            m.total_bytes,
            m.total_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
            m.mmproj.len(),
            m.gguf.as_ref().map(|g| g.arch.as_str()),
        );
        for s in &m.shards {
            eprintln!("      {} ({} bytes)", s.path, s.bytes);
        }
    }

    assert_eq!(
        models.len(),
        1,
        "this box has exactly one complete model; got {:?}",
        models.iter().map(|m| &m.id).collect::<Vec<_>>()
    );
    let m = &models[0];
    assert_eq!(m.shards.len(), 1, "Carnice is a single-file GGUF");
    assert_eq!(
        m.total_bytes, 7_359_259_424,
        "Carnice-9b-Q6_K.gguf is 7359259424 bytes on disk"
    );
    assert_eq!(
        m.primary_path(),
        Some(gguf_path.to_string_lossy().as_ref()),
        "-m must point at the real file"
    );
    assert_eq!(
        m.quant.as_deref(),
        Some("Q6_K"),
        "the quant token comes off the filename"
    );
    assert!(!m.is_vision(), "no mmproj sits beside Carnice");

    // Nothing may come out of a HuggingFace download cache, and the incomplete 2.8 GiB
    // Qwen3.6 download must not masquerade as a model.
    for m in &models {
        for s in &m.shards {
            assert!(
                !s.path.contains("/.cache/"),
                "a .cache path leaked into discovery: {}",
                s.path
            );
        }
    }
    assert!(
        !models.iter().any(|m| m.dir.contains("qwen36-35b-a3b")),
        "the incomplete qwen36-35b-a3b download must not appear as a model"
    );

    // The header the fit solver depends on.
    let meta = read_gguf_meta(&gguf_path).expect("read_gguf_meta");
    eprintln!("\n=== GGUF header ===\n  {meta:?}");
    assert_eq!(meta.arch, "qwen35");
    assert_eq!(meta.n_layer, 32);
    assert_eq!(meta.n_head_kv, 4);
    assert_eq!(meta.n_embd_head_k, 256);
    assert_eq!(meta.n_embd_head_v, 256);
    assert_eq!(meta.n_ctx_train, 262_144);
    assert_eq!(
        meta.full_attn_layers,
        Some(8),
        "Carnice is hybrid: full_attention_interval 4 over 32 blocks. Sizing KV over all 32 \
         layers would overestimate by 4x."
    );
    assert_eq!(
        m.gguf.as_ref(),
        Some(&meta),
        "discovery must carry the same header it would read directly"
    );

    // The default roots include ~/.cache/huggingface/hub; it must not change the answer.
    let with_hf = discover_models(&EndpointsCfg::default())
        .await
        .expect("discover_models with default roots");
    eprintln!(
        "  with the default roots (adds ~/.cache/huggingface/hub): {} model(s)",
        with_hf.len()
    );
    assert!(
        with_hf.iter().any(|x| x.total_bytes == 7_359_259_424),
        "Carnice must still be found with the shipped default roots"
    );
}

// ---------------------------------------------------------------------------------------
// 4. discovery → fit → argv, the whole local launch path
// ---------------------------------------------------------------------------------------

/// The seam test. Discovery's `LlamaBuild` + `GgufMeta` + a live VRAM budget go into the
/// solver, and the solver's answer goes into the argv builder, with nothing hand-fed.
#[tokio::test(flavor = "multi_thread")]
async fn real_machine_fit_then_argv_for_carnice_on_vulkan() {
    let (Some(root), Some(_)) = (llama_root(), carnice()) else {
        eprintln!("SKIP: needs both ~/llama.cpp and ~/models/carnice-9b");
        return;
    };
    let vulkan_bin = root.join("build-vulkan/bin/llama-server");
    if !vulkan_bin.is_file() {
        eprintln!("SKIP: no build-vulkan/bin/llama-server");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let builds = real_builds(tmp.path()).await;
    let build = by_id(&builds, "build-vulkan")
        .expect("build-vulkan")
        .clone();
    let models = discover_models(&cfg_with(&["~/models"], &[]))
        .await
        .expect("discover_models");
    let model: LocalModel = models
        .into_iter()
        .find(|m| m.name.contains("Carnice"))
        .expect("Carnice");
    let meta: GgufMeta = model.gguf.clone().expect("Carnice's header");

    // --- a live rig, assembled the way the daemon will ---------------------------------
    //
    // Every build, not just the Vulkan one: `scan_rig` merges devices across builds by
    // `-dev` token, so on this box the one Radeon 840M arrives as TWO rows — `Vulkan0` from
    // `build-vulkan` and `ROCm0` from `build`. That is the shape MK1-CORE finding A was
    // measured on, and probing only one build is exactly why the unit tests missed it.
    let mut gpus: Vec<apexrouter_core::protocol::Gpu> = Vec::new();
    for b in &builds {
        for gpu in probe_devices(Path::new(&b.server_path))
            .await
            .unwrap_or_default()
        {
            match gpus.iter_mut().find(|g| g.device == gpu.device) {
                Some(existing) => existing.seen_by_builds.push(b.id.clone()),
                None => gpus.push(gpu),
            }
        }
    }
    let rig = RigSnapshot {
        gpus,
        builds: builds.clone(),
        ram_free_mb: mem_available_mb(),
        ..RigSnapshot::default()
    };
    eprintln!("\n=== live rig ===");
    for g in &rig.gpus {
        eprintln!(
            "  {} {:?} {} — {} MiB total, {} MiB free, pci={:?}, used={:?}",
            g.device,
            g.backend,
            g.name,
            g.vram_total_mb,
            g.vram_free_mb,
            g.pci_bus_id,
            g.vram_used_mb()
        );
    }
    let physical = rig.physical_devices();
    for p in &physical {
        eprintln!(
            "  physical {} — {} — backends {:?} via {:?}",
            p.key,
            p.name,
            p.backends(),
            p.device_tokens()
        );
    }
    // One laptop, one iGPU. However many builds enumerate it.
    let amd: Vec<_> = physical
        .iter()
        .filter(|p| p.name.to_lowercase().contains("radeon"))
        .collect();
    assert!(
        amd.len() <= 1,
        "the 840M must be ONE physical device, not one per backend: {amd:?}"
    );

    // --- the budget, scoped to the build that will actually be exec'd ------------------
    let budget: VramBudget =
        fit::budget_from_rig(&rig, fit::BackendScope::Build(&build.id), &[], 1_024, &[]);
    eprintln!(
        "\n=== live budget ===\n  backend={:?} devices={:?} usable={} MiB (margin {} MiB), host MemAvailable={} MiB\n  notes={:?}",
        budget.backend,
        budget.device_names(),
        budget.total_usable_mb(),
        budget.margin_mb,
        budget.host_ram_free_mb,
        budget.notes,
    );
    assert_eq!(
        budget.device_names(),
        vec!["Vulkan0".to_owned()],
        "the software-device filter is the documented default"
    );
    // FINDING A: the budget is the Vulkan device's free VRAM, not that plus every other
    // backend's reading of the same silicon.
    let vulkan_free = rig
        .gpus
        .iter()
        .find(|g| g.device == "Vulkan0")
        .map(|g| g.vram_free_mb)
        .expect("Vulkan0");
    assert_eq!(budget.total_usable_mb(), vulkan_free - 1_024);
    let every_gpu: u64 = rig
        .gpus
        .iter()
        .filter(|g| !g.is_software)
        .map(|g| g.vram_free_mb)
        .sum();
    eprintln!(
        "  the pre-fix arithmetic — Σ over every backend's enumeration — would have said {} MiB",
        every_gpu.saturating_sub(1_024)
    );
    if rig.gpus.len() > 1 {
        assert!(
            budget.total_usable_mb() < every_gpu - 1_024,
            "a budget that sums across backends is inventing hardware"
        );
    }

    // --- solve -------------------------------------------------------------------------
    let input = FitInput {
        weights_bytes: model.total_bytes,
        gguf: meta.clone(),
        budget: budget.clone(),
        want_ctx: None,
        want_parallel: Some(1),
        want_kv: Some(KvType::Q8_0),
        split: SplitPlan::default(),
        batch: Some(2_048),
    };
    let plan = fit::fit(&input);
    eprintln!(
        "\n=== fit(Carnice-9b-Q6_K, kv q8_0, np 1, live budget) ===\n  \
         verdict={:?}\n  ctx={} parallel={} kv={:?} ngl={:?}\n  \
         weights={} MiB kv={} MiB compute={} MiB → total {} MiB, headroom {} MiB\n  \
         split={:?} per_device={:?}",
        plan.verdict,
        plan.ctx,
        plan.parallel,
        plan.kv_type,
        plan.ngl,
        plan.weights_mb,
        plan.kv_mb,
        plan.compute_mb,
        plan.weights_mb + plan.kv_mb + plan.compute_mb,
        plan.headroom_mb,
        plan.split,
        plan.per_device_mb,
    );
    for line in &plan.why {
        eprintln!("    why: {line}");
    }

    assert!(!plan.why.is_empty(), "an unexplained number is a bug");
    assert_eq!(
        plan.weights_mb, 7_019,
        "7359259424 bytes rounds up to 7019 MiB"
    );
    assert!(
        matches!(
            plan.verdict,
            FitVerdict::Fits { .. } | FitVerdict::Tight { .. }
        ),
        "a 7 GiB model must fit in ~18 GiB of usable VRAM, got {:?}",
        plan.verdict
    );
    assert_eq!(
        plan.ngl,
        NglPlan::All,
        "a fitting model gets every layer on the GPU"
    );
    assert_eq!(plan.split.devices, vec!["Vulkan0".to_owned()]);
    // Real finding, not an assumption: Carnice is hybrid, so at q8_0 its KV cache is small
    // enough that the auto-search reaches the model's whole 262144-token training context on
    // a 20 GiB shared-memory iGPU. The bound is kept loose because free VRAM is live data.
    assert!(
        plan.ctx >= 32_768,
        "auto-sizing should reach at least 32768 on a free 840M, got {}",
        plan.ctx
    );
    assert!(
        plan.ctx <= meta.n_ctx_train,
        "ctx {} exceeded the model's training context {}",
        plan.ctx,
        meta.n_ctx_train
    );
    assert!(
        plan.headroom_mb > 0,
        "headroom must be positive when it fits, got {}",
        plan.headroom_mb
    );
    assert!(
        (plan.weights_mb + plan.kv_mb + plan.compute_mb) <= budget.total_usable_mb(),
        "the plan spends more than the budget allows"
    );

    // The hybrid discount is not theoretical: sizing KV over all 32 layers instead of the
    // 8 attention layers would cost ~4x. Prove the solver used the right number.
    let mut all_layers = meta.clone();
    all_layers.full_attn_layers = None;
    let dense = fit::fit(&FitInput {
        gguf: all_layers,
        ..input.clone()
    });
    eprintln!(
        "  hybrid KV check: full_attn_layers=Some(8) → {} MiB, None (all 32) → {} MiB at ctx {}",
        plan.kv_mb, dense.kv_mb, plan.ctx
    );
    assert!(
        dense.kv_mb > plan.kv_mb,
        "treating Carnice as dense must cost strictly more KV"
    );

    // An explicit request is echoed back whatever the verdict — never silently shrunk. At
    // q8_0 the hybrid discount is large enough that the FULL 262144-token training context
    // fits on this iGPU, so the refusal path has to be provoked with a heavier KV type.
    let same = fit::fit(&FitInput {
        want_ctx: Some(plan.ctx),
        ..input.clone()
    });
    assert_eq!(
        same.ctx, plan.ctx,
        "an explicit ctx must be reported back verbatim"
    );
    let f32_kv = fit::fit(&FitInput {
        want_ctx: Some(meta.n_ctx_train),
        want_kv: Some(KvType::F32),
        ..input.clone()
    });
    eprintln!(
        "  want_ctx=262144 with kv f32 → verdict={:?}, kv={} MiB (vs {} MiB at q8_0)",
        f32_kv.verdict, f32_kv.kv_mb, plan.kv_mb
    );
    assert_eq!(
        f32_kv.ctx, meta.n_ctx_train,
        "an unfittable explicit ctx is reported as not fitting, not quietly reduced"
    );
    assert!(
        matches!(
            f32_kv.verdict,
            FitVerdict::WontFit { .. } | FitVerdict::NeedsOffload { .. }
        ),
        "16 GiB of f32 KV plus 7 GiB of weights cannot fit in 18 GiB, got {:?}",
        f32_kv.verdict
    );

    // --- build the command line --------------------------------------------------------
    let spec = LocalLlamaSpec {
        build: BuildId::parse("build-vulkan").expect("build id"),
        model_path: model.primary_path().expect("a first shard").to_owned(),
        mmproj: model.mmproj.first().map(|s| s.path.clone()),
        alias_flag: "carnice-9b".to_owned(),
        host: "127.0.0.1".to_owned(),
        port: Some(18_080),
        ctx: Some(plan.ctx),
        parallel: Some(plan.parallel),
        kv_type: Some(plan.kv_type),
        ngl: plan.ngl,
        split: plan.split.clone(),
        mode: SamplingMode::Thinking,
        flash_attn: Some(TriState::On),
        api_key: None,
        extra_args: Vec::new(),
    };
    let preview = argv::plan_local(&spec, &build, None).expect("plan_local");

    eprintln!(
        "\n=== argv (what the supervisor would exec) ===\n  {}",
        preview.program
    );
    eprintln!("  {}", preview.args.join(" "));
    eprintln!("  env: {:?}", preview.env);
    eprintln!("  cwd: {}", preview.cwd);
    for w in &preview.warnings {
        eprintln!("  warning: {w}");
    }

    assert_eq!(preview.program, vulkan_bin.to_string_lossy());
    let pos = |flag: &str| preview.args.iter().position(|a| a == flag);
    let value_of = |flag: &str| pos(flag).and_then(|i| preview.args.get(i + 1)).cloned();

    assert_eq!(value_of("-m").as_deref(), model.primary_path());
    assert!(
        Path::new(&value_of("-m").unwrap_or_default()).is_file(),
        "-m must point at a file that exists"
    );
    assert_eq!(value_of("--port").as_deref(), Some("18080"));
    assert_eq!(value_of("-a").as_deref(), Some("carnice-9b"));
    assert_eq!(value_of("-c"), Some(plan.ctx.to_string()));
    assert_eq!(value_of("-ngl").as_deref(), Some("999"));
    assert_eq!(value_of("-dev").as_deref(), Some("Vulkan0"));
    assert_eq!(value_of("-ctk").as_deref(), Some("q8_0"));
    assert_eq!(value_of("-ctv").as_deref(), Some("q8_0"));
    assert_eq!(
        value_of("-fa").as_deref(),
        Some("on"),
        "b9199 takes -fa on|off|auto, not a bare switch"
    );
    assert!(
        !preview.args.iter().any(|a| a == "--jinja"),
        "--jinja is already default-on in b9199; emitting it is the bug this build detects"
    );
    assert!(
        preview.args.iter().any(|a| a == "--top-k"),
        "every sampling preset carries --top-k 20"
    );

    // The RUNPATH defect fix, and no credential anywhere in argv.
    let ld = preview
        .env
        .iter()
        .find(|(k, _)| k == "LD_LIBRARY_PATH")
        .map(|(_, v)| v.clone())
        .expect("LD_LIBRARY_PATH must always be set");
    assert_eq!(
        Path::new(&ld),
        vulkan_bin.parent().expect("bin dir"),
        "LD_LIBRARY_PATH must be the binary's own directory"
    );
    assert!(
        !preview.args.iter().any(|a| a == "--api-key"),
        "a key may only travel by --api-key-file"
    );

    // Every emitted flag must be one the binary actually advertises.
    for a in preview.args.iter().filter(|a| a.starts_with('-')) {
        if a.parse::<f64>().is_ok() {
            continue; // a negative numeric value, not a flag
        }
        assert!(
            build.flags.has(a),
            "{a} was emitted but build-vulkan's --help never mentioned it"
        );
    }
}

/// One builder, two targets: the same module that produced the argv above produces the
/// container's variable contract, and neither path leaks a credential into a place that is
/// echoed back by an API.
#[test]
fn one_argv_builder_serves_both_the_local_and_container_targets() {
    let cfg = Config::default();
    let input = argv::ContainerLaunchInput {
        runtime: ContainerRuntime::LlamaCpp,
        image_type: None,
        model_repo: Some("unsloth/Qwen3.5-9B-GGUF".to_owned()),
        model_quant: Some("Q4_K_M".to_owned()),
        model_id: None,
        ctx: Some(65_536),
        parallel: Some(2),
        kv_type: Some(KvType::Q8_0),
        mode: SamplingMode::Thinking,
        mmproj: None,
        disk_gb: 60,
        tp: None,
        quantization: None,
        kv_cache_dtype: None,
        enforce_eager: false,
        reasoning_parser: None,
        expose_public: false,
        hf_token: Some(apexrouter_core::Secret::new(
            "hf_REAL_TOKEN_VALUE".to_owned(),
        )),
    };
    let (launch, preview) = argv::plan_container(&input, &cfg).expect("plan_container");

    eprintln!("\n=== container contract (same builder, other target) ===");
    eprintln!(
        "  image={} disk={} GB host={}",
        launch.image, launch.disk_gb, launch.host
    );
    eprintln!("  onstart={}", launch.onstart);
    for (k, v) in &preview.env {
        eprintln!("    {k}={v}");
    }

    assert_eq!(
        launch.env.len(),
        16,
        "the llama.cpp contract is exactly 16 variables, got {:?}",
        launch.env.keys().collect::<Vec<_>>()
    );
    assert_eq!(launch.host, "127.0.0.1", "tunnel-only unless expose_public");
    assert_eq!(
        launch.env.get("HF_TOKEN").map(String::as_str),
        Some("hf_REAL_TOKEN_VALUE"),
        "the real token belongs in the env map that vast receives"
    );
    assert!(
        !launch.onstart.contains("hf_REAL_TOKEN_VALUE"),
        "onstart is persisted and echoed by `show instance`; it must never carry a token"
    );
    let rendered = format!("{:?}", preview.env);
    assert!(
        !rendered.contains("hf_REAL_TOKEN_VALUE"),
        "the preview is what every surface logs; the token must be redacted there"
    );
    assert!(rendered.contains("***"), "the redaction must be visible");
}

/// Pins the device-mask behaviour ARCHITECTURE.md §4.6 mandates, **including a latent
/// multi-GPU hazard that this single-GPU box cannot expose.**
///
/// §4.6 says the visible-devices env var is set "and `-dev` carries the explicit device list
/// regardless". C-12 implemented exactly that. On this laptop the two agree by luck — one
/// GPU, so the mask is `0` and the token is `Vulkan0`.
///
/// They do **not** agree on a multi-GPU rig. `ggml/src/ggml-vulkan/ggml-vulkan.cpp:6489`
/// says, in its own comment, "Emulate behavior of CUDA_VISIBLE_DEVICES for Vulkan": the
/// masked devices are renumbered from zero inside the child. So selecting the second GPU
/// emits `-dev Vulkan1` alongside `GGML_VK_VISIBLE_DEVICES=1`, and inside the child that
/// device is called `Vulkan0` — `-dev Vulkan1` then names a device that does not exist.
/// Worse, the env var indexes the **raw** `enumeratePhysicalDevices()` list, which still
/// contains entries the default path skips (a software rasteriser is `eCpu` and is filtered
/// out of the `VulkanN` numbering but is still in the raw list), so the two index spaces can
/// disagree even at index 0 and the launch would land silently on llvmpipe.
///
/// This test does not assert the *desired* behaviour, because the desired behaviour is a
/// normative-document decision, not a gate decision. It asserts what is built today so the
/// change is deliberate and visible when Stage 3's supervisor or a real multi-GPU box
/// forces it.
#[test]
fn the_device_mask_and_the_dev_token_are_two_index_spaces() {
    let one = argv::backend_env(GpuBackend::Vulkan, &["Vulkan0".to_owned()]);
    assert_eq!(
        one,
        vec![("GGML_VK_VISIBLE_DEVICES".to_owned(), "0".to_owned())],
        "single-GPU: mask and -dev token agree, which is why this laptop launches fine"
    );

    // The multi-GPU shape. `-dev Vulkan1` + a mask of `1` is the collision described above.
    let second = argv::backend_env(GpuBackend::Vulkan, &["Vulkan1".to_owned()]);
    assert_eq!(
        second,
        vec![("GGML_VK_VISIBLE_DEVICES".to_owned(), "1".to_owned())],
        "HAZARD (ARCHITECTURE §4.6): after this mask the device is called Vulkan0 inside the \
         child, but plan_local still emits `-dev Vulkan1`"
    );
    let pair = argv::backend_env(GpuBackend::Cuda, &["CUDA2".to_owned(), "CUDA3".to_owned()]);
    assert_eq!(
        pair,
        vec![("CUDA_VISIBLE_DEVICES".to_owned(), "2,3".to_owned())],
        "same hazard on CUDA: masked to 2,3, the child calls them CUDA0 and CUDA1"
    );

    // Not a hazard, and deliberately so: masking nothing is not the same as masking all.
    assert!(
        argv::backend_env(GpuBackend::Vulkan, &[]).is_empty(),
        "an empty device list must express no preference, not strand the launch on the CPU"
    );
    assert!(
        argv::backend_env(GpuBackend::Cpu, &["Vulkan0".to_owned()]).is_empty(),
        "a CPU build has no device mask to set"
    );
}

// ---------------------------------------------------------------------------------------
// 5. the check registry, wired to a real rig — `apexrouter doctor`
// ---------------------------------------------------------------------------------------

/// `local_checks()` against this machine, with `ctx.rig` populated from real discovery.
///
/// This is the seam C-17 could not test: `builds.*` and `devices.*` read `ctx.rig` and
/// report `Skipped` when nobody filled it in, and `models.discovered` calls C-10 directly —
/// which was still `todo!()` when C-17 finished, so its row was a caught panic. Both are now
/// exercised for real.
#[tokio::test(flavor = "multi_thread")]
async fn doctor_runs_every_local_check_against_the_real_rig() {
    use apexrouter_core::checks::{local_checks, CheckCtx, Registry};
    use apexrouter_core::protocol::CheckStatus;
    use std::collections::HashMap;
    use std::sync::Arc;

    let paths = Paths::resolve().expect("Paths::resolve");
    let cfg = Arc::new(Config::load().expect("Config::load"));

    // Build a real rig snapshot the way the daemon's scanner will.
    let rig = if llama_root().is_some() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let builds = real_builds(tmp.path()).await;
        let mut gpus = Vec::new();
        for b in &builds {
            if b.flags.flags.is_empty() {
                continue;
            }
            if let Ok(found) = probe_devices(Path::new(&b.server_path)).await {
                for g in found {
                    if !gpus.iter().any(|x: &apexrouter_core::protocol::Gpu| {
                        x.device == g.device && x.backend == g.backend
                    }) {
                        gpus.push(g);
                    }
                }
            }
        }
        Some(Arc::new(RigSnapshot {
            gpus,
            builds,
            ram_free_mb: mem_available_mb(),
            cpu_threads: std::thread::available_parallelism()
                .map(|n| n.get() as u32)
                .unwrap_or(0),
            ..RigSnapshot::default()
        }))
    } else {
        None
    };
    if let Some(r) = rig.as_ref() {
        eprintln!(
            "\n=== rig handed to doctor ===\n  {} device(s) across {} build(s), {} CPU threads, {} MiB RAM free",
            r.gpus.len(),
            r.builds.len(),
            r.cpu_threads,
            r.ram_free_mb
        );
        for g in &r.gpus {
            eprintln!(
                "    {:<10} {:?} {} MiB total / {} MiB free",
                g.device, g.backend, g.vram_total_mb, g.vram_free_mb
            );
        }
    }

    let ctx = CheckCtx {
        paths,
        cfg,
        http: reqwest::Client::new(),
        rig,
        proxy_url: None,
        instance: None,
        ext: HashMap::new(),
    };

    let mut reg = Registry::new();
    for c in local_checks() {
        reg.register(c);
    }
    let ids = reg.ids();
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let streamed = tokio::spawn(async move {
        let mut seen = Vec::new();
        while let Some(r) = rx.recv().await {
            seen.push(r);
        }
        seen
    });
    let started = std::time::Instant::now();
    let results = reg.run(&ctx, None, tx).await;
    let elapsed = started.elapsed();
    let streamed = streamed.await.expect("the streaming task");

    eprintln!(
        "\n=== apexrouter doctor (local checks) — {} ms wall ===",
        elapsed.as_millis()
    );
    for r in &results {
        eprintln!(
            "  [{:<7}] {:<22} {:>4} ms  {}",
            format!("{:?}", r.status),
            r.id,
            r.ms,
            r.detail
        );
        if let Some(fix) = &r.fix {
            eprintln!("              fix: {fix}");
        }
    }

    assert_eq!(
        results.len(),
        ids.len(),
        "every registered check must report exactly once"
    );
    assert_eq!(
        results.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
        ids,
        "the returned order is registration order, so `doctor --json` is stable"
    );
    assert_eq!(
        streamed.len(),
        results.len(),
        "every result must also reach the stream"
    );

    // The seam: with a real rig, the discovery-backed checks must have actually run.
    if llama_root().is_some() {
        for id in ["builds.discovered", "builds.flags", "devices.enumerated"] {
            let r = results
                .iter()
                .find(|r| r.id.as_str() == id)
                .unwrap_or_else(|| panic!("{id} must be registered"));
            assert_ne!(
                r.status,
                CheckStatus::Skipped,
                "{id} was Skipped even though ctx.rig was populated: {}",
                r.detail
            );
        }
    }
    if models_root().is_some() {
        let r = results
            .iter()
            .find(|r| r.id.as_str() == "models.discovered")
            .expect("models.discovered must be registered");
        assert_ne!(
            r.status,
            CheckStatus::Fail,
            "models.discovered called C-10 for real and failed: {}",
            r.detail
        );
    }
    // A doctor run must never leave a check un-timed or un-labelled.
    for r in &results {
        assert!(!r.label.is_empty(), "{} has no label", r.id);
    }
}

// ---------------------------------------------------------------------------------------
// 6. migration, against Andre's real legacy state
// ---------------------------------------------------------------------------------------

/// Read the real `~/.vastai-gguf` and prove the dry run is a dry run.
///
/// This is the one test that touches state a human still depends on, so it hashes the whole
/// tree before and after and refuses to pass if a single byte moved.
#[test]
fn real_legacy_state_is_read_and_never_written() {
    let Some(home) = home() else {
        eprintln!("SKIP: no $HOME");
        return;
    };
    let legacy = home.join(".vastai-gguf");
    if !legacy.is_dir() {
        eprintln!("SKIP: no ~/.vastai-gguf on this box");
        return;
    }

    let paths = Paths::resolve().expect("Paths::resolve");
    let cfg = Config::load().expect("Config::load");
    assert_eq!(
        paths.legacy().vastai_gguf,
        legacy,
        "the resolver must point at the real legacy dir"
    );

    let before = tree_digest(&legacy);
    let lr_dir = paths.legacy().localrouter_dir.clone();
    let lr_before = lr_dir.as_ref().map(|d| tree_digest(d));

    let plan = migrate::plan(&paths, &cfg).expect("migrate::plan");

    let after = tree_digest(&legacy);
    assert_eq!(
        before, after,
        "migrate::plan modified ~/.vastai-gguf — it is documented to write nothing"
    );
    if let (Some(d), Some(b)) = (lr_dir.as_ref(), lr_before) {
        assert_eq!(
            b,
            tree_digest(d),
            "migrate::plan modified the LocalRouter checkout at {}",
            d.display()
        );
    }
    let mut imports = 0usize;
    let mut skips = 0usize;
    let mut warns = 0usize;
    for item in &plan.items {
        match item.action {
            MigrationAction::Import => imports += 1,
            MigrationAction::Skip => skips += 1,
            MigrationAction::Warn => warns += 1,
        }
    }
    eprintln!(
        "\n=== migrate --dry-run over the real legacy tree ===\n  \
         {} item(s): {imports} import, {skips} skip, {warns} warn\n  sources: {:?}",
        plan.items.len(),
        plan.source_paths
    );
    for item in plan.items.iter().take(24) {
        eprintln!(
            "    [{:?}] {:<22} {}\n        {}",
            item.action, item.what, item.from, item.detail
        );
    }
    if plan.items.len() > 24 {
        eprintln!("    … {} more", plan.items.len() - 24);
    }

    assert!(
        !plan.items.is_empty(),
        "this box has legacy state; the plan must not be empty"
    );
    assert!(
        plan.source_paths.iter().any(|p| p.contains(".vastai-gguf")),
        "the legacy dir must be named as a source, got {:?}",
        plan.source_paths
    );

    // A plan is printed to stdout by `apexrouter migrate --dry-run`. It must not be a way to
    // exfiltrate the Together key that sits in the legacy config.
    let serialised = serde_json::to_string(&plan).expect("the plan must serialise");
    if let Ok(legacy_cfg) = std::fs::read_to_string(legacy.join("config.toml")) {
        for line in legacy_cfg.lines() {
            let Some(rest) = line.split_once("api_key") else {
                continue;
            };
            let key: String = rest
                .1
                .trim()
                .trim_start_matches('=')
                .trim()
                .trim_matches('"')
                .to_owned();
            if key.len() >= 12 {
                assert!(
                    !serialised.contains(&key),
                    "the real Together key leaked into a printable migration plan"
                );
                eprintln!("  credential posture: the legacy api_key does NOT appear in the plan");
            }
        }
    }

    // The legacy readers, pointed at the real files.
    let instances =
        migrate::read_legacy_instances(&legacy.join("local_instances")).expect("read instances");
    eprintln!("  legacy local_instances: {} row(s)", instances.len());
    for i in &instances {
        eprintln!(
            "    {} pid={:?} port={:?} backend={:?} model={:?}",
            i.name, i.pid, i.port, i.backend, i.model_path
        );
    }
    assert!(
        !instances.is_empty(),
        "~/.vastai-gguf/local_instances holds one saved instance on this box"
    );

    // Stale paths are the normal case: the saved instance names a GGUF that is gone.
    for i in &instances {
        if let Some(p) = i.model_path.as_deref() {
            eprintln!(
                "    model_path {} → exists on disk: {}",
                p,
                Path::new(p).exists()
            );
        }
    }
}

/// The legacy usage log is read in place through the public reader, with zero failed rows,
/// no double counting, and no write of any kind.
///
/// The real file is nastier than its schema: `epoch` is a **float**, one row of the four is
/// missing `epoch` entirely, and every `timestamp` is local time with a `Z` glued on. All
/// three shapes have to survive one call.
#[test]
fn real_legacy_usage_log_reads_cleanly_through_the_public_reader() {
    let Some(home) = home() else {
        eprintln!("SKIP: no $HOME");
        return;
    };
    let log = home.join(".vastai-gguf/usage.log");
    if !log.is_file() {
        eprintln!("SKIP: no ~/.vastai-gguf/usage.log");
        return;
    }
    let paths = Paths::resolve().expect("Paths::resolve");
    let cfg = Config::load().expect("Config::load");

    let raw = std::fs::read_to_string(&log).expect("read the legacy log");
    let on_disk = raw.lines().filter(|l| !l.trim().is_empty()).count();
    let before = tree_digest(log.parent().expect("parent"));

    let rows = usage::read_all(&paths, &cfg.compat).expect("usage::read_all");
    eprintln!("\n=== legacy usage.log through usage::read_all ===\n  {on_disk} line(s) on disk → {} row(s) parsed", rows.len());
    for r in &rows {
        eprintln!(
            "    {} epoch={:?} {:<10} {:<44} {} + {} tok  ${:.6}",
            r.timestamp,
            r.epoch,
            r.provider,
            r.model_id,
            r.prompt_tokens,
            r.completion_tokens,
            r.cost_usd,
        );
    }
    // "Every legacy row must survive" is a property of the READER, so it must be measured
    // against the legacy file alone. `read_all` merges `$STATE/usage.jsonl` on top, and on a
    // machine where apexrouterd has ever served a request that file is non-empty — so
    // `rows.len() == on_disk` is only true on a box that has never run the daemon. It went
    // red for the first time during the MK1-CORE acceptance run, for a reason that has
    // nothing to do with legacy parsing.
    //
    // The state-independent form of the same criterion: every line of the real legacy file is
    // represented in the merged result. A row the mirror already duplicated is de-duped away
    // as a *legacy* row, but the identical `usage.jsonl` row carrying the same five legacy
    // columns is still there, so it is still "surviving" in the only sense that matters.
    let state_only = usage::read_all(
        &paths,
        &CompatCfg {
            read_legacy_state: false,
            ..cfg.compat.clone()
        },
    )
    .expect("usage::read_all without the legacy log");
    assert!(
        rows.len() >= state_only.len(),
        "merging the legacy log may only ever add rows: {} merged < {} from $STATE alone",
        rows.len(),
        state_only.len()
    );
    for line in raw.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value =
            serde_json::from_str(line).expect("every legacy line is a JSON object");
        let want = (
            v["timestamp"].as_str().unwrap_or_default().to_owned(),
            v["provider"].as_str().unwrap_or_default().to_owned(),
            v["model_id"].as_str().unwrap_or_default().to_owned(),
            v["prompt_tokens"].as_u64().unwrap_or_default() as u32,
            v["completion_tokens"].as_u64().unwrap_or_default() as u32,
        );
        assert!(
            rows.iter().any(|r| (
                r.timestamp.clone(),
                r.provider.clone(),
                r.model_id.clone(),
                r.prompt_tokens,
                r.completion_tokens,
            ) == want),
            "legacy row dropped by the reader: {line}"
        );
    }

    // The float `epoch` and the one row that has none.
    assert!(
        rows.iter().any(|r| r.epoch.is_some()),
        "the real file carries fractional epochs, e.g. 1777745481.5262182"
    );
    assert!(
        rows.iter().any(|r| r.epoch.is_none()),
        "row 3 of the real file has no epoch at all, and that is not an error"
    );

    // The lying `Z`: the stamp says 20:11:21Z, the epoch says 18:11:21 UTC. `epoch` wins.
    for r in &rows {
        let Some(epoch) = r.epoch else { continue };
        let from_string = usage::parse_lenient_timestamp(&r.timestamp)
            .expect("every legacy timestamp must parse");
        let drift = (epoch as i64 - from_string).abs();
        eprintln!(
            "      {} → epoch {} vs stamp-as-local {} (drift {} s = this machine's UTC offset)",
            r.timestamp, epoch as i64, from_string, drift
        );
        assert!(
            drift < 60,
            "the lying-Z stamp must be read as LOCAL time so it agrees with epoch; drift was {drift} s"
        );
    }

    let summary = usage::aggregate(&rows, None, usage::GroupBy::Provider);
    eprintln!(
        "  aggregate(all, by provider): window={:?} rows={} prompt={} completion={} total={:?}",
        summary.window,
        summary.rows,
        summary.total_prompt,
        summary.total_completion,
        summary.total_cost
    );
    for b in &summary.by {
        eprintln!("    {:<12} {:?}", b.key, b.cost);
    }
    assert_eq!(summary.rows as usize, rows.len());
    assert_eq!(summary.window, "all");

    assert_eq!(
        before,
        tree_digest(log.parent().expect("parent")),
        "reading the usage log must not modify it"
    );
}
