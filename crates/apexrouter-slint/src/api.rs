//! OWNER: unit U-02 (crates/apexrouter-slint/src/**, except build.rs). Do not edit outside
//! that unit.
//!
//! The `NodeClient` glue: one background task holds the `/ws` subscription and pushes
//! `Event`s into the Slint event loop, so the app renders the same `Snapshot` as the web UI
//! with **zero polling**.
//!
//! All fallible async work goes in one inner `async { … anyhow::Ok(v) }.await`, so a single
//! `match` handles every failure rather than one per call site.
