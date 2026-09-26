# scheduler selftest

> scopes: kernel, persistence, time
> requires: `uv sync`
> out-of-scope: HTTP, WS, AI

Tick-loop + schedule persistence THROUGH the loader (`persist_blob`/`load_blob`) — nothing to wire.

## Pre-flight

```bash
cd new_codebase
rm -rf .fantastic
```

## Tests

### Test 1: schedule without a store → failfast (boot still succeeds)

```bash
SC=$(fantastic kernel create_agent parent_id=kernel_state handler_module=scheduler.tools | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
fantastic $SC schedule target=cli payload='{"type":"say","text":"x"}' interval_seconds=5
```
Expected: `{"error":"scheduler.schedule: persist_blob: no store wired — persistence not available"}` (and `reflect` reports `running: true` — boot never needs a store).

### Test 2: open a store at `.fantastic`, then schedule persists through the loader

```bash
FA=$(fantastic kernel create_agent parent_id=kernel_state handler_module=file_bridge.tools root=.fantastic ingress_rule=allow_all_ingress | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
SCH=$(fantastic $SC schedule target=cli payload='{"type":"say","text":"hello"}' interval_seconds=60 | python -c "import json,sys;print(json.load(sys.stdin)['schedule_id'])")
test -f .fantastic/agents/$SC/schedules.yaml && echo "OK persisted through the loader"
```
Expected: `OK persisted through the loader`. File contents include the schedule_id.

### Test 3: tick_now fires synchronously

```bash
fantastic $SC tick_now schedule_id=$SCH
```
Expected: `{"fired":true,"schedule_id":"<SCH>"}`.

### Test 4: schedule_events.yaml populated

```bash
test -f .fantastic/agents/$SC/schedule_events.yaml && grep -c "schedule_fired" .fantastic/agents/$SC/schedule_events.yaml
```
Expected: ≥ 1.

### Test 5: pause + resume

```bash
fantastic $SC pause schedule_id=$SCH
fantastic $SC list | python -m json.tool | grep -F '"paused": true'
fantastic $SC resume schedule_id=$SCH
fantastic $SC list | python -m json.tool | grep -F '"paused": false'
```
Expected: each grep matches.

### Test 6: unschedule

```bash
fantastic $SC unschedule schedule_id=$SCH
```
Expected: `{"removed":true,"schedule_id":"<SCH>"}`.

### Test 7: history endpoint

```bash
fantastic $SC history limit=10 | python -m json.tool | grep -F '"count"'
```
Expected: `"count": <N>` with N ≥ 1.

## Summary

| # | Test | Pass |
|---|------|------|
| 1 | schedule fails without a store (boot succeeds) | |
| 2 | schedule persists through the loader | |
| 3 | tick_now fires | |
| 4 | schedule_events.yaml populated | |
| 5 | pause + resume toggle | |
| 6 | unschedule removes | |
| 7 | history endpoint | |
