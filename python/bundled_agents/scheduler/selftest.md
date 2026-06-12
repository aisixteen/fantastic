# scheduler selftest

> scopes: kernel, persistence, time
> requires: `uv sync`
> out-of-scope: HTTP, WS, AI

Tick-loop + schedule persistence routed through file_bridge.

## Pre-flight

```bash
cd new_codebase
rm -rf .fantastic
```

## Tests

### Test 1: schedule without file_bridge_id → failfast

```bash
SC=$(fantastic kernel_state create_agent handler_module=scheduler.tools | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
fantastic $SC schedule target=cli payload='{"type":"say","text":"x"}' interval_seconds=5
```
Expected: `{"error":"scheduler: file_bridge_id required"}`.

### Test 2: configure with file_bridge_id, then schedule persists

```bash
FA=$(fantastic kernel_state create_agent handler_module=file_bridge.tools root=.fantastic ingress_rule=allow_all | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
fantastic kernel_state update_agent id=$SC file_bridge_id=$FA
SCH=$(fantastic $SC schedule target=cli payload='{"type":"say","text":"hello"}' interval_seconds=60 | python -c "import json,sys;print(json.load(sys.stdin)['schedule_id'])")
test -f .fantastic/agents/$SC/schedules.json && echo "OK persisted via file_bridge agent"
```
Expected: `OK persisted via file_bridge agent`. File contents include the schedule_id.

### Test 3: tick_now fires synchronously

```bash
fantastic $SC tick_now schedule_id=$SCH
```
Expected: `{"fired":true,"schedule_id":"<SCH>"}`.

### Test 4: history.jsonl populated

```bash
test -f .fantastic/agents/$SC/history.jsonl && grep -c "schedule_fired" .fantastic/agents/$SC/history.jsonl
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
| 1 | schedule fails without file_bridge_id | |
| 2 | schedule persists via file_bridge agent | |
| 3 | tick_now fires | |
| 4 | history.jsonl populated | |
| 5 | pause + resume toggle | |
| 6 | unschedule removes | |
| 7 | history endpoint | |
