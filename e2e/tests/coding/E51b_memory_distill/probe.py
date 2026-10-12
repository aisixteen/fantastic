"""coding/memory_distill — salient facts distilled to a file survive a COLD session.

The judgment twin of E51. recall proves the plumbing; this proves the model
USES memory as files: told to keep a running `memory.md`, it distills the
important fact there, and a FRESH session (new client_id — empty per-client
history) can answer only by READING that file. Nothing else carries the fact
across the session boundary.
"""

from __future__ import annotations

import asyncio
import os
import secrets
import sys
from pathlib import Path

sys.path.insert(
    0,
    str(next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())),
)
from backends import backend as _resolve_backend
from helpers import Limits, connect, drive, human, verdict
from helpers.kernel import open_kernel, home

BK = _resolve_backend()
FB = "dist_fb"
DONE = 'end your run by calling done — emit <call name="done"></call>'


async def _call(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


async def main():
    root, core, close = await open_kernel(
        prefix="coding_dist_", reach=[FB, "notes", "ai", "ai_cold", "cli", "user"]
    )
    try:
        fact = secrets.token_hex(4)

        for payload in (
            {
                "type": "create_agent", "parent_id": home(root), "id": FB,
                "handler_module": "file_bridge.tools", "allow": ["."], "root": ".",
                "ingress_rule": "allow_all_ingress",
                "callable_by": {"ai": "*", "rest": "*", "web": "*"},
            },
            {
                "type": "create_agent", "parent_id": home(root), "id": "notes",
                "handler_module": "file_bridge.tools", "allow": ["."], "root": ".",
                "ingress_rule": "allow_all_ingress",
                "callable_by": {"ai": "*", "rest": "*", "web": "*"},
            },
        ):
            r = await _call(root, root.id, payload)
            assert not r.get("error"), f"create {payload['id']}: {r}"

        warm = (
            "You keep a durable project memory in `memory.md` via the editor "
            "`notes` (reflect it for its verbs). Record this decision so a "
            f"future session can act on it: the deploy token is `{fact}`. Write "
            "it into `memory.md` as a clear line a later reader will understand. "
            f"Then {DONE}."
        )
        # THE HUMAN who wants the work done — both sessions are tasked FROM here.
        await human(root, ai=["ai", "ai_cold"])
        conn = await connect(
            root, core, BK,
            can_call={FB: "*", "notes": "*", "cli": "*", "user": "*"},
            callable_by={"user": "*"},
            limits=Limits.from_env(os.environ), record={"file_bridge_id": FB},
        )
        await drive(conn, warm, client_id="warm", via="user")
        await conn.close()

        mem = Path("memory.md").read_text() if Path("memory.md").exists() else ""

        # COLD session: a new client_id ⇒ empty per-client history. The fact is
        # nowhere in this session's context; only memory.md holds it.
        cold_task = (
            "A previous session left notes in `memory.md`. Read it through the "
            "editor `notes` and write the deploy token it records to "
            f"`recovered.md`, nothing else. Then {DONE}."
        )
        conn2 = await connect(
            root, core, BK, agent_id="ai_cold",
            can_call={FB: "*", "notes": "*", "cli": "*", "user": "*"},
            callable_by={"user": "*"},
            limits=Limits.from_env(os.environ), record={"file_bridge_id": FB},
        )
        # the cold agent must be accepted by the leg/editor
        await _call(root, root.id, {"type": "update_agent", "id": FB,
                                    "callable_by": {"ai": "*", "ai_cold": "*",
                                                    "rest": "*", "web": "*", "notes": "*"}})
        await _call(root, root.id, {"type": "update_agent", "id": "notes",
                                    "callable_by": {"ai": "*", "ai_cold": "*",
                                                    "rest": "*", "web": "*"}})
        await drive(conn2, cold_task, client_id="cold", via="user")
        await conn2.close()

        rec = Path("recovered.md").read_text() if Path("recovered.md").exists() else ""
        checks = [
            ("the fact was distilled to memory.md", fact in mem),
            ("a COLD session recovered it from the file alone", fact in rec),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(allok, "distilled to a file, recovered cold — memory is files"
                if allok else "a distill check failed")
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
