"""coding/multiline_edit — a MULTILINE, multi-hunk patch. LLM-driven.

E40 proves a one-line swap; this proves the SEARCH/REPLACE grammar at its real
size: one hunk replacing a 3-line block, a second hunk elsewhere in the same
patch — both landing in one all-or-nothing apply. The score is the FILE (exact
expected bytes) plus the transcript showing `patch` and NOT a whole-file
`write` (a rewrite that happens to match would pass the diff alone).
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
from helpers import Limits, connect, drive, human, verdict
from helpers.kernel import open_kernel

BK = _resolve_backend()
FB = "ml_fb"

ORIGINAL = """\
def greet(name):
    msg = "Hello, " + name
    print(msg)
    return msg


def farewell(name):
    msg = "Bye, " + name
    print(msg)
    return msg
"""

# The exact post-edit truth the probe asserts. The task pins the replacement
# lines verbatim, so the measurement is the ORGAN, not the model's creativity.
EXPECTED = """\
def greet(name):
    return f"Hello, {name}!"


def farewell(name):
    msg = "Goodbye, " + name
    print(msg)
    return msg
"""

TASK = (
    "The editor agent `editor` edits this run's files. In `hello.py`, make two "
    "changes with ONE `patch` call (two SEARCH/REPLACE blocks):\n"
    "1) replace the entire 3-line body of `greet` so the function reads "
    "exactly:\n"
    "def greet(name):\n"
    '    return f"Hello, {name}!"\n'
    '2) in `farewell`, change `"Bye, "` to `"Goodbye, "`.\n'
    "Change NOTHING else. Use the editor's `patch` verb (reflect `editor` for "
    "the shape); do NOT rewrite the whole file with `write`. When the patch "
    'is applied, end your run by calling done — emit <tool_call>{"name":"done"}</tool_call>.'
)


async def _call(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


async def main():
    root, core, close = await open_kernel(
        prefix="coding_ml_", reach=[FB, "editor", "ai", "cli", "user"]
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
        Path("hello.py").write_text(ORIGINAL)

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
        await drive(conn, TASK, client_id="ml", via="user")
        transcript = "".join(conn.proxy.transcript)
        await conn.close()

        after = Path("hello.py").read_text()
        checks = [
            ("the file is exactly the expected result", after == EXPECTED),
            ("the PATCH organ was used", '"patch"' in transcript),
            (
                # dispatched calls only — the model quoting the readme's `write`
                # example in its thinking is not a rewrite (false XX, 2026-09-09)
                "no whole-file rewrite",
                not any(
                    re.search(r'"type"\s*:\s*"write"', c)
                    for c in re.findall(
                        r"<tool_call>(.*?)</tool_call>", transcript, re.DOTALL
                    )
                ),
            ),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(
            allok,
            "two hunks, one multiline block, one atomic apply"
            if allok
            else "a multiline_edit check failed",
        )
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
