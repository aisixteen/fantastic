# fantastic-ssh-runner selftest

> scopes: kernel, ssh
> requires: `cargo build --release --bin fantastic`. Test 4 needs a
> remote host with `fantastic` installed + passwordless `ssh <host>`.
> out-of-scope: cross-kernel messaging (kernel_bridge), the remote
> kernel's own webapp (web selftest).

Remote `fantastic --port N` lifecycle over SSH. One agent = one
project on one host. Verbs `boot`/`start`/`stop`/`restart`/`status`
exec `ssh` as a subprocess; `get_webapp` iframes the remote through a
local SSH tunnel. Pure subprocess ssh — keys, ssh-agent, and
`~/.ssh/config` all apply.

Stateful: a live `start` spins up a remote daemon AND a local
`ssh -L` tunnel held in this process's RAM. Tests 1–3 drive the
**offline** surface (record fields, idempotent stop, cascade) with no
remote. Test 4 is the live path — like the web/terminal bundles it
needs a real daemon + a reachable host, so it is **manual**.

## Pre-flight

All test state lives under `/tmp/sr_test/`.

```bash
rm -rf /tmp/sr_test && mkdir -p /tmp/sr_test && cd /tmp/sr_test
FANTASTIC=/path/to/rust/target/release/fantastic
SR=$($FANTASTIC core create_agent handler_module=ssh_runner.tools \
  host=nohost remote_path=/tmp/x remote_cmd=/bin/fantastic \
  remote_port=8888 local_port=49001 | jq -r .id)
```

## Tests

### Test 1: reflect surfaces every record field + dead tunnel

```bash
$FANTASTIC $SR reflect | jq -e '
  .host == "nohost" and .remote_port == 8888 and
  .local_port == 49001 and .tunnel_alive == false'
```

### Test 2: get_webapp builds a localhost tunnel URL

```bash
$FANTASTIC $SR get_webapp | jq -e '.url == "http://localhost:49001/"'
# local_port is mandatory — an agent without it errors instead.
NP=$($FANTASTIC core create_agent handler_module=ssh_runner.tools | jq -r .id)
$FANTASTIC $NP get_webapp | jq -e '.error | contains("local_port")'
```

### Test 3: stop is idempotent with no tunnel + unreachable host

```bash
# host=nohost → ssh can't reach it, but stop swallows that and still
# reports stopped:true (tunnel was never open, remote_pid unknown).
$FANTASTIC $SR stop | jq -e '.stopped == true and .remote_pid == null'
```

### Test 4: start/status/restart/stop against a real host (manual)

Prereqs: `ssh <host> 'echo ok'` works without a password, and
`fantastic` is installed at `remote_cmd` on `<host>`. This is the only
test that brings up the live daemon + tunnel.

```bash
HOST=gpu-box                                  # your ssh-config alias
LR=$($FANTASTIC core create_agent handler_module=ssh_runner.tools \
  host=$HOST remote_path=/home/me/proj \
  remote_cmd=/home/me/.venv/bin/fantastic \
  remote_port=8888 local_port=49001 | jq -r .id)

$FANTASTIC $LR start  | jq -e '.started == true and (.remote_pid|type)=="number"'
$FANTASTIC $LR status | jq -e '.tunnel_alive and .remote_alive'
curl -sf http://localhost:49001/ | grep -q fantastic    # tunnel reaches remote web
$FANTASTIC $LR restart | jq -e '.started == true'
$FANTASTIC $LR stop   | jq -e '.stopped == true'
$FANTASTIC $LR status | jq -e '.tunnel_alive == false and .remote_alive == false'
```

Regression signals:
- `start` errors `ssh failed (rc=255)` → key/auth not set up; run
  `ssh $HOST 'echo ok'` first.
- `start` errors `remote serve did not write lock.json in time` →
  remote `fantastic` failed to boot; `ssh $HOST cat <remote_path>/.fantastic/serve.log`.
- `start.tunnel_pid` set but `status.tunnel_alive=false` → tunnel died
  early; remote crashed after writing lock.json, or `remote_port`
  mismatch.

## Cleanup

```bash
$FANTASTIC core delete_agent id=$SR     # on_delete stops tunnel + remote
$FANTASTIC core delete_agent id=$NP
test ! -d /tmp/sr_test/.fantastic/agents/$SR && echo OK
rm -rf /tmp/sr_test
```

## Summary table

| Test | Pass / Fail | Notes |
|---|---|---|
| 1. reflect fields + dead tunnel |  |  |
| 2. get_webapp URL + missing local_port |  |  |
| 3. stop idempotent, host unreachable |  |  |
| 4. live start/status/restart/stop | manual | needs ssh + real host |
