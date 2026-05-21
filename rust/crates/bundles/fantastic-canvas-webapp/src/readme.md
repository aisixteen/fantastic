# canvas_webapp — spatial UI front-end
Serves the canvas HTML (DOM iframes + GL scene) at `/<id>/`. Pairs with a `canvas_backend` via `upstream_id` on the record. Itself canvas-eligible (answers `get_webapp`), so a canvas can host another canvas.
