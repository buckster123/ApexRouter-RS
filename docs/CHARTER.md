# ApexRouter-RS — Project Charter

> **One base URL, forever.** Point every agent, SDK and script on the machine at
> `http://127.0.0.1:8888/v1` with `model: "auto"` and never touch either again while the thing
> behind it changes from an iGPU to a rented 2×H100 and back.

*Charter locked 2026-07-31, at the end of the mk1 build. The decisions log below is **binding**:
change a locked decision only by adding a dated entry to the amendments log at the bottom, never
silently. `docs/ARCHITECTURE.md` is normative for **how**; this file is the record of **why**, and
of what was deliberately left out.*

---

## The problem

LocalRouter — the Python TUI this project ports — has a 417-line proxy that reads a JSON file per
request and forwards bytes to whatever it says. `docs/port/05-proxy.md` §14 lists sixteen things a
serious router needs and marks all sixteen absent. Fourteen TUI menus, 71 recipes and 19 GPU tiers
exist to set one string in that one file. Four separate implementations of "what is active"
disagreed with each other. A rented GPU was created and never recorded — a silent billing leak
against an account with $7.73 of credit and $3.34/hr boxes.

Three failure modes this project exists to kill:

1. **The base URL moves.** Switching backends silently breaks every client, because the `model`
   string goes upstream verbatim and nothing aliases it.
2. **Restarting the manager evicts the model.** A 30 GB model that took three minutes to load
   should not share a lifetime with a supervisor you want to restart freely.
3. **Money leaks quietly.** A box billing overnight with no local record; a crash that deletes one
   you are still paying for.

## What it is

One Rust daemon holding a **routing table** — named aliases pointing at ordered chains of live
OpenAI-compatible backends — served on `127.0.0.1:8888/v1`. Everything else it does (discovering
llama.cpp builds, GGUF weights and GPUs; solving what fits in VRAM; spawning and supervising
`llama-server`; renting a vast.ai GPU and tunnelling it home; talking to together.ai; registering a
LAN box) exists to **put rows in that table and keep them honest**. Five surfaces share one
serde-only protocol crate: REST + WebSocket, an embedded no-build web UI, a native Slint app, a
`clap` CLI, and an MCP stdio server for local agents.

## What it is not

- Not a model zoo, not a training tool, not a quantiser. We find, size, download and serve GGUFs.
- Not a cloud service. Loopback by default; a non-loopback bind refuses to start without auth.
- Not a TUI (D14).
- Not an Anthropic-upstream emulator (D13).

---

## Locked decisions

| # | Decision | Rationale |
|---|---|---|
| **D1** | **The routing table is the primary data structure.** Every feature is a way to add a row to it. | Inverting LocalRouter — where the table was one string in one file — makes most of its structure evaporate rather than get ported. Model aliasing is the feature that makes the whole thing worth running. |
| **D2** | **Two listeners in one process**: proxy/data plane `127.0.0.1:8888`, control plane `127.0.0.1:2739`. | A single listener cannot satisfy both contracts. The proxy is a catch-all *by contract*; a catch-all `any()` route and the static-asset `get("/{*path}")` route **panic on `Router::merge` in axum 0.8**. A shared socket also permanently shadows llama.cpp's `/health` for control clients. Two listeners let the two `/health` endpoints keep two different shapes and make it possible to expose the proxy to the LAN without exposing the control plane. The user only ever types `8888`. |
| **D3** | **The control plane serves `/v1/*`, not `/api/*`.** | House sibling Prefrontal-RS uses `/api`; ApexRouter does not, deliberately. Its clients are versioned API clients (CLI, Slint, MCP, ApexOS), the proxy's own `/v1` lives on a *different socket* so there is no collision, and `openapi/apexrouter-v1.yaml` is a real versioned contract. `GET /health` and `/metrics` are the only unversioned control paths. |
| **D4** | **Children outlive the manager.** `llama-server` and `ssh -L` are spawned with `setsid()`, stdio into an owned `File`, `ProcIdentity` written to `$STATE` **before** the spawn function returns. `[supervisor] kill_children_on_exit = false` is the default. | A `systemctl --user restart`, a crash, or a `cargo install` must not evict a model that took 90 seconds and 6 GB to load. This is what makes `serve --stop` safe to type while a model is hot. |
| **D5** | **Persisted records hold facts; status is computed on read.** `pid`, `start_time_ticks`, `boot_id`, `port`, `argv`, `desired_state` — and nothing else. | LocalRouter's four disagreeing authorities were cured not by adding a fifth but by persisting only what cannot be recomputed. `status: "running"` on disk is a lie the moment someone types `kill`. |
| **D6** | **`[router] unknown_model = "reject"` by default.** A model string that matches no alias, no backend model, and no pin returns `404 model_not_found`. | A typo must fail loudly. `fallback` exists for people who want the old behaviour, but silently serving a *different* model than the one asked for is how an agent spends an hour debugging the wrong thing. |
| **D7** | **`[compat] legacy_proxy_pidfile = false` by default.** | LocalRouter's `_proxy_down()` reads `/tmp/vastai-gguf-proxy.pid` and SIGTERMs whatever it names. Turning it on hands the old TUI's "Proxy → stop" menu item a kill switch for the entire daemon. When on, SIGTERM still drains gracefully and local children survive via `setsid`, but the routing table, tunnels and watchdogs go with it. The reason lives in the config comment, not only here. |
| **D8** | **`X-Usage` is emitted on buffered responses only.** Streams get `X-ApexRouter-Usage-Deferred: true`; the numbers land in `usage.jsonl`, the WS event and the live-request table. | Response headers flush before the first SSE chunk and usage arrives in the last one. A streaming `X-Usage` would be absent or fabricated. This is a **stated, tested divergence** from LocalRouter, not an oversight. The buffered header keeps LocalRouter's exact `"{prompt}+{completion}"` format. |
| **D9** | **The three legacy compat routes — `GET/HEAD /health`, `GET/HEAD /providers`, `POST /switch` — are byte-compatible through mk1 and are REMOVED AT 1.0.** | They exist so LocalRouter's own unmodified `smoke.sh` and the old TUI keep working during the transition; they are not a second API. Their replacements are already shipped and documented on the control plane (`GET /v1/snapshot`, `GET /v1/backends`, `PUT /v1/routes/{alias}`, `POST /v1/routes/default`). **Commitment:** before 1.0, a `legacy.traffic` check is added to the registry so `apexrouter doctor` (and `GET /v1/diagnose`, and the `apexrouter_diagnose` MCP tool — the same registry, three transports) warns while the old routes are still being used, and the removal is not a surprise. Deprecation is a decision made now, in writing, rather than a compatibility surface that accretes forever. |
| **D10** | **Money safety is structural.** The ledger row is written **before** the billing call; `ledger.jsonl` is append-only and "active" is a *query*; a `SpendApproval` cannot be fabricated by any code path; there is a daemon-side hard ceiling; startup reconciles against the live account. **Nothing that costs money is ever auto-destroyed, at any setting.** | The failure it prevents has already happened in this codebase. A crash must not delete a paid box, and a leak must be visible as an alert rather than as a surprise on the invoice. |
| **D11** | **One XDG state dir; nothing is ever written into a repo. `~/.vastai-gguf/` is another tool's directory and is read-only by default** (`[compat] mirror_usage_log = false`). | Merely starting the daemon must never mutate another tool's state. An acceptance run appended 15 rows to the real `usage.log` and they had to be restored; the mirror is now opt-in, offered by `apexrouter migrate`, which is the case it exists for. Migration must also treat **stale** legacy state (an instance record pointing at a model path that no longer exists) as the normal case, not an error. |
| **D12** | **`apexrouter-slint` is GPL-3.0-only and is NOT in `default-members`.** The other seven crates are `MIT OR Apache-2.0`. | Slint's GPL option, taken deliberately. Keeping the GUI crate out of the default workspace members means the headless node never links it and never inherits the obligation. See `docs/LICENSING.md`. |
| **D13** | **Anthropic ingress (`POST /v1/messages`) is IN mk1. OpenAI → Anthropic translation is PERMANENTLY OUT.** | *Amendment, see log.* Pointing `ANTHROPIC_BASE_URL` at ApexRouter is what lets the Claude Code harness drive a local or rented model — inference is exactly the point. The reverse direction has no user: ApexOS-RS already speaks Anthropic natively and calls `api.anthropic.com` with a real key. Building it would mean maintaining a second, unexercised translator. The matrix cell returns `501` with an OpenAI-shaped body. |
| **D14** | **No TUI. Declined outright, not deferred.** | The CLI with `--json` on every subcommand, plus a web UI and a Slint app, covers every use LocalRouter's fourteen TUI menus covered. A TUI is a third frontend to keep in sync with the protocol for no capability gain. |
| **D15** | **Linux only in mk1.** | The process model is `/proc`, `flock`, `setsid` and `boot_id`. `Backend::Metal` exists in the enum so the data model does not have to change later, but nothing pretends to be portable that is not. |
| **D16** | **One of each.** One `resolve()`, one argv builder, one `fit()` solver, one credential chain, one check registry, one SSE relay, one merge point for control routes. | Every duplicated table in LocalRouter — docker images in three places, GPU pricing in four, sampling presets in two that disagreed, "what is active" in four — became a bug. The port's job is not to translate them into Rust; it is to have one of each. |
| **D17** | **No sqlite.** Everything a human might `cat` or a script might `tail` stays a file. | `jsonl` + `toml` is greppable, diffable and recoverable by hand. If usage aggregation ever needs SQL, copy Imaginarium-RS's `Mutex<Connection>` + `migrate()` + terminal-guard pattern verbatim rather than inventing one. |
| **D18** | **MCP is stdio-only in mk1**, hand-rolled newline JSON-RPC, `initialize` echoing the client's requested `protocolVersion`. | Streamable-HTTP is real work with no user today, and the deprecated HTTP+SSE transport will never be implemented. `fn dispatch(method, params)` is transport-agnostic, so an axum route is a day's work when ApexOS-RV nodes need it over the network. |

---

## Deliberately out of mk1

Each with the reason, so a future reader knows it was a decision and not an oversight. This is the
long form of `ARCHITECTURE.md` §12; where the two differ, that one is normative for scope and this
one for reasoning.

**Permanently out**

- **OpenAI → Anthropic translation** (D13). `501`, OpenAI-shaped body.
- **`Strategy::Mirror`, `Strategy::Fastest`, sticky sessions.** Not in the enum at all, so no config
  value can reach an unimplemented arm. Batch comparison ships instead as `POST /v1/compare` /
  `apexrouter compare`, which is what the feature was actually wanted for.
- **A TUI** (D14). **CORS** — no browser client is cross-origin, the embedded UI is same-origin, and
  the mutation gate is a stronger defence than a CORS policy. **`sqlite`** (D17).
- **The deprecated MCP HTTP+SSE transport** (D18).

**Out of mk1, honestly deferred**

- **Perfect Anthropic tool-use translation.** `[router] anthropic_tools` is **off by default**, and
  when on it is *allowed to be imperfect* — which is the honest statement rather than a promise we
  would quietly break. `input_schema`/`parameters` and `tool_use`/`tool_calls` map cleanly; parallel
  tool calls, `tool_choice` variants, and a `tool_result` whose content is a block array do not map
  cleanly in every case. With the flag off, a `/v1/messages` body carrying `tools` is **refused with
  a clear error** — never silently stripped and answered wrongly, which is the failure mode that
  actually costs an agent an hour.
  **The default was reversed on 2026-07-31 — see the amendments log.** Everything else in this
  bullet still holds: translation remains best-effort, and an explicit `false` still refuses loudly.
- **`thinking` blocks and `POST /v1/messages/count_tokens`.** There is no OpenAI-side equivalent of
  an Anthropic `thinking` block, so mk1 neither synthesises one on the way out nor accepts one on
  the way in. llama.cpp b9199's `--reasoning-format` can emit `reasoning_content`; mk1 records that
  it exists and does **not** map it onto `thinking`. `count_tokens` is `501` for the same reason:
  the only honest answer needs a tokenizer we do not have, and a fabricated count is worse than an
  error the client can fall back from. This is the same honesty rule `TokenCount` and `CostEstimate`
  exist to enforce.
- **llama.cpp router mode** (`--models-dir`, `POST /models/load`, `--sleep-idle-seconds`) and
  **idle-unload**. b9199 already has it and it overlaps our supervision job; mk1 keeps direct
  single-model supervision because it matches the state model and the failure modes we understand.
  Filed as the mk2 *simplification*, not the mk2 feature.
- **Vast bidding (interruptible instances), volumes, multi-region orchestration.** On-demand
  `type: "ask"` only — an interruptible box that dies mid-generation is a worse product than a
  slightly dearer one that does not.
- **GPU-mesh scheduling across LAN nodes.** A LAN node is a `Node` backend today, which is the cheap
  90%. mDNS discovery and capacity-aware placement are mk2; nothing here architects them out.
- **Automatic model conversion / quantisation.** We do not run `llama-quantize`.
- **Windows and macOS** (D15).

---

## Acceptance, as it actually happened

- **MK1-CORE ACCEPTANCE** (`BUILD-PLAN.md` §7.1), on the real laptop, real release binary, real
  `Carnice-9b-Q6_K` on the real Vulkan build: **9.71 tok/s** generation, **53.71 tok/s** prompt eval
  from llama.cpp's own `timings`, corroborated by ApexRouter's `tok_per_s_p50` = 9.69 over 12
  requests; model load 7.39 s. GPU offload proven from `/sys` `mem_info_gtt_used` = 10.2 GB and two
  fds on `/dev/dri/renderD128`, not assumed. LocalRouter's own unmodified `smoke.sh` passed 4/4 in
  **both** base-URL forms. A daemon restart re-adopted the hot model by `(pid, start_time, boot_id)`.
  `X-Usage: 11+20` matched the body's token counts exactly.
- Every stage gate found real defects, each proved with a failing test before the fix. The ones
  worth remembering are in `CLAUDE.md` under "Sharp edges"; the recurring one — a module shipped
  implemented, tested and **unreachable** — now has a dedicated guard,
  `crates/apexrouter-server/tests/mounted_routes.rs`.

---

## Amendments log

- **2026-07-30** — *Anthropic ingress moved INTO mk1* (D13). The original reasoning — "Claude Code
  reaches ApexRouter through MCP for control, not for inference" — was wrong: inference is exactly
  the point. `Protocol::{OpenAi, Anthropic}` became a first-class field on `Backend`, `NodeSpec` and
  `ManagedSpec`, `ingress` was added to `RequestRecord`, and translation became work unit R-10 in
  Stage 5. OpenAI → Anthropic translation stayed permanently out.
- **2026-07-30** — *`[compat] mirror_usage_log` defaulted OFF* (D11), after MK1-CORE ACCEPTANCE
  finding B: an acceptance run appended 15 rows to the real `~/.vastai-gguf/usage.log`. Starting the
  daemon must never write into another tool's state directory.
- **2026-07-30** — *`RetryPolicy` became per-route* (authorised signature change: `Plan` gained
  `pub retry: RetryPolicy`). `routes.toml`'s `[retry]` block previously had no effect at all; all
  three fields including `honor_retry_after` are now live.
- **2026-07-30** — *One SSE relay* (D16). `relay::stream::sse_response` survives; `handler::relay`'s
  duplicate was deleted. The merge exposed two real bugs: the handler never set `guard.record`, so a
  client Ctrl-C emitted no `RequestFinished` at all, and the old relay silently truncated a stream
  that EOF'd without `[DONE]`.
- **2026-07-31** — **Charter locked.** D1–D18 recorded. D9 (legacy compat routes removed at 1.0) and
  D3 (`/v1` rather than `/api` on the control plane) are written down here for the first time; both
  describe the code as shipped, but neither had been recorded as a decision before.
- **2026-07-31** — **`[router] anthropic_tools` defaults to `true`** (reverses the "off by default"
  half of the "Perfect Anthropic tool-use translation" entry above, which is left in place rather
  than rewritten). The original reasoning was that tool translation is the imperfect part and should
  therefore be opt-in. That is wrong in one specific way: **the alternative to imperfect tool
  translation is not "no tools", it is "the feature does not work at all".** Real Claude Code sends
  **92 tool definitions on every single request**; tools are not an optional part of that client. So
  with the flag off, a stock config answered the flagship Anthropic client with
  `400 tool translation is off: set [router] anthropic_tools = true to enable it` on request one,
  and "point every agent at it and never change it again" was false for exactly the agent the
  ingress was built for.
  What does **not** change: translation is still declared best-effort and is still allowed to be
  imperfect (parallel tool calls, some `tool_choice` variants, a block-array `tool_result`); and an
  operator who sets `anthropic_tools = false` explicitly still gets the loud, remediation-naming
  refusal rather than silently stripped tools. That refusal was always the good part of the
  decision, and it is untouched.
  Changed: `RouterCfg::default()`, `config.example.toml`, `docs/API.md` §2.5, `README.md`.
  `docs/ARCHITECTURE.md` §5.2's config listing, `docs/AGENTS.md` and `skills/apexrouter/SKILL.md`
  still describe the old default and belong to other units — they must be brought in line.
- **2026-07-31** — **`warm_timeout` is patience, not a stopwatch** (§4.7). A sequential swap's warm
  window now **re-arms its deadline for as long as the launch it is waiting on is alive**, instead
  of expiring a fixed `health_deadline_ms + drain_timeout_secs` after it opened.
  The measurement that forced it: `warm_timeout` 3000 ms against a swap that ran **12,038 ms**. The
  4 parked requests were `503`'d at 2977 ms and the alias then produced a plain
  `no_healthy_backend` storm of **74,550 requests** over the remaining nine seconds — the outage
  §4.7 exists to prevent, merely delayed, and delayed past the point where every client has already
  retried. Re-run against the re-armed window (`tests/warm_rearm.rs`, fake `llama-server`,
  `load_ms=12000`, the same 3000 ms budget): **zero 5xx**, 437/437 × 200, 8 parked at peak, 240
  re-arms. The same 12 s outage with the re-arm removed, measured in the same run: **10,663 × 5xx**
  out of 10,887.
  Why it is not simply a bigger number: `[supervisor] health_deadline_ms` is not how long a launch
  may take, it is how long it may spend making **no progress** — the health gate resets it on every
  `503 {"status":"loading model"}` and every recognised log line — so the real start budget is
  unbounded while a load is progressing, and *any* fixed park is eventually shorter than it. The
  gate and the warm queue are waiting on the same event, so they must share one liveness signal. A
  park that gives up while the load is demonstrably progressing converts a survivable wait into the
  outage.
  What the signal is, precisely: **the launch future still being pending**, sampled on the gate's
  own `health_interval_ms` clock. The gate is defined to return within `health_deadline_ms` of the
  last progress it observed, so "`up()` has not returned" *is* "progress was observed recently",
  with no second opinion that can drift from the first. `Event::BootProgress` was considered and
  rejected as the trigger: it is emitted once per **transition**, never per tick, so the per-tick
  `loading` reset — the signal that carries a quiet mmap of a 30 GB file — never reaches the event
  bus, and a park re-armed from it would be strictly *less* patient than the gate it waits on.
  The bound is kept, as the ruling requires: the deadline still fires, it simply now measures wall
  clock **since the last sign of life** rather than since the swap began. A closed or superseded
  window refuses to re-arm, so a leaked pacemaker cannot hold requests open.
  Changed: `router/src/registry.rs` (`WarmWindow::rearm`, `WarmSlot::budget_ms`/`rearms`),
  `server/src/api/routes.rs` (`start_while_parked`), `docs/ARCHITECTURE.md` §4.7.
  *Completed at the final gate:* `[router] warm_queue_max` is now a field on
  `core::config::RouterCfg` (default 32) and `api/routes.rs::warm_queue_max` reads it. It had been
  named by §4.7 and `docs/API.md` while not existing, so writing it into a config file earned the
  "key is not one this build knows" warning that the *same day's* unknown-key work had just added —
  the two defects found each other. Verified by moving the bound under one unchanged 12 s swap with
  twelve clients: at `4`, peak parked 4 and the rest `503 warm_queue_full`; at `16`, all twelve park
  and the swap costs zero 5xx.
- **2026-07-31** — **`GET /v1/endpoints/{id}/argv` describes the launch that happened, not one that
  might.** The route called `supervisor.plan(&spec)`, which re-scans the rig, re-solves `fit()`
  against currently-free VRAM and leases a fresh port — a hypothetical *second* launch. Measured
  after a VRAM budget change, the daemon served **34** argv tokens where `/proc/<pid>/cmdline` had
  **36**: `-c 4096` for a child running `-c 32768`, and `-ngl 999` gone entirely, i.e. it described
  a CPU-only launch for a fully-offloaded child — with `warnings` empty, so nothing said the
  preview and the process disagreed. This is the daemon-served route and therefore the **normal**
  answer, and an operator debugging a launch is the person least able to afford a plausible lie.
  It now renders from `ResolvedSpec::from_record`: the record's draft with the record's `fit`
  folded back in, at the port the record was leased, against the build the record names in the
  supervisor's own rig snapshot. Nothing is re-solved and nothing is re-leased.
  `ResolvedSpec::disagreements` folds any residual divergence into `warnings` rather than hiding
  it. Two new refusals, both naming the remedy: `409` when the record's build has left the machine
  (rendering against another build's `FlagSupport` would silently emit a different flag set), and
  `409` for an endpoint this daemon does not launch. A second symptom nobody had recorded is fixed
  in passing — the route printed a literal `<id>.key` placeholder instead of the real
  `$STATE/endpoints/<id>.key` path. Acceptance is against `/proc/<pid>/cmdline`, not against the
  other preview: with a daemon up both routes are the same code, so their agreement was
  tautological. Changed: `server/src/api/endpoints.rs`, `docs/API.md` §5.5.
- **2026-07-31** — **Two legacy sources that mint one recipe id MERGE, field by field; neither
  silently wins** (D11, D16). `migrate::apply` de-duplicated ids against the catalog it was writing
  into but not against the batch it was writing, and the two sources mint ids from independent
  sets. On this machine `local_instances/local-qwen35-9b.json` and
  `recipes.toml#recipes.local-qwen35-9b` collide, so `catalog.toml` held two entries under one id:
  the second unreachable, and `recipe rm` deleting both — the exact silent shadowing
  `catalog::upsert_recipe` exists to prevent.
  The rule is a merge and not a pick, because a pick throws away a fact somebody wrote down.
  Sources are ranked (`recipes.toml` launch plan > `.pinned_provider` > `local_instances`
  snapshot), but rank is only a tie-break: a field **one** source sets is always taken whatever its
  rank — which is what saves `ctx = 32768` — a field **both** set differently goes to the higher
  rank *and is named with both values in the warning*, `description`/`provenance.source` are
  concatenated rather than chosen, and two sources meaning different `RecipeKind`s are both kept
  with the second renamed `<id>-2`. The merge is announced in the `--dry-run` plan *before*
  anything is written, and again as a report warning afterwards.
  `catalog.rs` now enforces uniqueness independently at both ends: `read_file` renames a repeat in
  memory rather than dropping it (so an already-corrupted catalog heals with both definitions
  reachable) and `write_file` refuses one with `Error::Invalid`.
  **Operator-visible:** an existing `catalog.toml` holding a duplicate shows the second entry as
  `<id>-2` from the next read, with a stderr warning. That is the repair, not a regression.
  Changed: `core/src/migrate.rs`, `core/src/catalog.rs`, `docs/MIGRATION.md` §4.1.
- **2026-07-31** — **An unknown config key WARNS; it never refuses to start** (D5-adjacent). A
  mistyped key used to be ignored in total silence: `proxy_port` for `proxy_bind` left the daemon
  binding the default while `config show` printed the default back, so the operator had no way to
  tell the file was not doing what it said.
  `serde(deny_unknown_fields)` was considered and **rejected**: an older binary must survive a
  newer file, and a section from next month's build must never stop today's daemon from starting.
  So every unknown key is logged at `warn` on stderr when the file is read, and carried on
  `Config::unknown_keys`, which `serializable()` passes into `ConfigFile` — so `config show` and
  `config show --json` surface it with no CLI change. Detection is **schema-free**: the document
  the user wrote is diffed against `toml::Value::try_from(cfg.serializable())`, so free-form maps
  (`[providers.<id>]`, `[known_forks.<name>]`) need no special-casing. The suggestion ranks by
  longest shared prefix first and Levenshtein only as a tie-break, because `proxy_port` →
  `proxy_bind` is four substitutions and no distance threshold would ever find it.
  `unknown_keys` is declared first in `ConfigFile` and cleared in `save_to`, so a save can never
  materialise it as a key — and never deletes the operator's mistyped key either.
  Changed: `core/src/config.rs`, `config.example.toml`. `Config::validate`/`validate_file` are
  built and unrendered; `apexrouter config validate` is still owed (`docs/MIGRATION.md` §10.3).
