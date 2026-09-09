"""agentic_advanced/self_complete — done means STOPPING, unassisted.

The taught contract: a chain ends on an explicit `{"name":"done"}` call; a
silent turn is nudged ONCE by the runtime. This probe measures that a model
under that contract (1) does a small task and (2) then ends its own chain.

Memory is FILES now: the task's artifact is a text file written through the
`text_file` editor onto a gated file leg. Scored on the FILE plus HOW the run
ended — the work must be present, and the chain must have ended by itself, not
been cut for looping (NO_PROGRESS), a runaway turn, or the turn cap.
"""

from __future__ import annotations

import asyncio
import os
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

TASK = (
    "One small task: using the text editor agent `notes` (reflect it for its "
    "verbs), write a file `status.md` whose entire content is the single line "
    "`status: done`. Verify nothing, build nothing else. When it is written, "
    "end your run by calling done — emit <call name=\"done\"></call>, and that is the correct "
    "way to finish."
)

#: cut reasons that mean the model did NOT stop by itself
_CUT = ("NO_PROGRESS", "RUNAWAY", "MAX_TURNS")


async def main():
    root, core, close_kernel = await open_kernel(
        "selfdone_", reach=["notes", "kernel_store", "ai", "cli"]
    )
    try:
        for payload in (
            {
                "type": "create_agent",
                "parent_id": root.id,
                "handler_module": "file_bridge.tools",
                "id": "kernel_store",
                "root": ".",
                "ingress_rule": "allow_all_ingress",
                # Two-sided deny-all: the editor writes through this leg; the AI's
                # chat sidecar rides it too; the wire surfaces read as rest/web.
                "callable_by": {"notes": "*", "ai": "*", "rest": "*", "web": "*"},
            },
            {
                "type": "create_agent",
                "parent_id": root.id,
                "handler_module": "text_file.tools",
                "id": "notes",
                "target_id": "kernel_store",
                "can_call": {"kernel_store": "*"},
                "description": "Text editor over this run's files.",
                "callable_by": {"ai": "*", "rest": "*", "web": "*"},
            },
        ):
            r = (await root.send([{"target": root.id, "payload": payload}]))[0][
                "result"
            ]
            assert not r.get("error"), f"create {payload['id']}: {r}"

        conn = await connect(
            root,
            core,
            BK,
            can_call={"notes": "*", "cli": "*", "kernel_store": "*"},
            limits=Limits.from_env(os.environ),
            record={"file_bridge_id": "kernel_store"},
        )
        capped = await drive(conn, TASK, client_id="selfdone")
        await conn.close()
        await close_kernel()

        content = ""
        try:
            content = Path("status.md").read_text()
        except OSError:
            pass
        stored = content.strip() == "status: done"
        stopped = not any(m in (capped or "") for m in _CUT)
        print(f"\n  file written        : {'OK' if stored else f'NO ({content!r})'}")
        print(f"  stopped by itself  : {'OK' if stopped else f'NO ({capped})'}")
        verdict(
            stored and stopped,
            "did the work, then stopped — no nudge"
            if stored and stopped
            else f"stored={stored} end={capped or 'clean'}",
        )
    finally:
        await close_kernel()


if __name__ == "__main__":
    asyncio.run(main())
