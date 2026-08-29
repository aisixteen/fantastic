"""The `flaky_plant` role — a pump that fails twice then works, and a
valve that never does.

Both RAISE on failure rather than returning an error dict: a terminal
failure is precisely what a retry budget and a dead-letter row exist to
record, and an error-shaped reply would settle the execution as success.
"""

from __future__ import annotations

import secrets

_STATE: dict = {"count": 0}

SENTENCE = "Industrial plant. Its pump is unreliable; its valve has seized."
VERBS = {
    "pump": "no args — runs the pump. Fails intermittently.",
    "valve": "no args — operates the valve.",
    "count": "how many times the plant has been called",
}


async def handler(id: str, payload: dict, agent) -> dict | None:
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {"id": id, "sentence": SENTENCE, "verbs": VERBS}
    if verb == "pump":
        _STATE["count"] += 1
        if _STATE["count"] < 3:
            raise RuntimeError("pump stalled")
        return {"pumped": True, "serial": secrets.token_hex(3)}
    if verb == "valve":
        _STATE["count"] += 1
        raise RuntimeError("valve is rusted shut")
    if verb == "count":
        return {"count": _STATE["count"]}
    return {"error": f"plant_probe: unknown verb {verb!r}"}
