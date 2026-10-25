"""The VIEW AGENTS over a file_bridge: `file` (one file) and `folder` (one folder).

A view is thin. Every disk operation is a verb it sends to its bridge, AS ITSELF,
with its own path, so the bridge stays the one gate to the disk. Two grants make
the scope: the view's `can_call` on the bridge names the verbs it may send (its
GRANTS, nothing else), and the bridge's `callable_by` names the view as a caller of
exactly those verbs AND carries the view's path: a scope token `file:<path>` or
`folder:<prefix>` in that entry. The bridge's path gate (`tools._view_scope`) holds
the caller to that path, and its watch relay delivers to the view only the events
under it. The kernel's gate matches verb names and ignores the token. The view
refuses a verb naming another path as well, before the bridge is asked. The bridge's
`allow` list still decides what is exposed at all, so a view of an unexposed path
reaches nothing.

ONE VIEW PER PATH: `open_view` on the bridge hands back the view that already
exists for a path, or makes it. Every view of one path shares one disk watch.

Who reaches a view: whoever reaches its bridge, for the verbs the view answers
(`view_callers`). A view admits no one the bridge does not.

Watching: a view's `watch` arms the bridge's disk watch for its path and relays the
bridge's changes onto its OWN inbox (`relay_inbox`). The bridge pushes each change
only to the view callers whose scope covers it, so the view is never a kernel
watcher of the bridge (the kernel would mirror every change to it). A file emits
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
#: The kinds a scope token names (`file:<path>`, `folder:<prefix>`).
SCOPE_KINDS = (FILE, FOLDER)

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
    path only; a folder is itself and everything under it (`.` is the whole root). An
    empty `own` is no path at all, so it admits nothing."""
    if not own:
        return False
    if kind == FILE:
        return target == own
    return own == "." or target == own or target.startswith(own + "/")


def scope_token(kind: str, path: str) -> str:
    """The token a bridge's `callable_by` entry for a view carries: its kind and path."""
    return f"{kind}:{path}"


def _scope_tokens(grant) -> list[str]:
    """The scope tokens (`file:<path>`, `folder:<prefix>`) in a `callable_by` entry."""
    return [
        v
        for v in (grant if isinstance(grant, list) else [])
        if isinstance(v, str) and ":" in v and v.partition(":")[0] in SCOPE_KINDS
    ]


def caller_scope(kernel, bridge: str, caller: str | None) -> tuple[str, str] | None:
    """The path a caller is scoped to on this bridge, read from the bridge's own
    `callable_by` entry for it: `(kind, path)` from its scope token. None when the caller
    is not path-scoped (an operator, or a plain agent granted on the bridge).

    Fail closed: a VIEW caller whose entry has no single scope token (or two) is refused
    in full, returned with an empty path, which matches nothing. So is a view of ANOTHER
    bridge: it has no standing here."""
    if not caller:
        return None
    rec = kernel.get(caller) or {}
    grant = ((kernel.get(bridge) or {}).get("callable_by") or {}).get(caller)
    tokens = _scope_tokens(grant)
    if rec.get("view") in ANSWERS and (rec.get("bridge") != bridge or len(tokens) != 1):
        return rec["view"], ""
    if len(tokens) == 1:
        kind, _, path = tokens[0].partition(":")
        return kind, path
    return None


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
    view exactly its GRANTS in its `callable_by`, with the view's scope token. A view
    that already exists, made before the scope token, has an entry with none: opening it
    again stamps the token (the entry becomes the union of what it held, the GRANTS and
    the token). An entry that has any scope token is left as it is, so a deliberate
    narrowing stands and a two-token entry stays refused.

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
            _restamp(bridge_agent, chosen, kind, path)
            return {"id": chosen, "kind": kind, "path": path, "created": False}
        if existing is not None:
            return refusal(
                EXISTS,
                f"open_view: {path!r} already has the {kind} view {existing!r}",
                id=existing,
                hint="open it by that id, or omit `id`",
            )
    if existing is not None:
        _restamp(bridge_agent, existing, kind, path)
        return {"id": existing, "kind": kind, "path": path, "created": False}
    grants = list(GRANTS[kind])
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
    _stamp(bridge_agent, vid, kind, path)
    return {"id": vid, "kind": kind, "path": path, "created": True}


def _stamp(bridge_agent, vid: str, kind: str, path: str) -> None:
    """Grant the view `vid` its GRANTS and its scope token in the bridge's `callable_by`."""
    cb = dict(bridge_agent._meta.get("callable_by") or {})
    entry = list(GRANTS[kind]) + [scope_token(kind, path)]
    held = cb.get(vid)
    cb[vid] = sorted(set(held if isinstance(held, list) else []) | set(entry))
    bridge_agent.update(bridge_agent.id, callable_by=cb)


def _restamp(bridge_agent, vid: str, kind: str, path: str) -> None:
    """Stamp a reused view whose entry has no scope token (made before the token)."""
    held = (bridge_agent._meta.get("callable_by") or {}).get(vid)
    if not _scope_tokens(held):
        _stamp(bridge_agent, vid, kind, path)


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


def relay_inbox(view: str) -> str:
    """The inbox a view's relay reads: the bridge pushes the changes its scope covers
    onto it (`tools._relay_to_views`)."""
    return f"{view}#relay"


def deliver(kernel, view: str, payload: dict) -> bool:
    """Put one change on a view's relay inbox, dropping the oldest when full. False (and
    logged) when the view has no relay inbox, so nothing is relayed to nobody."""
    q = kernel.ctx.inboxes.get(relay_inbox(view))
    if q is None:
        logger.warning("view relay: %r has no relay inbox; change dropped", view)
        return False
    try:
        q.put_nowait(payload)
    except asyncio.QueueFull:
        q.get_nowait()
        q.put_nowait(payload)
    return True


async def watch(view: str, kind: str, rec: dict, kernel) -> dict:
    """Arm the bridge's disk watch for this view's path (once) and relay its changes.
    The relay inbox is registered first, so the bridge's first push has a reader."""
    bridge, own = rec["bridge"], rec["path"]
    if view in RELAYS:
        return {"watching": own, "agent": view, "relayed": True}
    inbox = relay_inbox(view)
    q = kernel.ctx.ensure_inbox(inbox)
    r = await send_bridge(kernel, bridge, {"type": "watch_path", "path": own})
    if "error" in r:
        kernel.ctx.inboxes.pop(inbox, None)
        return r
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
    """The view is going away: its relay ends, and the bridge stops pushing it changes
    (its path is let go)."""
    st = RELAYS.pop(view, None)
    if st is None:
        return
    st["task"].cancel()
    agent.ctx.inboxes.pop(st["inbox"], None)
    await send_bridge(agent, st["bridge"], {"type": "unwatch_path", "path": st["path"]})
