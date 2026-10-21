"""Unit tests of container_check.py's pure helpers (stdlib unittest, plain python3).

    python3 -m unittest discover -s os/ci -p 'test_*.py'

The engine and the server are not run here: the CI job and a local podman run do
that. Here: the run command (the port map is loopback only, the volumes are the
state), the stop verdict, the exit-code parser, the terminal's output matcher, and
the page check's reuse of boot_check's helper.
"""

import unittest
from unittest import mock

import boot_check as bc
import container_check as cc


class RunArgv(unittest.TestCase):
    def test_docker_run_maps_loopback_only(self):
        argv = cc.run_argv("docker", "img:t", "n1", 40123, "st", "wk")
        self.assertEqual(argv[:2], ["docker", "run"])
        self.assertIn("127.0.0.1:40123:8088", argv)
        self.assertFalse(any(a.startswith("0.0.0.0") or a == "40123:8088" for a in argv), "never published on a public interface")

    def test_both_state_volumes_and_the_image_last(self):
        argv = cc.run_argv("podman", "img:t", "n1", 1, "st", "wk")
        self.assertIn(f"st:{cc.STATE_DIR}", argv)
        self.assertIn(f"wk:{cc.WORK_DIR}", argv)
        self.assertEqual(argv[-1], "img:t")
        self.assertIn("--detach", argv)
        self.assertEqual(argv[argv.index("--name") + 1], "n1")

    def test_unknown_engine_is_refused(self):
        with self.assertRaisesRegex(ValueError, "docker or podman"):
            cc.run_argv("nerdctl", "img", "n", 1, "s", "w")


class StopVerdict(unittest.TestCase):
    def test_orderly_exit_within_the_timeout_passes(self):
        self.assertIn("exited 0", cc.stop_verdict(0, 1.5, 30))

    def test_sigkill_is_refused(self):
        with self.assertRaisesRegex(bc.Fail, "exited 137 on stop"):
            cc.stop_verdict(137, 10.0, 30)

    def test_a_stop_at_the_timeout_is_refused(self):
        with self.assertRaisesRegex(bc.Fail, "at or past"):
            cc.stop_verdict(0, 30.0, 30)

    def test_a_nonzero_exit_is_refused(self):
        with self.assertRaisesRegex(bc.Fail, "exited 2"):
            cc.stop_verdict(2, 1.0, 30)


class ExitCode(unittest.TestCase):
    def test_reads_an_integer(self):
        self.assertEqual(cc.parse_exit_code("0\n"), 0)
        self.assertEqual(cc.parse_exit_code(" 137 "), 137)

    def test_not_a_number_is_refused(self):
        with self.assertRaisesRegex(bc.Fail, "not a number"):
            cc.parse_exit_code("<no value>")


class TerminalOutput(unittest.TestCase):
    def test_the_expansion_counts(self):
        self.assertTrue(cc.terminal_printed("$ echo smoke-$((6*7))\r\nsmoke-42\r\n$ "))

    def test_the_typed_line_alone_does_not_count(self):
        self.assertFalse(cc.terminal_printed("$ echo smoke-$((6*7))\r\n"))

    def test_empty_output_does_not_count(self):
        self.assertFalse(cc.terminal_printed(""))


class Ports(unittest.TestCase):
    def test_free_port_is_a_loopback_port_number(self):
        p = cc.free_port()
        self.assertTrue(1 <= p <= 65535)

    def test_the_page_check_dials_the_port_map_with_the_container_authority(self):
        seen = []

        def fake_get(port, path, *, connect, authority):
            seen.append((port, path, connect, authority))
            return 200, cc.CANVAS_TITLE

        with mock.patch.object(bc, "http_get", fake_get):
            self.assertTrue(bc.page_served(cc.get_for(40123), cc.CONTAINER_PORT, cc.CANVAS_PAGE, cc.CANVAS_TITLE))
        self.assertEqual(seen, [(cc.CONTAINER_PORT, cc.CANVAS_PAGE, 40123, f"0.0.0.0:{cc.CONTAINER_PORT}")])

    def test_authority_is_the_bind_address_the_guard_accepts(self):
        self.assertEqual(cc.AUTHORITY, "0.0.0.0:8088")


if __name__ == "__main__":
    unittest.main()
