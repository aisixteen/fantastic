> The browser canvas (`fantastic canvas`): a page kernel that views host agents as cells; its control-plane verbs.

# The canvas

`fantastic canvas [--port N]` runs the terminal UI and serves the canvas page
from this binary. The page is its OWN kernel (in the browser) bridged to the
host kernel through the socket leg `canvas_ws`. It holds no memory store and no
AI: every cell is a VIEW of a host agent (a terminal, an AI chat, a set-up
cell) or page-side content (html, gl).

## Two kernels, two address forms

- Host agents: a bare id (`exec`, `ai_<hex>`). The host root is `kernel`.
- Page agents: `<page guid>|<id>` — `<page guid>|canvas` is the canvas root.
  Reflect each kernel (`reflect` on `kernel`, and on `<page guid>|canvas`) to
  see what each holds.

## Where host-side canvas agents live

```
web
├── canvas_page      the embedded page
└── canvas_ws        the socket leg the page speaks through
    ├── web_loader   the page's records (.fantastic/web)
    ├── ai_cells     AI cells from the user's templates
    ├── activity     open questions to the human
    └── <backends>   one per spawned view (a terminal's exec_bridge, an AI, setup)
```

The leg — and every AI agent under it — reaches every host agent with every
verb (god mode, by grants both sides; the human narrows them with
`update_agent`).

## The canvas agent's verbs (page side, on `<page guid>|canvas`)

| verb | what |
|---|---|
| `around {x, y, radius}` | the rendered cells near a point: `{members: [{id, kind, title, rect, distance, backend_id?}]}`, nearest first |
| `place {kind, x, y, …}` | a new cell: `html`, `gl`, `terminal`, `chat`, `canvas` (a child canvas) |
| `move {id, x, y, …}` | move / resize a cell |
| `update {id, bytes?, fields?}` | a cell's new body (`bytes`, verbatim) or fields (display_name, trusted, readme, geometry) |
| `wire {sender, receiver, verbs}` | a grant between two cells, both sides |
| `remove {id}` | delete a cell (refused for a delete-locked agent) |
| `get_html` / `set_html` | a child canvas's card html, shown on its parent |

`update {id: <the canvas>, fields: {main_ai_chat: <chat cell id> | null}}`
names the canvas's main chat. Every cell answers `reflect`, `get_html`,
`set_html`, `emit`.

## The background

`<page guid>|background` (a child canvas: `<id>_bg`) is one GL source per
canvas, delete-locked: `set_bg {source}`, `clear_bg`, `get_bg`. The source is a
function body `(THREE, scene, t, onFrame, cleanup)`.

## The viewer

The page views ONE page agent at a time: a canvas (its cells on its world) or
any other agent full-page. A click in the sidebar tree views an agent;
⌘-click (Ctrl-click off macOS) focuses it on its canvas. `#view=<id>` in the
URL is the viewed agent. `around` answers only for the canvas being viewed.

## Cells

- Terminal: an `exec_bridge` PTY job running the human's login `$SHELL`; the
  shell ending closes the cell.
- AI chat: `ai_cells` spawns `aimem_<hex>` (a RAM store) with `ai_<hex>` under
  it; `keep` moves the dialog to the project's SQLite store.
- html panels run sandboxed (null origin) unless `trusted: true`; their only
  way out is the connector: `fantastic.send(target, payload, secret?)`,
  `fantastic.emit`, `fantastic.onMessage`, `fantastic.watch`.
- Set-up (⚙): writes AI agent records, keys and templates — `fantastic readme
  setup`.
