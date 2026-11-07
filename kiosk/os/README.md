# fantastic OS

A machine that boots straight into the desktop app's Home. Debian 13 "trixie",
minimal and headless, plus two packages:

- `aisixteen-fantastic`: the desktop app's `.deb`. It installs
  `/usr/bin/fantastic-app` and its sidecar `/usr/bin/fantastic`; the app hosts
  the home kernel in-process.
- `fantastic-session`: turns any headless Debian into the kiosk. A system unit,
  `fantastic-kiosk.service`, logs the user `fantastic` in on tty1 (a PAM and
  logind session on seat0) and runs `cage` with one client,
  `fantastic-app --fullscreen`. Screen blanking is off.

There is no browser, no desktop environment and no display manager. A folder is
opened by typing its path into the Home card's field (kiosk mode has no folder
dialog). The canvases the app starts keep running when the kiosk restarts, and
the new app adopts them.

```
kiosk/os/
  session/          the fantastic-session package's tree (installed paths)
  mkosi/            the image: mkosi.conf, one kernel per architecture, the ci profile
  build/debs.sh     the app .deb (the base's, its sidecar replaced by fantastic-kiosk)
                    and fantastic-session into dist/kiosk/deb/
  build/os.sh       the image into dist/kiosk/os/
  ci/               qemu.py: QEMU and the guest agent (the /user/ws client is the
                    base's container/server/kernel_client.py);
                    boot_check.py: boots the image in QEMU and checks the kiosk;
                    e2e_system.py: the system agents on the booted image
```

## On an existing Debian 13

```sh
sudo apt install ./fantastic-app-x86_64-unknown-linux-gnu.deb ./fantastic-session_<version>_all.deb
sudo reboot
```

The first install enables the kiosk but does not start it (that would take tty1
from whoever is on it). It starts on the next boot, or with
`sudo systemctl start fantastic-kiosk`. The packages are on the
[release](https://github.com/aisixteen/fantastic/releases/latest); verify them
as the main README says.

## Build the packages

The base builds the app's `.deb` (`scripts/build/deb.sh`); `kiosk/os/build/debs.sh`
takes it, replaces its sidecar `/usr/bin/fantastic` with the `fantastic-kiosk`
binary, and builds `fantastic-session`:

```sh
sh scripts/build/cli.sh --target x86_64-unknown-linux-musl     # the base's sidecar
sh scripts/build/app.sh --target x86_64-unknown-linux-musl     # the app
sh scripts/build/deb.sh --target x86_64-unknown-linux-musl     # dist/deb/fantastic-app-*.deb
(cd kiosk && cargo build --release --target x86_64-unknown-linux-gnu)
sh kiosk/os/build/debs.sh --target x86_64-unknown-linux-gnu    # dist/kiosk/deb/*.deb
```

(`aarch64-unknown-linux-gnu` for arm64.) The version is `FANTASTIC_VERSION`, else
the last git tag. The packages are unsigned; signing is the maintainer's, by
hand (`src/lib/rust/RELEASING.md`). `fantastic-session` depends on
`aisixteen-fantastic (= <version>)`, so the two always come from one build.

## Build the image

Needs [mkosi](https://github.com/systemd/mkosi) **v27.1** (Ubuntu 24.04's own
package is 20.2, too old; CI uses the `systemd/mkosi@v27.1` action) and the
packages in `dist/kiosk/deb/`. It runs as root: unprivileged, mkosi maps a single uid,
so the image's files would all be owned by root (the kiosk's home too).

```sh
sudo sh kiosk/os/build/os.sh --arch x86-64          # dist/kiosk/os/fantastic-os_<version>.raw
sudo sh kiosk/os/build/os.sh --arch arm64
sudo sh kiosk/os/build/os.sh --arch x86-64 --profile ci   # + a serial console, socat, the guest agent
```

The image is a UEFI disk (systemd-boot), x86_64 first, then arm64 (UEFI arm64:
QEMU `virt` and UEFI boards; Raspberry Pi firmware is not covered). It has no
root password, no SSH and no `Autologin=`: mkosi's would log root in on the
consoles and fight the kiosk for tty1. NetworkManager (wired DHCP), PipeWire,
UPower and `wlr-randr` are in the image, for the system agents; they are not
Depends of `fantastic-session`, so a server install stays small.

mkosi installs no Recommends, so `fantastic-session` depends on what the kiosk
needs: `cage`, `libgl1-mesa-dri` (WebKit's EGL) and `fonts-dejavu-core`.

## Boot it in QEMU

The checked path is the CI one: `kiosk/os/ci/boot_check.py` boots the `ci` profile
image, drives it through QMP and the guest agent, and writes screenshots and
logs:

```sh
uv run --project src/lib/python python kiosk/os/ci/boot_check.py \
  --image dist/kiosk/os/fantastic-os_<version>.raw --arch x86-64 --out os-check
```

The system canvas (power, network, audio, displays, notifications, lock) is checked
on the same image by `kiosk/os/ci/e2e_system.py` (same arguments, plus `--only` to pick
steps); it prints `SKIP` and exits 0 when QEMU or the image is missing.

It needs `qemu-system-x86` and `ovmf` (arm64: `qemu-system-arm`,
`qemu-efi-aarch64`). `/dev/kvm` makes it fast; without it, QEMU falls back to TCG
and the boot takes much longer (raise `--timeout` and `--step`).

To look at the kiosk yourself, with a window:

```sh
qemu-system-x86_64 -machine q35,accel=kvm:tcg -cpu max -smp 2 -m 4096 \
  -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
  -drive if=pflash,format=raw,file=OVMF_VARS_4M.fd \
  -drive if=virtio,format=raw,file=dist/kiosk/os/fantastic-os_<version>.raw,snapshot=on \
  -device virtio-vga -device qemu-xhci -device usb-kbd -device usb-tablet \
  -nic user,model=virtio-net-pci
```

(`cp /usr/share/OVMF/OVMF_VARS_4M.fd .` first; drop `snapshot=on` to keep changes.)
In QEMU's virtio graphics there is no 3D, so wlroots and WebKit need the
software paths the `ci` profile sets in a drop-in
(`WLR_RENDERER=pixman WEBKIT_DISABLE_DMABUF_RENDERER=1`); the shipped image does
not set them.

## Flash it

```sh
sudo dd if=dist/kiosk/os/fantastic-os_<version>.raw of=/dev/sdX bs=4M conv=fsync status=progress
```

Check `/dev/sdX` twice. To keep a smaller artifact, compress it: `zstd -T0 -10`.

Real GPUs may need firmware from Debian's `non-free-firmware` component
(`firmware-misc-nonfree`, `firmware-amd-graphics`). It is not in the image: add the
component and the packages to a local `mkosi.conf.d/` drop-in (the keys are in
`mkosi.1.md` of the mkosi you use) and rebuild.

## Customize

The kiosk is an ordinary systemd unit: `sudo systemctl edit fantastic-kiosk`.

- **The wlroots renderer.** Unset, wlroots picks. To force one:

  ```ini
  [Service]
  Environment=WLR_RENDERER=gles2
  ```

  (`gles2`, `pixman` or `vulkan`.)
- **No VT switching.** The unit runs `cage -d -s`; `-s` allows switching to
  another VT with Ctrl+Alt+F<n>. To forbid it:

  ```ini
  [Service]
  ExecStart=
  ExecStart=/usr/bin/cage -d -- /usr/bin/fantastic-app --fullscreen
  ```

The session sets no `WEBKIT_*` variable. WebKit's environment on real GPUs
belongs to the app.

## What starts

| piece | provided by | used for |
|---|---|---|
| automatic login | `fantastic-kiosk.service` (`User=`, `PAMName=`, `TTYPath=/dev/tty1`), `/etc/pam.d/fantastic-kiosk`, `libpam-systemd` | a logind session on seat0, type `wayland`: device ACLs for DRM and input, `XDG_RUNTIME_DIR`, the user manager |
| the compositor | `cage` | one full-screen client: the app |
| the app | `/usr/bin/fantastic-app --fullscreen` | the home kernel in-process, its web host on 127.0.0.1, the run file `~/.fantastic/app/home.yaml`, the Home page in one window |
| the canvases | `/usr/bin/fantastic`, the app's sidecar | each opened folder's kernel and page, on 127.0.0.1; they outlive a restart of the kiosk (`KillUserProcesses=no`) |
| no blanking | kernel default, `consoleblank=0`, logind `IdleAction=ignore` | the screen stays on |
| the system canvas | `fantastic-system.service`, a user unit (`ConditionUser=fantastic`, `Type=notify`), `upower`, `wlr-randr` | the machine's agents (`fantastic canvas` on the kiosk binary), see below; `audio` uses the image's PipeWire, WirePlumber (`wpctl`) and `pw-dump`; `displays` needs `wlr-randr` (the system canvas sets `WAYLAND_DISPLAY`) |

`Restart=always` brings the kiosk back 2 s after the app quits or crashes, with
no start-rate limit. The unit sets `SHELL` through the user's passwd entry
(`/bin/bash`); the home starts every canvas in the login environment `$SHELL`
prints, and fails without it.

### The system canvas

The machine's basics as agents an AI calls like any other (`power`, `network`,
`audio`, `displays`, `notifications` and `lock`: battery and power controls over
UPower and logind; NetworkManager's devices and Wi-Fi; PipeWire's sinks and sources
over `pw-dump` and `wpctl`; the compositor's outputs over `wlr-randr` and the
backlight over logind; the session's notification server; the PIN lock screen;
`fantastic readme system`) live in one ordinary fantastic folder,
`/home/fantastic/system`, whose kernel runs `fantastic canvas` (the kiosk binary, which adds the machine's agents) as a
systemd **user** unit, `fantastic-system.service`. The Home lists it as `system`;
its page holds the cells, placed from the host tiles.

- **Why a unit, not the Home.** It runs from boot with nobody at the screen; systemd
  supervises it and restarts it; and it is in no logind session, so a kiosk restart
  leaves no stale session behind it (logind resolves `session/auto` to the caller's
  own session, else the user's `Display`). Its environment is in the unit
  (`DBUS_SESSION_BUS_ADDRESS`, `WAYLAND_DISPLAY`; `XDG_RUNTIME_DIR` comes from the
  user manager), not from a login shell.
- **Readiness.** `Type=notify`: the CLI sends `READY=1` to `$NOTIFY_SOCKET` once the
  canvas serves and its MCP socket listens. Then `ExecStartPost=` lists it in the
  Home (`register_home_folder`) through `fantastic send`.
- **Files.** `usr/lib/systemd/user/fantastic-system.service` (enabled for every
  user's manager by the maintainer scripts, `deb-systemd-helper --user`; it runs in
  the kiosk user's only), `usr/lib/tmpfiles.d/fantastic-session.conf` (the folder,
  `fantastic:fantastic` 0700, made at every boot), and
  `usr/share/polkit-1/rules.d/50-fantastic-session.rules`.
- **Polkit.** There is no polkit agent, so every `auth_*` answer is a refusal. The
  rule grants the user `fantastic`, whatever the state of the kiosk session,
  NetworkManager's actions and logind's power off, reboot, suspend and hibernate.
- **The lock.** The Home page covers itself while `lock` says locked; it dials this
  canvas's socket, whose `web` record lets the Home's origin in (read from
  `~/.fantastic/app/home.yaml`). A UI lock, not a security boundary.
- **Purge** removes the unit's link and keeps `/home/fantastic/system`: it is the
  user's data, like the user.
- **The Home's actions on this row.** `stop` is a restart (SIGTERM; systemd starts it
  2 s later). `remove` lasts until the unit next starts; to drop it for good:
  `systemctl --user disable --now fantastic-system`, then `remove`. `start` is not
  for it: in those 2 s it would make a second host in the kiosk's session, holding
  the folder while systemd retries.

### No blanking, all of it

1. The kernel console has not blanked by default since Linux 4.12; the image also
   passes `consoleblank=0`.
2. logind's idle action is `ignore` (`50-fantastic-session.conf`).
3. cage has no idle blanking or DPMS of its own.
4. The session runs no idle daemon, and the app powers no output down.

CI cannot wait that long. Points 3 and 4 are checked by hand, once, for the PR:
boot the image as in "Boot it in QEMU" with
`-qmp unix:/tmp/qmp.sock,server=on,wait=off` added, leave it 11 minutes, and take a
screenshot through QMP:

```sh
printf '%s\n' '{"execute":"qmp_capabilities"}' \
  '{"execute":"screendump","arguments":{"filename":"idle.png","format":"png"}}' \
  | socat - UNIX-CONNECT:/tmp/qmp.sock
```

## Security

- **Loopback only.** The home kernel and every canvas listen on 127.0.0.1. This
  covers the kiosk. The shipped image has no debugging surface: no
  WebKit inspector (the release app is built without devtools), no WebDriver and no
  forwarder. `socat` and the guest agent exist in the `ci` profile only.
- **No password.** The kiosk user has none (the account is locked for password
  login). tty1 is the kiosk; the other VTs ask for a login nobody can give until
  an administrator sets a password. The PAM service `fantastic-kiosk` denies
  `auth`: it only opens the session.
- **Opening a folder.** A folder is opened by typing its path into the Home card.
  Anyone at the keyboard can open any folder the user `fantastic` can read.
- **Removal.** Removing `fantastic-session` masks the unit and keeps the user and
  its home; purging removes the unit and the PAM file. Debian packages do not
  delete users.

Administering the machine (a password for `fantastic`, an admin user, SSH, Wi-Fi)
is out of scope for this image.

## CI

`.github/workflows/kiosk.yml` builds and tests the kiosk on its own:

- `units`: the kiosk workspace (`cargo test`, the boundary test), and the lock
  screen's and tile size's TypeScript tests.
- `system-agents`: the six agents on a fake D-Bus (`cargo test -- --ignored`).
- `session-package`: `shellcheck -s sh` of the maintainer scripts and the unit
  tests of `kiosk/os/ci` (`python3 -m unittest discover -s kiosk/os/ci`).
- `image` (push to main, or by hand): the `fantastic-kiosk` binary, the latest
  Release's app `.deb` with that binary as its sidecar plus `fantastic-session`
  (`debs.sh`), an install on a stock `debian:trixie-slim` container without
  Recommends (remove masks the unit, purge leaves no link and no PAM file, the
  user stays), the mkosi image of exactly those packages, `boot_check.py` and
  `e2e_system.py` in QEMU (x86-64 on KVM; arm64 on TCG, not a gate yet). It keeps
  `os-check-<arch>` (screenshots, `boot_check.log`, `serial.log`, and on failure the
  journal and the kiosk's status) and `os-e2e-system-<arch>` for 14 days.

`boot_check.py` checks, in the booted image: the run file names a live
`fantastic-app` with `SHELL` set; the logind session is `wayland` on seat0/tty1
and active; the app announced its home; `home` `list` answers over `/user/ws`;
Ctrl+0, sent through QEMU, reaches `home` as the Home page's `select {id: null}`;
the screen is not blank and changes when a canvas page opens; a canvas started
from the kiosk runs on the sidecar; and after SIGTERM to the app the kiosk comes
back (`NRestarts` + 1, a new pid), the canvas keeps its pid and is adopted, and
the page checks pass again.

The Wi-Fi fixture of the system canvas's network checks is in the `ci` profile only:
`fantastic-ci-hwsim.service` loads `mac80211_hwsim` with two radios and runs hostapd
(`/etc/hostapd/fantastic-ci.conf`) on `wlan1`, the WPA2 network `fantastic-ci`
(passphrase `fantastic-ci-psk`); NetworkManager leaves `wlan1` alone
(`90-ci-hwsim.conf`) and `wlan0` sees the network. Debian's generic kernel builds the
module; a kernel without it skips the unit.
