# E139 · ws_route — a second `web_ws` leg with a literal `route` beside the canvas's catch-all

Scripted, no model, no browser (`costs: free`). #559: `web_ws` takes one optional `route` (a literal mount path); without it the leg is the host's one catch-all `/{host_id}/ws`, and a second catch-all leg is refused.

One `canvas` run records `web`, `canvas_ws` (the catch-all, `/user/ws`) and `user`; a second leg `page_ws` with `route: /page_ws/ws` is seeded under the same `web`; then `fantastic --headless` runs in the same folder.

**PASS** when every check holds:

| check | what |
|---|---|
| a1 | web boots with the two `web_ws` legs: the canvas page answers 200 |
| a2 | the canvas's catch-all works: a host call through `/user/ws` answers (`reflect` of the root, an id, no error) |
| b1 | the literal route answers as its own leg: a `reflect` call on `/page_ws/ws` returns `id: page_ws`, `path_pattern: /page_ws/ws` |
| b2 | the log after the headless start has no `a second catch-all` refusal |
