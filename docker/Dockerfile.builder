# syntax=docker/dockerfile:1
# vastai-gguf:builder — CUDA dev toolchain, NO pre-compiled llama-server.
# launch.sh detects the GPU's SM arch at runtime and compiles for the exact target
# (~8-12 min cold start; works on any CUDA-capable card, including arches the prebuilt
# fat binary does not carry). It is also the image that can pin an unmerged PR at boot
# via LLAMA_CPP_REF=pull/N/head — the only reliable MTP path (pull/22673/head).
#
# 2026-08 refresh: PR-ref fetch logic, webui stubs, ModelScope/aria2/hf_transfer in the
# image, pidfile stop/restart helpers. Receipts: ApexRouter-RS/docs/GARDEN-RUNS.md.
#
# Build:
#   docker build -f docker/Dockerfile.builder -t ghcr.io/buckster123/vastai-gguf:builder docker/

ARG CUDA_VERSION=12.8.0
FROM nvidia/cuda:${CUDA_VERSION}-devel-ubuntu24.04

ARG DEBIAN_FRONTEND=noninteractive
ARG LLAMA_CPP_REF=master

RUN apt-get update && apt-get install -y --no-install-recommends \
        git build-essential cmake ninja-build \
        curl ca-certificates jq tini \
        python3 python3-pip python3-venv \
        libcurl4-openssl-dev libcurl4 libgomp1 pciutils aria2 \
    && rm -rf /var/lib/apt/lists/*

# Cache the default source so a stock boot only runs cmake; launch.sh re-fetches when a
# custom repo/ref (including a PR ref) is requested.
WORKDIR /opt
RUN git clone --depth 1 --branch ${LLAMA_CPP_REF} \
        https://github.com/ggml-org/llama.cpp.git

RUN python3 -m venv /opt/hf-venv \
 && /opt/hf-venv/bin/pip install --no-cache-dir -U pip \
        "huggingface_hub[cli]>=0.36" hf_transfer modelscope \
 && ln -s /opt/hf-venv/bin/hf /usr/local/bin/hf \
 && ln -s /opt/hf-venv/bin/modelscope /usr/local/bin/modelscope

WORKDIR /app
COPY launch.sh stop.sh restart.sh /app/
RUN chmod +x /app/launch.sh /app/stop.sh /app/restart.sh

ENV MODELS_DIR=/workspace/models \
    PORT=8000 \
    HOST=127.0.0.1
EXPOSE 8000
VOLUME ["/workspace"]

ENTRYPOINT ["/usr/bin/tini", "-g", "--", "/app/launch.sh"]
