"""coding/program_tests — the model runs the tests, FIXES the bug, proves green.

Two organs COMPOSED, LLM-driven: pytest behind an exec_bridge allowlist (C20)
and the `text_file` editor's `patch` (C2). The fixture ships a program with a real off-by-one
and a suite that catches it; the task is the coder loop itself — run, read the
failure, fix the PROGRAM (not the test), rerun to green. Scored by the probe
rerunning the suite at the end + reading the program, plus transcript checks
that both organs ran.
"""

from __future__ import annotations

import asyncio
import os
import re
import subprocess
import sys
from pathlib import Path

# e2e bootstrap — walk up to `.e2e-root`; no hard-coded depth.
sys.path.insert(
    0,
    str(
        next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
    ),
)
from backends import backend as _resolve_backend
from helpers import Limits, connect, drive, human, verdict
from helpers.kernel import open_kernel

BK = _resolve_backend()
RUNNER, FB = "runner", "edit_fb"
INTERP = sys.executable

PROG_BUGGY = "def inc(x):\n    return x + 2  # BUG: off by one\n"
TEST = "from prog import inc\n\n\ndef test_inc():\n    assert inc(1) == 2\n"

TASK = (
    "This run's working dir holds `prog.py` and `test_prog.py`. The command "
    "runner `runner` runs the test suite (reflect it for the exact allowed "
    "command; start a job, poll {type:'status', job_id} for stdout). The suite "
    "currently FAILS. Run it, read the failure, then fix the bug IN `prog.py` "
    "— never touch `test_prog.py` — using the editor agent `editor`'s "
    "`patch` verb (reflect it for the shape). Rerun the suite until "
    "it passes. When it is green, end your run by calling done — emit <tool_call>{\"name\":\"done\"}</tool_call>."
)


async def _call(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


async def main():
    root, core, close = await open_kernel(
        prefix="coding_pytest_", reach=[RUNNER, FB, "editor", "ai", "cli", "user"]
    )
    try:
        Path("prog.py").write_text(PROG_BUGGY)
        Path("test_prog.py").write_text(TEST)

        for payload in (
            {
                "type": "create_agent",
                "parent_id": root.id,
                "id": FB,
                "handler_module": "file_bridge.tools",
                "root": ".",
                "ingress_rule": "allow_all_ingress",
                "callable_by": {"ai": "*", "rest": "*", "web": "*", "editor": "*"},
            },
            {
                "type": "create_agent",
                "parent_id": root.id,
                "id": "editor",
                "handler_module": "text_file.tools",
                "target_id": FB,
                "can_call": {FB: "*"},
                "callable_by": {"ai": "*", "rest": "*", "web": "*"},
            },
            {
                "type": "create_agent",
                "parent_id": root.id,
                "id": RUNNER,
                "handler_module": "exec_bridge.tools",
                "cwd": os.getcwd(),
                "ingress_rule": "allow_all_ingress",
                "allowed": [re.escape(INTERP) + r" -m pytest .*"],
                "callable_by": {"ai": "*", "rest": "*", "web": "*"},
            },
        ):
            r = await _call(root, root.id, payload)
            assert not r.get("error"), f"create {payload['id']}: {r}"

        # THE HUMAN who wants the work done — the task comes FROM here.

        await human(root)

        conn = await connect(
            root,
            core,
            BK,
            can_call={RUNNER: "*", FB: "*", "editor": "*", "cli": "*", "user": "*"},
            callable_by={"user": "*"},
            limits=Limits.from_env(os.environ),
            record={"file_bridge_id": FB},
        )
        await drive(conn, TASK, client_id="fix", via="user")
        transcript = "".join(conn.proxy.transcript)
        await conn.close()

        # TRUTH, recomputed by the probe: the suite as it stands NOW.
        final = await asyncio.to_thread(
            subprocess.run,
            [INTERP, "-m", "pytest", "test_prog.py", "-q"],
            capture_output=True,
            text=True,
            check=False,  # the returncode IS the measurement
            env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"},
        )
        checks = [
            ("the suite is green now", final.returncode == 0),
            ("the fix is in the PROGRAM", "x + 1" in Path("prog.py").read_text()),
            ("the test was never touched", Path("test_prog.py").read_text() == TEST),
            ("the runner organ ran", "pytest" in transcript and RUNNER in transcript),
            ("the edit organ made the fix", '\"patch\"' in transcript),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(
            allok,
            "run → read failure → fix → green: the coder loop, model-driven"
            if allok
            else "a program_tests check failed",
        )
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
