# Fantastic frontend kernel — `ts/`

> **This file is universal.** It is the `ts/` package doc when you browse the
> source tree, **and** the self-describing revive guide that travels *inside*
> `dist/js_kernel.zip`. An LLM reviving the artifact reads ONLY this file out of
> the zip (`unzip -p js_kernel.zip readme.md`) — it never unpacks the fat bundle.

The frontend kernel is the **view layer** of Fantastic, and it is a *real kernel*
— same `send`/`reflect`/`watch` protocol as the host, running in the browser as a
**peer**. The host (Python/Swift) knows nothing about it (weak binding). It dials
the host over a WebSocket bridge, hydrates its own agent tree, and renders it.

## What's in it

- **canvas** — the compositor root; renders the frontend's own member tree, each
  member mounted inline (its view bundle) or as an iframe (external content).
- **terminal_view** — xterm CLIENT for a host PTY backend. Bound to any agent
  answering the PTY surface (`boot`/`write`/`ack`/`resize`/`interrupt`/`stop`) by
  a `backend_id` peer ref.
- **ai_view** — chat CLIENT for a host LLM backend. Bound to any agent answering
  `send`/`history`/`interrupt`/`status` by `backend_id`.
- **html_agent** / **gl_agent** — frontend CONTENT agents (a mutable HTML body in
  a sandboxed frame; a WebGL shader). Content, not host clients.

The view↔backend pairing is **never hardcoded** — each view's reflect names the
host capability + verb surface it fronts; an LLM weaves the binding from that plus
the host's capability readme.

## Two ways to run it

**A) From source (dev).** Build the scattered ESM with the one pinned dev dep
(`typescript`) and serve `dist/` behind an import map — see `SERVE.md`:

```sh
cd ts && npm install && npm run build          # → ts/dist/*.js (ES modules)
```

**B) From the sovereign artifact `js_kernel.zip` (this guide).** One rolled-up,
self-contained file — no import map, no external CSS, no CDN, no npm. This is the
distribution form; the rest of this readme is its revive recipe.

## The artifact

`dist/js_kernel.zip` (built by `scripts/pack.sh`) holds exactly three entries:

| entry | what it is |
|---|---|
| `readme.md` | this file (the revive guide) |
| `bundle.min.js` | the WHOLE frontend kernel in ONE minified ESM file — all TS + three.js + xterm + xterm.css **inlined**. No chunks, no import map, no external assets. (Not a website; size is irrelevant — it is never read by an LLM, only served.) |
| `bundle.min.js.map` | source map → browser stack traces map back to TS |

Built with a **vendored, sha256-pinned esbuild Go binary** (`tools/esbuild/`), so
the build path touches no npm registry and no node. The npm supply chain is never
on the critical path.

## Revive recipe (for an LLM — without unpacking the bundle)

1. **Inspect, don't unpack.** List the zip from its central directory and read
   only this readme — the fat `bundle.min.js` is never opened:
   ```sh
   unzip -l js_kernel.zip          # lists the 3 entries
   unzip -p js_kernel.zip readme.md   # prints THIS file (KBs), bundle untouched
   ```
2. **Pull the members you serve — ON DEMAND, never a full unzip.** Make a serve
   dir and stream ONLY the files you actually serve straight out of the archive
   (`unzip -p` copies one member byte-for-byte to stdout; the archive is never
   extracted and the fat bundle is never *read*, only copied):
   ```sh
   mkdir -p /ABS/PATH/serve
   unzip -p js_kernel.zip bundle.min.js      > /ABS/PATH/serve/bundle.min.js
   unzip -p js_kernel.zip bundle.min.js.map  > /ABS/PATH/serve/bundle.min.js.map  # optional: debuggable stack traces
   ```
   That's it — no `unzip -d`, no extracted tree, nothing on disk you didn't ask for.
3. **Serve that dir through the host's generic `file` agent** — the host stays
   ignorant of the frontend; it just serves bytes:
   ```sh
   fantastic fs_loader create_agent handler_module=file.tools id=js_kernel \
     root=/ABS/PATH/serve
   # → GET /js_kernel/file/bundle.min.js now serves the kernel
   ```
4. **Prereq (operator, one-time)** — the host must expose HTTP + a WS surface +
   the loader the frontend federates to: a `web` agent (HTTP), a `web_ws` child
   (the WS verb surface), and an `fs_loader` rooted at `.fantastic/web` aliased
   `web_loader`. (See the host root readme — `reflect readme=true`.)
5. **Provide a trivial mount page** and serve it from the same file root. Because
   the CSS and all vendors are inlined, the page needs **NO import map and NO
   stylesheet link** — just the one script tag:
   ```html
   <!doctype html><html><head><meta charset="utf-8"><title>fantastic · canvas</title></head>
   <body><script type="module" src="/js_kernel/file/bundle.min.js"></script></body></html>
   ```
   Open it. The bundle injects `xterm.css`, opens the WS bridge to `web_ws`,
   hydrates the frontend's own tree from `web_loader` (`proxy_loader`), and mounts
   the canvas. Host backends (PTY, LLM) appear as weak peers referenced by id.

## Host contract (weak binding)

- Every agent answers `{type:"reflect"}` — the universal discovery verb. One
  round-trip describes the frontend tree and each view's verb surface.
- The frontend federates to the host's **`web_loader`** alias over the WS bridge;
  it persists every local change back through that loader. No per-page session id.
- Direction is **frontend → host**: a view knows it fronts a host capability; the
  host never references the frontend. Same rule as `kernel_bridge` — addressed by
  URL + id only, no shared types.

## Integrity (verify before serving — it runs in a browser)

```sh
shasum -a 256 bundle.min.js
# expected: __BUNDLE_SHA256__
```

> In the source tree `__BUNDLE_SHA256__` is a literal placeholder. `scripts/pack.sh`
> substitutes the real digest into the `readme.md` copy it places **inside**
> `js_kernel.zip`, so the artifact carries its own integrity check.

---

*Part of **Aisixteen Fantastic** — licensed **AGPL-3.0-or-later**
([`../LICENSE`](../LICENSE)). "Aisixteen Fantastic" and "AISIXTEEN" (USPTO reg.
7,238,635) are trademarks of AISixteen; the license covers the code only, not the
marks — forks must rename. See the [root README](../README.md#license--brand).*
