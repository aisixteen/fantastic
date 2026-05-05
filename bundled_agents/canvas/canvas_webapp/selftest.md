# canvas_webapp selftest

> scopes: webapp, web, bus
> requires: `uv sync`; running webapp serve; browser for visual + bus tests
> out-of-scope: deep iframe content (each child agent has its own selftest)

Spatial UI host. Tests get_webapp, frame filtering, drag persistence,
two-layer DOM/GL dispatch, Liquid Glass chrome, pure-streaming lifecycle
(no polling), and browser-bus integration.

## Pre-flight

```bash
cd new_codebase
rm -rf .fantastic
PORT=18904
pkill -9 -f "kernel.py serve" 2>/dev/null
uv run --active python kernel.py serve --port $PORT > /tmp/s.log 2>&1 &
SPID=$!
for i in $(seq 1 20); do grep -q "kernel up" /tmp/s.log 2>/dev/null && break; sleep 0.5; done
```

After all tests:
```bash
kill -9 $SPID 2>/dev/null; rm -rf .fantastic /tmp/s.log
```

## Tests

### Test 1: get_webapp descriptor

```bash
CB=$(curl -s -X POST http://localhost:$PORT/core/call -H 'content-type: application/json' \
  -d '{"type":"create_agent","handler_module":"canvas_backend.tools"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
CW=$(curl -s -X POST http://localhost:$PORT/core/call -H 'content-type: application/json' \
  -d "{\"type\":\"create_agent\",\"handler_module\":\"canvas_webapp.tools\",\"upstream_id\":\"$CB\"}" | python -c "import json,sys;print(json.load(sys.stdin)['id'])")

curl -s -X POST "http://localhost:$PORT/$CW/call" -H 'content-type: application/json' \
  -d '{"type":"get_webapp"}' | python -m json.tool
```
Expected: `{url, default_width:800, default_height:600, title:"canvas"}`.

### Test 2: served HTML uses explicit membership (drift guard)

```bash
HTML=$(curl -s "http://localhost:$PORT/$CW/")
echo "$HTML" | grep -qF "list_members" && echo "  uses list_members: OK"
echo "$HTML" | grep -qF "members_updated" && echo "  subscribes to members_updated: OK"
echo "$HTML" | grep -qF "add_agent" && echo "  dblclick auto-adds to membership: OK"
echo "$HTML" | grep -c "BroadcastChannel"
```
Expected: first three checks PASS; last (BroadcastChannel) 0
(canvas itself doesn't use the bus — transport.js carries it; fetched
separately). Regression signal: any of these missing → the
membership rewrite regressed and the canvas is auto-discovering again.

### Test 3 (manual, browser): explicit membership renders only members

Provision a target webapp (terminal pair) AND add it to this canvas:
```bash
TB=$(curl -s -X POST "http://localhost:$PORT/core/call" -H 'content-type: application/json' \
  -d '{"type":"create_agent","handler_module":"terminal_backend.tools"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
TW=$(curl -s -X POST "http://localhost:$PORT/core/call" -H 'content-type: application/json' \
  -d "{\"type\":\"create_agent\",\"handler_module\":\"terminal_webapp.tools\",\"upstream_id\":\"$TB\"}" | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
# IMPORTANT: explicit add — without this, the canvas shows 0 frames.
curl -s -X POST "http://localhost:$PORT/$CB/call" -H 'content-type: application/json' \
  -d "{\"type\":\"add_agent\",\"agent_id\":\"$TW\"}"
```
Open `http://localhost:$PORT/$CW/` in a browser.
Expected: ONE frame (terminal_webapp), title "xterm". Backend agents
and any UN-added html_agents stay out of frame. **Without the
add_agent call, the canvas would be empty** — that's the new model.
Regression signal: frames appear without an explicit add → auto-discover regressed.

### Test 4 (manual, browser): drag persists position

In browser, drag the terminal_webapp frame to a new spot. Then:
```bash
curl -s -X POST http://localhost:$PORT/core/call -H 'content-type: application/json' \
  -d "{\"type\":\"list_agents\"}" | python -c "import json,sys; [print(a['id'], a.get('x'), a.get('y')) for a in json.load(sys.stdin)['agents'] if 'terminal_webapp' in a['handler_module']]"
```
Expected: x/y match the drop position roughly.

### Test 5 (manual, browser): two canvases hold disjoint members

Provision a second canvas pair AND two distinct member webapps:
```bash
CB2=$(curl -s -X POST "http://localhost:$PORT/core/call" -H 'content-type: application/json' \
  -d '{"type":"create_agent","handler_module":"canvas_backend.tools"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
CW2=$(curl -s -X POST "http://localhost:$PORT/core/call" -H 'content-type: application/json' \
  -d "{\"type\":\"create_agent\",\"handler_module\":\"canvas_webapp.tools\",\"upstream_id\":\"$CB2\"}" | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
H1=$(curl -s -X POST "http://localhost:$PORT/core/call" -H 'content-type: application/json' \
  -d '{"type":"create_agent","handler_module":"html_agent.tools","html_content":"<h1>in canvas A</h1>"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
H2=$(curl -s -X POST "http://localhost:$PORT/core/call" -H 'content-type: application/json' \
  -d '{"type":"create_agent","handler_module":"html_agent.tools","html_content":"<h1>in canvas B</h1>"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
# Disjoint membership: H1 in A only, H2 in B only.
curl -s -X POST "http://localhost:$PORT/$CB/call"  -H 'content-type: application/json' -d "{\"type\":\"add_agent\",\"agent_id\":\"$H1\"}"
curl -s -X POST "http://localhost:$PORT/$CB2/call" -H 'content-type: application/json' -d "{\"type\":\"add_agent\",\"agent_id\":\"$H2\"}"
```
Open `/$CW/` in tab A and `/$CW2/` in tab B.
Expected:
- Tab A shows ONLY the "in canvas A" frame.
- Tab B shows ONLY the "in canvas B" frame.
- Adding H2 to canvas A's members (`add_agent agent_id=$H2` on `$CB`)
  → tab A's `members_updated` fires → frame appears live in A; tab B is unchanged.
Regression signal: an agent appears in both tabs without being added to
both → membership filter regressed. A canvas iframes itself or its
sibling → the self/upstream skip regressed.

### Test 6: bganim is GONE (negative drift guard)

The bganim system was removed. Particle effects come back later as
a peer GL agent.

```bash
# Verbs no longer exist — every probe should error.
curl -s -X POST "http://localhost:$PORT/$CW/call" -H 'content-type: application/json' \
  -d '{"type":"get_bganim"}' | python -m json.tool | grep -F "unknown type"
curl -s -X POST "http://localhost:$PORT/$CW/call" -H 'content-type: application/json' \
  -d '{"type":"set_bganim","source":"x"}' | python -m json.tool | grep -F "unknown type"
curl -s -X POST "http://localhost:$PORT/$CW/call" -H 'content-type: application/json' \
  -d '{"type":"reflect"}' | python -c "
import json,sys
d=json.load(sys.stdin)
ok = ('bganim_origin' not in d and
      'get_bganim' not in d.get('verbs',{}) and
      'set_bganim' not in d.get('verbs',{}))
print('PASS' if ok else f'FAIL — bganim residue: {d.get(\"verbs\")}')"
```
Expected: both grep matches; final line PASS.

### Test 7: terminal pair lifecycle + canvas membership add (programmatic — mirrors dblclick + close)

Verifies the create-pair → add-to-canvas → cascade-delete flow that
the canvas_webapp UI exercises on dblclick / "×":

```bash
# 1. dblclick create — backend first, then webapp pointing at it
TB=$(curl -s -X POST "http://localhost:$PORT/core/call" -H 'content-type: application/json' \
  -d '{"type":"create_agent","handler_module":"terminal_backend.tools"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
TW=$(curl -s -X POST "http://localhost:$PORT/core/call" -H 'content-type: application/json' \
  -d "{\"type\":\"create_agent\",\"handler_module\":\"terminal_webapp.tools\",\"upstream_id\":\"$TB\",\"x\":120,\"y\":80}" | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
echo "  created TB=$TB TW=$TW"

# 2. webapp's upstream_id points at backend
curl -s -X POST "http://localhost:$PORT/$TW/call" -H 'content-type: application/json' \
  -d '{"type":"reflect"}' | python -m json.tool | grep -F "\"upstream_id\": \"$TB\""

# 3. add_agent on the canvas backend so it appears in this canvas
curl -s -X POST "http://localhost:$PORT/$CB/call" -H 'content-type: application/json' \
  -d "{\"type\":\"add_agent\",\"agent_id\":\"$TW\"}" | python -m json.tool | grep -F '"ok": true'
curl -s -X POST "http://localhost:$PORT/$CB/call" -H 'content-type: application/json' \
  -d '{"type":"list_members"}' | python -m json.tool | grep -F "\"$TW\""

# 4. close — webapp first, then backend (mirrors × button order)
curl -s -X POST "http://localhost:$PORT/core/call" -H 'content-type: application/json' \
  -d "{\"type\":\"delete_agent\",\"id\":\"$TW\"}" | python -m json.tool | grep -F '"deleted": true'
curl -s -X POST "http://localhost:$PORT/core/call" -H 'content-type: application/json' \
  -d "{\"type\":\"delete_agent\",\"id\":\"$TB\"}" | python -m json.tool | grep -F '"deleted": true'

# 5. neither lingers in list_agents
curl -s -X POST "http://localhost:$PORT/core/call" -H 'content-type: application/json' \
  -d '{"type":"list_agents"}' | python -c "
import json, sys
d = json.load(sys.stdin)
ids = {a['id'] for a in d['agents']}
print('PASS' if '$TB' not in ids and '$TW' not in ids else f'FAIL leftover={ids & {\"$TB\",\"$TW\"}}')
"

# 6. members list is auto-pruned: the canvas frontend's close button
#    calls remove_agent BEFORE delete_agent, AND refresh() self-heals
#    by calling remove_agent on any member id whose record vanished
#    (covers any delete path — CLI, scheduler, programmatic). The CLI
#    delete above bypasses the close button, but the next refresh in
#    an open browser tab would scrub the membership; programmatic
#    callers should call remove_agent themselves for symmetry.
curl -s -X POST "http://localhost:$PORT/$CB/call" -H 'content-type: application/json' \
  -d "{\"type\":\"remove_agent\",\"agent_id\":\"$TW\"}" | python -m json.tool | grep -F '"removed":'
```
Expected: every grep matches AND final line prints `PASS`.
Regression signal: orphan agent in `list_agents` after delete → cascade
ordering broken or `delete_agent` regressed.

### Test 7b: shutdown lifecycle hook tears down PTY child

`core.delete_agent` sends `{type:"shutdown"}` to the agent before
removing the record (symmetric to the `boot` it sends on create).
`terminal_backend.shutdown` runs `_cleanup` → SIGKILLs the PTY.
Without this hook the subprocess outlives its agent record and keeps
emitting output, leaking ghost sprites in telemetry views.

```bash
TB=$(curl -s -X POST "http://localhost:$PORT/core/call" -H 'content-type: application/json' \
  -d '{"type":"create_agent","handler_module":"terminal_backend.tools"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
# capture the child pid before delete
PID=$(curl -s -X POST "http://localhost:$PORT/$TB/call" -H 'content-type: application/json' \
  -d '{"type":"reflect"}' | python -c "import json,sys;print(json.load(sys.stdin).get('pid',''))")
curl -s -X POST "http://localhost:$PORT/core/call" -H 'content-type: application/json' \
  -d "{\"type\":\"delete_agent\",\"id\":\"$TB\"}" > /dev/null
sleep 0.5
# kill -0 errors when the process is gone (zombie reaped or never existed).
if [ -n "$PID" ]; then
  kill -0 "$PID" 2>/dev/null && echo "FAIL pid $PID still alive" || echo "PASS pid $PID gone"
fi
```
Expected: `PASS pid <N> gone`. Regression signal: pid still alive →
shutdown verb missing / not invoked / `_cleanup` regressed.

### Test 8 (manual, browser): dblclick spawns pair, "×" cascades, ⟳ reloads, pan/wheel hygiene

Open `http://localhost:$PORT/$CW/` in a browser.

**Spawn + close:**
- **Dblclick** on empty canvas → a new xterm frame appears at the click
  position. (Backend `terminal_backend_xxx` + webapp `terminal_webapp_xxx`
  are both created AND the new webapp is added to THIS canvas's members.
  Other canvases are unaffected.)
- Type a command in the xterm — works (proves the upstream_id link).
- Click **×** on the frame head → both agents deleted; frame disappears.
  (Stale member id stays in `list_members` — iframe lookup hides it; can
  be scrubbed via `remove_agent`.)

**Reload button (⟳):**
- Spawn another frame. Click **⟳** on its head. The iframe reloads
  (you'll see xterm re-init briefly); other frames are unaffected.
- The button emits `reload_html` to that agent's inbox; transport.js's
  universal listener calls `location.reload()` inside the iframe.
  Same plumbing as `set_html` on an html_agent.

**Wheel over a cell:**
- Move the mouse OVER a frame body (not the header). Scroll wheel.
  Expected: scroll happens INSIDE the cell (xterm scrollback,
  chat scroll, etc.). The canvas does NOT zoom.
- Move to empty canvas. Wheel zooms (cursor-anchored).

**Drag-pan across cells:**
- Hold left-mouse on empty canvas, drag across a frame to the other
  side. Pan continues uninterrupted (frame's iframe doesn't trap
  the cursor mid-drag). On mouseup, `pointer-events: auto` returns
  to iframes — clicking inside works again.

Regression signals:
- ⟳ reloads ALL frames (or the whole page) → universal listener wired
  too aggressively.
- Wheel over a cell zooms canvas → `agent-frame` skip in wheel handler regressed.
- Drag pan stops mid-cell → `.panning .agent-frame iframe { pointer-events: none }` regressed.

### Test 9 (manual, browser): two-layer dispatch — DOM iframe + GL view

The canvas hosts two presentation layers per agent. Live agent-vis
lives in the `telemetry_pane` bundle now; add it to the canvas as
a peer to populate the GL layer. See
`bundled_agents/canvas/telemetry_pane/selftest.md` for the full
walkthrough; the canvas-side checks below confirm the dispatch.

```bash
# Sanity: empty canvas opens with no particles, no sprites.
echo "open http://localhost:$PORT/$CW/  -- expect: black scene, no foreground content"

# Now provision a DOM agent (terminal_webapp + backend) and add it.
TB=$(curl -s -X POST "http://localhost:$PORT/core/call" -H 'content-type: application/json' \
  -d '{"type":"create_agent","handler_module":"terminal_backend.tools"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
TW=$(curl -s -X POST "http://localhost:$PORT/core/call" -H 'content-type: application/json' \
  -d "{\"type\":\"create_agent\",\"handler_module\":\"terminal_webapp.tools\",\"upstream_id\":\"$TB\"}" | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
curl -s -X POST "http://localhost:$PORT/$CB/call" -H 'content-type: application/json' \
  -d "{\"type\":\"add_agent\",\"agent_id\":\"$TW\"}"

# And a GL-only agent.
TP=$(curl -s -X POST "http://localhost:$PORT/core/call" -H 'content-type: application/json' \
  -d '{"type":"create_agent","handler_module":"telemetry_pane.tools"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
curl -s -X POST "http://localhost:$PORT/$CB/call" -H 'content-type: application/json' \
  -d "{\"type\":\"add_agent\",\"agent_id\":\"$TP\"}"
```

In the browser:
- The terminal frame appears as an iframe in the DOM layer.
- Telemetry sprites appear as Three.js content in the GL layer
  behind the iframe.
- Status footer shows `N dom · M gl` member counts.

Removal teardown:
```bash
curl -s -X POST "http://localhost:$PORT/$CB/call" -H 'content-type: application/json' \
  -d "{\"type\":\"remove_agent\",\"agent_id\":\"$TP\"}"
```
Expected: telemetry sprites disappear cleanly; the terminal iframe
remains. Removing the terminal: its iframe disappears; sprites
remain. Layers are independent.

Regression signals:
- Adding a GL-only agent triggers no scene change → `installGlView`
  not wired or `get_gl_view` probe missing.
- Removing a GL agent leaves orphan sprites → `removeGlView` not
  running cleanup closures.
- Particle field reappears → bganim machinery resurrected (the
  `test_render_html_no_inline_bganim` drift-guard should have caught).

### Test 10: served HTML is Liquid Glass + streaming-only (drift guard)

The canvas chrome must stay Liquid Glass and lifecycle must be purely
event-driven (no polling). Flatten back to solid fills or sneak a
`setInterval` in and the candy/perf-feel breaks.

```bash
HTML=$(curl -s "http://localhost:$PORT/$CW/")
# Liquid Glass tokens.
echo "$HTML" | grep -qF "backdrop-filter" && echo "  glass blur: OK"
echo "$HTML" | grep -qF ".agent-frame::before" && echo "  specular layer: OK"
echo "$HTML" | grep -qF "liquid-distort" && echo "  refraction filter: OK"
# Streaming, no polling.
echo "$HTML" | grep -q "setInterval" && echo "FAIL: polling regressed" || echo "  no setInterval: OK"
echo "$HTML" | grep -qF "members_updated" && echo "  streamed members: OK"
echo "$HTML" | grep -qF "agent_deleted" && echo "  streamed deletes: OK"
# Drag-over-iframes lock.
echo "$HTML" | grep -qF "body.dragging-frame .agent-frame iframe" && echo "  drag pointer-events lock: OK"
```
Expected: every line ends with `OK`. Regression signal: any FAIL or
missing line → glass / streaming / drag fix regressed.

### Test 11 (manual, browser BUS): direct iframe-to-iframe via BroadcastChannel

Per `_kernel/reflect.browser_bus`, agents can bypass the kernel:
- Open browser devtools console on tab A (the canvas page).
- In console:
  ```js
  const ch = new BroadcastChannel('fantastic');
  ch.addEventListener('message', e => console.log('bus:', e.data));
  ```
- Open tab B (any other agent webapp) — in its devtools:
  ```js
  fantastic_transport().bus.broadcast({type:'ping', text:'hi from B'});
  ```
- Expected: tab A's console prints `bus: {type:'ping', source_id:'<B>', text:'hi from B'}`.
Regression signal: nothing logs → transport.js missing the bus or
BroadcastChannel name changed.

## Summary

| # | Test | Pass |
|---|------|------|
| 1 | get_webapp descriptor | |
| 2 | HTML uses explicit membership + dual-verb dispatch | |
| 3 (manual) | filters to webapps only | |
| 4 (manual) | drag persists x/y | |
| 5 (manual) | two canvases hold disjoint members | |
| 6 | bganim is GONE (negative drift guard) | |
| 7 | terminal-pair lifecycle (create + cascade-delete) | |
| 8 (manual) | browser dblclick spawns pair, × cascades, pan/wheel | |
| 7b | shutdown lifecycle hook kills PTY child | |
| 9 (manual) | two-layer dispatch — DOM iframe + GL view (telemetry pane) | |
| 10 | served HTML is Liquid Glass + streaming-only (drift guard) | |
| 11 (manual) | browser bus delivers across iframes | |
