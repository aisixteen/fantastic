# nvidia_nim_backend selftest

> scopes: kernel, ai, persistence, http
> requires: `uv sync`; live network for any test marked AI.
> Pre-flight: `NVIDIA_API_KEY` in the kernel's environment (an `nvapi-...` key from
> https://build.nvidia.com, ~40 RPM/model), and ASK the user for the endpoint + model
> (both REQUIRED on the record — nothing is guessed). Without the key, AI tests are
> skipped (the rest still run).
> out-of-scope: chat UI flow (now the TS `ai_view` in `ts/`)

Rate-limit handling: on HTTP 429 BEFORE any chunk has been streamed,
the backend honors `Retry-After` (clamped to 60s), emits a
`status(thinking, waiting_on='rate_limit')` event, and retries once. A
second 429 surfaces a clean error
(`send: rate limited (429); retry in Ns`). Mid-stream 429 is rare
and propagates without retry to avoid duplicate tokens.

NVIDIA NIM-backed LLM agent (OpenAI-compatible). Same surface as
`ollama_backend` (send/history/status/recall/context_status/…). There is no
`interrupt` verb — an in-flight `send` is a kernel task, killed with the universal
`ps` → `cancel` system verbs (the bundle's `on_cancel` closes the turn out). The key
is read from `NVIDIA_API_KEY` in the kernel's environment at call time — never on
the record, never returned by reflect (`has_api_key` is a boolean).

## Pre-flight

```bash
rm -rf .fantastic
PORT=18904
pkill -9 -f "fantastic" 2>/dev/null
uv run --active python fantastic kernel create_agent parent_id=kernel_state handler_module=web.tools port=$PORT >/dev/null
WEB_ID=$(ls .fantastic/agents | grep '^web_' | head -1)
uv run --active fantastic kernel create_agent parent_id=$WEB_ID handler_module=web_ws.tools ingress_rule=allow_all_ingress >/dev/null
uv run --active python fantastic > /tmp/n.log 2>&1 &
SPID=$!
for i in $(seq 1 20); do grep -q "kernel up" /tmp/n.log 2>/dev/null && break; sleep 0.5; done

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

MEM=$(call kernel '{"type":"create_agent","parent_id":"kernel_state","handler_module":"ram_store.tools"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
# ENDPOINT / MODEL: what the user named in pre-flight.
NB=$(call kernel "{\"type\":\"create_agent\",\"parent_id\":\"kernel_state\",\"handler_module\":\"nvidia_nim_backend.tools\",\"endpoint\":\"$ENDPOINT\",\"model\":\"$MODEL\",\"memory_store_id\":\"$MEM\",\"can_call\":{\"$MEM\":\"*\"}}" \
  | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
```

After all tests:
```bash
kill -9 $SPID 2>/dev/null; rm -rf .fantastic /tmp/n.log
```

## Tests

### Test 1: no endpoint/model on the record ⇒ send fails loud naming the field

```bash
NB2=$(call kernel '{"type":"create_agent","parent_id":"kernel_state","handler_module":"nvidia_nim_backend.tools"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
call $NB2 '{"type":"send","text":"hi"}' | python -m json.tool
```
Expected: an error naming `endpoint` (the first missing field) — nothing is guessed.

### Test 2: no key ⇒ send fails loud naming `NVIDIA_API_KEY`

Run with `NVIDIA_API_KEY` unset in the kernel's environment.

```bash
call $NB '{"type":"send","text":"hi"}' | python -m json.tool
```
Expected: `{"error":"nvidia_nim_backend: no api key — set NVIDIA_API_KEY", …}`.

### Test 3: `reflect.has_api_key` reports the key, never its value

```bash
call $NB '{"type":"reflect"}' | python -c "import json,sys;d=json.load(sys.stdin);print('has_api_key:',d['has_api_key']);print('leak:', 'nvapi-' in json.dumps(d))"
```
Expected: `has_api_key` matches whether the kernel has the env var; `leak: False`.

### Test 5: live single-shot generation (AI)

Skip if `NVIDIA_API_KEY` is unset.

```bash
[ -n "$NVIDIA_API_KEY" ] && {
  call $NB '{"type":"send","text":"reply with the single word: ok","client_id":"selftest"}' \
    | python -c "import json,sys;d=json.load(sys.stdin);print(repr(d.get('final','')))"
} || echo "SKIPPED (no NVIDIA_API_KEY)"
```
Expected: a string containing `ok` (case-insensitive). Verifies the
SSE stream + auth + model selection round-trip.

### Test 6: per-client chat threads persist

Skip if `NVIDIA_API_KEY` is unset.

The conversation is rows in the memory store the record names (the AI writes no
disk itself); read each thread back via the `history` verb.

```bash
[ -n "$NVIDIA_API_KEY" ] && {
  call $NB '{"type":"send","text":"my color is blue","client_id":"alice"}' >/dev/null
  call $NB '{"type":"send","text":"my color is red","client_id":"bob"}' >/dev/null
  call $NB '{"type":"history","client_id":"alice"}' | python -c "import json,sys;print('alice:', json.load(sys.stdin)['messages'][0]['content'])"
  call $NB '{"type":"history","client_id":"bob"}'   | python -c "import json,sys;print('bob:',   json.load(sys.stdin)['messages'][0]['content'])"
} || echo "SKIPPED (no NVIDIA_API_KEY)"
```
Expected: alice's thread opens with "my color is blue", bob's with "my
color is red" — distinct per-client threads.

### Test 8: status verb + reflect document the new pipeline

```bash
call $NB '{"type":"status","client_id":"alice"}' | python -m json.tool
call $NB '{"type":"reflect"}' | python -c "
import json,sys
d=json.load(sys.stdin)
print('status verb:', 'status' in d['verbs'])
print('status event:', 'status' in d['emits'])
"
```
Expected: idle snapshot `{generating:false, current:null, mine_pending:[], others_pending:0, …}`.
`status` is in both `verbs` and `emits`.

### Test 9: status events fire across phase transitions (AI)

Skip if `NVIDIA_API_KEY` is unset.

```bash
[ -n "$NVIDIA_API_KEY" ] && uv run --active python -c "
import asyncio, json, os, websockets
PORT = '$PORT'
NB = '$NB'
async def main():
    async with websockets.connect(f'ws://localhost:{PORT}/{NB}/ws') as ws:
        await ws.send(json.dumps({'type':'watch','src':NB}))
        await ws.send(json.dumps({
            'type':'call','target':NB,
            'payload':{'type':'send','text':'reply with: ok','client_id':'alice'},
            'id':'1',
        }))
        phases = []
        async for msg in ws:
            ev = json.loads(msg)
            p = ev.get('payload') or {}
            if p.get('type') == 'status' and p.get('client_id') == 'alice':
                phases.append(p['phase'])
                if p['phase'] == 'done': break
        ok = phases[0] == 'thinking' and phases[-1] == 'done' and 'streaming' in phases
        print('phases:', phases)
        print('PASS' if ok else 'FAIL')
asyncio.run(main())
" || echo "SKIPPED (no NVIDIA_API_KEY)"
```
Expected: `PASS`. Sequence starts with `thinking`, contains
`streaming`, ends with `done`. If the model emits a tool_call on this
prompt, also expect `tool_calling` (entry+exit).

### Test 10: rate-limit retry surfaces status(thinking, waiting_on='rate_limit')

Provider-faked test (no real network needed). The unit test
`test_status_thinking_during_429_wait` covers the same behavior; this
selftest is informational — if you hit a real 429 in normal use, the
chat UI will pulse a "rate-limited; waiting Ns" hint in the status
footer. See `bundled_agents/ai/nvidia/nvidia_nim_backend/tests/test_nvidia_nim_handler.py::test_status_thinking_during_429_wait`.

## Summary

| # | Test | Pass |
|---|------|------|
| 1 | send fails loud w/o endpoint/model | |
| 2 | send fails loud w/o NVIDIA_API_KEY | |
| 3 | reflect.has_api_key, no key leak | |
| 5 (AI) | live single-shot generation | |
| 6 (AI) | per-client chat threads | |
| 8 | status verb idle snapshot + reflect lists status | |
| 9 (AI) | status events fire across phase transitions | |
| 10 (unit-only) | rate-limit retry → status(thinking, waiting_on='rate_limit') | |
