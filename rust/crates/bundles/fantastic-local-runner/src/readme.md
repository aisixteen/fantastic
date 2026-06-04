# local_runner — `fantastic` lifecycle for local projects

Thin Transport + Bundle over `fantastic-runner-core`: this crate
supplies only the `LocalTransport` (subprocess + filesystem lock +
OS signals) and a thin `LocalRunnerBundle`. The lifecycle verb
dispatch (boot=null, restart=stop+start, unknown-verb error) lives
in `fantastic-runner-core`.

Each agent represents one project on this machine. Verbs spawn /
signal a `fantastic` subprocess directly (no SSH, no tunnels).
Live status is read from two sibling files in the project's
`.fantastic/` dir:

- `lock.json` — `{pid:int}`, PID-only (substrate's lock).
- `agents/web_*/agent.json` — the web bundle's persisted record,
  which carries the port.

**Record fields** (set on `create_agent`):

| key            | purpose                                                                  |
|----------------|--------------------------------------------------------------------------|
| `remote_path`  | project root (absolute filesystem path)                                  |
| `remote_cmd`   | `fantastic` CLI to invoke (default: lookup via `FANTASTIC_BIN` / PATH)   |
| `entry_path`   | URL suffix appended to the live serve URL for `get_webapp`               |

**Fantastic binary resolution**:

1. `record.remote_cmd` (if set)
2. `record.fantastic_path` (if set; legacy)
3. `FANTASTIC_BIN` env var
4. `which fantastic`
5. Error

**Verbs**:

| verb         | args  | reply                                                         |
|--------------|-------|---------------------------------------------------------------|
| `reflect`    | none  | `{id, sentence, remote_path, remote_cmd, entry_path, running, pid, port, verbs}` |
| `boot`       | none  | `null` — no auto-start; explicit `start` keeps lifecycle intentional |
| `start`      | none  | `{started, pid, port}` or `{error, requested_port?}`          |
| `stop`       | none  | `{stopped, pid, died_cleanly?}` — SIGTERM then SIGKILL after 6s |
| `restart`    | none  | `stop` then `start`                                           |
| `status`     | none  | `{running, pid, port}`                                        |
| `get_webapp` | none  | `{url, default_width, default_height, title}` when running, else `{error}` |
