# ApexRouter-RS — the Slint app

> **Status: as-built.** Cross-checked against `crates/apexrouter-slint/**` on 2026-07-31.
> `docs/ARCHITECTURE.md` §11 is the normative design; this file is the operational companion —
> what the tree actually contains, how the thread model works in practice, which web-UI panel
> became which Slint screen, what is deliberately missing, and **how to run it headless**, which
> costs an hour to rediscover.

`apexrouter-ui` is the native GUI. It is an **edge client of the same HTTP control plane** the
web UI, the CLI and the MCP server use: the only first-party crates it links are
`apexrouter-protocol` and `apexrouter-client` — **never `apexrouter-core`**. There is no second
business-logic path, no second state model, and no code in this crate that talks to a process, a
file or a GPU.

---

## 1. Where it sits

```
                          ┌──────────────────────────────────────────────┐
                          │  apexrouter serve   (the daemon, one process)│
                          │    :8888 proxy        :2739 control          │
                          └──────────────────────────────────────────────┘
                                 ▲                    ▲          ▲
                    GET /v1/snapshot        GET /ws (Event)   SSE: /v1/diagnose,
                    PUT /v1/routes …        snapshot-on-      /v1/backends/{id}/logs,
                    every mutation          connect            /v1/vast/instances/{id}/log
                                 │                    │          │
                          ┌──────┴────────────────────┴──────────┴───────┐
                          │              apexrouter-client               │
                          │        NodeClient  (reqwest + tungstenite)   │
                          └──────────────────────┬───────────────────────┘
                                                 │  Bridge  (src/api.rs)
              tokio multi-thread runtime ────────┤    fetch() / act() / spawn_ws()
              (2 worker threads, built by hand)  │    follow_checks() / follow_logs()
                                                 │
                                     upgrade_in_event_loop
                                                 │
                          ┌──────────────────────▼───────────────────────┐
                          │  Slint event loop — OWNS THE MAIN THREAD     │
                          │  AppWindow  ─ global State ─ 8 screens       │
                          │  Store (Arc<Mutex<…>>): last Snapshot + log  │
                          │  ring, so a re-render never needs the network│
                          └──────────────────────────────────────────────┘
```

Two data paths, exactly as in the web UI: **REST for first paint, WebSocket for everything
after.** `main` calls `bridge.refresh()` (`GET /v1/snapshot`), then `refresh_local_models()`,
then `spawn_ws()`. Long-running reads that are not the snapshot — the check registry, a log
follow, an instance boot log — are **SSE** streams folded in one row at a time so a run that
hangs on probe three still shows probes one and two.

---

## 2. Thread model

Five rules. Breaking any of them shows up as a frozen window rather than a compile error.

1. **`#[tokio::main]` appears nowhere in this crate.** It takes the main thread away from the
   event loop. `main` builds the runtime by hand, keeps it alive for the app's lifetime, and
   ends with `ui.run()?`:

   ```rust
   fn main() -> anyhow::Result<()> {
       let rt = tokio::runtime::Builder::new_multi_thread()
           .worker_threads(2).enable_all().build()?;
       let ui = AppWindow::new()?;
       let bridge = Bridge::new(NodeClient::new(control_url(), control_token()),
                                rt.handle().clone(), ui.as_weak());
       // … wire callbacks …
       ui.run()?;                      // Slint owns the main thread from here
       Ok(())
   }
   ```

2. **Properties are read on the UI thread; work is `handle.spawn()`ed; results come back through
   `weak.upgrade_in_event_loop(move |ui| …)`.** Every callback is wired inside a braced block
   that captures `ui.as_weak()` plus a `Bridge` clone, so nothing borrows across the await.

3. **One inner `async { … }.await` per operation**, so a single `match` handles every failure and
   there is exactly one place a toast is raised. That is `Bridge::fetch(what, job, apply)` for a
   read and `Bridge::act(what, job)` for a write; `act` toasts the daemon's own message and then
   re-pulls the snapshot, so the screen shows **what happened**, not what was asked for.

4. **`Store` is `Arc<Mutex<…>>` and holds the last `Snapshot` plus a log ring.** Re-rendering a
   filter, a device selection or a log grep never touches the network.

5. **The WS task reconnects forever.** A failed `subscribe()` sleeps 5 s and retries; a stream
   that ends sleeps 2 s, re-subscribes and re-pulls the snapshot. `State.connected` drives the
   connection dot, exactly like the web UI's.

`Bridge` is `Clone`; the `log_generation: AtomicU64` inside it is how a second `follow_logs`
invalidates the first, so switching backends mid-follow cannot interleave two logs.

---

## 3. The tree

| Path | What it is |
|---|---|
| `build.rs` | one line: `slint_build::compile("src/ui/appwindow.slint")` |
| `src/main.rs` | `fn main`, then one `wire_*` function per screen. All callbacks, no rendering |
| `src/api.rs` | `Bridge`, `Store`, and the pure `protocol → *Row` mappers (`backend_rows`, `offer_rows`, `fit_view`, …). Unit-testable without a window |
| `src/ui/appwindow.slint` | the root: router bar, rig strip, alert band, nav, screen switch, boot drawer, toast |
| `src/ui/state.slint` | **`export global State`** — the entire Rust↔UI contract in one global |
| `src/ui/types.slint` | the 20 row structs (`BackendRow`, `OfferRow`, `CheckRow`, …) |
| `src/ui/palette.slint` | `export global Palette` — **the only file that may contain a colour literal**; verified: zero `#rrggbb` outside it |
| `src/ui/components/*.slint` | `card`, `badge`, `meter`, `table`, `logview`, `drawer`, `widgets` |
| `src/ui/screens/*.slint` | `dashboard`, `routes`, `backends`, `launch`, `fleet`, `catalog`, `providers`, `doctor` |

**Why one `State` global instead of threading properties through `AppWindow`:** with eight
write-capable screens, that plumbing would be most of the file and all of the bugs. Screens read
`State.x` and call `State.y()` directly; Rust reaches them as `ui.global::<State>().set_x(…)` /
`.on_y(…)`. Slint's kebab-case maps to Rust snake_case — `base-url` → `get_base_url` /
`set_base_url`. Convention on that global: **every callback is a write unless its name starts
with `load-` or `refresh-`.** This app is not a viewer.

`Palette` carries the eleven web `:root` tokens byte-identically — `#0d0d0d` page, `#1a1a19`
surface, `#2c2c2a` hairline, `#ffffff` ink, `#c3c2b7` ink-2, `#898781` muted, `#3987e5` accent,
`#0ca30c` good, `#fab219` warn, `#ec835a` serious, `#d03b3b` critical — plus derived tints at the
same hues, the radius/metric scale, and `level-color(l)` / `level-tint(l)`. **Status colour is
reserved for health, never for identity**, and every badge pairs an icon with a label so colour
is never the only channel.

---

## 4. Web → Slint port map

The web UI is a router bar + a rig strip + eight tabs + two drawers. The Slint app is a router
bar + a rig strip + **eight screens** + one drawer. They are not the same eight, and this table
is why.

| Web UI | Slint | Notes on the port |
|---|---|---|
| Router bar (base URL, copy, `OPENAI_API_KEY=not-needed`, connection dot, in-flight, req/min, tok/s, 24 h spend, default-alias dropdown) | **same band, always visible** (`appwindow.slint`) | `Chooser` replaces `<select>`; copy writes the clipboard through Slint |
| Rig strip (one bar per device, click to filter Backends) | **same band, always visible** | `Meter` per device; clicking a device sets `State.device-filter` and jumps to Backends |
| Alerts band | **same band**, `Banner` rows | dismiss is a `State` callback |
| *(no equivalent — the stats live in the bar)* | **Dashboard** screen | stat tiles, backend cards, jobs pane, and the **live-request ticker** the web keeps in its own tab |
| **Routes** tab | **Routes** screen | list + editor pane; targets reorder with ↑/↓ buttons instead of drag (§5) |
| **Backends** tab | **Backends** screen | card list + detail pane + `LogView` with follow mode and a filter field |
| **Live requests** tab | folded into **Dashboard** | one ticker table, same columns, same "prompts are not captured unless `capture_bodies` is on" line |
| **Launch** drawer (3 tabs: Local / vLLM / Rent) | **Launch** *screen* (same 3 tabs) | a drawer overlaying a dense desktop window buys nothing; the boot view is still a drawer (below) |
| **Fleet & cost** tab | **Fleet** screen | instances, uptime, accrued cost, burn, stall banner + Restart download, tunnel toggle, always-visible Destroy |
| **Catalog** tab | **Catalog** screen | recipes + profiles + local models + HF repo/quant fetch (§5) |
| **Providers** tab | **Providers** screen | masked key entry, credential **source**, Test, live catalogue with Activate |
| **Usage** tab + **Doctor** tab | **one "Usage / Doctor"** screen | usage window/grouping + totals + a `BarRow` per bucket, then the check registry and the four smoke probes |
| **Edit** drawer (JSON editor) | inline editor panes | Routes/Catalog/Providers edit in place; no modal |
| *(the drawer becomes the boot view)* | **boot drawer** in `appwindow.slint` | `BootPhase`, elapsed timer, log stream, Destroy — the same "there is no separate Watch-boot screen" rule |
| toasts | `State.toast` + level | one toast surface, four levels |

Nothing that **spends, destroys, launches, routes or authors** is missing. Both GUIs can rent a
box, destroy one, start and stop endpoints, re-point aliases, author recipes and profiles, set
provider credentials and download weights.

---

## 5. Honest deferrals

Cosmetic only, and each is a deliberate trade rather than a gap.

| Web UI has | Slint has instead | Why |
|---|---|---|
| Drag-to-reorder route targets | **↑ / ↓ buttons** on each target row | Slint has no drag-and-drop model list; buttons are one keystroke and are testable |
| Stacked inline-SVG usage charts (tokens/day, $/day by provider) | **a `BarRow` per bucket** with the numbers beside it | hand-rolled SVG is a browser affordance; a labelled bar row carries the same information without a chart engine |
| HF **search browser** (query → results → per-file sizes → Download) | **repo + quant fields**, plus "open web UI" for browsing | searching is a browsing activity; fetching a known quant is the launch-path step, and that is kept |
| Live-reload of the UI from `[server] ui_dir` | n/a — the UI is compiled in | `build.rs` compiles `.slint`; a rebuild is the reload |

`GET /metrics` is not surfaced in either GUI (see `docs/ARCHITECTURE.md` §4.5 — the exposition is
implemented in `Telemetry::prometheus` and not yet mounted).

---

## 6. Building and running

```sh
# The crate is OUT of `default-members`, so plain `cargo build` / `clippy` / `test`
# never link the Slint ecosystem. Ask for it explicitly:
cargo build -p apexrouter-slint                 # -> target/debug/apexrouter-ui
cargo clippy -p apexrouter-slint --all-targets -- -D warnings

# It is GPL-3.0-only (Slint's GPL option) and `publish = false`. The rest of the
# workspace stays MIT OR Apache-2.0 — see docs/LICENSING.md.
```

Features: `default = ["winit"]`. A `linuxkms` line sits commented in `Cargo.toml` for a
pure-ApexOS KMS/DRM deployment with no compositor.

Run it against a daemon:

```sh
apexrouter serve --detach          # or leave it running in a terminal
apexrouter-ui                      # discovers the control plane from
                                   # $APEXROUTER_URL, else 127.0.0.1:2739
```

`control_url()` resolves in three steps: `$APEXROUTER_URL`, else **`[server] control_bind` read
from the config file the daemon itself would load**, else `http://127.0.0.1:2739`.
`control_token()` reads `$APEXROUTER_TOKEN`.

Reading the configured bind is not a nicety. Moving the control port in `config.toml` used to
leave this app pointed at `127.0.0.1:2739` with nothing behind it, and the only symptom was "not
connected" — a debugging cycle spent on a value that was written down the whole time. The config
path is resolved by mirroring `ARCHITECTURE.md` §5.1 (`$APEXROUTER_CONFIG` → `$APEXROUTER_HOME/
config.toml` → `$XDG_CONFIG_HOME/apexrouter/config.toml` → `~/.config/apexrouter/config.toml`),
and the one key is parsed by hand rather than by taking a TOML dependency.

It still does **not** read the lock file's owner record the way the CLI does — that would need
`apexrouter-core`, which this GPL crate may not link. `$APEXROUTER_URL` remains the override and
still wins. With no daemon the window still opens, the connection dot is red, and a toast says
why — it never blocks on a socket.

---

## 7. Running it headless (Xvfb) — verified

**Read this before trying to screenshot the app from an agent session.** Under a Wayland
session, winit prefers Wayland and **silently opens the window on the real desktop**, where an
X11 capture of the virtual display sees nothing at all — no error, an empty PNG, and a confused
half hour. The fix is to unset `WAYLAND_DISPLAY` *and* force the winit backend:

```sh
# 1. a virtual display
Xvfb :99 -screen 0 1600x1000x24 &
sleep 1

# 2. the app — BOTH the -u and the WINIT_UNIX_BACKEND matter
env -u WAYLAND_DISPLAY DISPLAY=:99 WINIT_UNIX_BACKEND=x11 \
    ./target/debug/apexrouter-ui &
sleep 6                                    # first paint = REST snapshot + WS connect

# 3. capture. `import`/`xwd`/`scrot` are NOT installed on this box; ffmpeg is.
ffmpeg -loglevel error -f x11grab -video_size 1600x1000 -i :99 \
       -frames:v 1 -y /tmp/apexrouter-ui.png
```

Verified on 2026-07-31 against a live daemon: the window opens, the router bar shows
`http://127.0.0.1:8888`, the rig strip shows both enumerations of the one Radeon 840M (`ROCm0`
2.2 GiB free of 11.1 GiB, `Vulkan0` 10.9 GiB free of 20.5 GiB — *never add those together*, see
ARCHITECTURE §3.2.1), and the dashboard lists the running `Carnice-9b-Q6_K` backend.

Notes that save a retry:

- The window is **1280×800** by default; size the Xvfb screen at least that or the capture is
  cropped.
- `sleep 6` is not superstition — first paint is a REST round-trip plus a WS handshake. A
  capture at 1 s gets an empty shell.
- The process does not exit on its own. Background it and `kill` it, or wrap it in `timeout`.
- No `$DISPLAY` at all: the app exits with a winit error on stderr, not a panic.

---

## 8. Rules that keep the two GUIs one product

- **No business logic here.** If a screen needs a computed value, the daemon computes it and
  ships it in the `Snapshot`, or `src/api.rs` maps it purely. Anything else is a second source
  of truth, and there is exactly one bug that pattern produces.
- **No colour outside `palette.slint`.**
- **`ModelRc::new(VecModel::from(rows))` to replace a list**; the one exception is the route
  editor's target list, which is mutated in place because reordering is the operation and a
  wholesale replace loses the scroll position.
- **Every mutation goes through `Bridge::act`**, which re-pulls the snapshot. A screen never
  mutates its own model to reflect a write it merely requested.
- **The crate must not depend on `apexrouter-core`.**

### 8.1 These are not enforced by CI — check them by hand

`.github/workflows/ci.yml` runs `fmt`, a shell-out grep, clippy over the **seven headless
crates**, `cargo test --workspace --exclude apexrouter-slint` and `cargo build --release`. With
the crate out of `default-members`, **CI never builds this crate at all**, so nothing above is
machine-checked today. Several doc comments in `src/` say "CI greps for this"; as of 2026-07-31
that is aspirational, and these four commands are what actually verify it:

```sh
! grep -rn 'tokio::main' crates/apexrouter-slint/src            # rule 1
! grep -rn '#[0-9a-fA-F]\{6\}' crates/apexrouter-slint/src/ui --include=*.slint \
      | grep -v palette.slint                                    # no stray colour
! cargo tree -p apexrouter-slint -e normal | grep apexrouter-core  # edge client only
cargo clippy -p apexrouter-slint --all-targets -- -D warnings      # it compiles clean
```

All four pass as of this writing. (A handful of `border-radius: 2px|3px|4px` literals do live in
`components/{meter,badge,widgets}.slint`; colours do not. Radii are shape, not identity, so the
grep above is deliberately about colour only.) Wiring these into CI means adding an
`apexrouter-slint` job with `libfontconfig1-dev` and the Slint toolchain — a real cost, and the
whole point of charter decision **D12** (`docs/CHARTER.md`) is that the ordinary build never pays
it. So the honest position is: these are hand-checked invariants, and this is the checklist.
