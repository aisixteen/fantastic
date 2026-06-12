# ws_bridge selftest

> scopes: kernel, ws, ssh
> requires: `uv sync`. The MemoryTransport spec is fully in-process
> (no network); the WS spec needs a running `fantastic`; the
> SSH+WS spec needs a real remote host with `fantastic` installed +
> SSH key access.
> out-of-scope: `ssh_runner` (separate bundle, separate selftest).

The bridge is **WS-only** (since the HTTP/REST removal). A bridge
agent on kernel A opens a WS to kernel B's `web_ws` endpoint and
ships **raw call frames** —
`{type:'call', id, target, payload}`. B's `web_ws` routes
`kernel.send(target, payload)` exactly like a browser frame.
The matching `{type:'reply', id, data}` flows back. **No B-side
bridge agent needed** — asymmetric client/server.

Streams use the same WS protocol's watch frames:
A → B `{type:'watch', src:<target>}`, B → A
`{type:'event', payload}`. The bridge re-emits each event on its own
inbox so local watchers see remote streams through
`kernel.watch(<bridge_id>, ...)`.

## Test 1 — MemoryTransport pair round-trip + streaming (in-process)

The headline correctness suite. Covered by the unit tests at
`bundled_agents/io/ws_bridge/tests/test_ws_bridge.py`:

  - `test_memory_transport_pair_round_trip` — two `Kernel()` instances,
    two paired `MemoryTransport`s, a
    `forward(target='kernel_state', payload={type:'reflect'})` from kernel A
    returns kernel B's `kernel_state.reflect` body.
  - `test_watch_remote_sends_watch_frame` — `watch_remote` ships the
    `{type:'watch', src:...}` frame.
  - `test_event_frame_re_emits_on_bridge_inbox` — inbound `event`
    frames fan out via the bridge agent's `_watcher_ids`.
  - `test_unwatch_remote_sends_unwatch_frame` — symmetric teardown.
  - `test_deny_inbound_refuses_inbound_call` — a leg with `auth="deny_inbound"`
    refuses an inbound `call`, replying `{reason:"unauthorized"}` (directional /
    hub→spoke push); `test_deny_inbound_default_refuses_inbound_call` proves the
    SEAL is the DEFAULT — an absent `auth`/`ingress_rule` refuses too (IO legs seal
    by default). The success path is `test_memory_transport_pair_round_trip`, whose
    serving leg is opened CONSCIOUSLY with `ingress_rule=allow_all`.
  - `test_password_*` — the kernel-group shared secret across the symmetric
    `ingress_rule` (CHECK the envelope `auth_token`) + `egress_rule` (PRESENT it)
    rules: an inbound `call` with the matching token dispatches, a wrong/missing
    token is refused `unauthorized`, a `password` leg PRESENTS its token on outbound
    `forward`s, and a non-`password` leg attaches none (wire unchanged). Rules
    resolve by name from the `ingress_rules` / `egress_rules` registries.

```bash
uv run pytest bundled_agents/io/ws_bridge/tests/ -v
```
Expected: all 20 tests pass.

## Test 2 — WS transport against a live `fantastic` (manual)

Boots two kernels on the same machine on different ports. A's bridge
connects to B's `/<kernel_state>/ws` directly — B does NOT need a bridge agent
because B's `web_ws` handles inbound call frames natively.

```bash
PORT_A=18900
PORT_B=18901
pkill -9 -f "fantastic" 2>/dev/null
mkdir -p /tmp/kb_test_a /tmp/kb_test_b

(cd /tmp/kb_test_a && uv run --project /Users/oleksandr/Projects/fantastic_canvas/python \
  python /Users/oleksandr/Projects/fantastic_canvas/python/fantastic kernel_state create_agent handler_module=web.tools port=$PORT_A) >/dev/null
(cd /tmp/kb_test_a && uv run --project /Users/oleksandr/Projects/fantastic_canvas/python \
  python /Users/oleksandr/Projects/fantastic_canvas/python/fantastic) \
  > /tmp/kb_a.log 2>&1 &
SPID_A=$!

(cd /tmp/kb_test_b && uv run --project /Users/oleksandr/Projects/fantastic_canvas/python \
  python /Users/oleksandr/Projects/fantastic_canvas/python/fantastic kernel_state create_agent handler_module=web.tools port=$PORT_B) >/dev/null
(cd /tmp/kb_test_b && uv run --project /Users/oleksandr/Projects/fantastic_canvas/python \
  python /Users/oleksandr/Projects/fantastic_canvas/python/fantastic) \
  > /tmp/kb_b.log 2>&1 &
SPID_B=$!

# Wait for both to print [kernel] up
for i in $(seq 1 30); do
  grep -q "kernel.*up" /tmp/kb_a.log && grep -q "kernel.*up" /tmp/kb_b.log && break
  sleep 0.3
done

# B needs a web_ws child of its web so the WS surface is mounted.
# Run via REPL/CLI on B — fastest path is a direct kernel.send via
# Python helper. The helper opens a one-shot WS against the given
# port, sends a `call` frame, prints the reply.
call_at() {
  PORT="$1" TARGET="$2" PAYLOAD="$3" uv run --active python - <<'PY'
import asyncio, json, os, websockets
port = os.environ["PORT"]; target = os.environ["TARGET"]
payload = json.loads(os.environ["PAYLOAD"])
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

# Create A's bridge — peer_id is the WS path segment on B that web_ws
# is mounted under. The web_ws agent's id is the path key (mounted at
# /<web_ws_id>/ws). For a default web with a web_ws child, that id is
# typically `web_ws_<hash>` — read it out of B's list_agents.
B_WS_ID=$(call_at $PORT_B kernel_state '{"type":"list_agents"}' \
  | python3 -c "import json,sys; ags=json.load(sys.stdin)['agents']; print(next(a['id'] for a in ags if a['handler_module']=='web_ws.tools'))")

A_BRIDGE_ID=$(call_at $PORT_A kernel_state "{\"type\":\"create_agent\",\"handler_module\":\"ws_bridge.tools\",\"transport\":\"ws\",\"peer_id\":\"$B_WS_ID\",\"host\":\"127.0.0.1\",\"local_port\":$PORT_B}" \
  | python3 -c "import json,sys; print(json.load(sys.stdin)['id'])")

# Boot A's bridge — opens WS to B's web_ws.
call_at $PORT_A $A_BRIDGE_ID '{"type":"boot"}' | python3 -m json.tool

# Forward a reflect to B's kernel_state through A's bridge.
call_at $PORT_A $A_BRIDGE_ID '{"type":"forward","target":"kernel_state","payload":{"type":"reflect"}}' \
  | python3 -m json.tool | head -20

# Cleanup — cascade-delete fires the on_delete hook which cancels
# the read loop, closes the transport, rejects pending Futures.
call_at $PORT_A kernel_state "{\"type\":\"delete_agent\",\"id\":\"$A_BRIDGE_ID\"}" >/dev/null
kill -9 $SPID_A $SPID_B
rm -rf /tmp/kb_test_a /tmp/kb_test_b /tmp/kb_a.log /tmp/kb_b.log
```
Expected: the forward returns kernel B's `kernel_state.reflect` body —
`{id:'kernel_state', sentence:'Core agent...', verbs:{...}, tree:..., ...}`.
Regression signal: `error: not connected` after boot → ws connect
failed (check kernel B's log + that `peer_id` matches B's web_ws
agent's id); hang on forward → corr_id mismatch in the read_loop
dispatch.

## Test 3 — SSH+WS transport against a real remote (manual)

Requires a remote host with fantastic installed and SSH key access:
`ssh <host>` works without password.

```bash
HOST=gpu-box                    # your real host alias
REMOTE_PORT=8888
LOCAL_PORT=49001
PORT_LOCAL=8888

# 1. On REMOTE: start fantastic with web + web_ws persisted.
ssh $HOST 'cd ~/fantastic_target_proj && uv run --project ~/fantastic_canvas/python fantastic >/tmp/remote-serve.log 2>&1 &'
sleep 3

# 2. On REMOTE: find the web_ws id.
B_WS_ID=$(ssh $HOST "REMOTE_PORT=$REMOTE_PORT python3 - <<'PY'
import asyncio, json, os, websockets
port = os.environ['REMOTE_PORT']
async def main():
    async with websockets.connect(f'ws://localhost:{port}/kernel_state/ws') as ws:
        await ws.send(json.dumps({'type':'call','target':'kernel_state','payload':{'type':'list_agents'},'id':'1'}))
        while True:
            m = json.loads(await ws.recv())
            if m.get('id')=='1' and m.get('type') in ('reply','error'):
                ws_id = next(a['id'] for a in m['data']['agents'] if a['handler_module']=='web_ws.tools')
                print(ws_id); return
asyncio.run(main())
PY")
echo "remote web_ws: $B_WS_ID"

# 3. LOCAL: create a bridge with transport=ssh+ws, host, local_port,
#    remote_port, peer_id=$B_WS_ID
A_ID=$(call_at $PORT_LOCAL kernel_state "{\"type\":\"create_agent\",\"handler_module\":\"ws_bridge.tools\",\"transport\":\"ssh+ws\",\"host\":\"$HOST\",\"local_port\":$LOCAL_PORT,\"remote_port\":$REMOTE_PORT,\"peer_id\":\"$B_WS_ID\"}" \
  | python3 -c 'import json,sys;print(json.load(sys.stdin)["id"])')

# 4. Boot — opens SSH tunnel + WS over it
call_at $PORT_LOCAL $A_ID '{"type":"boot"}' | python3 -m json.tool
# Expect: {"booted":true, "transport":"ssh+ws", "tunnel_pid":<int>}

# 5. Forward a reflect
call_at $PORT_LOCAL $A_ID '{"type":"forward","target":"kernel_state","payload":{"type":"reflect"}}' | python3 -m json.tool
# Expect: remote kernel_state.reflect body
```
Expected: `tunnel_pid` non-null in boot reply; forward returns the
remote kernel_state's reflect.
Regression signal: `tunnel failed: …` → SSH key or host lookup
problem (try `ssh $HOST 'echo ok'` first); `tunnel_pid: null` after
boot → ServerAliveCountMax tripped early (network flaky); reply
contains the remote web_ws's reflect instead of kernel_state's → peer_id
was used as a target id, not just a WS path (that's a bug — file).

## Test 4 — WS streaming (watch_remote) against a live `fantastic` (manual)

Same setup as Test 2. After A's bridge is booted, subscribe to B's
kernel_state inbox via `watch_remote`. Then trigger an event on B and verify
A's bridge re-emits it.

```bash
# Assumes the Test 2 setup is still up + A_BRIDGE_ID is exported.

# Subscribe to B.kernel_state's emits via A's bridge.
call_at $PORT_A $A_BRIDGE_ID '{"type":"watch_remote","target":"kernel_state"}'
# Expect: {"ok": true, "watching": "kernel_state"}

# In a second terminal: tail A's bridge inbox.
# (A separate WS that subscribes to the bridge's inbox.)
PORT="$PORT_A" TARGET="$A_BRIDGE_ID" uv run --active python - <<'PY' &
import asyncio, json, os, websockets
port = os.environ["PORT"]; target = os.environ["TARGET"]
async def main():
    async with websockets.connect(f"ws://localhost:{port}/{target}/ws") as ws:
        await ws.send(json.dumps({"type":"watch","src":target,"id":"w"}))
        while True:
            print(await ws.recv())
asyncio.run(main())
PY
TAIL_PID=$!
sleep 1

# Trigger something on B that emits — call any verb that emits state.
# Easiest: emit directly via kernel_state on B (substrate exposes it).
call_at $PORT_B kernel_state '{"type":"reflect"}' >/dev/null
# Or any verb that fans out a state event on B's kernel_state.

# Expect: A's bridge tail shows {"type":"event","payload":{...}} frames
# whose payload mirrors B.kernel_state's emits.

kill $TAIL_PID
```
Expected: A's bridge fan-out includes events that originated on B's
kernel_state. Regression signal: no events → check that B's web_ws received
the `{type:'watch'}` (look in B's log); A's read loop never re-emits
→ check that A's read_loop processes `event` frame type.

## Summary

| # | Test | Pass |
|---|------|------|
| 1 | MemoryTransport + streaming + auth-policy (deny_inbound + password) unit suite (20 tests, in-process) | |
| 2 (manual) | WS transport — A.bridge → B.web_ws round-trip | |
| 3 (manual) | SSH+WS transport against a real remote | |
| 4 (manual) | WS streaming — watch_remote re-emit | |
