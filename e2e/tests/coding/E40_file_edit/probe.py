"""coding/file_edit — the model EDITS with the patch organ. LLM-driven.

One organ, precisely: the `text_file` editor's `patch` verb (patch moved OFF
the bridge — text_file owns SEARCH/REPLACE and reaches disk only through gated
sends to the file agent). The task is a surgical config change; the score is
the FILE — the target line changed, every other line byte-identical (a
whole-file rewrite that happens to match would pass the diff, so the
transcript must also show a `patch` call — the organ, not a bypass).
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
FB = "edit_fb"

CONFIG = (
    "# service config\nhost = 127.0.0.1\nport = 8080\nworkers = 4\nlog_level = info\n"
)

TASK = (
    "The editor agent `editor` edits this run's files. In `config.txt`, change "
    "`port = 8080` to `port = 9090` — and change NOTHING else. Use the "
    "editor's `patch` verb (reflect `editor` to learn its exact shape); do "
    "NOT rewrite the whole file. When the patch is applied, end your run by calling done — emit <call name=\"done\"></call>."
)


async def _call(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


async def main():
    root, core, close = await open_kernel(
        prefix="coding_edit_", reach=[FB, "editor", "ai", "cli", "user"]
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
        await drive(conn, TASK, client_id="edit", via="user")
        transcript = "".join(conn.proxy.transcript)
        await conn.close()

        after = Path("config.txt").read_text()
        checks = [
            ("port changed", "port = 9090" in after and "8080" not in after),
            (
                "everything else byte-identical",
                after == CONFIG.replace("port = 8080", "port = 9090"),
            ),
            (
                "the PATCH organ was used, not a rewrite",
                '<type>patch</type>' in transcript,
            ),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(
            allok,
            "a surgical patch, by the model, through the organ"
            if allok
            else "a file_edit check failed",
        )
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
