# This folder holds a live Aisixteen Fantastic kernel

A tree of agents; everything is one verb — `send(target, payload) → reply`.

## Is it running?
- `.fantastic/lock.yaml` names the pid of the kernel that holds this folder;
  `.fantastic/cli/mcp.sock` exists while it runs (a canvas, the terminal UI,
  or `--headless`). Only ONE kernel runs per folder: never start a second.
- Check from a shell (attaches, starts nothing):
  `<fantastic> send '{"target":"kernel","payload":{"type":"reflect","tree":"ids"}}'`
  where `<fantastic>` is the `command` in this project's `.mcp.json`.

## Connect (MCP)
- `.mcp.json` (project root) names the server `fantastic`; it attaches to the
  running kernel. No `mcp__fantastic__*` tools in your session? Run `/mcp` and
  approve / reconnect `fantastic` (a server added after your session started
  needs this). Until then, `<fantastic> send '<item json>'` reaches the same
  kernel from a shell.

## Look, then call
- `reflect` (target `kernel`, `tree: ids`) — the whole tree, the bundles, the peers.
- How to call any agent: send `kernel` `{type: docs, of: <id>}`.
- The CANVAS page is a second kernel: its agents are `<page guid>|<id>` (the
  guid is in reflect's peers). E.g. the background:
  send `<guid>|background` `{type: set_bg, bytes: <three.js source>}`.

## What you may edit
- `.fantastic/prompt.md` — every in-canvas AI's system block (`{tree}`,
  `{self}`, `{howto}` placeholders); a save is live on the AIs' next turn.
- `.fantastic/bundles/<name>/` — your agents' code; changes reload live.
- Never hand-edit `.fantastic/agents/**`: records change through `update_agent`
  while the kernel runs.
