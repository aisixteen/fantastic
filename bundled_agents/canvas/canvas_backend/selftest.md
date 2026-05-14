# canvas_backend selftest

> scopes: kernel, cascade
> requires: `uv sync`
> out-of-scope: browser canvas UI (see canvas_webapp selftest)

Spatial host with **structural membership** — members are this
canvas's children. `add_agent handler_module=…` spawns a new child via
the substrate's `agent.create`; cascade-delete removes the canvas and
every member with it (no orphans). Tested in-process against the root
agent (no HTTP).

## Pre-flight

All test state lives in `/tmp/cb_test/`.

```bash
rm -rf /tmp/cb_test
mkdir -p /tmp/cb_test
cd /tmp/cb_test
```

## Tests

### Test 1: reflect surfaces verbs + emits

```bash
CB=$(fantastic call core create_agent handler_module=canvas_backend.tools | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
fantastic call $CB reflect | python -m json.tool | grep -F '"discover"'
fantastic call $CB reflect | python -m json.tool | grep -F '"add_agent"'
fantastic call $CB reflect | python -m json.tool | grep -F '"members_updated"'
```
Expected: every grep matches.

### Test 2: discover requires positive w, h

```bash
fantastic call $CB discover x=0 y=0 w=0 h=0
```
Expected: `{"error":"…w and h required and > 0"}`.

### Test 3: members empty by default

```bash
fantastic call $CB list_members | python -m json.tool | grep -F '"members": []'
fantastic call $CB reflect | python -m json.tool | grep -F '"member_count": 0'
```
Expected: both grep lines match.

### Test 4: add_agent spawns a renderable child + reflect.member_count tracks

```bash
fantastic call $CB add_agent handler_module=html_agent.tools html_content="<h1>hi</h1>" | python -c "
import json, sys
d = json.load(sys.stdin)
ok = d.get('ok') is True and isinstance(d.get('member_id'), str) and len(d.get('members', [])) == 1
print('add_agent: PASS' if ok else f'FAIL d={d}')
"
fantastic call $CB reflect | python -m json.tool | grep -F '"member_count": 1'
```
Expected: `add_agent: PASS` and `member_count: 1`.

### Test 5: add_agent refuses non-renderable handler

`file.tools` answers neither `get_webapp` nor `get_gl_view`. The
canvas spawns it, probes, finds nothing renderable, and rolls back
via cascade-delete.

```bash
fantastic call $CB add_agent handler_module=file.tools | python -c "
import json, sys
d = json.load(sys.stdin)
ok = 'answers neither get_webapp nor get_gl_view' in d.get('error','')
print('refuse-non-renderable: PASS' if ok else f'FAIL d={d}')
"
# member_count unchanged after the rollback
fantastic call $CB reflect | python -m json.tool | grep -F '"member_count": 1'
```
Expected: `refuse-non-renderable: PASS` and `member_count: 1`
(unchanged from Test 4).

### Test 6: discover finds intersecting members (children-only scope)

```bash
fantastic call $CB add_agent handler_module=html_agent.tools html_content="<p>m</p>" x=100 y=100 width=50 height=50 | python -c "
import json,sys; print(json.load(sys.stdin)['member_id'])
" > /tmp/cb_test/m.id
M=$(cat /tmp/cb_test/m.id)
fantastic call $CB discover x=0 y=0 w=200 h=200 | python -m json.tool | grep -F "\"id\": \"$M\""
```
Expected: the new member appears in the discover result. Note: only
direct children intersect (cross-canvas spatial queries walk the tree
explicitly).

### Test 7: remove_agent cascades; idempotent on missing id

```bash
fantastic call $CB remove_agent agent_id=$M | python -m json.tool | grep -F '"removed": true'
# Removing again — no-op, returns removed:false.
fantastic call $CB remove_agent agent_id=$M | python -m json.tool | grep -F '"removed": false'
```
Expected: first removes, second is a no-op.

### Test 8: cascade — deleting the canvas removes all members + self

```bash
# Spawn a fresh canvas with a member, then cascade-delete it.
CB2=$(fantastic call core create_agent handler_module=canvas_backend.tools | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
fantastic call $CB2 add_agent handler_module=html_agent.tools html_content="<i>x</i>" >/dev/null
test -d ".fantastic/agents/$CB2/agents" && echo "  child dir present: OK"
fantastic call core delete_agent id=$CB2 | python -m json.tool | grep -F '"deleted": true'
test ! -d ".fantastic/agents/$CB2" && echo "  canvas + subtree removed: OK"
```
Expected: `child dir present: OK`, `deleted: true`,
`canvas + subtree removed: OK`. The cascade ran the member's
`on_delete` (best-effort) before removing both records.

## Cleanup

```bash
cd /
rm -rf /tmp/cb_test
```

## Summary

| # | Test | Pass |
|---|------|------|
| 1 | reflect lists verbs + emits | |
| 2 | discover requires w,h > 0 | |
| 3 | empty members by default | |
| 4 | add_agent spawns renderable child | |
| 5 | add_agent refuses non-renderable + rollback | |
| 6 | discover finds intersecting members | |
| 7 | remove_agent cascades + idempotent | |
| 8 | cascade-delete removes canvas + members | |
