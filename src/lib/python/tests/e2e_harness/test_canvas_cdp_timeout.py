"""The canvas e2e runner's CDP call that gets no reply (#88) names what hung:
the method, the first 120 characters of its expression, how long it waited,
the main frame's load state and the page's last console lines.

`e2e/runners/canvas.py` is a script: importing it points FANTASTIC_HOME at a
throwaway home and puts the e2e tree on `sys.path`. So it is driven in its own
interpreter, against a fake CDP socket whose reply never comes, with the wait
cut to a fraction of a second; the child prints each case's outcome as JSON.
"""

from __future__ import annotations

import json
import re
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

CHILD = r"""
import asyncio, json, shutil, sys
sys.path.insert(0, sys.argv[1])
import canvas

canvas.REPLY_S = 0.2


class Ws:
    '''A CDP socket: hands out `events`, then the reply to call `reply_to` (when
    set), then nothing ever again.'''

    def __init__(self, events, reply_to=None):
        self.q = [json.dumps(e) for e in events]
        if reply_to is not None:
            self.q.append(json.dumps({"id": reply_to, "result": {"result": {"value": 7}}}))

    async def send(self, _):
        pass

    async def recv(self):
        if self.q:
            return self.q.pop(0)
        await asyncio.Event().wait()


def console(i):
    return {"method": "Runtime.consoleAPICalled", "params": {"type": "log", "args": [{"value": f"line {i}"}]}}


MAIN = {"id": "MAIN", "url": "http://127.0.0.1:1/"}
LONG = "window.__fid.snap(\n   " + "x" * 200


async def call(events, method, reply_to=None, **params):
    pg = canvas.Page(Ws(events, reply_to))
    try:
        return {"value": await pg.cdp(method, **params)}
    except Exception as e:
        return {"type": type(e).__name__, "msg": str(e)}


async def main():
    out = {}
    # a snap that hangs mid-load, after more console lines than are kept
    out["loading"] = await call(
        [{"method": "Page.frameStartedLoading", "params": {"frameId": "MAIN"}},
         {"method": "Page.frameNavigated", "params": {"frame": MAIN}},
         *[console(i) for i in range(12)],
         {"method": "Runtime.exceptionThrown", "params": {"exceptionDetails": {"exception": {"description": "TypeError:\n  boom"}}}}],
        "Runtime.evaluate", expression=LONG)
    # a call with no expression, before any navigation, with nothing on the console
    out["bare"] = await call([], "Input.dispatchMouseEvent", type="mousePressed")
    # the main frame loaded; a child frame still loading
    out["loaded"] = await call(
        [{"method": "Page.frameStartedLoading", "params": {"frameId": "MAIN"}},
         {"method": "Page.frameNavigated", "params": {"frame": MAIN}},
         {"method": "Page.frameStoppedLoading", "params": {"frameId": "MAIN"}},
         {"method": "Page.frameNavigated", "params": {"frame": {"id": "KID", "parentId": "MAIN", "url": "about:blank"}}},
         {"method": "Page.frameStartedLoading", "params": {"frameId": "KID"}}],
        "Runtime.evaluate", expression="1")
    # a reply after events: the call answers as before
    out["answered"] = await call([console(0)], "Runtime.evaluate", reply_to=1, expression="7")
    print(json.dumps(out))
    shutil.rmtree(canvas._HOME, ignore_errors=True)


asyncio.run(main())
"""


@pytest.fixture(scope="module")
def cases() -> dict:
    r = subprocess.run(
        [sys.executable, "-c", CHILD, str(RUNNERS)],
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert r.returncode == 0, r.stderr
    return json.loads(r.stdout.strip().splitlines()[-1])


def test_a_hung_evaluate_names_its_method_expression_and_wait(cases):
    c = cases["loading"]
    assert c["type"] == "TimeoutError"
    head = c["msg"].splitlines()[0]
    waited = re.match(
        r"CDP Runtime\.evaluate got no reply in ([\d.]+) s \(nothing from Chrome for 0\.2 s\)",
        head,
    )
    assert waited and float(waited[1]) >= 0.2
    # the expression, whitespace folded, cut at 120 characters
    shown = ("window.__fid.snap( " + "x" * 200)[:120]
    assert head.endswith(": " + shown)


def test_a_hung_call_says_the_main_frame_is_still_loading(cases):
    assert (
        "main frame STILL LOADING http://127.0.0.1:1/ (since +"
        in cases["loading"]["msg"]
    )
    assert "; 0 other frame(s) loading" in cases["loading"]["msg"]


def test_a_hung_call_shows_the_last_console_lines_only(cases):
    lines = cases["loading"]["msg"].split("last console lines:\n", 1)[1].splitlines()
    assert len(lines) == 8
    # the oldest dropped; an uncaught exception kept as a line, whitespace folded
    assert [ln.strip().split(" ", 1)[1] for ln in lines] == [
        *(f"log line {i}" for i in range(5, 12)),
        "exception TypeError: boom",
    ]


def test_a_hung_call_with_no_expression_and_no_navigation(cases):
    c = cases["bare"]
    assert c["type"] == "TimeoutError"
    head, frame, _, lines = c["msg"].splitlines()
    # no `: <expression>` tail
    assert re.fullmatch(
        r"CDP Input\.dispatchMouseEvent got no reply in [\d.]+ s \(nothing from Chrome for 0\.2 s\) at \+[\d.]+s",
        head,
    )
    assert frame == "no main-frame navigation seen; 0 other frame(s) loading"
    assert lines == "  (none)"


def test_a_hung_call_after_the_main_frame_loaded_counts_child_frames(cases):
    assert (
        "\nmain frame loaded http://127.0.0.1:1/; 1 other frame(s) loading\n"
        in cases["loaded"]["msg"]
    )


def test_a_reply_after_events_still_answers(cases):
    assert cases["answered"] == {"value": {"result": {"value": 7}}}
