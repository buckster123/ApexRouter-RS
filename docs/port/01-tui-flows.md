# 01 — LocalRouter TUI: exhaustive interactive-flow inventory

**Source of truth for the ApexRouter-RS GUI rewrite.**
Derived from a complete read of:

```
localrouter/__main__.py
localrouter/menus/main.py
localrouter/menus/local_menus.py
localrouter/menus/vast_menus.py
localrouter/menus/provider_menus.py
localrouter/menus/tool_menus.py
localrouter/menus/editor_menus.py
README.md
```

plus the modules those call into (`config.py`, `helpers.py`, `providers.py`, `local_endpoint.py`,
`vast_ops.py`, `hf_browser.py`, `cost.py`, `proxy.py`, `recipe_editor.py`, `endpoint_proxy.py`,
`vast_up.sh`, `vast_down.sh`, `tools/vast_tunnel.sh`, `smoke.sh`).

Read `00-machine-ground-truth.md` first — it overrides anything here about the *current* toolchain
(notably: the `vastai` CLI is broken on this machine, so every `vastai …` shell-out documented below
must be re-implemented as a direct REST call in Rust).

---

## 0. Execution model — what the TUI *is*

| Aspect | LocalRouter (Python TUI) |
|---|---|
| Entry | `localrouter` console script, or `python -m localrouter` → `menus.main.main()` |
| Loop | `while True:` → `console.clear()` → banner → `show_status()` → one `questionary.select` |
| Widgets | `questionary.select`, `.checkbox`, `.text`, `.password`, `.confirm`, `.autocomplete` |
| Display | `rich` `Panel` + `Table`, brand colour `#7c6af7`, border `#3d3d5c` |
| Nav sentinel | The literal string `"← Back"` appended by `helpers.ask_back()` |
| Cancel | `Ctrl-C` at a prompt → `.ask()` returns `None` → treated as Back |
| Global exit | `KeyboardInterrupt` caught in `__main__.py` → `sys.exit(0)` |
| Blocking | **Everything.** Every action is synchronous; `helpers.press_enter()` (`input()`) gates the return to the previous menu after almost every action |
| Concurrency | None. One SSH probe, one HTTP request, one poll loop at a time |
| Config reload | `load_config()` at startup, and again after `Editor` exits; `menu_local_dispatch` re-reads `recipes.toml` **on every loop iteration** just to render a count |

**Dispatch is by string prefix.** `main()` matches on `choice.startswith("Launch")`,
`startswith("Local")`, etc. Menu labels are simultaneously the display text *and* the dispatch key —
a fragile coupling the GUI must not reproduce (use typed enums / message variants).

### The blocking-status problem (most important single finding)

`show_status()` runs on **every** return to the main menu, after `console.clear()`. It performs, in
sequence and synchronously:

1. `last_instance()` — read `.last_instance`
2. `get_active_endpoint()` — read `.active_endpoint`, may stat a local PID
3. `vastai show instance <id> --raw` — **12 s timeout**
4. `curl … /health` — 3 s
5. `curl … /v1/models | jq` — 3 s
6. `curl … /slots | jq` — 3 s

Worst case ≈ **21 s of dead terminal** between pressing Back and seeing the main menu again. In the
GUI this must become a background poller feeding a persistent status region, never a blocking
pre-render step.

---

## 1. Global state — every file the TUI reads or writes

The GUI must be able to read all of these for migration (see `00-machine-ground-truth.md` §"Existing
LocalRouter state on disk").

| Path | Format | Written by | Read by |
|---|---|---|---|
| `<repo>/.last_instance` | plain text, one Vast instance id | `vast_up.sh`, **Instances** screen | status panel, Watch, Diagnose, Destroy, Tunnel, `vast_tunnel.sh` |
| `<repo>/.active_endpoint` | JSON | Together activation, local launch | status panel, Diagnose, Smoke, proxy `resolve_target()`, `smoke.sh --provider` |
| `<repo>/.hf_pin` | JSON `{MODEL_REPO, MODEL_QUANT, filename, size}` | HF Browse | Launch wizard; **deleted** on successful `vast_up.sh` |
| `<repo>/recipes.toml` | TOML | Editor → `tomli_w.dump` | everything |
| `<repo>/recipes.toml.bak` | TOML | Editor save (pre-write copy) | — |
| `~/.vastai-gguf/config.toml` | TOML `[providers.<name>]` `base_url`, `api_key` | Providers screen (`save_provider_config`, hand-rolled writer) | startup, Together flows, proxy, `smoke.sh` |
| `~/.vastai-gguf/.pinned_provider` | JSON `{provider, model_id, base_url}` | Together model browser | Launch wizard (Together branch). **Never cleared.** |
| `~/.vastai-gguf/usage.log` | JSONL | `cost.log_completion()` | Diagnose |
| `~/.vastai-gguf/local_instances/<name>.pid` | plain text PID | local launch/stop | `is_local_running()` |
| `~/.vastai-gguf/local_instances/<name>.json` | JSON metadata | local launch/stop | Local Status |
| `~/.vastai-gguf/local_logs/<name>.log` | text (stdout+stderr) | llama-server child | Local Status → View logs |
| `/tmp/vastai-gguf-tunnel.pid` | PID | `vast_tunnel.sh up` | `tunnel_running()` |
| `/tmp/vastai-gguf-tunnel.ssh` | ssh config | `vast_tunnel.sh` | — |
| `/tmp/vastai-gguf-proxy.pid` | PID | Proxy → `_proxy_up()` | Proxy, Smoke |
| `/tmp/vastai-gguf-proxy.log` | text | proxy child | Proxy → logs |

**Ports:** `LOCAL_PORT = 8800` (SSH tunnel local side), container-side `8000`, `PROXY_PORT = 8888`
(unified proxy), local llama-server default `8100`.

**`.active_endpoint` shapes:**

```jsonc
// together
{"provider":"together","model_id":"…","base_url":"https://api.together.ai/v1",
 "endpoint":"https://api.together.ai/v1/chat/completions","activated_at":"…Z"}
// local
{"provider":"local","name":"…","host":"127.0.0.1","port":8100,"pid":1234,
 "model_path":"…","activated_at":"…Z","api_key":"…"/*optional*/}
// vast — NOT a file; synthesized from .last_instance by get_active_endpoint()
{"provider":"vast-gguf","instance_id":"…","status":"running"}
```

Note the provider-key inconsistency the port should normalise: `"vast-gguf"` (hyphen, runtime),
`"vast_gguf"` (underscore, recipe default in `recipe_editor.py` and `_provider_color`), and
`"vllm"`/`"local"`/`"together"`.

---

## 2. Screen map

```
S1 Main menu  (banner + live status panel + 15-item select)
├─ S2  Launch ─── provider select ──┬─ S2T Together activate
│                                   ├─ S2L → S3a Local launch wizard
│                                   └─ S2V Vast GGUF / vLLM wizard (8 steps)
│                                        └─ S2O Offer browser
├─ S3  Local ───┬─ S3a Launch wizard
│               ├─ S3b Status → instance → action → logs | stop
│               └─ S3c Configure (scan / set models dir)
├─ S4  Providers ─── S4a Configure Together AI (5 steps)
├─ S5  Together model browser → family → list → pin
├─ S6  Batch compare (checkbox → prompt → sequential run → results)
├─ S7  Watch boot (10 s poll loop, Ctrl-C only)
├─ S8  Diagnose (usage → rate limits → [vast only] SSH probes → stall recovery)
├─ S9  Instances (list → set .last_instance)
├─ S10 HF Browse → repo → file table → pin quant
├─ S11 Editor ──┬─ S11a Recipes (browse/create/edit/dup/delete)
│               │      ├─ S11a1 Create wizard (4 provider branches)
│               │      └─ S11a2 Field editor
│               ├─ S11b GPU tiers (create wizard / field editor / delete)
│               ├─ S11c Docker images (add-update / remove)
│               └─ Save | Reload | dirty flag
├─ S12 Tunnel (up/status/down/logs)
├─ S13 Smoke (URL prompt → smoke.sh)
├─ S14 Proxy (up/down/logs/status)
└─ S15 Destroy (confirm → tunnel down → vast_down.sh)
```

Maximum nesting depth to a leaf action: **6** (Main → Editor → Recipes → Edit → field list → field
value prompt → confirm-delete-field).

---

## S1 — Main menu

**File:** `menus/main.py:27-77`

**Purpose:** Home. Shows a live snapshot of "what is currently serving" and routes to 14 features.

### Displays

**Banner** (`banner()`): app name, tagline `GGUF endpoint manager — local, Vast.ai & managed`,
`image: <docker prebuilt tag>  |  recipes: recipes.toml`.

**Status panel** (`tool_menus.show_status`, `tool_menus.py:218-295`) — titled *current state*, a
2-column dim/bold table. Rows are conditional:

| Row | Condition | Value |
|---|---|---|
| `endpoint` | `.active_endpoint` provider = together | `Together AI (managed)` |
| `model` | ″ | `model_id` |
| `est. cost` | ″ and a matching `provider=together` recipe exists | `$X.XXXX (rate, 1k+500 tok)` from `estimate_cost` |
| `endpoint` | provider = local | `Local (<name>)  running|stopped` |
| `model` | ″ | basename of `model_path`, 50 chars |
| `port` | ″ | `127.0.0.1:<port>` |
| `endpoint` | otherwise | `Vast GGUF (self-hosted)` |
| `instance id` / `status` / `gpu` / `$/hr` / `geo` / `ssh` | `.last_instance` exists and `vastai show instance` succeeds | from instance JSON; status colour green/yellow/red |
| `instance` | lookup failed | `<id> (vastai lookup failed)` |
| `instance` | no `.last_instance` | `none` |
| `tunnel` | always | `up` / `down` from PID liveness |
| `endpoint` / `model` / `slots` | tunnel up **and** `/health` returns `"ok"` | `healthy`, model id from `/v1/models`, slot count from `/slots` |
| `endpoint` | tunnel up, health bad | `unreachable` |

> **Quirk:** when an `.active_endpoint` exists *and* the tunnel is healthy, the table gets **two rows
> both keyed `endpoint`** and two keyed `model`. The panel is a bag of rows, not a model.

### Decision asked

One `select`, 15 options, no shortcuts (`use_shortcuts=False`):

`Launch · Local · Providers · Together · Batch · Watch · Diagnose · Instances · HF Browse · Editor ·
Tunnel · Smoke · Proxy · Destroy · Exit`

### Backend calls

`last_instance()`, `get_active_endpoint()`, `vastai show instance --raw`, 3× `curl` (+`jq`),
`load_config()` (recipe scan for the Together price lookup), `estimate_cost()`.

### State mutated

None directly. `Editor` triggers a `load_config()` refresh on return.

### GUI notes

- The status panel is the single most valuable artefact in the whole TUI — it must become a
  **persistent, always-visible, background-refreshed** header/sidebar, not a re-render gate.
- Rows are provider-conditional today; the GUI wants a **uniform endpoint card** (provider badge,
  model, address, health dot, cost meter) that renders for local / vast / vllm / together alike.
- 14 flat menu entries is a poor top level. Real groupings: **Serve** (Launch, Local, Instances,
  Destroy), **Connect** (Tunnel, Proxy, Providers), **Inspect** (Watch, Diagnose, Smoke, Batch),
  **Catalog** (HF Browse, Together, Editor).
- `Exit` is a menu item because there is no window chrome. Drop it.

---

## S2 — Launch wizard

**File:** `menus/vast_menus.py:47-398` (`menu_launch`)

**Purpose:** The money path — provision an endpoint. Four mutually exclusive sub-flows behind one
entry.

### Pre-step: pin banners (display only)

- If `.hf_pin` exists → green panel: `repo`, `quant (size)`, "Select the pinned option in the recipe
  step to use it."
- `~/.vastai-gguf/.pinned_provider` is loaded silently (no banner) for the Together branch.

### Step 0 — Compute type

| Choice | Effect |
|---|---|
| `Vast GGUF   — rent a GPU, run your own llama.cpp instance` | → S2V, `is_vllm=False` |
| `vLLM        — tensor-parallel on multi-GPU cluster (DSv4 Pro, etc.)` | → S2V, `is_vllm=True` |
| `Local       — run llama-server on your own hardware` | → delegates to S3a, **returns immediately after** |
| `Together AI — managed inference, pay per token` | → S2T, terminal |
| `← Back` | return |

---

### S2T — Together AI activation

**Guard:** `provider_cfg["together"]["api_key"]` or `$TOGETHER_API_KEY`; if absent → red error +
pointer to `Providers → Configure Together AI` + `press_enter` + return.

**Decision 1 — Model.** A hardcoded list of 5 "popular" models with hardcoded per-1M prices:

| Model | Price shown |
|---|---|
| `meta-llama/Llama-3.1-8B-Instruct-Turbo` | $0.18/M tok |
| `Qwen/Qwen2.5-Coder-32B-Instruct-Turbo` | $0.44/M tok |
| `meta-llama/Llama-3.3-70B-Instruct-Turbo` | $0.88/M tok |
| `Qwen/Qwen2.5-72B-Instruct-Turbo` | $0.88/M tok |
| `meta-llama/Llama-3.1-405B-Instruct-Turbo` | $3.50/M tok |

plus `[custom] enter model ID manually`, plus `[pinned] <model_id>` **prepended** if
`.pinned_provider` exists, plus `← Back`.

**Decision 2 (conditional)** — free-text model ID if `[custom]`.

**Backend:** `activate_together_endpoint(provider_cfg, model_id)` →
`GET {base_url}/models` (validation) → `POST {base_url}/chat/completions` with
`{"messages":[{"role":"user","content":"Say hello in 5 words"}],"max_tokens":20}` →
`log_completion("together", …)`.
If the completion fails, an extra **confirm "Activate anyway?" (default No)** appears.

**State mutated:** writes `.active_endpoint`; appends one line to `usage.log`.

**Display on success:** `✓ Together endpoint activated!` + Model / Endpoint / Config path.

**GUI notes:** the price table is stale hardcoded data — the GUI should pull pricing from the recipe
(`price_input`/`price_output`, which the recipe editor already collects) or from the provider API,
and should surface the *whole* catalog with search, not five entries. The "pinned" concept only
exists because there is no way to hand a model from the browser screen to the launch screen; with a
GUI, the browser row itself gets an **Activate** button and `.pinned_provider` disappears.

---

### S2V — Vast GGUF / vLLM wizard

Eight sequential modal questions, each of which can `← Back` — and **Back means abort the whole
wizard back to the main menu**, not "go to previous step". There is no way to revise step 2 after
answering step 5.

| # | Question | Choices / default | Skipped when | Notes |
|---|---|---|---|---|
| 1 | `GPU tier:` | `tier["label"]` for tiers filtered by `image_type == "vllm"` (vLLM) or `!= "vllm"` (GGUF) | — | Error + return if the filtered set is empty. (`gpu_choices` was the site of documented BUG FIX C1.) |
| 2 | `Model recipe:` | recipes where `r["gpu"] == gpu_key`, further filtered by `provider == "vllm"` / `!= "vllm"`; prefixed with `[pinned] <QUANT> from HF browser` when `.hf_pin` exists | — | Error + return if no recipes for that tier |
| 3 | `Inference mode:` | `MODES` keys: `thinking (temp 1.0, top-p 0.95, presence 1.5)`, `coding (0.6/0.95/0.0)`, `nonthinking (0.7/0.80, thinking OFF)` | vLLM | default `thinking` |
| 4 | `Geographic preference:` | `GEOS` keys: `EU Nordic (SE/NO/FI/DK/IS)`, `EU Broad (+DE/NL/FR/UK/…)`, `US`, `Any` | never | asked for vLLM too |
| 5 | `KV cache type (recipe default: <x>):` | `q8_0 (half KV VRAM, good quality — default)`, `q4_0 (quarter KV VRAM, tight-fit)`, `bf16 (full precision, most VRAM)`; pre-selected from recipe | vLLM | |
| 6 | `Vision support (mmproj, adds ~2 GB VRAM):` | `No (text-only, recommended)` / `Yes — enable mmproj F16` | vLLM, **or** recipe already sets `mmproj` (then it just prints `Vision: mmproj=… (from recipe)`) | |
| 7 | `Max price $/hr ceiling (default <tier.max_price or 0.55>):` | free text, pre-filled | never | no numeric validation |
| 8 | `Offer selection:` | `Auto — cheapest matching offer` / `Browse — pick from live offer list` | never | `Browse` → S2O |

**Pinned-quant caveat:** choosing `[pinned] …` sets `chosen_recipe = gpu_recipes[0]` — **the first
recipe for that tier, arbitrarily** — and overrides only `MODEL_REPO`/`MODEL_QUANT`. Every other
field (ctx, parallel, image_type, llama_cpp_ref…) silently comes from a recipe the user did not pick.

#### S2O — Offer browser (`vast_ops.browse_offers`)

**Backend:**
```
vastai search offers "<gpu_filter> num_gpus=<n> reliability>0.97 inet_down>300 \
  dph_total<<max_price> disk_space><min_disk> cuda_vers>=<min_cuda> rentable=true" \
  --order dph_total --raw
```
`gpu_filter` = `gpu_name=<X>` (1 vast name) / `gpu_name in [A,B]` (many) /
`gpu_name in [RTX_PRO_6000_WS,RTX_PRO_6000_S]` (`6000pro` special-case) / `gpu_name=RTX_<key>`.
Output is tolerated with a non-JSON prefix (finds first `[`). Then a client-side geo regex filter
(`EU_NORDIC` → `SE|NO|FI|DK|IS`, `EU` → 21-country alternation, `US`, `ANY` → `.*`), matched against
the **tail** of `geolocation` (`, XX$`). If nothing matches, falls back to showing all offers with a
yellow warning.

**Display:** table `Available <gpu> offers`, **top 12 only**, columns
`ID | $/hr | rel | VRAM | ↓ Mbps | CUDA | geo`. Bandwidth colour-coded green ≥2000 / yellow ≥500 /
red below. CUDA ≥ 13.0 rendered yellow with a `⚠` (Unsloth quality caveat).

**Decision:** `[auto] cheapest matching` | one of ≤12 offer rows | `← Back` (→ aborts whole wizard).

**Returns:** offer id string, `""` for auto, `None` for cancel.

**GUI notes:** a live, sortable, filterable offer table with more than 12 rows, price/reliability/
bandwidth columns sortable, and the geo filter as a chip instead of a wizard step answered three
screens earlier. Re-searching should not mean restarting the wizard.

#### Summary panel (Launch config)

A 2-column table before the final confirm:

`GPU · Recipe · HF repo · Quant · Ctx · Parallel · Mode · KV type · GEO · Vision · Max $/hr ·
Num GPUs · Min CUDA · Offer ID (or "auto-select") · Image type (+ cold-start estimate) · Image`
— plus conditionals: `llama.cpp` (`repo @ ref`, yellow) when the recipe pins a fork;
`Serving: vLLM (tensor parallel)`, `Model ID`, `KV dtype`, `Reasoning` for vLLM recipes;
`HOST: 127.0.0.1 (tunnel-only)`; and `est. cost` from `estimate_cost(ctx, 1000, provider_cfg)`
rendered as `$X / $Y` across providers.

Cold-start estimate: `prebuilt` → `~2 min (image pull only)`; `builder` → `~12-18 min (pull + SM compile)`.

**Final decision:** `confirm("Proceed with launch?", default=True)`.

#### Execution

`subprocess.run(["bash", "<repo>/vast_up.sh"], cwd=ROOT, env=env)` — **fully blocking, output goes
straight to the terminal, no progress UI, no cancel.**

Environment contract handed to `vast_up.sh` (this is a hard external interface):

| Env var | Source |
|---|---|
| `GPU` | tier key |
| `MODEL_REPO` / `MODEL_QUANT` | pin override, else recipe |
| `CTX` (default 65536), `PARALLEL` (1) | recipe |
| `KV_TYPE`, `MODE`, `GEO`, `MAX_PRICE` | wizard answers |
| `DOCKER_IMAGE`, `IMAGE_TYPE` | `image_for_type(docker_cfg, image_type)` |
| `MIN_DISK_GB` (60), `NUM_GPUS` (1), `MIN_CUDA` ("12.8") | recipe → tier → default |
| `MODEL` | recipe `name` (or `"custom"`) |
| `VAST_NAMES` | space-joined `tier.vast_names` |
| `MMPROJ` | only if vision on |
| `OFFER_ID` | only if an offer was picked |
| `LLAMA_CPP_REPO`, `LLAMA_CPP_REF` | only if recipe pins a fork |
| `MODEL_ID`, `QUANTIZATION`, `KV_CACHE_DTYPE`, `ENFORCE_EAGER`, `REASONING_PARSER` | vLLM recipe fields |

**On rc == 0:** prints `Instance created!` + next-step hints (`Watch`, then `Tunnel → up`, then
`Smoke`) and **deletes `.hf_pin`**. On non-zero: `vast_up.sh exited <rc>`.
`vast_up.sh` itself writes `.last_instance`.

**GUI notes:**
- This is a *linear 8-step modal wizard for an irreversible paid action* where you cannot go back one
  step and cannot see the summary until the end. The GUI wants a **single-page launch form** with all
  fields visible at once, live-validated, live cost estimate, and the offer table embedded as a
  panel that re-queries on filter change.
- The `is_vllm` split is presentational only — it filters tiers and recipes. In the GUI it's a
  provider facet, not a separate wizard.
- `vast_up.sh` should be replaced by native Rust orchestration (see `00-machine-ground-truth.md`),
  but the env-var table above documents exactly what the launch semantics are, and the shell script
  remains the reference for `ONSTART_CMD`, per-GPU defaults, and the wide-search fallback
  (`reliability>0.99, inet_down>500` first, then `>0.97, >300`).

---

## S3 — Local endpoints (umbrella)

**File:** `menus/local_menus.py:261-296` (`menu_local_dispatch`)

**Purpose:** manage llama.cpp on the operator's own hardware. This is the flow that matters most for
ApexRouter on Andre's laptop.

**Displays (per loop iteration):**
- `Active: <name> (running|stopped)  port <port>` if `.active_endpoint.provider == "local"`, else
  `No local endpoint active.`
- Recipe count embedded in the *Launch* label — computed by calling `load_config()` **inline in the
  choices list**, i.e. re-parsing `recipes.toml` from disk on every render.
- `list_local_instances()` is called and `running_count` computed — **and then never displayed.**
  Dead code that still costs a directory scan + N PID probes.

**Decision:** `Launch — start a local instance (<N> recipes)` | `Status — view / manage running
instances` | `Configure — scan hardware, set options` | `← Back`. Loops until Back.

---

### S3a — Local launch wizard

**File:** `local_menus.py:166-256` (`menu_local_launch`). Also reachable from S2 → `Local`.

**Guards (each is a dead end with `press_enter` + return):**
1. No recipes with `provider == "local"` → "Add recipes with provider=local to recipes.toml".
2. `discover_local()["binaries"]` empty → "No llama-server binary found… Run 'Local → Configure'".

**Display — recipe table:** `Name | Label | Model (basename, 35 ch) | Port | Status` where Status is
`running`/`stopped` per `is_local_running(name)`.

**Decision 1 — `Recipe to launch:`** — list of recipe **labels** (+ `← Back`).
> Selection is resolved by `labels.index(sel)`; two recipes with the same label collide silently.

**Guard 3:** if that recipe is already running → yellow "use Local → Status to manage" + return.

**Display — Launch config panel:** `name · model · ctx (32768) · parallel (1) · kv type (q8_0) ·
mode (thinking) · port (8100) · backend (auto)` + `desc` if present.

**Decision 2 — `confirm("Start this local instance?", default=True)`.**

**Backend — `local_endpoint.start_local_instance(recipe)`:**

1. Stale-PID cleanup, refuse if already alive.
2. Binary resolution: `recipe.binary`, else `discover_local()` with a backend-name path heuristic
   (`"vulkan" in backend and "vulkan" in path` → prefer; rocm/hip → prefer or first), else first found.
3. Arg construction (`_get_local_server_args`), mirroring `launch.sh`:
   `--model <expanded> --host <127.0.0.1> --port <p> --ctx-size <ctx> --parallel <n>
   --cache-type-k <kv> --cache-type-v <kv> --n-gpu-layers <999> --jinja --metrics --flash-attn on`
   \+ the sampling preset for `mode` + `--mmproj <path>` if the file exists.
   Sampling presets (`config.SAMPLING_PRESETS`):
   | mode | flags |
   |---|---|
   | `thinking` | `--temp 1.0 --top-p 0.95 --min-p 0.0 --presence-penalty 1.5` |
   | `coding` | `--temp 0.6 --top-p 0.95 --min-p 0.0 --presence-penalty 0.0` |
   | `nonthinking` | `--temp 0.7 --top-p 0.80 --min-p 0.0 --presence-penalty 1.5 --chat-template-kwargs {"enable_thinking":false}` |
4. Env: `GGML_VK_VISIBLE_DEVICES=0` (vulkan) or `HIP_VISIBLE_DEVICES=0` (rocm/hip).
5. `Popen` with stdout+stderr → `local_logs/<name>.log`, cwd = repo root.
6. Writes `<name>.pid`, `<name>.json` (`status: "starting"`), and `.active_endpoint`.
7. **Health loop: up to 60 × (sleep 1 s + `GET http://host:port/v1/models`).** On 200 → rewrite meta
   with `status: "running"`, print `✓ Local endpoint ready!` + Instance/Model/Endpoint/PID. On child
   exit → return the last 500 bytes of the log. On timeout → `Health check timed out after 60s`.

**The entire 60-second health loop blocks the TUI with a single `[dim]Starting llama-server (PID …)`
line and no progress indication.**

**GUI notes:** this is the flow that most needs redesign — a launch should immediately produce a
**live instance card** (state: starting → loading → ready/failed) with a **streaming log pane**, a
cancel button, and a progress signal derived from llama.cpp's own load output. Also: the wizard
exposes zero of the fields it launches with; the operator can't override ctx/port/backend at launch
time without editing the recipe first. The GUI should make the launch panel an editable, pre-filled
form (recipe = preset, not a lock).

Also see `00-machine-ground-truth.md`: `--jinja` is now default-on, `--flash-attn` takes
`on|off|auto`, `--metrics` is off by default, and `-np` defaults to `-1`. **Feature-detect before
emitting flags.**

---

### S3b — Local status

**File:** `local_menus.py:88-161` (`menu_local_status`). **Not a loop** — one action, then back.

**Guard:** no instances → "No local instances configured. Run 'Local → Launch' to start one."

**Display — table:** `Name | Status (running green / starting yellow / stopped red) | Port |
Model (basename, 35 ch) | Started (last 8 chars of the ISO timestamp — i.e. `HH:MM:SSZ` truncated)`.

**Decision 1 — `Select instance:`** (names + Back).
**Decision 2 — `Action for '<name>':`** — `View logs`, plus `Stop instance` **only when running**, plus Back.

- `View logs` → reads `local_logs/<name>.log`, prints **last 2000 characters**. Static; no tail, no
  follow, no filter. If missing: "No log file found."
- `Stop instance` → `stop_local_instance(name)`: SIGTERM → poll 10 × 0.5 s → SIGKILL → unlink PID →
  set meta `status: "stopped"` + `stopped_at` → **unlink `.active_endpoint` if it pointed at this
  instance**.

Then `press_enter()` and **return all the way to S3** — to stop a second instance you re-enter Status,
re-pick, re-pick.

**GUI notes:** one table with per-row action buttons (Stop / Logs / Restart / Make active), a
follow-mode log pane, and no re-entry. Also missing entirely today: **delete a stopped instance's
metadata**, **restart**, **set active without restarting**.

---

### S3c — Local configure

**File:** `local_menus.py:25-83` (`menu_local_config`). Loops.

**Display — discovery panel** (`discover_local()` on every iteration):

| Row | Content |
|---|---|
| `binaries` | `<N> found` + first 3 absolute paths, or red `none found` |
| `backends` | comma list, first one green, or yellow `cpu (fallback)` |
| `models` | `<N> found` + top 3 as `name (size_mb MB)`, or red `none found` |

`discover_local()` scans `~/llama.cpp`, `~/Projects/llama.cpp`, `/usr/local/bin`, and every `$PATH`
entry for `build/bin/llama-server`, `build-vulkan/bin/llama-server`, `build-rocm/bin/llama-server`,
`./llama-server`; probes backends by grepping `<bin> --help` for vulkan/cuda/hip/rocm; then
`rglob("*.gguf")` under `~/models` and `~/.cache/huggingface/hub`, skipping paths containing
`mmproj`/`vocab`, sorted by size descending.

**Decision:** `Refresh scan` | `Set models dir  — custom directory to scan` | `← Back`.

- `Refresh scan` → calls `discover_local()` again into a local variable that is then discarded (the
  loop re-scans anyway). Prints `Scan complete.`
- `Set models dir` → text prompt defaulting to `~/models`, then **prints instructions telling the
  user to hand-edit `recipes.toml`**. It persists nothing. A pure dead end.

**GUI notes:** discovery is genuinely useful and should be a first-class **Hardware** panel
(binaries with detected backend + version, GPU devices from Vulkan enumeration, models with sizes and
a "create recipe from this model" action). `models_dir` must actually be a persisted setting. The
model list should be a browsable table, not "top 3". Backend detection by grepping `--help` is
unreliable — enumerate devices instead (`--list-devices` / Vulkan enumeration), and exclude
`llvmpipe`.

---

## S4 — Providers

**File:** `menus/provider_menus.py:29-57` (`menu_providers`). Loops.

**Display:** table over `sorted(provider_cfg.keys())` — `provider (label from DEFAULT_PROVIDERS) |
status (`✓ set` / red `not configured`) | base_url (50 chars)`.

**Decision:** `Configure Together AI` | `← Back`.
> The table is generic over N providers but the action list is hardcoded to one. Together is the only
> provider `DEFAULT_PROVIDERS` knows about.

### S4a — Configure Together AI (`_configure_together`, 5 steps)

| # | Widget | Detail |
|---|---|---|
| 0 | display | `Current: ****<last 4>` or `(none)` |
| 1 | `password` | `Together AI API key (leave empty to keep current):` — empty keeps current; empty + no current → yellow warning |
| 2 | `text` | `Base URL (default: https://api.together.ai/v1):` pre-filled with current |
| 3 | auto | `test_together_connection()` → `GET {base}/models`; message is `OK — <N> models available. Examples: a, b, c…` or a mapped error (401 → "Authentication failed — check your API key", 429 → "Rate limited", other → `HTTP <code>: <reason>`) |
| 4 | `confirm` (only if step 3 OK) | `Run a quick completion test?` default Yes → then `text` `Model ID (default: meta-llama/Llama-3.1-8B-Instruct-Turbo)` → inline `POST /chat/completions` ("Say hello in 5 words", max_tokens 20) → prints `OK — '<content>' (<n> tokens)` or the error → `log_completion("together", …)` |
| 5 | auto | `save_provider_config()` → rewrites `~/.vastai-gguf/config.toml`, prints `✓ Configuration saved` |

**State mutated:** `provider_cfg` in memory (mutated in place, so the whole running app sees it),
`~/.vastai-gguf/config.toml`, `usage.log`.

> `save_provider_config` is a **hand-rolled TOML writer** (string concatenation, no escaping). Any
> key containing `"` would corrupt the file. The port must use a real TOML serializer.
> Note the asymmetry: reads use `[providers.<name>]`, and the config file is the *only* place keys
> live besides `$TOGETHER_API_KEY` (env is used only when the file has no key).

**GUI notes:** a provider settings page with a masked field, a "Test" button that shows the result
inline without advancing, and a provider list that is data-driven (add OpenAI-compatible providers
without code changes). Secrets must not be echoed into a config file in plaintext by default — flag
for the port.

---

## S5 — Together model browser

**File:** `provider_menus.py:155-284` (`menu_together_models`). Single pass, no loop.

**Guard:** no API key → error + pointer to Providers.

**Backend:** `GET {base_url}/models` (15 s), accepts `{"data":[…]}` or a bare list; entries need an
`id`.

**Displays, in order:**
1. `Fetching model catalog from <base_url>…`
2. **Model Families** table — `Family | Models (count)`, family = the part before the first `/`.
3. `Total: <N> models available`
4. After family selection: **Models (<n>)** table — `Model ID | Short name`, **first 50 only**, with
   `… and <N-50> more` printed *before* the table.

**Decisions:**
1. `Browse family:` — sorted family names + `[all families]` + Back.
2. `Action:` — `Pin a model for the next launch wizard` | `← Back to models` (which actually just
   returns to the main menu).
3. If Pin: `autocomplete("Select or type model ID:")` over the shown ids.

**State mutated:** writes `~/.vastai-gguf/.pinned_provider` =
`{"provider":"together","model_id":…,"base_url":…}`. Prints `Pinned: provider=together model=…` +
"Next Launch wizard will offer to use this."

**GUI notes:** one searchable/filterable table with columns for family, id, and (if the API gives it)
context length and pricing; row actions **Activate now** and **Save as recipe**. The 50-row cap and
the two-step family drill-down are terminal-space artefacts. Pinning is a workaround for the absence
of cross-screen state — delete the concept.

---

## S6 — Batch compare

**File:** `menus/tool_menus.py:54-213` (`menu_batch_compare`). Single pass.

**Availability probe (before any question):**
- Together: available if an api key resolves; contributes **three hardcoded model rows**
  (`Llama-3.1-8B-Instruct-Turbo`, `Qwen2.5-Coder-32B-Instruct-Turbo`, `Llama-3.3-70B-Instruct-Turbo`).
- Vast: available if `tunnel_running()` **and** `curl /health` contains `"ok"`; model name resolved
  via `curl /v1/models | jq -r '.data[0].id // "loading"'`.
- **Local endpoints are not offered at all.** A local llama-server cannot be benchmarked here.
- None available → red error + "Configure Together AI or start a Vast instance first."

**Display:** table `Provider | Model` of the available combinations.

**Decisions:**
1. `checkbox("Select providers to compare:")` — multi-select over `"<label> / <model>"` strings.
2. `text(multiline=True)` — `Prompt to send to all selected providers:`.

**Execution:** **sequential**, one `urllib` POST per selection, `max_tokens: 200`, 30 s timeout each.
Together goes to `{base_url}/chat/completions` with Bearer auth; Vast goes to
`http://127.0.0.1:8800/chat/completions` (note: **no `/v1` prefix** — works only because llama-server
also serves the unprefixed route).

**Results display:** for each provider, sequentially:
```
Provider N: <label>
  Model:   <id>
  Latency: <s>s | Tokens: <completion_tokens>
  <first 200 chars of content>...
```
Not actually side by side despite the name — a vertical list, truncated at 200 chars.

**State mutated:** one `usage.log` line per result, where `prompt_tokens` is **estimated as
`len(prompt.split()) * 1.3`** rather than taken from the API's `usage.prompt_tokens` (which was
fetched and discarded).

**GUI notes:** parallel requests, streaming into side-by-side columns, full (scrollable) responses,
real token counts, per-provider cost, TTFT and tok/s, and the ability to include **local** and
**vllm** endpoints. This screen is the seed of a proper eval/compare view.

---

## S7 — Watch boot

**File:** `vast_menus.py:498-544` (`menu_watch_boot`).

**Guard:** no `.last_instance` → yellow message + return.

**Loop (10 s period, exit only via Ctrl-C or a terminal condition):**
1. `get_instance_json(inst_id)` — on failure prints `vastai API unreachable, retrying...`, sleeps.
2. Prints `  HH:MM:SS  status=<colored>  <status_msg>`.
3. If `ssh_host`/`ssh_port` present: `ssh -p <port> -o StrictHostKeyChecking=no -o ConnectTimeout=5
   root@<host> 'tail -1 /var/log/launch.log'`; prints `log » <line>` **only when it changed**.
4. If `status == "running"` and the tunnel is up: `curl /health`; on `"ok"` prints
   `✓ Endpoint healthy!  http://127.0.0.1:8800/v1` and breaks.
5. If status in `("exited","offline")` → red message, break.

Then `press_enter()`.

**GUI notes:** this is a **progress view**, and in a GUI it should be a non-modal panel that starts
automatically the moment a launch begins — status timeline, full log stream (not `tail -1`),
health probe indicator, elapsed timer, and a Stop/Destroy button. The operator should never have to
navigate to a "Watch" menu item; the launch itself should open it.

---

## S8 — Diagnose

**File:** `tool_menus.py:300-476` (`menu_diagnose`).

**Guard:** no `.last_instance` and no active endpoint → yellow + return.

**Section 1 — Usage Summary (24h)** *(the title says 24h; `get_session_costs()` actually aggregates
the entire `usage.log`)*. Built by string-splitting `format_usage_summary()` on `:` into a table:
`Total sessions: <N> completions`, `Grand total: $X.XXXX`, then per provider
`<label>  $X.XXXX  (<tokens> tokens)`.

**Section 2 — Together Rate Status** (only if a Together key + base_url exist):
`format_rate_limits()` → probes `GET {base}/models` and reads `X-RateLimit-Limit` /
`-Remaining` / `-Reset` headers → `Limit: N/period | Remaining: N | Resets: HH:MM:SS`, or
`Rate limits: Not available`.

**Branch (documented BUG FIX M2):** if the active provider is `local` or `together`, print
`Active provider: <p> — SSH/Vast diagnostics skipped.` and return. **Local endpoints therefore have
no diagnostics at all** — no llama.cpp slot info, no memory usage, no tok/s.

**Then an unconditional `press_enter()` *in the middle of the flow*** (`tool_menus.py:354`) before
Vast diagnostics start. Pure friction.

**Section 3 — instance panel:** `status · status_msg · gpu + geo · $/hr · inet_down (rated Mbps) ·
disk (util% of GB) · ssh`. Early-returns if status ≠ running, or if there's no ssh host yet
("still provisioning").

**Section 4 — four sequential SSH probes** (`ps -eo pid,etime,pcpu,pmem,cmd --sort=-pcpu | head -15`;
`df -h /workspace || df -h /`; `find /workspace/models -type f \( -name '*.gguf' -o -name
'*.incomplete' \) -exec ls -lh {} \;| head -20`; `tail -30 /var/log/launch.log`), each 10–15 s
timeout.

**Section 5 — live download speed:** `_net_rx_delta(inst_id, seconds=4)` — an SSH round trip that
reads `/proc/net/dev` eth0, sleeps 4 s remotely, reads again. **~4 s of hard blocking.**

**Rendering:** `Processes (top CPU)`, `Disk /workspace`, `Model files` (each line classified
`⟳ downloading <name> (<size> so far)` for `.incomplete` or `✓ complete <name> (<size>)`),
`Network download speed` (`⚠ STALLED` if <1000 bytes/4 s, `slow` if <50 Mbps, `✓ active` otherwise),
`launch.log (last 30 lines)`.

**Section 6 — stall recovery (conditional):** if stalled, a yellow panel explains the HF-transfer
hang, then `confirm("Kill stalled download and restart launch.sh?", default=True)` →
`vast_ops._restart_launch()`: read the container env off `/proc/<pid>/environ` of
`bash /app/launch.sh` (grep `MODEL_|CTX|KV_TYPE|MODE|PARALLEL|MMPROJ|HF_TOKEN|HOST`), fall back to
hardcoded Qwen3.6-35B defaults if unreadable, force `HOST=127.0.0.1`, write `/tmp/restart_launch.sh`
(pkill launch.sh + pkill `hf download` + sleep 2 + re-exec with the same env), `ssh -f` it, then
`sleep 3` and shell out to `tools/vast_tunnel.sh logs`.

**Section 7 — endpoint slots** (if tunnel up): `curl /health`, then `curl /slots` → per slot
`slot <i>: <state>  ctx used <n_past>/<n_ctx> tokens`.

**GUI notes:** split into (a) a persistent **Usage/Cost** view fed by `usage.log` with real time
windows and charts, (b) a per-instance **Health** panel that polls in the background, (c) a
**Remote shell/log** pane. The stall detector is a genuinely good idea — keep it, but as a passive
alert on the instance card with a one-click Restart, not something you discover by running a 30-second
blocking diagnostic.

---

## S9 — Instances

**File:** `vast_menus.py:451-493` (`menu_instances`). Single pass.

**Backend:** `vastai show instances --raw` (15 s). Empty/`[]`/`null` → "No instances found."

**Display:** table `Active Instances` — `ID | Status (running green / loading yellow / else red) |
GPU | $/hr | geo`.

**Decision:** `Set .last_instance to:` — one row per instance (`<id>  (<status>)  <gpu>  <geo>`) +
`← Skip`.

**State mutated:** writes the chosen id to `.last_instance`; prints `Set .last_instance → <id>`.

**Missing:** you cannot destroy, stop, start, tunnel-to, or inspect an instance from this screen. It
is a radio button over a table. Total hourly burn is not shown even though `$/hr` is right there.

**GUI notes:** the fleet table should carry per-row actions (Attach / Tunnel / Watch / Diagnose /
Destroy), a running-cost total, and an "active" indicator instead of an invisible `.last_instance`
file.

---

## S10 — HF Browse

**File:** `hf_browser.py:45-141` (`menu_hf_browser`). Single pass.

**Decision 1 — `HF repo to browse:`** — deduplicated `model_repo` values across all recipes, rendered
as `<repo>  (used in recipe: <label>)`, plus `[custom] enter repo ID manually`, plus Back.
**Decision 1b (conditional)** — free text repo id.

**Backend:** `GET https://huggingface.co/api/models/<repo>?blobs=true`, 10 s, `User-Agent:
LocalRouter/1.0`, optional `Authorization: Bearer <token>` read from
`~/.cache/huggingface/token`. Reads the `siblings` array.

**Filtering/derivation:** keeps `*.gguf`; extracts a quant tag with
`(UD-Q\d+[^.\s_-]*|Q\d+_K_[A-Z]+|Q\d+_\d+)` (case-insensitive), `?` when no match; sizes via
`_fmt_bytes`.

**Display:** table titled with the repo — `filename | size | quant`, then
`<N> .gguf file(s) shown — sizes are per-shard`.

**Decision 2 — `Action:`** — `Pin a quant for the next launch wizard` | `← Back to main menu`.
**Decision 3 — `Select file to pin as MODEL_QUANT:`** — one entry per file + Back.

**State mutated:** writes `.hf_pin` = `{MODEL_REPO, MODEL_QUANT, filename, size}`.

**Missing:** no search over HF (you can only browse repos already referenced by a recipe, or type an
exact id), no multi-shard grouping (each shard is a separate row, hence the "sizes are per-shard"
disclaimer), no download, no "create a recipe from this file", no unpin.

**GUI notes:** HF search + repo view + shard-aware file grouping with a **total** size, and a direct
**"Create recipe"** / **"Download to ~/models"** action. Sizes matter enormously on a 22 GiB box —
show total download size vs free disk and vs VRAM.

---

## S11 — Configuration Editor

**File:** `menus/editor_menus.py`. Entered from Main → `Editor`; on return, `main()` re-runs
`load_config()`.

### S11 top level (`menu_editor`, loops)

**Display:** `recipes: <N>  |  GPU tiers: <N>  |  images: <N>  |  <"modified — unsaved" | "no changes">`

**Decisions:** `Recipes — browse / create / edit / delete (<N> recipes)` | `GPU Tiers — manage GPU
configurations (<N> tiers)` | `Docker — container images (<N> images)` | `Save — write changes to
recipes.toml` (with a yellow `●` when dirty) | `Reload — discard changes, re-read from disk` |
`← Back`.

- Back with unsaved changes → `confirm("Unsaved changes. Save before leaving?", default=True)`.
- Save → `save_recipes(data)` → `recipe_editor._write_toml`: **copies the current file to
  `recipes.toml.bak`, then `tomli_w.dump`s the whole in-memory dict.**
  **All comments, formatting, and key ordering in `recipes.toml` are destroyed on the first save.**
  Requires `tomli_w`; raises `RuntimeError` if missing.
- Reload with unsaved changes → `confirm("Discard unsaved changes?", default=False)`.

Dirty tracking is a single boolean OR-ed up from the submenus' return values, which are themselves
booleans. Nothing is per-entity; there is no undo, no diff preview, no "what changed".

---

### S11a — Recipe editor (`menu_edit_recipes`, loops)

**Display:** `<N> recipes across <M> providers: local (n), together (n), vast_gguf (n), vllm (n)`.

**Decisions:** `Browse — view all <N> recipes` | `Browse by provider — filter by provider type` |
`Browse by GPU tier — filter by GPU tier` | `Create — new recipe wizard` | `Edit — modify an existing
recipe` | `Duplicate — clone + rename a recipe` | `Delete — remove a recipe` | `← Back`.

**Recipe table** (`_recipe_table`): `# | Name | Label | Provider (colour-coded: local green,
together cyan, vast_gguf purple) | GPU (tier label, 14 ch) | Ctx (as `<n>K`)`.

| Action | Sub-decisions | Backend / state |
|---|---|---|
| Browse | — | prints the table, `press_enter` |
| Browse by provider | `Provider:` from the observed set | filtered table |
| Browse by GPU tier | `GPU tier:` from `gpu_tiers` keys | filtered table |
| Create | → S11a1 wizard | `validate_recipe()`; if errors, lists them and asks `confirm("Add anyway?", default=False)`; duplicate-name check ("Recipe 'x' already exists."); `add_recipe()`; `modified = True` |
| Edit | `Recipe to edit:` (names) → S11a2 field editor | on save: `recipe.clear(); recipe.update(result)` **in place** |
| Duplicate | `Recipe to duplicate:` (names) → `text("New name:", default="<name>-copy")` | `duplicate_recipe()` (deepcopy + new name, appended) |
| Delete | `Recipe to delete:` (names) → `confirm("Delete 'x'?", default=False)` | `remove_recipe()` |

> Edit/Duplicate/Delete pick by **name**, Browse shows **label** — two different identifiers for the
> same object in adjacent screens.
> Delete does **not** warn when the recipe is the active endpoint or currently running.

#### S11a1 — Create-recipe wizard (`_create_recipe_wizard`)

**Decision 0 — `Provider type:`** — `vast_gguf — GGUF on rented Vast.ai GPU (llama.cpp)` |
`vllm — vLLM tensor-parallel on multi-GPU cluster` | `local — llama.cpp on your own hardware` |
`together — Together AI managed endpoint` | Back.

| Provider | Prompts, in order |
|---|---|
| **local** | name (slug) · label · `Model path (e.g. ~/models/model.gguf)` · Port (8100) · Context length (32768) · Backend select `vulkan/rocm/cuda/cpu` · Sampling mode select `thinking/coding/nonthinking` |
| **together** | name · label · `Together model ID (e.g. Qwen/Qwen3-32B)` · ctx (131072) · `Price per 1M tokens (input)` (0.50) · `Price per 1M tokens (output)` (defaults to the input value) |
| **vllm** | name · label · **GPU tier picker** · `HF model ID (e.g. deepseek-ai/DeepSeek-V4-Pro)` · ctx (393216) · [`image_type = "vllm"` forced] · `KV cache dtype` select `auto/fp8/fp8_e5m2/fp8_e4m3` (auto → field omitted) · confirm `Enforce eager (disable CUDA graphs, saves VRAM)?` (No) → `enforce_eager = "true"` · confirm `Enable reasoning parser (deepseek_r1)?` (Yes) → `reasoning_parser = "deepseek_r1"` |
| **vast_gguf** | name · label · **GPU tier picker** · `HF model repo (e.g. unsloth/Qwen3.6-27B-GGUF)` · `Quant tag (substring match, e.g. Q6_K, UD-Q6_K_XL)` · ctx (98304) · `Parallel slots` (1; only stored if ≠ 1) · `KV cache type` select `q8_0/q4_0/bf16` (only stored if ≠ q8_0) · confirm `Override image type from tier default?` → select `prebuilt/builder` · confirm `Custom llama.cpp fork/branch? (for unmerged model support, e.g. DSv4)` → forces `image_type = "builder"`, then `GitHub repo (user/repo)` (default `fairydreaming/llama.cpp`) and `Branch/tag/commit` (default `deepseek-dsa`) |
| all | `Description (optional):` |

GPU tier picker (`_pick_gpu_tier`) renders `<key padded to 16> — <n×><vram>GB  <label>` and parses the
key back out with `sel.split()[0]`.

Numeric inputs go through `_safe_int` / `_safe_float`, which print
`Invalid number for <label>: '<v>' — using default <d>` and **silently substitute the default** rather
than re-prompting.

Final guard: `if not recipe.get("name")` → `Recipe name is required.` → discard the whole wizard.

#### S11a2 — Field editor (`_edit_recipe_fields`)

Works on a `copy.deepcopy` — has real Cancel semantics (unlike the tier editor).

**Display:** panel titled with the recipe name; every key/value as a row.

**Decision:** a select over `"<key padded to 16> = <value, 50 chars>"` for every existing key, plus
`+ Add field`, `✓ Done — save`, `✗ Cancel`.

- `+ Add field` → `Field name:` then `Value for '<key>':`, coerced by `_coerce_value`.
- `provider` field → routes to `_pick_provider()`.
- `gpu` field → routes to `_pick_gpu_tier()`.
- `name` field → prints a tip ("changing name creates a new identity. Use Duplicate instead.") then
  edits anyway.
- Any other field → `text(default=<current>)`. **Entering an empty string** on a field other than
  `name`/`label` triggers `confirm("Remove field '<x>'?", default=False)` → deletes the key.

`_coerce_value` heuristics: `isdigit()` → int; else `float()` → float; else `["a","b"]`-ish → list of
quoted strings; else the string. **This silently turns `min_cuda = "12.8"` into a float `12.8`, and a
model or quant named e.g. `"7"` into an int.** The port needs a typed schema per field.

---

### S11b — GPU tier editor (`menu_edit_gpu_tiers`, loops)

**Display — tier table:** `Key | Label | VRAM (`<n>GB` or `<k>×<n>GB`) | GPUs | Max $/hr | Image type |
Vast names (comma-joined)`.

**Decisions:** `Create — add a new GPU tier` | `Edit — modify an existing tier` | `Delete — remove a
tier` | `← Back`.

**Create wizard (`_create_tier_wizard`), in order:**
`Tier key (e.g. h100-sxm-2x)` · `Display label` (defaults to the key) ·
`Vast.ai GPU names (comma-separated, e.g. H100_SXM,H100_SXM5)` · `Max $/hr` (3.50, kept as a
**string**) · `VRAM per GPU (GB)` (80) · `Number of GPUs` (1) · `Min disk (GB)` (100) ·
`Default image type` select `prebuilt/builder` · `Min CUDA version` (12.8, kept as a string).
Then `validate_gpu_tier()` (requires `vast_names`, `label`, `max_price`; `vast_names` must be a list)
→ error list + `confirm("Add anyway?", default=False)` → `add_gpu_tier()`.

**Edit:** an inner loop over `"<key> = <value 40 ch>"` + `✓ Done` + `← Back`. `vast_names` gets a
comma-separated text prompt; everything else gets `text(default=…)` + `_coerce_value`.
**Edits are applied directly to the live dict — there is no Cancel**, and `✓ Done` and `← Back` do the
same thing. This is inconsistent with the recipe field editor.

**Delete:** computes an `in_use` set from recipes and builds annotated `labels` with
`(in use)` markers — **and then passes the unannotated `keys` to the select, so the annotation is
never displayed** (`editor_menus.py:576-585`). After selection it does print
`Warning: N recipe(s) use this tier.` before `confirm("Delete tier 'x'?", default=False)`.

---

### S11c — Docker image editor (`menu_edit_docker`, loops)

**Display:** table `Container Images` — `Key | Image`.
**Decisions:** `Add / Update  — set an image entry` | `Remove — delete an image entry` | `← Back`.

- Add/Update → `Image key (e.g. prebuilt, builder, dsv4-flash)` → `Image URI:` (default: existing
  value, else `ghcr.io/buckster123/`) → `add_docker_image()`.
- Remove → select a key → `confirm("Remove 'x'?", default=False)` → `del docker[key]`.

No check that a removed image key is still referenced by a tier's or recipe's `image_type`.

**GUI notes for the whole editor:** this should be a **table/detail split view** with a typed form per
provider kind, inline validation as you type, a visible dirty indicator per row, a diff preview before
save, and comment-preserving TOML writes (`toml_edit`) so a hand-maintained `recipes.toml` survives
a round trip. Recipe → tier → image are a small relational model; render the referential integrity
(which recipes use this tier / this image) instead of discovering it only at delete time.

---

## S12 — Tunnel

**File:** `vast_menus.py:403-420` (`menu_tunnel`). Loops.

**Display:** `Tunnel: running|down  (local :8800 → container :8000)`.

**Decision:** `up — start tunnel` | `status — detailed info` | `down — stop tunnel` |
`logs — tail container log` | `← Back`.

**Backend:** `run(f"bash {ROOT}/tools/vast_tunnel.sh {cmd}")` where `cmd = choice.split()[0]` — output
goes straight to the terminal; then `press_enter()`. `logs` blocks until the operator interrupts it.

`vast_tunnel.sh` reads `.last_instance`, calls `vastai show instance <id> --raw`, needs `jq`, requires
`ssh_host`/`ssh_port`, writes `/tmp/vastai-gguf-tunnel.pid` and `/tmp/vastai-gguf-tunnel.ssh`, and
documents a `ControlMaster` requirement in `~/.ssh/config` (~500 ms/request without it, ~RTT with).

**GUI notes:** a tunnel is a piece of *state*, not a command menu — show it as a toggle with a live
indicator on the instance card, plus a "measure latency" action. The ControlMaster advice should be a
checkable precondition the app reports on, not a comment in a shell script.

---

## S13 — Smoke test

**File:** `tool_menus.py:481-511` (`menu_smoke`).

**Default URL resolution, in precedence order:**
1. `http://127.0.0.1:8800` if the tunnel is running,
2. overridden by the Together endpoint (`.active_endpoint.endpoint` minus `/chat/completions`) if the
   active provider is together,
3. overridden by `http://127.0.0.1:8888` if the proxy PID file points at a live process.
   *(A running local endpoint on :8100 is never the default.)*

**Decision:** one `text("Endpoint base URL (no /v1 suffix):", default=<resolved>)`. Empty → return.

**Backend:** `bash <ROOT>/smoke.sh <url>` — blocking, raw terminal output. `smoke.sh` runs, in order:
`endpoint info` (base URL + provider guess) · `models` (`GET /v1/models | jq`) · `warm-up: short
completion` (80 tokens, wrapped in `time`) · `tool calling: get_weather` (a function-calling probe
with `tool_choice: auto`, 256 tokens) · `throughput: 200-token sustained generation` (300 tokens,
prints `completion_tokens`/`prompt_tokens`/`model`) · `DONE`. It also supports
`--provider` (resolve from `.active_endpoint` + parse the api key out of `config.toml`) and
`-k/--key`, neither of which the TUI ever uses. Requires `curl` and `jq`.

**GUI notes:** a test runner panel — pick a target from the *known* endpoints (dropdown, not a typed
URL), run the checks as discrete named steps with pass/fail badges and timings, keep a history, and
surface tok/s and TTFT as first-class numbers. Nothing here needs a shell script.

---

## S14 — Proxy manager

**File:** `tool_menus.py:516-577` (`menu_proxy`). Loops.

**Display:**
- `Proxy: running|stopped` (from `/tmp/vastai-gguf-proxy.pid` + `os.kill(pid, 0)`)
- `PID: <pid>  (port 8888)` when running
- `Target: <provider> → <base_url>` when running, via `endpoint_proxy.resolve_target()`
- A static reference table: `up / down / logs / status` with descriptions — printed **every
  iteration**, immediately above the identical select.

**Decision:** `up — start proxy` | `down — stop proxy` | `logs — tail output` | `status — detailed
info` | `← Back`.

| Action | Behaviour |
|---|---|
| `up` | `_proxy_up()`: refuse if already alive; `resolve_target()`; `Popen([sys.executable, "endpoint_proxy.py"])` with stdout+stderr → `/tmp/vastai-gguf-proxy.log`; write PID; print `✓ Proxy started (PID n)` + "Waits for target to become available..." |
| `down` | `_proxy_down(pid_file)`: SIGTERM + unlink; handles `ProcessLookupError` (but not a malformed PID file) |
| `logs` | `tail_proxy_logs()`: `while True: clear + print last 2000 chars + sleep 1` — Ctrl-C to exit |
| `status` | `proxy_status_detail()`: `curl http://127.0.0.1:8800/v1/models` and `curl -H "Authorization: Bearer <key>" https://api.together.ai/v1/models`, then a table `Vast GGUF: available|unreachable` / `Together AI: available|not configured` with their URLs |

`resolve_target()` (in `endpoint_proxy.py`) is the actual routing rule and must be preserved
semantically:
- `.active_endpoint.provider == "together"` → `base_url`, `Bearer` from `$TOGETHER_API_KEY` or a
  **section-aware** scan of `[providers.together]` in `config.toml`.
- `== "local"` → `http://<host>:<port>/v1`, `Bearer <api_key>` if the endpoint records one.
- otherwise → `http://127.0.0.1:8800/v1`, no auth, provider `vast-gguf`.

**Note:** `proxy_status_detail()` reports on exactly two hardcoded backends and never mentions a local
endpoint, even when local is the active target.

**GUI notes:** the proxy is the product's core value proposition (`localhost:8888` never changes,
clients don't care what's behind it) and it deserves far more than a 4-verb menu: a **routing view**
showing target, auth mode, request/error counters, recent requests, and a one-click target switch.

---

## S15 — Destroy

**File:** `vast_menus.py:425-446` (`menu_destroy`).

**Guard:** no `.last_instance` → `No .last_instance found.` + return.

**Display:** `Will destroy: <id>`, plus `Tunnel is running — will be stopped first.` when applicable.
**No cost, no uptime, no GPU, no confirmation of *what* that instance is** — just the bare id.

**Decision:** `confirm("Destroy instance <id>? This is irreversible.", default=False)`.

**Backend:** if the tunnel is up, `bash tools/vast_tunnel.sh down`; then `bash vast_down.sh`, which
runs `echo "y" | vastai destroy instance <id>` and **`rm -f .last_instance`**.

**GUI notes:** show the full instance card (GPU, geo, $/hr, uptime, accrued cost) in the confirmation,
allow destroying **any** instance from the fleet table (not only the "last" one), and clear
`.active_endpoint` too if it pointed at that instance (the TUI does not).

---

## 3. Cumbersome-by-design: the friction inventory

Ranked by how much a GUI can win.

| # | Friction | Where | GUI fix |
|---|---|---|---|
| 1 | **Blocking status refresh** on every main-menu render (up to ~21 s of `vastai` + `curl`) | `show_status` | Background poller → persistent status region; never gate navigation on I/O |
| 2 | **`press_enter()` after every single action** (~30 call sites), including one *mid-flow* in Diagnose | everywhere | Delete the concept; results appear in place |
| 3 | **Full-screen `console.clear()`** destroys all prior output every loop | `main()` | Panels retain their content |
| 4 | **Linear modal wizards with no back-one-step**; `← Back` aborts the whole flow | S2V (8 steps), S11a1, S11b create | Single-page forms; every field revisable; live summary |
| 5 | **Re-entry after every action** — stop one local instance, get dumped to the umbrella menu | S3b, S9, S11a | Table rows with inline actions, no navigation |
| 6 | **Only one thing at a time** — can't watch a boot while browsing recipes; can't compare providers in parallel | S6, S7, S8 | Async runtime + tabs/panes; parallel batch requests |
| 7 | **60-second silent local launch** and 10 s-granularity vast boot polling with `tail -1` | S3a, S7 | Streaming logs + phase progress from the moment launch starts |
| 8 | **Cross-screen state via "pins"** (`.hf_pin`, `.pinned_provider`) because screens can't hand data to each other | S10 → S2V, S5 → S2T | Direct actions on the row ("Launch with this", "Create recipe") |
| 9 | **Truncation everywhere** — 3 models, top 12 offers, 50 models, 35-char names, 200-char responses, last 2000 chars of a log | S3c, S2O, S5, S6, S3b | Virtualised scrollable tables/panes with search |
| 10 | **Labels are the dispatch keys** (`choice.startswith("Launch")`) and identity flips between `name` and `label` between screens | main, S11a | Typed messages; stable ids; display names separate |
| 11 | **Dead ends that only print advice** — "Set models dir" tells you to edit TOML by hand | S3c | Real, persisted settings |
| 12 | **Computed-then-discarded state** — `running_count`, tier `(in use)` labels, `usage.prompt_tokens` | S3, S11b, S6 | Display what you compute |
| 13 | **Destructive ops with thin context** — Destroy shows only an id; recipe delete doesn't warn if active/running; tier delete's in-use warning is invisible until after you pick | S15, S11a, S11b | Rich confirmations with impact analysis |
| 14 | **Editor save rewrites `recipes.toml` wholesale**, losing comments and ordering | S11 | `toml_edit`-style comment-preserving writes; diff preview |
| 15 | **`_coerce_value` type guessing** turns `"12.8"` into a float and numeric-looking names into ints | S11a2, S11b | Typed field schema per provider |
| 16 | **Local endpoints are second-class** — excluded from Batch, excluded from Diagnose, never the Smoke default | S6, S8, S13 | Providers are uniform; local is the primary case on this machine |
| 17 | **Hardcoded catalogs** — 5 Together models with 2024-era prices in the launch flow, 3 in Batch, a fixed Vast hourly rate table in `cost.py` | S2T, S6, `cost.py` | Fetch from the API / read from recipes |
| 18 | **No fleet-level view** — no total $/hr, no "everything I have running right now" across local + vast + managed | — | A single Fleet/Overview surface |
| 19 | **Nesting to depth 6** for a recipe field edit | S11 | Split view, one click to any field |
| 20 | **Shell-out for everything** (`vastai`, `bash *.sh`, `curl`, `jq`, `ssh`) — output is uncapturable, unparseable, unstyled, and `jq`/`vastai` are hard runtime deps | throughout | Native HTTP/JSON in Rust; `ssh` remains, but structured |

---

## 4. Capability → port disposition (summary)

| Capability | Port? | Note |
|---|---|---|
| Live status of active endpoint | yes, redesigned | persistent async panel, uniform across providers |
| Local llama.cpp launch / stop / logs | yes | primary path; streaming logs, editable launch form |
| Local hardware & model discovery | yes, redesigned | persisted `models_dir`, recursive scan, device enumeration not `--help` grep |
| Vast.ai launch wizard | redesign | single-page form; REST not CLI; embedded offer table |
| Vast offer browsing | yes, redesigned | full sortable table, live re-query |
| vLLM recipes | yes | a provider facet, not a separate wizard |
| Together AI activation + catalog | yes, redesigned | searchable catalog, direct activate, no pins |
| Provider config (key/base-url/test) | yes | data-driven provider list, real TOML writer, secret handling |
| SSH tunnel management | yes, redesigned | state toggle on the instance card |
| Unified proxy on :8888 | yes | keep `resolve_target()` semantics; add routing/telemetry view |
| Boot watcher | redesign | auto-opens on launch, non-modal |
| Deep diagnostics + stall recovery | yes, redesigned | passive alerts; local diagnostics added |
| Usage/cost tracking (JSONL) | yes | keep the file format; add charts and real time windows |
| Batch compare | yes, redesigned | parallel, streaming, includes local, real token counts |
| Smoke test | yes, redesigned | native step runner with pass/fail badges, no shell script |
| Instance fleet list | yes, redesigned | per-row actions + cost total |
| HF model browser + quant pin | yes, redesigned | search, shard grouping, "create recipe", drop pinning |
| Recipe / tier / image editor | yes, redesigned | typed forms, referential integrity, comment-preserving save |
| Destroy instance | yes | any instance, rich confirmation, clears active endpoint |
| `press_enter` / `← Back` / `console.clear` | **drop** | TUI artefacts |
| Hardcoded model & price tables | **drop** | fetch or derive |
| `.hf_pin` / `.pinned_provider` | **drop** (read once for migration) | replaced by direct actions |

---

## 5. Compatibility contracts the Rust port must respect

1. **`~/.vastai-gguf/` is the existing state root** and contains a real Together API key. Read it for
   migration; do not clobber it. New state belongs in an XDG-ish dir (per `00-machine-ground-truth.md`).
2. **`usage.log` JSONL fields:** `timestamp` (`%Y-%m-%dT%H:%M:%SZ`), `provider`, `model_id`,
   `prompt_tokens`, `completion_tokens`, `cost_usd`. Local inference logs `cost_usd: 0.0`.
   (The ground-truth doc also notes an `epoch` field present on disk.)
3. **`local_instances/<name>.json` fields:** `name`, `pid`, `port`, `host`, `binary`, `model_path`,
   `backend`, `started_at`, `status` (`starting|running|stopped`), `stopped_at`. Paths in these files
   are frequently stale — validate on load.
4. **`recipes.toml` schema** — `[[recipes]]`, `[gpu_tiers.<key>]`, `[docker]`.
   Required fields by provider (`recipe_editor.py`):
   - `vast_gguf`: `name, label, gpu, model_repo, model_quant, ctx`
   - `local`: `name, label, model_path, port`
   - `together`: `name, label, model_id`
   - `vllm`: `name, label, gpu, model_id, ctx`
   - tier: `vast_names (list), label, max_price`
   Optional vast fields: `parallel, kv_type, min_disk_gb, image_type, description, llama_cpp_repo,
   llama_cpp_ref`. Others seen in use: `num_gpus, min_cuda, mmproj, host, n_gpu_layers, backend,
   mode, api_key, binary, model_id, quantization, kv_cache_dtype, enforce_eager, reasoning_parser,
   price_input, price_output`.
   Name validation: alphanumeric plus `- _ .`; `ctx` must be a positive int.
5. **Provider key spelling must be normalised**: `vast-gguf` (runtime) vs `vast_gguf` (recipes) vs
   `vllm` / `local` / `together`.
6. **`vast_up.sh` env contract** (§S2V) is the launch semantics reference even after the shell script
   is replaced; likewise `launch.sh` ↔ `_get_local_server_args` for local flags.
7. **`smoke.sh` check list** (models / warm-up / tool-calling / throughput) defines what "working
   endpoint" means here — reimplement those four probes natively.
8. **Tunnel:** local `8800` → container `127.0.0.1:8000`; PID at `/tmp/vastai-gguf-tunnel.pid`;
   ControlMaster in `~/.ssh/config` is a real performance precondition.
9. **Proxy:** `127.0.0.1:8888`, OpenAI-compatible (`/v1/chat/completions`, `/v1/completions`,
   `/health`), routing per `resolve_target()`.
10. **Vast REST endpoints needed** (currently reached via the broken CLI): list instances, show
    instance, search offers (filter string grammar in §S2O and `vast_up.sh:178`), create instance
    with an `onstart` command, destroy instance.
11. **External binaries currently required:** `vastai`, `ssh`, `curl`, `jq`, `bash`. The Rust port
    should need only `ssh`.
