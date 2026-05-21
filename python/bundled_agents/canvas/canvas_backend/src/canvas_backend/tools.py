"""canvas_backend — spatial UI host as an agent.

The canvas is a web page that renders OTHER agents as positioned iframes.
Membership is **structural**: agents added to the canvas become its
children (`agent.create(...)`). The substrate's parent-child cascade
gives us correct lifecycle for free — delete the canvas and every
member dies; PTYs/uvicorn/etc. owned by member subtrees are torn
down via `_shutdown` before records vanish.

Layout is stored on each member's record: x, y, width, height. Drag/
resize in the browser sends `update_agent` against the canvas; the
substrate emits `agent_updated` events for the watchers.

Verbs:
  reflect       -> {sentence, member_count, viewport_default, ...}
  add_agent     args: handler_module:str (req) | agent_id:str
                  Spawn a new member as a child of this canvas (via
                  agent.create) OR re-parent an existing agent under
                  this canvas. Refused if the resulting member
                  doesn't answer get_webapp NOR get_gl_view.
                  Returns {ok, members[], member_id, already?}.
  remove_agent  args: agent_id:str (req)  -> {removed:bool, members[]}
                  Cascade-deletes the member (and its subtree).
  list_members  -> {members:[id,...]}      (this canvas's children)
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


def _member_ids(agent) -> list[str]:
    """Direct children of this canvas — the new structural members."""
    return list(agent._children.keys())


# ─── verbs ──────────────────────────────────────────────────────


async def _reflect(id, payload, agent):
    """Identity + viewport defaults + member count. No args."""
    rec = agent.get(id) or {}
    members = _member_ids(agent)
    return {
        "id": id,
        "sentence": "Spatial canvas with structural membership. Members are this agent's children — cascade-deleted with the canvas.",
        "viewport_default": {
            "width": int(rec.get("width", 1600)),
            "height": int(rec.get("height", 900)),
        },
        "member_count": len(members),
        "agent_count": len(agent.list()),
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
        "emits": {
            "members_updated": "{type:'members_updated', members:[id,...]} — broadcast on this canvas's inbox after every add_agent / remove_agent",
        },
    }


async def _boot(id, payload, agent):
    """No-op. Returns None."""
    return None


async def _list_members(id, payload, agent):
    """No args. Returns {members:[agent_id,...]} — this canvas's direct children."""
    return {"members": _member_ids(agent)}


async def _add_agent(id, payload, agent):
    """args: handler_module:str (req) [+ x,y,width,height,display_name,...].
    Spawns a new member as this canvas's child via agent.create. The
    new agent's `_boot` fires (which may itself create grandchildren —
    e.g. terminal_webapp → terminal_backend). Returns
    {ok, member_id, members}.

    Refused if the new agent doesn't answer get_webapp NOR get_gl_view
    (a canvas needs SOMETHING to render). On refusal, the spawned
    agent is rolled back via cascade-delete.
    """
    handler_module = payload.get("handler_module")
    if not handler_module or not isinstance(handler_module, str):
        return {"error": "add_agent: handler_module (str) required"}
    meta = {
        k: v
        for k, v in payload.items()
        if k not in ("type", "handler_module", "agent_id")
    }
    rec = agent.create(handler_module, **meta)
    if "error" in rec:
        return rec
    member_id = rec["id"]
    # Boot the new child so it can spawn its own subtree (idempotent
    # bundle-level patterns wire this up — terminal_webapp creates its
    # backend, etc.).
    await agent.send(member_id, {"type": "boot"})
    # Probe the renderable contract.
    wa = await agent.send(member_id, {"type": "get_webapp"})
    has_dom = isinstance(wa, dict) and wa.get("url") and not wa.get("error")
    gl = await agent.send(member_id, {"type": "get_gl_view"})
    has_gl = isinstance(gl, dict) and gl.get("source") and not gl.get("error")
    if not (has_dom or has_gl):
        # Roll back via cascade delete — kills the spawned agent's
        # subtree along with it.
        await agent.delete(member_id)
        return {
            "error": f"add_agent: {member_id!r} answers neither get_webapp nor get_gl_view; nothing to render"
        }
    members = _member_ids(agent)
    await agent.emit(id, {"type": "members_updated", "members": members})
    return {"ok": True, "member_id": member_id, "members": members}


async def _remove_agent(id, payload, agent):
    """args: agent_id:str (req). Cascade-deletes the member (and its
    subtree). Idempotent — non-member or unknown id returns
    {removed:false}. Emits members_updated when an actual removal
    happens. Returns {removed:bool, members}."""
    target = payload.get("agent_id")
    if not target or not isinstance(target, str):
        return {"error": "remove_agent: agent_id (str) required"}
    if target not in agent._children:
        return {"removed": False, "members": _member_ids(agent)}
    result = await agent.delete(target)
    if not result.get("deleted"):
        # Most likely delete_lock somewhere in the subtree.
        return {"removed": False, **result}
    members = _member_ids(agent)
    await agent.emit(id, {"type": "members_updated", "members": members})
    return {"removed": True, "members": members}


async def _discover(id, payload, agent):
    """args: x:float, y:float, w:float (>0), h:float (>0). Returns
    {agents:[{id,x,y,width,height},...]} for THIS canvas's members
    whose rect intersects the query rect. Only direct children — for
    cross-canvas spatial queries, walk the tree explicitly."""
    x = float(payload.get("x", 0))
    y = float(payload.get("y", 0))
    w = float(payload.get("w", 0))
    h = float(payload.get("h", 0))
    if w <= 0 or h <= 0:
        return {"error": "discover: w and h required and > 0"}
    target_rect = (x, y, w, h)
    hits = []
    for child in agent._children.values():
        rec = child.record
        if _intersects(_rect(rec), target_rect):
            hits.append(
                {
                    "id": child.id,
                    "x": rec.get("x", 0),
                    "y": rec.get("y", 0),
                    "width": rec.get("width", 320),
                    "height": rec.get("height", 220),
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


async def handler(id: str, payload: dict, agent) -> dict | None:
    t = payload.get("type")
    fn = VERBS.get(t)
    if fn is None:
        return {"error": f"canvas: unknown type {t!r}"}
    return await fn(id, payload, agent)
