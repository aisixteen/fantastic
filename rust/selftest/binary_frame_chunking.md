# binary_frame_chunking selftest (Rust)

> scopes: WS, transport, multi-modal
> requires: Rust `fantastic` binary built with `--features full`; a WebSocket client (Python `websockets` or `wscat`)

The Rust binary frame channel supports two modes:

1. **Single-frame** — no `upload_id` in the header. Whole payload in
   one binary WS frame. Byte-compatible with Python's wire.
2. **Chunked** (Rust-only today) — opt-in via `upload_id`. Per-chunk
   `chunk_index` + `total_chunks` + `final`; server reassembles in
   chunk-index order on `final=true`.

This spec drives both modes against a running Rust daemon.

## Pre-flight

```bash
cd rust
cargo build --release --bin fantastic --features full
BIN=$(pwd)/target/release/fantastic
WORK=$(mktemp -d)
cd "$WORK"

# Stage web + web_ws.
$BIN core create_agent handler_module=web.tools id=w port=18189 >/dev/null
$BIN w create_agent handler_module=web_ws.tools id=wws >/dev/null
$BIN &
DAEMON_PID=$!
sleep 2

# Verify WS endpoint live.
curl -sf -o /dev/null -w "%{http_code}\n" http://127.0.0.1:18189/w/ws
# (Should be 426 Upgrade Required — confirms the route exists.)
```

Cleanup at end: `kill $DAEMON_PID; rm -rf "$WORK"`.

## Wire shape

Each binary WS frame:

```
[4-byte BE u32 header_len]
[header_len bytes JSON header]
[remaining bytes — raw blob]
```

JSON header fields:

```json
{
  "target": "<agent_id>",        // required
  "type": "<verb>",              // required
  "id": "<correlation_id>",      // optional
  "upload_id": "u_xxxx",         // optional — opt-in chunked mode
  "chunk_index": 0,              // required if upload_id present
  "total_chunks": 3,             // required if upload_id present
  "final": false                 // required if upload_id present
}
```

## Tests

Use Python's `websockets` library for the frame construction. A
helper script:

```python
# /tmp/ws_chunk_helper.py
import asyncio, json, struct, sys, websockets

async def send_frame(url, header, blob):
    async with websockets.connect(url) as ws:
        hdr_bytes = json.dumps(header).encode("utf-8")
        frame = struct.pack(">I", len(hdr_bytes)) + hdr_bytes + blob
        await ws.send(frame)
        # Drain replies for 1s.
        try:
            while True:
                msg = await asyncio.wait_for(ws.recv(), timeout=1.0)
                print(msg)
        except asyncio.TimeoutError:
            pass

if __name__ == "__main__":
    asyncio.run(send_frame(sys.argv[1], json.loads(sys.argv[2]), open(sys.argv[3], "rb").read()))
```

### Test 1: single-frame upload reaches dispatch

```bash
echo "hello" > /tmp/small.bin
python3 /tmp/ws_chunk_helper.py 'ws://127.0.0.1:18189/w/ws' \
  '{"target":"nonexistent","type":"noop","id":"s1"}' /tmp/small.bin
```
Expected: a `{"type":"error","id":"s1","error":"no agent ..."}` reply.
Proves the binary-frame path round-trips through dispatch.

### Test 2: chunked upload reassembles in order

Send 3 chunks of a 6-byte blob (2 bytes each):

```bash
python3 - <<'PY'
import asyncio, json, struct, websockets

CHUNKS = [b"he", b"ll", b"o!"]

async def go():
    async with websockets.connect("ws://127.0.0.1:18189/w/ws") as ws:
        for i, blob in enumerate(CHUNKS):
            is_final = i == len(CHUNKS) - 1
            header = {
                "target": "nonexistent", "type": "noop", "id": "c1",
                "upload_id": "u_test", "chunk_index": i,
                "total_chunks": len(CHUNKS), "final": is_final,
            }
            hdr_bytes = json.dumps(header).encode("utf-8")
            frame = struct.pack(">I", len(hdr_bytes)) + hdr_bytes + blob
            await ws.send(frame)
            try:
                while True:
                    msg = await asyncio.wait_for(ws.recv(), timeout=0.3)
                    print(f"chunk{i}: {msg}")
            except asyncio.TimeoutError:
                pass

asyncio.run(go())
PY
```
Expected:
- After chunk 0: `{"type":"chunk_ack","upload_id":"u_test","chunk_index":0}`
- After chunk 1: `{"type":"chunk_ack","upload_id":"u_test","chunk_index":1}`
- After chunk 2 (final): `{"type":"error","id":"c1","error":"no agent ..."}`
  — the assembled 6-byte blob reached dispatch (which errored because target doesn't exist; that's the round-trip proof).

### Test 3: out-of-order chunks reassemble correctly

Same as Test 2 but send chunk indices in order `[2 (final), 0, 1]`:

```bash
# Server holds final's blob, accumulates 0 and 1; when all 3 are present
# the next chunk with final=true triggers assembly.
# Verify the assembled blob would have been in chunk-index order:
# "he" + "ll" + "o!"  →  "hello!"
# (Since dispatch errors, we can only assert the wire shape.)
```
Expected: chunk_ack frames + final error frame as in Test 2.

### Test 4: oversized single chunk rejected

```bash
dd if=/dev/zero of=/tmp/big.bin bs=1M count=2 2>/dev/null
python3 /tmp/ws_chunk_helper.py 'ws://127.0.0.1:18189/w/ws' \
  '{"target":"nonexistent","type":"noop","id":"big1"}' /tmp/big.bin
```
Expected: `{"type":"error","id":"big1","error":"binary frame: blob ... exceeds chunk cap ..."}`.

### Test 5: oversized total upload rejected

Send 101 chunks of 1 MB each (total > 100 MB cap):

```bash
# (Use a Python script; same pattern as Test 2 but with chunk_size=1MB,
#  total_chunks=101, and stop as soon as the server errors.)
# Expected: error frame with "exceeds total cap" before chunk 101 is fully accepted.
```

### Test 6: pending upload drops on WS disconnect

```bash
# Open WS, send chunk 0 (non-final), close WS.
# Open fresh WS, send chunk 0 with the same upload_id, total_chunks=2.
# Verify the fresh chunk gets a clean chunk_ack — no "total_chunks
# mismatch" error from the prior buffer.
```
Expected: fresh chunk_ack on the new WS. Proves per-WS state cleanup.

### Test 7: chunk_ack carries upload_id + chunk_index

Re-run Test 2 chunk 0 in isolation:

```bash
python3 - <<'PY'
import asyncio, json, struct, websockets
async def go():
    async with websockets.connect("ws://127.0.0.1:18189/w/ws") as ws:
        hdr = json.dumps({"target":"nonexistent","type":"noop","upload_id":"u_ack","chunk_index":0,"total_chunks":2,"final":False}).encode()
        frame = struct.pack(">I", len(hdr)) + hdr + b"xx"
        await ws.send(frame)
        msg = await asyncio.wait_for(ws.recv(), 1.0)
        print(msg)
asyncio.run(go())
PY
```
Expected: `{"type":"chunk_ack","upload_id":"u_ack","chunk_index":0}`. Exact field names matter for client flow control.

## Cleanup

```bash
kill $DAEMON_PID 2>/dev/null
rm -rf "$WORK"
```

## Regression signals

- Test 1 fails: binary frame decode is broken. Wire incompatible with frontend.
- Test 2 chunk_ack not emitted: clients can't flow-control uploads.
- Test 4 caps don't fire: memory unbounded by a single oversize blob.
- Test 6 buffer survives WS disconnect: per-WS state semantics broken (memory leak across connections).
