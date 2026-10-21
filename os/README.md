# fantastic OS

A machine that boots straight into the desktop app's Home. Debian 13 "trixie",
minimal and headless, plus two packages:

- `aisixteen-fantastic`: the desktop app's `.deb`. It installs
  `/usr/bin/fantastic-app` and its sidecar `/usr/bin/fantastic`; the app hosts
  the home kernel in-process.
- `fantastic-session`: turns any headless Debian into the kiosk. A system unit,
  `fantastic-kiosk.service`, logs the user `fantastic` in on tty1 (a PAM and
  logind session on seat0) and runs `cage` with one client,
  `fantastic-app --kiosk`. Screen blanking is off.

There is no browser, no desktop environment and no display manager. A folder is
opened by typing its path into the Home card's field (kiosk mode has no folder
dialog). The canvases the app starts keep running when the kiosk restarts, and
the new app adopts them.

```
os/
  session/          the fantastic-session package's tree (installed paths)
  mkosi/            the image: mkosi.conf, one kernel per architecture, the ci profile
  container/        the server image: Containerfile, fantastic-server (its entrypoint)
  ci/               boot_check.py: boots the image in QEMU and checks the kiosk;
                    container_check.py: smoke-tests the server image under Docker and Podman
scripts/build/deb.sh        the app's .deb and fantastic-session into dist/deb/
scripts/build/os.sh         the image into dist/os/
scripts/build/container.sh  the server image (docker or podman)
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

`deb.sh` takes the binaries CI built, where `scripts/build/app.sh` and
`cli.sh` leave them:

```sh
sh scripts/build/cli.sh --target x86_64-unknown-linux-musl     # the sidecar
sh scripts/build/app.sh --target x86_64-unknown-linux-musl     # the app
sh scripts/build/deb.sh --target x86_64-unknown-linux-musl     # dist/deb/*.deb
```

(`aarch64-unknown-linux-musl` for arm64.) The version is `FANTASTIC_VERSION`, else
the last git tag. The packages are unsigned; signing is the maintainer's, by
hand (`src/lib/rust/RELEASING.md`). `fantastic-session` depends on
`aisixteen-fantastic (= <version>)`, so the two always come from one build.

## Build the image

Needs [mkosi](https://github.com/systemd/mkosi) **v27.1** (Ubuntu 24.04's own
package is 20.2, too old; CI uses the `systemd/mkosi@v27.1` action) and the
packages in `dist/deb/`. It runs as root: unprivileged, mkosi maps a single uid,
so the image's files would all be owned by root (the kiosk's home too).

```sh
sudo sh scripts/build/os.sh --arch x86-64          # dist/os/fantastic-os_<version>.raw
sudo sh scripts/build/os.sh --arch arm64
sudo sh scripts/build/os.sh --arch x86-64 --profile ci   # + a serial console, socat, the guest agent
```

The image is a UEFI disk (systemd-boot), x86_64 first, then arm64 (UEFI arm64:
QEMU `virt` and UEFI boards; Raspberry Pi firmware is not covered). It has no
root password, no SSH and no `Autologin=`: mkosi's would log root in on the
consoles and fight the kiosk for tty1. NetworkManager (wired DHCP) and PipeWire
are in the image, for the system agents of a later phase; they are not Depends of
`fantastic-session`, so a server install stays small.

mkosi installs no Recommends, so `fantastic-session` depends on what the kiosk
needs: `cage`, `libgl1-mesa-dri` (WebKit's EGL) and `fonts-dejavu-core`.

## Boot it in QEMU

The checked path is the CI one: `os/ci/boot_check.py` boots the `ci` profile
image, drives it through QMP and the guest agent, and writes screenshots and
logs:

```sh
uv run --project src/lib/python python os/ci/boot_check.py \
  --image dist/os/fantastic-os_<version>.raw --arch x86-64 --out os-check
```

It needs `qemu-system-x86` and `ovmf` (arm64: `qemu-system-arm`,
`qemu-efi-aarch64`). `/dev/kvm` makes it fast; without it, QEMU falls back to TCG
and the boot takes much longer (raise `--timeout` and `--step`).

To look at the kiosk yourself, with a window:

```sh
qemu-system-x86_64 -machine q35,accel=kvm:tcg -cpu max -smp 2 -m 4096 \
  -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
  -drive if=pflash,format=raw,file=OVMF_VARS_4M.fd \
  -drive if=virtio,format=raw,file=dist/os/fantastic-os_<version>.raw,snapshot=on \
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
sudo dd if=dist/os/fantastic-os_<version>.raw of=/dev/sdX bs=4M conv=fsync status=progress
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
  ExecStart=/usr/bin/cage -d -- /usr/bin/fantastic-app --kiosk
  ```

The session sets no `WEBKIT_*` variable. WebKit's environment on real GPUs
belongs to the app.

## What starts

| piece | provided by | used for |
|---|---|---|
| automatic login | `fantastic-kiosk.service` (`User=`, `PAMName=`, `TTYPath=/dev/tty1`), `/etc/pam.d/fantastic-kiosk`, `libpam-systemd` | a logind session on seat0, type `wayland`: device ACLs for DRM and input, `XDG_RUNTIME_DIR`, the user manager |
| the compositor | `cage` | one full-screen client: the app |
| the app | `/usr/bin/fantastic-app --kiosk` | the home kernel in-process, its web host on 127.0.0.1, the run file `~/.fantastic/app/home.yaml`, the Home page in one window |
| the canvases | `/usr/bin/fantastic`, the app's sidecar | each opened folder's kernel and page, on 127.0.0.1; they outlive a restart of the kiosk (`KillUserProcesses=no`) |
| no blanking | kernel default, `consoleblank=0`, logind `IdleAction=ignore` | the screen stays on |

`Restart=always` brings the kiosk back 2 s after the app quits or crashes, with
no start-rate limit. The unit sets `SHELL` through the user's passwd entry
(`/bin/bash`); the home starts every canvas in the login environment `$SHELL`
prints, and fails without it.

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

## The server container

The same `fantastic` CLI, headless, in a container: one canvas on a port, for a
server or a CI box. It runs under Docker and under rootless Podman, on amd64 and
arm64, from `ghcr.io/aisixteen/fantastic` (tags `X.Y.Z`, the commit sha, and
`latest` for the newest release). Its base is the kiosk's, Debian 13 "trixie";
the `fantastic` binary is the one the `.deb` carries. The recipe is
`os/container/Containerfile`; the entrypoint is `os/container/fantastic-server`.

> **The canvas has terminals.** Anyone who reaches the port runs commands as the
> container's user. Publish it on `127.0.0.1` only, never on a public interface,
> and reach a remote server through an SSH tunnel.

```sh
# Docker: the canvas on this machine's loopback, its state and its folder kept
docker run -d --name fantastic -p 127.0.0.1:8088:8088 \
  -v fantastic-state:/home/fantastic/.fantastic -v fantastic-work:/work \
  ghcr.io/aisixteen/fantastic:latest

# Podman, rootless: the same
podman run -d --name fantastic -p 127.0.0.1:8088:8088 \
  -v fantastic-state:/home/fantastic/.fantastic -v fantastic-work:/work \
  ghcr.io/aisixteen/fantastic:latest

# a folder of yours as the canvas folder: the container's user (uid 1000) must write it
docker run … --user "$(id -u):$(id -g)" -v "$PWD/canvas:/work" …      # Docker
podman run … --userns=keep-id -v "$PWD/canvas:/work" …                 # Podman
```

- **Ports.** The server listens on `8088` inside the container
  (`FANTASTIC_PORT`), and the Host guard admits the loopback names on that port: map
  it as `-p 127.0.0.1:<port>:<port>` (the same port on both sides). Any other mapping
  reaches the server, but the browser's address is refused with a 403. For another
  port, set both: `-e FANTASTIC_PORT=9000 -p 127.0.0.1:9000:9000`.
- **One canvas.** One container is one canvas, with no headless home beside it. To
  serve another canvas, run another container with its own volumes and port.
- **No login.** The port's reach is the access: the Host guard checks the address,
  not a password. Keep the port on loopback, or behind an SSH tunnel.
- **Volumes.** `/home/fantastic/.fantastic` is the state (the saved set-ups and the
  shared stores); `/work` is the canvas folder. A new container on the same
  volumes keeps everything: `docker stop` then `docker start`, or a new `run`.
- **Stop.** `docker stop` / `podman stop` send SIGTERM. The kernel drains and exits
  0, so the stop is orderly; tini forwards the signal and reaps the children.
- **Recipes.** Arguments after the image are recipes, as on the command line:
  `… ghcr.io/aisixteen/fantastic:latest ai ollama -p 11434`.
- **Health.** The image's `HEALTHCHECK` asks the canvas page for a 200, on the
  `localhost` Host a browser sends. The build uses Docker's image format, so Podman
  keeps it too: `docker inspect` shows its status, and `podman healthcheck run
  fantastic` runs it once.
- **Remote, through a tunnel.** On the server, the same `run` as above. On your
  machine, the local port equal to the container's:

  ```sh
  ssh -N -L 8088:127.0.0.1:8088 you@server
  ```

  then open `http://localhost:8088/canvas_page/file/index.html`.
- **Opening it.** Open `http://localhost:8088/canvas_page/file/index.html` (or the same path on `127.0.0.1`). The
  container binds `0.0.0.0`, so the port map reaches it; the Host guard admits the
  loopback names on that port. The server prints this address at start (`docker
  logs fantastic`). The root `/` is not a page, so a bare `http://localhost:8088/`
  is a 404. Any other name, a LAN address or another port is refused with a 403, and
  so is a DNS-rebinding page. A browser cannot open `0.0.0.0`, so do not use it as
  the address.

The image's build is `scripts/build/container.sh` (`--arch amd64|arm64`, one
image, loaded locally; `--arch all --push`, the manifest). CI builds both
architectures and smoke-tests them under Docker and rootless Podman
(`os/ci/container_check.py`: starts, answers, a canvas page loads, a terminal runs
a command, stops with SIGTERM, restarts on the same volumes with its state kept).
The image is pushed only by `publish-container.yml`, by hand, from a green full
run. That workflow rebuilds the image from the run's archives rather than pushing
the smoke-tested image: the binary and the pinned base are the same, but the apt
packages are not pinned, so the pushed bytes can differ from the tested ones.

## Security

- **Loopback only.** The home kernel and every canvas listen on 127.0.0.1. The
  shipped image has no debugging surface: no WebKit inspector (the release app is
  built without devtools), no WebDriver and no forwarder. `socat` and the guest
  agent exist in the `ci` profile only.
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

`ci.yml -f e2e=true` (a full run) calls `build.yml` with `os: true` after the
binaries are built:

- `debs` (amd64 and arm64): `deb.sh`, then checks of both packages (names,
  architectures, versions, modes, the session's exact `Depends`, the sidecar's
  sha256 equal to the release archive's binary), `shellcheck -s sh` of the
  maintainer scripts, the boot check's unit tests, and an install on a stock
  `debian:trixie-slim` container without Recommends (remove masks the unit, purge
  leaves no link and no PAM file, the user stays).
- `os` (x86-64 on KVM; arm64 on TCG, not a gate yet): the mkosi image of exactly
  those packages and `boot_check.py`. It keeps `os-check-<arch>` (screenshots,
  `boot_check.log`, `serial.log`, and on failure the journal and the kiosk's
  status) for 14 days.

`boot_check.py` checks, in the booted image: the run file names a live
`fantastic-app` with `SHELL` set; the logind session is `wayland` on seat0/tty1
and active; the app announced its home; `home` `list` answers over `/user/ws`;
Ctrl+0, sent through QEMU, reaches `home` as the Home page's `select {id: null}`;
the screen is not blank and changes when a canvas page opens; a canvas started
from the kiosk runs on the sidecar; and after SIGTERM to the app the kiosk comes
back (`NRestarts` + 1, a new pid), the canvas keeps its pid and is adopted, and
the page checks pass again.
