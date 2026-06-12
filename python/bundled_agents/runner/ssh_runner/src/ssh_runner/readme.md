# ssh_runner — remote fantastic lifecycle over SSH
Drives a `fantastic` daemon on a remote machine using subprocess SSH and opens a local `ssh -L` tunnel so the remote daemon's HTTP port is reachable locally. Each agent represents one remote project. Composes with `ws_bridge` for messaging.

## Implementation
This bundle is a thin transport over the shared `runner_core` lib. `SSHTransport` implements the ssh/tunnel seam (`ssh` exec for remote commands, `ssh -L` for the port-forward tunnel); the shared lifecycle bodies (`reflect`, `boot`, `start`, `stop`, `status`, `get_webapp`) live in `runner_core.core`. Each verb handler builds an `SSHTransport` from the agent record per-call and delegates to core.

Authentication is whatever `ssh <host>` resolves in the user's shell — keys, ssh-agent, and `~/.ssh/config` all apply transparently (pure subprocess SSH; no paramiko).

## Verbs
- `reflect` — identity + every record field + live tunnel status. No args.
- `boot` — no-op. ssh_runner does NOT auto-start on kernel reboot; `start` is always explicit so a kernel restart does not unintentionally bring up every remote.
- `start` — SSHs to `<host>`, runs `cd <remote_path> && nohup <remote_cmd> > .fantastic/serve.log 2>&1 &`, polls the remote `.fantastic/lock.json` until `{pid}` appears (max ~30s), then opens the local SSH tunnel `-L <local_port>:localhost:<remote_port>`. Returns `{started, remote_pid, tunnel_pid}` on success or `{error}` on failure.
- `stop` — kills the local SSH tunnel (TERM, 2s, KILL); SSHs to the host, reads the remote pid from `.fantastic/lock.json`, SIGTERMs it. Idempotent — missing tunnel or missing remote pid is OK.
- `restart` — stop + start. Returns the start reply.
- `status` — `{tunnel_alive, remote_alive, remote_pid, ws_ok}`. `ws_ok` is a 2s WS probe through the SSH tunnel (reflect frame → reply) proving end-to-end liveness.
- `get_webapp` — `{url, default_width, default_height, title}`. The url points at the LOCAL tunnel (`http://localhost:<local_port>/<entry_path>`) so the agent is reachable transparently.
- `shutdown` — alias for `stop`; called automatically by `kernel_state.delete_agent`'s universal lifecycle hook when the agent record is deleted.

## Record fields
- `host` — ssh alias or hostname (passed to `ssh <host>`)
- `remote_path` — project root on the remote machine
- `remote_cmd` — absolute path to the remote `fantastic` CLI (e.g. `/home/me/.venv/bin/fantastic`)
- `remote_port` — port the remote daemon binds (required; no default)
- `local_port` — local port the SSH tunnel forwards from; used by `get_webapp` for the url
- `entry_path` — URL suffix appended to the local tunnel url for `get_webapp`
