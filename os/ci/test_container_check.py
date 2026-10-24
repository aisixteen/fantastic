"""Unit tests of container_check.py's pure helpers (stdlib unittest, plain python3).

    python3 -m unittest discover -s os/ci -p 'test_*.py'

The engine and the server are not run here: the CI job and a local podman run do
that. Here: the run command (the port map is loopback only, an identity map, the
volumes are the state), the browser's authority and origin, the stop verdict, the
exit-code parser, the terminal's output matcher, the state marker, the log saving,
the page check's reuse of boot_check's helper, and the HEALTHCHECK's status reading.
"""

import re
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import boot_check as bc
import container_check as cc


class RunArgv(unittest.TestCase):
    def test_docker_run_maps_loopback_only(self):
        argv = cc.run_argv("docker", "img:t", "n1", 40123, "st", "wk")
        self.assertEqual(argv[:2], ["docker", "run"])
        self.assertIn("127.0.0.1:40123:40123", argv)
        self.assertFalse(any(a.startswith("0.0.0.0") or a == "40123:40123" for a in argv), "never published on a public interface")

    def test_the_port_map_is_an_identity_map_and_the_server_listens_on_it(self):
        argv = cc.run_argv("podman", "img:t", "n1", 40123, "st", "wk")
        self.assertIn("FANTASTIC_PORT=40123", argv[argv.index("--env") + 1])
        self.assertEqual(argv[argv.index("--publish") + 1], "127.0.0.1:40123:40123")

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


class BrowserAuthority(unittest.TestCase):
    def test_the_host_is_localhost_on_the_port_the_map_serves(self):
        self.assertEqual(cc.authority_for(40123), "localhost:40123")

    def test_the_origin_is_the_pages_own_origin(self):
        self.assertEqual(cc.origin_for(40123), "http://localhost:40123")

    def test_a_run_dials_its_own_port_with_the_browser_authority(self):
        run = cc.Run("docker", "img", "n", "s", "w")
        self.assertEqual(run.authority, f"localhost:{run.port}")
        self.assertEqual(run.origin, f"http://localhost:{run.port}")

    def test_the_page_check_dials_the_port_map_with_the_browser_authority(self):
        seen = []

        def fake_get(port, path, *, connect, authority):
            seen.append((port, path, connect, authority))
            return 200, cc.CANVAS_TITLE

        run = cc.Run("docker", "img", "n", "s", "w")
        with mock.patch.object(bc, "http_get", fake_get):
            self.assertTrue(bc.page_served(cc.get_for(run), run.port, cc.CANVAS_PAGE, cc.CANVAS_TITLE))
        self.assertEqual(seen, [(run.port, cc.CANVAS_PAGE, run.port, f"localhost:{run.port}")])


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


class StateMarker(unittest.TestCase):
    def test_the_shell_writes_the_marker_under_fantastic_home(self):
        line = cc.home_marker_line()
        self.assertIn(f'"$FANTASTIC_HOME/{cc.HOME_MARKER}"', line)
        self.assertTrue(line.endswith("\n"), "a shell line")

    def test_the_marker_read_back_is_the_text_the_shell_wrote(self):
        self.assertTrue(cc.marker_kept(f"{cc.HOME_MARKER_TEXT}\n"))

    def test_a_missing_or_other_marker_is_not_kept(self):
        self.assertFalse(cc.marker_kept(""), "no file: cat printed nothing")
        self.assertFalse(cc.marker_kept("something else"))


class SaveLogs(unittest.TestCase):
    def test_each_containers_output_is_kept_in_the_out_dir(self):
        run = cc.Run("docker", "img", "fantastic-smoke-x-1", "s", "w")
        done = mock.Mock(stdout="kernel line\n", stderr="warn\n")
        with tempfile.TemporaryDirectory() as d, mock.patch.object(cc.subprocess, "run", return_value=done) as sp:
            run.save_logs(Path(d))
            self.assertEqual(sp.call_args.args[0], ["docker", "logs", "fantastic-smoke-x-1"])
            self.assertEqual((Path(d) / "fantastic-smoke-x-1.log").read_text(), "kernel line\nwarn\n")


class Ports(unittest.TestCase):
    def test_free_port_is_a_loopback_port_number(self):
        p = cc.free_port()
        self.assertTrue(1 <= p <= 65535)


class Health(unittest.TestCase):
    def test_healthy_passes_and_starting_waits(self):
        self.assertTrue(cc.health_ready("healthy"))
        self.assertFalse(cc.health_ready("starting"))
        self.assertFalse(cc.health_ready(""))

    def test_unhealthy_is_a_failure_now(self):
        with self.assertRaisesRegex(bc.Fail, "reports unhealthy"):
            cc.health_ready("unhealthy")


REPO = Path(__file__).resolve().parents[2]


class LocalhostHostHeader(unittest.TestCase):
    def test_check_uses_localhost_host_header(self):
        # the page check dials the port map and sends the Host a browser sends:
        # localhost:<port>, which the guard admits on a wildcard bind (#158 decision 4)
        run = cc.Run("podman", "img", "n", "s", "w")
        self.assertEqual(cc.authority_for(run.port), f"localhost:{run.port}")
        self.assertNotEqual(cc.authority_for(run.port), f"192.168.1.5:{run.port}")


class PublishPushesTestedBytes(unittest.TestCase):
    def workflow(self):
        return (REPO / ".github/workflows/publish-container.yml").read_text()

    def test_publish_workflow_pushes_tested_digest(self):
        text = self.workflow()
        # the publish job must not rebuild the image: no `container.sh --arch all`
        self.assertNotIn("container.sh --arch all", text, "publish rebuilds the image")
        # it takes the per-arch images the CI run's container job saved, and loads them
        self.assertIn("pattern: container-image-*", text)
        self.assertIn("run-id: ${{ needs.guard.outputs.ci_run }}", text)
        self.assertIn("docker load -i", text)
        # the manifest is made from the pushed per-arch images, not built again
        self.assertIn("docker buildx imagetools create", text)

    def test_ci_container_job_saves_the_image_it_smoke_tested(self):
        ci = (REPO / ".github/workflows/ci.yml").read_text()
        self.assertIn("name: container-image-${{ matrix.arch }}", ci)
        self.assertIn("docker save", ci)


class BaseDigestMatchesBuildYml(unittest.TestCase):
    def test_containerfile_base_digest_matches_build_yml(self):
        build = (REPO / ".github/workflows/build.yml").read_text()
        containerfile = (REPO / "os/container/Containerfile").read_text()
        trixie = re.search(r"^\s*TRIXIE_SLIM:\s*(\S+)", build, re.M)
        base = re.search(r"^FROM\s+(\S+)", containerfile, re.M)
        self.assertIsNotNone(trixie, "build.yml names TRIXIE_SLIM")
        self.assertIsNotNone(base, "Containerfile has a FROM")
        self.assertEqual(base.group(1), trixie.group(1))


if __name__ == "__main__":
    unittest.main()
