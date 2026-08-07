# vastai-gguf + vastai-studio — the serving images ApexRouter rents boxes with

## vastai-gguf (single-service LLM)

Two images, one launch script, published as
`ghcr.io/buckster123/vastai-gguf:{prebuilt,builder}`:

| Tag | What | When |
|---|---|---|
| `:prebuilt` | llama.cpp compiled fat: SM **80** (A100) / **86** (3090) / **89** (4090, incl. modded-48GB) / **90** (H100) / **120** (5090) + PTX | Default. Boots in seconds. |
| `:builder` | CUDA toolchain, no binary; compiles on the box for the exact SM arch (~8–12 min) | Odd arches, and **PR-pinned builds** (`LLAMA_CPP_REF=pull/N/head`) — the only reliable MTP path (`pull/22673/head`). |

This is the 2026-08 refresh of the original (authored by Qwen3.6-27B in the LocalRouter
era). Every change traces to a paid-for lesson in `docs/GARDEN-RUNS.md`:

- **`--flash-attn` is a value flag on 2026 master** (`on|off|auto`); the bare legacy form
  aborts the boot. `FLASH_ATTN=auto` (default) passes nothing; `on`/`off`/`bare` are
  explicit. The boot-run always executes the *unpatched* script, so this had to be fixed
  at source, not by `sed -i` on a live box.
- **Anchored download globs.** `*${QUANT}*.gguf` matched superstrings — asking for
  `Q6_K` also pulled `UD-Q6_K_XL`: 46 GB for a 23 GB model, twice, and on a metered host
  ($9–38/TB inbound exists) twice the bandwidth bill.
- **PR refs are fetchable.** `git clone --branch pull/22673/head` fails; the clone logic
  now fetches `pull/N/head` explicitly. Build-number tags **lie** (`b8991` resolved to
  master's tip) — pin branches, shas, or PR refs only.
- **Webui stubs.** Post-2026-05 trees expect a *built* webui `dist/` the source checkout
  does not carry; four stub files land in both known locations right before
  `cmake --build`, every attempt (configure clobbers them).
- **China playbook** (huggingface.co is hard-blocked from CN, Errno 101):
  - `MODEL_SOURCE=modelscope` — first choice from inside CN (~1.6 s to answer).
  - `HF_ENDPOINT=https://hf-mirror.com` works, but **hf_transfer wedges mirrors at
    ~40 Mbit** (measured) — launch.sh auto-disables it whenever `HF_ENDPOINT` is set.
  - Downloads retry 3× with resume: throttled-not-blocked links win on patience.
- **No more pkill.** The server pid lands in `/run/llama-server.pid`; use `/app/stop.sh`
  and `/app/restart.sh`. `pkill -f llama-server` over ssh matches your own command line
  and kills the session — it did, three separate times.
- **SM86 exists now.** The original prebuilt covered 89+120 only; every 3090 run
  silently fell back to compiling from source.

## Building and pushing

Push-button: the **vastai-gguf image** workflow
(`.github/workflows/vastai-gguf-image.yml`, `workflow_dispatch`) builds either or both
images on a GitHub runner and pushes `:prebuilt`/`:builder` plus dated rollback tags
(`:prebuilt-YYYYMMDD`). Locally (needs ~30 GB free):

```sh
docker build -f docker/Dockerfile         -t ghcr.io/buckster123/vastai-gguf:prebuilt docker/
docker build -f docker/Dockerfile.builder -t ghcr.io/buckster123/vastai-gguf:builder  docker/
docker push  ghcr.io/buckster123/vastai-gguf:prebuilt
docker push  ghcr.io/buckster123/vastai-gguf:builder
```

After a verified push, delete the `[known_forks."garden-mtp"]` block from
`~/.config/apexrouter/config.toml` — it exists only to force the builder image while the
published images predate the MTP pin logic.

## The aria2c mirror rescue (manual, from GARDEN-RUNS R4)

When even the mirror throttles per-connection, `aria2c -x8` straight off it runs 4–6×
the hf CLI. The trap: hf leaves `*.incomplete` files whose names are **etag hashes**,
not filenames. Map them before resuming — never guess:

```sh
# for each shard, HEAD the mirror and read the etag the incomplete file is named after
curl -sI "https://hf-mirror.com/<repo>/resolve/main/<shard>.gguf" | grep -i x-linked-etag
# then hand each mapped URL to aria2c with the incomplete file as -o, e.g.
aria2c -x8 -s8 -c -d "$DIR" -o "<shard>.gguf" "<mirror-url>"
```

(A `content-length: 1206` on the HEAD is the redirect stub, not the file —
`x-linked-etag` stays authoritative.)

## What ApexRouter sends

`apexrouter vast rent` launches these images with `args: ["sleep","infinity"]` (so the
entrypoint does not double-launch) and runs `bash /app/launch.sh > /var/log/launch.log
2>&1 &` from `onstart`, with the model/quant/ctx env from the container plan. `HOST` is
forced to `127.0.0.1` unless `expose_public` — the tunnel-only posture.

## vastai-studio (multi-service: LLM + Comfy video/image)

Published as `ghcr.io/buckster123/vastai-studio:cu128` (+ dated `:cu128-YYYYMMDD`).

One image, three processes (S6): llama-server + two ComfyUI instances. Built from
`docker/studio/` — see **[docker/studio/README.md](studio/README.md)** for the full
contract, env vars, and `STUDIO_FETCH_FILES` format. Workflow:

```
Actions → "vastai-studio image" → Run workflow
```

Requires a published `vastai-gguf:prebuilt` (llama-server is `COPY --from` that image).
