# CLAUDE.md — ApexRouter-RS maintainer's brief

You are working on ApexRouter-RS: a local inference manager and OpenAI-compatible proxy, ported
from LocalRouter (Python TUI) in one multi-agent build. This file is that build's expertise,
distilled for the next agent.

**Read in this order:** `docs/ARCHITECTURE.md` (normative — where it disagrees with the code, one
of them is a bug and you must say which), then `docs/CHARTER.md` (**the decisions log D1–D18 is
binding**; amend it with a dated entry, never silently), then `docs/BUILD-PLAN.md` §4 for the unit
you are implementing. `docs/port/00-machine-ground-truth.md`, `00b`, `00c` and `08` outrank
everything above them.

## Architecture in one breath

`apexrouter serve` is one process with **two listeners**. The **proxy** (`127.0.0.1:8888`) is the
product: it holds an `ArcSwap<RoutingTable>` of named aliases → ordered chains of live
OpenAI-compatible backends, and relays bytes verbatim. The **control plane** (`127.0.0.1:2739` —
`APEX` on a phone keypad) serves `/v1/*` REST, `/ws`, `/metrics` and the embedded web UI. Eight
crates: `apexrouter-protocol` is the wire (serde only, no I/O — **every surface deserializes the
same enums the daemon serializes; never string-match, never invent a shape**); `-core` is all
filesystem/process/config logic so the CLI works daemon-less; `-router` is the request path;
`-providers` is vast.ai / Hugging Face / together.ai / ssh / llama-server supervision; `-server` is
both listeners; `-client` is the SDK; `-cli` is `apexrouter` plus the `mcp` stdio server; `-slint`
is a GPL edge client, deliberately out of `default-members`. Everything the manager does — finding
llama.cpp builds and GGUFs, solving what fits in VRAM, renting a GPU, tunnelling it home — exists to
**put rows in that table and keep them honest**.

The promise: **point every agent at `http://127.0.0.1:8888/v1` and never change it again.**

## Invariants — break these and you've broken the product

These are `ARCHITECTURE.md` §0.1, verbatim in intent.

1. **One resolver.** There is exactly one `resolve()` (`router/src/resolve.rs`) and every surface
   calls it. LocalRouter had four implementations of "what is active" that disagreed. The answer is
   observable on every response as `X-ApexRouter-Route: <alias>|<reason>`.
2. **The request path never touches the filesystem.** `ArcSwap<RoutingTable>` plus a per-backend
   `Arc<LiveBackend>`. No `stat()`, no TOML parse, no lock beyond one `Semaphore`. If you are about
   to read a file in `router/src/handler.rs`, you are writing the wrong patch.
3. **Persisted records hold facts, never status.** `pid`, `start_time_ticks`, `boot_id`, `port`,
   `argv`, `desired_state`. Liveness and health are *computed* on read. No `status: "running"`
   string ever goes to disk — it is a lie the moment someone types `kill`.
4. **Nothing that costs money is auto-destroyed, and nothing that costs money happens without a
   `SpendApproval`.** The ledger row is written **before** the billing call. A crash must never
   delete a paid box; a leak must be visible.
5. **State lives in one XDG state dir. Nothing is ever written into a repo directory.**
   `$APEXROUTER_HOME` → `~/.local/state/apexrouter`. Tests use `tempfile::TempDir`.

### And the three house rules that are not negotiable in this repo

- **Hermeticity.** No test may connect anywhere but `127.0.0.x`. Provider-probing tests use the
  shared `test_config()` with `[providers.together] base_url` pointed at a **closed loopback port**.
  This is not paranoia: the Stage-3 suite once made live authenticated calls to `api.together.ai`
  with the real `$TOGETHER_API_KEY`. Hermeticity was afterwards verified by `strace` — every
  `connect()` is loopback — and is now guarded by a test.
- **Money.** **Never call a vast.ai endpoint that creates, modifies or destroys an instance.**
  Read-only and nothing else: `PUT /api/v0/search/asks/`, `GET /api/v0/instances/`,
  `GET /api/v0/users/current/`. Andre's credit is **$7.72899** and must be exactly that when you
  finish. Boxes cost up to $3.34/hr; the failure mode (a GPU billing overnight with no local
  record) has already happened once in this codebase.
- **`~/.vastai-gguf/` is another tool's state directory.** Read it; never write it. Merely starting
  the daemon must never append to it — an acceptance run added 15 rows to the real `usage.log` and
  they had to be restored, which is why `[compat] mirror_usage_log` defaults **off**. If you need to
  exercise migration, copy the directory to a scratch dir, migrate the copy, and hash the original
  before and after.

## The invariant that keeps getting violated: **mount it, don't describe it**

**Three times** a control-plane module shipped implemented, unit-tested green and completely
unreachable, because its one `.merge(…)` line in `v1_routes()` was written as prose instead of as
code:

- Stage 4's `api::catalog` — all of `/v1/recipes*` and `/v1/profiles*` served `404`.
- Stage 5's `api::{vast, hf, providers, checks, compare}` — `/v1/checks` and `/v1/vast/account`
  `404`, `POST /v1/vast/instances` and `/v1/vast/offers/search` `405`, so `apexrouter doctor` and
  `apexrouter vast rent` could not run **at all**.

A unit test cannot catch this. Every one of those modules had passing tests: they build the
module's own `axum::Router` in isolation and never see the composed application.
`tests/openapi_routes.rs` compares two *documents* — source and OpenAPI — which agreed with each
other while the daemon served neither.

The guard is **`crates/apexrouter-server/tests/mounted_routes.rs`**. It recovers the inventory of
`pub fn router()` under `src/api/` **from the source tree at test time** (a hand-maintained list has
exactly the failure mode of the doc comments that caused the bug), boots the real daemon, and asks
axum itself — via the `Allow` header of a `405`, and a `403` from the mutation gate for presence —
which paths and methods the composed application serves. No handler ever runs, so it stays hermetic
while probing `/v1/vast/*`.

**`crates/apexrouter-server/src/lib.rs::v1_routes()` is the single merge point and only S-01 may
edit it.** Generalise the lesson: a capability is not shipped until something *outside* its own
module proves it is reachable. The same shape of bug is available in `cmd/mod.rs::dispatch`, in the
MCP `tools/list` inventory, and in the web UI's event handlers.

## Where things live

| Path | What |
|---|---|
| `crates/apexrouter-protocol/src/` | 73 structs, 38 enums. Serde only. Changing a shape here changes five surfaces at once. |
| `core/src/paths.rs` `config.rs` `secret.rs` `store.rs` `lockfile.rs` `proc.rs` `exec.rs` | Foundations: XDG resolution, the fully-defaulted `config.toml`, `Secret<String>`, atomic `tmp→fsync→rename` writes at `0600`, `flock` + owner record, `/proc` identity and adoption, argv-only spawn. |
| `core/src/discover/` `fit.rs` `argv.rs` | The rig: builds (glob `build*/bin/llama-server`), GGUF headers, physical-GPU dedup, the one `fit()` solver that replaced 54 hand-solved recipe strings, and the **one** argv/env builder. |
| `core/src/ledger.rs` `money.rs` `usage.rs` `pricing.rs` | Append-only `ledger.jsonl` where "active" is a *query*; `TokenCount::{Reported,Estimated}` and `CostEstimate::{Metered,Approximate,Unknown}` — the honesty types. A degraded record says so; it never guesses in silence. |
| `core/src/migrate.rs` `catalog.rs` `checks.rs` | `~/.vastai-gguf` import (stale state is the normal case, never an error), `toml_edit` round-trip so hand comments survive, the check registry `doctor` renders. |
| `router/src/` | `table` + `registry` (live state), `resolve` + `policy`, `relay/{headers,body,stream}`, `attempt` + `breaker` + `limits`, `errors` + `models`, `handler`, `compat` (the three legacy routes), `anthropic/` (ingress translation). |
| `providers/src/` | `local/` (llama-server supervisor), `vast/`, `hf.rs`, `together.rs`, `ssh.rs`, `checks/smoke/compare`. |
| `server/src/` | `lib.rs` (both listeners + `v1_routes`), `auth.rs` (mutation gate), `api/*` (one `router()` per module), `ws.rs`, `assets.rs` (rust-embed), `prober.rs`, `watcher.rs`, `jobs.rs`. |
| `cli/src/cmd/*` `cli/src/mcp/*` `cli/src/daemon.rs` | One file per noun; `daemon::Need::{Pure,ReadState,Mutate}` decides per command whether a daemon is required, served from `$STATE`, or autostarted. MCP tools are all prefixed `apexrouter_` (three MCP servers share `~/Projects/.mcp.json`). |
| `~/.local/state/apexrouter/` | `apexrouterd.lock`, `state.lock`, `routes.json`, `endpoints/<id>.json`, `backends.json`, `tunnels.json`, `catalog.toml`, `credentials.toml` (0600), `ledger.jsonl`, `usage.jsonl`, `jobs/`, `logs/`, `ssh/`, `approvals/`. |

Ports are baked into agent configs and do not move: proxy **8888**, control **2739**, local
llama-server **8100+**, Vast tunnel **8800+**.

## Sharp edges met and filed down (don't rediscover these)

- **`build-vulkan`'s trailing-colon `RUNPATH` picks up a sibling build's `.so`.** Every child gets
  `LD_LIBRARY_PATH = dirname(binary)` explicitly; `cwd` is `$STATE` and is never load-bearing.
- **axum 0.8 routes are `/{param}` and `/{*path}`**, not `:param`. And a catch-all `any()` route
  merged with the static-asset `get("/{*path}")` route **panics on `Router::merge`** ("Overlapping
  method route") — which is half of why there are two listeners. The proxy catch-all is registered
  as `.fallback(any(proxy_handler))`, never as a route.
- **`POST /health` used to return `405`** because an axum `MethodRouter` shadows the fallback. The
  proxy contract is: everything that is not one of five (path, method) pairs is proxied, `POST
  /health` included.
- **Backend detection by grepping `--help` was measured wrong** — it reported `cuda` on an AMD box.
  Use `llama-server --list-devices`, exclude `llvmpipe`, fall back to inspecting sibling
  `libggml-*.so`. (Ground truth has since moved: `llvmpipe` no longer enumerates at all.)
- **`~/llama.cpp/build` is a WORKING ROCm build; `build-rocm` is BROKEN.** Do not infer capability
  from a directory name. **ROCm reports free > total — never compute `total - free`.**
- **Vast's rent response returns the instance id as `new_contract`, not `id`.** Reading `id` gives
  you the *offer* id, and you have just created a billing row you cannot find again. This is
  `07` A1, the silent billing leak.
- **`/proc/<pid>/stat` must be parsed after the LAST `)`.** `comm` is arbitrary and contains
  spaces and parens; field 22 (`starttime`) is only findable by splitting on the final `)`. Related:
  `/proc/<pid>/stat` read between `fork()` and `execve()` still shows the *parent* thread's `comm` —
  that caused a load-dependent flaky test in Stage 1. And `boot_id` is part of process identity
  because `start_time_ticks` is measured since boot and is not comparable across a reboot.
- **`X-Usage` is emitted on buffered responses only.** Response headers flush before the first SSE
  chunk and usage arrives in the last one, so a streaming `X-Usage` would be absent or a lie. On
  streams we set `X-ApexRouter-Usage-Deferred: true` and the numbers land in `usage.jsonl`, the WS
  event and the live-request table. This is a **stated, tested divergence** from LocalRouter — see
  CHARTER D8 before you "fix" it. Its format is LocalRouter's `"{prompt}+{completion}"`, not JSON.
- **`[compat] legacy_proxy_pidfile` is a kill switch and defaults off.** LocalRouter's
  `_proxy_down()` reads `/tmp/vastai-gguf-proxy.pid` and SIGTERMs whatever it names; turning this on
  hands the old TUI's "Proxy → stop" menu item the ability to kill the whole daemon.
- **`/v1` normalisation must ADD a missing `/v1`, not only collapse duplicates.** Both
  `http://127.0.0.1:8888` and `.../v1` are valid client base URLs; `smoke.sh` appends `/v1` to
  whatever you give it, and LocalRouter's own SKILL.md told agents to use the form that 404s.
- **Never a total timeout on a stream.** `connect_timeout` 5 s + `headers_timeout` **600 s** (a
  non-streaming llama.cpp completion sends no headers until the body is ready) + an inter-chunk
  idle timeout 300 s. A clean EOF with no `data: [DONE]` is death, not success, and gets the
  synthetic error frame.
- **`/slots` is never proxied outward** — it echoes prompts. `403 redacted_endpoint`.
- **Headless Slint** needs `env -u WAYLAND_DISPLAY WINIT_UNIX_BACKEND=x11` under `Xvfb`, or winit
  prefers Wayland and silently opens on the real desktop where X11 capture sees nothing.
- **A loopback control plane is not a trust boundary.** A cross-origin `fetch` with
  `Content-Type: text/plain` is a CORS *simple request* — delivered without preflight, and the
  attacker never needs to read the response. Hence the `Host` + `Origin` + `Sec-Fetch-Site`
  mutation gate on **both** listeners, and no `CorsLayer` anywhere.
- **Never re-plan to describe something that already happened.** `GET /v1/endpoints/{id}/argv`
  called `supervisor.plan(&spec)` and so re-solved `fit()` against *currently* free VRAM: it served
  34 tokens where `/proc/<pid>/cmdline` had 36, describing a CPU-only launch for a fully-offloaded
  child, with `warnings` empty. A preview of a running process is rendered from its **record**
  (`ResolvedSpec::from_record` — the draft plus the record's `fit`, at the leased port, against the
  build the record names). And when you test it, compare against `/proc/<pid>/cmdline`, not against
  the other preview: with a daemon up, both routes are the same code and their agreement proves
  nothing.
- **`health_deadline_ms` is not how long a launch may take — it is how long it may make *no
  progress*.** The gate resets it on every `503 {"status":"loading model"}`, so a 12 s load passes a
  1 s deadline and the real start budget is unbounded while a load is progressing. Anything that
  waits *on* the gate must therefore share the gate's liveness signal, or it will be less patient
  than the thing it is waiting for. A warm window read as a stopwatch `503`'d 4 parked requests at
  2977 ms of a 12,038 ms swap and the alias then answered **74,550** requests with
  `no_healthy_backend`. The signal that works is **the launch future still being pending**, sampled
  on `health_interval_ms`; `Event::BootProgress` does *not* work, because it fires once per
  transition and never per tick.

## Workflow

```sh
export CARGO_BUILD_JOBS=4                 # cargo file-locks target/; concurrent agents QUEUE
cargo fmt --all
cargo clippy -p apexrouter-protocol -p apexrouter-core -p apexrouter-router \
             -p apexrouter-providers -p apexrouter-client -p apexrouter-server \
             -p apexrouter-cli -- -D warnings          # apexrouter-slint is never linked in CI
cargo test --workspace
cargo build --release

./target/release/apexrouter serve --foreground        # or just let a Mutate verb autostart it
./target/release/apexrouter status --json
curl -s 127.0.0.1:8888/v1/models | head               # proxy
curl -s 127.0.0.1:2739/health                         # control
```

House rules the CI enforces or the reviewer will:

1. `docs/ARCHITECTURE.md` is normative. **Never change a signature Stage 0 published** — report it
   instead; changing it breaks other agents silently.
2. **No two agents write the same file.** `BUILD-PLAN.md` §5 is the ownership index. If you are
   about to write a file that is not on it, stop and say so.
3. **No `sh -c`, anywhere.** `core::exec` takes an argv vector; CI greps for `"sh", "-c"` and
   `.arg("-c")`.
4. **No `unwrap()`/`expect()`** outside tests, `main()` and `build.rs`. `thiserror` in libraries,
   `anyhow` in binaries.
5. **Nothing to stdout** except MCP JSON-RPC and `--json` output. `tracing` goes to stderr — `mcp`
   shares the binary and owns stdout.
6. Doc comment on every `pub fn`; `//!` on every crate. No colour crate, no emoji in output.
7. **Prove it on the real machine.** `llama-bench` or the `timings` object; GPU offload was proven
   from `/sys` `mem_info_gtt_used` and two fds on `/dev/dri/renderD128`, not assumed. Real numbers
   beat adjectives: MK1-CORE ACCEPTANCE ran Carnice-9b-Q6_K at **9.71 tok/s** generation /
   **53.71 tok/s** prompt eval through the proxy on a Radeon 840M iGPU, corroborated by
   ApexRouter's own `tok_per_s_p50` = 9.69 over 12 requests. **Do not invent benchmarks.**
8. Commit voice: story-telling subject lines, the gate-found defects named in the body.

The laptop is the **smoke-test box, not the design target** (`docs/port/00b`). 24 GB unified, the
iGPU shares it; check `free -h` before loading anything big. Carnice-9b fits its full
262144-token context here — that is measured, not aspirational.

## Roadmap seeds (charter-consistent, unscheduled)

`docs/CHARTER.md` §"Deliberately out of mk1" is the authoritative list with reasons. The ones most
likely to be picked up next: llama.cpp b9199's own router mode (`--models-dir`,
`POST /models/load`, `--sleep-idle-seconds`) plus idle-unload, as the mk2 *simplification* of the
supervisor; mDNS discovery and capacity-aware placement for LAN nodes (a LAN node is already a
`Node` backend, which was the cheap 90%); MCP streamable-HTTP, when ApexOS-RV nodes need dispatch
over the network — `fn dispatch(method, params)` is already transport-agnostic. None of these are
architected out.
