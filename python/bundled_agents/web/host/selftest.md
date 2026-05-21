# webapp selftest

> scopes: http, ws, web, binary
> requires: `uv sync`; ports free in 18800-18899
> out-of-scope: rendering of any specific bundle's HTML (those live in
> per-webapp selftests). The WS surface itself is tested by
> `bundled_agents/web/web_ws/selftest.md`; this file covers the
> rendering-host routes (HTML, file proxy, transport.js, favicon, lock).

HTTP rendering host. Tests static routes, transport.js, binary frame
plumbing. WS verb-channel tests live in web_ws's selftest.

## Pre-flight

```bash
cd new_codebase
rm -rf .fantastic
pkill -9 -f "fantastic" 2>/dev/null
PORT=18901
uv run --active fantastic core create_agent handler_module=web.tools port=$PORT >/dev/null
# Spawn web_ws as a child of web so the `call` helper below (which
# uses WS) works end-to-end. Call create_agent on the web agent
# itself — the new agent lands under <web_id>/agents/.
WEB_ID=$(ls .fantastic/agents | grep '^web_' | head -1)
uv run --active fantastic $WEB_ID create_agent handler_module=web_ws.tools >/dev/null
uv run --active fantastic > /tmp/serve.log 2>&1 &
SPID=$!
for i in $(seq 1 20); do grep -q "kernel up" /tmp/serve.log 2>/dev/null && break; sleep 0.5; done

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
kill -9 $SPID 2>/dev/null
rm -rf .fantastic /tmp/serve.log
```

## Tests

### Test 1: GET / serves the index HTML

```bash
curl -s http://localhost:$PORT/ | python -c "
import sys
body = sys.stdin.read().lower()
assert '<!doctype' in body, 'no doctype'
assert 'fantastic' in body, 'no fantastic title'
assert 'agent tree' in body, 'no agent tree header'
print('PASS')
"
```
Expected: `PASS`. The page is rendered from
`bundled_agents/web/src/web/templates/index.html` with the dynamic
`<ul>` tree substituted.

### Test 2: WS primer round-trip (substrate metadata)

The primer is reached over WS by calling `kernel.reflect`.

```bash
call kernel '{"type":"reflect"}' | python -c "
import json, sys
d = json.loads(sys.stdin.read())
need = ['primitive','envelope','transports','well_known','tree',
        'available_bundles','binary_protocol','browser_bus','agent_count']
missing = [k for k in need if k not in d]
assert not missing, f'missing top-level: {missing}'
names = {b['name'] for b in d['available_bundles']}
for n in ('cli','web'):
    assert n in names, f'{n} missing from available_bundles'
def walk_ids(n, out):
    out.append(n['id'])
    for c in n.get('children', []):
        walk_ids(c, out)
ids = []
walk_ids(d['tree'], ids)
assert 'core' in ids
assert any(i.startswith('web_') for i in ids)
print('PASS')
"
```
Expected: `PASS`.

### Test 3: GET /_fantastic/transport.js (must contain bus + binary)

```bash
curl -s http://localhost:$PORT/_fantastic/transport.js | grep -c "BroadcastChannel('fantastic')"
curl -s http://localhost:$PORT/_fantastic/transport.js | grep -c "binaryType = 'arraybuffer'"
```
Expected: each grep returns 1.
Regression signal: 0 means transport.js lost browser-bus or binary-frame support.

### Test 4: WS `call` frame to core/reflect

```bash
call core '{"type":"reflect"}' | python -m json.tool | head -20
```
Expected: JSON containing `"sentence":"Core agent…"`, `"verbs":[…]`.
Regression signal: `Connection refused` / `404` on the WS upgrade
means the WS proxy lost its `/<id>/ws` route binding.

### Test 5: GET /<missing_id>/ → 404

```bash
curl -s -i http://localhost:$PORT/nonexistent_xxx/ | head -1
```
Expected: `HTTP/1.1 404 Not Found`.

### Test 6: GET /<backend_id>/ (no UI) → 404

```bash
TB=$(call core '{"type":"create_agent","handler_module":"terminal_backend.tools"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
curl -s -i "http://localhost:$PORT/$TB/" | head -1
```
Expected: `HTTP/1.1 404 Not Found` (terminal_backend has no webapp/index.html).

### Test 7: WS call → reply round-trip

```bash
uv run --active python -c "
import asyncio, json, websockets
async def main():
    async with websockets.connect('ws://localhost:$PORT/core/ws') as ws:
        await ws.send(json.dumps({'type':'call','target':'core','payload':{'type':'reflect'},'id':'1'}))
        for _ in range(5):
            msg = json.loads(await ws.recv())
            if msg.get('type')=='reply' and msg.get('id')=='1':
                print('OK', 'verbs' in msg.get('data',{})); break
asyncio.run(main())
"
```
Expected: prints `OK True`.

### Test 8: PID lock — `.fantastic/lock.json` holds an alive pid

```bash
# NOTE: $SPID is the `uv run` wrapper pid; the actual kernel pid is one
# child below. Read the kernel pid from the lock file itself.
KPID=$(python -c "import json; print(json.load(open('.fantastic/lock.json'))['pid'])")
python -c "
import json, os
d = json.load(open('.fantastic/lock.json'))
def alive(p):
    try: os.kill(p,0); return True
    except: return False
print('PASS' if alive(d['pid']) else f'FAIL data={d}')
"
```
Expected: `PASS`. Regression signal: file missing → `acquire_lock`
not wired into `kernel._modes._default`.

### Test 9: second daemon refuses with "another fantastic owns this dir"

A second `fantastic` invocation in the same project dir rehydrates
the same web agent → tries to acquire the same lock → refused.

```bash
out=$(uv run --active fantastic 2>&1)
echo "$out" | grep -qF "another fantastic owns this dir" && echo "PASS" || echo "FAIL: $out"
```
Expected: `PASS`. The second invocation must NOT start (its uvicorn
would have failed to bind anyway, but the lock catches the conflict
earlier with a clearer error). Original serve still running.

### Test 10: stale lock (dead pid) is overwritten on next serve

```bash
# Kill the actual kernel pid (NOT $SPID — that's `uv run` wrapper).
KPID=$(python -c "import json; print(json.load(open('.fantastic/lock.json'))['pid'])")
kill -9 $KPID 2>/dev/null
sleep 0.5
test -f .fantastic/lock.json && echo "  lock survived SIGKILL: OK"

# Spin up a fresh serve on a different port — should overwrite.
PORT2=18902
uv run --active fantastic core create_agent handler_module=web.tools port=$PORT2 >/dev/null
uv run --active fantastic > /tmp/serve2.log 2>&1 &
WRAPPER2=$!
for i in $(seq 1 30); do grep -q "kernel up" /tmp/serve2.log 2>/dev/null && break; sleep 0.3; done
sleep 0.4
python -c "
import json, os
d = json.load(open('.fantastic/lock.json'))
def alive(p):
    try: os.kill(p,0); return True
    except: return False
# Lock is PID-only; the new serve owns it iff pid is alive and != $KPID.
print('PASS' if alive(d['pid']) and d['pid'] != $KPID else f'FAIL d={d}')
"
pkill -9 -f "fantastic" 2>/dev/null
rm -f /tmp/serve2.log
```
Expected: `PASS` (new serve replaced the stale lock). Regression
signal: stale lock blocked the relaunch → `_pid_alive` check broken.

### Test 11: render_html duck-type → html_agent's record HTML served

```bash
HA=$(call core '{"type":"create_agent","handler_module":"html_agent.tools","html_content":"<h1>marker</h1>"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
HTML=$(curl -s "http://localhost:$PORT/$HA/")
echo "$HTML" | grep -qF "marker" && echo "  record content served: OK" || echo "  FAIL"
echo "$HTML" | grep -qF "_fantastic/transport.js" && echo "  transport injected: OK; PASS" || echo "  FAIL"
```
Expected: both checks PASS. Verifies that `agent_index` consults
`render_html` first and serves whatever `{html:str}` it returns.

### Test 12: file blob proxy /<file>/file/<path>

```bash
mkdir -p /tmp/wfp && echo "hello bytes" > /tmp/wfp/foo.txt
FA=$(call core '{"type":"create_agent","handler_module":"file.tools","root":"/tmp/wfp"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
# text content
out=$(curl -s -i "http://localhost:$PORT/$FA/file/foo.txt")
echo "$out" | grep -qF "hello bytes" && echo "  text body: OK" || echo "  FAIL"
echo "$out" | grep -iqF "content-type: text/plain" && echo "  text mime: OK; PASS" || echo "  FAIL"

# missing path → 404
code=$(curl -s -o /dev/null -w '%{http_code}' "http://localhost:$PORT/$FA/file/nope.txt")
[ "$code" = "404" ] && echo "  missing → 404: OK" || echo "  FAIL: code=$code"

# escape attempt → 404 (file agent's path-safety bubbles up)
code=$(curl -s -o /dev/null -w '%{http_code}' "http://localhost:$PORT/$FA/file/../../etc/passwd")
[ "$code" = "404" ] && echo "  traversal → 404: OK" || echo "  FAIL: code=$code"
rm -rf /tmp/wfp
```
Expected: every grep + 404 check PASS. Replaces the old
`content_alias_file` registry — any html_agent can now do
`<img src="/<fa>/file/imgs/x.png">` without registering anything.

### Test 13: WS binary frame round-trip (bytes)

```bash
uv run --active python -c "
import asyncio, json, struct, websockets
async def main():
    body = b'\\x00\\x01\\x02\\x03'
    async with websockets.connect('ws://localhost:$PORT/core/ws') as ws:
        # Send a 'call' frame with bytes — text wrapper is fine for this test
        # because we're checking the SERVER → BROWSER binary path. Trigger
        # the server to emit something with bytes by adding to its inbox via emit.
        # Simpler: drain mode — the watcher receives any agent_created event.
        # Just verify binary frame DECODES correctly by emitting bytes via a
        # synthetic kernel.send — needs a special test agent.
        # For this selftest we just verify the WS accepted binary mode by checking
        # that ws.binaryType is set on the JS side (covered by test 3 grep).
        print('SKIP: needs in-process bytes producer; covered by pytest test_binary_protocol')
asyncio.run(main())
"
```
Expected: `SKIP …` (the round-trip is covered by pytest unit test
`test_binary_protocol.py`; this slot is reserved for live binary if a
binary-emitting agent is registered).

### Test 14: external traffic carries `sender = web_agent_id` in state events

Browser-originated calls have no agent context (the WS handler runs
outside any handler dispatch), so without help `_current_sender`
would be None and telemetry rays would have nowhere to start. The
proxy + HTTP routes set the contextvar to the webapp's own
`web_agent_id` so external traffic visually originates from the
webapp sprite.

```bash
WEB=$(call core '{"type":"list_agents"}' | python -c "
import json,sys
agents = json.loads(sys.stdin.read())['agents']
print(next((a['id'] for a in agents if a['handler_module']=='web.tools'), ''))
")
uv run --active python -c "
import asyncio, json, websockets
async def main():
    async with websockets.connect(f'ws://localhost:$PORT/core/ws') as obs:
        await obs.send(json.dumps({'type':'state_subscribe'}))
        async with websockets.connect(f'ws://localhost:$PORT/core/ws') as caller:
            await caller.send(json.dumps({'type':'call','target':'core','payload':{'type':'list_agents'},'id':'1'}))
            sends = []
            try:
                async with asyncio.timeout(2):
                    while True:
                        msg = json.loads(await obs.recv())
                        if msg.get('type') == 'state_event' and msg.get('kind') == 'send' and msg.get('agent_id') == 'core':
                            sends.append(msg)
            except TimeoutError:
                pass
            senders = {s.get('sender') for s in sends}
            ok = '$WEB' in senders
            print('PASS' if ok else f'FAIL senders={senders}')
asyncio.run(main())
"
```
Expected: `PASS`. Regression signal: senders empty / None → proxy
stopped tagging external dispatches; telemetry rays will silently
drop because addRay(null, …) finds no source sprite.

## Summary

| # | Test | Pass |
|---|------|------|
| 1 | GET / serves index HTML | |
| 2 | WS kernel.reflect primer round-trip | |
| 3 | transport.js has bus + binary | |
| 4 | WS call frame → core/reflect | |
| 5 | /<missing>/ → 404 | |
| 6 | /<backend>/ (no UI) → 404 | |
| 7 | WS call → reply | |
| 8 | PID lock written | |
| 9 | second serve refuses (live lock) | |
| 10 | stale lock overwritten on relaunch | |
| 11 | render_html → html_agent record served | |
| 12 | /<file>/file/<path> blob proxy | |
| 13 | WS binary (skip; pytest covers) | |
| 14 | external WS calls tag state events with web_agent_id | |
