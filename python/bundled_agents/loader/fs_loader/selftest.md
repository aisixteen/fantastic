# fs_loader selftest

> scopes: kernel, cli, cascade
> requires: `uv sync`
> out-of-scope: web/HTTP, AI providers, PTY

System verbs are baked into the `Agent` class itself; every agent
answers them natively for its own children. `fs_loader` IS the root
loader: the tree root is an `fs_loader` agent (`id="fs_loader"`,
`handler_module="fs_loader.tools"`) that owns `.fantastic/` and answers
`load_tree` / `persist_record` / `forget_record`, while the system verbs
stay native to `Agent`. Drives via subprocess (`fantastic …`) and the
REPL via stdin.

## Pre-flight

All test state lives in `/tmp/core_test/`. Project tree is
not touched.

```bash
rm -rf /tmp/core_test
mkdir -p /tmp/core_test
cd /tmp/core_test
```

## Tests

### Test 1: kernel reflect (uniform identity + composable flags)

`reflect` is ONE uniform verb. A reply on ANY agent (root included) is
that agent's identity — `{id, sentence, display_name, parent_id,
handler_module, description?, verbs?, ...flat state}` — plus whatever
the `tree` / `bundles` / `readme` flags compose in. Root is NOT
special: there is no `primer`. The transport/wire prose that used to
ride in reflect now lives in the root readme (`reflect readme=true`).

```bash
# Default reply: identity + the nested distilled subtree under `tree`.
fantastic reflect | python -c "
import json, sys
d = json.load(sys.stdin)
assert d['id'] == 'fs_loader', f'id={d.get(\"id\")}'
assert 'sentence' in d and 'tree' in d, f'keys={list(d)}'
# Old primer keys are GONE.
gone = [k for k in ('transports','primitive','envelope','browser_bus',
                     'binary_protocol','agent_count','available_bundles',
                     'well_known','universal_verb') if k in d]
assert not gone, f'stale primer keys leaked: {gone}'
# Default tree=all → nested subtree rooted at the agent.
assert isinstance(d['tree'], dict) and d['tree']['id'] == 'fs_loader'
print('uniform-shape: PASS')
"
# tree=ids → flat descendant-id list (cheap scan).
fantastic reflect tree=ids | python -c "
import json, sys
d = json.load(sys.stdin)
assert isinstance(d['tree'], list) and 'fs_loader' in d['tree'], f'tree={d[\"tree\"]}'
print('tree=ids: PASS')
"
# bundles=all → the installable-bundle catalog ({name, handler_module}).
fantastic reflect bundles=all | python -c "
import json, sys
d = json.load(sys.stdin)
bs = d.get('bundles')
assert isinstance(bs, list) and len(bs) >= 15, f'bundles={bs!r}'
assert {'name','handler_module'} <= set(bs[0]), f'bundle shape={bs[0]}'
print('bundles=all: PASS')
"
# readme=true → the addressed agent's readme.md attached (root → primer).
fantastic reflect readme=true | python -c "
import json, sys
d = json.load(sys.stdin)
assert isinstance(d.get('readme'), str), f'readme type={type(d.get(\"readme\"))}'
assert d['readme'].startswith('# This is a Fantastic kernel.'), d['readme'][:60]
print('readme=true: PASS')
"
```
Expected: `uniform-shape: PASS`, `tree=ids: PASS`, `bundles=all: PASS`,
`readme=true: PASS`. Regression signal: any old primer key
(`transports`, `available_bundles`, `agent_count`, `browser_bus`,
`binary_protocol`) reappearing in the reflect JSON → the primer/reflect
collapse regressed; or `tree`/`bundles`/`readme` not composing → a flag
stopped being honored.

### Test 1b: `description` round-trips through reflect

`create_agent … description="…"` stamps a one-line "what this agent is
for" that surfaces top-level in every reflect (and in each tree node).

```bash
rm -rf .fantastic
ID=$(fantastic fs_loader create_agent handler_module=file.tools description="x" | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
fantastic $ID reflect | python -c "
import json, sys
d = json.load(sys.stdin)
print('description-roundtrip: PASS' if d.get('description') == 'x' else f'FAIL d={d}')"
rm -rf .fantastic
```
Expected: `description-roundtrip: PASS`. Regression signal: missing
`description` key → create_agent dropped the field, or reflect stopped
surfacing it.

### Test 2: list_agents (flat all)

```bash
fantastic fs_loader list_agents
```
Expected: `{"agents":[…]}` containing `fs_loader` (root). If the
invocation is interactive (stdin is a tty), `cli` is also present
as an ephemeral child — composed by the bootstrap, never persisted to
disk. Non-tty invocations (pipes, scripted) see only `fs_loader`.

### Test 3: create_agent + auto-boot

```bash
fantastic fs_loader create_agent handler_module=file.tools
```
Expected: `{"id":"file_<hex6>", "handler_module":"file.tools",
"parent_id":"fs_loader"}`. File check:
`.fantastic/agents/file_<hex6>/agent.json` exists.

### Test 4: update_agent persists + emits agent_updated

```bash
ID=$(fantastic fs_loader list_agents | python -c "import json,sys;print([a for a in json.load(sys.stdin)['agents'] if a['handler_module']=='file.tools'][0]['id'])")
fantastic fs_loader update_agent id=$ID model=foo x=42
```
Expected: `{"updated":true,"id":"…","agent":{… "model":"foo", "x":42}}`.
File check: `agent.json` of that id contains `"model":"foo"` and
`"x":42`. Watchers of `fs_loader` see an `agent_updated` emit (verified
in pytest; fragile to drive via CLI).

### Test 5: delete_agent + cascade through subtree

```bash
fantastic fs_loader delete_agent id=$ID
```
Expected: `{"deleted":true,"id":"<ID>"}`. File check:
`.fantastic/agents/<ID>/` directory removed.

### Test 6: REPL @-tag routing (cli mode)

```bash
echo "@fs_loader list_agents" | fantastic
```
Expected: prints the agents list under `fantastic>` prompt and
exits.

### Test 7: delete_lock refuses; clear via update_agent → succeeds

```bash
rm -rf .fantastic
ID=$(fantastic fs_loader create_agent handler_module=file.tools delete_lock=true | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
# Locked → refused, response carries explicit `locked:true` and the
# `blocked_by` id (the deepest descendant with delete_lock — for a
# leaf, that's the agent itself).
fantastic fs_loader delete_agent id=$ID | python -c "
import json, sys
d = json.load(sys.stdin)
ok = d.get('locked') is True and d.get('blocked_by') == sys.argv[1] and 'delete_lock' in d.get('error','')
print('locked-refusal: PASS' if ok else f'FAIL d={d}')
" $ID
test -d .fantastic/agents/$ID && echo "  record still on disk: OK"
# Unlock → delete succeeds.
fantastic fs_loader update_agent id=$ID delete_lock=false >/dev/null
fantastic fs_loader delete_agent id=$ID | python -c "
import json, sys
d = json.load(sys.stdin)
print('post-unlock-delete: PASS' if d.get('deleted') is True else f'FAIL d={d}')
"
test ! -d .fantastic/agents/$ID && echo "  record removed: OK"
rm -rf .fantastic
```
Expected: `locked-refusal: PASS`, record still on disk, then
`post-unlock-delete: PASS`, record removed.
Regression signal: `locked` flag or `blocked_by` missing from
response → LLM callers can't programmatically detect the refusal.

### Test 8: on_delete cascade hook fires before record removal

`Agent.delete` cascades depth-first; for each descendant the substrate
calls `await agent.on_delete()` BEFORE detaching the record. `on_delete`
tears down PROCESS state only — DISK cleanup is NOT here; the loader
(fs_loader) rmtrees the dir on the `removed` state event. Bundles that
need to tear down process-memory state expose
`async def on_delete(agent)` in their tools.py — substrate looks it
up and invokes it (terminal_backend kills its PTY, web drains uvicorn,
kernel_bridge cancels its read loop).

```bash
rm -rf .fantastic
ID=$(fantastic fs_loader create_agent handler_module=file.tools | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
# file.tools has no on_delete — the loader rmtrees the dir on `removed`.
fantastic fs_loader delete_agent id=$ID | python -c "
import json, sys
d = json.load(sys.stdin)
print('default-on-delete: PASS' if d.get('deleted') is True else f'FAIL d={d}')"
test ! -d .fantastic/agents/$ID && echo "  record removed: OK"
rm -rf .fantastic
```
Expected: `default-on-delete: PASS` and `record removed: OK`.

### Test 9: cascade with nested children dies depth-first

```bash
rm -rf .fantastic
# Spawn a file agent, then create a terminal_backend child under it.
PARENT=$(fantastic fs_loader create_agent handler_module=file.tools | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
CHILD=$(fantastic $PARENT create_agent handler_module=terminal_backend.tools | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
# Verify the child exists on disk under parent's agents/ dir.
test -d .fantastic/agents/$PARENT/agents && echo "  child dir present: OK"
# Cascade-delete the parent.
fantastic fs_loader delete_agent id=$PARENT | python -c "
import json, sys
d = json.load(sys.stdin)
print('cascade-delete: PASS' if d.get('deleted') is True else f'FAIL d={d}')"
# Both records gone.
test ! -d .fantastic/agents/$PARENT && echo "  parent dir removed: OK"
rm -rf .fantastic
```
Expected: `child dir present: OK`, `cascade-delete: PASS`, `parent
dir removed: OK`. The cascade ran terminal_backend's `on_delete`
(real run would kill the PTY) before removing the file agent.

### Test 10: unknown verb / unknown agent rejected cleanly (substrate safety net)

Every bundle's handler rejects unknown `type` values with a
deterministic error shape. This is the substrate's defense against
malformed tool_call output from LLMs (chat-template tokens leaking
into `function.name`, model hallucinating verbs, etc.).

```bash
rm -rf .fantastic
# Send a chat-template-fragment-shaped bogus verb. Quoting matters.
fantastic fs_loader '<|"|list_agents<|"|' 2>&1 | python -c "
import sys
ok = 'unhandled system verb' in sys.stdin.read() or 'unknown' in sys.stdin.read()
print('unknown-verb-rejected: PASS' if ok else 'FAIL')"
# Send to a non-existent agent id.
fantastic file_does_not_exist garbage_verb 2>&1 | python -c "
import sys
print('unknown-agent-rejected: PASS' if 'no agent' in sys.stdin.read() else 'FAIL')"
rm -rf .fantastic
```
Expected: both PASS lines.

### Test 11: root readme seeded — the bootstrap primer is on disk

`fs_loader` ships a `readme.md`; the bootstrap copies it to
`.fantastic/readme.md` on the first `fantastic` invocation in a dir.
This is the file a code agent reads first to learn the system.

```bash
rm -rf .fantastic
fantastic reflect >/dev/null    # any invocation bootstraps the kernel
test -f .fantastic/readme.md && echo "  root readme on disk: OK"
grep -qF "Fantastic kernel" .fantastic/readme.md && echo "  is the primer: OK"
# The readme documents the readme flag (canonical `readme=true`; the
# legacy `return_readme` spelling still works and may also appear).
grep -qEF -e "readme=true" -e "return_readme" .fantastic/readme.md && echo "  documents the flag: OK"
rm -rf .fantastic
```
Expected: all three `OK` lines. Regression signal: missing file →
the bootstrap's root-readme seed not wired, or `fs_loader` ships no
`readme.md`.

### Test 12: create_agent seeds the bundle's readme into the agent dir

```bash
rm -rf .fantastic
ID=$(fantastic fs_loader create_agent handler_module=file.tools | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
test -f .fantastic/agents/$ID/readme.md && echo "  agent readme seeded: OK"
grep -qiF "file" .fantastic/agents/$ID/readme.md && echo "  is the file bundle's readme: OK"
rm -rf .fantastic
```
Expected: both `OK`. The substrate copies `<bundle>/readme.md` into
the new agent's dir on create (copy-if-missing).

### Test 13: reflect readme flag — lean by default, readme on request

The canonical flag is `readme=true`; the legacy `return_readme=true`
spelling still works as an alias.

```bash
rm -rf .fantastic
ID=$(fantastic fs_loader create_agent handler_module=file.tools | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
# Default reflect: no readme key.
fantastic $ID reflect | python -c "
import json,sys; d=json.load(sys.stdin)
print('lean-by-default: PASS' if 'readme' not in d else f'FAIL keys={list(d)}')"
# With the flag: readme content attached.
fantastic $ID reflect readme=true | python -c "
import json,sys; d=json.load(sys.stdin)
ok = isinstance(d.get('readme'), str) and 'file' in d['readme'].lower()
print('readme=true: PASS' if ok else 'FAIL')"
# Legacy spelling still honored.
fantastic $ID reflect return_readme=true | python -c "
import json,sys; d=json.load(sys.stdin)
ok = isinstance(d.get('readme'), str) and 'file' in d['readme'].lower()
print('return_readme alias: PASS' if ok else 'FAIL')"
# Same on the kernel target → the root readme (bootstrap primer).
fantastic reflect readme=true | python -c "
import json,sys; d=json.load(sys.stdin)
ok = isinstance(d.get('readme'), str) and 'Fantastic kernel' in d['readme']
print('kernel-readme: PASS' if ok else 'FAIL')"
rm -rf .fantastic
```
Expected: `lean-by-default: PASS`, `readme=true: PASS`,
`return_readme alias: PASS`, `kernel-readme: PASS`. Reflect stays lean
unless the flag is set; `reflect kernel readme=true` returns
`.fantastic/readme.md`.

### Test 14: `fantastic --help` prints the CLI cheatsheet (kernel/help.md)

`--help` / `-h` / `help` print `kernel/help.md` — a file-backed
markdown cheatsheet that points at `fantastic reflect
return_readme=true` for the live system bootstrap.

```bash
fantastic --help | python -c "
import sys; s = sys.stdin.read()
ok = ('fantastic — CLI' in s
      and 'reflect return_readme=true' in s
      and 'send(target_id, payload)' in s)
print('help-is-cheatsheet: PASS' if ok else 'FAIL')"
# -h and help are aliases — same output.
test \"$(fantastic -h)\" = \"$(fantastic --help)\" && echo 'alias -h: PASS' || echo 'FAIL'
test \"$(fantastic help)\" = \"$(fantastic --help)\" && echo 'alias help: PASS' || echo 'FAIL'
```
Expected: `help-is-cheatsheet: PASS`, `alias -h: PASS`, `alias help:
PASS`. Works regardless of daemon state — `--help` reads a static
file, never touches the lock. Regression signal: JSON or a Python
traceback instead of the markdown → `kernel/help.md` missing or
`_print_help` not wired.

## Cleanup

```bash
cd /
rm -rf /tmp/core_test
```

## Summary

| # | Test | Pass |
|---|------|------|
| 1 | kernel reflect — uniform identity + tree/bundles/readme flags | |
| 1b | description round-trips through reflect | |
| 2 | list_agents | |
| 3 | create_agent + auto-boot | |
| 4 | update_agent persists + emits | |
| 5 | delete_agent + dir removed | |
| 6 | REPL @-tag routes to handler | |
| 7 | delete_lock refuses; unlocks via update_agent | |
| 8 | on_delete cascade hook (loader rmtrees on `removed`) | |
| 9 | cascade delete depth-first through nested children | |
| 10 | unknown verb / unknown agent rejected cleanly | |
| 11 | root readme seeded (bootstrap primer on disk) | |
| 12 | create_agent seeds bundle readme into agent dir | |
| 13 | reflect readme flag (lean default, readme on request; return_readme alias) | |
| 14 | --help prints the CLI cheatsheet (kernel/help.md) | |
