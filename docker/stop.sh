#!/usr/bin/env bash
# Stop llama-server by its recorded pid — NEVER `pkill -f llama-server`, which over ssh
# matches your own command line and kills the session (GARDEN-RUNS, the pkill trilogy).
set -euo pipefail
PIDFILE=/run/llama-server.pid
if [ ! -f "${PIDFILE}" ]; then
    echo "no ${PIDFILE}; nothing recorded as running" >&2
    exit 1
fi
PID="$(cat "${PIDFILE}")"
if kill -0 "${PID}" 2>/dev/null; then
    kill "${PID}"
    echo "sent SIGTERM to llama-server (pid ${PID})"
else
    echo "pid ${PID} is not running; removing the stale pidfile" >&2
fi
rm -f "${PIDFILE}"
