# fantastic-python-runtime selftest

> scopes: compute
> requires: `cargo build --release --bin fantastic`; `python3` on PATH
> out-of-scope: async job lifecycle, sandboxing, venv resolution (covered by `python_runtime_resolution.md`)

Subprocess Python exec. `exec` runs `python -c <code>` per call and BLOCKS
until it returns `{stdout, stderr, exit_code, timed_out}` — synchronous,
stateless per-call (no job table, no `start`/`job_id` like the Python peer).
`interrupt`/`stop` signal the in-flight PIDs for that agent. A one-shot
`fantastic <id> <verb>` is a full kernel boot, so every verb here checks
end-to-end against `core`.

## Pre-flight

```bash
rm -rf .fantastic
FANTASTIC=/path/to/rust/target/release/fantastic
$FANTASTIC core create_agent handler_module=python_runtime.tools id=pr
```

## Tests

### Test 1: reflect lists the verbs + 0 in-flight

```bash
$FANTASTIC pr reflect | jq -e '.in_flight == 0 and ([.verbs | keys[]] | contains(["exec","interrupt","stop","boot","reflect"]))'
```

Expect: `exec`, `interrupt`, `stop`, `boot`, `reflect` all present;
`in_flight` is 0 on a fresh agent.

### Test 2: exec round-trips stdout + exit_code

```bash
$FANTASTIC pr exec code='print(2*21)' | jq -e '.stdout == "42\n" and .exit_code == 0 and .timed_out == false'
```

### Test 3: exec rejects empty code

```bash
$FANTASTIC pr exec code='' | jq -e '.error | contains("code (str) required")'
```

### Test 4: unknown verb errors

```bash
$FANTASTIC pr garbage | jq -e '.error | contains("unknown type")'
```

## Cleanup

```bash
rm -rf .fantastic
```

## Summary

| # | Test | Pass / Fail | Notes |
|---|------|---|---|
| 1 | reflect lists exec/interrupt/stop/… + in_flight=0 | | |
| 2 | exec round-trips stdout + exit_code | | |
| 3 | exec rejects empty code | | |
| 4 | unknown verb errors | | |
