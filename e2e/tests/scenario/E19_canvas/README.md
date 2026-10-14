# E19 · the canvas — thin agents through the ws loader

**Costs:** free. No model runs. A real headless Chrome is driven over CDP.

The canvas is a thin web UI. The page holds no store and no AI. Its thin agents
(a terminal view here) are records that the ws loader (`web_loader`, a
`kernel_state` under the socket leg `canvas_ws`, rooted at `.fantastic/web`) writes as
folders under the canvas. On boot the page's `proxy_loader` reads them back and
they rehydrate.

A view that fronts a backend it spawned is one half of a PAIR; the other half
is a child of the socket leg (`canvas_ws`) carrying the view's id (`view_id`). The
leg lives its own life: the page connecting, leaving or reloading never touches
the backends behind it. Nothing on the host enforces the pairing, so it is kept
by rule, by the page at load: a backend whose view is gone is deleted, a view
whose backend is gone is marked `backend_lost` and waits for ⟳.

| run | proves |
|---|---|
| `first-load` | the page seeds `canvas`; the loader writes its anchor and `agents/canvas/agent.yaml`; the grouping is empty |
| `spawn-terminal` | a double click creates an `exec_bridge` agent (`shell: "login"`) running the child's own `$SHELL -l` as a PTY job, as a CHILD OF THE LEG (`canvas_ws`, the loader's parent), carrying its view's id and its canvas's (`view_id`, `canvas_id`), and a `terminal_view` member whose own `can_call` grants exactly that backend (the view sends as itself); `echo hi-$((6*7))` prints `hi-42`; the member is a folder UNDER the canvas and nowhere else |
| `page-reload` | the page leaves and comes back with the host up: the same backend, its shell state intact (`export FC_MARK=7`, confirmed before leaving → `mark-77` after) — the leg lives its own life |
| `move` | a drag updates the member's `x`/`y` in the same folder; nothing is duplicated |
| `restart` | the host stops (lock released) and restarts; `load_tree` returns canvas + member (parent `canvas`); the reloaded page rehydrates the member, typing reaches the rehydrated PTY (`again-42`), the position is kept |
| `lost-backend` | the backend's record is removed while the host is down: on load the view is marked `backend_lost` and says so; nothing is respawned |
| `respawn` | ⟳ on the lost view spawns a fresh backend under the leg, bound to the same view (`back-42`) |
| `close` | the close button removes the member's folder and the backend it owns; the canvas stays |
| `orphan` | a spawned view's folder is removed while the host is down: on load the orphan backend is deleted |
| `exit-closes` | a fresh terminal, `exit` typed into it: the shell ending closes the frame — view and exec agent gone together, as ✕ |
| `restart` (also) | a different `--port` is refused first (creation-only, exit 2, names `update_agent`); rehydrate reads the port from `web`'s record; the headless stdout is the message stream (`[<id>] <agent> <kind>` lines, no escapes) — what E18 checked |
| `restart-empty` | after another restart only the canvas comes back, with no frames and no backends under the leg |

Runner: `e2e/runners/canvas.py`. Host: the product, `fantastic canvas --port N`
(the canvas under `web`: `canvas_page`, `canvas_ws`,
`web_loader`). Page: the canvas app `src/tui/web/canvas`, embedded in the binary.

    cd src/lib/python && uv run python -u ../../../e2e/runners/canvas.py canvas
