# web_ws selftest

> scopes: ws, web, web_ws
> requires: `uv sync`; port 18901 free

WebSocket call-surface sub-agent of `web`. Mounts `/{host_id}/ws` on
the parent web's FastAPI app via the duck-typed `get_routes` verb.

## Pre-flight

```bash
cd new_codebase
rm -rf .fantastic
pkill -9 -f "fantastic" 2>/dev/null
PORT=18901
uv run --active fantastic core create_agent handler_module=web.tools port=$PORT >/dev/null
WEB_ID=$(ls .fantastic/agents | grep '^web_' | head -1)
echo "WEB_ID=$WEB_ID"
# Spawn web_ws as a child of web. Call create_agent on the web agent
# itself so the new agent lands under <web_id>/agents/.
uv run --active fantastic $WEB_ID create_agent handler_module=web_ws.tools >/dev/null
uv run --active fantastic > /tmp/serve.log 2>&1 &
SPID=$!
for i in $(seq 1 20); do grep -q "kernel up" /tmp/serve.log 2>/dev/null && break; sleep 0.5; done
```

After all tests:
```bash
kill -9 $SPID 2>/dev/null
rm -rf .fantastic /tmp/serve.log
```

## Tests

### Test 1: WS handshake to /core/ws succeeds

```bash
uv run --active python -c "
import asyncio, json, websockets
async def main():
    async with websockets.connect('ws://localhost:$PORT/core/ws') as ws:
        await ws.send(json.dumps({'type':'call','target':'core','payload':{'type':'reflect'},'id':'1'}))
        for _ in range(5):
            m = json.loads(await ws.recv())
            if m.get('id')=='1' and m.get('type')=='reply':
                print('PASS' if 'transports' in m['data'] else 'FAIL'); return
asyncio.run(main())
"
```
Expected: `PASS`. Regression signal: connection refused → web_ws not
mounted; check `fantastic <WEB_ID> reflect` for `surfaces` listing.

### Test 2: web.reflect surfaces include web_ws's route

```bash
uv run --active python -c "
import asyncio, json, websockets
async def main():
    async with websockets.connect('ws://localhost:$PORT/core/ws') as ws:
        await ws.send(json.dumps({'type':'call','target':'$WEB_ID','payload':{'type':'reflect'},'id':'1'}))
        while True:
            m = json.loads(await ws.recv())
            if m.get('id')=='1' and m.get('type')=='reply':
                surfaces = m['data'].get('surfaces') or {}
                print('PASS' if any('/ws' in p for paths in surfaces.values() for p in paths) else f'FAIL surfaces={surfaces}')
                return
asyncio.run(main())
"
```
Expected: `PASS`.

### Test 3: web_ws.reflect describes its surface

```bash
WSID=$(ls .fantastic/agents/$WEB_ID/agents 2>/dev/null | grep '^web_ws_' | head -1)
uv run --active python -c "
import asyncio, json, websockets
async def main():
    async with websockets.connect('ws://localhost:$PORT/core/ws') as ws:
        await ws.send(json.dumps({'type':'call','target':'$WSID','payload':{'type':'reflect'},'id':'1'}))
        while True:
            m = json.loads(await ws.recv())
            if m.get('id')=='1' and m.get('type')=='reply':
                d = m['data']
                ok = d.get('path_pattern') == '/{host_id}/ws'
                print('PASS' if ok else f'FAIL d={d}'); return
asyncio.run(main())
"
```

### Test 4: delete web_ws → /core/ws stops handshaking

```bash
WSID=$(ls .fantastic/agents/$WEB_ID/agents 2>/dev/null | grep '^web_ws_' | head -1)
uv run --active python -c "
import asyncio, json, websockets
async def main():
    async with websockets.connect('ws://localhost:$PORT/core/ws') as ws:
        await ws.send(json.dumps({'type':'call','target':'core','payload':{'type':'delete_agent','id':'$WSID'},'id':'1'}))
        while True:
            m = json.loads(await ws.recv())
            if m.get('id')=='1' and m.get('type')=='reply': break
asyncio.run(main())
"
sleep 0.3
uv run --active python -c "
import asyncio, websockets
async def main():
    try:
        await websockets.connect('ws://localhost:$PORT/core/ws')
        print('FAIL: WS still open after web_ws delete')
    except Exception:
        print('PASS')
asyncio.run(main())
"
```
Expected: `PASS`. Regression signal: WS connects → web's
`_unmount_surface` didn't strip the routes on cascade-delete.

## Summary

| # | Test | Pass |
|---|------|------|
| 1 | WS handshake + call/reply | |
| 2 | web.reflect surfaces include /ws | |
| 3 | web_ws.reflect describes surface | |
| 4 | delete web_ws → /ws stops handshaking | |
