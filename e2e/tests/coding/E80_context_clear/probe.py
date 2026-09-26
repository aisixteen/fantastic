"""coding/context_clear — a clear is a BOUNDARY, not a deletion.

Three phases on ONE agent, scored on the store and on the agent:
  1. chat / clear / chat — a codename told before the clear is not in the agent's
     thread after it (the `history` verb starts at the `[cleared …]` row; asked without
     tools, the agent answers NONE), yet `grep` still finds it behind the boundary and
     the agent's memory store still holds every row — the clear is a CLEAR-FOLD whose
     members are the rows before it.
  2. chat / fold / clear / chat — fillers at a squeezed window make folds fire; the
     clear drops them from the thread (no fold row, no filler text after the mark), the
     marker planted mid-corpus is unknown to the agent afterwards, and `grep` reaches it.
  3. chat / fold / clear / STOP / START — a new kernel on the same dir, the memory
     store's tables carried across: the thread still starts at the last clear, the
     first-turn recall did not resurrect the folded past, the rows behind are all still
     there, and the agent still answers NONE.
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
from helpers.facts_arm import FOLD_ARMS, WINDOW, call, leg, plant_fillers
from helpers import memory
from helpers.kernel import open_kernel, restart

BK = _resolve_backend()
DONE = 'end your run by calling done — emit <call name="done"></call>'
FB = "clear_fb"
AI = "ai_c"
_, STRATEGY, EXTRA = FOLD_ARMS[1]
# 12 fillers at 4000 chars. The read reply must be INLINED for the view to grow: the
# inline cap is budget/8 = 1872 tokens ≈ 7488 chars of the RENDERED reply, and a
# 6800-char file renders (line-numbered items) past it — every runtime stored those
# as digests and the fold count then depended on how many grep turns the MODEL chose
# (python 45 turns → 4 folds, swift fewer → 0; measured 2026-09-23). At 4000 chars a
# read renders ≈ 6k chars, inlined; twelve of them ≈ 18k tokens against a 16k window,
# so folds fire on every runtime whatever the model does between reads.
N = 12
PAD = 4000


async def _fixture(root):
    r = await call(root, root.id, {"type": "create_agent", "parent_id": root.id, "id": FB,
                                    "handler_module": "file_bridge.tools", "root": ".fantastic",
                                    "ingress_rule": "allow_all_ingress",
                                    "callable_by": {AI: "*", "notes": "*", "rest": "*", "web": "*"}})
    assert not r.get("error"), f"create {FB}: {r}"
    r = await call(root, root.id, {"type": "create_agent", "parent_id": root.id, "id": "work_fb",
                                    "handler_module": "file_bridge.tools", "root": ".",
                                    "ingress_rule": "allow_all_ingress",
                                    "callable_by": {"notes": "*", "rest": "*", "web": "*"}})
    assert not r.get("error"), f"create work_fb: {r}"
    r = await call(root, root.id, {"type": "create_agent", "parent_id": root.id, "id": "notes",
                                    "handler_module": "text_file.tools", "target_id": "work_fb",
                                    "can_call": {"work_fb": "*"},
                                    "callable_by": {AI: "*", "rest": "*", "web": "*"}})
    assert not r.get("error"), f"create notes: {r}"
    await call(root, root.id, {"type": "boot"})
    await human(root, ai=[AI])


async def _connect(root, core):
    return await connect(root, core, BK, agent_id=AI,
                         can_call={FB: "*", "notes": "*", "cli": "*", "user": "*", AI: "*"},
                         callable_by={"user": "*"}, limits=Limits.from_env(os.environ),
                         record={**leg(FB), "context_window": WINDOW, "num_ctx": WINDOW,
                                 "context_strategy": STRATEGY, **EXTRA})


async def _ask(root, text: str, *, timeout: float = 240.0) -> None:
    """The human sends to the EXISTING agent and waits for its chain to go quiet."""
    from helpers.ai import _live_tasks

    r = await call(root, "user", {"type": "send", "target": AI,
                                  "payload": {"type": "send", "text": text, "client_id": AI}})
    assert not (isinstance(r, dict) and r.get("error")), f"ask: {r}"
    end = asyncio.get_event_loop().time() + timeout
    await asyncio.sleep(2.0)
    quiet = 0
    while asyncio.get_event_loop().time() < end:
        if not await _live_tasks(root, AI):
            quiet += 1
            if quiet >= 3:
                return
        else:
            quiet = 0
        await asyncio.sleep(1.0)


async def _thread(root) -> list[dict]:
    """The agent's THREAD as the model sees it — the `history` verb, every runtime."""
    out, after = [], None
    while True:
        r = await call(root, AI, {"type": "history", "limit": 200, **({"after": after} if after else {})})
        assert isinstance(r, dict) and "rows" in r, f"history refused: {r}"
        out += list(r.get("rows") or [])
        after = r.get("next")
        if not after:
            return out


async def _behind(root) -> list[dict]:
    """EVERY conversation row, the boundary ignored — the agent's memory store."""
    return await memory.rows_of(root, AI, conversation_only=True)


def _answers(rows: list[dict]) -> str:
    return " ".join(str(r.get("head") or r.get("content") or "") for r in rows if r.get("role") == "assistant")


def _starts_at_clear(thread: list[dict]) -> bool:
    return bool(thread) and thread[0].get("kind") == "clear" and not any(r.get("kind") == "clear" for r in thread[1:])


async def _grep_finds(root, needle: str) -> bool:
    r = await call(root, AI, {"type": "grep", "query": needle})
    return isinstance(r, dict) and any(needle in str(m.get("text", "")) for m in r.get("matches") or [])


ASK = ("From THIS conversation only — do NOT call grep, history, recall, read or any other agent — "
       "which {what} did I tell you? Answer with the exact value, or the single word NONE if it is "
       "not in your conversation. Then " + DONE)


async def main():
    root, core, close = await open_kernel(prefix="coding_clear_", reach=[FB, "work_fb", "notes", AI, "cli", "user"])
    try:
        code1, marker = secrets.token_hex(4), secrets.token_hex(4)
        plant_fillers(N, marker_in=3, marker=marker, pad_to=PAD)
        await _fixture(root)
        conn = await _connect(root, core)

        # ── 1. chat / clear / chat ──
        await drive(conn, f"Remember this codename: {code1}. Reply with OK and then {DONE}", client_id=AI, via="user")
        r1 = await call(root, AI, {"type": "context_clear", "why": "phase 1"})
        t1 = await _thread(root)
        await _ask(root, ASK.format(what="codename"))
        t1b = await _thread(root)
        ans1 = _answers(t1b[len(t1):])
        grep1 = await _grep_finds(root, code1)
        behind1 = await _behind(root)

        # ── 2. chat / fold / clear / chat ──
        await _ask(root, f"Read `filler_0.txt` … `filler_{N - 1}.txt` through `notes`, ONE PER TURN, noting anything "
                         f"unusual (an audit marker, if any). Do not write anything. After the last file, {DONE}")
        folds2 = sum(1 for r in await _thread(root) if r.get("kind") == "fold")
        cs2 = await call(root, AI, {"type": "context_status"})
        print(f"     p2 context_status: {cs2}")
        r2 = await call(root, AI, {"type": "context_clear", "why": "phase 2"})
        t2 = await _thread(root)
        await _ask(root, ASK.format(what="audit marker"))
        t2b = await _thread(root)
        ans2 = _answers(t2b[len(t2):])
        grep2 = await _grep_finds(root, marker)
        await conn.close()

        # ── 3. fold / clear / SHUTDOWN / BOOT ──
        await _ask(root, f"Read `filler_1.txt` and `filler_3.txt` through `notes`, one per turn, noting anything unusual. Then {DONE}")
        r3 = await call(root, AI, {"type": "context_clear", "why": "phase 3"})
        behind3 = await _behind(root)
        root2, stop = await restart(root, core)
        t3 = await _thread(root2)
        behind3b = await _behind(root2)
        await _ask(root2, ASK.format(what="audit marker"))
        t3b = await _thread(root2)
        ans3 = _answers(t3b[len(t3):])
        grep3 = await _grep_finds(root2, marker)

        print(f"     p1: clear={r1} thread={len(t1)} behind={len(behind1)} ans={ans1[:60]!r}  "
              f"p2: folds={folds2} clear={r2} thread={len(t2)} ans={ans2[:60]!r}  "
              f"p3: clear={r3} behind={len(behind3)}→{len(behind3b)} thread={len(t3)} ans={ans3[:60]!r} stop={stop.get('cancelled') if isinstance(stop, dict) else stop}")
        checks = [
            ("1 the thread starts at the clear mark, nothing before it", _starts_at_clear(t1) and code1 not in str(t1)),
            ("1 the clear-fold holds the rows before it", isinstance(r1, dict) and r1.get("rows", 0) >= 2),
            ("1 asked without tools, the agent no longer knows the codename", code1 not in ans1 and "NONE" in ans1.upper()),
            ("1 grep still reaches the codename behind the boundary", grep1),
            ("1 the memory store still holds every row", any(code1 in str(r.get("content")) for r in behind1)),
            ("2 folds fired before the clear", folds2 >= 1),
            ("2 the thread after the clear carries no fold and no filler", _starts_at_clear(t2) and not any(r.get("kind") == "fold" for r in t2) and "chunk" not in str(t2)),
            ("2 the agent no longer knows the marker", marker not in ans2 and "NONE" in ans2.upper()),
            ("2 grep still reaches the marker", grep2),
            ("3 every row behind survived the restart", len(behind3b) >= len(behind3)),
            ("3 after the restart the thread still starts at the last clear", _starts_at_clear(t3) and not any(r.get("kind") == "fold" for r in t3)),
            ("3 the rebooted agent still does not know the marker", marker not in ans3 and "NONE" in ans3.upper()),
            ("3 and grep still reaches it", grep3),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(allok, "a clear is a boundary: the thread restarts, the store keeps everything, a restart keeps the boundary"
                if allok else "a context_clear check failed")
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
