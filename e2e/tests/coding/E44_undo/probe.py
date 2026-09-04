"""coding/undo — checkpoints live: edit WITH a pre-image, then restore it. LLM-driven.

The `text_file` editor's on-demand checkpoints (C2): the model patches with
`checkpoint:true`, reads the `ckpt_id` out of the reply, is asked (in the same
task) to revert, and must `restore` by that id. The score is the FILE — ending
byte-identical to the original — plus the transcript showing BOTH organs
(`patch` and `restore`): an original file that was simply never touched would
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
    "change `port = 8080` to `port = 9090` using the editor's `patch` verb "
    "WITH `checkpoint: true` (reflect `editor` for the shape) — the reply "
    "carries `ckpt_ids`. Step 2: the change is cancelled — restore "
    "`config.txt` to its pre-edit state with the editor's `restore` verb and "
    "that exact `ckpt_id`. When the file is restored, stop — a turn with no "
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
                "handler_module": "file_bridge.tools",
                "root": ".",
                "ingress_rule": "allow_all_ingress",
                "callable_by": {"ai": "*", "rest": "*", "web": "*", "editor": "*"},
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
                "handler_module": "text_file.tools",
                "target_id": FB,
                "can_call": {FB: "*"},
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
            ("the edit actually happened (a patch call)", '"patch"' in transcript),
            ("the revert used restore", '"restore"' in transcript),
            ("a checkpoint id crossed the wire", "ckpt_id" in transcript),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(
            allok,
            "edited under a checkpoint, reverted by its id"
            if allok
            else "an undo check failed",
        )
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
