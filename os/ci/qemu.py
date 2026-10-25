#!/usr/bin/env python3
"""QEMU and the kernel's sockets, for the fantastic OS checks (Phase 9, #54; Phase 10, #55).

Not run by itself: `boot_check.py` (the kiosk) and `e2e_system.py` (the system
canvas) import it.

  Vm          QEMU and everything that talks to it: QMP, the guest agent, the
              forwarders. `start(audio=True)` adds a sound card.
  forwards    `hostfwd` reaches the guest's NIC, never its loopback, so a socat in
              the guest listens on 18765 (HOME_FORWARD: the home kernel) and 18766
              (SYSTEM_FORWARD: the system canvas), each behind its `hostfwd`.
  the /user/ws client   `http_get`, `ws_connect`, `forward_call`: requests name the
              guest's port (`Host: 127.0.0.1:<port>`) and are dialled at the forward;
              no `Origin` is sent (a browser page's is checked by the guard).

Stdlib plus `websockets` (imported in the socket helpers only, so the pure helpers
import anywhere: see test_qemu.py). It imports nothing from `e2e/`.
"""

from __future__ import annotations

import base64
import http.client
import itertools
import json
import asyncio
import select
import secrets
import shutil
import socket
import subprocess
import time
from pathlib import Path

# the guest-side forwarders' ports, and the host's ends of QEMU's hostfwd
HOME_FORWARD = 18765
SYSTEM_FORWARD = 18766
# how often wait_agent() offers the guest agent a new guest-sync, in s
SYNC_EVERY = 2.0


class Fail(Exception):
    """The first failure ends the run, with one line naming the step."""


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


def audio_argv(arch: str) -> list[str]:
    """The sound device, a null backend (the issue's PR C): the guest sees a card, nothing plays."""
    if arch == "x86-64":
        return ["-audiodev", "none,id=snd0", "-device", "intel-hda", "-device", "hda-duplex,audiodev=snd0"]
    if arch == "arm64":
        return ["-audiodev", "none,id=snd0", "-device", "virtio-sound-pci,audiodev=snd0"]
    raise ValueError(f"unknown arch {arch!r}")


def qemu_argv(arch: str, image: Path, out: Path, audio: bool = False) -> list[str]:
    """The QEMU command line (the issue's §5.1). `snapshot=on` keeps the image pristine.

    The NIC forwards the home's port (HOME_FORWARD) and the system canvas's
    (SYSTEM_FORWARD); `audio` adds a sound card."""
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
        "-nic", "user,model=virtio-net-pci,"
        + ",".join(f"hostfwd=tcp:127.0.0.1:{p}-:{p}" for p in (HOME_FORWARD, SYSTEM_FORWARD)),
        "-qmp", f"unix:{qmp},server=on,wait=off",
        "-chardev", f"socket,id=qga0,path={qga},server=on,wait=off",
        "-device", "virtio-serial-pci",
        "-device", "virtserialport,chardev=qga0,name=org.qemu.guest_agent.0",
        "-serial", f"file:{out / 'serial.log'}",
    ] + (audio_argv(arch) if audio else [])  # fmt: skip


def vars_firmware(arch: str) -> Path:
    """The firmware's variable store QEMU writes to (copied into --out)."""
    return Path("/usr/share/OVMF/OVMF_VARS_4M.fd" if arch == "x86-64" else "/usr/share/AAVMF/AAVMF_VARS.fd")


# ── the log ──────────────────────────────────────────────────────────────────

T0 = time.monotonic()
_log_file: Path | None = None


def set_log_file(path: Path | None) -> None:
    """Every `log` line is also appended to `path`."""
    global _log_file
    _log_file = path


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
        # the guest's socat, by the port it listens on (HOME_FORWARD, SYSTEM_FORWARD): its pid
        self.forwarders: dict[int, int] = {}

    # -- QEMU --
    def start(self, audio: bool = False) -> None:
        """Start QEMU; `audio` gives the guest a sound card."""
        shutil.copy(vars_firmware(self.arch), self.out / vars_firmware(self.arch).name)
        argv = qemu_argv(self.arch, self.image, self.out, audio)
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

    # -- the kernel ports --
    def forward(self, port: int, via: int) -> None:
        """socat in the guest: 0.0.0.0:`via` -> the kernel's loopback `port`; one per `via`
        (HOME_FORWARD for the home, SYSTEM_FORWARD for the system canvas). A second
        forward on the same `via` replaces the first.

        QEMU's hostfwd delivers to the guest's NIC address, never to its loopback."""
        if via in self.forwarders:
            self.sh(f"kill {self.forwarders.pop(via)}")
        self.forwarders[via] = self.ga_spawn(["/usr/bin/socat", f"TCP-LISTEN:{via},fork,reuseaddr", f"TCP:127.0.0.1:{port}"])
        log(f"forwarder: host 127.0.0.1:{via} -> guest 127.0.0.1:{port} (socat pid {self.forwarders[via]})")


# ── the home kernel, through the forward ─────────────────────────────────────

_call_ids = itertools.count(1)


def http_get(port: int, path: str, *, connect: int = HOME_FORWARD, authority: str | None = None) -> tuple[int, str]:
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
    port: int, path: str, *, connect: int = HOME_FORWARD, authority: str | None = None, origin: str | None = None
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


async def forward_call(
    port: int,
    target: str,
    payload: dict,
    timeout: float = 10,
    *,
    connect: int = HOME_FORWARD,
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


