"""Shared pieces of the `memory` group's LLM probes — one fixture, one ask, one read.

Every probe here runs ONE or TWO AI agents over the same file surface (`notes` →
`work_fb`), a human to speak for, and the kernel's shared memory store. The helpers
keep each probe to what it measures."""

from __future__ import annotations

import asyncio
import os
from typing import Any

from helpers.kernel import home
from helpers import Limits, connect, human
from helpers.facts_arm import WINDOW, call

DONE = 'end your run by calling done — emit <call name="done"></call>'


async def files(root, ids: list[str]) -> None:
    """`work_fb` (the run dir, open) and `notes` (a second file agent over the same
    dir — the one the model edits through), both reachable by `ids`."""
    for payload in (
        {"type": "create_agent", "parent_id": home(root), "id": "work_fb",
         "handler_module": "file_bridge.tools", "allow": ["."], "root": ".",
         "ingress_rule": "allow_all_ingress",
         "callable_by": {"rest": "*", "web": "*", **{i: "*" for i in ids}}},
        {"type": "create_agent", "parent_id": home(root), "id": "notes",
         "handler_module": "file_bridge.tools", "allow": ["."], "root": ".",
         "ingress_rule": "allow_all_ingress",
         "callable_by": {"rest": "*", "web": "*", **{i: "*" for i in ids}}},
    ):
        r = await call(root, root.id, payload)
        assert not r.get("error"), f"create {payload['id']}: {r}"
    await human(root, ai=list(ids))


async def agent(root, core, bk, aid: str, *, strategy: str = "fold", window: int | None = None,
                extra_can_call: dict | None = None, extra_callable_by: dict | None = None,
                record: dict | None = None):
    """One AI agent: notes + the human + itself, the given strategy and window."""
    w = window or WINDOW
    return await connect(
        root, core, bk, agent_id=aid,
        can_call={"notes": "*", "cli": "*", "user": "*", aid: "*", **(extra_can_call or {})},
        callable_by={"user": "*", **(extra_callable_by or {})},
        limits=Limits.from_env(os.environ),
        record={"context_window": w, "num_ctx": w, "context_strategy": strategy,
                "context_async": True, "context_titles": True, **(record or {})},
    )


async def ask(root, aid: str, text: str, *, timeout: float = 300.0) -> None:
    """The human sends to an EXISTING agent and waits for its chain to go quiet."""
    from helpers.ai import _live_tasks

    r = await call(root, "user", {"type": "send", "target": aid,
                                  "payload": {"type": "send", "text": text, "client_id": aid}})
    assert not (isinstance(r, dict) and r.get("error")), f"ask {aid}: {r}"
    end = asyncio.get_event_loop().time() + timeout
    await asyncio.sleep(2.0)
    quiet = 0
    while asyncio.get_event_loop().time() < end:
        if not await _live_tasks(root, aid):
            quiet += 1
            if quiet >= 3:
                return
        else:
            quiet = 0
        await asyncio.sleep(1.0)


def answers(rows: list[dict], since: int = 0) -> str:
    """What the agent SAID (assistant turns) from row index `since` on."""
    return " ".join(str(r.get("content") or "") for r in rows[since:] if r.get("role") == "assistant")


def reads(rows: list[dict], path_prefix: str = "filler_") -> list[str]:
    """Every file the agent asked `notes` to read, in order."""
    from helpers import ctx_score

    return [str(c["payload"].get("path", "")) for c in ctx_score.calls_made(rows, target="notes", verb="read")
            if str(c["payload"].get("path", "")).startswith(path_prefix)]


def seen(rows: list[dict], path_prefix: str = "filler_") -> set[str]:
    """Every file whose read REPLY reached the agent — a read cancelled mid-flight (its
    reply never delivered) is not one. The reply's `origin` names the path."""
    import re

    from helpers import ctx_score

    return {m.group(0) for b in ctx_score.reply_blocks(rows, agent="notes", verb="read")
            for m in re.finditer(rf"{re.escape(path_prefix)}[\w.]+\.txt", b)}


async def status(root, aid: str) -> dict[str, Any]:
    return await call(root, aid, {"type": "context_status"})


def report(checks) -> bool:
    for name, ok in checks:
        print(f"  [{'ok' if ok else 'XX'}] {name}")
    return all(ok for _, ok in checks)
