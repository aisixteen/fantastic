"""memory/fork_marks — two forks of one mark start from the same rows.

Python runtime, NO MODEL: a stub provider answers seven live turns. After the sixth the
agent marks its history; `fork_points` lists the mark; `fork {at}` runs twice on it.
Scored: each fork holds the parent's rows up to the mark, byte for byte (ids and
content), the two forks equal each other, the seventh turn reached neither, the mark
reads back with `rows_after` 2 and the record's model, and an `at` that cuts a turn is
refused naming its reason and creates no agent.
"""

from __future__ import annotations

import asyncio
import json
import sys
from pathlib import Path

sys.path.insert(
    0,
    str(next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())),
)
from helpers import memory, verdict
from helpers.ai import _live_tasks
from helpers.kernel import WRAPS, home, open_kernel, replace_with


class _Stub:
    last_prompt_tokens = None
    seen: list = []

    async def chat(self, messages, max_tokens=None):
        _Stub.seen.append([dict(m) for m in messages])
        yield f'ok {len(_Stub.seen)} <call name="done"></call>'


async def _call(root, payload, target="ai"):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


async def _turn(root, text):
    r = await _call(root, {"type": "send", "text": text})
    assert not (isinstance(r, dict) and r.get("error")), r
    for _ in range(200):
        await asyncio.sleep(0.05)
        if not await _live_tasks(root, "ai"):
            await asyncio.sleep(0.2)
            if not await _live_tasks(root, "ai"):
                return
    raise AssertionError("the turn never went quiet")


async def _rows(root, aid) -> list[dict]:
    """The agent's stored rows, without the harness's `agent` stamp."""
    rows = await memory.rows_of(root, aid)
    return [{k: v for k, v in r.items() if k != "agent"} for r in rows]


def _dump(rows):
    return json.dumps(rows, sort_keys=True)


async def main():
    root, core, close = await open_kernel(prefix="mem_fork_", reach=["ai", "cli"])
    try:
        _Stub.seen = []
        WRAPS["ai"] = replace_with(_Stub())
        r = (await root.send([{"target": root.id, "payload": {
            "type": "create_agent", "parent_id": home(root), "id": "ai",
            "handler_module": "ollama_backend.tools", "model": "stub",
            "endpoint": "http://stub.invalid",  # never dialled: WRAPS seats the stub
            "context_window": 32000, "context_strategy": "fold",
            **memory.ai_memory_meta({"cli": "*"})}}]))[0]["result"]
        assert not r.get("error"), r
        await memory.open_ingress(root, memory.MEMORY, "ai")
        for i in range(6):
            await _turn(root, f"task step {i}")

        marked = await _call(root, {"type": "mark", "label": "six steps done",
                                    "note": "before the seventh", "version": "abc123"})
        at = marked.get("at")
        await _turn(root, "task step 6")
        points = await _call(root, {"type": "fork_points"})
        point = (points.get("points") or [{}])[0]

        parent_rows = await _rows(root, "ai")
        prefix = [r for r in parent_rows if r.get("id") is not None and r["id"] <= at
                  and r.get("kind") not in ("mark", "pause", "held", "late")]
        first = await _call(root, {"type": "fork", "at": at, "id": "twin_a"})
        second = await _call(root, {"type": "fork", "at": at, "id": "twin_b"})
        a_rows, b_rows = await _rows(root, "twin_a"), await _rows(root, "twin_b")
        # an `at` inside a turn (the 2nd conversation row: a question without its answer)
        conv = [r for r in parent_rows if r.get("role") in ("user", "assistant")]
        cut = await _call(root, {"type": "fork", "at": conv[0]["id"], "id": "twin_cut"})
        listed = (await root.send([{"target": "kernel", "payload": {"type": "list_agents"}}]))[0]["result"]
        agents = {a.get("id") for a in (listed or {}).get("agents") or []}

        print(f"     mark={marked} point={point} first={first} second={second} cut={cut.get('reason')}")
        checks = [
            ("the mark answers its place, its label and an id",
             isinstance(at, int) and marked.get("label") == "six steps done"
             and isinstance(marked.get("marked"), int) and marked["marked"] > at),
            ("fork_points lists the mark with its note, version, model and rows_after 2",
             point.get("at") == at and point.get("note") == "before the seventh"
             and point.get("version") == "abc123" and point.get("model") == "stub"
             and point.get("rows_after") == 2 and point.get("agent") == "ai"),
            ("both forks reply {id, copied, at}",
             first.get("id") == "twin_a" and second.get("id") == "twin_b"
             and first.get("at") == at and first.get("copied") == len(prefix) == second.get("copied")),
            ("the parent held more than the prefix (a seventh turn)", len(parent_rows) > len(prefix) + 2),
            ("fork A holds exactly the parent's rows up to the mark, ids and content alike",
             _dump(a_rows) == _dump(prefix)),
            ("fork B holds exactly the parent's rows up to the mark, ids and content alike",
             _dump(b_rows) == _dump(prefix)),
            ("the two forks are the same rows", _dump(a_rows) == _dump(b_rows)),
            ("the seventh turn reached neither fork",
             all("task step 6" not in json.dumps(rs) for rs in (a_rows, b_rows))),
            ("an at inside a turn is refused cuts_turn and no agent is created",
             cut.get("reason") == "cuts_turn" and "twin_cut" not in agents),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(allok, "two forks of one mark started from the same rows"
                if allok else "a fork check failed")
    finally:
        WRAPS.pop("ai", None)
        await close()


if __name__ == "__main__":
    asyncio.run(main())
