# nvidia_nim_backend selftest

> scopes: kernel, ai, persistence, http
> requires: `uv sync`; live network for any test marked AI.
> Pre-flight: an `NVAPI_KEY` env var with an `nvapi-...` key from
> https://build.nvidia.com (free signup, ~40 RPM/model rate limit).
> If `NVAPI_KEY` is absent, AI tests are skipped (the rest still run).
> out-of-scope: chat UI flow (now the TS `ai_view` in `ts/`)

Rate-limit handling: on HTTP 429 BEFORE any chunk has been streamed,
the backend honors `Retry-After` (clamped to 60s), emits a
`status(thinking, waiting_on='rate_limit')` event, and retries once. A
second 429 surfaces a clean error
(`send: rate limited (429); retry in Ns`). Mid-stream 429 is rare
and propagates without retry to avoid duplicate tokens.

NVIDIA NIM-backed LLM agent (OpenAI-compatible). Same surface as
`ollama_backend` (send/history/interrupt/refresh_menu) plus
`set_api_key`/`clear_api_key`. The api_key is stored as a sidecar
file at `.fantastic/agents/<id>/api_key` via `file_agent_id` —
never in `agent.json`, never returned by reflect.

## Pre-flight

```bash
cd new_codebase
rm -rf .fantastic
PORT=18904
pkill -9 -f "fantastic" 2>/dev/null
uv run --active python fantastic fs_loader create_agent handler_module=web.tools port=$PORT >/dev/null
WEB_ID=$(ls .fantastic/agents | grep '^web_' | head -1)
uv run --active fantastic $WEB_ID create_agent handler_module=web_ws.tools >/dev/null
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

FA=$(call fs_loader '{"type":"create_agent","handler_module":"file.tools"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
NB=$(call fs_loader "{\"type\":\"create_agent\",\"handler_module\":\"nvidia_nim_backend.tools\",\"file_agent_id\":\"$FA\"}" \
  | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
```

After all tests:
```bash
kill -9 $SPID 2>/dev/null; rm -rf .fantastic /tmp/n.log
```

## Tests

### Test 1: `_send` failfast when `file_agent_id` unset

```bash
NB2=$(call fs_loader '{"type":"create_agent","handler_module":"nvidia_nim_backend.tools"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
call $NB2 '{"type":"send","text":"hi"}' | python -m json.tool
```
Expected: `{"error":"nvidia_nim_backend: file_agent_id required"}`.

### Test 2: `set_api_key` failfast when `file_agent_id` unset

```bash
call $NB2 '{"type":"set_api_key","api_key":"nvapi-x"}' | python -m json.tool
```
Expected: `{"error":"nvidia_nim_backend: file_agent_id required"}`.

### Test 3: `_send` failfast when api_key not set

```bash
call $NB '{"type":"send","text":"hi"}' | python -m json.tool
```
Expected: `{"error":"nvidia_nim_backend: api_key not set; call set_api_key first"}`.

### Test 4: `set_api_key` writes sidecar; `reflect.has_api_key` flips

Skip if `NVAPI_KEY` is unset.

```bash
[ -n "$NVAPI_KEY" ] && {
  call $NB "{\"type\":\"set_api_key\",\"api_key\":\"$NVAPI_KEY\"}" | python -m json.tool

  test -f ".fantastic/agents/$NB/api_key" && echo "key file present"
  call $NB '{"type":"reflect"}' | python -c "import json,sys;d=json.load(sys.stdin);print('has_api_key:',d['has_api_key'])"
} || echo "SKIPPED (no NVAPI_KEY)"
```
Expected: `{"ok": true}`, sidecar exists, `has_api_key: True`. The
reflect blob never contains the key value.

### Test 5: live single-shot generation (AI)

Skip if `NVAPI_KEY` is unset.

```bash
[ -n "$NVAPI_KEY" ] && {
  call $NB '{"type":"send","text":"reply with the single word: ok","client_id":"selftest"}' \
    | python -c "import json,sys;d=json.load(sys.stdin);print(repr(d.get('final','')))"
} || echo "SKIPPED (no NVAPI_KEY)"
```
Expected: a string containing `ok` (case-insensitive). Verifies the
SSE stream + auth + model selection round-trip.

### Test 6: per-client chat threads persist

Skip if `NVAPI_KEY` is unset.

```bash
[ -n "$NVAPI_KEY" ] && {
  call $NB '{"type":"send","text":"my color is blue","client_id":"alice"}' >/dev/null
  call $NB '{"type":"send","text":"my color is red","client_id":"bob"}' >/dev/null
  ls .fantastic/agents/$NB/chat_*.json
} || echo "SKIPPED (no NVAPI_KEY)"
```
Expected: `chat_alice.json` AND `chat_bob.json` exist, distinct
content.

### Test 7: `clear_api_key` removes sidecar; subsequent send refuses

```bash
call $NB '{"type":"clear_api_key"}' | python -m json.tool
test -f ".fantastic/agents/$NB/api_key" && echo "FAIL: still present" || echo "key file removed"
call $NB '{"type":"send","text":"hi"}' | python -c "import json,sys;d=json.load(sys.stdin);print('error' in d and 'api_key' in d['error'])"
```
Expected: `{"ok": true, "deleted": true}`, file gone, `True`.

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

Skip if `NVAPI_KEY` is unset.

```bash
[ -n "$NVAPI_KEY" ] && uv run --active python -c "
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
" || echo "SKIPPED (no NVAPI_KEY)"
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

### Test 11: chat UI integration via the TS frontend (ai_view)

Skip if `NVAPI_KEY` is unset. Manual / browser-based.

The chat UI is now `ai_view` — a TS view-agent in `ts/` that renders
INLINE in the canvas, fronting any backend answering
send/history/interrupt/status. The backend surface is covered by Tests
5-10; this is the end-to-end browser check.

```bash
# A NIM backend on the host; the TS ai_view (frontend) fronts it by id
# over the bridge and renders the chat inline.
NB2=$(call fs_loader '{"type":"create_agent","handler_module":"nvidia_nim_backend.tools","provider":"nvidia_nim"}' \
  | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
call fs_loader "{\"type\":\"update_agent\",\"id\":\"$NB2\",\"file_agent_id\":\"$FA\"}"
call $NB2 "{\"type\":\"set_api_key\",\"api_key\":\"$NVAPI_KEY\"}"
echo "open the TS canvas in a browser, point ai_view at $NB2 — see ts/SERVE.md"
```
Expected: the inline chat view loads, typing streams tokens, stop button
or ESC interrupts mid-stream. ai_view is provider-agnostic.

## Summary

| # | Test | Pass |
|---|------|------|
| 1 | send failfast w/o file_agent_id | |
| 2 | set_api_key failfast w/o file_agent_id | |
| 3 | send failfast w/o api_key | |
| 4 | set_api_key writes sidecar, flips has_api_key, reflect doesn't leak key | |
| 5 (AI) | live single-shot generation | |
| 6 (AI) | per-client chat persistence | |
| 7 | clear_api_key + send refuses | |
| 8 | status verb idle snapshot + reflect lists status | |
| 9 (AI) | status events fire across phase transitions | |
| 10 (unit-only) | rate-limit retry → status(thinking, waiting_on='rate_limit') | |
| 11 (manual) | TS ai_view drives nvidia_nim_backend | |
