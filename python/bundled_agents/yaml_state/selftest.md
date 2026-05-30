# yaml_state selftest

> scopes: kernel, persistence
> requires: `uv sync`
> out-of-scope: HTTP, AI, the FM auto-inject hook (Swift-only)

Durable YAML key-value memory agent. CRUD (`read`/`keys`/`set`/`delete`/
`replace`/`state_yaml`), disk-is-truth, mode sentence, cascade cleanup.
One-shot CLI form: `fantastic <id> <verb> [k=v ...]`.

## Pre-flight

All state under `/tmp/ys_test/`. Nothing written to the project tree.

```bash
rm -rf /tmp/ys_test && mkdir -p /tmp/ys_test && cd /tmp/ys_test
```

## Tests

### Test 1: set + read round-trip

```bash
YS=$(fantastic core create_agent handler_module=yaml_state.tools mode=mem | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
fantastic $YS set key=user.name value=Ada
fantastic $YS read key=user.name | python -m json.tool | grep -F '"value": "Ada"'
```
Expected: grep matches. Missing key → `{"value": null}`.

### Test 2: keys survey — sorted, with sizes

```bash
fantastic $YS set key=decision.db value="postgres for JSON support"
fantastic $YS keys | python -c "import json,sys;print([k['key'] for k in json.load(sys.stdin)['keys']])" | grep -F "['decision.db', 'user.name']"
```
Expected: sorted key list (the table-of-contents).

### Test 3: state_yaml is the injected block

```bash
fantastic $YS state_yaml | python -c "import json,sys;sys.stdout.write(json.load(sys.stdin)['yaml'])" | grep -F 'user.name: Ada'
```
Expected: YAML text carrying the fact (this is what auto-injects on boot).

### Test 4: disk-is-truth — the YAML file IS the authoritative copy

```bash
find /tmp/ys_test/.fantastic -name state.yaml -exec grep -l 'user.name: Ada' {} \;
```
Expected: prints the `state.yaml` path (human-editable, git-diffable).

### Test 5: read whole doc (key omitted)

```bash
fantastic $YS read | python -m json.tool | grep -F '"user.name"'
```
Expected: the whole doc under `"doc"`.

### Test 6: delete prunes a key

```bash
fantastic $YS delete key=decision.db | python -m json.tool | grep -F '"deleted": true'
fantastic $YS read key=decision.db | python -m json.tool | grep -F '"value": null'
```
Expected: `deleted: true`, then the value is null. Re-deleting → `deleted: false` (no error).

### Test 7: replace overwrites the whole store

```bash
fantastic $YS replace doc='{"only":"this"}' | python -m json.tool | grep -F '"replaced": true'
fantastic $YS read | python -m json.tool | grep -F '"only": "this"'
```
Expected: store replaced. `replace doc={}` clears it.

### Test 8: reflect — mode drives the sentence (mem vs data)

```bash
fantastic $YS reflect | python -c "import json,sys;d=json.load(sys.stdin);print(d['mode'], 'durable memory' in d['sentence'])" | grep -F 'mem True'
DS=$(fantastic core create_agent handler_module=yaml_state.tools mode=data | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
fantastic $DS reflect | python -c "import json,sys;d=json.load(sys.stdin);print(d['mode'], 'scratch-state' in d['sentence'])" | grep -F 'data True'
```
Expected: `mem` agent says "durable memory"; `data` agent says "scratch-state".

### Test 9: cascade-delete removes the agent AND its YAML file

```bash
fantastic core delete_agent id=$YS
test ! -d /tmp/ys_test/.fantastic/agents/$YS && echo OK
```
Expected: `OK` — the substrate cascade removed the agent dir (and `state.yaml`).

## Cleanup

```bash
cd / && rm -rf /tmp/ys_test
```

## Summary

| # | Test | Pass |
|---|------|------|
| 1 | set + read round-trip | |
| 2 | keys survey sorted + sizes | |
| 3 | state_yaml injected block | |
| 4 | disk-is-truth | |
| 5 | read whole doc | |
| 6 | delete prunes | |
| 7 | replace overwrites | |
| 8 | reflect mode sentence | |
| 9 | cascade-delete cleanup | |
