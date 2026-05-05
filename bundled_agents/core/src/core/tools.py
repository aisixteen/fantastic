"""core singleton — system verbs.

Handles `boot`, `list_agents`, `create_agent`, `delete_agent`, `update_agent`,
`reflect`. Verb fns registered in `VERBS`; the handler is a dispatcher.
"""

from __future__ import annotations


# ─── verbs ──────────────────────────────────────────────────────


async def _reflect(id, payload, kernel):
    """Identity + flat state. No args. Returns {id, verbs, emits, agent_count}."""
    return {
        "id": id,
        "sentence": "Core agent. System verbs over the kernel.",
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
        "emits": {
            "agent_created": "{type:'agent_created', id, agent} — broadcast on every create_agent",
            "agent_deleted": "{type:'agent_deleted', id} — broadcast on every delete_agent",
            "agent_updated": "{type:'agent_updated', id, changed:[keys]} — broadcast on every update_agent",
        },
        "agent_count": len(kernel.list()),
    }


async def _boot(id, payload, kernel):
    """Idempotent. Logs agent count to stdout. Returns None."""
    print(f"  [core] booted. {len(kernel.list())} agent(s) loaded.")
    return None


async def _list_agents(id, payload, kernel):
    """No args. Returns {agents: [<full record>, ...]} — every running agent."""
    return {"agents": kernel.list()}


async def _create_agent(id, payload, kernel):
    """args: handler_module:str (req), id:str?, **meta. Returns the new record or {error}. Auto-emits agent_created and sends boot."""
    hm = payload.get("handler_module")
    if not hm:
        return {"error": "create_agent: handler_module required"}
    meta = {
        k: v for k, v in payload.items() if k not in ("type", "handler_module", "id")
    }
    rec = kernel.create(hm, id=payload.get("id"), **meta)
    if "id" in rec:
        await kernel.emit(
            "core", {"type": "agent_created", "id": rec["id"], "agent": rec}
        )
        await kernel.send(rec["id"], {"type": "boot"})
    return rec


async def _delete_agent(id, payload, kernel):
    """args: id:str (req). Returns {deleted:bool, id}. Refuses singletons AND agents with delete_lock=true (clear it via update_agent first). Auto-sends `shutdown` to the agent for process-memory teardown (PTY, uvicorn, etc.) symmetric to create_agent's `boot`; ignores unknown-verb errors so bundles can opt in. Auto-emits agent_deleted."""
    target = payload.get("id")
    if not target:
        return {"error": "delete_agent: id required"}
    rec = kernel.get(target)
    if rec and rec.get("delete_lock"):
        # Machine-parseable for LLM callers: explicit `locked` flag plus
        # human-readable error message. The deleter agent receives this
        # in its tool-call reply and can react (e.g. surface to user
        # "cannot delete X, delete_lock is set").
        return {
            "error": f"delete_agent: {target!r} has delete_lock=true; clear it via update_agent before deleting",
            "locked": True,
            "id": target,
        }
    if rec:
        # Symmetric to create_agent's `boot`: give the agent one chance
        # to tear down process-memory state (PTY children, uvicorn
        # servers, in-flight tasks) before its record disappears.
        # Best-effort: bundles that don't implement `shutdown` return
        # an unknown-verb error which we ignore. Real exceptions would
        # log via the kernel's normal handler-error path.
        try:
            await kernel.send(target, {"type": "shutdown"})
        except Exception:
            pass
    ok = kernel.delete(target)
    if ok:
        await kernel.emit("core", {"type": "agent_deleted", "id": target})
    return {"deleted": ok, "id": target}


async def _update_agent(id, payload, kernel):
    """args: id:str (req), **meta (any fields to merge). Returns {updated, id, agent} or {error}. Auto-emits agent_updated with changed key list."""
    target = payload.get("id")
    if not target:
        return {"error": "update_agent: id required"}
    meta = {k: v for k, v in payload.items() if k not in ("type", "id")}
    rec = kernel.update(target, **meta)
    if rec is None:
        return {"error": f"no agent {target!r}"}
    await kernel.emit(
        "core",
        {"type": "agent_updated", "id": target, "changed": list(meta.keys())},
    )
    return {"updated": True, "id": target, "agent": rec}


# ─── dispatch ───────────────────────────────────────────────────


VERBS = {
    "reflect": _reflect,
    "boot": _boot,
    "list_agents": _list_agents,
    "create_agent": _create_agent,
    "delete_agent": _delete_agent,
    "update_agent": _update_agent,
}


async def handler(id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    fn = VERBS.get(t)
    if fn is None:
        return {"error": f"core: unknown type {t!r}"}
    return await fn(id, payload, kernel)
