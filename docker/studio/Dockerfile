# syntax=docker/dockerfile:1
# vastai-studio:cu128 — multi-service studio box for ApexRouter (LLM + Comfy lanes).
#
# Charter: docs/STUDIO.md S6. One image, three services:
#   llama-server  (OpenAI path, in-container :8000)
#   ComfyUI video (Wan lane,        :8188, GPU0)
#   ComfyUI image (Qwen-Image lane, :8189, GPU1)
#
# llama-server is COPY'd from the published vastai-gguf:prebuilt image so we do not
# recompile a 6-arch fat binary. ComfyUI + torch + custom nodes are pinned at image
# build time (workflow-template brittleness is an image-rebuild discipline, not a
# boot-time pip gamble). Weights stay on the rented disk (/workspace) and are
# pulled by studio.sh — never baked into the layer.
#
# Build (needs network + ~15 GB free; prefer GHA):
#   docker build -f docker/studio/Dockerfile \
#     --build-arg GGUF_IMAGE=ghcr.io/buckster123/vastai-gguf:prebuilt \
#     -t ghcr.io/buckster123/vastai-studio:cu128 docker/studio/
# Or: .github/workflows/vastai-studio-image.yml (workflow_dispatch).

ARG GGUF_IMAGE=ghcr.io/buckster123/vastai-gguf:prebuilt
FROM ${GGUF_IMAGE} AS gguf

FROM nvidia/cuda:12.8.0-runtime-ubuntu24.04

ARG DEBIAN_FRONTEND=noninteractive
# ComfyUI commit measured in R3 (docs/GARDEN-RUNS.md). Full SHA required for
# `git fetch origin <ref>` — a short hash is not a remote ref name.
# Bump only with a re-measure.
ARG COMFYUI_REF=e803f24ea090de7108772d65957fd6388d3f2085
# Torch wheel index for CUDA 12.8. S6 pins cu128 to the base image family.
ARG TORCH_INDEX=https://download.pytorch.org/whl/cu128
ARG TORCH_VERSION=2.7.1

# python3.12-dev is NON-NEGOTIABLE: the fp8-scaled path JIT-compiles a triton shim
# and needs Python.h. Without it the first image job dies in ~1.9 s (R3 trap).
RUN apt-get update && apt-get install -y --no-install-recommends \
        python3.12 python3.12-dev python3.12-venv python3-pip \
        ca-certificates curl jq tini git git-lfs aria2 \
        libcurl4 libgomp1 build-essential \
    && rm -rf /var/lib/apt/lists/* \
    && ln -sf /usr/bin/python3.12 /usr/local/bin/python3 \
    && ln -sf /usr/bin/python3.12 /usr/local/bin/python

# llama-server (and friends) from the fat prebuilt image — one binary, six SM arches.
COPY --from=gguf /usr/local/bin/llama-server     /usr/local/bin/
COPY --from=gguf /usr/local/bin/llama-cli        /usr/local/bin/
COPY --from=gguf /usr/local/bin/llama-bench      /usr/local/bin/
COPY --from=gguf /usr/local/bin/llama-gguf-split /usr/local/bin/
COPY --from=gguf /usr/local/bin/llama-mtmd-cli   /usr/local/bin/

# hf + modelscope CLIs (studio.sh reuses the launch.sh download doctrine).
RUN python3 -m venv /opt/hf-venv \
 && /opt/hf-venv/bin/pip install --no-cache-dir -U pip \
        "huggingface_hub[cli]>=0.36" hf_transfer modelscope \
 && ln -s /opt/hf-venv/bin/hf /usr/local/bin/hf \
 && ln -s /opt/hf-venv/bin/modelscope /usr/local/bin/modelscope

# ComfyUI venv: torch cu128 first, then ComfyUI requirements, then pinned tree.
# Image rebuild is the node-pack bump discipline — never `pip install` at boot.
RUN python3 -m venv /opt/comfy-venv \
 && /opt/comfy-venv/bin/pip install --no-cache-dir -U pip setuptools wheel \
 && /opt/comfy-venv/bin/pip install --no-cache-dir \
        --index-url "${TORCH_INDEX}" \
        "torch==${TORCH_VERSION}" torchvision torchaudio \
 && git init /opt/ComfyUI \
 && git -C /opt/ComfyUI remote add origin https://github.com/comfyanonymous/ComfyUI.git \
 && git -C /opt/ComfyUI fetch --depth 1 origin "${COMFYUI_REF}" \
 && git -C /opt/ComfyUI checkout --detach FETCH_HEAD \
 && /opt/comfy-venv/bin/pip install --no-cache-dir -r /opt/ComfyUI/requirements.txt \
 && /opt/comfy-venv/bin/pip install --no-cache-dir \
        "triton>=3.0" \
        "transformers>=4.45" \
        "accelerate>=0.33" \
        "safetensors>=0.4" \
        "einops" \
        "sentencepiece" \
 && mkdir -p /opt/ComfyUI/custom_nodes /run/studio /workspace \
 && printf '%s\n' "${COMFYUI_REF}" > /opt/ComfyUI/.studio-pin \
 && /opt/comfy-venv/bin/python -c "import torch; print('torch', torch.__version__, 'cuda', torch.version.cuda)"

# Optional custom-node packs: --build-arg CUSTOM_NODES="owner/repo@sha,owner2/repo2@ref"
# Empty by default — R3 used stock ComfyUI + fixed workflow JSONs (Imaginarium owns graphs).
ARG CUSTOM_NODES=""
COPY install-custom-nodes.sh /tmp/install-custom-nodes.sh
RUN chmod +x /tmp/install-custom-nodes.sh \
 && /tmp/install-custom-nodes.sh "${CUSTOM_NODES}" \
 && rm -f /tmp/install-custom-nodes.sh

WORKDIR /app
COPY studio.sh stop.sh /app/
RUN chmod +x /app/studio.sh /app/stop.sh

# Tunnel-only posture. ApexRouter force-sets HOST=127.0.0.1 at rent time either way.
ENV MODELS_DIR=/workspace/models \
    COMFY_DIR=/opt/ComfyUI \
    COMFY_VENV=/opt/comfy-venv \
    WORKSPACE=/workspace \
    HOST=127.0.0.1 \
    LLM_PORT=8000 \
    VIDEO_PORT=8188 \
    IMAGE_PORT=8189 \
    STUDIO_PID_DIR=/run/studio \
    PATH="/opt/comfy-venv/bin:/opt/hf-venv/bin:${PATH}"

EXPOSE 8000 8188 8189
VOLUME ["/workspace"]

# Rent path uses args=["sleep","infinity"] + onstart=studio.sh (same as vastai-gguf).
# tini as PID 1 so SIGTERM on destroy reaches children cleanly.
ENTRYPOINT ["/usr/bin/tini","-g","--"]
CMD ["sleep", "infinity"]
