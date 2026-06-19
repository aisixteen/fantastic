#!/usr/bin/env bash
# Boot a BARE fantastic substrate daemon for the emergence runbook
# (integration_tests/py_ts/e2e/RUN.md):
#   web + web_ws + web_rest + web_loader + a seeded `canvas` + the built frontend
#   served at /ts_dist/file/<path>.  NO workflow agents (python_runtime / scheduler /
#   AI / panels) — the spawned builder agent creates all of those from zero, through
#   the REST door only.  Prints connection info + the daemon PID; leaves it running.
#
#   usage: integration_tests/py_ts/e2e/boot_bare_host.sh [PORT]   (default 8930)
#   teardown: integration_tests/py_ts/e2e/teardown_host.sh <PID> <TMP>
set -euo pipefail

PORT="${1:-8930}"
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
FAN="$REPO/src/lib/python/.venv/bin/fantastic_kernel"
DIST="$REPO/src/lib/ts/dist"

[ -x "$FAN" ] || { echo "ERR: no fantastic bin at $FAN — run 'uv sync' in python/" >&2; exit 1; }
[ -f "$DIST/main.js" ] || { echo "ERR: ts/dist not built — run 'npm run build' in ts/" >&2; exit 1; }

TMP="$(mktemp -d "${TMPDIR:-/tmp}/ft-e2e-XXXXXX")"
# so the daemon's cwd-relative _load_dotenv finds ANTHROPIC_KEY
[ -f "$REPO/.env" ] && cp "$REPO/.env" "$TMP/.env"

# a canvas page that boots the frontend kernel (mirrors _host.ts MOUNT_HTML)
cat > "$DIST/_e2e_canvas.html" <<'HTML'
<!doctype html><html><head><meta charset="utf-8"><title>fantastic · e2e canvas</title>
<link rel="stylesheet" href="/ts_dist/file/vendor/xterm.css">
<script type="importmap">{ "imports": {
  "three": "/ts_dist/file/vendor/three.module.js",
  "@xterm/xterm": "/ts_dist/file/vendor/xterm.js",
  "@xterm/addon-fit": "/ts_dist/file/vendor/addon-fit.js"
}}</script></head><body>
<script type="module" src="/ts_dist/file/main.js"></script></body></html>
HTML

idof() { grep -oE "\"id\": *\"$1_[0-9a-f]+\"" | head -1 | grep -oE "$1_[0-9a-f]+"; }

cd "$TMP"
# Persistence inversion (no fallback): the root persists records ONLY through a
# discovered `file_bridge` store. Wire it FIRST (rooted at `.fantastic`, opened)
# so the subsequent one-shot creates below survive across processes — without it
# every create stays in RAM and the next one-shot can't see it.
"$FAN" kernel_state create_agent handler_module=file_bridge.tools id=store \
    root=.fantastic ingress_rule=allow_all >/dev/null
WEB=$("$FAN" kernel_state create_agent handler_module=web.tools "port=$PORT" | idof web)
[ -n "$WEB" ] || { echo "ERR: failed to create web agent" >&2; exit 1; }
# web_ws / web_rest SEAL by default — open them so the browser + the REST-only
# builder can drive the kernel (the inbound-call edge is gated; absent ⇒ deny).
"$FAN" "$WEB" create_agent handler_module=web_ws.tools ingress_rule=allow_all >/dev/null
REST=$("$FAN" "$WEB" create_agent handler_module=web_rest.tools ingress_rule=allow_all | idof web_rest)
"$FAN" "$WEB" create_agent handler_module=kernel_state.tools \
    root=.fantastic/web watch=false alias=web_loader >/dev/null
"$FAN" web_loader persist_record \
    record='{"id":"canvas","handler_module":"canvas.ts","display_name":"canvas"}' >/dev/null
# file_bridge clamps every root inside the running dir (the running-dir law) and
# seals by default → copy the built dist INTO the workdir, root it relatively, open it.
cp -R "$DIST" "$TMP/ts_dist_src"
"$FAN" "$WEB" create_agent handler_module=file_bridge.tools id=ts_dist \
    root=ts_dist_src ingress_rule=allow_all >/dev/null

nohup "$FAN" >"$TMP/daemon.log" 2>&1 &
PID=$!
disown || true

up=""
for _ in $(seq 1 80); do
  if curl -sf "http://127.0.0.1:$PORT/" >/dev/null 2>&1; then up="yes"; break; fi
  sleep 0.25
done
[ -n "$up" ] || { echo "ERR: daemon did not come up on :$PORT (see $TMP/daemon.log)" >&2; kill "$PID" 2>/dev/null || true; exit 1; }

echo "TMP=$TMP"
echo "PID=$PID"
echo "PORT=$PORT"
echo "WEB=$WEB"
echo "REST=$REST"
echo "REST_URL=http://127.0.0.1:$PORT/$REST"
echo "CANVAS_URL=http://127.0.0.1:$PORT/ts_dist/file/_e2e_canvas.html"
