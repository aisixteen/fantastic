# E161 · typed_peer_call — a call to a page agent is typed by the page's schema

Scripted, no model, with a browser (`costs: free`). #719 (#721): a call to an agent on a
CONNECTED kernel (`<page guid>|view_root`) is typed by that kernel's declaration — the
sender asks the peer's `docs {of, verb, part: schema}` before every send and reads the
text by it. #728: the free e2e of that round trip.

The sender is `user.forward` (what a canvas tab's host call is), one of the senders that
type through `typing::payload`. The host and a real canvas page in headless Chrome; the
page's socket frames are read over CDP (Network enabled before the page opens).

| check | what |
|---|---|
| 0 | the page has a guid |
| 1 | `place {kind: html, id, bytes, x: "100", y: "200", width: "320", height: "180"}` (every geometry value TEXT) to `<guid>\|view_root` answers `placed` |
| 2 | the page received `docs {of: view_root, verb: place, part: schema}` on its `kernel` |
| 3 | the page received the `place` call once, after that docs ask |
| 4 | the `place` it received carries numbers: `x == 100`, `y == 200`, `width == 320`, `height == 180` |
| 5 | the free-canvas layout on disk (`layout_canvas` `rects`) holds the cell at that rect |
| 6 | the cell's frame is drawn at that rect (`left/top/width/height` in px) |
| 7 | the same call with `x: "far"` is refused on the host: `reason: bad_arg`, `arg: x` |
| 8 | that `place` never reached the page (no call frame for it) |
| 9 | no cell was made for it (layout and page) |
| 10 | no page error |

    cd src/lib/python && uv run python ../../../e2e/run.py --only-free --id typed_peer_call
