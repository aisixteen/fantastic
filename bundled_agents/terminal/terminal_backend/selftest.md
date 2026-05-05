# terminal_backend selftest

> scopes: kernel, pty, http
> requires: `uv sync`; a real shell at `/bin/sh`; tests run against a
> running `kernel.py serve` (PTYs are process-memory; can't survive
> separate `python kernel.py call` invocations)
> out-of-scope: HTTP routes, browser xterm rendering

PTY shell agent. Done-token shell verb, timeout recovery, scrollback.

**Why a running serve is required:** the PTY is a child process of the
running kernel. `python kernel.py call …` spawns a fresh kernel for
each invocation; the PTY would be killed between calls. We use one
persistent kernel via `serve` and drive it through HTTP `POST /<id>/call`.

## Pre-flight

```bash
cd new_codebase
rm -rf .fantastic
PORT=18910
pkill -9 -f "kernel.py serve" 2>/dev/null; sleep 0.3
uv run --active python kernel.py serve --port $PORT > /tmp/s.log 2>&1 &
SPID=$!
for i in $(seq 1 20); do grep -q "kernel up" /tmp/s.log 2>/dev/null && break; sleep 0.5; done

# helper
call() { curl -s -X POST "http://localhost:$PORT/$1/call" -H 'content-type: application/json' -d "$2"; }
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

### Test 8: shutdown lifecycle — core.delete_agent kills the PTY

The kernel's universal teardown hook: `core.delete_agent` sends
`{type:"shutdown"}` to the agent before removing the record. The
backend's `shutdown` verb runs `_cleanup` → SIGKILLs the child.
Without this, a deleted record would leak a live PTY emitting
output to a dead inbox, ghost-spawning sprites in telemetry views.

```bash
TB2=$(call core '{"type":"create_agent","handler_module":"terminal_backend.tools"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
PID=$(call $TB2 '{"type":"reflect"}' | python -c "import json,sys;print(json.load(sys.stdin).get('pid',''))")
call core "{\"type\":\"delete_agent\",\"id\":\"$TB2\"}" >/dev/null
sleep 0.5
[ -n "$PID" ] && (kill -0 "$PID" 2>/dev/null && echo "FAIL pid $PID still alive" || echo "PASS pid $PID gone")
```
Expected: `PASS pid <N> gone`. Regression signal: pid alive → shutdown
verb missing OR core.delete_agent stopped sending it.

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
| 8 | shutdown lifecycle: core.delete_agent → PTY child reaped | |
