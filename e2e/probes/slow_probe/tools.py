"""The `slow` role — work that is still IN FLIGHT when the kernel stops, work that
FAILS, and work that just runs. TEST-ONLY: importable through `e2e/probes` alone."""

from __future__ import annotations

import asyncio

_STATE: dict = {"started": 0, "runs": 0}

SENTENCE = "Probe work: `slow` (never finishes by itself), `work` (counts), `fail` (raises)."
VERBS = {
    "slow": "sleeps for an hour — in flight until cancelled",
    "work": "counts one run, returns {runs}",
    "fail": "raises — a failed execution, a dead letter",
    "started": "how many `slow` calls have started",
}


async def handler(id: str, payload: dict, agent) -> dict | None:
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {"id": id, "sentence": SENTENCE, "verbs": VERBS}
    if verb == "slow":
        _STATE["started"] += 1
        await asyncio.sleep(3600)
        return {"finished": True}
    if verb == "work":
        _STATE["runs"] += 1
        return {"runs": _STATE["runs"]}
    if verb == "fail":
        raise RuntimeError("slow_probe: failed on purpose")
    if verb == "started":
        return {"started": _STATE["started"]}
    return {"error": f"slow_probe: unknown verb {verb!r}", "reason": "unhandled"}
