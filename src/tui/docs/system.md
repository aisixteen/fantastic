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
