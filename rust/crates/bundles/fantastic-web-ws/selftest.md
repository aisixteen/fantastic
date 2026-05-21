# fantastic-web-ws selftest

> scopes: WS, transport
> requires: free port (suggest 18181), `cargo build --release --bin fantastic`, parent web agent
> out-of-scope: HTTP routes (covered by fantastic-web selftest)

WebSocket verb channel. Mounts `ws://host/<agent_id>/ws` on the
parent web agent. Carries `call` / `emit` / `watch` / `unwatch`
frames inbound; `reply` / `error` / `event` outbound. Binary frames:
`[4-byte BE u32 H][JSON header][raw blob]`.

## Pre-flight

```bash
rm -rf /tmp/fws_test
mkdir -p /tmp/fws_test
cd /tmp/fws_test
FANTASTIC=/path/to/rust/target/release/fantastic
PORT=18181
$FANTASTIC core create_agent handler_module=web.tools id=w port=$PORT
$FANTASTIC w create_agent handler_module=web_ws.tools id=wws
$FANTASTIC &
DAEMON_PID=$!
trap 'kill $DAEMON_PID 2>/dev/null' EXIT
sleep 2

# Shell helper that wraps a one-shot WS round-trip in inline Python:
call() {
  python3 - <<PY
import asyncio, json, websockets, sys
async def main():
    async with websockets.connect("ws://localhost:$PORT/$1/ws") as ws:
        await ws.send(json.dumps({"type":"call","target":"$2","payload":json.loads('''$3'''),"id":"1"}))
        async for msg in ws:
            d = json.loads(msg)
            if d.get("type") in ("reply","error") and d.get("id") == "1":
                print(json.dumps(d.get("data") if d["type"]=="reply" else {"error": d["error"]}))
                return
asyncio.run(main())
PY
}
```

## Tests

### Test 1: WS handshake at /<agent>/ws

```bash
call w kernel '{"type":"reflect"}' | jq -e '.primitive == "send(target_id, payload) -> reply | None"'
```

### Test 2: call frame returns reply

```bash
call w core '{"type":"list_agents"}' | jq -e '.agents | length >= 1'
```

### Test 3: error frame on bad target

```bash
call w nonexistent_agent '{"type":"reflect"}' | jq -e '.error | type == "string"'
```

### Test 4: watch frame mirrors source inbox

(Drive an emit on src → confirm an `event` frame arrives.)

```bash
python3 - <<'PY'
import asyncio, json, websockets
async def main():
    async with websockets.connect(f"ws://localhost:18181/w/ws") as ws:
        await ws.send(json.dumps({"type":"watch","src":"core"}))
        await ws.send(json.dumps({"type":"emit","target":"core","payload":{"type":"hello"}}))
        # First message after watch should be the mirrored event:
        async for msg in ws:
            d = json.loads(msg)
            if d.get("type") == "event":
                assert d["payload"]["type"] == "hello"
                print("OK")
                return
asyncio.run(main())
PY
```

### Test 5: binary frame round-trip

```bash
# Send a binary frame with a 4-byte header + JSON header + raw blob;
# verify the receiver unpacks it correctly. Spec lives in
# crates/bundles/fantastic-web-ws/src/lib.rs.
echo "TODO once binary impl lands (Phase 1, task #229)"
```

## Summary table

| Test | Pass / Fail | Notes |
|---|---|---|
| 1. handshake |  |  |
| 2. call → reply |  |  |
| 3. error on bad target |  |  |
| 4. watch mirrors inbox |  |  |
| 5. binary frame | skip | impl pending |
