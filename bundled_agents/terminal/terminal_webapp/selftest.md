# terminal_webapp selftest

> scopes: webapp, web, cascade
> requires: `uv sync`; ports free in 18900-18999
> out-of-scope: actual xterm rendering inside a browser (manual)

UI agent for terminal_backend. Post-substrate-rewrite, terminal_webapp
**owns** its terminal_backend as a child agent — the pair is created
automatically on first `_boot` and cascade-deleted as one unit. Tests
verify the auto-pairing, the served HTML shape, and the cascade.

## Pre-flight

```bash
cd new_codebase
rm -rf .fantastic
PORT=18902
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

### Test 1: create terminal_webapp → backend child auto-spawned

```bash
TW=$(call core '{"type":"create_agent","handler_module":"terminal_webapp.tools"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
sleep 0.3
# Reflect surfaces auto-set upstream_id pointing at the child
call $TW '{"type":"reflect"}' | python -c "
import json,sys
d=json.load(sys.stdin)
ok=isinstance(d.get('upstream_id'),str) and d['upstream_id'].startswith('terminal_backend_')
print('upstream auto-set:', 'PASS' if ok else f'FAIL d={d}')
"
# Disk check: backend record lives UNDER the webapp's directory
test -d ".fantastic/agents/$TW/agents" && ls .fantastic/agents/$TW/agents | head -1
```
Expected: `upstream auto-set: PASS`, and a `terminal_backend_<hex>`
directory under `.fantastic/agents/$TW/agents/`. The `_boot` hook
made the pair structural — no upstream_id field had to be passed.

### Test 2: get_webapp returns canvas descriptor

```bash
call $TW '{"type":"get_webapp"}' | python -m json.tool
```
Expected: JSON with `url:"/<TW>/"`, `default_width:600`,
`default_height:400`, `title:"xterm"`, and `header_buttons` listing
the autoscroll toggle.

### Test 3: served HTML uses xterm 6.0.0

```bash
curl -s "http://localhost:$PORT/$TW/" | grep -c "xterm@6.0.0"
```
Expected: 2 (the CSS link + the JS lib).

### Test 4: served HTML uses fantastic_transport

```bash
curl -s "http://localhost:$PORT/$TW/" | grep -c "fantastic_transport"
```
Expected: ≥ 1.

### Test 5: cascade delete kills the PTY child

The substrate guarantee: deleting terminal_webapp must cascade to
terminal_backend, whose `on_delete` kills the PTY. Verify the disk
records are gone post-delete (the live PTY-pid check is exercised in
the kernel `cascade` selftest scope; here we confirm the structural
cascade through HTTP).

```bash
BACKEND_ID=$(call $TW '{"type":"reflect"}' | python -c "import json,sys;print(json.load(sys.stdin)['upstream_id'])")
call core "{\"type\":\"delete_agent\",\"id\":\"$TW\"}" | python -c "
import json,sys
d=json.load(sys.stdin)
print('cascade-delete:', 'PASS' if d.get('deleted') is True else f'FAIL d={d}')"
test ! -d ".fantastic/agents/$TW" && echo "  webapp dir removed: OK"
# Backend's record was nested under webapp's, so it's gone too.
echo "  backend id $BACKEND_ID — record path was nested under webapp; cleanup is by rmtree"
```
Expected: `cascade-delete: PASS` and `webapp dir removed: OK`.

### Test 6 (manual): browser xterm

Open `http://localhost:$PORT/$TW/` in a browser. Expected:
- xterm renders, prompt visible.
- Type `echo browser-works` + Enter → output appears.
- Resize the window → xterm fits.
Regression signal: garbled redraw on TUI apps after browser refresh
indicates resize-before-replay ordering broken.

### Test 7 (manual): smart autoscroll (tail vs frozen)

In the browser, with the xterm scrolled to the bottom:

1. Run `seq 1 200` in the PTY. Expected: viewport follows the tail.
2. Scroll up in the xterm viewport. Run `seq 1 200` again.
   Expected: viewport STAYS where you scrolled.
3. Scroll back to the bottom. Next output auto-follows again.

Regression signal: viewport yanks to bottom even when scrolled up.

## Summary

| # | Test | Pass |
|---|------|------|
| 1 | _boot auto-spawns backend child | |
| 2 | get_webapp descriptor | |
| 3 | xterm@6.0.0 in served HTML | |
| 4 | fantastic_transport injected | |
| 5 | cascade-delete cleans up the pair | |
| 6 (manual) | xterm renders + resizes | |
| 7 (manual) | smart autoscroll: tail vs frozen | |
