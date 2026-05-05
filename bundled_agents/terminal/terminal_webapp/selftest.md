# terminal_webapp selftest

> scopes: webapp, web
> requires: `uv sync`; running webapp serve on a known port
> out-of-scope: actual xterm rendering inside a browser (manual)

UI agent for terminal_backend. Tests get_webapp + that the served HTML
loads. Browser-side xterm behavior is manual.

## Pre-flight

```bash
cd new_codebase
rm -rf .fantastic
PORT=18902
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

### Test 1: create with upstream_id, get_webapp returns descriptor

```bash
TB=$(curl -s -X POST http://localhost:$PORT/core/call -H 'content-type: application/json' \
  -d '{"type":"create_agent","handler_module":"terminal_backend.tools"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
TW=$(curl -s -X POST http://localhost:$PORT/core/call -H 'content-type: application/json' \
  -d "{\"type\":\"create_agent\",\"handler_module\":\"terminal_webapp.tools\",\"upstream_id\":\"$TB\"}" | python -c "import json,sys;print(json.load(sys.stdin)['id'])")

curl -s -X POST "http://localhost:$PORT/$TW/call" -H 'content-type: application/json' \
  -d '{"type":"get_webapp"}' | python -m json.tool
```
Expected: JSON with `url:"/<TW>/"`, `default_width:600`, `default_height:400`, `title:"xterm"`.

### Test 2: served HTML uses xterm 6.0.0

```bash
curl -s "http://localhost:$PORT/$TW/" | grep -c "xterm@6.0.0"
```
Expected: 2 (the CSS link + the JS lib).

### Test 3: served HTML uses fantastic_transport

```bash
curl -s "http://localhost:$PORT/$TW/" | grep -c "fantastic_transport"
```
Expected: ≥ 1.

### Test 4: missing upstream_id → page shows clear error

```bash
TW2=$(curl -s -X POST http://localhost:$PORT/core/call -H 'content-type: application/json' \
  -d '{"type":"create_agent","handler_module":"terminal_webapp.tools"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
curl -s "http://localhost:$PORT/$TW2/" | grep -F "upstream_id not set"
```
Expected: matches.

### Test 5 (manual): browser xterm

Open `http://localhost:$PORT/$TW/` in a browser. Expected:
- xterm renders, prompt visible.
- Type `echo browser-works` + Enter → output appears.
- Resize the window → xterm fits.
Regression signal: garbled redraw on TUI apps after browser refresh
indicates resize-before-replay ordering broken.

### Test 6 (manual): smart autoscroll (tail vs frozen)

In the browser, with the xterm scrolled to the bottom:

1. Run `seq 1 200` in the PTY (or wait for any high-volume output).
   Expected: viewport follows the tail — last lines visible as they
   stream.

2. Scroll up in the xterm viewport (mouse wheel inside the term)
   so older lines are showing. Run `seq 1 200` again.
   Expected: viewport STAYS where you scrolled. Output keeps
   flowing into the buffer, but you keep reading old lines.

3. Scroll back to the bottom. The next output should auto-follow again.

Regression signal: viewport yanks to bottom even when you've scrolled
up to read → `tailing` gate isn't being computed from viewport scroll
position (capture-phase listener regressed or threshold too generous).

## Summary

| # | Test | Pass |
|---|------|------|
| 1 | get_webapp descriptor | |
| 2 | xterm@6.0.0 in served HTML | |
| 3 | fantastic_transport injected | |
| 4 | missing upstream_id → error message | |
| 5 (manual) | xterm renders + resizes | |
| 6 (manual) | smart autoscroll: tail vs frozen | |
