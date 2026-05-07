# kernel_bridge selftest

> scopes: kernel, ws, ssh
> requires: `uv sync`. The MemoryTransport spec is fully in-process
> (no network); the WS spec needs a running `fantastic serve`; the
> SSH+WS spec needs a real remote host with `fantastic` installed +
> SSH key access.
> out-of-scope: `ssh_runner` (separate bundle, separate selftest).

Pairs of bridge agents on two kernels exchange `forward` envelopes
over a transport (memory / WS / SSH+WS). A local agent that wants to
reach a remote one calls
`kernel.send(local_bridge_id, {type:'forward', target, payload})`
and gets the unwrapped reply back. The bridge speaks the existing
webapp/_proxy.py WS frame protocol — remote needs zero changes.

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

## Test 2 — WS transport against a live `fantastic serve` (manual)

Boots two kernels in the same machine on different ports, with
real WS over the existing webapp `/<id>/ws` proxy. No SSH.

```bash
PORT_A=18900
PORT_B=18901
pkill -9 -f "kernel.py serve" 2>/dev/null
mkdir -p /tmp/kb_test_a /tmp/kb_test_b

(cd /tmp/kb_test_a && uv run --project /Users/oleksandr/Projects/fantastic_canvas \
  python /Users/oleksandr/Projects/fantastic_canvas/kernel.py serve --port $PORT_A) \
  > /tmp/kb_a.log 2>&1 &
SPID_A=$!
(cd /tmp/kb_test_b && uv run --project /Users/oleksandr/Projects/fantastic_canvas \
  python /Users/oleksandr/Projects/fantastic_canvas/kernel.py serve --port $PORT_B) \
  > /tmp/kb_b.log 2>&1 &
SPID_B=$!
for i in $(seq 1 30); do
  grep -q "kernel up" /tmp/kb_a.log && grep -q "kernel up" /tmp/kb_b.log && break
  sleep 0.3
done

# Create bridge B (server-side) on kernel B; peer_id placeholder.
B_ID=$(curl -s -X POST http://localhost:$PORT_B/core/call -H 'content-type: application/json' \
  -d '{"type":"create_agent","handler_module":"kernel_bridge.tools","transport":"ws","peer_id":"placeholder"}' \
  | python3 -c "import json,sys; print(json.load(sys.stdin)['id'])")

# Create bridge A on kernel A pointing at kernel B's WS endpoint.
A_ID=$(curl -s -X POST http://localhost:$PORT_A/core/call -H 'content-type: application/json' \
  -d "{\"type\":\"create_agent\",\"handler_module\":\"kernel_bridge.tools\",\"transport\":\"ws\",\"peer_id\":\"$B_ID\",\"host\":\"127.0.0.1\",\"local_port\":$PORT_B}" \
  | python3 -c "import json,sys; print(json.load(sys.stdin)['id'])")

# Boot A's bridge (opens WS to kernel B).
curl -s -X POST http://localhost:$PORT_A/$A_ID/call -H 'content-type: application/json' \
  -d '{"type":"boot"}'

# Forward a reflect on kernel B's core through the bridge.
curl -s -X POST http://localhost:$PORT_A/$A_ID/call -H 'content-type: application/json' \
  -d '{"type":"forward","target":"core","payload":{"type":"reflect"}}' \
  | python3 -m json.tool | head -20

# Cleanup
curl -s -X POST http://localhost:$PORT_A/$A_ID/call -H 'content-type: application/json' \
  -d '{"type":"shutdown"}' >/dev/null
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
ssh $HOST 'cd ~/fantastic_target_proj && uv run --project ~/fantastic_canvas python kernel.py serve --port 8888 >/tmp/remote-serve.log 2>&1 &'

# 2. On the REMOTE: create a bridge that will be the WS server-side
B_ID=$(ssh $HOST "curl -s -X POST http://localhost:$REMOTE_PORT/core/call \
  -H 'content-type: application/json' \
  -d '{\"type\":\"create_agent\",\"handler_module\":\"kernel_bridge.tools\",\"transport\":\"ws\",\"peer_id\":\"placeholder\"}' \
  | python3 -c 'import json,sys;print(json.load(sys.stdin)[\"id\"])'")
echo "remote bridge: $B_ID"

# 3. LOCAL: create a bridge with transport=ssh+ws, host, local_port,
#    remote_port, peer_id
A_ID=$(curl -s -X POST http://localhost:8888/core/call -H 'content-type: application/json' \
  -d "{\"type\":\"create_agent\",\"handler_module\":\"kernel_bridge.tools\",\"transport\":\"ssh+ws\",\"host\":\"$HOST\",\"local_port\":$LOCAL_PORT,\"remote_port\":$REMOTE_PORT,\"peer_id\":\"$B_ID\"}" \
  | python3 -c 'import json,sys;print(json.load(sys.stdin)["id"])')

# 4. Boot — opens SSH tunnel + WS over it
curl -s -X POST http://localhost:8888/$A_ID/call -H 'content-type: application/json' \
  -d '{"type":"boot"}' | python3 -m json.tool
# Expect: {"booted":true, "transport":"ssh+ws", "tunnel_pid":<int>}

# 5. Forward a reflect
curl -s -X POST http://localhost:8888/$A_ID/call -H 'content-type: application/json' \
  -d '{"type":"forward","target":"core","payload":{"type":"reflect"}}' | python3 -m json.tool
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
