# 03 — Local llama.cpp endpoint management

Port spec for `LocalRouter/localrouter/local_endpoint.py` (433 lines) →
ApexRouter-RS.

Source of truth read in full:

- `/home/andre/Projects/Inference/tools/LocalRouter/localrouter/local_endpoint.py`
- support: `localrouter/config.py` (constants, `SAMPLING_PRESETS`),
  `localrouter/helpers.py` (`capture`, `_expand_tilde`),
  `localrouter/menus/local_menus.py` (the only caller),
  `LocalRouter/launch.sh` (the remote/container reference mapping this module
  "mirrors"), `LocalRouter/recipes.toml` (`provider = "local"` recipes).

This module is the whole "run a model on *this* laptop" story: discover
binaries + weights, translate a recipe into a `llama-server` command line,
spawn/supervise/stop it, and record enough state on disk that a later process
can find it again.

---

## 1. Constants and on-disk layout

From `config.py`:

| Constant | Value | Meaning |
|---|---|---|
| `ROOT` | parent dir of the `localrouter/` package (= the LocalRouter repo dir) | cwd for spawned children; home of `.active_endpoint` |
| `PROVIDER_DIR` | `~/.vastai-gguf` | provider state root |
| `LOCAL_INSTANCES` | `~/.vastai-gguf/local_instances` | pidfiles + per-instance metadata JSON |
| `LOCAL_LOGS` | `~/.vastai-gguf/local_logs` | one `<name>.log` per instance |
| `LOCAL_PID_SUFFIX` | `".pid"` | pidfile suffix |

Files created:

```
~/.vastai-gguf/local_instances/<name>.pid    # decimal PID, no trailing newline
~/.vastai-gguf/local_instances/<name>.json   # instance metadata (see §6)
~/.vastai-gguf/local_logs/<name>.log         # stdout+stderr, truncated on each start
<ROOT>/.active_endpoint                      # currently-selected endpoint (any provider)
```

`_ensure_local_dirs()` does `mkdir -p` on `LOCAL_INSTANCES` and `LOCAL_LOGS`;
called by `start_local_instance` and `list_local_instances` (**not** by
`stop_local_instance` / `is_local_running`).

Real machine state right now:

```
~/.vastai-gguf/local_instances/local-qwen35-9b.json   (status "stopped")
~/.vastai-gguf/local_logs/local-qwen35-9b.log         (16 KB of a real Vulkan run)
<ROOT>/.active_endpoint                               (absent)
```

**Port decision**: `~/.vastai-gguf` is a Vast.ai-era name. ApexRouter-RS should
use an XDG path (`$XDG_STATE_HOME/apexrouter` or `~/.local/state/apexrouter`)
but must be able to *read* the legacy dir for migration, and must keep the
`<name>.pid` / `<name>.json` / `<name>.log` file shapes so a half-migrated
system still works.

---

## 2. `discover_local(models_dir=None) -> {binaries, models, backends}`

### 2.1 Binary probing

Search bases, in order:

1. `~/llama.cpp`
2. `~/Projects/llama.cpp`
3. `/usr/local/bin`
4. every entry of `$PATH`, split on `:` (appended in `$PATH` order)

For **each** base, four candidates are tested in this fixed order:

```
<base>/build/bin/llama-server
<base>/build-vulkan/bin/llama-server
<base>/build-rocm/bin/llama-server
<base>/llama-server
```

A candidate counts if `exists() and is_file()`. The path is `resolve()`d
(symlinks followed) and deduped in a `set`. Each hit becomes
`{"path": <resolved>, "label": <basename>}` — `label` is therefore always the
literal string `"llama-server"`, which makes the TUI list three identical
labels. **Fix in the port**: label should be the build-dir name
(`build-vulkan`, `build-rocm`, …) plus the detected backend.

Discovery order matters: `result["binaries"][0]` is the fallback binary in
`start_local_instance`.

**Ground truth on this laptop** (Ryzen AI 5 340 / Radeon 840M, Vulkan works,
ROCm does not):

| Path | exists | discovered? | actual backend | notes |
|---|---|---|---|---|
| `~/llama.cpp/build/bin/llama-server` | yes (9.4 MB, Apr 19) | **yes — index 0** | HIP/ROCm (`libggml-hip.so`) | this is the default pick, and it is the *worst* backend for this box |
| `~/llama.cpp/build-vulkan/bin/llama-server` | yes (8.3 MB, May 17) | yes — index 1 | Vulkan (`libggml-vulkan.so`) | the one that actually works |
| `~/llama.cpp/build-rocm/bin/llama-server` | yes (17.9 KB stub → `libllama-server-impl.so`) | yes — index 2 | HIP/ROCm | newer split-shared-lib layout |
| `~/llama.cpp/build-mtp/bin/llama-server` | yes (8.3 MB) | **no** | HIP | dir name not in candidate list |
| `~/llama.cpp/build-zaya1/bin/llama-server` | yes (9.5 MB) | **no** | Vulkan | dir name not in candidate list |
| `/usr/local/bin/llama-server` | no | n/a | | |
| anything on `$PATH` | no | n/a | | `which llama-server` → not found |

So the hard-coded candidate list misses two real builds and ranks a
non-functional-on-this-hardware ROCm build first. **Port requirement**: glob
`<base>/build*/bin/llama-server` instead of a fixed list, and rank by *probed*
backend, not by directory order.

### 2.2 Backend detection (broken — do not port as-is)

Only the **first** binary is probed:

```python
help_out, _, rc = capture(f"{main_bin} --help 2>&1", timeout=5)
if rc == 0 and help_out:
    if "vulkan" in help_out.lower() or "--gpu-vk" in help_out.lower(): backends.append("vulkan")
    if "cuda"   in help_out.lower() or "--gpu"    in help_out.lower(): backends.append("cuda")
    if "hip"    in help_out.lower() or "rocm"     in help_out.lower(): backends.append("rocm")
    result["backends"] = backends if backends else ["cpu"]
```

Measured against the real binaries here (635/641 lines of `--help`):

| substring | occurrences in `llama-server --help` |
|---|---|
| `vulkan` | **0** |
| `cuda` | **0** |
| `hip` | **0** |
| `rocm` | **0** |
| `--gpu` | ≥1 — from `-ngl, --gpu-layers, --n-gpu-layers` |

Result: on this machine `discover_local()` reports `backends == ["cuda"]`, on a
laptop with no NVIDIA hardware at all. It never reports `vulkan`, never reports
`cpu`. The TUI prints the first backend in green as "the" backend. Also, if the
first binary fails to run, `result["backends"]` stays `[]` (the `["cpu"]`
fallback is inside the `rc == 0` branch, so it never fires on failure).

**Port requirement — replace with real probing.** Options, best first:

1. `llama-server --list-devices` — authoritative. On this box:
   `Vulkan0: AMD Radeon 840M Graphics (RADV KRACKAN1) (20992 MiB, 19519 MiB free)`.
   Gives device name *and* free VRAM, which is worth surfacing on a
   memory-constrained laptop.
2. inspect the `libggml-*.so` siblings in the binary's `bin/` dir
   (`libggml-vulkan.so` → vulkan, `libggml-hip.so` → rocm, only
   `libggml-cpu.so` → cpu). Cheap, no exec.
3. `ldd`/`readelf -d` on the binary.

Probe **every** binary, not just the first, and cache the result keyed by
`(path, mtime)`.

### 2.3 The RUNPATH trap (machine-specific, must be handled)

```
$ readelf -d build-vulkan/bin/llama-server | grep RUNPATH
  RUNPATH  [/home/andre/llama.cpp/build-vulkan/bin:]
```

Note the **trailing colon** — an empty RUNPATH entry, which `ld.so` interprets
as the current working directory. Measured:

| cwd when exec'ing `build-vulkan/bin/llama-server --list-devices` | result |
|---|---|
| `~` | OK |
| `~/Projects/Inference/tools/LocalRouter` (= `ROOT`) | OK |
| `~/llama.cpp/build-vulkan/bin` | OK |
| `~/llama.cpp/build-rocm/bin` | **`symbol lookup error: undefined symbol: _Z23common_init_from_paramsR13common_params`** |

Running from a *sibling build's* bin dir loads that build's
`libllama-common.so.0` (0.0.9496 vs 0.0.9199) and the binary dies. The Python
code happens to dodge this because `capture()` and `Popen` both use
`cwd=ROOT`.

**Port requirement**: never probe or spawn a llama.cpp binary with cwd set to
some other build's `bin/`. Either set cwd to the binary's own directory, or
explicitly set `LD_LIBRARY_PATH=<dirname(binary)>` in the child env. Prefer the
latter — it is deterministic regardless of cwd.

### 2.4 Model (GGUF) discovery

Scan dirs, in order:

1. `models_dir` argument, `expanduser()`'d — **dead parameter**: no caller ever
   passes it. `menus/local_menus.py` calls `discover_local()` bare, and
   `start_local_instance` calls it bare. `recipes.toml` has an empty `[local]`
   table and the config menu *prints instructions* to add `models_dir` there,
   but nothing reads it.
2. `~/models`
3. `~/.cache/huggingface/hub`

Algorithm: `d.rglob("*.gguf")` per dir; skip if the lowercased **full path**
contains `mmproj` or `vocab`; `resolve()`; dedupe by resolved path; record
`{"path", "name" (basename), "size_mb" (round(bytes/1MiB, 1))}`. `PermissionError`
during a walk is swallowed for that whole directory (`try` wraps the entire
loop, so one unreadable subdir aborts the rest of that root). Finally sorted by
`-size_mb` (largest first) — the comment says "larger = usually more
interesting", which on a 24 GB shared-memory laptop is precisely backwards.

**No multi-part shard handling.** `launch.sh` handles splits
(`find … | grep -iE "$MODEL_QUANT" | grep -v mmproj | sort | head -n1`, i.e.
pick the lowest-numbered shard and let llama.cpp find the rest), but
`discover_local` lists every `…-00001-of-00003.gguf`, `…-00002-of-00003.gguf`,
… as separate models. Benchmarks in `~/models/results/` reference
`qwen2.5-7b-instruct-q4_k_m-00001-of-00002`, so shards have existed here.

**Symlinks are not followed into.** Python 3.14's `Path.rglob` defaults to
`recurse_symlinks=False`, so
`Inference/resources/models/ternary-bonsai-27b -> ../../stacks/Bonsai-demo/models/ternary-gguf/27B`
is invisible even if `~/models` linked to it. (Individual symlinked *files*
would still be found and `resolve()`d.)

**Ground truth on this laptop:**

```
~/models/
  bench_vulkan_cpu.py
  carnice-9b/Carnice-9b-Q6_K.gguf          7,359,259,424 B  → 7017.4 MB   ← only GGUF here
  qwen36-35b-a3b/                           (empty except .cache/huggingface)
  results/*.json|*.md|*.svg                 benchmark output, no GGUF
~/.cache/huggingface/hub/                   ~20 repos, all AMD RyzenAI **ONNX/NPU** models
                                            (no .gguf) + BAAI/bge-small, Qwen2.5-0.5B
```

Not scanned but present:
`~/Projects/Inference/stacks/Bonsai-demo/models/ternary-gguf/27B/` containing
`Ternary-Bonsai-27B-Q2_0.gguf` (7.2 GB),
`Ternary-Bonsai-27B-dspark-Q4_1.gguf` (1.9 GB) and two `*-mmproj-*.gguf`
projectors — a real vision-capable local stack the current discovery cannot
see.

**Port requirements** for model discovery:

- configurable scan roots (default: `~/models`,
  `~/Projects/Inference/resources/models`, `~/Projects/Inference/stacks/*/models`,
  `~/.cache/huggingface/hub`), with the `models_dir` override actually wired up;
- follow directory symlinks (deliberately, with a visited-inode set to avoid
  loops) — `resources/models/*` is a symlink farm by design here;
- **group shards**: detect `-(\d{5})-of-(\d{5})\.gguf$`, collapse to one logical
  model whose `path` is shard 00001, whose `size` is the sum, and which knows
  its shard count;
- keep `mmproj` files out of the model list but **surface them separately** and
  auto-pair them with the sibling model (basename prefix match) so
  `--mmproj` can be offered without hand-editing a recipe;
- skip on filename token `mmproj`/`vocab`, not on any substring of the full
  path (a user directory named `~/models/vocab-experiments/` currently hides
  everything under it);
- record free RAM/VRAM vs model size so the TUI can warn before OOM (24 GB
  unified, iGPU shares it).

---

## 3. Recipe schema (`provider = "local"`)

Union of keys across the three local recipes in `recipes.toml`, plus keys the
code reads but no recipe sets:

| Key | Type | Default in code | Read by | Notes |
|---|---|---|---|---|
| `name` | str | `"local-default"` | lifecycle | pidfile/log/meta filename — **used unsanitised as a path component** |
| `provider` | str | — | menu filter | must be `"local"` |
| `label` | str | falls back to `name` | menu | display only |
| `description` | str | — | menu | display only |
| `model_path` | str | `""` | args | `~` expanded + `resolve()`d; must exist or start aborts |
| `mmproj` | str | `""` | args | expanded; **silently ignored if the file is missing** |
| `host` | str | `"127.0.0.1"` | args, meta, `.active_endpoint` | |
| `port` | int | `8100` | args, meta, `.active_endpoint` | no free-port check, no collision check across recipes (8100/8101/8102 by convention) |
| `ctx` | int | `32768` | args | `int()`-coerced |
| `parallel` | int | `1` | args | `int()`-coerced |
| `kv_type` | str | `"q8_0"` | args | applied to **both** K and V |
| `n_gpu_layers` | int | `999` | args | `int()`-coerced |
| `mode` | str | `"thinking"` | args | key into `SAMPLING_PRESETS`; unknown value ⇒ **no sampling flags at all**, silently |
| `backend` | str | `""` | binary pick, env | lowercased; `"vulkan"` / `"rocm"` / `"hip"` recognised |
| `binary` | str | `""` | binary pick | explicit override; skips discovery |
| `api_key` | str | `""` | `.active_endpoint` only | **never passed to llama-server** — see §5 |

`launch.sh` additionally supports `EXTRA_ARGS` passthrough; the local path has
no equivalent. **Port**: add `extra_args: Vec<String>` and an `alias` field.

---

## 4. Recipe → `llama-server` argv (`_get_local_server_args`)

Emitted in exactly this order. `argv[0]` is the binary path.

| # | Flag | Value | Condition | Source |
|---|---|---|---|---|
| 1 | *(argv0)* | `<binary>` | always | resolved binary |
| 2 | `--model` | `_expand_tilde(recipe.model_path)` | always; **aborts with `Model not found: <raw>` if missing** | |
| 3 | `--host` | `recipe.host` or `127.0.0.1` | always | |
| 4 | `--port` | `recipe.port` or `8100` | always | |
| 5 | `--ctx-size` | `int(recipe.ctx)` or `32768` | always | |
| 6 | `--parallel` | `int(recipe.parallel)` or `1` | always | |
| 7 | `--cache-type-k` | `recipe.kv_type` or `q8_0` | always | |
| 8 | `--cache-type-v` | same value as `--cache-type-k` | always | |
| 9 | `--n-gpu-layers` | `int(recipe.n_gpu_layers)` or `999` | always | |
| 10 | `--jinja` | *(bare)* | always | |
| 11 | `--metrics` | *(bare)* | always | |
| 12 | `--flash-attn` | `on` | always | note: **value form**, unlike `launch.sh` |
| 13 | `--temp` | preset | `mode ∈ SAMPLING_PRESETS` | §7 |
| 14 | `--top-p` | preset | same | |
| 15 | `--min-p` | preset | same | |
| 16 | `--presence-penalty` | preset | same | |
| 17 | `--chat-template-kwargs` | `{"enable_thinking":false}` | `mode == "nonthinking"` only | one argv element, raw JSON, no shell quoting (Popen list form) |
| 18 | `--mmproj` | `_expand_tilde(recipe.mmproj)` | `recipe.mmproj` truthy **and** file exists | |

Return contract: `(args: list[str] | None, err: str | None)` — exactly one of
the two is non-`None`.

### 4.1 Verified against the installed binary

`build-vulkan/bin/llama-server --help` (build `b8960-19821178b`-era) confirms
every flag above:

```
-c,   --ctx-size N
-np,  --parallel N                    number of server slots (default: -1, -1 = auto)
-ctk, --cache-type-k TYPE             f32,f16,bf16,q8_0,q4_0,q4_1,iq4_nl,q5_0,q5_1  (default f16)
-ctv, --cache-type-v TYPE             same set
-ngl, --gpu-layers, --n-gpu-layers N  exact number, 'auto', or 'all' (default: auto)
-fa,  --flash-attn [on|off|auto]      (default 'auto')
-m,   --model FNAME
      --host HOST / --port PORT       (port default 8080)
      --jinja, --no-jinja             (default: ENABLED)
      --metrics                       (default: disabled)
      --chat-template-kwargs STRING
      --temp N / --top-k N / --top-p N / --min-p N / --presence-penalty N
      --mmproj FILE  (+ --mmproj-auto / --no-mmproj / --mmproj-offload)
-a,   --alias STRING
      --api-key KEY / --api-key-file FNAME
      --slots / --no-slots            (default: enabled)
      --props                         (default: disabled)
-dev, --device <dev1,dev2,..>         --list-devices to enumerate
-fit, --fit [on|off]                  auto-shrink unset args to fit device memory (default ON)
-fitc,--fit-ctx N                     min ctx --fit may choose (default 4096)
```

Two things worth acting on in the port:

- `--jinja` is now the **default**, so flag 10 is a no-op on current builds.
  Harmless; keep it for older binaries, or emit only when a
  `no_jinja: false` is not set.
- `--fit on` is the new default and it *rewrites unset params to fit VRAM*. The
  real log here shows it working
  (`common_params_fit_impl: projected to use 5956 MiB … will leave 13990 >= 1024 MiB`).
  Because the recipe always passes explicit `--ctx-size` and `--n-gpu-layers`,
  `--fit` cannot shrink those. On a 24 GB shared-memory laptop the port should
  expose `--fit`/`--fit-ctx`/`--fit-target` as recipe fields, and consider
  *omitting* `--ctx-size`/`--n-gpu-layers` when the recipe leaves them unset
  rather than substituting 32768/999.

### 4.2 Divergences from `launch.sh` (the "mirror" it claims to be)

| | `launch.sh` (container) | `_get_local_server_args` |
|---|---|---|
| `--top-k 20` | **present** in all three presets | **absent** — real behavioural difference, top-k falls back to llama.cpp default 40 |
| `--flash-attn` | bare (old CLI) | `--flash-attn on` |
| `--ctx-size` default | `65536` | `32768` |
| `--port` default | `8000` | `8100` |
| `EXTRA_ARGS` | passthrough supported | not supported |
| model resolution | `find … | grep -iE "$QUANT" | grep -v mmproj | sort | head -1` (shard-aware) | literal `model_path` only |
| mmproj missing | `die` | silently dropped |
| unknown `MODE` | `die` | silently emits no sampling flags |

**Port decision**: unify on one arg-builder used by both the local and remote
paths, with `top_k` restored, and make "unknown mode" and "mmproj missing"
hard errors.

---

## 5. `start_local_instance(recipe) -> (bool, str)`

Sequence:

1. `_ensure_local_dirs()`.
2. `name = recipe.name or "local-default"`.
3. **Already-running guard**: if `<name>.pid` exists → parse int → `os.kill(pid, 0)`.
   - alive → print "already running", return `(False, "Already running")`
   - `ProcessLookupError` or `ValueError` → `unlink()` the stale pidfile and continue
   - `PermissionError` (PID owned by another user) is **not caught** and
     propagates out of `start_local_instance` — a crash path. (`helpers.tunnel_running`
     handles this case correctly; this function does not.)
   - No PID-reuse validation (no start-time / cmdline check).
4. **Binary selection**: if `recipe.binary` is empty, run `discover_local()` and:
   ```python
   for b in disc["binaries"]:
       bp = b["path"].lower()
       if "vulkan" in target_backend and "vulkan" in bp:
           preferred = b["path"]; break
       elif "rocm" in target_backend or "hip" in target_backend:
           if "rocm" in bp or (preferred is None):
               preferred = b["path"]          # note: no break
   binary = preferred or disc["binaries"][0]["path"]
   ```
   - `backend = "vulkan"` → picks the first path containing `vulkan` → correct here.
   - `backend = "rocm"`/`"hip"` → the `or preferred is None` clause latches the
     **first binary of any kind** on the first iteration, then keeps overwriting
     with later rocm hits; effectively "last rocm binary, else first binary".
   - `backend = "cpu"`, `""`, or anything else → `preferred` stays `None` →
     `binaries[0]` → **`~/llama.cpp/build/bin/llama-server`, the HIP build**, on a
     machine where ROCm does not work. Silent wrong-backend selection.
   - no binaries at all → `(False, "No llama-server binary found. Run 'Local → Configure' to scan.")`
5. `args, err = _get_local_server_args(recipe, binary)`; on `err` → `(False, err)`.
6. **Child env** = `os.environ.copy()` plus:
   | condition | var |
   |---|---|
   | `"vulkan" in backend` | `GGML_VK_VISIBLE_DEVICES=0` |
   | `"rocm" in backend` or `"hip" in backend` | `HIP_VISIBLE_DEVICES=0` |

   Nothing else. No `LD_LIBRARY_PATH` (see §2.3), no `GGML_VK_DISABLE_F16`, no
   `RADV_PERFTEST`, no thread pinning, no `AMD_VULKAN_ICD`.
7. **Spawn**:
   ```python
   log_fh = open(LOCAL_LOGS / f"{name}.log", "w")     # truncates previous log
   proc = subprocess.Popen(args, stdout=log_fh, stderr=subprocess.STDOUT,
                           cwd=str(ROOT), env=env)
   ```
   - `FileNotFoundError` → close fh → `(False, f"Binary not found: {binary}")`
   - any other exception → close fh → `(False, str(e))`
   - **`log_fh` is never closed on the success path** — fd leak in the parent,
     one per start, for the life of the TUI.
   - **No `start_new_session` / `setsid`**: the child stays in the TUI's process
     group, so Ctrl-C in the TUI SIGINTs the model server too, and the server
     dies with the terminal.
   - No `nice`/`ionice`, no rlimits.
8. Write `<name>.pid` (`str(proc.pid)`, no newline).
9. Write `<name>.json` metadata with `status: "starting"` (§6).
10. Write `<ROOT>/.active_endpoint` (§6).
11. **Health-check poll loop** — `for i in range(60)`:
    1. `time.sleep(1)` **first** (so there is always ≥1 s latency),
    2. `proc.poll()` → if the process exited, read the log, return
       `(False, f"Process exited immediately. Last log: {log[-500:].strip()}")`
       — note the pidfile, metadata (`status: "starting"`) and
       `.active_endpoint` are all left behind pointing at a dead PID,
    3. `GET http://{host}:{port}/v1/models` via `urllib.request`, `timeout=3`;
       any exception → `continue`,
    4. HTTP 200 → rewrite metadata with `status: "running"`, print the success
       panel, return `(True, "Started successfully")`.
    - "60 seconds max" is wrong: each iteration is 1 s sleep **plus** up to 3 s
      of connect/read timeout, so the wall clock can reach ~4 min. On this
      laptop a 7 GB Q6_K model from cold page cache takes well over 60 s to
      load, so this timeout is genuinely reachable.
    - The probe sends no `Authorization` header. It only works because
      `--api-key` is never passed (§4); if it were, `/v1/models` would 401 and
      the health check would always time out.
12. Fall-through → `(False, f"Health check timed out after 60s. Check log: {log_file}")`,
    **leaving the process running** with `status: "starting"` on disk and a
    live `.active_endpoint`. The instance is now an orphan the TUI reports as
    "starting" forever until someone stops it.

**Port requirements**:

- `setsid` / new process group + detach, so the server survives the TUI;
- close the log fd in the parent (or hand the pipe to a logging task);
- write the pidfile **after** a successful spawn *and* record `start_time`
  (`/proc/<pid>/stat` field 22) so PID reuse can be detected;
- health-check budget as a real deadline (`Instant` + total timeout), with the
  per-request timeout separate; probe `/health` first (llama.cpp exposes it) and
  fall back to `/v1/models`;
- on both failure paths: kill the child, remove pidfile, mark metadata
  `status: "failed"` with the captured log tail, and clear `.active_endpoint`
  if it points at this instance;
- stream the last N log lines into the failure message instead of a raw
  500-byte slice (which can cut mid-UTF-8; Python's `read_text()[-500:]` slices
  *characters* so it is safe there, but a naive Rust `&s[len-500..]` would
  panic — slice on char boundaries);
- verify the port is free before spawning (bind test), since nothing else does.

---

## 6. Instance metadata + `.active_endpoint`

`<name>.json` — written twice (at spawn with `status:"starting"`, again on
health-check success with `status:"running"`), `json.dumps(..., indent=2)`:

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

(verbatim from `~/.vastai-gguf/local_instances/local-qwen35-9b.json`)

Field notes:

- `model_path` is the **raw, unexpanded** recipe value (`~/…`), while `--model`
  got the expanded one. Consumers must expand again.
- `backend` is `recipe.backend.lower()` or the literal `"auto"` if unset — but
  the actual binary chosen may not match (§5.4).
- `started_at` / `stopped_at`: `time.strftime("%Y-%m-%dT%H:%M:%SZ")` — this is
  **local time with a `Z` suffix**, i.e. a lying timestamp. Port must use real
  UTC (or RFC3339 with offset).
- `status` ∈ `{"starting", "running", "stopped"}`; `"failed"` does not exist.
- `stopped_at` only appears after `stop_local_instance`.
- No `ctx`, `kv_type`, `mode`, `parallel`, `n_gpu_layers`, or the actual argv —
  so nothing on disk records *how* the server was launched. **Port: store the
  full argv and the resolved env overrides**; it makes restart-after-reboot and
  "why is this slow" debugging possible.

`<ROOT>/.active_endpoint` — written on start, deleted on stop:

```json
{
  "provider": "local",
  "name": "<name>",
  "host": "127.0.0.1",
  "port": 8100,
  "pid": 12345,
  "model_path": "<raw recipe value>",
  "activated_at": "<same fake-UTC stamp>",
  "api_key": "<only if recipe.api_key is non-empty>"
}
```

Consumed by `providers.get_active_endpoint()`: if `provider == "local"` it
calls `is_local_running(name)` and injects `status: "running"|"stopped"` into
the returned dict. Other providers write `provider: "together"` (with an
`endpoint` URL) to the same file, and a missing file falls back to the Vast
instance. The port must keep this file's schema — `endpoint_proxy.py`,
`proxy.py`, the tool menus and `smoke.sh` all read it.

Note the base URL is only ever *implied* (`http://{host}:{port}/v1`). The port
should write an explicit `base_url` field alongside host/port.

---

## 7. Sampling presets

`config.SAMPLING_PRESETS` — a `dict[str, list[str]]` spliced verbatim into argv:

| mode | `--temp` | `--top-p` | `--min-p` | `--presence-penalty` | extra |
|---|---|---|---|---|---|
| `thinking` (default) | `1.0` | `0.95` | `0.0` | `1.5` | — |
| `coding` | `0.6` | `0.95` | `0.0` | `0.0` | — |
| `nonthinking` | `0.7` | `0.80` | `0.0` | `1.5` | `--chat-template-kwargs {"enable_thinking":false}` |

`launch.sh` uses the same numbers **plus `--top-k 20`** in every mode. The
missing `--top-k` in the local path is the one real semantic divergence — with
llama.cpp's default `top_k = 40`, local "thinking" mode is measurably looser
than the container "thinking" mode.

`config.MODES` maps human labels to these keys for the launch wizard;
`config.KV_TYPES` maps labels to `q8_0` / `q4_0` / `bf16` (all valid
`--cache-type-*` values per `--help`; note `f16` is the llama.cpp default and
is *not* offered).

An unrecognised `mode` yields **no sampling flags whatsoever** and no warning.

---

## 8. `stop_local_instance(name) -> (bool, str)`

1. No `<name>.pid` → `(False, f"No PID file for '{name}'. Not running?")`.
2. `pid = int(pidfile)`; `os.kill(pid, SIGTERM)`.
3. Escalation loop — `for _ in range(10)`:
   - `os.kill(pid, 0)`; if it succeeds, `time.sleep(0.5)` and loop
   - `ProcessLookupError` → `break` (graceful exit confirmed)
   - loop exhausts without break → `else:` → `os.kill(pid, SIGKILL)`
     (swallowing `ProcessLookupError`)
   - Max graceful wait: **5 s**. A 7 GB model with a dirty KV cache can take
     longer; llama.cpp handles SIGTERM promptly, so 5 s is usually fine, but
     the port should make it configurable and wait for actual exit after
     SIGKILL too (this code does not).
4. `pidfile.unlink()`.
5. Metadata update (best-effort, all exceptions swallowed): load `<name>.json`,
   set `status: "stopped"`, add `stopped_at`, rewrite.
6. Clear `<ROOT>/.active_endpoint` **only if** it parses and
   `provider == "local"` and `name` matches.
7. Return `(True, f"Stopped '{name}' (PID {pid})")`.

Error paths: `ValueError` (garbage pidfile) → unlink, `(False, "Invalid PID file")`.
`ProcessLookupError` from the initial SIGTERM → unlink,
`(True, f"'{name}' was already stopped")`. `PermissionError` is again uncaught.

**No child reaping.** The `Popen` object from `start_local_instance` is long
gone, so after SIGTERM the server becomes a zombie owned by the TUI until the
TUI itself exits. In Rust: keep the `Child` handle (or double-fork/`setsid` so
init reaps it) and `waitpid` after signalling.

**PID reuse**: nothing verifies the PID still belongs to a llama-server. A
stale pidfile whose number has been recycled will get SIGTERM'd — the port must
compare `/proc/<pid>/comm` (or `cmdline`) and the process start time before
signalling.

---

## 9. Query functions

`is_local_running(name) -> bool`

- no pidfile → `False`
- `int()` + `os.kill(pid, 0)` → `True`
- `ProcessLookupError` / `ValueError` → **unlinks the pidfile** and returns
  `False`. A read-only-looking predicate with a destructive side effect, called
  from render loops (`menu_local_launch` calls it per recipe on every draw) and
  from `providers.get_active_endpoint()`.
- `PermissionError` uncaught (again).

`list_local_instances() -> list[dict]`

- `_ensure_local_dirs()`, then `sorted(LOCAL_INSTANCES.glob("*.json"))`
- per file: parse JSON, `name = data.name or file.stem`,
  `data["running"] = is_local_running(name)`,
  and if not running and `status == "starting"` → downgrade to `"stopped"`
  **in memory only** (never persisted, so a crashed instance shows "starting"
  in the file forever)
- any parse error → that file is silently skipped.

**Port**: make `is_local_running` pure; add a separate explicit
`reap_stale_pidfiles()`. Persist the starting→stopped downgrade. Distinguish
"metadata unparseable" from "no such instance" in the UI.

---

## 10. Callers / blast radius

Only `localrouter/menus/local_menus.py` and `localrouter/providers.py` import
this module.

- `menu_local_config()` → `discover_local()` twice (once per draw, once on
  "Refresh"); renders binaries[:3], backends, models[:3]. Offers "Set models
  dir" which **only prints a suggestion** — it changes nothing.
- `menu_local_status()` → `list_local_instances()`, per-instance "View logs"
  (`log[-2000:]`) and "Stop instance" → `stop_local_instance`.
- `menu_local_launch(recipes)` → filters `provider == "local"`, refuses if
  `discover_local()["binaries"]` is empty, shows a table + a confirm panel,
  then `start_local_instance(recipe)`.
- `menu_local_dispatch()` → umbrella; calls `load_config()` a second time
  purely to count local recipes for a menu label.
- `providers.get_active_endpoint()` → `is_local_running` (lazy import to break
  a circular dependency — in Rust this is just a module boundary).

Unused import in the module: `PROVIDER_DIR` (imported, never referenced).

---

## 11. Rust port checklist

Behaviour to keep bit-compatible:

- [ ] pidfile / metadata / log filenames and locations (legacy read path)
- [ ] `.active_endpoint` JSON schema incl. the optional `api_key` key
- [ ] instance metadata field names and `status` vocabulary (extend, don't rename)
- [ ] recipe key names and defaults (`ctx` 32768, `port` 8100, `kv_type` q8_0,
      `n_gpu_layers` 999, `mode` thinking, `parallel` 1, `host` 127.0.0.1)
- [ ] flag order and spellings from §4 (external contract with `llama-server`)
- [ ] the three sampling presets' numeric values

Behaviour to fix while porting:

- [ ] backend detection via `--list-devices` / `libggml-*.so`, per binary, not `--help` substrings
- [ ] glob `build*/bin/llama-server`; rank Vulkan first on this hardware, never default to a HIP build
- [ ] set `LD_LIBRARY_PATH` to the binary's own dir (RUNPATH trailing-colon trap, §2.3)
- [ ] shard grouping + mmproj pairing in model discovery; follow symlinks; wire up `models_dir`
- [ ] restore `--top-k 20`; add `extra_args`, `alias`, `api_key` → `--api-key`, `--fit*` fields
- [ ] `setsid`, no leaked log fd, reap the child, PID+start_time validation before signalling
- [ ] real UTC timestamps
- [ ] real deadline on the health loop, `/health` before `/v1/models`, cleanup on failure/timeout
- [ ] hard-error (not silent skip) on unknown `mode` and missing `mmproj`
- [ ] `is_local_running` pure; explicit stale-pidfile reaper
- [ ] catch `PermissionError`-equivalent (`EPERM` from `kill(pid, 0)` ⇒ process exists)
- [ ] port-in-use pre-check before spawn

Hardware reality to encode as defaults (Ryzen AI 5 340 / Radeon 840M
"KRACKAN1", RADV, 24 GB shared):

- Vulkan is the working GPU backend; ROCm/HIP is present in three build dirs
  and must never be auto-selected.
- `--list-devices` reports `Vulkan0 … (20992 MiB, 19519 MiB free)` — the iGPU
  carves from system RAM, so "free VRAM" and "free RAM" are the same pool.
  Check `free -h` *and* the Vulkan report before launching.
- Real observed footprint from the archived log: Qwen3.5-9B Q4_K_M, ctx 32768,
  kv q8_0 → 5956 MiB device (4861 model + 594 context + 501 compute) + 625 MiB
  host. Prompt processing ran at ~2048-token batches.
- llama.cpp's `--fit on` already does memory-aware shrinking; prefer leaning on
  it over hard-coded `999` layers.
