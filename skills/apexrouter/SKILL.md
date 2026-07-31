---
name: apexrouter
description: Run, route and pay for local and rented inference through ApexRouter-RS — USE WHEN you need an OpenAI- or Anthropic-compatible base URL to point an SDK/agent at, when asked "what model do I have / what's running / what can I run", to start, stop, swap or size a local llama.cpp or vLLM endpoint, to ask what fits in VRAM, to search or download GGUF weights from HuggingFace, to rent or destroy a vast.ai GPU box (this spends real money), to see tokens/cost/tok-s, or when an inference request is 404ing, 503ing or answering from the wrong backend. Works via MCP tools, CLI, or REST.
---

# ApexRouter — one base URL that never changes

ApexRouter holds a **routing table**: named aliases (`auto`, `coder`, `big`) pointing at ordered
chains of live OpenAI-compatible backends — a local `llama-server`, a rented vast.ai box, a
managed provider, a LAN node. It serves that table on `http://127.0.0.1:8888/v1`, so every SDK on
the machine has one base URL and one `model` string that never change while the thing behind them
does. Everything else it does — finding llama.cpp builds and GGUFs, solving what fits in VRAM,
spawning and supervising `llama-server`, renting a GPU and tunnelling it home — exists to put rows
in that table and keep them honest.

## The base URL, correctly (read this before you write a client)

```
OPENAI_BASE_URL=http://127.0.0.1:8888/v1      # canonical
OPENAI_BASE_URL=http://127.0.0.1:8888         # also correct — the proxy normalises /v1
OPENAI_API_KEY=not-needed                     # any non-empty string
ANTHROPIC_BASE_URL=http://127.0.0.1:8888      # POST /v1/messages, translated to the upstream
model: "auto"                                 # an ALIAS, not an upstream model id
```

Both forms work; so does a doubled `/v1`. Never hand-build `…:8888/v1/v1/chat/completions` on
purpose, but if a client does, it is answered rather than 404'd. The **control plane** is a
different socket — `http://127.0.0.1:2739` — and that is where "what is running / start this /
what did it cost" lives. Do not send completions there, and do not send control calls to 8888.

Get the live values instead of trusting this file: `apexrouter url`, `apexrouter env`, or the
`how_to_use` block at the top of every `apexrouter_status` result.

## Pick your surface (in order of preference)

1. **MCP tools** (if `mcp__apexrouter__*` / `apexrouter_*` are loaded). 24 tools, all prefixed
   `apexrouter_`. Start with `apexrouter_status` — it returns the base URL, the `model` string,
   every alias and where it points, backend health, the rig, in-flight, 24 h spend and vast
   credit. Read-only tools answer **with the daemon down** (`served_by: "offline"`).

   | Want | Call |
   |---|---|
   | what is my situation | `apexrouter_status` |
   | what `model` string do I send | `apexrouter_models` |
   | GPUs, VRAM, builds | `apexrouter_rig` |
   | will this fit, at what ctx | `apexrouter_fit {model, ctx?, parallel?, kv?, devices?}` |
   | start it and bind an alias | `apexrouter_up {model, alias?, ctx?, wait?}` |
   | it failed — why | `apexrouter_logs {id, tail?}` |
   | swap the model behind an alias | `apexrouter_swap {alias, to, mode?}` |
   | point an alias at a chain | `apexrouter_route_set {alias, targets[], strategy?, failover?, default?}` |
   | is this endpoint actually working | `apexrouter_smoke {alias?}` |
   | find / size / download weights | `apexrouter_hf_search` → `apexrouter_hf_files` → `apexrouter_hf_get` |
   | what is on the vast market | `apexrouter_vast_offers` *(free, read-only)* |
   | **rent a box** | `apexrouter_vast_rent {offer_id\|profile, launch, confirm, max_usd_per_hour}` **— spends money** |
   | **destroy a box** | `apexrouter_vast_destroy {id, confirm}` |
   | tokens, cost, tok/s | `apexrouter_usage {since?, by?}` |
   | which of these should I use | `apexrouter_compare {aliases[], prompt, max_tokens?}` |
   | something is wrong | `apexrouter_diagnose {only?}` |

2. **CLI** — `apexrouter`, one binary, `--json` on every leaf verb (never global). Pure and
   read-only verbs work with no daemon; mutating verbs autostart one unless `--no-autostart`.

   ```sh
   apexrouter                      # bare = status
   apexrouter url                  # prints http://127.0.0.1:8888/v1 and nothing else
   apexrouter env                  # the three exports, ready to eval
   apexrouter models ls            # local GGUFs (shards grouped, real sizes)
   apexrouter rig                  # GPUs free/total, who holds them, builds, RAM, swap
   apexrouter fit <model> --ctx 32768 --kv q8_0 --json
   apexrouter up <model> --alias auto          # resolve → fit → spawn → health-gate → bind
   apexrouter endpoint ls | logs <id> -f | stop <id> | argv <id>
   apexrouter route ls | set <alias> --target <backend>[:<model>] --strategy first-healthy
   apexrouter swap <alias> --to <backend|recipe|model>
   apexrouter smoke --alias auto               # 4 probes with TTFT and tok/s
   apexrouter usage --since 7d --by day
   apexrouter doctor                           # the check registry, one fix line per row
   apexrouter serve --detach                   # start the daemon and return when /health answers
   ```

   `apexrouter mcp` is **intercepted before clap**, so it does not appear in `apexrouter --help`
   and its own `--help` prints to **stderr**. That is deliberate: stdout is the MCP protocol.

3. **REST** — control plane at `http://127.0.0.1:2739`, everything under `/v1/`. Full reference:
   `docs/API.md`; machine-readable: `openapi/apexrouter-v1.yaml`.

   ```sh
   curl -s 127.0.0.1:2739/v1/snapshot            # the whole world in one object
   curl -s 127.0.0.1:2739/v1/rig
   curl -s '127.0.0.1:2739/v1/fit?model=Carnice-9b&ctx=32768&kv=q8_0'
   curl -s 127.0.0.1:2739/v1/usage?since=24h
   # a WebSocket at /ws pushes a Snapshot on connect and Events after
   ```

   Loopback needs no token. A non-loopback bind refuses to start without one
   (`apexrouter token create`, then `APEXROUTER_TOKEN`).

## Knowledge you need

- **`model` is an alias, not an upstream id.** `auto` is the default alias. An *upstream* id
  (`Carnice-9b-Q6_K`) also routes if exactly one enabled backend advertises it; a
  `backend-id/model` pin routes to exactly that one. `""`, `"x"`, `"auto"`, `"default"` and an
  absent `model` all reach the default alias — which is why old scripts keep working. **A model
  string nobody advertises is `404 model_not_found` by default** (`[router] unknown_model =
  "reject"`), deliberately: a fat-fingered id must not silently bill a rented H100. Call
  `apexrouter_models` before choosing one.
- **Every response says how it was routed.** `X-ApexRouter-Route: <alias>|<reason>`, plus
  `X-ApexRouter-Backend`, `-Attempts`, `-Fallback`, and `X-ApexRouter-Protocol:
  anthropic->open_ai` when a translation ran. When something answered surprisingly, read those
  headers before theorising.
- **Never retried after the first upstream byte.** Failover happens on connect/DNS/TLS failures,
  `429` with `Retry-After`, and `502/503/504/529`. Anything else is relayed verbatim. A stream
  that dies mid-flight gets one synthetic `data: {"error":…}` frame then `data: [DONE]` — never a
  silent truncation.
- **Streams carry no `X-Usage`.** Headers flush before the first chunk. Streams get
  `X-ApexRouter-Usage-Deferred: true`; the real numbers land in `apexrouter usage`, the WS event
  and the live-request table.
- **VRAM: never add two GPU rows together.** One physical card is enumerated once per compute
  backend — the same Radeon 840M is `ROCm0` (11.1 GiB) *and* `Vulkan0` (20.5 GiB). Both readings
  are true; neither may be summed. And an APU can report **free > total** (GTT accounting), so
  `total - free` is not "used", it is an underflow. Ask `apexrouter_fit`, which does this
  correctly, rather than doing the arithmetic yourself.
- **`ctx` is the TOTAL pool shared across `parallel` slots**, not per slot. `parallel: 4` with
  `ctx: 32768` gives each slot 8192.
- **Money is gated in the daemon, not by prompt.** `apexrouter_vast_rent` without both
  `confirm: true` and a positive `max_usd_per_hour` creates nothing and returns the **full cost
  preview** — $/hr, 1 h and 24 h projections, the daemon's hard ceiling, remaining credit. There
  is a hard `[vast] max_usd_per_hour_ceiling` an agent cannot exceed, and optionally a human
  approval (`apexrouter approvals grant <id>`). Nothing is ever auto-destroyed; ask before
  spending, and say the number out loud.
- **A local model may be a *thinking* model**: `content` empty, the answer in `reasoning_content`.
  That is the model, not a routing bug. Start it with `--mode nonthinking` if the client insists
  on `content`.
- **Anthropic ingress**: `POST /v1/messages` requires `max_tokens` (400 otherwise, with an
  Anthropic-shaped body). `tools` are translated by default (`[router] anthropic_tools = true`)
  and the translation is **best-effort** — parallel tool calls, some `tool_choice` variants and a
  block-array `tool_result` may not survive. Only if an operator sets the key **explicitly to
  `false`** are `tools` **refused with a clear error** naming it. `thinking` blocks and
  `count_tokens` are `501` — deliberately, because a fabricated token count is worse than an error
  you can fall back from.
- **Offline is a first-class answer.** Every `--json` envelope and every MCP result carries
  `served_by` (`daemon` | `offline`), `as_of_unix` and `stale`. An offline answer is facts from
  disk with health and throughput left at zero rather than invented.

## Patterns

- **"Point me at a model"** → `apexrouter_status`. Use `how_to_use.openai_base_url` and
  `how_to_use.model` verbatim. Do not construct a URL from memory.
- **"Run model X locally"** → `apexrouter_fit {model:"X"}` first (instant, no side effects), then
  `apexrouter_up {model:"X", alias:"auto"}`. If `up` fails, the next call is
  `apexrouter_logs {id}` — the reason is nearly always in the last 50 lines, and guessing costs a
  turn.
- **"Switch to a different model without breaking clients"** → `apexrouter_swap {alias:"auto",
  to:…}`. One call, and the alias never moves from the client's point of view. Do not stop and
  start endpoints by hand and then re-point a route; that is the four-call version of this.
- **"Is it working?"** → `apexrouter_smoke {alias:"auto"}`. Four probes with TTFT and tok/s read
  from the upstream's own `timings`, not stopwatched. Make this call before committing a long
  agent run to an endpoint you have not used yet.
- **"I need a bigger GPU"** → `apexrouter_vast_offers` (free) → present $/hr and total to the
  human → only then `apexrouter_vast_rent` with `confirm: true` and their number. Afterwards,
  **remind them the meter is running**, and use `apexrouter_vast_destroy` when the work is done.
- **"Get me weights"** → `apexrouter_hf_search` → `apexrouter_hf_files` (authoritative sizes from
  `paths-info`, shards summed) → `apexrouter_hf_get {repo, quant, no_wait: true}`. Use `no_wait`;
  a 20 GB download outlives any tool timeout.
- **"Which model should I use for this?"** → `apexrouter_compare {aliases:[…], prompt}`. One
  prompt, N aliases in parallel, with latency, tok/s, real token counts and cost per row.
- **A request 404s** → `apexrouter_models` and compare the string you sent. **A request 503s** →
  `apexrouter_status` for backend health, then `apexrouter_logs`. **A request went to the wrong
  place** → read `X-ApexRouter-Route`. **Everything is odd** → `apexrouter_diagnose`.
- **Before saying "you need to install/configure X"** → run `apexrouter_diagnose` or
  `apexrouter doctor`. It carries a fix line per row, and it is usually already configured.

## Where the details are

`docs/API.md` (every route with a jsonc example) · `docs/ROUTING.md` (the six resolution rules) ·
`docs/AGENTS.md` (registering the MCP server in a harness — Claude Code, ApexOS) ·
`docs/ARCHITECTURE.md` (normative design) · `docs/SLINT.md` (the native GUI) ·
`openapi/apexrouter-v1.yaml`.
