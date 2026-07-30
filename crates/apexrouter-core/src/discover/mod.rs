//! OWNER: unit C-09 (core/discover/builds.rs, core/discover/mod.rs). Do not edit outside
//! that unit — `models.rs` and `gguf.rs` belong to C-10.
//!
//! Discovery: what llama.cpp builds exist, what devices they can see, and what weights are
//! on disk. Everything here is a **list**; nothing is singular.

pub mod builds;
pub mod gguf;
pub mod models;

pub use builds::{choose_build, discover_builds, probe_devices, probe_flags};
pub use gguf::read_gguf_meta;
pub use models::discover_models;
