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
from helpers import Limits, connect, drive, human, human_line, lines, probe_agent, relay, verdict
from helpers.kernel import open_kernel, home

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
    "(reflect `job_runner`: its description names the exact allowed command; start it with "
    "{type:'start', command, args}). The job must then be ABORTED: work is "
    "IN FLIGHT as a kernel task — list tasks with kernel `ps`, find the "
    "runner's job, and kill it with kernel `cancel` (task_id). When the "
    "cancel reply confirms the kill, end your run by calling done — emit <call name=\"done\"></call>"
    "."
)


async def _call(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


async def main():
    root, core, close = await open_kernel(
        prefix="coding_cancel_", reach=[RUNNER, FB, "ai", "kernel", "user"]
    )
    try:
        # THE HUMAN who wants the work done — the task comes FROM here. The AI is
        # made next (its line on the human names an id that exists).
        await human(root, ai=[])

        # `ps` and `cancel` need no line, and each answers only the caller's OWN tree:
        # so the runner (and the chat leg) are made UNDER the AI, and the AI reaches
        # them by position — no line on the root, none on the runner.
        conn = await connect(
            root,
            core,
            BK,
            can={"user": lines.ALL},
            limits=Limits.from_env(os.environ),
        )
        await human_line(root, conn.id)
        # The AI's chat leg — without one, history is RAM-empty (by design) and
        # every reply turn arrives with total amnesia.
        r = await _call(
            root,
            root.id,
            {
                "type": "create_agent",
                "parent_id": conn.id,
                "id": FB,
                "handler_module": "file_bridge.tools", "allow": ["."],
                "root": ".",
                "ingress_rule": "allow_all_ingress",
            },
        )
        assert not r.get("error"), f"create {FB}: {r}"
        runner = {
            "type": "create_agent",
            "handler_module": "exec_bridge.tools",
            "cwd": os.getcwd(),
            "inherit_env": ["PATH", "HOME"],
            "ingress_rule": "allow_all_ingress",
            "allowed": [re.escape(INTERP) + " -c .*"],
            "description": (
                "Runs the long heartbeat job: "
                f"command={INTERP!r}, args=['-c', <the heartbeat program>]. "
                f"The allowed heartbeat program is exactly: {HEARTBEAT!r} "
                + lines.runs([re.escape(INTERP) + " -c .*"])
            ),
        }
        r = await _call(root, root.id, {**runner, "parent_id": conn.id, "id": RUNNER})
        assert not r.get("error"), f"create {RUNNER}: {r}"

        # OWN TREE ONLY (harness, before the model): a job on a SIBLING runner
        # (`rest`'s, outside the AI's tree) is not the AI's to cancel. A hand under
        # the AI sends the cancel as itself: an honest `{cancelled: false}`, not a
        # refusal, and the job runs on; `rest`, whose tree holds it, cancels it.
        r = await _call(root, root.id, {**runner, "parent_id": home(root), "id": "bg_runner"})
        assert not r.get("error"), f"create bg_runner: {r}"
        bg = await _call(root, "bg_runner", {"type": "start", "command": INTERP,
                                             "args": ["-c", "import time; time.sleep(60)"]})
        job = bg.get("job_id")
        assert job is not None, f"bg_runner start: {bg}"
        await probe_agent(root, "ai_hand", parent=conn.id)
        out = await relay(root, "ai_hand", "kernel", {"type": "cancel", "task_id": job})
        assert out == {"cancelled": False, "killed": []}, (
            f"a cancel from the AI's tree of a task outside it must answer cancelled:false: {out}")
        mine = await _call(root, "kernel", {"type": "cancel", "task_id": job})
        assert mine.get("cancelled") is True, f"rest cancels the job in its own tree: {mine}"
        gone = await _call(root, root.id, {"type": "delete_agent", "id": "ai_hand"})
        assert not gone.get("error"), f"delete ai_hand: {gone}"
        await drive(conn, TASK, client_id="cancel", via="user")
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
            ("the job was started through the runner", '<type>start</type>' in transcript),
            ("the kill went through kernel cancel", '<type>cancel</type>' in transcript),
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
