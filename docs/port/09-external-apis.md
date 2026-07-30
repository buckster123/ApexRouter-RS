# External API contracts — ApexRouter-RS

Researched 2026-07-30. Everything ApexRouter-RS talks to over a socket, and the exact shape of
those conversations.

## Verification legend

Every claim below carries a marker. **Do not treat unmarked prose as fact.**

| Mark | Meaning |
|---|---|
| `[V-LOCAL]` | Verified by reading a file on this laptop or running a local binary today. Highest confidence. |
| `[V-DOC]` | Verified against the vendor's own current documentation, fetched 2026-07-30. |
| `[V-BOTH]` | Local artefact and vendor docs agree. |
| `[CONFLICT]` | Local artefact and vendor docs **disagree**. Implementation must tolerate both. |
| `[?]` | Inferred, second-hand, or undocumented. **Must be probed at runtime, never assumed.** |

The general rule for this whole document: **parse permissively, emit conservatively.** Every
response struct should be `#[serde(default)]` with `Option<T>` fields and an
`#[serde(flatten)] extra: serde_json::Map<String, Value>` escape hatch. Three of the five APIs here
returned fields the vendor docs do not mention.

---

## 1. Vast.ai REST API

### 1.1 Where the ground truth came from

`00-machine-ground-truth.md` says the `vastai` CLI is broken on this box. **That is correct, and I
found out why** `[V-LOCAL]`:

```
/home/andre/.local/bin/vastai         → #!/usr/bin/python3 ... from vastai.cli.main import main
/usr/bin/python3                      → Python 3.14.4
package installed at                  → ~/.local/lib/python3.13/site-packages/vastai   (v1.0.4)
```

The package is intact; the interpreter moved from 3.13 to 3.14 and left it behind. So:

- The **decision stands**: speak REST directly, never shell out. A CLI that breaks on a Python
  point-release is not a dependency.
- But the **source is readable**, and it is the single best specification of the API that exists.
  Everything in §1 marked `[V-LOCAL]` comes from reading
  `~/.local/lib/python3.13/site-packages/vastai/**/*.py` (v1.0.4, dated 2026-04-22).

Also present: `~/.local/lib/python3.13/site-packages/vastai/SKILL.md` (17 KB), Vast's own
agent-facing cheat-sheet. Worth a read before implementing.

**There is no Vast.ai crate on crates.io** `[V-LOCAL]` — `vastai` and `vast-ai` are unregistered;
`vast` is a Verilog AST library. Hand-roll it.

### 1.2 Base URL, auth, transport

```
base            https://console.vast.ai
api prefix      /api/v0        (auto-prepended unless the path already matches ^/api/v\d+/)
auth header     Authorization: Bearer <API_KEY>
content type    application/json
```

`[V-BOTH]` — `vastai/api/client.py:22,41-43,66` and
<https://docs.vast.ai/api-reference/authentication>.

Override base URL via `VAST_URL` env var `[V-LOCAL]` (`client.py:22`). Useful for tests.

**Gotcha `[V-LOCAL]`**: the Python client sends the key **twice** — as a Bearer header *and* as an
`?api_key=` query parameter (`client.py:39-40`). We should send the header only. If an endpoint
mysteriously 401s, try adding the query param before assuming the key is bad; some older routes
may only read the query form. Log the *absence* of a key, never its value.

**Key on disk** `[V-LOCAL]`: `~/.config/vastai/vast_api_key`, 64 bytes, plaintext, no newline
handling guaranteed — `.trim()` it. Legacy path `~/.vast_api_key` is copied forward by the CLI
(`cli/util.py:245-251`). Env var `VAST_API_KEY` takes precedence in the CLI (`cli/main.py:55`).
There is also `~/.config/vastai/vast_tfa_key` for 2FA-scoped keys, preferred over the main key when
present (`cli/main.py:78`) `[V-LOCAL]` — we can ignore 2FA for mk1 but should not clobber the file.

### 1.3 Rate limiting

`[V-DOC]` <https://docs.vast.ai/api-reference/rate-limits-and-errors>:

- 429 with body message `"API requests too frequent"` or
  `"API requests too frequent: endpoint threshold=..."`.
- **No numeric limit is published.** It is "a minimum interval between requests for a given
  endpoint and identity".
- **No `Retry-After` and no `X-RateLimit-*` headers are set.** Clients must implement their own
  backoff.

The CLI's backoff `[V-LOCAL]` (`client.py:71-102`): 3 attempts, sleep starts at 150 ms and
multiplies by 1.5 each retry. That is almost certainly too aggressive for a polling loop. Use
exponential backoff with jitter, cap around 30 s, and **treat instance-status polling as the
dominant cost** — poll no faster than every 5 s.

### 1.4 Search offers

**`POST /api/v0/bundles/`** `[V-BOTH]` — the whole query *is* the request body, filters and control
keys mixed together at the top level.

```jsonc
{
  // --- filters: every key is {op: value}, ops are eq neq gt gte lt lte in notin
  "verified":   { "eq": true },
  "external":   { "eq": false },
  "rentable":   { "eq": true },
  "rented":     { "eq": false },
  "gpu_name":   { "in": ["RTX 4090", "RTX 3090"] },
  "num_gpus":   { "gte": 1 },
  "gpu_ram":    { "gte": 24000 },      // MEGABYTES
  "dph_total":  { "lte": 0.5 },
  "reliability":{ "gte": 0.99 },       // 0..1, not a percentage
  "inet_down":  { "gte": 100 },        // Mbit/s
  "inet_up":    { "gte": 50 },
  "cuda_max_good": { "gte": 12.0 },
  "geolocation":{ "in": ["US", "CA"] },
  "direct_port_count": { "gte": 1 },
  "disk_space": { "gte": 40 },         // GB

  // --- control keys, NOT filters
  "type":  "on-demand",                // see CONFLICT below
  "order": [["dph_total", "asc"]],     // list of [field, "asc"|"desc"]
  "limit": 100,
  "allocated_storage": 40.0            // GiB, feeds the storage part of dph_total
}
```

Response: `{"offers": [ {...}, ... ]}` `[V-BOTH]`.

**`[CONFLICT]` — the `type` value.** The installed CLI sends `"on-demand"` with a hyphen
(`api/offers.py:36`, and maps the user-facing `"interruptible"` → `"bid"`). The published API
reference example sends `"ondemand"` with no hyphen. Both appear in the wild.
**Defend:** send `"on-demand"` (the CLI is what actually runs against production), and if a search
returns zero offers with otherwise-sane filters, retry once with `"ondemand"` before reporting
"no capacity". Other values: `"bid"`, `"reserved"` `[V-BOTH]`.

**Default filters `[V-LOCAL]`** (`api/offers.py:26-27`) — the CLI silently injects these unless you
pass `--no-default`:
`{"verified":{"eq":true},"external":{"eq":false},"rentable":{"eq":true},"rented":{"eq":false}}`.
We should inject the same set explicitly rather than relying on server defaults.
Default order is `[["score","desc"]]` `[V-LOCAL]`.

**Unit multipliers `[V-LOCAL]`** (`api/query.py:176-181`) — the CLI's *human* query language
multiplies before sending. Over raw REST **you must pre-multiply yourself**:

| Field | Human unit | Wire unit | Multiplier |
|---|---|---|---|
| `cpu_ram` | GB | MB | ×1000 |
| `gpu_ram` | GB | MB | ×1000 |
| `gpu_total_ram` | GB | MB | ×1000 |
| `duration` | days | seconds | ×86400 |

`reliability` is a 0..1 float on the wire; the CLI display multiplies by 100 `[V-LOCAL]`
(`cli/display.py:77`).

**Field aliases `[V-LOCAL]`** (`api/query.py:167-174`) — accept these in *our* CLI/config surface
and translate, because users will type them:
`cuda_vers → cuda_max_good`, `dph → dph_total`, `display_active → gpu_display_active`,
`dlperf_usd → dlperf_per_dphtotal`, `flops_usd → flops_per_dphtotal`.

**Full filterable field set `[V-LOCAL]`** (`api/query.py:108-165`) — validate against this before
sending, since unknown fields are silently ignored server-side:

```
bw_nvlink compute_cap cpu_arch cpu_cores cpu_cores_effective cpu_ghz cpu_ram cuda_max_good
datacenter direct_port_count driver_version disk_bw disk_space dlperf dlperf_per_dphtotal
dph_total duration external flops_per_dphtotal gpu_arch gpu_display_active gpu_frac gpu_mem_bw
gpu_name gpu_ram gpu_total_ram gpu_max_power gpu_max_temp has_avx host_id id inet_down
inet_down_cost inet_up inet_up_cost machine_id min_bid mobo_name num_gpus pci_gen pcie_bw
reliability rentable rented storage_cost static_ip total_flops ubuntu_version verification
verified vms_enabled geolocation cluster_id
```

**Geolocation** is a 2-letter country code on the wire; the CLI expands friendly region names
client-side into `in [..]` lists `[V-LOCAL]` (`api/offers.py:400-407`). If we want
`--region Europe` we must ship that table ourselves — it is 6 regions, ~200 codes, copy it from
`offers.py`. Note the response's `geolocation` field is a *display string* like
`"Atlantis, AT"` `[V-DOC]`, not a bare code — parse the trailing code, don't string-equal it.

**Offer response fields `[V-LOCAL]`** — the `Offer` dataclass in `vastai/data/offer.py` is a
complete, typed, field-by-field transcription of the `/bundles/` response and is the best struct
definition available. Key ones for scoring:
`id` (this is the **ask id** you rent, not a machine id), `machine_id`, `host_id`, `num_gpus`,
`gpu_name`, `gpu_ram` (MB), `gpu_total_ram` (MB), `cuda_max_good`, `dph_base`, `dph_total`,
`min_bid`, `storage_cost`, `storage_total_cost`, `dlperf`, `dlperf_per_dphtotal`, `score`,
`reliability`, `verification`, `rentable`, `rented`, `inet_up`, `inet_down`, `direct_port_count`,
`disk_space`, `geolocation`, `public_ipaddr`, `duration`, `cuda_max_good`, `static_ip`.

**`direct_port_count` is the field that decides whether we can expose a port.** Filter
`direct_port_count >= 2` (one for SSH, one for the model server) or accept proxy-only SSH.

**New endpoint `[V-LOCAL]`**: `PUT /api/v0/search/asks/` with body
`{"select_cols": ["*"], "q": <same query object>}` returns the same `{"offers": [...]}`
(`api/offers.py:94-97`). The CLI ships both and defaults to `/bundles/`. Prefer `/bundles/` for
mk1; note `/search/asks/` exists in case `/bundles/` is retired.

### 1.5 Create instance

**`PUT /api/v0/asks/{offer_id}/`** `[V-BOTH]`. The offer id comes from `id` in the search result.
Each offer id can be rented **once**.

Body actually sent by the CLI `[V-LOCAL]` (`api/instances.py:77-101`) — note the key is `onstart`,
not `onstart_cmd`:

```jsonc
{
  "client_id": "me",
  "image": "ghcr.io/ggml-org/llama.cpp:server-cuda",
  "disk": 40,                      // GB, local disk partition
  "env": { "-p 18080:18080": "1", "MODEL_URL": "...", "HF_TOKEN": "..." },
  "price": null,                   // $/hr bid price → creates an INTERRUPTIBLE instance
  "label": "apexrouter-mk1",
  "extra": null,
  "onstart": "#!/bin/bash\n...",   // startup script CONTENTS, not a path
  "image_login": null,             // "-u user -p pass docker.io" for private registries
  "python_utf8": false,
  "lang_utf8": false,
  "use_jupyter_lab": false,
  "jupyter_dir": null,
  "force": false,
  "cancel_unavail": false,         // true = fail instead of creating a stopped instance
  "template_hash_id": null,
  "user": null,
  "runtype": "ssh_direc ssh_proxy",   // see below
  "args": null,                    // entrypoint args (args runtype only)
  "volume_info": null
}
```

Response `[V-BOTH]`:

```json
{ "success": true, "new_contract": 7835610 }
```

**The instance id is `new_contract`, not `id`.** `[V-BOTH]` — this trips everyone.
`vastai/data/instance.py:34-49` also declares optional `instance_api_key` and `ask_id` in the
response `[V-LOCAL]`; the docs mention neither. Capture `instance_api_key` if present — it may be
the per-instance credential for portal auth `[?]`.

**`runtype` is a space-separated set of tokens, not an enum** `[V-LOCAL]`
(`cli/commands/instances.py:94-110`). Derivation:

| Intent | `runtype` value |
|---|---|
| ssh, proxy only | `ssh_proxy` |
| ssh, direct preferred | `ssh_direc ssh_proxy`  ← note the missing `t`, it is spelled `direc` |
| jupyter, proxy | `jupyter_proxy ssh_proxy` |
| jupyter, direct | `jupyter_direc ssh_direc ssh_proxy` |
| raw entrypoint | `args` |

`[CONFLICT]` — the API reference lists `runtype` as an enum of
`ssh, jupyter, args, ssh_proxy, ssh_direct, jupyter_proxy, jupyter_direct` (with a `t`).
**Send the CLI's `ssh_direc ssh_proxy` form**, which is what production actually receives.

**`env` is a flat string→string map that encodes Docker flags as keys** `[V-LOCAL]`
(`cli/util.py:509-545`). This is the ugliest part of the API:

- `-e FOO=bar` → `{"FOO": "bar"}`
- `-p 18080:18080` → `{"-p 18080:18080": "1"}`  ← the flag is the *key*, value is the string `"1"`
- `-p 8081:8081/udp` → `{"-p 8081:8081/udp": "1"}`
- `-h hostname` → `{"-h": "hostname"}`
- `-v /host:/ctr` → `{"-v /host:/ctr": "1"}`
- `-n netname` → `{"-n netname": "1"}`

Build this map programmatically; never string-concatenate a user-supplied port.

**`PORTAL_CONFIG` validation `[V-LOCAL]`** (`cli/commands/instances.py:137-146`): if you set
`PORTAL_CONFIG` in env and `runtype` does not contain `jupyter`, the CLI strips every
jupyter-related segment and errors if nothing remains. Mirror that check client-side or the
instance will boot into a broken portal.

**Bulk create**: `POST /api/v0/asks/bulk/` with `{"ids": [...]}` plus the same body `[V-LOCAL]`.
Batches are split at 64 `[V-LOCAL]` (`cli/commands/instances.py:304`).

**One-shot search+rent**: `PUT /api/v0/launch_instance/` takes `gpu_name`, `num_gpus`, `region`,
`image`, `disk`, and the whole query object under `q`, and rents the top match server-side
`[V-LOCAL]` (`api/offers.py:447-474`). Tempting, but it hides *which* offer was taken and gives us
no scoring control. **Do not use it for mk1** — search then rent, so we can log the decision.

### 1.6 Show instances / instance detail

- **`GET /api/v0/instances/?owner=me`** → `{"instances": [ ... ]}` `[V-LOCAL]` (`api/instances.py:29`)
- **`GET /api/v0/instances/{id}/?owner=me`** → `{"instances": { ... }}` `[V-LOCAL]`
  (`api/instances.py:63`) — **singular detail is still under the plural key `instances`, but it is
  an object, not an array.** `[V-BOTH]`
- **`GET /api/v1/instances/`** (note **v1**) — paginated, takes `select_filters`, `order_by`,
  `limit`, `after_token`, `select_cols`; returns
  `{instances, next_token, total_instances, label_counts}` `[V-LOCAL]` (`api/instances.py:40-52`).
  Not needed for mk1 (we own few instances) but exists if listing gets slow.

Two fields need post-processing that the CLI does client-side `[V-LOCAL]` (`api/instances.py:32-37`):

- `extra_env` arrives as a **list of `[key, value]` pairs**, not a map. Convert.
- `duration` is **computed by the client** as `now - start_date`; the server does not send it.
  `start_date` is a float unix epoch.

**Fields that matter to us** (from `vastai/data/instance.py` `[V-LOCAL]`, cross-checked against the
docs `[V-DOC]`):

| Field | Type | Notes |
|---|---|---|
| `id` | int | instance id (== `new_contract`) |
| `actual_status` | `Option<String>` | **`null` means provisioning** — see table below |
| `intended_status` / `cur_state` / `next_state` | string | secondary signals |
| `status_msg` | `Option<String>` | free text; contains the reason on failure |
| `ssh_host` | string | e.g. `ssh2281.vast.ai` (proxy) or a bare IP (direct) |
| `ssh_port` | int | |
| `ssh_idx` | string | |
| `public_ipaddr` | string | the machine's shared public IP |
| `ports` | see `[CONFLICT]` below | port map |
| `direct_port_start` / `direct_port_end` | int | **`-1` when direct ports are unavailable** `[V-DOC]` |
| `gpu_util` | `Option<f32>` | percent |
| `gpu_temp`, `cpu_util`, `mem_usage`, `mem_limit`, `disk_util` | Option<f32> | telemetry |
| `image_uuid`, `image_runtype`, `image_args` | | what is running |
| `dph_total` | f32 | live burn rate |
| `start_date`, `end_date` | f32 epoch | |
| `uptime_mins` | Option<f32> | |
| `jupyter_token` | string | |
| `label` | Option<String> | **our correlation key — always set it** |
| `machine_id`, `host_id` | int | |

**`[CONFLICT]` — the shape of `ports`.** The API reference says `"ports": integer[]`, e.g.
`[8080, 8081]` `[V-DOC]`. The CLI reads it as a **Docker-style map** `[V-LOCAL]`
(`cli/commands/misc.py:209-215`):

```json
"ports": { "22/tcp": [ { "HostIp": "0.0.0.0", "HostPort": "40023" } ] }
```

and does `ports["22/tcp"][0]["HostPort"]`. The map form is what running instances actually return;
the array form may be what an *offer* or a not-yet-running instance returns.
**Defend:** deserialize `ports` as `serde_json::Value` and write one function
`external_port(inst, internal: u16, proto: &str) -> Option<u16>` that handles map-of-arrays,
map-of-single-object, array-of-int, and `null`. Never index blindly.

**Status values `[V-DOC]`** (`SKILL.md`, and the same table is in the online docs):

| `actual_status` | Meaning |
|---|---|
| `null` | provisioning |
| `created` | created, not provisioned |
| `loading` | pulling image / starting container |
| `running` | active — **GPU charges start here** |
| `stopped` | halted, disk charges only |
| `frozen` | paused with memory, GPU charges apply |
| `exited` | container process exited |
| `rebooting` | transient |
| `unknown` | no recent host heartbeat |
| `offline` | host disconnected |

**Poll-loop rule, quoted from Vast's own docs `[V-DOC]`:** *"If `actual_status` becomes `exited`,
`unknown`, or `offline` it will never reach `running`. Always add a timeout and error branch —
otherwise your script loops forever while disk charges accrue."* Our state machine **must** treat
those three as terminal-failure, destroy the instance, and either retry a different offer or give
up. **Storage charges begin at creation; GPU charges begin at `running`.**

### 1.7 Destroy, stop, start

| Action | Method + path | Body |
|---|---|---|
| destroy one | `DELETE /api/v0/instances/{id}/` | `{}` |
| destroy many | `DELETE /api/v0/instances/` | `{"instance_ids": [...]}` |
| stop | `PUT /api/v0/instances/{id}/` | `{"state": "stopped"}` |
| start | `PUT /api/v0/instances/{id}/` | `{"state": "running"}` |
| reboot | `PUT /api/v0/instances/reboot/{id}/` | `{}` |
| recycle | `PUT /api/v0/instances/recycle/{id}/` | `{}` |
| label | `PUT /api/v0/instances/{id}/` | `{"label": "..."}` |
| change bid | `PUT /api/v0/instances/bid_price/{id}/` | `{"client_id":"me","price":0.2}` |

All `[V-LOCAL]` (`api/instances.py:112-172`). Responses are `{"success": bool, ...}`; failures
carry `msg` `[V-LOCAL]` (`cli/commands/instances.py:318-321`).

**Destroy is irreversible and deletes the disk.** ApexRouter must guarantee destroy-on-exit
(Drop guard + a persisted "orphan list" checked at startup), because a leaked instance bills
until someone notices.

### 1.8 Logs and exec — the two-phase `result_url` pattern

**`PUT /api/v0/instances/request_logs/{id}/`** `[V-LOCAL]` (`api/instances.py:214-229`):

```jsonc
// request
{ "tail": "1000", "filter": "error", "daemon_logs": "true" }   // all optional
// response
{ "success": true, "result_url": "https://s3.../logs/....txt?..." }
```

Then **GET the `result_url` with no auth** until it returns 200. The CLI polls 30 times at 300 ms
(≈9 s total) and raises `TimeoutError` otherwise `[V-LOCAL]` (`api/instances.py:7-14`).
The URL is not immediately live — a 403/404 on the first fetch is normal, not an error.
If the response has no `result_url`, the JSON body itself is the answer.

**`PUT /api/v0/instances/command/{id}/`** with `{"command": "nvidia-smi"}` uses the identical
two-phase pattern `[V-LOCAL]` (`api/instances.py:203-211`). This is how we can run a health probe
inside the container without SSH.

`[?]` The `result_url` host, TTL, and whether it is ever `http://` are undocumented. Fetch it with
a plain client, allow redirects, do **not** attach our Bearer token to it, and cap the poll at
~30 s with exponential backoff rather than the CLI's tight loop.

### 1.9 Exposing the served model — three mechanisms, pick one

This is the question that decides ApexRouter's remote-serving design.

#### (a) Docker port mapping → public IP + random high port

`[V-DOC]` <https://docs.vast.ai/guides/instances/connect/networking>:

- Instances share a public IP. `-p INTERNAL:INTERNAL` in `env` maps the internal port to a
  **random external port** on `public_ipaddr`.
- Identity mapping for ports ≥ 70000 (`-p 70000:70000`) also maps to a random external port.
- **Limit: 64 open ports per container.**
- Inside the container, Vast injects `VAST_TCP_PORT_<N>` and `VAST_UDP_PORT_<N>` env vars holding
  the external port for internal port N. `VAST_TCP_PORT_22` is SSH, `VAST_TCP_PORT_8080` is
  Jupyter. **An onstart script can read `$VAST_TCP_PORT_18080` and report it back to us.**
- From outside, the mapping is read from the instance's `ports` field (§1.6) →
  `http://{public_ipaddr}:{HostPort}`.

**This is plaintext HTTP over the open internet.** If we use it, `llama-server` **must** be started
with `--api-key` and the key must be a fresh random secret per instance. Do not skip this: an
unauthenticated llama-server on a public IP is an open relay for whoever port-scans it.

Requires the offer to have `direct_port_count >= 1`, otherwise there are no ports to hand out.

#### (b) SSH local port forward

`[V-DOC]` <https://docs.vast.ai/guides/instances/connect/ssh>. Vast offers two SSH flavours:

- **proxy SSH** — always works, goes through `sshN.vast.ai`, slower for bulk transfer.
- **direct SSH** — needs `--direct` at create time and an open port; faster and more reliable.

The connection string the CLI computes `[V-LOCAL]` (`cli/commands/misc.py:173-232`):

```
if ports["22/tcp"] exists:  root@{public_ipaddr}:{ports["22/tcp"][0].HostPort}
else:                       root@{ssh_host}:{ssh_port}
                            ... but +1 on ssh_port if "jupyter" in image_runtype  ← real quirk
```

That `ssh_port + 1` for jupyter runtypes is undocumented and easy to miss. We use ssh runtypes, so
it should not bite, but encode the rule anyway.

Then: `ssh -p <port> root@<host> -L 18080:localhost:18080` `[V-DOC]`. Traffic is encrypted, the
model server binds `127.0.0.1` inside the container, and nothing is exposed publicly.

**Password auth is disabled**; ED25519 key must be registered on the account *before* instance
creation `[V-DOC]`. `POST /api/v0/ssh/` (via `vastai create ssh-key`) and
`vastai attach ssh <id> "<pubkey>"` exist `[V-DOC]` (SKILL.md); exact REST paths for key management
are `[?]` — read `vastai/api/keys.py` and `auth.py` before implementing.

**Recommendation for mk1: (b) SSH tunnel, with (a) as an opt-in `--public` flag.** Rationale: no
TLS story is needed, no secret is exposed to the network, and `ssh` 10.2p1 is already on the box
and verified working. Cost: we must supervise a child `ssh` process (or embed russh — see §6).

#### (c) Instance Portal / Caddy / Cloudflare tunnel

`[V-DOC]` <https://docs.vast.ai/documentation/instances/connect/instance-portal>. Only present in
`vastai/*` base images. `PORTAL_CONFIG` format, verbatim:

```
localhost:1111:11111:/:Instance Portal|localhost:8080:18080:/:Jupyter|...
```

per app: `interface:external_port:internal_port:url_path:name`, apps separated by `|`.
Caddy reverse-proxies when external ≠ internal; when they are equal no proxying happens and secure
tunnel links are generated instead. Public HTTPS is achieved via **Cloudflare quick tunnels**
(`https://four-random-words.trycloudflare.com`) created during boot.

**Do not build on this for mk1.** It binds us to Vast's base images, the tunnel hostname is random
and only discoverable from inside the container, and the auth-token mechanism is undocumented
`[?]`. Note it exists so nobody reinvents it later.

### 1.10 Other Vast endpoints worth knowing

- `GET /api/v0/metrics/gpu/current/?verified=all&hosting_type=all` → market-wide GPU
  supply/demand/pricing snapshot; `GET /api/v0/metrics/gpu/history/` for time series
  `[V-LOCAL]` (`api/metrics.py`). Docstring says "available to admins and hosts" — may 403 for us
  `[?]`. Would be a nice input to offer scoring if it works.
- `GET /api/v0/template/?select_cols=["*"]&select_filters={...}` → templates `[V-LOCAL]`.
- `GET /api/v0/instances/filters/` → distinct filterable values for instances `[V-LOCAL]`.
- `PUT /api/v0/instances/update_template/{id}/` → change image/args/env/onstart in place
  `[V-LOCAL]`.

---

## 2. Together AI

### 2.1 Base URL and auth

```
base     https://api.together.ai/v1          [V-BOTH]
auth     Authorization: Bearer $TOGETHER_API_KEY
```

`[V-LOCAL]`: `~/.vastai-gguf/config.toml` already contains
`[providers.together] base_url = "https://api.together.ai/v1"`, so the migration path is to read
that value rather than hardcode. `api.together.xyz` is the historical host and still resolves
`[?]` — accept either in config, do not rewrite the user's URL.

`[V-DOC]` also mentions `https://api-inference.together.ai/v2` as an "optimized inference" host.
Unclear whether it is generally available or its request shape differs `[?]` — ignore for mk1.

Credential precedence per ground truth: explicit config → ApexRouter config → `~/.vastai-gguf/config.toml`
→ `$TOGETHER_API_KEY`.

### 2.2 `GET /v1/models`

`[V-DOC]` <https://docs.together.ai/reference/models-1>. **Returns a bare JSON array, not an
OpenAI-style `{"object":"list","data":[...]}` envelope.** This is the single most important
difference from every other OpenAI-shaped API we speak, and it will break a naive shared
deserializer.

```json
[
  {
    "id": "Austism/chronos-hermes-13b",
    "object": "model",
    "created": 1692896905,
    "type": "chat",
    "display_name": "Chronos Hermes (13B)",
    "organization": "Austism",
    "link": null,
    "license": null,
    "context_length": 2048,
    "pricing": {
      "base": 0,
      "finetune": 0,
      "hourly": 0,
      "input": 0.3,
      "output": 0.3,
      "cached_input": 0.2
    }
  }
]
```

`type` ∈ `chat | language | code | image | embedding | moderation | rerank` `[V-DOC]`. Filter to
`chat` (and maybe `language`) when presenting a model list to a router user.

**Pricing IS programmatically available** — it is the `pricing` object on each model, no separate
endpoint needed `[V-DOC]`. The **units are not stated in the reference** `[?]`. Given
`input: 0.3` for a 13B model, the near-certain unit is **USD per 1M tokens**. LocalRouter's
`usage.log` already records `cost_usd`, so:
- store the raw `pricing` object verbatim alongside the computed cost,
- record the assumed unit in the log line,
- and expose a config override. Do not silently bake ×10⁻⁶ into a constant with no note.

`context_length` is `Option<i64>` — absent for some model types `[V-DOC]`.

### 2.3 `POST /v1/chat/completions`

`[V-DOC]` <https://docs.together.ai/reference/chat-completions-1>. OpenAI-compatible.

Request: `model` and `messages` required. Optional: `max_tokens`, `stop` (array), `temperature`
(0–1), `top_p`, `top_k`, `repetition_penalty`, `stream`, `logprobs` (0–20), `tools`,
`response_format`.

Note `top_k` and `repetition_penalty` are **Together extensions** — not OpenAI, and not accepted by
every backend. Since ApexRouter proxies raw bodies, this is a non-issue: pass the client's JSON
through untouched. **Do not model the request body.** Only parse the response enough to extract
`usage` for the cost log, and re-emit the original bytes.

Response envelope is standard: `id`, `object: "chat.completion"`, `created`, `model`, `choices[]`
with `index`/`message`/`finish_reason`, and `usage {prompt_tokens, completion_tokens, total_tokens}`
`[V-DOC]`. `finish_reason` ∈ `stop | eos | length | tool_calls | function_call` — **`eos` is a
Together-specific value** not in the OpenAI enum `[V-DOC]`; deserialize `finish_reason` as a
`String`, never an enum.

Streaming: SSE, `ChatCompletionChunk` deltas, terminated by `data: [DONE]` `[V-DOC]`.

`[?]` Whether Together emits a final chunk carrying `usage` when `stream: true` (the OpenAI
`stream_options: {include_usage: true}` convention) is **not documented**. Our cost accounting must
therefore handle "streamed response, no usage reported" — fall back to counting chunks or mark the
log entry `tokens_estimated: true`. Do not let a missing `usage` block the proxy.

### 2.4 Rate limits

`[V-DOC]` <https://docs.together.ai/docs/serverless/rate-limits>:

- **Dynamic per-organization, per-model limits that scale with sustained traffic. No published
  RPM/TPM tiers.** There is nothing to precompute.
- **Successful responses carry no rate-limit headers.**
- On 429, the response includes **`x-ratelimit-reset`** = seconds to wait. That is the only header
  to read.
- The 429 body carries `error_type` ∈ `"dynamic_request_limited"` | `"dynamic_token_limited"`
  `[V-DOC]`. Full body shape is `[?]`.
- `x-ratelimit-remaining` / `x-ratelimit-limit` are **not** reliably returned `[V-DOC]` — do not
  build a budget display on them.

Implementation: on 429, read `x-ratelimit-reset` as f64 seconds; if absent or unparseable, fall
back to exponential backoff with jitter. Surface `error_type` in the error message so the user
knows whether they hit a request or token wall.

---

## 3. HuggingFace Hub API

Base: `https://huggingface.co`. Auth: `Authorization: Bearer hf_...` `[V-DOC]`.

**The canonical, always-current spec is the OpenAPI document**, not the prose docs — HF explicitly
redirected their API page to it `[V-DOC]`:

```
https://huggingface.co/.well-known/openapi.json     (machine-readable)
https://huggingface.co/.well-known/openapi.md       (markdown, agent-friendly)
```

Fetch that when a detail here is insufficient.

### 3.1 Token on disk

`[V-LOCAL]`, verified on this laptop:

```
~/.cache/huggingface/token            37 bytes, the plain token, no JSON wrapper
~/.cache/huggingface/stored_tokens    59 bytes, named-profile store
~/.cache/huggingface/hub/             the model blob cache (35 entries present)
~/.cache/huggingface/xet/             Xet dedup cache
```

Resolution order to implement: `$HF_TOKEN` → `$HUGGING_FACE_HUB_TOKEN` → `$HF_HOME/token` →
`~/.cache/huggingface/token`. `[?]` — the env-var names and `HF_HOME` override are the documented
`huggingface_hub` convention but I did not verify them from installed source (the library is **not
installed** on this box `[V-LOCAL]`). Treat the file path as verified and the env names as
best-effort; try all of them, use the first non-empty.

Always `.trim()`. Never log it. Never copy it into ApexRouter's own config file.

### 3.2 Model search — `GET /api/models`

`[V-DOC]` (docs + OpenAPI). Query params:

| Param | Type | Notes |
|---|---|---|
| `search` | string | substring over repo id and username |
| `author` | string | org/user filter |
| `filter` | string | tag filter (library, task, etc.); repeatable |
| `library` | string | e.g. `transformers`, `gguf` |
| `tags` | string[] | |
| `task` | string | pipeline tag |
| `sort` | string | e.g. `downloads`, `likes`, `createdAt`, `lastModified` |
| `direction` | int | `-1` descending, `1` ascending |
| `limit` | int | observed max **1000** per request `[?]` |
| `full` | bool | include full metadata |
| `cardData` | bool | include parsed model-card YAML |
| `gated` | bool | filter gated repos |
| `expand` | string[] | select specific properties |

Example that works: `GET /api/models?search=gguf&filter=gguf&sort=downloads&direction=-1&limit=50`.

**Pagination is via the HTTP `Link` header** (RFC 5988 `rel="next"`), not a JSON cursor `[?]` —
reported consistently by third-party sources but I could not confirm it in the OpenAPI extract.
Implement: follow `Link: <...>; rel="next"` if present; if absent, stop. Do not assume a
`next_token` field exists.

**For our purposes the useful query is GGUF-only**: `?filter=gguf` (or `library=gguf`) plus
`search=<model name>`. That is how we find quantised weights we can actually run.

### 3.3 Repo detail and file listing — three different endpoints

This is where implementations usually go wrong. There are three ways to learn what files a repo
contains, and they return different things.

**(a) `GET /api/models/{repo_id}`** — the model-info object, includes `siblings`.

`siblings` is an array of `RepoSibling` `[V-DOC]`:
```json
{ "rfilename": "model-Q4_K_M.gguf", "size": 4912898304, "blob_id": "…", "lfs": { "size": …, "sha256": "…", "pointer_size": … } }
```

**Critical caveat `[V-DOC]`: by default `siblings` contains only `rfilename` — sizes are absent.**
You must ask for them. The `huggingface_hub` client exposes this as `files_metadata=True`, which
maps to a query param (`blobs=true` in older docs, `files_metadata` in the client)
`[CONFLICT]`/`[?]` — the two sources disagree on the wire name.
**Defend:** request `GET /api/models/{repo}?blobs=true`; if `siblings[].size` comes back `null`,
fall back to (c) `paths-info`, which is unambiguous.

Other useful model-info fields: `gated` (bool or string), `private`, `downloads`, `likes`, `tags`,
`cardData`, `sha` (the resolved commit) `[V-DOC]`.

**(b) `GET /api/models/{namespace}/{repo}/tree/{rev}/{path}`** — "List folder content" `[V-DOC]`,
confirmed present in the OpenAPI spec. Query params (both documented as **strings**, not booleans —
send `"true"`/`"1"` `[V-DOC]`):

- `recursive` — walk subdirectories
- `expand` — include commit data and security-scanner metadata

Returns an array of entries. Good for browsing `main` without pulling the whole model-info blob.

**(c) `POST /api/models/{namespace}/{repo}/paths-info/{rev}`** — "List paths info" `[V-DOC]`,
confirmed in the OpenAPI spec. Body:

```json
{ "paths": ["model-Q4_K_M.gguf", "mmproj-F16.gguf"], "expand": false }
```

`paths` accepts a string or an array, **max 2000 items** `[V-DOC]`. Returns per-path `path`, `size`,
`blob_id`, and `lfs` metadata `[V-DOC]`. **This is the reliable way to get file sizes** and should
be the primary sizing call — we need sizes to decide whether a quant fits in 22 GiB before
downloading gigabytes.

### 3.4 Gated repos

`[V-DOC]` behaviour:

- Access must be requested and approved on the repo page; a valid token alone is not enough.
- Without access, the API returns **403** (sometimes **401**, e.g. when the token lacks the right
  permission scope) and clients surface it as `GatedRepoError`.
- The `huggingface_hub` helper `auth_check(repo_id)` exists purely to test access without
  downloading — the equivalent for us is a `HEAD`/`GET` on the resolve URL.

`[?]` HF is widely reported to send `X-Error-Code: GatedRepo` / `RepoNotFound` /
`RevisionNotFound` and `X-Error-Message` headers. **I could not verify this today.**
**Defend:** classify on `(status, X-Error-Code if present, body text)` in that priority, and always
produce an actionable message: *"repo X is gated — request access at
https://huggingface.co/X and ensure your token has read permission"*. Never report a gated repo as
"not found"; that sends the user hunting for a typo.

`[?]` A 401 on a *public* repo means the token itself is bad or expired — distinguish it from 401
on a gated repo by retrying once **without** the Authorization header. If the anonymous request
succeeds, our token is the problem.

### 3.5 Identity and rate limits

- **`GET /api/whoami-v2`** → auth info for the current token `[V-DOC]`. Cheapest possible
  credential validity check — call it once at startup when `--check` is passed, not on every run.
- All Hub API calls are subject to **HF-wide rate limits** `[V-DOC]`
  (<https://huggingface.co/docs/hub/rate-limits>); numbers not extracted here `[?]`. Assume they
  exist, back off on 429.

### 3.6 Download URL

`[?]` Not verified today, but the long-standing stable form is:

```
https://huggingface.co/{repo_id}/resolve/{revision}/{filename}
```

It 302-redirects to a CDN (and now often to a Xet endpoint). **Must follow redirects, and must
carry the Authorization header only to `huggingface.co`** — reqwest's default redirect policy
strips sensitive headers on cross-host redirects, which is the behaviour we want; verify it is
still the default in whichever reqwest version we pin.

If we ever want the download path handled for us, `hf-hub` 1.0.0 exists — see §6.

---

## 4. llama.cpp `llama-server`

**Everything in §4 marked `[V-LOCAL]` was verified today by running the binary at
`/home/andre/llama.cpp/build-vulkan/bin/llama-server` and by reading
`/home/andre/llama.cpp/tools/server/README.md` (repo HEAD `94a220cd`, 2026-06-03).**

Binary version `[V-LOCAL]`: `version: 9199 (39cf5d619)`, built with GNU 15.2.0 for Linux x86_64.

> **Version skew warning.** The checked-out README (June 3) is *newer* than the built binary
> (b9199, built May 17). Where the two could disagree I re-checked against `--help` and say so.
> The general rule from `00-machine-ground-truth.md` stands: **feature-detect by grepping
> `--help`** before emitting any flag.

### 4.1 The headline finding: llama-server has its own router now

`[V-LOCAL]` — **and the flags are present in the installed b9199 binary**, not just the newer
README:

```
--models-dir PATH        directory containing models for the router server (default: disabled)
--models-preset PATH     path to INI file containing model presets for the router server
--models-max N           for router server, maximum number of models to load simultaneously
--models-autoload, --no-models-autoload
--sleep-idle-seconds SECONDS
```

Launching `llama-server` **with no `-m`** starts it in **router mode** `[V-DOC]` (README §"Using
multiple models"). In that mode it:

- lists everything in `LLAMA_CACHE` / `--models-dir` / `--models-preset` via `GET /models`
- routes `POST` requests by the `"model"` field in the JSON body
- routes `GET` requests by a `?model=<url-encoded-id>` query param
- **auto-loads a model on first request** (disable with `--no-models-autoload`, or per-request
  `?autoload=true|false`)
- exposes `POST /models/load` and `POST /models/unload`, both `{"model": "..."} → {"success": true}`
- supports `--sleep-idle-seconds` to unload the model and its KV cache after inactivity, reloading
  on the next request; `GET /health`, `GET /props`, `GET /models` are **exempt** from resetting the
  idle timer

`GET /models` status object `[V-DOC]`:
```json
{ "value": "loaded" | "loading" | "unloaded" | "sleeping", "args": ["llama-server", "-ctx", "4096"], "failed": true, "exit_code": 1 }
```
(`failed`/`exit_code` only on the `unloaded` variant.)

**This overlaps significantly with what ApexRouter does locally.** Before building process
supervision and model swapping from scratch, decide deliberately: supervise N single-model servers
ourselves (full control, matches LocalRouter's existing `local_instances/*.json` state), or drive
one router-mode server (less code, but its lifecycle, sleep, and failure semantics are now ours to
babysit and are only ~2 months old upstream). **My recommendation: mk1 keeps direct supervision of
single-model servers** — it is the thing the existing on-disk state already describes and the
failure modes are understood — **but the port spec should note router mode as the mk2 simplification.**

Directory layout router mode expects `[V-DOC]`: flat `.gguf` files, or a subdirectory per model for
multimodal (`mmproj*.gguf` inside) and multi-shard (`*-00001-of-000NN.gguf`). Note this is
**exactly the layout `~/models` already uses** (per-model folders) — see
`00-machine-ground-truth.md`.

### 4.2 Endpoints

| Endpoint | Notes |
|---|---|
| `GET /health` | `[V-DOC]` **Public — no API-key check.** `/v1/health` also works. 200 `{"status":"ok"}`; **503** `{"error":{"code":503,"message":"Loading model","type":"unavailable_error"}}` while loading. This is the readiness probe. |
| `GET /props` | server global properties, read-only by default |
| `POST /props` | requires `--props` (**disabled by default** `[V-LOCAL]`); "Options: None yet" |
| `GET /slots` | **enabled by default** in b9199 `[V-LOCAL]`; disable with `--no-slots`. `?fail_on_no_slot=1` → 503 when saturated |
| `GET /metrics` | requires `--metrics` (**disabled by default** `[V-LOCAL]`); Prometheus text format. **In router mode `?model=` is REQUIRED or you get 400 `model name is missing from the request`** `[V-DOC]` |
| `GET /v1/models` | OpenAI-shaped, **always exactly one element** in single-model mode |
| `POST /completion` | native, not OAI-compatible |
| `POST /v1/completions`, `POST /v1/chat/completions` | OAI-compatible |
| `POST /v1/responses` | OpenAI Responses API `[V-DOC]` |
| `POST /v1/messages`, `POST /v1/messages/count_tokens` | **Anthropic-compatible Messages API** `[V-DOC]` |
| `POST /tokenize`, `/detokenize`, `/apply-template`, `/infill`, `/embedding`, `/v1/embeddings`, `/embeddings`, `/reranking` | utility |
| `GET`/`POST /lora-adapters` | LoRA |
| `POST /slots/{id}?action=save\|restore\|erase` | prompt-cache management, needs `--slot-save-path` |
| `POST /v1/chat/completions/control` | control a running completion in real time `[V-DOC]` |
| `GET /models`, `POST /models/load`, `POST /models/unload` | router mode only |

Errors are **OpenAI-shaped** `[V-DOC]`:
```json
{ "error": { "code": 401, "message": "Invalid API Key", "type": "authentication_error" } }
```

### 4.3 `GET /v1/models` response

`[V-DOC]`:

```json
{
  "object": "list",
  "data": [{
    "id": "../models/Meta-Llama-3.1-8B-Instruct-Q4_K_M.gguf",
    "object": "model",
    "created": 1735142223,
    "owned_by": "llamacpp",
    "meta": { "vocab_type": 2, "n_vocab": 128256, "n_ctx_train": 131072, "n_embd": 4096, "n_params": 8030261312, "size": 4912898304 }
  }]
}
```

- **`id` defaults to the model file path** unless `-a/--alias` is set. ApexRouter should always pass
  `--alias` so the id is a stable logical name and not a filesystem path that leaks into client
  configs.
- **`meta` can be `null`** while the model is loading `[V-DOC]`. `Option<Meta>`, always.
- Note the **inconsistency with Together**: llama.cpp uses the `{"object":"list","data":[...]}`
  envelope; Together returns a bare array (§2.2). One shared deserializer will not work.

### 4.4 `GET /props` response

`[V-DOC]`, abridged — the fields we care about:

```json
{
  "default_generation_settings": { "id": 0, "n_ctx": 1024, "params": { ...full sampler config... } },
  "total_slots": 1,
  "model_path": "../models/Meta-Llama-3.1-8B-Instruct-Q4_K_M.gguf",
  "chat_template": "...",
  "chat_template_caps": {},
  "modalities": { "vision": false },
  "media_marker": "<__media_...__>",
  "build_info": "b(build number)-(build commit hash)",
  "is_sleeping": false
}
```

- `model_path` is the real file path (`/v1/models` gives the alias) — use `/props` to answer "what
  is actually loaded".
- `total_slots` = the effective `--parallel` value. **This is how we discover concurrency** without
  parsing our own command line back.
- `build_info` gives us the server's build for telemetry / bug reports.
- `is_sleeping` — see `--sleep-idle-seconds`.
- `default_generation_settings.n_ctx` is the **per-slot** context, i.e. total ctx ÷ slots.

### 4.5 `GET /slots` response

`[V-DOC]` — a **bare JSON array** of slot objects (no envelope). Per slot: `id`, `id_task`,
`n_ctx`, `speculative`, `is_processing`, `params` (the full sampler config incl. `chat_format`,
`reasoning_format`, `samplers[]`, `lora[]`), `next_token {has_next_token, has_new_line, n_remain,
n_decoded, stopping_word}`, and `prompt`.

**Privacy note:** `/slots` echoes the `prompt`. If ApexRouter ever proxies `/slots` outward, redact
it. `--no-slots` exists for exactly this reason.

Liveness heuristic: `slots.iter().all(|s| !s.is_processing)` ⇒ idle. Or use
`?fail_on_no_slot=1` and treat 503 as "busy" for load-balancing across local instances.

### 4.6 `GET /metrics` (Prometheus)

`[V-DOC]`, requires `--metrics`:

```
llamacpp:prompt_tokens_total              Counter
llamacpp:prompt_seconds_total             Counter
llamacpp:prompt_tokens_seconds            Gauge   avg prompt throughput tok/s
llamacpp:tokens_predicted_total           Counter
llamacpp:tokens_predicted_seconds_total   Counter
llamacpp:predicted_tokens_seconds         Gauge   avg generation throughput tok/s
llamacpp:requests_processing              Gauge
llamacpp:requests_deferred                Gauge
llamacpp:n_tokens_max                     Counter  high-watermark ctx observed
llamacpp:n_decode_total                   Counter
llamacpp:n_busy_slots_per_decode          Gauge
```

Router mode: **`?model=` is mandatory**, else 400.

### 4.7 The `timings` object — our tok/s telemetry

`[V-DOC]`, verbatim from `README.md:1307-1331`:

```jsonc
{
  "timings": {
    "cache_n": 236,                          // prompt tokens reused from cache
    "prompt_n": 1,                           // prompt tokens actually processed
    "prompt_ms": 30.958,
    "prompt_per_token_ms": 30.958,
    "prompt_per_second": 32.301828283480845,
    "predicted_n": 35,                       // generated tokens
    "predicted_ms": 661.064,
    "predicted_per_token_ms": 18.887542857142858,
    "predicted_per_second": 52.94494935437416
  }
}
```

Two rules straight from the README `[V-DOC]`:

1. **`timings` appears on `/v1/chat/completions` responses too**, alongside the standard `usage`
   object — it is not restricted to the native `/completion` endpoint.
2. **Total tokens in context = `prompt_n + cache_n + predicted_n`.** That is the supported way to
   compute live context usage. Use it for the "you are at 78% of ctx" display rather than
   re-tokenizing.

For per-token streaming telemetry set **`"timings_per_token": true`** in the request
`[V-DOC]` (README:522) — each streamed chunk then carries timing info. Default `false`.
Only enable when the user asked for live tok/s; it inflates every chunk.

Beware collisions: `/slots/{id}?action=save|restore` responses also contain a `timings` object, but
with completely different keys (`{"save_ms": 49.865}` / `{"restore_ms": 42.937}`) `[V-DOC]`.
Do not share one `Timings` struct across both.

`predicted_per_second` is the number to record in the benchmark log. Ground truth for this laptop
(memory + `00-machine-ground-truth.md`): ~4 tok/s on a 27B; expect single digits.

### 4.8 CLI flags — verified against the installed b9199 binary

All rows `[V-LOCAL]`, from `llama-server --help` run today.

| Flag | Verified reality |
|---|---|
| `-m, --model FNAME` | model path |
| `-c, --ctx-size N` | **default `0` = take from model** |
| `-ngl, --gpu-layers, --n-gpu-layers N` | max layers in VRAM; exact number or non-numeric form |
| `-fa, --flash-attn [on\|off\|auto]` | **not a boolean; default `auto`** |
| `-ctk, --cache-type-k TYPE` | allowed: `f32, f16, bf16, q8_0, q4_0, q4_1, iq4_nl, q5_0, q5_1` — **default `f16`**. `iq4_nl` is in this build but absent from the upstream README's list |
| `-ctv, --cache-type-v TYPE` | same set |
| `-np, --parallel N` | **default `-1` = auto**, not 1 |
| `-cb, --cont-batching` / `-nocb, --no-cont-batching` | on by default |
| `--jinja` / `--no-jinja` | **default ENABLED** — passing `--jinja` is a no-op; the flag to remember is `--no-jinja` |
| `--chat-template JINJA_TEMPLATE` | only common named templates unless `--jinja` |
| `--chat-template-file PATH` | |
| `--chat-template-kwargs STRING` | JSON extra params for the template parser |
| `--host HOST` | also accepts a path ending appropriately to **bind a UNIX socket** |
| `--port PORT` | default 8080 |
| `--api-key KEY` | multiple keys may be supplied |
| `--api-key-file FNAME` | prefer this — keeps the secret out of `/proc/*/cmdline` |
| `-mm, --mmproj FILE` / `-mmu, --mmproj-url URL` | multimodal projector; `--no-mmproj`, `--mmproj-auto`, `--mmproj-offload` also exist |
| `-a, --alias STRING` | **comma-separated** aliases surfaced through the API |
| `--slots` / `--no-slots` | **enabled by default** |
| `--props` | **disabled by default** |
| `--metrics` | **disabled by default** |
| `--ui` / `--no-ui` | Web UI, enabled by default. `--webui/--no-webui` is DEPRECATED |
| `-t, --threads N` | default `-1` |
| `--threads-http N` | default `-1` |
| `-b, --batch-size N` | default 2048 |
| `-ub, --ubatch-size N` | default 512 |
| `-n, --predict N` | default -1 |
| `--keep N` | default 0 |
| `-to, --timeout N` | **server read/write timeout, default 600 s** — matters for long generations behind our proxy |
| `--context-shift` / `--no-context-shift` | |
| `--cache-reuse N` | min chunk size for KV-shift reuse |
| `--slot-save-path PATH` | disabled by default |
| `-dev, --device <dev1,dev2>` | e.g. `Vulkan0` — **required here to avoid llvmpipe** |
| `-sm, --split-mode {none,layer,row,tensor}` | |
| `-hf, -hfr, --hf-repo <user>/<model>[:quant]` | server can download from HF itself |
| `-hft, --hf-token TOKEN` | **defaults to `$HF_TOKEN`** |
| `-mu, --model-url URL` | |
| `--reasoning-format FORMAT`, `-rea/--reasoning [on\|off\|auto]`, `--reasoning-budget N`, `--reasoning-budget-message MSG` | reasoning control |
| `--models-dir`, `--models-preset`, `--models-max`, `--models-autoload/--no-models-autoload`, `--sleep-idle-seconds` | router mode (§4.1) |
| `--swa-full`, `-ctxcp/--ctx-checkpoints` | SWA cache |
| `-dt, --defrag-thold` | **DEPRECATED** |

Most flags also have an `LLAMA_ARG_*` env var (e.g. `LLAMA_ARG_CACHE_TYPE_K`) `[V-LOCAL]`.
Env vars are a cleaner way to pass secrets than argv.

**`--help` is 635 lines.** Do not hardcode a flag whitelist; grep `--help` at startup and warn
(don't fail) when a configured flag is missing from the user's build.

---

## 5. MCP (Model Context Protocol)

### 5.1 The situation as of 2026-07-30 — read this before writing any MCP code

**The current spec revision is `2026-07-28`. It was published two days ago and it is the largest
breaking change in MCP's history.** `[V-DOC]`
<https://modelcontextprotocol.io/specification/2026-07-28/changelog>

What it removes:

1. **`initialize` / `notifications/initialized` are gone.** The protocol is now stateless. Every
   request carries its version, client identity, and capabilities in `_meta`.
2. **Sessions are gone** — no `Mcp-Session-Id` header.
3. **`ping`, `logging/setLevel`, `notifications/roots/list_changed` removed.**
4. **The HTTP GET stream endpoint is gone**; `resources/subscribe`/`unsubscribe` replaced by a
   single `subscriptions/listen` long-poll.
5. **SSE resumability (`Last-Event-ID`) removed.**
6. **Roots, Sampling, and Logging are deprecated** (12-month window).
7. Server-initiated requests replaced by **MRTR**: the server returns
   `resultType: "input_required"` and the client *retries the original request* with
   `inputResponses`.
8. **Every result now carries a required `resultType`** field (`"complete"` | `"input_required"`).
   Clients MUST treat a missing `resultType` from an older server as `"complete"`.
9. `server/discover` is **mandatory** for servers.

Version history, for the `supportedVersions` list: `2024-11-05`, `2025-03-26`, `2025-06-18`,
`2025-11-25`, `2026-07-28` `[V-DOC]`.

**The practical problem:** almost nothing in the wild speaks `2026-07-28` yet. The proof is in this
very garden — `Prefrontal-RS`, Andre's own working MCP server that Claude Code uses daily, pins
`PROTOCOL_VERSION = "2024-11-05"` `[V-LOCAL]`
(`/home/andre/Projects/Prefrontal-RS/prefrontal-cli/src/mcp.rs:18`).

### 5.2 The house pattern — copy it

`[V-LOCAL]` `Prefrontal-RS/prefrontal-cli/src/mcp.rs` is 282 lines, hand-rolled, no MCP crate, and
it works with Claude Code today. Its design:

- newline-delimited JSON-RPC 2.0 over stdin/stdout, `serde_json::Value` throughout
- **echoes the client's requested `protocolVersion` straight back** rather than asserting one:
  ```rust
  let requested = params.get("protocolVersion").and_then(|v| v.as_str()).unwrap_or(PROTOCOL_VERSION);
  json!({ "protocolVersion": requested, "capabilities": { "tools": {} }, "serverInfo": {...} })
  ```
  This one trick makes the server compatible with every legacy revision at once. **Do this.**
- silently ignores notifications (`msg["id"]` absent ⇒ no reply)
- handles exactly four methods: `initialize`, `ping`, `tools/list`, `tools/call`
- **tool failures are results with `isError: true`, never JSON-RPC errors** — JSON-RPC errors are
  reserved for protocol breakage. The file even carries a comment saying so. Match that discipline.

**Recommendation for ApexRouter-RS: be dual-era, legacy-first.**
Implement the legacy handshake exactly as Prefrontal does (it is ~40 lines), *and* add
`server/discover` returning `supportedVersions: ["2026-07-28", "2025-11-25", "2024-11-05"]` so a
modern client probing over stdio gets a deterministic answer instead of a `-32601`. Accept and
ignore `_meta` on every request. Emit `resultType: "complete"` on results — legacy clients ignore
unknown fields, modern clients require it. That combination costs almost nothing and satisfies both
eras.

### 5.3 Legacy wire shapes (what clients actually send today)

`initialize` — request/response, as implemented and working `[V-LOCAL]`:

```jsonc
// →
{"jsonrpc":"2.0","id":0,"method":"initialize","params":{
  "protocolVersion":"2024-11-05",
  "capabilities":{},
  "clientInfo":{"name":"claude-code","version":"..."}}}
// ←
{"jsonrpc":"2.0","id":0,"result":{
  "protocolVersion":"2024-11-05",
  "capabilities":{"tools":{}},
  "serverInfo":{"name":"apexrouter","version":"0.1.0"}}}
// → (notification, no reply)
{"jsonrpc":"2.0","method":"notifications/initialized"}
```

`tools/list` `[V-LOCAL]`: `{"jsonrpc":"2.0","id":1,"method":"tools/list"}` →
`{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":…,"description":…,"inputSchema":{…}}]}}`

`tools/call` `[V-LOCAL]`:
```jsonc
// →
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_models","arguments":{}}}
// ←
{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"…"}],"isError":false}}
```

### 5.4 Modern (2026-07-28) wire shapes

`[V-DOC]`, verbatim from the spec.

`server/discover` request/response:

```json
{ "jsonrpc": "2.0", "id": "discover-1", "method": "server/discover",
  "params": { "_meta": {
    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
    "io.modelcontextprotocol/clientInfo": { "name": "ExampleClient", "version": "1.0.0" },
    "io.modelcontextprotocol/clientCapabilities": {} } } }
```
```json
{ "jsonrpc": "2.0", "id": "discover-1", "result": {
    "resultType": "complete",
    "supportedVersions": ["2026-07-28"],
    "capabilities": { "tools": {}, "resources": {} },
    "_meta": { "io.modelcontextprotocol/serverInfo": { "name": "ExampleServer", "version": "1.0.0" } },
    "instructions": "This server provides weather and resource utilities.",
    "ttlMs": 3600000, "cacheScope": "public" } }
```

`tools/list` result now requires `resultType`, `ttlMs`, `cacheScope`:

```json
{ "jsonrpc": "2.0", "id": 1, "result": {
    "resultType": "complete",
    "tools": [ { "name": "get_weather", "title": "Weather Information Provider",
                 "description": "…", "inputSchema": { "type":"object", "properties": {…}, "required":["location"] } } ],
    "nextCursor": "next-page-cursor", "ttlMs": 300000, "cacheScope": "public" } }
```

`tools/call` result:

```json
{ "jsonrpc": "2.0", "id": 2, "result": {
    "resultType": "complete",
    "content": [ { "type": "text", "text": "…" } ],
    "isError": false } }
```

Other modern requirements worth knowing `[V-DOC]`:
- Tools **SHOULD** be returned in a deterministic order (client caching + LLM prompt-cache hits).
  Sort our tool list; it is free.
- Tool names: 1–128 chars, `[A-Za-z0-9_.-]`, case-sensitive.
- No-parameter tools should use `{"type":"object","additionalProperties":false}`.
- Error codes: `-32020` HeaderMismatch, `-32021` MissingRequiredClientCapability,
  `-32022` UnsupportedProtocolVersion. Resource-not-found moved from `-32002` to `-32602`.
- Version mismatch response:
  ```json
  {"jsonrpc":"2.0","id":1,"error":{"code":-32022,"message":"Unsupported protocol version",
    "data":{"supported":["2026-07-28","2025-11-25"],"requested":"1900-01-01"}}}
  ```

### 5.5 stdio transport rules

`[V-DOC]`, the MUSTs:

- One JSON-RPC message per line. Messages **MUST NOT** contain embedded newlines.
  → serialize with `serde_json::to_string` (compact), never `to_string_pretty`.
- The server **MUST NOT** write anything to stdout that is not a valid MCP message.
  → **every log line goes to stderr.** Configure `tracing_subscriber` with
  `.with_writer(std::io::stderr)` before anything else, and make sure no dependency `println!`s.
- stderr is free-form UTF-8; clients may ignore it and **SHOULD NOT** treat output there as errors.
- Servers **SHOULD** exit promptly on stdin EOF. Implement it — it is the only portable shutdown
  signal.
- The client **MUST NOT** send JSON-RPC responses; the server **MUST NOT** send JSON-RPC requests.
- Same framing works over UNIX sockets / TCP unchanged (relevant if we want ApexRouter's MCP face
  reachable without a subprocess).

### 5.6 Is streamable-HTTP worth offering?

**Not for mk1. `[V-DOC]`-informed judgement.**

Arguments against:
- The transport just changed shape *again* (GET endpoint removed, sessions removed, resumability
  removed). Anything we build today against `2026-07-28` will not match the clients that exist, and
  anything we build against `2025-11-25` is already superseded.
- Modern streamable-HTTP demands header/body mirroring validation: `MCP-Protocol-Version`,
  `Mcp-Method`, `Mcp-Name` are **required** on every POST, must match the body exactly, with a
  base64 sentinel encoding (`=?base64?…?=`) for non-ASCII values, plus `x-mcp-header` parameter
  mirroring into `Mcp-Param-{Name}`. Mismatch ⇒ 400 + `-32020`. That is a real chunk of work with
  no user today.
- Origin validation is mandatory (403 on bad Origin), and localhost binding is a SHOULD.
- HTTP+SSE (2024-11-05) is formally Deprecated — do not implement it at all.

Argument for, later: ApexRouter is a **network** service. Once it serves ApexOS-RS/RV nodes, an
HTTP MCP face lets remote agents use it without spawning a subprocess. Design the tool dispatch
layer transport-agnostic (`fn dispatch(method, params) -> Result<Value, RpcError>`, exactly as
Prefrontal does) so adding an axum route later is a day's work, not a rewrite.

If/when we do it: single POST endpoint, `Accept: application/json, text/event-stream`, respond with
either a single JSON object or an SSE stream per request, `202 Accepted` for notifications,
`405` for GET/DELETE, ignore `Mcp-Session-Id` and `Last-Event-ID`, and send
**`X-Accel-Buffering: no`** on SSE responses `[V-DOC]`.

---

## 6. Rust crates

Versions from crates.io on 2026-07-30 `[V-LOCAL]` (queried the registry API directly).

### 6.1 Core

| Crate | Version | Verdict |
|---|---|---|
| `tokio` | 1.x | given |
| `axum` | **0.8.9** (2026-04-14) | Prefrontal-RS's `prefrontald` already uses `axum = "0.8"` `[V-LOCAL]` — match it for cross-project familiarity. Built-in `axum::response::sse::Sse` handles our streaming proxy without a third-party crate. |
| `tower-http` | **0.7.0** (2026-06-15) | Prefrontal uses `0.7.0` with `fs`. Add `trace`, `cors`, `timeout`. |
| `reqwest` | **0.13.4** (2026-05-25) | See the trap below. |
| `rustls` | 0.23.43 | use `rustls-tls` on reqwest; avoids an OpenSSL build dep |
| `serde` / `serde_json` | 1 | given |
| `futures-util` | 0.3.33 | stream combinators for SSE relay |

### 6.2 The reqwest 0.13 trap — decide this early

`[V-LOCAL]`, from the crates.io dependency API:

- `hf-hub` **1.0.0** requires `reqwest ^0.13`.
- `reqwest-eventsource` **0.6.0** (last released 2024-03-29) requires `reqwest ^0.12`.

**They cannot coexist.** Pick one:

- **Take `reqwest 0.13`** (recommended) and **do not use `reqwest-eventsource`**. Parse SSE with
  `eventsource-stream` 0.2.3 applied to `response.bytes_stream()` — that crate is
  reqwest-independent (it operates on any `Stream<Item = Result<Bytes, E>>`), which is why its 2022
  release date is not a problem.
- Or stay on `reqwest 0.12.28` and give up `hf-hub` 1.0.

`[?]` reqwest 0.13 is a major bump from 0.12; I did not audit its changelog. Feature-flag names,
the default TLS backend, and the redirect/`blocking` surface may have moved. **Verify
`redirect::Policy` still strips the `Authorization` header on cross-host redirects** before relying
on it for HF downloads (§3.6).

Honestly, for our volume the simplest answer may be to skip `hf-hub` entirely — we make maybe six
distinct HF calls, all documented in §3, and a hand-rolled client keeps the reqwest version free.

### 6.3 SSE — for proxying streamed completions

- **Inbound (we are the client, calling Together / a remote llama-server):**
  `reqwest::Response::bytes_stream()` → `eventsource-stream` 0.2.3 `[V-LOCAL]` → yields
  `Event { event, data, id, retry }`. Watch for the `data: [DONE]` sentinel.
- **Outbound (we are the server):** `axum::response::sse::Sse` + `KeepAlive`. No extra crate.
- **The lazy option that is often correct:** for a pure pass-through proxy, **don't parse SSE at
  all** — relay `bytes_stream()` straight into an axum `Body::from_stream`. Only tee a parsed copy
  when we need the `usage`/`timings` for the cost log. Byte-for-byte relay also means we cannot
  corrupt a provider's framing.
- `async-sse` 5.1.0 (2021) — dead, async-std ecosystem. Do not use.

### 6.4 SSH — `russh` vs shelling out

`russh` **0.62.4** (2026-07-22) `[V-LOCAL]` — actively maintained, pure Rust, client and server.

**Recommendation: shell out to `ssh` for mk1.**

- `OpenSSH_10.2p1` is verified present and working on this box `[V-LOCAL]` (ground-truth doc).
- `ssh -N -L 18080:localhost:18080 -p PORT root@HOST` is one `tokio::process::Command`, and it
  inherits the user's `~/.ssh/config`, agent, known_hosts, and jump-host settings **for free**. That
  is a lot of behaviour to reimplement.
- Failure modes are legible in the process's stderr.
- Downside: we must supervise the child, detect tunnel death (the process exits, or the local port
  stops accepting), and restart with backoff. Add `-o ServerAliveInterval=30
  -o ServerAliveCountMax=3 -o ExitOnForwardFailure=yes` — that last one is essential, otherwise
  `ssh` happily stays up with a dead forward.
- Host-key policy: Vast reuses `sshN.vast.ai` hostnames across machines, so `known_hosts` conflicts
  are routine. Use a **dedicated** `-o UserKnownHostsFile=<our state dir>/known_hosts
  -o StrictHostKeyChecking=accept-new` rather than teaching users to edit their real known_hosts.

Revisit `russh` if we ever need in-process port forwarding without a child process (e.g. a
single static binary for ApexOS-RV nodes). It is the right crate when that day comes.

### 6.5 Deliberately not used

| Crate | Why not |
|---|---|
| `async-openai` 0.41.1, `openai-api-rs` 10.0.1 | We **proxy raw bodies**. Typing the OpenAI schema would force us to re-serialize and silently drop provider extensions (`top_k`, `repetition_penalty`, `timings_per_token`, llama.cpp's reasoning fields). Pass bytes through; parse only `usage`/`timings` opportunistically. |
| any MCP SDK crate | No `rmcp` anywhere in the garden `[V-LOCAL]`; Prefrontal-RS hand-rolls it in 282 lines and it works. The spec just broke compatibility — a crate pinned to one revision is a liability right now. |
| `vastai` / `vast-ai` | **Do not exist** `[V-LOCAL]`. |
| `tokio-tungstenite` 0.30.0 | No WebSocket contract in scope. Note `axum 0.8` has `features = ["ws"]` if a future dashboard wants live push — and Prefrontal-RS already enables it, so there is a house precedent to copy. |
| `async-sse` | Dead since 2021. |

### 6.6 Possibly useful

| Crate | Version | Use |
|---|---|---|
| `hf-hub` | 1.0.0 (2026-07-10) | official-ish HF client incl. Xet; pulls in reqwest 0.13, hyper 1, tokio-retry, sha2. Heavy for six endpoints — weigh against §6.2. |
| `gguf-rs` | 0.1.8 (2026-06-15) | parse GGUF headers to read `n_ctx_train`, arch, quant, and param count **without loading the model**. Directly useful for "will this fit in 22 GiB". Small crate, verify it handles multi-shard. |
| `sysinfo` | 0.39.6 | RAM/swap headroom checks before launching a local server — this laptop needs them (`free -h` habit, per CLAUDE.md). |
| `secrecy` | 0.10.3 | `SecretString` for the four API keys; makes accidental `Debug`-logging a compile error. Cheap insurance given we handle real money-spending credentials. |
| `keyring` | 4.1.5 | OS keychain. Probably overkill — the existing convention is plaintext files, and matching it keeps migration honest. |

---

## 7. Defensive checklist for the implementation

Distilled from every `[CONFLICT]` and `[?]` above. Each of these is a real, observed hazard.

1. **`ports` on a Vast instance may be a map-of-arrays or an array of ints.** One tolerant accessor
   function; never index blindly. (§1.6)
2. **The instance id is `new_contract`.** (§1.5)
3. **`type: "on-demand"` vs `"ondemand"`** — send the hyphenated form, retry once with the other on
   an empty result set. (§1.4)
4. **`runtype` is `ssh_direc ssh_proxy`** (no `t`), space-separated tokens, not an enum. (§1.5)
5. **Pre-multiply `gpu_ram`/`cpu_ram` by 1000 and `duration` by 86400** in offer queries. (§1.4)
6. **`actual_status` of `exited`/`unknown`/`offline` is terminal** — destroy and bail, never keep
   polling. Timeout every poll loop. Storage bills from creation. (§1.6)
7. **`result_url` for logs/exec is a second, unauthenticated fetch that is not immediately ready.**
   403/404 on the first try is normal. Cap the poll. (§1.8)
8. **Destroy-on-exit must be guaranteed**, including after a panic — persist an orphan list and
   reconcile at startup. (§1.7)
9. **Together `/v1/models` returns a bare array**; llama.cpp `/v1/models` returns
   `{"object":"list","data":[…]}`. Two deserializers. (§2.2, §4.3)
10. **Together `finish_reason` can be `eos`** — never an enum. (§2.3)
11. **Together pricing units are undocumented** — store raw, note the assumption, allow override.
    (§2.2)
12. **Streamed responses may carry no `usage`** — cost logging must degrade, not fail. (§2.3)
13. **HF `siblings` has no sizes by default** — use `paths-info` for authoritative sizes. (§3.3)
14. **Gated HF repos are 403 (sometimes 401)** — never report them as "not found"; emit the
    request-access URL. Retry anonymously to distinguish a bad token. (§3.4)
15. **llama.cpp `--jinja` is already on by default; `-fa` is tri-state; `-np` defaults to `-1`.**
    Feature-detect flags from `--help`. (§4.8)
16. **`/props`, `/metrics` are off by default; `/slots` is on.** Pass `--metrics`/`--props`
    explicitly if we intend to scrape them. (§4.2)
17. **In llama.cpp router mode, `GET /metrics` and `GET /props` require `?model=`.** (§4.2, §4.1)
18. **`/v1/models[].meta` can be null while loading.** (§4.3)
19. **`/slots` leaks prompts** — never proxy it outward unredacted. (§4.5)
20. **Set `--alias`** so model ids aren't filesystem paths. (§4.3)
21. **MCP: every log line to stderr, compact JSON, one line per message, exit on stdin EOF.** (§5.5)
22. **MCP: echo the client's requested `protocolVersion`** instead of asserting one. (§5.2)
23. **MCP: tool failures are `isError: true` results, not JSON-RPC errors.** (§5.2)
24. **`reqwest 0.13` and `reqwest-eventsource 0.6` are mutually exclusive.** (§6.2)
25. **`ssh -o ExitOnForwardFailure=yes`** or the tunnel dies silently while the process lives.
    (§6.4)
26. **Never log any of the four credentials**; `~/.config/vastai/vast_api_key`,
    `~/.cache/huggingface/token`, `$TOGETHER_API_KEY`, and the per-instance llama-server key. Prefer
    `--api-key-file` and `LLAMA_ARG_*` env over argv so secrets stay out of `/proc/*/cmdline`.

---

## Sources

Local, read 2026-07-30:
- `/home/andre/.local/lib/python3.13/site-packages/vastai/` — v1.0.4: `api/client.py`,
  `api/instances.py`, `api/offers.py`, `api/query.py`, `api/metrics.py`, `data/offer.py`,
  `data/instance.py`, `cli/util.py`, `cli/display.py`, `cli/commands/instances.py`,
  `cli/commands/misc.py`, `SKILL.md`
- `/home/andre/llama.cpp/build-vulkan/bin/llama-server --help` and `--version` (b9199, 39cf5d619)
- `/home/andre/llama.cpp/tools/server/README.md` (repo HEAD 94a220cd, 2026-06-03)
- `/home/andre/Projects/Prefrontal-RS/prefrontal-cli/src/mcp.rs`,
  `/home/andre/Projects/Prefrontal-RS/prefrontald/Cargo.toml`
- `~/.config/vastai/vast_api_key`, `~/.cache/huggingface/{token,stored_tokens}`,
  `~/.vastai-gguf/config.toml`
- crates.io registry API

Web, fetched 2026-07-30:
- <https://docs.vast.ai/api-reference/authentication>
- <https://docs.vast.ai/api-reference/rate-limits-and-errors>
- <https://docs.vast.ai/api-reference/search/search-offers>
- <https://docs.vast.ai/api-reference/instances/create-instance>
- <https://docs.vast.ai/api-reference/instances/show-instance>
- <https://docs.vast.ai/guides/instances/connect/networking>
- <https://docs.vast.ai/guides/instances/connect/ssh>
- <https://docs.vast.ai/documentation/instances/connect/instance-portal>
- <https://docs.vast.ai/llms.txt>
- <https://docs.together.ai/reference/models-1>
- <https://docs.together.ai/reference/chat-completions-1>
- <https://docs.together.ai/docs/serverless/rate-limits>
- <https://huggingface.co/docs/hub/api> and <https://huggingface.co/.well-known/openapi.md>
- <https://huggingface.co/docs/huggingface_hub/main/en/package_reference/hf_api>
- <https://modelcontextprotocol.io/specification/latest>
- <https://modelcontextprotocol.io/specification/2026-07-28/changelog>
- <https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning>
- <https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/stdio>
- <https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http>
- <https://modelcontextprotocol.io/specification/2026-07-28/server/tools>
- <https://modelcontextprotocol.io/specification/2026-07-28/server/discover>
