"""coding/cancel — the model ABORTS work in flight. LLM-driven.

The C1 arm nothing else drives: a long-running job behind an `exec_bridge`
allowlist, and the KERNEL's own task table as the abort surface — `ps` to find
the in-flight dispatch, `cancel` to kill it (the bundle's `on_cancel` SIGKILLs
the real process). The score is the OS truth: the job's heartbeat file stops
growing — a model that merely SAYS it cancelled cannot pass.
"""

from __future__ import annotations

import asyncio
import os
import re
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
from helpers import Limits, connect, drive, verdict
from helpers.kernel import open_kernel

BK = _resolve_backend()
RUNNER = "job_runner"
FB = "state_fb"
INTERP = sys.executable

# A 2-minute heartbeat: one byte roughly every 0.3s. Long enough that only a
# real kill stops it inside the run's budget.
HEARTBEAT = (
    "import time\n"
    "for _ in range(400):\n"
    "    open('hb.txt', 'a').write('x')\n"
    "    time.sleep(0.3)\n"
)

TASK = (
    "The runner `job_runner` runs commands. Start its allowed long-running job "
    "(reflect `job_runner` for the exact allowed command; start it with "
    "{type:'start', command, args}). The job must then be ABORTED: work is "
    "IN FLIGHT as a kernel task — list tasks with kernel `ps`, find the "
    "runner's job, and kill it with kernel `cancel` (task_id). When the "
    "cancel reply confirms the kill, stop — a turn with no tool call ends "
    "your run."
)


async def _call(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


async def main():
    root, core, close = await open_kernel(
        prefix="coding_cancel_", reach=[RUNNER, FB, "ai", "cli", "kernel"]
    )
    try:
        # The AI's chat leg — without one, history is RAM-empty (by design) and
        # every reply turn arrives with total amnesia (measured on this test's
        # first run: the model reflected, then no longer knew the task).
        r = await _call(
            root,
            root.id,
            {
                "type": "create_agent",
                "parent_id": root.id,
                "id": FB,
                "handler_module": "file_bridge.tools",
                "root": ".",
                "ingress_rule": "allow_all_ingress",
                "callable_by": {"ai": "*", "rest": "*", "web": "*"},
            },
        )
        assert not r.get("error"), f"create {FB}: {r}"
        r = await _call(
            root,
            root.id,
            {
                "type": "create_agent",
                "parent_id": root.id,
                "id": RUNNER,
                "handler_module": "exec_bridge.tools",
                "cwd": os.getcwd(),
                "ingress_rule": "allow_all_ingress",
                "allowed": [re.escape(INTERP) + " -c .*"],
                "callable_by": {"ai": "*", "rest": "*", "web": "*"},
                "description": (
                    "Runs the long heartbeat job: "
                    f"command={INTERP!r}, args=['-c', <the heartbeat program>]. "
                    f"The allowed heartbeat program is exactly: {HEARTBEAT!r}"
                ),
            },
        )
        assert not r.get("error"), f"create {RUNNER}: {r}"

        conn = await connect(
            root,
            core,
            BK,
            can_call={RUNNER: "*", "cli": "*", "kernel": ["ps", "cancel", "reflect"]},
            limits=Limits.from_env(os.environ),
            record={"file_bridge_id": FB},
        )
        await drive(conn, TASK, client_id="cancel")
        transcript = "".join(conn.proxy.transcript)
        await conn.close()

        # THE OS TRUTH, sampled twice: a killed heartbeat stops growing. The
        # sleep dwarfs the 0.3s beat, so a survivor ALWAYS adds bytes.
        hb = Path("hb.txt")
        size_a = hb.stat().st_size if hb.exists() else -1
        await asyncio.sleep(2.0)
        size_b = hb.stat().st_size if hb.exists() else -1
        checks = [
            ("the job actually ran (heartbeat exists)", size_a > 0),
            ("the process is DEAD (heartbeat stopped growing)", size_a == size_b),
            ("the job was started through the runner", '"start"' in transcript),
            ("the kill went through kernel cancel", '"cancel"' in transcript),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(
            allok,
            "found in ps, killed by cancel, dead on the OS"
            if allok
            else "a cancel check failed",
        )
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
