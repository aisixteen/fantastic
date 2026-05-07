# local_runner — selftest

scopes: `kernel`

`local_runner` manages `fantastic serve` as a subprocess on this
machine. Each agent represents one project; verbs spawn / signal
the kernel and read truth from the project's own
`<remote_path>/.fantastic/lock.json`.

## Pre-flight

- A local project directory you don't mind starting/stopping a
  fantastic kernel inside (e.g. a fresh `tmp/test_local_runner/`).
- The `fantastic` CLI on PATH (verify: `which fantastic`).

## 1. reflect — record fields surface

```bash
mkdir -p /tmp/test_local_runner
ID=$(curl -s -X POST http://localhost:<PORT>/core/call \
  -H 'content-type: application/json' \
  -d '{"type":"create_agent","handler_module":"local_runner.tools",
       "remote_path":"/tmp/test_local_runner",
       "display_name":"trial"}' | python -m json.tool | grep '"id"' | awk -F'"' '{print $4}')
curl -s -X POST http://localhost:<PORT>/$ID/call \
  -H 'content-type: application/json' \
  -d '{"type":"reflect"}' | python -m json.tool
```

Expected: `running:false`, `pid:null`, `port:null`, `verbs` lists
`start/stop/restart/status/get_webapp/shutdown` etc.

## 2. start — spawns + writes lock.json

```bash
curl -s -X POST http://localhost:<PORT>/$ID/call \
  -H 'content-type: application/json' \
  -d '{"type":"start"}' | python -m json.tool
```

Expected: `started:true`, `pid:<int>`, `port:<int>`. Verify on disk:

```bash
cat /tmp/test_local_runner/.fantastic/lock.json
ps -p <pid>
```

## 3. status — http_ok proves end-to-end liveness

```bash
curl -s -X POST http://localhost:<PORT>/$ID/call \
  -H 'content-type: application/json' \
  -d '{"type":"status"}' | python -m json.tool
```

Expected: `running:true`, `http_ok:true`. If `http_ok:false` while
`running:true`, the kernel started but isn't bound yet — wait 0.5s
and re-probe. Persistent `http_ok:false` is a regression flag.

## 4. get_webapp — URL points at the live serve

```bash
curl -s -X POST http://localhost:<PORT>/$ID/call \
  -H 'content-type: application/json' \
  -d '{"type":"get_webapp"}' | python -m json.tool
```

Expected: `url == "http://localhost:<port>/"` (matches lock.json port).
Open that URL in a browser — should return the inner project's
fantastic index page.

## 5. start (already running) — idempotent

```bash
curl -s -X POST http://localhost:<PORT>/$ID/call \
  -H 'content-type: application/json' \
  -d '{"type":"start"}' | python -m json.tool
```

Expected: `started:true`, `already_running:true`, same pid as before.
NO second subprocess spawned.

## 6. stop — pid dies, lock cleared

```bash
curl -s -X POST http://localhost:<PORT>/$ID/call \
  -H 'content-type: application/json' \
  -d '{"type":"stop"}' | python -m json.tool
```

Expected: `stopped:true`, `died_cleanly:true`. Verify:

```bash
ps -p <pid>           # should report no process
ls /tmp/test_local_runner/.fantastic/lock.json    # should not exist
```

## 7. status — running:false after stop

```bash
curl -s -X POST http://localhost:<PORT>/$ID/call \
  -H 'content-type: application/json' \
  -d '{"type":"status"}' | python -m json.tool
```

Expected: `running:false`, `pid:null`, `http_ok:false`.

## 8. restart — stop+start in one call

```bash
curl -s -X POST http://localhost:<PORT>/$ID/call \
  -H 'content-type: application/json' \
  -d '{"type":"restart"}' | python -m json.tool
```

Expected: `started:true` with a NEW pid (different from step 2's
pid). Lock.json reflects the new pid+port.

## 9. shutdown via core.delete_agent — universal lifecycle hook

```bash
curl -s -X POST http://localhost:<PORT>/core/call \
  -H 'content-type: application/json' \
  -d "{\"type\":\"delete_agent\",\"id\":\"$ID\"}" | python -m json.tool
```

Expected: agent record gone, the running serve killed (its pid no
longer in `ps`), lock.json removed. `core` calls `_shutdown` (which
aliases to `_stop`) before removing the record.

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
