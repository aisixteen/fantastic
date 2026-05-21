# ollama_backend selftest

> scopes: kernel, ai, persistence, http
> requires: `uv sync`; live ollama at a reachable endpoint with a
> tool-calling model (gemma4:e2b, llama3.1+, qwen2.5+); tests run
> against a running `fantastic` so the provider HTTP client +
> in-flight tasks stay alive across calls
> out-of-scope: HTTP routes (covered by webapp selftest), browser

Reflect-driven LLM agent. Tests prompt assembly, native tool-calls,
file_agent persistence.

**Why a running serve is required:** ollama_backend caches the
`OllamaProvider` HTTP client and in-flight `_run` tasks in
process-memory. Multi-step tests would lose state between separate
`fantastic call` invocations. Drive via `fantastic` + HTTP.

## Pre-flight

ASK USER which provider + model:
- ollama endpoint (default `http://localhost:11434`)
- model name (e.g. `gemma4:e2b`, `qwen2.5:3b`, `llama3.1:8b`)

Verify reachable BEFORE wiping state:
```bash
curl -s http://localhost:11434/api/tags | head
# confirm the named model is in the list
```
If unreachable or model missing → STOP, report.

```bash
cd new_codebase
rm -rf .fantastic
PORT=18911
pkill -9 -f "fantastic" 2>/dev/null; sleep 0.3
uv run --active python fantastic core create_agent handler_module=web.tools port=$PORT >/dev/null
WEB_ID=$(ls .fantastic/agents | grep '^web_' | head -1)
uv run --active fantastic $WEB_ID create_agent handler_module=web_ws.tools >/dev/null
uv run --active python fantastic > /tmp/s.log 2>&1 &
SPID=$!
for i in $(seq 1 20); do grep -q "kernel up" /tmp/s.log 2>/dev/null && break; sleep 0.5; done

# This helper opens a one-shot WS, sends a `call` frame, prints reply.
call() {
  TARGET="$1" PAYLOAD="$2" PORT="$PORT" uv run --active python - <<'PY'
import asyncio, json, os, websockets
target = os.environ["TARGET"]; payload = json.loads(os.environ["PAYLOAD"])
port = os.environ["PORT"]
async def main():
    async with websockets.connect(f"ws://localhost:{port}/{target}/ws") as ws:
        await ws.send(json.dumps({"type":"call","target":target,"payload":payload,"id":"1"}))
        while True:
            m = json.loads(await ws.recv())
            if m.get("id") == "1" and m.get("type") in ("reply","error"):
                print(json.dumps(m.get("data"))); return
asyncio.run(main())
PY
}
```

After all tests:
```bash
kill -9 $SPID 2>/dev/null; rm -rf .fantastic /tmp/s.log
```

## Tests

### Test 1: send without file_agent_id → failfast

```bash
OB=$(call core '{"type":"create_agent","handler_module":"ollama_backend.tools"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
call $OB '{"type":"send","text":"hi"}'
```
Expected: `{"error":"ollama_backend: file_agent_id required"}`.

### Test 2: configure file_agent_id, reflect shows it

```bash
FA=$(call core '{"type":"create_agent","handler_module":"file.tools"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
call core "{\"type\":\"update_agent\",\"id\":\"$OB\",\"file_agent_id\":\"$FA\"}"
call $OB '{"type":"reflect"}' | python -m json.tool | grep -F "\"file_agent_id\": \"$FA\""
```
Expected: matches.

### Test 3: simple send streams tokens, persists chat_cli.json

The default caller is `client_id="cli"` (REPL/daemon flow). With
per-client chat storage, the file lives at `chat_cli.json` — passing
a different `client_id` would write `chat_<that>.json`.

```bash
call $OB '{"type":"send","text":"reply with the single word: ok"}' | python -m json.tool | grep -F '"final"'
test -f .fantastic/agents/$OB/chat_cli.json && python -c "
import json
d = json.load(open('.fantastic/agents/$OB/chat_cli.json'))
print('messages:', len(d), 'last:', d[-1]['content'][:50])
"
```
Expected: messages ≥ 2; last message contains "ok" (case-insensitive).

### Test 4: tool-call round-trip — and the chat sidecar keeps the round-trip on disk

```bash
call $OB '{"type":"send","text":"how many agents are online? actually check using the send tool"}' | python -c "import json,sys; print(json.load(sys.stdin).get('final',''))"
```
Expected: model emits a tool_call to `core` with `list_agents`, reads
the reply, summarizes. Final answer mentions a number.
Regression signal: model just says "I cannot check" without emitting
tool_calls → either model lacks tool support or SEND_TOOL definition broke.

The persistence is **lossless** — the sidecar must contain the full
`assistant`-with-`tool_calls` turn AND its `role:tool` reply, not just
the user/final-assistant pair. This is the audit trail for malformed
tool-calls (Gemma chat-template-token leaks like `<|"|verb<|"|`,
hallucinated verbs, args-as-string vs dict).

```bash
python -c "
import json
d = json.load(open('.fantastic/agents/$OB/chat_cli.json'))
roles = [m['role'] for m in d]
has_tcs = any(m.get('tool_calls') for m in d if m['role']=='assistant')
has_tool = 'tool' in roles
print('PASS' if has_tcs and has_tool else f'FAIL roles={roles} tcs={has_tcs}')
print('  recent tool_call name:',
      next((tc['function']['name']
            for m in reversed(d) if m.get('tool_calls')
            for tc in m['tool_calls']), '(none)'))
"
```
Expected: `PASS` and a non-`(none)` tool_call name (e.g. `send`).
Regression signal: PASS missing → `_run` reverted to lossy persistence
(saving only user + final assistant). Faulty tool calls evaporate.

### Test 5: history persists across calls

```bash
call $OB '{"type":"send","text":"my favorite color is teal, remember it"}' >/dev/null
call $OB '{"type":"send","text":"what color did I just say?"}' | python -c "import json,sys; print(json.load(sys.stdin).get('final','').lower())"
```
Expected: response mentions "teal" — proves chat_cli.json round-trip via file agent.

### Test 6: history verb returns messages

```bash
call $OB '{"type":"history"}' | python -c "import json,sys; d=json.load(sys.stdin); print('messages:', len(d['messages']))"
```
Expected: ≥ 4 (multiple user/assistant pairs after Tests 3 + 5).

### Test 7: status verb snapshot when idle

```bash
call $OB '{"type":"status","client_id":"alice"}' | python -m json.tool
```
Expected: `{generating:false, current:null, mine_pending:[], others_pending:0, source:"<OB>", client_id:"alice"}`.
Reflect should also list `status` in `verbs` and document the `status`
event type in `emits`:
```bash
call $OB '{"type":"reflect"}' | python -c "
import json,sys
d=json.load(sys.stdin)
print('status verb:', 'status' in d['verbs'])
print('status event:', 'status' in d['emits'])
"
```
Expected: both `True`.

### Test 8: status events fire at every phase transition

```bash
uv run --active python -c "
import asyncio, json, websockets
async def main():
    async with websockets.connect('ws://localhost:$PORT/$OB/ws') as ws:
        await ws.send(json.dumps({'type':'watch','src':'$OB'}))
        await ws.send(json.dumps({
            'type':'call','target':'$OB',
            'payload':{'type':'send','text':'use the send tool to call list_agents on core, then summarize in one sentence','client_id':'alice'},
            'id':'1',
        }))
        phases = []
        async for msg in ws:
            ev = json.loads(msg)
            p = ev.get('payload') or {}
            if p.get('type') == 'status' and p.get('client_id') == 'alice':
                phases.append(p['phase'])
                if p['phase'] == 'done': break
        # Expect: thinking → streaming/tool_calling → … → done.
        print('phases:', phases)
        ok = phases[0] == 'thinking' and phases[-1] == 'done' and 'tool_calling' in phases
        print('PASS' if ok else 'FAIL')
asyncio.run(main())
"
```
Expected: `PASS`. Phases must start with `thinking`, end with `done`,
and include at least one `tool_calling` (entry+exit). Each
`tool_calling` event carries `detail.tool` with `target`, `verb`,
`args`, and (on exit) `reply_preview`.

### Test 9: contended send → caller receives `queued` event + `status(queued)`

```bash
# Background a slow send (alice). While it's holding the lock, fire
# a second send (bob). Bob should receive a `queued` event tagged
# with his client_id BEFORE his first `token` arrives.
uv run --active python -c "
import asyncio, json, websockets
async def main():
    async with websockets.connect('ws://localhost:$PORT/$OB/ws') as wsB:
        await wsB.send(json.dumps({'type':'watch','src':'$OB'}))

        # Alice (client A) starts first.
        async with websockets.connect('ws://localhost:$PORT/$OB/ws') as wsA:
            await wsA.send(json.dumps({'type':'watch','src':'$OB'}))
            await wsA.send(json.dumps({
                'type':'call','target':'$OB',
                'payload':{'type':'send','text':'count slowly to 20','client_id':'alice'},
                'id':'A',
            }))

            # Tiny pause so alice acquires the lock.
            await asyncio.sleep(0.5)

            # Bob (client B) sends — should hit the lock and get queued.
            await wsB.send(json.dumps({
                'type':'call','target':'$OB',
                'payload':{'type':'send','text':'one word reply','client_id':'bob'},
                'id':'B',
            }))

            queued_seen = False
            status_queued_seen = False
            token_after_queued = False
            async for msg in wsB:
                ev = json.loads(msg)
                if ev.get('type') != 'event': continue
                p = ev['payload']
                if p.get('client_id') != 'bob': continue
                if p.get('type') == 'queued':
                    queued_seen = True
                elif p.get('type') == 'status' and p.get('phase') == 'queued':
                    status_queued_seen = True
                elif p.get('type') == 'token' and queued_seen:
                    token_after_queued = True
                    break
            ok = queued_seen and status_queued_seen and token_after_queued
            print('PASS' if ok else f'FAIL queued={queued_seen} status_queued={status_queued_seen} token={token_after_queued}')
asyncio.run(main())
"
```
Expected: `PASS`. Bob's WS sees BOTH the back-compat `queued` event
AND a `status` event with `phase='queued'` before any token, then
tokens arrive once alice releases the lock.

### Test 10: status verb privacy filter (mid-flight)

```bash
# Drive a long-running send as alice in the background.
( call $OB '{"type":"send","text":"count slowly to 20","client_id":"alice"}' >/dev/null ) &
sleep 1.0

# Snapshot from BOB's perspective: alice is current, bob has nothing.
# Bob must not see alice's text in `current`.
call $OB '{"type":"status","client_id":"bob"}' | python -c "
import json,sys
d=json.load(sys.stdin)
cur=d['current']
ok = (cur is not None
      and cur.get('is_mine') is False
      and 'text' not in cur
      and 'text_so_far' not in cur)
print('PASS' if ok else f'FAIL {cur!r}')
"
wait
```
Expected: `PASS`. Privacy filter strips `text`/`text_so_far` for any
client that is not the current entry's owner.

### Test 11: interrupt → status(done, reason=interrupted)

```bash
( call $OB '{"type":"send","text":"count slowly to 200","client_id":"alice"}' >/dev/null ) &
sleep 0.8

uv run --active python -c "
import asyncio, json, websockets
async def main():
    async with websockets.connect('ws://localhost:$PORT/$OB/ws') as ws:
        await ws.send(json.dumps({'type':'watch','src':'$OB'}))
        # Trigger interrupt
        await ws.send(json.dumps({'type':'call','target':'$OB','payload':{'type':'interrupt'},'id':'X'}))
        async for msg in ws:
            ev = json.loads(msg)
            p = ev.get('payload') or {}
            if p.get('type') == 'status' and p.get('phase') == 'done':
                ok = p.get('detail', {}).get('reason') == 'interrupted'
                print('PASS' if ok else f'FAIL detail={p.get(\"detail\")}')
                return
asyncio.run(main())
"
wait
```
Expected: `PASS`. The terminal `status` carries `detail.reason='interrupted'`.

## Summary

| # | Test | Pass |
|---|------|------|
| 1 | send fails without file_agent_id | |
| 2 | reflect shows file_agent_id | |
| 3 | send streams + persists chat_<client_id>.json | |
| 4 | tool-call round-trip + lossless tool history on disk | |
| 5 | history persists across calls | |
| 6 | history verb returns messages | |
| 7 | status verb idle snapshot + reflect lists status | |
| 8 | status events fire at every phase transition | |
| 9 | contended send emits queued + status(queued) | |
| 10 | status verb privacy filter (cross-client) | |
| 11 | interrupt → status(done, reason='interrupted') | |

Also report:
- provider + model used.
- whether tool-calling Test 4 / Test 8 was skipped (model dependent).
