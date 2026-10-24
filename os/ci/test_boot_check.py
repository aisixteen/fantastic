"""Unit tests of boot_check.py's pure helpers (stdlib unittest, plain python3).

    python3 -m unittest discover -s os/ci -p 'test_*.py'

The QEMU side is covered by the boot itself; here: the PPM reader and the two
screen checks, the run-file parser, the frame matchers, the logind and
environment checks, the QEMU command line, and the QMP / guest-agent channel
against a fake agent on a unix socket.
"""

import base64
import contextlib
import itertools
import json
import socket
import http.client
import tempfile
import threading
import time
import unittest
from unittest import mock
from pathlib import Path

import boot_check as bc


def ppm(w: int, h: int, pixels: bytes, header: bytes = b"P6\n") -> bytes:
    return header + f"{w} {h}\n255\n".encode() + pixels


class Ppm(unittest.TestCase):
    def test_reads_header_and_pixels(self):
        px = bytes(range(12))
        self.assertEqual(bc.parse_ppm(ppm(2, 2, px)), (2, 2, px))

    def test_comment_in_header(self):
        px = b"\x01\x02\x03"
        self.assertEqual(bc.parse_ppm(ppm(1, 1, px, b"P6\n# made by qemu\n")), (1, 1, px))

    def test_pixel_bytes_that_look_like_whitespace_are_pixels(self):
        px = b"\n\n\n \x00 "
        self.assertEqual(bc.parse_ppm(ppm(2, 1, px)), (2, 1, px))

    def test_refused(self):
        for bad, why in [
            (b"P5\n1 1\n255\n\x00", "magic"),
            (ppm(1, 1, b"\x00\x00"), "short"),
            (ppm(1, 1, b"\x00" * 6), "long"),
            (b"P6\n1 1\n65535\n" + b"\x00" * 6, "maxval"),
            (b"P6\n0 1\n255\n", "size"),
            (b"P6\n1", "truncated"),
            (b"P6\nx 1\n255\n\x00\x00\x00", "not a number"),
        ]:
            with self.subTest(why), self.assertRaises(ValueError):
                bc.parse_ppm(bad)


class Screen(unittest.TestCase):
    def test_uniform(self):
        self.assertTrue(bc.is_uniform(b"\x10\x20\x30" * 5))
        self.assertTrue(bc.is_uniform(b"\x00\x00\x00"))

    def test_one_odd_pixel_is_not_uniform(self):
        self.assertFalse(bc.is_uniform(b"\x10\x20\x30" * 4 + b"\x10\x20\x31"))

    def test_uniform_needs_whole_pixels(self):
        for bad in (b"", b"\x00\x00"):
            with self.assertRaises(ValueError):
                bc.is_uniform(bad)

    def test_changed_fraction(self):
        a = (2, 2, b"\x00\x00\x00" * 4)
        self.assertEqual(bc.changed_fraction(a, a), 0.0)
        one = (2, 2, b"\x00\x00\x00" * 3 + b"\x00\x00\x01")
        self.assertEqual(bc.changed_fraction(a, one), 0.25)
        every = (2, 2, b"\xff\xff\xff" * 4)
        self.assertEqual(bc.changed_fraction(a, every), 1.0)

    def test_changed_fraction_counts_pixels_not_bytes(self):
        a = (1, 2, b"\x00\x00\x00\x00\x00\x00")
        b = (1, 2, b"\x01\x02\x03\x00\x00\x00")
        self.assertEqual(bc.changed_fraction(a, b), 0.5)

    def test_sizes_must_match(self):
        with self.assertRaises(ValueError):
            bc.changed_fraction((1, 1, b"\x00" * 3), (2, 1, b"\x00" * 6))


class RunFile(unittest.TestCase):
    def test_unquoted(self):
        self.assertEqual(
            bc.parse_run_file("pid: 4242\nport: 8123\nurl: http://127.0.0.1:8123/\n"),
            {"pid": 4242, "port": 8123, "url": "http://127.0.0.1:8123/"},
        )

    def test_quoted(self):
        self.assertEqual(
            bc.parse_run_file("pid: '7'\nport: \"9000\"\nurl: \"http://127.0.0.1:9000/home.html\"\n"),
            {"pid": 7, "port": 9000, "url": "http://127.0.0.1:9000/home.html"},
        )

    def test_a_missing_key_is_refused(self):
        for text in ("port: 1\nurl: http://127.0.0.1:1/\n", "pid: 1\nurl: http://127.0.0.1:1/\n", "pid: 1\nport: 1\n", ""):
            with self.subTest(text), self.assertRaises(ValueError):
                bc.parse_run_file(text)

    def test_an_empty_value_is_a_missing_key(self):
        with self.assertRaises(ValueError):
            bc.parse_run_file("pid: 1\nport: 1\nurl:\n")

    def test_url_must_be_the_loopback_port(self):
        for url in ("http://localhost:1/", "http://127.0.0.1:2/", "https://127.0.0.1:1/", "http://127.0.0.1:1"):
            with self.subTest(url), self.assertRaises(ValueError):
                bc.parse_run_file(f"pid: 1\nport: 1\nurl: {url}\n")

    def test_numbers_must_be_numbers(self):
        with self.assertRaises(ValueError):
            bc.parse_run_file("pid: x\nport: 1\nurl: http://127.0.0.1:1/\n")


class Frames(unittest.TestCase):
    def test_select_home_event(self):
        self.assertTrue(bc.is_select_home({"type": "event", "payload": {"type": "select", "id": None}}))

    def test_other_events_do_not_match(self):
        for frame in (
            {"type": "event", "payload": {"type": "select", "id": "g-1"}},
            {"type": "event", "payload": {"type": "state", "seq": 1}},
            {"type": "event", "payload": {"type": "select", "id": None, "extra": 1}},
            {"type": "event", "payload": {"type": "select"}},
            {"type": "reply", "payload": {"type": "select", "id": None}},
            {"type": "event"},
            ["event"],
            None,
        ):
            with self.subTest(frame):
                self.assertFalse(bc.is_select_home(frame))

    def test_reply_data(self):
        self.assertEqual(bc.reply_data({"type": "reply", "id": "a", "data": {"x": 1}}, "a"), {"x": 1})

    def test_other_frames_are_not_the_reply(self):
        for frame in ({"type": "reply", "id": "b", "data": 1}, {"type": "event", "id": "a"}, {"type": "reply"}, [], None):
            with self.subTest(frame):
                self.assertIsNone(bc.reply_data(frame, "a"))

    def test_an_error_frame_for_the_call_fails(self):
        with self.assertRaises(bc.Fail):
            bc.reply_data({"type": "error", "id": "a", "data": "no"}, "a")

    def test_state(self):
        state = {"type": "state", "protocol": "fantastic.home", "version": 1, "seq": 3, "rows": []}
        self.assertIs(bc.check_state(state), state)

    def test_not_a_v1_state(self):
        for bad in (
            None,
            "x",
            {"error": "no"},
            {"type": "state", "protocol": "fantastic.home", "version": 1, "error": "x"},
            {"type": "state", "protocol": "fantastic.home", "version": 2},
            {"type": "state", "protocol": "other", "version": 1},
            {"type": "reply", "protocol": "fantastic.home", "version": 1},
        ):
            with self.subTest(bad), self.assertRaises(bc.Fail):
                bc.check_state(bad)

    def test_find_row(self):
        state = {"rows": [{"path": "/a", "status": "stopped"}, {"path": "/b", "status": "running", "pid": 5}]}
        self.assertEqual(bc.find_row(state, "/b")["pid"], 5)
        self.assertIsNone(bc.find_row(state, "/c"))
        self.assertIsNone(bc.find_row({}, "/a"))


class Guest(unittest.TestCase):
    GOOD = {"Type": "wayland", "Class": "user", "Seat": "seat0", "TTY": "tty1", "Active": "yes"}

    def test_props(self):
        self.assertEqual(bc.parse_props("Type=wayland\nTTY=tty1\nNRestarts=2\nnoise\nA=b=c\n"), {"Type": "wayland", "TTY": "tty1", "NRestarts": "2", "A": "b=c"})

    def test_session_ok(self):
        bc.check_session(self.GOOD)
        bc.check_session({**self.GOOD, "Extra": "x"})

    def test_session_wrong(self):
        for key, value in [("Type", "tty"), ("Class", "greeter"), ("Seat", "seat1"), ("TTY", "tty2"), ("Active", "no")]:
            with self.subTest(key), self.assertRaises(bc.Fail):
                bc.check_session({**self.GOOD, key: value})

    def test_session_missing_key(self):
        bad = dict(self.GOOD)
        del bad["TTY"]
        with self.assertRaises(bc.Fail):
            bc.check_session(bad)

    def test_environ(self):
        raw = b"HOME=/home/fantastic\0SHELL=/bin/bash\0PATH=/usr/bin:/bin\0EMPTY=\0junk\0"
        env = bc.parse_environ(raw)
        self.assertEqual(env["SHELL"], "/bin/bash")
        self.assertEqual(env["EMPTY"], "")
        self.assertNotIn("junk", env)
        self.assertIsNone(bc.parse_environ(b"HOME=/\0").get("SHELL"))


class Qemu(unittest.TestCase):
    def argv(self, arch):
        return bc.qemu_argv(arch, Path("/img/fantastic-os.raw"), Path("/out"))

    def test_x86(self):
        a = self.argv("x86-64")
        self.assertEqual(a[0], "qemu-system-x86_64")
        self.assertIn("q35,accel=kvm:tcg", a)
        self.assertIn("if=virtio,format=raw,file=/img/fantastic-os.raw,snapshot=on", a)
        self.assertIn("if=pflash,format=raw,file=/out/OVMF_VARS_4M.fd", a)
        self.assertIn("virtio-vga", a)
        self.assertIn("usb-kbd", a)

    def test_arm64(self):
        a = self.argv("arm64")
        self.assertEqual(a[0], "qemu-system-aarch64")
        self.assertIn("virt,accel=kvm:tcg", a)
        self.assertIn("if=pflash,format=raw,file=/out/AAVMF_VARS.fd", a)
        self.assertIn("virtio-gpu-pci", a)
        self.assertIn("virtio-keyboard-pci", a)
        self.assertNotIn("usb-kbd", a)

    def test_both(self):
        for arch in ("x86-64", "arm64"):
            a = self.argv(arch)
            with self.subTest(arch):
                self.assertIn(f"user,model=virtio-net-pci,hostfwd=tcp:127.0.0.1:{bc.FORWARD_PORT}-:{bc.FORWARD_PORT}", a)
                self.assertIn("unix:/out/qmp.sock,server=on,wait=off", a)
                self.assertIn("socket,id=qga0,path=/out/qga.sock,server=on,wait=off", a)
                self.assertIn("file:/out/serial.log", a)
                self.assertEqual(a[a.index("-display") + 1], "none")

    def test_unknown_arch(self):
        with self.assertRaises(ValueError):
            self.argv("riscv")

    def test_vars_firmware(self):
        self.assertEqual(bc.vars_firmware("x86-64").name, "OVMF_VARS_4M.fd")
        self.assertEqual(bc.vars_firmware("arm64").name, "AAVMF_VARS.fd")


class FakeGuest:
    """A unix-socket server speaking QMP and the guest agent's protocol."""

    def __init__(self, path: Path, handler):
        self.srv = socket.socket(socket.AF_UNIX)
        self.srv.bind(str(path))
        self.srv.listen(1)
        self.handler = handler
        self.seen = []
        threading.Thread(target=self.serve, daemon=True).start()

    def serve(self):
        conn, _ = self.srv.accept()
        self.handler(conn, self.seen)
        conn.close()

    def close(self):
        self.srv.close()


def lines(conn):
    buf = b""
    while True:
        chunk = conn.recv(65536)
        if not chunk:
            return
        buf += chunk
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            yield json.loads(line)


def send(conn, obj):
    conn.sendall(json.dumps(obj).encode() + b"\n")


def guest_replies(replies):
    """A guest agent: answers guest-sync, and each other command with replies(m) (None: silence)."""

    def handler(conn, seen):
        for m in lines(conn):
            seen.append(m)
            if m["execute"] == "guest-sync":
                send(conn, {"return": m["arguments"]["id"]})
            elif (reply := replies(m)) is not None:
                send(conn, reply)

    return handler


def agent(conn, seen):
    """A guest agent that answers its first sync late, with a stale reply before it."""
    status_calls = 0
    for m in lines(conn):
        seen.append(m)
        cmd = m["execute"]
        if cmd == "guest-sync":
            send(conn, {"return": 1})  # a stale reply to an older sync
            send(conn, {"return": m["arguments"]["id"]})
        elif cmd == "guest-exec":
            send(conn, {"return": {"pid": 77}})
        elif cmd == "guest-exec-status":
            status_calls += 1
            if status_calls == 1:
                send(conn, {"return": {"exited": False}})
            else:
                send(conn, {"return": {"exited": True, "exitcode": 3, "out-data": base64.b64encode(b"out\n").decode(), "err-data": base64.b64encode(b"err\n").decode()}})


def qmp(conn, seen):
    send(conn, {"QMP": {"version": {}}})
    for m in lines(conn):
        seen.append(m)
        if m["execute"] == "send-key":
            send(conn, {"event": "RESET"})  # asynchronous events are not replies
            send(conn, {"return": {}})
        elif m["execute"] == "bad":
            send(conn, {"error": {"class": "GenericError", "desc": "nope"}})
        else:
            send(conn, {"return": {}})


class Channels(unittest.TestCase):
    def setUp(self):
        self.dir = Path(tempfile.mkdtemp(prefix="bc"))
        self.addCleanup(lambda: [p.unlink() for p in self.dir.iterdir()] and self.dir.rmdir())

    def vm(self):
        vm = bc.Vm(Path("/img"), "x86-64", self.dir)
        return vm

    def test_guest_agent(self):
        guest = FakeGuest(self.dir / "qga.sock", agent)
        self.addCleanup(guest.close)
        vm = self.vm()
        vm.wait_agent(10)
        code, out, err = vm.ga_run(["/bin/echo", "x", "y"])
        self.assertEqual((code, out, err), (3, "out\n", "err\n"))
        exec_call = next(m for m in guest.seen if m["execute"] == "guest-exec")
        self.assertEqual(exec_call["arguments"], {"path": "/bin/echo", "arg": ["x", "y"], "capture-output": True})
        self.assertEqual(vm.ga_spawn(["/bin/sleep", "9"]), 77)
        spawn = [m for m in guest.seen if m["execute"] == "guest-exec"][-1]
        self.assertEqual(spawn["arguments"]["capture-output"], False)

    def test_guest_agent_after_a_slow_boot(self):
        # qemu-ga starts late (arm64 on TCG): until then nothing reads the socket,
        # and the syncs fill its buffer (on CI after about 150 s). A small send
        # buffer fills it here within the 2 s; the wait must hold off rather than
        # block on the full socket.
        started = threading.Event()
        real_socket = socket.socket

        def small_socket(*args, **kwargs):
            s = real_socket(*args, **kwargs)
            s.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 1024)
            return s

        def late_agent(conn, seen):
            started.wait(10)
            agent(conn, seen)

        guest = FakeGuest(self.dir / "qga.sock", late_agent)
        self.addCleanup(guest.close)
        vm = self.vm()
        threading.Timer(2.0, started.set).start()
        with mock.patch.object(bc, "SYNC_EVERY", 0.005), mock.patch.object(bc.socket, "socket", small_socket):
            vm.wait_agent(10)
        self.assertTrue(started.is_set())
        self.assertEqual(vm.ga_run(["/bin/true"])[0], 3)

    def test_sh_fails_with_the_output(self):
        guest = FakeGuest(self.dir / "qga.sock", agent)
        self.addCleanup(guest.close)
        vm = self.vm()
        vm.wait_agent(10)
        with self.assertRaisesRegex(bc.Fail, "exited 3"):
            vm.sh("false")

    def test_qmp(self):
        guest = FakeGuest(self.dir / "qmp.sock", qmp)
        self.addCleanup(guest.close)
        vm = self.vm()
        vm.send_keys("ctrl", "0")
        self.assertEqual([m["execute"] for m in guest.seen], ["qmp_capabilities", "send-key"])
        self.assertEqual(
            guest.seen[1]["arguments"],
            {"keys": [{"type": "qcode", "data": "ctrl"}, {"type": "qcode", "data": "0"}]},
        )
        with self.assertRaisesRegex(bc.Fail, "nope"):
            vm.qmp("bad")

    def agent_vm(self, replies, name="qga.sock"):
        guest = FakeGuest(self.dir / name, guest_replies(replies))
        self.addCleanup(guest.close)
        vm = self.vm()
        vm.wait_agent(10)
        self.addCleanup(vm._qga.close)  # ends the fake agent's loop
        return vm

    def test_ga_errors_are_failures(self):
        vm = self.agent_vm(lambda m: {"error": {"desc": "denied"}})
        with self.assertRaisesRegex(bc.Fail, "guest agent guest-exec: .*denied"):
            vm.ga_run(["/bin/true"])

    def test_ga_reply_that_never_comes_fails_in_30_s(self):
        vm = self.agent_vm(lambda m: None)
        real_recv = bc.Chan.recv

        def recv_at_most_0_2_s(self, timeout):
            return real_recv(self, min(timeout, 0.2))

        with mock.patch.object(bc.Chan, "recv", recv_at_most_0_2_s):
            with self.assertRaisesRegex(bc.Fail, "guest agent guest-exec: no reply within 30 s"):
                vm.ga_run(["/bin/true"])

    def test_ga_not_connected(self):
        with self.assertRaisesRegex(bc.Fail, "the guest agent is not connected"):
            self.vm().ga_run(["/bin/true"])

    def test_ga_run_that_never_finishes_fails_at_its_timeout(self):
        def running(m):
            return {"return": {"pid": 5}} if m["execute"] == "guest-exec" else {"return": {"exited": False}}

        vm = self.agent_vm(running)
        with mock.patch.object(bc.time, "sleep"):
            with self.assertRaisesRegex(bc.Fail, "not finished within 0.3 s"):
                vm.ga_run(["/bin/sleep", "9"], timeout=0.3)

    def test_ga_run_killed_by_a_signal_is_its_negative_number(self):
        def killed(m):
            return {"return": {"pid": 5}} if m["execute"] == "guest-exec" else {"return": {"exited": True, "signal": 9}}

        vm = self.agent_vm(killed)
        self.assertEqual(vm.ga_run(["/bin/sleep", "9"]), (-9, "", ""))

    def test_qmp_no_greeting(self):
        guest = FakeGuest(self.dir / "qmp.sock", lambda conn, seen: send(conn, {"return": {}}))
        self.addCleanup(guest.close)
        with self.assertRaisesRegex(bc.Fail, "qmp: no greeting"):
            self.vm().qmp("query-status")

    def test_qmp_no_reply_within_30_s(self):
        def qmp_silent_on_bad(conn, seen):
            send(conn, {"QMP": {"version": {}}})
            for m in lines(conn):
                seen.append(m)
                if m["execute"] != "bad":
                    send(conn, {"return": {}})

        guest = FakeGuest(self.dir / "qmp.sock", qmp_silent_on_bad)
        self.addCleanup(guest.close)
        vm = self.vm()
        vm.qmp("ok")  # the handshake, on the real clock
        ticks = itertools.count()  # each clock read is one second later
        with mock.patch.object(bc.Chan, "recv", lambda self, timeout: None), mock.patch.object(
            bc.time, "monotonic", lambda: float(next(ticks))
        ):
            with self.assertRaisesRegex(bc.Fail, "qmp bad: no reply within 30 s"):
                vm.qmp("bad")

    def test_chan_not_connectable(self):
        with self.assertRaisesRegex(bc.Fail, "missing.sock: not connectable within 0.3 s"):
            bc.Chan(self.dir / "missing.sock", 0.3, lambda: None)


class Framing(unittest.TestCase):
    """Chan's line framing and its close, on a socket pair (no path, no QEMU)."""

    def chan(self, alive=lambda: None):
        a, b = socket.socketpair()
        self.addCleanup(a.close)
        self.addCleanup(b.close)
        ch = bc.Chan.__new__(bc.Chan)  # no connect: the pair's end is the socket
        ch.path, ch.alive, ch.buf, ch.sock = Path("qmp.sock"), alive, b"", a
        return ch, b

    def test_recv_frames_lines_split_across_chunks(self):
        ch, peer = self.chan()

        def write():
            peer.sendall(b'{"retu')
            time.sleep(0.05)
            peer.sendall(b'rn": 1}\n\n{"return": 2}\n{"ret')  # a blank line, two lines, a split one
            time.sleep(0.05)
            peer.sendall(b'urn": 3}\n')

        threading.Thread(target=write, daemon=True).start()
        self.assertEqual(ch.recv(2), {"return": 1})
        self.assertEqual(ch.recv(2), {"return": 2})
        self.assertEqual(ch.recv(2), {"return": 3})

    def test_recv_returns_none_on_timeout(self):
        ch, _ = self.chan()
        self.assertIsNone(ch.recv(0.2))

    def test_a_socket_closed_by_qemu_fails(self):
        alive = mock.Mock()
        ch, peer = self.chan(alive)
        peer.close()
        with self.assertRaisesRegex(bc.Fail, "closed by QEMU"):
            ch.recv(2)
        alive.assert_called()


class Until(unittest.TestCase):
    class Vm:
        def alive(self):
            pass

    def test_returns_the_value(self):
        calls = []
        got = bc.until(self.Vm(), "x", lambda: calls.append(1) or (len(calls) > 2 and "v"), 5, every=0.01)
        self.assertEqual(got, "v")

    def test_fails_naming_the_wait(self):
        with self.assertRaisesRegex(bc.Fail, "never: not within"):
            bc.until(self.Vm(), "never", lambda: None, 0.05, every=0.01)


class Shell(unittest.TestCase):
    def test_bash_passes(self):
        self.assertEqual(bc.check_shell({"SHELL": "/bin/bash"}), "/bin/bash")

    def test_missing_fails(self):
        with self.assertRaisesRegex(bc.Fail, "SHELL is None"):
            bc.check_shell({"HOME": "/home/fantastic"})

    def test_another_shell_fails(self):
        with self.assertRaisesRegex(bc.Fail, "SHELL is '/bin/sh'"):
            bc.check_shell({"SHELL": "/bin/sh"})


class StubVm:
    """The Vm calls the steps make, answered without QEMU."""

    def __init__(self, restarts=("0", "1")):
        self.restarts = list(restarts)
        self.forwarded = []

    def alive(self):
        pass

    def sh(self, script, timeout=60):
        return f"NRestarts={self.restarts.pop(0)}" if "NRestarts" in script else ""

    def ga_run(self, argv, timeout=60):
        return 0, "", ""

    def forward(self, port):
        self.forwarded.append(port)

    def screendump(self, name):
        return 1, 1, b"\0\0\0"


RUN = {"pid": 10, "port": 4000, "url": "http://127.0.0.1:4000/"}


async def refuses(*_a):
    raise ConnectionRefusedError("down")


class Step(unittest.TestCase):
    def test_at_sets_the_step_main_names(self):
        bc.at("9, restart")
        self.assertEqual(bc.STEP, "9, restart")
        bc.at("0")


class WaitPage(unittest.TestCase):
    def test_a_protocol_error_is_not_yet_served(self):
        calls = []

        def get(port, path):
            calls.append(1)
            if len(calls) == 1:
                raise http.client.RemoteDisconnected("closed")
            return 200, '<body data-page="home">'

        with mock.patch.object(bc, "http_get", get), mock.patch.object(bc.time, "sleep"):
            bc.wait_page(StubVm(), RUN, 5)
        self.assertEqual(len(calls), 2)


class CanvasAndRestart(unittest.TestCase):
    def setUp(self):
        # the websockets package is only on the CI job's pip path, not needed to unit-test the Fail
        p = mock.patch.object(bc, "ws_errors", lambda: (OSError,))
        p.start()
        self.addCleanup(p.stop)

    def test_open_refused_by_the_transport_names_the_call(self):
        with mock.patch.object(bc, "home_call", refuses):
            with self.assertRaisesRegex(bc.Fail, "home open .*ConnectionRefusedError"):
                bc.check_canvas(StubVm(), RUN, (1, 1, b"\0\0\0"), 5)

    def test_list_after_the_restart_names_the_call(self):
        new = {"pid": 11, "port": 4001, "url": "http://127.0.0.1:4001/"}
        vm = StubVm()
        with (
            mock.patch.object(bc, "read_run_file", lambda vm: new),
            mock.patch.object(bc, "comm", lambda vm, pid: "fantastic-app"),
            mock.patch.object(bc, "wait_page"),
            mock.patch.object(bc, "check_kernel"),
            mock.patch.object(bc, "home_call", refuses),
        ):
            with self.assertRaisesRegex(bc.Fail, "home list after the restart: ConnectionRefusedError"):
                bc.check_restart(vm, RUN, 99, 5)
        self.assertEqual(vm.forwarded, [4001])

    def test_restart_count_must_rise_by_one(self):
        new = {"pid": 11, "port": 4001, "url": "http://127.0.0.1:4001/"}
        with mock.patch.object(bc, "read_run_file", lambda vm: new):
            with self.assertRaisesRegex(bc.Fail, "NRestarts is 0, wanted 1"):
                bc.check_restart(StubVm(("0", "0")), RUN, 99, 5)

    @contextlib.contextmanager
    def restart_env(self, comm_name="fantastic-app", home=refuses):
        new = {"pid": 11, "port": 4001, "url": "http://127.0.0.1:4001/"}
        with (
            mock.patch.object(bc, "read_run_file", lambda vm: new),
            mock.patch.object(bc, "comm", lambda vm, pid: comm_name),
            mock.patch.object(bc, "wait_page"),
            mock.patch.object(bc, "check_kernel"),
            mock.patch.object(bc, "home_call", home),
        ):
            yield

    @staticmethod
    def home_listing(pid):
        state = {
            "type": "state",
            "protocol": "fantastic.home",
            "version": 1,
            "seq": 4,
            "rows": [{"path": bc.CANVAS_DIR, "status": "running", "pid": pid}],
        }

        async def answer(*_a):
            return state

        return answer

    def test_canvas_must_be_adopted_with_the_same_pid(self):
        with self.restart_env(home=self.home_listing(5)), mock.patch.object(bc, "check_keys") as keys:
            with self.assertRaisesRegex(bc.Fail, "after the restart .* wanted running with pid 99"):
                bc.check_restart(StubVm(), RUN, 99, 5)
            keys.assert_not_called()
        with self.restart_env(home=self.home_listing(99)), mock.patch.object(bc, "check_keys") as keys, mock.patch.object(
            bc, "check_screen"
        ) as screen:
            bc.check_restart(StubVm(), RUN, 99, 5)
        keys.assert_called_once()
        screen.assert_called_once()

    def test_old_app_must_be_gone(self):
        vm = StubVm()
        vm.ga_run = lambda argv, timeout=60: (1, "", "") if argv[-1] == "test ! -e /proc/10" else (0, "", "")
        with self.restart_env(home=self.home_listing(99)):
            with self.assertRaisesRegex(bc.Fail, r"the old app \(pid 10\) is still there"):
                bc.check_restart(vm, RUN, 99, 5)

    def test_new_pid_must_be_fantastic_app(self):
        with self.restart_env(comm_name="bash", home=self.home_listing(99)):
            with self.assertRaisesRegex(bc.Fail, "the new pid 11's comm is 'bash', not fantastic-app"):
                bc.check_restart(StubVm(), RUN, 99, 5)


if __name__ == "__main__":
    unittest.main()
