# durable_memory selftest

> scopes: kernel, persistence
> requires: `uv sync`
> out-of-scope: HTTP, AI, the FM auto-inject hook (Swift-only)

Modeless durable key-value memory agent: `set` / `read` / `append` / `keys` /
`delete`, disk-is-truth, cascade cleanup. One-shot CLI form:
`fantastic <id> <verb> [k=v ...]`.

durable_memory owns NO disk surface of its own — it persists `state.yaml` THROUGH a
`file_bridge` agent (deny-all by default), referenced by `file_bridge_id`. Wire it to
the `.fantastic` store (the same one the loader persists records through); `set` fails
fast until it is wired (Test 0). There is no `mode`: the key names its purpose.

## Pre-flight

All state under `/tmp/dm_test/`. Nothing written to the project tree.

```bash
rm -rf /tmp/dm_test && mkdir -p /tmp/dm_test && cd /tmp/dm_test
```

## Tests

### Test 0: set without a wired provider → failfast (deny-by-default)

```bash
# a store so the UNWIRED agent's RECORD persists across one-shot calls; its own
# file_bridge_id is still unset, so `set` must fail fast (not silently fall to RAM).
fantastic kernel create_agent parent_id=kernel_state handler_module=file_bridge.tools root=.fantastic ingress_rule=allow_all_ingress >/dev/null
UW=$(fantastic kernel create_agent parent_id=kernel_state handler_module=durable_memory.tools | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
fantastic $UW set key=k value=v | python -m json.tool | grep -F 'file_bridge_id'
```
Expected: an error naming `file_bridge_id` — no silent RAM write.

### Test 1: wire the `.fantastic` store, then set + read round-trip

```bash
# the .fantastic store: one file_bridge serves both record persistence AND
# durable_memory's sidecar (store-relative `agents/<id>/state.yaml`, next to agent.json).
FA=$(fantastic kernel create_agent parent_id=kernel_state handler_module=file_bridge.tools root=.fantastic ingress_rule=allow_all_ingress | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
DM=$(fantastic kernel create_agent parent_id=kernel_state handler_module=durable_memory.tools file_bridge_id=$FA | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
fantastic $DM set key=user.name value=Ada
fantastic $DM read key=user.name | python -m json.tool | grep -F '"value": "Ada"'
```
Expected: grep matches. Missing key → `{"value": null}`.

### Test 2: keys survey — sorted list

```bash
fantastic $DM set key=decision.db value="postgres for JSON support"
fantastic $DM keys | python -c "import json,sys;print(json.load(sys.stdin)['keys'])" | grep -F "['decision.db', 'user.name']"
```
Expected: the sorted key list (the table of contents).

### Test 3: append builds a list at a key

```bash
fantastic $DM append key=log value=first | python -m json.tool | grep -F '"length": 1'
fantastic $DM append key=log value=second | python -m json.tool | grep -F '"length": 2'
fantastic $DM read key=log | python -c "import json,sys;print(json.load(sys.stdin)['value'])" | grep -F "['first', 'second']"
```
Expected: append grows the list; a non-list key is refused (`set` it to a list first).

### Test 4: disk-is-truth — the YAML file IS the authoritative copy

```bash
find /tmp/dm_test/.fantastic -name state.yaml -exec grep -l 'user.name: Ada' {} \;
```
Expected: prints the `state.yaml` path (human-editable, git-diffable).

### Test 5: delete prunes a key

```bash
fantastic $DM delete key=decision.db | python -m json.tool | grep -F '"deleted": true'
fantastic $DM read key=decision.db | python -m json.tool | grep -F '"value": null'
```
Expected: `deleted: true`, then the value is null. Re-deleting → `deleted: false` (no error).

### Test 6: reflect — the sentence is the record's description

```bash
DESCR=$(fantastic kernel create_agent parent_id=kernel_state handler_module=durable_memory.tools file_bridge_id=$FA description="user preferences" | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
fantastic $DESCR reflect | python -c "import json,sys;print(json.load(sys.stdin)['sentence'])" | grep -F 'user preferences'
```
Expected: the `description` is what `reflect` reports as the sentence — no bundle-private `mode` enum.

### Test 7: cascade-delete removes the agent AND its YAML file

```bash
fantastic kernel_state delete_agent id=$DM
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
