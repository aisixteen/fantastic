# This is a Fantastic kernel.

A tree of agents. One primitive: `send(target_id, payload) -> reply`.
Every agent answers `{"type":"reflect"}` — identity, verbs, state.

## First move

`fantastic --help` — the CLI cheatsheet (invocation forms, the
daemon-lock rule). Then:

`fantastic reflect return_readme=true` — your one-shot bootstrap:
the live agent tree + available bundles + this readme, in one call.
Read-only and lock-free, so it works whether or not a daemon is
running here. Start there.

## Reach this kernel

Check `.fantastic/lock.json` first — it holds `{pid}`. One kernel per
dir, so the two paths are mutually exclusive:

**A daemon IS running** (lock.json has a live pid). The one-shot CLI
is locked out — you MUST go through the daemon's surface. Find it in
the tree: a `web.tools` node carries the HTTP `port`; its children
are the surfaces.
  - `web_rest.tools` child → `POST http://localhost:<port>/<rest_id>/<target>`
    body=`{"type":"<verb>",...}`; or browser-pastable
    `GET http://localhost:<port>/<rest_id>/_reflect[/<target>][?readme=1]`
  - `web_ws.tools` child → `ws://localhost:<port>/<agent>/ws`
    (frames: `{"type":"call","target":..,"payload":..,"id":..}`)
  To get the tree itself: `GET http://localhost:<port>/<rest_id>/_reflect`.

**No daemon running** (no live pid). The one-shot CLI works — each
call spawns a fresh kernel, reads disk, dispatches, exits:
  - `fantastic reflect` — the agent tree + available_bundles
  - `fantastic <id> <verb> [k=v ...]` — call any verb on any agent
  (one-shots don't see live process-memory state — they're disk-only.)

**A daemon is running but there's no `web.tools` node** → it has no
HTTP surface and one-shots are locked out. Tell the user to add one
(don't guess a port yourself):
    fantastic core create_agent handler_module=web.tools port=<N>
    fantastic <web_id> create_agent handler_module=web_ws.tools
    fantastic <web_id> create_agent handler_module=web_rest.tools
...then they restart the daemon.

## Per-agent readmes

Every agent carries its own `readme.md` (copied from its bundle at
creation). Reflect with the flag to get it:
    {"type":"reflect", "return_readme": true}
Default is false — reflect stays lean unless you ask. This file IS
the root agent's readme.

## Mental model

Agents are recursive — an agent can own children. `create_agent
handler_module=<bundle>.tools` spawns one (as a child of whatever
you call it on); `delete_agent` cascades depth-first.

A `canvas_webapp` is a spatial UI; its `canvas_backend` child holds
members — `add_agent handler_module=X` spawns a member, `list_members`
lists them, `remove_agent` cascades one out. To drive a project's
canvas: reflect the canvas_backend with `return_readme:true`, then
add/inspect/remove members.

## If you are a terminal inside a canvas

You may be `claude` running in a `terminal_backend` PTY that is a
member of some canvas. To find the canvas you live on: reflect
yourself, then walk `parent_id` up the tree —
`terminal_backend → terminal_webapp → canvas_backend → canvas_webapp`.
Once you have the `canvas_backend` id, you can `add_agent` siblings
next to yourself, `list_members`, or reshape the canvas you're in.
(If your own agent id isn't obvious, `fantastic reflect` shows the
whole tree — find the `terminal_backend` node whose PTY is yours.)

To learn ANY agent: reflect it with `return_readme:true`.
