"""The canvas e2e runner's `attach()` (#97) waits for Chrome's page target and
connects to it. Every poll checks Chrome first: a Chrome that has exited fails
at once with its status and output; a Chrome still unattached at the deadline
fails with the last poll's error and that output; the HTTP and socket errors
of a Chrome not up yet are retried; any other error propagates.

`e2e/runners/canvas.py` is a script: importing it points FANTASTIC_HOME at a
throwaway home and puts the e2e tree on `sys.path`. So it is driven in its own
interpreter, with `subprocess.Popen`, `urllib.request.urlopen` and
`websockets.connect` replaced, and the child prints each case's outcome as JSON.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

import pytest

RUNNERS = (
    next(
        p
        for p in Path(__file__).resolve().parents
        if (p / "e2e" / ".e2e-root").exists()
    )
    / "e2e"
    / "runners"
)
E2E = RUNNERS.parent

CHILD = r"""
import asyncio, http.client, json, shutil, subprocess, sys, tempfile, urllib.request
from pathlib import Path
sys.path.insert(0, sys.argv[1])
import canvas
from helpers.chrome import Chrome
import websockets
from websockets.exceptions import WebSocketException

PORT = 9222
PAGE = {"type": "page", "webSocketDebuggerUrl": "ws://127.0.0.1:9222/devtools/page/1"}
WS = "CONNECTED"  # what the fake websockets.connect hands back


class FakeProc:
    '''Runs until its `poll` has been called `exit_after` times, then exits with `status`.'''

    def __init__(self, exit_after, status):
        self.exit_after, self.status, self.polls, self.returncode = exit_after, status, 0, None

    def poll(self):
        self.polls += 1
        if self.exit_after is not None and self.polls > self.exit_after:
            self.returncode = self.status
        return self.returncode


class Resp:
    def __init__(self, body):
        self.body = body

    def read(self):
        return self.body


def steps_of(steps, calls):
    '''Each call takes the next step; the last one repeats. An exception is raised, anything else returned.'''
    def take(args):
        calls.append(args)
        step = steps.pop(0) if len(steps) > 1 else steps[0]
        if isinstance(step, BaseException):
            raise step
        return step
    return take


def chrome(tmp, output="", exit_after=None, status=0):
    def popen(argv, stdout, stderr):
        stdout.write(output.encode())
        stdout.flush()
        return FakeProc(exit_after, status)
    subprocess.Popen = popen
    return Chrome(["/fake/Chrome", f"--remote-debugging-port={PORT}"], Path(tmp) / "chrome.log", PORT)


async def run(name, *, steps=(), ws=(), output="", exit_after=None, status=0, timeout=0.3):
    tmp = tempfile.mkdtemp(prefix="attach-")
    tabs_calls, ws_calls = [], []
    take_tabs = steps_of(list(steps), tabs_calls)
    take_ws = steps_of(list(ws), ws_calls)

    def urlopen(url):
        return Resp(json.dumps(take_tabs(url)).encode())

    async def connect(url, **kw):
        return take_ws(url)

    urllib.request.urlopen = urlopen
    websockets.connect = connect
    br = chrome(tmp, output, exit_after, status)
    out = {"case": name}
    try:
        out["value"] = await canvas.attach(br, timeout=timeout)
    except SystemExit as e:
        out["exit"] = str(e.code)
    except BaseException as e:  # noqa: BLE001 — the case records what propagated
        out["raised"] = type(e).__name__
        out["msg"] = str(e)
    out["tabs_calls"] = len(tabs_calls)
    out["ws_calls"] = len(ws_calls)
    out["polls"] = br.proc.polls
    shutil.rmtree(tmp, ignore_errors=True)
    return out


async def main():
    refused = ConnectionRefusedError("[Errno 61] refused")
    results = {}
    results["ok"] = await run("ok", steps=[[PAGE]], ws=[WS])
    results["exits"] = await run(
        "exits", steps=[refused], timeout=5, exit_after=0, status=21,
        output="".join(f"line {i}\n" for i in range(45)),
    )
    results["exits_mid_wait"] = await run(
        "exits_mid_wait", steps=[refused], timeout=5, exit_after=2, status=3, output="bye\n",
    )
    results["silent"] = await run(
        "silent", steps=[refused], output="DevTools: bind failed: address in use\n",
    )
    results["no_page"] = await run("no_page", steps=[[{"type": "iframe"}]])
    results["retried"] = await run(
        "retried",
        steps=[http.client.IncompleteRead(b""), ConnectionResetError("reset"), ValueError("bad json"), [PAGE]],
        ws=[WebSocketException("handshake: 404"), WS],
        timeout=5,  # four polls at 0.2 s each: the page answers on the fourth
    )
    results["typeerror"] = await run("typeerror", steps=[TypeError("a bug in attach")])
    print(json.dumps(results))


asyncio.run(main())
"""


@pytest.fixture(scope="module")
def cases() -> dict:
    r = subprocess.run(
        [sys.executable, "-c", CHILD, str(RUNNERS)],
        capture_output=True,
        text=True,
        timeout=120,
    )
    assert r.returncode == 0, r.stderr
    return json.loads(r.stdout.strip().splitlines()[-1])


def test_a_chrome_that_answers_is_attached_at_once(cases):
    c = cases["ok"]
    assert c["value"] == "CONNECTED"
    assert (c["tabs_calls"], c["ws_calls"]) == (1, 1)


def test_a_chrome_that_exits_before_its_port_fails_at_once_with_its_status(cases):
    c = cases["exits"]
    assert c["tabs_calls"] == 0  # the port is never asked once Chrome has exited
    msg = c["exit"]
    assert (
        msg.splitlines()[0]
        == "Chrome exited with status 21 before opening its debugging port"
    )
    assert (
        "last poll error: no poll yet" in msg
    )  # Chrome exited before the first poll's error
    assert "    line 44" in msg
    assert "    line 4\n" not in msg + "\n"  # only the last TAIL lines


def test_a_chrome_that_exits_while_waiting_names_the_last_poll_error(cases):
    c = cases["exits_mid_wait"]
    assert c["tabs_calls"] == 2  # polled twice, then found exited
    msg = c["exit"]
    assert "Chrome exited with status 3 before opening its debugging port" in msg
    assert "last poll error: ConnectionRefusedError: [Errno 61] refused" in msg
    assert "    bye" in msg


def test_a_chrome_that_never_answers_fails_at_the_deadline_with_the_last_error_and_output(
    cases,
):
    c = cases["silent"]
    assert c["tabs_calls"] >= 2
    assert c["polls"] >= 2  # Chrome is checked on every poll
    msg = c["exit"]
    assert msg.startswith("Chrome on port 9222: not attached in 0 s")
    assert "last poll error: ConnectionRefusedError: [Errno 61] refused" in msg
    assert "    DevTools: bind failed: address in use" in msg


def test_a_chrome_that_lists_no_page_fails_at_the_deadline_saying_so(cases):
    msg = cases["no_page"]["exit"]
    assert msg.startswith("Chrome on port 9222: not attached in 0 s")
    assert "last poll error: it listed no page target" in msg


def test_http_and_socket_errors_are_retried_until_the_page_connects(cases):
    c = cases["retried"]
    assert c["value"] == "CONNECTED"
    # the list: IncompleteRead, reset, bad JSON, then the page; the handshake
    # that failed sends the loop back to the list, so the page is listed twice
    assert c["tabs_calls"] == 5
    assert c["ws_calls"] == 2  # a handshake error, then the connect


def test_an_error_that_is_not_a_poll_failure_propagates_unretried(cases):
    c = cases["typeerror"]
    assert c["raised"] == "TypeError"
    assert c["msg"] == "a bug in attach"
    assert c["tabs_calls"] == 1
