# canvas_backend selftest

> scopes: kernel
> requires: `uv sync`
> out-of-scope: browser canvas UI (see canvas_webapp selftest)

Spatial discovery + **explicit membership**. Each canvas hosts only
the agents in its `members` list — no auto-include. Kernel-side only.

## Pre-flight

```bash
rm -rf .fantastic
```

## Tests

### Test 1: reflect

```bash
CB=$(uv run python kernel.py call core create_agent handler_module=canvas_backend.tools | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
uv run python kernel.py call $CB reflect | python -m json.tool | grep -F '"discover"'
uv run python kernel.py call $CB reflect | python -m json.tool | grep -F '"add_agent"'
uv run python kernel.py call $CB reflect | python -m json.tool | grep -F '"members_updated"'
```
Expected: every grep matches — `discover`, `add_agent`, `members_updated` all present.

### Test 2: discover requires positive w, h

```bash
uv run python kernel.py call $CB discover x=0 y=0 w=0 h=0
```
Expected: `{"error":"…w and h required and > 0"}`.

### Test 3: discover finds intersecting agents

```bash
A=$(uv run python kernel.py call core create_agent handler_module=file.tools x=100 y=100 width=50 height=50 | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
uv run python kernel.py call $CB discover x=0 y=0 w=200 h=200 | python -m json.tool | grep -F "\"id\": \"$A\""
```
Expected: matches (the file agent at (100,100) is in the search rect).

### Test 4: discover excludes self

```bash
uv run python kernel.py call $CB discover x=0 y=0 w=10000 h=10000 | python -m json.tool | grep -c "\"id\": \"$CB\""
```
Expected: 0.

### Test 5: members empty by default + add/remove + reflect.member_count

```bash
# Empty by default.
uv run python kernel.py call $CB list_members | python -m json.tool | grep -F '"members": []'

# Create a get_webapp-answering agent (html_agent qualifies); add it.
HA=$(uv run python kernel.py call core create_agent handler_module=html_agent.tools | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
uv run python kernel.py call $CB add_agent agent_id=$HA | python -c "
import json, sys
d = json.load(sys.stdin)
print('PASS' if d.get('ok') is True and d.get('members') == ['$HA'] else f'FAIL d={d}')
"

# member_count tracks the list.
uv run python kernel.py call $CB reflect | python -m json.tool | grep -F '"member_count": 1'

# Idempotent re-add.
uv run python kernel.py call $CB add_agent agent_id=$HA | python -c "
import json, sys
d = json.load(sys.stdin)
print('PASS-already' if d.get('already') is True else f'FAIL d={d}')
"

# Remove.
uv run python kernel.py call $CB remove_agent agent_id=$HA | python -m json.tool | grep -F '"removed": true'
uv run python kernel.py call $CB list_members | python -m json.tool | grep -F '"members": []'
```
Expected: every grep matches; `PASS` / `PASS-already` print.
Regression signal: empty default missing → `members` field leaked
from somewhere; `already:true` not set → idempotency broken.

### Test 6: add_agent refuses non-webapp targets

```bash
# file agent doesn't answer get_webapp.
FA=$(uv run python kernel.py call core create_agent handler_module=file.tools | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
uv run python kernel.py call $CB add_agent agent_id=$FA | python -c "
import json, sys
d = json.load(sys.stdin)
print('PASS' if 'does not answer get_webapp' in d.get('error','') else f'FAIL d={d}')
"
# Members list still empty.
uv run python kernel.py call $CB list_members | python -m json.tool | grep -F '"members": []'
```
Expected: `PASS` and members empty.
Regression signal: file agent ends up in members → the get_webapp
sanity check regressed.

## Cleanup

```bash
rm -rf .fantastic
```

## Summary

| # | Test | Pass |
|---|------|------|
| 1 | reflect lists verbs | |
| 2 | discover requires w,h > 0 | |
| 3 | discover finds intersecting | |
| 4 | discover excludes self | |
| 5 | empty default + add/remove + member_count + idempotent | |
| 6 | add_agent refuses non-webapp targets | |
