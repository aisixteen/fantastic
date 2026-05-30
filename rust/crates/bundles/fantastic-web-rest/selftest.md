# fantastic-web-rest selftest

> scopes: HTTP, REST
> requires: free port (suggest 18181), `cargo build --release --bin fantastic`, parent web agent

REST verb channel. `POST /<self_id>/<target_id>` body=payload →
kernel.send → JSON reply. Browser-pastable shortcuts:
`GET /<self_id>/_reflect[/<target>][?readme=1]`.

## Pre-flight

```bash
rm -rf /tmp/fwr_test
mkdir -p /tmp/fwr_test
cd /tmp/fwr_test
FANTASTIC=/path/to/rust/target/release/fantastic
PORT=18181
$FANTASTIC core create_agent handler_module=web.tools id=w port=$PORT
$FANTASTIC w create_agent handler_module=web_rest.tools id=wr
$FANTASTIC &
DAEMON_PID=$!
trap 'kill $DAEMON_PID 2>/dev/null' EXIT
sleep 2
```

## Tests

### Test 1: POST routes to kernel.send

```bash
curl -sf -X POST -H "Content-Type: application/json" \
  -d '{"type":"reflect"}' \
  http://localhost:$PORT/wr/kernel \
  | jq -e '.id == "core" and .tree.id == "core"'
```

### Test 2: error on unknown target

```bash
curl -s -X POST -H "Content-Type: application/json" \
  -d '{"type":"reflect"}' \
  http://localhost:$PORT/wr/no_such_agent \
  | jq -e '.error | type == "string"'
```

### Test 3: GET _reflect shortcut

```bash
curl -sf http://localhost:$PORT/wr/_reflect | jq -e '.tree.id == "core"'
curl -sf http://localhost:$PORT/wr/_reflect/core | jq -e '.id == "core"'
```

### Test 4: GET _reflect?readme=1 includes readme

```bash
curl -sf "http://localhost:$PORT/wr/_reflect/core?readme=1" \
  | jq -e '.readme | type == "string" and contains("Fantastic kernel")'
```

### Test 5: multiple instances coexist

```bash
$FANTASTIC w create_agent handler_module=web_rest.tools id=wr2
kill $DAEMON_PID; sleep 1
$FANTASTIC &
DAEMON_PID=$!
sleep 2
curl -sf http://localhost:$PORT/wr/_reflect  | jq -e '.tree.id == "core"'
curl -sf http://localhost:$PORT/wr2/_reflect | jq -e '.tree.id == "core"'
```

## Summary table

| Test | Pass / Fail | Notes |
|---|---|---|
| 1. POST → kernel.send |  |  |
| 2. error on bad target |  |  |
| 3. GET _reflect |  |  |
| 4. readme flag |  |  |
| 5. multiple instances |  |  |
