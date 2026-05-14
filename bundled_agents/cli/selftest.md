# cli selftest

> scopes: cli
> requires: `uv sync`
> out-of-scope: any non-renderer verb

Renderer agent. Verifies token/done/say/error/status print correctly.

Cli is **ephemeral** — never persists to disk, composed per-process.
The pipe-stdin pattern (`echo "..." | fantastic`) is non-tty so Core
doesn't auto-compose Cli; tests drive it via direct Python composition
instead.

## Pre-flight

```bash
cd new_codebase
rm -rf .fantastic
```

## Tests

### Test 1: say with source prefix

```bash
uv run python -c "
import asyncio
from kernel import Kernel
from core import Core
from cli import Cli
async def main():
    k = Core(Kernel(), argv=[])
    Cli(k.ctx, parent=k)
    await k.send('cli', {'type':'say','text':'hello','source':'agent_x'})
asyncio.run(main())
" 2>&1 | grep -F "[agent_x] hello"
```
Expected: line containing `[agent_x] hello`.

### Test 2: token streaming (no newline)

```bash
uv run python -c "
import asyncio
from kernel import Kernel
from core import Core
from cli import Cli
async def main():
    k = Core(Kernel(), argv=[])
    Cli(k.ctx, parent=k)
    await k.send('cli', {'type':'token','text':'ABCDEF'})
asyncio.run(main())
" 2>&1 | grep -F "ABCDEF"
```
Expected: ABCDEF appears in output (no trailing newline from token alone).

### Test 3: done emits newline

```bash
uv run python -c "
import asyncio
from kernel import Kernel
from core import Core
from cli import Cli
async def main():
    k = Core(Kernel(), argv=[])
    Cli(k.ctx, parent=k)
    await k.send('cli', {'type':'token','text':'part1'})
    await k.send('cli', {'type':'done'})
asyncio.run(main())
" 2>&1 | grep -F "part1"
```
Expected: `part1` followed by a newline in output.

### Test 4: error prefixed with ERROR

```bash
uv run python -c "
import asyncio
from kernel import Kernel
from core import Core
from cli import Cli
async def main():
    k = Core(Kernel(), argv=[])
    Cli(k.ctx, parent=k)
    await k.send('cli', {'type':'error','text':'boom'})
asyncio.run(main())
" 2>&1 | grep -F "ERROR: boom"
```
Expected: line with `ERROR: boom`.

### Test 5: status phase markers

```bash
uv run python -c "
import asyncio
from kernel import Kernel
from core import Core
from cli import Cli
async def main():
    k = Core(Kernel(), argv=[])
    Cli(k.ctx, parent=k)
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
uv run python -c "
import asyncio, json
from kernel import Kernel
from core import Core
from cli import Cli
async def main():
    k = Core(Kernel(), argv=[])
    Cli(k.ctx, parent=k)
    r = await k.send('cli', {'type':'reflect'})
    print(json.dumps(r, indent=2))
asyncio.run(main())
" 2>&1 | grep -F '"status"'
```
Expected: matches inside reflect's `verbs`/`accepts` listing.

## Summary

| # | Test | Pass |
|---|------|------|
| 1 | say with source prefix | |
| 2 | token (no newline) | |
| 3 | done emits newline | |
| 4 | error prefix | |
| 5 | status phase markers (queued/thinking/tool_calling entry+exit) | |
| 6 | reflect lists status as accepted | |
