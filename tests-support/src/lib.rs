//! Test doubles for ApexRouter-RS: a fake `llama-server` and a stub OpenAI upstream.
//!
//! Two things, one implementation. The same HTTP surface runs either as a **subprocess**
//! called `llama-server`, which the real supervisor discovers, spawns and health-gates, or
//! as an **in-process** stub bound to a loopback port, which a routing table can point an
//! alias at. Neither needs a GPU, a GGUF or 7 GB of page cache; both start in
//! milliseconds.
//!
//! ```text
//! FakeBuild  ── a llama.cpp-shaped build tree + the binary   → supervisor tests
//! Stub       ── the same server, in this process, on :0      → router tests
//! Control    ── talk to a fake the supervisor started        → both
//! ```
//!
//! # Spawn a fake endpoint under the real supervisor
//!
//! ```no_run
//! # use apexrouter_tests_support::{FakeBuild, GgufSpec};
//! let fake = FakeBuild::new();
//! let model = fake.model("Fake-9b-Q6_K.gguf", &GgufSpec::default());
//!
//! // Hand the supervisor a rig instead of letting it probe the real machine.
//! // provisioner.set_rig(fake.rig(20_992, 19_518));
//! // ...then `plan()` + `up()` with `LocalLlamaSpec { build: fake.build_id(), .. }`.
//!
//! // Afterwards: what did the child actually get?
//! // let rec = fake.records().for_port(port).expect("launch record");
//! // assert_eq!(rec.flag("-c"), Some("32768"));
//! # let _ = model;
//! ```
//!
//! # Point an alias at a stub upstream
//!
//! ```no_run
//! # use apexrouter_tests_support::{routing, Stub};
//! let upstream = Stub::with("echo");
//! let backend = upstream.backend("stub-a");            // an apexrouter_protocol::Backend
//! let file = routing::route_file("auto", vec![routing::route("auto", &["stub-a"])]);
//! // registry.upsert(backend, &cfg.router);
//! // let table = TableBuilder::compile(&cfg, &file, registry)?;
//! # let _ = (backend, file);
//! ```
//!
//! See `tests-support/README.md` for the copy-paste versions with the two lines of
//! router wiring filled in.
//!
//! **Nothing here measures anything.** Every `timings` block the fake emits is fabricated
//! from a configured token rate; no number produced by this crate is a benchmark.

#![deny(missing_docs)]

pub mod behavior;
pub mod control;
pub mod fake;
pub mod gguf;
pub mod http;
pub mod record;
pub mod routing;
pub mod server;
pub mod stub;

pub use behavior::{Behavior, Toggle};
pub use control::Control;
pub use fake::{binary_path, FakeBuild};
pub use gguf::GgufSpec;
pub use record::{LaunchRecord, Records};
pub use server::{RecordedRequest, BUILD_INFO};
pub use stub::Stub;
