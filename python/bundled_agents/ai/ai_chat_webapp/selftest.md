# ai_chat_webapp selftest

> scopes: webapp, web
> requires: `uv sync`; running webapp serve; for browser-manual test, a
> live LLM backend agent (ollama_backend, nvidia_nim_backend, …) + browser
> out-of-scope: actual chat dialogue (covered by each backend's selftest)

Provider-agnostic chat UI agent fronting any LLM backend that answers
`send` / `history` / `interrupt`.

## Pre-flight

```bash
cd new_codebase
rm -rf .fantastic
PORT=18903
pkill -9 -f "fantastic" 2>/dev/null
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

### Test 1: create ai_chat_webapp → provider backend auto-spawned

Default provider is `ollama` (override with `provider:"nvidia_nim"`).
The webapp's `_boot` creates the corresponding backend as its child
and sets `upstream_id` on its own record.

```bash
OW=$(call core '{"type":"create_agent","handler_module":"ai_chat_webapp.tools"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
sleep 0.3
call $OW '{"type":"reflect"}' | python -c "
import json,sys
d=json.load(sys.stdin)
ok=isinstance(d.get('upstream_id'),str) and d['upstream_id'].startswith('ollama_backend_')
print('upstream auto-set:', 'PASS' if ok else f'FAIL d={d}')
"

call $OW '{"type":"get_webapp"}' | python -m json.tool
```
Expected: `upstream auto-set: PASS` then
`{url:"/<OW>/", default_width:360, default_height:480, title:"chat"}`.

### Test 2: served HTML has chat UI markers

```bash
curl -s "http://localhost:$PORT/$OW/" | grep -E "chat|fantastic_transport|status-footer|tool-block" | wc -l
```
Expected: ≥ 4 lines match (chat title, transport injection, the
status-footer band, and the tool-block CSS class).

### Test 2b: served HTML carries the status pipeline

```bash
HTML=$(curl -s "http://localhost:$PORT/$OW/")
echo "$HTML" | grep -F "type: 'status'" >/dev/null && echo "boot status call: yes"
echo "$HTML" | grep -F "t.on('status'" >/dev/null && echo "status subscription: yes"
echo "$HTML" | grep -F "queuedBubbles" >/dev/null && echo "FIFO map: yes"
echo "$HTML" | grep -F "mine_pending" >/dev/null && echo "boot snapshot consumed: yes"
echo "$HTML" | grep -F "@keyframes" >/dev/null && echo "pulse animation: yes"
```
Expected: every line ends in `yes`. Drift guard against UI regressions.

### Test 3: provider switch — `provider:"nvidia_nim"` spawns the NIM backend

```bash
OW2=$(call core '{"type":"create_agent","handler_module":"ai_chat_webapp.tools","provider":"nvidia_nim"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
sleep 0.3
call $OW2 '{"type":"reflect"}' | python -c "
import json,sys
d=json.load(sys.stdin)
ok=isinstance(d.get('upstream_id'),str) and d['upstream_id'].startswith('nvidia_nim_backend_')
print('provider-switch: PASS' if ok else f'FAIL d={d}')
"
```
Expected: `provider-switch: PASS`.

### Test 4 (manual, requires live LLM backend): chat in browser

The chat webapp from Test 1 already has its provider backend wired
via `_boot`. Configure the backend's `file_agent_id` (and `api_key`
for nvidia_nim) before opening:

```bash
# Find backend id from the webapp's reflect.
BACKEND=$(call $OW '{"type":"reflect"}' | python -c "import json,sys;print(json.load(sys.stdin)['upstream_id'])")
# Provision a file agent for the backend's persistence + (for nvidia) api_key.
FA=$(call core '{"type":"create_agent","handler_module":"file.tools"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
call core "{\"type\":\"update_agent\",\"id\":\"$BACKEND\",\"file_agent_id\":\"$FA\"}"
```

Open `http://localhost:$PORT/$OW/` in browser.

Walk through the user-visible behaviors in order. Each is a separate
PASS/FAIL signal — note which step regresses if any do.

1. **Stream**: type "hi" + Enter → tokens stream into an assistant
   bubble in `#dialog`.
2. **Phase pill**: status footer shows the current phase (`thinking`
   → `streaming` → `done`) and pulses on thinking/tool_calling. An
   elapsed counter ticks every ~250 ms during active phases.
3. **Tool blocks**: ask "use the send tool to call list_agents on
   core, then summarize". A tool block appears inline in the
   assistant bubble: header `list_agents(core)…` while pending,
   settles to `list_agents(core) ✓` with `<details>args/reply</details>`
   on completion. Each tool_call gets its own block.
4. **Queue stack**: while a generation is running, type a second
   message + Enter. Bubble lands in `#queued` (above the status
   footer) with a ⌛ marker, input clears, current generation
   continues. Stack a third. After the running turn's `done`, the
   topmost queued bubble is promoted into `#dialog` and the next turn
   begins. Drains FIFO.
5. **Stop button**: while a turn streams, the send button reads
   "stop" (red). Click it → in-flight cancels mid-stream;
   `done(reason='interrupted')` lands; queued bubbles REMAIN and
   start draining once the lock releases.
5b. **ESC key**: while a turn streams, press `Esc` (regardless of
   whether the input has focus). Same effect as clicking stop —
   `interrupt` verb fires, `done(reason='interrupted')` lands,
   queued bubbles persist. Confirms the global keydown binding.
6. **Reload mid-flight**: with a long generation in progress, refresh
   the tab. The boot snapshot rebuilds: in-flight user bubble +
   assistant bubble pre-filled with `text_so_far`, last tool block
   restored, phase pill at the right phase. New tokens continue to
   land. (Tokens already streamed before the reload are not replayed
   — the next `done` triggers a clean state; the assistant bubble's
   final form lives in `chat_<client>.json`.)
7. **Reload with queue**: stack 2-3 queued bubbles, refresh. Boot
   snapshot rebuilds the queued band from `mine_pending`; bubbles
   keep the ⌛ marker until promoted.
8. **Cross-tab privacy**: open a SECOND browser tab on the same chat
   URL (different `client_id` because localStorage isolates per-tab
   only if you clear it; otherwise same client). With distinct
   client_ids, send from each. Each tab sees only its own bubbles
   and stream. Tab 1's status footer shows "+N from other clients"
   when tab 2 has queued items.

Regression signals:
- Tokens stream but no phase pill change → status subscription broke.
- Tool calls happen (in cli) but no inline block → tool_calling phase
  not received or markup regressed.
- Reload shows blank dialog mid-flight → boot snapshot not consumed.
- Enter while busy is ignored → submit guard reintroduced.

## Summary

| # | Test | Pass |
|---|------|------|
| 1 | _boot auto-spawns provider backend (default ollama) | |
| 2 | served HTML has chat markers | |
| 2b | served HTML carries status pipeline (drift guard) | |
| 3 | provider switch (`nvidia_nim`) spawns NIM backend | |
| 4.1 (manual) | streaming into assistant bubble | |
| 4.2 (manual) | phase pill cycles + elapsed ticks | |
| 4.3 (manual) | tool blocks render inline | |
| 4.4 (manual) | Enter-while-busy stacks queued FIFO | |
| 4.5 (manual) | stop button interrupts; queue persists | |
| 4.5b (manual) | ESC key interrupts (any focus); queue persists | |
| 4.6 (manual) | reload mid-flight rebuilds in-flight + tool blocks | |
| 4.7 (manual) | reload with queued bubbles rebuilds queue band | |
| 4.8 (manual) | cross-client privacy (text scoped to own client_id) | |
