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
(none: the main one). The canvas-list cell is off the double-click menu for
now.
A canvas shows only its own subtree and the agents under no canvas; another
canvas's agents never appear on it. `canvases` (any canvas node) lists them.
NOTHING FLOATS: a canvas opens as its background alone. Everything is a cell
placed from the double-click — among them a `tree view` (the agent tree), as
many as you like, moved, resized and closed like any cell. There is
no main chat: a chat cell goes wherever you place one.

THE SYSTEM BLOCK: `.fantastic/prompt.md` is every AI's system block. It starts
as the default (`{tree}`, `{self}`, `{howto}` — the agent tree, who the AI is,
how to call agents); edit it and every AI uses your text from its next turn.
Keep the placeholders you still want.

TERMINALS draw with WebGL, through one shared GPU context for the whole page —
a canvas of a few hundred live terminals stays smooth. A terminal's scrollbar
appears while the pointer is on it (the wheel scrolls either way). macOS allows
511 terminals (PTYs) at once by default (`kern.tty.ptmx_max`).

THE POPUP: drag it by its title and the spot it places at
moves with it. ←↑↓→ move across the tiles as drawn. Double-click → `tips` puts
a tips & tricks card on the canvas — an html cell: ask your AI to add a tip, or
edit its html.

CANVAS THEME: double-click → `canvas theme` places the view's theme manager,
two sections. BACKGROUNDS: four stock backgrounds — Isoline, Glyph, Copper,
Moire — each in several colour sets, your own list, `random bg`, `every
minute`, `none` and `default` (Isoline). × hides a stock one (`restore stock`
brings them back) or deletes one of yours. THEMES: where the cells sit — stock
`canvas` (the free canvas) and `mobile` (one column, auto under 768px wide),
and yours: `+ new theme` / ✎ edit a theme's name, layout (free canvas or one
column) and AUTO trigger (when the tab is at most / at least N px wide); ×
hides a stock one or deletes yours — never the default `canvas`; `auto` lets
the triggers choose (yours first). A NEW theme (or background) is your AI
agent's to make: ask an AI cell — it calls `add` / `add_theme` on the manager,
and they stay.

THE DOUBLE-CLICK: `AI agent` — the ONE way to make an AI: your default (★)
set-up, typed text its first turn; with none set up, the set-up wizard. Your
saved set-ups are not tiles: another one is the AI cell's ⚙ settings →
`‹ other set-ups` (a saved one or a new provider, in that cell).

ESC: viewing one agent full screen, Esc goes back to the canvas (unless you are
typing in a terminal or a field).

THE SELECTOR: a double click adds a `selector` agent at the spot — the popup
— and it deletes itself once you pick. While it is open, `tiles {typed?}` on it
says what can be made there.

CODING AGENTS: the double-click lists `claude`, `codex`, `aider`,
`opencode` and `fantastic` when they are on your PATH (`user path` — every program on PATH,
with its full path). A tile starts that program in a terminal cell, as you
would in a shell; it runs with your own account and settings.

MCP SERVERS: the double-click's `mcp connector` adds one remote MCP server (its
url, and a key if it needs one) as a host agent whose tools are its verbs — an
AI calls a tool like any agent. Its cell shows the server and its tools; ⟳
asks the server again. The key goes to your credential store, never a record.

USER STORE: the double-click's `user store` shows the `user_store` host agent:
what your home keeps across every fantastic — the stored keys (refs and hosts,
never values), the AI set-ups, and the stored LOCATIONS of your fantastics
(`<home>/fantastics/`, one file each: path, name, when stored, last seen).
`store` / `unstore` this fantastic, × unstores one, `unstore all others` keeps
only this one. A stored fantastic's `last seen` moves on each start — the list a
local manager of fantastics reads.

PAGE BUNDLES: `<a canvas's records root>/bundles/<name>/bundle.yaml` with
`base: html_agent`, `sentence`, and `html: |` (the body each instance starts
with). They load before the page tree; a changed `bundle.yaml` reloads at once.
An instance is a page agent whose `handler_module` is the folder name.

Every page call reaches the host as `user` (`user.forward`). `user` — and every AI agent under a
canvas node — reaches every host agent with every verb (god mode, by grants
both sides; the human narrows them with `update_agent`).

## A view's verbs (page side, on `<page guid>|<view id>`)

| verb | what |
|---|---|
| `place {kind, x, y, …}` | a new cell: `html`, `gl`, `terminal`, `chat` — or `handler_module` (any page bundle, `.ts` optional) instead of `kind` (one view for now — no nested views) |
| `move {id, x, y, …}` | move / resize a cell — in the layout of the theme in use |
| `update {id, bytes?, fields?}` | a cell's new body (`bytes`, verbatim) or fields (display_name, trusted, readme, geometry, can_call, callable_by) |
| `theme {name?}` | which layout the cells sit in: `canvas` (the free 2D canvas), `mobile` (one column), `auto` (default — mobile in a tab under 768px wide); no name: what it is |
| `wire {sender, receiver, verbs}` | a grant, written on each side that is a cell of the view (a host agent receiver: the sender's side only) |
| (a live URL or a file) | an html cell whose body is `<iframe src=URL>` — `trusted: true` for the host's own files |
| (grant hint) | a body that calls an agent the cell's `can_call` lacks → the reply names `grants_missing` and the `update` that adds them |
| (html check) | an html body that is empty, a placeholder (no element) or truncated (an unclosed script, a cut tag) is refused, saying why |
| `remove {id}` | delete a cell (refused for a delete-locked agent) |

`update {id: <the view>, fields: {…}}` sets the view's own fields. Every cell answers `reflect`, `get_html`,
`set_html`, `emit`.

## Themes — where the cells sit

A cell's `x, y, width, height` are not on the cell: they are in its view's
LAYOUT for the theme in use — one agent per theme, `layout_<theme>`
(`layout.ts`, children of the view, `rects: {<cell id>: {x, y, width,
height}}`). Switching theme moves the cells, never the agents. A cell with no
rect in the theme in use is placed by its layout, once: free — the nearest free
space around the centre of what the tab shows; column — the bottom of the
column. In a column theme the wheel scrolls the column, and a cell's 📌 PINS it
(one per layout, `pinned` on its layout agent): held on the screen's bottom,
full width, while the column scrolls (e.g. a chat). The column is flat (no
border, no rounding, no holes); the ⠿ grip in the middle of a cell's head drags
it up or down the column. A double click on any cell's head MINIMIZES it to its
bar (`minimized` on its layout agent); again, it is back. Themes are chosen and edited
on the canvas theme agent (`themes`, `use_theme`, `add_theme`), or with the
view's `theme` verb.

## Views of host agents

A cell is a VIEW of a host agent: the agent's record says how it is drawn
(`view: {kind, bytes, calls}`, else terminal for an exec agent, chat for an AI,
else its reflect card). `place {backend_id, x, y}` shows one; ✕ closes the view
and the agent lives on (the tree view lists it; ⌫ there deletes it). A cell whose
agent is gone disappears.

## The background

Each view has one `background` agent with no children (`background` on the
view, delete-locked): one GL source,
`set_bg {source}`, `default_bg` (the bundled scene, `default_bg.ts` — black for now), `clear_bg`, `get_bg`. The source is a function body
`(THREE, scene, t, onFrame, cleanup)`.

## The viewer

The page views ONE page agent at a time: a view (its cells on its world) or
any other agent full-page. A click in the tree view views an agent;
⌘-click (Ctrl-click off macOS) focuses it on its view. `#view=<id>` in the
URL is the viewed agent.

## Cells

- Terminal: an `exec_bridge` PTY job running the human's login `$SHELL`; the
  shell ending closes the cell. The host watches each shell for the command it
  launches (its foreground job) and keeps the last one on the exec agent
  (`last_cmd`): the cell shows it top right. NOTHING STARTS ON ITS OWN (TOS): a
  terminal's program — its shell, or claude / codex / `fantastic` / any program
  it was placed with — starts when you place the cell; after a restart (no live
  job) the cell shows it STOPPED, and only your `▶ start` (or, for a shell,
  `▶ run <last command>`) starts it. Our own AI agent cells restart by themselves.
- AI chat: every dialog is KEPT from its start (as in the terminal): listed
  under the double-click's chats for reopening; its badge is its name (click to
  rename). The footer: `clear` (a new thread — nothing deleted), the last
  generation's speed and the context it used of the window (`N tok/s · Xk / Wk
  ctx`, from the backend's reported usage; `~` = its estimate). Thinking is a
  collapsible block (collapsed when done), KEPT on its answer's row: it is back
  after a reload. In the terminal's AI panel a line with a kept thought shows 💭
  and `t` opens it; `C` clears, and the status line shows the same speed and
  context. A call whose `<` the server dropped is
  repaired by the call checker and runs; any other malformed call shows the
  checker's notice.
- Files: `GET /static_files/file/<path>` — one read-only file leg over the
  project dir for every view, deny-all: its `allow` list (on its record,
  `update_agent` from above) exposes what the cells load.
- html panels run sandboxed (null origin) unless `trusted: true`; their only
  way out is the connector: `fantastic.send(target, payload)`,
  `fantastic.emit`, `fantastic.onMessage`, `fantastic.watch`.
- AI set-up: a two-page WIZARD (double-click → `AI agent`, any time). Page 1:
  a saved provider (its agent is made at once) or a new one — responses,
  anthropic, ollama, vllm. Page 2: a name (auto), the URL (filled), the model,
  the backend's required fields, and the key (the stored key for that host, a
  new one pasted into the keychain or a 0600 file, or none); "create & open
  chat" saves it as a saved set-up (the first becomes the default). Every page has
  SKIP: an AI cell "not set up yet", whose ⚙ settings runs the wizard for it.
  ⚙ on any AI cell is its settings (page 2 with its values), live on its next
  turn. A failed turn offers settings, or a coding agent started next to the
  cell to help. Keys are never read from the environment — `fantastic readme
  setup`.
- An AI's QUESTIONS to you: an `inquiry` it sends `user` (text, single, multi
  or confirm) is held there (`inquiries`) and shows as a card in its chat — a
  field, choice buttons, checkboxes or yes / no; your answer goes back to it on
  its thread (`answer_inquiry`). Open questions are kept in RAM: a host restart
  forgets them (the AI's history keeps the question).
