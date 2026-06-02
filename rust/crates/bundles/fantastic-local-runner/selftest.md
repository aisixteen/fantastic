# fantastic-local-runner selftest

> scopes: kernel, lifecycle
> requires: `cargo build --release --bin fantastic`; `fantastic` on PATH (or set `FANTASTIC_BIN`)
> out-of-scope: SSH lifecycle (ssh_runner selftest), remote messaging (kernel_bridge)

Local `fantastic` lifecycle as an agent. Each agent owns one project
dir; verbs spawn / signal a `fantastic` SUBPROCESS and read truth from
that project's own `.fantastic/lock.json` (pid) + its persisted `web`
record (port). No SSH, no tunnels.

The inner kernel it spawns is a stateful daemon — but `local_runner`
verbs are driven one-shot here: `start` polls lock.json (~30s),
`status`/`stop`/`get_webapp` read it back off disk, so the outer CLI
never needs its own live serve. (Compare stateful bundles — ollama /
terminal_backend / web — which DO need a live daemon + WS to exercise.)

## Pre-flight

```bash
rm -rf /tmp/lr_test
mkdir -p /tmp/lr_test /tmp/lr_proj
cd /tmp/lr_test
FANTASTIC=/path/to/rust/target/release/fantastic
export FANTASTIC_BIN=$FANTASTIC
$FANTASTIC core create_agent handler_module=local_runner.tools id=lr remote_path=/tmp/lr_proj
```

## Tests

### Test 1: reflect — record fields + idle status

```bash
$FANTASTIC lr reflect | jq -e '.remote_path == "/tmp/lr_proj" and .running == false and .pid == null'
$FANTASTIC lr reflect | jq -e '.verbs | has("start") and has("stop") and has("get_webapp")'
```

Expect: `running:false`, `pid:null`, `port:null` before any `start`.

### Test 2: start — spawns inner kernel, writes lock.json

```bash
$FANTASTIC lr start | jq -e '.started == true and (.pid|type == "number") and (.port|type == "number")'
test -f /tmp/lr_proj/.fantastic/lock.json
PID=$($FANTASTIC lr status | jq -r '.pid'); ps -p $PID >/dev/null
```

Expect: a `web` record auto-created in the project, lock.json present,
the recorded pid alive.

### Test 3: status + start are idempotent

```bash
$FANTASTIC lr status | jq -e '.running == true and (.pid|type == "number")'
$FANTASTIC lr start | jq -e '.already_running == true'   # NO second subprocess
```

### Test 4: stop — pid dies, lock cleared; status goes idle

```bash
$FANTASTIC lr stop | jq -e '.stopped == true'
test ! -f /tmp/lr_proj/.fantastic/lock.json
$FANTASTIC lr status | jq -e '.running == false and .pid == null'
```

Expect: SIGTERM (SIGKILL after 6s), stale lock removed. `get_webapp`
now returns `{error}` (not running).

## Cleanup

```bash
$FANTASTIC lr stop 2>/dev/null || true   # ensure inner kernel is down
rm -rf /tmp/lr_test /tmp/lr_proj
```

## Summary table

| Test | Pass / Fail | Notes |
|---|---|---|
| 1. reflect + idle status |  |  |
| 2. start spawns + lock.json |  |  |
| 3. status + start idempotent |  |  |
| 4. stop clears + idle |  |  |
