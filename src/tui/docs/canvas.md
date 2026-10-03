> The browser canvas (`fantastic canvas`): a page kernel that views host agents as cells; its control-plane verbs.

# The canvas

`fantastic canvas [--port N]` runs the terminal UI and serves the canvas page
from this binary. The page is its OWN kernel (in the browser) bridged to the
host kernel through the socket leg `canvas_ws`. It holds no memory store and no
AI: every cell is a VIEW of a host agent (a terminal, an AI chat, a set-up
cell) or page-side content (html, gl).

## Two kernels, two address forms

- Host agents: a bare id (`exec`, `ai_<hex>`). The host root is `kernel`.
- Page agents: `<page guid>|<id>` — `<page guid>|canvas` is the page root,
  `<page guid>|view_root` its root view. Reflect each kernel (`reflect` on
  `kernel`, and on `<page guid>|canvas`) to see what each holds.

## The tree

```
user
└── web
    ├── canvas_ws               the socket leg the page speaks through
    └── canvas_page             the embedded page
        └── canvas              the canvas's host node
            ├── web_loader      the page's records (.fantastic/web)
            │   ├── static_files                 the files every view's cells load
            │   └── canvas                       (page side)
            │       └── view_root
            │           ├── background · <its cells>
            │           └── view_<id> …          nested views, each the same
            └── backend         creates / deletes its own children
                └── view_root   one folder per page view, the same hierarchy
                    ├── <backends>   a terminal's exec_bridge, an AI, setup
                    └── view_<id> …
```

Every page call reaches the host as `user` (`user.forward`, with the token
when `user` carries a `token_sha256`). `user` — and every AI agent under
`backend` — reaches every host agent with every verb (god mode, by grants
both sides; the human narrows them with `update_agent`).

## A view's verbs (page side, on `<page guid>|<view id>`)

| verb | what |
|---|---|
| `place {kind, x, y, …}` | a new cell: `html`, `gl`, `terminal`, `chat`, `view` (a nested view) |
| `move {id, x, y, …}` | move / resize a cell |
| `update {id, bytes?, fields?}` | a cell's new body (`bytes`, verbatim) or fields (display_name, trusted, readme, geometry) |
| `wire {sender, receiver, verbs}` | a grant between two cells, both sides |
| `remove {id}` | delete a cell (refused for a delete-locked agent) |
| `get_html` / `set_html` | a nested view's card html, shown on its parent |

`update {id: <the view>, fields: {main_ai_chat: <chat cell id> | null}}`
names the view's main chat. Every cell answers `reflect`, `get_html`,
`set_html`, `emit`. What is near a point is the host's: `around {view?, x, y,
radius}` on `user` (default view: `view_root`) — `{view, members: [{id,
kind, title, rect, distance, backend_id?}]}`, nearest first.

## The background

Each view has one `background` agent with no children (`background` on the
root view, delete-locked; a nested view's is `<view>_bg`): one GL source,
`set_bg {source}`, `clear_bg`, `get_bg`. The source is a function body
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
