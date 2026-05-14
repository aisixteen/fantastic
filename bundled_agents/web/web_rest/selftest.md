# web_rest selftest

> scopes: http, web, web_rest
> requires: `uv sync`; port 18901 free

REST verb-invocation surface (diagnostic). `POST /<self_id>/<target_id>`
body=payload → kernel.send(target_id, payload) → JSON reply.

## Pre-flight

```bash
cd new_codebase
rm -rf .fantastic
pkill -9 -f "fantastic" 2>/dev/null
PORT=18901
uv run --active fantastic core create_agent handler_module=web.tools port=$PORT >/dev/null
WEB_ID=$(ls .fantastic/agents | grep '^web_' | head -1)
# WS verb channel + REST surface as children of web. Call create_agent
# on the web agent (not core) so the new agents land under web's dir.
uv run --active fantastic $WEB_ID create_agent handler_module=web_ws.tools >/dev/null
uv run --active fantastic $WEB_ID create_agent handler_module=web_rest.tools >/dev/null
uv run --active fantastic > /tmp/serve.log 2>&1 &
SPID=$!
for i in $(seq 1 20); do grep -q "kernel up" /tmp/serve.log 2>/dev/null && break; sleep 0.5; done
RID=$(ls .fantastic/agents/$WEB_ID/agents | grep '^web_rest_' | head -1)
echo "RID=$RID"
```

After all tests:
```bash
kill -9 $SPID 2>/dev/null
rm -rf .fantastic /tmp/serve.log
```

## Tests

### Test 1: POST /<rest_id>/core body={type:reflect} returns primer

```bash
curl -s -X POST -H 'content-type: application/json' \
  -d '{"type":"reflect"}' http://localhost:$PORT/$RID/core | python -c "
import json, sys
d = json.load(sys.stdin)
assert 'transports' in d, f'no transports: {list(d)}'
assert 'tree' in d
print('PASS')
"
```
Expected: `PASS`.

### Test 2: POST /<rest_id>/core body={type:list_agents} returns agents

```bash
curl -s -X POST -H 'content-type: application/json' \
  -d '{"type":"list_agents"}' http://localhost:$PORT/$RID/core | python -m json.tool | head -10
```
Expected: JSON with `agents:[…]` containing core, the web agent, the
web_rest child.

### Test 3: bad JSON → 400

```bash
curl -s -o /dev/null -w '%{http_code}\n' -X POST \
  -H 'content-type: application/json' -d 'not json {{{' \
  http://localhost:$PORT/$RID/core
```
Expected: `400`.

### Test 4: non-object JSON → 400

```bash
curl -s -o /dev/null -w '%{http_code}\n' -X POST \
  -H 'content-type: application/json' -d '[1,2,3]' \
  http://localhost:$PORT/$RID/core
```
Expected: `400`.

### Test 5: two web_rest instances coexist with different ids

```bash
# Spawn a second web_rest as another child of web.
uv run --active python -c "
import asyncio, json, websockets
async def main():
    async with websockets.connect('ws://localhost:$PORT/core/ws') as ws:
        await ws.send(json.dumps({'type':'call','target':'core','payload':{'type':'create_agent','handler_module':'web_rest.tools','parent_id':'$WEB_ID'},'id':'1'}))
        while True:
            m = json.loads(await ws.recv())
            if m.get('id')=='1' and m.get('type')=='reply':
                print(m['data']['id']); return
asyncio.run(main())
" > /tmp/rid2
RID2=$(cat /tmp/rid2)
sleep 0.3
# Both URLs answer separately.
curl -s -X POST -d '{"type":"reflect"}' http://localhost:$PORT/$RID/core | python -c "import json,sys;print('rid1' if 'transports' in json.load(sys.stdin) else 'FAIL')"
curl -s -X POST -d '{"type":"reflect"}' http://localhost:$PORT/$RID2/core | python -c "import json,sys;print('rid2' if 'transports' in json.load(sys.stdin) else 'FAIL')"
```
Expected: `rid1` and `rid2`. Regression signal: one of the URLs 404s →
web_rest got the path literal wrong (must embed self_id).

### Test 6: GET /<rest_id>/_reflect → substrate primer (browser-pastable)

Default-no-target GET maps to `kernel.reflect`. Open the URL in a
browser address bar → JSON. No body, no headers.

```bash
curl -s http://localhost:$PORT/$RID/_reflect | python -c "
import json, sys
d = json.load(sys.stdin)
assert 'transports' in d
assert 'tree' in d
assert 'available_bundles' in d
print('PASS')
"
```
Expected: `PASS`.

### Test 7: GET /<rest_id>/_reflect/<target_id> → that agent's reflect

```bash
curl -s http://localhost:$PORT/$RID/_reflect/core | python -c "
import json, sys
d = json.load(sys.stdin)
# core reflect IS the primer (root reflect returns it).
assert 'transports' in d
assert 'tree' in d
print('PASS')
"
```
Expected: `PASS`.

### Test 8: GET /<rest_id>/_reflect/<missing> → JSON error body

```bash
curl -s http://localhost:$PORT/$RID/_reflect/nonexistent_xxx | python -c "
import json, sys
d = json.load(sys.stdin)
assert 'error' in d, f'no error key: {d}'
assert 'no agent' in d['error']
print('PASS')
"
```
Expected: `PASS`. Status code is 200 — the error rides in the JSON
body. (Unknown-target is a kernel-level miss, not an HTTP miss; the
GET shortcut faithfully serializes whatever `kernel.send` returns.)

### Test 9: delete web_rest → URL 404s

```bash
uv run --active python -c "
import asyncio, json, websockets
async def main():
    async with websockets.connect('ws://localhost:$PORT/core/ws') as ws:
        await ws.send(json.dumps({'type':'call','target':'core','payload':{'type':'delete_agent','id':'$RID'},'id':'1'}))
        while True:
            m = json.loads(await ws.recv())
            if m.get('id')=='1': break
asyncio.run(main())
"
sleep 0.3
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST -d '{"type":"reflect"}' http://localhost:$PORT/$RID/core)
[ "$code" = "404" ] && echo "PASS (POST 404)" || echo "FAIL: code=$code"
# The GET shortcuts also vanish.
code2=$(curl -s -o /dev/null -w '%{http_code}' http://localhost:$PORT/$RID/_reflect)
[ "$code2" = "404" ] && echo "PASS (GET 404)" || echo "FAIL: code=$code2"
```
Expected: `PASS (POST 404)` and `PASS (GET 404)`. Cascade-delete
unmounts every route the surface owned.

## Summary

| # | Test | Pass |
|---|------|------|
| 1 | POST → primer | |
| 2 | POST → list_agents | |
| 3 | bad JSON → 400 | |
| 4 | non-object body → 400 | |
| 5 | two rest instances coexist | |
| 6 | GET /_reflect → primer | |
| 7 | GET /_reflect/<id> → agent reflect | |
| 8 | GET /_reflect/<missing> → error JSON | |
| 9 | delete → URLs 404 (POST + GET) | |
