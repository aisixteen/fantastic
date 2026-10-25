"""user — the one human, as an agent. Exports a subtree as a blueprint folder; see
readme.md for the verbs, the reply, the refusals and what is never written."""

from __future__ import annotations

from kernel._refusal import UNHANDLED, refusal

from user import export

NAME = "user"


async def _reflect(id, payload, agent):
    """No args. Identity: id, sentence, handler_module, verbs."""
    return {
        "id": id,
        "sentence": "User \u2014 the one human, as an agent.",
        "handler_module": "user.tools",
        "verbs": sorted(VERBS),
    }


async def _export_subtree(id, payload, agent):
    """args: agent_id:str (req), path:str (req) — writes the agent and everything under it as a blueprint folder; outside references become ports; no keys, no absolute paths, no rows. path: a new or empty folder (relative to the running dir; no `..`). Reply {exported, path, ports, files}."""
    return export.export_subtree(agent, payload)


VERBS = {
    "reflect": _reflect,
    "export_subtree": _export_subtree,
}


def validate_meta(meta: dict, record: dict) -> str | None:
    """The user agent keeps no fields of its own."""
    return None


async def handler(id: str, payload: dict, agent) -> dict | None:
    t = payload.get("type")
    if t in ("boot", "shutdown"):
        return None
    fn = VERBS.get(t)
    if fn is None:
        return refusal(UNHANDLED, f"{NAME}: unknown type {t!r}", verb=t)
    return await fn(id, payload, agent)
