# fantastic-kernel-bridge selftest

> scopes: kernel, ws, bridge
> requires: `cargo build --release --bin fantastic`. `reflect`/`boot`
> are stateless one-shots; `forward`/`watch_remote` need a LIVE remote
> daemon serving `web_ws` over WS (boot a second `fantastic` on a free
> port first — same live-daemon rule as ollama/terminal/web).
> out-of-scope: SSH+WS transport (needs a real remote host + `full`
> feature; covered by `ssh_runner`), MemoryTransport pair (unit tests
> in `src/tests.rs`).

WS-only, asymmetric. A bridge agent opens a WS to the remote's
`web_ws` and ships raw `{type:"call", id, target, payload}` frames; the
remote dispatches `kernel.send` like a browser frame and replies over
the same socket — no peer bridge. Streaming via `watch_remote`
(`{type:"watch", src}` out, `{type:"event"}` back, re-emitted on the
bridge's own inbox). One-shot CLI: `fantastic <id> <verb> [k=v ...]`.

## Pre-flight

All test state lives under `/tmp/kb_test/`. Tests 3–4 boot a SECOND
kernel (the remote peer) on `$PORT_B` as a background daemon.

```bash
rm -rf /tmp/kb_test && mkdir -p /tmp/kb_test/a /tmp/kb_test/b
cd /tmp/kb_test/a
FANTASTIC=/path/to/rust/target/release/fantastic
PORT_B=18190
```

## Tests

### Test 1: reflect on an unbooted ws bridge

```bash
BR=$($FANTASTIC core create_agent handler_module=kernel_bridge.tools \
  transport=ws host=127.0.0.1 local_port=$PORT_B peer_id=wws | jq -r .id)
$FANTASTIC $BR reflect | jq -e '.transport == "ws" and .connected == false and .pending_count == 0'
```

### Test 2: forward before boot is refused

```bash
$FANTASTIC $BR forward target=core payload='{"type":"reflect"}' \
  | jq -e '.error | contains("not connected")'
```

### Test 3: boot + forward a reflect to the remote core (LIVE)

```bash
# Bring up peer kernel B with a web + web_ws on $PORT_B.
(cd /tmp/kb_test/b && $FANTASTIC core create_agent handler_module=web.tools id=w port=$PORT_B)
(cd /tmp/kb_test/b && $FANTASTIC w create_agent handler_module=web_ws.tools id=wws)
(cd /tmp/kb_test/b && $FANTASTIC) &
DAEMON_PID=$!
trap 'kill $DAEMON_PID 2>/dev/null' EXIT
sleep 2

$FANTASTIC $BR boot | jq -e '.booted == true and .transport == "ws"'
$FANTASTIC $BR forward target=core payload='{"type":"reflect"}' \
  | jq -e '.id == "core"'
```

Expect: the forward returns kernel B's `core.reflect` body unwrapped.
Regression signal: `error: ws connect failed` → peer_id must match B's
web_ws id (`wws`) and `$PORT_B` must be live; hang on forward →
corr_id mismatch in the read loop.

### Test 4: watch_remote subscribes + shutdown rejects pending (LIVE)

```bash
$FANTASTIC $BR watch_remote target=core | jq -e '.ok == true and .watching == "core"'
$FANTASTIC $BR shutdown | jq -e '.stopped == true'
$FANTASTIC $BR reflect | jq -e '.connected == false'
```

## Cleanup

```bash
kill $DAEMON_PID 2>/dev/null
cd / && rm -rf /tmp/kb_test
```

## Summary table

| Test | Pass / Fail | Notes |
|---|---|---|
| 1. reflect unbooted |  |  |
| 2. forward refused pre-boot |  |  |
| 3. boot + forward (live) |  |  |
| 4. watch_remote + shutdown (live) |  |  |
