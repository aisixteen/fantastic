#!/usr/bin/env bash
set -euo pipefail
cd /workdir

# Seed the canvas stack on first boot. Idempotent: subsequent
# starts find `.fantastic/agent.json` and skip seeding.
if [ ! -f .fantastic/agent.json ]; then
  # web + its call-surface children (ws/rest mount routes on web's app).
  fantastic core create_agent handler_module=web.tools port=8080 >/dev/null
  WEB_ID=$(ls .fantastic/agents | grep '^web_' | head -1)
  fantastic "$WEB_ID" create_agent handler_module=web_ws.tools   >/dev/null
  fantastic "$WEB_ID" create_agent handler_module=web_rest.tools >/dev/null
  # canvas_webapp auto-spawns its canvas_backend child on first boot
  # (per the substrate's _boot hook). One create_agent gives us the
  # canvas + the backend it needs, no manual wiring.
  fantastic core create_agent handler_module=canvas_webapp.tools >/dev/null
fi

# `fantastic` blocks on the web agent (uvicorn task) and listens for
# SIGTERM/SIGINT/SIGHUP via the graceful-shutdown path (PR #14).
# `podman stop` sends SIGTERM → graceful walk → clean exit.
exec fantastic
