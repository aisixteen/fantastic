# canvas_webapp selftest

> scopes: webapp, web, bus, cascade
> requires: `uv sync`; running `fantastic` on a known port; browser for manual tests
> out-of-scope: deep iframe content (each child agent has its own selftest)

Spatial UI host. Post-substrate-rewrite, the canvas pair (webapp +
backend) auto-wires on first boot — `canvas_webapp._boot` spawns
`canvas_backend` as its child. Membership is structural: the backend's
`add_agent handler_module=...` always spawns a NEW child (no
re-parenting of existing agents). Cascade-delete the canvas → all
members + their subtrees die in order.

## Pre-flight

```bash
cd new_codebase
rm -rf .fantastic
PORT=18904
pkill -9 -f "fantastic" 2>/dev/null
uv run --active fantastic core create_agent handler_module=web.tools port=$PORT >/dev/null
# WS verb channel is now a sub-agent of web — spawn under web's id.
WEB_ID=$(ls .fantastic/agents | grep '^web_' | head -1)
uv run --active fantastic $WEB_ID create_agent handler_module=web_ws.tools >/dev/null
uv run --active fantastic > /tmp/s.log 2>&1 &
SPID=$!
for i in $(seq 1 20); do grep -q "kernel up" /tmp/s.log 2>/dev/null && break; sleep 0.5; done

# This helper opens a one-shot WS, sends a `call` frame, prints reply.
call() {
  TARGET="$1" PAYLOAD="$2" PORT="$PORT" uv run --active python - <<'PY'
import asyncio, json, os, websockets
target = os.environ["TARGET"]; payload = json.loads(os.environ["PAYLOAD"])
port = os.environ["PORT"]
async def main():
    async with websockets.connect(f"ws://localhost:{port}/{target}/ws") as ws:
        await ws.send(json.dumps({"type":"call","target":target,"payload":payload,"id":"1"}))
        while True:
            m = json.loads(await ws.recv())
            if m.get("id") == "1" and m.get("type") in ("reply","error"):
                print(json.dumps(m.get("data"))); return
asyncio.run(main())
PY
}
```

After all tests:
```bash
kill -9 $SPID 2>/dev/null; rm -rf .fantastic /tmp/s.log
```

## Tests

### Test 1: create canvas_webapp → backend child auto-spawned

```bash
CW=$(call core '{"type":"create_agent","handler_module":"canvas_webapp.tools"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
sleep 0.3
call $CW '{"type":"reflect"}' | python -c "
import json,sys
d=json.load(sys.stdin)
ok=isinstance(d.get('upstream_id'),str) and d['upstream_id'].startswith('canvas_backend_')
print('upstream auto-set:', 'PASS' if ok else f'FAIL d={d}')
"
CB=$(call $CW '{"type":"reflect"}' | python -c "import json,sys;print(json.load(sys.stdin)['upstream_id'])")
echo "  backend id: $CB"
```
Expected: `upstream auto-set: PASS`. The backend's record lives at
`.fantastic/agents/$CW/agents/$CB/agent.json`.

### Test 2: get_webapp descriptor

```bash
call $CW '{"type":"get_webapp"}' | python -m json.tool
```
Expected: `{url, default_width:800, default_height:600, title:"canvas"}`.

### Test 3: served HTML uses explicit membership (drift guard)

```bash
HTML=$(curl -s "http://localhost:$PORT/$CW/")
echo "$HTML" | grep -qF "list_members" && echo "  uses list_members: OK"
echo "$HTML" | grep -qF "members_updated" && echo "  subscribes to members_updated: OK"
echo "$HTML" | grep -qF "add_agent" && echo "  dblclick spawns via add_agent: OK"
```
Expected: all three OK. Regression signal: any missing → membership
streaming regressed.

### Test 4: add_agent spawns a renderable child (the dblclick path)

`canvas_webapp.html` calls `add_agent handler_module=…` on the backend
when the user double-clicks. Verify the substrate plumbing:

```bash
call $CB '{"type":"add_agent","handler_module":"html_agent.tools","html_content":"<h1>hello</h1>"}' \
  | python -c "
import json,sys
d=json.load(sys.stdin)
ok=d.get('ok') is True and isinstance(d.get('member_id'),str) and len(d.get('members',[]))==1
print('add_agent: PASS' if ok else f'FAIL d={d}')
"
call $CB '{"type":"list_members"}' | python -m json.tool | grep -F '"members"'
```
Expected: `add_agent: PASS` and `list_members` shows one entry.

### Test 5: add_agent refuses non-renderable handler

```bash
call $CB '{"type":"add_agent","handler_module":"file.tools"}' | python -c "
import json,sys
d=json.load(sys.stdin)
print('refuse-non-renderable: PASS' if 'answers neither get_webapp nor get_gl_view' in d.get('error','') else f'FAIL d={d}')
"
```
Expected: `refuse-non-renderable: PASS`. The substrate cascade-deleted
the spawned-then-rejected agent so member_count stays at 1.

### Test 6: add_agent spawns terminal_webapp; auto-pairs its backend

The cascade is depth-2: canvas → terminal_webapp → terminal_backend.

```bash
call $CB '{"type":"add_agent","handler_module":"terminal_webapp.tools","x":120,"y":80}' \
  | python -c "import json,sys;print(json.load(sys.stdin)['member_id'])" > /tmp/cw_tw.id
TW=$(cat /tmp/cw_tw.id)
sleep 0.3
# terminal_webapp's _boot already created its terminal_backend child.
call $TW '{"type":"reflect"}' | python -c "
import json,sys
d=json.load(sys.stdin)
print('nested-pair-spawned: PASS' if isinstance(d.get('upstream_id'),str) else f'FAIL d={d}')
"
test -d ".fantastic/agents/$CW/agents/$CB/agents/$TW/agents" && echo "  nested terminal_backend dir present: OK"
```
Expected: `nested-pair-spawned: PASS`,
`nested terminal_backend dir present: OK`. The whole subtree is on
disk under the canvas.

### Test 7: cascade-delete the canvas → entire subtree removed

```bash
TB=$(ls .fantastic/agents/$CW/agents/$CB/agents/$TW/agents/ | head -1)
echo "  about to cascade: CW → CB → TW → TB ($TB)"
call core "{\"type\":\"delete_agent\",\"id\":\"$CW\"}" | python -m json.tool | grep -F '"deleted": true'
test ! -d ".fantastic/agents/$CW" && echo "  canvas + backend + terminal pair: ALL GONE"
```
Expected: `deleted: true` and the canvas's directory is gone with
every nested record. The cascade ran each `on_delete` deepest-first
(terminal_backend's killed the PTY) before record removal.

### Test 8: served HTML is Liquid Glass + streaming-only (drift guard)

The canvas chrome must stay Liquid Glass and lifecycle must be purely
event-driven (no polling).

```bash
# Re-spawn a canvas for this test.
CW=$(call core '{"type":"create_agent","handler_module":"canvas_webapp.tools"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
HTML=$(curl -s "http://localhost:$PORT/$CW/")
echo "$HTML" | grep -qF "backdrop-filter" && echo "  glass blur: OK"
echo "$HTML" | grep -qF ".agent-frame::before" && echo "  specular layer: OK"
echo "$HTML" | grep -qF "liquid-distort" && echo "  refraction filter: OK"
echo "$HTML" | grep -q "setInterval" && echo "FAIL: polling regressed" || echo "  no setInterval: OK"
echo "$HTML" | grep -qF "members_updated" && echo "  streamed members: OK"
echo "$HTML" | grep -qF "agent_deleted" && echo "  streamed deletes: OK"
echo "$HTML" | grep -qF "body.dragging-frame .agent-frame iframe" && echo "  drag pointer-events lock: OK"
```
Expected: every line ends with `OK`.

### Test 9 (manual, browser): dblclick spawns pair, × cascades

Open `http://localhost:$PORT/$CW/` in a browser.

- **Dblclick** on empty canvas → an xterm frame appears at the click
  position. Behind the scenes: `add_agent handler_module=terminal_webapp.tools`
  spawned the webapp; its `_boot` spawned the backend.
- Type a command in the xterm — works.
- Click **×** on the frame → cascade-delete; frame disappears, PTY
  killed. Disk records gone.
- Click **⟳** → iframe reloads (transport.js's universal `reload_html`
  listener fires `location.reload()`).

Wheel/drag hygiene:
- Wheel over a frame body → scrolls inside the frame, canvas does not zoom.
- Wheel over empty canvas → cursor-anchored zoom.
- Drag-pan crossing a frame → pan continues; on mouseup the iframe's
  `pointer-events: auto` returns.

### Test 10 (manual, browser BUS): direct iframe-to-iframe via BroadcastChannel

- Tab A (canvas), in console:
  ```js
  const ch = new BroadcastChannel('fantastic');
  ch.addEventListener('message', e => console.log('bus:', e.data));
  ```
- Tab B (any agent UI), in console:
  ```js
  fantastic_transport().bus.broadcast({type:'ping', text:'hi from B'});
  ```
- Expected: tab A logs `bus: {type:'ping', source_id:'<B>', text:'hi from B'}`.

## Summary

| # | Test | Pass |
|---|------|------|
| 1 | _boot auto-spawns canvas_backend child | |
| 2 | get_webapp descriptor | |
| 3 | HTML uses explicit membership | |
| 4 | add_agent spawns renderable child | |
| 5 | add_agent refuses non-renderable | |
| 6 | nested pair spawn (terminal_webapp + backend) | |
| 7 | cascade-delete removes entire subtree | |
| 8 | Liquid Glass + streaming-only (drift guard) | |
| 9 (manual) | dblclick + × + ⟳ + wheel/drag hygiene | |
| 10 (manual) | browser bus delivers across iframes | |
