# ssh_runner — remote `fantastic` lifecycle over SSH

Thin Transport + Bundle over `fantastic-runner-core`: this crate
supplies only the `SshTransport` (ssh exec + `ssh -L` tunnel) and
a thin `SshRunnerBundle`. The lifecycle verb dispatch (boot=null,
restart=stop+start, unknown-verb error) lives in
`fantastic-runner-core`.

Each agent represents one project on one remote host. Verbs exec
`ssh` as a subprocess to control the remote kernel and maintain a
local SSH tunnel that exposes the remote daemon's HTTP port on a
local address, reachable at `http://localhost:<local_port>/`.

Pure subprocess SSH (no paramiko / russh). Authentication is whatever
`ssh <host>` works as in the user's shell — keys, ssh-agent, and
`~/.ssh/config` all apply transparently.

**Record fields** (set on `create_agent`):

| key            | purpose                                                                  |
|----------------|--------------------------------------------------------------------------|
| `host`         | ssh alias / hostname (passed to `ssh <host>`)                            |
| `remote_path`  | project root on the remote box                                           |
| `remote_cmd`   | absolute path to the remote `fantastic` CLI                              |
| `remote_port`  | port the remote daemon binds (REQUIRED, no default)                      |
| `local_port`   | local port the SSH tunnel forwards from (used by `get_webapp`)           |
| `entry_path`   | URL suffix appended to the local tunnel for `get_webapp`                 |

**Verbs**:

| verb         | args  | reply                                                              |
|--------------|-------|--------------------------------------------------------------------|
| `reflect`    | none  | `{id, sentence, host, remote_path, remote_cmd, remote_port, local_port, entry_path, tunnel_pid, tunnel_alive, verbs}` |
| `boot`       | none  | `null` — no auto-start; explicit `start` keeps remote control intentional |
| `start`      | none  | `{started, remote_pid, tunnel_pid}` or `{error}`                   |
| `stop`       | none  | `{stopped, remote_pid}` — kills local tunnel + remote pid via lock.json |
| `restart`    | none  | `stop` then `start`                                                |
| `status`     | none  | `{tunnel_alive, remote_alive, remote_pid}`                         |
| `get_webapp` | none  | `{url, default_width, default_height, title}` when `local_port` is set, else `{error}` |
