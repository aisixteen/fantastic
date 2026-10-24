"""folder agent — the VIEW of one folder on a file_bridge (`file_bridge/views.py`).

A folder agent is the folder's one live object: `list` and `reveal` go to its bridge
AS THIS AGENT, for paths inside its folder; `watch` relays the folder's entry changes
as `added` / `changed` / `removed` events. Its `can_call` names the bridge's verbs it
needs and nothing else. Every path it is given is relative to the folder, and a path
that climbs out of it is refused here, before the bridge is asked.
"""

from __future__ import annotations

import posixpath

from file_bridge import views
from kernel._refusal import NOT_FOUND, UNHANDLED, missing, refusal

KIND = views.FOLDER


async def _list(id, payload, kernel):
    """args: path? (a folder inside this one, relative to it; empty = this folder) — the folder's entries {files: [{name, path, type, size?}]}."""
    rec = kernel.get(id) or {}
    target, err = views.scope("list", KIND, rec.get("path", "."), payload.get("path"))
    if err:
        return err
    return await views.send_bridge(
        kernel, rec["bridge"], {"type": "list", "path": target}
    )


async def _reveal(id, payload, kernel):
    """args: path (req — a file or folder inside this one, relative to it) — shows where it sits: {revealed, folder_path, dirs} where dirs are the folders to open on the way down; not_found when nothing is there."""
    rec = kernel.get(id) or {}
    own = rec.get("path", ".")
    rel = payload.get("path")
    if not isinstance(rel, str) or not rel:
        return missing("reveal", "path")
    target, err = views.scope("reveal", KIND, own, rel)
    if err:
        return err
    parent = posixpath.dirname(target) or "."
    listed = await views.send_bridge(
        kernel, rec["bridge"], {"type": "list", "path": parent}
    )
    if "error" in listed:
        return listed
    name = posixpath.basename(target)
    if not any(f.get("name") == name for f in listed.get("files", [])):
        return refusal(NOT_FOUND, f"reveal: {rel!r} is not in this folder", path=rel)
    # The folders to open on the way down, each relative to this folder.
    inside = posixpath.relpath(target, own) if own != "." else target
    parts = posixpath.dirname(inside).split("/") if posixpath.dirname(inside) else []
    dirs = ["/".join(parts[: i + 1]) for i in range(len(parts))]
    return {"revealed": inside, "folder_path": own, "dirs": dirs}


async def _watch(id, payload, kernel):
    """args: none — start relaying this folder's entry changes onto this agent: `{type: added|changed|removed, path, name}` (path relative to the folder's bridge); WATCH this agent to receive them. Idempotent."""
    rec = kernel.get(id) or {}
    return await views.watch(id, KIND, rec, kernel)


async def _reflect(id, payload, kernel):
    """Identity, the folder it views, its bridge, and whether its change relay is on. No args."""
    rec = kernel.get(id) or {}
    return {
        "id": id,
        "sentence": "A folder on a file_bridge — one live object per folder: its entries and their changes.",
        "kind": KIND,
        "path": rec.get("path"),
        "bridge": rec.get("bridge"),
        "watching": id in views.RELAYS,
    }


VERBS = {
    "reflect": _reflect,
    "list": _list,
    "reveal": _reveal,
    "watch": _watch,
}

EMITS = {
    "added": "{type: added, path, name} — an entry appeared in this folder",
    "changed": "{type: changed, path, name} — an entry in this folder was modified",
    "removed": "{type: removed, path, name} — an entry in this folder was deleted",
}

NOTES = (
    "A folder agent reaches its bridge AS ITSELF, for paths inside its folder only. Open one "
    "with `open_view` on the bridge — one agent per folder, reused."
)


async def handler(id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    fn = VERBS.get(t)
    if fn is None:
        return refusal(UNHANDLED, f"folder agent: unknown type {t!r}", verb=t)
    return await fn(id, payload, kernel)


async def on_delete(agent) -> None:
    """The folder agent is going away: its relay ends and its bridge watch is let go."""
    await views.stop(agent.id, agent)
