# canvas_webapp — spatial UI front-end

Serves the canvas HTML (DOM iframes + GL scene) at `/<id>/`. Pairs with a `canvas_backend` via `upstream_id` on the record. Itself canvas-eligible (answers `get_webapp`), so a canvas can host another canvas.

## Web dependencies

Three.js is loaded from `/_assets/three.module.js`, served by the
`fantastic-web` bundle from a vendored copy (v0.160.0). No CDN
dependency at runtime — the kernel is the single source of truth for
shared web deps. To update the pinned version, see
`rust/crates/bundles/fantastic-web/src/readme.md`.
