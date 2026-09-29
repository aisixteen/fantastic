"""coding/undo — edit, then take it back with `undo`. LLM-driven.

The file agent keeps ONE undo slot per path, in RAM (C2): the model edits, is
asked (in the same task) to revert, and must `undo`. The score is the FILE — ending
byte-identical to the original — plus the transcript showing BOTH organs
(`edit` and `undo`): an original file that was simply never touched would
pass the byte check alone.
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
from helpers import Limits, connect, drive, human, verdict
from helpers.kernel import open_kernel

BK = _resolve_backend()
FB = "undo_fb"

CONFIG = (
    "# service config\nhost = 127.0.0.1\nport = 8080\nworkers = 4\nlog_level = info\n"
)

TASK = (
    "The editor agent `editor` edits this run's files. Step 1: in `config.txt` "
    "change `port = 8080` to `port = 9090` using the editor's `edit` verb "
    "(reflect `editor` for the shape). Step 2: the change is cancelled — put "
    "`config.txt` back to its pre-edit state with the editor's `undo` verb. "
    "When the file is restored, stop — a turn with no "
    "tool call ends your run."
)


async def _call(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


async def main():
    root, core, close = await open_kernel(
        prefix="coding_undo_", reach=[FB, "editor", "ai", "cli", "user"]
    )
    try:
        r = await _call(
            root,
            root.id,
            {
                "type": "create_agent",
                "parent_id": root.id,
                "id": FB,
                "handler_module": "file_bridge.tools", "allow": ["."],
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
                "id": "editor",
                "handler_module": "file_bridge.tools", "allow": ["."],
                "root": ".",
                "ingress_rule": "allow_all_ingress",
                "callable_by": {"ai": "*", "rest": "*", "web": "*"},
            },
        )
        assert not r.get("error"), f"create editor: {r}"
        Path("config.txt").write_text(CONFIG)

        # THE HUMAN who wants the work done — the task comes FROM here.

        await human(root)

        conn = await connect(
            root,
            core,
            BK,
            can_call={FB: "*", "editor": "*", "cli": "*", "user": "*"},
            callable_by={"user": "*"},
            limits=Limits.from_env(os.environ),
            record={"file_bridge_id": FB},
        )
        await drive(conn, TASK, client_id="undo", via="user")
        transcript = "".join(conn.proxy.transcript)
        await conn.close()

        after = Path("config.txt").read_text()
        checks = [
            ("the file ends byte-identical to the original", after == CONFIG),
            ("the edit actually happened (an edit call)", '<type>edit</type>' in transcript),
            ("the revert used undo", '<type>undo</type>' in transcript),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(
            allok,
            "edited, then reverted with undo"
            if allok
            else "an undo check failed",
        )
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
