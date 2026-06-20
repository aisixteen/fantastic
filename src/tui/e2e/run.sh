#!/bin/sh
# Headful TUI e2e runner. Extracts the ```script fenced block from a scenario
# .md, runs the PTY screenshot harness against the built `fantastic` binary, and
# writes text "screenshots" (frames) to an output dir for an LLM to inspect.
#
#   sh run.sh <scenario.md> [out_dir]            (default out: /tmp/ft-e2e/<scenario>)
#   FT_COLS=120 FT_ROWS=34 sh run.sh attract.md  (override the PTY size)
#
# See headful.e2e.md for the full operator (LLM) instructions.
set -eu

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
SRC=$(CDPATH= cd -- "$HERE/../.." && pwd)          # …/src
BIN="$SRC/target/debug/fantastic"

[ $# -ge 1 ] || { echo "usage: sh run.sh <scenario.md> [out_dir]" >&2; exit 2; }
SCENARIO="$1"
[ -f "$SCENARIO" ] || SCENARIO="$HERE/$1"           # allow a bare name in this dir
[ -f "$SCENARIO" ] || { echo "no scenario: $1" >&2; exit 2; }
OUT="${2:-/tmp/ft-e2e/$(basename "$SCENARIO" .md)}"
mkdir -p "$OUT"

# Build the binary if needed.
[ -x "$BIN" ] || ( cd "$SRC" && cargo build -q )

# Extract the first ```script … ``` fenced block into a runnable script file.
awk '/^```script$/{f=1;next} /^```/{if(f){exit}} f' "$SCENARIO" > "$OUT/_script.txt"
[ -s "$OUT/_script.txt" ] || { echo "scenario has no \`\`\`script block: $SCENARIO" >&2; exit 2; }

echo "== $(basename "$SCENARIO") → $OUT =="
( cd "$SRC" && cargo run -q --example screenshot -p fantastic-term -- "$BIN" "$OUT/_script.txt" "$OUT" )
echo "frames:"
ls -1 "$OUT"/*.txt | grep -v '_script.txt' | sed 's/^/  /'
