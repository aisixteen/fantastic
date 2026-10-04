"""memory/growth — the kernel's store does not grow without bound (no model).

`ledger_cap` is the store-side window. 150 plain calls, one named, one failed: after
the flushes the store holds at most the window of executions; the named one (its dedup
contract) and the unacked dead letter are still there; nothing in flight was touched;
every surviving call still has its settle row."""

from __future__ import annotations

import asyncio
import sys
from pathlib import Path

sys.path.insert(
    0,
    str(next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())),
)
from helpers import memory, verdict
from helpers.kernel import open_kernel, home

CAP = 30


async def main():
    root, core, close = await open_kernel(prefix="memory_growth_", reach=["worker"])
    try:
        await root.send([{"target": root.id, "payload": {
            "type": "create_agent", "parent_id": home(root), "id": "worker",
            "handler_module": "slow_probe.tools"}}])
        root.ctx.ledger_cap = CAP
        named = (await root.send([{"target": "worker", "payload": {"type": "work"}, "id": "keep"}]))[0]
        dead = (await root.send([{"target": "worker", "payload": {"type": "fail"}}]))[0]
        for i in range(150):
            await root.send([{"target": "worker", "payload": {"type": "work"}}])
            if i % 25 == 0:
                await root.ctx.flush_executions()
        await root.ctx.flush_executions()
        tables = await memory.dump(root)
        guid = root.record["guid"]
        rows = [r["row"] for r in tables["rows"] if r["owner"] == f"{guid}|kernel"]
        calls = [r for r in rows if r.get("kind") == "call" and not r.get("internal")]
        ids = {r["id"] for r in calls}
        settled = {r["parent"] for r in rows if r.get("kind") == "settle"}
        again = (await root.send([{"target": "worker", "payload": {"type": "work"}, "id": "keep"}]))[0]
        print(f"     calls in store={len(calls)} (window {CAP})  rows={len(rows)}  named kept={named['pid'] in ids}  "
              f"dead kept={dead['pid'] in ids}  dedup={again['result'] == named['result']}")
        checks = [
            (f"the store holds at most the window ({len(calls)} ≤ {CAP + 5})", len(calls) <= CAP + 5),
            ("the named execution survived the pruning", named["pid"] in ids),
            ("its dedup contract still holds", again["result"] == named["result"]),
            ("the unacked dead letter survived", dead["pid"] in ids),
            ("every surviving settled call kept its settle row",
             all(c["id"] in settled for c in calls if c["id"] not in root.ctx.executions)),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(allok, "the kernel's store stays inside its window; named and dead letters kept"
                if allok else "a growth check failed")
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
