# canvas_webapp — spatial UI
The infinite-canvas browser UI. Owns a canvas_backend child (auto-created on boot, tracked via `upstream_id`). `get_webapp` → iframe descriptor.

Zoom is **horizon-anchored** and **smoothed**: the wheel pulls toward/away from screen center, not the cursor, and only nudges a `targetZ` — the rAF loop lerps `view.z` toward it for a soft glide instead of a stepped jump. The 2D iframe layer and the GL scene scale around that one shared point and stay in sync (`camera.zoom` follows the CSS `view.z`) — pure proportional zoom, no dolly/slide. 2D HTML panes live on the depth-0 plane and zoom as the main camera; GL content keeps its depth parallax around the same locked plane.

Each GL view (an agent answering `get_gl_view`) runs in its **own `THREE.Group` container** — the scene-graph analogue of an html_agent's iframe. The view's source is injected the group as its `scene`. `gl_source_changed` (emitted by `gl_agent.set_gl_source`) reloads that one view in place: dispose the group + recompile, scoped to the view — no canvas refresh, sibling views untouched.
