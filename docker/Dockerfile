# syntax=docker/dockerfile:1
# vastai-gguf:prebuilt — GGUF serving on rented vast.ai GPUs via llama.cpp.
# 2026-08 refresh of the Qwen-authored original, with the garden campaign's receipts
# baked in (ApexRouter-RS/docs/GARDEN-RUNS.md).
#
# Fat binary covering every card the campaign actually rented plus the datacenter tier:
#   80 = A100 · 86 = 3090 (the R1/R2b workhorses — MISSING from the original, which is
#   why every 3090 run had to compile from source) · 89 = 4090 (incl. the modded-48GB
#   pair) · 90 = H100 · 120 = 5090. `120-virtual` embeds PTX so a future arch can JIT
#   instead of failing to load.
#
# Build:
#   docker build -f docker/Dockerfile -t ghcr.io/buckster123/vastai-gguf:prebuilt docker/
# or push-button via .github/workflows/vastai-gguf-image.yml.

FROM nvidia/cuda:12.8.0-devel-ubuntu24.04 AS builder

ARG DEBIAN_FRONTEND=noninteractive
# Pin a known-good ref at build time (`--build-arg LLAMA_CPP_REF=...`). Build-number
# tags LIE (b8991 resolves to master's tip); use a branch, a commit sha, or a PR ref
# in the form pull/N/head — MTP needs pull/22673/head.
ARG LLAMA_CPP_REPO=ggml-org/llama.cpp
ARG LLAMA_CPP_REF=master
ARG CUDA_ARCHS="80-real;86-real;89-real;90-real;120-real;120-virtual"

RUN apt-get update && apt-get install -y --no-install-recommends \
        git build-essential cmake ninja-build curl ca-certificates \
        libcurl4-openssl-dev pciutils \
    && rm -rf /var/lib/apt/lists/*

# `git clone --branch` cannot fetch a PR ref, so pull/N/head gets a default clone plus
# an explicit fetch — the same logic launch.sh uses on the builder image.
WORKDIR /opt
RUN set -eux; \
    case "${LLAMA_CPP_REF}" in \
        pull/*/head) \
            git clone --depth 1 "https://github.com/${LLAMA_CPP_REPO}.git" llama.cpp; \
            git -C llama.cpp fetch --depth 1 origin "${LLAMA_CPP_REF}:pr-pin"; \
            git -C llama.cpp checkout pr-pin;; \
        *) \
            git clone --depth 1 --branch "${LLAMA_CPP_REF}" \
                "https://github.com/${LLAMA_CPP_REPO}.git" llama.cpp \
            || { git clone "https://github.com/${LLAMA_CPP_REPO}.git" llama.cpp; \
                 git -C llama.cpp checkout "${LLAMA_CPP_REF}"; };; \
    esac; \
    git -C llama.cpp rev-parse HEAD

# Headless UI posture. Current trees (scripts/ui-assets.cmake): DO NOT stub anything —
# a partial dist/ passes the "npm output up-to-date" check and then fails the embed
# tool's 8-file manifest validation (proven the expensive way: a 63-minute CI build died
# on the last step). With npm provisioning and the HF asset download both OFF and no
# dist present, the script's own fallback embeds a placeholder UI and succeeds — exactly
# what a serving image wants. (The old xxd-based trees that DO need stub files are the
# builder image's problem; launch.sh detects which world it is in.)
WORKDIR /opt/llama.cpp
RUN set -eux; \
    cmake -B build -G Ninja \
        -DCMAKE_BUILD_TYPE=Release \
        -DGGML_CUDA=ON \
        -DGGML_NATIVE=OFF \
        -DCMAKE_CUDA_ARCHITECTURES="${CUDA_ARCHS}" \
        -DLLAMA_CURL=ON \
        -DBUILD_SHARED_LIBS=OFF \
        -DLLAMA_BUILD_UI=OFF \
        -DLLAMA_USE_PREBUILT_UI=OFF; \
    cmake --build build --config Release -j"$(nproc)" \
        --target llama-server llama-cli llama-mtmd-cli llama-gguf-split llama-bench; \
    strip build/bin/llama-* || true

# ---------------- runtime ----------------
FROM nvidia/cuda:12.8.0-runtime-ubuntu24.04

ARG DEBIAN_FRONTEND=noninteractive
# aria2 is the manual rescue on throttling mirrors (4-6× the hf CLI, measured in CN).
RUN apt-get update && apt-get install -y --no-install-recommends \
        python3 python3-pip python3-venv ca-certificates curl jq tini \
        libcurl4 libgomp1 aria2 \
    && rm -rf /var/lib/apt/lists/*

# hf + modelscope CLIs in one venv; hf_transfer for the real CDN (launch.sh disables it
# on mirrors, where it wedges at ~40 Mbit — measured).
RUN python3 -m venv /opt/hf-venv \
 && /opt/hf-venv/bin/pip install --no-cache-dir -U pip \
        "huggingface_hub[cli]>=0.36" hf_transfer modelscope \
 && ln -s /opt/hf-venv/bin/hf /usr/local/bin/hf \
 && ln -s /opt/hf-venv/bin/modelscope /usr/local/bin/modelscope

COPY --from=builder /opt/llama.cpp/build/bin/llama-server     /usr/local/bin/
COPY --from=builder /opt/llama.cpp/build/bin/llama-cli        /usr/local/bin/
COPY --from=builder /opt/llama.cpp/build/bin/llama-mtmd-cli   /usr/local/bin/
COPY --from=builder /opt/llama.cpp/build/bin/llama-bench      /usr/local/bin/
COPY --from=builder /opt/llama.cpp/build/bin/llama-gguf-split /usr/local/bin/

WORKDIR /app
COPY launch.sh stop.sh restart.sh /app/
RUN chmod +x /app/launch.sh /app/stop.sh /app/restart.sh

# Tunnel-only posture by default; ApexRouter sets HOST explicitly either way.
ENV MODELS_DIR=/workspace/models \
    PORT=8000 \
    HOST=127.0.0.1
EXPOSE 8000
VOLUME ["/workspace"]

# tini handles PID 1 + signal forwarding (clean SIGTERM on vast destroy)
ENTRYPOINT ["/usr/bin/tini","-g","--","/app/launch.sh"]
