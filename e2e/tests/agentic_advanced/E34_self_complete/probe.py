"""agentic_advanced/self_complete — done means STOPPING, unassisted.

The runtime reminder for a no-call turn (the nudge) is deleted from every
runtime: a turn that emits no tool call ENDS the chain, and that is the taught
contract — "emit no call when the work is done". This probe measures that a
model under that contract (1) does a small task and (2) then stops on its own.

Scored on the STORE plus HOW the run ended: the work must be present, and the
chain must have ended by itself — not been cut for looping (NO_PROGRESS), a
runaway turn, or the turn cap. The clean endings are a natural chain end or the
idle window expiring over a finished task.
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
    "One small task: in the memory agent `notes`, set the key 'status' to the "
    "exact value 'done'. Verify nothing, build nothing else. When it is stored, "
    "stop — a turn with no tool call ends your run, and that is the correct way "
    "to finish."
)

#: cut reasons that mean the model did NOT stop by itself
_CUT = ("NO_PROGRESS", "RUNAWAY", "MAX_TURNS")


async def main():
    root, core, close_kernel = await open_kernel(
        "selfdone_", reach=["notes", "kernel_store", "ai", "cli"]
    )
    store = (
        await root.send(
            [
                {
                    "target": root.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": root.id,
                        "handler_module": "file_bridge.tools",
                        "id": "kernel_store",
                        "root": ".",
                        "ingress_rule": "allow_all_ingress",
                        # Two-sided deny-all: the AI, plus the wire surfaces the harness's
                        # own reads arrive as on a spawned kernel (inert in-process).
                        "callable_by": {"ai": "*", "rest": "*", "web": "*"},
                    },
                }
            ]
        )
    )[0]["result"]
    assert not store.get("error"), f"store: {store}"
    made = (
        await root.send(
            [
                {
                    "target": root.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": root.id,
                        "handler_module": "durable_memory.tools",
                        "id": "notes",
                        "file_bridge_id": store["id"],
                        "description": "Durable notes.",
                        "callable_by": {"ai": "*", "rest": "*", "web": "*"},
                    },
                }
            ]
        )
    )[0]["result"]
    assert not made.get("error"), f"notes: {made}"

    conn = await connect(
        root,
        core,
        BK,
        # `kernel_store` too: chat persistence rides `file_bridge_id`, and on a
        # spawned kernel that write goes through the gate like everything else.
        can_call={"notes": "*", "cli": "*", "kernel_store": "*"},
        limits=Limits.from_env(os.environ),
        record={"file_bridge_id": store["id"]},
    )
    capped = await drive(conn, TASK, client_id="selfdone")
    val = (
        (
            await root.send(
                [{"target": "notes", "payload": {"type": "read", "key": "status"}}]
            )
        )[0]["result"]
        or {}
    ).get("value")
    await conn.close()
    await close_kernel()

    stored = val == "done"
    stopped = not any(m in (capped or "") for m in _CUT)
    print(f"\n  stored 'done'      : {'OK' if stored else f'NO ({val!r})'}")
    print(f"  stopped by itself  : {'OK' if stopped else f'NO ({capped})'}")
    verdict(
        stored and stopped,
        "did the work, then stopped — no nudge"
        if stored and stopped
        else f"stored={stored} end={capped or 'clean'}",
    )


if __name__ == "__main__":
    asyncio.run(main())
