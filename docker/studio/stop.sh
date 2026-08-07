#!/usr/bin/env bash
# Stop every studio service by its recorded pid under /run/studio/.
#
# NEVER `pkill -f llama-server` / `pkill -f ComfyUI` over ssh: the pattern matches
# YOUR OWN command line and kills the session (GARDEN-RUNS, the pkill trilogy).
set -euo pipefail

PID_DIR="${STUDIO_PID_DIR:-/run/studio}"
SERVICES="${1:-llm video image}"

if [ ! -d "${PID_DIR}" ]; then
    echo "no ${PID_DIR}; nothing recorded as running" >&2
    exit 1
fi

status=0
for name in ${SERVICES}; do
    pidfile="${PID_DIR}/${name}.pid"
    if [ ! -f "${pidfile}" ]; then
        echo "no ${pidfile}; ${name} not recorded" >&2
        continue
    fi
    pid="$(cat "${pidfile}")"
    if kill -0 "${pid}" 2>/dev/null; then
        kill "${pid}" || true
        # brief wait, then escalate
        for _ in 1 2 3 4 5; do
            kill -0 "${pid}" 2>/dev/null || break
            sleep 0.4
        done
        if kill -0 "${pid}" 2>/dev/null; then
            kill -9 "${pid}" 2>/dev/null || true
            echo "sent SIGKILL to ${name} (pid ${pid})"
        else
            echo "sent SIGTERM to ${name} (pid ${pid})"
        fi
    else
        echo "pid ${pid} (${name}) is not running; removing the stale pidfile" >&2
        status=1
    fi
    rm -f "${pidfile}"
done
exit "${status}"
