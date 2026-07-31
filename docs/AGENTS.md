# ApexRouter-RS for agents

> **Status: as-built.** Verified against the running daemon and the `apexrouter mcp` server on
> 2026-07-31. Companion to `skills/apexrouter/SKILL.md` (which is written *for* the agent);
> this file is the **operator's** page — how to register ApexRouter with a harness, in both
> modes, with snippets you can paste.

An agent meets ApexRouter through **two independent surfaces**, and conflating them is the
single most common setup mistake:

| Surface | Port | What it is for | Auth |
|---|---|---|---|
| **Inference** — the proxy | `127.0.0.1:8888` | the OpenAI- and Anthropic-shaped endpoints your SDK calls. `OPENAI_BASE_URL` / `ANTHROPIC_BASE_URL` point here | open on loopback; any non-empty key |
| **Control** — MCP or REST | `127.0.0.1:2739` | "what is running, start this model, what fits, rent a box, what did that cost" | one configured bearer; loopback bypass |

You usually want both. They are the same daemon.

---

## 1. The base URLs (the fact worth memorising)

```
OPENAI_BASE_URL=http://127.0.0.1:8888/v1     # works
OPENAI_BASE_URL=http://127.0.0.1:8888        # ALSO works — the proxy normalises /v1
ANTHROPIC_BASE_URL=http://127.0.0.1:8888     # /v1/messages is translated to the upstream
OPENAI_API_KEY=not-needed                    # any non-empty string, unless a token is configured
```

All three forms are live-verified: `/v1/models`, `/models` and even the doubled `/v1/v1/models`
all answer `200`. LocalRouter's own skill file told agents to use the form that 404'd; that class
of bug is gone by construction, and
`router::handler::tests::both_client_base_urls_work_and_neither_doubles_v1` keeps it that way
against an upstream that answers `/v1/…` only.

`apexrouter env` prints the exports; `apexrouter url` prints just the URL, for `$(…)`:

```console
$ apexrouter env
export OPENAI_BASE_URL=http://127.0.0.1:8888/v1
export OPENAI_API_KEY=not-needed
export ANTHROPIC_BASE_URL=http://127.0.0.1:8888
```

The `model` string is an **alias** — `auto` by default. It never changes when the thing behind it
does. `apexrouter_models` (MCP) or `GET /v1/models` lists every routable string.

---

## 2. Registering the MCP server

One binary, one verb: `apexrouter mcp`. Newline-delimited JSON-RPC 2.0 over **stdio**, 24 tools,
all prefixed `apexrouter_`. Nothing but protocol JSON reaches stdout; every log line goes to
stderr.

Two modes, and the difference is one flag:

| Mode | Command | Behaviour |
|---|---|---|
| **Local** (default) | `apexrouter mcp` | reads `$STATE` and config directly. Read-only tools (`status`, `models`, `rig`, `fit`, `logs`, `recipe_list`, `usage`) work **with the daemon down**, tagged `served_by: "offline"`. Mutations forward to a running daemon, or return a helpful `isError` naming `apexrouter serve --detach` |
| **Proxy** | `apexrouter mcp --proxy http://127.0.0.1:2739` | every tool is forwarded to that control plane. No `$STATE` access, therefore no offline mode. This is the mode for a **remote** node, or a sandboxed harness that cannot see the state dir |

`--proxy` wins over `$APEXROUTER_URL`; with neither, local mode. The bearer comes from the
variable named by `[server] token_env`, then `$APEXROUTER_TOKEN`.

### Claude Code — local mode

Project-scoped `.mcp.json` (the house pattern — `~/Projects/.mcp.json` already registers
`prefrontal` and `imaginarium` this way; add a third key beside them):

```json
{
  "mcpServers": {
    "apexrouter": {
      "command": "/home/andre/Projects/Inference/tools/ApexRouter-RS/target/release/apexrouter",
      "args": ["mcp"]
    }
  }
}
```

Or from the CLI: `claude mcp add apexrouter /path/to/apexrouter mcp`.

### Claude Code — proxy mode (a remote or sandboxed node)

```json
{
  "mcpServers": {
    "apexrouter": {
      "command": "/usr/local/bin/apexrouter",
      "args": ["mcp", "--proxy", "http://127.0.0.1:2739"],
      "env": {
        "APEXROUTER_TOKEN": "<64 hex chars from `apexrouter token create`>"
      }
    }
  }
}
```

`--proxy` may name any reachable node (`http://192.168.0.42:2739`). A non-loopback control plane
**refuses to start without a token**, so `APEXROUTER_TOKEN` is not optional there — mint one with
`apexrouter token create --scope write`.

### Claude Code — driving a local model *as the model*

The other half: point the harness's own inference at ApexRouter.

```sh
export ANTHROPIC_BASE_URL=http://127.0.0.1:8888
export ANTHROPIC_API_KEY=not-needed
claude
```

`POST /v1/messages` is translated to whatever the alias resolves to — request body, response
body, and the SSE stream, both directions. Live check:

```console
$ curl -s http://127.0.0.1:8888/v1/messages \
    -H 'anthropic-version: 2023-06-01' -H 'content-type: application/json' \
    -d '{"model":"auto","max_tokens":24,"messages":[{"role":"user","content":"Say OK."}]}' -D-
…
x-apexrouter-protocol: anthropic->open_ai
x-apexrouter-route: auto|alias
x-apexrouter-backend: local-carnice-9b-q6_k
```

Caveats that will bite, in the order they bite:

- **`max_tokens` is required** (the Anthropic API requires it). Omit it and you get a `400` with
  an **Anthropic-shaped** body, because an Anthropic SDK will parse the error as one.
- **Tools are refused, loudly, by default.** `[router] anthropic_tools = false` means a
  `/v1/messages` body carrying `tools` gets a `400` naming the config key rather than a silently
  tool-less answer. Turn it on knowing it is the imperfect part (`ARCHITECTURE.md` §12).
- **`thinking` blocks and `/v1/messages/count_tokens` are `501`.** Deliberate: there is no honest
  OpenAI-side equivalent, and a fabricated token count is worse than an error you can fall back
  from.
- Your local model may be a *thinking* model that puts its answer in `reasoning_content` with an
  empty `content` — start it with `--mode nonthinking` if a harness insists on `content`.

### ApexOS — `agentd` plugin

ApexOS registers MCP servers as plugins in `/etc/agentd/plugins.toml` (template:
`ApexOS-RS/config/plugins.toml`). Local mode, on a node that runs the daemon itself:

```toml
[[plugin]]
id      = "apexrouter"
cmd     = "/usr/local/bin/apexrouter"
args    = ["mcp"]
restart = "always"
[plugin.env]
APEXROUTER_HOME = "/var/lib/agentd/apexrouter"
RUST_LOG        = "warn"
```

Proxy mode, on a node whose inference lives on another box:

```toml
[[plugin]]
id      = "apexrouter"
cmd     = "/usr/local/bin/apexrouter"
args    = ["mcp", "--proxy", "http://192.168.0.42:2739"]
restart = "always"
[plugin.env]
APEXROUTER_TOKEN = "…"          # required: a non-loopback control plane always demands one
RUST_LOG         = "warn"
```

Then allow the tools in `/etc/agentd/policy.toml`. Read-only ones are safe to `allow`; the two
that spend money should stay `ask` (or be omitted entirely):

```toml
"apexrouter_status"       = "allow"
"apexrouter_models"       = "allow"
"apexrouter_rig"          = "allow"
"apexrouter_fit"          = "allow"
"apexrouter_usage"        = "allow"
"apexrouter_logs"         = "allow"
"apexrouter_vast_offers"  = "allow"    # market search creates nothing
"apexrouter_up"           = "ask"
"apexrouter_swap"         = "ask"
"apexrouter_vast_rent"    = "ask"      # SPENDS MONEY
"apexrouter_vast_destroy" = "ask"      # stops the meter, but is destructive
```

### ApexOS — as a compute node

ApexOS's Settings → "auto-discover compute" sweep (`agentd/crates/gateway/src/compute.rs`)
verifies a candidate by the **OpenAI list shape** of `GET /v1/models`, which ApexRouter emits
byte-exactly. But the sweep only probes **ports 11434, 8000, 1234 and 8080** — `8888` is not in
`OAI_PROBE_PORTS`. So the shape is necessary and not sufficient; do one of:

- **paste the URL** `http://<host>:8888/v1` into the Settings compute field (it is verified on
  adoption by the same shape check, so this works today), or
- **bind the proxy to a probed port**: `[server] proxy_bind = "0.0.0.0:8080"` (or
  `apexrouter serve --proxy-bind 0.0.0.0:8080`), which makes the sweep find it automatically.

Binding the proxy off-loopback is a deliberate exposure decision: the **proxy** may go on the LAN,
the **control plane** should not. They are separate listeners precisely so that choice exists.

### Any other stdio MCP client

```json
{ "command": "/path/to/apexrouter", "args": ["mcp"] }
```

`initialize` **echoes back the client's requested `protocolVersion`** (falling back to
`2024-11-05`), so every revision from the legacy one through 2026-07-28 connects without a shim.
Verified: a client asking for `2025-06-18` gets `2025-06-18`, plus `serverInfo` and an
`instructions` block that already tells the model the base URL and the `model` string.

---

## 3. The 24 tools, grouped by what they cost

Full descriptions come from `tools/list` and are deliberately long and operational — an agent
should get from `apexrouter_status` to a working `OPENAI_BASE_URL` without reading a doc.

| Free and read-only | Starts or stops things | **Spends money** |
|---|---|---|
| `status` `models` `rig` `fit` `logs` `usage` `recipe_list` `vast_offers` `diagnose` `smoke` `compare` `hf_search` `hf_files` | `up` `endpoint_start` `endpoint_stop` `swap` `route_set` `backend_set` `recipe_save` `recipe_run` `hf_get` (bandwidth + disk) | `vast_rent` `vast_destroy` |

Money rules, enforced in the daemon rather than by prompt:

- `apexrouter_vast_rent` without **both** `confirm: true` and a positive `max_usd_per_hour`
  creates nothing and returns an `isError` **carrying the full cost preview** — $/hr, projected
  1 h and 24 h totals, the daemon's hard ceiling and remaining credit. The refusal doubles as a
  dry run that shows the bill.
- `[vast] max_usd_per_hour_ceiling` (default `4.00`) is a daemon-side hard cap. An agent that
  fills in a bigger number still cannot exceed what the human configured.
- `[vast] require_human_confirm = true` makes an MCP-sourced approval come back as
  `HumanConfirmationRequired { pending }`; the human clears it with `apexrouter approvals grant
  <id>` or in either GUI. Nothing bills until they do.
- Instances are **never** auto-destroyed on shutdown, at any setting. A crash must not delete a
  paid box; a leak must be visible instead.

---

## 4. Checking it works

```console
$ apexrouter status                # is there a daemon, and what is bound
$ apexrouter smoke --alias auto    # four probes: models, warmup, tools, throughput
PROBE             OK    MS     TTFT  TOK/S  TOKENS  DETAIL
smoke.models      pass  0      -     -      -       1 model(s): Carnice-9b-Q6_K
smoke.warmup      FAIL  8812   580   9.76   80      answered, but with empty content
smoke.tools       pass  17415  4382  9.69   126     tool_calls: get_weather
smoke.throughput  pass  21233  602   9.71   200     200 tokens at 9.71 tok/s
$ apexrouter doctor                # the check registry, with a fix line per row
```

(The `smoke.warmup` row above is the thinking-model case: the answer arrived in
`reasoning_content`, and the probe is honest about `content` being empty rather than scoring it a
pass.)

To prove the MCP server itself, without a harness:

```sh
printf '%s\n' \
 '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"probe","version":"1"}}}' \
 '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
 | apexrouter mcp
```

Expect two single-line JSON responses and 24 tools. Anything non-JSON on stdout is a bug worth
reporting — that channel is the protocol.

---

## 5. Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| MCP server "crashed" at startup in the harness | a non-JSON byte on stdout | `apexrouter mcp` intercepts its own argv *before* clap precisely so a clap error cannot land there. If you see this, capture stderr and file it |
| `apexrouter --help` does not list `mcp` | by design — `mcp` never reaches clap | `apexrouter mcp --help` prints usage **on stderr** |
| Every mutation returns "needs a running daemon" | local mode, no daemon | `apexrouter serve --detach`, or switch to `--proxy` |
| `401`/`403` from the control plane | non-loopback bind without a token | `apexrouter token create --scope write`, then `APEXROUTER_TOKEN` in the plugin/server env |
| `404 model_not_found` on a model string | `[router] unknown_model = "reject"`, the default, and nothing in the table matched | the body lists the known aliases; `apexrouter_models` lists them with their backends. The refusal is the feature — a fat-fingered id must not silently bill a rented H100. `= "fallback"` restores LocalRouter's send-it-to-the-default behaviour |
| A request went somewhere unexpected | an implicit or fallback rule matched | read `X-ApexRouter-Route: <alias>\|<reason>` — `alias`, `explicit_pin`, `upstream_id_match`, `implicit_multi`, `default_fallback` or `legacy_model_name`. A refused request carries `-\|-`, because no route was chosen |
| Streamed responses have no `X-Usage` | deliberate | headers flush before the first SSE chunk. Streams carry `X-ApexRouter-Usage-Deferred: true`; the numbers land in `usage.jsonl`, the WS event and `apexrouter usage` |
