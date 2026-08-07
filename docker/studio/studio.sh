#!/usr/bin/env bash
# Studio onstart — bring up llm + Comfy video + Comfy image on one rented box.
#
# Charter: docs/STUDIO.md S6 / S2 / S5. Idempotent by construction: wake re-runs
# this script, so warm start must do **zero** downloads and must not double-spawn.
#
# Services (in-container, tunnel-only HOST=127.0.0.1):
#   llm   :8000  → local tunnel lease  (OpenAI alias studio-llm via proxy 8888)
#   video :8188  → local **8811**      (ComfyUI, GPU0)
#   image :8189  → local **8812**      (ComfyUI, GPU1)
#
# Process control: pids under /run/studio/<name>.pid — use /app/stop.sh, never pkill.
#
# Env (all optional unless a lane is enabled and needs weights):
#   STUDIO_SERVICES     space list, default "llm video image"
#   HOST                default 127.0.0.1 (ALWAYS tunnel-only)
#   LLM_PORT / VIDEO_PORT / IMAGE_PORT   defaults 8000 / 8188 / 8189
#   VIDEO_CUDA / IMAGE_CUDA              defaults 0 / 1
#   MODEL_SOURCE        modelscope | hf   (default modelscope — CN-first, R3)
#   HF_ENDPOINT         e.g. https://hf-mirror.com (disables hf_transfer)
#   HF_TOKEN            gated repos
#   # LLM (GGUF) — same contract as launch.sh when llm is enabled:
#   MODEL_REPO / MODEL_QUANT / CTX / KV_TYPE / MODE / N_GPU_LAYERS / PARALLEL / FLASH_ATTN / EXTRA_ARGS
#   # Exact-file pulls for Comfy weights (skip if present and non-empty):
#   #   STUDIO_FETCH_FILE="<source>|<repo>|<filename>|<dest_subdir>"
#   # Repeated via STUDIO_FETCH_FILES with newlines, or call fetch_exact from a recipe onstart.
#   STUDIO_FETCH_FILES  multiline exact-file list (see README)
#   STUDIO_PULL_MODELS  1 (default if any fetch list / MODEL_REPO set) | 0
#   COMFY_DIR / COMFY_VENV / MODELS_DIR / STUDIO_PID_DIR

set -euo pipefail

log() { printf '[%s] %s\n' "$(date -u +%H:%M:%SZ)" "$*"; }
die() { log "FATAL: $*"; exit 1; }

HOST="${HOST:-127.0.0.1}"
LLM_PORT="${LLM_PORT:-8000}"
VIDEO_PORT="${VIDEO_PORT:-8188}"
IMAGE_PORT="${IMAGE_PORT:-8189}"
VIDEO_CUDA="${VIDEO_CUDA:-0}"
IMAGE_CUDA="${IMAGE_CUDA:-1}"
MODELS_DIR="${MODELS_DIR:-/workspace/models}"
COMFY_DIR="${COMFY_DIR:-/opt/ComfyUI}"
COMFY_VENV="${COMFY_VENV:-/opt/comfy-venv}"
PID_DIR="${STUDIO_PID_DIR:-/run/studio}"
MODEL_SOURCE="${MODEL_SOURCE:-modelscope}"
STUDIO_SERVICES="${STUDIO_SERVICES:-llm video image}"
WORKSPACE="${WORKSPACE:-/workspace}"

mkdir -p "${PID_DIR}" "${MODELS_DIR}" \
    "${COMFY_DIR}/models/checkpoints" \
    "${COMFY_DIR}/models/diffusion_models" \
    "${COMFY_DIR}/models/vae" \
    "${COMFY_DIR}/models/text_encoders" \
    "${COMFY_DIR}/models/clip" \
    "${COMFY_DIR}/models/unet" \
    "${COMFY_DIR}/output" \
    "${WORKSPACE}/comfy-output"

# Symlink Comfy output onto the persistent volume so park/wake keeps renders.
if [ ! -L "${COMFY_DIR}/output" ] && [ -d "${WORKSPACE}/comfy-output" ]; then
    # Keep image-baked output dir; also expose workspace path for operators.
    :
fi
ln -sfn "${WORKSPACE}/comfy-output" "${COMFY_DIR}/output-workspace" 2>/dev/null || true

# ── helpers ───────────────────────────────────────────────────────────────────

is_alive() {
    local pidfile="$1"
    [ -f "${pidfile}" ] || return 1
    local pid
    pid="$(cat "${pidfile}")"
    kill -0 "${pid}" 2>/dev/null
}

# Exact filename, never a glob. Skip if present and non-empty (size-match is a
# later refinement once we persist expected bytes; for now non-empty + exists).
fetch_exact() {
    local source="$1" repo="$2" filename="$3" dest_dir="$4"
    local dest="${dest_dir}/${filename}"
    mkdir -p "${dest_dir}"
    if [ -f "${dest}" ] && [ -s "${dest}" ]; then
        log "  present $(du -h "${dest}" | awk '{print $1}')  ${dest}"
        return 0
    fi
    log "  fetching ${source}:${repo}/${filename} -> ${dest_dir}"
    local attempt
    for attempt in 1 2 3; do
        case "${source}" in
            modelscope)
                if modelscope download --model "${repo}" --local_dir "${dest_dir}" \
                        --include "${filename}"; then
                    # modelscope may nest under repo name; flatten if needed
                    if [ ! -f "${dest}" ]; then
                        local found
                        found="$(find "${dest_dir}" -type f -name "${filename}" | head -n1 || true)"
                        if [ -n "${found}" ] && [ "${found}" != "${dest}" ]; then
                            mv -f "${found}" "${dest}"
                        fi
                    fi
                    [ -s "${dest}" ] && return 0
                fi
                ;;
            hf)
                if [ -n "${HF_ENDPOINT:-}" ]; then
                    if env -u HF_HUB_ENABLE_HF_TRANSFER \
                        hf download "${repo}" --local-dir "${dest_dir}" --include "${filename}"; then
                        [ -s "${dest}" ] && return 0
                    fi
                else
                    if env HF_HUB_ENABLE_HF_TRANSFER=1 \
                        hf download "${repo}" --local-dir "${dest_dir}" --include "${filename}"; then
                        [ -s "${dest}" ] && return 0
                    fi
                fi
                ;;
            *)
                die "fetch source must be modelscope|hf, got: ${source}"
                ;;
        esac
        log "  fetch attempt ${attempt}/3 failed for ${filename}; retry in 10 s"
        sleep 10
    done
    die "could not fetch ${filename} from ${source}:${repo} after 3 attempts"
}

fetch_declared_files() {
    [ -n "${STUDIO_FETCH_FILES:-}" ] || return 0
    log "==> pulling declared studio weight files (exact names)"
    # format per line: source|repo|filename|dest_subdir_under_COMFY_DIR/models or absolute
    while IFS= read -r line; do
        line="$(echo "${line}" | sed 's/#.*//;s/^[[:space:]]*//;s/[[:space:]]*$//')"
        [ -n "${line}" ] || continue
        IFS='|' read -r src repo file dest <<<"${line}"
        [ -n "${src}" ] && [ -n "${repo}" ] && [ -n "${file}" ] && [ -n "${dest}" ] \
            || die "bad STUDIO_FETCH_FILES line: ${line}"
        case "${dest}" in
            /*) ;;
            *) dest="${COMFY_DIR}/models/${dest}" ;;
        esac
        fetch_exact "${src}" "${repo}" "${file}" "${dest}"
    done <<<"${STUDIO_FETCH_FILES}"
}

# GGUF pull for the llm slot — anchored patterns from launch.sh (no fat globs).
fetch_gguf() {
    : "${MODEL_REPO:?MODEL_REPO required when llm is enabled}"
    : "${MODEL_QUANT:?MODEL_QUANT required when llm is enabled}"
    local target_dir="${MODELS_DIR}/$(basename "${MODEL_REPO}")"
    mkdir -p "${target_dir}"
    if find "${target_dir}" -maxdepth 2 -name '*.gguf' 2>/dev/null \
            | grep -iE "${MODEL_QUANT}" | grep -v mmproj | grep -q .; then
        log "gguf already present in ${target_dir}, skipping fetch"
        return 0
    fi
    log "fetching GGUF ${MODEL_REPO} quant=${MODEL_QUANT} via ${MODEL_SOURCE}"
    local includes=(
        "--include" "*-${MODEL_QUANT}.gguf"
        "--include" "*-${MODEL_QUANT}-00*-of-*.gguf"
        "--include" "*${MODEL_QUANT}/*.gguf"
    )
    local attempt
    for attempt in 1 2 3; do
        case "${MODEL_SOURCE}" in
            modelscope)
                modelscope download --model "${MODEL_REPO}" --local_dir "${target_dir}" \
                    "${includes[@]}" && return 0
                ;;
            hf)
                if [ -n "${HF_ENDPOINT:-}" ]; then
                    env -u HF_HUB_ENABLE_HF_TRANSFER \
                        hf download "${MODEL_REPO}" --local-dir "${target_dir}" "${includes[@]}" \
                        && return 0
                else
                    env HF_HUB_ENABLE_HF_TRANSFER=1 \
                        hf download "${MODEL_REPO}" --local-dir "${target_dir}" "${includes[@]}" \
                        && return 0
                fi
                ;;
            *) die "MODEL_SOURCE must be hf|modelscope" ;;
        esac
        log "  gguf attempt ${attempt}/3 failed; retry in 10 s"
        sleep 10
    done
    die "gguf download failed after 3 attempts"
}

start_llm() {
    local pidfile="${PID_DIR}/llm.pid"
    if is_alive "${pidfile}"; then
        log "llm already up (pid $(cat "${pidfile}"))"
        return 0
    fi
    [ -x /usr/local/bin/llama-server ] || die "llama-server missing from image"
    fetch_gguf
    local target_dir="${MODELS_DIR}/$(basename "${MODEL_REPO}")"
    local model_file
    model_file="$(find "${target_dir}" -maxdepth 2 -name "*.gguf" \
        | grep -iE "${MODEL_QUANT}" | grep -v mmproj | sort | head -n1 || true)"
    [ -n "${model_file}" ] || die "no .gguf matching '${MODEL_QUANT}' in ${target_dir}"

    local CTX="${CTX:-65536}"
    local KV_TYPE="${KV_TYPE:-q8_0}"
    local MODE="${MODE:-thinking}"
    local N_GPU_LAYERS="${N_GPU_LAYERS:-999}"
    local PARALLEL="${PARALLEL:-1}"
    local FLASH_ATTN="${FLASH_ATTN:-auto}"
    local EXTRA_ARGS="${EXTRA_ARGS:-}"
    local SAMPLE_ARGS="" TPL_KW="" TPL_KW_JSON=""
    case "${MODE}" in
        thinking)
            SAMPLE_ARGS="--temp 1.0 --top-p 0.95 --top-k 20 --min-p 0.0 --presence-penalty 1.5"
            ;;
        coding)
            SAMPLE_ARGS="--temp 0.6 --top-p 0.95 --top-k 20 --min-p 0.0 --presence-penalty 0.0"
            ;;
        nonthinking)
            SAMPLE_ARGS="--temp 0.7 --top-p 0.80 --top-k 20 --min-p 0.0 --presence-penalty 1.5"
            TPL_KW='--chat-template-kwargs'
            TPL_KW_JSON='{"enable_thinking":false}'
            ;;
        *) die "MODE must be thinking|coding|nonthinking" ;;
    esac
    local FA_ARGS=""
    case "${FLASH_ATTN}" in
        auto) ;;
        on|off) FA_ARGS="--flash-attn ${FLASH_ATTN}" ;;
        bare) FA_ARGS="--flash-attn" ;;
        *) die "FLASH_ATTN must be auto|on|off|bare" ;;
    esac

    log "==> starting llm on ${HOST}:${LLM_PORT}  model=${model_file}"
    # shellcheck disable=SC2086
    nohup llama-server \
        --model "${model_file}" \
        --host "${HOST}" --port "${LLM_PORT}" \
        --ctx-size "${CTX}" \
        --cache-type-k "${KV_TYPE}" --cache-type-v "${KV_TYPE}" \
        --n-gpu-layers "${N_GPU_LAYERS}" \
        --parallel "${PARALLEL}" \
        --jinja --metrics \
        ${FA_ARGS} \
        ${SAMPLE_ARGS} ${TPL_KW} ${TPL_KW_JSON:+"$TPL_KW_JSON"} ${EXTRA_ARGS} \
        > /var/log/studio-llm.log 2>&1 &
    echo $! > "${pidfile}"
    log "    llm pid $(cat "${pidfile}")  log=/var/log/studio-llm.log"
}

start_comfy() {
    local name="$1" port="$2" cuda="$3"
    local pidfile="${PID_DIR}/${name}.pid"
    if is_alive "${pidfile}"; then
        log "${name} already up (pid $(cat "${pidfile}"))"
        return 0
    fi
    [ -x "${COMFY_VENV}/bin/python" ] || die "comfy venv missing"
    [ -d "${COMFY_DIR}" ] || die "ComfyUI tree missing at ${COMFY_DIR}"

    log "==> starting ${name} on ${HOST}:${port}  CUDA_VISIBLE_DEVICES=${cuda}"
    # One process per card. --disable-auto-launch keeps headless.
    # Listen on loopback only — tunnel is the boundary (S5 / S11).
    nohup env CUDA_VISIBLE_DEVICES="${cuda}" \
        "${COMFY_VENV}/bin/python" "${COMFY_DIR}/main.py" \
        --listen "${HOST}" \
        --port "${port}" \
        --disable-auto-launch \
        > "/var/log/studio-${name}.log" 2>&1 &
    echo $! > "${pidfile}"
    log "    ${name} pid $(cat "${pidfile}")  log=/var/log/studio-${name}.log"
}

# ── main ──────────────────────────────────────────────────────────────────────

log "studio.sh start  services=[${STUDIO_SERVICES}]  host=${HOST}"
log "  comfy pin: $(cat "${COMFY_DIR}/.studio-pin" 2>/dev/null || echo unknown)"
nvidia-smi --query-gpu=index,name,memory.total,memory.free --format=csv,noheader 2>/dev/null \
    || log "  nvidia-smi unavailable (ok during image build smoke)"

# Weight pulls first when declared — warm wake with files present is a no-op.
if [ "${STUDIO_PULL_MODELS:-}" = "1" ] || [ -n "${STUDIO_FETCH_FILES:-}" ] || [ -n "${MODEL_REPO:-}" ]; then
    fetch_declared_files
fi

for svc in ${STUDIO_SERVICES}; do
    case "${svc}" in
        llm)   start_llm ;;
        video) start_comfy video "${VIDEO_PORT}" "${VIDEO_CUDA}" ;;
        image) start_comfy image "${IMAGE_PORT}" "${IMAGE_CUDA}" ;;
        *)     die "unknown service in STUDIO_SERVICES: ${svc}" ;;
    esac
done

log "studio posture requested; pids:"
for f in "${PID_DIR}"/*.pid; do
    [ -f "${f}" ] || continue
    log "  $(basename "${f}" .pid)=$(cat "${f}") alive=$(kill -0 "$(cat "${f}")" 2>/dev/null && echo yes || echo no)"
done
log "done (readiness is probed by ApexRouter svc_prober through the tunnels — S8)"
# Stay alive only if we are PID 1-ish under onstart backgrounding; onstart already &s us.
exit 0
