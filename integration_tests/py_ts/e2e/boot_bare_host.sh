#!/usr/bin/env bash
# Boot a BARE fantastic substrate daemon for the emergence runbook
# (integration_tests/py_ts/e2e/RUN.md):
#   web + web_ws + web_rest + web_loader + a seeded `canvas` + the canvas page
#   (the canvas app's `canvas.zip`) served at /canvas_page/file/index.html.
#   NO workflow agents (exec_bridge / scheduler / AI / panels) — the spawned
#   builder agent creates all of those from zero, through the REST door only.
#   Prints connection info + the daemon PID; leaves it running.
#
#   usage: integration_tests/py_ts/e2e/boot_bare_host.sh [PORT]   (default 8930)
#   teardown: integration_tests/py_ts/e2e/teardown_host.sh <PID> <TMP>
set -euo pipefail

PORT="${1:-8930}"
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
FAN="$REPO/src/lib/python/.venv/bin/fantastic_kernel"
CANVAS="$REPO/src/tui/web/canvas"

[ -x "$FAN" ] || { echo "ERR: no fantastic bin at $FAN — run 'uv sync' in src/lib/python/" >&2; exit 1; }
sh "$CANVAS/scripts/pack.sh" >/dev/null

TMP="$(mktemp -d "${TMPDIR:-/tmp}/ft-e2e-XXXXXX")"
# so the daemon's cwd-relative _load_dotenv finds ANTHROPIC_API_KEY
[ -f "$REPO/.env" ] && cp "$REPO/.env" "$TMP/.env"

# the canvas page: the packed bundle beside a one-tag page, in the workdir
# (file_bridge clamps every root inside the running dir)
mkdir -p "$TMP/canvas_page"
unzip -o -q "$CANVAS/dist/canvas.zip" bundle.min.js -d "$TMP/canvas_page"
cat > "$TMP/canvas_page/index.html" <<'HTML'
<!doctype html><html><head><meta charset="utf-8"><title>fantastic · e2e canvas</title></head>
<body><script type="module" src="/canvas_page/file/bundle.min.js"></script></body></html>
HTML

idof() { grep -oE "\"id\": *\"$1_[0-9a-f]+\"" | head -1 | grep -oE "$1_[0-9a-f]+"; }
k() { "$FAN" kernel "$@"; }   # kernel verbs: the root answers, `parent_id` names the owner

cd "$TMP"
# The record provider FIRST (no fallback): the root persists records ONLY through
# a discovered `file_bridge` rooted at `.fantastic` — without it every one-shot
# create below stays in RAM and the next one-shot can't see it.
k create_agent parent_id=kernel_state handler_module=file_bridge.tools id=store \
    root=.fantastic allow='["."]' ingress_rule=allow_all_ingress >/dev/null
# The memory store: a started kernel (the daemon) refuses to run without one.
k create_agent parent_id=kernel_state handler_module=ram_store.tools id=memory \
    operator=true 'admins=["kernel"]' >/dev/null
k update_agent id=kernel_state memory_store_id=memory >/dev/null
WEB=$(k create_agent parent_id=kernel_state handler_module=web.tools "port=$PORT" | idof web)
[ -n "$WEB" ] || { echo "ERR: failed to create web agent" >&2; exit 1; }
# web_ws / web_rest SEAL by default — open them so the browser + the REST-only
# builder can drive the kernel (the inbound-call edge is gated; absent ⇒ deny).
WS=$(k create_agent "parent_id=$WEB" handler_module=web_ws.tools ingress_rule=allow_all_ingress | idof web_ws)
REST=$(k create_agent "parent_id=$WEB" handler_module=web_rest.tools ingress_rule=allow_all_ingress | idof web_rest)
# The page's loader is a CHILD OF THE WS LEG: the page's record verbs arrive as
# the leg, and the record-owner guard admits only records in its own subtree.
k create_agent "parent_id=$WS" handler_module=kernel_state.tools \
    id=web_loader root=.fantastic/web watch=false >/dev/null
"$FAN" web_loader persist_record \
    record='{"id":"canvas","handler_module":"canvas.ts","display_name":"canvas"}' >/dev/null
k create_agent "parent_id=$WEB" handler_module=file_bridge.tools id=canvas_page \
    root=canvas_page allow='["."]' readonly=true ingress_rule=allow_all_ingress >/dev/null
# THE LEGS' RIGHTS, both halves: each reaches the root (privileged verbs named —
# never under `*`), web and the memory store; the root accepts both.
BASE='"$self":"*","$parent":"*","$children":"*"'
for LEG in "$WS" "$REST"; do
  k update_agent "id=$LEG" \
    "can_call={$BASE,\"kernel_state\":[\"*\",\"create_agent\",\"update_agent\",\"delete_agent\"],\"$WEB\":\"*\",\"memory\":\"*\"}" >/dev/null
done
k update_agent id=kernel_state "callable_by={$BASE,\"$WS\":\"*\",\"$REST\":\"*\"}" >/dev/null

nohup "$FAN" >"$TMP/daemon.log" 2>&1 &
PID=$!
disown || true

up=""
for _ in $(seq 1 80); do
  if curl -sf "http://127.0.0.1:$PORT/_assets/favicon.png" >/dev/null 2>&1; then up="yes"; break; fi
  sleep 0.25
done
[ -n "$up" ] || { echo "ERR: daemon did not come up on :$PORT (see $TMP/daemon.log)" >&2; kill "$PID" 2>/dev/null || true; exit 1; }

echo "TMP=$TMP"
echo "PID=$PID"
echo "PORT=$PORT"
echo "WEB=$WEB"
echo "WS=$WS"
echo "REST=$REST"
echo "REST_URL=http://127.0.0.1:$PORT/$REST"
echo "CANVAS_URL=http://127.0.0.1:$PORT/canvas_page/file/index.html"
