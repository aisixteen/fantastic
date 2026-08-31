# fantastic-durable-memory selftest

> scopes: persistence, kernel
> requires: `cargo build --release --bin fantastic_kernel`
> out-of-scope: the FM auto-inject hook (Swift-only)

Modeless durable key-value memory agent: `set` / `read` / `append` / `keys` /
`delete`, disk-is-truth, cascade cleanup. One-shot CLI: `fantastic <id> <verb>
[k=v ...]`.

durable_memory owns NO disk surface of its own — it persists `state.yaml`
THROUGH a `file_bridge` agent (deny-all by default), referenced by
`file_bridge_id`, over `write_stream`/`read_stream`. Wire it to the `.fantastic`
store; `set` fails fast until it is wired (Test 0). There is no `mode`: the key
names its purpose.

## Pre-flight

All test state lives under `/tmp/dm_test/`. Nothing written to the project tree.

```bash
rm -rf /tmp/dm_test && mkdir -p /tmp/dm_test && cd /tmp/dm_test
FANTASTIC=/path/to/rust/target/release/fantastic_kernel
```

## Tests

### Test 0: set without a wired provider → failfast (deny-by-default)

```bash
# a store so the UNWIRED agent's RECORD persists across one-shot calls; its own
# file_bridge_id is still unset, so `set` must fail fast (not silently fall to RAM).
$FANTASTIC core create_agent handler_module=file_bridge.tools root=.fantastic ingress_rule=allow_all_ingress >/dev/null
UW=$($FANTASTIC core create_agent handler_module=durable_memory.tools | jq -r .id)
$FANTASTIC $UW set key=k value=v | jq -e '.error | contains("file_bridge_id")'
```
Expected: an error naming `file_bridge_id` — no silent RAM write.

### Test 1: wire the `.fantastic` store, then set + read round-trip

```bash
# the .fantastic store: one file_bridge serves both record persistence AND
# durable_memory's sidecar (store-relative `agents/<id>/state.yaml`, next to agent.json).
FA=$($FANTASTIC core create_agent handler_module=file_bridge.tools root=.fantastic ingress_rule=allow_all_ingress | jq -r .id)
DM=$($FANTASTIC core create_agent handler_module=durable_memory.tools file_bridge_id=$FA | jq -r .id)
$FANTASTIC $DM set key=user.name value=Ada
$FANTASTIC $DM read key=user.name | jq -e '.value == "Ada"'
$FANTASTIC $DM read key=nope | jq -e '.value == null'
```
Expected: both checks pass. Missing key → `{value: null}`.

### Test 2: keys survey — sorted list

```bash
$FANTASTIC $DM set key=decision.db value="postgres for JSON"
$FANTASTIC $DM keys | jq -e '.keys == ["decision.db","user.name"]'
```
Expected: the sorted key list (the table of contents).

### Test 3: append builds a list at a key

```bash
$FANTASTIC $DM append key=log value=first | jq -e '.length == 1'
$FANTASTIC $DM append key=log value=second | jq -e '.length == 2'
$FANTASTIC $DM read key=log | jq -e '.value == ["first","second"]'
```
Expected: append grows the list; a non-list key is refused (`set` it to a list first).

### Test 4: disk-is-truth — the YAML file IS the authoritative copy

```bash
find /tmp/dm_test/.fantastic -name state.yaml -exec grep -l 'user.name: Ada' {} \;
```
Expected: prints the `state.yaml` path (human-editable, git-diffable).

### Test 5: delete prunes a key

```bash
$FANTASTIC $DM delete key=decision.db | jq -e '.deleted == true'
$FANTASTIC $DM read key=decision.db | jq -e '.value == null'
```
Expected: `deleted: true`, then the value is null. Re-deleting → `deleted: false` (no error).

### Test 6: reflect — the sentence is the record's description

```bash
DESCR=$($FANTASTIC core create_agent handler_module=durable_memory.tools file_bridge_id=$FA description="user preferences" | jq -r .id)
$FANTASTIC $DESCR reflect | jq -e '.sentence | contains("user preferences")'
```
Expected: the `description` is what `reflect` reports as the sentence — no bundle-private `mode` enum.

### Test 7: cascade-delete removes the agent AND its YAML file

```bash
$FANTASTIC core delete_agent id=$DM
test ! -d /tmp/dm_test/.fantastic/agents/$DM && echo OK
```
Expected: `OK` — the substrate cascade removed the agent dir (and `state.yaml`).

## Cleanup

```bash
cd / && rm -rf /tmp/dm_test
```

## Summary

| # | Test | Pass |
|---|------|------|
| 0 | set failfast without a wired provider | |
| 1 | wire store, set + read round-trip | |
| 2 | keys survey (sorted) | |
| 3 | append builds a list | |
| 4 | disk-is-truth (state.yaml) | |
| 5 | delete prunes a key | |
| 6 | reflect sentence = description | |
| 7 | cascade-delete removes the dir | |
