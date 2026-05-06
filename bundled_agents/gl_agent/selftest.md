# gl_agent selftest

> scopes: kernel, http, web
> requires: `uv sync`; `kernel.py serve` running for HTTP tests
> out-of-scope: actual rendering of the GL view (manual, browser)

GL-view-as-a-record. Mirror of `html_agent` for WebGL content. The
agent's `gl_source` field is a JS function body that the canvas host
runs via `new Function('THREE','scene','t','onFrame','cleanup',
source)`. Adding a gl_agent instance to a canvas via `add_agent`
installs its source as a per-frame ticking peer in the canvas's
WebGL scene — without scaffolding a Python bundle.

## Pre-flight

```bash
rm -rf .fantastic
PORT=18908
pkill -9 -f "kernel.py serve" 2>/dev/null
uv run --active python kernel.py serve --port $PORT > /tmp/gl.log 2>&1 &
SPID=$!
for i in $(seq 1 30); do grep -q "kernel up" /tmp/gl.log 2>/dev/null && break; sleep 0.3; done

call() { curl -s -X POST "http://localhost:$PORT/$1/call" -H 'content-type: application/json' -d "$2"; }
```

After all tests:
```bash
kill -9 $SPID 2>/dev/null; rm -rf .fantastic /tmp/gl.log
```

## Tests

### Test 1: create + reflect

```bash
GL=$(call core '{"type":"create_agent","handler_module":"gl_agent.tools","gl_source":"// hello","title":"hi","display_name":"DemoVis"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
echo "GL=$GL"
call $GL '{"type":"reflect"}' | python -c "
import json, sys
d = json.loads(sys.stdin.read(), strict=False)
ok = d['id'] == '$GL' and d['display_name'] == 'DemoVis' and d['title'] == 'hi' and d['source_bytes'] >= 8
print('PASS' if ok else f'FAIL d={d}')
"
```
Expected: `PASS`.

### Test 2: get_gl_view returns canvas-host envelope

```bash
call $GL '{"type":"get_gl_view"}' | python -c "
import json, sys
d = json.loads(sys.stdin.read(), strict=False)
ok = d == {'source': '// hello', 'title': 'hi'}
print('PASS' if ok else f'FAIL d={d}')
"
```
Expected: `PASS`.
Regression signal: extra fields (e.g. accidental `display_name`
leak) → the contract diverged from `telemetry_pane.get_gl_view`,
canvas dispatch may misbehave.

### Test 3: title falls back display_name → id

```bash
GL2=$(call core '{"type":"create_agent","handler_module":"gl_agent.tools","gl_source":"x","display_name":"OnlyDN"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
call $GL2 '{"type":"get_gl_view"}' | python -c "
import json, sys
d = json.loads(sys.stdin.read(), strict=False)
print('PASS-display-name' if d['title'] == 'OnlyDN' else f'FAIL d={d}')
"

GL3=$(call core '{"type":"create_agent","handler_module":"gl_agent.tools","gl_source":"x"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
call $GL3 '{"type":"get_gl_view"}' | python -c "
import json, sys
d = json.loads(sys.stdin.read(), strict=False)
print('PASS-id-fallback' if d['title'] == '$GL3' else f'FAIL d={d}')
"
```
Expected: `PASS-display-name` then `PASS-id-fallback`.

### Test 4: set_gl_source updates the record

```bash
call $GL '{"type":"set_gl_source","source":"// v2","title":"hi-v2"}' | python -m json.tool | grep -F '"ok": true'
call $GL '{"type":"get_gl_view"}' | python -c "
import json, sys
d = json.loads(sys.stdin.read(), strict=False)
ok = d['source'] == '// v2' and d['title'] == 'hi-v2'
print('PASS' if ok else f'FAIL d={d}')
"
```
Expected: `\"ok\": true` then `PASS`.
Note: a canvas already hosting this gl_agent does NOT auto-reinstall
on `set_gl_source` — operator must remove + re-add the agent on the
canvas (or refresh the tab) to pick up the new body. Documented
behavior; mirrors the symmetric note in `tools.py`.

### Test 5: set_gl_source requires a string

```bash
call $GL '{"type":"set_gl_source","source":42}' | grep -qF "source (str) required" && echo "PASS" || echo "FAIL"
```

### Test 6: get_gl_source returns raw stored body

```bash
call $GL '{"type":"get_gl_source"}' | python -c "
import json, sys
d = json.loads(sys.stdin.read(), strict=False)
print('PASS' if d == {'source': '// v2'} else f'FAIL d={d}')
"
```
Expected: `PASS`. Regression signal: this verb growing a `title`
field would conflate the raw-body view with the canvas envelope.

### Test 7: canvas_backend.add_agent accepts a gl_agent (dual-verb gate)

This is the whole point of the abstraction — drop-in GL views with
no per-bundle scaffolding. `canvas_backend._add_agent` probes
`get_webapp` AND `get_gl_view`; either passing satisfies the gate.

```bash
CB=$(call core '{"type":"create_agent","handler_module":"canvas_backend.tools"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
call $CB "{\"type\":\"add_agent\",\"agent_id\":\"$GL\"}" | python -c "
import json, sys
d = json.loads(sys.stdin.read(), strict=False)
ok = d.get('ok') is True and '$GL' in d['members']
print('PASS' if ok else f'FAIL d={d}')
"
```
Expected: `PASS`.
Regression signal: gl_agent rejected → the dual-verb dispatch in
`canvas_backend._add_agent` regressed back to webapp-only.

### Test 8: unknown verb errors cleanly

```bash
call $GL '{"type":"garbage"}' | grep -qF "unknown type" && echo "PASS" || echo "FAIL"
```

### Test 9 (manual, browser): inline GL source actually animates the canvas

Provision a canvas pair + a gl_agent carrying a tiny rotating-cube
source. Adding it to the canvas should make the cube appear in the
WebGL layer (no per-bundle code).

```bash
CW=$(call core "{\"type\":\"create_agent\",\"handler_module\":\"canvas_webapp.tools\",\"upstream_id\":\"$CB\"}" | python -c "import json,sys;print(json.load(sys.stdin)['id'])")

# A 12-line GL source — a single rotating cube. Tests onFrame + cleanup.
SRC=$(python -c "
print('''
const geom = new THREE.BoxGeometry(8, 8, 8);
const mat = new THREE.MeshNormalMaterial();
const cube = new THREE.Mesh(geom, mat);
scene.add(cube);
onFrame((time) => { cube.rotation.x = time * 0.7; cube.rotation.y = time * 0.9; });
cleanup.push(() => { scene.remove(cube); geom.dispose(); mat.dispose(); });
''')")

CUBE=$(call core "{\"type\":\"create_agent\",\"handler_module\":\"gl_agent.tools\",\"display_name\":\"cube\",\"gl_source\":$(python -c "import json,sys; print(json.dumps(sys.stdin.read()))" <<<"$SRC")}" | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
call $CB "{\"type\":\"add_agent\",\"agent_id\":\"$CUBE\"}" >/dev/null

echo "open http://localhost:$PORT/$CW/  — expect a normal-shaded rotating cube in the WebGL layer"
```

Then in the browser:
1. The cube appears within ~100 ms (refresh probes after add).
2. It rotates continuously (proves `onFrame` is wired).
3. Remove it: `call $CB "{\"type\":\"remove_agent\",\"agent_id\":\"$CUBE\"}"` — the cube
   disappears (proves `cleanup` ran: scene.remove + dispose).
4. Refresh tab — cube re-appears (canvas re-installs from `get_gl_view`).

Regression signals:
- Cube doesn't appear → `add_agent` dual-verb gate broke or
  `installGlView` not running.
- Cube stays after `remove_agent` → cleanup closures didn't fire;
  scene leaks geometry/material.
- Cube freezes → `onFrame` registry not pumping the cb each rAF
  tick.

## Summary

| # | Test | Pass |
|---|------|------|
| 1 | create + reflect | |
| 2 | get_gl_view returns {source, title} | |
| 3 | title fallback display_name → id | |
| 4 | set_gl_source updates record | |
| 5 | set_gl_source rejects non-string | |
| 6 | get_gl_source returns raw body | |
| 7 | canvas_backend accepts gl_agent | |
| 8 | unknown verb errors cleanly | |
| 9 (manual) | inline cube source animates on canvas | |
