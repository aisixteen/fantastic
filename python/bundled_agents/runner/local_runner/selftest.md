# local_runner — selftest

scopes: `kernel`

`local_runner` manages `fantastic` as a subprocess on this
machine. Each agent represents one project; verbs spawn / signal
the kernel and read truth from the project's own
`<remote_path>/.fantastic/lock.json`.

## Pre-flight

- A local project directory you don't mind starting/stopping a
  fantastic kernel inside (e.g. a fresh `tmp/test_local_runner/`).
- The `fantastic` CLI on PATH (verify: `which fantastic`).
- A running outer `fantastic` serve. Set `PORT=<its port>` in your
  shell before running the snippets below.

```bash
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

## 1. reflect — record fields surface

```bash
mkdir -p /tmp/test_local_runner
ID=$(call core '{"type":"create_agent","handler_module":"local_runner.tools",
       "remote_path":"/tmp/test_local_runner",
       "display_name":"trial"}' | python -m json.tool | grep '"id"' | awk -F'"' '{print $4}')
call $ID '{"type":"reflect"}' | python -m json.tool
```

Expected: `running:false`, `pid:null`, `port:null`, `verbs` lists
`start/stop/restart/status/get_webapp` etc. (no `shutdown` verb —
cascade teardown happens through the `on_delete` hook now).

## 2. start — spawns + writes lock.json

```bash
call $ID '{"type":"start"}' | python -m json.tool
```

Expected: `started:true`, `pid:<int>`, `port:<int>`. Verify on disk:

```bash
cat /tmp/test_local_runner/.fantastic/lock.json
ps -p <pid>
```

## 3. status — ws_ok proves end-to-end liveness

```bash
call $ID '{"type":"status"}' | python -m json.tool
```

Expected: `running:true`, `ws_ok:true`. If `ws_ok:false` while
`running:true`, the kernel started but isn't answering yet — wait
0.5s and re-probe. Persistent `ws_ok:false` is a regression flag.

## 4. get_webapp — URL points at the live serve

```bash
call $ID '{"type":"get_webapp"}' | python -m json.tool
```

Expected: `url == "http://localhost:<port>/"` (matches lock.json port).
Open that URL in a browser — should return the inner project's
fantastic index page.

## 5. start (already running) — idempotent

```bash
call $ID '{"type":"start"}' | python -m json.tool
```

Expected: `started:true`, `already_running:true`, same pid as before.
NO second subprocess spawned.

## 6. stop — pid dies, lock cleared

```bash
call $ID '{"type":"stop"}' | python -m json.tool
```

Expected: `stopped:true`, `died_cleanly:true`. Verify:

```bash
ps -p <pid>           # should report no process
ls /tmp/test_local_runner/.fantastic/lock.json    # should not exist
```

## 7. status — running:false after stop

```bash
call $ID '{"type":"status"}' | python -m json.tool
```

Expected: `running:false`, `pid:null`, `ws_ok:false`.

## 8. restart — stop+start in one call

```bash
call $ID '{"type":"restart"}' | python -m json.tool
```

Expected: `started:true` with a NEW pid (different from step 2's
pid). Lock.json reflects the new pid+port.

## 9. cascade-delete fires `on_delete` — universal lifecycle hook

```bash
call core "{\"type\":\"delete_agent\",\"id\":\"$ID\"}" | python -m json.tool
```

Expected: agent record gone, the running serve killed (its pid no
longer in `ps`), lock.json removed. The substrate invokes
`local_runner.on_delete` (which calls `_stop`) before detaching the
record.

## Cleanup

```bash
rm -rf /tmp/test_local_runner
```

## Pitfalls

- `start` polls lock.json for up to 30s. If `fantastic` ImportError-s
  on the spawned kernel, lock.json never appears → `start` returns
  `{error: "lock.json never appeared"}`. Tail
  `/tmp/test_local_runner/.fantastic/serve.log` for the traceback.
- `stop` waits up to 6s for the SIGTERMed pid to die before
  escalating to SIGKILL. If your project has an atexit hook that
  blocks (e.g., flushing a DB), the wait runs to the limit.
- `get_webapp` returns `{error}` when the project isn't running. The
  canvas filters errored probes (`hasWa = wa && wa.url && !wa.error`),
  so dead instances render no frame. Start them first to see them.
