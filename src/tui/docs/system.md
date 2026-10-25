> The machine's basics as agents (Linux, `fantastic [canvas] system`): `power`, `network`, `audio`, `displays`, `notifications` and `lock` — battery and power controls over UPower and logind; NetworkManager's devices, Wi-Fi and saved connections; PipeWire's sinks and sources over `pw-dump` and `wpctl`; the compositor's outputs over `wlr-randr` and the backlight over logind; the session's notification server; the kiosk's PIN lock screen — and the system canvas fantastic OS runs them in.

# The system agents

`fantastic [canvas] system` adds the machine's agents under `user` if they are
missing (and resets their views on every start that names it). An AI calls them
like any other agent: `reflect`, `state`, then their verbs. Each has an html
view, a canvas cell: add one from the canvas's action popup, a tile per agent
(`power`, `network`, `audio`, `displays`, `notifications`, `lock`; `network`
opens 640x480, the others 380x420). All of them except `lock` are open to an
AI like any other agent; `lock` answers `user` alone.

The word is refused off Linux: "`system` needs Linux with D-Bus: the system
agents are fantastic OS's". Alone (`fantastic system`) the terminal UI opens on
`network`.

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

## network (`network.tools`, id `network`)

OS needs: NetworkManager (`org.freedesktop.NetworkManager`) on the system bus.
On fantastic OS the polkit rule lets the user `fantastic` use it (below).

```json
{"available": true,
 "state": "connected_global", "connectivity": "full",
 "wireless": {"enabled": true, "hardware_enabled": true},
 "devices": [{"iface": "enp0s2", "kind": "ethernet", "state": "activated",
              "connection": "Wired connection 1", "ip4": ["10.0.2.15/24"]}],
 "access_points": [{"ssid": "home", "strength": 72, "security": "wpa-psk",
                    "frequency_mhz": 2437, "active": false, "known": true}],
 "known": [{"uuid": "...", "id": "home", "kind": "wifi", "autoconnect": true}],
 "busy": null}
```

- `state` names NetworkManager's `NMState` (`unknown`, `asleep`, `disconnected`,
  `disconnecting`, `connecting`, `connected_local`, `connected_site`,
  `connected_global`); `connectivity`, `NMConnectivityState` (`unknown`,
  `none`, `portal`, `limited`, `full`).
- A device's `state` names `NMDeviceState` (`unmanaged` ... `activated`,
  `deactivating`, `failed`); its `kind` is `ethernet`, `wifi` or `other`. The
  loopback is not listed.
- `access_points` come from every Wi-Fi device, one per SSID (the strongest
  kept), strongest first. An SSID that is not UTF-8 is shown as `0x<hex>`;
  `connect` takes it by that name. A hidden network (no SSID) is not listed.
  `security` is decided from the access point's flags: `sae` (WPA3), `enterprise`
  (802.1X), `wpa-psk`, `wep`, else `open`. `known` is true when a saved Wi-Fi
  connection has that SSID.
- The state follows NetworkManager's signals, and is read again every 30 s.

Verbs: `reflect`, `state`, then

- `scan`: asks every Wi-Fi device to scan. Refused: `unavailable` (wireless
  off), `not_found` (no Wi-Fi device).
- `connect {ssid, password?}`: a saved Wi-Fi connection with that SSID is
  activated; otherwise a new one is made (`AddAndActivateConnection`, owned by
  the calling user: `permissions: ["user:<user>"]`). Refused: `bad_arg` (no
  `ssid`, no `password` for a protected network, a WPA password not 8 to 63
  characters or 64 hex digits), `not_found`, `unsupported` (`enterprise` and
  `wep` networks), `unavailable` (wireless off). The password is never kept: not
  in the state, the record or a log line.
- `activate {uuid}`: a saved connection on the first device of its kind.
  Refused: `bad_arg`, `not_found`.
- `deactivate {iface}`: the device's active connection. Refused: `bad_arg`,
  `not_found` (no such device, or nothing active on it).
- `forget {uuid}`: deletes the saved connection. Refused: `bad_arg`,
  `not_found`.
- `set_wireless {enabled}`: the Wi-Fi radio. Refused: `bad_arg`, `unsupported`
  (the hardware switch is off).

The reply of a verb is the state; `denied` is NetworkManager's polkit saying no,
`failed` its error.

The pure module's Wi-Fi test reads a fixture access-point set
(`crates/fantastic-system/tests/fixtures/network_aps.json`: open, WPA2, WPA3,
WEP, enterprise, a non-UTF-8 SSID, a hidden one, duplicates).

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

## displays (`displays.tools`, id `displays`)

OS needs: `wlr-randr` on `$PATH` and a Wayland compositor that implements
wlr-output-management (cage does), at `$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY`
(`wayland-0` when unset); for brightness, logind (`SetBrightness` on the user's
display session) and a backlight in `/sys/class/backlight`. Without the tool,
`why` is "wlr-randr is not installed"; while the socket is missing (the kiosk
restarting) it is "no Wayland display at /run/user/<uid>/wayland-0".

```json
{"available": true,
 "outputs": [{"name": "Virtual-1", "enabled": true, "make": "", "model": "",
              "mode": {"width": 1280, "height": 800, "refresh_mhz": 60000},
              "modes": [{"width": 1280, "height": 800, "refresh_mhz": 60000, "preferred": true, "current": true}],
              "scale": 1.0, "transform": "normal", "position": [0, 0]}],
 "backlight": {"name": "intel_backlight", "value": 0.6}}
```

- `mode` is the current mode, `null` for an output that is off. `backlight` is
  `null` without one; with several, `raw` is taken over `platform` over
  `firmware`, else the first by name.
- `wlr-randr` has no change stream: the state is read every 5 s and after each
  verb, and emitted only when it differs.

Verbs: `reflect`, `state`, then

- `set_output {name, mode?, scale?, transform?, position?, enabled?}`: runs
  `wlr-randr --output <name> ...` (`--mode WxH[@R]`, `--scale`, `--transform`,
  `--pos X,Y`, `--on|--off`). `mode` is `"WxH"`, `"WxH@R"` or
  `{width, height, refresh_mhz?}` and must be one of the output's `modes`;
  `transform` is `normal`, `90`, `180`, `270`, `flipped`, `flipped-90`,
  `flipped-180` or `flipped-270`; `position` is `[x, y]`. Refused: `bad_arg`
  (nothing to change, a bad value), `not_found` (no such output),
  `unsupported` (turning the last enabled output off).
- `set_brightness {value}`: 0.0 to 1.0, clamped. Refused: `bad_arg`,
  `unsupported` (no backlight), `denied` (logind: not the active session).

## notifications (`notifications.tools`, id `notifications`)

OS needs: the session bus (`DBUS_SESSION_BUS_ADDRESS`). The agent is the
session's notification server: it owns `org.freedesktop.Notifications` at
`/org/freedesktop/Notifications` (without queueing; another server that holds
the name is not replaced), so every notification a program sends (`notify-send`,
a web page of the app, `busctl --user call ... Notify`) lands in `state`. It
serves `GetCapabilities` (`body`, `actions`, `persistence`),
`GetServerInformation` (`fantastic`, `aisixteen`, the crate version, `1.2`),
`Notify`, `CloseNotification`, and emits `NotificationClosed` and
`ActionInvoked`.

```json
{"available": true, "server": "owned",
 "items": [{"id": 3, "app": "notify-send", "summary": "...", "body": "...", "urgency": "normal",
            "actions": [["default", "Open"]], "at_ms": 0, "expires_ms": 8000}],
 "do_not_disturb": false}
```

- `server` is `owned`, `taken: <unique name>` (another server holds the name:
  `available` is false and every verb is refused `unavailable`), or `none`.
- Ids are numbers from 1, never 0; a `replaces_id` that exists replaces the item
  in place. At most 50 items, the oldest dropped (closed with reason 4).
- Expiry, on the agent's 5 s clock: `expire_timeout` -1 is 8 s, 0 never, and
  critical urgency never. Closing reasons: 1 expired, 2 dismissed, 3 closed by
  a call, 4 undefined.
- Hints read: `urgency` (byte), `category`, `desktop-entry` (strings),
  `resident` (bool); the rest are ignored. An item shows `category`,
  `desktop_entry` and `resident` when set, and `quiet: true` when it arrived
  while do-not-disturb was on.

Verbs: `reflect`, `state`, then

- `notify {summary, body?, urgency?}`: posts one as app `fantastic`
  (`urgency`: `low`, `normal`, `critical`). Refused: `bad_arg`, `unavailable`.
- `dismiss {id}`, `dismiss_all`: close with reason 2. Refused: `bad_arg`,
  `not_found`.
- `invoke {id, action}`: emits `ActionInvoked`, then closes the item with reason
  2 unless it is `resident`. Refused: `bad_arg` (no such action), `not_found`.
- `set_dnd {on}`: new items still arrive, marked `quiet`.

## lock (`lock.tools`, id `lock`)

OS needs: logind (the user's display session) on the system bus, and
`<home>/lock.yaml` (`<home>` as the saved set-ups: `FANTASTIC_HOME`, else
`~/.fantastic`; mode 0600, the PIN as an Argon2id hash).

**A kiosk UI lock, not a security boundary**: code already running as the user
(an exec agent) is not stopped by it. The screen is the Home page's: while
`state.locked` it draws a full-screen overlay that nothing but a lock state
saying `locked: false` removes (not Esc, not a click). The Home page reaches the
agent over the system canvas's `/user/ws` (the running Home row named `system`);
the system canvas lets the Home's origin in (it reads `<home>/app/home.yaml`
and adds the origin to its `web` record's `allowed_origins`). `lock` answers
`user` alone: no AI may unlock the screen or change the PIN.

```json
{"available": true, "locked": false, "has_pin": true, "auto_lock_min": 10,
 "failures": 0, "retry_at_ms": null}
```

- `failures` counts wrong PINs in a row; from the 5th, `retry_at_ms` (epoch ms)
  is when the next try is looked at: 30 s, doubling to 15 min.
- The agent follows the user's display session in logind (it changes at every
  session start): the session's `Lock` (`loginctl lock-session`) locks, `Unlock`
  unlocks. `locked` is mirrored into the session's `LockedHint`. It holds a
  `delay` sleep inhibitor; on `PrepareForSleep` it locks and lets sleep proceed.
- Nothing locks without a PIN (nothing could unlock it): `lock` is refused,
  logind's `Lock`, the idle clock and sleep are ignored.

Verbs: `reflect`, `state`, then

- `lock`. Refused: `unsupported` (no PIN set).
- `unlock {pin}`: the PIN is text of 4 to 12 digits. No effect when no PIN is
  set or the screen is not locked. Refused: `wrong_pin`, `retry_later` (during
  the wait), `bad_arg`, `busy`.
- `set_pin {new, old?}`: `old` is required when a PIN is set. `clear_pin {pin}`
  removes it (and opens a locked screen). Refused as `unlock` is.
- `set_auto_lock {minutes}`: 0 (off) to 1440; the screen locks after that many
  minutes without `activity`. Refused: `bad_arg`.
- `activity`: the Home page's input signal, taken at most once per 10 s.

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
- The image has `wlr-randr` (displays); the notification server needs the
  session bus (`DBUS_SESSION_BUS_ADDRESS`, above).
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
kernel's root, and owned files beyond the directive (an html view's body, a
starter's `bundle.yaml`, code and panels). The import is below.

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

Not yet: the canvas drop (a folder dragged onto a cell or the canvas), a TUI
command, the verb on the kernel's root, `@path:*` ports, and owned files
beyond the directive (a directive keeps its relative `system_prompt_file`; the
file is not written into the new agent's folder).
