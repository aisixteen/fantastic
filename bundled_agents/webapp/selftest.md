# webapp selftest

> scopes: http, ws, web, binary
> requires: `uv sync`; ports free in 18800-18899
> out-of-scope: rendering of any specific bundle's HTML (those live in
> per-webapp selftests)

HTTP + WS transport. Tests routes, transport.js, binary frame protocol.

## Pre-flight

```bash
cd new_codebase
rm -rf .fantastic
pkill -9 -f "kernel.py serve" 2>/dev/null
PORT=18901
uv run --active python kernel.py serve --port $PORT > /tmp/serve.log 2>&1 &
SPID=$!
for i in $(seq 1 20); do grep -q "kernel up" /tmp/serve.log 2>/dev/null && break; sleep 0.5; done
```

After all tests:
```bash
kill -9 $SPID 2>/dev/null
rm -rf .fantastic /tmp/serve.log
```

## Tests

### Test 1: GET /_kernel/reflect (primer is bootstrap-complete)

The primer must carry every URL/transport/bundle/agent a remote
caller needs to issue its first send WITHOUT reading source.

```bash
curl -s http://localhost:$PORT/_kernel/reflect | python -c "
import json, sys
d = json.load(sys.stdin)
need = ['primitive','envelope','transports','well_known','agents',
        'available_bundles','binary_protocol','browser_bus']
missing = [k for k in need if k not in d]
assert not missing, f'missing top-level: {missing}'
# webapp must augment with http + ws specifics
assert 'http' in d['transports'] and 'ws' in d['transports']
assert d['transports']['http']['agent_call'].startswith('POST ')
assert '<agent_id>/call' in d['transports']['http']['agent_call']
assert d['transports']['ws']['url'].startswith('ws://')
# misleading top-level fields must NOT be back
assert 'send_syntax' not in d, 'send_syntax leaked back to top level'
assert 'example' not in d, 'example leaked back to top level'
# available_bundles surfaces installable bundles
names = {b['name'] for b in d['available_bundles']}
for n in ('core','cli','webapp'):
    assert n in names, f'{n} missing from available_bundles'
# agents lists running ids
ids = {a['id'] for a in d['agents']}
for n in ('core','cli'):
    assert n in ids, f'{n} not in running agents'
print('PASS')
"
```
Expected: `PASS`.
Regression signal: any AssertionError → primer drifted away from
self-bootstrap. The most common cause is a kernel/webapp refactor
that removed/renamed a top-level key or transport.

### Test 2: GET /_agents

```bash
curl -s http://localhost:$PORT/_agents | python -m json.tool
```
Expected: `{"agents":[…]}` containing core, cli, the spawned webapp.

### Test 3: GET /_fantastic/transport.js (must contain bus + binary)

```bash
curl -s http://localhost:$PORT/_fantastic/transport.js | grep -c "BroadcastChannel('fantastic')"
curl -s http://localhost:$PORT/_fantastic/transport.js | grep -c "binaryType = 'arraybuffer'"
```
Expected: each grep returns 1.
Regression signal: 0 means transport.js lost browser-bus or binary-frame support.

### Test 4: POST /core/call routes to handler

```bash
curl -s -X POST http://localhost:$PORT/core/call -H 'content-type: application/json' \
  -d '{"type":"reflect"}'
```
Expected: JSON containing `"sentence":"Core agent…`, `"verbs":[…]`.

### Test 5: GET /<missing_id>/ → 404

```bash
curl -s -i http://localhost:$PORT/nonexistent_xxx/ | head -1
```
Expected: `HTTP/1.1 404 Not Found`.

### Test 6: GET /<backend_id>/ (no UI) → 404

```bash
TB=$(curl -s -X POST http://localhost:$PORT/core/call -H 'content-type: application/json' \
  -d '{"type":"create_agent","handler_module":"terminal_backend.tools"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
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

### Test 8: serve lock — `.fantastic/lock.json` holds an alive pid + port

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
ok = alive(d['pid']) and d['port']==$PORT
print('PASS' if ok else f'FAIL data={d}')
"
```
Expected: `PASS`. Regression signal: file missing →
`acquire_serve_lock` not wired into `cmd_serve`.

### Test 9: second serve refuses with "kernel already running"

```bash
out=$(uv run --active python kernel.py serve --port 18999 2>&1)
echo "$out" | grep -qF "kernel already running" && echo "PASS" || echo "FAIL: $out"
```
Expected: `PASS`. The second serve must NOT start (its uvicorn would
have failed to bind anyway, but the lock catches the conflict earlier
with a clearer error). Original serve still running unaffected.

### Test 10: stale lock (dead pid) is overwritten on next serve

```bash
# Kill the actual kernel pid (NOT $SPID — that's `uv run` wrapper).
KPID=$(python -c "import json; print(json.load(open('.fantastic/lock.json'))['pid'])")
kill -9 $KPID 2>/dev/null
sleep 0.5
test -f .fantastic/lock.json && echo "  lock survived SIGKILL: OK"

# Spin up a fresh serve on a different port — should overwrite.
PORT2=18902
uv run --active python kernel.py serve --port $PORT2 > /tmp/serve2.log 2>&1 &
WRAPPER2=$!
for i in $(seq 1 30); do grep -q "kernel up" /tmp/serve2.log 2>/dev/null && break; sleep 0.3; done
sleep 0.4
python -c "
import json, os
d = json.load(open('.fantastic/lock.json'))
def alive(p):
    try: os.kill(p,0); return True
    except: return False
print('PASS' if alive(d['pid']) and d['port']==$PORT2 else f'FAIL d={d}')
"
pkill -9 -f "kernel.py serve" 2>/dev/null
rm -f /tmp/serve2.log
```
Expected: `PASS` (new serve replaced the stale lock). Regression
signal: stale lock blocked the relaunch → `_pid_alive` check broken.

### Test 11: render_html duck-type → html_agent's record HTML served

```bash
HA=$(curl -s -X POST "http://localhost:$PORT/core/call" -H 'content-type: application/json' \
  -d '{"type":"create_agent","handler_module":"html_agent.tools","html_content":"<h1>marker</h1>"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
HTML=$(curl -s "http://localhost:$PORT/$HA/")
echo "$HTML" | grep -qF "marker" && echo "  record content served: OK" || echo "  FAIL"
echo "$HTML" | grep -qF "_fantastic/transport.js" && echo "  transport injected: OK; PASS" || echo "  FAIL"
```
Expected: both checks PASS. Verifies that `agent_index` consults
`render_html` first and serves whatever `{html:str}` it returns.

### Test 12: file blob proxy /<file>/file/<path>

```bash
mkdir -p /tmp/wfp && echo "hello bytes" > /tmp/wfp/foo.txt
FA=$(curl -s -X POST "http://localhost:$PORT/core/call" -H 'content-type: application/json' \
  -d '{"type":"create_agent","handler_module":"file.tools","root":"/tmp/wfp"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
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
WEB=$(curl -s "http://localhost:$PORT/_agents" | python -c "
import json,sys
agents = json.load(sys.stdin)['agents']
print(next((a['id'] for a in agents if a['handler_module']=='webapp.tools'), ''))
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
| 1 | /_kernel/reflect contains primer | |
| 2 | /_agents lists agents | |
| 3 | transport.js has bus + binary | |
| 4 | POST /core/call routes | |
| 5 | /<missing>/ → 404 | |
| 6 | /<backend>/ (no UI) → 404 | |
| 7 | WS call → reply | |
| 8 | serve lock written with pid+port | |
| 9 | second serve refuses (live lock) | |
| 10 | stale lock overwritten on relaunch | |
| 11 | render_html → html_agent record served | |
| 12 | /<file>/file/<path> blob proxy | |
| 13 | WS binary (skip; pytest covers) | |
| 14 | external WS/HTTP calls tag state events with web_agent_id | |
