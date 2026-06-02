# fantastic-scheduler selftest

> scopes: persistence, kernel, time
> requires: `cargo build --release --bin fantastic`
> out-of-scope: the autonomous tick loop (`boot`/`shutdown`) — that needs
> a live daemon + WS to observe `schedule_fired` over time; one-shot CLI
> exercises the synchronous surface via `tick_now`

Recurring-task scheduler. Schedules + fire history persist through the
configured `file_agent_id` (failfast if unset). One-shot CLI:
`fantastic <id> <verb> [k=v ...]`. JSON-valued args (`payload='{...}'`)
are coerced.

## Pre-flight

All test state lives under `/tmp/sc_test/`.

```bash
rm -rf /tmp/sc_test && mkdir -p /tmp/sc_test && cd /tmp/sc_test
FANTASTIC=/path/to/rust/target/release/fantastic
SC=$($FANTASTIC core create_agent handler_module=scheduler.tools | jq -r .id)
FA=$($FANTASTIC core create_agent handler_module=file.tools root=/tmp/sc_test | jq -r .id)
```

## Tests

### Test 1: schedule without file_agent_id → failfast

```bash
$FANTASTIC $SC schedule target=core payload='{"type":"reflect"}' interval_seconds=5 \
  | jq -e '.error == "scheduler: file_agent_id required"'
```

Expect: no implicit fallback — persistence is mandatory.

### Test 2: configure file_agent_id, then schedule persists

```bash
$FANTASTIC core update_agent id=$SC file_agent_id=$FA
SCH=$($FANTASTIC $SC schedule target=core payload='{"type":"reflect"}' interval_seconds=60 \
  | jq -r .schedule_id)
$FANTASTIC $SC list | jq -e --arg s "$SCH" '[.schedules[].id] | contains([$s])'
test -f /tmp/sc_test/.fantastic/agents/$SC/schedules.json && echo "OK persisted via file agent"
```

Expect: `OK persisted via file agent`; the file routes through the file agent.

### Test 3: tick_now fires synchronously + appends history

```bash
$FANTASTIC $SC tick_now schedule_id=$SCH | jq -e '.fired == true'
$FANTASTIC $SC history limit=10 | jq -e '.count >= 1 and (.history[-1].schedule_id == $SCH)' --arg SCH "$SCH"
test -f /tmp/sc_test/.fantastic/agents/$SC/history.jsonl && grep -q "schedule_fired" \
  /tmp/sc_test/.fantastic/agents/$SC/history.jsonl
```

### Test 4: pause + resume toggle, then unschedule removes

```bash
$FANTASTIC $SC pause schedule_id=$SCH | jq -e '.paused == true'
$FANTASTIC $SC list | jq -e --arg s "$SCH" '.schedules[] | select(.id == $s) | .paused == true'
$FANTASTIC $SC resume schedule_id=$SCH | jq -e '.resumed == true'
$FANTASTIC $SC list | jq -e --arg s "$SCH" '.schedules[] | select(.id == $s) | .paused == false'
$FANTASTIC $SC unschedule schedule_id=$SCH | jq -e '.removed == true'
$FANTASTIC $SC list | jq -e '.schedules == []'
```

## Cleanup

```bash
$FANTASTIC core delete_agent id=$SC
$FANTASTIC core delete_agent id=$FA
rm -rf /tmp/sc_test
```

## Summary table

| Test | Pass / Fail | Notes |
|---|---|---|
| 1. schedule fails without file_agent_id |  |  |
| 2. schedule persists via file agent |  |  |
| 3. tick_now fires + history |  |  |
| 4. pause/resume + unschedule |  |  |
