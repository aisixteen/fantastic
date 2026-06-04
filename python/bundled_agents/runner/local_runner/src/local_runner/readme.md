# local_runner — local sub-fantastic lifecycle
Manages a `fantastic` daemon for one project directory on this machine. Each agent represents one project. Truth read from the project's `.fantastic/lock.json` (PID) and its web agent record (port).

## Implementation
This bundle is a thin transport over the shared `runner_core` lib. `LocalTransport` implements the filesystem/subprocess seam (lock.json reads, free-port allocation, `subprocess.Popen` spawn, SIGTERM/SIGKILL stop); the shared lifecycle bodies (`reflect`, `boot`, `start`, `stop`, `status`, `get_webapp`) live in `runner_core.core`. Each verb handler builds a `LocalTransport` from the agent record per-call and delegates to core.

## Verbs
- `reflect` — identity + every record field + live status (running, pid, port). No args.
- `boot` — no-op. local_runner does NOT auto-start on kernel reboot; `start` is always explicit so a kernel restart does not unintentionally boot every registered project.
- `start` — picks a free port, ensures a web agent record exists at that port in `<remote_path>/.fantastic/`, then spawns `<remote_cmd>` as a detached subprocess. Polls `.fantastic/lock.json` until `{pid}` appears and the web record has a port (max ~30s). Returns `{started, pid, port}` on success or `{error, requested_port}` on failure (tail `<remote_path>/.fantastic/serve.log`).
- `stop` — SIGTERMs the pid from `.fantastic/lock.json`, polls until the process exits (max 6s; escalates to SIGKILL), then removes the stale lock file. Idempotent — missing lock or already-dead pid returns ok.
- `restart` — stop + start. Returns the start reply.
- `status` — `{running, pid, port, ws_ok}`. `ws_ok` is a 2s WS probe (reflect frame → reply) proving the kernel is alive and answering, not just that lock.json exists.
- `get_webapp` — `{url, default_width, default_height, title}` when the project is running, `{error}` when not. The url points at `http://localhost:<port>/<entry_path>`.
- `shutdown` — alias for `stop`; called automatically by `fs_loader.delete_agent`'s universal lifecycle hook when the agent record is deleted.

## Record fields
- `remote_path` — project root (absolute filesystem path; required)
- `remote_cmd` — `fantastic` CLI to invoke (default: `"fantastic"` from PATH)
- `entry_path` — URL suffix appended to the live serve URL for `get_webapp` (e.g. `"<canvas_id>/"` to land the viewer directly on the project's canvas)
