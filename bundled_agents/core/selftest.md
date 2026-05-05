# core selftest

> scopes: kernel, cli
> requires: `uv sync`
> out-of-scope: web/HTTP, AI providers, PTY

System-verbs agent. Drives via subprocess (`python kernel.py call …`)
and the REPL via stdin.

## Pre-flight

```bash
cd new_codebase
rm -rf .fantastic
```

## Tests

### Test 1: kernel reflect

```bash
uv run python kernel.py reflect
```
Expected: JSON containing `primitive`, `envelope`, `well_known`,
`browser_bus`, `binary_protocol`. `agent_count` = 2 (core, cli).
Regression signal: missing `browser_bus` or `binary_protocol` → reflect primer regressed.

### Test 2: list_agents

```bash
uv run python kernel.py call core list_agents
```
Expected: `{"agents":[{"id":"cli", …}, {"id":"core", …}]}` (in some order).

### Test 3: create_agent + auto-boot

```bash
uv run python kernel.py call core create_agent handler_module=file.tools
```
Expected: `{"id":"file_<hex6>", "handler_module":"file.tools"}`.
File check: `.fantastic/agents/file_<hex6>/agent.json` exists.

### Test 4: update_agent persists

```bash
ID=$(uv run python kernel.py call core list_agents | python -c "import json,sys;print([a for a in json.load(sys.stdin)['agents'] if a['handler_module']=='file.tools'][0]['id'])")
uv run python kernel.py call core update_agent id=$ID model=foo x=42
```
Expected: `{"updated":true,"id":"…","agent":{… "model":"foo", "x":42}}`.
File check: `agent.json` of that id contains `"model":"foo"` and `"x":42`.

### Test 5: delete_agent emits + persists

```bash
uv run python kernel.py call core delete_agent id=$ID
```
Expected: `{"deleted":true,"id":"<ID>"}`.
File check: `.fantastic/agents/<ID>/` directory removed.

### Test 6: REPL @-tag routing (cli mode)

```bash
echo "@core list_agents" | uv run python kernel.py
```
Expected: prints the agents list under `fantastic>` prompt and exits.

### Test 7: delete_lock refuses; clear via update_agent → succeeds

```bash
rm -rf .fantastic
ID=$(uv run python kernel.py call core create_agent handler_module=file.tools delete_lock=true | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
# Locked → refused, response carries explicit `locked:true` for LLM callers.
uv run python kernel.py call core delete_agent id=$ID | python -c "
import json, sys
d = json.load(sys.stdin)
ok = d.get('locked') is True and 'delete_lock' in d.get('error','')
print('locked-refusal: PASS' if ok else f'FAIL d={d}')
"
test -d .fantastic/agents/$ID && echo "  record still on disk: OK"
# Unlock → delete succeeds.
uv run python kernel.py call core update_agent id=$ID delete_lock=false >/dev/null
uv run python kernel.py call core delete_agent id=$ID | python -c "
import json, sys
d = json.load(sys.stdin)
print('post-unlock-delete: PASS' if d.get('deleted') is True else f'FAIL d={d}')
"
test ! -d .fantastic/agents/$ID && echo "  record removed: OK"
rm -rf .fantastic
```
Expected: `locked-refusal: PASS`, record still on disk, then
`post-unlock-delete: PASS`, record removed.
Regression signal: `locked` flag missing from response → LLM callers
can't programmatically detect the refusal reason.

### Test 8: shutdown lifecycle hook fires before delete

`core.delete_agent` sends `{type:"shutdown"}` to the agent before
calling `kernel.delete`, symmetric to the `boot` it sends on create.
This is the universal teardown hook bundles use to release process-
memory state (PTY children, uvicorn servers, in-flight tasks). Opt-
in: bundles that don't implement `shutdown` return unknown-verb
which core silently ignores.

```bash
rm -rf .fantastic
ID=$(uv run python kernel.py call core create_agent handler_module=file.tools | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
# file.tools doesn't implement shutdown — delete must still succeed.
uv run python kernel.py call core delete_agent id=$ID | python -c "
import json, sys
d = json.load(sys.stdin)
print('unknown-verb-ignored: PASS' if d.get('deleted') is True else f'FAIL d={d}')"
test ! -d .fantastic/agents/$ID && echo "  record removed: OK"
rm -rf .fantastic
```
Expected: `unknown-verb-ignored: PASS` and `record removed: OK`.
Regression signal: delete fails when bundle has no `shutdown` →
core stopped silently ignoring unknown-verb / didn't make `shutdown`
optional.

### Test 9: unknown verb / unknown agent rejected cleanly (substrate safety net)

Every bundle's handler rejects unknown `type` values with a
deterministic error shape. This is the substrate's defense against
malformed tool_call output from LLMs (chat-template tokens leaking
into `function.name`, model hallucinating verbs, etc.).

```bash
rm -rf .fantastic
# Send a chat-template-fragment-shaped bogus verb.
uv run python kernel.py call core '<|"|list_agents<|"|' 2>&1 | python -c "
import sys
ok = 'unknown type' in sys.stdin.read()
print('unknown-verb-rejected: PASS' if ok else 'FAIL')"
# Send to a non-existent agent id.
uv run python kernel.py call file_does_not_exist garbage_verb 2>&1 | python -c "
import sys
print('unknown-agent-rejected: PASS' if 'no agent' in sys.stdin.read() else 'FAIL')"
rm -rf .fantastic
```
Expected: both PASS lines. The kernel returns
`{"error":"<bundle>: unknown type '...'"}` and
`{"error":"no agent '...'"}`. LLM agentic loops feed the error back
as a role:tool reply; well-trained models correct on the next turn.
Models with weak tool-call discipline (some Gemma variants) may
chase the same bad verb across many iterations — visible as `+N
more` overflow on the agent's telemetry sprite. Not a substrate
issue: kernel rejection is correct; switch to a stronger tool-
trained model (Llama-3.1-Nemotron-Ultra, Qwen3-Coder).

## Summary

| # | Test | Pass |
|---|------|------|
| 1 | kernel reflect contains primer fields | |
| 2 | list_agents | |
| 3 | create_agent + auto-boot | |
| 4 | update_agent persists | |
| 5 | delete_agent + dir removed | |
| 6 | REPL @-tag routes to handler | |
| 7 | delete_lock refuses; unlocks via update_agent | |
| 8 | shutdown lifecycle hook fires (unknown-verb tolerated) | |
| 9 | unknown verb / unknown agent rejected cleanly | |
