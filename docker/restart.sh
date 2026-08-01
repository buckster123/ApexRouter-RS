#!/usr/bin/env bash
# Patch-then-RELAUNCH, as one verb. `sed -i` on a running launch.sh does nothing to the
# running copy (bash holds the old inode), so the only way an edit takes effect is a
# clean stop and a fresh exec of the file as it now exists on disk (GARDEN-RUNS R2a).
set -euo pipefail
/app/stop.sh || true
sleep 2
nohup bash /app/launch.sh > /var/log/launch.log 2>&1 &
echo "relaunched; tail -f /var/log/launch.log"
