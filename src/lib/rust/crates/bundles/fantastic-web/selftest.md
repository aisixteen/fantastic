# fantastic-web selftest

> scopes: HTTP, rendering
> requires: free port (suggest 18181), `cargo build --release --bin fantastic_kernel`
> out-of-scope: WS frames (web_ws selftest), REST verb dispatch (web_rest selftest)

HTTP host. Serves python's two rendering routes — `/_assets/favicon.png`
and `/<id>/file/<path>`. Verb-invocation surfaces (web_ws, web_rest) mount
sibling routes via `get_routes`.

## Pre-flight

```bash
rm -rf /tmp/fw_test
mkdir -p /tmp/fw_test
cd /tmp/fw_test
FANTASTIC=/path/to/rust/target/release/fantastic_kernel
PORT=18181
$FANTASTIC kernel_state create_agent handler_module=web.tools id=w port=$PORT
$FANTASTIC &
DAEMON_PID=$!
trap 'kill $DAEMON_PID 2>/dev/null' EXIT
sleep 2  # let axum bind
```

## Tests

### Test 1: the favicon is the only static asset

```bash
curl -s -o /dev/null -w '%{http_code}' http://localhost:$PORT/_assets/favicon.png | grep -q "200"
curl -s -o /dev/null -w '%{http_code}' http://localhost:$PORT/favicon.png | grep -q "404"
curl -s -o /dev/null -w '%{http_code}' http://localhost:$PORT/ | grep -q "404"
```

### Test 2: no server-side render route

Rendering routes are python's two; frontend panels live in the TS
kernel, so `/`, `/<id>/` and `/transport.js` are NOT served.

```bash
curl -s -o /dev/null -w '%{http_code}' http://localhost:$PORT/transport.js | grep -q "404"
curl -s -o /dev/null -w '%{http_code}' http://localhost:$PORT/w/ | grep -q "404"
```

### Test 3: file proxy at /<file_id>/file/<path> serves only what `allow` exposes

```bash
mkdir -p /tmp/fw_test/fw_root
echo "file content" > /tmp/fw_test/fw_root/test.txt
echo "private" > /tmp/fw_test/fw_root/secret.txt
# The fs edge is an io_bridge leg — SEALED by default (ingress_rule), DENY-ALL at
# the path level (`allow`), and the read runs AS THE WEB AGENT, so both call-gate
# halves must name it. The root is RELATIVE: it clamps inside the running dir.
$FANTASTIC kernel_state create_agent handler_module=file_bridge.tools id=fw_f root=fw_root \
  allow='["test.txt"]' ingress_rule=allow_all_ingress callable_by='{"w":["read_stream"]}'
$FANTASTIC kernel_state update_agent id=w can_call='{"fw_f":["read_stream"]}'
# Daemon needs to be aware of the new agent — restart cycle:
kill $DAEMON_PID; sleep 1
$FANTASTIC &
DAEMON_PID=$!
sleep 2
# /file/ pipes via read_stream (raw bytes, chunked) — content rides verbatim.
curl -sf http://localhost:$PORT/fw_f/file/test.txt | grep -q "file content"
curl -s -o /dev/null -w '%{http_code}' http://localhost:$PORT/fw_f/file/secret.txt | grep -q "404"
```

### Test 4: 404 on an unmatched path

```bash
curl -s http://localhost:$PORT/no_such_agent/ | grep -q '{"detail":"Not Found"}'
```

## Summary table

| Test | Pass / Fail | Notes |
|---|---|---|
| 1. favicon is the only asset |  |  |
| 2. no render route |  |  |
| 3. file proxy serves only `allow` |  |  |
| 4. 404 JSON on a miss |  |  |
