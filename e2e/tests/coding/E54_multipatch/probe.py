"""coding/multipatch — the edit TRANSACTION across files. LLM-driven (C2).

A coordinated rename spanning two files — the kind of edit where a half-applied
state is a broken program. The model must make BOTH changes with ONE `edit`
call (`edits` array over one hunk blob); the score is both files exact plus the
transcript showing a single transactional call, not two independent edits.
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
FB = "mp_fb"

LIB = "def make_widget(size):\n    return {'size': size}\n"
APP = "from lib import make_widget\n\nw = make_widget(3)\n"

LIB_AFTER = "def build_widget(size):\n    return {'size': size}\n"
APP_AFTER = "from lib import build_widget\n\nw = build_widget(3)\n"

TASK = (
    "The editor agent `editor` edits this run's files. Rename the function "
    "`make_widget` to `build_widget` EVERYWHERE it appears: its definition in "
    "`lib.py` and both uses in `app.py`. A half-renamed state is a broken "
    "program, so make ALL the changes with ONE `edit` call — the edit verb "
    "is a TRANSACTION over an `edits` array (reflect `editor` for the exact "
    "shape). Change nothing else. When the edit is applied, end your run by calling done — emit <call name=\"done\"></call>."
)


async def _call(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


async def main():
    root, core, close = await open_kernel(
        prefix="coding_mp_", reach=[FB, "editor", "ai", "cli", "user"]
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
        Path("lib.py").write_text(LIB)
        Path("app.py").write_text(APP)

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
        await drive(conn, TASK, client_id="mp", via="user")
        transcript = "".join(conn.proxy.transcript)
        await conn.close()

        lib_after = Path("lib.py").read_text()
        app_after = Path("app.py").read_text()
        checks = [
            ("lib.py is exactly the expected result", lib_after == LIB_AFTER),
            ("app.py is exactly the expected result", app_after == APP_AFTER),
            ("ONE transactional call (an `edits` array crossed the wire)",
             '<edits>' in transcript),
            ("no whole-file rewrite",
             '<type>write</type>' not in transcript),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(
            allok,
            "one transaction, two files, coordinated and atomic"
            if allok
            else "a multipatch check failed",
        )
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
