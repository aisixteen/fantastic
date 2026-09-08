"""coding/thread_fork — a forked agent inherits the pre-fork conversation. LLM-driven.

Codex `fork_thread.rs` / `multi_agent_*`. A conversation IS the agent's rows in the
kernel's row store (agent id is conversation id). Fork = create a clone + copy the rows
under the clone's id from above (`rows_for` → `rows_put`, re-minted, parents re-linked).
The clone wakes with the original's full history and answers a question only that
history can answer. No new primitive — create_agent + the row store. (The operator
orchestrates the fork here; the model-forks-itself variant is a follow-on.)
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
from helpers.facts_arm import copy_rows, store_rows
from helpers.kernel import open_kernel

BK = _resolve_backend()
DONE = 'end your run by calling done — emit <tool_call>{"name":"done"}</tool_call>'


async def _call(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


async def main():
    root, core, close = await open_kernel(
        prefix="coding_fork_", reach=["ai", "ai_fork", "cli", "user"]
    )
    try:
        secret = secrets.token_hex(4)

        # THE HUMAN who wants the work done — the task comes FROM here.
        await human(root)

        # ── the ORIGINAL learns a fact (a row in the kernel's store) ──
        a = await connect(root, core, BK, agent_id="ai",
                          can_call={"cli": "*", "user": "*"},
                          callable_by={"user": "*"},
                          limits=Limits.from_env(os.environ))
        await drive(a,
            f"Remember this for later — the vault code is `{secret}`. "
            f"Acknowledge that you have it, then {DONE}.", client_id="c", via="user")
        await a.close()
        before = await store_rows(root, core, "ai")

        # ── FORK: clone the agent + copy its rows under the clone's id ──
        made = await _call(root, root.id, {
            "type": "create_agent", "parent_id": root.id, "id": "ai_fork",
            **BK.record(),
            "context_window": 32768,
            "can_call": {"cli": "*"},
            "callable_by": {"$self": "*", "$parent": "*", "$children": "*",
                            "rest": "*", "web": "*"},
        })
        assert not made.get("error"), f"fork create: {made}"
        copied = await copy_rows(root, "ai", "ai_fork")

        # ── the CLONE answers from the inherited conversation ──
        # ai_fork already exists; drive it as OPERATOR (one send) and read back
        # its rows — it assembles from the copied history + this question, and can
        # only answer from what it inherited.
        _ = await _call(root, "ai_fork", {
            "type": "send",
            "text": ("What vault code did I give you earlier in this "
                     "conversation? Reply with just the code."),
            "client_id": "c",
        })
        await asyncio.sleep(1.0)
        after = await store_rows(root, core, "ai_fork")
        answer = str([r.get("content") for r in after[copied:] if r.get("role") == "assistant"])

        checks = [
            ("the original wrote its conversation", any(secret in str(r.get("content")) for r in before)),
            ("the fork is a distinct agent", made.get("id") == "ai_fork"),
            (f"every row was copied under a new id ({copied} copied, {len(before)} before, {len(after)} after)", copied == len(before) and not ({r["id"] for r in after} & {r["id"] for r in before})),
            ("the fork's copy keeps the original's rows intact", len(await store_rows(root, core, "ai")) == len(before)),
            ("the fork answered the pre-fork fact from the copied rows", secret in answer),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(allok, "forked the conversation — the clone knows the pre-fork fact"
                if allok else "a thread_fork check failed")
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
