"""The `slow` role — work that is still IN FLIGHT when the kernel stops, work that
FAILS, work that just runs, and work that takes a stated time (`sleep`, the slow
verb the parity AI paths time out on). TEST-ONLY: importable through `e2e/probes`
alone; the compiled kernels carry a twin behind their scenario-probe gate."""

from __future__ import annotations

import asyncio

_STATE: dict = {"started": 0, "runs": 0}

SENTENCE = (
    "Probe work: `slow` (never finishes by itself), `work` (counts), `fail` (raises), "
    "`sleep` (takes `seconds`)."
)
VERBS = {
    "slow": "sleeps for an hour — in flight until cancelled",
    "work": "counts one run, returns {runs}",
    "fail": "raises — a failed execution, a dead letter",
    "started": "how many `slow` calls have started",
    "sleep": "seconds:number (req) — sleeps that long, then returns {slept}",
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
    if verb == "sleep":
        s = payload.get("seconds")
        if isinstance(s, bool) or not isinstance(s, (int, float)) or s < 0:
            return {
                "error": "slow_probe: sleep needs seconds, a non-negative number",
                "reason": "bad_arg",
                "arg": "seconds",
            }
        await asyncio.sleep(s)
        return {"slept": s}
    return {"error": f"slow_probe: unknown verb {verb!r}", "reason": "unhandled"}
