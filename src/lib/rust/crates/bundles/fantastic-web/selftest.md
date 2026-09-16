# fantastic-web selftest

> scopes: HTTP, rendering
> requires: free port (suggest 18181), `cargo build --release --bin fantastic_kernel`
> out-of-scope: WS frames (web_ws selftest), REST verb dispatch (web_rest selftest)

HTTP host. Serves python's four rendering routes — `/`,
`/_assets/favicon.png`, `/favicon.png`, `/<id>/file/<path>`.
Verb-invocation surfaces (web_ws, web_rest) mount sibling routes via
`get_routes`.

## Pre-flight

```bash
rm -rf /tmp/fw_test
mkdir -p /tmp/fw_test
cd /tmp/fw_test
FANTASTIC=/path/to/rust/target/release/fantastic_kernel
PORT=18181
$FANTASTIC core create_agent handler_module=web.tools id=w port=$PORT
$FANTASTIC &
DAEMON_PID=$!
trap 'kill $DAEMON_PID 2>/dev/null' EXIT
sleep 2  # let axum bind
```

## Tests

### Test 1: root index returns 200

```bash
curl -sf -o /tmp/fw_test/root.html http://localhost:$PORT/
grep -q "fantastic" /tmp/fw_test/root.html
```

### Test 2: the favicon is the only static asset

```bash
curl -s -o /dev/null -w '%{http_code}' http://localhost:$PORT/favicon.png | grep -q "200"
curl -s -o /dev/null -w '%{http_code}' http://localhost:$PORT/_assets/favicon.png | grep -q "200"
```

### Test 3: no server-side render route

Rendering routes are python's four; frontend panels live in the TS
kernel, so `/<id>/` and `/transport.js` are NOT served.

```bash
curl -s -o /dev/null -w '%{http_code}' http://localhost:$PORT/transport.js | grep -q "404"
curl -s -o /dev/null -w '%{http_code}' http://localhost:$PORT/w/ | grep -q "404"
```

### Test 4: file proxy at /<file_id>/file/<path>

```bash
mkdir -p /tmp/fw_root
echo "file content" > /tmp/fw_root/test.txt
# The fs edge is an io_bridge leg — SEALED by default. Open it (ingress_rule=allow_all_ingress)
# or the /file/ route's read_stream is denied and the proxy returns 404.
$FANTASTIC core create_agent handler_module=file_bridge.tools id=fw_f root=/tmp/fw_root ingress_rule=allow_all_ingress
# Daemon needs to be aware of the new agent — restart cycle:
kill $DAEMON_PID; sleep 1
$FANTASTIC &
DAEMON_PID=$!
sleep 2
# /file/ now pipes via read_stream (raw bytes, chunked) — content rides verbatim.
curl -sf http://localhost:$PORT/fw_f/file/test.txt | grep -q "file content"
```

### Test 5: 404 on an unmatched path

```bash
curl -s http://localhost:$PORT/no_such_agent/ | grep -q '{"detail":"Not Found"}'
```

## Summary table

| Test | Pass / Fail | Notes |
|---|---|---|
| 1. root index 200 |  |  |
| 2. favicon |  |  |
| 3. no render route |  |  |
| 4. file proxy |  |  |
| 5. 404 JSON on a miss |  |  |
