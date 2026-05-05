# cli selftest

> scopes: cli
> requires: `uv sync`
> out-of-scope: any non-renderer verb

Renderer agent. Verifies token/done/say/error/status print correctly.

## Pre-flight

```bash
cd new_codebase
rm -rf .fantastic
```

## Tests

### Test 1: say with source prefix

```bash
echo "@cli say text=hello source=agent_x" | uv run python kernel.py 2>&1 | grep -F "[agent_x] hello"
```
Expected: line containing `[agent_x] hello`.

### Test 2: token streaming (no newline)

```bash
echo "@cli token text=ABCDEF" | uv run python kernel.py 2>&1 | grep -F "ABCDEF"
```
Expected: ABCDEF appears in output (no trailing newline from token alone).

### Test 3: done emits newline

```bash
{ echo "@cli token text=part1"; echo "@cli done"; echo "exit"; } | uv run python kernel.py
```
Expected: `part1` followed by a newline in output.

### Test 4: error prefixed with ERROR

```bash
echo "@cli error text=boom" | uv run python kernel.py 2>&1 | grep -F "ERROR: boom"
```
Expected: line with `ERROR: boom`.

### Test 5: status phase markers

```bash
uv run python -c "
import asyncio
from kernel import Kernel
async def main():
    k = Kernel()
    k.ensure('cli', 'cli.tools', singleton=True, display_name='cli')
    await k.send('cli', {'type':'status','source':'ollama_x','phase':'queued','detail':{'ahead':2,'send_id':'a'}})
    await k.send('cli', {'type':'status','source':'ollama_x','phase':'thinking','detail':{}})
    await k.send('cli', {'type':'status','source':'nv_x','phase':'thinking','detail':{'waiting_on':'rate_limit','wait_s':5}})
    await k.send('cli', {'type':'status','source':'ollama_x','phase':'tool_calling','detail':{'tool':{'call_id':'c1','target':'core','verb':'list_agents','args':{}}}})
    await k.send('cli', {'type':'status','source':'ollama_x','phase':'tool_calling','detail':{'tool':{'call_id':'c1','target':'core','verb':'list_agents','args':{},'reply_preview':'{ok:1}'}}})
    await k.send('cli', {'type':'status','source':'ollama_x','phase':'streaming','detail':{}})
    await k.send('cli', {'type':'status','source':'ollama_x','phase':'done','detail':{'reason':'ok'}})
asyncio.run(main())
"
```
Expected stdout:
```
  [ollama_x] queued (2 ahead)
  [ollama_x] thinking…
  [nv_x] rate-limited; waiting 5s
  [ollama_x] → list_agents(core)  {}
  [ollama_x] ← list_agents(core)  {ok:1}
```
Five lines exactly. `streaming` and `done` produce no output (token
and done verbs cover them).

### Test 6: reflect lists status as accepted event

```bash
echo "@cli reflect" | uv run python kernel.py 2>&1 | grep -F '"status"'
```
Expected: matches inside the `accepts` list.

## Summary

| # | Test | Pass |
|---|------|------|
| 1 | say with source prefix | |
| 2 | token (no newline) | |
| 3 | done emits newline | |
| 4 | error prefix | |
| 5 | status phase markers (queued/thinking/tool_calling entry+exit) | |
| 6 | reflect lists status as accepted | |
