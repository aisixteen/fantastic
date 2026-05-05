"""canvas bundle — spatial UI host as an agent.

The canvas is a web page that renders OTHER agents as positioned iframes.
Membership is **explicit**: each canvas keeps a `members: [agent_id, ...]`
list on its own record; the webapp iframes only those agents. There is
NO auto-include — two canvases can host disjoint sets cleanly.

Layout is stored on each agent's record: x, y, width, height. Drag/resize
in the browser sends update_agent through core; everyone watching `core`
(including this canvas) gets `agent_updated` events.

Verbs:
  reflect       -> {sentence, member_count, viewport_default, ...}
  add_agent     args: agent_id:str (req)  -> {ok, members[], already?}
                  Refused if target doesn't answer get_webapp.
  remove_agent  args: agent_id:str (req)  -> {removed:bool, members[]}
  list_members  -> {members:[id,...]}
  discover      args: x, y, w, h          -> {agents:[...]}  spatial intersection
  boot          -> no-op (canvas is browser-driven)

Emits:
  members_updated  {type, members:[id,...]}  on add/remove
"""

from __future__ import annotations


def _rect(rec: dict) -> tuple[float, float, float, float]:
    return (
        float(rec.get("x", 0)),
        float(rec.get("y", 0)),
        float(rec.get("width", 320)),
        float(rec.get("height", 220)),
    )


def _intersects(
    a: tuple[float, float, float, float], b: tuple[float, float, float, float]
) -> bool:
    ax, ay, aw, ah = a
    bx, by, bw, bh = b
    return not (ax + aw < bx or bx + bw < ax or ay + ah < by or by + bh < ay)


def _members_of(rec: dict | None) -> list[str]:
    """Defensive read — record may be missing or have non-list members."""
    if not rec:
        return []
    m = rec.get("members")
    return list(m) if isinstance(m, list) else []


# ─── verbs ──────────────────────────────────────────────────────


async def _reflect(id, payload, kernel):
    """Identity + viewport defaults + member count. No args."""
    rec = kernel.get(id) or {}
    members = _members_of(rec)
    return {
        "id": id,
        "sentence": "Spatial canvas with explicit membership. Renders agents listed in `members` as positioned iframes; drag/resize updates their record.",
        "viewport_default": {
            "width": int(rec.get("width", 1600)),
            "height": int(rec.get("height", 900)),
        },
        "member_count": len(members),
        "agent_count": len(kernel.list()),
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
        "emits": {
            "members_updated": "{type:'members_updated', members:[id,...]} — broadcast on this canvas's inbox after every add_agent / remove_agent",
        },
    }


async def _boot(id, payload, kernel):
    """No-op. Returns None."""
    return None


async def _list_members(id, payload, kernel):
    """No args. Returns {members:[agent_id,...]} — explicit list of agents this canvas hosts."""
    rec = kernel.get(id) or {}
    return {"members": _members_of(rec)}


async def _add_agent(id, payload, kernel):
    """args: agent_id:str (req). Append to this canvas's members. Refused if target answers neither get_webapp NOR get_gl_view (a canvas needs SOMETHING to render — a DOM iframe, a GL view, or both). Idempotent — re-adding returns {ok, already:true} without re-emit. Emits members_updated on first add."""
    target = payload.get("agent_id")
    if not target or not isinstance(target, str):
        return {"error": "add_agent: agent_id (str) required"}
    if not kernel.get(target):
        return {"error": f"add_agent: no agent {target!r}"}
    # Probe both presentation verbs. The canvas hosts two layers (DOM
    # iframe + GL view) and an agent answering EITHER is addable; one
    # answering both gets BOTH presentations.
    wa = await kernel.send(target, {"type": "get_webapp"})
    has_dom = isinstance(wa, dict) and wa.get("url") and not wa.get("error")
    gl = await kernel.send(target, {"type": "get_gl_view"})
    has_gl = isinstance(gl, dict) and gl.get("source") and not gl.get("error")
    if not (has_dom or has_gl):
        return {
            "error": f"add_agent: {target!r} answers neither get_webapp nor get_gl_view; nothing to render"
        }
    rec = kernel.get(id) or {}
    members = _members_of(rec)
    if target in members:
        return {"ok": True, "members": members, "already": True}
    members.append(target)
    kernel.update(id, members=members)
    await kernel.emit(id, {"type": "members_updated", "members": members})
    return {"ok": True, "members": members}


async def _remove_agent(id, payload, kernel):
    """args: agent_id:str (req). Remove from members. Idempotent — non-member returns {removed:false} without emit. Emits members_updated when an actual removal happens. Returns {removed:bool, members}."""
    target = payload.get("agent_id")
    if not target or not isinstance(target, str):
        return {"error": "remove_agent: agent_id (str) required"}
    rec = kernel.get(id) or {}
    members = _members_of(rec)
    if target not in members:
        return {"removed": False, "members": members}
    members.remove(target)
    kernel.update(id, members=members)
    await kernel.emit(id, {"type": "members_updated", "members": members})
    return {"removed": True, "members": members}


async def _discover(id, payload, kernel):
    """args: x:float, y:float, w:float (>0), h:float (>0). Returns {agents:[{id,x,y,width,height},...]} for agents whose rect intersects."""
    x = float(payload.get("x", 0))
    y = float(payload.get("y", 0))
    w = float(payload.get("w", 0))
    h = float(payload.get("h", 0))
    if w <= 0 or h <= 0:
        return {"error": "discover: w and h required and > 0"}
    target_rect = (x, y, w, h)
    hits = []
    for a in kernel.list():
        if a["id"] == id:
            continue
        if _intersects(_rect(a), target_rect):
            hits.append(
                {
                    "id": a["id"],
                    "x": a.get("x", 0),
                    "y": a.get("y", 0),
                    "width": a.get("width", 320),
                    "height": a.get("height", 220),
                }
            )
    return {"agents": hits}


# ─── dispatch ───────────────────────────────────────────────────


VERBS = {
    "reflect": _reflect,
    "boot": _boot,
    "list_members": _list_members,
    "add_agent": _add_agent,
    "remove_agent": _remove_agent,
    "discover": _discover,
}


async def handler(id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    fn = VERBS.get(t)
    if fn is None:
        return {"error": f"canvas: unknown type {t!r}"}
    return await fn(id, payload, kernel)
