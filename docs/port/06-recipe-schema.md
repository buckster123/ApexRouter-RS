# 06 — `recipes.toml`: complete reverse-engineered schema, catalogue inventory, and the case for discovery

Source of truth for this document: `/home/andre/Projects/Inference/tools/LocalRouter/recipes.toml`
(1048 lines, read in full) cross-checked against every consumer of it:

| Consumer | Role |
|---|---|
| `localrouter/config.py` | `load_config()`, `image_for_type()`, `cold_start_estimate()`, `SAMPLING_PRESETS`, `GEOS`, `MODES`, `KV_TYPES` |
| `localrouter/recipe_editor.py` | read/write, CRUD, **the only validation rules that exist** |
| `localrouter/menus/vast_menus.py` | launch wizard — field resolution order, recipe→env mapping |
| `localrouter/menus/editor_menus.py` | GUI-ish wizard: which fields it asks for, defaults, coercion |
| `localrouter/menus/local_menus.py` | local launch wizard |
| `localrouter/local_endpoint.py` | local recipe → `llama-server` argv |
| `localrouter/vast_ops.py` | `browse_offers()` — tier → vast search filter |
| `localrouter/hf_browser.py` | HF pin (`.hf_pin`) which bypasses `model_repo`/`model_quant` |
| `localrouter/cost.py` | price tables that **duplicate and override** the recipe price fields |
| `vast_up.sh`, `launch.sh`, `launch_vllm.sh` | the env-var contract the recipe ultimately compiles down to |

Headline counts: **1 `[docker]` table (4 keys), 19 `[gpu_tiers.*]` tables, 71 `[[recipes]]` entries
(54 `vast_gguf` + 7 `vllm` + 7 `together` + 3 `local`), 1 empty `[local]` table.**

---

## 1. File shape

```toml
[docker]                 # 1 table,  key → image URI (free-form key set)
[gpu_tiers.<key>]        # 19 tables, key is an arbitrary slug referenced by recipe.gpu
[[recipes]]              # 71 array-of-table entries, ORDER IS SEMANTIC (see §5.3)
[local]                  # 1 table, PRESENT BUT EMPTY — aspirational, never read
```

There is no `version` key, no schema declaration, and no `$schema`-like marker. Any parser must
tolerate unknown keys: the editor's `+ Add field` action lets a user write arbitrary keys into a
recipe, and `_coerce_value()` will guess int/float/list-of-string/str for them.

---

## 2. `[docker]` — container image registry

**Type:** flat `table<string, string>`. Keys are free-form; the *values of `image_type`* elsewhere
in the file are looked up here by `image_for_type(docker_cfg, image_type)`.

| Key | Value (verbatim) | Status |
|---|---|---|
| `prebuilt` | `ghcr.io/buckster123/vastai-gguf:prebuilt` | live; default when `image_type` unknown |
| `builder` | `ghcr.io/buckster123/vastai-gguf:builder` | live; compiles llama.cpp for exact SM arch at boot |
| `prebuilt_legacy` | `ghcr.io/buckster123/qwen36-llamacpp:latest` | **DEAD** — no recipe or tier ever sets `image_type = "prebuilt_legacy"`, and nothing in the code references the string |
| `vllm` | `ghcr.io/buckster123/vastai-gguf:vllm` | live; used by `image_type = "vllm"` |

Resolution rules (`config.py:75-83`):

```
image_for_type(cfg, t) = cfg.get(t) ?? cfg.get("prebuilt") ?? "ghcr.io/buckster123/vastai-gguf:prebuilt"
cold_start_estimate("prebuilt") = "~2 min  (image pull only)"
cold_start_estimate("builder")  = "~12-18 min  (pull + SM compile)"
cold_start_estimate(*)          = "unknown"      # note: "vllm" falls here
```

`load_config()` back-fills `prebuilt` and `builder` with the same hard-coded defaults if the table
is missing, so `[docker]` is effectively optional. `vast_up.sh:44-51` has a *third* copy of the
same mapping for direct CLI invocation. **Three sources of truth for four strings.**

---

## 3. `[gpu_tiers.<key>]` — formal schema

The tier key (the part after `gpu_tiers.`) is the identifier referenced by `recipe.gpu`. Keys in
use follow no enforced convention but observably are `<gpu><-variant><-Nx>`: `5090`, `4090`,
`6000pro`, `5090-dc`, `h100-sxm`, `h100-pcie`, `a100-sxm`, `a100-pcie`, `h200-sxm`, `b200-sxm`,
`h100-sxm-2x`, `h100-sxm-4x`, `h100-sxm-8x`, `h200-sxm-2x`, `h200-sxm-4x`, `h200-sxm-5x`,
`b200-sxm-2x`, `b200-sxm-4x`, `a100-sxm-8x`.

| Key | Type | Req? | Default | Present in file | Consumed by | Notes |
|---|---|---|---|---|---|---|
| `vast_names` | `array<string>` | **required** (`REQUIRED_TIER_FIELDS`) | `[]` | 19/19 | `vast_ops.browse_offers`, `vast_menus` → `VAST_NAMES` env → `vast_up.sh` | Vast `gpu_name` enum values. 1 name → `gpu_name=X`; ≥2 → `gpu_name in [A,B]`. A bare string is tolerated by `browse_offers` (wrapped in a list) but **fails** `validate_gpu_tier` |
| `label` | `string` | **required** | — | 19/19 | wizard menu text, editor table | Human string that also smuggles **VRAM total and a price hint** (`"H100 SXM 80GB   (~$2.50/hr)"`). Not machine-parsed |
| `max_price` | `string` | **required** | `"0.55"` fallback in wizard | 19/19 | `dph_total<{max_price}` in the vast filter; wizard default | **A STRING, not a float.** Interpolated raw into the query string |
| `min_disk_gb` | `integer` | optional | `60` | 19/19 | `disk_space>{n}`; `--disk` on `vastai create` | See resolution-order bug in §6.2 |
| `image_type` | `string` enum | optional | `"prebuilt"` | 19/19 | tier filter in the wizard, `image_for_type`, `IMAGE_TYPE` env | Values in file: `prebuilt` (4), `builder` (11), `vllm` (4). **Also acts as the provider discriminator for tier visibility** — see §6.1 |
| `vram_gb` | `integer` | optional | `"?"` (display) | 19/19 | **display only** — `editor_menus._tier_table`, `_pick_gpu_tier` | **PER GPU**, not total. `h100-sxm-2x` has `vram_gb = 80`, `num_gpus = 2`, label says `160GB`. Never used for any fit calculation |
| `num_gpus` | `integer` | optional | `1` | 9/19 | `num_gpus={n}` in vast filter; `NUM_GPUS` env → `launch_vllm.sh` TP autodetect | Absent on all single-GPU tiers |
| `min_cuda` | `string` | optional | `"12.8"` | **0/19** | `cuda_vers>={v}` in vast filter | Schema key with zero instances. Written by `_create_tier_wizard`, read by `vast_menus`, never present in this file. Every search therefore uses the hard-coded `"12.8"` |

`validate_gpu_tier()` (`recipe_editor.py:215`) checks exactly two things:
1. `{vast_names, label, max_price}` all present.
2. `vast_names` is a list.

Nothing validates that `vram_gb`/`num_gpus` are positive, that `max_price` parses as a number, that
`image_type` is a known `[docker]` key, or that the tier is reachable.

### 3.1 Full tier table (all 19, all fields, verbatim values)

| key | vast_names | num_gpus | vram_gb (per GPU) | max_price | min_disk_gb | image_type | label (price hint) | recipes referencing |
|---|---|---|---|---|---|---|---|---|
| `5090` | `RTX_5090` | 1 | 32 | `0.55` | 60 | prebuilt | `RTX 5090 32GB   (~$0.34/hr)` | 9 |
| `4090` | `RTX_4090` | 1 | 24 | `0.45` | 60 | prebuilt | `RTX 4090 24GB   (~$0.28/hr)` | 4 |
| `6000pro` | `RTX_PRO_6000_WS`, `RTX_PRO_6000_S` | 1 | 96 | `1.60` | 80 | prebuilt | `RTX PRO 6000 96GB  (~$0.93/hr)` | 8 |
| `5090-dc` | `RTX_5090` | 1 | 32 | `0.60` | 60 | prebuilt | `RTX 5090 32GB  DC  (~$0.40/hr)` | **0 — unreachable** |
| `h100-sxm` | `H100_SXM`, `H100_SXM5`, `H100X` | 1 | 80 | `3.50` | 100 | builder | `H100 SXM 80GB   (~$2.50/hr)` | 15 |
| `h100-pcie` | `H100_PCIE` | 1 | 80 | `2.80` | 100 | builder | `H100 PCIe 80GB  (~$1.80/hr)` | 2 |
| `a100-sxm` | `A100_SXM4_80GB`, `A100_SXM`, `A100X` | 1 | 80 | `2.00` | 80 | builder | `A100 SXM 80GB   (~$1.20/hr)` | 3 |
| `a100-pcie` | `A100_PCIE`, `A100_PCIE_80GB` | 1 | 80 | `1.80` | 80 | builder | `A100 PCIe 80GB  (~$1.00/hr)` | 1 |
| `h200-sxm` | `H200_SXM`, `H200` | 1 | 141 | `5.50` | 150 | builder | `H200 SXM 141GB  (~$3.50/hr)` | 3 |
| `b200-sxm` | `B200_SXM`, `B200` | 1 | 192 | `9.00` | 200 | builder | `B200 SXM 192GB  (~$5+/hr)` | 2 |
| `h100-sxm-2x` | `H100_SXM`, `H100_SXM5`, `H100X` | 2 | 80 | `7.00` | 200 | builder | `2× H100 SXM 160GB  (~$5/hr)` | 2 |
| `h100-sxm-4x` | `H100_SXM`, `H100_SXM5`, `H100X` | 4 | 80 | `14.00` | 400 | builder | `4× H100 SXM 320GB  (~$10/hr)` | 3 (2 gguf + 1 vllm) |
| `h200-sxm-2x` | `H200_SXM`, `H200` | 2 | 141 | `11.00` | 300 | builder | `2× H200 SXM 282GB  (~$7/hr)` | 3 (2 gguf + 1 vllm) |
| `b200-sxm-2x` | `B200_SXM`, `B200` | 2 | 192 | `18.00` | 400 | builder | `2× B200 SXM 384GB  (~$10+/hr)` | 1 |
| `b200-sxm-4x` | `B200_SXM`, `B200` | 4 | 192 | `36.00` | 800 | builder | `4× B200 SXM 768GB  (~$20+/hr)` | 1 (vllm) |
| `h200-sxm-4x` | `H200_SXM`, `H200` | 4 | 141 | `22.00` | 600 | **vllm** | `4× H200 SXM 564GB  (~$14/hr)` | 1 (vllm) |
| `h200-sxm-5x` | `H200_SXM`, `H200` | 5 | 141 | `28.00` | 800 | **vllm** | `5× H200 SXM 705GB  (~$17.50/hr)` | 1 (vllm) |
| `h100-sxm-8x` | `H100_SXM`, `H100_SXM5`, `H100X` | 8 | 80 | `28.00` | 600 | **vllm** | `8× H100 SXM 640GB  (~$20/hr)` | 1 (vllm) |
| `a100-sxm-8x` | `A100_SXM4_80GB`, `A100_SXM`, `A100X` | 8 | 80 | `16.00` | 600 | **vllm** | `8× A100 SXM 640GB  (~$9.60/hr)` | 1 (vllm) |

Observations that matter for the port:

- **No RTX 3090 tier exists.** The target tier set Andre wants (2–4× 3090, up to 2× H100) is a
  *new* set, not a subset. Only `h100-sxm` / `h100-pcie` / `h100-sxm-2x` carry over.
- `5090` and `5090-dc` are byte-identical except `max_price` and `label` — the "tier" abstraction is
  already being abused as a *saved search preset*. That is what it should openly become.
- Price hints live inside `label` and are stale by construction. `cost.py:51` keeps yet another copy
  (`vast_hourly_rates = {"5090": 0.34, "4090": 0.28, "6000pro": 0.93, "h100-sxm": 2.50, ...}`) —
  a fourth place where GPU pricing is hardcoded, and it does not even cover all tiers.

### 3.2 The offer-search contract a tier compiles to

`vast_ops.browse_offers` (interactive list):

```
vastai search offers "<gpu_filter> num_gpus={n} reliability>0.97 inet_down>300
                      dph_total<{max_price} disk_space>{min_disk} cuda_vers>={min_cuda}
                      rentable=true" --order dph_total --raw
```

`vast_up.sh:178` (auto-pick) uses **stricter** thresholds first, then retries wide:

```
strict: reliability>0.99 inet_down>500 ...
wide:   reliability>0.97 inet_down>300 ...   (and drops the geo filter entirely)
```

Geo is *not* a Vast filter — it is a post-hoc regex on `offer.geolocation` matching `, (RE)$`:

| GEO key | regex |
|---|---|
| `EU_NORDIC` | `SE\|NO\|FI\|DK\|IS` |
| `EU` | `SE\|NO\|FI\|DK\|IS\|DE\|NL\|FR\|BE\|UK\|IE\|EE\|LV\|LT\|PL\|CZ\|AT\|CH\|ES\|PT\|IT` |
| `US` | `US` |
| `ANY` | `.*` |

`browse_offers` shows the first 12 filtered offers and flags `cuda_max_good >= 13.0` with a yellow
`⚠` (Unsloth quality note). These thresholds (`0.97`/`0.99`, `300`/`500`, `12`, `13.0`) are all
hardcoded in Python/bash, **not** in `recipes.toml` — they are prime candidates for promotion into
real config.

---

## 4. `[[recipes]]` — formal schema

`provider` is the discriminator. **It is absent on 54 of 71 entries** and defaults to
`"vast_gguf"` everywhere it is read (`recipe.get("provider", "vast_gguf")`).

Known values: `vast_gguf` (implicit), `vllm`, `together`, `local`.

### 4.1 Required-field matrix (`recipe_editor.py:166-175`)

| provider | required fields | note |
|---|---|---|
| *(absent)* / `vast_gguf` | `name, label, gpu, model_repo, model_quant, ctx` | + `gpu` must exist in `gpu_tiers` |
| `vllm` | `name, label, gpu, model_id, ctx` | + `gpu` must exist in `gpu_tiers` |
| `together` | `name, label, model_id` | no GPU check |
| `local` | `name, label, model_path, port` | no GPU check; **`ctx` is NOT required** |

Cross-cutting validation, and this is the *entire* rule set:

```python
name must be all(c.isalnum() or c in "-_.")          # slug check, no uniqueness check!
ctx, if present, must be int >= 1
gpu, if set and provider not in (local, together), must be a key of gpu_tiers
```

Uniqueness of `name` is **not** validated by `validate_recipe`; it is only checked opportunistically
in the create-flow (`editor_menus.py:425`). Duplicate names would silently shadow in `find_recipe`.

### 4.2 Full key schema

Legend for **Req**: `R` required, `O` optional, `—` not applicable to that kind.

| Key | Type | vast_gguf | vllm | together | local | Default when absent | Consumed as |
|---|---|:--:|:--:|:--:|:--:|---|---|
| `name` | `string` (slug) | R | R | R | R | — | `MODEL` env; PID/log/meta filenames for local; identity for CRUD |
| `label` | `string` | R | R | R | R | falls back to `name` | menu text only |
| `provider` | `string` enum | O (omit) | R | R | R | `"vast_gguf"` | discriminator |
| `description` | `string` | O (54/54) | O (7/7) | O (7/7) | O (3/3) | `""` | display only |
| `gpu` | `string` → tier key | R | R | — | — | — | tier lookup; wizard grouping |
| `ctx` | `integer` | R | R | R | O | `65536` (vast), `32768` (local), `131072` (vllm sh) | `CTX` env → `--ctx-size` / `--max-model-len`. **TOTAL across slots** |
| `model_repo` | `string` (HF repo) | R | — | — | — | `""` | `MODEL_REPO` env → `hf download` |
| `model_quant` | `string` | R | — | — | — | `""` | `MODEL_QUANT` env → `--include "*${Q}*.gguf"` and `grep -iE`. **A substring/regex, not an enum** |
| `parallel` | `integer` | O (54/54) | — | — | O (3/3) | `1` | `PARALLEL` env → `--parallel` |
| `kv_type` | `string` enum | O (54/54) | — | — | O (3/3) | `"q8_0"` | `KV_TYPE` env → `--cache-type-k/v`. Allowed by UI: `q8_0`, `q4_0`, `bf16`. Present in file: `q8_0` (48), `q4_0` (9) |
| `mmproj` | `string` | O (2/54) | — | — | O | `""` → wizard prompts | `MMPROJ` env → downloads `*mmproj-${V}*.gguf`. Vast: a **tag** (`"F16"`). Local: a **path** |
| `min_disk_gb` | `integer` | O (4/54) | O | — | — | tier value, then `60` | `--disk` on create. See §6.2 |
| `image_type` | `string` enum | O (7/54) | R-by-convention (7/7 = `"vllm"`) | — | — | tier value, then `"prebuilt"` | selects `[docker]` entry + `IMAGE_TYPE` env |
| `llama_cpp_repo` | `string` `user/repo` | O (7/54) | — | — | — | `ggml-org/llama.cpp` | `LLAMA_CPP_REPO` env; forces a source re-clone in `launch.sh` |
| `llama_cpp_ref` | `string` branch/tag/sha | O (7/54) | — | — | — | `master` | `LLAMA_CPP_REF` env. **Implies `image_type = "builder"`** (the editor enforces this; the file does not) |
| `num_gpus` | `integer` | O (0/54) | O (0/7) | — | — | tier value, then `1` | recipe-level override of the tier — schema key with zero instances |
| `min_cuda` | `string` | O (0/54) | O (0/7) | — | — | tier value, then `"12.8"` | same — zero instances |
| `model_id` | `string` (HF id / provider id) | — | R | R | — | — | vllm: `MODEL_ID` env → `vllm serve --model`. together: the managed model name |
| `kv_cache_dtype` | `string` enum | — | O (7/7) | — | — | `auto` | `KV_CACHE_DTYPE` env → `--kv-cache-dtype`. UI enum: `auto\|fp8\|fp8_e5m2\|fp8_e4m3`. All 7 use `"fp8"` |
| `enforce_eager` | **`string`** `"true"`/`"false"` | — | O (4/7) | — | — | `false` | `ENFORCE_EAGER` env → `--enforce-eager`. **Stringly-typed bool, see §6.3** |
| `reasoning_parser` | `string` | — | O (7/7) | — | — | unset | `REASONING_PARSER` env → `--enable-reasoning --reasoning-parser`. All 7 use `"deepseek_r1"` |
| `quantization` | `string` | — | O (0/7) | — | — | unset | `QUANTIZATION` env → `--quantization`. Read by `vast_menus.py:378`, **zero instances in file** |
| `price_input` | `float` ($/1M tok) | — | — | O (7/7) | — | — | **WRITE-ONLY. Never read by any code.** `cost.py` uses its own hardcoded `together_rates` dict |
| `price_output` | `float` ($/1M tok) | — | — | O (7/7) | — | — | same — dead |
| `model_path` | `string` (may start `~`) | — | — | — | R | — | `_expand_tilde` → `--model`. Must exist at start time or the launch aborts |
| `port` | `integer` | — | — | — | R | `8100` | `--port`; also the key the proxy/`.active_endpoint` uses |
| `host` | `string` | — | — | — | O (0/3) | `"127.0.0.1"` | `--host`. Zero instances |
| `n_gpu_layers` | `integer` | — | — | — | O (3/3) | `999` | `--n-gpu-layers` |
| `backend` | `string` enum | — | — | — | O (3/3) | `""` → auto | binary-path heuristic + env (`GGML_VK_VISIBLE_DEVICES` / `HIP_VISIBLE_DEVICES`). UI enum: `vulkan\|rocm\|cuda\|cpu`. All 3 use `vulkan` |
| `mode` | `string` enum | — | — | — | O (3/3) | `"thinking"` | picks a `SAMPLING_PRESETS` entry. Enum: `thinking\|coding\|nonthinking`. **On the vast path this is NOT a recipe field — the wizard always asks** |
| `binary` | `string` path | — | — | — | O (0/3) | auto-discovered | explicit `llama-server` path. Zero instances |
| `api_key` | `string` | — | — | — | O (0/3) | unset | copied into `.active_endpoint`. Zero instances |

Sampling presets (`config.py:88`, local path) vs `launch.sh:108` (vast path) — **they differ**:

| mode | local (`SAMPLING_PRESETS`) | vast (`launch.sh`) |
|---|---|---|
| `thinking` | `--temp 1.0 --top-p 0.95 --min-p 0.0 --presence-penalty 1.5` | same **+ `--top-k 20`** |
| `coding` | `--temp 0.6 --top-p 0.95 --min-p 0.0 --presence-penalty 0.0` | same **+ `--top-k 20`** |
| `nonthinking` | `--temp 0.7 --top-p 0.80 --min-p 0.0 --presence-penalty 1.5 --chat-template-kwargs {"enable_thinking":false}` | same **+ `--top-k 20`** |

### 4.3 Field resolution order (recipe → tier → constant)

From `vast_menus.py:276-288, 344-385`:

```
image_type   = recipe.image_type   ?? tier.image_type   ?? "prebuilt"
num_gpus     = recipe.num_gpus     ?? tier.num_gpus     ?? 1
min_cuda     = recipe.min_cuda     ?? tier.min_cuda     ?? "12.8"
MIN_DISK_GB  = recipe.min_disk_gb  ?? tier.min_disk_gb  ?? 60      # <- launch env
min_disk     =                        tier.min_disk_gb  ?? 60      # <- browse_offers, BUG §6.2
max_price    = user input          ?? tier.max_price    ?? "0.55"
kv_type      = user input (default seeded from recipe.kv_type ?? "q8_0")
mode         = user input (vast) / recipe.mode ?? "thinking" (local)
mmproj       = recipe.mmproj if set, else user y/n → "F16" | ""
```

`ctx`, `parallel`, `model_repo`, `model_quant` are taken from the recipe with fallbacks
`65536`/`1`/`""`/`""` and are never tier-derived.

### 4.4 `[local]` — dead table

```toml
[local]
```

Empty, last line of the file. `local_menus.py:81` *prints a suggestion* to add
`models_dir = "..."` under it, but nothing ever reads `cfg["local"]`. `discover_local(models_dir)`
takes the value as a function argument that no caller ever supplies. Treat as a stub.

---

## 5. The 71 recipes, categorised

### 5.1 `vast_gguf` — 54 entries (no `provider` key)

Verbatim representative (lines 413–422):

```toml
[[recipes]]
name = "qwen36-27b-q6-5090"
label = "Qwen3.6-27B  Q6_K  96K ctx"
gpu = "5090"
model_repo = "unsloth/Qwen3.6-27B-GGUF"
model_quant = "UD-Q6_K_XL"
ctx = 98304
parallel = 1
kv_type = "q8_0"
description = "Dense 27B near-lossless quality. Best single-user throughput on 5090."
```

Sub-variant A — **custom llama.cpp fork** (7 entries, all DeepSeek-V4-Flash; lines 227–239):

```toml
[[recipes]]
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
description = "DSv4-Flash Q2_K (96 GB) on 2×H100 (160 GB). Lightest quant, fits with KV headroom."
```

Sub-variant B — **vision / mmproj** (2 entries; lines 755–766):

```toml
[[recipes]]
name = "qwen3-vl-30b-q8-h100"
label = "Qwen3-VL-30B-A3B  Q8  8×128K  🖼 vision"
gpu = "h100-sxm"
model_repo = "unsloth/Qwen3-VL-30B-A3B-Instruct-GGUF"
model_quant = "Q8_K_XL"
mmproj = "F16"
ctx = 1048576
parallel = 8
kv_type = "q8_0"
min_disk_gb = 120
description = "Vision MoE: 3B active params + vision encoder. 8 slots × 128K. Image + text in, full ctx depth."
```

The 54 break down by **model family** (13 distinct HF repos):

| HF repo | count | quants used |
|---|---|---|
| `unsloth/Qwen3.6-35B-A3B-GGUF` | 12 | `UD-Q8_K_XL`, `UD-Q6_K_XL`, `UD-Q5_K_XL`, `UD-Q4_K_XL` |
| `unsloth/Qwen3.6-27B-GGUF` | 11 | `UD-Q8_K_XL`, `UD-Q6_K_XL`, `UD-Q5_K_XL`, `UD-Q4_K_XL` |
| `kai-os/Carnice-V2-27b-GGUF` | 8 | `Q8_0`, `Q5_K_M`, `Q4_K_M` |
| `bartowski/Qwen3-72B-GGUF` | 6 | `Q8_0`, `Q6_K_L`, `Q4_K_M` |
| `Preyazz/DeepSeek-V4-Flash-GGUF` | 5 | `Q2_K`, `Q3_K_M`, `Q4_K_M` |
| `samuelcardillo/Carnice-Qwen3.6-MoE-35B-A3B-GGUF` | 5 | `Q8_0`, `Q5_K_M`, `Q4_K_M` |
| `lovedheart/DeepSeek-V4-Flash-GGUF` | 1 | `MXFP4` |
| `Preyazz/DeepSeek-V4-Flash-Q8_0-GGUF` | 1 | `Q8_0` |
| `unsloth/Qwen3.5-27B-GGUF` | 1 | `Q8_0` |
| `unsloth/Qwen3.5-35B-A3B-GGUF` | 1 | `Q8_0` |
| `unsloth/Qwen3.5-122B-A10B-GGUF` | 1 | `Q3_K_M` |
| `unsloth/Qwen3-VL-30B-A3B-Instruct-GGUF` | 1 | `Q8_K_XL` + mmproj |
| `unsloth/Qwen3-VL-32B-Instruct-GGUF` | 1 | `Q6_K` + mmproj |

…and by tier: `h100-sxm` 15, `5090` 9, `6000pro` 8, `4090` 4, `a100-sxm` 3, `h200-sxm` 3,
`h100-sxm-4x` 2, `h100-sxm-2x` 2, `h200-sxm-2x` 2, `h100-pcie` 2, `b200-sxm` 2, `a100-pcie` 1,
`b200-sxm-2x` 1.

**This is the crux: `(13 repos × ~4 quants) × (13 tiers) × (ctx, parallel, kv_type)` hand-enumerated
into 54 rows.** `ctx` ranges 49 152 → 3 145 728; `parallel` ∈ {1,2,3,4,6,8,10,12}. The
`description` fields *contain the arithmetic that produced each row*:

> `"Q4 (21.2 GiB) + hybrid KV (8.2 GiB) = ~29.4 GiB. Fits."`
> `"~29 GiB weights + ~59 GiB KV = ~88 GiB. ~8 GiB headroom on 96GB."`
> `"Hybrid-linear arch: only 10/41 layers use full KV — 2.7 GB per 256K slot."`

Those are three inputs and one comparison. They are a **function**, currently frozen as data.

### 5.2 `vllm` — 7 entries

Verbatim representative (lines 325–336):

```toml
[[recipes]]
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
description = "DSv4-Pro FP4+FP8 (805 GB) on 5×H200 (705 GB). Sweet spot: ~40 t/s, $17.50/hr."
```

All 7: two models (`deepseek-ai/DeepSeek-V4-Pro` ×5, `deepseek-ai/DeepSeek-V4-Flash` ×2),
`kv_cache_dtype = "fp8"` and `reasoning_parser = "deepseek_r1"` on every one,
`enforce_eager` on 4 (`"true"` ×3, `"false"` ×1). `ctx` ∈ {131072, 262144, 393216, 524288}.
The only real axes are `gpu` (5 tiers) and `ctx`.

### 5.3 `together` — 7 entries

Verbatim representative (lines 925–933):

```toml
[[recipes]]
name = "together-llama3.1-8b"
provider = "together"
label = "Llama 3.1 8B ($0.18/M tokens)"
model_id = "meta-llama/Llama-3.1-8B-Instruct-Turbo"
ctx = 131072
price_input = 0.18
price_output = 0.18
description = "Fastest/cheapest on Together. Great for quick tasks."
```

| name | model_id | ctx | $/1M in/out |
|---|---|---|---|
| `together-llama3.1-8b` | `meta-llama/Llama-3.1-8B-Instruct-Turbo` | 131072 | 0.18 |
| `together-qwen-coder-32b` | `Qwen/Qwen2.5-Coder-32B-Instruct-Turbo` | 131072 | 0.44 |
| `together-llama3.3-70b` | `meta-llama/Llama-3.3-70B-Instruct-Turbo` | 131072 | 0.88 |
| `together-qwen2.5-72b` | `Qwen/Qwen2.5-72B-Instruct-Turbo` | 131072 | 0.88 |
| `together-llama3.1-405b` | `meta-llama/Llama-3.1-405B-Instruct-Turbo` | 131072 | 3.50 |
| `together-mixtral-8x7b` | `mistralai/Mixtral-8x7B-Instruct-v0.1` | 32768 | 0.60 |
| `together-qwq-32b` | `Qwen/QwQ-32B-Preview` | 32768 | 0.44 |

**These 7 are almost entirely dead weight.** The Together launch path
(`vast_menus.py:110-116`) does *not* read them — it has its own hardcoded `popular_models` list of
5 entries. `cost.py:57-65` has a *third* copy of the same 7 model IDs with the same prices. The
recipes are consulted only in `tool_menus.show_status` to test whether the active model_id matches
a known recipe name — and even then the displayed price comes from `estimate_cost`, i.e. from
`cost.py`'s dict, not from `price_input`. Deleting all 7 changes almost nothing.

### 5.4 `local` — 3 entries

Verbatim representative (lines 995–1007):

```toml
[[recipes]]
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
description = "Qwen3.5-9B on AMD iGPU via Vulkan backend. ~4 GB VRAM."
```

| name | model_path | port | ctx | mode |
|---|---|---|---|---|
| `local-qwen35-9b` | `~/models/Qwen3.5-9B-Q4_K_M.gguf` | 8100 | 32768 | thinking |
| `local-qwen35-9b-coding` | `~/models/Qwen3.5-9B-Q4_K_M.gguf` | 8101 | 65536 | coding |
| `local-carnice-9b` | `~/models/carnice-9b/Carnice-9b-Q6_K.gguf` | 8102 | 32768 | thinking |

**2 of 3 point at a file that no longer exists** (see `00-machine-ground-truth.md`: only
`Carnice-9b-Q6_K.gguf` is present). This is the strongest single argument for discovery: a
hand-written catalogue of local paths rots the moment a model is deleted, and LocalRouter only
finds out at launch time, after it has already written a PID file.

---

## 6. Latent bugs and traps found while reverse-engineering

### 6.1 Three vLLM recipes are unreachable from the UI

The launch wizard partitions **tiers** by `tier.image_type == "vllm"` (`vast_menus.py:153-157`) and
then partitions **recipes** by `recipe.provider == "vllm"` (`:172-176`). A vLLM recipe whose tier is
not itself `image_type = "vllm"` therefore appears in neither branch:

| recipe | its tier | tier.image_type | reachable? |
|---|---|---|---|
| `dsv4-pro-4xb200` | `b200-sxm-4x` | `builder` | **no** |
| `dsv4-flash-vllm-4xh100` | `h100-sxm-4x` | `builder` | **no** |
| `dsv4-flash-vllm-2xh200` | `h200-sxm-2x` | `builder` | **no** |

`5090-dc` is unreachable for the mirror reason (a tier with zero recipes dead-ends the wizard).
**43 % of the vLLM catalogue and 5 % of the tier catalogue are unusable and nobody noticed** —
which is itself evidence that hand-maintained cross-referenced catalogues do not survive.

### 6.2 `min_disk_gb` resolution is inconsistent

`browse_offers` is called with `min_disk = tier_cfg.get("min_disk_gb", 60)`, ignoring the recipe
override, while the launch env uses `recipe.min_disk_gb ?? tier.min_disk_gb ?? 60`. So
`qwen3-vl-30b-q8-h100` (recipe wants 120 GB) browses offers filtered at `disk_space>100` and can be
pinned to a host that then gets `--disk 120` at create time.

### 6.3 `enforce_eager` is a string, and Python truthiness betrays it

`vast_menus.py:382`: `if chosen_recipe.get("enforce_eager"):` — the string `"false"` is **truthy**,
so `dsv4-pro-5xh200` exports `ENFORCE_EAGER=false`. It happens to be harmless because
`launch_vllm.sh:89` tests `= "true"`, but any refactor that treats the env var as "set ⇒ enabled"
inverts the behaviour of that recipe. In Rust: make it `bool`, and on legacy import parse
`"true"|"1"|"yes"` case-insensitively, everything else `false`.

### 6.4 Other type/consistency traps

- `max_price` is a TOML **string**; `price_input`/`price_output` are TOML **floats**; `min_disk_gb`,
  `vram_gb`, `num_gpus`, `ctx`, `parallel`, `port`, `n_gpu_layers` are **integers**. Do not assume
  numeric-looking values are numbers.
- The editor's `_coerce_value` will happily turn `"12.8"` typed into the `min_cuda` field into the
  **float** `12.8`, silently changing its TOML type on the next save.
- `tomli_w.dump` round-tripping **destroys all comments and key ordering** and rewrites
  `recipes.toml` wholesale (a `.toml.bak` is written first). The current file has no comments, so
  this has not bitten yet.
- `model_quant` is used as both a `--include` glob (`*${Q}*.gguf`) and a `grep -iE` regex. A quant
  string containing regex metacharacters would misbehave. `UD-Q6_K_XL` is safe; user input is not.
- `ctx` is the **total** context pool; per-slot context is `ctx / parallel`. Every multi-slot label
  in the file confirms this (`ctx = 2097152`, `parallel = 8` ⇒ `8×256K`).
- `vram_gb` is **per GPU**; `label` states the total. Anything computing fit must multiply.

---

## 7. Verdict: CONFIG vs CATALOG vs COMPUTED

Splitting the file three ways by *what kind of thing each part is*:

### 7.1 Genuine CONFIG — keep, hand-edited, small, in the repo/XDG config

| Thing | Why it is config | Change vs today |
|---|---|---|
| `[docker]` image map | These are artifacts **Andre publishes**; no API can discover them | Keep. Reduce to `llamacpp_prebuilt`, `llamacpp_builder`, `vllm`. Drop `prebuilt_legacy`. One source of truth (delete the copies in `vast_up.sh` and `config.py`) |
| GPU **search profiles** (née tiers) | The *shortlist of hardware Andre is willing to rent* is a preference, not a fact | Keep, but shrink to 4–5 (§8) and rename the concept: it is a saved offer-search, not a hardware catalogue |
| Geo regions, reliability/bandwidth floors, offer-list length, CUDA warn threshold | Real knobs, currently hardcoded in three languages | **Promote into config** — they are more deserving of a TOML home than the 54 recipes are |
| Sampling presets (`thinking`/`coding`/`nonthinking`) | Andre's opinions about temperature | Keep as config; unify the local/vast divergence (`--top-k 20`) |
| KV-type menu, backend menu | Small closed enums | Keep as code-level enums, validated against a runtime `--help` probe (see `00-machine-ground-truth.md`) |
| Local scan roots (`models_dir`, llama.cpp build dirs) | Machine-specific paths | **Add** — this is the `[local]` table's unfulfilled promise. Config supplies *where to look*; discovery supplies *what is there* |
| Provider credentials | Already in `~/.vastai-gguf/config.toml` | Keep separate from recipes; never in the same file |

### 7.2 Hardcoded CATALOG — delete, replace with live DISCOVERY

| Catalogue in the file | Replace with | Already half-built in LocalRouter |
|---|---|---|
| 13 `model_repo` values + 13 `model_quant` values (54 rows) | **HF model search** (`/api/models?search=&filter=gguf`) + **HF file listing** (`/api/models/{id}?blobs=true` → `siblings[].rfilename`, `.size`) | `hf_browser._hf_list_files` does exactly the second half already, and `.hf_pin` already bypasses the recipe |
| 7 `together` recipes + `price_input`/`price_output` | **Together `/v1/models`** (returns id, context_length, pricing) | nothing — but `cost.py`'s dict and `vast_menus`'s `popular_models` are two dead copies to delete |
| 3 `local` recipes' `model_path` | **Local GGUF scan** | `local_endpoint.discover_local()` already scans `~/models`, HF hub cache, and probes binaries/backends. It is *better* than the recipes and is currently ignored by the launch path |
| `vram_gb`, `num_gpus`, `label` price hints | **Live offer search** — `dph_total`, `gpu_ram`, `num_gpus`, `reliability2`, `inet_down`, `cuda_max_good`, `geolocation` all come back per offer | `vast_ops.browse_offers` already renders this table; it just isn't allowed to *define* anything |
| `cost.py::vast_hourly_rates`, `cost.py::together_rates` | derived from the above | — |
| 7 `vllm` recipes' `model_id` | HF search filtered to non-GGUF repos | — |

**Discovery sources for ApexRouter-RS (three, all already proven necessary):**

1. **Vast offer search** — REST (`GET /api/v0/bundles`, not the broken CLI). Returns the real
   hardware, real price, real CUDA, real bandwidth, right now.
2. **HF model + file search** — `huggingface.co/api/models` and `.../models/{id}?blobs=true`.
   Returns the real repos, real quant filenames, and **real file sizes** — which is the missing
   input for automatic VRAM fit.
3. **Local scan** — filesystem walk for `*.gguf` + `llama-server` binaries + backend probe.

### 7.3 COMPUTED — neither config nor catalogue: this should be a solver

`ctx`, `parallel`, `kv_type`, and "does it fit" are the *output* of a calculation that the 54
`description` strings spell out longhand. Replace the catalogue with:

```
fit(model, gpu_budget) -> {max_ctx, max_parallel, kv_type, headroom}

weights_bytes  <- sum of GGUF shard sizes from the HF blobs API (exact, free, no download)
kv_bytes       <- 2 * n_kv_layers * n_head_kv * head_dim * bytes(kv_type) * ctx * parallel
                  where n_kv_layers accounts for hybrid-linear archs (Qwen3.6 MoE: 10 of 41)
budget         <- num_gpus * vram_gb_from_the_actual_offer * safety_factor (~0.92)
```

Read `n_layer`/`n_head_kv`/`n_embd_head` from the GGUF header (first 4 KB of the first shard via an
HTTP range request, or locally from the file) rather than hardcoding per-model. This single
function subsumes 54 hand-tuned rows and, unlike them, works for a model published tomorrow.

Also worth keeping as *derived hints*, not config: `image_type` should be inferred
(`llama_cpp_ref` set ⇒ builder; engine == vllm ⇒ vllm; else prebuilt) rather than stored on
both the tier and the recipe with an override chain.

---

## 8. The reduced fixed tier set Andre asked for

Target: **2–4× RTX 3090, and up to 2× H100 NVIDIA.** None of the 3090 tiers exist today, so this is
new config, not a filter over the old file. Proposed, in the shape §9 uses:

| key | vast_names | num_gpus | vram/GPU | total | suggested max_price | min_disk_gb | engine default |
|---|---|---|---|---|---|---|---|
| `3090-2x` | `RTX_3090` | 2 | 24 | 48 GB | verify live | 80 | llama.cpp prebuilt |
| `3090-4x` | `RTX_3090` | 4 | 24 | 96 GB | verify live | 150 | llama.cpp prebuilt |
| `h100-1x` | `H100_SXM`, `H100_SXM5`, `H100X`, `H100_PCIE`, `H100_NVL` | 1 | 80 | 80 GB | verify live | 100 | llama.cpp builder |
| `h100-2x` | `H100_SXM`, `H100_SXM5`, `H100X`, `H100_PCIE`, `H100_NVL` | 2 | 80 | 160 GB | verify live | 200 | llama.cpp builder |

Optionally `3090-3x` (72 GB) — the "2–4×" phrasing suggests `num_gpus` should be a *range filter*
on the search rather than a discrete tier per count. Strongly recommended: make it
`num_gpus_min`/`num_gpus_max` and let one profile cover 2–4× 3090, collapsing three tiers into one.

Deliberately **do not** carry over any price in the tier. Set a ceiling only as a *filter*, and
default it from a live percentile of current offers rather than a hand-typed constant that ages.

3090-specific things the port must get right, none of which the old file could express:

- **Ampere SM86.** The `builder` image compiles for the detected `compute_cap`, so it works, but the
  `prebuilt` image must actually ship SM86 cubins — verify before trusting `image_type = "prebuilt"`
  on the 3090 tiers.
- **No FP8.** `kv_cache_dtype = "fp8"` and vLLM FP8 weights are H100+. A 3090 profile must reject
  or downgrade those settings; today nothing would stop the combination.
- **`cuda_vers >= 12.8` is likely too strict for 3090 hosts.** Do not hardcode it; surface it as a
  slider whose default comes from what current offers actually report.
- 24 GB/GPU with **no NVLink guarantee** ⇒ llama.cpp `--split-mode layer` across 2–4 cards is the
  realistic path; vLLM tensor-parallel across 4× 3090 over PCIe is possible but bandwidth-bound.
  The engine choice should be a *consequence* of the profile, not a free field.

---

## 9. What a "recipe" must be if it is GUI-built rather than pre-written

Reframe: today a recipe is **an authored catalogue entry** that the UI reads. It should become
**a saved result of a discovery session** that the UI writes. Two consequences:

1. It lives in **state**, not config (`$XDG_STATE_HOME/apexrouter/recipes/*.toml` or one
   `recipes.json`), alongside `.active_endpoint`-equivalents — never in the repo (see
   `00-machine-ground-truth.md` on LocalRouter writing state into its own source dir).
2. It carries **provenance and staleness**, because everything in it was true at discovery time and
   may not be true now.

### 9.1 Minimal viable recipe

Four required things, universally: **an id, a target, a model, and a runtime.** Everything else is
either derived, defaulted, or provenance.

```toml
schema  = 1
id      = "01JX...ULID"          # generated; stable identity, never user-typed
label   = "Carnice-9B Q6_K · Vulkan · 32K"   # auto-composed, user-overridable
created = "2026-07-30T14:02:11Z"

[target]                          # WHERE it runs — exactly one variant
kind = "local"                    # local | vast | api
# local:
device  = "Vulkan0"               # from the local backend probe
binary  = "/home/andre/llama.cpp/build-vulkan/bin/llama-server"   # resolved, not guessed
port    = 8100
# vast:  profile = "h100-2x"  (or an inline offer filter)  [+ pinned offer_id, optional]
# api:   base_url + credential_ref

[model]                           # WHAT runs — exactly one source
source = "local"                  # local | hf | api
path   = "/home/andre/models/carnice-9b/Carnice-9b-Q6_K.gguf"
# hf:    repo, file (exact filename, NOT a substring), revision, size_bytes
# api:   model_id
size_bytes = 6900000000           # captured at discovery, used for fit + staleness check

[engine]
kind = "llama_cpp"                # llama_cpp | vllm | remote_openai
image = "ghcr.io/buckster123/vastai-gguf:prebuilt"   # vast only; derived from engine+fork
# fork = "fairydreaming/llama.cpp" ; ref = "deepseek-dsa"   # optional; implies builder image

[runtime]
ctx      = 32768                  # TOTAL across slots — keep the old semantics, document it
parallel = 1
kv_type  = "q8_0"
mode     = "thinking"
# ngl, mmproj, extra_args, host, api_key: optional overrides

[provenance]                      # NEW — what discovery told us when this was built
discovered_at = "2026-07-30T14:01:58Z"
fit_estimate  = { weights_gib = 6.4, kv_gib = 1.1, budget_gib = 19.0, headroom_gib = 11.5 }
notes = "auto: fits with 60% headroom"
```

**Absolute minimum a GUI must collect per kind** (everything else defaulted or derived):

| kind | user must choose | derived automatically |
|---|---|---|
| local | a discovered model file | binary+device from the backend probe, `ctx`/`parallel`/`kv_type` from the fit solver, port from the first free one |
| vast + llama.cpp | a profile, a discovered HF repo, a discovered quant **file** | image (engine+fork), disk (from file size ×1.3), `ctx`/`parallel`/`kv_type` from fit against the profile's VRAM, `num_gpus`/`min_cuda` from the profile |
| vast + vLLM | a profile, an HF model id | image, `ctx` from fit, TP from `num_gpus`, `kv_cache_dtype` gated on GPU capability |
| api | a provider + a model from its live `/models` | ctx and pricing from the API response |

### 9.2 Rules the new format should enforce that the old one did not

- `id` is generated, not typed ⇒ no uniqueness bug, and renaming the label is free.
- `model.file` is an **exact filename**, not a substring/regex. Discovery knows the exact file; keep
  the substring behaviour only as a legacy-import escape hatch.
- `provider` is never implicit. Write `target.kind` + `engine.kind` explicitly on every record.
- No key may exist in two places with an override chain (`image_type`, `num_gpus`, `min_cuda`,
  `min_disk_gb` are all currently dual-homed). Profiles own hardware; recipes own the model+runtime.
- Every recipe is **re-validatable**: `validate(recipe) -> {ok | stale(reason)}` — model file gone,
  HF revision moved, profile deleted, offer no longer rentable. LocalRouter had 2 of 3 local
  recipes pointing at a deleted file with no way to know.
- Recipes are **disposable**. The GUI should be able to produce a working launch with zero saved
  recipes; saving one is an optimisation ("run this again"), not a prerequisite.

### 9.3 Migration from the existing file

Worth writing a one-shot importer, mostly to preserve the DeepSeek fork knowledge:

- 54 `vast_gguf` → drop; the (repo, quant) pairs become **seed entries in an HF search-history /
  favourites list**, not recipes. Keep the 7 fork recipes' `llama_cpp_repo`/`llama_cpp_ref` as a
  small `known_forks` config table keyed by model family — that is real, hard-won, undiscoverable
  knowledge.
- 7 `vllm` → drop; keep `deepseek-ai/DeepSeek-V4-{Pro,Flash}` as favourites.
- 7 `together` → drop entirely; fetch live.
- 3 `local` → drop; the scan finds `Carnice-9b-Q6_K.gguf` and the other two are already broken.
- 19 tiers → drop 15, replace with the 4–5 profiles in §8.
- `[docker]` → keep 3 of 4 keys.

Net: **~1050 lines of TOML becomes ~40 lines of config plus three live queries.**

---

## 10. Contracts the Rust port must not break

Even with discovery replacing the catalogue, these external interfaces are fixed by artifacts that
already exist in the world:

**`launch.sh` env contract** (the `:prebuilt` / `:builder` images consume exactly these):
`MODEL_REPO`(req), `MODEL_QUANT`(req), `IMAGE_TYPE`, `CTX`, `KV_TYPE`, `MODE`, `N_GPU_LAYERS`,
`PARALLEL`, `EXTRA_ARGS`, `HF_TOKEN`, `MODELS_DIR`, `MMPROJ`, `PORT`, `HOST`, `LLAMA_CPP_REPO`,
`LLAMA_CPP_REF`. `MODE` must be one of `thinking|coding|nonthinking` or the container dies.

**`launch_vllm.sh` env contract**: `MODEL_ID`(req), `TP`, `CTX`, `QUANTIZATION`, `KV_CACHE_DTYPE`,
`GPU_UTIL`, `EXTRA_ARGS`, `HF_TOKEN`, `PORT`, `HOST`, `DTYPE`, `MAX_NUM_SEQS`, `TRUST_REMOTE`,
`ENFORCE_EAGER`, `CHUNKED_PREFILL`, `REASONING_PARSER`. Booleans are compared to the literal
string `"true"`.

**`vast_up.sh`**: additionally `GPU`, `VAST_NAMES` (space-separated), `MODEL`, `GEO`, `MIN_CUDA`,
`NUM_GPUS`, `MAX_PRICE`, `MIN_DISK_GB`, `OFFER_ID`, `DOCKER_IMAGE`. If ApexRouter talks to the Vast
REST API directly (it must — the CLI is broken on this machine) it replaces `vast_up.sh` but still
has to produce the same `--onstart-cmd` string and `--env` blob, or the images will not boot.

**Vast search filter grammar**: `gpu_name=X` / `gpu_name in [A,B]`, `num_gpus=N`,
`reliability>F`, `inet_down>N`, `dph_total<F`, `disk_space>N`, `cuda_vers>=V`, `rentable=true`,
`--order dph_total`. Over REST this becomes a JSON query object, not a string — needs translation.

**HF API**: `GET https://huggingface.co/api/models/{repo}?blobs=true` → `siblings[]` with
`rfilename` and `size`. Bearer token from `~/.cache/huggingface/token`. Container-side download is
`hf download {repo} --local-dir {dir} --include "*{quant}*.gguf"` with
`HF_HUB_ENABLE_HF_TRANSFER=1`.

**On-disk state that must stay readable**: see `00-machine-ground-truth.md` §"Existing LocalRouter
state" — `~/.vastai-gguf/{config.toml,.pinned_provider,usage.log,local_instances/,local_logs/}`.
`.hf_pin` (`{MODEL_REPO, MODEL_QUANT, filename, size}`) is the one piece of LocalRouter that already
worked the discovery way and should be generalised, not ported as-is.
