"""coding/kernel_restart — a kernel stopped and started again, observed from the AI
agents' side.

Two AI agents (a and b) each survey their own fillers at a 16k window (strips and/or folds
fire as the window decides); b's corpus carries an audit marker. The stop runs its agent
half (`shutdown_agents`) and the kernel's teardown; a NEW kernel starts on the same
`.fantastic`, the memory store's tables carried across (a ram_store dies with its process
— `helpers.kernel.restart`). Scored on the store — the counter continues above every id of
the first life, each agent's rows are ITS OWN, the folds and the view are back — and on
the agents: each, asked for what it found, answers from its own conversation (b names the
marker, a says none) without re-reading its files. Reply bodies are never persisted, so b
can only name the marker if it SAID it when it read it.
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
from helpers.facts_arm import (
    FOLD_ARMS,
    WINDOW,
    call,
    leg,
    plant_fillers,
    read_view,
    store_rows,
)
from helpers.kernel import open_kernel, restart

BK = _resolve_backend()
DONE = 'end your run by calling done — emit <call name="done"></call>'
FB = "restart_fb"
_, STRATEGY, EXTRA = FOLD_ARMS[1]
N = 8


async def _fixture(root, ids):
    # THE STORE: a file_bridge rooted at `.fantastic` — what the loader discovers as its
    # provider (records, ledger and now the message log). Created BEFORE the root boots.
    r = await call(root, root.id, {"type": "create_agent", "parent_id": root.id, "id": FB,
                                    "handler_module": "file_bridge.tools", "root": ".fantastic",
                                    "ingress_rule": "allow_all_ingress",
                                    "callable_by": {**{i: "*" for i in ids}, "notes": "*", "rest": "*", "web": "*"}})
    assert not r.get("error"), f"create {FB}: {r}"
    # the editor the agents read through (its own bridge over the run dir)
    r = await call(root, root.id, {"type": "create_agent", "parent_id": root.id, "id": "work_fb",
                                    "handler_module": "file_bridge.tools", "root": ".",
                                    "ingress_rule": "allow_all_ingress",
                                    "callable_by": {"notes": "*", "rest": "*", "web": "*"}})
    assert not r.get("error"), f"create work_fb: {r}"
    r = await call(root, root.id, {"type": "create_agent", "parent_id": root.id, "id": "notes",
                                    "handler_module": "text_file.tools", "target_id": "work_fb",
                                    "can_call": {"work_fb": "*"},
                                    "callable_by": {**{i: "*" for i in ids}, "rest": "*", "web": "*"}})
    assert not r.get("error"), f"create notes: {r}"
    # THE ORDER: hydrated (boot_root did it, cold) → the root boots and the pump
    # subscribes → only now the agents that will talk.
    await call(root, root.id, {"type": "boot"})
    await human(root, ai=list(ids))


async def _connect(root, core, aid):
    return await connect(root, core, BK, agent_id=aid,
                         can_call={FB: "*", "notes": "*", "cli": "*", "user": "*", aid: "*"},
                         callable_by={"user": "*"}, limits=Limits.from_env(os.environ),
                         record={**leg(FB), "context_window": WINDOW, "num_ctx": WINDOW,
                                 "context_strategy": STRATEGY, **EXTRA})


async def _ask(root, aid: str, text: str, *, timeout: float = 240.0) -> None:
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


def _hydrated(before: list[dict], after: list[dict]) -> bool:
    """`after` starts with exactly `before`'s ids, and any row past them is the
    booted agent's own `recall` (see the check that calls this)."""
    ids0 = [r["id"] for r in before]
    ids1 = [r["id"] for r in after]
    return ids1[: len(ids0)] == ids0 and all(r.get("kind") == "recall" for r in after[len(ids0):])


async def main():
    root, core, close = await open_kernel(prefix="coding_restart_", reach=[FB, "work_fb", "notes", "ai_a", "ai_b", "cli", "user"])
    try:
        marker = secrets.token_hex(4)
        plant_fillers(N, marker_in=3, marker=marker, pad_to=6800)
        await _fixture(root, ["ai_a", "ai_b"])
        task = (f"Read `filler_0.txt` … `filler_{N - 1}.txt` through `notes`, ONE PER TURN, noting anything "
                f"unusual (an audit marker, if any). Do not write anything. After the last file, {DONE}.")
        # a reads only the odd files (no marker there), b reads all (the marker is in filler_3)
        a = await _connect(root, core, "ai_a")
        await drive(a, task.replace(f"`filler_0.txt` … `filler_{N - 1}.txt`", "`filler_1.txt`, `filler_5.txt`, `filler_7.txt`"), client_id="ai_a", via="user")
        await a.close()
        b = await _connect(root, core, "ai_b")
        await drive(b, task, client_id="ai_b", via="user")
        await b.close()
        rows_a1, rows_b1 = await store_rows(root, core, "ai_a"), await store_rows(root, core, "ai_b")
        folds_b1 = sum(1 for r in rows_b1 if r.get("kind") == "fold")
        top1 = root.ctx.next_id
        # a reply body this run held (context_async digests every filler read)
        digested_b1 = sum(1 for r in rows_b1 if r.get("role") == "user" and "NOT inlined" in str(r.get("content")))
        # ── STOP (the agent half, then the kernel's teardown) and a NEW KERNEL on the
        # same dir, the memory store's tables carried across ──
        root2, stop = await restart(root, core)
        top2 = root2.ctx.next_id
        rows_a2, rows_b2 = await store_rows(root2, core, "ai_a"), await store_rows(root2, core, "ai_b")
        view_b = await read_view(core, root2, "ai_b")
        # BODIES ARE NEVER PERSISTED: after the restart a digested reply is its origin
        # line only — the model re-reads what it still needs.
        stubs_b2 = [r for r in rows_b2 if r.get("role") == "user" and "origin:" in str(r.get("content"))]
        ask = ("From what you already read in this conversation — do NOT read any file again — "
               "what audit marker did you find, if any? Answer with the value or the word none, then "
               + DONE)
        # THE AGENTS ALREADY EXIST — their records came back with the tree. No connect,
        # no create: the human sends; the first turn reads the head and recalls.
        await _ask(root2, "ai_a", ask)
        await _ask(root2, "ai_b", ask)
        rows_a3, rows_b3 = await store_rows(root2, core, "ai_a"), await store_rows(root2, core, "ai_b")
        ans_a = " ".join(str(r.get("content")) for r in rows_a3[len(rows_a2):] if r.get("role") == "assistant")
        ans_b = " ".join(str(r.get("content")) for r in rows_b3[len(rows_b2):] if r.get("role") == "assistant")
        reads_after = sum(1 for r in rows_a3[len(rows_a2):] + rows_b3[len(rows_b2):] if '<type>read</type>' in str(r.get("content")))
        print(f"     life1: a={len(rows_a1)} b={len(rows_b1)} folds_b={folds_b1} digested_b={digested_b1} next_id={top1}  "
              f"life2: a={len(rows_a2)} b={len(rows_b2)} next_id={top2} view_b={len((view_b or {}).get('ids') or [])} origin_stubs_b={len(stubs_b2)}  "
              f"stop={ {k: (v if k != 'cancelled' else len(v)) for k, v in (stop or {}).items()} }")
        checks = [
            ("the agent half of the stop ran", (stop or {}).get("drained") is True),
            ("the counter continues above every id of the first life", top2 >= top1),
            # THE CARRIED ROWS ARE THE PREFIX, byte for byte; anything past them may only
            # be the rebooted agent's own `[recall …]` row.
            ("a came back with its own rows only", _hydrated(rows_a1, rows_a2)),
            ("b came back with its own rows only", _hydrated(rows_b1, rows_b2)),
            ("b's folds are back", sum(1 for r in rows_b2 if r.get("kind") == "fold") == folds_b1),
            # A VIEW EXISTS ONLY ONCE A PROJECTION WROTE ONE; scored when life 1 folded.
            ("b's view is back and resolves", (folds_b1 == 0 and not view_b) or (bool(view_b) and all(any(r["id"] == e["id"] for r in rows_b2) for e in view_b["ids"]))),
            ("b names the marker from its carried context", marker in ans_b),
            ("a says none — it never saw the marker", marker not in ans_a),
            ("neither re-read a file", reads_after == 0),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(allok, "stopped, hydrated into a new kernel, each agent continued from its own rows"
                if allok else "a kernel_restart check failed")
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
