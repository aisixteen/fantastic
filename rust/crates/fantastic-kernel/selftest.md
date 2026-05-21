# fantastic-kernel selftest

> scopes: substrate, persistence
> requires: `cargo build --release --bin fantastic`
> out-of-scope: HTTP, WS, bundles other than core/file

Substrate-level — Agent + Kernel + send/emit/watch + persistence +
lock + reflect + weak-load. No HTTP, no WS. The contract that every
bundle test depends on.

## Pre-flight

```bash
rm -rf /tmp/fk_test
mkdir -p /tmp/fk_test
cd /tmp/fk_test
FANTASTIC=/path/to/rust/target/release/fantastic
```

## Tests

### Test 1: cold-boot writes a root record

```bash
$FANTASTIC reflect >/tmp/fk_test/reflect1.json
test -f .fantastic/agent.json
jq -e '.id == "core"' .fantastic/agent.json
```

Expect: `.fantastic/agent.json` exists with `id=core` after first
invocation. Reflect returns the primer (transports + tree +
available_bundles).

### Test 2: persistence round-trip

```bash
$FANTASTIC core create_agent handler_module=file.tools id=foo root=/tmp/fk_root
test -f .fantastic/agents/foo/agent.json
jq -e '.handler_module == "file.tools"' .fantastic/agents/foo/agent.json
```

Expect: child agent persisted with handler_module + meta intact.

### Test 3: cascade delete removes subtree

```bash
$FANTASTIC core create_agent handler_module=file.tools id=tmp_a
$FANTASTIC tmp_a create_agent handler_module=file.tools id=tmp_b
$FANTASTIC core delete_agent id=tmp_a
test ! -e .fantastic/agents/tmp_a
test ! -e .fantastic/agents/tmp_a/agents/tmp_b
```

Expect: both agent directories are removed on cascade delete.
`tmp_b`'s `on_delete` hook fires before its parent's.

### Test 4: weak-load skip+log

```bash
mkdir -p .fantastic/agents/ghost_1
echo '{"id":"ghost_1","handler_module":"nonexistent.tools","parent_id":"core"}' \
  > .fantastic/agents/ghost_1/agent.json
$FANTASTIC reflect 2>&1 | tee /tmp/fk_test/boot.log
grep -q '\[kernel\] skipping agent ghost_1: bundle nonexistent.tools not installed in this runtime' \
  /tmp/fk_test/boot.log
jq -e '[.tree | recurse(.children[]?) | select(.id == "ghost_1")] | length == 0' \
  /tmp/fk_test/boot.log
```

Expect: ghost_1 is logged + skipped, NOT in the reflected tree. The
record `agent.json` is left untouched on disk for the next runtime.

### Test 5: lock.json + PID liveness

```bash
$FANTASTIC core create_agent handler_module=web.tools port=18181
$FANTASTIC &
DAEMON_PID=$!
sleep 1
test -f .fantastic/lock.json
jq -e ".pid == $DAEMON_PID" .fantastic/lock.json
# Second invocation refuses while the lock is held:
! $FANTASTIC reflect 2>&1 | grep -q "lock"
kill $DAEMON_PID
sleep 1
test ! -f .fantastic/lock.json
```

Expect: lock.json written on boot with current PID. Second invocation
in the same dir refuses with a clear error. Lock released on
graceful shutdown.

## Summary table

| Test | Pass / Fail | Notes |
|---|---|---|
| 1. cold-boot writes root record |  |  |
| 2. persistence round-trip |  |  |
| 3. cascade delete |  |  |
| 4. weak-load skip+log |  |  |
| 5. lock.json + PID liveness |  |  |
