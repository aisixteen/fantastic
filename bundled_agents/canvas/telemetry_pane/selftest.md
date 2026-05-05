# telemetry_pane selftest

> scopes: webapp, web
> requires: `uv sync`; running webapp serve. Live agent-vis is
> manual / browser.
> out-of-scope: kernel state-stream itself (covered by
> `tests/test_kernel_state_stream.py` and the proxy tests).

A peer GL agent. Subscribes to the kernel state stream and renders
each agent as a Three.js sprite with name + backlog dots + send/emit
blip. Add it to any canvas via `canvas_backend.add_agent` to get the
live system-pulse view.

The agent-vis source previously lived inline in
`canvas_webapp/index.html`; it now ships in
`telemetry_pane/glview.js` and reaches a canvas only through the
generic GL-view dispatch on `add_agent`.

## Pre-flight

```bash
cd new_codebase
rm -rf .fantastic
PORT=18935
pkill -9 -f "kernel.py serve" 2>/dev/null; sleep 0.5
uv run --active python kernel.py serve --port $PORT > /tmp/tp.log 2>&1 &
SPID=$!
for i in $(seq 1 20); do grep -q "kernel up" /tmp/tp.log 2>/dev/null && break; sleep 0.5; done
call() { curl -s -X POST "http://localhost:$PORT/$1/call" -H 'content-type: application/json' -d "$2"; }
```

After all tests:
```bash
kill -9 $SPID 2>/dev/null; rm -rf .fantastic /tmp/tp.log
```

## Tests

### Test 1: reflect lists `get_gl_view`

```bash
TP=$(call core '{"type":"create_agent","handler_module":"telemetry_pane.tools"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
call $TP '{"type":"reflect"}' | python -m json.tool | grep -F "get_gl_view"
```
Expected: matches.

### Test 2: get_gl_view returns source + title

```bash
call $TP '{"type":"get_gl_view"}' | python -c "
import json,sys
d=json.load(sys.stdin)
print('  source bytes:', len(d.get('source','')))
print('  title:', d.get('title'))
"
```
Expected: source ≥ 1000 bytes; title `'telemetry'`.

### Test 3: GL view source carries the right tokens

```bash
call $TP '{"type":"get_gl_view"}' | python -c "
import json,sys
src=json.load(sys.stdin)['source']
ok = (
    'THREE.Sprite' in src and
    'THREE.CanvasTexture' in src and
    't.subscribeState' in src and
    'cleanup.push(' in src
)
print('PASS' if ok else 'FAIL')"
```
Expected: PASS.

### Test 4: source does NOT call kernel verbs (drift guard)

```bash
call $TP '{"type":"get_gl_view"}' | python -c "
import json,sys
src=json.load(sys.stdin)['source']
forbidden = ['t.call(', 't.send(', 't.emit(']
hits = [tok for tok in forbidden if tok in src]
print('PASS' if not hits else 'FAIL ' + repr(hits))"
```
Expected: PASS. The render path must be a pure consumer of the
kernel state stream — no calls back through the substrate, so a
self-visualizing instance does not feedback-loop.

### Test 5: add_agent on a canvas accepts the GL-only telemetry pane

```bash
CB=$(call core '{"type":"create_agent","handler_module":"canvas_backend.tools"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
call $CB "{\"type\":\"add_agent\",\"agent_id\":\"$TP\"}" | python -m json.tool | grep -F '"ok": true'
call $CB '{"type":"list_members"}' | python -c "import json,sys;print('member?', '$TP' in json.load(sys.stdin)['members'])"
```
Expected: first grep matches; second prints `member? True`.

### Test 6: WS pseudo-clients do NOT appear in the state stream

Each browser tab + iframe opens its own WS to its host agent.
The webapp proxy registers a `_ws_*` pseudo-client per connection
(`kernel._ensure_inbox` + `kernel.watch`), but those are
transport-layer endpoints — not agents. The state stream MUST filter
them out so they don't mint phantom sprites in the agent-vis.

```bash
uv run --active python -c "
import asyncio, json, websockets

async def main():
    PORT = $PORT
    # Open multiple WS connections — each mints a fresh _ws_* client
    # in the proxy. None should appear in the state stream.
    extra1 = await websockets.connect(f'ws://localhost:{PORT}/core/ws')
    extra2 = await websockets.connect(f'ws://localhost:{PORT}/core/ws')
    try:
        async with websockets.connect(f'ws://localhost:{PORT}/core/ws') as obs:
            await obs.send(json.dumps({'type':'state_subscribe'}))
            ws_in_stream = []
            real_in_stream = []
            try:
                async with asyncio.timeout(2):
                    while True:
                        msg = json.loads(await obs.recv())
                        ids = []
                        if msg.get('type') == 'state_snapshot':
                            ids = [a['agent_id'] for a in msg['agents']]
                        elif msg.get('type') == 'state_event':
                            ids = [msg.get('agent_id', '')]
                        for aid in ids:
                            (ws_in_stream if aid.startswith('_ws_')
                             else real_in_stream).append(aid)
            except TimeoutError:
                pass
            print('  real agents seen:', len(set(real_in_stream)))
            print('  _ws_* phantoms in stream:', ws_in_stream)
            print('PASS' if not ws_in_stream else 'FAIL')
    finally:
        await extra1.close(); await extra2.close()

asyncio.run(main())
"
```
Expected: `_ws_* phantoms in stream: []` and `PASS`. Drift guard
against `kernel._fanout` ever forgetting to filter non-agent
watchers.

### Test 7 (manual, browser): live agent-vis sprites

Provision a canvas pair + the telemetry pane, open the canvas in a
browser, and observe the live-pulse view.

```bash
CW=$(call core "{\"type\":\"create_agent\",\"handler_module\":\"canvas_webapp.tools\",\"upstream_id\":\"$CB\"}" | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
echo "open http://localhost:$PORT/$CW/ in a browser"
```

In the browser:

1. **Boot snapshot**: ~6 sprites appear (one per running agent — core,
   cli, webapp_*, $CB, $CW, $TP). Each shows display_name + 0 dots.
2. **Live add**: in another shell:
   ```bash
   NEW=$(call core '{"type":"create_agent","handler_module":"file.tools","display_name":"hello-fs"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
   ```
   A new sprite labeled `hello-fs` appears within ~100 ms.
3. **Traffic blip**: hit the new agent.
   ```bash
   call $NEW '{"type":"reflect"}' >/dev/null
   ```
   The `hello-fs` sprite's border briefly flashes blue (kind='send'),
   AND core's sprite blips too.
4. **Backlog dots + drain**: drive a quick burst.
   ```bash
   for i in $(seq 1 12); do call cli '{"type":"token","text":"x"}' & done; wait
   ```
   cli's sprite shows several filled dots briefly, then drains back.
5. **Live remove**:
   ```bash
   call core "{\"type\":\"delete_agent\",\"id\":\"$NEW\"}" >/dev/null
   ```
   The sprite disappears. Other sprites do NOT reflow; the slot
   stays empty until reclaimed by the next new agent.
6. **Pan/zoom**: drag canvas, scroll-wheel. Sprites move with iframes
   (shared world coordinates).
7. **Refresh**: reload tab. Sprites re-bootstrap from a fresh
   `state_snapshot` after canvas_webapp re-installs the GL view.
8. **Water wobble**: every sprite drifts in a slow independent
   sin/cos orbit (~0.5 wu × 0.4 wu, ~5.5s period, random phase per
   sprite). Different sprites should never beat in unison.
9. **Sender→receiver wires**: drive a real agent-to-agent call.
   ```bash
   call $TP '{"type":"reflect"}' >/dev/null
   ```
   When the kernel `_fanout` reports a `sender` (set via
   `_current_sender` contextvar around handler dispatch), the GL
   view draws a fat 4-layer translucent wire from sender's sprite
   to recipient's sprite. The browser's external HTTP/WS calls are
   tagged with the webapp's own id by the proxy, so even pure
   browser-driven traffic produces wires.
10. **Traveling pulse**: along each wire, a small bright glow box
    sprints from sender → receiver in ~0.18s, fading near the end.
11. **Messages pane (right)**: a tall vertical sprite to the right
    of the agent grid shows the last 10 traffic events: `sender →
    target [kind]` tinted cyan/mint, plus a trimmed payload summary.
    Bytes show as `<bytes:N>`; long payloads truncate with `…`.
12. **Remove the pane**:
   ```bash
   call $CB "{\"type\":\"remove_agent\",\"agent_id\":\"$TP\"}" >/dev/null
   ```
   All telemetry sprites + wires + pulses + the messages pane
   disappear (cleanup closures run, textures + materials disposed).
   The canvas's WebGL scene is empty again. Other DOM iframes
   (e.g. terminal_webapp) are unaffected.

Regression signals:
- New agent doesn't appear → state stream not flowing OR cleanup ran
  too eagerly.
- Sprite stays after delete → 'removed' kind handler regressed.
- Removing telemetry pane leaves orphan sprites → cleanup closures
  regressed.
- Sprites all sync into one wave → wobble random phase regressed.
- Pure browser-driven calls produce no wires → webapp proxy stopped
  tagging external traffic with its `web_agent_id`.
- Messages pane stuck on a single payload / never updates → kernel
  `summary` field on state events missing.

## Summary

| # | Test | Pass |
|---|------|------|
| 1 | reflect lists get_gl_view | |
| 2 | get_gl_view returns source + title | |
| 3 | source carries THREE.Sprite/CanvasTexture/subscribeState/cleanup | |
| 4 | source does NOT call kernel verbs (no feedback loop) | |
| 5 | canvas_backend.add_agent accepts GL-only pane | |
| 6 (manual) | live add/remove/blip/drain/refresh/teardown via browser | |
| 6.x (manual) | water wobble + sender→receiver wires + traveling pulses + messages pane | |
