# fantastic-terminal-backend selftest

> scopes: pty, process
> requires: live daemon + WS — `cargo build --release --bin fantastic`,
> a parent web + web_ws, a real shell at `/bin/sh`, free port (suggest
> 18920). PTYs are process-memory; they can't survive a separate
> `fantastic call` invocation, so drive a running daemon over WS.
> out-of-scope: flow-control watermarks, image paste, browser xterm view

PTY shell session as an agent. Spawn, write/read round-trip, scrollback,
stop. One PTY per agent; state is process-only (no `.fantastic/` sidecar).

**Why a live daemon is required:** the PTY is a child of the running
kernel. `fantastic <id> <verb>` spawns a fresh kernel per call, which
would kill the PTY between calls — same live-daemon rule as the Python
peer. We boot one persistent daemon and drive it through one-shot WS
`call` frames.

## Pre-flight

```bash
rm -rf /tmp/ftb_test
mkdir -p /tmp/ftb_test
cd /tmp/ftb_test
FANTASTIC=/path/to/rust/target/release/fantastic
PORT=18920
$FANTASTIC core create_agent handler_module=web.tools id=w port=$PORT
$FANTASTIC w create_agent handler_module=web_ws.tools id=wws
$FANTASTIC &
DAEMON_PID=$!
trap 'kill $DAEMON_PID 2>/dev/null' EXIT
sleep 2

# One-shot WS round-trip helper: call <host_agent> <target> <payload-json>
call() {
  python3 - <<PY
import asyncio, json, websockets
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

### Test 1: spawn + reflect running

```bash
TB=$(call w core '{"type":"create_agent","handler_module":"terminal_backend.tools","cmd":["/bin/sh"],"auto_start":false}' | jq -r '.id')
call w $TB '{"type":"spawn","cmd":["/bin/sh"]}' | jq -e '.spawned == true and (.pid | type == "number")'
call w $TB '{"type":"reflect"}' | jq -e '.running == true'
```

Expect: spawn reports a pid; reflect shows `running: true`.

### Test 2: write + output scrollback round-trip

```bash
call w $TB '{"type":"write","data":"echo wired-up\n"}' | jq -e '.written >= 1'
sleep 0.4
call w $TB '{"type":"output"}' | jq -e '.output | contains("wired-up")'
```

Expect: the echoed string lands in the scrollback ring.

### Test 3: write on a stopped PTY → failfast

```bash
call w $TB '{"type":"stop"}' | jq -e '.stopped == true'
call w $TB '{"type":"reflect"}' | jq -e '.running == false'
call w $TB '{"type":"write","data":"x"}' | jq -e '.error == "not running"'
```

Expect: stop reaps the child; reflect flips to `running: false`; a
post-stop `write` is refused with `not running`.

### Test 4: on_delete reaps the PTY child

The substrate calls `on_delete` depth-first during cascade-delete;
terminal_backend's hook SIGKILLs the child so a deleted record can't
leak a live PTY emitting into a dead inbox.

```bash
TB2=$(call w core '{"type":"create_agent","handler_module":"terminal_backend.tools","cmd":["/bin/sh"]}' | jq -r '.id')
sleep 0.3
PID=$(call w $TB2 '{"type":"reflect"}' | jq -r '.pid')
call w core "{\"type\":\"delete_agent\",\"id\":\"$TB2\"}" | jq -e '.removed == true or .deleted == true'
sleep 0.5
kill -0 "$PID" 2>/dev/null && echo "FAIL pid $PID alive" || echo "PASS pid $PID gone"
```

Expect: `PASS pid <N> gone`. Regression signal: pid alive → on_delete
missing or cascade stopped invoking it.

## Cleanup

```bash
kill $DAEMON_PID 2>/dev/null
rm -rf /tmp/ftb_test
```

## Summary table

| Test | Pass / Fail | Notes |
|---|---|---|
| 1. spawn + reflect running |  |  |
| 2. write + output round-trip |  |  |
| 3. write on stopped → error |  |  |
| 4. on_delete reaps PTY child |  |  |
