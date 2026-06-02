# webapp selftest

> scopes: http, ws, web, binary
> requires: `uv sync`; ports free in 18800-18899
> out-of-scope: rendering of any specific bundle's HTML (those live in
> per-webapp selftests). The WS surface itself is tested by
> `bundled_agents/web/web_ws/selftest.md`; this file covers the
> rendering-host routes (HTML, file proxy, favicon, lock).

HTTP rendering host. Tests static routes and the file proxy. WS
verb-channel tests live in web_ws's selftest.

## Pre-flight

```bash
cd new_codebase
rm -rf .fantastic
pkill -9 -f "fantastic" 2>/dev/null
PORT=18901
uv run --active fantastic fs_loader create_agent handler_module=web.tools port=$PORT >/dev/null
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

### Test 2: WS reflect round-trip (uniform identity + tree)

`reflect` is the one uniform discovery verb, reached over WS by calling
`kernel.reflect`. The reply is the root agent's identity plus its
`tree`; the old primer keys (`transports`, `available_bundles`,
`agent_count`, …) are gone — that prose moved into the root readme
(`reflect readme=true`). Add `bundles=all` to get the catalog.

```bash
call kernel '{"type":"reflect"}' | python -c "
import json, sys
d = json.loads(sys.stdin.read())
assert d.get('id') == 'fs_loader', f'id={d.get(\"id\")}'
assert 'sentence' in d and 'tree' in d, f'keys={list(d)}'
# Old primer keys must be gone.
gone = [k for k in ('transports','primitive','envelope','browser_bus',
                    'binary_protocol','agent_count','available_bundles',
                    'well_known') if k in d]
assert not gone, f'stale primer keys leaked: {gone}'
def walk_ids(n, out):
    out.append(n['id'])
    for c in n.get('children', []):
        walk_ids(c, out)
ids = []
walk_ids(d['tree'], ids)
assert 'fs_loader' in ids
assert any(i.startswith('web_') for i in ids)
print('PASS')
"
```
Expected: `PASS`.

### Test 3: WS `call` frame to fs_loader/reflect

```bash
call fs_loader '{"type":"reflect"}' | python -m json.tool | head -20
```
Expected: JSON for the `fs_loader` root — `"id":"fs_loader"`, a
`"sentence":…`, and `"verbs":{…}`. Regression signal: `Connection
refused` / `404` on the WS upgrade means the WS proxy lost its
`/<id>/ws` route binding.

### Test 4: GET /<missing_id>/ → 404

```bash
curl -s -i http://localhost:$PORT/nonexistent_xxx/ | head -1
```
Expected: `HTTP/1.1 404 Not Found`.

### Test 5: GET /<backend_id>/ (no UI) → 404

```bash
TB=$(call fs_loader '{"type":"create_agent","handler_module":"terminal_backend.tools"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
curl -s -i "http://localhost:$PORT/$TB/" | head -1
```
Expected: `HTTP/1.1 404 Not Found` (terminal_backend has no webapp/index.html).

### Test 6: WS call → reply round-trip

```bash
uv run --active python -c "
import asyncio, json, websockets
async def main():
    async with websockets.connect('ws://localhost:$PORT/fs_loader/ws') as ws:
        await ws.send(json.dumps({'type':'call','target':'fs_loader','payload':{'type':'reflect'},'id':'1'}))
        for _ in range(5):
            msg = json.loads(await ws.recv())
            if msg.get('type')=='reply' and msg.get('id')=='1':
                print('OK', 'verbs' in msg.get('data',{})); break
asyncio.run(main())
"
```
Expected: prints `OK True`.

### Test 7: PID lock — `.fantastic/lock.json` holds an alive pid

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

### Test 8: second daemon refuses with "another fantastic owns this dir"

A second `fantastic` invocation in the same project dir rehydrates
the same web agent → tries to acquire the same lock → refused.

```bash
out=$(uv run --active fantastic 2>&1)
echo "$out" | grep -qF "another fantastic owns this dir" && echo "PASS" || echo "FAIL: $out"
```
Expected: `PASS`. The second invocation must NOT start (its uvicorn
would have failed to bind anyway, but the lock catches the conflict
earlier with a clearer error). Original serve still running.

### Test 9: stale lock (dead pid) is overwritten on next serve

```bash
# Kill the actual kernel pid (NOT $SPID — that's `uv run` wrapper).
KPID=$(python -c "import json; print(json.load(open('.fantastic/lock.json'))['pid'])")
kill -9 $KPID 2>/dev/null
sleep 0.5
test -f .fantastic/lock.json && echo "  lock survived SIGKILL: OK"

# Spin up a fresh serve on a different port — should overwrite.
PORT2=18902
uv run --active fantastic fs_loader create_agent handler_module=web.tools port=$PORT2 >/dev/null
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

### Test 10: there is NO server-side render route (`/<id>/` is not served)

The web host renders no agent UI server-side: the `GET /<id>/` →
`render_html` page route was REMOVED. The host does exactly two things —
serve STATIC files via the `file` alias (`/<id>/file/<path>`, Test 11)
and carry `send()` over the WS bus. Frontend panels live in the TS
kernel (`html_agent.ts` → canvas), not as host pages. So `/<id>/` matches
no route at all → 404.

```bash
# no /<id>/ render route exists → any /<id>/ is an unmatched route → 404.
code=$(curl -s -o /dev/null -w '%{http_code}' "http://localhost:$PORT/fs_loader/")
[ "$code" = "404" ] && echo "  no /<id>/ render route → 404: OK; PASS" || echo "  FAIL: code=$code"
```
Expected: the check PASSES. Frontend panel rendering is exercised in the
frontend kernel's selftest (`ts/`), where the `html_agent.ts` record holds
the body and the canvas renders it; the host only serves the static `dist`
and relays the bus.

### Test 11: file blob proxy /<file>/file/<path>

```bash
mkdir -p /tmp/wfp && echo "hello bytes" > /tmp/wfp/foo.txt
FA=$(call fs_loader '{"type":"create_agent","handler_module":"file.tools","root":"/tmp/wfp"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
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
`content_alias_file` registry — any view (e.g. an `html_agent.ts` in the
frontend kernel) can now do `<img src="/<fa>/file/imgs/x.png">` without
registering anything.

### Test 12: WS binary frame round-trip (bytes)

```bash
uv run --active python -c "
import asyncio, json, struct, websockets
async def main():
    body = b'\\x00\\x01\\x02\\x03'
    async with websockets.connect('ws://localhost:$PORT/fs_loader/ws') as ws:
        # Send a 'call' frame with bytes — text wrapper is fine for this test
        # because we're checking the SERVER → BROWSER binary path. Trigger
        # the server to emit something with bytes by adding to its inbox via emit.
        # Simpler: drain mode — the watcher receives any agent_created event.
        # Just verify binary frame DECODES correctly by emitting bytes via a
        # synthetic kernel.send — needs a special test agent.
        # For this selftest we just verify the WS accepted binary mode; the
        # server → browser binary path is covered by pytest test_binary_protocol.
        print('SKIP: needs in-process bytes producer; covered by pytest test_binary_protocol')
asyncio.run(main())
"
```
Expected: `SKIP …` (the round-trip is covered by pytest unit test
`test_binary_protocol.py`; this slot is reserved for live binary if a
binary-emitting agent is registered).

### Test 13: external traffic carries `sender = web_agent_id` in state events

Browser-originated calls have no agent context (the WS handler runs
outside any handler dispatch), so without help `_current_sender`
would be None and telemetry rays would have nowhere to start. The
proxy + HTTP routes set the contextvar to the webapp's own
`web_agent_id` so external traffic visually originates from the
webapp sprite.

```bash
WEB=$(call fs_loader '{"type":"list_agents"}' | python -c "
import json,sys
agents = json.loads(sys.stdin.read())['agents']
print(next((a['id'] for a in agents if a['handler_module']=='web.tools'), ''))
")
uv run --active python -c "
import asyncio, json, websockets
async def main():
    async with websockets.connect(f'ws://localhost:$PORT/fs_loader/ws') as obs:
        await obs.send(json.dumps({'type':'state_subscribe'}))
        async with websockets.connect(f'ws://localhost:$PORT/fs_loader/ws') as caller:
            await caller.send(json.dumps({'type':'call','target':'fs_loader','payload':{'type':'list_agents'},'id':'1'}))
            sends = []
            try:
                async with asyncio.timeout(2):
                    while True:
                        msg = json.loads(await obs.recv())
                        if msg.get('type') == 'state_event' and msg.get('kind') == 'send' and msg.get('agent_id') == 'fs_loader':
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
| 2 | WS kernel.reflect round-trip (uniform identity + tree) | |
| 3 | WS call frame → fs_loader/reflect | |
| 4 | /<missing>/ → 404 | |
| 5 | /<backend>/ (no UI) → 404 | |
| 6 | WS call → reply | |
| 7 | PID lock written | |
| 8 | second serve refuses (live lock) | |
| 9 | stale lock overwritten on relaunch | |
| 10 | no server-side render route — /<id>/ is unmatched (→ 404) | |
| 11 | /<file>/file/<path> blob proxy | |
| 12 | WS binary (skip; pytest covers) | |
| 13 | external WS calls tag state events with web_agent_id | |
