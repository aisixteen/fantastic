#!/usr/bin/env bash
# Tear down a daemon booted by boot_bare_host.sh.
#   usage: integration_tests/py_ts/e2e/teardown_host.sh <PID> <TMP>
set -uo pipefail
PID="${1:-}"
TMP="${2:-}"
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
[ -n "$PID" ] && kill "$PID" 2>/dev/null && echo "killed daemon $PID" || echo "no live pid $PID"
[ -n "$TMP" ] && rm -rf "$TMP" && echo "removed $TMP"
rm -f "$REPO/ts/dist/_e2e_canvas.html" 2>/dev/null || true
echo "torn down"
