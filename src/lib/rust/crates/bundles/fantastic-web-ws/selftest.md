# fantastic-web-ws selftest

> scopes: WS, transport
> requires: free port (suggest 18181), `cargo build --release --bin fantastic_kernel`, parent web agent
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
FANTASTIC=/path/to/rust/target/release/fantastic_kernel
PORT=18181
$FANTASTIC kernel_state create_agent handler_module=web.tools id=w port=$PORT
# THE LEG IS THE SENDER of every frame, and a leg with no `ingress_rule` is
# SEALED — open it consciously, and grant it the reach the frames below need
# (a grant map is keyed by a literal id, never `*`).
$FANTASTIC w create_agent handler_module=web_ws.tools id=wws ingress_rule=allow_all_ingress can_call='{"kernel_state": "*"}'
$FANTASTIC kernel_state update_agent id=kernel_state callable_by='{"$self": "*", "$parent": "*", "$children": "*", "wws": "*"}'
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
call w kernel '{"type":"reflect"}' | jq -e '.id == "kernel_state" and .tree.id == "kernel_state"'
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
        await ws.send(json.dumps({"type":"watch","src":"kernel_state"}))
        await ws.send(json.dumps({"type":"emit","target":"kernel_state","payload":{"type":"hello"}}))
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

A binary frame is an ORDINARY envelope plus a body: the header names the
body's slot in `_binary_path`, and the frame is routed by `type` exactly like
a text one. There is no chunked-upload dialect.

```bash
python3 - <<'PY'
import asyncio, json, struct, websockets
async def main():
    head = {"type": "call", "target": "fwsf", "id": "b1",
            "payload": {"type": "write_stream", "path": "x.bin", "truncate": True},
            "_binary_path": "payload.bytes"}
    h = json.dumps(head).encode()
    async with websockets.connect("ws://localhost:18181/w/ws") as ws:
        await ws.send(struct.pack(">I", len(h)) + h + bytes([0, 1, 2]))
        async for msg in ws:
            d = json.loads(msg) if isinstance(msg, str) else None
            if d and d.get("id") == "b1":
                assert d["data"]["written"] == 3, d
                print("OK")
                return
asyncio.run(main())
PY
```

(Needs a `file_bridge` named `fwsf` that `wws` may call — see the
fantastic-file selftest for the two-sided wiring.)

## Summary table

| Test | Pass / Fail | Notes |
|---|---|---|
| 1. handshake |  |  |
| 2. call → reply |  |  |
| 3. error on bad target |  |  |
| 4. watch mirrors inbox |  |  |
| 5. binary frame |  |  |
