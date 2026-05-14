# kernel_bridge selftest

> scopes: kernel, ws, ssh
> requires: `uv sync`. The MemoryTransport spec is fully in-process
> (no network); the WS spec needs a running `fantastic`; the
> SSH+WS spec needs a real remote host with `fantastic` installed +
> SSH key access.
> out-of-scope: `ssh_runner` (separate bundle, separate selftest).

Pairs of bridge agents on two kernels exchange `forward` envelopes
over a transport (memory / WS / SSH+WS). A local agent that wants to
reach a remote one calls
`kernel.send(local_bridge_id, {type:'forward', target, payload})`
and gets the unwrapped reply back. The bridge speaks the existing
web/_proxy.py WS frame protocol — remote needs zero changes.

## Test 1 — MemoryTransport pair round-trip (in-process)

The headline correctness test. Covered by the unit suite at
`bundled_agents/kernel_bridge/tests/test_kernel_bridge.py::test_memory_transport_pair_round_trip`
— two `Kernel()` instances, two paired `MemoryTransport`s, a
`forward(target='core', payload={type:'reflect'})` from kernel A
returns kernel B's `core.reflect` body.

```bash
uv run pytest bundled_agents/kernel_bridge/tests/ -v
```
Expected: all 10 tests pass.

## Test 2 — WS transport against a live `fantastic` (manual)

Boots two kernels in the same machine on different ports, with
real WS over the existing webapp `/<id>/ws` proxy. No SSH.

```bash
PORT_A=18900
PORT_B=18901
pkill -9 -f "fantastic" 2>/dev/null
mkdir -p /tmp/kb_test_a /tmp/kb_test_b

(cd /tmp/kb_test_a && uv run --project /Users/oleksandr/Projects/fantastic_canvas \
  python /Users/oleksandr/Projects/fantastic_canvas/fantastic --port $PORT_A) \
  > /tmp/kb_a.log 2>&1 &
SPID_A=$!
(cd /tmp/kb_test_b && uv run --project /Users/oleksandr/Projects/fantastic_canvas \
  python /Users/oleksandr/Projects/fantastic_canvas/fantastic --port $PORT_B) \
  > /tmp/kb_b.log 2>&1 &
SPID_B=$!
for i in $(seq 1 30); do
  grep -q "kernel up" /tmp/kb_a.log && grep -q "kernel up" /tmp/kb_b.log && break
  sleep 0.3
done

# This helper opens a one-shot WS against the given port, sends a `call`
# frame, prints reply. First arg is the port to dial.
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

# Create bridge B (server-side) on kernel B; peer_id placeholder.
B_ID=$(call_at $PORT_B core '{"type":"create_agent","handler_module":"kernel_bridge.tools","transport":"ws","peer_id":"placeholder"}' \
  | python3 -c "import json,sys; print(json.load(sys.stdin)['id'])")

# Create bridge A on kernel A pointing at kernel B's WS endpoint.
A_ID=$(call_at $PORT_A core "{\"type\":\"create_agent\",\"handler_module\":\"kernel_bridge.tools\",\"transport\":\"ws\",\"peer_id\":\"$B_ID\",\"host\":\"127.0.0.1\",\"local_port\":$PORT_B}" \
  | python3 -c "import json,sys; print(json.load(sys.stdin)['id'])")

# Boot A's bridge (opens WS to kernel B).
call_at $PORT_A $A_ID '{"type":"boot"}'

# Forward a reflect on kernel B's core through the bridge.
call_at $PORT_A $A_ID '{"type":"forward","target":"core","payload":{"type":"reflect"}}' \
  | python3 -m json.tool | head -20

# Cleanup — cascade-delete fires the on_delete hook which cancels
# the read loop, closes the transport, kills the tunnel, rejects
# pending Futures.
call_at $PORT_A core "{\"type\":\"delete_agent\",\"id\":\"$A_ID\"}" >/dev/null
kill -9 $SPID_A $SPID_B
rm -rf /tmp/kb_test_a /tmp/kb_test_b /tmp/kb_a.log /tmp/kb_b.log
```
Expected: the forward returns kernel B's `core.reflect` body —
`{id:'core', sentence:'Core agent...', verbs:{...}, ...}`.
Regression signal: `error: not connected` after boot → ws connect
failed (check kernel B's log + that `peer_id` matches B's bridge id);
hang on forward → corr_id mismatch in the read_loop dispatch.

## Test 3 — SSH+WS transport against a real remote (manual)

Requires a remote host with fantastic installed and SSH key access:
`ssh <host>` works without password.

```bash
HOST=gpu-box                    # your real host alias
REMOTE_PORT=8888
LOCAL_PORT=49001

# 1. On the REMOTE host (one-time): install fantastic, run serve
ssh $HOST 'cd ~/fantastic_target_proj && uv run --project ~/fantastic_canvas fantastic --port 8888 >/tmp/remote-serve.log 2>&1 &'

# Assumes a local `fantastic` running on $PORT_LOCAL (set below).
PORT_LOCAL=8888

# 2. On the REMOTE: create a bridge that will be the WS server-side.
#    Verbs are invoked over WS — use a remote python one-liner.
B_ID=$(ssh $HOST "REMOTE_PORT=$REMOTE_PORT python3 - <<'PY'
import asyncio, json, os, websockets
port = os.environ['REMOTE_PORT']
async def main():
    async with websockets.connect(f'ws://localhost:{port}/core/ws') as ws:
        await ws.send(json.dumps({'type':'call','target':'core',
            'payload':{'type':'create_agent','handler_module':'kernel_bridge.tools',
                       'transport':'ws','peer_id':'placeholder'},'id':'1'}))
        while True:
            m = json.loads(await ws.recv())
            if m.get('id')=='1' and m.get('type') in ('reply','error'):
                print(m['data']['id']); return
asyncio.run(main())
PY")
echo "remote bridge: $B_ID"

# 3. LOCAL: create a bridge with transport=ssh+ws, host, local_port,
#    remote_port, peer_id
A_ID=$(call_at $PORT_LOCAL core "{\"type\":\"create_agent\",\"handler_module\":\"kernel_bridge.tools\",\"transport\":\"ssh+ws\",\"host\":\"$HOST\",\"local_port\":$LOCAL_PORT,\"remote_port\":$REMOTE_PORT,\"peer_id\":\"$B_ID\"}" \
  | python3 -c 'import json,sys;print(json.load(sys.stdin)["id"])')

# 4. Boot — opens SSH tunnel + WS over it
call_at $PORT_LOCAL $A_ID '{"type":"boot"}' | python3 -m json.tool
# Expect: {"booted":true, "transport":"ssh+ws", "tunnel_pid":<int>}

# 5. Forward a reflect
call_at $PORT_LOCAL $A_ID '{"type":"forward","target":"core","payload":{"type":"reflect"}}' | python3 -m json.tool
# Expect: remote core.reflect body
```
Expected: `tunnel_pid` non-null in boot reply; forward returns the
remote core's reflect.
Regression signal: `tunnel failed: …` → SSH key or host lookup
problem (try `ssh $HOST 'echo ok'` first); `tunnel_pid: null` after
boot → ServerAliveCountMax tripped early (network flaky); reflect
returns kernel B's bridge id instead of `core` → peer_id
mis-configured.

## Summary

| # | Test | Pass |
|---|------|------|
| 1 | MemoryTransport unit suite (10 tests, in-process) | |
| 2 (manual) | WS transport between two local serves | |
| 3 (manual) | SSH+WS transport against a real remote | |
