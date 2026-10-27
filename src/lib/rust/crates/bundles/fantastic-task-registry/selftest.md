# fantastic-task-registry selftest

> scopes: kernel, tasks
> requires: `cargo build --release --bin fantastic_kernel`
> out-of-scope: a live store behind `task_store_id`. `ram_store` forgets everything
> when the one-shot process exits, so the verbs against a store are exercised by the
> unit tests (a kernel that stays up); here only the off-by-default contract is checked

A generic task registry. **Off by default**: the host turns it on with
`FANTASTIC_TASK_REGISTRY=1`, and the registry agent's record carries its config
(`task_store_id`: the store agent that keeps the tables). One-shot CLI:
`fantastic <id> <verb> [k=v ...]`; JSON-valued args are coerced.

## Pre-flight

All test state lives under `/tmp/tr_test/`.

```bash
rm -rf /tmp/tr_test && mkdir -p /tmp/tr_test && cd /tmp/tr_test
export FANTASTIC_TASK_REGISTRY=1
FANTASTIC=/path/to/rust/target/release/fantastic_kernel
TR=$($FANTASTIC kernel_state create_agent handler_module=task_registry.tools | jq -r .id)
```

## Tests

### Test 1: reflect says it is not configured

```bash
$FANTASTIC $TR reflect | jq -e '.enabled == false'
```

### Test 2: every other verb is refused `disabled` without a task_store_id

```bash
$FANTASTIC $TR list | jq -e '.reason == "disabled"'
$FANTASTIC $TR add id=t1 source=gh:o/r#1 | jq -e '.reason == "disabled"'
```

## Cleanup

```bash
$FANTASTIC kernel_state delete_agent id=$TR
rm -rf /tmp/tr_test
```

## Summary table

| Test | Pass / Fail | Notes |
|---|---|---|
| 1. reflect | | |
| 2. disabled without a store | | |
