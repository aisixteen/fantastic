> Host kernel + a browser frontend kernel; `*.tools` run here, `*.ts` render there.

# Two kernels — host + browser frontend

This is the HOST kernel (`*.tools` bundles). A second kernel — the ts kernel
LIBRARY (`src/lib/ts`), booted in the browser by an APP that registers its own
`*.ts` bundles (the canvas and its views are such an app, the CLI's) —
federates over the SAME WS and renders all UI. The `handler_module` SUFFIX says
where an agent runs:

- `*.tools` → HERE. `create_agent parent_id=<owner> handler_module=<x>.tools` on the host tree.
- `*.ts`    → the FRONTEND. Do NOT `create_agent` these here (the host
  weak-loads `*.ts` → inert). They are created THERE, by their parent — ask
  the frontend view that owns that part of the tree; its loader persists them
  to the host store `web_loader` by itself.

There is NO server-side page/render route, no `mount` verb, no per-verb REST
surface — don't invent those. The host serves only STATIC files (via a
`file` agent) and carries `send()`/events over the WS bus. ALL UI is `*.ts`
agents the browser renders. A web UI is an **html agent** — a general-purpose
frontend agent (`handler_module` ending in `.ts`) whose record carries its
`html` body — never a `web.tools` page.

Sub-docs: `reflect kernel readme=frontend/html-agent` (add an html agent) ·
`reflect kernel readme=frontend/routing` (how messages route).
