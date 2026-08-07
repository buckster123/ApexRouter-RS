# vastai-studio — the multi-service studio image

`ghcr.io/buckster123/vastai-studio:cu128` — one container, three services, tunnel-only.

| Service | Runtime | In-container | Local tunnel (S5) | GPU |
|---|---|---|---|---|
| `llm` | llama-server (from `vastai-gguf:prebuilt`) | 8000 | ordinary 88xx lease → alias `studio-llm` | remainder of GPU0 |
| `video` | ComfyUI (pinned SHA) | 8188 | **8811** | 0 |
| `image` | ComfyUI (pinned SHA) | 8189 | **8812** | 1 |

Charter: `docs/STUDIO.md` S6. Measured posture: `docs/GARDEN-RUNS.md` R3.

## Design laws baked into the image

1. **No boot-time pip.** ComfyUI + torch cu128 + node packs are image layers. A node-pack
   bump is a **rebuild**, not a wake-time gamble.
2. **`python3.12-dev` is present.** The fp8 path JIT-compiles triton; without Python.h the
   first image job dies in ~1.9 s (R3).
3. **llama-server is COPY'd from `vastai-gguf:prebuilt`**, not recompiled — fat SM coverage
   without a second 6-arch build.
4. **Weights live on `/workspace`**, never in the image. `studio.sh` pulls by **exact
   filename** (no fat globs), ModelScope-first, skip-if-present.
5. **Idempotent onstart.** Wake re-runs `studio.sh`; live pids are left alone; present
   weights are not re-downloaded. Target: wake ≈ service starts only.
6. **No pkill.** Pids under `/run/studio/<name>.pid`. Use `/app/stop.sh`.
7. **HOST=127.0.0.1 always.** The ssh tunnel is the boundary.

## Building

Push-button (preferred — CUDA layers are large):

```
# Actions → "vastai-studio image" → Run workflow
```

Locally (needs the prebuilt image already pulled, ~15+ GB free):

```sh
docker pull ghcr.io/buckster123/vastai-gguf:prebuilt
docker build -f docker/studio/Dockerfile \
  --build-arg GGUF_IMAGE=ghcr.io/buckster123/vastai-gguf:prebuilt \
  --build-arg COMFYUI_REF=e803f24 \
  -t ghcr.io/buckster123/vastai-studio:cu128 \
  docker/studio/
```

Optional custom nodes at build time:

```sh
--build-arg CUSTOM_NODES="ltdrdata/ComfyUI-Manager@main,..."
```

Dated rollback tags (`:cu128-YYYYMMDD`) are pushed by the workflow.

## What ApexRouter sends at rent time

```
image:       ghcr.io/buckster123/vastai-studio:cu128
args:        ["sleep", "infinity"]
onstart:     bash /app/studio.sh > /var/log/studio.log 2>&1 &
host:        127.0.0.1
env:         MODEL_REPO, MODEL_QUANT, STUDIO_FETCH_FILES, MODEL_SOURCE=modelscope, …
```

Then three `ensure_tunnel` calls: remote 8000 / 8188 / 8189 → local lease / **8811** / **8812**.

## `STUDIO_FETCH_FILES` format

One exact file per line (comments with `#` ok):

```
source|repo|filename|dest_subdir
```

`dest_subdir` is under `$COMFY_DIR/models/` unless absolute. Example (illustrative — pin
real R3 filenames from the cell before production rent):

```
modelscope|Wan-AI/Wan2.2-TI2V-5B|wan2.2_ti2v_5B_fp16.safetensors|diffusion_models
modelscope|…|umt5-xxl-enc-fp8.safetensors|text_encoders
hf|…|qwen_image_fp8.safetensors|diffusion_models
```

GGUF for the llm slot still uses `MODEL_REPO` + `MODEL_QUANT` with the **anchored**
patterns from `docker/launch.sh` (never `*Q6_K*` alone).

## Operator quick checks (on the box, through the tunnel)

```sh
curl -sS http://127.0.0.1:8811/system_stats | head
curl -sS http://127.0.0.1:8812/system_stats | head
curl -sS http://127.0.0.1:8888/v1/models   # after alias bind
cat /run/studio/*.pid
tail -f /var/log/studio-*.log
```

Imaginarium owns Comfy wire protocol. This image only **runs** the processes.
