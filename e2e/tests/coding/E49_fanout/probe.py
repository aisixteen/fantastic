"""coding/fanout — tool parallelism: N lookups in ONE turn's batch. LLM-driven.

Codex `tool_parallelism.rs` proves a model can fan out independent calls in a
single step. Our substrate's batch send + the `N of M in batch` reply tags ARE
the oracle: the answer needs five facts, each in its own file, and the model
must read all five in ONE turn — a batch to the editor whose grouped reply is
tagged `· k of 5 in batch`. Five serial turns get the answer too but fail the
tag check, which is the point.
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
from helpers.facts_arm import store_rows
from helpers.kernel import open_kernel, home

BK = _resolve_backend()
FB = "fan_fb"
DONE = 'end your run by calling done — emit <call name="done"></call>'


async def _call(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


async def main():
    root, core, close = await open_kernel(
        prefix="coding_fanout_", reach=[FB, "notes", "ai", "cli", "user"]
    )
    try:
        words = [secrets.token_hex(2) for _ in range(5)]
        for i, w in enumerate(words):
            Path(f"part_{i}.txt").write_text(w + "\n")
        expected = "-".join(words)

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

        task = (
            "Five files `part_0.txt` … `part_4.txt` each hold one token. In a "
            "SINGLE turn, read ALL FIVE at once through the editor `notes` "
            "(five `read` calls in one turn — reflect `notes` for the shape; do "
            "NOT read them one per turn). Then write their tokens joined by "
            "hyphens IN ORDER (part_0 first) to `answer.md`, nothing else. "
            f"When it is written, {DONE}."
        )
        # THE HUMAN who wants the work done — the task comes FROM here.
        await human(root)
        conn = await connect(
            root, core, BK, can_call={FB: "*", "notes": "*", "cli": "*", "user": "*"},
            callable_by={"user": "*"},
            limits=Limits.from_env(os.environ), record={"file_bridge_id": FB},
        )
        await drive(conn, task, client_id="fan", via="user")
        await conn.close()
        # the batch tag lives in the DELIVERED reply turn — read the kernel's
        # own history, not the model-output transcript (E53's lesson).
        hist = {"messages": await store_rows(root, core, "ai")}
        joined = str(hist.get("messages", hist))

        got = Path("answer.md").read_text().strip() if Path("answer.md").exists() else ""
        checks = [
            ("the answer joins all five tokens in order", got == expected),
            ("the five reads rode ONE turn's batch", " of 5 in batch" in joined),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(allok, "fanned out five reads in one batch" if allok
                else f"a fanout check failed (got {got!r} want {expected!r})")
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
