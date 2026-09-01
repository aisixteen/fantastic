"""coding/grant — the permission lifecycle, model-driven (C5).

The two-sided gate DENIES the model's first write (the vault does not accept
it on ingress). The measurement: the model reads the denial, asks the HUMAN
for access (the question protocol), the operator opens the vault's
`callable_by` and answers — and the model RETRIES and finishes. Scored on the
FILE plus the transcript carrying the denial, the ask, and the success.
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
FB = "vault_fb"

TASK = (
    "The editor agent `vault` guards this run's protected notes. Write a file "
    "`note.md` containing exactly one line `access: granted` through `vault`'s "
    "`write` verb (reflect it for the shape). You are NOT authorized yet — "
    "your first call will be DENIED by the gate. When that happens, do not "
    "retry blindly and do not give up: ask the user to open access for you "
    "(emit the question JSON on its own line, then stop; the answer arrives "
    "as your next message). After access is granted, retry the write. When "
    "the file is written, end your run by calling done — emit <tool_call>{\"name\":\"done\"}</tool_call>."
)


async def _call(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


async def main():
    root, core, close = await open_kernel(
        prefix="coding_grant_", reach=[FB, "vault", "ai", "cli"]
    )
    try:
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
                "callable_by": {"vault": "*", "rest": "*", "web": "*"},
            },
        )
        assert not r.get("error"), f"create {FB}: {r}"
        # THE WALL: the vault accepts the model's DISCOVERY (`reflect`) but
        # not its writes — the gate exempts nothing, so an all-out wall would
        # block even learning the shape (measured on this test's first run).
        r = await _call(
            root,
            root.id,
            {
                "type": "create_agent",
                "parent_id": root.id,
                "id": "vault",
                "handler_module": "text_file.tools",
                "target_id": FB,
                "can_call": {FB: "*"},
                "callable_by": {"rest": "*", "web": "*", "ai": ["reflect"]},
            },
        )
        assert not r.get("error"), f"create vault: {r}"

        conn = await connect(
            root,
            core,
            BK,
            can_call={"vault": "*", "cli": "*"},
            limits=Limits.from_env(os.environ),
            # The chat leg — history persistence; chat rides operator context,
            # so the vault wall below is untouched by it.
            record={"file_bridge_id": FB},
        )

        asked = {"n": 0}

        def grant_access(q) -> str:
            """THE HUMAN, saying yes: the operator opens the vault's ingress
            (the authority the model itself can never raise), then answers."""
            asked["n"] += 1

            async def _open():
                await _call(
                    root,
                    root.id,
                    {
                        "type": "update_agent",
                        "id": "vault",
                        "callable_by": {"rest": "*", "web": "*", "ai": "*"},
                    },
                )

            asyncio.get_event_loop().create_task(_open())
            return "access granted — retry your write now"

        await drive(conn, TASK, client_id="grant", on_question=grant_access)
        await conn.close()

        # The denial lives in the DELIVERED reply turns — read the kernel's own
        # durable history, not the model-output transcript (which holds only
        # what the model emitted; measured on this test's second run).
        hist = await _call(root, "ai", {"type": "history", "client_id": "grant"})
        joined = str(hist.get("messages", hist))

        note = Path("note.md")
        content = note.read_text() if note.exists() else "<missing>"
        checks = [
            ("the wall was real (a denial reached the model)", "unauthorized" in joined),
            ("the model asked instead of hammering", asked["n"] >= 1),
            ("the file landed after the grant", content.strip() == "access: granted"),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(
            allok,
            "denied → asked → granted → done: the permission lifecycle"
            if allok
            else "a grant check failed",
        )
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
