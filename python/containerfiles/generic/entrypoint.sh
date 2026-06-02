#!/usr/bin/env bash
set -euo pipefail
cd /workdir

# Seed the host stack on first boot. Idempotent: subsequent starts find
# `.fantastic/agent.json` and skip seeding.
if [ ! -f .fantastic/agent.json ]; then
  # web + its call-surface children (ws/rest mount routes on web's app).
  # The root agent IS the `fs_loader` (id="fs_loader") — there is no `core`.
  fantastic fs_loader create_agent handler_module=web.tools port=8080 >/dev/null
  WEB_ID=$(ls .fantastic/agents | grep '^web_' | head -1)
  fantastic "$WEB_ID" create_agent handler_module=web_ws.tools   >/dev/null
  fantastic "$WEB_ID" create_agent handler_module=web_rest.tools >/dev/null
  # This is a PURE host: data/compute/transport only. The UI (canvas
  # compositor + its view/content agents) is the TS FRONTEND kernel in the
  # repo's top-level `ts/` package — a federated peer over the WS bridge, NOT
  # host agents. The host knows nothing of it. To serve that frontend, build it
  # (`cd ts && npm run build` → ts/dist) and seed a GENERIC `file` agent rooted
  # at the build output, which serves the static mount page + ESM modules over
  # /<id>/file/<path> (see ts/SERVE.md):
  #   fantastic fs_loader create_agent handler_module=file.tools id=ts_dist root=/path/to/ts/dist
  # Wiring that into this image needs a node build step for ts/dist; tracked
  # as a follow-up.
fi

# `fantastic` blocks on the web agent (uvicorn task) and listens for
# SIGTERM/SIGINT/SIGHUP via the graceful-shutdown path (PR #14).
# `podman stop` sends SIGTERM → graceful walk → clean exit.
exec fantastic
