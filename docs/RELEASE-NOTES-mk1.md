# ApexRouter-RS **mk1** — release notes

*Tagged 2026-07-31. Written at the final verification gate, from measurements taken during it.*

> **One base URL, forever.** Point every agent, SDK and script on the machine at
> `http://127.0.0.1:8888/v1` with `model: "auto"` and never touch either again while the thing
> behind it changes from an iGPU to a rented 2×H100 and back.

mk1 is the Rust successor to LocalRouter, a Python TUI whose 417-line proxy read a JSON file per
request. It is one daemon, eight crates, ~116,000 lines of Rust, 1,608 tests.

**Read this section first if you are deciding what to trust:** §6 splits every claim into three
buckets — *verified at this gate*, *banked from earlier real-hardware runs*, and *waiting on real
hardware*. The third bucket is the honest one, and it is not short.

---

## 1. What it is

One process, two listeners.

- **The proxy — `127.0.0.1:8888`.** The product. It holds an `ArcSwap<RoutingTable>` of named
  aliases → ordered chains of live OpenAI-compatible backends, and relays bytes verbatim. The
  request path never touches the filesystem: no `stat()`, no TOML parse, no lock beyond one
  `Semaphore`.
- **The control plane — `127.0.0.1:2739`** (`APEX` on a phone keypad). `/v1/*` REST, `/ws`,
  `/metrics`, and the embedded web UI.

Everything else the manager does — discovering llama.cpp builds, GGUF weights and GPUs; solving
what fits in VRAM; spawning and supervising `llama-server`; renting a vast.ai GPU and tunnelling it
home; talking to together.ai; registering a LAN box — exists to **put rows in that table and keep
them honest**.

Five surfaces share one serde-only protocol crate: REST + WebSocket, a no-build embedded web UI, a
native Slint app, a `clap` CLI with `--json` on every verb, and an MCP stdio server exposing 24
tools to local agents.

### The five invariants

1. **One resolver.** Exactly one `resolve()`, called by every surface. LocalRouter had four
   implementations of "what is active" that disagreed with each other. The answer is observable on
   every response as `X-ApexRouter-Route: <alias>|<reason>`.
2. **The request path never touches the filesystem.**
3. **Persisted records hold facts, never status.** `pid`, `start_time_ticks`, `boot_id`, `port`,
   `argv`, `desired_state`. Liveness is *computed* on read — `status: "running"` on disk is a lie
   the moment someone types `kill`.
4. **Nothing that costs money is auto-destroyed, and nothing that costs money happens without a
   `SpendApproval`.** The ledger row is written *before* the billing call.
5. **One XDG state dir. Nothing is ever written into a repo.**

---

## 2. What shipped

| Area | What |
|---|---|
| **Routing** | Named aliases → ordered backend chains; `first_healthy` / `round_robin` / `least_busy` / `cheapest`; per-route retry policy with `honor_retry_after`; circuit breakers; failover across backends |
| **Local supervision** | `llama-server` spawn with `setsid()`, one argv/env builder, health gate, log rotation, re-adoption by `(pid, start_time_ticks, boot_id, exe)` |
| **The fit solver** | One `fit()` that replaced 54 hand-solved recipe strings and 19 GPU tiers — context, KV type and layer offload solved live against the rig you actually have |
| **Swap** | One verb, mode chosen by `fit()`. Hot when the replacement fits alongside; sequential otherwise, with a **warm queue** that parks requests instead of `503`-ing them |
| **Anthropic ingress** | `POST /v1/messages` on the proxy port, translated against an OpenAI upstream. Tool translation **on by default** and best-effort |
| **Money** | Append-only `ledger.jsonl` where "active" is a query; reserve-before-billing; a daemon-side hard spend ceiling; startup reconciliation |
| **vast.ai** | One offer-query builder, boot-phase state machine, SSH tunnels with a reconnect supervisor and ControlMaster teardown, download-stall detection **and** recovery |
| **Migration** | `~/.vastai-gguf` + a LocalRouter checkout imported read-only, with stale state treated as normal rather than as an error |
| **Honesty types** | `TokenCount::{Reported,Estimated}`, `CostEstimate::{Metered,Approximate,Unknown}`. A degraded record says so; it never guesses in silence |
| **Compatibility** | LocalRouter's three legacy routes (`GET/HEAD /health`, `GET/HEAD /providers`, `POST /switch`) byte-compatible — and **removed at 1.0** (CHARTER D9) |

---

## 3. Numbers measured at this gate

All against the fake `llama-server` in `tests-support/` — faithful to llama.cpp b9199's wire
surface, no GPU, no model load. **No number here is a benchmark.** Throughput figures from the fake
are arithmetic from a configured `tok_per_s`; the only real inference numbers are in §6.2.

### The warm queue, the headline fix

`warm_timeout` used to be a stopwatch. A sequential swap that outran it produced an outage with a
delay on it:

| Scenario | Requests | Result |
|---|---:|---|
| **12,318 ms swap, 3000 ms warm window, 8 clients** | 24,620 | **24,620 × 200 — zero 5xx.** Peak parked 8 |
| Same, instrumented | 24,055 | zero 5xx, **61 re-arms**, peak parked 8, swap 12,226 ms |
| *Control:* the same alias left unserved for ~3 s | 32,799 | **20,613 × 503** `no_healthy_backend` |
| *Historical, pre-fix:* 12,038 ms swap, same 3000 ms window | — | 4 parked `503`'d at 2977 ms, then **74,550 × `no_healthy_backend`** |

61 re-arms is exactly one per 200 ms health probe across a 12.2 s launch, which is the mechanism
working rather than a coincidence: the park re-arms on the **launch future still being pending**,
sampled on the health gate's own clock, so the gate and the queue cannot form two opinions about
whether progress is happening. The deadline still fires — it now measures wall clock since the last
sign of life rather than since the swap began.

Corroborating the premise: **a 12.2 s model load passed a `health_deadline_ms` of 1000 ms**, because
the gate resets on every `503 {"status":"loading model"}`. The start budget really is unbounded
while a load is progressing, so any fixed park is eventually shorter than it.

### `warm_queue_max`, moved under one unchanged swap

| `[router] warm_queue_max` | Clients | Peak parked | Result |
|---:|---:|---:|---|
| `4` | 12 | **4** | the other 8 get `503 warm_queue_full` immediately, with `Retry-After` |
| `16` | 12 | **12** | **zero 5xx** |

### argv fidelity

`GET /v1/endpoints/{id}/argv` versus `/proc/<pid>/cmdline`, token for token:

- **36 / 36 identical.**
- Still 36/36 **after the VRAM budget was moved under the running child** (`vram_margin_mb`
  1024 → 20000) **and the daemon restarted and re-adopted it**. That is the exact condition under
  which the old code served 34 tokens describing a CPU-only launch for a fully-offloaded child.

### Everything else measured live

| Check | Result |
|---|---|
| Full suite, twice, with a leak check between | **1,608 passed / 0 failed**, 0 stray processes both times |
| `clippy --all-targets -D warnings` (7 crates) | clean |
| `cargo fmt --all --check` | clean |
| `cargo build --release`, `cargo build -p apexrouter-slint` | clean |
| Real **Claude Code**, stock config, no edits | `is_error: false`, `subtype: "success"`, `api_error_status: null` |
| Tool translation at Claude Code's scale | **92 `input_schema` → 92 OpenAI `parameters`**, all well-formed |
| `anthropic_tools = false` set *explicitly* | still `400 tool translation is off: set [router] anthropic_tools = true to enable it` |
| LocalRouter's **unmodified** `smoke.sh` | **4/4**, both `http://127.0.0.1:8888` and `.../v1` (the second becomes `/v1/v1`, which the proxy collapses) |
| MCP over stdio | `initialize` + `tools/list` → **24 tools** |
| `doctor --json` | 12 pass / 3 warn / 3 skipped / 1 fail — the fail is `together.ratelimits` against the deliberately closed loopback port |
| Every `--json` verb | 15/15 parse as pure JSON; error paths write **0 bytes** to stdout |
| Daemon stdout, including at `-vv` | **0 bytes** |
| `X-Usage` on a buffered response | `1+2` — LocalRouter's exact `{prompt}+{completion}` format |
| Streaming | `X-ApexRouter-Usage-Deferred: true`, never a fabricated count (CHARTER D8) |
| `/slots` | `403 redacted_endpoint` — it echoes prompts and is never proxied outward |
| Unknown model | `404` (CHARTER D6) |
| `POST /health` | proxied, **not** `405` |
| Legacy `GET /health`, `GET /providers` | `200` (CHARTER D9) |
| Slint GUI with `$APEXROUTER_URL` **unset**, `control_bind` moved to 3739 | three ESTABLISHED connections to **127.0.0.1:3739** |
| Migration against a **copy**, twice | one `local-qwen35-9b`, `ctx = 32768` preserved, conflict warned, byte-identical `catalog.toml` on re-apply |
| `~/.vastai-gguf` before/after | `c84a0e72…fa186d` → `c84a0e72…fa186d`, **byte-identical** |
| LocalRouter checkout before/after | byte-identical |
| vast.ai credit | **$7.72899119913999**, unchanged; zero instances; only read-only endpoints called |

---

## 4. Defects this gate cleared

Ten residual defects were open when mk1 was first signed off. Nine are closed and verified; one is
closed by this gate. Each was proved with a failing observation before the fix.

| # | Severity | What it was |
|---|---|---|
| **D1** | HIGH | `GET /v1/endpoints/{id}/argv` re-ran the *planner* instead of reading the record, so it described a launch that never happened — 34 tokens against `/proc`'s 36, with `warnings` empty. It also printed a literal `<id>.key` placeholder instead of the real key path. |
| **D2** | MEDIUM | The warm window treated `warm_timeout` as a total budget, so a swap that outran it converted a survivable wait into a 74,550-request outage. |
| **D3** | MEDIUM | `migrate::apply` de-duplicated recipe ids against the catalog but not within its own batch, writing two recipes under one id — the second unreachable, and `recipe rm` deleting both. |
| **D4** | MEDIUM | `smoke --alias A --json` printed a human banner line before the JSON, so `\| jq` failed. |
| **D5** | MEDIUM | A mistyped config key was ignored in total silence. |
| **D6** | LOW | `[router] warm_queue_max` was documented in two places and did not exist. **Closed at this gate.** |
| **D7** | LOW | The warm queue was undocumented. |
| **D8** | MEDIUM | `anthropic_tools` defaulted to `false`, so a stock config answered real Claude Code with a `400` on request one. |
| **D10** | LOW | The Slint GUI read only `$APEXROUTER_URL`, so moving `control_bind` left it pointed at nothing with "not connected" as the only symptom. |

Two of these found each other, which is worth recording: writing `[router] warm_queue_max` into a
config file earned the *"key is not one this build knows"* warning that D5's work had added the same
day. D5's fix is what proved D6 was real.

### Behaviour changes for an existing install

- **`[router] anthropic_tools` now defaults to `true`.** An operator who wants the old refusal must
  now set it to `false` explicitly — and that refusal is unchanged, loud, and names the key.
- **A `catalog.toml` that already holds a duplicate recipe id** will show the second entry as
  `<id>-2` in `recipe ls` from the next read, with a warning on stderr. That is the repair: before
  it, the second entry was unreachable and `recipe rm` deleted both.
- **Unknown config keys now warn on stderr** and appear under `unknown_keys` in `config show`. They
  are still not errors, and your file is never rewritten to remove them.

---

## 5. Deliberately out of mk1

`docs/CHARTER.md` is authoritative and gives a reason for each. In brief:

**Permanently out.** OpenAI → Anthropic translation (`501`, D13) — ApexOS-RS already speaks
Anthropic natively, so the reverse translator would be a second unexercised one.
`Strategy::Mirror` / `Fastest` / sticky sessions — not in the enum at all, so no config value can
reach an unimplemented arm; `POST /v1/compare` ships instead. A TUI (D14). CORS. sqlite (D17). The
deprecated MCP HTTP+SSE transport (D18).

**Deferred, honestly.** Perfect Anthropic tool-use translation — parallel tool calls, some
`tool_choice` variants and a block-array `tool_result` do not map cleanly in every case, and mk1
says so rather than promising otherwise. `thinking` blocks and `POST /v1/messages/count_tokens`
(`501` — the only honest count needs a tokenizer we do not have). llama.cpp b9199's own router mode
and idle-unload, filed as the mk2 *simplification*. Vast bidding, volumes, multi-region. GPU-mesh
scheduling across LAN nodes (a LAN node is already a `Node` backend, which was the cheap 90%).
Automatic quantisation. Windows and macOS (D15 — the process model is `/proc`, `flock`, `setsid`
and `boot_id`).

---

## 6. What to trust — the three buckets

### 6.1 Verified at this gate (2026-07-31, this laptop, fake `llama-server`)

Everything in §3. In summary: the full test suite twice with leak checks, clippy/fmt/release/slint,
argv fidelity against `/proc`, the warm queue under load including the configurable bound, real
Claude Code end-to-end on a stock config, 92-tool translation, LocalRouter's unmodified `smoke.sh`
on both base-URL forms, MCP over stdio, `doctor --json`, `--json` purity across every verb,
migration against a copy with the originals hashed before and after, and vast.ai read-only with
credit unchanged.

### 6.2 Banked from earlier real-hardware runs

From **MK1-CORE ACCEPTANCE** (`BUILD-PLAN.md` §7.1) — real laptop, real release binary, real
`Carnice-9b-Q6_K` on the real Vulkan build. These are the only genuine inference numbers in this
document and they were **not** re-measured today (the standing instruction for this gate was no GPU
model load):

- **9.71 tok/s** generation, **53.71 tok/s** prompt eval, from llama.cpp's own `timings`,
  corroborated by ApexRouter's `tok_per_s_p50` = 9.69 over 12 requests. Model load 7.39 s.
- GPU offload proven from `/sys` `mem_info_gtt_used` = 10.2 GB and two fds on `/dev/dri/renderD128`
  — measured, not assumed.
- LocalRouter's unmodified `smoke.sh` 4/4 in both base-URL forms.
- A daemon restart re-adopted the hot model by `(pid, start_time, boot_id)`.
- `X-Usage: 11+20` matched the body's token counts exactly.
- Hermeticity verified by `strace`: every `connect()` is loopback.

The laptop is the **smoke-test box, not the design target**: 24 GB unified, the iGPU shares it.

### 6.3 Waiting on real hardware — the honest list

> **A note on provenance.** The previous sign-off's `unverified` list was not recorded in the repo
> and was not available to this gate, so the list below is **reconstructed** from the charter, the
> code, and what this gate could not reach. Treat it as a careful reconstruction rather than a
> carried-forward original, and add to it rather than assuming it is complete.

Nothing below is known broken. Each is code that exists, compiles, is unit-tested against fakes,
and **has never executed against the real thing**.

**Money and remote hardware — never executed end to end, by rule.** The house rule is that no
vast.ai endpoint which creates, modifies or destroys an instance may be called; credit is
$7.72899119913999 and must stay there. So the whole rental lifecycle is unproven in the wild:
`vast rent`, the `BootPhase` state machine driven by `request_logs` → two-phase `result_url` poll,
`vast watch`, `vast log`, `vast destroy`'s verify-before-forget, the `max_boot_secs` watchdog,
startup `reconcile()` against a live account with real rows, and the `OrphanSuspect` path on a
`Drop` that actually fires. The one thing genuinely proven is that reading is safe and the credit
does not move.

**SSH.** Tunnel supervision against a real remote: the reconnect supervisor's exponential backoff,
`ExitOnForwardFailure` behaviour on a real dead link, `ssh -O exit` plus ControlPath unlink on
teardown, and the dedicated `known_hosts` surviving Vast's recycled `sshN.vast.ai` hostnames.
`ssh.binary` passes; `ssh.controlmaster` skips with "no instance selected".

**Download-stall detection and recovery.** The 4 s `/proc/net/dev` eth0 RX delta over SSH, and the
one-click *Restart download* that pkills `launch.sh` + `hf download` and re-execs with the
environment recovered from `/proc/<pid>/environ`. Never run against a real stalled download.

**Large models and long loads.** The fake's longest load here was 12 s. A 30 GB GGUF whose mmap
takes minutes is the case the warm-window re-arm was designed for, and it has been proven only at
12 s. The 7 GB Carnice load is banked at 7.39 s (§6.2).

**Multi-GPU.** `-sm row`, `--tensor-split`, `--main-gpu` and multi-device `-dev` are emitted by the
one argv builder and asserted against the fake. No multi-GPU box has ever run them. Note the
recorded trap: **ROCm reports free > total — never compute `total - free`.**

**vLLM.** `endpoint vllm` and `argv::plan_local_vllm` are implemented and have never launched a real
vLLM.

**together.ai and HuggingFace, live.** Hermeticity requires `[providers.together]` to point at a
closed loopback port, so `together.ratelimits` *fails by construction* in every hermetic run,
including this gate's `doctor`. Real rate-limit-header parsing and a real `hf get` of large weights
are unexercised. `creds.hf` and `creds.vast` pass — the credentials are found, not used.

**Non-loopback operation.** A `0.0.0.0` bind with token auth refuses to start without a token, which
is tested; being *reached* by a real LAN peer, and the mutation gate's `Host`/`Origin`/
`Sec-Fetch-Site` behaviour against a real browser, are not.

**`Backend::Metal`.** Present in the enum so the data model need not change later. Nothing pretends
it works (D15).

**Adoption across a reboot.** `boot_id` is part of process identity precisely because
`start_time_ticks` is not comparable across reboots. Restart-adoption is verified; reboot-adoption
is not.

---

## 7. Known open items at tag time

None is a defect in shipped behaviour. All are recorded rather than remembered.

| Severity | Item |
|---|---|
| LOW | **`apexrouter config validate` is not a CLI verb.** `Config::validate` / `validate_file` exist in core and are infallible. Rendering them is a two-liner plus a `ConfigCmd` arm (owner S-06). Until then: `config show --json \| jq .unknown_keys`, and every unknown key already warns on stderr. `docs/MIGRATION.md` §10.3. |
| LOW | **`POST /v1/migrate` is documented and not built.** In `ARCHITECTURE.md` §6 and the OpenAPI document, listed `PENDING` by `openapi_routes.rs`, owned by no unit's file list. Needs a new `api` module *and* its one `.merge(…)` line in `v1_routes()`. `docs/MIGRATION.md` §10.2. |
| LOW | **No CLI surface strikes a row from a migration plan.** `migrate::apply` honours a row downgraded to `Skip`, but nothing exposes the edit. `docs/MIGRATION.md` §10.1. |
| INFO | **`docs/BUILD-PLAN.md` still describes `anthropic_tools = false`** at four places. Left as written: it is the historical build plan, not a description of the shipped system. |

---

## 8. Upgrading, and the one thing to read

If you are coming from LocalRouter: `apexrouter migrate --dry-run` prints the whole plan and writes
nothing, which is the default. It reads `~/.vastai-gguf` and never writes to it
(`[compat] mirror_usage_log` is off — an acceptance run once appended 15 rows to the real
`usage.log` and they had to be restored). Credentials are imported as a **reference**
(`api_key_env` / `api_key_file`), never copied; this gate confirmed no key text reaches any file
ApexRouter writes.

Then point everything at `http://127.0.0.1:8888/v1` with `model: "auto"` — including
`ANTHROPIC_BASE_URL`, which is what lets Claude Code drive a local or rented model — and do not
change it again.
