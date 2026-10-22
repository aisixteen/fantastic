"""The e2e harness's one Chrome launcher (`e2e/helpers/chrome.py`, #97): a Chrome
that exits fails at once with its status and output; a Chrome that never answers
fails at the deadline with the last poll's error and that output; a poll's
HTTP error is retried; any other error is a bug and propagates; and the output
file is closed on every path.

The process is a fake: `subprocess.Popen` is replaced, so no Chrome runs.
"""

from __future__ import annotations

import http.client
import subprocess
import sys
from pathlib import Path

import pytest

E2E = (
    next(
        p
        for p in Path(__file__).resolve().parents
        if (p / "e2e" / ".e2e-root").exists()
    )
    / "e2e"
)
sys.path.insert(0, str(E2E))

from helpers import chrome as launch  # noqa: E402
from helpers.chrome import TAIL, Chrome  # noqa: E402

ARGV = ["/fake/Chrome", "--remote-debugging-port=9222", "about:blank"]


class FakeProc:
    """Runs until its `poll` has been called `exit_after` times, then exits with `status`."""

    def __init__(self, exit_after: int | None = None, status: int = 0):
        self.exit_after = exit_after
        self.status = status
        self.polls = 0
        self.returncode: int | None = None

    def poll(self) -> int | None:
        self.polls += 1
        if self.exit_after is not None and self.polls > self.exit_after:
            self.returncode = self.status
        return self.returncode

    def terminate(self) -> None:
        self.returncode = -15


@pytest.fixture
def fake_chrome(monkeypatch):
    """Patches `subprocess.Popen`: returns a dict whose `proc` the fake creates,
    `output` is what it writes to Chrome's stdout, and `stdout` is the file it got."""
    seen: dict = {"output": "", "proc": None}

    def install(exit_after=None, status=0, output="", fail: Exception | None = None):
        seen["output"] = output

        def popen(argv, stdout, stderr):
            assert argv == ARGV
            assert stderr == subprocess.STDOUT  # Chrome's stderr goes to the same file
            seen["stdout"] = stdout
            if fail is not None:
                raise fail
            stdout.write(seen["output"].encode())
            stdout.flush()
            seen["proc"] = FakeProc(exit_after, status)
            return seen["proc"]

        monkeypatch.setattr(launch.subprocess, "Popen", popen)
        return seen

    return install


def launch_fake(tmp_path: Path, **kw) -> Chrome:
    return Chrome(list(ARGV), tmp_path / "chrome.log", 9222, **kw)


def test_the_output_file_is_closed_once_chrome_is_started(tmp_path, fake_chrome):
    seen = fake_chrome(output="hello\n")
    c = launch_fake(tmp_path)
    assert seen["stdout"].closed
    assert c.out.read_text() == "hello\n"
    assert c.proc is seen["proc"]


def test_the_output_file_is_closed_when_chrome_cannot_start(tmp_path, fake_chrome):
    seen = fake_chrome(fail=OSError("no such file"))
    with pytest.raises(OSError, match="no such file"):
        launch_fake(tmp_path)
    assert seen["stdout"].closed


def test_a_chrome_that_exits_early_fails_at_once_with_its_status_and_output(
    tmp_path, fake_chrome
):
    lines = "".join(f"line {i}\n" for i in range(TAIL + 5))
    fake_chrome(exit_after=0, status=21, output=lines)
    c = launch_fake(tmp_path)
    probes = []

    def probe():
        probes.append(1)
        raise AssertionError("probe must not run once Chrome has exited")

    with pytest.raises(SystemExit) as e:
        c.wait_port(probe, timeout=60)
    msg = str(e.value)
    assert probes == []
    assert "Chrome exited with status 21 before opening its debugging port" in msg
    assert "command: /fake/Chrome --remote-debugging-port=9222 about:blank" in msg
    assert "last poll error: no poll yet" in msg
    # the last TAIL lines of the output, and only those
    assert "    line 5\n" in msg + "\n"
    assert "    line 4\n" not in msg + "\n"
    assert msg.endswith("    line 44")


def test_a_chrome_that_never_answers_fails_at_the_deadline_with_the_last_error(
    tmp_path, fake_chrome
):
    fake_chrome(output="DevTools: bind failed: address in use\n")
    c = launch_fake(tmp_path)
    calls = []

    def probe():
        calls.append(1)
        raise ConnectionRefusedError("[Errno 61] refused")

    with pytest.raises(SystemExit) as e:
        c.wait_port(probe, timeout=0.05, interval=0.001)
    msg = str(e.value)
    assert len(calls) >= 2
    assert c.proc.polls >= 2  # Chrome is checked on every poll
    assert msg.startswith("Chrome never opened its debugging port 9222 in 0 s")
    assert "last poll error: ConnectionRefusedError: [Errno 61] refused" in msg
    assert "    DevTools: bind failed: address in use" in msg


def test_a_chrome_that_exits_while_the_deadline_passes_says_it_exited(
    tmp_path, fake_chrome
):
    fake_chrome(exit_after=1, status=3)
    c = launch_fake(tmp_path)

    with pytest.raises(SystemExit) as e:
        c.wait_port(lambda: (_ for _ in ()).throw(OSError("down")), timeout=0)
    msg = str(e.value)
    assert "Chrome exited with status 3" in msg
    assert "last poll error: OSError: down" in msg


def test_a_poll_that_answers_after_http_and_socket_errors_returns_its_value(
    tmp_path, fake_chrome
):
    fake_chrome()
    c = launch_fake(tmp_path)
    failures = [
        http.client.IncompleteRead(b""),
        ConnectionResetError("reset"),
        KeyError("webSocketDebuggerUrl"),
    ]
    calls = []

    def probe():
        calls.append(1)
        if failures:
            raise failures.pop(0)
        return "ws://127.0.0.1:9222/devtools/page/1"

    assert (
        c.wait_port(probe, timeout=5, interval=0.001)
        == "ws://127.0.0.1:9222/devtools/page/1"
    )
    assert len(calls) == 4


def test_a_poll_that_answers_at_once_is_not_retried(tmp_path, fake_chrome):
    fake_chrome()
    c = launch_fake(tmp_path)
    calls = []

    def probe():
        calls.append(1)
        return 7

    assert c.wait_port(probe, timeout=5) == 7
    assert calls == [1]


def test_an_error_that_is_not_a_poll_failure_propagates(tmp_path, fake_chrome):
    fake_chrome()
    c = launch_fake(tmp_path)

    def probe():
        raise TypeError("a bug in the probe")

    with pytest.raises(TypeError, match="a bug in the probe"):
        c.wait_port(probe, timeout=5, interval=0.001)


def test_the_evidence_says_when_the_output_cannot_be_read(tmp_path, fake_chrome):
    fake_chrome(exit_after=0, status=1)
    c = launch_fake(tmp_path)
    c.out.unlink()
    msg = c.evidence("gone")
    assert msg.splitlines()[0] == "gone"
    assert "(output unreadable:" in msg
    assert "(last 1 lines)" in msg


def test_an_empty_output_file_is_shown_as_no_lines(tmp_path, fake_chrome):
    fake_chrome(output="")
    c = launch_fake(tmp_path)
    msg = c.evidence("silent")
    assert "(last 0 lines)" in msg
    assert "last poll error" not in msg
