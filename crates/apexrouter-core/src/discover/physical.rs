//! OWNER: unit C-11b (core/src/discover/physical.rs) — the MK1-CORE acceptance finding-A
//! fix. Do not edit outside that unit.
//!
//! **Physical device identity across backends.** One card, seen by two llama.cpp builds, is
//! two `Gpu` rows with different `-dev` tokens and different VRAM readings. This module
//! answers "which of these rows are the same piece of silicon", so that
//! [`crate::fit::budget_from_rig`] never spends the same card twice and `apexrouter rig`
//! never shows a laptop with one iGPU as a two-GPU workstation.
//!
//! Identity is established from the **PCI bus** where it can be, because that is the only
//! answer that survives a driver renumbering its devices:
//!
//! ```text
//! /sys/bus/pci/devices/0000:04:00.0/class   0x030000   (display controller)
//! /sys/bus/pci/devices/0000:04:00.0/vendor  0x1002     (AMD)
//! ```
//!
//! llama.cpp does not print a bus id — `--list-devices` gives a token, a name and two memory
//! numbers and nothing else — so the enumeration has to be *aligned* with sysfs rather than
//! read off it. Alignment is by vendor and count, per backend, and it only happens when it is
//! unambiguous:
//!
//! * every non-software device of one backend is bucketed by the vendor inferred from its
//!   name, and a bucket whose size equals the number of that vendor's PCI GPUs is matched
//!   1:1 in enumeration order;
//! * whatever is left over is matched the same way against the leftover PCI GPUs, which is
//!   what rescues a device whose name names no vendor we know;
//! * anything still unmatched keeps `pci_bus_id: None` and falls back to the documented
//!   name heuristic in `Gpu::physical_key`.
//!
//! The residual assumption — that two backends enumerate identical cards in the same order —
//! is exactly the assumption the name heuristic already makes, so alignment never makes
//! identity *worse* than it was; when it succeeds it makes it exact. On the machine in
//! `docs/port/00-machine-ground-truth.md` it succeeds: one AMD display controller at
//! `0000:04:00.0`, one Vulkan device, one ROCm device, and the two enumerations collapse onto
//! the one card they describe.

use apexrouter_protocol::{Gpu, GpuBackend};
use std::path::{Path, PathBuf};

/// Where the kernel lists PCI functions.
const PCI_DEVICES: &str = "/sys/bus/pci/devices";

/// PCI class high byte for a display controller (`0x03xxxx`).
const CLASS_DISPLAY: u32 = 0x03;

/// A GPU as the PCI bus knows it: an address, and who made it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PciGpu {
    /// `"0000:04:00.0"` — domain:bus:device.function, the sysfs directory name.
    pub bus_id: String,
    /// PCI vendor id, e.g. `0x1002`.
    pub vendor_id: u16,
    /// PCI device id, e.g. `0x1114`.
    pub device_id: u16,
    /// The vendor, resolved.
    pub vendor: GpuVendor,
}

/// Who made a GPU. Enough to align an enumeration with the bus; not a hardware database.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuVendor {
    /// AMD/ATI.
    Amd,
    /// NVIDIA.
    Nvidia,
    /// Intel.
    Intel,
    /// Anything else, or a name that does not say.
    Unknown,
}

impl GpuVendor {
    /// From the PCI vendor id. `0x1002`/`0x1022` are AMD, `0x10de` NVIDIA, `0x8086` Intel.
    pub fn from_pci_id(id: u16) -> Self {
        match id {
            0x1002 | 0x1022 => GpuVendor::Amd,
            0x10de => GpuVendor::Nvidia,
            0x8086 => GpuVendor::Intel,
            _ => GpuVendor::Unknown,
        }
    }

    /// From a `--list-devices` name such as `"AMD Radeon 840M Graphics (RADV KRACKAN1)"`.
    ///
    /// Substring matching, deliberately narrow: guessing wrong here costs an alignment, and
    /// a missed alignment falls back to the name heuristic rather than to a wrong answer.
    pub fn from_device_name(name: &str) -> Self {
        let n = name.to_ascii_lowercase();
        const AMD: &[&str] = &["amd", "radeon", "instinct", "gfx", "radv", "advanced micro"];
        const NVIDIA: &[&str] = &[
            "nvidia", "geforce", "rtx", "gtx", "tesla", "quadro", "titan", "h100", "a100", "l40",
            "h200", "b200",
        ];
        const INTEL: &[&str] = &["intel", "arc ", "iris", "uhd graphics", "xe graphics"];
        if NVIDIA.iter().any(|m| n.contains(m)) {
            return GpuVendor::Nvidia;
        }
        if AMD.iter().any(|m| n.contains(m)) {
            return GpuVendor::Amd;
        }
        if INTEL.iter().any(|m| n.contains(m)) {
            return GpuVendor::Intel;
        }
        GpuVendor::Unknown
    }
}

/// Every display controller on the PCI bus, sorted by address.
///
/// Sorted because bus order is the one enumeration order every backend agrees on when it
/// agrees on anything: `CUDA_DEVICE_ORDER=PCI_BUS_ID`, RADV's physical-device order and
/// `/sys/class/drm` all follow it.
///
/// An unreadable or absent `/sys` — a container, a non-Linux target — yields an empty list,
/// and identity falls back to the name heuristic. That is a degradation, not a failure.
pub fn scan_pci_gpus() -> Vec<PciGpu> {
    scan_pci_gpus_in(Path::new(PCI_DEVICES))
}

/// [`scan_pci_gpus`] against an arbitrary sysfs root, so the parser is testable without one.
pub fn scan_pci_gpus_in(root: &Path) -> Vec<PciGpu> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out: Vec<PciGpu> = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        let Some(bus_id) = dir.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
            continue;
        };
        let Some(class) = read_hex(&dir, "class") else {
            continue;
        };
        if class >> 16 != CLASS_DISPLAY {
            continue;
        }
        let vendor_id = read_hex(&dir, "vendor")
            .and_then(|v| u16::try_from(v).ok())
            .unwrap_or(0);
        let device_id = read_hex(&dir, "device")
            .and_then(|v| u16::try_from(v).ok())
            .unwrap_or(0);
        out.push(PciGpu {
            bus_id,
            vendor_id,
            device_id,
            vendor: GpuVendor::from_pci_id(vendor_id),
        });
    }
    out.sort_by(|a, b| a.bus_id.cmp(&b.bus_id));
    out
}

/// Fill in [`Gpu::pci_bus_id`] for every enumeration that can be aligned with the bus.
///
/// Pure: the sysfs read is [`scan_pci_gpus`]'s job, so the alignment rules can be tested
/// against a synthetic 4×H100 or a laptop iGPU without either being present.
///
/// Alignment is per backend — one backend's device 0 and another's device 0 are aligned
/// independently, and that is precisely what makes them come out with the same bus id when
/// they are the same card. Software rasterisers are skipped: `llvmpipe` is not on the bus.
pub fn attach_pci_ids(gpus: &mut [Gpu], pci: &[PciGpu]) {
    if pci.is_empty() {
        return;
    }
    let backends: Vec<GpuBackend> = {
        let mut seen: Vec<GpuBackend> = Vec::new();
        for g in gpus.iter() {
            if !g.is_software && !seen.contains(&g.backend) {
                seen.push(g.backend.clone());
            }
        }
        seen
    };

    for backend in backends {
        // Enumeration order within the backend, by the index llama.cpp gave it.
        let mut idx: Vec<usize> = (0..gpus.len())
            .filter(|i| !gpus[*i].is_software && gpus[*i].backend == backend)
            .collect();
        idx.sort_by_key(|i| gpus[*i].index);

        let mut used: Vec<&str> = Vec::new();

        // Pass 1: per vendor, when the counts agree exactly.
        for vendor in [GpuVendor::Nvidia, GpuVendor::Amd, GpuVendor::Intel] {
            let group: Vec<usize> = idx
                .iter()
                .copied()
                .filter(|i| {
                    gpus[*i].pci_bus_id.is_none()
                        && GpuVendor::from_device_name(&gpus[*i].name) == vendor
                })
                .collect();
            let cards: Vec<&PciGpu> = pci
                .iter()
                .filter(|p| p.vendor == vendor && !used.contains(&p.bus_id.as_str()))
                .collect();
            if !group.is_empty() && group.len() == cards.len() {
                for (i, card) in group.iter().zip(cards.iter()) {
                    gpus[*i].pci_bus_id = Some(card.bus_id.clone());
                    used.push(&card.bus_id);
                }
            }
        }

        // Pass 2: whatever is left, when what is left also agrees exactly. This is what
        // identifies a device whose name names no vendor we recognise.
        let rest: Vec<usize> = idx
            .iter()
            .copied()
            .filter(|i| gpus[*i].pci_bus_id.is_none())
            .collect();
        let cards: Vec<&PciGpu> = pci
            .iter()
            .filter(|p| !used.contains(&p.bus_id.as_str()))
            .collect();
        if !rest.is_empty() && rest.len() == cards.len() {
            for (i, card) in rest.iter().zip(cards.iter()) {
                gpus[*i].pci_bus_id = Some(card.bus_id.clone());
            }
        }
    }
}

/// `0x030000` -> `0x030000`. `None` when the file is missing or not a hex word.
fn read_hex(dir: &Path, file: &str) -> Option<u32> {
    let raw = std::fs::read_to_string(PathBuf::from(dir).join(file)).ok()?;
    let text = raw.trim();
    let body = text.strip_prefix("0x").unwrap_or(text);
    u32::from_str_radix(body, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_protocol::physical_devices;

    fn gpu(device: &str, index: u32, name: &str, backend: GpuBackend, total: u64) -> Gpu {
        Gpu {
            device: device.into(),
            index,
            name: name.into(),
            backend,
            vram_total_mb: total,
            vram_free_mb: total,
            pci_bus_id: None,
            driver: None,
            is_software: false,
            seen_by_builds: vec![],
            held_by: vec![],
            reserved_mb: 0,
        }
    }

    fn pci(bus: &str, vendor: GpuVendor) -> PciGpu {
        PciGpu {
            bus_id: bus.into(),
            vendor_id: match vendor {
                GpuVendor::Amd => 0x1002,
                GpuVendor::Nvidia => 0x10de,
                GpuVendor::Intel => 0x8086,
                GpuVendor::Unknown => 0,
            },
            device_id: 0x1114,
            vendor,
        }
    }

    /// The live reading from this laptop: one AMD display controller, two backends.
    #[test]
    fn one_card_two_backends_collapses_to_one_physical_device() {
        let mut gpus = vec![
            gpu(
                "Vulkan0",
                0,
                "AMD Radeon 840M Graphics (RADV KRACKAN1)",
                GpuBackend::Vulkan,
                20_992,
            ),
            gpu(
                "ROCm0",
                0,
                "AMD Radeon 840M Graphics",
                GpuBackend::Rocm,
                11_397,
            ),
        ];
        attach_pci_ids(&mut gpus, &[pci("0000:04:00.0", GpuVendor::Amd)]);
        assert_eq!(gpus[0].pci_bus_id.as_deref(), Some("0000:04:00.0"));
        assert_eq!(gpus[1].pci_bus_id.as_deref(), Some("0000:04:00.0"));

        let physical = physical_devices(&gpus);
        assert_eq!(physical.len(), 1, "one card, not two: {physical:?}");
        assert_eq!(physical[0].key, "pci:0000:04:00.0");
        assert_eq!(physical[0].backends().len(), 2);
        // The VRAM readings legitimately differ and both survive.
        assert_eq!(
            physical[0]
                .view_for(&GpuBackend::Vulkan)
                .map(|v| v.vram_total_mb),
            Some(20_992)
        );
        assert_eq!(
            physical[0]
                .view_for(&GpuBackend::Rocm)
                .map(|v| v.vram_total_mb),
            Some(11_397)
        );
    }

    /// Without any sysfs at all, the name heuristic still collapses the pair.
    #[test]
    fn the_name_heuristic_pairs_the_same_card_without_pci() {
        let gpus = vec![
            gpu(
                "Vulkan0",
                0,
                "AMD Radeon 840M Graphics (RADV KRACKAN1)",
                GpuBackend::Vulkan,
                20_992,
            ),
            gpu(
                "ROCm0",
                0,
                "AMD Radeon 840M Graphics",
                GpuBackend::Rocm,
                11_397,
            ),
        ];
        let physical = physical_devices(&gpus);
        assert_eq!(physical.len(), 1, "{physical:?}");
        assert_eq!(physical[0].key, "name:amd radeon 840m graphics#0");
    }

    /// Four identical cards are four cards, in both backends, and they pair up index-wise.
    #[test]
    fn four_identical_cards_stay_four_devices() {
        let mut gpus: Vec<Gpu> = (0..4)
            .map(|i| {
                gpu(
                    &format!("CUDA{i}"),
                    i,
                    "NVIDIA H100 PCIe",
                    GpuBackend::Cuda,
                    81_559,
                )
            })
            .collect();
        gpus.extend((0..4).map(|i| {
            gpu(
                &format!("Vulkan{i}"),
                i,
                "NVIDIA H100 PCIe",
                GpuBackend::Vulkan,
                81_559,
            )
        }));
        let bus: Vec<PciGpu> = [
            "0000:07:00.0",
            "0000:0a:00.0",
            "0000:47:00.0",
            "0000:4e:00.0",
        ]
        .iter()
        .map(|b| pci(b, GpuVendor::Nvidia))
        .collect();
        attach_pci_ids(&mut gpus, &bus);

        let physical = physical_devices(&gpus);
        assert_eq!(physical.len(), 4, "four cards, eight enumerations");
        for p in &physical {
            assert_eq!(p.views.len(), 2, "{p:?}");
            assert!(p.pci_bus_id.is_some());
        }
        assert_eq!(physical[0].device_tokens(), vec!["CUDA0", "Vulkan0"]);
        assert_eq!(physical[3].device_tokens(), vec!["CUDA3", "Vulkan3"]);
    }

    /// A dGPU plus an iGPU: vendor bucketing keeps the alignment honest even though the
    /// CUDA build sees one device and the Vulkan build sees two.
    #[test]
    fn a_mixed_vendor_box_aligns_per_vendor() {
        let mut gpus = vec![
            gpu(
                "CUDA0",
                0,
                "NVIDIA GeForce RTX 4090",
                GpuBackend::Cuda,
                24_564,
            ),
            gpu(
                "Vulkan0",
                0,
                "NVIDIA GeForce RTX 4090",
                GpuBackend::Vulkan,
                24_564,
            ),
            gpu(
                "Vulkan1",
                1,
                "AMD Radeon 840M Graphics (RADV KRACKAN1)",
                GpuBackend::Vulkan,
                20_992,
            ),
        ];
        attach_pci_ids(
            &mut gpus,
            &[
                pci("0000:01:00.0", GpuVendor::Nvidia),
                pci("0000:04:00.0", GpuVendor::Amd),
            ],
        );
        assert_eq!(gpus[0].pci_bus_id.as_deref(), Some("0000:01:00.0"));
        assert_eq!(gpus[1].pci_bus_id.as_deref(), Some("0000:01:00.0"));
        assert_eq!(gpus[2].pci_bus_id.as_deref(), Some("0000:04:00.0"));
        assert_eq!(physical_devices(&gpus).len(), 2);
    }

    /// When the counts do not agree, nothing is guessed.
    #[test]
    fn an_ambiguous_alignment_assigns_nothing() {
        let mut gpus = vec![
            gpu("CUDA0", 0, "NVIDIA A100", GpuBackend::Cuda, 81_920),
            gpu("CUDA1", 1, "NVIDIA A100", GpuBackend::Cuda, 81_920),
        ];
        // Three NVIDIA cards on the bus, two visible to CUDA: which two? Unknowable.
        attach_pci_ids(
            &mut gpus,
            &[
                pci("0000:07:00.0", GpuVendor::Nvidia),
                pci("0000:0a:00.0", GpuVendor::Nvidia),
                pci("0000:47:00.0", GpuVendor::Nvidia),
            ],
        );
        assert!(gpus.iter().all(|g| g.pci_bus_id.is_none()));
        // The name heuristic still keeps them apart.
        let physical = physical_devices(&gpus);
        assert_eq!(physical.len(), 2);
        assert_eq!(physical[0].key, "name:nvidia a100#0");
        assert_eq!(physical[1].key, "name:nvidia a100#1");
    }

    /// A software rasteriser is not on the bus and never merges with hardware.
    #[test]
    fn software_devices_are_never_given_a_bus_id() {
        let mut soft = gpu(
            "Vulkan1",
            1,
            "llvmpipe (LLVM 19.1.0, 256 bits)",
            GpuBackend::Vulkan,
            8_192,
        );
        soft.is_software = true;
        let mut gpus = vec![
            gpu(
                "Vulkan0",
                0,
                "AMD Radeon 840M Graphics",
                GpuBackend::Vulkan,
                20_992,
            ),
            soft,
        ];
        attach_pci_ids(&mut gpus, &[pci("0000:04:00.0", GpuVendor::Amd)]);
        assert_eq!(gpus[0].pci_bus_id.as_deref(), Some("0000:04:00.0"));
        assert_eq!(gpus[1].pci_bus_id, None);
        assert_eq!(physical_devices(&gpus).len(), 2);
    }

    #[test]
    fn sysfs_parsing_keeps_display_controllers_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let write = |bus: &str, class: &str, vendor: &str, device: &str| {
            let d = root.join(bus);
            std::fs::create_dir_all(&d).expect("mkdir");
            std::fs::write(d.join("class"), class).expect("class");
            std::fs::write(d.join("vendor"), vendor).expect("vendor");
            std::fs::write(d.join("device"), device).expect("device");
        };
        // The real values from this laptop, plus a non-GPU function and a dGPU.
        write("0000:04:00.0", "0x030000\n", "0x1002\n", "0x1114\n");
        write("0000:00:14.3", "0x0c0500\n", "0x1022\n", "0x1507\n");
        write("0000:01:00.0", "0x030200\n", "0x10de\n", "0x2684\n");

        let found = scan_pci_gpus_in(root);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0].bus_id, "0000:01:00.0");
        assert_eq!(found[0].vendor, GpuVendor::Nvidia);
        assert_eq!(found[1].bus_id, "0000:04:00.0");
        assert_eq!(found[1].vendor, GpuVendor::Amd);
        assert_eq!(found[1].device_id, 0x1114);

        // A missing sysfs is a degradation, not a panic.
        assert!(scan_pci_gpus_in(&root.join("nope")).is_empty());
    }

    #[test]
    fn vendors_come_from_names_narrowly() {
        assert_eq!(
            GpuVendor::from_device_name("AMD Radeon 840M Graphics (RADV KRACKAN1)"),
            GpuVendor::Amd
        );
        assert_eq!(
            GpuVendor::from_device_name("NVIDIA H100 80GB HBM3"),
            GpuVendor::Nvidia
        );
        assert_eq!(
            GpuVendor::from_device_name("Intel(R) Arc(tm) A770"),
            GpuVendor::Intel
        );
        assert_eq!(
            GpuVendor::from_device_name("Zhaoxin C-960"),
            GpuVendor::Unknown
        );
        assert_eq!(GpuVendor::from_pci_id(0x10de), GpuVendor::Nvidia);
        assert_eq!(GpuVendor::from_pci_id(0x1234), GpuVendor::Unknown);
    }
}
