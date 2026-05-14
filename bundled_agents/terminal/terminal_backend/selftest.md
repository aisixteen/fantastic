# terminal_backend selftest

> scopes: kernel, pty, http
> requires: `uv sync`; a real shell at `/bin/sh`; tests run against a
> running `fantastic` (PTYs are process-memory; can't survive
> separate `fantastic call` invocations)
> out-of-scope: HTTP routes, browser xterm rendering

PTY shell agent. Done-token shell verb, timeout recovery, scrollback.

**Why a running serve is required:** the PTY is a child process of the
running kernel. `fantastic call …` spawns a fresh kernel for
each invocation; the PTY would be killed between calls. We use one
persistent kernel via `serve` and drive it through a one-shot WS
`call` frame.

## Pre-flight

```bash
cd new_codebase
rm -rf .fantastic
PORT=18910
pkill -9 -f "fantastic" 2>/dev/null; sleep 0.3
uv run --active fantastic core create_agent handler_module=web.tools port=$PORT >/dev/null
# WS verb channel is now a sub-agent of web — spawn under web's id.
WEB_ID=$(ls .fantastic/agents | grep '^web_' | head -1)
uv run --active fantastic $WEB_ID create_agent handler_module=web_ws.tools >/dev/null
uv run --active fantastic > /tmp/s.log 2>&1 &
SPID=$!
for i in $(seq 1 20); do grep -q "kernel up" /tmp/s.log 2>/dev/null && break; sleep 0.5; done

# This helper opens a one-shot WS, sends a `call` frame, prints reply.
call() {
  TARGET="$1" PAYLOAD="$2" PORT="$PORT" uv run --active python - <<'PY'
import asyncio, json, os, websockets
target = os.environ["TARGET"]; payload = json.loads(os.environ["PAYLOAD"])
port = os.environ["PORT"]
async def main():
    async with websockets.connect(f"ws://localhost:{port}/{target}/ws") as ws:
        await ws.send(json.dumps({"type":"call","target":target,"payload":payload,"id":"1"}))
        while True:
            m = json.loads(await ws.recv())
            if m.get("id") == "1" and m.get("type") in ("reply","error"):
                print(json.dumps(m.get("data"))); return
asyncio.run(main())
PY
}
```

After all tests:
```bash
kill -9 $SPID 2>/dev/null; rm -rf .fantastic /tmp/s.log
```

## Tests

### Test 1: spawn + reflect

```bash
TB=$(call core '{"type":"create_agent","handler_module":"terminal_backend.tools","command":"/bin/sh"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
sleep 0.3
call $TB '{"type":"reflect"}' | python -m json.tool | grep -F '"running": true'
```
Expected: matches.

### Test 2: shell done-token fast command

```bash
call $TB '{"type":"shell","cmd":"echo hello-from-pty"}' | python -m json.tool | grep -F "hello-from-pty"
```
Expected: matches; `"completed": true` in same output.

### Test 3: shell timeout

```bash
START=$(python -c "import time;print(time.time())")
call $TB '{"type":"shell","cmd":"sleep 60","timeout":1}' | python -m json.tool | grep -F '"error": "timeout"'
END=$(python -c "import time;print(time.time())")
ELAPSED=$(python -c "print(f'{$END-$START:.2f}')")
echo "elapsed: $ELAPSED s"
```
Expected: error timeout reported, elapsed < 3s.

### Test 4: recover after timeout (Ctrl-C sent)

```bash
call $TB '{"type":"shell","cmd":"echo recovered"}'
```
Expected: completes with `"recovered"` in output.

### Test 5: write + output scrollback

```bash
call $TB '{"type":"write","data":"echo wired-up\n"}'
sleep 0.4
call $TB '{"type":"output"}' | python -m json.tool | grep -F "wired-up"
```
Expected: matches.

### Test 6: stop kills PTY

```bash
call $TB '{"type":"stop"}'
sleep 0.2
call $TB '{"type":"reflect"}' | python -m json.tool | grep -F '"running": false'
```
Expected: matches.

### Test 7: shell on stopped PTY → failfast

```bash
call $TB '{"type":"shell","cmd":"echo x"}' | python -m json.tool | grep -F "not running"
```
Expected: matches.

### Test 8: on_delete cascade hook — delete_agent kills the PTY

The substrate calls each agent's `on_delete` depth-first during
cascade-delete. terminal_backend's `on_delete` runs `_cleanup` →
SIGKILLs the child. Without this, a deleted record would leak a
live PTY emitting output to a dead inbox, ghost-spawning sprites in
telemetry views.

```bash
TB2=$(call core '{"type":"create_agent","handler_module":"terminal_backend.tools"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
PID=$(call $TB2 '{"type":"reflect"}' | python -c "import json,sys;print(json.load(sys.stdin).get('pid',''))")
call core "{\"type\":\"delete_agent\",\"id\":\"$TB2\"}" >/dev/null
sleep 0.5
[ -n "$PID" ] && (kill -0 "$PID" 2>/dev/null && echo "FAIL pid $PID still alive" || echo "PASS pid $PID gone")
```
Expected: `PASS pid <N> gone`. Regression signal: pid alive →
on_delete missing OR cascade stopped invoking it.

## Summary

| # | Test | Pass |
|---|------|------|
| 1 | spawn + reflect running | |
| 2 | shell done-token fast cmd | |
| 3 | shell timeout fires <3s | |
| 4 | recovers after timeout | |
| 5 | write + output round-trip | |
| 6 | stop kills PTY | |
| 7 | shell on stopped → error | |
| 8 | on_delete cascade hook → PTY child reaped | |
