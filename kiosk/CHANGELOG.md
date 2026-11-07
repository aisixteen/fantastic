# Changelog of the kiosk

History moved from the base's `CHANGELOG.md` (#475): what these entries describe is in `kiosk/`.

- System agents (Linux, `fantastic [canvas] system`): the machine's basics as
  agents an AI calls like any other, each with its view as a canvas cell — first
  `power` (battery over UPower; power off, reboot, suspend over logind), then
  `audio` (PipeWire's sinks and sources: volume, mute and the default, read with
  `pw-dump` and followed with `pw-dump --monitor`, set with `wpctl`). The model
  and ViewModel are pure (`fantastic-system`); every OS call is in the bundle's
  `sys/linux`. On fantastic OS they run in the system canvas `/home/fantastic/system`,
  the user service `fantastic-system`, which the Home lists.
- System agents, `network` (#55): NetworkManager's devices, Wi-Fi networks (one per
  SSID, the strongest first, with their security: open, wpa-psk, sae, wep or
  enterprise) and saved connections; `scan`, `connect` (a saved network is
  activated, a new one made with the password never kept), `activate`,
  `deactivate`, `forget`, `set_wireless`. A host tile opens it at 640x480.
- System agents, `notifications` (#55): the session's notification server
  (`org.freedesktop.Notifications`, spec 1.2): what programs send lands in the
  agent's state; `notify`, `dismiss`, `dismiss_all`, `invoke`, `set_dnd`.
- System agents, `displays` (#55): the compositor's outputs through `wlr-randr`
  (mode, scale, transform, position, on and off; never the last output off) and the
  backlight through logind; `set_output`, `set_brightness`.
- System agents, `lock` (#55): the kiosk's lock screen. A PIN of 4 to 12 digits
  (Argon2id, in `<home>/lock.yaml`, mode 0600), the wait after wrong PINs, an idle
  lock, logind's `Lock`/`Unlock` and sleep; `lock`, `unlock`, `set_pin`, `clear_pin`,
  `set_auto_lock`, `activity`. The Home page covers itself with an overlay only
  `unlock` removes, over the system canvas's socket. `user` alone may call it. A UI
  lock, not a security boundary.
- fantastic OS: a Debian 13 (trixie) kiosk image built with mkosi (x86_64, arm64)
  that boots into the desktop app. The `fantastic-session` package logs the user
  `fantastic` in on tty1 and runs `fantastic-app --kiosk` full screen under cage,
  with screen blanking off; `scripts/build/deb.sh` builds it and the app's `.deb`.
  CI boots the image in QEMU and checks that the home kernel serves, the Home page
  answers a key press, and a canvas survives a restart of the kiosk. The
  installers carry `fantastic-session` beside the app's `.deb`.
