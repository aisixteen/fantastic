# Fantastic canvas — the frontend page

> **This file is universal.** It is the canvas app's doc when you browse the
> source tree, **and** the self-describing revive guide that travels *inside*
> `dist/canvas.zip`. An LLM reviving the artifact reads ONLY this file out of the
> zip (`unzip -p canvas.zip readme.md`) — it never unpacks the fat bundle.

The canvas is the **view layer** of Fantastic: a page that runs the ts kernel
library (`src/lib/ts`) in the browser as a **peer** of the host — same
`send`/`reflect`/`watch` protocol — and renders that kernel's own agent tree. The
host (Python/Rust) knows nothing about it (weak binding). The library does
the kernel, the WS bridge and the persistence (`boot`); this app registers its
bundles, seeds an empty tree with the canvas root, and mounts.

## What's in it

- **canvas** — the compositor root; renders the frontend's own member tree, each
  member mounted inline (its view bundle) or as an iframe (external content).
- **terminal_view** — xterm CLIENT for a host PTY backend. Bound to any agent
  answering the PTY surface (`boot`/`write`/`ack`/`resize`/`interrupt`/`stop`) by
  a `backend_id` peer ref.
- **ai_view** — chat CLIENT for a host LLM backend. Bound to any agent answering
  `send`/`history`/`status` by `backend_id`; ESC cancels the in-flight turn
  through the kernel (`ps` → `cancel`).
- **resources_view** / **tasks_view** / **store_view** / **scheduler_view** /
  **cabinets_view** / **credentials_view** — READ-ONLY views of the agents that had
  only the stub (`agentic_resources`, `task_registry`, `ram_store` and `sqlite_store`,
  `scheduler`, the `cabinet` manager, `credential_store`). A record with no `view` gets
  its module's view (`host_views.ts` `MODULE_KINDS`); a record's own `view` still wins
  (a cabinet cell keeps its status cell). Each reads with the verbs of its grant
  (`RENDERER_CALLS`) plus `watch`, and an event on the agent reads it again. The
  rows come from pure models (`src/bundles/agent_models/`) that keep only whitelisted
  keys: the store view shows counts and the viewer's own search hits, never another
  owner's row, and the credentials view shows ref names, hosts, field names and grants,
  never a value (its grant has no `get`).
- **html_agent** / **gl_agent** — frontend CONTENT agents (a mutable HTML body in
  a sandboxed frame; a WebGL shader). Content, not host clients.
- **home_view** — the Home (`home.html`, served by the home kernel): the view of
  the host's `home` agent. It renders the agent's `state` (your fantastics, the
  open pages, the selection) and sends every click and key as a `home` verb; one
  live canvas frame per open page, never reloaded while you switch.
- **files_view** / **preview_view** — the file tree (the project's folder, live: every
  agent is a folder in it) and the previewer/editor (a file or the live agent). Page
  agents over the project's `files` bridge, with verbs an AI drives them by.
  A drop or `open` on the canvas makes (or reuses) the **file agent** or **folder
  agent** of that path (`open_view` on the bridge, `src/bundles/files/file_bridge.ts`):
  a preview then carries `view_agent` and is that file agent's view, a tree rooted at
  a folder is its folder agent's view. Its reads, saves and live changes go through
  the agent (its `read`/`write`, or its `list` and `watch`); what the agent does not
  answer (the stream reads, undo, the tree's own file operations) stays with the
  bridge. A cell saved before this is bound to the view of its path when it mounts
  (`bindCellToView`): its view is named on the cell, and the page waits for its grant
  on the view before the first read (`awaitGrant`: the host grants it after `open_view`
  answers, and says so on the state stream as `granted`, or `grant_refused` with why; no
  timeout and no probe loop). The page listens before it probes and remembers each word,
  so either order settles; a restored cell's view, which the host never announces
  again, is refused at once rather than waited on. A refusal is said in the cell, which
  stays open. A cell holds the path it
  serves: when it unmounts the path is released, and a view the state stream reports
  removed releases every path it serves (`release`, `onStateFrame`).
- **skin** — the page's skin agent: the design tokens (W3C 2025.10) every view is
  drawn with, per backdrop; it takes skins an AI writes (`fantastic readme skins`).
- **the canvas's own file tree sidebar** (#148, `canvas/tree_sidebar.ts`) — the file
  tree fused into the canvas page, not an agent and not a cell: a column fixed to the
  viewport's left edge, mounted beside the zoomed world (never in it), so a zoom or a
  pan never moves or scales it. On a wide screen (over 768px) the canvas sits beside it
  (the page's `tree-beside` and `tree-open` classes set the canvas's margin to the
  sidebar's width: 300px open, 22px as a rail); on a narrow one it is an overlay, opened
  only when asked. From a bird's-eye zoom its rows drag onto the canvas like the tree
  cell's: the same panel (`files/tree_panel.ts`, the drawing, the marks and the drag
  source) and the same drop on `#canvas`, which places each item by `dropPlan` (a grid
  centred on the point in world coordinates, at any zoom). Its tree is the project
  folder's view (#150), bound on the bridge as the tree cell binds its folder: the view's
  id is kept on the root view (`tree_sidebar.view_agent`), and dispose lets it go. A
  `‹`/`›` button collapses it, to a strip; its open state, its view and its tree's
  folders and selection are kept on the root view (`tree_sidebar`), so a reload restores
  them. Its verbs are the tree's, reached through the root view alone: `tree {verb:
  select|expand|collapse|reveal, path | agent}`; any other view refuses it (`bad_arg`,
  `arg: view`). The tree cell (`files_view.ts`) stays as the extra kind: a folder dropped
  on the canvas opens a tree rooted at it. Its colours are skin tokens (`--fc-*`); its
  widths are the two sizes in `styles.ts`.

A cell's head: **✕ hides** it — the view lets go of the cell, its agent keeps
running and its stream is torn down until it is shown again (the view's `show`
puts it back where it was, or at `x`, `y`). **Shift+✕** and the head's
right-click menu (`cell_menu.ts`) **delete** the agent, through the sidebar's
delete path (a locked cell or agent refuses); its **Export…** entry asks for a
folder and sends `export_subtree` for the cell's host agent to `user` (#268). The hidden set is on the view
(`hidden: [<cell id>…]`), so a reload keeps the cells hidden. Picking a hidden
agent in the sidebar shows its cell where it was.

**Chat cards (#339).** A chat cell (`ai_view.ts`) shows a host agent's panel INLINE, where the
AI's `embed {id}` row sits in the conversation (`history` lists it as a row of kind `embed`;
`unembed {id}` takes it out). The card (`ai_view/chat_card.ts`) is 100% of the chat column
wide and as tall as its panel says, at most `CHAT_CARD_MAX_HEIGHT` = 1200 px (taller content
scrolls inside; the twin of ai-core's constant). Its panel is the one `place {backend_id}` draws
(`mountHostPanel`), its calls go out as the chat cell, granted from the record's `view.calls`.
**✕ hides** the card (its hide is `embed:<chat cell>:<agent>` in the view's hidden set, so a
reload keeps it), **Shift+✕** and the menu **delete the agent** (the delete lock refuses, said),
the menu's *remove card* is `unembed`. The host's `removed` state frame turns a card "deleted";
a redraw with the agent gone shows `[deleted from canvas]`, and pressing it checks again.
The AI is told, by a short message through its `send`, when a fresh card overflows the column
or the cap.

The view↔backend pairing is **never hardcoded** — each view's reflect names the
host capability + verb surface it fronts; an LLM weaves the binding from that plus
the host's capability readme.

## Layout

```
src/main.ts            the page: boot (the library) → register → seed → mountCanvas
src/home.ts            the Home page (home.html): boot → its records via web_loader → mount home_view
src/bundle.ts          the esbuild entry: inlines xterm.css, then loads main.ts, or home.ts on <body data-page="home">
src/lib.ts             the ONE seam to the library's public entry (src/lib/ts/src/index.ts)
src/bundles/           canvas/, view.ts, terminal_view/, ai_view/, html_agent/, gl_agent/, skin/, shell.ts,
                       agent_models/ (pure models of the read-only agent views), read_view.ts (their shared body)
src/bundles/home_view/ the Home: home_view.ts (the view), home_model.ts (pure: state, labels, cards, shortcuts),
                       pages.ts (the canvas-frame pool), home_css.ts (its style, every colour one --home-* property)
src/vendor/            three, xterm, addon-fit, xterm.css (hermetic, no CDN — VENDOR.md)
tests/                 node --test "tests/**/*.test.ts"   (no build step)
scripts/pack.sh        → dist/canvas.zip
```

Typecheck (the library's pinned typescript, no install):
`node ../../../lib/ts/node_modules/typescript/bin/tsc -p tsconfig.json`.

## The artifact

`dist/canvas.zip` (built by `sh scripts/pack.sh`, with the library's
**vendored, sha256-pinned esbuild** — no npm registry, no node on the build path)
holds exactly three entries:

| entry | what it is |
|---|---|
| `readme.md` | this file (the revive guide) |
| `bundle.min.js` | the WHOLE page in ONE minified ESM file — the app, the ts kernel library, three.js, xterm and xterm.css **inlined**. No chunks, no import map, no external assets. (Size is irrelevant — it is never read by an LLM, only served.) |
| `bundle.min.js.map` | source map → browser stack traces map back to TS |

## Revive recipe (for an LLM — without unpacking the bundle)

1. **Inspect, don't unpack.** List the zip from its central directory and read
   only this readme — the fat `bundle.min.js` is never opened:
   ```sh
   unzip -l canvas.zip             # lists the 3 entries
   unzip -p canvas.zip readme.md   # prints THIS file (KBs), bundle untouched
   ```
2. **Pull the members you serve — ON DEMAND, never a full unzip:**
   ```sh
   mkdir -p serve            # INSIDE the kernel's running dir — file_bridge
                             # clamps every root to the dir the kernel runs in
   unzip -p canvas.zip bundle.min.js      > serve/bundle.min.js
   unzip -p canvas.zip bundle.min.js.map  > serve/bundle.min.js.map  # optional
   ```
3. **Serve that dir through the host's generic `file_bridge` agent** — the host
   stays ignorant of the frontend; it just serves bytes. The fs edge is SEALED
   by default — open it consciously:
   ```sh
   fantastic kernel_state create_agent handler_module=file_bridge.tools id=js_kernel \
     root=serve ingress_rule=allow_all_ingress
   # → GET /js_kernel/file/bundle.min.js now serves the page
   ```
4. **Prereq (operator, one-time)** — the host must expose HTTP + a WS surface +
   the loader the frontend federates to: a `web` agent (HTTP), a `web_ws` child
   (the WS verb surface), and a `kernel_state` rooted at `.fantastic/web` aliased
   `web_loader`. (See the host root readme — `reflect readme=true`.)
5. **Provide a trivial mount page** and serve it from the same file root. The
   CSS and all vendors are inlined, so the page needs **NO import map and NO
   stylesheet link** — just the one script tag:
   ```html
   <!doctype html><html><head><meta charset="utf-8"><title>fantastic · canvas</title></head>
   <body><script type="module" src="/js_kernel/file/bundle.min.js"></script></body></html>
   ```
   Open it. The bundle injects `xterm.css`, boots the kernel (the WS bridge to the
   host, the frontend's own tree hydrated from `web_loader`), seeds the canvas
   root on a fresh namespace, and mounts the canvas. Host backends (PTY, LLM)
   appear as weak peers referenced by id.

## Host contract (weak binding)

- Every agent answers `{type:"reflect"}` — the universal discovery verb.
- The frontend federates to the host's **`web_loader`** alias over the WS bridge;
  it persists every local change back through that loader. No per-page session id.
- Direction is **frontend → host**: a view knows it fronts a host capability; the
  host never references the frontend.

## Integrity (verify before serving — it runs in a browser)

```sh
shasum -a 256 bundle.min.js
# expected: __BUNDLE_SHA256__
```

> In the source tree the digest above is a placeholder token. The pack
> substitutes the real digest into the `readme.md` copy it places **inside**
> `canvas.zip`, so the artifact carries its own integrity check.

---

*Part of **Aisixteen Fantastic** — licensed **AGPL-3.0-or-later**. "Aisixteen
Fantastic" and "AISIXTEEN" (USPTO reg. 7,238,635) are trademarks of AISixteen; the
license covers the code only, not the marks — forks must rename.*
