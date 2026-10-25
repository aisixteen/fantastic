> The machine's basics as agents (Linux, `fantastic [canvas] system`): `power` so far — battery and lid over UPower, power off / reboot / suspend / hibernate over logind — and the system canvas fantastic OS runs them in.

# The system agents

`fantastic [canvas] system` adds the machine's agents under `user` if they are
missing (and resets their views on every start that names it). An AI calls them
like any other agent: `reflect`, `state`, then their verbs. Each has an html
view, a canvas cell: add one from the canvas's action popup, a tile per agent
(`power`).

The word is refused off Linux: "`system` needs Linux with D-Bus: the system
agents are fantastic OS's". Alone (`fantastic system`) the terminal UI opens on
the first of them.

Every agent is a pure model and ViewModel (`fantastic-system`) driven against
an OS port: all OS calls live in the bundle's `sys/linux`. When its service is
missing the agent still exists: `reflect` and `state` say `available: false`
with a `why`, and its other verbs are refused with the reason `unavailable`.
A refusal is `{error, reason}`; the reasons here are `denied` (the OS asked for
authentication), `unsupported` (the machine cannot), `unavailable` (no
service), `failed`.

Every agent emits `{type: "state", state}` to its watchers after each change of
its state: at most one per 100 ms, the latest.

## power (`power.tools`, id `power`)

OS needs: UPower (`org.freedesktop.UPower`) and logind
(`org.freedesktop.login1`) on the system bus. Without a battery `battery` is
`null`; without a lid, `lid_closed` is `null`.

```json
{"available": true, "on_battery": false,
 "battery": {"percent": 81.0, "state": "discharging", "time_to_empty_s": 9120, "time_to_full_s": 0, "warning": "none"},
 "lid_closed": null,
 "can": {"power_off": "yes", "reboot": "yes", "suspend": "yes", "hibernate": "na"},
 "sleeping": false}
```

- `battery.state`: `unknown`, `charging`, `discharging`, `empty`, `fully_charged`, `pending_charge`, `pending_discharge`.
- `battery.warning`: `unknown`, `none`, `discharging`, `low`, `critical`, `action`.
- `can.*`: logind's `yes`, `no`, `challenge`, `na`; read at start, after every wake from sleep and every 30 s.

Verbs: `reflect`, `state`, `power_off`, `reboot`, `suspend`, `hibernate`. A power
verb is refused unless its `can` is `yes`: `challenge` is `denied` ("needs
authentication — is the polkit rule installed?"), `no` and `na` are
`unsupported`. The reply of a verb is the state. Calls are never interactive:
there is no polkit agent, so on fantastic OS a rule grants them (below).

## The system canvas on fantastic OS

On the kiosk the agents live in one ordinary fantastic folder,
`/home/fantastic/system`, whose kernel is a systemd user unit,
`fantastic-system.service`, running `fantastic canvas system`:

- It starts at boot, before anyone touches the screen, and systemd restarts it
  (`Restart=always`, 2 s). It is in no logind session.
- `Type=notify`: the CLI sends `READY=1` to `$NOTIFY_SOCKET` once the canvas
  serves and its MCP socket listens (a failed send prints
  `fantastic: NOTIFY_SOCKET: <error>` on stderr; unset, nothing).
- Its environment is in the unit: `DBUS_SESSION_BUS_ADDRESS=unix:path=%t/bus`,
  `WAYLAND_DISPLAY=wayland-0`, `XDG_RUNTIME_DIR` from the user manager.
- `ExecStartPost` lists it in the Home (`register_home_folder`).
- `/usr/share/polkit-1/rules.d/50-fantastic-session.rules` lets the user
  `fantastic` power off, reboot, suspend and hibernate, and use NetworkManager,
  with no password prompt, whatever the state of the kiosk session.
- `fantastic-session`'s `postinst` creates the folder (tmpfiles) and enables
  the unit for every user manager; the unit runs only for the user `fantastic`.

The Home lists it as `system`, with the canvas's URL; its page holds the cells.
The Home's actions on this row:

- `stop` is a restart: SIGTERM ends it and systemd starts it again 2 s later.
- `remove` lasts until the unit next starts. To drop it for good:
  `systemctl --user disable --now fantastic-system`, then `remove`.
- `start` is not for it: in those 2 s it would make a second host in the
  kiosk's session, holding the folder while systemd retries.

## Export a subtree

`user`'s verb `export_subtree {agent_id, path}` writes an agent and every agent
under it as a blueprint folder at `path` (relative to the running dir; a new
or empty folder; no `..`):

- `blueprint.yaml` in the shape of `src/tui/blueprint.yaml`: nesting under
  `agents:` is the parent (no `parent_id`), children in creation order, grants
  by id on both ends, `@root` for the root. Ids stay the readable names; the
  importer makes fresh ones.
- the owned files beside it: an agent's `system_prompt_file` (relative and
  readable) as `<agent id>/directive.md`, the record pointing there.

The reply is `{exported, path, ports, files}`. Files are written `0600`,
folders `0700`, into a temp folder that is renamed into place.

A reference that points OUTSIDE the subtree (a grant key in `can_call` /
`callable_by`, `memory_store_id`, `file_bridge_id`, `backend_id`,
`credentials.store`) becomes a PORT, listed under `ports:` with its `kind`,
`why` and `used_by`:

| port | kind | for |
|---|---|---|
| `@memory` | memory | a memory store (`memory_store_id`, a `*_store.tools`) |
| `@credentials` | credentials | the key store (`credential_store.tools`) |
| `@exec` | exec | an exec agent (`exec_bridge.tools`) |
| `@files` | files | a file bridge (`file_bridge.tools`) |
| `@model` | model | a chat model (`*_backend.tools`, `backend_id`) |
| `@connector:<id>` | connector | anything else |
| `@path:<field>` | path | an absolute path outside the subtree's folder |

Two outside agents of one kind are `@memory`, `@memory_2`…, numbered by
outside id. `$self`, `$parent`, `$subtree`, `*` and page grants
(`<guid>|<id>`) are written as they are.

Stripped: runtime state (`guid`, `pid`, `version`, `rows`, `history`, `cache`,
`usage`, `ts`, `created_at`, `updated_at`, `error`, `last_error`, any `last_*`,
`*_error`, `*_at`, `*_ts`), every `api_key`, `token`, `secret` and `password`
(at any depth; `credentials.ref` stays), and absolute paths (relative to the
subtree's folder when under it, else a port). Only records are read, never
rows. A secrets scan runs over the written folder; a hit deletes it and the
export is refused (`secret_found`).

Not yet: the canvas cell menu "Export…", a TUI command, the verb on the
kernel's root, the import (the next issue), and owned files beyond the
directive (an html view's body, a starter's `bundle.yaml`, code and panels).
