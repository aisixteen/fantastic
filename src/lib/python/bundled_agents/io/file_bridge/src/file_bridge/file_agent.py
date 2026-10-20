"""file agent — the VIEW of one file on a file_bridge (`file_bridge/views.py`).

A file agent is the file's one live object: `read` and `write` go to its bridge AS
THIS AGENT, for its own path; `watch` relays the bridge's changes to the file as
`changed` events. Its `can_call` names the bridge's verbs it needs and nothing else;
a verb naming another path is refused here, before the bridge is asked.
"""

from __future__ import annotations

from file_bridge import views
from kernel._refusal import UNHANDLED, refusal

KIND = views.FILE


async def _forward(verb: str, id: str, payload: dict, kernel, bridge_type: str) -> dict:
    """Send one verb to the bridge for this file's path (the one path a file reaches)."""
    rec = kernel.get(id) or {}
    target, err = views.scope(verb, KIND, rec.get("path", ""), payload.get("path"))
    if err:
        return err
    return await views.send_bridge(
        kernel,
        rec["bridge"],
        {**views.forward_args(payload), "type": bridge_type, "path": target},
    )


async def _read(id, payload, kernel):
    """args: path? (this file only), offset:int=0 (line), limit:int=200 — the file's text page {path, lines:[{n,text}], total, eof, version}; refuses binary."""
    return await _forward("read", id, payload, kernel, "read")


async def _write(id, payload, kernel):
    """args: bytes (req — the whole document), if_match:str? (the version you read; a changed document is refused) — atomic whole-document write; {path, size, version}."""
    return await _forward("write", id, payload, kernel, "write")


async def _watch(id, payload, kernel):
    """args: none — start relaying this file's changes onto this agent: `{type: changed, path, kind: added|modified|deleted}`; WATCH this agent to receive them. Idempotent."""
    rec = kernel.get(id) or {}
    return await views.watch(id, KIND, rec, kernel)


async def _reflect(id, payload, kernel):
    """Identity, the file it views, its bridge, and whether its change relay is on. No args."""
    rec = kernel.get(id) or {}
    return {
        "id": id,
        "sentence": "A file on a file_bridge — one live object per path: its text, its writes and its changes.",
        "kind": KIND,
        "path": rec.get("path"),
        "bridge": rec.get("bridge"),
        "watching": id in views.RELAYS,
    }


VERBS = {
    "reflect": _reflect,
    "read": _read,
    "write": _write,
    "watch": _watch,
}

EMITS = {
    "changed": "{type: changed, path, kind: added|modified|deleted} — this file changed on disk, by any writer; watch this agent to receive it",
}

NOTES = (
    "A file agent reaches its bridge AS ITSELF: its can_call names the bridge with the "
    "verbs in views.GRANTS, and the bridge's callable_by names it back. Open one with "
    "`open_view` on the bridge — one agent per path, reused."
)


async def handler(id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    fn = VERBS.get(t)
    if fn is None:
        return refusal(UNHANDLED, f"file agent: unknown type {t!r}", verb=t)
    return await fn(id, payload, kernel)


async def on_delete(agent) -> None:
    """The file agent is going away: its relay ends and its bridge watch is let go."""
    await views.stop(agent.id, agent)
