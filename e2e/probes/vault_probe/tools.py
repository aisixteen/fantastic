"""A VAULT — opens only with a code its DETAILED docs teach.

The fixture of the `docs_mechanics` test: the `open` verb's oneliner names its
argument (`code:str`), and only the detailed docs say how the code is made from
the vault's `serial` (shown on its reflect). A model that never pulls detailed
docs for `open` cannot open it, except by guessing an unguessable rule.

Policy is ON THE RECORD (`serial`, `secret`), written by the fixture; the bundle
holds nothing. `opened` on reflect and `_OPENED` are the tally the scorer reads.
"""

from __future__ import annotations

SENTENCE = "A vault. It holds one secret and opens only with the right code."
VERBS = {
    "open": (
        "args: code:str (req). Opens the vault and returns {secret}.\n\n"
        "THE CODE: this vault's `serial` (its reflect shows it) written BACKWARDS "
        "and prefixed with `V-` — a vault with serial `48213` opens with "
        "`V-31284`. Any other code is refused `bad_arg`, and the vault stays shut."
    ),
    "count": "No args. How many times this vault has been opened. Returns {opened}.",
}

_OPENED: dict[str, int] = {}


async def handler(id: str, payload: dict, agent) -> dict | None:
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    rec = agent.get(id) or {}
    if verb == "reflect":
        return {
            "id": id,
            "sentence": SENTENCE,
            "serial": rec.get("serial"),
            "opened": _OPENED.get(id, 0),
        }
    if verb == "open":
        code = payload.get("code")
        want = "V-" + str(rec.get("serial") or "")[::-1]
        if not isinstance(code, str) or code.strip() != want:
            return {
                "error": "vault_probe: open: wrong code — the vault stays shut",
                "reason": "bad_arg",
                "arg": "code",
            }
        _OPENED[id] = _OPENED.get(id, 0) + 1
        return {"secret": rec.get("secret")}
    if verb == "count":
        return {"opened": _OPENED.get(id, 0)}
    return {
        "error": f"vault_probe: unknown type {verb!r}",
        "reason": "unhandled",
        "verb": verb,
    }
