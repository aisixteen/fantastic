#!/bin/sh
# Universal entrypoint — pick a runtime at launch, ensure a web agent on the
# requested port, then exec the kernel as the (tini-supervised) daemon.
#
#   FANTASTIC_RUNTIME = python (default) | rust | ts
#   FANTASTIC_PORT    = port bound 0.0.0.0:<port> INSIDE the container (default 8088)
#   FANTASTIC_WORKDIR = /work (bind-mounted; holds .fantastic/lock.json)
#   FANTASTIC_HEAD    = on (default) | off — serve the descriptive head page at `/`
#
# Every runtime SERVES the descriptive "head" page at `/` BY DEFAULT (it costs
# almost nothing and makes a fresh container self-explaining). The kernel under
# it stays fully reflectable/bridgeable (reflect / web_ws / web_rest) regardless.
# Set FANTASTIC_HEAD=off to drop the head and show the plain agent-tree index.
#
# The JS runtime is the prebuilt zip at $FANTASTIC_JS_KERNEL_ZIP — no engine
# runs it; it's discovered + served statically. Swift is not in this image.
set -eu

RUNTIME="${FANTASTIC_RUNTIME:-python}"
WORKDIR="${FANTASTIC_WORKDIR:-/work}"
# Bind :8088 INSIDE the container by default — an unprivileged port, so uid 1000
# binds it with no caps / no root. The host maps it wherever; the documented
# default is `-p 8088:8088` (same in/out).
PORT="${FANTASTIC_PORT:-8088}"
export FANTASTIC_JS_KERNEL_ZIP="${FANTASTIC_JS_KERNEL_ZIP:-/opt/fantastic/js_kernel.zip}"

# Head ON by default; a flag turns it OFF (never on). off/0/false/no all disable.
HEAD="${FANTASTIC_HEAD:-on}"
case "$HEAD" in
  off|0|false|no|OFF|FALSE|NO) : ;;   # head disabled → plain agent-tree index at /
  *) export FANTASTIC_WEB_INDEX="${FANTASTIC_WEB_INDEX:-/opt/fantastic/head/index.html}" ;;
esac

PY="${FANTASTIC_PY:-/opt/fantastic/venv/bin/fantastic}"
RUST="${FANTASTIC_RUST:-/opt/fantastic/bin/fantastic-rust}"

# Runtime → (binary, root id). python root = fs_loader; rust root = core.
# `ts` is hosted by the python kernel (no JS engine) + it serves the embedded
# frontend zip via a file agent so an LLM can discover + pull-revive it.
# `head` is accepted as a back-compat alias of `python` (the head is now on by
# default for every runtime, not a separate mode).
case "$RUNTIME" in
  python) BIN="$PY";   ROOT="fs_loader" ;;
  ts)     BIN="$PY";   ROOT="fs_loader" ;;
  head)   BIN="$PY";   ROOT="fs_loader"; RUNTIME=python ;;
  rust)   BIN="$RUST"; ROOT="core" ;;
  *) echo "entrypoint: unknown FANTASTIC_RUNTIME='$RUNTIME' (use python|rust|ts)" >&2
     exit 2 ;;
esac

cd "$WORKDIR"
mkdir -p "$WORKDIR/.fantastic"

have() { grep -rqs "\"id\"[[:space:]]*:[[:space:]]*\"$1\"" .fantastic/agents 2>/dev/null; }

# Compose the call surface (idempotent — the bind-mounted workdir persists across
# restarts, so this runs once). All created BEFORE the daemon boots them:
#   web     = HTTP host on $PORT (rendering + mounts children's routes)
#   web_ws  = the WebSocket verb surface Fantastic clients use to call verbs (/web/ws)
#   rest    = a REST diagnostic surface (POST /rest/<target> body=payload → reply)
if ! have web; then
  echo "entrypoint: composing web + web_ws + rest on port $PORT ($RUNTIME / $ROOT)"
  "$BIN" "$ROOT" create_agent handler_module=web.tools id=web "port=$PORT" >/dev/null
  "$BIN" web create_agent handler_module=web_ws.tools id=web_ws >/dev/null
  "$BIN" web create_agent handler_module=web_rest.tools id=rest >/dev/null
fi

# ts: also expose the embedded frontend zip via a generic file agent so it's
# HTTP-discoverable (GET /js_kernel/file/js_kernel.zip) + pull-revivable.
if [ "$RUNTIME" = ts ] && ! have js_kernel; then
  echo "entrypoint: ts — serving embedded $FANTASTIC_JS_KERNEL_ZIP via file agent 'js_kernel'"
  "$BIN" "$ROOT" create_agent handler_module=file.tools id=js_kernel root=/opt/fantastic >/dev/null || true
fi

echo "entrypoint: exec $RUNTIME kernel — bind 0.0.0.0:$PORT, workdir $WORKDIR"
# exec → the kernel becomes the process tini supervises; SIGTERM reaches it for
# graceful shutdown (release .fantastic/lock.json, drain uvicorn/axum).
exec "$BIN"
