"""Unit tests of e2e_system.py's pure helpers (stdlib unittest, plain python3).

    python3 -m unittest discover -s os/ci -p 'test_*.py'

The QEMU run itself is the `os` job's; here: the cgroup and tool-output parsers,
the state checks, the step selection, and the skip when QEMU or the image is missing.
"""

import contextlib
import io
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import e2e_system as es

USER_CG = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/fantastic-system.service\n"


class Parsers(unittest.TestCase):
    def test_cgroup_accepts_only_the_user_unit(self):
        self.assertTrue(es.cgroup_is_unit(USER_CG))
        self.assertEqual(es.parse_cgroup(USER_CG), "fantastic-system.service")
        self.assertFalse(es.cgroup_is_unit("0::/user.slice/user-1000.slice/session-3.scope\n"))
        self.assertFalse(es.cgroup_is_unit("0::/system.slice/fantastic-system.service\n"))
        self.assertFalse(es.cgroup_is_unit("0::/user.slice/user-1000.slice/user@1000.service/app.slice/other.service\n"))
        self.assertFalse(es.cgroup_is_unit("garbage"))
        self.assertFalse(es.cgroup_is_unit(""))

    def test_parse_wpctl_volume_and_mute(self):
        self.assertEqual(es.parse_wpctl("Volume: 0.30\n"), (0.3, False))
        self.assertEqual(es.parse_wpctl("Volume: 0.30 [MUTED]\n"), (0.3, True))
        with self.assertRaises(ValueError):
            es.parse_wpctl("nothing")

    def test_nmcli_states_parse(self):
        self.assertEqual(es.parse_nmcli_states("enp0s2:connected\nlo:unmanaged\n"), {"enp0s2": "connected", "lo": "unmanaged"})
        self.assertEqual(es.parse_nmcli_states(""), {})

    def test_notify_reply_parses_u_id_and_refuses_other_text(self):
        self.assertEqual(es.parse_notify_reply("u 7\n"), 7)
        for bad in ("", "s 7", "u", "u x", "Call failed: no such name"):
            with self.assertRaises(ValueError, msg=bad):
                es.parse_notify_reply(bad)


class States(unittest.TestCase):
    def test_audio_ready_needs_available_and_one_default_sink(self):
        sink = {"default": True}
        self.assertTrue(es.audio_ready({"available": True, "sinks": [sink]}))
        self.assertTrue(es.audio_ready({"sinks": [sink]}))
        self.assertFalse(es.audio_ready({"available": False, "why": "pw-dump took longer than 5 s", "sinks": [sink]}))
        self.assertFalse(es.audio_ready({"available": True, "sinks": []}))
        self.assertFalse(es.audio_ready(None))

    def test_one_default_sink_counts_exactly_one(self):
        sink = lambda d: {"id": 1, "default": d}  # noqa: E731
        self.assertFalse(es.one_default_sink({"sinks": []}))
        self.assertFalse(es.one_default_sink({"sinks": [sink(True), sink(True)]}))
        self.assertFalse(es.one_default_sink({"sinks": [sink(False)]}))
        self.assertFalse(es.one_default_sink({}))
        self.assertTrue(es.one_default_sink({"sinks": [sink(False), sink(True)]}))

    def test_default_volume_reads_the_default_sink(self):
        st = {"sinks": [{"default": False, "volume": 1.0}, {"default": True, "volume": 0.5}]}
        self.assertEqual(es.default_volume(st), 0.5)
        self.assertIsNone(es.default_volume({"sinks": []}))

    def test_ip4_in_checks_the_subnet(self):
        self.assertTrue(es.ip4_in(["10.0.2.15/24"], "10.0.2.0/24"))
        self.assertFalse(es.ip4_in(["192.168.1.5/24"], "10.0.2.0/24"))
        self.assertFalse(es.ip4_in([], "10.0.2.0/24"))
        self.assertFalse(es.ip4_in(["nonsense"], "10.0.2.0/24"))
        self.assertFalse(es.ip4_in(None, "10.0.2.0/24"))

    def test_wifi_row_finds_the_ssid_with_security(self):
        st = {"access_points": [{"ssid": "other", "security": "open"}, {"ssid": "fantastic-ci", "security": "wpa-psk"}]}
        self.assertEqual(es.wifi_row(st, "fantastic-ci")["security"], "wpa-psk")
        self.assertIsNone(es.wifi_row(st, "missing"))
        self.assertIsNone(es.wifi_row({}, "fantastic-ci"))

    def test_displays_ok_accepts_outputs_or_a_why_and_refuses_a_bare_unavailable(self):
        self.assertTrue(es.displays_ok({"available": True, "outputs": [{"name": "Virtual-1"}]}))
        self.assertFalse(es.displays_ok({"available": True, "outputs": []}))
        self.assertTrue(es.displays_ok({"available": False, "why": "no wlr-randr"}))
        self.assertFalse(es.displays_ok({"available": False}))
        self.assertFalse(es.displays_ok({"available": False, "why": "  "}))
        self.assertFalse(es.displays_ok(None))


class Resend(unittest.TestCase):
    def clock(self):
        now = [0.0]

        def sleep(d):
            now[0] += d

        return mock.patch.object(es.time, "monotonic", lambda: now[0]), mock.patch.object(es.time, "sleep", sleep)

    def test_resend_until_sends_again_when_the_check_stays_false(self):
        sends, polls = [], []

        def check():
            polls.append(1)
            return "ok" if len(polls) > 3 else None

        c, s = self.clock()
        with c, s:
            got = es.resend_until(None, "x", lambda: sends.append(1), check, 60, every=2)
        self.assertEqual(got, "ok")
        self.assertGreater(len(sends), 1)

    def test_resend_until_sends_once_when_the_first_check_passes(self):
        sends = []
        c, s = self.clock()
        with c, s:
            self.assertEqual(es.resend_until(None, "x", lambda: sends.append(1), lambda: "yes", 60), "yes")
        self.assertEqual(len(sends), 1)

    def test_resend_until_fails_naming_what_after_the_timeout(self):
        sends = []
        c, s = self.clock()
        with c, s, self.assertRaises(es.Fail) as cm:
            es.resend_until(None, "the thing", lambda: sends.append(1), lambda: None, 20, every=5)
        self.assertIn("not within", str(cm.exception))
        self.assertIn("the thing", str(cm.exception))
        self.assertGreaterEqual(len(sends), 2)

    def test_audio_diag_names_the_wpctl_line_and_survives_a_failing_wpctl(self):
        with mock.patch.object(es, "state_of", return_value={"sinks": [{"default": True, "volume": 0.4}]}):
            with mock.patch.object(es, "as_user", return_value="Volume: 0.40"):
                out = es.audio_diag(None)
            self.assertIn("Volume: 0.40", out)
            self.assertIn("0.4", out)
            with mock.patch.object(es, "as_user", side_effect=es.Fail("exit 2")):
                out = es.audio_diag(None)
            self.assertIn("exit 2", out)

    def test_wpctl_volume_or_none_is_none_when_wpctl_fails_and_parsed_otherwise(self):
        with mock.patch.object(es, "as_user", return_value="Volume: 0.30 [MUTED]\n"):
            self.assertEqual(es.wpctl_volume_or_none(None), (0.3, True))
        with mock.patch.object(es, "as_user", side_effect=es.Fail("exit 1")):
            self.assertIsNone(es.wpctl_volume_or_none(None))


class Run(unittest.TestCase):
    def test_choose_steps_orders_validates_and_puts_system_first(self):
        self.assertEqual(es.choose_steps(None), ["system", "audio", "network", "notifications", "displays"])
        self.assertEqual(es.choose_steps("displays,audio"), ["system", "audio", "displays"])
        self.assertEqual(es.choose_steps("system"), ["system"])
        with self.assertRaises(ValueError):
            es.choose_steps("audio,bogus")

    def test_main_skips_with_exit_zero_when_qemu_or_image_is_missing(self):
        with tempfile.TemporaryDirectory() as d:
            image = Path(d) / "x.raw"
            image.write_bytes(b"")
            for which, img, want in ((None, image, "qemu-system-x86_64 is not on PATH"), ("/usr/bin/q", Path(d) / "none.raw", "no image at")):
                out = io.StringIO()
                with mock.patch("shutil.which", return_value=which), contextlib.redirect_stdout(out):
                    rc = es.main(["--image", str(img), "--arch", "x86-64", "--out", str(Path(d) / "out")])
                self.assertEqual(rc, 0)
                self.assertIn("SKIP e2e_system:", out.getvalue())
                self.assertIn(want, out.getvalue())
            self.assertFalse((Path(d) / "out").exists(), "a skip leaves nothing behind")

    def test_main_refuses_an_unknown_step(self):
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            rc = es.main(["--image", "x", "--arch", "arm64", "--out", "y", "--only", "nope"])
        self.assertEqual(rc, 2)


if __name__ == "__main__":
    unittest.main()
