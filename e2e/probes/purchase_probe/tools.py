"""The `purchase_desk` role — spends real money.

Only a person may press this button, which is why the scenario grants the
AI no edge to it: the AI proposes, and the human operator dispatches. The
`user_actions` scenario then presses TWICE with one execution id, and the
ledger is what must make the second press harmless.
"""

from __future__ import annotations

import secrets

_STATE: dict = {"count": 0, "receipts": []}

SENTENCE = ("Purchase desk. Spends real money — only a person may press "
            "the button.")
VERBS = {
    "buy": "args: item:str — buys ONE. Spending is a side effect.",
    "count": "how many purchases have ever been made",
}


async def handler(id: str, payload: dict, agent) -> dict | None:
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {"id": id, "sentence": SENTENCE}
    if verb == "buy":
        _STATE["count"] += 1
        receipt = secrets.token_hex(4)
        _STATE["receipts"].append(receipt)
        return {"bought": payload.get("item"), "receipt": receipt,
                "n": _STATE["count"]}
    if verb == "count":
        return {"count": _STATE["count"]}
    return {"error": f"purchase_probe: unknown verb {verb!r}"}
