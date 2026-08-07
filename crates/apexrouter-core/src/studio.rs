//! OWNER: studio phase 6 (core/studio.rs).
//!
//! Studio VRAM planning. **fit() is not taught torch** (STUDIO.md S7): ComfyUI lanes enter
//! as static per-device `reserved_mb` (R3-measured + margin). This module subtracts those
//! reservations from each device's capacity, holds back headroom, and returns a
//! [`VramBudget`] the existing [`crate::fit::fit`] can spend for the LLM slot.
//!
//! Budgets stay **per-device, never summed**. One process, one card.

use apexrouter_protocol::{
    DeviceBudget, ServiceRuntime, ServiceSpec, StudioBudget, StudioDeviceBudget, VramBudget,
};
use std::collections::BTreeMap;

/// Default headroom held back on every device after Comfy reservations, MiB.
///
/// Not a fit()-internal margin: this is the studio-level cushion so a slightly-fatter Wan
/// job cannot OOM-starve the co-resident llm on GPU0 (S7 / open question 5).
pub const DEFAULT_STUDIO_HEADROOM_MB: u64 = 1_024;

/// Plan per-device free VRAM for studio LLM `fit()`, given card capacities and the recipe's
/// service list.
///
/// * `device_capacity` — `(device_token, capacity_mb)` pairs. Capacity is usually the card's
///   total VRAM at rent time (or free-at-plan). Devices not listed but referenced by a
///   service are synthesised with `capacity_mb = 0` and a note.
/// * `services` — the recipe's [`ServiceSpec`] list. Only non-zero `reserved_mb` rows
///   (Comfy lanes) reduce free; the LLM slot itself must keep `reserved_mb = 0` and be
///   solved by `fit()` against the remainder.
/// * `headroom_mb` — global cushion subtracted from every device (use
///   [`DEFAULT_STUDIO_HEADROOM_MB`] when the caller has no opinion).
///
/// Returns both the studio-native [`StudioBudget`] (for UIs / status) and a [`VramBudget`]
/// ready for [`crate::fit::fit`].
pub fn studio_budget(
    device_capacity: &[(String, u64)],
    services: &[ServiceSpec],
    headroom_mb: u64,
) -> (StudioBudget, VramBudget) {
    let mut notes: Vec<String> = Vec::new();
    let mut caps: BTreeMap<String, u64> = BTreeMap::new();
    for (dev, mb) in device_capacity {
        caps.insert(dev.clone(), *mb);
    }

    // Σ reserved_mb per device from static lanes (Comfy / anything with a reservation).
    let mut reserved: BTreeMap<String, u64> = BTreeMap::new();
    for svc in services {
        if svc.reserved_mb == 0 {
            continue;
        }
        let mb = u64::from(svc.reserved_mb);
        let targets: Vec<String> = if svc.devices.is_empty() {
            notes.push(format!(
                "service `{}` reserves {mb} MiB but names no devices — attributed to no card",
                svc.name
            ));
            Vec::new()
        } else {
            svc.devices.clone()
        };
        for dev in targets {
            if !caps.contains_key(&dev) {
                notes.push(format!(
                    "service `{}` names device `{dev}` which has no capacity entry — treating capacity as 0",
                    svc.name
                ));
                caps.entry(dev.clone()).or_insert(0);
            }
            *reserved.entry(dev).or_default() = reserved
                .get(dev.as_str())
                .copied()
                .unwrap_or(0)
                .saturating_add(mb);
            if matches!(svc.runtime, ServiceRuntime::ComfyUi) {
                // already counted; note once per service is enough
            }
        }
        notes.push(format!(
            "reserved {} MiB for `{}` ({:?}) on devices {:?}",
            svc.reserved_mb, svc.name, svc.runtime, svc.devices
        ));
    }

    // Ensure every capacity device appears even with zero reservation.
    for dev in caps.keys() {
        reserved.entry(dev.clone()).or_insert(0);
    }

    let mut devices: Vec<StudioDeviceBudget> = Vec::new();
    for (dev, capacity_mb) in &caps {
        let res = reserved.get(dev).copied().unwrap_or(0);
        let free = capacity_mb.saturating_sub(res).saturating_sub(headroom_mb);
        if res > *capacity_mb {
            notes.push(format!(
                "device `{dev}`: reservations ({res} MiB) exceed capacity ({capacity_mb} MiB) — free_for_llm is 0"
            ));
        }
        notes.push(format!(
            "device `{dev}`: capacity {capacity_mb} − reserved {res} − headroom {headroom_mb} = {free} MiB for llm"
        ));
        devices.push(StudioDeviceBudget {
            device: dev.clone(),
            capacity_mb: *capacity_mb,
            reserved_mb: res,
            headroom_mb,
            free_for_llm_mb: free,
        });
    }
    devices.sort_by(|a, b| a.device.cmp(&b.device));

    let vram = VramBudget {
        devices: devices
            .iter()
            .map(|d| DeviceBudget {
                // free_mb is the raw capacity; reserved_mb carries the studio reservations.
                // fit() spends free − reserved − margin. We put headroom in margin_mb once.
                device: d.device.clone(),
                free_mb: d.capacity_mb,
                reserved_mb: d.reserved_mb,
            })
            .collect(),
        // Headroom is global margin so it is not double-counted per device inside fit().
        margin_mb: headroom_mb,
        host_ram_free_mb: 0,
        backend: None,
        notes: notes.clone(),
    };

    (StudioBudget { devices, notes }, vram)
}

/// Convenience: budget for the R3 dual-48GB posture (two devices `"0"`/`"1"`, 48 GiB each).
pub fn studio_96gb_device_caps() -> Vec<(String, u64)> {
    // 48 GiB ≈ 49152 MiB; R3 cards are modded-48GB 4090s.
    vec![("0".into(), 48 * 1024), ("1".into(), 48 * 1024)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_protocol::{
        studio_96gb_services, ServiceHealthProbe, ServiceRouting, ServiceRuntime, ServiceSpec,
        STUDIO_IMAGE_PORT, STUDIO_VIDEO_PORT,
    };
    use std::collections::BTreeMap;

    #[test]
    fn r3_posture_leaves_llm_room_on_gpu0_after_video_reservation() {
        let services = studio_96gb_services();
        let (plan, vram) = studio_budget(
            &studio_96gb_device_caps(),
            &services,
            DEFAULT_STUDIO_HEADROOM_MB,
        );

        let gpu0 = plan.devices.iter().find(|d| d.device == "0").expect("gpu0");
        let gpu1 = plan.devices.iter().find(|d| d.device == "1").expect("gpu1");

        // video 23000 on 0, image 32000 on 1, headroom 1024 each side via margin
        assert_eq!(gpu0.reserved_mb, 23_000);
        assert_eq!(gpu1.reserved_mb, 32_000);
        // 49152 - 23000 - 1024 = 25128
        assert_eq!(gpu0.free_for_llm_mb, 48 * 1024 - 23_000 - 1_024);
        // 49152 - 32000 - 1024 = 16128
        assert_eq!(gpu1.free_for_llm_mb, 48 * 1024 - 32_000 - 1_024);

        // VramBudget total usable = sum(capacity - reserved) - margin
        // (49152-23000) + (49152-32000) - 1024 = 26152 + 17152 - 1024 = 42280
        assert_eq!(
            vram.total_usable_mb(),
            gpu0.free_for_llm_mb + (gpu1.capacity_mb - gpu1.reserved_mb)
        );
        assert!(!plan.notes.is_empty());
    }

    #[test]
    fn budgets_never_sum_devices_into_one_pool_silently() {
        // A single-device service must not invent free VRAM on a sibling card.
        let services = vec![ServiceSpec {
            name: "video".into(),
            runtime: ServiceRuntime::ComfyUi,
            port: 8188,
            devices: vec!["0".into()],
            reserved_mb: 23_000,
            env: BTreeMap::new(),
            health: ServiceHealthProbe::ComfySystemStats,
            routing: ServiceRouting::LocalOnly,
            fit: None,
            local_port: Some(STUDIO_VIDEO_PORT),
        }];
        let caps = vec![("0".into(), 48 * 1024), ("1".into(), 48 * 1024)];
        let (plan, _) = studio_budget(&caps, &services, 0);
        let gpu1 = plan.devices.iter().find(|d| d.device == "1").unwrap();
        assert_eq!(gpu1.reserved_mb, 0);
        assert_eq!(gpu1.free_for_llm_mb, 48 * 1024);
    }

    #[test]
    fn over_reserved_device_saturates_to_zero_not_underflow() {
        let services = vec![ServiceSpec {
            name: "image".into(),
            runtime: ServiceRuntime::ComfyUi,
            port: 8189,
            devices: vec!["1".into()],
            reserved_mb: 99_000,
            env: BTreeMap::new(),
            health: ServiceHealthProbe::ComfySystemStats,
            routing: ServiceRouting::LocalOnly,
            fit: None,
            local_port: Some(STUDIO_IMAGE_PORT),
        }];
        let (plan, _) = studio_budget(&[("1".into(), 48 * 1024)], &services, 1_024);
        assert_eq!(plan.devices[0].free_for_llm_mb, 0);
        assert!(plan.notes.iter().any(|n| n.contains("exceed")));
    }
}
