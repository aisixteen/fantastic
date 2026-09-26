"""memory/graceful_stop — IN FLIGHT AT STOP (no model).

A slow call is in flight when the kernel stops. The stop is graceful: drain, the
quiet window, then the call is CANCELLED — it settles, its settle row is flushed into
the kernel's memory store with the last rows, and a new kernel on the same directory
(the store's tables carried across) lists it in `ledger` as a DEAD LETTER until it is
acked. There is no re-send path: nothing runs it again.
"""

from __future__ import annotations

import asyncio
import sys
from pathlib import Path

sys.path.insert(
    0,
    str(next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())),
)
from helpers import verdict
from helpers.kernel import open_kernel, restart


async def _call(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


async def main():
    root, core, close = await open_kernel(prefix="memory_stop_", reach=["slow"])
    try:
        r = await _call(root, root.id, {"type": "create_agent", "parent_id": root.id,
                                         "id": "slow", "handler_module": "slow_probe.tools"})
        assert not r.get("error"), r
        root.ctx.QUIET_TIMEOUT = 0.5
        inflight = asyncio.create_task(root.send([{"target": "slow", "payload": {"type": "slow"}}]))
        for _ in range(100):
            if (await _call(root, "slow", {"type": "started"})).get("started"):
                break
            await asyncio.sleep(0.05)
        pid = next(t["id"] for t in root.ctx.task_rows() if t["verb"] == "slow")
        started = (await _call(root, "slow", {"type": "started"})).get("started")
        root2, stop = await restart(root, core)
        settled = await asyncio.wait_for(inflight, 5)
        led = (await _call(root2, "kernel", {"type": "ledger"}))["executions"]
        entry = next((e for e in led if e["id"] == pid), None)
        rerun = (await _call(root2, "slow", {"type": "started"})).get("started")
        acked = await _call(root2, "kernel", {"type": "ack_deadletter", "id": pid})
        led2 = (await _call(root2, "kernel", {"type": "ledger"}))["executions"]
        after = next((e for e in led2 if e["id"] == pid), {})
        print(f"     stop={stop}  settled={settled[0]['status']}  entry={entry}  acked={acked}")
        checks = [
            ("the stop cancelled the call in flight", pid in (stop or {}).get("cancelled", [])),
            ("its caller saw it settle cancelled", settled[0]["status"] == "cancelled"),
            ("the new kernel's ledger holds it — from the store", entry is not None),
            ("it is a dead letter", (entry or {}).get("status") == "cancelled" and not (entry or {}).get("acked")),
            ("nothing ran it again", rerun == started),
            ("ack_deadletter retires it", acked.get("acked") is True and after.get("acked") is True),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(allok, "in flight at the stop → cancelled → a dead letter in the store, acked"
                if allok else "a graceful_stop check failed")
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
