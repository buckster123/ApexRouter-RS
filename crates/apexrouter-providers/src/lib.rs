//! ApexRouter-RS provisioners: **how backends come to exist.**
//!
//! The local `llama-server`/vLLM supervisor, vast.ai REST + boot + tunnel, together.ai,
//! HuggingFace, and the plain OpenAI-compatible node. Provider-specific `Check`s live here
//! and are registered by `apexrouter-server` at startup, which is what lets `core::checks`
//! define the trait without `core` ever depending on this crate.
//!
//! Stage 0 skeleton: every module below is a stub owned by a later work unit. See
//! `docs/BUILD-PLAN.md` §5 for the ownership index.

#![allow(unused)]

pub mod checks;
pub mod compare;
pub mod hf;
pub mod local;
pub mod smoke;
pub mod ssh;
pub mod together;
pub mod vast;

pub use local::{DownMode, LaunchError, LaunchPlan, LocalProvisioner, Provisioner};
