# fantastic-web selftest

> scopes: HTTP, rendering
> requires: free port (suggest 18181), `cargo build --release --bin fantastic`
> out-of-scope: WS frames (web_ws selftest), REST verb dispatch (web_rest selftest)

axum HTTP host. Serves `/`, `/<id>/`, `/<id>/file/<path>`,
`/transport.js`, `/favicon.ico`. Verb-invocation surfaces (web_ws,
web_rest) mount sibling routes via `get_routes`.

## Pre-flight

```bash
rm -rf /tmp/fw_test
mkdir -p /tmp/fw_test
cd /tmp/fw_test
FANTASTIC=/path/to/rust/target/release/fantastic
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

### Test 2: transport.js served + injectable

```bash
curl -sf http://localhost:$PORT/transport.js | grep -q "fantastic_transport"
# Page render also injects transport.js automatically — at the root
# index (`/`) and at every renderable agent's path (`/<id>/`). The
# `web` agent itself does NOT answer `render_html` (it's the host,
# not a renderable), so `/w/` 404s by design. Test the root index:
curl -sf http://localhost:$PORT/ | grep -q "transport.js"
```

### Test 3: per-agent render_html

Add an html_agent (Phase 2 deliverable). Until then: any agent
returning `{html: str}` from `render_html` gets that body served at
`/<id>/`.

```bash
# Phase 2 — needs fantastic-html-agent.
echo "SKIP: requires fantastic-html-agent (Phase 2)"
```

### Test 4: file proxy at /<file_id>/file/<path>

```bash
mkdir -p /tmp/fw_root
echo "file content" > /tmp/fw_root/test.txt
# The fs edge is an io_bridge leg — SEALED by default. Open it (ingress_rule=allow_all)
# or the /file/ route's read_stream is denied and the proxy returns 404.
$FANTASTIC core create_agent handler_module=file_bridge.tools id=fw_f root=/tmp/fw_root ingress_rule=allow_all
# Daemon needs to be aware of the new agent — restart cycle:
kill $DAEMON_PID; sleep 1
$FANTASTIC &
DAEMON_PID=$!
sleep 2
# /file/ now pipes via read_stream (raw bytes, chunked) — content rides verbatim.
curl -sf http://localhost:$PORT/fw_f/file/test.txt | grep -q "file content"
```

### Test 5: 404 on unknown agent

```bash
curl -s -o /dev/null -w '%{http_code}' http://localhost:$PORT/no_such_agent/ | grep -q "404"
```

## Summary table

| Test | Pass / Fail | Notes |
|---|---|---|
| 1. root index 200 |  |  |
| 2. transport.js |  |  |
| 3. render_html | skip | needs Phase 2 |
| 4. file proxy |  |  |
| 5. 404 on unknown |  |  |
