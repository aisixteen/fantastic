#!/usr/bin/env bash
# Tear down a daemon booted by boot_bare_host.sh.
#   usage: integration_tests/py_ts/e2e/teardown_host.sh <PID> <TMP>
set -uo pipefail
PID="${1:-}"
TMP="${2:-}"
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
if [ -n "$PID" ] && kill -0 "$PID" 2>/dev/null; then
  # TERM, wait for a real exit, escalate to KILL — a fired signal is not a dead
  # process (the daemon previously survived a bare `kill` and kept the port).
  kill "$PID" 2>/dev/null
  i=0
  while [ $i -lt 20 ] && kill -0 "$PID" 2>/dev/null; do sleep 0.25; i=$((i+1)); done
  if kill -0 "$PID" 2>/dev/null; then
    kill -9 "$PID" 2>/dev/null
    i=0
    while [ $i -lt 8 ] && kill -0 "$PID" 2>/dev/null; do sleep 0.25; i=$((i+1)); done
  fi
  if kill -0 "$PID" 2>/dev/null; then
    echo "ERR: daemon $PID still alive after SIGKILL" >&2
    exit 1
  fi
  echo "killed daemon $PID"
else
  echo "no live pid $PID"
fi
# Remove the workdir only once the daemon is confirmed dead (it writes there).
[ -n "$TMP" ] && rm -rf "$TMP" && echo "removed $TMP"
rm -f "$REPO/src/lib/ts/dist/_e2e_canvas.html" 2>/dev/null || true
echo "torn down"
