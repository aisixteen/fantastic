> Host kernel + a browser frontend kernel; `*.tools` run here, `*.ts` render there.

# Two kernels — host + browser frontend

This is the HOST kernel (`*.tools` bundles). A second kernel — the ts kernel
LIBRARY (`src/lib/ts`), booted in the browser by an APP that registers its own
`*.ts` bundles (the canvas and its views are such an app, the CLI's) —
federates over the SAME WS and renders all UI. The `handler_module` SUFFIX says
where an agent runs:

- `*.tools` → HERE. `create_agent parent_id=<owner> handler_module=<x>.tools` on the host tree.
- `*.ts`    → the FRONTEND. Do NOT `create_agent` these here (the host
  weak-loads `*.ts` → inert). You SPAWN one by `persist_record`-ing its
  record into the frontend store `web_loader`; the browser hydrates + runs it.

There is NO server-side page/render route, no `mount` verb, no per-verb REST
surface — don't invent those. The host serves only STATIC files (via a
`file` agent) and carries `send()`/events over the WS bus. ALL UI is `*.ts`
agents the browser renders. A "web panel" is a frontend view record
(`handler_module` ending in `.ts`) the frontend kernel renders — never a
`web.tools` page.

Sub-docs: `reflect kernel readme=frontend/panel-recipe` (add an interactive
panel) · `reflect kernel readme=frontend/routing` (how messages route).
