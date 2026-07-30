# 04 — Vast.ai rented-GPU workflow (port spec)

Source of truth (LocalRouter, Python + bash):

| File | Role |
|---|---|
| `tools/LocalRouter/localrouter/vast_ops.py` (234 L) | offer browser, net diagnostics, container env read, stalled-download restart |
| `tools/LocalRouter/localrouter/helpers.py` (138 L) | `capture` / `run` / `get_instance_json` / `get_ssh` / `ssh_run` / `last_instance` / `tunnel_running` |
| `tools/LocalRouter/localrouter/config.py` (116 L) | paths, ports, `GEOS`/`MODES`/`KV_TYPES`, recipe loader |
| `tools/LocalRouter/localrouter/menus/vast_menus.py` (545 L) | launch wizard, tunnel menu, destroy, instance list, **boot watcher** |
| `tools/LocalRouter/localrouter/menus/tool_menus.py` (578 L) | status panel, **deep diagnostics + stall detection/recovery**, smoke launcher |
| `tools/LocalRouter/vast_up.sh` (293 L) | offer search → `vastai create instance` → record ID |
| `tools/LocalRouter/vast_down.sh` (8 L) | destroy |
| `tools/LocalRouter/tools/vast_tunnel.sh` (189 L) | SSH tunnel up/status/down/logs |
| `tools/LocalRouter/launch.sh` (189 L) | **in-container** entrypoint: llama.cpp path |
| `tools/LocalRouter/launch_vllm.sh` (125 L) | **in-container** entrypoint: vLLM path |
| `tools/LocalRouter/smoke.sh` (126 L) | OpenAI-compat endpoint smoke test |
| `tools/LocalRouter/recipes.toml` | `[docker]`, `[gpu_tiers.*]`, `[[recipes]]` |
| `tools/LocalRouter/Dockerfile{,.builder,.vllm}` | the three images the workflow rents |

---

## 0. Lifecycle at a glance

```
recipes.toml ──▶ launch wizard (TUI)
                     │  builds env dict
                     ▼
              bash vast_up.sh          [local]
                     │  vastai search offers  (×1–3)
                     │  vastai create instance
                     │  writes .last_instance + .instance_history
                     ▼
            Vast provisions container   [remote]
                     │  docker pull ghcr.io/buckster123/vastai-gguf:{prebuilt,builder,vllm}
                     │  onstart-cmd:  ENV... bash /app/launch.sh > /var/log/launch.log 2>&1 &
                     ▼
              launch.sh                 [remote]
                     │  (builder only) nvidia-smi → SM arch → cmake/ninja compile llama.cpp
                     │  hf download <repo> --include "*QUANT*.gguf"
                     │  exec llama-server --host 127.0.0.1 --port 8000 ...
                     ▼
        boot watcher (10 s poll)       [local]  vastai show instance + ssh tail -1 launch.log
                     ▼
        vast_tunnel.sh up              [local]  ssh -f -N -L 8800:127.0.0.1:8000
                     ▼
        smoke.sh http://127.0.0.1:8800 [local]
                     ▼
        vast_down.sh                   [local]  echo y | vastai destroy instance
```

Security posture that must be preserved: **the model server never binds a public
interface.** `HOST=127.0.0.1` is forced in the create-time env *and* re-forced on
every stall-restart. All access is through the SSH tunnel. `-p 8000:8000` is
still declared to Vast (so the port exists in the container spec) but nothing
listens on the external side.

---

## 1. Local state files (exact paths)

| Path | Written by | Content |
|---|---|---|
| `<ROOT>/.last_instance` | `vast_up.sh`, `menu_instances()` | bare instance ID, no newline guarantee (`echo` adds one; `LAST_INST.write_text(new_id)` does not) |
| `<ROOT>/.instance_history` | `vast_up.sh` | append-only TSV: `printf '%s\t%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "${INST_ID}"` |
| `<ROOT>/.hf_pin` | HF browser | JSON `{MODEL_REPO, MODEL_QUANT, size}`; **deleted after a successful launch** |
| `/tmp/vastai-gguf-tunnel.pid` | `vast_tunnel.sh up` | SSH tunnel PID |
| `/tmp/vastai-gguf-tunnel.ssh` | `vast_tunnel.sh up` | ephemeral `ssh_config` (see §7) |
| `/tmp/qwen36-vast-cm` | ssh ControlMaster | control socket (removed by `down`) |
| `/tmp/vastai-gguf-proxy.pid` | proxy | consulted by smoke menu only |
| `~/.cache/huggingface/token` | user | read verbatim into `HF_TOKEN` env |
| `~/.vastai-gguf/config.toml` | provider config | Together key etc. (not Vast) |

`ROOT` = `Path(__file__).parent.parent.resolve()` = the LocalRouter project dir.
Both shell scripts use **relative** `.last_instance`, so they are only correct
when `cwd == ROOT`. Python always passes `cwd=ROOT`. **Port note: make this an
explicit absolute state dir, not CWD-relative.**

Ports: `LOCAL_PORT = 8800` (tunnel local side), remote `8000`, `PROXY_PORT = 8888`.

---

## 2. External binaries invoked

`vastai`, `jq`, `ssh`, `pgrep`, `kill`, `curl`, `bash`, `cat`, `date`, `printf`.
Remote-side (inside the container): `nvidia-smi`, `git`, `cmake`, `ninja`,
`install`, `hf` (huggingface_hub CLI), `find`, `grep`, `sort`, `ps`, `df`,
`tail`, `pkill`, `awk`, `llama-server`, `vllm`.

Everything local is shelled out through `helpers.capture()`:

```python
subprocess.run(cmd, shell=True, cwd=ROOT, capture_output=True, text=True, timeout=timeout)
# TimeoutExpired -> returns ("", f"timed out after {timeout}s", 124)   # never raises
```

**rc 124 == timeout** is a load-bearing convention.

---

## 3. Offer search

There are **two independent implementations with different thresholds.** Both
must be understood; the Rust port should unify them.

### 3.1 GPU name filter

Built from `gpu_tiers.<key>.vast_names` (list of Vast `gpu_name` strings):

* 1 name → `gpu_name=RTX_5090`
* ≥2 names → `gpu_name in [RTX_PRO_6000_WS,RTX_PRO_6000_S]`  (comma-joined, **no spaces**, no quotes, literal brackets)

`vast_up.sh` receives them via `VAST_NAMES` (space-separated) and does
`tr ' ' ','`; `browse_offers()` receives the list directly.

Fallbacks when `vast_names` is absent (`vast_up.sh` case block, authoritative):

```
5090|5090-dc  gpu_name=RTX_5090
4090          gpu_name=RTX_4090
6000pro       gpu_name in [RTX_PRO_6000_WS,RTX_PRO_6000_S]
h100-sxm      gpu_name in [H100_SXM,H100_SXM5,H100X]
h100-pcie     gpu_name=H100_PCIE
a100-sxm      gpu_name in [A100_SXM4_80GB,A100_SXM,A100X]
a100-pcie     gpu_name in [A100_PCIE,A100_PCIE_80GB]
h200-sxm      gpu_name in [H200_SXM,H200]
b200-sxm      gpu_name in [B200_SXM,B200]
*             gpu_name=RTX_${GPU}
```

`browse_offers()` has a smaller fallback set: only `6000pro` special-cased,
everything else `gpu_name=RTX_{gpu_key}` — so `h100-sxm` without `tier_cfg`
would produce the nonsense `gpu_name=RTX_h100-sxm`. (Callers always pass
`tier_cfg`, so it is latent, but do not reproduce it.)

### 3.2 Geo mapping (identical tables in both implementations)

```
EU_NORDIC  SE|NO|FI|DK|IS
EU         SE|NO|FI|DK|IS|DE|NL|FR|BE|UK|IE|EE|LV|LT|PL|CZ|AT|CH|ES|PT|IT
US         US
ANY        .*
*          (raw regex passed through — vast_up.sh only)
```

Geo is **never** sent to Vast. It is a client-side regex over the offer's
`geolocation` field, anchored as `", (" + RE + ")$"` — i.e. the field format is
`"City, CC"` (e.g. `"Stockholm, SE"`).

* `vast_up.sh`: `jq -r --arg re "$GEO_RE" '[.[] | select((.geolocation // "") | test(", (" + $re + ")$"))] | .[0].id // empty'`
* `browse_offers`: `re.compile(rf", ({geo_re})$")` + `pattern.search(o.get("geolocation",""))`

TUI menu labels → keys (`config.GEOS`):
`"EU Nordic   (SE/NO/FI/DK/IS)"→EU_NORDIC`, `"EU Broad    (+ DE/NL/FR/UK/...)"→EU`,
`"US"→US`, `"Any"→ANY`.

### 3.3 Query — interactive browser (`vast_ops.browse_offers`)

Verbatim shell string (single query, no widening):

```
vastai search offers "{gpu_filter} num_gpus={num_gpus} reliability>0.97 inet_down>300 dph_total<{max_price} disk_space>{min_disk} cuda_vers>={min_cuda} rentable=true" --order dph_total --raw
```

timeout 20 s. Defaults: `num_gpus=1`, `min_cuda="12.8"`, `min_disk=60`.

Output handling (**must be ported**): the vastai CLI sometimes emits status
lines before the JSON body, so:

```python
json_start = raw.find('[')
if json_start < 0: -> error
if json_start > 0: raw = raw[json_start:]
offers = json.loads(raw)
```

Then client-side geo filter; **if the filter empties the list, the code falls
back to showing all offers** with a yellow warning. Top **12** are rendered.

Columns and fields read from each offer object:
`id`, `dph_total` (`:.3f`), `reliability2` (`:.2f`), `gpu_ram` (bytes/1024 → GB,
i.e. the field is MiB), `inet_down` (`:.0f` Mbps), `cuda_max_good`, `geolocation`.

Display rules to preserve:
* bandwidth colour: `>=2000` green, `>=500` yellow, else red
* **CUDA ≥ 13.0 is flagged with a `⚠`** — comment: "Unsloth notes quality issues above this version". Note the *filter* is on `cuda_vers` but the *display/warn* is on `cuda_max_good`.

Return contract: `None` = cancelled/error, `""` = "auto cheapest" (caller then
leaves `OFFER_ID` unset), otherwise the offer-id string.

### 3.4 Query — non-interactive (`vast_up.sh`), two-stage

Stage 1 (strict, geo-filtered):

```bash
SEARCH_FILTER="${GPU_FILTER} num_gpus=${NUM_GPUS} reliability>0.99 inet_down>500 dph_total<${MAX_PRICE} disk_space>${MIN_DISK_GB} cuda_vers>=${MIN_CUDA} rentable=true"
OFFER_ID="$(vastai search offers "${SEARCH_FILTER}" \
    --order 'dph_total' --raw 2>/dev/null \
    | jq -r --arg re "${GEO_RE}" \
        '[.[] | select((.geolocation // "") | test(", (" + $re + ")$"))] | .[0].id // empty')"
```

Stage 2 (widened — **drops the geo filter entirely** and relaxes reliability
0.99→0.97, inet_down 500→300):

```bash
SEARCH_FILTER_WIDE="${GPU_FILTER} num_gpus=${NUM_GPUS} reliability>0.97 inet_down>300 dph_total<${MAX_PRICE} disk_space>${MIN_DISK_GB} cuda_vers>=${MIN_CUDA} rentable=true"
OFFER_ID="$(vastai search offers "${SEARCH_FILTER_WIDE}" \
    --order 'dph_total' --raw 2>/dev/null \
    | jq -r '.[0].id // empty')"
```

Stage 3 (cosmetic, best-effort, `|| true`) — re-query loosely to print a summary
line for the chosen offer:

```bash
vastai search offers "${GPU_FILTER} num_gpus=${NUM_GPUS} reliability>0.90 rentable=true" \
    --raw 2>/dev/null \
  | jq -r --arg id "${OFFER_ID}" \
      '.[] | select((.id|tostring) == $id)
       | "    $\(.dph_total)/hr  rel=\(.reliability2)  \(.gpu_name)  \(.gpu_ram/1024|floor)GB VRAM  ↓\(.inet_down|floor)Mbps  cuda=\(.cuda_max_good)  \(.geolocation)"'
```

`OFFER_ID` empty after both stages → `echo "FATAL: no matching offers found"; exit 1`.
`OFFER_ID` set in the environment → search skipped entirely.

> **Behavioural trap worth fixing in the port:** picking "Auto — cheapest
> matching offer" in the TUI does *not* rent the cheapest offer shown by the
> browser. The browser uses reliability>0.97/inet>300; auto goes through
> `vast_up.sh` stage 1 with reliability>0.99/inet>500. Different candidate sets.

### 3.5 Price caps and disk floors

TUI default comes from `gpu_tiers.<key>.max_price` (string), user-editable text
prompt. `vast_up.sh` has its own defaults if `MAX_PRICE` is unset:

```
6000pro              1.60   (MIN_DISK_GB default 80)
h100-sxm|h100-pcie   3.50   (100)
a100-sxm|a100-pcie   2.00   (80)
h200-sxm             5.50   (150)
b200-sxm             9.00   (200)
*                    0.55
global MIN_DISK_GB fallback: 60
```

`recipes.toml` tier table (authoritative for the TUI) — key, max_price,
min_disk_gb, image_type, num_gpus:

```
5090 .55/60/prebuilt      4090 .45/60/prebuilt     6000pro 1.60/80/prebuilt
5090-dc .60/60/prebuilt   h100-sxm 3.50/100/builder  h100-pcie 2.80/100/builder
a100-sxm 2.00/80/builder  a100-pcie 1.80/80/builder  h200-sxm 5.50/150/builder
b200-sxm 9.00/200/builder h100-sxm-2x 7.00/200/builder ×2   h100-sxm-4x 14.00/400/builder ×4
h200-sxm-2x 11.00/300/builder ×2  b200-sxm-2x 18.00/400/builder ×2
b200-sxm-4x 36.00/800/builder ×4  h200-sxm-4x 22.00/600/vllm ×4
h200-sxm-5x 28.00/800/vllm ×5     h100-sxm-8x 28.00/600/vllm ×8
a100-sxm-8x 16.00/600/vllm ×8
```

`MIN_DISK_GB` is used twice: as the search filter `disk_space>N` **and** as the
allocated `--disk N` at create time.

### 3.6 CUDA gating

`MIN_CUDA` default `"12.8"` (env → recipe `min_cuda` → tier `min_cuda` → `"12.8"`).
Emitted as `cuda_vers>=12.8`. Rationale: the prebuilt image is
`nvidia/cuda:12.8.0-*` with `CUDA_ARCHS="89-real;120-real"`. The `⚠` on
`cuda_max_good >= 13.0` is advisory only — offers are not excluded.

---

## 4. Instance creation

### 4.1 Container env block

```bash
HF_TOKEN_VAL="$(cat ~/.cache/huggingface/token 2>/dev/null || echo "")"

ENV_ARGS=(
    -e "MODEL_REPO=${MODEL_REPO}"
    -e "MODEL_QUANT=${MODEL_QUANT}"
    -e "CTX=${CTX}"
    -e "KV_TYPE=${KV_TYPE}"
    -e "MODE=${MODE}"
    -e "PARALLEL=${PARALLEL}"
    -e "HOST=127.0.0.1"
    -e "IMAGE_TYPE=${IMAGE_TYPE}"
    -p "8000:8000"
)
[ -n "${MMPROJ:-}"         ] && ENV_ARGS+=(-e "MMPROJ=${MMPROJ}")
[ -n "${HF_TOKEN_VAL}"     ] && ENV_ARGS+=(-e "HF_TOKEN=${HF_TOKEN_VAL}")
[ -n "${LLAMA_CPP_REPO:-}" ] && ENV_ARGS+=(-e "LLAMA_CPP_REPO=${LLAMA_CPP_REPO}")
[ -n "${LLAMA_CPP_REF:-}"  ] && ENV_ARGS+=(-e "LLAMA_CPP_REF=${LLAMA_CPP_REF}")
```

The array is later flattened with `"${ENV_ARGS[*]}"` — i.e. Vast's `--env` takes
a **single docker-run-ish string**: `-e K=V -e K=V -p 8000:8000`. Space-bearing
values would corrupt it. (Note the vLLM branch never appends its extra env to
`ENV_ARGS`, only to the onstart command — see below.)

### 4.2 onstart command

llama.cpp path:

```
MODEL_REPO=... MODEL_QUANT=... CTX=... KV_TYPE=... MODE=... PARALLEL=... HOST=127.0.0.1 IMAGE_TYPE=... \
[MMPROJ=...] [HF_TOKEN=...] [LLAMA_CPP_REPO=...] [LLAMA_CPP_REF=...] \
bash /app/launch.sh > /var/log/launch.log 2>&1 &
```

vLLM path (`IMAGE_TYPE=vllm`) — **replaces** the string built above:

```
MODEL_ID=${MODEL_ID:-${MODEL_REPO}} CTX=... HOST=127.0.0.1 \
[HF_TOKEN=...] [QUANTIZATION=...] [KV_CACHE_DTYPE=...] [ENFORCE_EAGER=...] [REASONING_PARSER=...] [EXTRA_ARGS='...'] \
bash /app/launch_vllm.sh > /var/log/launch.log 2>&1 &
```

Both end with `&` — backgrounded, stdout+stderr to `/var/log/launch.log`. That
log path is the single observability contract for boot watching, diagnostics and
`vast_tunnel.sh logs`.

> Note the images already declare `ENTRYPOINT ["/usr/bin/tini","-g","--","/app/launch.sh"]`.
> With an onstart-cmd, launch.sh can end up invoked twice (entrypoint + onstart).
> `launch.sh` is idempotent on the download, but two `llama-server` processes
> would fight for port 8000. Worth resolving explicitly in the port (either drop
> the ENTRYPOINT or drop the onstart re-launch).

### 4.3 The create call

```bash
RESULT="$(vastai create instance "${OFFER_ID}" \
    --image "${DOCKER_IMAGE}" \
    --disk "${MIN_DISK_GB}" \
    --env "${ENV_ARGS[*]}" \
    --onstart-cmd "${ONSTART_CMD}" \
    --raw)"
```

stderr deliberately **not** captured (streams to terminal) so warnings don't
contaminate the JSON.

Image resolution: `DOCKER_IMAGE` explicit override wins; else by `IMAGE_TYPE`:

```
builder   ghcr.io/buckster123/vastai-gguf:builder
prebuilt  ghcr.io/buckster123/vastai-gguf:prebuilt
vllm      ghcr.io/buckster123/vastai-gguf:vllm
*         ghcr.io/buckster123/vastai-gguf:prebuilt
```

(`recipes.toml [docker]` also carries `prebuilt_legacy = "ghcr.io/buckster123/qwen36-llamacpp:latest"`.)

### 4.4 Response parsing + billing-leak guards (critical)

```bash
JSON="{${RESULT#*\{}"                                   # slice from first '{'
INST_ID="$(printf '%s' "${JSON}" | jq -r '.new_contract // empty' 2>/dev/null || true)"
```

The instance id is `new_contract` in the create response.

If `INST_ID` is empty the script **exits 1** with an explicit leak warning —
because a created instance bills immediately:

```
!! WARNING: the create call returned, but no instance ID could be parsed.
!! An instance may be RUNNING AND BILLING with no local record.
!! Check now:   vastai show instances
!! Then either record the ID to .last_instance or tear it down:
!!              vastai destroy instance <ID>
```

If `.last_instance` already held a different id, it warns that the previous one
is now untracked and prints `vastai destroy instance ${PREV_ID}`.

Then:

```bash
echo "${INST_ID}" > .last_instance
printf '%s\t%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "${INST_ID}" >> .instance_history
```

**This is the single most important behaviour to port faithfully.** Money leaks
here. In Rust: create → persist id *before* anything else can fail; on any
parse/IO failure, attempt an automatic destroy or fail loudly.

### 4.5 Wizard → env contract (`menu_launch`)

`subprocess.run(["bash", str(ROOT / "vast_up.sh")], cwd=ROOT, env=env)` where
`env = os.environ.copy()` plus:

```
GPU, MODEL_REPO, MODEL_QUANT, CTX, PARALLEL, KV_TYPE, MODE, GEO, MAX_PRICE,
DOCKER_IMAGE, IMAGE_TYPE, MIN_DISK_GB, NUM_GPUS, MIN_CUDA, MODEL (recipe name),
VAST_NAMES (space-joined), [MMPROJ], [OFFER_ID],
[LLAMA_CPP_REPO], [LLAMA_CPP_REF],
[MODEL_ID], [QUANTIZATION], [KV_CACHE_DTYPE], [ENFORCE_EAGER], [REASONING_PARSER]
```

On rc 0 the wizard deletes `.hf_pin`. Wizard step order:
provider → GPU tier → recipe → mode (llama.cpp only) → geo → KV type
(llama.cpp only) → vision/mmproj (llama.cpp only, skipped if recipe sets it) →
max price → auto|browse offer → confirm.

Tier/recipe filtering: vLLM shows only `image_type == "vllm"` tiers and
`provider == "vllm"` recipes; GGUF shows the complement. Recipes are matched to
the tier by `recipe.gpu == tier_key`.

Presets (`config.MODES`, `config.KV_TYPES`) map menu labels to
`thinking|coding|nonthinking` and `q8_0|q4_0|bf16`.

---

## 5. Remote provisioning — `launch.sh` (llama.cpp)

Required env: `MODEL_REPO`, `MODEL_QUANT` (bash `:?` guards).
Defaults: `IMAGE_TYPE=prebuilt`, `CTX=65536`, `KV_TYPE=q8_0`, `MODE=thinking`,
`N_GPU_LAYERS=999`, `PARALLEL=1`, `EXTRA_ARGS=""`, `MODELS_DIR=/workspace/models`,
`PORT=8000`, `HOST=127.0.0.1` (image ENV sets `HOST=0.0.0.0`; the onstart env
overrides it — do not lose this).

Log format: `printf '[%s] %s\n' "$(date -u +%H:%M:%SZ)" "$*"`. Boot-watch and
diagnostics parse nothing structured, just tail lines.

### 5.1 Builder path (compile at boot)

Triggered when `IMAGE_TYPE=builder` **and** `/usr/local/bin/llama-server` is not
executable.

```bash
RAW_CAP="$(nvidia-smi --query-gpu=compute_cap --format=csv,noheader 2>/dev/null | head -1 | tr -d ' ')"
SM="${RAW_CAP//./}"                      # 9.0→90, 8.0→80, 10.0→100
```

Empty `compute_cap` → fatal. `SM=100` emits a bleeding-edge warning.

Source at `/opt/llama.cpp` (pre-cloned in the builder image). Re-clone when the
dir is missing, **or** when `LLAMA_CPP_REPO != ggml-org/llama.cpp` or
`LLAMA_CPP_REF != master`:

```bash
git clone --depth 1 --branch "${LLAMA_CPP_REF}" "https://github.com/${LLAMA_CPP_REPO}.git" "/opt/llama.cpp"
```

Configure + build:

```bash
cmake -B /opt/llama.cpp/build -G Ninja \
    -DCMAKE_BUILD_TYPE=Release \
    -DGGML_CUDA=ON \
    -DGGML_NATIVE=OFF \
    -DCMAKE_CUDA_ARCHITECTURES="${SM}-real" \
    -DLLAMA_CURL=ON \
    -DBUILD_SHARED_LIBS=OFF \
    /opt/llama.cpp 2>&1 | tail -5

cmake --build /opt/llama.cpp/build --config Release -j"$(nproc)" \
    --target llama-server llama-bench 2>&1 | grep -E '^\[|error:|warning:' | tail -20

install -m755 /opt/llama.cpp/build/bin/llama-server /usr/local/bin/llama-server
install -m755 /opt/llama.cpp/build/bin/llama-bench  /usr/local/bin/llama-bench 2>/dev/null || true
```

~8–12 min. Cold-start estimates surfaced in the TUI (`config.cold_start_estimate`):
`prebuilt → "~2 min  (image pull only)"`, `builder → "~12-18 min  (pull + SM compile)"`.

Then a hard gate: `[ -x /usr/local/bin/llama-server ] || die`.

### 5.2 Sampling presets (bash, must match `config.SAMPLING_PRESETS`)

```
thinking     --temp 1.0 --top-p 0.95 --top-k 20 --min-p 0.0 --presence-penalty 1.5
coding       --temp 0.6 --top-p 0.95 --top-k 20 --min-p 0.0 --presence-penalty 0.0
nonthinking  --temp 0.7 --top-p 0.80 --top-k 20 --min-p 0.0 --presence-penalty 1.5
             + --chat-template-kwargs '{"enable_thinking":false}'
```

Anything else → fatal. **Discrepancy to reconcile in the port:** the Python
`SAMPLING_PRESETS` table in `config.py` omits `--top-k 20` in all three presets.
`launch.sh` is the one that actually runs.

### 5.3 Model fetch (idempotent)

```bash
TARGET_DIR="${MODELS_DIR}/$(basename "${MODEL_REPO}")"
```

Skip condition: `TARGET_DIR` exists **and** `ls -1 "$TARGET_DIR" | grep -i "$MODEL_QUANT"` is non-empty
(note: non-recursive `ls`, so shard-in-subdir layouts will re-fetch — HF resumes, so it is only wasteful).

```bash
INCLUDE_ARGS=(--include "*${MODEL_QUANT}*.gguf")
[ -n "$MMPROJ" ] && INCLUDE_ARGS+=(--include "*mmproj-${MMPROJ}*.gguf")

HF_HUB_ENABLE_HF_TRANSFER=1 \
hf download "${MODEL_REPO}" --local-dir "${TARGET_DIR}" "${INCLUDE_ARGS[@]}"
```

`hf` is the `huggingface_hub[cli]>=0.36` binary, installed into `/opt/hf-venv`
and symlinked to `/usr/local/bin/hf`. `HF_HUB_ENABLE_HF_TRANSFER=1` is the
reason stalls happen and why the stall detector exists. `HF_TOKEN` comes from
the env for gated repos.

### 5.4 Weight discovery

```bash
MODEL_FILE="$(find "${TARGET_DIR}" -maxdepth 2 -name "*.gguf" \
    | grep -iE "${MODEL_QUANT}" | grep -v 'mmproj' | sort | head -n1 || true)"
```

`sort | head -n1` picks shard `-00001-of-000NN` for split GGUFs. `-maxdepth 2`
covers repos that nest shards in a quant-named subdirectory. Empty → fatal.

mmproj (when requested):

```bash
MMPROJ_FILE="$(find "${TARGET_DIR}" -maxdepth 2 -name "*.gguf" | grep -iE "mmproj-${MMPROJ}" | head -n1 || true)"
MMPROJ_ARGS="--mmproj ${MMPROJ_FILE}"
```

### 5.5 Serve

```bash
nvidia-smi --query-gpu=name,memory.total,memory.free --format=csv,noheader 2>/dev/null || true

exec llama-server \
    --model "${MODEL_PATH}" \
    ${MMPROJ_ARGS} \
    --host "${HOST}" --port "${PORT}" \
    --ctx-size "${CTX}" \
    --cache-type-k "${KV_TYPE}" --cache-type-v "${KV_TYPE}" \
    --n-gpu-layers "${N_GPU_LAYERS}" \
    --parallel "${PARALLEL}" \
    --jinja \
    --metrics \
    --flash-attn \
    ${SAMPLE_ARGS} ${TPL_KW} ${TPL_KW_JSON:+"$TPL_KW_JSON"} ${EXTRA_ARGS}
```

---

## 6. Remote provisioning — `launch_vllm.sh`

Required: `MODEL_ID`. Defaults: `CTX=131072`, `GPU_UTIL=0.95`, `DTYPE=auto`,
`MAX_NUM_SEQS=64`, `PORT=8000`, **`HOST=0.0.0.0`** (the onstart command forces
`HOST=127.0.0.1`, so the tunnel-only posture is preserved — but the script's own
default is open; keep the override).

Tensor-parallel auto-detect: `TP="$(nvidia-smi -L 2>/dev/null | wc -l)"`, 0 → fatal.
Inventory: `nvidia-smi --query-gpu=index,name,memory.total --format=csv,noheader`.

```bash
ARGS=(
    --model "${MODEL_ID}"
    --tensor-parallel-size "${TP}"
    --max-model-len "${CTX}"
    --gpu-memory-utilization "${GPU_UTIL}"
    --dtype "${DTYPE}"
    --max-num-seqs "${MAX_NUM_SEQS}"
    --host "${HOST}"
    --port "${PORT}"
)
[ TRUST_REMOTE=true ]                    -> --trust-remote-code            (default true)
[ QUANTIZATION not empty and != None ]   -> --quantization "$QUANTIZATION"
[ KV_CACHE_DTYPE not empty and != auto ] -> --kv-cache-dtype "$KV_CACHE_DTYPE"
[ ENFORCE_EAGER=true ]                   -> --enforce-eager
[ CHUNKED_PREFILL=true ]                 -> --enable-chunked-prefill        (default true)
[ REASONING_PARSER not empty ]           -> --enable-reasoning --reasoning-parser "$REASONING_PARSER"

export VLLM_ATTENTION_BACKEND="${VLLM_ATTENTION_BACKEND:-FLASHINFER}"
exec vllm serve "${ARGS[@]}" ${EXTRA_ARGS}
```

Model download is vLLM/HF-internal (no `hf download` step), so the
`.incomplete`-file diagnostics in §8 do not apply to the vLLM path.

vLLM recipe fields consumed: `model_id`, `ctx`, `kv_cache_dtype`,
`enforce_eager` (string `"true"`/`"false"`), `reasoning_parser`, `quantization`.

---

## 7. SSH tunnel (`tools/vast_tunnel.sh`)

Subcommands: `up | status | down | logs` (default `status`).
`LOCAL_PORT=8800`, `REMOTE_PORT=8000`, `INST_FILE=<ROOT>/.last_instance`,
`PID_FILE=/tmp/vastai-gguf-tunnel.pid`, `SSH_CFG_FILE=/tmp/vastai-gguf-tunnel.ssh`.

### 7.1 Instance lookup

```bash
raw=$(vastai show instance "$inst_id" --raw 2>/dev/null)
as=$(echo "$raw" | jq -r '.actual_status // "unknown"')
ssh_host=$(echo "$raw" | jq -r '.ssh_host // empty')
ssh_port=$(echo "$raw" | jq -r '.ssh_port // empty')
```

Missing host/port → die "instance $id has no SSH info yet (status=$as)".
(`helpers.get_instance_json` does the same call but **without** the
first-`{`-slicing that offer parsing has — an inconsistency; the port should
slice uniformly.)

### 7.2 `up`

1. If `PID_FILE` exists and `kill -0 $OLD_PID` succeeds → `kill $OLD_PID`, `sleep 1`, remove pid + cfg files.
2. Warn if `actual_status != running`.
3. Write the ephemeral ssh config verbatim:

```
Host vast-qwen36
    HostName $SSH_HOST
    Port $SSH_PORT
    User root
    StrictHostKeyChecking no
    ControlMaster auto
    ControlPath /tmp/qwen36-vast-cm
    ControlPersist 5m
    ServerAliveInterval 30
    ExitOnForwardFailure yes
```

4. Open the tunnel:

```bash
ssh -f -N -F "$SSH_CFG_FILE" -L "${LOCAL_PORT}:127.0.0.1:${REMOTE_PORT}" vast-qwen36
```

5. Recover the PID (0.5 s later):

```bash
TPID=$(pgrep -f "ssh.*${LOCAL_PORT}:127.0.0.1:${REMOTE_PORT}.*vast-qwen36" | head -1 || true)
[ -z "$TPID" ] && TPID=$(pgrep -n ssh || true)      # ← fallback can grab an unrelated ssh
echo "$TPID" > "$PID_FILE"
```

6. Health probe, 3 attempts, 3 s apart:

```bash
curl -s --max-time 3 "http://127.0.0.1:${LOCAL_PORT}/health"        # match substring "ok"
curl -s --max-time 3 "http://127.0.0.1:${LOCAL_PORT}/v1/models" | jq -r '.data[0].id // "loading"'
```

The ControlMaster block is a documented performance requirement: *"Without it:
~500ms per request. With it: ~RTT (20-130ms depending on geo)."* The header also
suggests the equivalent `~/.ssh/config` stanza for `Host ssh*.vast.ai`.

### 7.3 `status`

Pidfile liveness (`kill -0`, stale pidfile removed), instance id/status/ssh, then:

```bash
curl -s --max-time 5 http://127.0.0.1:8800/health
curl -s --max-time 5 http://127.0.0.1:8800/v1/models | jq -r '[.data[].id] | join(", ")'
curl -s --max-time 5 http://127.0.0.1:8800/slots     | jq 'length'
```

### 7.4 `down`

```bash
kill "$PID"                                          # if alive
rm -f "$PID_FILE"
ssh -F "$SSH_CFG_FILE" -O exit vast-qwen36 2>/dev/null || true
rm -f "$SSH_CFG_FILE" /tmp/qwen36-vast-cm 2>/dev/null || true
```

### 7.5 `logs`

```bash
ssh -p "$SSH_PORT" -o StrictHostKeyChecking=no -o ConnectTimeout=10 \
    root@"$SSH_HOST" 'tail -f /var/log/launch.log' 2>&1
```

Blocking / Ctrl-C to stop.

### 7.6 Local liveness check (Python)

`helpers.tunnel_running()` reads `/tmp/vastai-gguf-tunnel.pid` and does
`os.kill(pid, 0)`; `PermissionError` counts as alive; `ProcessLookupError`/`ValueError`
as dead.

---

## 8. Boot watching, diagnostics, stall detection & recovery

### 8.1 Boot watcher (`menu_watch_boot`, `vast_menus.py:498`)

Reads `.last_instance`; polls **every 10 s**; Ctrl-C exits cleanly.

Per iteration:
1. `get_instance_json(inst_id)` → `vastai show instance <id> --raw 2>/dev/null` (12 s timeout). `None` → print "vastai API unreachable, retrying...", sleep 10, continue.
2. Print `HH:MM:SS  status=<actual_status>  <status_msg>`; colours: `running`→green, `loading`→yellow, else red.
3. If `ssh_host` and `ssh_port` present, tail one log line and print only on change:

```bash
ssh -p {port} -o StrictHostKeyChecking=no -o ConnectTimeout=5 root@{host} 'tail -1 /var/log/launch.log 2>/dev/null'
```
(10 s timeout)

4. **Success exit:** `status == "running"` AND `tunnel_running()` AND
   `curl -s --max-time 3 http://127.0.0.1:8800/health` contains `"ok"` →
   print `✓ Endpoint healthy!  http://127.0.0.1:8800/v1`, break.
   *(Note: the watcher never starts the tunnel itself — it can only succeed if
   the user already ran `tunnel up`. In the Rust port, auto-open the tunnel once
   SSH info appears.)*
5. **Failure exit:** `status in ("exited", "offline")` → break.

### 8.2 Deep diagnostics (`menu_diagnose`, `tool_menus.py:300`)

Skipped entirely if the active endpoint provider is `local` or `together`.

Instance panel fields: `actual_status`, `status_msg`, `gpu_name`, `geolocation`,
`dph_total`, `inet_down`, `disk_util` (%), `disk_space` (GB), `ssh_host:ssh_port`.
Bails if status != running or no ssh_host.

Four SSH probes, verbatim remote commands:

```bash
ps -eo pid,etime,pcpu,pmem,cmd --sort=-pcpu | head -15                                   # timeout 15
df -h /workspace 2>/dev/null || df -h /                                                  # timeout 10
find /workspace/models -type f \( -name '*.gguf' -o -name '*.incomplete' \) -exec ls -lh {} \; 2>/dev/null | head -20   # timeout 12
tail -30 /var/log/launch.log 2>/dev/null || echo '(no log yet)'                          # timeout 12
```

Model-file classification: a line containing `.incomplete` → `⟳ downloading`
(size = field index 4 of the `ls -lh` output); a line containing `.gguf` →
`✓ complete`.

### 8.3 Stall detection (`vast_ops._net_rx_delta`)

Remote script, 4-second sample by default:

```bash
RX1=$(cat /proc/net/dev | awk '/eth0/{print $2}'); sleep 4; RX2=$(cat /proc/net/dev | awk '/eth0/{print $2}'); echo $((RX2-RX1))
```

SSH timeout = `seconds + 15`. Non-zero rc or non-integer output → `None`.

Thresholds (`tool_menus.py:427`):

```
speed_mbps = (rx_bytes * 8) / (4 * 1_000_000)
rx_bytes < 1000        -> red   "⚠  STALLED"
speed_mbps < 50        -> yellow "  slow"
otherwise              -> green "✓  active"
```

`rx_bytes < 1000` (i.e. <1 KB in 4 s) is the stall trigger and offers recovery.

### 8.4 Container env read (`vast_ops._get_container_env`)

```bash
cat /proc/$(pgrep -f 'bash /app/launch.sh' | head -1)/environ 2>/dev/null | tr '\0' '\n' | grep -E 'MODEL_|CTX|KV_TYPE|MODE|PARALLEL|MMPROJ|HF_TOKEN|HOST'
```
(12 s timeout) → parsed into a dict on the first `=`.

### 8.5 Stall recovery (`vast_ops._restart_launch`)

1. Read env as above. If empty, fall back to hardcoded defaults:
   `MODEL_REPO=unsloth/Qwen3.6-35B-A3B-GGUF`, `MODEL_QUANT=UD-Q5_K_XL`,
   `CTX=131072`, `KV_TYPE=q8_0`, `MODE=thinking`, `PARALLEL=1`.
2. **Always** force `env["HOST"] = "127.0.0.1"` ("always harden on restart").
3. Build the restart script (`env_str` = `k=v` pairs space-joined):

```bash
#!/bin/bash
pkill -f 'bash /app/launch.sh' 2>/dev/null || true
pkill -f 'hf download' 2>/dev/null || true
sleep 2
${env_str} bash /app/launch.sh >> /var/log/launch.log 2>&1 &
echo "restarted pid=$!"
```

4. Write it remotely (10 s timeout):

```bash
cat > /tmp/restart_launch.sh << 'HEREDOC'
<script>
HEREDOC
chmod +x /tmp/restart_launch.sh
```

5. Execute detached:

```python
subprocess.run(["ssh", "-f", "-p", str(port),
                "-o", "StrictHostKeyChecking=no", "-o", "ConnectTimeout=8",
                f"root@{host}", "/tmp/restart_launch.sh"],
               capture_output=True, timeout=15)
```

6. `time.sleep(3)` then `bash <ROOT>/tools/vast_tunnel.sh logs` (blocking tail).

Rationale printed to the user: *"HF transfer connection likely hung. Restart
will kill the stalled process and re-run launch.sh — HF hub resumes from the
.incomplete file."* Note `>>` (append) on restart vs `>` (truncate) on first boot.

### 8.6 Slot inspection (post-health)

```bash
curl -s --max-time 5 http://127.0.0.1:8800/health
curl -s --max-time 5 http://127.0.0.1:8800/slots 2>/dev/null
```
Per slot: `state`, `n_past`, `n_ctx` → `slot i: <state>  ctx used n_past/n_ctx tokens`.

---

## 9. Instance listing / re-attach (`menu_instances`)

```bash
vastai show instances --raw 2>/dev/null        # 15 s timeout
```

Empty / `[]` / `null` → "No instances found". Table columns from each entry:
`id`, `actual_status` (running→green, loading→yellow, else red), `gpu_name`,
`dph_total` (`:.3f`), `geolocation`. Selecting a row writes the id to
`.last_instance` via `LAST_INST.write_text(new_id)` (no trailing newline —
harmless because every reader `.strip()`s or `cat`s it).

---

## 10. Destroy

`menu_destroy()`:
1. Requires `.last_instance`; warns if the tunnel is up.
2. `questionary.confirm(f"Destroy instance {inst_id}? This is irreversible.", default=False)`.
3. If tunnel running: `bash <ROOT>/tools/vast_tunnel.sh down`.
4. `bash <ROOT>/vast_down.sh`.

`vast_down.sh` entire body:

```bash
set -euo pipefail
INST_ID="${1:-$(cat .last_instance 2>/dev/null || true)}"
[ -n "${INST_ID}" ] || { echo "no instance id (pass arg or have .last_instance)"; exit 2; }
echo "==> destroying ${INST_ID}"
echo "y" | vastai destroy instance "${INST_ID}"
rm -f .last_instance
```

The `echo "y" |` pipe answers the CLI's interactive confirmation. **A REST DELETE
removes the need for this entirely.** Also note `.last_instance` is removed even
if `vastai destroy` fails silently — the port should verify the destroy
succeeded (poll `show instance` until 404/`exited`) before dropping the record.

---

## 11. Smoke test (`smoke.sh`)

Usage forms:

```
./smoke.sh http://HOST:PORT
./smoke.sh --provider together
./smoke.sh https://api.example.com/v1 -k sk-...
```

Args: `--provider` (resolve from `<ROOT>/.active_endpoint`), `-k|--key`, first
positional = base URL. `URL="${BASE%/}/v1"`.

Together resolution: read `.active_endpoint` JSON → `.provider`, `.endpoint`
(strip trailing `/chat/completions` with sed), then scrape
`~/.vastai-gguf/config.toml` for `api_key` inside `[providers.together]` using
`grep -oP 'api_key\s*=\s*"\K[^"]+'`, falling back to `$TOGETHER_API_KEY`.
Auth header built as an **array** (`AUTH_HEADER=(-H "Authorization: Bearer $KEY")`)
specifically to avoid word-splitting/injection.

Four sections, verbatim requests:

```bash
# 1. models
curl -fsS "${AUTH_HEADER[@]}" "${URL}/models" | jq '[.data // .] | flatten | .[0:3]'

# 2. warm-up (wrapped in `time`)
curl -fsS "${AUTH_HEADER[@]}" "${URL}/chat/completions" -H 'content-type: application/json' -d '{
  "model":"x",
  "messages":[{"role":"user","content":"In one sentence, what is a hash table?"}],
  "max_tokens":80
}' | jq -r '.choices[0].message.content // empty'

# 3. tool calling
curl -fsS "${AUTH_HEADER[@]}" "${URL}/chat/completions" -H 'content-type: application/json' -d '{
  "model":"x",
  "messages":[{"role":"user","content":"What is the weather in Reykjavik right now? Use the tool."}],
  "tools":[{"type":"function","function":{"name":"get_weather","description":"Get current weather for a city.","parameters":{"type":"object","properties":{"city":{"type":"string","description":"City name"},"unit":{"type":"string","enum":["c","f"],"description":"Temp unit"}},"required":["city"]}}}],
  "tool_choice":"auto",
  "max_tokens":256
}' | jq '.choices[0].message | {content, tool_calls}'

# 4. throughput
curl -fsS "${AUTH_HEADER[@]}" "${URL}/chat/completions" -H 'content-type: application/json' -d '{
  "model":"x",
  "messages":[{"role":"user","content":"Write a 200-word explanation of how a B-tree differs from a hash table for database indexing."}],
  "max_tokens":300,
  "stream":false
}' | jq '{completion_tokens: .usage.completion_tokens, prompt_tokens: .usage.prompt_tokens, model: .model}'
```

Provider label heuristic: `BASE` containing `api.together` → "Together AI
(managed)", else "Self-hosted (tunnel/localhost)".

**Bug to fix in the port:** `"model":"x"` is hardcoded in sections 2–4. That
works for llama-server (which ignores the model name) but every managed provider
will 400 on it, so `--provider together` mode is broken past section 1.

Invocation from the TUI (`menu_smoke`): default URL is
`http://127.0.0.1:8888` if the proxy pidfile is live, else the Together endpoint
if active, else `http://127.0.0.1:8800` if the tunnel is up; then
`run(f"bash {ROOT}/smoke.sh {url}")` (unquoted interpolation — another thing to
drop in Rust).

---

## 12. Endpoint contracts the port must keep speaking

| Endpoint | Used by | Parsed as |
|---|---|---|
| `GET /health` | tunnel up/status, boot watcher, status panel, diagnostics | substring match on `"ok"` |
| `GET /v1/models` | tunnel up/status, status panel, batch compare | `.data[0].id`, `[.data[].id]` |
| `GET /slots` | tunnel status, diagnostics | array; `length`; `.state`, `.n_past`, `.n_ctx` |
| `POST /v1/chat/completions` | smoke, batch compare | `.choices[0].message.{content,tool_calls}`, `.usage.*` |
| `GET /metrics` | (enabled via `--metrics`, not yet consumed) | — |

Caveat: recent llama.cpp gates `/slots` behind an explicit `--slots` flag, which
`launch.sh` does **not** pass. Expect 501 on newer builds — either add `--slots`
to the launch line or degrade gracefully.

---

## 13. Where the Vast REST API is strictly better

The CLI is used for exactly five operations. All five have clean REST
equivalents on `https://console.vast.ai/api/v0` with
`Authorization: Bearer <api_key>` (key file: `~/.config/vastai/vast_api_key`,
older installs `~/.vast_api_key`, env `VAST_API_KEY`).

| Current shellout | REST equivalent | Why REST wins |
|---|---|---|
| `vastai search offers "<DSL>" --order dph_total --raw` | `PUT /bundles/` with a JSON query object + `"order": [["dph_total","asc"]]` | no string-DSL construction, no `jq`, no "strip junk before the first `[`" hack, no shell quoting of `gpu_name in [A,B]`; **geo could be filtered server-side** instead of by client regex |
| `vastai create instance <id> --image --disk --env "<docker-string>" --onstart-cmd "..." --raw` | `PUT /asks/{offer_id}/` with `{client_id, image, disk, env:{...}, onstart, runtype:"ssh", target_state:"running"}` | `env` becomes a real map (space-safe values, no `"${ENV_ARGS[*]}"` flattening); response is guaranteed JSON so `new_contract` extraction stops needing `{${RESULT#*\{}` |
| `vastai show instance <id> --raw` | `GET /instances/{id}/` | typed struct; no `jq`, no prefix-slicing inconsistency |
| `vastai show instances --raw` | `GET /instances/` | same |
| `echo y \| vastai destroy instance <id>` | `DELETE /instances/{id}/` | no interactive-confirm pipe; real status code to verify before dropping `.last_instance` |

Field-name mapping to remember: the query DSL key `reliability` maps to the
response field `reliability2`; the DSL key `cuda_vers` is distinct from the
response field `cuda_max_good`; `gpu_ram` is in **MiB** (the code divides by 1024
to get GB).

Keep `ssh` shellouts (or move to a Rust ssh client such as `russh` /
`openssh` crate) — Vast has no REST for exec/tunnel. If shelling out, keep the
exact flags: `-o StrictHostKeyChecking=no -o ConnectTimeout=8`, `-f -N`,
`-L 8800:127.0.0.1:8000`, and the ControlMaster config (it is a real 500 ms →
RTT win for agentic loops).

---

## 14. Defects and rough edges found while reading

1. **Auto-offer mismatch** (§3.4) — browser thresholds ≠ `vast_up.sh` thresholds, so "auto cheapest" rents something the user never saw.
2. **Widened search silently drops the geo constraint** — a user who asked for EU_NORDIC can get a US box with no confirmation.
3. **`pgrep -n ssh` fallback** (§7.2) can record an unrelated SSH PID, which `down` then kills.
4. **`get_instance_json` lacks the `{`-slicing** that offer parsing has → same CLI quirk breaks it.
5. **Double launch risk** — image `ENTRYPOINT` is `launch.sh` *and* the onstart-cmd runs `launch.sh`.
6. **`HF_TOKEN` is embedded in `--onstart-cmd`**, which Vast stores and echoes back in `show instance` output. Prefer the `env` map, or better, inject the token post-boot over SSH.
7. **`vast_down.sh` removes `.last_instance` unconditionally**, even if the destroy failed → the leak the create path works so hard to prevent, reintroduced on teardown.
8. **`smoke.sh` hardcodes `"model":"x"`** — broken for managed providers.
9. **`config.SAMPLING_PRESETS` omits `--top-k 20`** present in `launch.sh`.
10. **CWD-relative state files** (`.last_instance`, `.instance_history`).
11. **`ls -1 | grep` skip check is non-recursive** while the `find` that locates weights is `-maxdepth 2` — mismatched, causing redundant re-downloads for nested-shard repos.
12. **No timeout/oversight on the whole boot** — the watcher polls forever until Ctrl-C; a wedged instance bills indefinitely. Add a max-boot-time policy with auto-destroy.
