"""agentic_resources — a generic registry of agent ROLES (off by default).

A role card is one file (`cards`); the registry lists them (`roles`, `role`),
creates an agent from one (`spawn`), reports who it spawned (`fleet`) and caps how
many run (`set_limit`, `get_count`). Not code-oriented: a role is any kind of agent.

OFF BY DEFAULT: the kernel resolves this bundle only when
`FANTASTIC_AGENTIC_RESOURCES` is truthy (`kernel._bundles.bundle_enabled`), and an
instance whose record has no `role_dirs` refuses every verb `disabled`.

Config is the registry agent's own record: `role_dirs` (folders of cards), `tiers`
(`{tier: backend fields}` — the card names a tier KEY, never a model), `limits`
(`{total?, per_role?: {role: n}}`, kept by `set_limit`).

NOTHING is counted in memory. Every spawned agent carries `spawned_by` (this
registry) and `role`, so `fleet` and `get_count` are read off the records: a delete
lowers the count and a restart rebuilds it. `running` counts the agents whose
task-line `state` is running or waiting (no task line = running).

NO CLEANUP: `report done` only sets a state. Nothing is retired or deleted;
`delete_agent` is the only way out.

Verbs (mirror rust verb-for-verb): reflect, roles, role, spawn, report, fleet,
set_limit, get_count.
"""

from __future__ import annotations

import asyncio

from agentic_resources import cards as cards_mod
from agentic_resources.cards import REGISTRY_TOKEN

from kernel import current_origin
from kernel._refusal import NOT_FOUND, UNHANDLED, bad, missing, refusal

#: Detached first-message sends, held so they are not collected mid-flight.
_DETACHED: set = set()

STATES = ("waiting", "running", "blocked", "done", "failed", "paused")
#: What a parent holds over every agent it spawns.
PARENT_VERBS = ["pause", "resume", "stop"]

VERB_DOCS = {
    "reflect": "Identity, whether the registry is enabled, and the role names. No args.",
    "roles": "No args. Returns {roles:[{name, purpose}], errors:[...]} — every role card the config's folders hold, plus the shipped project_lead. Cards are re-read on each call.",
    "role": "args: name:str (req). Returns the full card {name, purpose, tier, grants, directive}; an unknown name is `not_found`.",
    "spawn": "args: role:str (req), task:str|{label, waits_on?}? (a string is sent to the child as its first message; an object is stored on its record as the task line {label, state, waits_on, result}), name:str?, limits:{total?, per_role?}? (part of YOUR quota handed to the child as its own subtree cap). Creates the agent from the card (directive as its system prompt, the tier's backend fields from config, the card's grants) as a CHILD OF THE CALLER, and gives the caller pause/resume/stop on it. Refused past a limit: {error:'limit', scope, role, running, max, reason}. Returns {id, role, tier}.",
    "report": "args: state:waiting|running|blocked|done|failed|paused (req), result:any?. A spawned agent sets its OWN task line; its parent gets a message {type:'report', from, state, result}. Refused `not_spawned` for any other caller. Nothing is cleaned up on done. Returns {ok:true}.",
    "fleet": "No args. Returns {roles:{<role>:[{id, state}]}} — the agents this registry spawned, per role, read from the records (survives restart).",
    "set_limit": "args: role:str? (omitted = all agents in total), max:int|null (req; null clears). Returns {ok:true, role, max}.",
    "get_count": "args: role:str? (omitted = in total). Returns {running, max} — running counts ACTIVE agents only (running, waiting); max is null when uncapped.",
}


# ─── record helpers ──────────────────────────────────────────────


def _role_dirs(meta: dict):
    """The registry's `role_dirs`, or None when the registry is not configured."""
    d = meta.get("role_dirs")
    if not isinstance(d, list):
        return None
    return [x for x in d if isinstance(x, str)]


def _disabled() -> dict:
    return {"error": "agentic_resources is disabled"}


def _fail(code: str, reason: str) -> dict:
    """A refusal of the registry's own: {error:<code>, reason:<text>}."""
    return {"error": code, "reason": reason}


def _task_state(meta: dict) -> str:
    t = meta.get("task")
    s = t.get("state") if isinstance(t, dict) else None
    return s if isinstance(s, str) else "running"


def _active(state: str) -> bool:
    return state in ("running", "waiting")


def _spawned(kernel, reg: str) -> list[dict]:
    out = [
        {
            "id": a.id,
            "role": a._meta.get("role") if isinstance(a._meta.get("role"), str) else "",
            "state": _task_state(a._meta),
        }
        for a in kernel.ctx.agents.values()
        if a._meta.get("spawned_by") == reg
    ]
    return sorted(out, key=lambda s: s["id"])


def _ancestors(kernel, aid: str) -> list[str]:
    """Every id above `aid`, nearest first."""
    out = []
    a = kernel.ctx.agents.get(aid)
    p = a.parent if a is not None else None
    while p is not None:
        out.append(p.id)
        p = p.parent
    return out


def _cap(limits, role):
    if not isinstance(limits, dict):
        return None
    if role is not None:
        per = limits.get("per_role")
        v = per.get(role) if isinstance(per, dict) else None
    else:
        v = limits.get("total")
    return v if isinstance(v, int) and not isinstance(v, bool) and v >= 0 else None


def _limit_refusal(scope, role, running, mx, who=None) -> dict:
    if scope == "role":
        what = f"role {role!r} is at its limit"
    elif scope == "total":
        what = "all roles together are at the limit"
    elif scope == "request":
        what = f"the limits asked for exceed {who or 'the caller'}'s remaining quota"
    else:
        what = f"{who or 'the caller'}'s delegated quota is used up"
    r = {
        "error": "limit",
        "scope": scope,
        "role": role,
        "running": running,
        "max": mx,
        "reason": f"{what} ({running} running, max {mx})",
    }
    if who is not None:
        r["lead"] = who
    return r


def _grant_verbs(m: dict, key: str, verbs) -> None:
    """Add `verbs` to a grant map's `key` (a "*" stays "*")."""
    cur = m.get(key)
    if cur == "*" or verbs == "*":
        m[key] = "*"
    elif isinstance(cur, list) and isinstance(verbs, list):
        m[key] = cur + [v for v in verbs if v not in cur]
    else:
        m[key] = verbs


def _cards_of(meta: dict):
    return cards_mod.load_dir(_role_dirs(meta) or [])


def _count_arg(v, verb: str, arg: str):
    if isinstance(v, bool) or not isinstance(v, int) or v < 0:
        return None, bad(verb, arg, "a non-negative int")
    return v, None


# ─── verbs ───────────────────────────────────────────────────────


async def _reflect(id, payload, kernel):
    meta = kernel._meta
    on = _role_dirs(meta) is not None
    names = [c["name"] for c in _cards_of(meta)[0]] if on else []
    return {
        "id": id,
        "sentence": "A registry of agent roles: list them, spawn from a card, cap how many run.",
        "enabled": on,
        "roles": names,
    }


async def _roles(id, payload, kernel):
    cards, errors = _cards_of(kernel._meta)
    return {
        "roles": [{"name": c["name"], "purpose": c["purpose"]} for c in cards],
        "errors": errors,
    }


async def _role(id, payload, kernel):
    name = payload.get("name")
    if not isinstance(name, str) or not name:
        return missing("role", "name")
    for c in _cards_of(kernel._meta)[0]:
        if c["name"] == name:
            return dict(c)
    return refusal(NOT_FOUND, f"role {name!r} not found")


async def _spawn(reg, payload, kernel):
    meta = kernel._meta
    ctx = kernel.ctx
    role = payload.get("role")
    if not isinstance(role, str) or not role:
        return missing("spawn", "role")
    card = next((c for c in _cards_of(meta)[0] if c["name"] == role), None)
    if card is None:
        return refusal(NOT_FOUND, f"role {role!r} not found")
    tiers = meta.get("tiers")
    tier_fields = tiers.get(card["tier"]) if isinstance(tiers, dict) else None
    if not isinstance(tier_fields, dict):
        r = _fail(
            "unknown_tier",
            f"tier {card['tier']!r} (named by role {role!r}) is not in this registry's tiers",
        )
        r["tier"] = card["tier"]
        return r
    if not isinstance(tier_fields.get("handler_module"), str):
        return _fail("bad_tier", f"tier {card['tier']!r} has no handler_module")
    # the task: a string is the first message, an object is the task line
    first_message = None
    line = None
    task = payload.get("task")
    if task is None:
        pass
    elif isinstance(task, str):
        first_message = task
    elif isinstance(task, dict):
        label = task.get("label")
        if not isinstance(label, str) or not label:
            return bad("spawn", "task.label", "a non-empty string")
        waits = task.get("waits_on")
        if waits is None:
            waits = []
        elif not (isinstance(waits, list) and all(isinstance(w, str) for w in waits)):
            return bad("spawn", "task.waits_on", "a list of agent ids")
        line = {
            "label": label,
            "state": "waiting" if waits else "running",
            "waits_on": list(waits),
            "result": None,
        }
    else:
        return bad("spawn", "task", "a string or {label, waits_on?}")
    delegated: dict = {}
    lim = payload.get("limits")
    if lim is not None:
        if not isinstance(lim, dict):
            return bad("spawn", "limits", "{total?, per_role?}")
        for k in ("total", "per_role"):
            if lim.get(k) is not None:
                n, err = _count_arg(lim[k], "spawn", f"limits.{k}")
                if err:
                    return err
                delegated[k] = n
    name = payload.get("name")
    if name is not None and (not isinstance(name, str) or not name):
        return bad("spawn", "name", "a non-empty string")

    caller = current_origin()
    parent = caller or reg
    allsp = _spawned(kernel, reg)
    limits = meta.get("limits")

    # the registry's caps: per role, then in total
    in_role = sum(1 for s in allsp if s["role"] == role and _active(s["state"]))
    mx = _cap(limits, role)
    if mx is not None and in_role >= mx:
        return _limit_refusal("role", role, in_role, mx)
    total = sum(1 for s in allsp if _active(s["state"]))
    mx = _cap(limits, None)
    if mx is not None and total >= mx:
        return _limit_refusal("total", role, total, mx)
    # delegated quotas: the caller and every lead above it
    for lead in [parent, *_ancestors(kernel, parent)]:
        la = ctx.agents.get(lead)
        if la is None or la._meta.get("spawned_by") != reg:
            continue
        q = la._meta.get("limits")
        if not isinstance(q, dict):
            continue
        below = [
            s
            for s in allsp
            if _active(s["state"]) and lead in _ancestors(kernel, s["id"])
        ]
        mx = _cap(q, None)
        if mx is not None and len(below) >= mx:
            return _limit_refusal("subtree_total", role, len(below), mx, lead)
        pr = q.get("per_role")
        if isinstance(pr, int) and not isinstance(pr, bool):
            n = sum(1 for s in below if s["role"] == role)
            if n >= pr:
                return _limit_refusal("subtree_role", role, n, pr, lead)
    # what the caller may hand on: its remaining quota, less the sub-lead's own slot
    if delegated and caller is not None:
        pa = ctx.agents.get(parent)
        q = pa._meta.get("limits") if pa is not None and pa._meta.get("spawned_by") == reg else None
        if isinstance(q, dict):
            used = sum(
                1
                for s in allsp
                if _active(s["state"]) and parent in _ancestors(kernel, s["id"])
            )
            want, mx = delegated.get("total"), _cap(q, None)
            if want is not None and mx is not None:
                left = max(mx - used, 0)
                left = max(left - 1, 0)
                if want > left:
                    return _limit_refusal("request", role, want, left, parent)
            want, pr = delegated.get("per_role"), q.get("per_role")
            if want is not None and isinstance(pr, int) and want > pr:
                return _limit_refusal("request", role, want, pr, parent)

    # the child's grants: the card's, with `$registry` made real, plus `report`
    can_call: dict = {}
    for k, v in card["grants"].items():
        _grant_verbs(can_call, reg if k == REGISTRY_TOKEN else k, v)
    _grant_verbs(can_call, reg, ["report"])
    # a tier that names the store the agent keeps its conversation in: the agent may
    # use it (the other end is opened below, before the first message)
    store = tier_fields.get("memory_store_id")
    store = store if isinstance(store, str) and store else None
    if store is not None:
        _grant_verbs(can_call, store, "*")
    reg_verbs = can_call.get(reg, ["report"])
    callable_by: dict = {}
    if caller is not None:
        callable_by[caller] = list(PARENT_VERBS)
    if first_message is not None:
        _grant_verbs(callable_by, reg, ["send"])

    create = dict(tier_fields)
    create.update(
        {
            "type": "create_agent",
            "parent_id": parent,
            "system_prompt": card["directive"],
            "spawned_by": reg,
            "role": role,
            "tier": card["tier"],
            "can_call": can_call,
            "callable_by": callable_by,
        }
    )
    if name is not None:
        create["name"] = name
    if line is not None:
        create["task"] = line
    if delegated:
        create["limits"] = delegated
    made = await kernel._verb_create_agent(create)
    child = made.get("id") if isinstance(made, dict) else None
    if not isinstance(child, str):
        return made

    # the registry may hear the child's `report`; the parent controls its subtree
    if store is not None and store in ctx.agents:

        def _store_edit(m):
            _grant_verbs(m, child, "*")
            return True

        ctx.edit_callable_by(store, _store_edit)
    def _reg_edit(m):
        _grant_verbs(m, child, reg_verbs)
        return True

    _, err = ctx.edit_callable_by(reg, _reg_edit)
    if err:
        return _fail("grant_failed", f"spawn: {err}")
    if caller is not None:

        def _parent_edit(m):
            m[child] = list(PARENT_VERBS)
            return True

        _, err = ctx.edit_can_call(caller, _parent_edit)
        if err:
            return _fail("grant_failed", f"spawn: {err}")
    if first_message is not None:

        def _send_edit(m):
            _grant_verbs(m, child, ["send"])
            return True

        ctx.edit_can_call(reg, _send_edit)
        # DETACHED, as the registry: a spawn answers at once, it never waits for the
        # child's turn to end
        t = asyncio.ensure_future(
            kernel.send(
                [{"target": child, "payload": {"type": "send", "text": first_message}}]
            )
        )
        _DETACHED.add(t)
        t.add_done_callback(_DETACHED.discard)
    return {"id": child, "role": role, "tier": card["tier"]}


async def _report(reg, payload, kernel):
    ctx = kernel.ctx
    state = payload.get("state")
    if state is None:
        return missing("report", "state")
    if not isinstance(state, str) or state not in STATES:
        return bad("report", "state", f"one of {'|'.join(STATES)}")
    result = payload.get("result")
    me = current_origin()
    mine = ctx.agents.get(me) if me is not None else None
    if mine is None:
        return _fail(
            "not_spawned", "report: only an agent this registry spawned may report"
        )
    if mine._meta.get("spawned_by") != reg:
        return _fail(
            "not_spawned", f"report: {mine.id!r} was not spawned by this registry"
        )
    task = mine._meta.get("task")
    task = dict(task) if isinstance(task, dict) else {"label": None, "waits_on": []}
    task["state"] = state
    task["result"] = result
    kernel.update(mine.id, task=task)
    # the parent hears it; an emit is gated, so the registry is let in first
    parent = mine.parent.id if mine.parent is not None else None
    if parent is not None and parent != reg:

        def _cc(m):
            _grant_verbs(m, parent, ["emit"])
            return True

        def _cb(m):
            _grant_verbs(m, reg, ["emit"])
            return True

        ctx.edit_can_call(reg, _cc)
        ctx.edit_callable_by(parent, _cb)
        await kernel.emit(
            parent,
            {"type": "report", "from": mine.id, "state": state, "result": result},
        )
    return {"ok": True}


async def _fleet(reg, payload, kernel):
    roles: dict = {}
    for s in _spawned(kernel, reg):
        roles.setdefault(s["role"], []).append({"id": s["id"], "state": s["state"]})
    return {"roles": dict(sorted(roles.items()))}


async def _set_limit(reg, payload, kernel):
    role = payload.get("role")
    if role is not None and (not isinstance(role, str) or not role):
        return bad("set_limit", "role", "a non-empty string")
    if "max" not in payload:
        return missing("set_limit", "max")
    mx = payload["max"]
    if mx is not None:
        mx, err = _count_arg(mx, "set_limit", "max")
        if err:
            return err
    limits = dict(kernel._meta.get("limits") or {})
    if role is None:
        if mx is None:
            limits.pop("total", None)
        else:
            limits["total"] = mx
    else:
        per = dict(limits.get("per_role") or {})
        if mx is None:
            per.pop(role, None)
        else:
            per[role] = mx
        if per:
            limits["per_role"] = per
        else:
            limits.pop("per_role", None)
    kernel.update(reg, limits=limits)
    return {"ok": True, "role": role, "max": mx}


async def _get_count(reg, payload, kernel):
    role = payload.get("role")
    if role is not None and (not isinstance(role, str) or not role):
        return bad("get_count", "role", "a non-empty string")
    running = sum(
        1
        for s in _spawned(kernel, reg)
        if _active(s["state"]) and (role is None or s["role"] == role)
    )
    return {"running": running, "max": _cap(kernel._meta.get("limits"), role)}


VERBS = {
    "reflect": _reflect,
    "roles": _roles,
    "role": _role,
    "spawn": _spawn,
    "report": _report,
    "fleet": _fleet,
    "set_limit": _set_limit,
    "get_count": _get_count,
}


def instance_verbs(agent):
    """THIS registry's verb docs: the `roles` doc carries the live list, so any agent
    with reach learns who is who. None while the registry is not configured."""
    meta = agent._meta
    if _role_dirs(meta) is None:
        return None
    cards, _ = _cards_of(meta)
    listing = "; ".join(f"{c['name']}: {c['purpose']}" for c in cards)
    docs = dict(VERB_DOCS)
    docs["roles"] = f"{docs['roles']} Roles now: {listing}"
    return docs


for _n, _f in VERBS.items():
    _f.__doc__ = VERB_DOCS[_n]


async def handler(id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    if t in ("boot", "shutdown"):
        return None
    if t == "reflect":
        return await _reflect(id, payload, kernel)
    if _role_dirs(kernel._meta) is None:
        return _disabled()
    fn = VERBS.get(t)
    if fn is None:
        return refusal(
            UNHANDLED, f"agentic_resources: unknown type {t!r}", verb=t
        )
    return await fn(id, payload, kernel)
