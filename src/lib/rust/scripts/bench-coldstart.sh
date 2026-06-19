#!/usr/bin/env bash
# bench-coldstart.sh — pin the kernel's cold-start budget.
#
# Three measurements, each with a fail threshold:
#
#   1. virgin-dir reflect       (target < 50 ms,  ceiling 100 ms)
#   2. 18-agent hydrate reflect (target < 100 ms, ceiling 200 ms)
#   3. boot-to-listening        (target < 200 ms, ceiling 400 ms)
#
# Run locally with strict targets; CI runs with the relaxed ceilings
# (2× the targets) via FANTASTIC_BENCH_RELAXED=1 to absorb cloud-runner
# variance.
#
# Environment:
#   FANTASTIC_RUST          path to the release binary
#                            (default: src/lib/rust/target/release/fantastic_kernel)
#   FANTASTIC_BENCH_RELAXED 1 → use 2× ceilings (CI default)
#   FANTASTIC_BENCH_PORT    port for boot-to-listening (default 18282)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../../.." && pwd)"
RUST_BIN="${FANTASTIC_RUST:-$REPO_ROOT/src/lib/rust/target/release/fantastic_kernel}"
PORT="${FANTASTIC_BENCH_PORT:-18282}"

if [ ! -x "$RUST_BIN" ]; then
    echo "[bench] building release binary first..."
    (cd "$REPO_ROOT/src/lib/rust" && cargo build --release --bin fantastic_kernel >/dev/null 2>&1)
fi
test -x "$RUST_BIN" || { echo "[bench] rust binary missing at $RUST_BIN"; exit 2; }

if [ "${FANTASTIC_BENCH_RELAXED:-0}" = "1" ]; then
    TARGET_VIRGIN_MS=100
    TARGET_HYDRATE_MS=200
    TARGET_LISTEN_MS=400
    echo "[bench] mode: RELAXED (CI ceilings, 2× local targets)"
else
    TARGET_VIRGIN_MS=50
    TARGET_HYDRATE_MS=100
    TARGET_LISTEN_MS=200
    echo "[bench] mode: STRICT (local targets)"
fi

# Portable epoch-millis. macOS `date` doesn't support %N, so use python.
now_ms() {
    python3 -c 'import time; print(int(time.time()*1000))'
}

pass() { printf "  ✓ %s   %4d ms (target %d ms)\n" "$1" "$2" "$3"; }
fail() { printf "  ✗ %s   %4d ms exceeds ceiling %d ms\n" "$1" "$2" "$3"; FAILED=1; }

FAILED=0

# ── 1. Virgin-dir reflect ───────────────────────────────────────────
{
    WORK="$(mktemp -d)"
    trap 'rm -rf "$WORK"' EXIT
    cd "$WORK"
    T0=$(now_ms)
    "$RUST_BIN" reflect >/dev/null
    T1=$(now_ms)
    ELAPSED=$(( T1 - T0 ))
    if [ "$ELAPSED" -le "$TARGET_VIRGIN_MS" ]; then
        pass "virgin-dir reflect       " "$ELAPSED" "$TARGET_VIRGIN_MS"
    else
        fail "virgin-dir reflect       " "$ELAPSED" "$TARGET_VIRGIN_MS"
    fi
    cd "$REPO_ROOT"
    rm -rf "$WORK"
    trap - EXIT
}

# ── 2. 18-agent hydrate reflect ─────────────────────────────────────
{
    WORK="$(mktemp -d)"
    trap 'rm -rf "$WORK"' EXIT
    cd "$WORK"
    # Stage 18 file agents (matches typical Python tree size).
    for i in $(seq 1 18); do
        "$RUST_BIN" core create_agent handler_module=file.tools "id=ff_$i" root=/tmp >/dev/null
    done
    T0=$(now_ms)
    "$RUST_BIN" reflect >/dev/null
    T1=$(now_ms)
    ELAPSED=$(( T1 - T0 ))
    if [ "$ELAPSED" -le "$TARGET_HYDRATE_MS" ]; then
        pass "18-agent reflect         " "$ELAPSED" "$TARGET_HYDRATE_MS"
    else
        fail "18-agent reflect         " "$ELAPSED" "$TARGET_HYDRATE_MS"
    fi
    cd "$REPO_ROOT"
    rm -rf "$WORK"
    trap - EXIT
}

# ── 3. Boot-to-listening ─────────────────────────────────────────────
{
    WORK="$(mktemp -d)"
    trap '[ -n "${DPID:-}" ] && kill "$DPID" 2>/dev/null; wait "${DPID:-0}" 2>/dev/null || true; rm -rf "$WORK"' EXIT
    cd "$WORK"
    "$RUST_BIN" core create_agent handler_module=web.tools id=w port="$PORT" >/dev/null
    T0=$(now_ms)
    "$RUST_BIN" >/tmp/bench-daemon.log 2>&1 &
    DPID=$!
    # Poll until the listener answers. Add a hard cutoff so we don't
    # spin forever if it never comes up.
    DEADLINE=$(( $(now_ms) + 5000 ))
    while [ "$(now_ms)" -lt "$DEADLINE" ]; do
        if curl -sf -m 1 "http://localhost:$PORT/" >/dev/null 2>&1; then
            break
        fi
        # Tight loop — sleep would inflate the measurement.
    done
    T1=$(now_ms)
    ELAPSED=$(( T1 - T0 ))
    if [ "$ELAPSED" -le "$TARGET_LISTEN_MS" ]; then
        pass "boot-to-listening        " "$ELAPSED" "$TARGET_LISTEN_MS"
    else
        fail "boot-to-listening        " "$ELAPSED" "$TARGET_LISTEN_MS"
    fi
    kill "$DPID" 2>/dev/null || true
    wait "$DPID" 2>/dev/null || true
    cd "$REPO_ROOT"
    rm -rf "$WORK"
    trap - EXIT
}

echo
if [ "$FAILED" -eq 0 ]; then
    echo "[bench] ✓ all cold-start measurements within budget"
    exit 0
else
    echo "[bench] ✗ one or more measurements exceeded their ceiling"
    exit 1
fi
