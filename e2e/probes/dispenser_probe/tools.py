"""The `dispenser` role — issuing a coin is a SIDE EFFECT.

Calling `dispense` twice dispenses twice. That is the whole point: the
`ledger` scenario is trying to catch a second issue, so this agent must
never quietly deduplicate on its own.
"""

from __future__ import annotations

import secrets

#: Process-local tally. One kernel process per scenario run, so module
#: state IS the agent's state — the same lifetime the compiled probes get.
_STATE: dict = {"count": 0, "receipts": []}

SENTENCE = (
    "Coin dispenser — one coin per `dispense` call. A coin must never be "
    "issued twice for one job."
)
VERBS = {
    "dispense": "args: job:str — issues ONE coin, returns {coin, receipt}. "
                "Issuing is a side effect: calling twice dispenses twice.",
    "count": "how many coins have ever been dispensed",
}


async def handler(id: str, payload: dict, agent) -> dict | None:
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {"id": id, "sentence": SENTENCE, "verbs": VERBS}
    if verb == "dispense":
        _STATE["count"] += 1
        receipt = secrets.token_hex(4)
        _STATE["receipts"].append(receipt)
        return {"coin": _STATE["count"], "receipt": receipt,
                "job": payload.get("job")}
    if verb == "count":
        return {"count": _STATE["count"]}
    return {"error": f"dispenser_probe: unknown verb {verb!r}"}
