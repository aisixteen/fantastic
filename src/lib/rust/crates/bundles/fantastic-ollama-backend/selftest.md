# fantastic-ollama-backend selftest

> scopes: ai, kernel, persistence, http
> requires: `cargo build --release --bin fantastic_kernel`; live ollama at a
> reachable endpoint with a tool-calling model (a tool-calling model);
> a LIVE daemon + WS — this bundle caches the HTTP client + in-flight
> tasks in process-memory, so multi-step state would be lost between
> separate one-shot `fantastic` calls. Drive via the WS surface.
> out-of-scope: HTTP routes (covered by fantastic-web selftest), browser

Reflect-driven LLM agent. Per-client chat threads, FIFO lock, `<call>` tag
tool-calls; the conversation lives in the memory store the record names
(`memory_store_id`).

**Why a live daemon is required:** the `OllamaProvider` HTTP client and
the in-flight streaming task live in `BACKENDS` (process-global). A
fresh `fantastic <id> send` spawns a SEPARATE kernel each time and loses
the lock/queue/history. Boot ONE daemon, then drive it over WS.

## Pre-flight

ASK USER for endpoint (default `http://localhost:11434`) and a
tool-calling model. Verify reachable BEFORE wiping state:

```bash
curl -s http://localhost:11434/api/tags | jq -e '.models | length >= 1'
# confirm the named model is in the list; else STOP, report.
```

```bash
rm -rf /tmp/ob_test
mkdir -p /tmp/ob_test
cd /tmp/ob_test
FANTASTIC=/path/to/rust/target/release/fantastic_kernel
PORT=18911
MODEL=<your tool-calling model>

$FANTASTIC kernel_state create_agent handler_module=web.tools id=w port=$PORT
$FANTASTIC w create_agent handler_module=web_ws.tools id=wws
$FANTASTIC kernel_state create_agent handler_module=ram_store.tools id=mem
$FANTASTIC kernel_state create_agent handler_module=ollama_backend.tools id=ob \
  model=$MODEL endpoint=http://localhost:11434 memory_store_id=mem can_call='{"mem":"*"}'
$FANTASTIC &
DAEMON_PID=$!
trap 'kill $DAEMON_PID 2>/dev/null' EXIT
sleep 2

# One-shot WS round-trip: open ws, send a call frame, print reply data.
call() {
  python3 - <<PY
import asyncio, json, websockets
async def main():
    async with websockets.connect("ws://localhost:$PORT/wws/ws") as ws:
        await ws.send(json.dumps({"type":"call","target":"$1","payload":json.loads('''$2'''),"id":"1"}))
        async for msg in ws:
            d = json.loads(msg)
            if d.get("id")=="1" and d.get("type") in ("reply","error"):
                print(json.dumps(d.get("data") if d["type"]=="reply" else {"error":d["error"]}))
                return
asyncio.run(main())
PY
}
```

## Tests

### Test 1: reflect lists the backend contract

```bash
call ob '{"type":"reflect"}' | jq -e '
  .memory_store_id == "mem"
  and (.verbs | has("send") and has("history") and has("status"))
  and (.verbs | has("interrupt") | not)
  and (.cancel | test("cancel"))'
```
Expect: `memory_store_id` is set; `verbs` covers the LLM-backend surface. There is
NO `interrupt` verb — an in-flight `send` is a KERNEL TASK, killed with the
substrate's universal `ps` + `cancel` (the `cancel` key teaches how).

### Test 2: send streams + persists the thread

Default caller is `client_id="cli"`. The thread is rows in the memory store
(`mem`) — the AI writes no disk itself. Read it back with the `history` verb.

```bash
call ob '{"type":"send","text":"reply with the single word: ok"}' | jq -e '.final'
call ob '{"type":"history"}' \
  | jq -e '.messages | length >= 2 and (.[-1].content | ascii_downcase | contains("ok"))'
```
Expect: a `final`; `history` has ≥ 2 messages, last contains "ok".

### Test 3: history persists across calls

```bash
call ob '{"type":"send","text":"my favorite color is teal, remember it"}' >/dev/null
call ob '{"type":"send","text":"what color did I just say?"}' \
  | jq -e '.final | ascii_downcase | contains("teal")'
```
Expect: response mentions "teal" — proves the thread round-trip via `mem`.

### Test 4: history verb returns the thread

```bash
call ob '{"type":"history"}' | jq -e '.messages | length >= 4'
```
Expect: ≥ 4 messages (the pairs from Tests 2 + 3).

### Test 5: status snapshot when idle

```bash
call ob '{"type":"status","client_id":"alice"}' \
  | jq -e '.generating == false and .current == null and .client_id == "alice"'
```
Expect: idle snapshot scoped to the caller's `client_id`.

## Cleanup

```bash
kill $DAEMON_PID 2>/dev/null
cd / && rm -rf /tmp/ob_test
```

## Summary table

| Test | Pass / Fail | Notes |
|---|---|---|
| 1. reflect contract |  |  |
| 2. send streams + persists |  |  |
| 3. history persists across calls |  |  |
| 4. history verb |  |  |
| 5. status idle snapshot |  |  |

Also report: endpoint + model used.
