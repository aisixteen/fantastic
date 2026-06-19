#!/bin/sh
# Universal entrypoint — pick a runtime at launch, then exec the kernel as the
# (tini-supervised) daemon. It performs NO agent autocreation: the kernel boots
# exactly what the bind-mounted workdir already contains. Composition is the
# operator's job (a project that carries its own web stack, or an LLM driving the
# kernel) — see the note printed when no web host is found.
#
#   FANTASTIC_RUNTIME = python (default) | rust | swift — all three are full
#                       runtimes that serve HTTP + WS on Linux (swift's web is
#                       swift-nio, at parity with python/rust)
#   FANTASTIC_PORT    = suggested port for a web you compose (default 8088); used
#                       only in the "compose a web" hint, not bound by the entrypoint
#   FANTASTIC_WORKDIR = /work (bind-mounted; holds .fantastic/lock.json)
#
# On EVERY runtime the web's `/` is the agent-tree index — the kernel's own web
# owns no `fs` surface. A custom landing (e.g. the all-readmes head page baked at
# /opt/fantastic/head/index.html) is served the one gated way: create a read-only
# `file_bridge` over that dir and let `/<file_bridge>/file/index.html` serve it.
# There is no `FANTASTIC_WEB_INDEX` env back-channel any more (deleted to match
# Python — no host reads a landing file off disk behind the gate). No web →
# nothing serves.
#
# The frontend is the prebuilt zip at $FANTASTIC_JS_KERNEL_ZIP — the image only
# CARRIES it (not a CDN); copy bundle.min.js out of it into your project and serve
# it via a file agent (the copy-from-zip convention).
#
# Swift is a FULL runtime here, at parity with python/rust: its web server is
# swift-nio (cross-platform), so FANTASTIC_RUNTIME=swift serves HTTP on Linux
# just the same — plus the NIO ws_bridge / relay_connector for cross-kernel
# comms (e.g. dialling the fantastic_relay router). (The macOS-only bundles — terminal,
# runners, FoundationModels, apple_kvs — still compile to no-ops on Linux.)
set -eu

RUNTIME="${FANTASTIC_RUNTIME:-python}"
WORKDIR="${FANTASTIC_WORKDIR:-/work}"
# Bind :8088 INSIDE the container by default — an unprivileged port, so uid 1000
# binds it with no caps / no root. The host maps it wherever; the documented
# default is `-p 8088:8088` (same in/out).
PORT="${FANTASTIC_PORT:-8088}"
export FANTASTIC_JS_KERNEL_ZIP="${FANTASTIC_JS_KERNEL_ZIP:-/opt/fantastic/js_kernel.zip}"

PY="${FANTASTIC_PY:-/opt/fantastic/venv/bin/fantastic_kernel}"
RUST="${FANTASTIC_RUST:-/opt/fantastic/bin/fantastic-rust}"
SWIFT="${FANTASTIC_SWIFT:-/opt/fantastic/bin/fantastic-swift}"

# Runtime → (binary, root id). python root = kernel_state; rust + swift root = core.
case "$RUNTIME" in
  python) BIN="$PY";    ROOT="kernel_state" ;;
  rust)   BIN="$RUST";  ROOT="core" ;;
  swift)  BIN="$SWIFT"; ROOT="core" ;;
  *) echo "entrypoint: unknown FANTASTIC_RUNTIME='$RUNTIME' (use python|rust|swift)" >&2
     exit 2 ;;
esac

cd "$WORKDIR"

# Preflight: $WORKDIR must be writable by THIS user (the container's uid). When
# you bind-mount your OWN folder, a rootless container's uid (1000) may not own
# it, so the kernel couldn't write .fantastic/. Fail with the fix, not a cryptic
# mkdir error. (On podman/docker for macOS the VM maps ownership automatically,
# so this just passes.)
if ! mkdir -p "$WORKDIR/.fantastic" 2>/dev/null || ! : >"$WORKDIR/.fantastic/.wtest" 2>/dev/null; then
  echo "entrypoint: FATAL — $WORKDIR is not writable by uid $(id -u) (the container user)." >&2
  echo "  You bind-mounted a host folder the container can't write. Grant access:" >&2
  echo "    podman:  add  --userns=keep-id        (map your host user into the container)" >&2
  echo "    docker:  add  -u \$(id -u):\$(id -g)     (run as your host uid)" >&2
  exit 1
fi
rm -f "$WORKDIR/.fantastic/.wtest" 2>/dev/null || true

# NO AGENT AUTOCREATION. Composition is the operator's explicit act (the
# substrate's no-architectural-automation rule: the only autoagent is the
# loader). The kernel boots whatever the bind-mounted workdir already contains —
# a real project carries its own web stack (+ a frontend bundle copied from
# $FANTASTIC_JS_KERNEL_ZIP per the copy-from-zip convention); a fresh/empty
# workdir serves nothing until you (or an LLM driving the kernel) compose one.
# We only DETECT a web host (by handler_module, so a suffixed id like web_ab12cd
# counts) to print a helpful note — we create nothing.
has_web() { grep -rqs '"handler_module"[[:space:]]*:[[:space:]]*"web.tools"' .fantastic/agents 2>/dev/null; }
if ! has_web; then
  echo "entrypoint: no web agent in $WORKDIR/.fantastic — this image composes nothing." >&2
  echo "  Mount a project that carries its own web stack, or compose one explicitly:" >&2
  echo "    $BIN $ROOT create_agent handler_module=web.tools id=web port=$PORT" >&2
  echo "    $BIN web create_agent handler_module=web_ws.tools id=web_ws" >&2
  echo "    $BIN web create_agent handler_module=web_rest.tools id=rest" >&2
  echo "  (without a web host the kernel boots, has nothing to serve, and exits.)" >&2
fi

echo "entrypoint: exec $RUNTIME kernel — workdir $WORKDIR (binds the port of the web you composed)"
# exec → the kernel becomes the process tini supervises; SIGTERM reaches it for
# graceful shutdown (release .fantastic/lock.json, drain uvicorn/axum).
exec "$BIN"
