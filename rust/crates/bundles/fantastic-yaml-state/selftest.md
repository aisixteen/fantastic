# fantastic-yaml-state selftest

> scopes: persistence, kernel
> requires: `cargo build --release --bin fantastic`
> out-of-scope: the FM auto-inject hook (Swift-only)

Durable YAML key-value memory agent. CRUD (`read`/`keys`/`set`/`delete`/
`replace`/`state_yaml`), disk-is-truth, mode sentence, cascade cleanup.
One-shot CLI: `fantastic <id> <verb> [k=v ...]`.

## Pre-flight

All test state lives under `/tmp/ys_test/`.

```bash
rm -rf /tmp/ys_test && mkdir -p /tmp/ys_test && cd /tmp/ys_test
FANTASTIC=/path/to/rust/target/release/fantastic
YS=$($FANTASTIC core create_agent handler_module=yaml_state.tools mode=mem | jq -r .id)
```

## Tests

### Test 1: set + read round-trip

```bash
$FANTASTIC $YS set key=user.name value=Ada
$FANTASTIC $YS read key=user.name | jq -e '.value == "Ada"'
$FANTASTIC $YS read key=nope | jq -e '.value == null'
```

### Test 2: keys survey — sorted, with sizes

```bash
$FANTASTIC $YS set key=decision.db value="postgres for JSON"
$FANTASTIC $YS keys | jq -e '[.keys[].key] == ["decision.db","user.name"]'
```

### Test 3: state_yaml is the injected block

```bash
$FANTASTIC $YS state_yaml | jq -r .yaml | grep -F 'user.name: Ada'
```

### Test 4: disk-is-truth — the YAML file IS the authoritative copy

```bash
find /tmp/ys_test/.fantastic -name state.yaml -exec grep -l 'user.name: Ada' {} \;
```

### Test 5: read whole doc (key omitted)

```bash
$FANTASTIC $YS read | jq -e '.doc["user.name"] == "Ada"'
```

### Test 6: delete prunes a key

```bash
$FANTASTIC $YS delete key=decision.db | jq -e '.deleted == true'
$FANTASTIC $YS read key=decision.db | jq -e '.value == null'
```

### Test 7: replace overwrites the whole store

```bash
$FANTASTIC $YS replace doc='{"only":"this"}' | jq -e '.replaced == true'
$FANTASTIC $YS read | jq -e '.doc.only == "this"'
```

### Test 8: reflect — mode drives the sentence (mem vs data)

```bash
$FANTASTIC $YS reflect | jq -e '.mode == "mem" and (.sentence | contains("durable memory"))'
DS=$($FANTASTIC core create_agent handler_module=yaml_state.tools mode=data | jq -r .id)
$FANTASTIC $DS reflect | jq -e '.mode == "data" and (.sentence | contains("scratch-state"))'
```

### Test 9: cascade-delete removes the agent AND its YAML file

```bash
$FANTASTIC core delete_agent id=$YS
test ! -d /tmp/ys_test/.fantastic/agents/$YS && echo OK
```

## Summary table

| Test | Pass / Fail | Notes |
|---|---|---|
| 1. set + read |  |  |
| 2. keys sorted |  |  |
| 3. state_yaml |  |  |
| 4. disk-is-truth |  |  |
| 5. read whole doc |  |  |
| 6. delete prunes |  |  |
| 7. replace |  |  |
| 8. reflect mode |  |  |
| 9. cascade cleanup |  |  |
