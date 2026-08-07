# Routing

> Normative source: `ARCHITECTURE.md` §3.5 (route types), §4.1 (the table), §4.2 (resolution),
> §4.3 (the request pipeline). Where this document and `ARCHITECTURE.md` disagree,
> `ARCHITECTURE.md` wins. Where either disagrees with `docs/port/00-machine-ground-truth.md`,
> the ground truth wins.

Routing is the product. Everything else in ApexRouter — the rig scan, `fit()`, the supervisor, the
Vast panel — exists so that a `"model"` string a client already sends lands on something that can
answer it. This document says exactly how that string becomes an upstream request, what is
observable while it happens, and which knob changes which decision.

---

## 1. The shape of the thing

```
"model": "auto"                     resolve()            attempt loop
     │                                  │                     │
     ▼                                  ▼                     ▼
 RequestPeek ───► RoutingTable ───► Plan{candidates, ───► candidate 1 ─► 502
 (top-level key    (ArcSwap)         reason, alias,   ───► candidate 2 ─► 200 ✓
  scanner)                           rewrite_model_to,
                                     retry}
```

Three properties hold at every point:

1. **`resolve()` is synchronous and does no I/O.** It reads the compiled table and returns. Nothing
   in resolution can block, probe, time out or cost money.
2. **The decision is observable.** Every response carries
   `X-ApexRouter-Route: <alias-or-"-">|<reason>`, so the answer to "why did that go there?" is a
   response header, not a log grep.
3. **Rebuilding the table never resets live state.** The table holds `Arc<LiveBackend>` pointers;
   a reload swaps the map, not the semaphores, breakers, EWMAs or in-flight counters.

---

## 2. The routing table

### 2.1 What is compiled

`TableBuilder::compile(&Config, &RouteFile, &BackendRegistry) -> Result<RoutingTable, CompileError>`
clones `Arc`s out of the registry into four indexes:

| Index | Key | Used by |
|---|---|---|
| `by_alias` | `Alias` | rule 1 |
| `by_upstream_id` | upstream model id | rules 3 and 4 |
| `by_id` | `BackendId` | rule 2 |
| `legacy_model_names` | `""`, `"x"`, `"auto"`, `"default"` | rule 5 |

plus `default_alias` and a monotonic `generation`.

### 2.2 Reload is atomic, and a failed compile changes nothing

```
parse → compile → validate → ArcSwap::store
                        └── on error: keep the running table, raise an Alert
```

A table that does not compile **never reaches disk and never displaces the one that is serving**.
`ProxyStatus.table_valid` goes `false`, `table_error` carries the message, both GUIs show red, and
`apexrouter status` says so. This is the single most important operational property of the reload
path: a bad edit degrades to "your last good table is still routing", never to "the router is down".

Reload is triggered by, in order of latency: `notify` on `$CONFIG` and `$STATE/routes.json` with a
250 ms debounce, a 10 s poll fallback, `SIGHUP`, and `POST /v1/reload`. The watcher watches **those
two paths only** — never a directory holding endpoint logs, which children append to continuously.

### 2.3 Compile-time validation refuses

| Refusal | Why it is an error and not a warning |
|---|---|
| a dangling target (`BackendSelector::Id` naming nothing) | it would 503 at request time instead of at edit time |
| a duplicate alias | the second one silently wins; nobody can tell which |
| an alias shadowing a live upstream id (unless `allow_shadow`) | rule 1 beats rule 3, so the upstream id becomes unreachable |
| an unsatisfiable `require_tags` | the route can never produce a candidate |
| `Strategy::Cheapest` where no target has a `PriceModel` *and* no `tps_hint` | ranking by an invented number is worse than refusing |

Staleness is never an error. A recipe pointing at a model you deleted is `Severity::Warning`; a
route that cannot ever work is `Severity::Error`. `POST /v1/routes/validate` returns the same
`ValidationReport` without touching the live table, which is what the editors in both GUIs call on
every keystroke.

---

## 3. Resolution — the six rules, in order

```rust
pub fn resolve(&self, model: Option<&str>, class: RequestClass) -> Result<Plan, RouteError>;
```

`class` is `Models | Chat | Completion | Embedding | Rerank | Opaque`, derived from the path. It
narrows candidates before the rules run: an `Embedding` request only ever sees embedding-capable
backends, so `/v1/embeddings` cannot land on a chat-only llama-server.

| # | Condition | Result | `RouteReason` | Carries the route's `[retry]`? |
|---|---|---|---|---|
| 1 | `model` matches an **alias** | that route | `alias` | yes |
| 2 | `model` is `"<backend_id>/<upstream_model>"` | one candidate, **explicit pin** | `explicit_pin` | no — `RetryPolicy::default()` |
| 3 | `model` matches an **upstream model id** on exactly one enabled backend | that backend | `upstream_id_match` | no |
| 4 | same upstream id on several backends | implicit route via `[router] implicit_strategy` | `implicit_multi` | no |
| 5 | `model` ∈ `legacy_model_names` (`""`, `"x"`, `"auto"`, `"default"`, or absent) | `default_alias` | `legacy_model_name` | yes |
| 6 | anything else | `[router] unknown_model` — `reject` → 404; `fallback` → `default_alias` | `default_fallback` only under `fallback` | yes when fallback |

**What each rule buys.**

- **Rule 1** is the product: `auto`, `coder`, `big`, `local` are strings a human types once and
  never revisits.
- **Rule 2** is the escape hatch. `local-carnice/Carnice-9b-Q6_K` names a backend and a model with
  no ambiguity and no failover, which is what you want when you are benchmarking or bisecting.
- **Rule 3 is what makes every existing client work unchanged.** A tool that hardcodes
  `meta-llama/Llama-3.3-70B-Instruct-Turbo` keeps working the day you put ApexRouter in front of it.
  It reads `model_index`, which the health prober maintains — a cold index means rule 3 misses and
  rule 5 or 6 applies. That is documented, deterministic, and visible in `X-ApexRouter-Route`.
- **Rule 4** additionally raises a one-shot `Alert` naming the collision. A duplicate upstream id
  appearing right after a rental is exactly the moment routing silently changes under you, so it is
  loud once rather than silent forever.
- **Rule 5 is why `smoke.sh`'s hardcoded `"model":"x"` keeps working.** So does an absent `model`
  key, and `""`.
- **Rule 6 defaults to `reject`**: `404 model_not_found`, listing the known aliases. Setting
  `[router] unknown_model = "fallback"` restores LocalRouter's old behaviour. Rejecting by default
  is deliberate — a fat-fingered `gpt-4o-mimi` must not silently bill a rented H100.

### 3.1 `Plan`, and why `retry` lives on it

```rust
pub struct Plan {
    pub candidates: SmallVec<[Candidate; 4]>,
    pub reason: RouteReason,
    pub alias: Option<Alias>,
    pub rewrite_model_to: Option<String>,
    pub retry: RetryPolicy,
}
pub struct Candidate { pub backend: Arc<LiveBackend>, pub upstream_model: String }
```

`Plan::retry` is **the matched route's own `[retry]` block**, and it is on the `Plan` because that
is the only path from `routes.toml` to the attempt loop. Rules 1, 5 and 6 copy it off the matched
`CompiledRoute`; rules 2, 3 and 4 name a *backend* rather than a *route*, so they carry
`RetryPolicy::default()` — which is also what a route declaring no `[retry]` block compiles to. A
`Plan` without this field is precisely how a config key gets parsed, validated, and then silently
ignored.

`rewrite_model_to` is `Some` only when the alias differs from the upstream id. When it is `None`
the body is relayed **byte-for-byte**; when it is `Some` exactly one JSON key changes.

---

## 4. Strategies

mk1 ships exactly the strategies it implements. There is no reachable `todo!()` from config.

| `Strategy` | Picks | Ties broken by | Use it when |
|---|---|---|---|
| `first_healthy` | the first routable target in declaration order | declaration order | you have a preference order (local first, rented second, managed last) |
| `round_robin` | next routable target, per-alias cursor | cursor | N identical backends |
| `least_busy` | lowest `inflight / max_concurrent`, then lowest `LatencyEwma` | declaration order | heterogeneous hardware serving the same model |
| `cheapest` | lowest `PriceModel::per_mtok(tps_hint)` | declaration order | a route mixing a free local box with a metered provider |

`cheapest` normalises a `PerHour { dph }` rental into per-Mtok, which needs a throughput
assumption. The assumption is **returned with the number** as
`CostEstimate::Approximate { assumption }` and rendered as such in every surface. Nothing buries a
100 tok/s constant.

A target is **routable** when: `enabled`, `Health` is `Ready` (or `Starting` is not counted as
routable), `accepting` is true (it is false while draining), and the breaker is not `Open`.

### 4.1 Filters

`RouteFilter` narrows the candidate set *before* the strategy ranks it:

```toml
[routes.big.filter]
require_tags       = ["gpu"]      # every tag must be present
exclude_tags       = ["rented"]   # any match excludes
max_cost_per_mtok  = 2.00         # in USD; a target with no price model is excluded
min_ctx            = 32768        # from BackendLimits.ctx
require_vision     = true         # from UpstreamModel.vision
require_tools      = false
```

An empty candidate set after filtering is `503 no_healthy_backend` — never a silent fall-through to
some other route.

---

## 5. The attempt loop

```
for candidate in plan.candidates,
    bounded by plan.retry.attempts AND a wall-clock deadline:
  ├─ (ingress, candidate.protocol) selects the matrix cell   → relay | translate | 501
  ├─ breaker.check()                     → skip if Open (min_volume 5 before it can trip)
  ├─ InFlightGuard::acquire(backend)
  │     .timeout(queue_timeout_ms)       → 503 + Retry-After if saturated
  ├─ outbound_headers(inbound, cred)     → CONSTRUCTED from an allowlist
  ├─ body: Passthrough(bytes) | Rewritten (only "model" changes)
  ├─ send: connect_timeout → headers_timeout
  └─ classify the failure
```

### 5.1 What is retryable, and what is not

| Outcome | Class | Retried on a different target? |
|---|---|---|
| connect / DNS / TLS failure | `Retryable` + `breaker.trip()` | yes |
| timeout **before** response headers | `Retryable` + `breaker.trip()` | yes |
| `429` with `Retry-After` | `Retryable` | yes — and only on a **different** target |
| `502` `503` `504` `529` | `Retryable` | yes |
| any other status, including `4xx` | `Terminal` | no — relayed verbatim |
| **anything after the first upstream byte** | `Committed` | **never** |

**The first byte commits the request.** This is enforced by types, not by a comment: the loop
consumes `PreFlight` values and can only exit by producing a `Committed`, so
"retry after the first byte" is unrepresentable.

```rust
async fn attempt(p: PreFlight<'_>) -> Result<Committed, Retryable>;
```

`failover = false` restricts the loop to the first candidate; `attempts` bounds it; the wall-clock
deadline bounds it again, because N candidates × a slow connect is still a hang from the client's
point of view. `honor_retry_after` is consumed inside `attempt()`.

A 4xx is terminal on purpose. A `400 {"error":"context length exceeded"}` means the *request* is
wrong; trying it on three more backends produces three more identical 400s, three times the latency,
and on a metered provider, three times the bill.

### 5.2 The breaker and the retry budget

- The breaker needs `breaker_min_volume` (default **5**) observations before it can open, so a
  single 200 ms blip on a 1 rps rig does not manufacture a 30 s outage.
- Retries are drawn from a **per-backend token bucket** (`retry_budget_per_min`, default 30). A
  struggling backend therefore cannot be amplified into a storm by its own failures.
- `X-ApexRouter-Attempts` reports how many were made; `X-ApexRouter-Fallback` reports whether the
  answer came from a target other than the first.

### 5.3 Admission control

Per-backend concurrency is a `Semaphore` sized from `/props.total_slots` when available, else from
the length of `/slots`, else from `BackendLimits.max_concurrent`, else **`[router] max_inflight`**
as that backend's permit default (a *per-backend* count, not a global one). Globally there is a
**`max_inflight_bytes`** budget (512 MiB by default) on resident request bodies — without it,
N backends × large bodies would be unbounded RSS.

`InFlightGuard` owns the semaphore permit, the byte-budget permit, the in-flight gauge and the
`RequestRecord`. Its `Drop` emits `RequestFinished { aborted: true }` when `finish()` was never
called, so a client Ctrl-C cannot leak a permit or leave a zombie row in the UI.

---

## 6. Observability

Every response, success or failure:

```
X-ApexRouter-Route:     auto|alias          # <alias-or-"-">|<reason>
X-ApexRouter-Backend:   local-carnice
X-ApexRouter-Attempts:  1
X-ApexRouter-Fallback:  false
X-Request-Id:           01JB2Z…             # also on the RequestRecord
Via:                    1.1 apexrouter      # the loop guard's own token
```

`reason` is one of `alias`, `explicit_pin`, `upstream_id_match`, `implicit_multi`,
`default_fallback`, `legacy_model_name`.

When the ingress dialect is not `open_ai`, `X-ApexRouter-Protocol: anthropic->open_ai` names the
matrix cell that ran (`ARCHITECTURE.md` §3.4).

The same decision lands in `RequestRecord` (`GET /v1/requests`), in the `RequestFinished` WS event,
and in `apexrouter_requests_total{alias,backend,status}`.

---

## 7. The route file

`$STATE/routes.json` is the canonical form — a `RouteFile`,
`{ "schema_version": 1, "default_alias": "auto", "routes": [ModelRoute] }`. `routes.example.toml` at
the repo root is the same data, hand-editable, and every field maps 1:1 onto
`apexrouter_protocol::route::*`. `PUT /v1/routes` writes the whole table atomically; so do
`apexrouter route set`, the web UI and the Slint app, all four through the same validator.

```toml
# routes.example.toml
schema_version = 1
default_alias  = "auto"

[[routes]]
alias       = "auto"
strategy    = "first_healthy"      # first_healthy | round_robin | least_busy | cheapest
is_default  = true
description = "whatever is up, cheapest-to-run first"

  [[routes.targets]]
  backend = { sel = "id", value = "local-carnice" }   # sel: id | tag | glob
  model   = "Carnice-9b-Q6_K"                         # omit to pass the alias through
  weight  = 1

  [[routes.targets]]
  backend = { sel = "tag", value = "rented" }
  weight  = 1

  [routes.filter]
  require_tags   = []
  exclude_tags   = []
  min_ctx        = 8192
  require_vision = false
  require_tools  = false

  [routes.retry]
  attempts          = 2            # 1 = no failover attempt at all
  failover          = true
  honor_retry_after = true
```

`BackendSelector` has three forms, all resolved **at compile time**, so a `glob` that matches
nothing is a compile error rather than a runtime 503:

| `sel` | Matches | Example |
|---|---|---|
| `id` | exactly one backend | `local-carnice` |
| `tag` | every backend carrying the tag | `rented`, `vision`, `gpu:vulkan` |
| `glob` | backend ids by shell glob | `vast-*` |

---

## 8. Worked examples

**A client that knows nothing about ApexRouter.**

```
POST /v1/chat/completions  {"model":"meta-llama/Llama-3.3-70B-Instruct-Turbo"}
  rule 3 → the Together backend that advertises that id
  X-ApexRouter-Route: -|upstream_id_match
```

Nothing was configured. The prober had already indexed Together's catalogue.

**`smoke.sh`, unchanged since 2024.**

```
POST /v1/v1/chat/completions  {"model":"x"}
  path normalisation collapses the doubled /v1
  rule 5 → default_alias "auto" → first_healthy → local-carnice
  X-ApexRouter-Route: auto|legacy_model_name
```

**A typo.**

```
POST /v1/chat/completions  {"model":"gpt-4o-mimi"}
  rule 6, unknown_model = "reject"
  404 {"error":{"type":"model_not_found",
                "message":"unknown model 'gpt-4o-mimi'; known aliases: auto, coder, big, local"}}
```

Zero upstream hops, zero cost.

**Failover in the open.**

```
POST /v1/chat/completions  {"model":"big"}
  rule 1 → route "big" → [vast-h100, together]
  attempt 1: vast-h100 → connect refused (instance still booting) → breaker.trip()
  attempt 2: together  → 200
  X-ApexRouter-Route: big|alias   X-ApexRouter-Attempts: 2   X-ApexRouter-Fallback: true
```

**Committed, so not retried.**

```
POST /v1/chat/completions  {"model":"big","stream":true}
  attempt 1: vast-h100 → 200, first SSE frame delivered … then the socket dies
  → one synthetic frame: data: {"error":{"message":"upstream ended mid-stream",
                                         "type":"upstream_unavailable"}}
  → data: [DONE], close.
  NOT re-sent to `together`: the client has already seen tokens from vast-h100.
```

---

## 9. Knobs, and what each one actually changes

| `[router]` key | Default | Changes |
|---|---|---|
| `default_alias` | `"auto"` | where rules 5 and 6-as-fallback land |
| `implicit_strategy` | `"first_healthy"` | rule 4's ranking |
| `unknown_model` | `"reject"` | rule 6: `reject` → 404, `fallback` → `default_alias` |
| `max_inflight` | 64 | **per-backend** permit default when `/props` and limits are silent |
| `max_inflight_bytes` | 536870912 | **global** resident-body budget |
| `request_usage` | `"off"` | parsed; **not yet applied** on the OpenAI path in mk1 (see config doc) |
| `max_body_bytes` | 33554432 | per-request 413 threshold |
| `connect_timeout_ms` | 5000 | TCP/TLS establishment |
| `headers_timeout_ms` | 600000 | first response byte — **not** a total timeout |
| `idle_timeout_ms` | 300000 | **between** stream chunks |
| `queue_timeout_ms` | 30000 | how long to wait for a backend permit before 503 |
| `retry_budget_per_min` | 30 | per-backend retry token bucket |
| `breaker_min_volume` | 5 | observations before the breaker may open |

`headers_timeout_ms` is 600 s rather than the usual 30 because for a **non-streaming** completion
llama.cpp sends no headers until generation finishes, and 600 tokens at 4 tok/s is 150 s before the
first byte. A 30 s total timeout here does not protect anything; it just cancels correct requests.

---

## 10. What routing deliberately does not do

- **No health probing inside `resolve()`.** Liveness comes from the prober, asynchronously. A
  routing decision that makes a network call is a routing decision that can hang.
- **No content-based routing.** Nothing reads the prompt to pick a backend. `RequestPeek` is a
  top-level-key scanner for `model`, `stream` and `stream_options.include_usage` — not a
  `serde_json::Value` parse, and never the messages.
- **No queueing across backends.** If every candidate is saturated, the answer is
  `503 server_overloaded` with `Retry-After`, immediately. A router that queues indefinitely is a
  router that hides an outage.
- **No mid-stream re-routing.** See §5.1.

---

## 11. See also

- `docs/API.md` — every route, with a body example per route.
- `openapi/apexrouter-v1.yaml` — the machine-readable form of the same contract.
- `ARCHITECTURE.md` §4.2 — the normative statement of the six rules.
