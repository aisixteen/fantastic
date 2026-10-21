#!/usr/bin/env python3
"""Boot the fantastic OS image in QEMU and check the kiosk (Phase 9, #54).

    uv run --project src/lib/python python os/ci/boot_check.py \
        --image dist/os/fantastic-os_<v>.raw --arch x86-64|arm64 --out os-check \
        [--timeout 600] [--step 90]

`--timeout` bounds the boot, up to the app's run file; `--step` bounds each later
wait. What it proves, in order (the steps are numbered as in the issue):

  3  the run file names a live `fantastic-app`, whose environment has SHELL (the
     home starts each canvas in the login shell's environment); logind has a
     `wayland` session on seat0/tty1, active; the app announced its home
  4  the home kernel's loopback port, forwarded out of the guest
  5  the kernel answers `home` `list` over /user/ws (`fantastic.home` v1)
  6  the Home page, in the app's window, answers a key press: Ctrl+0 goes in
     through QEMU, and `select {id: null}` comes out of the page through the
     kernel (this check never sends it itself; only the Home page turns Ctrl+0
     into that call)
  7  the screen shows something (QMP screendump, not uniform)
  8  a canvas started from the kiosk runs on the bundled sidecar, and the screen
     changes when its page slides in
  9  SIGTERM to the app: the kiosk comes back by itself, the canvas keeps its pid
     and is adopted, and 5-7 pass again

The QEMU parts are one class, `Vm`. Everything else is stdlib plus `websockets`
(imported in the socket helpers only, so the pure helpers import anywhere: see
test_boot_check.py). It imports nothing from `e2e/`.
"""

from __future__ import annotations

import argparse
import asyncio
import base64
import http.client
import itertools
import json
import re
import secrets
import select
import shutil
import socket
import subprocess
import sys
import time
from pathlib import Path
from urllib.parse import urlsplit

# the guest-side forwarder's port, and the host's end of QEMU's hostfwd
FORWARD_PORT = 18765
KIOSK_USER = "fantastic"
KIOSK_HOME = f"/home/{KIOSK_USER}"
RUN_FILE = f"{KIOSK_HOME}/.fantastic/app/home.yaml"
CANVAS_DIR = f"{KIOSK_HOME}/boot-check"
# the first screen of a canvas-less kiosk differs from a canvas page by at least this
MIN_CHANGED = 0.01
# how often wait_agent() offers the guest agent a new guest-sync, in s
SYNC_EVERY = 2.0


class Fail(Exception):
    """The first failure ends the run, with one line naming the step."""


# the step now running; main() puts it in the one failure line
STEP = "0"


def at(step: str) -> None:
    global STEP
    STEP = step


# ── pure helpers (unit-tested) ───────────────────────────────────────────────


def parse_ppm(data: bytes) -> tuple[int, int, bytes]:
    """A binary PPM (`P6`, maxval 255): (width, height, RGB pixels)."""
    pos = 0
    tokens: list[bytes] = []
    while len(tokens) < 4:
        while pos < len(data) and data[pos : pos + 1].isspace():
            pos += 1
        if pos < len(data) and data[pos : pos + 1] == b"#":
            while pos < len(data) and data[pos : pos + 1] != b"\n":
                pos += 1
            continue
        start = pos
        while pos < len(data) and not data[pos : pos + 1].isspace():
            pos += 1
        if start == pos:
            raise ValueError("ppm: truncated header")
        tokens.append(data[start:pos])
    magic, w, h, maxval = tokens
    if magic != b"P6":
        raise ValueError(f"ppm: magic {magic!r}, not P6")
    try:
        width, height, top = int(w), int(h), int(maxval)
    except ValueError as e:
        raise ValueError(f"ppm: bad header {tokens!r}") from e
    if width <= 0 or height <= 0:
        raise ValueError(f"ppm: size {width}x{height}")
    if top != 255:
        raise ValueError(f"ppm: maxval {top}, not 255")
    pos += 1  # the single whitespace byte after maxval
    pixels = data[pos:]
    if len(pixels) != width * height * 3:
        raise ValueError(f"ppm: {len(pixels)} pixel bytes, {width}x{height} needs {width * height * 3}")
    return width, height, pixels


def is_uniform(pixels: bytes) -> bool:
    """Every RGB pixel equal to the first (a blank screen)."""
    if len(pixels) % 3 or not pixels:
        raise ValueError(f"{len(pixels)} bytes are not RGB pixels")
    return pixels == pixels[:3] * (len(pixels) // 3)


def changed_fraction(a: tuple[int, int, bytes], b: tuple[int, int, bytes]) -> float:
    """The fraction of pixels that differ between two same-sized images."""
    if a[:2] != b[:2]:
        raise ValueError(f"images differ in size: {a[:2]} vs {b[:2]}")
    pa, pb = a[2], b[2]
    changed = sum(
        1
        for r, g, bl, r2, g2, b2 in zip(pa[0::3], pa[1::3], pa[2::3], pb[0::3], pb[1::3], pb[2::3])
        if (r, g, bl) != (r2, g2, b2)
    )
    return changed / (a[0] * a[1])


def parse_run_file(text: str) -> dict:
    """`{pid, port, url}` from the app's flat run file; all three keys required."""
    out: dict = {}
    for line in text.splitlines():
        m = re.match(r"^(pid|port|url): *(.*)$", line)
        if not m:
            continue
        v = m.group(2).strip()
        if len(v) >= 2 and v[0] == v[-1] and v[0] in "'\"":
            v = v[1:-1]
        out[m.group(1)] = v
    missing = [k for k in ("pid", "port", "url") if not out.get(k)]
    if missing:
        raise ValueError(f"the run file has no {', '.join(missing)}: {text!r}")
    try:
        pid, port = int(out["pid"]), int(out["port"])
    except ValueError as e:
        raise ValueError(f"the run file's pid/port are not numbers: {text!r}") from e
    if not out["url"].startswith(f"http://127.0.0.1:{port}/"):
        raise ValueError(f"the run file's url {out['url']!r} is not http://127.0.0.1:{port}/…")
    return {"pid": pid, "port": port, "url": out["url"]}


def is_select_home(frame: object) -> bool:
    """The watcher's frame for the Home page's `select {id: null}` call, exactly."""
    return (
        isinstance(frame, dict)
        and frame.get("type") == "event"
        and frame.get("payload") == {"type": "select", "id": None}
    )


def reply_data(frame: object, call_id: str) -> object | None:
    """The `data` of the reply to call `call_id`; None for any other frame.

    An `error` frame for the call is a failure, never a value."""
    if not isinstance(frame, dict) or frame.get("id") != call_id:
        return None
    if frame.get("type") == "error":
        raise Fail(f"the call {call_id} failed: {frame}")
    if frame.get("type") == "reply":
        return frame.get("data")
    return None


def check_state(data: object) -> dict:
    """`home` `list`'s answer: a `fantastic.home` v1 state, with no error."""
    if not isinstance(data, dict):
        raise Fail(f"home list answered {data!r}, not a state")
    if "error" in data:
        raise Fail(f"home list answered an error: {data['error']!r}")
    if (data.get("type"), data.get("protocol"), data.get("version")) != ("state", "fantastic.home", 1):
        raise Fail(f"home list answered type/protocol/version {data.get('type')!r}/{data.get('protocol')!r}/{data.get('version')!r}")
    return data


def find_row(state: dict, path: str) -> dict | None:
    return next((r for r in state.get("rows", []) if r.get("path") == path), None)


def parse_props(text: str) -> dict[str, str]:
    """`systemctl show` / `loginctl show-session` output: `Key=value` lines."""
    return dict(line.split("=", 1) for line in text.splitlines() if "=" in line)


def check_session(props: dict[str, str]) -> None:
    """The kiosk's logind session: wayland, user class, seat0, tty1, active."""
    want = {"Type": "wayland", "Class": "user", "Seat": "seat0", "TTY": "tty1", "Active": "yes"}
    bad = {k: props.get(k) for k, v in want.items() if props.get(k) != v}
    if bad:
        raise Fail(f"the logind session is {bad}, wanted {want}")


def check_shell(environ: dict[str, str]) -> str:
    """The app's SHELL: the home cannot start a canvas without it."""
    shell = environ.get("SHELL")
    if shell != "/bin/bash":
        raise Fail(f"the app's SHELL is {shell!r}, not /bin/bash (the home cannot start a canvas without it)")
    return shell


def parse_environ(raw: bytes) -> dict[str, str]:
    """`/proc/<pid>/environ`: NUL-separated `NAME=value`."""
    out = {}
    for item in raw.split(b"\0"):
        if b"=" in item:
            k, v = item.split(b"=", 1)
            out[k.decode("utf-8", "replace")] = v.decode("utf-8", "replace")
    return out


def qemu_argv(arch: str, image: Path, out: Path) -> list[str]:
    """The QEMU command line (the issue's §5.1). `snapshot=on` keeps the image pristine."""
    qmp, qga = out / "qmp.sock", out / "qga.sock"
    if arch == "x86-64":
        argv = [
            "qemu-system-x86_64", "-machine", "q35,accel=kvm:tcg", "-cpu", "max",
            "-drive", "if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd",
            "-drive", f"if=pflash,format=raw,file={out / 'OVMF_VARS_4M.fd'}",
            "-device", "virtio-vga",
            "-device", "qemu-xhci", "-device", "usb-kbd", "-device", "usb-tablet",
        ]  # fmt: skip
    elif arch == "arm64":
        argv = [
            "qemu-system-aarch64", "-machine", "virt,accel=kvm:tcg", "-cpu", "max",
            "-drive", "if=pflash,format=raw,readonly=on,file=/usr/share/AAVMF/AAVMF_CODE.fd",
            "-drive", f"if=pflash,format=raw,file={out / 'AAVMF_VARS.fd'}",
            "-device", "virtio-gpu-pci",
            "-device", "virtio-keyboard-pci", "-device", "virtio-tablet-pci",
        ]  # fmt: skip
    else:
        raise ValueError(f"unknown arch {arch!r}")
    return argv + [
        "-smp", "2", "-m", "4096",
        "-drive", f"if=virtio,format=raw,file={image},snapshot=on",
        "-display", "none",
        "-nic", f"user,model=virtio-net-pci,hostfwd=tcp:127.0.0.1:{FORWARD_PORT}-:{FORWARD_PORT}",
        "-qmp", f"unix:{qmp},server=on,wait=off",
        "-chardev", f"socket,id=qga0,path={qga},server=on,wait=off",
        "-device", "virtio-serial-pci",
        "-device", "virtserialport,chardev=qga0,name=org.qemu.guest_agent.0",
        "-serial", f"file:{out / 'serial.log'}",
    ]  # fmt: skip


def vars_firmware(arch: str) -> Path:
    """The firmware's variable store QEMU writes to (copied into --out)."""
    return Path("/usr/share/OVMF/OVMF_VARS_4M.fd" if arch == "x86-64" else "/usr/share/AAVMF/AAVMF_VARS.fd")


# ── the log ──────────────────────────────────────────────────────────────────

T0 = time.monotonic()
_log_file: Path | None = None


def log(text: str) -> None:
    line = f"[{time.monotonic() - T0:7.1f}s] {text}"
    print(line, flush=True)
    if _log_file is not None:
        with _log_file.open("a", encoding="utf-8") as f:
            f.write(line + "\n")


# ── QEMU: the QMP and guest-agent channels, and the guest ────────────────────


class Chan:
    """A line-delimited JSON channel on a unix socket (QMP, the guest agent)."""

    def __init__(self, path: Path, wait: float, alive):
        self.path, self.alive, self.buf = path, alive, b""
        end = time.monotonic() + wait
        while True:
            alive()
            s = socket.socket(socket.AF_UNIX)
            try:
                s.connect(str(path))
                break
            except OSError as e:
                s.close()
                if time.monotonic() > end:
                    raise Fail(f"{path.name}: not connectable within {wait:g} s: {e}") from e
                time.sleep(0.5)
        self.sock = s

    def send(self, obj: dict) -> None:
        # its own bound: recv() leaves the socket with whatever timeout it last set
        self.sock.settimeout(30)
        self.sock.sendall(json.dumps(obj).encode() + b"\n")

    def writable(self) -> bool:
        """Whether the socket takes a short write now. QEMU reads the guest agent's
        socket only while the guest has its port open, so before qemu-ga starts the
        writes queue up until the socket's buffer is full."""
        return bool(select.select([], [self.sock], [], 0)[1])

    def recv(self, timeout: float) -> dict | None:
        """The next JSON line, or None after `timeout` s."""
        end = time.monotonic() + timeout
        while b"\n" not in self.buf:
            left = end - time.monotonic()
            if left <= 0:
                return None
            self.sock.settimeout(left)
            try:
                chunk = self.sock.recv(65536)
            except TimeoutError:
                return None
            if not chunk:
                self.alive()
                raise Fail(f"{self.path.name}: closed by QEMU")
            self.buf += chunk
        line, self.buf = self.buf.split(b"\n", 1)
        return json.loads(line) if line.strip() else self.recv(max(0.0, end - time.monotonic()))

    def close(self) -> None:
        self.sock.close()


class Vm:
    """QEMU and everything that talks to it: QMP, the guest agent, the forwarder."""

    def __init__(self, image: Path, arch: str, out: Path):
        self.image, self.arch, self.out = image, arch, out
        self.proc: subprocess.Popen | None = None
        self._qmp: Chan | None = None
        self._qga: Chan | None = None
        self.forwarder: int | None = None

    # -- QEMU --
    def start(self) -> None:
        shutil.copy(vars_firmware(self.arch), self.out / vars_firmware(self.arch).name)
        argv = qemu_argv(self.arch, self.image, self.out)
        log("qemu: " + " ".join(argv))
        self.proc = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=(self.out / "qemu.out").open("wb"), stderr=subprocess.STDOUT)

    def alive(self) -> None:
        """Fails, with the serial log's tail, once QEMU has exited."""
        if self.proc is not None and self.proc.poll() is not None:
            raise Fail(f"qemu exited with {self.proc.returncode}; serial.log ends:\n{self.serial_tail()}")

    def serial_tail(self, lines: int = 40) -> str:
        try:
            return "\n".join((self.out / "serial.log").read_text(errors="replace").splitlines()[-lines:])
        except OSError as e:
            return f"(no serial.log: {e})"

    def quit(self) -> None:
        """QMP `quit`, then reap QEMU (killed only if it does not go)."""
        if self.proc is None or self.proc.poll() is not None:
            return
        try:
            self.qmp("quit")
        except (Fail, OSError) as e:
            log(f"qmp quit: {e}")
        try:
            self.proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            log("qemu did not exit within 30 s of quit: killing it")
            self.proc.kill()
            self.proc.wait()

    # -- QMP --
    def qmp(self, command: str, **arguments) -> object:
        if self._qmp is None:
            ch = Chan(self.out / "qmp.sock", 30, self.alive)
            if not (ch.recv(10) or {}).get("QMP"):
                raise Fail("qmp: no greeting")
            ch.send({"execute": "qmp_capabilities"})
            self._qmp = ch
            self._qmp_reply(ch, "qmp_capabilities")
        self._qmp.send({"execute": command, **({"arguments": arguments} if arguments else {})})
        return self._qmp_reply(self._qmp, command)

    def _qmp_reply(self, ch: Chan, command: str) -> object:
        end = time.monotonic() + 30
        while time.monotonic() < end:
            self.alive()
            m = ch.recv(1)
            if m is None or "event" in m:
                continue  # asynchronous events are not replies
            if "error" in m:
                raise Fail(f"qmp {command}: {m['error']}")
            return m.get("return")
        raise Fail(f"qmp {command}: no reply within 30 s")

    def send_keys(self, *qcodes: str) -> None:
        """Press and release `qcodes` together on the guest's keyboard."""
        self.qmp("send-key", keys=[{"type": "qcode", "data": q} for q in qcodes])

    def screendump(self, name: str) -> tuple[int, int, bytes]:
        """`<out>/<name>.ppm` (parsed and returned) and `<name>.png` (for the humans)."""
        ppm = self.out / f"{name}.ppm"
        self.qmp("screendump", filename=str(ppm))
        self.qmp("screendump", filename=str(self.out / f"{name}.png"), format="png")
        try:
            return parse_ppm(ppm.read_bytes())
        except (OSError, ValueError) as e:
            raise Fail(f"{name}.ppm: {e}") from e

    # -- the guest agent --
    def wait_agent(self, timeout: float) -> None:
        """`guest-sync` every SYNC_EVERY s until the reply carries our id. A sync is
        sent only while the socket takes it: a slow boot (arm64 on TCG) would
        otherwise fill the unread socket and block the send. The queued syncs reach
        qemu-ga once it starts; it answers each, and only the newest id counts."""
        ch = Chan(self.out / "qga.sock", timeout, self.alive)
        end = time.monotonic() + timeout
        n = None  # the newest sync's id
        while time.monotonic() < end:
            self.alive()
            if ch.writable():
                n = secrets.randbelow(2**31)
                ch.send({"execute": "guest-sync", "arguments": {"id": n}})
            end_try = time.monotonic() + SYNC_EVERY
            while time.monotonic() < end_try:
                m = ch.recv(max(0.1, end_try - time.monotonic()))
                if m is not None and m.get("return") == n:
                    self._qga = ch
                    return
        ch.close()  # the timeout ends the wait: do not leak the channel
        raise Fail(f"the guest agent did not answer within {timeout:g} s; serial.log ends:\n{self.serial_tail()}")

    def _ga(self, command: str, **arguments) -> object:
        if self._qga is None:
            raise Fail("the guest agent is not connected")
        self._qga.send({"execute": command, "arguments": arguments})
        m = self._qga.recv(30)
        if m is None:
            raise Fail(f"guest agent {command}: no reply within 30 s")
        if "error" in m:
            raise Fail(f"guest agent {command}: {m['error']}")
        return m.get("return")

    def ga_run(self, argv: list[str], timeout: float = 60) -> tuple[int, str, str]:
        """Run `argv` in the guest as root; (exit code, stdout, stderr)."""
        pid = self._ga("guest-exec", path=argv[0], arg=argv[1:], **{"capture-output": True})["pid"]
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            self.alive()
            st = self._ga("guest-exec-status", pid=pid)
            if st.get("exited"):
                code = st.get("exitcode", -st.get("signal", 1))
                dec = lambda k: base64.b64decode(st.get(k, "")).decode("utf-8", "replace")  # noqa: E731
                return code, dec("out-data"), dec("err-data")
            time.sleep(0.25)
        raise Fail(f"guest {argv}: not finished within {timeout:g} s")

    def ga_spawn(self, argv: list[str]) -> int:
        """Start `argv` in the guest and leave it running; its pid."""
        return self._ga("guest-exec", path=argv[0], arg=argv[1:], **{"capture-output": False})["pid"]

    def sh(self, script: str, timeout: float = 60) -> str:
        """`sh -c script` in the guest; stdout, or a failure with its output."""
        code, out, err = self.ga_run(["/bin/sh", "-c", script], timeout)
        if code != 0:
            raise Fail(f"guest `{script}` exited {code}: {out.strip()} {err.strip()}")
        return out

    # -- the home kernel's port --
    def forward(self, port: int) -> None:
        """socat in the guest: 0.0.0.0:FORWARD_PORT -> the kernel's loopback port.

        QEMU's hostfwd delivers to the guest's NIC address, never to its loopback."""
        if self.forwarder is not None:
            self.sh(f"kill {self.forwarder}")
        self.forwarder = self.ga_spawn(["/usr/bin/socat", f"TCP-LISTEN:{FORWARD_PORT},fork,reuseaddr", f"TCP:127.0.0.1:{port}"])
        log(f"forwarder: host 127.0.0.1:{FORWARD_PORT} -> guest 127.0.0.1:{port} (socat pid {self.forwarder})")


# ── the home kernel, through the forward ─────────────────────────────────────

_call_ids = itertools.count(1)


def http_get(port: int, path: str, *, connect: int = FORWARD_PORT, authority: str | None = None) -> tuple[int, str]:
    """GET through the forward, naming the guest's authority in `Host` (the host guard).

    `connect` is the TCP port dialled on loopback (the forward, or a container's port
    map); `authority` is the `Host` value (default `127.0.0.1:<port>`, a loopback bind's
    own authority)."""
    c = http.client.HTTPConnection("127.0.0.1", connect, timeout=5)
    try:
        c.putrequest("GET", path, skip_host=True, skip_accept_encoding=True)
        c.putheader("Host", authority or f"127.0.0.1:{port}")
        c.endheaders()
        r = c.getresponse()
        return r.status, r.read().decode("utf-8", "replace")
    finally:
        c.close()


def ws_connect(
    port: int, path: str, *, connect: int = FORWARD_PORT, authority: str | None = None, origin: str | None = None
):
    """A websocket to the guest's kernel: the TCP connection goes to the forward, the
    URI (and so `Host`) names the guest's port. No `Origin` is sent unless `origin` is
    given: a browser page's websocket sends its own (the guard checks it)."""
    from websockets.asyncio.client import connect as open_ws

    return open_ws(
        f"ws://{authority or f'127.0.0.1:{port}'}{path}", host="127.0.0.1", port=connect, open_timeout=10, origin=origin
    )


def ws_errors() -> tuple[type[BaseException], ...]:
    """What a websocket raises while the forward or the kernel is not (yet) there."""
    from websockets.exceptions import WebSocketException

    return OSError, TimeoutError, EOFError, WebSocketException


_seen: set[str] = set()


def retrying(what: str, e: BaseException) -> None:
    """Log a retried error once per distinct text (the wait's failure names the step)."""
    text = f"{what}: {type(e).__name__}: {e}"
    if text not in _seen:
        _seen.add(text)
        log(f"(retrying) {text}")


async def home_call(port: int, payload: dict, timeout: float = 10) -> object:
    """A call to `home` the way a page makes one: through `user`."""
    return await forward_call(port, "home", payload, timeout)


async def forward_call(
    port: int,
    target: str,
    payload: dict,
    timeout: float = 10,
    *,
    connect: int = FORWARD_PORT,
    authority: str | None = None,
    origin: str | None = None,
) -> object:
    """A call to agent `target` the way a page makes one: through `user`'s `forward`."""
    call_id = f"boot-check-{next(_call_ids)}"
    frame = {"type": "call", "target": "user", "payload": {"type": "forward", "target": target, "payload": payload}, "id": call_id}
    async with ws_connect(port, "/user/ws", connect=connect, authority=authority, origin=origin) as ws:
        await ws.send(json.dumps(frame))
        end = time.monotonic() + timeout
        while True:
            data = reply_data(json.loads(await asyncio.wait_for(ws.recv(), max(0.1, end - time.monotonic()))), call_id)
            if data is not None:
                return data


async def key_reaches_home(vm: Vm, port: int, step: float) -> None:
    """Ctrl+0 every 2 s until the `home` watcher sees the Home page's `select {id: null}`."""
    async with ws_connect(port, "/home/ws") as ws:
        end = time.monotonic() + step
        while True:
            vm.alive()
            vm.send_keys("ctrl", "0")
            until = time.monotonic() + 2
            while time.monotonic() < until:
                try:
                    raw = await asyncio.wait_for(ws.recv(), until - time.monotonic())
                except TimeoutError:
                    break
                if is_select_home(json.loads(raw)):
                    return
            if time.monotonic() > end:
                raise Fail(f"Ctrl+0 never reached `home` as select {{id: null}} within {step:g} s")


def until(vm: Vm | None, what: str, check, timeout: float, every: float = 1.0):
    """`check()` until it is truthy (its value); fails naming `what` after `timeout`.

    `vm` is the QEMU run to watch while waiting (a fail-fast on its exit), or None."""
    end = time.monotonic() + timeout
    while True:
        if vm is not None:
            vm.alive()
        got = check()
        if got:
            return got
        if time.monotonic() > end:
            raise Fail(f"{what}: not within {timeout:g} s")
        time.sleep(every)


def read_run_file(vm: Vm) -> dict | None:
    """The app's run file, or None while the guest has none."""
    code, out, _ = vm.ga_run(["/bin/cat", RUN_FILE])
    return parse_run_file(out) if code == 0 else None


def page_served(get, port: int, path: str, marker: str) -> bool:
    """`get(port, path)` answers 200 with `marker` in the body. A refused or
    half-closed connection is not yet served (the server is still coming up)."""
    try:
        status, body = get(port, path)
    except (OSError, http.client.HTTPException):
        return False
    return status == 200 and marker in body


def wait_page(vm: Vm, run: dict, step: float) -> None:
    """The run file's url, through the forward: 200 and the Home document."""
    path = urlsplit(run["url"]).path or "/"
    until(
        vm,
        f"GET {run['url']} through the forward (200, data-page=\"home\")",
        lambda: page_served(http_get, run["port"], path, 'data-page="home"'),
        step,
    )


def check_kernel(vm: Vm, run: dict, step: float) -> dict:
    """Step 5: `home` `list` over /user/ws answers a `fantastic.home` v1 state."""

    def listed():
        try:
            return asyncio.run(home_call(run["port"], {"type": "list"}))
        except ws_errors() as e:
            retrying("home list", e)  # the forward or the kernel is not up yet
            return None

    state = check_state(until(vm, "home list over /user/ws", listed, step))
    log(f"step 5: home list answers fantastic.home v1 (seq {state.get('seq')}, {len(state.get('rows', []))} rows)")
    return state


def check_keys(vm: Vm, run: dict, step: float) -> None:
    """Step 6: Ctrl+0 through QEMU reaches `home` as the Home page's `select {id: null}`."""
    try:
        asyncio.run(key_reaches_home(vm, run["port"], step))
    except ws_errors() as e:
        raise Fail(f"the /home/ws watcher: {type(e).__name__}: {e}") from e
    log("step 6: Ctrl+0 reached home as the Home page's select {id: null}")


def check_screen(vm: Vm, name: str) -> tuple[int, int, bytes]:
    """Step 7: the screen is not uniform (and kept as <name>.ppm / .png)."""
    img = vm.screendump(name)
    if is_uniform(img[2]):
        raise Fail(f"{name}: the screen is one uniform colour ({img[0]}x{img[1]})")
    log(f"step 7: {name} is {img[0]}x{img[1]}, not uniform")
    return img


def comm(vm: Vm, pid: int) -> str:
    code, out, _ = vm.ga_run(["/bin/cat", f"/proc/{pid}/comm"])
    return out.strip() if code == 0 else f"(no process {pid})"


# ── the steps ────────────────────────────────────────────────────────────────


def check_session_and_app(vm: Vm, run: dict, step: float) -> None:
    """Step 3.2-3.4: the process, SHELL, the logind session, the app's own lines."""
    pid = run["pid"]
    if (c := comm(vm, pid)) != "fantastic-app":
        raise Fail(f"/proc/{pid}/comm reads {c!r}, not fantastic-app")
    # The home starts each canvas in the login environment its $SHELL prints: a
    # system unit with User= gets SHELL from passwd (useradd --shell /bin/bash).
    code, raw, _ = vm.ga_run(["/bin/cat", f"/proc/{pid}/environ"])
    if code != 0:
        raise Fail(f"/proc/{pid}/environ unreadable")
    shell = check_shell(parse_environ(raw.encode()))
    log(f"step 3: pid {pid} is fantastic-app with SHELL={shell}")

    sid = vm.sh(f"loginctl show-user {KIOSK_USER} -p Display --value").strip()
    if not sid:
        raise Fail(f"logind: {KIOSK_USER} has no display session")
    props = parse_props(vm.sh(f"loginctl show-session {sid} -p Type -p Class -p Seat -p TTY -p Active"))
    check_session(props)
    log(f"step 3: logind session {sid}: {props}")

    def announced() -> bool:
        journal = vm.sh("journalctl -b -o cat --no-pager").splitlines()
        return f"fantastic-app: the home: {run['url']}" in journal and "fantastic-app: the backdrop: none" in journal

    until(vm, "the journal's `fantastic-app: the home:` and `the backdrop: none` lines", announced, step)
    log("step 3: the app announced its home and its backdrop")


def check_canvas(vm: Vm, run: dict, home_img, step: float) -> int:
    """Step 8: a canvas opened from the kiosk runs on the sidecar; the screen follows."""
    vm.sh(f"runuser -u {KIOSK_USER} -- mkdir -p {CANVAS_DIR}")
    try:
        opened = asyncio.run(home_call(run["port"], {"type": "open", "paths": [CANVAS_DIR]}))
    except ws_errors() as e:
        raise Fail(f"home open {CANVAS_DIR}: {type(e).__name__}: {e}") from e
    if isinstance(opened, dict) and "error" in opened:
        raise Fail(f"home open refused {CANVAS_DIR}: {opened['error']}")

    def running():
        try:
            row = find_row(check_state(asyncio.run(home_call(run["port"], {"type": "list"}))), CANVAS_DIR)
        except ws_errors() as e:
            retrying("home list", e)
            return None
        return row if row and row.get("status") == "running" and isinstance(row.get("pid"), int) else None

    row = until(vm, f"{CANVAS_DIR} listed running with a pid", running, step)
    pid = row["pid"]
    if (c := comm(vm, pid)) != "fantastic":
        raise Fail(f"the canvas pid {pid}'s comm is {c!r}, not fantastic (the app's sidecar)")
    log(f"step 8: {CANVAS_DIR} runs as pid {pid} (fantastic, the sidecar)")
    time.sleep(5)
    img = vm.screendump("screen-canvas")
    frac = changed_fraction(home_img, img)
    if frac < MIN_CHANGED:
        raise Fail(f"screen-canvas differs from screen-home in {frac:.2%} of its pixels, wanted at least {MIN_CHANGED:.0%}")
    log(f"step 8: the screen changed with the canvas page: {frac:.1%} of the pixels")
    return pid


def check_restart(vm: Vm, run: dict, canvas_pid: int, step: float) -> None:
    """Step 9: SIGTERM to the app; the kiosk, the canvas and the page checks come back."""
    at("9, restart")
    n0 = int(parse_props(vm.sh("systemctl show -p NRestarts fantastic-kiosk.service"))["NRestarts"])
    vm.sh(f"kill -TERM {run['pid']}")
    log(f"step 9: SIGTERM to the app (pid {run['pid']}); NRestarts was {n0}")

    def renewed():
        r = read_run_file(vm)
        return r if r and r["pid"] != run["pid"] else None

    new = until(vm, "a new run file (another pid) after SIGTERM", renewed, step)
    n1 = int(parse_props(vm.sh("systemctl show -p NRestarts fantastic-kiosk.service"))["NRestarts"])
    if n1 != n0 + 1:
        raise Fail(f"NRestarts is {n1}, wanted {n0 + 1}")
    if vm.ga_run(["/bin/sh", "-c", f"test ! -e /proc/{run['pid']}"])[0] != 0:
        raise Fail(f"the old app (pid {run['pid']}) is still there")
    if (c := comm(vm, new["pid"])) != "fantastic-app":
        raise Fail(f"the new pid {new['pid']}'s comm is {c!r}, not fantastic-app")
    log(f"step 9: the kiosk came back: pid {new['pid']}, NRestarts {n1}")

    at("9, after the restart: 4")
    vm.forward(new["port"])
    wait_page(vm, new, step)
    at("9, after the restart: 5")
    check_kernel(vm, new, step)
    at("9, after the restart: canvas adopted")
    # the canvas outlived the kiosk (KillUserProcesses=no), and the new app adopted it
    try:
        state = check_state(asyncio.run(home_call(new["port"], {"type": "list"})))
    except ws_errors() as e:
        raise Fail(f"home list after the restart: {type(e).__name__}: {e}") from e
    row = find_row(state, CANVAS_DIR)
    if not row or row.get("status") != "running" or row.get("pid") != canvas_pid:
        raise Fail(f"after the restart {CANVAS_DIR} is {row}, wanted running with pid {canvas_pid}")
    log(f"step 9: the canvas still runs as pid {canvas_pid}, adopted by the new app")
    at("9, after the restart: 6")
    check_keys(vm, new, step)
    at("9, after the restart: 7")
    check_screen(vm, "screen-restart")


def diagnostics(vm: Vm) -> None:
    """Step 10, on failure only: what the guest says, kept beside the screenshots."""
    for name, script in {
        "journal.txt": "journalctl -b --no-pager",
        "kiosk.txt": "systemctl status fantastic-kiosk --no-pager",
        "sessions.txt": "loginctl list-sessions --no-pager",
        "home.txt": f"ls -laR {KIOSK_HOME}/.fantastic/app",
        "canvas-logs.txt": f"tail -n +1 {KIOSK_HOME}/.fantastic/logs/* {CANVAS_DIR}/.fantastic/logs/*",
        # what is still alive, and where it waits: step 9 found the run file gone
        # but cage up and no restart, with nothing in the journal to say why
        "processes.txt": "ps -eo pid,ppid,stat,wchan:32,etime,args --forest",
        "cgroups.txt": "systemd-cgls --no-pager; loginctl session-status --no-pager",
        # who still holds a connection to cage (its Wayland socket's peers)
        "sockets.txt": (
            "ss -xp; for p in $(pgrep -f 'cage|fantastic'); do [ \"$p\" = $$ ] && continue;"
            " echo \"== $p $(tr '\\0' ' ' < /proc/$p/cmdline)\"; ls -l /proc/$p/fd; done"
        ),
        "stacks.txt": (
            "for p in $(pgrep -f 'fantastic-app|WebKit|cage'); do [ \"$p\" = $$ ] && continue;"
            " echo \"== $p $(tr '\\0' ' ' < /proc/$p/cmdline)\";"
            " grep -E '^(State|Threads|SigCgt|SigBlk|SigPnd|ShdPnd):' /proc/$p/status;"
            " for t in /proc/$p/task/*; do echo \"-- $t $(cat $t/comm) $(cat $t/wchan)\"; done;"
            " done"
        ),
    }.items():
        try:
            _, out, err = vm.ga_run(["/bin/sh", "-c", script])
            (vm.out / name).write_text(out + err, encoding="utf-8")
        except (Fail, OSError) as e:
            log(f"diagnostics: {name}: {e}")


def run_check(vm: Vm, timeout: float, step: float) -> None:
    at("1")
    log("step 1: starting QEMU")
    vm.start()
    at("2")
    log("step 2: waiting for the guest agent")
    vm.wait_agent(timeout)
    log("step 2: the guest agent answers")

    at("3")
    run = until(vm, f"the run file {RUN_FILE}", lambda: read_run_file(vm), timeout)
    log(f"step 3: run file: {run}")
    check_session_and_app(vm, run, step)

    at("4")
    vm.forward(run["port"])
    wait_page(vm, run, step)
    log("step 4: the Home document is served through the forward")
    at("5")
    check_kernel(vm, run, step)
    at("6")
    check_keys(vm, run, step)
    at("7")
    home_img = check_screen(vm, "screen-home")

    at("8")
    canvas_pid = check_canvas(vm, run, home_img, step)
    check_restart(vm, run, canvas_pid, step)


def main() -> int:
    global _log_file
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--image", type=Path, required=True)
    ap.add_argument("--arch", choices=["x86-64", "arm64"], required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--timeout", type=float, default=600, help="the boot, up to the run file (s)")
    ap.add_argument("--step", type=float, default=90, help="each later wait (s)")
    a = ap.parse_args()
    if not a.image.is_file():
        print(f"boot_check: no image at {a.image}", file=sys.stderr)
        return 2
    a.out.mkdir(parents=True, exist_ok=True)
    _log_file = a.out / "boot_check.log"
    vm = Vm(a.image.resolve(), a.arch, a.out.resolve())
    try:
        run_check(vm, a.timeout, a.step)
    except Fail as e:
        log(f"FAIL at step {STEP}: {e}")
        diagnostics(vm)
        return 1
    except Exception as e:  # a bug here is a failure too, with its diagnostics
        log(f"FAIL at step {STEP}: {type(e).__name__}: {e}")
        diagnostics(vm)
        return 1
    finally:
        vm.quit()
    log("PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
