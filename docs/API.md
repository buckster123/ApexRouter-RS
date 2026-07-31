# HTTP API

> Normative source: `ARCHITECTURE.md` §6 (the route tables), §4.4 (streaming), §4.5 (headers and
> errors), §9 (security). The machine-readable form of this document is
> `openapi/apexrouter-v1.yaml`, and a test in CI diffs that file against the routes axum actually
> registers — see §11.

Every example below is **jsonc**: real JSON with comments naming the enum variants, so the set of
legal values is next to the field rather than three sections away. Comments are not part of the
wire format.

---

## 0. Two listeners, and the one thing you must not miss

| Listener | Default | What it is | Auth |
|---|---|---|---|
| **proxy** | `127.0.0.1:8888` | the OpenAI/Anthropic drop-in surface | none by default; the backend's credential is constructed outbound |
| **control** | `127.0.0.1:2739` | everything else — rig, routes, endpoints, providers, jobs | bearer/scope + the mutation gate (§2.3) |

> **Both `http://127.0.0.1:8888` and `http://127.0.0.1:8888/v1` work as client base URLs.**
> Every `Backend.base_url` is stored **without** `/v1`; inbound paths get a repeated leading `/v1`
> collapsed to one. This is mandatory, not a courtesy: `smoke.sh` appends `/v1` to whatever you give
> it, and LocalRouter's own `SKILL.md` told agents to use the form that 404s. Non-OpenAI paths
> (`/props`, `/metrics`) forward raw. A collapse is logged once per `(User-Agent, path)` at `debug`,
> so a genuinely broken client stays discoverable, and `GET /v1/diagnose` surfaces a
> "clients sending a doubled prefix" note.

The control listener's `/v1` lives on a different socket from the proxy's `/v1`, so there is no
collision and no disambiguation rule to remember.

---

## 1. Conventions

### 1.1 Types

Every control-plane response body is a protocol type from `apexrouter-protocol`. `--json` on the CLI
prints exactly the same type. The GUIs and the MCP server consume the same types. There is no
hand-rolled JSON anywhere on the control plane.

### 1.2 Errors

**Proxy listener** errors are OpenAI-shaped everywhere:

```jsonc
{
  "error": {
    "message": "unknown model 'gpt-4o-mimi'; known aliases: auto, coder, big, local",
    "type": "model_not_found", // model_not_found | upstream_unavailable | upstream_timeout
                               // | no_healthy_backend | server_overloaded | request_too_large
                               // | loop_detected | provider_not_configured | starting
                               // | redacted_endpoint | protocol_not_supported
                               // | warm_queue_full | warm_timeout
    "code": null,
    "param": null
  }
}
```

| `type` | Status | Meaning |
|---|---|---|
| `model_not_found` | 404 | rule 6 with `unknown_model = "reject"` |
| `upstream_unavailable` | 502 | the upstream answered wrongly or died |
| `upstream_timeout` | 504 | `headers_timeout_ms` or `idle_timeout_ms` elapsed |
| `no_healthy_backend` | 503 | every candidate filtered out or breaker-open |
| `server_overloaded` | 503 + `Retry-After` | admission control refused |
| `request_too_large` | 413 | over `max_body_bytes` |
| `loop_detected` | 508 | inbound `Via` already carried `apexrouter` |
| `provider_not_configured` | 503 | a credential is missing, not a network failure |
| `starting` | 503 | the target is in `Health::Starting` |
| `warm_queue_full` | 503 + `Retry-After` | the alias is mid-swap and `warm_queue_max` requests are already parked |
| `warm_timeout` | 503 + `Retry-After` | parked behind a swap that over-ran its whole budget |

The 502-vs-503 distinction is load-bearing in both house projects: 502 means "it answered and it was
wrong", 503 means "there was nothing to ask".

**The two warm codes are the sequential-swap exits** (`ARCHITECTURE.md` §4.7). While
`POST /v1/routes/{alias}/swap` is replacing what an alias points at, requests for that alias
**park** rather than fail: the window closes the moment the replacement can serve, and the parked
request is re-resolved against it and answered normally. `warm_queue_full` is the immediate refusal
once the queue bound (`warm_queue_max`, **32**) is already reached — deepening a queue that is
already the wrong answer only moves the failure later. `warm_timeout` is the backstop when the swap
itself over-ran; it is **not an independent number** but
`[supervisor] health_deadline_ms + [server] drain_timeout_secs`, floored at
`[router] queue_timeout_ms`, because a park shorter than the operation it waits on is an arithmetic
guarantee of failure — and it measures wall clock **since the last sign of life from the launch**,
not since the swap began, so a load that is still visibly progressing never trips it. Both carry
`Retry-After` in seconds, and both are rendered in the **envelope**
the client spoke — an Anthropic-ingress client gets `{"type":"error","error":{"type":"warm_queue_full",…}}`,
i.e. the Anthropic shape carrying the same code string. A park that *is* served is not an error at
all: it is reported by `X-ApexRouter-Warm` (§4) on an ordinary `200`.

**Control listener** errors are an `ErrorEnvelope`, so clients branch on `error.kind` rather than on
prose:

```jsonc
{
  "error": {
    "kind": "not_found",        // not_found | invalid_request | conflict | port_in_use
                                // | unauthorized | forbidden | upstream_unavailable | internal
    "message": "no endpoint 'local-carnice'",
    "param": "id",              // null when the failure is not about one parameter
    "code": null
  }
}
```

**An Anthropic-ingress request gets an Anthropic-shaped error**, because the client is an Anthropic
SDK and will parse it as one:

```jsonc
{
  "type": "error",
  "error": {
    "type": "invalid_request_error", // invalid_request_error | authentication_error
                                     // | permission_error | not_found_error | request_too_large
                                     // | rate_limit_error | api_error | overloaded_error
    "message": "max_tokens is required"
  }
}
```

Symmetrically, the `OpenAi → Anthropic` refusal carries an **OpenAI-shaped** body. The dialect of
the error always matches the dialect the client spoke.

### 1.3 `?no_wait=true`

The house pattern for anything long-running. Return `202` with a `JobRecord` immediately; the
spawned task flips the row to `Failed` on **every** error path including a `JoinError` from a panic,
so nothing sits `pending` forever.

```jsonc
{
  "id": "01JB2ZQK8H0000000000000000",
  "kind": "hf.download",       // hf.download | vast.rent | endpoint.start | compare | swap
  "state": "running",          // pending | running | succeeded | failed | cancelled
  "pct": 41.5,                 // null when progress is not derivable
  "message": "downloading shard 2 of 5",
  "started_unix": 1780000000,
  "finished_unix": null,
  "result": null,              // the protocol type the operation produced, once succeeded
  "error": null
}
```

Poll `GET /v1/jobs/{id}`, or subscribe to `GET /ws` and watch `job_changed`.

### 1.4 Auth (control listener)

A bearer token is accepted three ways: `Authorization: Bearer <t>`, `X-ApexRouter-Token: <t>`, or
`?token=<t>`. Scopes are `read | write | admin`, derived from `(path, method)`. Tokens are stored
hashed, shown once at mint, and their hashes are never serialised. `GET /health` is public.

`[server] loopback_bypass = true` (the default) skips the token for a genuinely loopback peer IP
read from `ConnectInfo`. **Absent connect-info fails closed**, never open.

### 1.5 The mutation gate

A loopback control plane is not a trust boundary — a cross-origin `fetch` with
`Content-Type: text/plain` is a CORS *simple request*, delivered without a preflight, and the
attacker never needs to read the response. So **every mutating request on either listener** passes
`require_mutation_origin()`:

1. `Host` must be in the bind allowlist (`127.0.0.1:PORT`, `localhost:PORT`, or a configured name).
   This closes DNS rebinding.
2. If `Origin` is present it must be same-origin; if `Sec-Fetch-Site` is present it must be
   `same-origin` or `none`.
3. Otherwise a bearer token with `write` scope is required.

`curl`, the CLI and the Slint app send neither header, so they pass rule 2 unchanged. There is no
`CorsLayer` on the authenticated API, ever.

---

## 2. Proxy listener — `127.0.0.1:8888`

| Method | Path | Behaviour |
|---|---|---|
| `POST` | `/v1/chat/completions` | routed; streaming or buffered |
| `POST` | `/v1/completions` | routed |
| `POST` | `/v1/embeddings` | routed, class `Embedding` |
| `POST` | `/v1/rerank` | routed if the target supports it |
| `POST` | `/v1/messages` | Anthropic ingress |
| `POST` | `/v1/messages/count_tokens` | `501`, Anthropic-shaped |
| `GET` | `/v1/models` | aggregated |
| `GET` | `/v1/models/{id}` | one entry |
| `GET`/`HEAD` | `/health` | always 200, never probes |
| `GET`/`HEAD` | `/providers` | the exact LocalRouter shape, plus additive keys |
| `POST` | `/switch` | retarget `default_alias` |
| `GET` | `/slots` | `403 redacted_endpoint` |
| `*` | anything else | opaque passthrough to the default alias's primary target |

Only `/health`, `/providers` and `/switch` are registered as explicit routes (each with its own
`.fallback(proxy_handler)`, so an unmatched method is forwarded rather than 405'd). Everything else
is served through `.fallback(any(proxy_handler))` — **not** a `/{*path}` route, because a catch-all
`any()` and the static-asset `get("/{*path}")` panic on `Router::merge` in axum 0.8.

### 2.1 `POST /v1/chat/completions`

Request — standard OpenAI, relayed byte-for-byte unless the alias differs from the upstream id, in
which case exactly one key (`model`) is rewritten:

```jsonc
{
  "model": "auto",             // an alias | "<backend_id>/<model>" | an upstream model id
                               // | "" | "x" | "default" (all legacy names → default_alias)
  "messages": [
    { "role": "system", "content": "be brief" },   // role: system | user | assistant | tool
    { "role": "user",   "content": "hello" }
  ],
  "stream": false,             // strict bool; anything else is ignored by the peek scanner
  "stream_options": { "include_usage": true },     // honoured; never injected unless
                                                   // [router] request_usage = "passthrough"
  "max_tokens": 256,
  "temperature": 0.7,
  "tools": []
}
```

Response — the upstream's body, verbatim:

```jsonc
{
  "id": "chatcmpl-…",
  "object": "chat.completion",
  "created": 1780000000,
  "model": "Carnice-9b-Q6_K",  // the UPSTREAM id, not the alias — the upstream wrote this
  "choices": [
    {
      "index": 0,
      "message": { "role": "assistant", "content": "hi" },
      "finish_reason": "stop"  // stop | length | tool_calls | content_filter | null
    }
  ],
  "usage": { "prompt_tokens": 9, "completion_tokens": 2, "total_tokens": 11 }
}
```

Headers on the way out: see §4.

### 2.2 `POST /v1/completions`

Legacy text completion. Same routing, same class-`Completion` filtering.

```jsonc
{
  "model": "local",            // same model-string grammar as /v1/chat/completions
  "prompt": "The capital of France is",
  "max_tokens": 16,
  "stream": false              // true | false
}
```

### 2.3 `POST /v1/embeddings`

Class `Embedding`: **only embedding-capable backends are candidates.** A chat-only llama-server can
never be selected here, whatever the alias says.

```jsonc
{
  "model": "embed",            // alias | upstream id; must resolve to an embedding backend
  "input": ["one string", "or an array"],
  "encoding_format": "float"   // float | base64
}
```

```jsonc
{
  "object": "list",
  "data": [ { "object": "embedding", "index": 0, "embedding": [0.01, -0.02] } ],
  "model": "nomic-embed-text-v1.5",
  "usage": { "prompt_tokens": 4, "total_tokens": 4 }
}
```

### 2.4 `POST /v1/rerank`

Routed only if the resolved target advertises rerank support; otherwise `503 no_healthy_backend`.

```jsonc
{
  "model": "rerank",
  "query": "what is a GGUF",
  "documents": ["…", "…"],
  "top_n": 3
}
```

### 2.5 `POST /v1/messages` — Anthropic ingress

Requires `anthropic-version: 2023-06-01`. `x-api-key` is accepted for auth and **never forwarded**;
neither is `anthropic-version`. `501` when `[router] anthropic_ingress = false`.

```jsonc
{
  "model": "auto",             // the same alias grammar; resolve() does not care about dialect
  "max_tokens": 1024,          // REQUIRED. Absent ⇒ 400, Anthropic-shaped
  "system": "be brief",        // string or a content-block array; hoisted to a system message
                               // when the upstream is OpenAI
  "messages": [
    {
      "role": "user",          // user | assistant  (no "system" role in this dialect)
      "content": [
        { "type": "text", "text": "hello" }   // type: text | image | tool_use | tool_result
                                              //       | thinking (⇒ UnsupportedBlock, never
                                              //       a silent drop)
      ]
    }
  ],
  "stream": false,
  "tools": []                  // translated by default ([router] anthropic_tools = true).
                               // Set that key to false and a non-empty `tools` is a 400
                               // naming it. Zero upstream hops.
}
```

`[router] anthropic_tools` defaults to **`true`** (CHARTER amendment, 2026-07-31). Claude Code sends
92 tool definitions on *every* request, so an off-by-default flag made this endpoint a `400` on
request one for the client it exists to serve. Translation is **best-effort** and says so: parallel
tool calls, some `tool_choice` variants and a `tool_result` whose content is a block array do not map
cleanly in every case. Turn it off explicitly and a body carrying `tools` is refused loudly —
`400 tool translation is off: set [router] anthropic_tools = true to enable it` — rather than
silently stripped and answered wrongly.

Response, when the upstream is `Protocol::OpenAi` and the body was translated:

```jsonc
{
  "id": "msg_…",
  "type": "message",
  "role": "assistant",
  "model": "Carnice-9b-Q6_K",
  "content": [ { "type": "text", "text": "hi" } ],
  "stop_reason": "end_turn",   // end_turn | max_tokens | stop_sequence | tool_use
                               // mapped from OpenAI finish_reason: stop→end_turn,
                               // length→max_tokens, tool_calls→tool_use
  "stop_sequence": null,
  "usage": { "input_tokens": 9, "output_tokens": 2 }   // Anthropic field names, not OpenAI's
}
```

**What is translated and what is relayed** — three rules, so nobody has to guess:

| Upstream `Protocol` | `/v1/messages` | `/v1/models` |
|---|---|---|
| `Anthropic` | **relayed**, byte-for-byte; only the credential is swapped. No translation code is on this path | re-rendered from the same table |
| `OpenAi` | **fully translated** — request body, response body, and the SSE stream in both directions (work unit R-10) | re-rendered from the same table |

Streaming translation rebuilds Anthropic's *named* SSE events from OpenAI's single repeated delta
shape: exactly one `message_start`; each content block opened and closed exactly once with indices
`0..n` and no gaps; `message_delta` carrying both `stop_reason` and the final `usage`; then exactly
one `message_stop`. If the upstream dies mid-stream, every open block is closed first — never a
truncated block, never a dangling index.

### 2.6 `POST /v1/messages/count_tokens`

Not in mk1 (`ARCHITECTURE.md` §12). `501`, Anthropic-shaped:

```jsonc
{
  "type": "error",
  "error": {
    "type": "not_found_error",
    "message": "count_tokens is not implemented in mk1; see ARCHITECTURE.md §12"
  }
}
```

### 2.7 `GET /v1/models`

Aggregated across aliases **and** every enabled backend, served from the routing table — no probe
runs. **The OpenAI list shape is the default and stays byte-exact**: ApexOS's LAN compute sweep
identifies a node by exactly this shape. Extras live under a single `apexrouter` key so strict
clients ignore them.

```jsonc
{
  "object": "list",
  "data": [
    {
      "id": "auto",
      "object": "model",
      "created": 1780000000,
      "owned_by": "apexrouter",
      "apexrouter": {
        "kind": "alias",             // alias | backend_model
        "strategy": "first_healthy", // first_healthy | round_robin | least_busy | cheapest
        "healthy": true,
        "targets": ["local-carnice", "together:meta-llama/Llama-3.3-70B-Instruct-Turbo"]
      }
    },
    {
      "id": "local-carnice/Carnice-9b-Q6_K",
      "object": "model",
      "owned_by": "local-carnice",
      "apexrouter": {
        "kind": "backend_model",
        "status": "ready",           // unknown | starting | ready | degraded | down | draining
        "ctx": 32768,
        "slots": "1/4",
        "vision": false,
        "price": null,               // null | {kind:"per_token"|"per_hour"|"free", …}
        "tok_per_s_p50": 4.1
      }
    }
  ]
}
```

With an `anthropic-version` header — and **only** with it — the same rows are re-rendered in the
Anthropic list shape. The `apexrouter` extras key is carried through untouched:

```jsonc
{
  "data": [
    {
      "type": "model",
      "id": "auto",
      "display_name": "auto",
      "created_at": "2026-05-30T00:00:00Z",
      "apexrouter": { "kind": "alias", "healthy": true }
    }
  ],
  "has_more": false,
  "first_id": "auto",
  "last_id": "local-carnice/Carnice-9b-Q6_K"
}
```

### 2.8 `GET /v1/models/{id}`

One entry — an alias or a `backend/model` pin. Same header rule as §2.7. `404 model_not_found`
otherwise.

```jsonc
{
  "id": "coder",
  "object": "model",
  "created": 1780000000,
  "owned_by": "apexrouter",
  "apexrouter": { "kind": "alias", "strategy": "least_busy", "healthy": true,
                  "targets": ["local-qwen-coder"] }
}
```

### 2.9 `GET|HEAD /health`

Always 200. **Never probes a backend** — this is the answer a supervisor polls. A superset of the
LocalRouter shape and of the house shape.

```jsonc
{
  "ok": true,
  "product": "apexrouter",
  "version": "0.1.0",
  "provider": "local",         // the ACTIVE provider label: local | together | vast-gguf | endpoint
  "uptime": 1843.21            // seconds, the process's real age from /proc/self/stat
}
```

### 2.10 `GET|HEAD /providers`

The **exact** LocalRouter JSON shape, plus additive `endpoints[]` and `routes[]`. Probes run
**concurrently** with a 3 s cap (LocalRouter's were ~8 s serial), and Together is detected from the
**full credential chain**, not just `$TOGETHER_API_KEY` — the documented inconsistency, fixed.

```jsonc
{
  "active": "local",           // local | together | vast-gguf | endpoint
  "target": "http://127.0.0.1:8101",
  "providers": {
    "vast-gguf": { "available": false, "url": null },
    "together":  { "available": true,  "url": "https://api.together.ai/v1" }
  },
  "local_instances": [
    { "name": "local-carnice", "port": 8101, "running": true }
  ],
  "endpoints": [               // ADDITIVE — the legacy client ignores unknown keys
    { "id": "local-carnice", "kind": "local_llama", "desired": "running" }
  ],
  "routes": [                  // ADDITIVE
    { "alias": "auto", "is_default": true, "healthy": true }
  ]
}
```

### 2.11 `POST /switch`

The legacy verb, retargeting `default_alias`. Same request and response shapes as LocalRouter,
extended with two additive forms. Gated by the mutation gate (§1.5), and any supplied `base_url` is
validated against `[compat] allow_switch_hosts` — unauthenticated `/switch` with an arbitrary URL
plus an injected Together key is a credential-exfiltration primitive, not merely SSRF.

```jsonc
// form 1 — together
{
  "provider": "together",      // together | vast-gguf | local | endpoint
  "base_url": "https://api.together.ai/v1",   // optional; host-allowlisted
  "model_id": "meta-llama/Llama-3.3-70B-Instruct-Turbo",
  "api_key": "…"               // FIX for silent no-op #1: now persisted as a CredentialRef
}
// form 2 — a local instance from ~/.vastai-gguf/local_instances/<name>.json
{ "provider": "local", "name": "carnice" }    // FIX #2: the instance's api_key is now copied
// form 3 — vast-gguf: the legacy "delete .active_endpoint" branch
{ "provider": "vast-gguf" }
// form 4 — ADDITIVE: point at a registered backend
{ "provider": "endpoint", "id": "local-carnice" }
// form 5 — ADDITIVE: point at a route
{ "alias": "coder" }
```

```jsonc
{ "status": "ok", "provider": "together" }   // ok is the only success value
```

Failures are the legacy status with the legacy body — and a malformed instance JSON now returns a
JSON `400`, not an HTML `500` (**fix for silent no-op #3**):

```jsonc
{ "error": "unknown local instance 'carnce'" }
```

### 2.12 `GET /slots`

Refused, always:

```jsonc
{
  "error": {
    "message": "/slots echoes prompt text and is never proxied outward",
    "type": "redacted_endpoint",
    "code": null,
    "param": null
  }
}
```

llama.cpp's `/slots` **is** read internally, for semaphore sizing and the live slot counter. It just
never leaves the process.

### 2.13 The catch-all

Anything else is an opaque passthrough to the resolved default alias's primary target, after `/v1`
normalisation. `/props`, `/metrics`, `/infill`, `/tokenize` and every future llama.cpp endpoint
therefore work through the proxy without a code change here. Method, body and response are relayed
untouched; only the header set is reconstructed (§4).

---

## 3. Streaming

- **Byte-for-byte relay into `Body::from_stream`, never re-framed.** A chunk boundary may split an
  SSE event, and every OpenAI SDK buffers on `\n\n`. There is exactly one implementation of these
  rules (`router::relay::stream`); the proxy handler holds no framing code of its own.
- `Content-Type: text/event-stream` is forced **only** when the upstream is 2xx *and* already says
  `text/event-stream`. A `400 {"error":…}` on a `stream:true` request reaches the client as JSON,
  which is what the SDK expects.
- **Never a total timeout on a stream.** `connect_timeout` (5 s) + `headers_timeout` (600 s) + an
  **inter-chunk idle timeout** (300 s).
- **Client disconnect aborts upstream.** Dropping `Committed` cancels the reqwest future, so
  llama.cpp stops generating and frees its slot within ~1 s. That is integration-tested.
- **Mid-stream upstream death has a defined client-visible behaviour.** A clean EOF with no
  `data: [DONE]` terminator counts as death too — a socket closing politely mid-generation is
  indistinguishable from a finished stream to every SDK:

```jsonc
// one synthetic frame, then the terminator, then close. Never a silent truncation.
data: {"error":{"message":"upstream ended mid-stream","type":"upstream_unavailable"}}

data: [DONE]
```

  The idle timeout emits the same pair with `"type":"upstream_timeout"`.

- **`X-Usage` is emitted on buffered responses only.** Response headers flush before the first SSE
  chunk and usage arrives in the last one, so a streaming `X-Usage` would be absent or a lie. On
  streams the response carries `X-ApexRouter-Usage-Deferred: true` and the numbers land in
  `usage.jsonl`, in the `request_finished` WS event and in the live-request table. **This is a
  stated, tested divergence from LocalRouter.**
- `[router] request_usage` defaults to **`"off"`**. Injecting `stream_options.include_usage` when
  the client did not ask changes what every streaming client receives, so opting in is a choice.
  When it is `"passthrough"` we do **not** filter the extra chunk back out — that would break the
  byte-exactness claim in the other direction.
- `X-Accel-Buffering: no`, `Cache-Control: no-cache`.

---

## 4. Headers

Outbound headers are **constructed** from an allowlist, never cloned from the inbound map, so a
client's `Authorization` cannot reach a third party — and a local `llama-server --api-key` becomes
reachable through the proxy for the first time.

| Direction | Header | Notes |
|---|---|---|
| out | `content-type`, `accept`, `user-agent`, `x-request-id` | copied when present |
| out | `accept-encoding: identity` | forced; the proxy never negotiates compression |
| out | the backend's own credential | from `CredentialSource`, resolved per request |
| out | `Via: 1.1 apexrouter` | also the loop guard's token |
| **never** out | `authorization` from the client, `x-api-key`, `anthropic-version`, `cookie` | consumed by the proxy |
| back | `X-Request-Id` | the `RequestId` on the record |
| back | `X-Provider`, `X-Usage` | preserved for LocalRouter compat; `X-Usage` is buffered-only |
| back | `X-ApexRouter-Backend` | which `BackendId` answered |
| back | `X-ApexRouter-Route` | `<alias-or-"-">|<reason>` |
| back | `X-ApexRouter-Attempts` | how many candidates were tried |
| back | `X-ApexRouter-Fallback` | `true` when the answer came from a non-first candidate |
| back | `X-ApexRouter-Protocol` | only when the ingress is not `open_ai`, e.g. `anthropic->open_ai` |
| back | `X-ApexRouter-Usage-Deferred` | `true` on streams |
| back | `X-ApexRouter-Warm` | `parked=N,waited_ms=N` — **only** on a response that parked behind a swap |

`X-ApexRouter-Warm` is how a sequential swap becomes observable rather than merely invisible. A
request that arrives while `POST /v1/routes/{alias}/swap` is mid-flight parks instead of failing
(§1.2, `ARCHITECTURE.md` §4.7); when the window closes it is re-resolved against the replacement and
answered normally, and this header is the only trace that anything happened. `parked` is the queue
depth this request saw, `waited_ms` how long it waited. **Absent on every request that did not
park** — its presence, not its value, is the signal, so a client can log "this one rode through a
swap" without parsing anything. The two refusals that end a park instead of serving it are
`warm_queue_full` and `warm_timeout`.

---

## 5. Control listener — `127.0.0.1:2739`

All under `/v1/`. Every response body is a protocol type; every mutation is `Origin`/`Host`-gated.

### 5.1 Health, snapshot, lifecycle

#### `GET /health` — public, no token

```jsonc
{ "ok": true, "product": "apexrouter", "version": "0.1.0", "uptime": 1843.21 }
```

#### `GET /v1/snapshot` → `Snapshot`

The full picture, exactly as both GUIs and `apexrouter status` render it. One call, no fan-out.

```jsonc
{
  "product": "apexrouter",
  "version": "0.1.0",
  "served_by": "daemon",       // daemon | offline (offline = read from $STATE under a shared lock)
  "as_of_unix": 1780000000,
  "stale": false,              // true when poller-derived fields could not be refreshed
  "proxy": {
    "base_url": "http://127.0.0.1:8888",     // the one string the user copies
    "control_url": "http://127.0.0.1:2739",
    "uptime_secs": 1843.21,
    "inflight": 0,
    "req_per_min": 0.0,
    "tok_per_s": 0.0,
    "default_alias": "auto",
    "table_valid": true,       // false ⇒ the PREVIOUS table is still serving
    "table_error": null
  },
  "backends": [],              // Vec<Backend>
  "routes": [],                // Vec<ModelRoute>
  "endpoints": [],             // Vec<EndpointRecord>
  "rig": null,                 // RigSnapshot | null
  "alerts": []
}
```

#### `POST /v1/reload` → `ValidationReport`

Reparse config + routes. **Keeps the old table on failure.**

```jsonc
{
  "ok": true,
  "issues": [
    {
      "field": "routes[1].targets[0].backend",
      "severity": "warning",   // info | warning | error
      "message": "backend 'vast-h100' is registered but not enabled",
      "fix": "apexrouter backend enable vast-h100"
    }
  ]
}
```

#### `POST /v1/shutdown` → `ShutdownAck` (admin scope)

```jsonc
{
  "ok": true,
  "drain_timeout_secs": 30,
  "message": "children are NOT killed: [supervisor] kill_children_on_exit = false"
}
```

Children outlive the manager by design (`ARCHITECTURE.md` §1.4). That is the surprising part, so the
ack spells it out.

#### `GET /ws` — WebSocket, `Event` stream

Subscribe to the broadcast **before** sending the snapshot; re-send a full snapshot on
`RecvError::Lagged`; `tokio::select!` also drains `socket.recv()` to notice a close.
`request_started`/`request_finished` are serialised only when at least one subscriber exists, and
`usage_tick` is coalesced to 1 Hz — a router at 50 rps must not drown its own dashboard.

```jsonc
// first frame, always
{ "type": "snapshot", /* … the Snapshot above … */ }

// then, tagged by `type`:
{ "type": "backend_changed", "backend": { /* Backend */ } }
{ "type": "backend_removed", "id": "vast-h100" }
{ "type": "route_table_changed", "routes": [], "valid": true, "error": null }
{ "type": "rig_changed", "rig": { /* RigSnapshot */ } }
{ "type": "request_started", "id": "01JB2Z…", "alias": "auto", "backend": "local-carnice" }
{ "type": "request_finished", "record": { /* RequestRecord */ } }
{ "type": "boot_progress", "backend": "vast-h100",
  "phase": { "phase": "downloading", "pct": 41.5, "mbps": 118.0 },
            // phase: reserved | provisioning | pulling | compiling | downloading
            //      | loading | healthy | failed | destroyed
  "line": "…" }
{ "type": "log_line", "source": { /* LogSource */ }, "line": "…" }
{ "type": "vast_fleet_changed", "instances": [], "credit": 7.73 }
{ "type": "usage_tick", "window": { /* UsageSummary */ } }
{ "type": "job_changed", "job": { /* JobRecord */ } }
{ "type": "check_result", "result": { /* CheckResult */ } }
{ "type": "alert", "level": "warning", "message": "…", "action": null }
            // level: info | warning | error
```

#### `GET /metrics` — Prometheus text

```
apexrouter_requests_total{alias,backend,status}
apexrouter_ttft_seconds
apexrouter_tokens_total{kind}          # kind: prompt | completion
apexrouter_tokens_per_second
apexrouter_backend_up{backend}
apexrouter_inflight{backend}
apexrouter_queue_depth
apexrouter_cost_usd_total{provider}
apexrouter_vram_free_mb{device}
```

### 5.2 Rig, discovery, fit

#### `GET /v1/rig` → `RigSnapshot`

```jsonc
{
  "gpus": [
    {
      "device": "Vulkan0",     // the EXACT -dev token; one ENUMERATION, not one piece of silicon
      "index": 0,
      "name": "AMD Radeon 840M Graphics (RADV KRACKAN1)",
      "backend": "vulkan",     // vulkan | cuda | rocm | hip | metal | sycl | cpu | other
      "vram_total_mb": 20992,
      "vram_free_mb": 20480,
      "pci_bus_id": "0000:c5:00.0",
      "driver": "radv",
      "is_software": false,    // llvmpipe ⇒ true; excluded from default selection
      "seen_by_builds": ["build-vulkan"],
      "held_by": ["local-carnice"],
      "reserved_mb": 5956
    }
  ],
  "builds": [
    {
      "id": "build-vulkan",
      "server_path": "/home/andre/llama.cpp/build-vulkan/bin/llama-server",
      "label": "Vulkan (RADV)",
      "build_info": "b9199 (39cf5d619)",
      "backends": ["vulkan"],  // from --list-devices, NEVER from grepping --help
      "devices": ["Vulkan0"],
      "flags": { "flags": ["--fit", "--jinja"], "jinja_default_on": true,
                 "fa_tristate": true, "has_fit": true, "has_router_mode": false },
      "probed_at_unix": 1780000000
    }
  ],
  "ram_total_mb": 22000, "ram_free_mb": 14000,
  "swap_total_mb": 8192,  "swap_used_mb": 512,
  "cpu_threads": 12,
  "scanned_at_unix": 1780000000
}
```

> **Never add two GPUs' VRAM together without checking `physical_key` first.** The same card is a
> different `Gpu` in every backend that can reach it: on this box the single Radeon 840M is `ROCm0`
> (11397 MiB) *and* `Vulkan0` (20992 MiB). Both readings are true; neither may be added to the
> other. And ROCm reports `free > total` (GTT accounting), so `total - free` is an underflowed
> `u64`, not a small number — `Gpu::vram_used_mb()` returns `null` rather than a lie.

#### `POST /v1/rig/rescan?builds=&models=` → `RigSnapshot`

Both query params default to `true`. Force the scan the 300 s cache would otherwise have served.

```jsonc
// the same RigSnapshot as GET /v1/rig, freshly scanned
{ "gpus": [ { "device": "Vulkan0",
              "backend": "vulkan",   // vulkan | cuda | rocm | hip | metal | sycl | cpu | other
              "is_software": false } ],
  "builds": [ { "id": "build-vulkan", "backends": ["vulkan"] } ],
  "scanned_at_unix": 1780000042 }
```

#### `GET /v1/models/local?refresh=` → `Vec<LocalModel>`

Discovered GGUFs, with `-00001-of-000NN` shards grouped into **one** logical model.

```jsonc
[
  {
    "id": "models-carnice-9b-q6-k",
    "name": "Carnice-9b-Q6_K",
    "dir": "/home/andre/models/Carnice-9b",
    "shards": [ { "path": "/home/andre/models/Carnice-9b/Carnice-9b-Q6_K.gguf",
                  "bytes": 7516192768 } ],
    "total_bytes": 7516192768,
    "mmproj": [],              // vision projectors found alongside; empty = text-only
    "quant": "Q6_K",
    "gguf": {                  // null when the header could not be read
      "arch": "qwen3", "n_layer": 41, "n_head_kv": 8,
      "n_embd_head_k": 128, "n_embd_head_v": 128, "n_ctx_train": 262144,
      "full_attn_layers": 10,  // hybrid-linear models carry KV on only some layers
      "n_expert": null
    },
    "discovered_at_unix": 1780000000
  }
]
```

#### `GET /v1/fit?model=&ctx=&parallel=&kv=&devices=&build=&split_mode=&tensor_split=&main_gpu=` → `FitPlan`

The one pure function that replaced 54 hand-solved recipe strings. `model` is required; everything
else narrows the search.

```jsonc
{
  "ctx": 32768,                // TOTAL pool, shared across `parallel` slots — NOT per-slot
  "parallel": 4,
  "kv_type": "q8_0",           // f32 | f16 | bf16 | q8_0 | q4_0 | q4_1 | iq4_nl | q5_0 | q5_1
  "ngl": { "ngl": "all" },     // ngl: auto | all | layers   (auto = emit nothing, let --fit decide)
  "split": {
    "devices": ["Vulkan0"],
    "mode": "layer",           // none | layer | row | tensor
    "main_gpu": null,
    "tensor_split": null
  },
  "weights_mb": 4861, "kv_mb": 594, "compute_mb": 501, "headroom_mb": 14036,
  "per_device_mb": [["Vulkan0", 5956]],
  "verdict": { "verdict": "fits", "headroom_mb": 14036 },
                               // verdict: fits | tight | needs_offload | wont_fit
  "why": [                     // rendered as tooltips next to every derived field
    "kv_layers = full_attn_layers (10 of 41)",
    "budget scoped to backend `vulkan` via build `build-vulkan`",
    "ROCm0 excluded: same physical card, different backend — a budget is never summed across backends"
  ]
}
```

> **The budget is per backend.** `budget_from_rig` resolves a `BackendScope` to exactly one backend
> and selects only that backend's devices; `devices` narrows within it and can never widen across
> it. Anything dropped lands in `VramBudget::notes` and is folded into `FitPlan::why`, because a
> fallback is a visible value. Reservations are attributed by `Gpu::physical_key`, so an endpoint
> holding a card through one backend is subtracted from the same card's budget under another. Four
> cards on **one** backend still sum: a genuine 4× H100 box budgets 4 × 81559 MiB.

#### `POST /v1/fit` — body `FitInput` → `FitPlan`

The pure form: you supply the budget, nothing is read from the machine.

```jsonc
{
  "weights_bytes": 7516192768,
  "gguf": { "arch": "qwen3", "n_layer": 41, "n_head_kv": 8,
            "n_embd_head_k": 128, "n_embd_head_v": 128, "n_ctx_train": 262144,
            "full_attn_layers": 10, "n_expert": null },
  "budget": {
    "devices": [ { "device": "Vulkan0", "free_mb": 20480, "reserved_mb": 0 } ],
    "margin_mb": 1024,
    "host_ram_free_mb": 14000,
    "backend": "vulkan",       // null = no device; NEVER "all of them"
    "notes": []
  },
  "want_ctx": 32768,
  "want_parallel": 4,
  "want_kv": "q8_0",
  "split": { "devices": ["Vulkan0"], "mode": "layer", "main_gpu": null, "tensor_split": null }
}
```

A caller-supplied budget mixing device tokens of two backends is **not** added up: the largest
single-backend group wins and a `WARNING` line says so in `why`. Over-optimism is the direction that
OOMs a spawn, so an ambiguous budget resolves downwards.

#### `POST /v1/fit/input` — body `FitQuery` → `FitInput`

The bridge between the two forms above: takes the same fields `GET /v1/fit` accepts as query
parameters and returns the fully-resolved `FitInput` the machine would have used — the live budget,
the parsed GGUF header, the summed shard bytes — **without** solving. This is what a GUI calls to
show "here is what we are about to solve over" before the user touches a slider.

```jsonc
{ "model": "Carnice-9b-Q6_K", "ctx": 32768, "parallel": 4, "kv": "q8_0",
  "devices": "Vulkan0", "build": "build-vulkan" }
```

### 5.3 Backends

#### `GET /v1/backends` → `Vec<Backend>`

```jsonc
[
  {
    "id": "local-carnice",
    "kind": "local_llama",     // local_llama | local_vllm | vast_llama | vast_vllm | managed | node
    "protocol": "open_ai",     // open_ai | anthropic   (defaulted; OpenAi unless stated)
    "label": "Carnice 9B (Vulkan)",
    "base_url": "http://127.0.0.1:8101",   // ALWAYS stored WITHOUT a trailing /v1
    "credential": { "kind": "file", "path": "…/state/keys/local-carnice" },
                               // kind: none | env | file | managed | instance
                               // a DESCRIPTION, never key material
    "tags": ["local", "gpu:vulkan", "tools"],
    "models": [ { "id": "Carnice-9b-Q6_K", "ctx": 32768, "vision": false } ],
    "limits": { "max_concurrent": 4, "queue_depth": 16, "ctx": 32768, "slots_total": 4 },
    "price": { "kind": "free" },   // kind: per_token | per_hour | free   (or null)
    "health": { "state": "ready", "since_unix": 1780000000,
                "slots_busy": 1, "slots_total": 4, "tps_p50": 4.1 },
                               // state: unknown | starting | ready | degraded | down | draining
    "provenance": "spawned",   // discovered | spawned | rented | manual | adopted | imported
    "endpoint": { "id": "local-carnice", "kind": "local_llama" },   // null when we own no lifecycle
    "enabled": true
  }
]
```

#### `POST /v1/backends` — body `NodeSpec` → `Backend`

Register a URL that something else is running. No lifecycle is taken over.

```jsonc
{
  "base_url": "http://192.168.1.40:8080",   // stored WITHOUT /v1
  "credential": { "kind": "env", "var": "LAN_NODE_KEY" },
  "label": "the workstation",
  "declared_models": ["Qwen3-Coder-30B"],   // empty ⇒ discovered by the prober
  "protocol": "open_ai"        // open_ai | anthropic
}
```

#### `GET /v1/backends/{id}` → `Backend` · `DELETE /v1/backends/{id}`

```jsonc
// GET → one Backend, exactly as in the list above. DELETE → 204, or:
{ "error": { "kind": "not_found",  // not_found | invalid_request | conflict | forbidden | internal
             "message": "no backend 'vast-h100'", "param": "id", "code": null } }
```

#### `PATCH /v1/backends/{id}` → `Backend`

Every field optional; absent means unchanged.

```jsonc
{
  "tags": ["local", "cheap"],
  "label": "Carnice 9B",
  "limits": { "max_concurrent": 2, "queue_depth": 8, "ctx": 16384, "slots_total": 2 },
  "enabled": true              // false takes it out of the table without forgetting it
}
```

#### `POST /v1/backends/{id}/probe|enable|disable|drain` → `Backend`

`drain` sets `accepting = false` and lets in-flight requests finish; `Health` becomes
`{"state":"draining","in_flight":N}`.

```jsonc
// the Backend with `health` updated. After /drain:
{ "id": "local-carnice", "enabled": true,
  "health": { "state": "draining",  // unknown | starting | ready | degraded | down | draining
              "in_flight": 2 } }
```

#### `GET /v1/backends/{id}/logs?tail=200&follow=1`

`follow` absent or `0` returns `text/plain`. `follow=1` returns SSE, one event per line:

```jsonc
data: {"type":"log_line","source":{"backend":"local-carnice"},"line":"slot released"}
```

### 5.4 Routes

#### `GET /v1/routes` → `Vec<ModelRoute>` · `PUT /v1/routes` (whole table, atomic)

```jsonc
[
  {
    "alias": "auto",
    "targets": [
      {
        "backend": { "sel": "id", "value": "local-carnice" },  // sel: id | tag | glob
        "model": "Carnice-9b-Q6_K",   // null ⇒ pass the alias through unchanged
        "weight": 1
      },
      { "backend": { "sel": "tag", "value": "rented" }, "model": null, "weight": 1 }
    ],
    "strategy": "first_healthy",  // first_healthy | round_robin | least_busy | cheapest
    "filter": {
      "require_tags": [], "exclude_tags": [],
      "max_cost_per_mtok": null,  // Money, integer micro-USD
      "min_ctx": null, "require_vision": false, "require_tools": false
    },
    "retry": { "attempts": 2, "failover": true, "honor_retry_after": true },
    "is_default": true,
    "description": "whatever is up"
  }
]
```

`PUT` compiles **before** it persists: a table that does not compile never reaches disk and never
displaces the one that is serving. On failure the response is a `ValidationReport` with `ok:false`
and the running table is untouched.

#### `GET|PUT|DELETE /v1/routes/{alias}`

Single-route forms of the above. `PUT` forces `alias` to the path segment, so the body cannot rename
a route by accident.

```jsonc
// one ModelRoute, both as the PUT body and as the GET response
{ "alias": "coder",
  "targets": [ { "backend": { "sel": "id",       // id | tag | glob
                              "value": "local-qwen-coder" },
                 "model": null, "weight": 1 } ],
  "strategy": "least_busy",  // first_healthy | round_robin | least_busy | cheapest
  "retry": { "attempts": 2, "failover": true, "honor_retry_after": true },
  "is_default": false }
```

#### `POST /v1/routes/validate` — body `Vec<ModelRoute>` → `ValidationReport`

Never touches the live table. This is what the editors in both GUIs call on every keystroke.

```jsonc
{
  "ok": false,
  "issues": [
    { "field": "routes[0].strategy",
      "severity": "error",   // info | warning | error — only `error` sets ok:false
      "message": "cheapest needs a price model or a tps_hint on at least one target",
      "fix": "set strategy = \"first_healthy\", or give the target a [price] block" }
  ]
}
```

#### `POST /v1/routes/{alias}/test` → `SmokeProbe`

A 20-token probe through the **resolved** route, using the resolved model id — not `smoke.sh`'s
hardcoded `"model":"x"`, which 400s on every managed provider.

```jsonc
{
  "name": "smoke.warmup",      // smoke.models | smoke.warmup | smoke.tools | smoke.throughput
  "ok": true,
  "ms": 4820,
  "detail": "20 tokens from local-carnice/Carnice-9b-Q6_K",
  "ttft_ms": 310,
  "tok_per_s": 9.71,
  "tokens": 20
}
```

#### `POST /v1/routes/{alias}/swap` → `SwapReport`

One verb; the mode is chosen for you by `fit()` unless you override it.

```jsonc
// `to` is untagged: a bare string is a backend that already exists,
// an object is an EndpointSpec to start first.
{ "to": "vast-h100", "mode": "hot" }          // mode: hot | sequential (null ⇒ fit() decides)
{ "to": { "kind": "local_llama", "build": "build-vulkan", "model_path": "/…/model.gguf",
          "alias_flag": "big", "host": "127.0.0.1", "ngl": { "ngl": "auto" },
          "split": { "devices": [], "mode": "layer", "main_gpu": null, "tensor_split": null },
          "mode": "thinking", "extra_args": [] } }
```

```jsonc
{
  "alias": "auto",
  "mode": "sequential",        // hot | sequential
  "from": "local-carnice",     // null when the alias pointed at nothing
  "to": "local-qwen-coder",
  "parked": 3,                 // requests that waited on the Notify during a sequential swap
  "drained_ms": 1840,
  "total_ms": 41220
}
```

`hot` needs both models resident simultaneously; `sequential` stops the old one first and parks
in-flight requests rather than failing them. `fit()` picks `sequential` when the two together would
not fit — which is the whole reason the verb exists.

#### `POST /v1/routes/default`

```jsonc
{ "alias": "coder" }           // must be a known alias; 400 invalid_request otherwise
```

### 5.5 Endpoints (lifecycle)

#### `GET /v1/endpoints` → `Vec<EndpointRecord>`

**There is deliberately no `status` field.** Expectation *is* state, and it is a fact:

```jsonc
[
  {
    "id": "local-carnice",
    "spec": { "kind": "local_llama", /* … LocalLlamaSpec … */ },
                               // kind: local_llama | local_vllm | vast | node | managed
    "desired": "running",      // running | stopped
    "proc": {                  // null for managed/node endpoints
      "pid": 40122,
      "start_time_ticks": 189223,   // read from /proc/<pid>/stat AFTER THE LAST ')'
      "boot_id": "5f2c…",           // start_time_ticks is not comparable across a reboot
      "exe": "/home/andre/llama.cpp/build-vulkan/bin/llama-server",
      "cmdline_sha256": "9a3f…"
    },
    "port": 8101,
    "log_path": "/…/state/logs/local-carnice.log",
    "started_at_unix": 1780000000,
    "fit": { /* FitPlan — what we planned; used for VRAM reservation accounting */ },
    "adopted": false
  }
]
```

#### `POST /v1/endpoints?no_wait=&alias=&force=` — body `EndpointSpec`

`alias` binds a route to the endpoint once it is `Ready`. `force` skips the VRAM admission refusal
**and only that one**. `no_wait=true` returns `202` + `JobRecord` instead of waiting for the health
gate.

```jsonc
{
  "kind": "local_llama",       // local_llama | local_vllm | vast | node | managed
  "build": "build-vulkan",
  "model_path": "/home/andre/models/Carnice-9b/Carnice-9b-Q6_K.gguf",  // absolute; ~ is NEVER stored
  "mmproj": null,
  "alias_flag": "Carnice-9b-Q6_K",   // the -a value: the model id this server advertises
  "host": "127.0.0.1",
  "port": null,                // null ⇒ allocate from [endpoints] port_range
  "ctx": null,                 // null ⇒ leave --ctx-size UNSET so llama.cpp --fit can work
  "parallel": 4,
  "kv_type": "q8_0",           // f32 | f16 | bf16 | q8_0 | q4_0 | q4_1 | iq4_nl | q5_0 | q5_1
  "ngl": { "ngl": "auto" },    // auto | all | layers
  "split": { "devices": ["Vulkan0"], "mode": "layer",   // none | layer | row | tensor
             "main_gpu": null, "tensor_split": null },
  "mode": "thinking",          // thinking | coding | nonthinking | raw
  "flash_attn": "auto",        // on | off | auto  (null ⇒ don't pass the flag)
  "api_key": null,             // written via --api-key-file, NEVER argv
  "extra_args": []
}
```

#### `GET /v1/endpoints/{id}` → `EndpointRecord` · `DELETE /v1/endpoints/{id}` (stop + forget)

```jsonc
// GET → one EndpointRecord. DELETE stops the child, then forgets the record → 204.
{ "id": "local-carnice",
  "desired": "stopped",      // running | stopped
  "proc": null, "adopted": false }
```

#### `POST /v1/endpoints/{id}/stop|restart|adopt` → `EndpointRecord`

`adopt` requires `/props` (or `/v1/models`) to match the spec's model path, and records
`adopted: true`. A process identified as `Foreign` is **never signalled**.

```jsonc
{ "id": "local-carnice",
  "desired": "running",      // running | stopped — expectation IS state, and is a fact
  "adopted": true,           // only /adopt sets this, and only on an identity match
  "proc": { "pid": 40122, "boot_id": "5f2c…", "start_time_ticks": 189223 } }
```

#### `GET /v1/endpoints/{id}/argv` → `ArgvPreview`

**What this endpoint *was* exec'd with** — resolved from the endpoint's own record, never from a
fresh plan.

That distinction is the whole contract. The route used to call `supervisor.plan(&spec)`, which
re-scans the rig, re-solves `fit()` against whatever VRAM is free *now*, and leases a fresh port —
a hypothetical *second* launch that diverges from the running child the moment anything moves.
Measured after a VRAM budget change, the daemon served 34 tokens where `/proc/<pid>/cmdline` had
36: `-c 4096` instead of `-c 32768` and no `-ngl` at all, i.e. it described a CPU-only launch for a
fully-offloaded child, with an empty `warnings`. It now renders from `ResolvedSpec::from_record` —
the record's draft with the record's `fit` folded back in, at the port the record was leased, using
the build the record names — so the answer is a fact about the process, not a forecast. Any
divergence between the rendered argv and the plan the record reports lands in `warnings` rather
than being invisible.

**No credential ever appears here** — a key is passed as `--api-key-file` and the preview names the
real `$STATE/endpoints/<id>.key` path the supervisor wrote, never its contents. `core::exec` takes
an argv vector and there is no `sh -c` anywhere in the codebase.

| Status | When |
|---|---|
| `404` | no endpoint with that id |
| `409` | the build the record names is no longer on this machine — rendering against a different build's `FlagSupport` would silently emit a different flag set, so it refuses and names `apexrouter rig` |
| `409` | the endpoint is not one this daemon launches (a LAN node or a managed provider has no local argv) |

```jsonc
{
  "program": "/home/andre/llama.cpp/build-vulkan/bin/llama-server",
  "args": ["-m", "/…/Carnice-9b-Q6_K.gguf", "-a", "Carnice-9b-Q6_K",
           "--host", "127.0.0.1", "--port", "8101",
           "-np", "4", "-ctk", "q8_0", "-ctv", "q8_0",
           "-dev", "Vulkan0", "-sm", "layer",
           "--props", "--metrics", "--slots",         // feature-detected, always passed
           "--api-key-file", "/…/state/keys/local-carnice"],
  "env": [["LD_LIBRARY_PATH", "/home/andre/llama.cpp/build-vulkan/bin"]],
  "cwd": "/…/state",
  "warnings": ["--jinja is default-on in b9199; the flag is omitted"]
}
```

### 5.6 Recipes and search profiles

A `Recipe` is the **saved result of a discovery session**, with provenance so staleness is
detectable. A `SearchProfile` is a **query template over the live Vast market**, not a fixed tier.
Together they replace `recipes.toml`'s 71 hand-written entries.

#### `GET|POST /v1/recipes` · `GET|PUT|DELETE /v1/recipes/{id}` → `Recipe`

```jsonc
{
  "id": "carnice-vulkan",
  "label": "Carnice 9B on the iGPU",
  "description": null,
  "kind": { "kind": "local", /* … LocalLlamaSpec … */ },
                               // kind: local | vllm | vast | node | managed
  "provenance": { /* Provenance2 — where it came from and when it was last verified */ },
  "created_at_unix": 1780000000,
  "updated_at_unix": 1780000000
}
```

#### `POST /v1/recipes/{id}/validate` → `ValidationReport`

Staleness (a model file that has moved) is a `warning`, never an `error`.

```jsonc
{
  "ok": true,
  "issues": [
    { "field": "kind.model_path",
      "severity": "warning", // info | warning | error — staleness is never an error
      "message": "the model file has moved since this recipe was saved",
      "fix": "apexrouter models rescan" }
  ]
}
```

#### `POST /v1/recipes/{id}/instantiate?alias=&no_wait=&force=` → `EndpointRecord` | `JobRecord`

```jsonc
// blocking → an EndpointRecord. ?no_wait=true → 202 and:
{ "id": "01JB2Z…", "kind": "endpoint.start",
  "state": "running",        // pending | running | succeeded | failed | cancelled
  "pct": 40.0, "message": "loading weights", "result": null, "error": null }
```

#### `POST /v1/recipes/from-endpoint/{id}?label=` → `Recipe`

"Save this running thing as a recipe."

```jsonc
{
  "id": "local-carnice",
  "label": "local-carnice",
  "kind": { "kind": "local",  // local | vllm | vast | node | managed
            "build": "build-vulkan",
            "model_path": "/home/andre/models/Carnice-9b/Carnice-9b-Q6_K.gguf" },
  "created_at_unix": 1780000000, "updated_at_unix": 1780000000
}
```

#### `GET|POST /v1/profiles` · `GET|PUT|DELETE /v1/profiles/{id}` → `SearchProfile`

```jsonc
{
  "id": "cheap-24g",
  "label": "anything with 24 GB under $0.40/h",
  "gpu_names": ["RTX 3090", "RTX 4090"],   // from GET /v1/vast/gpu-names, LIVE vocabulary
  "min_vram_gb": 24,
  "max_dph": 0.40,
  "geo": "eu",                 // any | eu | us | asia
  "min_reliability": 0.98,
  "min_inet_down_mbps": 500,
  "sort": "dph_asc"            // dph_asc | dph_desc | reliability | vram | score
}
```

### 5.7 Providers and credentials

#### `GET /v1/providers` → `Vec<ProviderStatus>`

The **source** of the credential, never the value. No probe runs.

```jsonc
[
  {
    "id": "together",
    "base_url": "https://api.together.ai/v1",   // a legacy api.together.xyz is used as-is,
                                                // never silently rewritten to .ai
    "credential": { "kind": "env", "var": "TOGETHER_API_KEY" },
                               // kind: none | env | file | managed | instance
    "credential_present": true,
    "models_cached": 84,
    "last_ok_unix": 1780000000,
    "last_error": null,
    "rate_limit": null
  }
]
```

#### `PUT /v1/providers/{id}` → `ProviderStatus`

Exactly one of the three key forms. A key the user *typed* goes to `credentials.toml` at `0600`,
never to `config.toml`; a **borrowed** credential is never copied at all.

```jsonc
{
  "base_url": "https://api.together.ai/v1",
  "api_key": "…",              // typed by a human ⇒ persisted to credentials.toml 0600
  "api_key_env": null,         // OR name an env var — a reference, nothing copied
  "api_key_file": null         // OR name a file — a reference, nothing copied
}
```

#### `POST /v1/providers/{id}/test?completion=1&model=` → `Vec<CheckResult>`

Connection probe, plus a 16-token completion when `completion=1`. `model` defaults to **the first
model the connection probe listed** — never the hardcoded `"x"` that 400s on a managed provider.

```jsonc
[
  {
    "id": "provider.connect",
    "label": "GET /v1/models",
    "status": "pass",          // pass | warn | fail | skipped
    "ms": 214,
    "detail": "84 models",
    "fix": null                // an actionable line, never prose
  }
]
```

#### `GET /v1/providers/{id}/models` → `Vec<UpstreamModel>`

The live catalogue, grouped by org.

```jsonc
[ { "id": "meta-llama/Llama-3.3-70B-Instruct-Turbo", "ctx": 131072, "vision": false } ]
```

### 5.8 Vast

> **Money.** Every path that creates, modifies or destroys an instance is gated behind
> `SpendApproval` and is unreachable from a test. `POST /v1/vast/instances` returns **409** without
> `{confirm:true, max_usd_per_hour}`, and the 409 body carries the cost preview and the current
> credit. `[providers.vast] max_usd_per_hour_ceiling` is a **hard daemon-side cap** that a
> `SpendApproval` cannot exceed.

#### `GET /v1/vast/account` → `VastAccount`

**No `api_key` field exists on this type**, even though the upstream API echoes one.

```jsonc
{ "credit": 7.73, "balance": 7.73, "can_pay": true }
```

#### `GET /v1/vast/gpu-names` → `Vec<String>`

The **live** vocabulary for the dropdown, read from the market rather than hardcoded.

```jsonc
// LIVE, read from the market. Never a hardcoded list that goes stale the week a card ships.
["A100 PCIE", "H100 NVL", "H100 SXM", "RTX 3090", "RTX 4090"]
```

#### `POST /v1/vast/offers/search` — body `OfferQuery` | `{profile}` → `OfferSearchResult`

```jsonc
{ "profile": "cheap-24g" }     // OR the full query:
{
  "gpu_names": ["RTX 4090"],
  "num_gpus": 1,
  "min_vram_gb": 24,
  "max_dph": 0.40,
  "geo": "eu",                 // any | eu | us | asia
  "min_reliability": 0.98,
  "sort": "dph_asc",           // dph_asc | dph_desc | reliability | vram | score
  "limit": 20
}
```

#### `GET /v1/vast/instances` → `Vec<VastInstance>` · `DELETE /v1/vast/instances/{id}?confirm=true`

`DELETE` requires `?confirm=true`; destroying is irreversible.

```jsonc
[
  {
    "id": 12345678,            // the Vast CONTRACT id — create returns it as `new_contract`, not `id`
    "actual_status": "running",// created | scheduling | starting | loading | pulling | running
                               // | stopped | inactive | exited | offline | unknown
                               // Anything unrecognised maps to BootPhase::Provisioning, never
                               // Failed: a status we have never seen is not evidence that a box
                               // we are paying for is dead.
    "status_msg": null,
    "ssh_host": "ssh4.vast.ai",// recycled by Vast, which is why $STATE has its own known_hosts
    "ssh_port": 12345,
    "public_ipaddr": "203.0.113.7",
    "ports": {},               // the docs say int[]; the CLI writes a Docker map. Kept RAW and
                               // read tolerantly by VastInstance::external_port().
    "direct_port_start": 40000,
    "direct_port_end": 40009,
    "gpu_name": "RTX 4090",
    "num_gpus": 1,
    "gpu_util": 0.0,
    "dph_total": 0.36,         // what we are actually being billed, per hour
    "geolocation": "Czechia, CZ",
    "label": "apexrouter-coder",
    "start_date": 1780000000.0,
    "disk_util": 22.4, "disk_space": 60.0,
    "inet_down": 940.0
  }
]
```

#### `POST /v1/vast/instances?no_wait=` → `409` | `JobRecord`

```jsonc
{
  "profile": "cheap-24g",      // OR "offer_id": 12345678
  "launch": { /* ContainerLaunch — image, env map, disk, ports */ },
  "confirm": true,             // false or absent ⇒ 409 with the cost preview
  "max_usd_per_hour": 0.40     // must be ≤ [providers.vast] max_usd_per_hour_ceiling
}
```

The 409:

```jsonc
{
  "error": {
    "kind": "approval_required",
    "message": "renting 1× RTX 4090 at $0.36/h would spend your $7.73 credit in ~21 h",
    "param": "confirm",
    "code": "spend_approval"
  }
}
```

#### `GET /v1/vast/instances/{id}/log?follow=1` — text, or SSE

`follow` absent returns `text/plain`. `follow=1` returns SSE, and the boot state machine's
transitions arrive as `boot_progress` frames alongside the raw lines:

```jsonc
data: {"type":"boot_progress","backend":"vast-h100",
       "phase":{"phase":"downloading",   // reserved | provisioning | pulling | compiling
                                         // | downloading | loading | healthy | failed | destroyed
                "pct":41.5,"mbps":118.0},
       "line":"Qwen3-Coder-30B-Q4_K_M-00001-of-00002.gguf  41%"}
```

#### `POST /v1/vast/instances/{id}/restart-download` → `DownloadHealth` — the stall recovery

Restarts a wedged model download in place rather than re-renting, then re-samples. `HOST=127.0.0.1`
is re-forced here as well as at create time, because `launch_vllm.sh`'s own default is `0.0.0.0`.
The sample is a 4-second `/proc/net/dev` delta.

```jsonc
{
  "sampled_at_unix": 1780000000,
  "rx_bytes_4s": 512,          // bytes received on eth0 over the window
  "mbps": 0.001,
  "verdict": "stalled"         // active | slow | stalled
                               // < 1000 bytes over the window is stalled; < 50 Mbps is slow
}
```

#### `POST /v1/vast/instances/{id}/tunnel` → `TunnelStatus` · `DELETE …/tunnel` → `Vec<TunnelStatus>`

`DELETE` returns the remaining tunnels rather than `204`, so a UI needs no second call to refresh
its list.

```jsonc
{
  "instance_id": 12345678,
  "local_port": 8801,
  "pid": 40988,
  "state": "up",               // starting | up | down | adopted
  "since_unix": 1780000000,
  "last_error": null
}
```

Default posture is **tunnel-only**: `HOST=127.0.0.1` is forced at create time *and* on every
stall-restart. `expose_public = true` is an explicit opt-in and **requires** a freshly minted
per-instance `llama-server` API key, because a Vast direct port is plaintext HTTP on a shared
public IP.

#### `GET /v1/vast/instances/{id}/diagnose` → `Vec<CheckResult>`

The four SSH probes plus an RX sample.

```jsonc
[
  { "id": "vast.ssh.connect", "label": "ssh -o BatchMode=yes", "status": "pass",
                               // pass | warn | fail | skipped
    "ms": 812, "detail": "root@ssh4.vast.ai:12345", "fix": null },
  { "id": "vast.rx", "label": "download throughput sample", "status": "warn",
    "ms": 3010, "detail": "1.2 MB/s over 3 s",
    "fix": "POST /v1/vast/instances/12345678/restart-download" }
]
```

#### `GET /v1/tunnels` → `Vec<TunnelStatus>`

```jsonc
[ { "instance_id": 12345678, "local_port": 8801, "pid": 40988,
    "state": "up",             // starting | up | down | adopted
    "since_unix": 1780000000, "last_error": null } ]
```

#### `GET /v1/approvals` → `Vec<ApprovalRequest>` · `POST /v1/approvals/{id}/grant|deny` → `ApprovalRequest`

```jsonc
[
  {
    "id": "01JB2Z…",           // the JobId the approval unblocks
    "what": "rent 1× RTX 4090 @ $0.36/h for ~4 h",
    "max_usd_per_hour": 0.40,  // the requested ceiling; never above max_usd_per_hour_ceiling
    "est_total_usd": 1.44,     // the BILL, not just the rate — a human sees both
    "credit": 7.73,            // null when we could not read it
    "requested_at_unix": 1780000000,
    "source": "mcp"            // cli | mcp | web_ui | slint
  }
]
```

Only populated when `[providers.vast] require_human_confirm = true`, which makes an MCP-initiated
rental need a human.

### 5.9 Hugging Face

#### `GET /v1/hf/search?q=&limit=` → `Vec<HfModel>`

An empty `q` is allowed: HF returns its most-downloaded GGUF repos.

```jsonc
[
  {
    "id": "unsloth/Qwen3-Coder-30B-A3B-Instruct-GGUF",
    "author": "unsloth",
    "downloads": 184223,
    "likes": 412,
    "gated": false,            // true ⇒ the UI must show the request-access URL, never "not found"
    "last_modified": "2026-05-02T11:04:00.000Z",
    "tags": ["gguf", "text-generation"]
  }
]
```

#### `GET /v1/hf/models/{repo}/files` → `Vec<HfFileGroup>`

The authoritative per-file sizes from `paths-info`, grouped by quant — the same grouping the UI
shows, so what downloads is what was clicked. `{repo}` contains a `/`, so the route is registered as
an axum wildcard.

```jsonc
[
  {
    "label": "Q4_K_M · 2 shards · 30.0 GiB",
    "quant": "Q4_K_M",         // regex (UD-Q\d+[^.\s_-]*|Q\d+_K_[A-Z]+|Q\d+_\d+); null if undetected
    "total_bytes": 32212254720,// the number the fit solver and the disk check use
    "files": [
      {
        "rfilename": "Qwen3-Coder-30B-Q4_K_M-00001-of-00002.gguf",
        "size": 16106127360,   // from paths-info, not from the repo listing
        "quant": "Q4_K_M",
        "is_mmproj": false,    // matched as a filename TOKEN, so a dir named `vocab-x` hides nothing
        "shard_of": [1, 2]     // parsed from -00001-of-000NN; null when unsharded
      }
    ],
    "mmproj": []               // vision projectors that pair with this group
  }
]
```

#### `POST /v1/hf/downloads?no_wait=` → `JobRecord`

Either name the `files` explicitly, or name a `quant` and let the grouped listing pick the shards.

```jsonc
{
  "repo": "unsloth/Qwen3-Coder-30B-A3B-Instruct-GGUF",
  "files": [],                 // exact repo-relative paths; wins over `quant` when both are given
  "quant": "Q4_K_M",
  "dest": null                 // null ⇒ [hf] download_dir
}
```

#### `GET /v1/hf/downloads` → `Vec<JobRecord>` · `DELETE /v1/hf/downloads/{job}` → `JobRecord`

```jsonc
[
  { "id": "01JB2Z…", "kind": "hf.download",
    "state": "running",      // pending | running | succeeded | failed | cancelled
    "pct": 41.5, "message": "shard 2 of 5", "error": null }
]
```

### 5.10 Observability

#### `GET /v1/requests?limit=&alias=&backend=&since=` → `Vec<RequestRecord>`

`limit` defaults to 100 and is capped at the ring. `since` accepts `all`, a duration (`30m`, `24h`,
`7d`, `4w`) or an absolute timestamp.

```jsonc
[
  {
    "id": "01JB2ZQK8H0000000000000000",
    "started_unix": 1780000000,
    "alias": "auto",
    "backend": "local-carnice",
    "upstream_model": "Carnice-9b-Q6_K",
    "route_reason": "alias",   // alias | explicit_pin | upstream_id_match | implicit_multi
                               // | default_fallback | legacy_model_name
    "ingress": "open_ai",      // open_ai | anthropic — which dialect the CLIENT spoke
    "method": "POST", "path": "/v1/chat/completions",
    "status": 200, "attempts": 1, "streamed": true, "aborted": false,
    "ttft_ms": 310, "total_ms": 4820,
    "prompt_tokens":     { "kind": "reported", "n": 9 },   // kind: reported | estimated
    "completion_tokens": { "kind": "reported", "n": 47 },
    "cached_tokens": 0,        // llama.cpp timings.cache_n
    "tok_per_s": 9.71,         // timings.predicted_per_second
    "cost": { "kind": "unknown" },
             // kind: metered | approximate | unknown
             // metered:     {usd, source}
             // approximate: {usd, source, assumption}  ← the assumption travels WITH the number
             // source: provider_api | vast_offer | config_table | recipe_field | derived
    "error": null
  }
]
```

#### `GET /v1/requests/{id}` → `RequestRecord` · `POST /v1/requests/{id}/cancel` → `RequestRecord`

```jsonc
// after a cancel, the same record with `aborted` set — the InFlightGuard's Drop wrote it
{ "id": "01JB2Z…", "status": 499, "aborted": true,
  "route_reason": "alias",   // alias | explicit_pin | upstream_id_match | implicit_multi
                             // | default_fallback | legacy_model_name
  "ingress": "open_ai" }     // open_ai | anthropic
```

#### `GET /v1/usage?since=24h&by=provider|model|backend|alias|day` → `UsageSummary`

`by` defaults to `provider`, which is what the legacy `cost.py` printed.

```jsonc
{
  "window": "24h",
  "by": [
    {
      "key": "vast-gguf",      // the legacy provider name stays on the wire, exactly
      "cost": { "kind": "approximate", "usd": 410000, "source": "vast_offer",
                "assumption": "100 tok/s sustained" },
      "prompt_tokens": 18422, "completion_tokens": 9110, "requests": 41,
      "tok_per_s_p50": 38.2
    }
  ],
  "total_cost": { "kind": "approximate", "usd": 410000, "source": "derived",
                  "assumption": "sum of per-bucket approximations" },
  "total_prompt": 18422, "total_completion": 9110, "rows": 41
}
```

`Money` is **integer micro-USD** — `410000` is $0.41. No float dust, ever.

> `[compat] mirror_usage_log` defaults to **false**. Rows go to `$STATE/usage.jsonl`; appending
> them to `~/.vastai-gguf/usage.log` — another tool's state file — is opt-in.

#### `POST /v1/compare?no_wait=` → `Vec<CompareRow>` | `JobRecord`

The same prompt against N aliases at the same time. Blocking by default, because the answer *is* the
comparison.

```jsonc
{ "aliases": ["local", "big"], "prompt": "explain a GGUF in one sentence", "max_tokens": 128 }
```

```jsonc
[
  {
    "alias": "local", "backend": "local-carnice", "model": "Carnice-9b-Q6_K",
    "ok": true, "ms": 12400, "ttft_ms": 310, "tok_per_s": 9.71,
    "prompt_tokens": { "kind": "reported", "n": 14 },   // the REAL number, never word_count × 1.3
    "text": "…"
  }
]
```

#### `POST /v1/smoke` — SSE, one event per probe

Exactly one of `alias` and `base_url` is required. An alias is resolved through the **live** table,
which is the point: the probe then uses the resolved route's model id.

```jsonc
{ "alias": "auto", "base_url": null, "model": null }   // `base_url` is used WITHOUT a trailing /v1
```

```jsonc
data: {"type":"check_result","result":{"id":"smoke.models","label":"GET /v1/models",
       "status":"pass","ms":12,"detail":"3 models","fix":null}}
```

#### `GET /v1/diagnose?only=` — SSE, one event per check

`only` is a comma-separated list of check ids; absent runs them all. A check that panics yields
`fail` and never poisons the run.

```jsonc
data: {"type":"check_result","result":{
       "id":"proxy.v1_normalisation","label":"clients sending a doubled prefix",
       "status":"warn",      // pass | warn | fail | skipped
       "ms":3,"detail":"2 user-agents sent /v1/v1/…",
       "fix":"point the client at http://127.0.0.1:8888 — both forms work, but one is a typo"}}
```

#### `GET /v1/checks` → the registry

```jsonc
["rig.builds", "rig.devices", "proxy.listening", "proxy.v1_normalisation",
 "provider.together", "provider.vast", "state.permissions", "compat.legacy_state"]
```

#### `GET /v1/jobs` → `Vec<JobRecord>` · `GET /v1/jobs/{id}` · `POST /v1/jobs/{id}/cancel`

```jsonc
[
  { "id": "01JB2Z…", "kind": "vast.rent",
    "state": "failed",       // pending | running | succeeded | failed | cancelled
                             // a JoinError from a panicking task also lands here, never `pending`
    "error": "offer 12345678 was taken", "finished_unix": 1780000042 }
]
```

#### `POST /v1/migrate` → `MigrationPlan` | `MigrationReport`

```jsonc
{ "dry_run": true }            // true ⇒ MigrationPlan and NOTHING is written
```

```jsonc
// dry_run: true
{
  "items": [
    { "what": "recipe",        // recipe | usage row | known_fork | provider | instance | docker map
      "from": "~/.vastai-gguf/recipes.toml [vast_gguf.tier3]",
      "action": "skip",        // import | skip | warn
      "detail": "hand-solved -ngl 62; superseded by fit() — kept as a search profile instead" }
  ],
  "source_paths": ["~/.vastai-gguf"]
}
// dry_run: false
{ "imported": 61, "skipped": 54, "warnings": ["3 usage rows had a local-time 'Z' timestamp"] }
```

`--apply` imports providers as credential **references** — the real Together key is *not* copied.

---

## 6. Status codes

| Code | When |
|---|---|
| 200 | fine |
| 202 | `?no_wait=true` accepted; a `JobRecord` is the body |
| 204 | `DELETE` succeeded |
| 400 | `invalid_request` — a malformed body or an impossible parameter |
| 401 | no token where one was required |
| 403 | scope insufficient, mutation gate refused, or `redacted_endpoint` |
| 404 | `not_found` / `model_not_found` |
| 409 | `conflict` — `port_in_use`, an approval required, a spend gate |
| 413 | over `max_body_bytes` |
| 429 | relayed from upstream, with `Retry-After` |
| 500 | `internal` — a bug; the message names the operation |
| 501 | `OpenAi → Anthropic`, or `/v1/messages` with `anthropic_ingress = false` |
| 502 | `upstream_unavailable` — it answered and it was wrong |
| 503 | `no_healthy_backend` / `server_overloaded` / `provider_not_configured` / `starting` |
| 504 | `upstream_timeout` |
| 508 | `loop_detected` |

---

## 7. Curl, end to end

```bash
# the drop-in surface — both base URLs work
curl -s http://127.0.0.1:8888/v1/models | jq '.data[].id'
curl -s http://127.0.0.1:8888/v1/v1/models | jq '.data[].id'   # identical: /v1 collapses

curl -sN http://127.0.0.1:8888/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"auto","messages":[{"role":"user","content":"hi"}],"stream":true}'

# the Anthropic ingress
curl -s http://127.0.0.1:8888/v1/messages \
  -H 'content-type: application/json' -H 'anthropic-version: 2023-06-01' \
  -d '{"model":"auto","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}'

# the control plane
curl -s http://127.0.0.1:2739/v1/snapshot | jq '.proxy'
curl -s 'http://127.0.0.1:2739/v1/fit?model=Carnice-9b-Q6_K&ctx=32768&kv=q8_0' | jq '.verdict'
curl -s -X POST http://127.0.0.1:2739/v1/routes/default \
  -H 'content-type: application/json' -d '{"alias":"coder"}'
```

For a non-loopback deployment add `-H "Authorization: Bearer $APEXROUTER_TOKEN"`.

---

## 8. What is deliberately absent

- `OpenAi → Anthropic` translation. Permanently out of scope; nothing in the ecosystem needs it.
- `POST /v1/messages/count_tokens`. `501` in mk1.
- `GET /slots` through the proxy. It echoes prompts.
- Any endpoint that returns key material. `credential` is always a *description*.
- A `CorsLayer` on the authenticated API.

---

## 9. See also

- `docs/ROUTING.md` — how a `"model"` string becomes an upstream request.
- `openapi/apexrouter-v1.yaml` — the machine-readable contract.
- `ARCHITECTURE.md` §6 — the normative route tables.

---

## 10. Keeping this file honest

`openapi/apexrouter-v1.yaml` and the axum route table are diffed by
`crates/apexrouter-server/tests/openapi_routes.rs`, which runs in CI as part of
`cargo test --workspace`. It enforces three things:

1. **Every route axum registers is in the OpenAPI file.** Adding a handler without documenting it
   fails the build.
2. **Every documented path is either registered or on an explicit `PENDING` list** naming the work
   unit that will wire it. Nothing can be documented into existence quietly.
3. **A `PENDING` entry that has since been wired fails too**, so the list cannot rot.
