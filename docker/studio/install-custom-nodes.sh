#!/usr/bin/env bash
# Install optional ComfyUI custom-node packs at image build time.
# Spec list: "owner/repo@ref,owner2/repo2@sha"  (empty = no-op).
set -euo pipefail

list="${1:-}"
if [ -z "${list}" ]; then
    echo "no CUSTOM_NODES requested"
    exit 0
fi

IFS=',' read -ra specs <<<"${list}"
for spec in "${specs[@]}"; do
    spec="$(echo "${spec}" | xargs)"
    [ -n "${spec}" ] || continue
    repo="${spec%@*}"
    ref="${spec#*@}"
    name="$(basename "${repo}")"
    dest="/opt/ComfyUI/custom_nodes/${name}"
    echo "installing custom node ${repo} @ ${ref} -> ${dest}"
    if ! git clone --depth 1 "https://github.com/${repo}.git" "${dest}" 2>/dev/null; then
        git clone "https://github.com/${repo}.git" "${dest}"
    fi
    if [ "${ref}" != "${repo}" ] && [ -n "${ref}" ]; then
        git -C "${dest}" fetch --depth 1 origin "${ref}" || true
        git -C "${dest}" checkout "${ref}" || true
    fi
    if [ -f "${dest}/requirements.txt" ]; then
        /opt/comfy-venv/bin/pip install --no-cache-dir -r "${dest}/requirements.txt" || true
    fi
done
