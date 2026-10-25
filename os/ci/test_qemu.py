"""Unit tests of qemu.py: the QEMU command line (the sound card, the two forwards) and
the guest forwarders (stdlib unittest, plain python3).

    python3 -m unittest discover -s os/ci -p 'test_*.py'

No QEMU runs: `Vm.start` is seen through a fake `Popen`, `Vm.forward` through a fake
guest agent that records what it is asked to run. The channels are test_boot_check.py's.
"""

import shutil
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import qemu as qm


OUT: Path


def setUpModule():
    global OUT
    OUT = Path(tempfile.mkdtemp(prefix="test_qemu_"))


def tearDownModule():
    shutil.rmtree(OUT, ignore_errors=True)


def started_argv(arch: str, **start) -> list[str]:
    """The argv `Vm.start(**start)` hands to Popen."""
    with mock.patch.object(qm.subprocess, "Popen") as popen, mock.patch.object(qm.shutil, "copy"), mock.patch.object(
        qm, "log"
    ):
        qm.Vm(Path("/img/fantastic-os.raw"), arch, OUT).start(**start)
        file = popen.call_args.kwargs["stdout"]
        file.close()  # the qemu.out Vm.start opened for QEMU
        return popen.call_args.args[0]


class FakeGuest(qm.Vm):
    """A Vm whose guest agent only records: what it spawned, what shell it ran."""

    def __init__(self):
        super().__init__(Path("/img"), "x86-64", Path("/out"))
        self.events: list[tuple] = []
        self.next_pid = 100

    def ga_spawn(self, argv):
        self.next_pid += 1
        self.events.append(("spawn", argv, self.next_pid))
        return self.next_pid

    def sh(self, script, timeout=60):
        self.events.append(("sh", script))
        return ""


class Start(unittest.TestCase):
    def test_start_without_audio_has_no_audio_device(self):
        for arch in ("x86-64", "arm64"):
            with self.subTest(arch):
                a = started_argv(arch)
                self.assertNotIn("-audiodev", a)
                self.assertFalse([x for x in a if "hda" in x or "sound" in x], a)
        # and the default is the same as saying so
        self.assertEqual(started_argv("x86-64"), started_argv("x86-64", audio=False))

    def test_start_audio_x86_adds_intel_hda_duplex(self):
        a = started_argv("x86-64", audio=True)
        i = a.index("-audiodev")
        self.assertEqual(
            a[i:],
            ["-audiodev", "none,id=snd0", "-device", "intel-hda", "-device", "hda-duplex,audiodev=snd0"],
        )
        self.assertEqual(a[0], "qemu-system-x86_64")

    def test_start_audio_arm64_adds_virtio_sound_pci(self):
        a = started_argv("arm64", audio=True)
        i = a.index("-audiodev")
        self.assertEqual(a[i:], ["-audiodev", "none,id=snd0", "-device", "virtio-sound-pci,audiodev=snd0"])
        self.assertEqual(a[0], "qemu-system-aarch64")
        self.assertNotIn("intel-hda", a)

    def test_nic_has_both_hostfwds_18765_and_18766(self):
        self.assertEqual((qm.HOME_FORWARD, qm.SYSTEM_FORWARD), (18765, 18766))
        for arch in ("x86-64", "arm64"):
            for audio in (False, True):
                a = qm.qemu_argv(arch, Path("/img/x.raw"), Path("/out"), audio)
                nic = a[a.index("-nic") + 1]
                with self.subTest(arch=arch, audio=audio):
                    self.assertEqual(
                        nic,
                        "user,model=virtio-net-pci,"
                        "hostfwd=tcp:127.0.0.1:18765-:18765,hostfwd=tcp:127.0.0.1:18766-:18766",
                    )

    def test_an_unknown_arch_is_refused_with_or_without_audio(self):
        with self.assertRaises(ValueError):
            qm.qemu_argv("riscv", Path("/i"), Path("/o"), True)
        with self.assertRaises(ValueError):
            qm.audio_argv("riscv")


class Forward(unittest.TestCase):
    def setUp(self):
        p = mock.patch.object(qm, "log")
        p.start()
        self.addCleanup(p.stop)

    def test_forward_spawns_one_socat_per_via_with_the_exact_command(self):
        vm = FakeGuest()
        vm.forward(4242, qm.SYSTEM_FORWARD)
        self.assertEqual(
            vm.events,
            [("spawn", ["/usr/bin/socat", "TCP-LISTEN:18766,fork,reuseaddr", "TCP:127.0.0.1:4242"], 101)],
        )
        self.assertEqual(vm.forwarders, {18766: 101})
        vm.forward(5000, qm.HOME_FORWARD)
        self.assertEqual(
            vm.events[-1],
            ("spawn", ["/usr/bin/socat", "TCP-LISTEN:18765,fork,reuseaddr", "TCP:127.0.0.1:5000"], 102),
        )

    def test_forward_same_via_kills_the_old_socat_first(self):
        vm = FakeGuest()
        vm.forward(4242, qm.HOME_FORWARD)
        vm.forward(4343, qm.HOME_FORWARD)
        kinds = [e[0] for e in vm.events]
        self.assertEqual(kinds, ["spawn", "sh", "spawn"], "the old one is killed before the new one listens")
        self.assertEqual(vm.events[1], ("sh", "kill 101"))
        self.assertEqual(vm.events[2][1][-1], "TCP:127.0.0.1:4343")
        self.assertEqual(vm.forwarders, {18765: 102}, "one socat for the via")

    def test_forward_two_vias_keep_both_listeners(self):
        vm = FakeGuest()
        vm.forward(4242, qm.HOME_FORWARD)
        vm.forward(4343, qm.SYSTEM_FORWARD)
        self.assertEqual([e[0] for e in vm.events], ["spawn", "spawn"], "neither killed the other")
        self.assertEqual(vm.forwarders, {18765: 101, 18766: 102})
        # moving one leaves the other alone
        vm.forward(4444, qm.SYSTEM_FORWARD)
        self.assertEqual(vm.events[-2], ("sh", "kill 102"))
        self.assertEqual(vm.forwarders, {18765: 101, 18766: 103})


if __name__ == "__main__":
    unittest.main()
