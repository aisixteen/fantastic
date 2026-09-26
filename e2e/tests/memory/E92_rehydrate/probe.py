"""memory/rehydrate — a kernel starts on a memory store it did not write (stubbed model).

The directory holds a previous run's records: the root (its guid, and the id bound
`next_id` — past every seeded id), the store, an AI agent, a viewer, a worker. The
store's tables are INJECTED before `kernel_start`: the AI's conversation (a goal, a
stretch cleared by a clear-fold, then a fold and a live tail), a frozen grant from
the AI to the viewer, and the kernel's own rows (a named execution that completed,
one that failed and was never acked). Scored after the start and ONE stubbed turn:
the AI's view is rebuilt from its head (the fold visible as its one-line row, nothing
from before the clear), grep reaches behind the clear, `ledger` reads the kernel's
rows back (the dead letter still dead), the named execution is not re-run, and the
viewer still reads the fold under its grant."""

from __future__ import annotations

import asyncio
import json
import os
import sys
import tempfile
from pathlib import Path

sys.path.insert(
    0,
    str(next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())),
)
from helpers import memory, verdict

GUID = "g92rehydrate"
BASE = {"$self": "*", "$parent": "*", "$children": "*"}


def _record(d: Path, rec: dict) -> None:
    p = d if rec.get("parent_id") is None else d / "agents" / rec["id"]
    p.mkdir(parents=True, exist_ok=True)
    (p / "agent.yaml").write_text(json.dumps(rec))


class _Stub:
    last_prompt_tokens = None
    seen: list = []

    async def chat(self, messages, max_tokens=None):
        _Stub.seen = messages
        yield 'noted <call name="done"></call>'


def _tables() -> dict:
    ai, k, viewer = f"{GUID}|ai", f"{GUID}|kernel", f"{GUID}|viewer"
    conv = [
        {"id": 100, "role": "user", "content": "the goal: keep the survey going"},
        {"id": 101, "role": "user", "content": "the codename behind the clear is ORCHID-7"},
        {"id": 102, "role": "assistant", "content": "noted ORCHID-7"},
        {"id": 103, "role": "user", "kind": "clear", "refs": [100, 101, 102],
         "title": "fresh start", "content": "[cleared 103 · 3 rows] fresh start"},
        {"id": 104, "role": "user", "content": "read filler_1 please"},
        {"id": 105, "role": "assistant", "content": "filler_1 says: nothing unusual"},
        {"id": 106, "role": "user", "kind": "fold", "refs": [104, 105], "level": 1,
         "title": "filler_1 surveyed", "body": "nothing unusual in filler_1",
         "content": '[fold 106 · 2 refs · L1] "filler_1 surveyed"\nnothing unusual in filler_1'},
        {"id": 107, "role": "user", "content": "carry on"},
    ]
    kernel = [
        {"id": 200, "kind": "call", "agent": "worker", "verb": "work", "name": "nightly",
         "named": True, "retries": 0, "attempts": 1, "caller": None,
         "payload": {"type": "work"}},
        {"id": 201, "kind": "settle", "agent": "kernel_state", "parent": 200,
         "settled": "worker", "attempts": 1,
         "outcome": {"status": "completed", "result": {"runs": 41}}},
        {"id": 202, "kind": "call", "agent": "worker", "verb": "fail", "named": False,
         "retries": 0, "attempts": 1, "caller": None, "payload": {"type": "fail"}},
        {"id": 203, "kind": "settle", "agent": "kernel_state", "parent": 202,
         "settled": "worker", "attempts": 1,
         "outcome": {"status": "failed", "result": {"error": "boom", "raised": True}}},
    ]
    return {
        "rows": [{"owner": ai, "row": r} for r in conv] + [{"owner": k, "row": r} for r in kernel],
        "fold_refs": [
            {"owner": ai, "fold": 103, "members": [100, 101, 102]},
            {"owner": ai, "fold": 106, "members": [104, 105]},
        ],
        "grants": [
            {"grant": 1, "owner": ai, "reader": viewer, "scope": {"fold": 106},
             "mode": "frozen", "anchor": 106, "as_of": 107, "until": None,
             "why": "seeded", "created_by": None, "ts": 0},
        ],
    }


async def main():
    os.chdir(tempfile.mkdtemp(prefix="memory_rehydrate_"))
    d = Path(".fantastic")
    _record(d, {"id": "kernel_state", "handler_module": "kernel_state.tools", "guid": GUID,
                "next_id": 50_000, "memory_store_id": memory.MEMORY})
    _record(d, memory.store_record("kernel_state", admins=["kernel"],
                                   callable_by={**BASE, "ai": "*", "viewer": "*"}))
    _record(d, {"id": "ai", "handler_module": "ollama_backend.tools", "parent_id": "kernel_state",
                "model": "stub", "context_window": 32000, "context_strategy": "fold",
                **memory.ai_memory_meta({**BASE, "cli": "*"})})
    _record(d, {"id": "viewer", "parent_id": "kernel_state", "can_call": {**BASE, memory.MEMORY: "*"}})
    _record(d, {"id": "worker", "handler_module": "slow_probe.tools", "parent_id": "kernel_state"})
    sys.path.insert(0, str(next(p for p in Path(__file__).resolve().parents if (p / ".e2e-root").exists()) / "probes"))
    from _testkit import boot_root
    from ai_core import core
    from cli import Cli

    from kernel import sender_context

    root = boot_root()
    Cli(root.ctx, parent=root)
    try:
        injected = await memory.inject(root, _tables())
        started = await root.ctx.start(boot_all=True)
        core._providers["ai"] = _Stub()
        turn = (await root.send([{"target": "ai", "payload": {"type": "send", "text": "status?"}}]))[0]
        await asyncio.sleep(0.3)
        sent = "\n".join(str(m.get("content")) for m in _Stub.seen)
        view = await memory.latest_view(root, "ai")
        rows = await memory.rows_of(root, "ai")
        recall = [r for r in rows if r.get("kind") == "recall"]
        grep = (await root.send([{"target": "ai", "payload": {"type": "grep", "query": "orchid-7"}}]))[0]["result"]
        led = (await root.send([{"target": "kernel", "payload": {"type": "ledger"}}]))[0]["result"]["executions"]
        by = {e["id"]: e for e in led}
        again = (await root.send([{"target": "worker", "payload": {"type": "work"}, "id": "nightly"}]))[0]
        with sender_context("viewer"):
            shared = (await root.send([{"target": memory.MEMORY, "payload": {"type": "read", "grant": 1}}]))[0]["result"]
        print(f"     injected={injected} started={started.get('started')} turn={turn['status']} "
              f"view={len((view or {}).get('ids') or [])} recall={len(recall)} grep={grep.get('total')} "
              f"ledger={[(e['id'], e['status']) for e in led if e['id'] < 1000]} again={again['result']} "
              f"shared={[r['id'] for r in shared.get('rows') or []]}")
        checks = [
            ("the seeded tables were injected before the start", injected == {"rows": 12, "grants": 1}),
            ("the kernel started on the seeded store", started.get("started") is True),
            ("the first turn ran", turn["status"] == "completed"),
            ("the model saw the fold as its one-line row", '[fold 106 · 2 refs' in sent),
            ("nothing from before the clear reached the model", "ORCHID-7" not in sent),
            ("the view was rebuilt on the first turn", bool(view) and bool(recall)),
            ("grep reaches behind the clear", (grep.get("total") or 0) >= 1),
            ("the ledger reads the kernel's rows back", by.get(200, {}).get("status") == "completed"),
            ("the dead letter is still dead", by.get(202, {}).get("status") == "failed" and not by[202].get("acked")),
            ("the named execution was not re-run", again["result"] == {"runs": 41}),
            ("the viewer still reads the fold under its grant",
             [r["id"] for r in shared.get("rows") or []] == [104, 105, 106]),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(allok, "a seeded store came back whole: view, clear, folds, grants, ledger, dedup"
                if allok else "a rehydrate check failed")
    finally:
        core._providers.pop("ai", None)
        await root.ctx.shutdown()


if __name__ == "__main__":
    asyncio.run(main())
