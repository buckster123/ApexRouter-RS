<div align="center">

<img src="assets/banner.png" alt="ApexRouter-RS" width="100%">

# ApexRouter-RS

**One base URL for every model you can reach — local, rented, or managed.**

*Point every agent, SDK and script at `http://127.0.0.1:8888/v1` and never change it again.*
*Aliases move underneath. Nothing downstream notices.*

[![rust](https://img.shields.io/badge/100%25-Rust-orange?style=for-the-badge&logo=rust)](https://www.rust-lang.org/)
[![local](https://img.shields.io/badge/127.0.0.1-only-22c55e?style=for-the-badge)]()
[![license](https://img.shields.io/badge/license-MIT_OR_Apache--2.0-blue?style=for-the-badge)](docs/LICENSING.md)
[![native app](https://img.shields.io/badge/native_app-GPL--3.0-8A2BE2?style=for-the-badge)](docs/LICENSING.md)
[![mcp](https://img.shields.io/badge/agents-MCP_·_24_tools-8b5cf6?style=for-the-badge)](docs/AGENTS.md)

</div>

---

> [!NOTE]
> **Both `http://127.0.0.1:8888` and `http://127.0.0.1:8888/v1` work as your base URL.** A repeated
> leading `/v1` is collapsed to one, so an SDK that appends `/v1` for you and a script that already
> has it both hit the same route. You never have to remember which form a given client wants.

## Why

You have a laptop iGPU that can serve a 9B model, a vast.ai box you rent when you need 70B, a
together.ai key, and maybe a LAN machine with a real GPU. Every one of them speaks
OpenAI-compatible HTTP on a different host, a different port, and a different model string — and
every agent, `.env`, notebook and shell profile on your machine has one of those strings baked in.
Change what you run and you go editing.

ApexRouter-RS holds a **routing table** — named aliases pointing at ordered chains of live
backends — and serves it on one loopback port:

- **One URL, forever.** `auto` is an alias, not a model. Move it from the local `llama-server` to a
  rented 4090 to together.ai and every client keeps working, mid-session, with no restart.
- **It knows your rig.** It finds your llama.cpp builds, your GGUFs and your GPUs, and a pure
  `fit()` solver answers *what actually fits* — context, KV cache type, layers to offload — instead
  of you hand-tuning 54 recipe strings until one stops OOMing.
- **It supervises, and gets out of the way.** `llama-server` children are spawned with `setsid()`
  and **outlive the manager**: restart the daemon, upgrade the binary, crash it — the model that
  took 90 seconds and 6 GB to load is still there and gets re-adopted.
- **It's honest about money.** vast.ai rentals need a `SpendApproval`, the ledger row is written
  *before* the billing call, and **nothing that costs money is ever auto-destroyed** — not on
  shutdown, not on crash, not at any setting.
- **It answers when it's off.** `status`, `rig`, `models ls`, `usage` and friends read `$STATE`
  directly with the daemon down, and tag the answer `served_by: "offline"` so a script knows.

Measured, not adjectival: through the proxy on a **Radeon 840M iGPU**, `Carnice-9b-Q6_K` runs at
**9.71 tok/s** generation and **53.71 tok/s** prompt eval — numbers read from llama.cpp's own
`timings`, on the far end of a route the proxy resolved without touching the filesystem once.

## How it works

```
                    ┌──────────────────────────────────────────────────────────┐
                    │                  apexrouter serve                        │
curl / SDKs      ──►│ :8888  proxy (data plane)                                │
OPENAI_BASE_URL     │   ├─ collapse duplicate /v1                              │
ANTHROPIC_BASE_URL  │   ├─ resolve(model) ─► ArcSwap<RoutingTable>  (no I/O)   │
                    │   ├─ in-flight permit + budget + RequestRecord           │
                    │   ├─ retry/failover — never past the first byte          │
                    │   └─ relay bytes verbatim; SSE never re-framed           │
                    │                                                          │
web UI  ─────────►  │ :2739  control plane                                     │
Slint   ─────────►  │   ├─ /v1/… REST   ├─ /ws (snapshot on connect)           │
CLI     ─────────►  │   ├─ /metrics     └─ embedded ui-web (no npm)            │
MCP     ─────────►  │                                                          │
                    │── shared, in-process ────────────────────────────────────│
                    │ BackendRegistry (semaphore · breaker · health)           │
                    │ Supervisor ── setsid ──► llama-server ×N                 │
                    │ TunnelSupervisor ── ssh -L ──► vast.ai box ×N            │
                    │ Ledger · UsageWriter · HealthProber · Watcher            │
                    │ flock $STATE/apexrouterd.lock  (owner record)            │
                    └──────────────────────────────────────────────────────────┘
              │                                   │
   ~/.local/state/apexrouter/          console.vast.ai · api.together.ai
   (facts, ledger, usage, logs)        huggingface.co · your LAN boxes
```

Five invariants hold the thing together, and every one of them is a test:

1. **One resolver.** Exactly one `resolve()`, called by every surface, and its answer is on every
   response as `X-ApexRouter-Route: <alias>|<reason>`. No surface gets to have its own opinion
   about what is active.
2. **The request path never touches the filesystem.** An `ArcSwap<RoutingTable>` and an
   `Arc<LiveBackend>` per backend. No `stat()`, no TOML parse, no lock beyond one semaphore.
3. **Persisted records hold facts, never status.** `pid`, `start_time_ticks`, `boot_id`, `port`,
   `argv`. Liveness is *computed* on read — no `status: "running"` string ever reaches disk to go
   stale.
4. **Money is deliberate.** Ledger before billing call; nothing paid-for is auto-destroyed.
5. **State lives in one XDG dir.** Nothing is ever written into a repo directory.

## Install

```sh
git clone https://github.com/buckster123/ApexRouter-RS && cd ApexRouter-RS
./install.sh                      # asks two things: automatic-or-manual, then the plan. Defaults do it.
./install.sh --yes                # unattended — every default, nothing asked. Implied when piped.
./install.sh --dry-run            # print the whole plan and touch nothing
```

Rust ≥ 1.75 and a C linker are the entire build dependency list — no npm, no OpenSSL, no `sudo`,
nothing outside your home directory. It builds **one** ~19 MB binary (the CLI, the daemon, the MCP
server and the web UI are all the same executable), puts it in `~/.local/bin`, offers you a systemd
`--user` service, and finishes by running `doctor`. Linux only in mk1 — the process model is
`/proc`, `flock`, `setsid` and `boot_id`.

You are installed when `apexrouter doctor` prints a table. Warnings on day one are normal; it tells
you what each one wants.

Prefer not to pipe a script anywhere near your shell — or did something already go wrong?
**[`docs/INSTALL.md`](docs/INSTALL.md)** is every step written out by hand: prerequisites per
distro, `cargo build --release` and what it produces, the llama.cpp backend matrix
(Vulkan/CUDA/ROCm/CPU), the systemd unit and the one line in it that is load-bearing, and a
troubleshooting section built entirely from edges this project has actually cut itself on. Leaving
again is [`./uninstall.sh`](uninstall.sh), which keeps your data unless you say otherwise and prints
exactly what it will delete first.

> Field-testing this on a real multi-GPU rig? Read
> [`docs/RELEASE-NOTES-mk1.md` §6](docs/RELEASE-NOTES-mk1.md#6-what-to-trust--the-three-buckets)
> first — it splits every claim into *verified*, *banked from real hardware*, and *never run against
> the real thing*. The third bucket is the honest one and it is where your rig can find things this
> laptop cannot.

## Quick start

```sh
apexrouter doctor                 # what's here, what's missing, one fix line per row
apexrouter rig                    # GPUs (free/total), llama.cpp builds, RAM, swap
apexrouter models ls              # local GGUFs, shards grouped into one row

apexrouter up Carnice-9b-Q6_K --alias auto
#   solves the fit, spawns llama-server, health-gates it, binds the alias,
#   and prints:  http://127.0.0.1:8888/v1
```

Then point anything at it:

```sh
eval "$(apexrouter env)"          # export OPENAI_BASE_URL=…/v1 ; OPENAI_API_KEY=not-needed

curl -s http://127.0.0.1:8888/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"auto","messages":[{"role":"user","content":"hi"}]}'
```

`auto` never changes. What's behind it does:

```sh
apexrouter swap auto --to <model|recipe|backend-id>   # hot or sequential — the mode is chosen for you
apexrouter switch together                            # …or a managed provider
apexrouter route set auto --target local-carnice --target together:meta-llama/Llama-3.3-70B-Instruct-Turbo \
                         --strategy first_healthy --failover
```

Zero config is a working install. To tune anything, copy
[`config.example.toml`](config.example.toml) to `~/.config/apexrouter/config.toml` — every field is
defaulted and commented.

### The CLI

```sh
apexrouter                       # bare = status: what's bound, what's live, what it's doing
apexrouter url                   # prints http://127.0.0.1:8888/v1 and nothing else
apexrouter fit <model> --ctx 32768        # what fits, and why — a verdict plus its reasoning
apexrouter endpoint ls                    # local llama-server lifecycle
apexrouter endpoint logs <id> -f          # …and its log, followed
apexrouter usage --since 24h --by model   # tokens and cost, from real `timings`
apexrouter smoke                 # four native probes: models, warmup, tools, throughput
apexrouter compare --alias a --alias b --prompt "…"   # one prompt, two backends, side by side

apexrouter vast offers --gpu "RTX 4090" --max-price 0.40   # read-only search
apexrouter vast rent <offer> --profile p --model-repo R --quant Q --max-hourly 0.40 --yes
apexrouter tunnel up <instance-id>        # supervised ssh -L; the box stays loopback-only
apexrouter approvals ls                   # nothing spends without a granted approval
apexrouter approvals grant <id>
```

`--json` is per-subcommand and prints the protocol type and **nothing else** on stdout, with
`served_by`, `as_of_unix` and `stale` on every envelope. Full reference:
[`docs/API.md`](docs/API.md).

### Agents (MCP)

```sh
claude mcp add apexrouter -- /path/to/apexrouter mcp
```

**24 tools** — `apexrouter_status` first in any session (it hands back the base URL and the model
string to use), then `models` / `rig` / `fit` / `up` / `swap` / `route_set` / `logs` / `usage` /
`smoke` / `diagnose`, HuggingFace search-and-fetch, and the money-gated `vast_*` trio. They work
with the daemon down, answering from `$STATE` and saying so.

Edge boxes run the same binary as a thin proxy to a fat node, so only one machine holds credentials:

```sh
apexrouter mcp --proxy http://fat-node:2739      # or set $APEXROUTER_URL
```

Copy-paste registration per harness: [`docs/AGENTS.md`](docs/AGENTS.md). For harnesses with
**Agent Skills**, ship the skill so a smaller model knows *how* to drive it:

```sh
cp -r skills/apexrouter ~/.claude/skills/        # Claude Code (user-level)
```

## Surfaces

| Surface | For | How |
|---|---|---|
| **OpenAI proxy** | every SDK, agent and script | `http://127.0.0.1:8888/v1` — `chat/completions`, `completions`, `embeddings`, `rerank`, aggregated `models` |
| **Anthropic ingress** | Claude Code and friends | `POST /v1/messages` on the same port; translated both ways against an OpenAI upstream, relayed verbatim against an Anthropic one. Tool translation is **on by default** and best-effort — Claude Code sends 92 tool definitions on every request, so opt-in would have meant "does not work" |
| **Control REST + WS** | UIs, automation, ApexOS | `http://127.0.0.1:2739/v1/…` + `/ws` (snapshot on connect, deltas after) |
| **Web UI** | humans, no install | served from the control port — three files, no npm, no CDN, no build step |
| **Native app** | desktop & kiosk | `apexrouter-ui` (Slint) — a client of the same API, separate GPL binary |
| **CLI** | you, and shell scripts | `apexrouter <verb>`, `--json` everywhere it matters |
| **MCP** | local agents | `apexrouter mcp` over stdio, or `--proxy` to a fat node |

Three legacy routes (`GET /health`, `GET /providers`, `POST /switch`) are served byte-compatibly
with LocalRouter's proxy, so existing scripts keep working unchanged.

## Repository layout

| Crate / dir | What |
|---|---|
| `apexrouter-protocol` | every wire and domain type — the one vocabulary all surfaces share |
| `apexrouter-core` | paths, config, secrets, atomic store + locks, process identity, discovery, the `fit()` solver, pricing, usage, ledger, migration, checks |
| `apexrouter-router` | **the request path** — table, `resolve()`, relay, SSE, retry, breaker, limits, telemetry, Anthropic translation |
| `apexrouter-providers` | how backends come to exist — local supervisor, vast.ai, ssh tunnels, together.ai, HuggingFace |
| `apexrouter-client` | `NodeClient` — thin HTTP + WS client, no business logic |
| `apexrouter-server` | the axum app: both listeners, `/ws`, auth, embedded assets, jobs |
| `apexrouter-cli` | the `apexrouter` binary — every verb, plus `serve` and the MCP stdio server |
| `apexrouter-slint` | `apexrouter-ui`, the native app (GPL-3.0-only, out of `default-members`) |
| `ui-web/` | the embedded web UI — `index.html`, `app.js`, `style.css` |
| `openapi/` | `apexrouter-v1.yaml`, checked in CI against the live route table |
| `docs/` | [INSTALL](docs/INSTALL.md) · [RELEASE NOTES](docs/RELEASE-NOTES-mk1.md) · [ARCHITECTURE](docs/ARCHITECTURE.md) · [API](docs/API.md) · [ROUTING](docs/ROUTING.md) · [CHARTER](docs/CHARTER.md) · [MIGRATION](docs/MIGRATION.md) · [LICENSING](docs/LICENSING.md) |

## Security posture

- **Loopback by default, both listeners.** A non-loopback bind **refuses to start** without a
  configured token, and says how to fix it. The loopback bypass needs both an explicit opt-in and a
  genuinely loopback peer IP from `ConnectInfo` — absent connect-info fails *closed*.
- **A loopback port is not a trust boundary.** Every mutation on either listener passes a gate:
  `Host` allowlist (closes DNS rebinding), `Origin`/`Sec-Fetch-Site` same-origin when present,
  bearer with `write` scope otherwise. There is no `CorsLayer` on the authenticated API.
- **Credentials are borrowed, never copied.** A key found in your vast/HF/together config stays
  where it is; only a key you explicitly type is written, to `credentials.toml` at `0600`. `Secret`
  prints `***`; the API returns `{source: "env:TOGETHER_API_KEY", present: true}` — the source,
  never the value. No secret ever reaches an argv (`--api-key-file`, not `--api-key`), a query-string
  span, or a Vast `--onstart-cmd`.
- **Rented boxes are tunnel-only by default.** `HOST=127.0.0.1` is forced at create time *and* on
  every stall-restart; `expose_public` is an explicit opt-in that requires a freshly minted
  per-instance key. `GET /slots` is never proxied outward — it echoes prompts.
- **Prompts are not stored** unless you turn `capture_bodies` on, and it is off by default.

## Configuration

| What | Where |
|---|---|
| Config | `$APEXROUTER_CONFIG` → `$APEXROUTER_HOME/config.toml` → `~/.config/apexrouter/config.toml` |
| State (facts, ledger, usage, logs) | `$APEXROUTER_HOME` or `~/.local/state/apexrouter/` |
| Cache (HF metadata, probes, offers) | `~/.cache/apexrouter/` |
| Proxy / control ports | `[server] proxy_bind` = `127.0.0.1:8888`, `control_bind` = `127.0.0.1:2739` |
| Control URL, for clients | the lock file's owner record (CLI, MCP), or `[server] control_bind` (native app), always overridden by `$APEXROUTER_URL` |
| Bearer token (non-loopback only) | the var named by `[server] token_env`, default `APEXROUTER_TOKEN` |
| vast.ai key | `~/.config/vastai/vast_api_key` (read, never copied) |
| HuggingFace token | `~/.cache/huggingface/token` |
| together.ai key | `$TOGETHER_API_KEY`, or `[providers.together] api_key_env` |
| Migrating from LocalRouter | `apexrouter migrate --dry-run` — see [`docs/MIGRATION.md`](docs/MIGRATION.md) |

## License

**MIT OR Apache-2.0** ([`LICENSE-MIT`](LICENSE-MIT) · [`LICENSE-APACHE`](LICENSE-APACHE)) for the
whole headless stack — protocol, core, router, providers, client, server, CLI, MCP server and the
embedded web UI. Use it however you like.

One caveat, stated plainly: the optional native app `apexrouter-slint` links the
[Slint](https://slint.dev) toolkit, which is separately licensed (GPL-3.0 / Royalty-Free Desktop /
commercial). That one crate is therefore **GPL-3.0-only** ([`LICENSE-GPL`](LICENSE-GPL)), and it is
deliberately kept out of `default-members` so a normal `cargo build` never pulls it in. If you ship
the native GUI, you take
on GPL obligations *for that binary*; the daemon, proxy, web UI, CLI, MCP server and SDK are
Slint-free and stay permissive. Details in [`docs/LICENSING.md`](docs/LICENSING.md).

---

<div align="center">
<sub>Part of the <a href="https://github.com/buckster123/ApexOS-RS">ApexOS</a> ecosystem ·
sibling to <a href="https://github.com/buckster123/Prefrontal-RS">Prefrontal-RS</a> and
<a href="https://github.com/buckster123/Imaginarium-RS">Imaginarium-RS</a> ·
banner generated locally by <a href="https://github.com/buckster123/Imaginarium-RS">Imaginarium-RS</a>
via <code>grok-imagine-image-quality</code> 🔀</sub>
</div>
