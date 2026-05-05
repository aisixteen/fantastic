# scheduler selftest

> scopes: kernel, persistence, time
> requires: `uv sync`
> out-of-scope: HTTP, WS, AI

Tick-loop + schedule persistence routed through file_agent.

## Pre-flight

```bash
cd new_codebase
rm -rf .fantastic
```

## Tests

### Test 1: schedule without file_agent_id → failfast

```bash
SC=$(uv run python kernel.py call core create_agent handler_module=scheduler.tools | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
uv run python kernel.py call $SC schedule target=cli payload='{"type":"say","text":"x"}' interval_seconds=5
```
Expected: `{"error":"scheduler: file_agent_id required"}`.

### Test 2: configure with file_agent_id, then schedule persists

```bash
FA=$(uv run python kernel.py call core create_agent handler_module=file.tools | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
uv run python kernel.py call core update_agent id=$SC file_agent_id=$FA
SCH=$(uv run python kernel.py call $SC schedule target=cli payload='{"type":"say","text":"hello"}' interval_seconds=60 | python -c "import json,sys;print(json.load(sys.stdin)['schedule_id'])")
test -f .fantastic/agents/$SC/schedules.json && echo "OK persisted via file agent"
```
Expected: `OK persisted via file agent`. File contents include the schedule_id.

### Test 3: tick_now fires synchronously

```bash
uv run python kernel.py call $SC tick_now schedule_id=$SCH
```
Expected: `{"fired":true,"schedule_id":"<SCH>"}`.

### Test 4: history.jsonl populated

```bash
test -f .fantastic/agents/$SC/history.jsonl && grep -c "schedule_fired" .fantastic/agents/$SC/history.jsonl
```
Expected: ≥ 1.

### Test 5: pause + resume

```bash
uv run python kernel.py call $SC pause schedule_id=$SCH
uv run python kernel.py call $SC list | python -m json.tool | grep -F '"paused": true'
uv run python kernel.py call $SC resume schedule_id=$SCH
uv run python kernel.py call $SC list | python -m json.tool | grep -F '"paused": false'
```
Expected: each grep matches.

### Test 6: unschedule

```bash
uv run python kernel.py call $SC unschedule schedule_id=$SCH
```
Expected: `{"removed":true,"schedule_id":"<SCH>"}`.

### Test 7: history endpoint

```bash
uv run python kernel.py call $SC history limit=10 | python -m json.tool | grep -F '"count"'
```
Expected: `"count": <N>` with N ≥ 1.

## Summary

| # | Test | Pass |
|---|------|------|
| 1 | schedule fails without file_agent_id | |
| 2 | schedule persists via file agent | |
| 3 | tick_now fires | |
| 4 | history.jsonl populated | |
| 5 | pause + resume toggle | |
| 6 | unschedule removes | |
| 7 | history endpoint | |
