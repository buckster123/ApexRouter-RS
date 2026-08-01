#!/usr/bin/env bash
# Container entrypoint for vastai-gguf — the GGUF serving image ApexRouter rents boxes with.
#
# Env-driven so the same image serves any GGUF model without rebuilding. This is the
# 2026-08 refresh: the original (authored by Qwen3.6-27B for LocalRouter) carried four
# defects the garden campaign paid real money to find — each fix below names its receipt
# (ApexRouter-RS/docs/GARDEN-RUNS.md).
#
# Required env:
#   MODEL_REPO    HF repo,  e.g. unsloth/Qwen3.6-27B-GGUF
#   MODEL_QUANT   quant tag, e.g. UD-Q6_K_XL  (anchored match — see "model fetch")
#
# Optional env:
#   IMAGE_TYPE       prebuilt | builder  (default prebuilt)
#                    builder: compiles llama.cpp for the host GPU's exact SM arch
#   CTX              context tokens (default 65536)
#   KV_TYPE          bf16 | q8_0 | q4_0   (default q8_0; q4_0 is the 256k fitting lever)
#   MODE             thinking | coding | nonthinking   (default thinking)
#   N_GPU_LAYERS     default 999 (all on GPU)
#   PARALLEL         concurrent decode slots (default 1)
#   FLASH_ATTN       auto | on | off | bare   (default auto = pass nothing; modern
#                    llama-server defaults to auto. "on"/"off" use the value form the
#                    2026 master REQUIRES; "bare" emits legacy `--flash-attn` for old
#                    builds. The bare flag on master kills the boot — GARDEN-RUNS R2a.)
#   EXTRA_ARGS       passthrough to llama-server (e.g. "-sm none --device CUDA0")
#   HF_TOKEN         for gated repos
#   HF_ENDPOINT      HF mirror, e.g. https://hf-mirror.com — hf CLI honors it natively.
#                    Setting it DISABLES hf_transfer (it wedges mirrors at ~40 Mbit —
#                    measured, GARDEN-RUNS R4 China playbook).
#   MODEL_SOURCE     hf | modelscope   (default hf). ModelScope answers in ~1.6 s from
#                    inside CN where huggingface.co is hard-blocked (Errno 101).
#   MODELS_DIR       default /workspace/models
#   MMPROJ           F16 to enable vision (downloads the matching mmproj gguf)
#   PORT, HOST       default 8000, 127.0.0.1 (tunnel-only posture; ApexRouter sets HOST)
#   LLAMA_CPP_REPO   custom llama.cpp fork (default: ggml-org/llama.cpp)
#   LLAMA_CPP_REF    branch/tag/commit/PR ref to build from (default: master).
#                    `pull/N/head` is supported and is the ONLY reliable way to pin an
#                    unmerged PR: build-number tags lie (b8991 resolves to master's tip —
#                    GARDEN-RUNS "locked llama.cpp refs"). MTP needs pull/22673/head.
#
# Process control (kills the pkill trilogy — GARDEN-RUNS R2a):
#   the server's PID lands in /run/llama-server.pid; use /app/stop.sh and
#   /app/restart.sh instead of `pkill -f llama-server`, which over ssh matches YOUR OWN
#   command line and kills the session. Also: `sed -i` on this script while it runs does
#   nothing to the running copy (bash holds the old inode) — patch, then RELAUNCH.

set -euo pipefail

log() { printf '[%s] %s\n' "$(date -u +%H:%M:%SZ)" "$*"; }
die() { log "FATAL: $*"; exit 1; }

: "${MODEL_REPO:?MODEL_REPO required (e.g. unsloth/Qwen3.6-27B-GGUF)}"
: "${MODEL_QUANT:?MODEL_QUANT required (e.g. UD-Q6_K_XL)}"

IMAGE_TYPE="${IMAGE_TYPE:-prebuilt}"
CTX="${CTX:-65536}"
KV_TYPE="${KV_TYPE:-q8_0}"
MODE="${MODE:-thinking}"
N_GPU_LAYERS="${N_GPU_LAYERS:-999}"
PARALLEL="${PARALLEL:-1}"
FLASH_ATTN="${FLASH_ATTN:-auto}"
EXTRA_ARGS="${EXTRA_ARGS:-}"
MODELS_DIR="${MODELS_DIR:-/workspace/models}"
MODEL_SOURCE="${MODEL_SOURCE:-hf}"
PORT="${PORT:-8000}"
HOST="${HOST:-127.0.0.1}"

# ── builder path: compile llama.cpp for exact SM arch ─────────────────────────
if [ "${IMAGE_TYPE}" = "builder" ] && [ ! -x /usr/local/bin/llama-server ]; then
    log "==> builder image: no pre-compiled llama-server — detecting GPU arch..."

    LLAMA_CPP_REPO="${LLAMA_CPP_REPO:-ggml-org/llama.cpp}"
    LLAMA_CPP_REF="${LLAMA_CPP_REF:-master}"

    RAW_CAP="$(nvidia-smi --query-gpu=compute_cap --format=csv,noheader 2>/dev/null | head -1 | tr -d ' ')"
    if [ -z "${RAW_CAP}" ]; then
        die "nvidia-smi returned no compute_cap — is the GPU visible?"
    fi
    SM="${RAW_CAP//./}"   # "9.0" → "90", "12.0" → "120"
    log "    detected SM arch: ${SM}  (compute_cap ${RAW_CAP})"

    SRC_DIR="/opt/llama.cpp"
    BUILD_DIR="${SRC_DIR}/build"

    # Fetch the requested tree. `git clone --branch` cannot fetch a PR ref, and
    # build-number tags cannot be trusted (b8991 resolved to master's tip), so:
    # branches/tags clone shallow; `pull/N/head` and bare commits fetch into a fresh
    # default clone and check out explicitly.
    fetch_tree() {
        local repo="$1" ref="$2"
        rm -rf "${SRC_DIR}"
        case "${ref}" in
            pull/*/head)
                log "    cloning ${repo} then fetching PR ref ${ref}..."
                git clone --depth 1 "https://github.com/${repo}.git" "${SRC_DIR}"
                git -C "${SRC_DIR}" fetch --depth 1 origin "${ref}:pr-pin"
                git -C "${SRC_DIR}" checkout pr-pin
                ;;
            *)
                if git clone --depth 1 --branch "${ref}" \
                        "https://github.com/${repo}.git" "${SRC_DIR}" 2>/dev/null; then
                    :
                else
                    # A bare commit sha: full-ish clone, then checkout.
                    log "    ${ref} is not a branch/tag — cloning and checking out the commit..."
                    git clone "https://github.com/${repo}.git" "${SRC_DIR}"
                    git -C "${SRC_DIR}" checkout "${ref}"
                fi
                ;;
        esac
        log "    tree at $(git -C "${SRC_DIR}" rev-parse --short HEAD)"
    }

    if [ ! -d "${SRC_DIR}" ]; then
        fetch_tree "${LLAMA_CPP_REPO}" "${LLAMA_CPP_REF}"
    elif [ "${LLAMA_CPP_REPO}" != "ggml-org/llama.cpp" ] || [ "${LLAMA_CPP_REF}" != "master" ]; then
        log "    custom repo/ref requested (${LLAMA_CPP_REPO} @ ${LLAMA_CPP_REF}) — re-fetching..."
        fetch_tree "${LLAMA_CPP_REPO}" "${LLAMA_CPP_REF}"
    fi

    # The server build embeds webui assets; trees after ~2026-05 expect a BUILT dist/
    # that a source checkout does not carry, and the configure step can clobber stubs.
    # So: stub every path any known tree reads, immediately before the build. Four
    # files, both locations, harmless where unused (GARDEN-RUNS "builder webui").
    stub_webui() {
        local d f
        for d in "${BUILD_DIR}/tools/ui/dist" "${SRC_DIR}/tools/server/webui/dist"; do
            mkdir -p "${d}"
            for f in index.html bundle.js loading.html bundle.css; do
                [ -s "${d}/${f}" ] || printf '<!-- stub: headless build -->\n' > "${d}/${f}"
            done
        done
    }

    log "    configuring for SM${SM}..."
    cmake -B "${BUILD_DIR}" -G Ninja \
        -DCMAKE_BUILD_TYPE=Release \
        -DGGML_CUDA=ON \
        -DGGML_NATIVE=OFF \
        -DCMAKE_CUDA_ARCHITECTURES="${SM}-real" \
        -DLLAMA_CURL=ON \
        -DBUILD_SHARED_LIBS=OFF \
        "${SRC_DIR}" 2>&1 | tail -5

    stub_webui
    log "    compiling llama-server (~8-12 min on first boot)..."
    cmake --build "${BUILD_DIR}" --config Release \
        -j"$(nproc)" \
        --target llama-server llama-bench 2>&1 | grep -E '^\[|error:|warning:' | tail -20

    install -m755 "${BUILD_DIR}/bin/llama-server" /usr/local/bin/llama-server
    install -m755 "${BUILD_DIR}/bin/llama-bench"  /usr/local/bin/llama-bench 2>/dev/null || true
    log "    compile done — llama-server installed"
fi

[ -x /usr/local/bin/llama-server ] || die "llama-server not found — check image or build log"

# ── sampling presets ───────────────────────────────────────────────────────────
case "${MODE}" in
    thinking)
        SAMPLE_ARGS="--temp 1.0 --top-p 0.95 --top-k 20 --min-p 0.0 --presence-penalty 1.5"
        TPL_KW=""
        TPL_KW_JSON=""
        ;;
    coding)
        SAMPLE_ARGS="--temp 0.6 --top-p 0.95 --top-k 20 --min-p 0.0 --presence-penalty 0.0"
        TPL_KW=""
        TPL_KW_JSON=""
        ;;
    nonthinking)
        SAMPLE_ARGS="--temp 0.7 --top-p 0.80 --top-k 20 --min-p 0.0 --presence-penalty 1.5"
        TPL_KW='--chat-template-kwargs'
        TPL_KW_JSON='{"enable_thinking":false}'
        ;;
    *)
        die "MODE must be thinking|coding|nonthinking, got: ${MODE}"
        ;;
esac

# ── model fetch (idempotent, ANCHORED patterns) ────────────────────────────────
# The old glob `*${MODEL_QUANT}*.gguf` matched supersets: asking for Q6_K also pulled
# UD-Q6_K_XL — 46 GB for a 23 GB model, twice the wait, and on a metered host twice the
# bandwidth bill (GARDEN-RUNS R2a, "the fat glob became a money bug"). These patterns
# anchor the quant at a filename or directory boundary so a tag can never match its own
# superstring: `-Q6_K.gguf` does not match `-UD-Q6_K_XL.gguf`.
mkdir -p "${MODELS_DIR}"
TARGET_DIR="${MODELS_DIR}/$(basename "${MODEL_REPO}")"

INCLUDE_PATTERNS=(
    "*-${MODEL_QUANT}.gguf"              # single file:  Model-UD-Q6_K_XL.gguf
    "*-${MODEL_QUANT}-00*-of-*.gguf"     # shards:       Model-UD-Q6_K_XL-00001-of-00002.gguf
    "*${MODEL_QUANT}/*.gguf"             # subdir layout: UD-Q6_K_XL/Model-....gguf
)
if [ -n "${MMPROJ:-}" ]; then
    INCLUDE_PATTERNS+=("*mmproj-${MMPROJ}*.gguf")
fi

# Throttled-not-blocked networks (github/HF through the GFW) win on retries, not on
# giving up: three attempts, resumable both engines (GARDEN-RUNS R4 China playbook).
fetch_with_retries() {
    local attempt
    for attempt in 1 2 3; do
        if "$@"; then
            return 0
        fi
        log "  download attempt ${attempt}/3 failed; retrying in 10 s (resume supported)"
        sleep 10
    done
    return 1
}

need_fetch=0
if [ ! -d "${TARGET_DIR}" ] || \
   [ -z "$(find "${TARGET_DIR}" -maxdepth 2 -name '*.gguf' 2>/dev/null | grep -i "${MODEL_QUANT}" || true)" ]; then
    need_fetch=1
fi

if [ "${need_fetch}" = "1" ]; then
    log "fetching ${MODEL_REPO} (quant=${MODEL_QUANT}, source=${MODEL_SOURCE})  ->  ${TARGET_DIR}"
    [ -n "${MMPROJ:-}" ] && log "  + mmproj-${MMPROJ} (vision)"

    case "${MODEL_SOURCE}" in
        modelscope)
            # First choice inside CN: huggingface.co is hard-blocked (Errno 101) and the
            # mirror throttles per-connection; ModelScope answers in ~1.6 s.
            MS_INCLUDES=()
            for p in "${INCLUDE_PATTERNS[@]}"; do MS_INCLUDES+=(--include "${p}"); done
            fetch_with_retries modelscope download --model "${MODEL_REPO}" \
                --local_dir "${TARGET_DIR}" "${MS_INCLUDES[@]}" \
                || die "modelscope download failed after 3 attempts"
            ;;
        hf)
            HF_INCLUDES=()
            for p in "${INCLUDE_PATTERNS[@]}"; do HF_INCLUDES+=(--include "${p}"); done
            if [ -n "${HF_ENDPOINT:-}" ]; then
                # Mirror in play: hf_transfer makes mirrors WORSE (18 MB/s → a 40 Mbit
                # wedge, measured). Plain resume, and aria2c -x8 straight off the mirror
                # is the manual rescue at 4-6× — see docker/README.md for the
                # etag↔shard mapping recipe.
                log "  HF_ENDPOINT=${HF_ENDPOINT} — hf_transfer disabled on mirrors"
                fetch_with_retries env -u HF_HUB_ENABLE_HF_TRANSFER \
                    hf download "${MODEL_REPO}" --local-dir "${TARGET_DIR}" "${HF_INCLUDES[@]}" \
                    || die "hf download via mirror failed after 3 attempts"
            else
                fetch_with_retries env HF_HUB_ENABLE_HF_TRANSFER=1 \
                    hf download "${MODEL_REPO}" --local-dir "${TARGET_DIR}" "${HF_INCLUDES[@]}" \
                    || die "hf download failed after 3 attempts"
            fi
            ;;
        *)
            die "MODEL_SOURCE must be hf|modelscope, got: ${MODEL_SOURCE}"
            ;;
    esac
else
    log "model already present in ${TARGET_DIR}, skipping fetch"
fi

# ── locate weights ─────────────────────────────────────────────────────────────
# Split GGUFs: llama-server takes the first shard; 00001 sorts first.
MODEL_FILE="$(find "${TARGET_DIR}" -maxdepth 2 -name "*.gguf" \
    | grep -iE "${MODEL_QUANT}" | grep -v 'mmproj' | sort | head -n1 || true)"
[ -n "${MODEL_FILE}" ] || die "no .gguf matching '${MODEL_QUANT}' in ${TARGET_DIR}"
MODEL_PATH="${MODEL_FILE}"

MMPROJ_ARGS=""
if [ -n "${MMPROJ:-}" ]; then
    MMPROJ_FILE="$(find "${TARGET_DIR}" -maxdepth 2 -name "*.gguf" \
        | grep -iE "mmproj-${MMPROJ}" | head -n1 || true)"
    [ -n "${MMPROJ_FILE}" ] || die "MMPROJ requested but no mmproj file found"
    MMPROJ_ARGS="--mmproj ${MMPROJ_FILE}"
fi

# ── flash attention: the flag whose SYNTAX is version-dependent ────────────────
# 2026 master takes `--flash-attn on|off|auto` (a value); older builds take a bare
# `--flash-attn`. Feeding master the bare form aborts the boot — and the boot-run
# always executes the unpatched script, so this must be right the first time.
FA_ARGS=""
case "${FLASH_ATTN}" in
    auto) ;;                                  # modern default is auto; pass nothing
    on|off) FA_ARGS="--flash-attn ${FLASH_ATTN}" ;;
    bare) FA_ARGS="--flash-attn" ;;           # legacy builds only
    *) die "FLASH_ATTN must be auto|on|off|bare, got: ${FLASH_ATTN}" ;;
esac

# ── launch ─────────────────────────────────────────────────────────────────────
log "==> launching llama-server"
log "    model   : ${MODEL_PATH}"
log "    mmproj  : ${MMPROJ_ARGS:-<none>}"
log "    ctx     : ${CTX}    kv-cache: ${KV_TYPE}    n-gpu-layers: ${N_GPU_LAYERS}"
log "    mode    : ${MODE}   parallel: ${PARALLEL}   flash-attn: ${FLASH_ATTN}"
log "    listen  : ${HOST}:${PORT}"
nvidia-smi --query-gpu=name,memory.total,memory.free --format=csv,noheader 2>/dev/null || true

# The exec below replaces this shell, so $$ IS the server's pid. /app/stop.sh and
# /app/restart.sh read this file — never `pkill -f llama-server` over ssh: the pattern
# matches your own command line and kills the session (three times; GARDEN-RUNS).
mkdir -p /run
echo $$ > /run/llama-server.pid

# shellcheck disable=SC2086
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
    ${FA_ARGS} \
    ${SAMPLE_ARGS} ${TPL_KW} ${TPL_KW_JSON:+"$TPL_KW_JSON"} ${EXTRA_ARGS}
