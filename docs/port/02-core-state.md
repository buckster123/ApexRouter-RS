# 02 — Core / state layer (LocalRouter → ApexRouter-RS)

Source of truth for this document: read in full on 2026-07-30 from
`/home/andre/Projects/Inference/tools/LocalRouter/`:

| File | Lines | Role |
|---|---|---|
| `localrouter/config.py` | 115 | path constants, console/style, `recipes.toml` loader, sampling presets, wizard maps |
| `localrouter/recipe_editor.py` | 225 | typed CRUD + validation over `recipes.toml`, TOML read/write with backup |
| `localrouter/providers.py` | 327 | provider registry, `~/.vastai-gguf/config.toml` load/save, Together probes, `.active_endpoint` write/resolve |
| `localrouter/cost.py` | 271 | pricing tables, cost estimation, `usage.log` JSONL append/aggregate, rate-limit probe |
| `localrouter/helpers.py` | 137 | subprocess wrappers, `.last_instance` read, tunnel PID liveness, HF token read, byte/path formatting |
| `localrouter/hf_browser.py` | 141 | HuggingFace repo file listing + `.hf_pin` write |
| `localrouter/__init__.py` | 4 | `__version__ = "0.3.1"` |
| `pyproject.toml` | 47 | package metadata, deps, console script |

Cross-referenced (not assigned, but they own state formats the core layer reads):
`local_endpoint.py`, `endpoint_proxy.py`, `proxy.py`, `vast_ops.py`,
`menus/provider_menus.py`, `menus/vast_menus.py`, `menus/main.py`, `recipes.toml`,
plus the **actual on-disk state** in `~/.vastai-gguf/` (dumped and diffed against the code).

Companion doc: `00-machine-ground-truth.md` (hardware + verified llama.cpp flags + broken `vastai`
CLI). Where the two conflict, ground truth wins.

---

## 1. Path map — every file LocalRouter touches

`ROOT` is defined as `Path(__file__).parent.parent.resolve()`, i.e. **the LocalRouter repo
checkout directory**, currently `/home/andre/Projects/Inference/tools/LocalRouter`. Three state
files land there; everything else lands in `~/.vastai-gguf/`. This split is a design flaw
(`00-machine-ground-truth.md` §"Existing LocalRouter state") — ApexRouter should consolidate, but
must still *read* the old locations for migration.

| Constant | Path (as resolved today) | Format | Written by | Read by |
|---|---|---|---|---|
| `ROOT` | `…/tools/LocalRouter` | dir | — | everything |
| `ROOT/recipes.toml` | `…/LocalRouter/recipes.toml` | TOML | `recipe_editor._write_toml` (tomli_w) | `config.load_config`, `recipe_editor` |
| `ROOT/recipes.toml.bak` | same dir | TOML | `recipe_editor._write_toml` (pre-write copy) | nothing (manual restore) |
| `LAST_INST` | `ROOT/.last_instance` | bare text | `vast_up.sh`, `menus/vast_menus.py:491` | `helpers.last_instance()`, `vast_down.sh`, `tools/vast_tunnel.sh` |
| `HF_PIN` | `ROOT/.hf_pin` | JSON object | `hf_browser.menu_hf_browser` | `menus/vast_menus.menu_launch` (deleted after a successful launch) |
| *(no constant)* | `ROOT/.active_endpoint` | JSON object | `providers.activate_together_endpoint`, `local_endpoint.start_local_instance`, `endpoint_proxy.switch_provider` | `providers.get_active_endpoint`, `endpoint_proxy.resolve_target`, `smoke.sh` |
| `TUNNEL_PID` | `/tmp/vastai-gguf-tunnel.pid` | bare PID text | `tools/vast_tunnel.sh` | `helpers.tunnel_running()` |
| *(no constant)* | `/tmp/vastai-gguf-proxy.pid` | bare PID text | `proxy._proxy_up`, `endpoint_proxy.run` | `proxy._proxy_up/_proxy_down`, `menus/tool_menus.py` |
| *(no constant)* | `/tmp/vastai-gguf-proxy.log` | plain text | `proxy._proxy_up` (`open(..., "w")`) | `proxy.tail_proxy_logs` |
| `PROVIDER_DIR` | `~/.vastai-gguf/` | dir | `save_provider_config`, `cost.ensure_usage_dir`, `_ensure_local_dirs` | all |
| `PROVIDER_CFG` | `~/.vastai-gguf/config.toml` | TOML (hand-written) | `providers.save_provider_config` | `providers.load_provider_config`, `endpoint_proxy.resolve_target` (own mini-parser) |
| *(no constant)* | `~/.vastai-gguf/.pinned_provider` | JSON object | `menus/provider_menus.py:274` | `menus/vast_menus.py:67` |
| `USAGE_LOG` | `~/.vastai-gguf/usage.log` | JSONL append | `cost.log_completion` | `cost.get_session_costs` |
| `LOCAL_INSTANCES` | `~/.vastai-gguf/local_instances/` | dir | `local_endpoint._ensure_local_dirs` | — |
| — | `~/.vastai-gguf/local_instances/<name>.json` | JSON object | `local_endpoint.start_local_instance/stop_local_instance` | `list_local_instances`, `endpoint_proxy` |
| `LOCAL_PID_SUFFIX` | `~/.vastai-gguf/local_instances/<name>.pid` | bare PID text | `start_local_instance` | `is_local_running`, `stop_local_instance`, `endpoint_proxy.list_providers` |
| `LOCAL_LOGS` | `~/.vastai-gguf/local_logs/<name>.log` | plain text (truncated per start) | `start_local_instance` (`open(..., "w")`) | error tail on startup failure |
| — | `~/.cache/huggingface/token` | bare text | (huggingface-cli) | `helpers._hf_token()` |

Ports (all in `config.py` unless noted):

| Constant | Value | Meaning |
|---|---|---|
| `LOCAL_PORT` | `8800` | local end of the SSH tunnel (remote container `:8000` → local `:8800`) |
| `PROXY_PORT` | `8888` | `endpoint_proxy.py` listen port; env-overridable as `PROXY_PORT` |
| `LOCAL_TUNNEL_PORT` | `8800` | duplicate of `LOCAL_PORT` inside `endpoint_proxy.py` |
| local recipe `port` | `8100` default | llama-server port for a `provider="local"` recipe |

---

## 2. On-disk formats, field by field

### 2.1 `~/.vastai-gguf/config.toml` — provider credentials

Real file on this machine (key redacted):

```toml
# ~/.vastai-gguf/config.toml — provider configuration
#
# Edit this file to add API keys and base URLs for external providers.
# You can also set environment variables (TOGETHER_API_KEY, etc.) as fallback.

[providers.together]
base_url  = "https://api.together.ai/v1"
api_key = "REDACTED"
```

Schema: a single top-level table `providers`, with one sub-table per provider key. Only two fields
per provider are ever read:

| Field | Type | Default when absent |
|---|---|---|
| `base_url` | string | `DEFAULT_PROVIDERS[key]["base_url"]`, else `""` |
| `api_key` | string | `""` |

Read path (`providers.load_provider_config`, exact order):

1. If the file exists, parse with stdlib `tomllib`. On any exception, print a yellow warning and
   continue with an empty dict — **parse failure is non-fatal**.
2. For each `providers.<key>` whose value is a dict, keep only `base_url` and `api_key`. *(Bug to
   not reproduce: if the value is **not** a dict, `config[pkey]` is set to an empty `{}` rather
   than skipped.)*
3. Merge `DEFAULT_PROVIDERS` for any provider missing from the file, and backfill `base_url` where
   the file left it empty.
4. Env override: `TOGETHER_API_KEY` → `together.api_key`, **only if the file supplied no key**
   (file wins over env). This is the opposite of the usual precedence — see §7.

Write path (`providers.save_provider_config`) — **does not use a TOML writer**. It emits the 5
header comment lines above, then per provider:

```
[providers.<key>]
base_url  = "<value>"          # only if non-empty
api_key   = "<value>"          # only if non-empty, else two comment lines:
# Set your API key here, or export the corresponding env var
# api_key = "..."
                                # blank line between blocks
```

Consequences the Rust port must handle:

- Values are interpolated with no escaping. An `api_key` containing `"` or `\` produces invalid
  TOML. Use a real serializer (`toml` / `toml_edit`) on write.
- Any provider field other than `base_url`/`api_key`, and any non-`providers` top-level table, is
  **silently destroyed** on save. ApexRouter should round-trip with `toml_edit` to preserve
  unknown keys and user comments.
- `mkdir(parents=True, exist_ok=True)` on `~/.vastai-gguf` before writing. Mode is left at umask
  default (0644) — a secrets file; ApexRouter should chmod 0600.

`endpoint_proxy.resolve_target` contains a **second, independent parser** for the same file: a
line scanner that tracks `[section]` headers, and only accepts an `api_key…=…` line while the
current section is exactly `providers.together`, stripping surrounding `"` and rejecting values
starting with `#`. Behaviourally equivalent for the simple file above; do not port the duplicate.

### 2.2 `~/.vastai-gguf/.pinned_provider` — pinned managed model

Real file (single line, no trailing newline):

```json
{"provider": "together", "model_id": "deepseek-ai/DeepSeek-V4-Pro", "base_url": "https://api.together.ai/v1"}
```

| Field | Type | Notes |
|---|---|---|
| `provider` | string | always `"together"` in the current writer |
| `model_id` | string | Together model id, as returned by `GET /v1/models` |
| `base_url` | string | copied from the effective provider config at pin time |

Written by `menus/provider_menus.py` after the Together model browser's "Pin a model" action
(`json.dumps`, no indent). Read by `menus/vast_menus.menu_launch`, which — if the JSON parses and
has `model_id` — inserts a `[pinned] <model_id>` choice at the top of the model list. Parse errors
are swallowed to `None`. **Never deleted**, unlike `.hf_pin`.

### 2.3 `ROOT/.hf_pin` — pinned GGUF quant

```json
{"MODEL_REPO": "unsloth/Qwen3.6-27B-GGUF", "MODEL_QUANT": "UD-Q5_K_XL", "filename": "Qwen3.6-27B-UD-Q5_K_XL.gguf", "size": "18.3 GB"}
```

| Field | Type | Notes |
|---|---|---|
| `MODEL_REPO` | string | HF repo id (uppercase key — matches the launcher env var name) |
| `MODEL_QUANT` | string | quant token extracted by regex from the filename, or `"?"` |
| `filename` | string | full `rfilename` from the HF API |
| `size` | string | **already humanized** by `_fmt_bytes`, e.g. `"18.3 GB"` — not a number |

Written by `hf_browser.menu_hf_browser` (`json.dumps`, no indent). Read by
`menus/vast_menus.menu_launch`, which renders a panel using `pin['MODEL_REPO']`,
`pin['MODEL_QUANT']`, `pin['size']` — **direct indexing, so a pin missing any of those three keys
raises and is caught, disabling the pin**. Deleted (`HF_PIN.unlink()`) after `vast_up.sh` exits 0.

Not present on this machine at survey time.

### 2.4 `ROOT/.last_instance` — active Vast instance id

Bare text, the numeric Vast instance id. Written by `vast_up.sh` via `echo "${INST_ID}" >
.last_instance` (so it **has a trailing newline**) and by the TUI's reattach action
(`LAST_INST.write_text(new_id)` — **no** newline). Readers must `.strip()`.

`helpers.last_instance()` returns `None` on `FileNotFoundError` only — a permissions error or a
directory at that path propagates.

Security note carried over from `AUDIT_REPORT.md`: the value is interpolated straight into shell
command strings (`vastai show instance {inst_id}`), so a malicious file content is command
injection. **The Rust port must never shell-interpolate this value** — and per ground truth it
should be talking to the Vast REST API anyway.

Not present on this machine at survey time.

### 2.5 `ROOT/.active_endpoint` — the router's current target

The single most important state file: it is what `endpoint_proxy` reads on **every request**.
There are **four different shapes**, distinguished by `provider`:

**(a) Together, written by `providers.activate_together_endpoint`** (`json.dumps(..., indent=2)`):

| Field | Type | Value |
|---|---|---|
| `provider` | string | `"together"` |
| `model_id` | string | selected model |
| `base_url` | string | e.g. `https://api.together.ai/v1` |
| `endpoint` | string | `f"{base_url}/chat/completions"` (derived, redundant) |
| `activated_at` | string | `time.strftime("%Y-%m-%dT%H:%M:%SZ")` — **local time labelled `Z`** |

**(b) Together, written by `endpoint_proxy.switch_provider`** — same as (a) except the timestamp
key is `switched_at` and it uses `time.gmtime()` (actually UTC). A reader must accept either key.

**(c) Local, written by `local_endpoint.start_local_instance`:**

| Field | Type | Value |
|---|---|---|
| `provider` | string | `"local"` |
| `name` | string | recipe name; keys into `local_instances/<name>.{json,pid}` |
| `host` | string | default `"127.0.0.1"` |
| `port` | int | default `8100` |
| `pid` | int | llama-server PID |
| `model_path` | string | **as written in the recipe, tilde NOT expanded** |
| `activated_at` | string | `%Y-%m-%dT%H:%M:%SZ` |
| `api_key` | string | **optional**, only present if the recipe had one; the proxy sends it as `Bearer` |

**(d) Local, written by `endpoint_proxy.switch_provider`** — `provider`, `name`, `host`, `port`,
`model_path`, `switched_at`. No `pid`, no `api_key`.

Absence of the file (or any `provider` other than `together`/`local`) means **fall back to Vast**:
`resolve_target()` returns `http://127.0.0.1:8800/v1`, no auth, provider `"vast-gguf"`.
`switch_provider("vast-gguf")` implements the switch by **deleting** the file.

`providers.get_active_endpoint()` resolution order:

1. `.active_endpoint` exists and parses:
   - `provider == "together"` → return the dict verbatim.
   - `provider == "local"` → add `status: "running"|"stopped"` from `is_local_running(name)`, return.
   - any other provider → fall through (does *not* return).
2. Else `last_instance()` → `vastai show instance <id> --raw` → return
   `{"provider": "vast-gguf", "instance_id": <id>, "status": <actual_status or "?">}`.
   *(Note: `"vast-gguf"` with a hyphen here, while recipes use `provider = "vast_gguf"` with an
   underscore. Both spellings are live in the data — normalize on ingest.)*
3. Else `None`.

Leak to fix, not port: `AUDIT_REPORT.md` flags that a local recipe's `api_key` gets written into
`.active_endpoint` in the repo directory at default file mode.

### 2.6 `~/.vastai-gguf/local_instances/<name>.json`

Real file on this machine, `local-qwen35-9b.json`:

```json
{
  "name": "local-qwen35-9b",
  "pid": 649035,
  "port": 8100,
  "host": "127.0.0.1",
  "binary": "/home/andre/llama.cpp/build-vulkan/bin/llama-server",
  "model_path": "~/models/Qwen3.5-9B-Q4_K_M.gguf",
  "backend": "vulkan",
  "started_at": "2026-05-03T00:34:36Z",
  "status": "stopped",
  "stopped_at": "2026-05-03T00:38:32Z"
}
```

| Field | Type | Written when | Notes |
|---|---|---|---|
| `name` | string | start | filename stem; `list_local_instances` falls back to the stem if absent |
| `pid` | int | start | never cleared on stop (stale by design) |
| `port` | int | start | `int(recipe.port)`, default 8100 |
| `host` | string | start | default `"127.0.0.1"` |
| `binary` | string | start | resolved absolute path to `llama-server` |
| `model_path` | string | start | **unexpanded** recipe value (`~/...`) |
| `backend` | string | start | lowercased recipe backend, or the literal `"auto"` |
| `started_at` | string | start | `%Y-%m-%dT%H:%M:%SZ` local time |
| `status` | string | start / health / stop | `"starting"` → `"running"` (on first successful health probe) → `"stopped"` |
| `stopped_at` | string | stop only | present only after a clean `stop_local_instance` |
| `running` | bool | **injected at read time**, never persisted | `list_local_instances` adds it |

Written with `indent=2`. Note the health-check success path rewrites the file as
`{**instance_meta, "status": "running"}` — it drops nothing, but it also does **not** re-read, so a
concurrent edit is lost.

`list_local_instances()` globs `*.json` sorted by path, swallows per-file exceptions, and
downgrades `status == "starting"` to `"stopped"` when the PID is not alive.

This machine's saved `model_path` **no longer exists** (see ground truth). Validate on load; do
not error out.

### 2.7 `~/.vastai-gguf/local_instances/<name>.pid`

Bare integer PID, no newline (`pid_file.write_text(str(proc.pid))`). Liveness is `os.kill(pid, 0)`.
`is_local_running` **unlinks the PID file** when the process is gone or the content is not an int —
i.e. reading has a side effect. `stop_local_instance` unlinks after SIGTERM (10 × 0.5 s poll, then
SIGKILL).

Gotcha to fix in Rust: `os.kill(pid,0)` raising `PermissionError` is *not* caught here (unlike
`helpers.tunnel_running`, which treats it as alive), so a PID owned by another user crashes the
caller. Also, plain PID reuse is unguarded — ApexRouter should additionally verify the process
cmdline/start-time before trusting a PID.

### 2.8 `~/.vastai-gguf/local_logs/<name>.log`

Raw combined stdout+stderr of `llama-server`, opened with mode `"w"` — **truncated on every
start**, no rotation. On a startup failure the last 500 bytes are surfaced in the error message.
Current file on disk: 16 675 bytes for `local-qwen35-9b`.

### 2.9 `~/.vastai-gguf/usage.log` — JSONL usage/cost ledger

Real contents (4 lines) on this machine:

```json
{"timestamp": "2026-05-02T20:11:21Z", "epoch": 1777745481.5262182, "provider": "together", "model_id": "meta-llama/Llama-3.1-8B-Instruct-Turbo", "prompt_tokens": 100, "completion_tokens": 50, "cost_usd": 2.7e-05}
{"timestamp": "2026-05-02T20:11:21Z", "epoch": 1777745481.5263166, "provider": "vast-gguf", "model_id": "Qwen3.6-27B-Q8.gguf", "prompt_tokens": 100, "completion_tokens": 50, "cost_usd": 0.000766}
{"timestamp": "2026-05-02T20:11:49Z", "provider": "together", "model_id": "Qwen/Qwen2.5-72B-Instruct-Turbo", "prompt_tokens": 131072, "completion_tokens": 500, "cost_usd": 0.115783}
{"timestamp": "2026-05-02T20:12:11Z", "epoch": 1777745531.4937491, "provider": "together", "model_id": "Qwen/Qwen2.5-72B-Instruct-Turbo", "prompt_tokens": 131072, "completion_tokens": 500, "cost_usd": 0.115783}
```

Current writer (`cost.log_completion`) emits exactly:

| Field | Type | Notes |
|---|---|---|
| `timestamp` | string | `%Y-%m-%dT%H:%M:%SZ`, **local time with a `Z` suffix** (wrong but must be tolerated) |
| `provider` | string | `"together"`, `"vast-gguf"`, `"local"`, `"local-gguf"`, … free-form |
| `model_id` | string | may be `""` |
| `prompt_tokens` | int | |
| `completion_tokens` | int | |
| `cost_usd` | float | computed at log time, see §4.2 |

`epoch` (float unix seconds) appears in 3 of 4 real lines but is **not produced by any code in the
current tree** — a legacy field from an earlier version. Rule for the Rust port: **deserialize
permissively, ignore unknown fields, default missing ones**. `README.md` documents yet another
(never-written) schema with `ts`/`model`/`cost` keys; ignore the README.

Append is `open(USAGE_LOG, "a")` + one `json.dumps` line + `"\n"`. No locking — concurrent writers
(TUI and proxy both do it) can interleave; single `write()` of a short line is atomic enough in
practice on Linux, but the Rust port should still use `O_APPEND` and one `write` syscall per line.

Reader (`cost.get_session_costs`) ignores blank lines, skips undecodable lines, and returns:

```json
{"by_provider": {"<provider>": {"cost": <f64>, "tokens": <prompt+completion>}},
 "grand_total": <round(sum, 4)>,
 "total_entries": <line count>}
```

Despite the name, it aggregates the **entire file**, not a session. There is no rotation, pruning
or per-day bucketing anywhere.

### 2.10 `recipes.toml` — the recipe/tier catalogue

28 KB, in the repo root, **zero comment lines** (already round-tripped through `tomli_w`).
Top-level keys today: `docker`, `gpu_tiers`, `recipes`, and a stray empty `[local]` table at the
end of the file (line 1048) that no code reads.

`config.load_config()` returns `(cfg, recipes, gpu_tiers, docker_cfg)` and mutates `docker_cfg`
with two `setdefault`s. Missing file → red message + `sys.exit(1)`.

**`[docker]`** — flat `image_type → image reference` map:

```toml
prebuilt        = "ghcr.io/buckster123/vastai-gguf:prebuilt"
builder         = "ghcr.io/buckster123/vastai-gguf:builder"
prebuilt_legacy = "ghcr.io/buckster123/qwen36-llamacpp:latest"
vllm            = "ghcr.io/buckster123/vastai-gguf:vllm"
```

`image_for_type(docker_cfg, t)` = `docker_cfg.get(t, docker_cfg.get("prebuilt", <hardcoded>))`.
`cold_start_estimate(t)`: `prebuilt` → `"~2 min  (image pull only)"`, `builder` →
`"~12-18 min  (pull + SM compile)"`, anything else → `"unknown"`.

**`[gpu_tiers.<key>]`** — 19 tiers keyed by short name
(`5090`, `4090`, `6000pro`, `5090-dc`, `h100-sxm`, `h100-pcie`, `a100-sxm`, `a100-pcie`,
`h200-sxm`, `b200-sxm`, `h100-sxm-2x`, `h100-sxm-4x`, `h200-sxm-2x`, `b200-sxm-2x`,
`b200-sxm-4x`, `h200-sxm-4x`, `h200-sxm-5x`, `h100-sxm-8x`, `a100-sxm-8x`):

| Field | Type (verified across all 19) | Req? | Notes |
|---|---|---|---|
| `vast_names` | array of string | ✅ required | Vast `gpu_name` values; 1 → `gpu_name=X`, >1 → `gpu_name in [A,B]` |
| `label` | string | ✅ required | shown in the wizard, e.g. `"RTX 5090 32GB   (~$0.34/hr)"` |
| `max_price` | **string**, 19/19 | ✅ required | `"0.55"` — quoted decimal, NOT a float. Parse leniently. |
| `min_disk_gb` | int, 19/19 | optional | default 60 |
| `image_type` | string, 19/19 | optional | `prebuilt` / `builder` / `vllm`; `vllm` tiers are filtered *in* only for vLLM launches and *out* otherwise |
| `vram_gb` | int, 19/19 | optional | display only |
| `num_gpus` | int, 9/19 | optional | default 1; present on the multi-GPU tiers |

`validate_gpu_tier` requires `{vast_names, label, max_price}` and asserts `vast_names` is a list.

**`[[recipes]]`** — 71 entries, array of tables. Union of fields observed, with counts:

| Field | Type | Count/71 | Meaning |
|---|---|---|---|
| `name` | string | 71 | slug; unique key; validator allows only `[alnum]-_.` |
| `label` | string | 71 | menu text |
| `description` | string | 71 | long text |
| `ctx` | int | 71 | context window; validator requires positive int |
| `gpu` | string | 61 | key into `gpu_tiers` (validated to exist for non-local/non-together) |
| `model_repo` | string | 54 | HF repo (GGUF path) |
| `model_quant` | string | 54 | quant token, e.g. `UD-Q5_K_XL` |
| `parallel` | int | 57 | llama-server `--parallel` |
| `kv_type` | string | 57 | `q8_0` / `q4_0` / `bf16` |
| `provider` | string | 17 | `local` / `together` / `vllm`; **absent ⇒ `vast_gguf`** |
| `image_type` | string | 14 | overrides the tier's `image_type` |
| `model_id` | string | 14 | Together / vLLM model identifier |
| `llama_cpp_repo` | string | 7 | custom fork, e.g. `fairydreaming/llama.cpp` |
| `llama_cpp_ref` | string | 7 | branch/tag, e.g. `deepseek-dsa` |
| `kv_cache_dtype` | string | 7 | vLLM, e.g. `fp8` |
| `reasoning_parser` | string | 7 | vLLM, e.g. `deepseek_r1` |
| `price_input` | float | 7 | Together $/M input tokens (**declared but never read by cost.py**) |
| `price_output` | float | 7 | Together $/M output tokens (same) |
| `enforce_eager` | **string** | 4 | vLLM; `"false"`/`"true"` as strings, not bools |
| `min_disk_gb` | int | 4 | overrides tier |
| `model_path` | string | 3 | local: GGUF path, may contain `~` |
| `port` | int | 3 | local: llama-server port |
| `n_gpu_layers` | int | 3 | local: `--n-gpu-layers` |
| `backend` | string | 3 | local: `vulkan` / `rocm` / `hip` / … |
| `mode` | string | 3 | sampling preset key |
| `mmproj` | string | 2 | vision projector path/file |
| `quantization` | — | 0 | read by the launcher (`env["QUANTIZATION"]`) but no recipe sets it |
| `host` | — | 0 | read by `_get_local_server_args`, defaults `127.0.0.1` |
| `api_key` | — | 0 | read by `start_local_instance`; if set, propagates into `.active_endpoint` |
| `binary` | — | 0 | read by `start_local_instance`; if unset, auto-discovery runs |

Example of each provider shape:

```toml
[[recipes]]                                   # vast_gguf (implicit)
name = "dsv4-flash-q2k-2xh100"
label = "DSv4-Flash 284B  Q2_K  128K ctx  (2×H100)"
gpu = "h100-sxm-2x"
model_repo = "Preyazz/DeepSeek-V4-Flash-GGUF"
model_quant = "Q2_K"
ctx = 131072
parallel = 1
kv_type = "q8_0"
image_type = "builder"
llama_cpp_repo = "fairydreaming/llama.cpp"
llama_cpp_ref = "deepseek-dsa"

[[recipes]]                                   # local
name = "local-qwen35-9b"
provider = "local"
label = "Qwen3.5-9B  Q4_K_M  (local Vulkan)"
model_path = "~/models/Qwen3.5-9B-Q4_K_M.gguf"
port = 8100
ctx = 32768
parallel = 1
kv_type = "q8_0"
n_gpu_layers = 999
backend = "vulkan"
mode = "thinking"

[[recipes]]                                   # together
name = "together-llama3.1-8b"
provider = "together"
label = "Llama 3.1 8B ($0.18/M tokens)"
model_id = "meta-llama/Llama-3.1-8B-Instruct-Turbo"
ctx = 131072
price_input = 0.18
price_output = 0.18

[[recipes]]                                   # vllm
name = "dsv4-pro-5xh200"
provider = "vllm"
label = "DSv4-Pro 1.6T  FP4+FP8  384K ctx  (5×H200) 🏆"
gpu = "h200-sxm-5x"
model_id = "deepseek-ai/DeepSeek-V4-Pro"
ctx = 393216
image_type = "vllm"
kv_cache_dtype = "fp8"
enforce_eager = "false"
reasoning_parser = "deepseek_r1"
```

Rust modelling advice: a `Recipe` struct with `#[serde(default)]` on everything except
`name`/`label`, `provider: Provider` defaulting to `VastGguf`, `#[serde(flatten)] extra:
Map<String, Value>` to survive round-trips, and lenient number-or-string handling for `max_price`
and `enforce_eager`.

---

## 3. Recipe editor semantics (`recipe_editor.py`)

Pure data layer, no TUI. Reader: `tomllib` (3.11+) / `tomli` fallback / `config._load_toml`.
Writer: `tomli_w.dump`, and **before every write it copies the file to
`recipes.toml.bak`** (`p.with_suffix(".toml.bak")` — a single-slot backup, overwritten each time).

CRUD surface (`data` is the whole parsed document):

- `load_recipes() -> dict`, `save_recipes(dict)`
- `get_recipes(data) -> list`, `get_gpu_tiers(data) -> dict`, `get_docker_cfg(data) -> dict`
- `find_recipe(data, name)` — linear scan on `name`
- `add_recipe(data, recipe)` — appends, **no duplicate-name check**
- `remove_recipe(data, name) -> bool`
- `update_recipe(data, name, updates) -> bool` — shallow `dict.update`
- `duplicate_recipe(data, name, new_name)` — deep copy + rename + append
- `add_gpu_tier(data, key, tier)`, `remove_gpu_tier(data, key) -> bool`
- `add_docker_image(data, key, image)`, `list_docker_images(data) -> dict`

Validation constants (port these verbatim — they are the contract the TUI wizard enforces):

```python
REQUIRED_RECIPE_FIELDS_VAST     = {"name","label","gpu","model_repo","model_quant","ctx"}
REQUIRED_RECIPE_FIELDS_LOCAL    = {"name","label","model_path","port"}
REQUIRED_RECIPE_FIELDS_TOGETHER = {"name","label","model_id"}
REQUIRED_RECIPE_FIELDS_VLLM     = {"name","label","gpu","model_id","ctx"}
REQUIRED_TIER_FIELDS            = {"vast_names","label","max_price"}
OPTIONAL_RECIPE_FIELDS_VAST     = {"parallel","kv_type","min_disk_gb","image_type",
                                   "description","llama_cpp_repo","llama_cpp_ref"}
```

`validate_recipe(recipe, gpu_tiers)` returns a list of human-readable error strings:

1. missing-required for the recipe's provider (default `vast_gguf`),
2. `gpu` not in `gpu_tiers` (skipped for `local`/`together` — **note `vllm` is *not* skipped**),
3. `name` containing anything outside `[alnum]-_.`,
4. `ctx` present but not a positive int.

`validate_gpu_tier(tier)` → missing-required + `vast_names` must be a list.

Known data-loss risk to fix in the port: `tomli_w.dump` rewrites the entire document, discarding
comments and formatting. Today's `recipes.toml` has no comments (already lost), so nothing is at
risk right now — but ApexRouter should use `toml_edit` so future hand-written comments survive.
Also, the `.bak` copy is not atomic w.r.t. the write; use write-temp + `rename` for the real file.

---

## 4. Cost model (`cost.py`)

### 4.1 Static pricing tables (hardcoded inside `estimate_cost`)

Vast $/hr, by GPU short name:

| Key | $/hr |
|---|---|
| `5090` | 0.34 |
| `4090` | 0.28 |
| `6000pro` | 0.93 |
| `h100-sxm` | 2.50 |
| `a100-sxm` | 1.20 |
| `h200-sxm` | 3.50 |

Together $/1M tokens (input and output are identical in every row):

| Model | in | out |
|---|---|---|
| `meta-llama/Llama-3.1-8B-Instruct-Turbo` | 0.18 | 0.18 |
| `Qwen/Qwen2.5-Coder-32B-Instruct-Turbo` | 0.44 | 0.44 |
| `meta-llama/Llama-3.3-70B-Instruct-Turbo` | 0.88 | 0.88 |
| `Qwen/Qwen2.5-72B-Instruct-Turbo` | 0.88 | 0.88 |
| `meta-llama/Llama-3.1-405B-Instruct-Turbo` | 3.50 | 3.50 |
| `mistralai/Mixtral-8x7B-Instruct-v0.1` | 0.60 | 0.60 |
| `Qwen/QwQ-32B-Preview` | 0.44 | 0.44 |

Other magic numbers: assumed throughput `vast_throughput = 100 tok/s`; the `log_completion`
fallback rate `$0.50/hr`; the `log_completion` Together flat rate `$0.88/M`.

The same price list is duplicated as display strings in `menus/vast_menus.py`
(`popular_models`), and `price_input`/`price_output` exist per-recipe in `recipes.toml` but are
never consulted. **In the Rust port, make the recipe fields authoritative and the table a
fallback**, with one pricing source.

### 4.2 Formulas — exactly as implemented

`estimate_cost(ctx_tokens, output_tokens, provider_cfg) -> dict`:

```
total          = ctx_tokens + output_tokens
vast_hours     = total / (100 * 3600)
avg_vast_rate  = mean(vast_hourly_rates.values())              # = 1.4583…
estimates["vast-gguf"] = {cost_usd: round(vast_hours*avg_vast_rate, 4),
                          rate: "$<avg>/hr (avg)", type: "hourly"}

if provider_cfg has together with a non-empty api_key:
    avg_tog_rate = mean(r["in"] for r in together_rates.values())   # = 0.9886/M
    estimates["together"] = {cost_usd: round(total * avg_tog_rate/1e6, 4),
                             rate: "$<avg>/M tok", type: "per-token"}
```

Both branches average across *all* models/GPUs rather than using the selected one — the estimate
is deliberately coarse. The Together branch is omitted entirely when no key is configured.

`log_completion(provider, model_id, prompt_tokens, completion_tokens)` cost rule:

| provider | cost |
|---|---|
| `"together"` | `round((prompt+completion) * 0.88/1e6, 6)` |
| `"local"` / `"local-gguf"` | `0.0` |
| anything else | `round(((prompt+completion)/(100*3600)) * 0.50, 4)` |

`format_cost_comparison(ctx=1000, out=500, cfg)` and `format_usage_summary(cfg)` produce Rich
markup strings for the TUI (`"[dim]No usage tracked yet[/dim]"` when the log is empty). Display
labels: `vast-gguf` → `"Vast GGUF"`, `together` → `"Together AI"`, else the raw key.

Call sites of `log_completion`: `providers.activate_together_endpoint` (the activation smoke
test), `menus/provider_menus._configure_together` (config smoke test), and
`menus/tool_menus.py` (batch compare / smoke). **The proxy itself does not log usage** — it only
sets an `X-Usage: <prompt>+<completion>` response header. So `usage.log` measures TUI-initiated
requests only. ApexRouter should log from the proxy path, which is where the real traffic is.

### 4.3 Rate limits

`check_together_rate_limits(provider_cfg)`:

- key from `provider_cfg["together"]["api_key"]` else `$TOGETHER_API_KEY`; returns `None` if none.
- `GET {base_url}/models`, headers `Authorization: Bearer …`, `User-Agent: LocalRouter/1.0`,
  timeout 10 s.
- Reads response headers **`X-RateLimit-Limit`, `X-RateLimit-Remaining`, `X-RateLimit-Reset`**,
  each `int(...)` or `None`. Returns `{"limit":…, "remaining":…, "reset":…}`.
- Any exception → `None`. No caching, no backoff, no retry.

`format_rate_limits` renders `"Limit: N/period | Remaining: N | Resets: HH:MM:SS"` (reset treated
as a unix timestamp via `datetime.fromtimestamp`, i.e. local tz).

There is **no proactive rate limiting anywhere** — no token bucket, no 429 retry. The only 429
handling in the whole codebase is `test_together_connection` mapping HTTP 429 to the string
`"Rate limited (429) — try again later"`. Backoff/retry is a gap the Rust port should fill.

---

## 5. Provider abstraction shape (`providers.py`)

There is **no trait/interface** — the "abstraction" is:

```python
DEFAULT_PROVIDERS = {
    "together": {"base_url": "https://api.together.ai/v1", "label": "Together AI"},
}
```

…plus a hardcoded `env_map = {"together": "TOGETHER_API_KEY"}` inside `load_provider_config`, plus
`if provider == "together" / "local" / "vllm"` branches scattered across
`providers.py`, `cost.py`, `endpoint_proxy.py`, `recipe_editor.py` and the menus. Adding a
provider today means editing ~6 files.

Provider identifiers in use, and where each spelling appears — **the port must normalize these**:

| String | Appears in |
|---|---|
| `together` | provider config key, recipe `provider`, `.active_endpoint.provider`, usage-log `provider` |
| `local` | recipe `provider`, `.active_endpoint.provider`, usage-log `provider` |
| `local-gguf` | usage-log `provider` (cost branch only) |
| `vast_gguf` | recipe `provider` (implicit default) |
| `vast-gguf` | `.active_endpoint` fallback, usage-log `provider`, all display code |
| `vllm` | recipe `provider`, `docker` image key, `gpu_tiers.*.image_type` |

Functions (the de-facto provider surface to re-express as a Rust trait):

| Function | Behaviour |
|---|---|
| `load_provider_config() -> dict` | §2.1 |
| `save_provider_config(cfg)` | §2.1 |
| `test_together_connection(base_url, api_key) -> (bool, str)` | `GET {base}/models`, 10 s; accepts `{"data":[…]}` **or** a bare list; keeps entries that are dicts with `"id"`; success message lists the first 5 ids. 401 → "Authentication failed", 429 → "Rate limited", other HTTP → `HTTP {code}: {reason}` |
| `run_together_completion(base_url, api_key, model_id, prompt="Say hello in 5 words") -> (bool,str)` | `POST {base}/chat/completions` `{model, messages:[{role:"user",content:prompt}], max_tokens:20}`, 15 s; error body truncated to 200 chars |
| `activate_together_endpoint(provider_cfg, model_id) -> bool` | key from config else `$TOGETHER_API_KEY`; connection test (hard gate); inline completion test (soft gate — on failure asks "Activate anyway?"); `log_completion("together", …)`; writes `.active_endpoint` shape (a) |
| `get_active_endpoint() -> dict\|None` | §2.5 |

A proper Rust design would be `trait Provider { fn id(); fn base_url(); fn probe(); fn
complete(); fn price(); }` with `Together`, `LlamaCppLocal`, `VastGguf`, `Vllm` implementations,
plus a `ProviderId` enum with serde aliases covering every spelling above.

---

## 6. External calls (HTTP + CLI) the port must reproduce

### HuggingFace (`hf_browser.py`, `helpers._hf_token`)

- **`GET https://huggingface.co/api/models/{repo_id}?blobs=true`**
  - Headers: `User-Agent: LocalRouter/1.0`; `Authorization: Bearer <token>` if
    `~/.cache/huggingface/token` exists (bare token file, `.strip()`ed).
  - Timeout 10 s. Any exception → `[]` (silently).
  - Uses only the `siblings` array: each element's `rfilename` (string) and `size` (int; `?blobs=true`
    is what makes `size` present).
- Filter: `rfilename.endswith(".gguf")`.
- Quant extraction regex (case-insensitive), applied to the filename:
  `(UD-Q\d+[^.\s_-]*|Q\d+_K_[A-Z]+|Q\d+_\d+)` → first match, else `"?"`.
- Sizes are formatted by `_fmt_bytes` (binary 1024 steps, one decimal, `B/KB/MB/GB/TB/PB`) and the
  formatted string is what gets persisted into `.hf_pin`.
- No pagination, no repo search endpoint, no download. That is all of the HF surface.

### Together AI

- `GET {base_url}/models` — used for connection test, model browser, and the rate-limit probe.
  Response tolerated as `{"data":[…]}` or a bare array; entries need an `id`. The browser groups by
  the org prefix (`id.split("/",1)[0]`, else `"other"`) and truncates the table to 50 rows.
- `POST {base_url}/chat/completions` — smoke tests only, `max_tokens: 20`.
- Auth: `Authorization: Bearer <key>`. Timeouts: 10 s (models), 15 s (completions).
- The proxy forwards arbitrary `/v1/...` paths to the same base URL, injecting the same header.

### Vast.ai — **shell-outs to a CLI that is broken on this machine**

`helpers`/`vast_ops` shell out via `subprocess.run(cmd, shell=True, cwd=ROOT, timeout=…)`
(`capture()` returns rc **124** on timeout instead of raising):

- `vastai show instance {id} --raw` (timeout 12 s) → instance JSON; fields used: `ssh_host`,
  `ssh_port`, `actual_status`.
- `vastai search offers "<filter>" --order dph_total --raw` (timeout 20 s). Filter string:
  `gpu_name=X` or `gpu_name in [A,B]`, plus `num_gpus={n} reliability>0.97 inet_down>300
  dph_total<{max_price} disk_space>{min_disk} cuda_vers>={min_cuda} rentable=true`.
  Output is de-prefixed by finding the first `[`. Offer fields used: `id`, `dph_total`,
  `reliability2`, `gpu_ram` (MiB → GB by /1024), `inet_down`, `cuda_max_good`, `geolocation`.
  Geo filter is a **regex on the tail of `geolocation`**: `", (SE|NO|FI|DK|IS)$"` etc.
- `ssh -p <port> -o StrictHostKeyChecking=no -o ConnectTimeout=8 root@<host> <shlex-quoted cmd>`
  for remote diagnostics.
- `bash vast_up.sh` / `vast_down.sh` / `tools/vast_tunnel.sh {up,logs}`.

Per `00-machine-ground-truth.md` the `vastai` package is **uninstalled** — every one of these
returns `ModuleNotFoundError`. ApexRouter must use the Vast REST API over HTTPS with the key at
`~/.config/vastai/vast_api_key`, and treat the CLI path as dead.

---

## 7. Environment variables honoured

| Var | Read at | Semantics |
|---|---|---|
| `TOGETHER_API_KEY` | `providers.load_provider_config` (env_map), `providers.activate_together_endpoint`, `cost.check_together_rate_limits`, `proxy.proxy_status_detail`, `endpoint_proxy.{resolve_target,switch_provider,list_providers}`, `menus/{provider,vast,tool}_menus` | Together credential. **In `load_provider_config` the file wins over the env** (env only fills an empty key); most direct call sites use `cfg.get("api_key", os.environ.get(...))` which is also file-first. `endpoint_proxy.resolve_target` is the exception — it checks **env first**, then the file. Inconsistent; the port should pick one order (ground truth §Credentials suggests: explicit → config file → conventional path → env). |
| `PROXY_PORT` | `endpoint_proxy.py:34` | proxy listen port, default `8888`. Not honoured by `config.PROXY_PORT`, which is a hardcoded `8888` used for display and health checks — they can disagree. |
| `PATH` | `local_endpoint.discover_local` | extra directories to scan for `llama-server` |
| `HOME` | everywhere via `Path.home()` | roots `~/.vastai-gguf`, `~/models`, `~/llama.cpp`, `~/.cache/huggingface` |
| `GGML_VK_VISIBLE_DEVICES` | **set** to `"0"` by `start_local_instance` when the recipe backend contains `vulkan` | child env only |
| `HIP_VISIBLE_DEVICES` | **set** to `"0"` when the backend contains `rocm`/`hip` | child env only |

The launch wizard additionally **exports a large env contract to `vast_up.sh`**
(`menus/vast_menus.py:344-380`) — this is the shell-script API and must be preserved if the
scripts are kept:

`GPU`, `MODEL_REPO`, `MODEL_QUANT`, `CTX`, `PARALLEL`, `KV_TYPE`, `MODE`, `GEO`, `MAX_PRICE`,
`DOCKER_IMAGE`, `IMAGE_TYPE`, `MIN_DISK_GB`, `NUM_GPUS`, `MIN_CUDA`, `MODEL` (= recipe name),
`VAST_NAMES` (conditional), `MMPROJ` (conditional), `OFFER_ID` (conditional),
`LLAMA_CPP_REPO`, `LLAMA_CPP_REF` (conditional), and for vLLM: `MODEL_ID`, `QUANTIZATION`,
`KV_CACHE_DTYPE`, `ENFORCE_EAGER`, `REASONING_PARSER`.

Other vars the shell scripts consume on their own: `HF_TOKEN`, `HOST`, `PORT`, `MODELS_DIR`,
`BUILD_DIR`, `N_GPU_LAYERS`, `MODEL_PATH`, `MODEL_FILE`, `TP`, `GPU_UTIL`, `MAX_NUM_SEQS`,
`DTYPE`, `TRUST_REMOTE`, `CHUNKED_PREFILL`, `VLLM_ATTENTION_BACKEND`, `REMOTE_PORT`,
`LOCAL_PORT`, `INST_FILE`, `PROJECT_DIR`.

---

## 8. Constants worth porting verbatim

`SAMPLING_PRESETS` (`config.py`) — appended to the llama-server argv by
`_get_local_server_args`, and mapped to the `MODE` env var for Vast:

```python
"thinking":    ["--temp","1.0","--top-p","0.95","--min-p","0.0","--presence-penalty","1.5"]
"coding":      ["--temp","0.6","--top-p","0.95","--min-p","0.0","--presence-penalty","0.0"]
"nonthinking": ["--temp","0.7","--top-p","0.80","--min-p","0.0","--presence-penalty","1.5",
                "--chat-template-kwargs", '{"enable_thinking":false}']
```

`GEOS` (label → key): `EU_NORDIC`, `EU`, `US`, `ANY`.
`geo_re_map` (`vast_ops.py`, key → regex alternation):
`EU_NORDIC = SE|NO|FI|DK|IS`;
`EU = SE|NO|FI|DK|IS|DE|NL|FR|BE|UK|IE|EE|LV|LT|PL|CZ|AT|CH|ES|PT|IT`;
`US = US`; `ANY = .*`.

`MODES` (label → key): `thinking`, `coding`, `nonthinking`.
`KV_TYPES` (label → key): `q8_0`, `q4_0`, `bf16`. *(Ground truth: the current llama.cpp build also
accepts `f32,f16,q4_1,iq4_nl,q5_0,q5_1`; the menu list is stale.)*

`MENU_STYLE` accent palette: primary `#7c6af7`, selected `#9d8ff7`, dim `#555555`, panel border
`#3d3d5c`. Keep these if ApexRouter's TUI should look like its ancestor.

Local llama-server argv order built by `_get_local_server_args` (verify each flag against ground
truth before emitting — several defaults changed in b9199):
`<binary> --model <expanded> --host <h> --port <p> --ctx-size <n> --parallel <n>
--cache-type-k <kv> --cache-type-v <kv> --n-gpu-layers <n> --jinja --metrics --flash-attn on
<sampling preset…> [--mmproj <path>]`.

---

## 9. Behaviours to fix rather than port

1. **Timestamps**: `time.strftime("%Y-%m-%dT%H:%M:%SZ")` stamps **local** time with a `Z`. Write
   real RFC 3339 UTC; parse the old values leniently (assume local when a `Z` value is ambiguous —
   or just treat them as opaque display strings for migrated rows).
2. **State in the repo directory**: `.active_endpoint`, `.last_instance`, `.hf_pin` belong under
   `~/.local/state/apexrouter/` (or `$XDG_STATE_HOME`). Read the legacy paths once, migrate, leave
   the originals untouched.
3. **Secrets at 0644**: `config.toml` and `.active_endpoint` (which can carry a local `api_key`)
   are written world-readable. Use 0600.
4. **Hand-rolled TOML writing** with no escaping (§2.1) and **whole-document rewrites** that drop
   unknown keys (§2.1, §3).
5. **Shell interpolation of file-sourced values** (`.last_instance` → `vastai show instance {id}`).
   No `shell=True` in the port; use argv vectors, or better, the REST API.
6. **PID trust**: no PID-reuse guard, and `PermissionError` from `os.kill(pid,0)` is handled
   inconsistently (alive in `tunnel_running`, crash in `is_local_running`).
7. **Unbounded `usage.log`** and a `get_session_costs()` that actually totals all of history.
   Add rotation and real per-session/per-day windows.
8. **Cost tables hardcoded in three places** and per-recipe `price_input`/`price_output` ignored.
9. **No retry/backoff** on any HTTP call; every failure collapses to `None`/`[]`/a string.
10. **Log truncation** (`local_logs/<name>.log` opened `"w"`) loses the previous run's crash log —
    exactly when you need it.

---

## 10. Packaging facts (`pyproject.toml`)

`name = "localrouter"`, `version = "0.3.1"` (matches `__init__.__version__`), MIT, requires Python
≥ 3.10. Runtime deps `questionary>=2.0.0`, `rich>=13.0.0`, `tomli_w>=1.0.0`; extras
`proxy`/`all` = `aiohttp>=3.9.0`. Console script `localrouter = localrouter.menus.main:main`;
`python -m localrouter` also works via `__main__.py` (which swallows `KeyboardInterrupt` and exits
0). Homepage `https://github.com/buckster123/LocalRouter`.

Rust equivalents to reach for: `serde`/`serde_json`, `toml` + `toml_edit`, `reqwest` (rustls),
`tokio`, `ratatui`/`inquire` (TUI), `directories` (XDG), `nix`/`sysinfo` (PID liveness),
`tracing`.
