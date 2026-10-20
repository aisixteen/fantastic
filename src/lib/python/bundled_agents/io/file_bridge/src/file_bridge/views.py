"""The VIEW AGENTS over a file_bridge: `file` (one file) and `folder` (one folder).

A view is thin. Every disk operation is a verb it sends to its bridge, AS ITSELF,
with its own path, so the bridge stays the one gate to the disk. Two grants make
the scope: the view's `can_call` on the bridge names the verbs it may send (its
GRANTS, nothing else), and the bridge's `callable_by` names the view as a caller of
exactly those verbs. The path is the view's own: a verb naming another path is
refused here, before the bridge is asked. The bridge's `allow` list still decides
what is exposed at all, so a view of an unexposed path reaches nothing.

ONE VIEW PER PATH: `open_view` on the bridge hands back the view that already
exists for a path, or makes it. Every view of one path shares one disk watch.

Who reaches a view: whoever reaches its bridge, for the verbs the view answers
(`view_callers`). A view admits no one the bridge does not.

Watching: a view's `watch` arms the bridge's disk watch for its path and relays the
bridge's `changed` events onto its OWN inbox, filtered to its path. A file emits
`changed`; a folder emits `added` / `changed` / `removed`. Watch a view to receive them.
"""

from __future__ import annotations

import asyncio
import logging
import posixpath

from file_bridge import fs
from kernel._refusal import EXISTS, TRANSPORT_ERROR, UNAUTHORIZED, bad, refusal

logger = logging.getLogger(__name__)

FILE = "file"
FOLDER = "folder"

#: The handler module each view kind is shipped as (both in this distribution).
#: A view is a `file_bridge.tools` record carrying `view: file|folder` — the bundle's
#: own handler dispatches it, so the views add no installable bundle kind of their own.
BRIDGE_HANDLER = "file_bridge.tools"

#: The verbs a view may send its bridge: its whole grant on it, nothing else.
GRANTS = {
    FILE: ["read", "write", "watch_path", "unwatch_path"],
    FOLDER: ["list", "watch_path", "unwatch_path"],
}
#: A view's relay watches its bridge: the one verb it needs beyond its GRANTS.
RELAY_VERB = "watch"

#: The verbs each view answers (its handler's table): what a caller may be granted on it.
ANSWERS = {
    FILE: ["read", "write", "watch", "reflect"],
    FOLDER: ["list", "reveal", "watch", "reflect"],
}

#: The bridge's disk `kind` → a folder view's event type. Every kind the bridge reports
#: is one of these keys; any other kind is dropped by the relay, never guessed at.
FOLDER_EVENT = {"added": "added", "modified": "changed", "deleted": "removed"}


def norm(path: str) -> str | None:
    """A root-relative path, normalised; None when absolute or climbing out of the
    view. The empty path is the view's own root, `.`."""
    if fs.is_absolute(path):
        return None
    p = posixpath.normpath(path or ".")
    if p == ".." or p.startswith("../"):
        return None
    return p


def bridge_path(kind: str, own: str, rel: str) -> str | None:
    """The bridge path a view's relative `rel` names. A file takes no operand but its
    own path; a folder's operand is inside the folder. None when it is outside."""
    if kind == FILE:
        sub = norm(rel) if rel else own
        return own if sub == own else None
    sub = norm(rel)
    if sub is None:
        return None
    if own == ".":
        return sub
    return own if sub == "." else f"{own}/{sub}"


def scope(
    verb: str, kind: str, own: str, rel: object
) -> tuple[str | None, dict | None]:
    """`(bridge_path, None)` or `(None, refusal)` for the `path` a verb names."""
    if rel is None:
        rel = ""
    if not isinstance(rel, str):
        return None, bad(verb, "path", "a string")
    target = bridge_path(kind, own, rel)
    if target is None:
        return None, refusal(
            UNAUTHORIZED,
            f"{verb}: {rel!r} is outside this agent's path {own!r}",
            hint="a view reaches only its own path; open a view of that path (open_view)",
            path=rel,
        )
    return target, None


async def send_bridge(kernel, bridge: str, payload: dict) -> dict:
    """One send to the bridge, as the calling view. A send that never settled is a
    refusal of its own, never a silent empty answer."""
    (out,) = await kernel.send([{"target": bridge, "payload": payload}])
    res = out.get("result")
    if not isinstance(res, dict):
        return refusal(
            TRANSPORT_ERROR,
            f"{bridge} {payload.get('type')}: {out.get('status')}",
            bridge=bridge,
        )
    return res


def forward_args(payload: dict) -> dict:
    """The verb's own arguments, minus the routing fields a view owns."""
    return {k: v for k, v in payload.items() if k not in ("type", "path")}


def inside(kind: str, own: str, target: str) -> bool:
    """Is the bridge path `target` inside the view's own path `own`? A file is its own
    path only; a folder is itself and everything under it (`.` is the whole root)."""
    if kind == FILE:
        return target == own
    return own == "." or target == own or target.startswith(own + "/")


def caller_scope(kernel, bridge: str, caller: str | None) -> tuple[str, str] | None:
    """The path a VIEW caller is scoped to on this bridge: `(kind, path)`, or None for a
    caller that is not a view (an agent granted on the bridge is not path-scoped). A view
    of ANOTHER bridge has no standing here, so the caller is refused in full (`bridge`
    is returned as `""`, which matches no path)."""
    rec = kernel.get(caller) if caller else None
    if not rec or rec.get("view") not in ANSWERS:
        return None
    if rec.get("bridge") != bridge:
        return rec["view"], ""
    return rec["view"], rec.get("path") or "."


def view_callers(bridge_agent, kind: str) -> dict[str, list[str]]:
    """Who may reach a view: whoever may reach its bridge, for the verbs the view answers.
    A bridge grant of `*` is all of them; a list keeps the verbs the view answers. The
    bridge's `$parent` is named literally (the view's own `$parent` is the bridge), and
    its `$self` is not a caller of its view. A view admits no one the bridge does not."""
    own = ANSWERS[kind]
    out: dict[str, list[str]] = {}
    for caller, grant in (bridge_agent._meta.get("callable_by") or {}).items():
        if caller == "$self":
            continue
        if caller == "$parent":
            if bridge_agent.parent is None:
                continue
            caller = bridge_agent.parent.id
        verbs = own if grant == "*" else [v for v in grant if v in own]
        if verbs:
            out[caller] = sorted(set(out.get(caller, [])) | set(verbs))
    return out


def find_view(kernel, bridge: str, kind: str, path: str) -> str | None:
    """The view of `path` of this kind on this bridge, if one exists."""
    for rec in kernel.list():
        if (
            rec.get("view") == kind
            and rec.get("bridge") == bridge
            and rec.get("path") == path
        ):
            return rec["id"]
    return None


def open_view(
    kernel, bridge_agent, kind: str, path: str, chosen: str | None = None
) -> dict:
    """Reuse the view of `path` on this bridge, or make it. The bridge grants the new
    view exactly its GRANTS (and the relay's `watch`) in its `callable_by`.

    `chosen` is a caller-picked id for the new view (so a caller can declare it up
    front). It is taken only when it is free; a `chosen` that is already another agent,
    or a view of another path, or a path that already has a view under another id, is
    refused rather than replaced or ignored."""
    existing = find_view(kernel, bridge_agent.id, kind, path)
    if chosen is not None:
        held = kernel.get(chosen)
        if held is not None:
            if not _is_view_of(held, bridge_agent.id, kind, path):
                return refusal(
                    EXISTS,
                    f"open_view: {chosen!r} is already an agent, not a {kind} view of {path!r}",
                    id=chosen,
                )
            return {"id": chosen, "kind": kind, "path": path, "created": False}
        if existing is not None:
            return refusal(
                EXISTS,
                f"open_view: {path!r} already has the {kind} view {existing!r}",
                id=existing,
                hint="open it by that id, or omit `id`",
            )
    if existing is not None:
        return {"id": existing, "kind": kind, "path": path, "created": False}
    grants = list(GRANTS[kind]) + [RELAY_VERB]
    made = bridge_agent.create(
        BRIDGE_HANDLER,
        id=chosen,
        view=kind,
        bridge=bridge_agent.id,
        path=path,
        display_name=posixpath.basename(path) or ".",
        can_call={bridge_agent.id: grants},
        callable_by=view_callers(bridge_agent, kind),
    )
    if "error" in made:
        return made
    vid = made["id"]
    cb = dict(bridge_agent._meta.get("callable_by") or {})
    cb[vid] = sorted(set(cb.get(vid, [])) | set(grants))
    bridge_agent.update(bridge_agent.id, callable_by=cb)
    return {"id": vid, "kind": kind, "path": path, "created": True}


def _is_view_of(rec: dict, bridge: str, kind: str, path: str) -> bool:
    return (
        rec.get("view") == kind
        and rec.get("bridge") == bridge
        and rec.get("path") == path
    )


# ─── watching: the bridge's `changed`, relayed onto the view's inbox ─────────

#: view id → the relay's task, inbox key and what it holds on the bridge.
RELAYS: dict[str, dict] = {}


def _relay_rule(kind: str, own: str):
    """The filter and mapping from one bridge `changed` to the view's event, or None."""

    def rule(ev: dict) -> dict | None:
        p = ev.get("path")
        if not isinstance(p, str):
            return None
        raw = ev.get("kind")
        if raw not in FOLDER_EVENT:
            # The bridge reports added / modified / deleted, always. Anything else is a
            # bug upstream: dropped and logged, never relabelled as a change.
            logger.warning("view relay: dropped event with kind %r for %r", raw, p)
            return None
        if kind == FILE:
            if p != own:
                return None
            return {"type": "changed", "path": own, "kind": raw}
        parent = posixpath.dirname(p) or "."
        if p == own or parent != own:
            return None  # the folder itself, or something deeper than its entries
        return {
            "type": FOLDER_EVENT[raw],
            "path": p,
            "name": posixpath.basename(p),
        }

    return rule


async def _relay(view: str, kernel, q: asyncio.Queue, rule) -> None:
    while True:
        ev = await q.get()
        if ev.get("type") != "changed":
            continue
        out = rule(ev)
        if out is not None:
            await kernel.emit(view, out)


async def watch(view: str, kind: str, rec: dict, kernel) -> dict:
    """Arm the bridge's disk watch for this view's path (once) and relay its changes."""
    bridge, own = rec["bridge"], rec["path"]
    if view in RELAYS:
        return {"watching": own, "agent": view, "relayed": True}
    r = await send_bridge(kernel, bridge, {"type": "watch_path", "path": own})
    if "error" in r:
        return r
    inbox = f"{view}#relay"
    q = kernel.ctx.ensure_inbox(inbox)
    gate = kernel.watch(bridge, inbox)
    if gate is not None:
        await send_bridge(kernel, bridge, {"type": "unwatch_path", "path": own})
        return gate
    tid = kernel.ctx.spawn_task(
        view, "watch", _relay(view, kernel, q, _relay_rule(kind, own))
    )
    RELAYS[view] = {
        "task": kernel.ctx.tasks[tid]["_task"],
        "inbox": inbox,
        "bridge": bridge,
        "path": own,
    }
    return {"watching": own, "agent": view, "relayed": False}


async def stop(view: str, agent) -> None:
    """The view is going away: its relay ends, and the bridge's watch is let go."""
    st = RELAYS.pop(view, None)
    if st is None:
        return
    st["task"].cancel()
    agent.unwatch(st["bridge"], st["inbox"])
    await send_bridge(agent, st["bridge"], {"type": "unwatch_path", "path": st["path"]})
