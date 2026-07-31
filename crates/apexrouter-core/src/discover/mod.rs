//! OWNER: unit C-09 (core/discover/builds.rs, core/discover/mod.rs). Do not edit outside
//! that unit — `models.rs` and `gguf.rs` belong to C-10, `physical.rs` to C-11b.
//!
//! Discovery: what llama.cpp builds exist, what devices they can see, and what weights are
//! on disk. Everything here is a **list**; nothing is singular.
//!
//! A device is a list *twice over*: one entry per backend that can see it. `physical`
//! answers which of those entries are the same silicon, which is what stops the fit solver
//! spending one card's VRAM twice.

pub mod builds;
pub mod gguf;
pub mod models;
pub mod physical;

pub use builds::{choose_build, discover_builds, probe_devices, probe_flags};
pub use gguf::read_gguf_meta;
pub use models::discover_models;
pub use physical::{attach_pci_ids, scan_pci_gpus, GpuVendor, PciGpu};
