> The browser canvas (`fantastic canvas`): a page kernel that views host agents as cells; its control-plane verbs.

# The canvas

`fantastic canvas [--port N]` runs the terminal UI and serves the canvas page
from this binary. The page is its OWN kernel (in the browser) bridged to the
host kernel through the socket leg `canvas_ws`. It holds no memory store and no
AI: every cell is a VIEW of a host agent (a terminal, an AI chat, a set-up
cell) or page-side content (html, gl).

## The root AI and your own programs

`fantastic [canvas] ai vllm -p PORT` (also `ai ollama -p PORT`, `ai responses
--model M`, `ai anthropic --model M`) puts the ROOT AI `ai` on that backend; on the canvas, any chat cell can
show it. What the server knows (model, window) is read from it.
`fantastic [canvas] run COMMAND [ARGS…]` runs your own program from `$PATH`,
as is, in the terminal — or, with the canvas, in a terminal cell. Open the
canvas once first (its view must exist).

## Two kernels, two address forms

- Host agents: a bare id (`exec`, `ai_<hex>`). The host root is `kernel`.
- Page agents: `<page guid>|<id>` — `<page guid>|canvas` is the page root,
  `<page guid>|view_root` its root view. Reflect each kernel (`reflect` on
  `kernel`, and on `<page guid>|canvas`) to see what each holds.

## The tree — flat peer canvases

```
user
├── ai                          the root AI (`fantastic … ai BACKEND`), under no canvas
└── web
    ├── canvas_ws               the socket leg every page speaks through
    └── canvas_page             the embedded page
        ├── canvas_root         THE MAIN canvas (main: true): create / delete / spawn
        │   ├── <backends>      a terminal's exec_bridge, an AI, setup, a bundle's agent
        │   ├── static_files    the files the cells load
        │   └── web_loader      its page records (.fantastic/web)
        │       └── canvas      (page side) → view_root → background · <its cells>
        └── canvas_<name>       a PEER canvas: new_canvas {name} on any canvas node
            ├── <backends>
            └── canvas_<name>_loader   its page records (.fantastic/web/canvas_<name>)
```

Canvases are FLAT peers: each its own page kernel (own guid), own cells' host
backends. Open one at the page URL with `?canvas=<node id>`
(none: the main one); a `canvases` cell lists them (★ the main one).
A canvas shows only its own subtree and the agents under no canvas; another
canvas's agents never appear on it. `canvases` (any canvas node) lists them.
NOTHING FLOATS: a canvas opens as its background alone. Everything is a cell
placed from the double-click — among them a `sidebar` (the agent tree) and `canvases` (every canvas; a click opens one, `+ new canvas` makes a
peer), as many as you like, moved, resized and closed like any cell. There is
no main chat: a chat cell goes wherever you place one.

THE SYSTEM BLOCK: `.fantastic/prompt.md` is every AI's system block. It starts
as the default (`{tree}`, `{self}`, `{howto}` — the agent tree, who the AI is,
how to call agents); edit it and every AI uses your text from its next turn.
Keep the placeholders you still want.

TERMINALS draw with WebGL, through one shared GPU context for the whole page —
a canvas of a few hundred live terminals stays smooth. A terminal's scrollbar
appears while the pointer is on it (the wheel scrolls either way). macOS allows
511 terminals (PTYs) at once by default (`kern.tty.ptmx_max`).

ESC: viewing one agent full screen, Esc goes back to the canvas (unless you are
typing in a terminal or a field).

THE SELECTOR: a double click adds a `selector` agent at the spot — the popup
— and it deletes itself once you pick. While it is open, `tiles {typed?}` on it
says what can be made there.

CODING AGENTS: the double-click lists `claude`, `codex`, `aider` and
`opencode` when they are on your PATH (`user path` — every program on PATH,
with its full path). A tile starts that program in a terminal cell, as you
would in a shell; it runs with your own account and settings.

MCP SERVERS: the double-click's `mcp connector` adds one remote MCP server (its
url, and a key if it needs one) as a host agent whose tools are its verbs — an
AI calls a tool like any agent. Its cell shows the server and its tools; ⟳
asks the server again. The key goes to your credential store, never a record.

PAGE BUNDLES: `<a canvas's records root>/bundles/<name>/bundle.yaml` with
`base: html_agent`, `sentence`, and `html: |` (the body each instance starts
with). They load before the page tree; a changed `bundle.yaml` reloads at once.
An instance is a page agent whose `handler_module` is the folder name.

Every page call reaches the host as `user` (`user.forward`, with the token
when `user` carries a `token_sha256`). `user` — and every AI agent under a
canvas node — reaches every host agent with every verb (god mode, by grants
both sides; the human narrows them with `update_agent`).

## A view's verbs (page side, on `<page guid>|<view id>`)

| verb | what |
|---|---|
| `place {kind, x, y, …}` | a new cell: `html`, `gl`, `terminal`, `chat` — or `handler_module` (any page bundle, `.ts` optional) instead of `kind` (one view for now — no nested views) |
| `move {id, x, y, …}` | move / resize a cell |
| `update {id, bytes?, fields?}` | a cell's new body (`bytes`, verbatim) or fields (display_name, trusted, readme, geometry, can_call, callable_by) |
| `wire {sender, receiver, verbs}` | a grant, written on each side that is a cell of the view (a host agent receiver: the sender's side only) |
| (a live URL or a file) | an html cell whose body is `<iframe src=URL>` — `trusted: true` for the host's own files |
| (grant hint) | a body that calls an agent the cell's `can_call` lacks → the reply names `grants_missing` and the `update` that adds them |
| (html check) | an html body that is empty, a placeholder (no element) or truncated (an unclosed script, a cut tag) is refused, saying why |
| `remove {id}` | delete a cell (refused for a delete-locked agent) |

`update {id: <the view>, fields: {…}}` sets the view's own fields. Every cell answers `reflect`, `get_html`,
`set_html`, `emit`. What is near a point is the host's: `around {view?, x, y,
radius}` on `user` (default view: `view_root`) — `{view, members: [{id,
kind, title, rect, distance, backend_id?}]}`, nearest first.

## Views of host agents

A cell is a VIEW of a host agent: the agent's record says how it is drawn
(`view: {kind, bytes, calls}`, else terminal for an exec agent, chat for an AI,
else its reflect card). `place {backend_id, x, y}` shows one; ✕ closes the view
and the agent lives on (the sidebar lists it; ⌫ there deletes it). A cell whose
agent is gone disappears.

## The background

Each view has one `background` agent with no children (`background` on the
view, delete-locked): one GL source,
`set_bg {source}`, `default_bg` (the bundled scene, `default_bg.ts` — black for now), `clear_bg`, `get_bg`. The source is a function body
`(THREE, scene, t, onFrame, cleanup)`.

## The viewer

The page views ONE page agent at a time: a view (its cells on its world) or
any other agent full-page. A click in the sidebar tree views an agent;
⌘-click (Ctrl-click off macOS) focuses it on its view. `#view=<id>` in the
URL is the viewed agent.

## Cells

- Terminal: an `exec_bridge` PTY job running the human's login `$SHELL`; the
  shell ending closes the cell.
- AI chat: the cell's view folder spawns `aimem_<hex>` (a RAM store) with
  `ai_<hex>` under it; `keep` (on `backend`) moves the dialog to the project's
  SQLite store.
- Files: `GET /static_files/file/<path>` — one read-only file leg over the
  project dir for every view, deny-all: its `allow` list (on its record,
  `update_agent` from above) exposes what the cells load.
- html panels run sandboxed (null origin) unless `trusted: true`; their only
  way out is the connector: `fantastic.send(target, payload)`,
  `fantastic.emit`, `fantastic.onMessage`, `fantastic.watch`.
- Set-up (⚙): writes AI agent records, keys and templates — `fantastic readme
  setup`.
