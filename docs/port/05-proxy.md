# 05 — The Proxy (OpenAI-compatible local endpoint)

Port spec for ApexRouter-RS. Source of truth read in full:

- `/home/andre/Projects/Inference/tools/LocalRouter/endpoint_proxy.py` (417 lines) — the server
- `/home/andre/Projects/Inference/tools/LocalRouter/localrouter/proxy.py` (169 lines) — lifecycle/TUI wrapper

Supporting files consulted: `localrouter/config.py`, `localrouter/cost.py`,
`localrouter/menus/tool_menus.py`, `docs/SKILL.md`, `README.md`,
`~/.vastai-gguf/{config.toml,usage.log,local_instances/*.json,.pinned_provider}`.

> A near-identical older copy lives at `/home/andre/Projects/qwen36-vast/endpoint_proxy.py`.
> The LocalRouter copy is newer: it adds `authorization` to the request hop-by-hop
> drop set, adds `_DROP_RESPONSE_HEADERS`/`_relay_headers`, and makes the
> `config.toml` parse section-aware. **Port the LocalRouter copy, not the qwen36-vast one.**

---

## 1. Process model, bind address, lifecycle

| Property | Value | Source |
|---|---|---|
| Bind host | `127.0.0.1` — loopback only, hardcoded | `web.run_app(app, host="127.0.0.1", ...)` L412 |
| Port | `PROXY_PORT` env var, default `8888` | L34 |
| Entrypoint | `python3 endpoint_proxy.py` (cwd = LocalRouter root) | L415-416 |
| PID file | `/tmp/vastai-gguf-proxy.pid` | L33; written in `run()` L395 **and** by `_proxy_up()` L70 |
| Log file | `/tmp/vastai-gguf-proxy.log`, **truncated (`"w"`) on every start** | `proxy.py` L61 |
| Runtime | single process, single asyncio event loop, aiohttp `web.run_app` | |
| Startup side effect | `create_app()` → `ensure_usage_dir()` → `mkdir -p ~/.vastai-gguf` | L371-376 |

Startup banner printed to stdout (→ the log file):

```
[proxy] Starting on http://localhost:8888/
[proxy] Forwarding to: {provider} → {base_url}
[proxy] Management: /health, /providers, /switch
[proxy] PID: {pid} (saved to /tmp/vastai-gguf-proxy.pid)
```

`web.run_app(..., print=None)` suppresses aiohttp's own banner.

**Shutdown**: `run()` installs SIGINT/SIGTERM handlers that unlink the PID file and
`sys.exit(0)`. **No in-flight drain guarantee** — the signal handler calls `sys.exit`
directly rather than letting aiohttp's graceful-shutdown path run to completion.

**Lifecycle from the TUI** (`localrouter/proxy.py`):

- `_proxy_up()`: if the PID file exists and `os.kill(pid, 0)` succeeds → refuse ("already
  running"). If the PID is stale (`ProcessLookupError`/`ValueError`) → unlink and continue.
  Then `subprocess.Popen([sys.executable, ROOT/"endpoint_proxy.py"], stdout=log, stderr=STDOUT, cwd=ROOT)`,
  write `proc.pid`, print. **Does not wait for the port to be listening** — "Waits for
  target to become available…" is a lie; nothing polls. If port 8888 is already held by a
  foreign process, aiohttp dies with `EADDRINUSE` into the log, but the PID file was already
  written → the TUI shows "running" for a dead process until the next `os.kill` probe.
- `_proxy_down(pid_file)`: `SIGTERM` + unlink. Handles `ProcessLookupError` (already exited)
  but **not** `ValueError` from a corrupt PID file (unhandled traceback), and not `PermissionError`.
- `tail_proxy_logs()`: naive `console.clear()` + re-read whole file + last 2000 chars, 1 s loop,
  Ctrl-C to exit. Not a real tail (no `seek`), O(filesize) per second.
- `proxy_status_detail()`: shells out to `curl` (not aiohttp) to probe
  `http://127.0.0.1:8800/v1/models` (`--max-time 3`) and `https://api.together.ai/v1/models`
  (`--max-time 5`, Bearer from `load_provider_config()` or `TOGETHER_API_KEY`). Renders a
  2-row rich table. **`returncode == 0` is the success test — curl without `-f` returns 0 on
  HTTP 401/404/500**, so "available" only really means "TCP+TLS connected".

---

## 2. Routes served

Registered in `create_app()` (L375-387), **in this order**:

```python
app.router.add_get ("/health",    health_check)     # + implicit HEAD
app.router.add_get ("/providers", list_providers)   # + implicit HEAD
app.router.add_post("/switch",    switch_provider)
app.router.add_route("*", "/{tail:.*}", forward_request)   # catch-all, ALL methods
```

### Routing subtlety that must be preserved

aiohttp's `UrlDispatcher` accumulates "path matched but method didn't" and keeps scanning.
Because the catch-all resource accepts method `*`, **only these five (path, method) pairs are
intercepted**:

- `GET /health`, `HEAD /health`
- `GET /providers`, `HEAD /providers`
- `POST /switch`

Everything else is proxied, including `POST /health`, `DELETE /providers`, `GET /switch`,
`/v1/health`, `/v1/switch`, `/`, `/metrics`, `/slots`, `/props`, …

Consequence: **llama-server's own `/health` is shadowed** at the top level (but reachable as
`POST /health`, which llama-server will 405). Keep the shadowing — the TUI and `docs/SKILL.md`
both document `curl http://localhost:8888/health`.

### `GET /health`

```json
{"ok": true, "provider": "vast-gguf", "uptime": 1234.56}
```

- `provider` from `resolve_target()`; `uptime` = `time.time() - START_TIME` (float seconds,
  `START_TIME` captured at import, L67).
- **Always 200, never probes the backend.** It is a liveness check for the proxy only, not a
  readiness check for the route. No CORS header, no `X-Provider` header.

### `GET /providers`

```json
{
  "active": "vast-gguf",
  "target": "http://127.0.0.1:8800/v1",
  "providers": {
    "vast-gguf": {"available": false, "url": "http://127.0.0.1:8800/v1"},
    "together":  {"available": false, "url": "https://api.together.ai/v1"}
  },
  "local_instances": [{"name": "local-qwen35-9b", "port": 8100, "running": false}]
}
```

Probing behaviour:

- vast-gguf: `GET http://127.0.0.1:8800/v1/models`, `ClientTimeout(total=3)`, `available = (status == 200)`.
- together: only probed **if `TOGETHER_API_KEY` is in the environment** — it does **not**
  fall back to `~/.vastai-gguf/config.toml` here (unlike `resolve_target()` and unlike
  `proxy_status_detail()`). So with a key only in `config.toml`, `/providers` reports
  `together.available = false` while requests to Together work fine. **Inconsistency to fix.**
  `GET https://api.together.ai/v1/models` with `Authorization: Bearer …`, `total=5`.
- local instances: glob `~/.vastai-gguf/local_instances/*.json`; for each, `name`/`port` from
  the JSON, `running` = `os.kill(int(<name>.pid), 0)` succeeds. Per-file exceptions swallowed.
- Every probe is wrapped in a bare `except Exception: pass`. Blocking latency of this endpoint
  is up to ~8 s serial (3 s + 5 s, **not** run concurrently).
- No CORS header.

### `POST /switch`

Request body must be JSON, else `400 {"error": "Invalid JSON body"}`.

| `provider` | Extra fields | Effect | Response |
|---|---|---|---|
| `"together"` | `api_key`? `base_url`? (default `https://api.together.ai/v1`) `model_id`? (default `meta-llama/Llama-3.1-8B-Instruct-Turbo`) | writes `.active_endpoint` | `200 {"status":"ok","provider":"together"}` |
| `"vast-gguf"` | — | **deletes** `.active_endpoint` | `200 {"status":"ok","provider":"vast-gguf"}` |
| `"local"` | `name` (**required**) | reads `~/.vastai-gguf/local_instances/{name}.json`, writes `.active_endpoint` | `200 {"status":"ok","provider":"local"}` |
| anything else | — | — | `400 {"error":"Unknown provider: X"}` |

`local` errors: missing `name` → `400 {"error":"Missing 'name' for local provider"}`;
missing meta file → `404 {"error":"Local instance 'X' not found"}`.

`.active_endpoint` written by `/switch`:

```jsonc
// together
{"provider":"together","model_id":"…","base_url":"https://api.together.ai/v1",
 "endpoint":"https://api.together.ai/v1/chat/completions","switched_at":"2026-07-30T12:00:00Z"}
// local
{"provider":"local","name":"local-qwen35-9b","host":"127.0.0.1","port":8100,
 "model_path":"~/models/…gguf","switched_at":"2026-07-30T12:00:00Z"}
```

`switched_at` uses `time.gmtime()` → genuinely UTC (unlike `cost.py`, see §9).

**Bugs in `/switch` to fix in the port:**

1. `api_key` accepted in the together body is **never persisted** — `resolve_target()` only
   reads the key from env or `config.toml`. Passing it is a silent no-op.
2. `local` does **not** copy `api_key` from the instance meta into `.active_endpoint`, yet
   `resolve_target()` reads `api_key` from `.active_endpoint` for local. So a local backend
   started with `--api-key` can never be authenticated through `/switch`.
3. `json.loads(meta_file.read_text())` (L290) is unguarded → a malformed instance JSON yields
   an aiohttp 500 HTML traceback page, not a JSON error.
4. `write_text` is **not atomic** (truncate-then-write). See §7 race.
5. **No authentication.** See §11.

---

## 3. `resolve_target()` — the whole routing decision

```python
def resolve_target() -> (base_url: str, auth_header: str|None, provider: str)
```

Reads `<dir of endpoint_proxy.py>/.active_endpoint` — i.e. **inside the LocalRouter repo
checkout**, *not* under `~/.vastai-gguf`. (Currently absent on this machine → the fallback
branch is live.)

```
file missing / unreadable / JSON parse error / provider not in {together, local}
    → ("http://127.0.0.1:8800/v1", None, "vast-gguf")     # LOCAL_TUNNEL_PORT = 8800
provider == "together"
    → (data.base_url or "https://api.together.ai/v1",
       "Bearer " + key  (or None if no key),
       "together")
provider == "local"
    → (f"http://{data.host or '127.0.0.1'}:{data.port or 8100}/v1",
       "Bearer " + data.api_key  (or None if empty/absent),
       "local")
```

Parse errors print `[proxy] Failed to parse active endpoint: {e}` to **stderr** and then fall
through to the vast-gguf fallback — a corrupted file silently reroutes all traffic to
`127.0.0.1:8800`.

**Together key resolution order** (L83-99):

1. `os.environ["TOGETHER_API_KEY"]`
2. `~/.vastai-gguf/config.toml`, hand-rolled line scanner (not `tomllib`):
   - tracks the current `[section]` via `s.startswith("[") and s.endswith("]")` → `s[1:-1].strip()`
   - only accepts `api_key` lines while `section == "providers.together"`
   - value = `line.split("=", 1)[1].strip().strip('"')`; rejected if it starts with `#`
   - takes the **first** match then `break`s
   - Does not handle: single-quoted values, trailing inline comments (`api_key = "x"  # note`
     yields `x"  # note` after `strip('"')` only strips the *outer* quotes… in practice it
     yields `x"  # note` → broken), multi-line strings, `[providers."together"]`.
3. No key → `auth = None` → requests go to Together unauthenticated → 401.

Live config on this machine (`~/.vastai-gguf/config.toml`):

```toml
[providers.together]
base_url  = "https://api.together.ai/v1"
api_key   = "tgp_v1_…"
```

**Called once per request**, at the top of `forward_request` (L125), plus in `health_check`
(L244) and `list_providers` (L308). No caching, no mtime check, no inotify — a `stat()` +
`read()` + `json.loads()` (+ possibly a `config.toml` read) **on every single proxied request**.

**Not read by the proxy**: `~/.vastai-gguf/.pinned_provider` (that is TUI-only, consumed by
`localrouter/menus/vast_menus.py`). Also unused by the proxy from `.active_endpoint`:
`model_id`, `endpoint`, `switched_at`, `name`, `model_path`.

---

## 4. Request path: URL construction, headers, body

### 4.1 Target URL — ⚠️ the `/v1` doubling bug

```python
path       = request.rel_url.path            # e.g. "/v1/chat/completions"
query      = request.rel_url.query_string
target_url = f"{base_url.rstrip('/')}{path}" # base_url already ends in "/v1"
if query: target_url += f"?{query}"
```

`base_url` **always ends in `/v1`** for all three providers. The client path is appended
whole, with no prefix stripping. Therefore:

| Client base URL | Client sends | Upstream URL | Works? |
|---|---|---|---|
| `http://localhost:8888` | `/chat/completions` | `http://127.0.0.1:8800/v1/chat/completions` | ✅ |
| `http://localhost:8888/v1` | `/v1/chat/completions` | `http://127.0.0.1:8800/v1/v1/chat/completions` | ❌ 404 |

The repo contradicts itself: the module docstring says "Listen on: `http://localhost:8888/v1/...`"
and `docs/SKILL.md` L131/L135 instruct `OPENAI_BASE_URL=http://localhost:8888/v1` — both of
which hit the broken path. `README.md` L136 and `tool_menus.py` L502 use bare
`http://127.0.0.1:8888` — the working path.

**Port requirement: ApexRouter-RS must accept BOTH.** Normalise by stripping a leading `/v1`
from the incoming path when the resolved `base_url` already ends in `/v1` (or, cleaner: keep
the upstream base *without* `/v1`, and canonicalise the incoming path to exactly one `/v1`
prefix). This is the single highest-risk drop-in incompatibility.

### 4.2 Request headers

`_clean_headers()` drops these, case-insensitively (`_HOP_BY_HOP`, L47-51):

```
host, transfer-encoding, connection, keep-alive, upgrade, te, trailer,
x-proxy-forwarded, x-provider, authorization
```

Everything else is forwarded verbatim — including `content-length`, `accept-encoding`,
`content-type`, `user-agent`, `accept`, `cookie`, and all other `x-*` headers.

- **`authorization` is deliberately stripped** so the client's own key never leaks to a rented
  Vast box (see the comment at L44-46). The proxy then re-injects `Authorization: <auth_header>`
  **only if** `resolve_target()` produced one. For `vast-gguf` (always) and for `local` without
  an `api_key`, the upstream request carries **no** `Authorization` at all → a llama-server
  started with `--api-key` will 401 and there is no way to fix it short of editing
  `.active_endpoint` by hand.
- `content-length` is forwarded *and* aiohttp also derives one from `data=body`. They agree
  because `body` is the exact bytes read, but it is fragile; the Rust port should let the HTTP
  client own framing headers.
- Nothing is added: no `X-Forwarded-For`, no `X-Request-Id`, no `Via`, no loop guard.
  `x-proxy-forwarded` is in the drop set but **nothing ever sets it** — dead intent.

### 4.3 Body and method dispatch

```python
body = await request.read() if method in ("POST","PUT","PATCH") else None
```

- `GET` → `session.get(url, headers)` (no body)
- `DELETE` → `session.delete(url, headers)` (no body)
- everything else (`POST/PUT/PATCH/HEAD/OPTIONS/…`) → `session.request(method, url, headers, data=body)`
  (this was "FIX M3": previously PUT/PATCH were sent as POST)
- **A body on `GET`/`DELETE` is silently dropped.**
- ⚠️ **1 MiB request-body cap.** `web.Application()` defaults to `client_max_size = 1024**2`, and
  `await request.read()` enforces it → `413 Request Entity Too Large` (aiohttp HTML page, not
  JSON) for larger bodies. Reachable with long chat histories or base64 image parts. The Rust
  port should have no such cap by default, or make it configurable and return an OpenAI-shaped
  413.

### 4.4 Streaming detection

```python
use_streaming = False
if body and method == "POST":
    try: use_streaming = json.loads(body).get("stream", False)
    except (json.JSONDecodeError, Exception): pass
```

- Only `POST` with a body is ever considered for streaming.
- **Truthiness, not type check**: `"stream": "false"` (a string) is truthy → wrongly streams.
- Parsing the whole JSON body on every POST just to read one boolean.
- `stream_options` / `include_usage` is not inspected.

---

## 5. Response path (non-streaming) — `build_response()`

```python
content = await resp.read()                 # ENTIRE body buffered in memory
headers = _relay_headers(resp.headers)      # drop content-encoding/-length,
                                            #      transfer-encoding, connection, keep-alive
headers["X-Provider"] = provider
headers["Access-Control-Allow-Origin"] = "*"
# best-effort: parse JSON, if data["usage"] exists →
headers["X-Usage"] = f"{usage.prompt_tokens}+{usage.completion_tokens}"   # e.g. "131072+500"
return web.Response(status=resp.status, body=content, headers=headers)
```

- **Status code is passed through verbatim** (including 4xx/5xx from the backend).
- Response body bytes are passed through verbatim — no rewriting of `model`, `id`, etc.
- `Content-Encoding`/`Content-Length` are dropped because aiohttp transparently decompresses
  the upstream body; aiohttp then recomputes `Content-Length` from `body`. **The Rust port must
  do the same** (or disable automatic decompression and relay bytes+encoding untouched — that
  is strictly better and avoids a decompress/recompress round trip).
- `dict(resp.headers)` collapses a `CIMultiDict`: **duplicate response headers are lost**
  (only the last `Set-Cookie` survives). Rust port should relay multi-valued headers properly.
- Whole-body buffering means a large non-stream completion is fully materialised twice
  (aiohttp buffer + `web.Response` body). Rust port should stream both directions.
- `X-Usage` is **not** emitted for streaming responses.

---

## 6. Streaming (SSE) — `build_streaming_response()`

```python
headers = _relay_headers(resp.headers)
headers["X-Provider"]  = provider
headers["Access-Control-Allow-Origin"] = "*"
headers["Content-Type"]  = "text/event-stream"    # FORCED
headers["Cache-Control"] = "no-cache"
sresp = web.StreamResponse(status=resp.status, headers=headers)
await sresp.prepare(request)
try:
    async for chunk in resp.content.iter_chunked(4096):
        await sresp.write(chunk)
    await sresp.write_eof()
except (ConnectionResetError, asyncio.CancelledError):
    pass
return sresp
```

Contract details:

- Raw byte relay, **no SSE frame parsing** — chunk boundaries may split an `data: …\n\n` event.
  Correct for any client that buffers on `\n\n`, which all OpenAI SDKs do. The Rust port should
  likewise relay bytes, not re-frame.
- `iter_chunked(4096)` is *not* a fixed-size buffer: aiohttp's `StreamReader.read(n)` returns as
  soon as ≥1 byte is available, capped at `n`. So there is no artificial TTFT delay. The Rust
  port must preserve this (forward whatever arrives; never wait to fill a buffer).
- No `Content-Length` (dropped) → aiohttp emits `Transfer-Encoding: chunked`.
- **`Content-Type: text/event-stream` is forced even on errors.** If the client sent
  `stream: true` and the backend replies `400 {"error": …}` as `application/json`, the client
  receives a JSON body labelled `text/event-stream` with status 400. Streaming SDK parsers
  choke on this. **Port fix: only force SSE headers when `resp.status` is 2xx *and* the
  upstream `Content-Type` is `text/event-stream`; otherwise fall through to `build_response`.**
- Client disconnect: `ConnectionResetError`/`CancelledError` swallowed. Upstream is torn down
  only as a side effect of `async with ClientSession(...)` exiting when the handler returns —
  there is no explicit `resp.close()`/abort. Swallowing `CancelledError` is an asyncio
  anti-pattern (breaks task-cancellation semantics).
- **The 300 s total timeout also covers the streaming body.** An `asyncio.TimeoutError` raised
  inside the `async for` is *not* in the caught tuple → it propagates to `forward_request`'s
  `except Exception`, which tries `web.json_response(..., 503)` on an **already-prepared**
  response → `RuntimeError`, half-written stream, ugly client-side truncation. Long generations
  break badly. **Port must use a connect timeout + an idle/inter-chunk timeout, never a total
  timeout on a stream.**
- No `X-Accel-Buffering: no`, no keep-alive ping/heartbeat, no usage capture from the terminal
  `data:` chunk.

---

## 7. How provider switching takes effect

**Single source of truth: the `.active_endpoint` file** in the LocalRouter repo root.

- **New requests**: `resolve_target()` runs at the top of every `forward_request`. The very
  next request after the file changes uses the new target. No restart, no cache invalidation,
  no TTL, no reload signal. Latency of the switch = one file read.
- **In-flight requests**: completely unaffected. `base_url`/`auth_header`/`provider` are
  captured in locals before the upstream call; the open connection runs to completion against
  the old backend and returns the old `X-Provider` header. There is **no draining, no
  cancellation, no notification, no "switching" state**. Two concurrent requests can legally
  hit two different providers.
- **Two equivalent writers**: `POST /switch` on the proxy, and the TUI writing
  `.active_endpoint` directly. Both work because the file is authoritative.
- ⚠️ **Torn-read race**: `Path.write_text()` truncates then writes. A `resolve_target()` that
  lands mid-write reads `""` or partial JSON → `json.loads` raises → message to stderr →
  **that request silently routes to the vast-gguf fallback (`127.0.0.1:8800`)**. Under a switch
  storm this is a real misroute, not a theoretical one.
  **Port fix: write to a temp file in the same directory and `rename()`; on read, retry once
  before falling back.**
- Switching to `vast-gguf` **deletes** the file, destroying the previously configured Together
  `model_id`/`base_url`. Switching back requires re-supplying them.

Rust design note: keep the file as the interop contract (the Python TUI must keep working
during migration), but hold the parsed config in an `ArcSwap`/`RwLock` refreshed by an mtime
check or an inotify watch instead of parsing JSON+TOML on every request.

---

## 8. Timeouts

| Layer | Value | Notes |
|---|---|---|
| Upstream (all proxied requests) | `ClientTimeout(total=300)` | 5 min covering DNS + connect + TLS + headers + **entire body/stream** |
| Upstream connect | — | not set separately |
| Upstream idle/read | — | not set |
| `/providers` vast probe | `total=3` | |
| `/providers` together probe | `total=5` | serial, not concurrent |
| `proxy_status_detail` curl probes | `--max-time 3` / `--max-time 5`, `subprocess` `timeout=5`/`8` | |
| Server keep-alive | aiohttp default 75 s | |
| Graceful shutdown | aiohttp default 60 s, **bypassed** by `sys.exit(0)` in the signal handler | |

**Port requirement**: split into `connect_timeout` (~5 s), `headers_timeout` (~60 s, tunable —
a cold llama.cpp prompt-eval on this laptop can exceed 30 s), and `idle_timeout` between stream
chunks (~120 s). Never a total timeout on a streaming response.

---

## 9. Error mapping — as-implemented vs as-intended

```python
except ConnectionRefusedError:
    return web.json_response({"error": f"Cannot connect to {base_url} — is the backend running?"}, status=502)
except Exception as e:
    return web.json_response({"error": str(e)}, status=503)
```

⚠️ **The 502 branch is effectively dead code.** aiohttp raises
`ClientConnectorError(ClientOSError(ClientConnectionError, OSError))` for a refused connection;
`ClientConnectorError` is **not** a subclass of `ConnectionRefusedError`. So "backend is down"
actually produces:

```
HTTP/1.1 503 Service Unavailable
Content-Type: application/json
{"error": "Cannot connect to host 127.0.0.1:8800 ssl:default [Connect call failed ('127.0.0.1', 8800)]"}
```

Everything upstream-related — connect refused, DNS failure, TLS failure, timeout
(`asyncio.TimeoutError`), malformed response — collapses to **503 with a stringified Python
exception**.

Other observable error shapes:

| Condition | Status | Body |
|---|---|---|
| Backend returns 4xx/5xx | passed through verbatim | backend's own body |
| Request body > 1 MiB | 413 | aiohttp HTML error page |
| `POST /switch` bad JSON | 400 | `{"error":"Invalid JSON body"}` |
| `/switch` local meta malformed | 500 | aiohttp HTML traceback page |
| Timeout mid-stream | truncated stream, then `RuntimeError` | — |

**None of these are OpenAI-shaped.** OpenAI/Anthropic SDKs expect
`{"error": {"message": …, "type": …, "code": …, "param": …}}`; here it is a flat
`{"error": "<string>"}`. No client in the tree depends on the flat form, so the Rust port
should emit proper OpenAI error objects (and map: connect refused → 502, upstream timeout →
504, no active endpoint → 503, oversized body → 413, unknown route on a dead backend → 502).

---

## 10. Usage logging — the hook exists but is **not wired**

```python
USAGE_LOG = PROVIDER_DIR / "usage.log"     # L40  — DEFINED AND NEVER USED
USAGE_DIR = PROVIDER_DIR                   # L41
def ensure_usage_dir(): USAGE_DIR.mkdir(parents=True, exist_ok=True)   # called from create_app()
```

The proxy **never writes a usage record**. The only usage-related output is the `X-Usage:
"{prompt}+{completion}"` response header on non-streaming responses. Real writes come from
`localrouter/cost.py::log_completion()`, invoked by the TUI when *it* makes test calls
(`localrouter/menus/provider_menus.py` L109) — i.e. traffic through the proxy is invisible to
cost tracking.

**JSONL format that already exists in `~/.vastai-gguf/usage.log`** (must stay readable by
`cost.py::get_session_costs()` / `format_usage_summary()`):

```jsonc
{"timestamp":"2026-05-02T20:11:21Z","epoch":1777745481.526,"provider":"together",
 "model_id":"meta-llama/Llama-3.1-8B-Instruct-Turbo","prompt_tokens":100,
 "completion_tokens":50,"cost_usd":2.7e-05}
```

`epoch` is present on some lines and absent on others — readers must treat it as optional.

Cost estimation in `cost.py::log_completion`:

| provider | formula |
|---|---|
| `together` | `round((p + c) * 0.88/1_000_000, 6)` — flat blended rate |
| `local`, `local-gguf` | `0.0` |
| anything else (vast) | `round(((p + c) / (100 * 3600)) * 0.50, 4)` — tokens ÷ 100 tok/s ÷ 3600 × $0.50/h |

⚠️ `cost.py` writes `time.strftime("%Y-%m-%dT%H:%M:%SZ")` — **local time with a `Z` suffix**,
which is wrong. `/switch` correctly uses `time.gmtime()`. The Rust port should write real UTC
(`Z`) and keep field names identical.

**Port requirement**: actually log every completion — non-streaming (parse `usage` from the
buffered JSON) *and* streaming (tee the byte stream, parse the final `data:` chunk's `usage`
when `stream_options.include_usage` is set; otherwise record token counts as `0` or estimate).
Append with `O_APPEND` single-`write` semantics so concurrent writers don't interleave.

---

## 11. Security posture (current)

- Binds loopback only — the one real mitigation. `docs/SKILL.md` pitfall #6 states plainly:
  *"The proxy on localhost:8888 has no auth."*
- **`POST /switch` is unauthenticated and accepts an arbitrary `base_url`.** Any local process
  (or any page that can issue a same-origin-less `fetch` to `127.0.0.1:8888` — and CORS is
  `*` on proxied responses) can repoint the router at an attacker-controlled host. Every
  subsequent prompt is exfiltrated, and for `provider: "together"` the **user's real Together
  API key is attached as `Authorization: Bearer` to that arbitrary URL**. This is a
  credential-exfiltration primitive, not just SSRF.
- **Self-loop footgun**: `/switch {"provider":"local","host":"127.0.0.1","port":8888}` makes the
  proxy call itself recursively until FD exhaustion / the 300 s timeout. `x-proxy-forwarded` is
  stripped but never set, so there is no loop guard.
- `config.toml` holds a plaintext Together key (`tgp_v1_…`) with default file permissions.
- Client `Authorization` is stripped (good) but nothing tells the client that happened.

**Port requirements**: optional shared-secret / bearer for the proxy itself (env or config,
compared in constant time), mandatory auth on all mutating endpoints, an allowlist or
scheme+host validation for `base_url`, a `Via`/`X-ApexRouter-Hop` loop guard returning 508,
and real CORS (explicit origins + `Access-Control-Allow-Headers/Methods` + an `OPTIONS`
handler) rather than a blanket `*` with no preflight support.

---

## 12. CORS (current behaviour, worth calling out)

- `Access-Control-Allow-Origin: *` is added **only** on proxied responses (streaming and
  non-streaming). Not on `/health`, `/providers`, `/switch`.
- **No `OPTIONS` handler.** A preflight `OPTIONS /v1/chat/completions` is *forwarded to the
  backend*; llama-server 405s it and the browser preflight fails.
- `Access-Control-Allow-Headers` / `-Methods` / `-Expose-Headers` are never sent, so browsers
  can neither send `Authorization`/`Content-Type: application/json` nor read `X-Provider`/`X-Usage`.
- Net: browser clients do not work today despite the `*`. Either implement CORS properly or
  drop the header.

---

## 13. Concurrency and connection handling

- Unlimited concurrent requests. No semaphore, no queue, no `max_inflight`, no backpressure,
  no rate limit. Overload is absorbed (badly) by the upstream's own slot limit (`llama-server -np`).
- ⚠️ **A brand-new `ClientSession` per request** (`async with ClientSession(timeout=timeout) as session:`
  inside `forward_request`, L142). That means a fresh `TCPConnector`, a fresh TCP connection,
  and for Together a **full TLS handshake on every single request**. No keep-alive reuse, no
  pooling, unbounded socket/FD growth under concurrency.
  **Port requirement: one process-wide `reqwest::Client` (or one per upstream origin), built
  once, with a bounded pool and keep-alive.** This alone is a large latency win against Together.
- No HTTP/2 to upstream (aiohttp is HTTP/1.1). `upgrade` is stripped → WebSocket proxying is
  impossible.
- aiohttp's access logger (`aiohttp.access`, INFO) has no configured handler → **access logs are
  silently discarded**. `/tmp/vastai-gguf-proxy.log` contains only the startup banner and
  stderr tracebacks.

---

## 14. What is MISSING that a serious router needs

Explicitly checked; all absent.

| # | Capability | Current state | Port priority |
|---|---|---|---|
| 1 | **Multi-endpoint fanout / load balancing** | Exactly one global active target, file-backed. No pools, no weights, no least-loaded, no shadow/mirror traffic, no A/B. | High |
| 2 | **Retries & failover** | Zero. No retry on connect error, no idempotency awareness, no secondary provider, no circuit breaker, no health-gated routing. `/providers` computes health and **nothing consumes it**. | High |
| 3 | **Request logging** | None. No access log (silently dropped), no request id, no latency/TTFT metrics, no status counters, no body capture, no `/metrics`. | High |
| 4 | **Model aliasing / model-name rewriting** | None. `.active_endpoint.model_id` is stored, displayed in the TUI, and **never injected into the request body**. The client's `"model"` string goes upstream verbatim. Switching vast-gguf (`"model":"x"` — llama-server ignores it) → Together (needs a real Together id) **silently breaks every client**. | **Highest** — this is the #1 drop-in papercut |
| 5 | **`/v1/models` aggregation** | None — `/v1/models` is a plain proxy to whichever backend is active, so the list changes under the client's feet, never includes aliases, and never unions across providers. Claude Code and ApexOS both enumerate models. | High |
| 6 | **Cancellation** | Partial/accidental. Client disconnect is swallowed; upstream aborts only as a side effect of the `ClientSession` context exiting. `CancelledError` is swallowed. No explicit abort, no `/cancel`, no request-id-addressable cancel. | Medium |
| 7 | **Concurrency limits** | None (see §13). | Medium |
| 8 | **API-key auth for the proxy itself** | None, by documented design. Combined with unauthenticated `/switch`, this is the biggest security gap (§11). | High |
| 9 | Streaming usage capture | Never parsed; `X-Usage` absent on streams; nothing logged. | Medium |
| 10 | OpenAI-shaped errors | Flat `{"error": "<python str>"}`, wrong status codes (§9). | High |
| 11 | Graceful shutdown / drain | `sys.exit(0)` from the signal handler; in-flight requests dropped. | Medium |
| 12 | Atomic config writes / hot reload | `write_text` torn-read race (§7); per-request file+TOML parse. | High |
| 13 | Request/response size policy | Silent 1 MiB request cap via aiohttp default; full body buffering on responses. | High |
| 14 | Budget enforcement | `usage.log` + cost model exist but nothing reads them at request time. | Low |
| 15 | Sticky routing / session affinity | None. | Low |
| 16 | Anthropic `/v1/messages` support | Purely transparent — no translation. If Claude Code is pointed at ApexRouter via `ANTHROPIC_BASE_URL`, it will speak `/v1/messages`, which today just gets (double-`/v1`-mangled and) forwarded to an OpenAI-only backend. **Open question for the port owner.** | TBD |

---

## 15. Drop-in compatibility checklist for ApexRouter-RS

Must be true for existing clients (Claude Code, ApexOS, `curl`, `smoke.sh`, the LocalRouter TUI):

1. Listen on `127.0.0.1:8888`, port overridable by `PROXY_PORT`.
2. Write `/tmp/vastai-gguf-proxy.pid` at startup, remove it on SIGTERM/SIGINT — the TUI's
   `menu_proxy()`/`_proxy_down()` liveness check is `os.kill(pid, 0)` on that exact path.
3. Serve `GET /health` → `{"ok":true,"provider":…,"uptime":<float secs>}`, always 200.
4. Serve `GET /providers` with the exact JSON shape in §2.
5. Serve `POST /switch` with the exact request/response shapes in §2 (add auth as opt-in so
   existing callers keep working when it's off).
6. Every other (path, method) pair is proxied, including `POST /health`.
7. Read/write `<LocalRouter>/.active_endpoint` with the same schema (§3, §7) so the Python TUI
   and the Rust router stay interoperable during migration. Same for
   `~/.vastai-gguf/{config.toml, local_instances/*.json, usage.log}`.
8. Accept **both** `http://localhost:8888` and `http://localhost:8888/v1` as the client base URL.
9. Preserve `X-Provider` and `X-Usage: "{prompt}+{completion}"` response headers.
10. Strip the client's `Authorization`; inject the per-provider credential.
11. Pass upstream status codes and bodies through unchanged.
12. Stream `text/event-stream` with no added buffering; `Cache-Control: no-cache`; chunked.
13. `smoke.sh` must pass against `http://127.0.0.1:8888` — it exercises `/v1/models`,
    `/v1/chat/completions` (plain, tool-calling with `tools`+`tool_choice`, and a 300-token
    generation) with `"model":"x"` and reads `.usage.prompt_tokens` / `.usage.completion_tokens`
    / `.model` from the response. Note `smoke.sh` appends `/v1` to whatever base you give it —
    another reason item 8 is mandatory.

### Reference values

```
PROXY_PORT        = 8888    (env-overridable)
LOCAL_TUNNEL_PORT = 8800    (vast SSH tunnel; vast_tunnel.sh maps remote 8000 → local 8800)
default local llama-server port = 8100
PID  file  = /tmp/vastai-gguf-proxy.pid
log  file  = /tmp/vastai-gguf-proxy.log
config dir = ~/.vastai-gguf/
active ep  = <LocalRouter checkout>/.active_endpoint
```
