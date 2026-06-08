#!/bin/sh
# Bounded swift-test runner — swift-testing is notoriously hang-prone, so run it
# with progress polls and a HARD wall-clock cap, killing the test process if it
# stalls instead of letting it wedge a session forever.
#
#   sh scripts/bounded-swift-test.sh ['<filter>'] [cap_seconds] [poll_seconds]
#
# Defaults: no filter (whole suite), cap 120s, poll 15s. The cap bounds EXECUTION
# only — tests are built first in a separate, unbounded step (a cold NIOSSL build
# can exceed the run cap). Exit: 0 pass, 1 fail, 124 timed-out (killed), 2 build.
set -u

FILTER="${1:-}"
CAP="${2:-120}"
POLL="${3:-15}"
LOG="${TMPDIR:-/tmp}/bounded-swift-test.$$.log"

cd "$(dirname "$0")/.." || exit 2

echo "[build] swift build --build-tests (unbounded)"
if ! swift build --build-tests >"$LOG.build" 2>&1; then
    echo "[build] FAILED:"
    tail -40 "$LOG.build"
    exit 2
fi
echo "[build] ok"

set --
[ -n "$FILTER" ] && set -- --filter "$FILTER"

: >"$LOG"
echo "[run] swift test ${FILTER:+--filter $FILTER}  (cap ${CAP}s, poll ${POLL}s)"
swift test "$@" >"$LOG" 2>&1 &
PID=$!

elapsed=0
status="HUNG"
while [ "$elapsed" -lt "$CAP" ]; do
    if ! kill -0 "$PID" 2>/dev/null; then
        wait "$PID"
        status="exit $?"
        break
    fi
    sleep "$POLL"
    elapsed=$((elapsed + POLL))
    echo "--- poll ${elapsed}s/${CAP}s ---"
    tail -4 "$LOG" 2>/dev/null
done

if kill -0 "$PID" 2>/dev/null; then
    echo "[timeout] ${CAP}s exceeded — killing the hung swift-testing process"
    kill -9 "$PID" 2>/dev/null
    pkill -9 -f FantasticKernelPackageTests 2>/dev/null
    pkill -9 -f swiftpm-testing-helper 2>/dev/null
    status="TIMEOUT"
fi

echo "=== swift test output ==="
cat "$LOG"
echo "=== status: $status ==="
case "$status" in
"exit 0") exit 0 ;;
TIMEOUT) exit 124 ;;
*) exit 1 ;;
esac
