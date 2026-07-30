//! ApexRouter-RS wire and domain types. Serde only: no I/O, no tokio, no reqwest.
//! Every surface (daemon, CLI, MCP, web UI, Slint) deserializes the same types the daemon
//! serializes. No frontend ever string-matches a status.
//!
//! House rules baked into every type here:
//!
//! * `#[serde(rename_all = "snake_case")]` on every enum, `#[serde(tag = …)]` where the
//!   document says so, and `#[serde(deny_unknown_fields)]` **nowhere** — we must survive
//!   additive changes made by an older or newer peer.
//! * `PartialEq` everywhere, so the daemon can suppress no-op broadcasts.
//! * `#[serde(default)]` on additive `Vec` and `Option` fields.
//! * Honesty types, not booleans: [`CostEstimate`] can say `Unknown`, [`TokenCount`] can say
//!   `Estimated`. A guess must never render as a fact.
//! * No key material. [`CredentialSource`] is a *description* of where a credential lives;
//!   `Secret` itself lives in `apexrouter-core`.

pub mod backend;
pub mod catalog;
pub mod check;
pub mod endpoint;
pub mod event;
pub mod fit;
pub mod hf;
pub mod ids;
pub mod money;
pub mod provider;
pub mod rig;
pub mod route;
pub mod telemetry;
pub mod vast;

pub use backend::*;
pub use catalog::*;
pub use check::*;
pub use endpoint::*;
pub use event::*;
pub use fit::*;
pub use hf::*;
pub use ids::*;
pub use money::*;
pub use provider::*;
pub use rig::*;
pub use route::*;
pub use telemetry::*;
pub use vast::*;

// ---------------------------------------------------------------------------
// The one name collision in the published contract, resolved once, here.
//
// `rig::Backend` is the *compute* backend of a llama.cpp build (Vulkan, CUDA, …).
// `backend::Backend` is a *live upstream* in the routing table. Both are published
// under those module paths and both keep them. At the crate root the bare name
// `Backend` means the upstream struct, because that is what `Snapshot.backends`,
// `BackendRegistry` and every surface refer to; the GPU enum is additionally
// re-exported at the root as `GpuBackend`.
//
// An explicit `use` shadows a glob `use`, so these two lines disambiguate the two
// globs above rather than colliding with them.
// ---------------------------------------------------------------------------
pub use backend::Backend;
pub use rig::Backend as GpuBackend;

/// The product name. Appears in `/health`, the owner record and the `Via` header.
pub const PRODUCT: &str = "apexrouter";
/// The workspace version, compiled in.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Default bind for the proxy / data plane. Baked into agent configs — do not change it.
pub const DEFAULT_PROXY_BIND: &str = "127.0.0.1:8888";
/// Default bind for the control plane. `APEX` on a phone keypad.
pub const DEFAULT_CONTROL_BIND: &str = "127.0.0.1:2739";
/// Port pool for locally supervised `llama-server` processes.
pub const DEFAULT_LOCAL_PORT_RANGE: (u16, u16) = (8100, 8199);
/// Port pool for `ssh -L` tunnels to rented boxes. Multiple rentals is the normal case.
pub const DEFAULT_TUNNEL_PORT_RANGE: (u16, u16) = (8800, 8899);
/// Model names that ALWAYS fall through to the default alias, so smoke.sh's hardcoded
/// `"model":"x"` and an absent model field keep working regardless of `unknown_model`.
pub const LEGACY_MODEL_NAMES: &[&str] = &["", "x", "auto", "default"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_are_the_documented_ones() {
        assert_eq!(PRODUCT, "apexrouter");
        assert_eq!(DEFAULT_PROXY_BIND, "127.0.0.1:8888");
        assert_eq!(DEFAULT_CONTROL_BIND, "127.0.0.1:2739");
        assert_eq!(DEFAULT_LOCAL_PORT_RANGE, (8100, 8199));
        assert_eq!(DEFAULT_TUNNEL_PORT_RANGE, (8800, 8899));
        assert_eq!(LEGACY_MODEL_NAMES, &["", "x", "auto", "default"]);
    }

    #[test]
    fn root_backend_is_the_upstream_and_gpubackend_is_the_enum() {
        // Compile-time proof that the glob collision is resolved in the documented direction.
        fn _upstream(_: &Backend) {}
        fn _gpu(_: &GpuBackend) {}
        let g: GpuBackend = GpuBackend::Vulkan;
        assert_eq!(g, rig::Backend::Vulkan);
    }
}
