"""coding/thread_fork — a forked agent starts from the original's conversation BY GRANT.
LLM-driven.

Codex `fork_thread.rs` / `multi_agent_*`. A conversation is the agent's rows in its
MEMORY STORE, keyed by the agent. A fork is REFERENCE-ONLY, by explicit choice of the
original's model: it `share`s its head (frozen) with the clone — only its OWN rows can be
shared — and the clone starts with `fork_from {owner, grant, message}`: the granted rows
become read-only context, quoted and attributed, followed by the framing line and the
kickstart message, and the clone's turn starts at once. Nothing is copied: the clone's
own key stays its own, and the original's rows are unchanged.
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
from helpers import Limits, connect, drive, human, memory, verdict
from helpers.facts_arm import store_rows
from helpers.kernel import open_kernel

BK = _resolve_backend()
DONE = 'end your run by calling done — emit <call name="done"></call>'


async def _call(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


async def _grants_given(root, owner: str) -> list[dict]:
    """The grants `owner` gave — read off the store's tables (the harness is its admin)."""
    tables = await memory.dump(root)
    return [g for g in tables.get("grants") or [] if str(g.get("owner", "")).endswith(f"|{owner}")]


async def main():
    root, core, close = await open_kernel(
        prefix="coding_fork_", reach=["ai", "ai_fork", "cli", "user"]
    )
    try:
        secret = secrets.token_hex(4)
        await human(root, ai=["ai", "ai_fork"])

        # ── the ORIGINAL learns a fact, then SHARES its head with the clone ──
        a = await connect(root, core, BK, agent_id="ai",
                          can_call={"cli": "*", "user": "*"},
                          callable_by={"user": "*"},
                          limits=Limits.from_env(os.environ))
        await drive(a,
            f"Remember this for later — the vault code is `{secret}`. "
            f"Acknowledge that you have it, then {DONE}.", client_id="c", via="user")
        await drive(a,
            "An agent `ai_fork` will continue this work. Share your conversation so far "
            f"with it: send your memory store `{memory.MEMORY}` a share of your head "
            "(frozen), naming `ai_fork` as the reader and saying why. Then "
            f"{DONE}.", client_id="c", via="user")
        await a.close()
        before = await store_rows(root, core, "ai")
        grants = await _grants_given(root, "ai")

        # ── the CLONE: its own agent, its own key, started from the grant ──
        fork = await connect(root, core, BK, agent_id="ai_fork",
                             can_call={"cli": "*", "user": "*"},
                             callable_by={"user": "*"},
                             limits=Limits.from_env(os.environ))
        grant = grants[0]["grant"] if grants else None
        out = await _call(root, "ai_fork", {
            "type": "fork_from", "owner": "ai", "grant": grant, "client_id": "c",
            "message": ("What vault code did the earlier conversation above establish? "
                        f"Reply with just the code, then {DONE}."),
        })
        # the kickstart turn runs detached — wait for the clone's chain to settle
        for _ in range(120):
            await asyncio.sleep(1.0)
            after = await store_rows(root, core, "ai_fork")
            if any(r.get("role") == "assistant" for r in after):
                break
        await fork.close()
        after = await store_rows(root, core, "ai_fork")
        answer = str([r.get("content") for r in after if r.get("role") == "assistant"])
        fork_row = next((r for r in after if r.get("kind") == "fork"), None)

        checks = [
            ("the original wrote its conversation", any(secret in str(r.get("content")) for r in before)),
            ("the original's MODEL shared its head (an explicit grant)", bool(grants)),
            ("fork_from accepted the grant", isinstance(out, dict) and out.get("forked") is True),
            ("the clone holds ONE fork row, not a copy", fork_row is not None
             and not any(secret in str(r.get("content")) for r in after if r.get("kind") != "fork"
                         and r.get("role") != "assistant")),
            ("the original's rows are unchanged", len(await store_rows(root, core, "ai")) == len(before)),
            ("the clone answered the pre-fork fact from the shared rows", secret in answer),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(allok, "forked by grant — the clone knows the pre-fork fact, nothing copied"
                if allok else "a thread_fork check failed")
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
