# fantastic-kernel selftest

> scopes: substrate, persistence, save/load, storage, lock
> requires: `cargo build --release --bin fantastic`
> out-of-scope: HTTP, WS, bundles other than core/file

Substrate-level — Agent + Kernel + send/emit/watch + persistence +
storage modes + save/load + lock + reflect + weak-load. No HTTP, no
WS. The contract that every bundle test depends on.

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

### Test 2: persistence round-trip — merge-only

```bash
$FANTASTIC core create_agent handler_module=file.tools id=foo root=/tmp/fk_root
test -f .fantastic/agents/foo/agent.json
jq -e '.handler_module == "file.tools"' .fantastic/agents/foo/agent.json
# Plant a custom field the kernel doesn't know:
jq '. + {user_note: "preserve me"}' .fantastic/agents/foo/agent.json \
  > /tmp/foo.json && mv /tmp/foo.json .fantastic/agents/foo/agent.json
# Mutate a kernel-managed field:
$FANTASTIC core update_agent id=foo root=/tmp/fk_root_v2
# The custom field must survive the merge:
jq -e '.user_note == "preserve me" and .root == "/tmp/fk_root_v2"' .fantastic/agents/foo/agent.json
```

Expect: child agent persisted with handler_module + meta intact.
Subsequent updates merge into the existing `agent.json` — fields
the kernel doesn't manage (the dirty-binding contract).

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

### Test 6: in-memory bootstrap + save/load round-trip

```bash
cd /path/to/fantastic_canvas/rust
# In-memory boot leaves zero fs trace:
cargo test -p fantastic-kernel --test in_memory_bootstrap
# kernel.save() / kernel.load() round-trip across modes:
cargo test -p fantastic-kernel --test save_load_roundtrip
```

Expect: both test files pass. Together they assert:
- `BootstrapOptions::in_memory()` boots a kernel with no
  `.fantastic/` dir appearing anywhere on disk
- `kernel.save()` returns a `KernelState` (pure in-RAM type — never
  written as a `state.json` file anywhere)
- `kernel.save_json()` output is byte-deterministic (agents sorted
  by id)
- Round-trip Disk-mode kernel → `save_json()` → InMemory kernel →
  `load_json()` preserves the agent tree
- Weak-load drops unknown handler_modules during `load`
- Snapshot validation rejects: future schema version, missing root,
  duplicate ids, dangling parent references
- The persistence merge-only contract: existing agent.json fields
  the kernel doesn't manage survive a subsequent update_agent call
  (dirty binding)

## Summary table

| Test | Pass / Fail | Notes |
|---|---|---|
| 1. cold-boot writes root record |  |  |
| 2. persistence round-trip (merge) |  |  |
| 3. cascade delete |  |  |
| 4. weak-load skip+log |  |  |
| 5. lock.json + PID liveness |  |  |
| 6. in-memory boot + save/load |  |  |
