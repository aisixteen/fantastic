# text_file selftest

> scopes: kernel, persistence
> requires: `uv sync`
> out-of-scope: HTTP, AI

Stateless text-document agent over a file surface: `read` / `write` / `patch` /
`append`, with ON-DEMAND `version`+`if_match` and `checkpoint`/`checkpoints`/
`restore`/`drop`. Owns no disk — every byte is a gated send to the `file_bridge`
named by `on:` or its `target_id` meta. One-shot CLI:
`fantastic <id> <verb> [k=v ...]`.

## Pre-flight

All state under `/tmp/tf_test/`. Nothing written to the project tree.

```bash
rm -rf /tmp/tf_test && mkdir -p /tmp/tf_test && cd /tmp/tf_test
FB=$(fantastic kernel create_agent parent_id=kernel_state handler_module=file_bridge.tools root=work ingress_rule=allow_all_ingress | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
# wire BOTH ends: the editor's egress AND the leg's ingress
TF=$(fantastic kernel create_agent parent_id=kernel_state handler_module=text_file.tools target_id=$FB can_call="{\"$FB\": [\"*\"]}" | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
fantastic kernel update_agent id=$FB callable_by="{\"$TF\": [\"*\"]}"
```

## Tests

### Test 0: unwired editor fails loud

```bash
UW=$(fantastic kernel create_agent parent_id=kernel_state handler_module=text_file.tools | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
fantastic $UW read path=x.txt | python -m json.tool | grep -F 'target_id'
```
Expected: an error naming `target_id` — no surface, no silent anything.

### Test 1: write + read round-trip (atomic, versioned)

```bash
fantastic $TF write path=notes.md bytes='# notes
- first fact
'
fantastic $TF read path=notes.md | python -m json.tool | grep -F '"total": 2'
```
Expected: two numbered lines back, plus a `version` hash on both replies.

### Test 2: patch — exactly-once match, all-or-nothing

```bash
printf '<<<<<<< SEARCH\n- first fact\n=======\n- first fact (checked)\n>>>>>>> REPLACE' > /tmp/tf_test/p.patch
# the patch rides as raw bytes; over the CLI use the file's contents verbatim
fantastic $TF patch path=notes.md bytes="$(cat /tmp/tf_test/p.patch)" | python -m json.tool | grep -F '"patched": true'
fantastic $TF patch path=notes.md bytes="$(cat /tmp/tf_test/p.patch)" | python -m json.tool | grep -F 'no_match'
```
Expected: first applies; the second (same patch again) fails loud `no_match`,
file untouched.

### Test 3: append builds a log without read-modify-write

```bash
fantastic $TF append path=log.md bytes='- entry one
'
fantastic $TF append path=log.md bytes='- entry two
'
fantastic $TF read path=log.md | python -m json.tool | grep -F '"total": 2'
```

### Test 4: checkpoint → mutate → restore

```bash
CK=$(fantastic $TF checkpoint path=notes.md | python -c "import json,sys;print(json.load(sys.stdin)['ckpt_id'])")
fantastic $TF write path=notes.md bytes='clobbered
'
fantastic $TF restore path=notes.md ckpt_id=$CK | python -m json.tool | grep -F '"restored": true'
fantastic $TF read path=notes.md | python -m json.tool | grep -F 'first fact'
```
Expected: the pre-image comes back byte-identical; the ckpt id is the dispatch's
task pid (one id space with `ps`).

### Test 5: if_match guards a concurrent edit

```bash
V=$(fantastic $TF read path=notes.md | python -c "import json,sys;print(json.load(sys.stdin)['version'])")
fantastic $TF write path=notes.md bytes='new
' if_match=$V | python -m json.tool | grep -F '"version"'
fantastic $TF write path=notes.md bytes='stale
' if_match=$V | python -m json.tool | grep -F 'version_mismatch'
```

### Test 6: the gate is real — a sealed surface refuses

```bash
SFB=$(fantastic kernel create_agent parent_id=kernel_state handler_module=file_bridge.tools root=sealed | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
fantastic $TF read path=x.txt on=$SFB | python -m json.tool | grep -F 'unauthorized'
```
Expected: the LEG refuses (sealed by default / no grant) — text_file cannot
bypass it, because it has no disk path of its own.

## Cleanup

```bash
cd / && rm -rf /tmp/tf_test
```

## Summary

| # | Test | Pass |
|---|------|------|
| 0 | unwired fails loud | |
| 1 | write + read, versioned | |
| 2 | patch exactly-once, all-or-nothing | |
| 3 | append log | |
| 4 | checkpoint → restore | |
| 5 | if_match guard | |
| 6 | sealed surface refuses | |
