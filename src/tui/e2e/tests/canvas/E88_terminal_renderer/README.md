# E88 `terminal_renderer` — the terminals' renderer per webview

Phase 7 (#52): a view's terminals draw with the shared WebGL renderer, except
in WebKitGTK (the app on Linux, GNOME Web), where `auto` uses xterm's DOM
renderer. WebKitGTK can back WebGL with a slow path that reports no error, so
it is known by the webview: by the shell's flag
(`__FANTASTIC_SHELL__.webview`), or else by the user agent. The view's
`terminal_renderer {name?: auto | webgl | dom}` switches them live.

Seeded: 4 terminal cells with their exec agents (E65's `seeds(4)`), each
running E65's `ticker.py` through its backend's `write`, so 4 terminals stream
5 lines/s. The host-side jobs are shared by every lane.

THE MEASUREMENT, the same in every lane: `window.__measure(cid, mk)` types
`: <mk>\r` through xterm's `input` (its `onData`, as typed). The sample is from
that keystroke to two animation frames after the first render that shows the
echo, so a slow presentation path counts. In between come the page, the
WebSocket, the host, the PTY echo, the output, xterm's parse and the render.
There are 8 rounds × 4 terminals = 32 samples per renderer, 150 ms apart; one
still unanswered after 5 s counts as 5000 ms. Each renderer also gets one 1 s
rAF frame-rate sample, for information.

CHROME LANE (every OS, `rig.page(1400, 900)`):

1. `terminal_renderer` with no name answers `in_use: webgl` and a `why`
   starting `auto: not WebKitGTK`; every terminal frame body has
   `dataset.renderer === "webgl"`;
2. measure → `chrome/webgl`;
3. `window.__rProbe = 1`; `terminal_renderer {name: dom}` → within 5 s every
   terminal is `dom`, `.frame-body.webgl` is undefined and `.xterm-rows` holds
   text; `__fantasticWebgl.renders` does not move over 1 s; `__rProbe` is still
   1 (no reload); the view's `agent.yaml` holds `terminal_renderer: dom`;
4. measure → `chrome/dom`;
5. `terminal_renderer {name: auto}` → every terminal is back on `webgl`; the
   view's `agent.yaml` holds `terminal_renderer: auto`;
6. THE FLAG PATH: `Page.addScriptToEvaluateOnNewDocument` sets
   `__FANTASTIC_SHELL__ = {name: probe, os: linux, webview: webkitgtk,
   backdrop: web}`, the page opens again → `auto` answers `in_use: dom`,
   `why: auto: the shell's webview is webkitgtk`, and the terminals draw with
   DOM.

WEBKITGTK LANE (Linux only, and required there; run after the Chrome page is
closed): a real WebKitGTK webview (`_lib/webkitgtk_host.py`, gi `WebKit2` 4.1
on GTK 3, as Tauri 2 uses it, under the system `/usr/bin/python3`, in
`xvfb-run -a` with no display). With no WebKit2 4.1 the probe FAILS:
"install python3-gi gir1.2-webkit2-4.1 xvfb". On macOS and Windows the lane is
not run, and `info` says so.

7. the page with no flag → `auto` answers `in_use: dom`, `why: auto: the user
   agent is WebKitGTK's`; the terminals are `dom` (`info.webkitgtk_ua` is the
   user agent);
8. measure → `webkitgtk/dom` (= auto);
9. `terminal_renderer {name: webgl}` → measure → `webkitgtk/webgl` (information
   only); then `auto` again.

CHECKS: auto is webgl in Chrome · the switch to dom is live and complete · auto
comes back · the setting persists on the view · the shell's flag picks dom ·
every Chrome-lane sample answered · Chrome auto p95 ≤ `LATENCY_P95_MS` (250).
On Linux also: auto is dom in WebKitGTK by its user agent · WebKitGTK auto p95
≤ `LATENCY_P95_MS` · WebKitGTK auto p95 ≤ 1.5 × Chrome auto p95 + 20 ms on the
same runner.

`info.table` is `{lane: {renderer: {p50, p95, max, n, unanswered, fps}}}`. When
a latency bound fails, the numbers go on #52 and the bound stays: changing it
is the maintainer's call.

`E2E_SYSTEM_PYTHON` names the WebKitGTK host's python, for a machine whose
`/usr/bin/python3` is not the distro's (its python3-gi would not load).
