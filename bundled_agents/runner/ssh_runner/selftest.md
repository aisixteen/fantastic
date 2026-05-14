# ssh_runner selftest

> scopes: kernel, ssh
> requires: `uv sync`. Manual tests need a remote host with
> `fantastic` installed + SSH key access (passwordless).
> out-of-scope: cross-kernel comms (that's `kernel_bridge`).

Each `ssh_runner` agent is one project on one remote host. Verbs:
`start` / `stop` / `restart` / `status` over SSH-as-subprocess,
`get_webapp` for canvas iframing through a local SSH tunnel. No
paramiko — pure `ssh <host>` invocations, leveraging the user's
existing `~/.ssh/config` + agent.

## Test 1 — verb shapes + SSH command construction (in-process)

Covered by the unit suite. Mocks `_ssh_exec` + `_open_tunnel` and
asserts the bundle invokes them with correctly-shaped arguments
(quoted paths, ports, log redirection, lock.json polling).

```bash
uv run pytest bundled_agents/ssh_runner/tests/ -v
```
Expected: 11 tests pass.

## Test 2 — start/stop/status against a real remote (manual)

Prereqs:
- `ssh <host>` works without password (key, agent forward, or
  ssh-config alias).
- On `<host>`: fantastic installed somewhere reachable. Path will
  be the value of `remote_cmd`.

```bash
HOST=gpu-box                                         # your alias
REMOTE_PATH=/home/me/proj
REMOTE_CMD=/home/me/.venv/bin/fantastic
REMOTE_PORT=8888
LOCAL_PORT=49001

# Boot a local fantastic to call from
PORT=18901
pkill -9 -f "fantastic" 2>/dev/null
mkdir -p /tmp/sr_test && cd /tmp/sr_test
uv run --project /Users/oleksandr/Projects/fantastic_canvas \
  python /Users/oleksandr/Projects/fantastic_canvas/fantastic --port $PORT \
  > /tmp/sr.log 2>&1 &
SPID=$!
for i in $(seq 1 30); do grep -q "kernel up" /tmp/sr.log && break; sleep 0.3; done

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

# Provision the runner
RID=$(call core "{
  \"type\":\"create_agent\",
  \"handler_module\":\"ssh_runner.tools\",
  \"display_name\":\"$HOST\",
  \"host\":\"$HOST\",
  \"remote_path\":\"$REMOTE_PATH\",
  \"remote_cmd\":\"$REMOTE_CMD\",
  \"remote_port\":$REMOTE_PORT,
  \"local_port\":$LOCAL_PORT
}" | python3 -c "import json,sys;print(json.load(sys.stdin)['id'])")
echo "runner: $RID"

# Reflect — should surface the record fields, tunnel_alive=false
call $RID '{"type":"reflect"}' | python3 -m json.tool

# Start — SSH brings up `fantastic` on remote, opens tunnel
call $RID '{"type":"start"}' | python3 -m json.tool
# Expect: {"started": true, "remote_pid": <int>, "tunnel_pid": <int>}

# Status — tunnel_alive + remote_alive + ws_ok all true
sleep 1
call $RID '{"type":"status"}' | python3 -m json.tool

# Probe the remote kernel through the tunnel over WS
uv run --active python - "$LOCAL_PORT" <<'PY'
import asyncio, json, sys, websockets
port = sys.argv[1]
async def main():
  async with websockets.connect(f"ws://localhost:{port}/core/ws") as ws:
    await ws.send(json.dumps({"type":"call","target":"kernel","payload":{"type":"reflect"},"id":"1"}))
    while True:
      m = json.loads(await ws.recv())
      if m.get("id")=="1" and m.get("type")=="reply":
        print(json.dumps({k: m["data"].get(k) for k in ("agent_count","available_bundles")}, default=str)[:200])
        return
asyncio.run(main())
PY

# get_webapp — canvas iframes the remote
call $RID '{"type":"get_webapp"}' | python3 -m json.tool

# Restart
call $RID '{"type":"restart"}' | python3 -m json.tool | head

# Stop — kills tunnel + remote serve
call $RID '{"type":"stop"}' | python3 -m json.tool

# Cleanup — cascade-delete fires the on_delete hook which stops the
# tunnel + remote serve.
call core "{\"type\":\"delete_agent\",\"id\":\"$RID\"}"
kill -9 $SPID
rm -rf /tmp/sr_test /tmp/sr.log
```
Expected:
- `start`: `{started: true, remote_pid: <int>, tunnel_pid: <int>}`
- `status` after start: all four flags true (`tunnel_alive`,
  `remote_alive`, `ws_ok`, `remote_pid`)
- WS round-trip to `localhost:$LOCAL_PORT/core/ws` returns the
  remote kernel's primer
- `stop`: tunnel + remote process gone (`status` flags all false)

Regression signals:
- `start` errors `ssh failed (rc=255)` → SSH key/auth not set up.
  Run `ssh $HOST 'echo ok'` first.
- `start` errors `remote serve did not write lock.json in time`
  → remote `fantastic` failed; ssh `cat <remote_path>/.fantastic/serve.log`.
- `start.tunnel_pid` set but `status.ws_ok=false` → tunnel up but
  remote not listening. Either the remote crashed after lock.json
  was written, or `remote_port` doesn't match the persisted web
  agent's port.
- `delete_agent` doesn't kill the tunnel → `on_delete` cascade hook
  regressed.

## Summary

| # | Test | Pass |
|---|------|------|
| 1 | unit suite (11 tests, mocked SSH) | |
| 2 (manual) | start/stop/restart/status against a real host | |
