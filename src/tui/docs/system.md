> The machine's basics as agents (Linux, `fantastic [canvas] system`): `power` and `audio` so far — battery and lid over UPower, power off / reboot / suspend / hibernate over logind; PipeWire's sinks and sources, their volume, mute and default, over `pw-dump` and `wpctl` — and the system canvas fantastic OS runs them in.

# The system agents

`fantastic [canvas] system` adds the machine's agents under `user` if they are
missing (and resets their views on every start that names it). An AI calls them
like any other agent: `reflect`, `state`, then their verbs. Each has an html
view, a canvas cell: add one from the canvas's action popup, a tile per agent
(`power`, `audio`).

The word is refused off Linux: "`system` needs Linux with D-Bus: the system
agents are fantastic OS's". Alone (`fantastic system`) the terminal UI opens on
the first of them.

Every agent is a pure model and ViewModel (`fantastic-system`) driven against
an OS port: all OS calls live in the bundle's `sys/linux`. When its service is
missing the agent still exists: `reflect` and `state` say `available: false`
with a `why`, and its other verbs are refused with the reason `unavailable`.
A refusal is `{error, reason}`; the reasons here are `denied` (the OS asked for
authentication), `unsupported` (the machine cannot), `unavailable` (no
service), `failed`, and for a verb's arguments `bad_arg` and `not_found`.

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

## audio (`audio.tools`, id `audio`)

OS needs: PipeWire with WirePlumber in the user's session, its socket at
`$XDG_RUNTIME_DIR/pipewire-0`, and its tools on `$PATH`: `pw-dump` (reads every
object as JSON; `pw-dump --monitor` follows the changes) and `wpctl` (acts).
Without `wpctl`, `why` is "wpctl is not installed"; without `pw-dump`, "pw-dump
is not installed"; a PipeWire that does not answer is `why` with `pw-dump`'s
error.

```json
{"available": true,
 "sinks": [{"id": 52, "name": "alsa_output.pci-0000_00_1b.0.analog-stereo",
            "description": "Built-in Audio Analog Stereo", "volume": 0.4, "muted": false, "default": true}],
 "sources": [{"id": 54, "name": "alsa_input.pci-0000_00_1b.0.analog-stereo",
              "description": "alsa_input.pci-0000_00_1b.0.analog-stereo", "volume": 0.5, "muted": false, "default": true}]}
```

- `sinks` are the `Audio/Sink` nodes, `sources` the `Audio/Source` ones, each by
  `id` (PipeWire's object id, what `wpctl` takes). Other nodes are not listed.
- `description` is `node.description`, else `node.nick`, else `name`
  (`node.name`).
- `volume` is what `wpctl get-volume` shows: the cube root of the mean of the
  node's channel volumes, two places; 1.0 is 100 %.
- `default` is the node the `default` metadata names (`default.audio.sink`,
  `default.audio.source`).
- The state follows `pw-dump --monitor`: a change made elsewhere (another app,
  `wpctl` in a terminal) reaches the watchers too.

Verbs: `reflect`, `state`, then

- `set_volume {volume, id?}`: `wpctl set-volume <id> <volume> -l 1.5`. The
  volume is clamped to 0.0..1.5. Without `id`, the default sink. Refused:
  `bad_arg` (no number), `not_found` (no such sink or source, or no default
  sink).
- `set_mute {muted, id?}`: `wpctl set-mute <id> 1|0`. Without `id`, the default
  sink. Refused: `bad_arg` (no `muted`), `not_found`.
- `set_default {id}`: `wpctl set-default <id>`. Refused: `bad_arg` (no `id`),
  `not_found`.

Any of them is `unavailable` while the state is (`<verb>: <why>`), and `failed`
with `wpctl`'s error when it exits non-zero. The reply of a verb is the state.

The parser's tests read a hand-written `pw-dump` fixture
(`crates/fantastic-system/tests/fixtures/pw_dump.json`) in pw-dump's shape,
until one recorded in the fantastic OS guest replaces it.

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

The verb `export_subtree {agent_id, path}` (on `user` and on the kernel's
root, the same engine and the same reply) writes an agent and every agent
under it as a blueprint folder at `path` (relative to the running dir; a new
or empty folder; no `..`). In the TUI, `/export <path>` exports the selected
agent; on the canvas, a cell's right-click menu has "Export…":

- `blueprint.yaml` in the shape of `src/tui/blueprint.yaml`: nesting under
  `agents:` is the parent (no `parent_id`), children in creation order, grants
  by id on both ends, `@root` for the root. Ids stay the readable names; the
  importer makes fresh ones.
- the owned files beside it: an agent's `system_prompt_file` (relative and
  readable) as `<agent id>/directive.md`, the record pointing there; an html
  view's body (`view.bytes`) as `<agent id>/view.html`, the record keeping
  `view.bytes_file` in its place (`kind` and `calls` stay); and, for an agent
  that runs on a FOLDER BUNDLE, the plain top-level files of
  `.fantastic/bundles/<handler_module>/` (`bundle.yaml`, `agent.py`,
  `panel.html`…) as `bundles/<handler_module>/<name>`, once per bundle.
  `<agent id>/view.html` and `bundles/…` are CODE: the blueprint's header
  lists each as `# code: <path>`, and the importer takes them only with
  `allow_code`. A symlink, a hidden file, a file that is not UTF-8 text or is
  over 1 MiB is left out and noted in the header.

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

Not yet: a frontend agent's top-level `html` field stays inline in the
blueprint. The import is below.

## Import a subtree

`user`'s verb `import_subtree {path, parent_id, bind?, allow_code?, approve?}`
reads the blueprint folder at `path` (relative to the running dir; no `..`;
plain files only, no symlinks) and makes its agents under `parent_id`.
Everything is checked before anything is created:

- the shape: one top entry under `agents:`, nesting is the parent, a
  `parent_id` in an entry is refused (`bad_arg`);
- NO SECRETS (`secret_found`): an `api_key`, `token`, `secret` or `password`
  key at any depth, `rows` or `history`, an `sk-` token in any file, or an
  absolute path. The message names the agent and the key, never the value;
  `credentials.ref` is a reference and passes;
- PORTS: every `@…` name used must be declared under `ports:`, and every
  declared port bound with `bind: {"@memory": "<agent id>", …}` to an
  existing agent of the port's kind (a `@connector:<id>` takes any agent).
  Every unbound port is named at once (`unbound_ports`); a bind of the wrong
  kind, of no agent, or of a name that is no port is `bad_binding`.
  `@path:*` ports are not bindable yet;
- CODE (`code_not_allowed`): a handler that is not a registered bundle (a
  starter, `py_<hex>`…) or a file other than `blueprint.yaml` and
  `<agent id>/directive.md`. `allow_code: true` lets it in.

FRESH IDS: every id gets the same suffix `_<k>`, the smallest `k` for which
none of the new ids is taken, so a second import under the same parent gives
`a_2`, `b_2`… beside `a_1`, `b_1`…. Grants inside the blueprint are re-wired
to the new ids, `@root` becomes the root's id, a port becomes the bound
agent; `$self`, `$parent`, `$subtree`, `*` and page grants stay. A name that
is neither in the blueprint nor a port is refused.

APPROVAL: without `approve: true` (only the literal counts, as for
`allow_code`) nothing is created and the reply is the preview:
`{approval_required, agents, new_ids, reaches: [{id, from, can_call,
callable_by, ports}], ports: {port: agent id}, code}`. With it the agents are
created depth first and the other end of every bound edge is written on the
bound agent (its `callable_by` for a `can_call` to the port, its `can_call`
for a `callable_by`), so the copy reaches exactly what it was granted and what
its ports were bound to. A failure deletes what was made (`failed`). The
reply is `{imported, ids, parent_id, ports}`.

From a shell: `fantastic import <path> --parent <agent id> [--bind
@port=agent id]… [--allow-code] [--approve]` calls the verb in the running
kernel (attach-only, like `send`). It prints the preview (new ids, what each
reaches, the ports, the code) and creates nothing; on a terminal it then asks
`approve? [y/N]`, and `--approve` sends the approved call at once. Exit 0 for a
preview or an import, 1 for a refusal (`refused (<reason>): <message>`), 2 for
a bad command line. On the canvas, dropping a folder that holds
`blueprint.yaml` onto a cell (the parent) or the canvas (the canvas's host
node) shows the same preview with Approve/Cancel (code needs its own "allow
code" tick) instead of opening the folder.

Not yet: the verb on the kernel's root, `@path:*` ports, and owned files
beyond the directive (a directive keeps its relative `system_prompt_file`; the
file is not written into the new agent's folder).
