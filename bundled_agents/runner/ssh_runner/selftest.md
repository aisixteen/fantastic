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
pkill -9 -f "kernel.py serve" 2>/dev/null
mkdir -p /tmp/sr_test && cd /tmp/sr_test
uv run --project /Users/oleksandr/Projects/fantastic_canvas \
  python /Users/oleksandr/Projects/fantastic_canvas/kernel.py serve --port $PORT \
  > /tmp/sr.log 2>&1 &
SPID=$!
for i in $(seq 1 30); do grep -q "kernel up" /tmp/sr.log && break; sleep 0.3; done

call() { curl -s -X POST "http://localhost:$PORT/$1/call" -H 'content-type: application/json' -d "$2"; }

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

# Start — SSH brings up `fantastic serve` on remote, opens tunnel
call $RID '{"type":"start"}' | python3 -m json.tool
# Expect: {"started": true, "remote_pid": <int>, "tunnel_pid": <int>}

# Status — tunnel_alive + remote_alive + http_ok all true
sleep 1
call $RID '{"type":"status"}' | python3 -m json.tool

# Probe the remote kernel through the tunnel
curl -s http://localhost:$LOCAL_PORT/_kernel/reflect | python3 -m json.tool | head -10

# get_webapp — canvas iframes the remote
call $RID '{"type":"get_webapp"}' | python3 -m json.tool

# Restart
call $RID '{"type":"restart"}' | python3 -m json.tool | head

# Stop — kills tunnel + remote serve
call $RID '{"type":"stop"}' | python3 -m json.tool

# Cleanup
call core "{\"type\":\"delete_agent\",\"id\":\"$RID\"}"  # universal shutdown hook fires stop
kill -9 $SPID
rm -rf /tmp/sr_test /tmp/sr.log
```
Expected:
- `start`: `{started: true, remote_pid: <int>, tunnel_pid: <int>}`
- `status` after start: all four flags true (`tunnel_alive`,
  `remote_alive`, `http_ok`, `remote_pid`)
- `curl` to `localhost:$LOCAL_PORT/_kernel/reflect` returns the
  remote kernel's primer
- `stop`: tunnel + remote process gone (`status` flags all false)

Regression signals:
- `start` errors `ssh failed (rc=255)` → SSH key/auth not set up.
  Run `ssh $HOST 'echo ok'` first.
- `start` errors `remote serve did not write lock.json in time`
  → remote `fantastic serve` failed; ssh `cat <remote_path>/.fantastic/serve.log`.
- `start.tunnel_pid` set but `status.http_ok=false` → tunnel up but
  remote not listening. Either the remote crashed after lock.json
  was written, or `remote_port` doesn't match `serve --port`.
- `delete_agent` doesn't kill the tunnel → universal `shutdown`
  lifecycle hook regressed.

## Summary

| # | Test | Pass |
|---|------|------|
| 1 | unit suite (11 tests, mocked SSH) | |
| 2 (manual) | start/stop/restart/status against a real host | |
