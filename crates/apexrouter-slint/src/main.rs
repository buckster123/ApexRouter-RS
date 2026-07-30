//! OWNER: unit U-02 (crates/apexrouter-slint/src/**, except build.rs). Do not edit outside
//! that unit.
//!
//! `apexrouter-ui` — the native app. **An edge client of the same HTTP API; there is no
//! second business-logic path.** It links `apexrouter-protocol` and `apexrouter-client`
//! only, and CI asserts with `cargo tree` that it does not link `apexrouter-core`.
//!
//! **Thread model: never `#[tokio::main]`** (CI greps for it in this crate). `main` builds a
//! multi-thread runtime by hand, keeps it alive for the app's lifetime, and ends with
//! `ui.run()?` — Slint owns the main thread. Each callback captures `ui.as_weak()` plus
//! `rt.handle().clone()`; properties are read on the UI thread, work is `handle.spawn()`ed,
//! and results come back via `weak.upgrade_in_event_loop(move |ui| …)`.

#![allow(unused)]

mod api;

slint::include_modules!();

fn main() -> anyhow::Result<()> {
    // U-02 replaces this with the runtime + callback wiring. The shape below is the
    // contract: build the runtime by hand, keep it alive, and give Slint the main thread.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let _guard = rt.handle().clone();

    let ui = AppWindow::new()?;
    ui.run()?;
    Ok(())
}
