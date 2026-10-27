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
lowers the count and a restart rebuilds it. `running` counts EVERY spawned agent
that still exists, so a slot frees only on `delete_agent`. The task tree is not kept
here: that is `task_registry`.

NO CLEANUP: `report` only messages the parent. Nothing is retired or deleted;
`delete_agent` is the only way out.

`spawn` may branch `from` a fork point of an existing agent (`from:{agent, at}`): the
registry forks it (ai_core `fork {at}`) and applies the role over the copy.

Verbs (mirror rust verb-for-verb): reflect, roles, role, spawn, report, fleet,
set_limit, get_count.
"""

from __future__ import annotations

import asyncio

from agentic_resources import cards as cards_mod
from agentic_resources.cards import REGISTRY_TOKEN

from kernel import current_origin
from kernel._authz import ancestor_ids, authorize_call
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
    "spawn": "args: role:str (req), task:str? (sent to the child as its first message; anything but a string is `bad_arg`), name:str?, limits:{total?, per_role?}? (part of YOUR quota handed to the child as its own subtree cap). Creates the agent from the card (directive as its system prompt, the tier's backend fields from config, the card's grants) as a CHILD OF THE CALLER, and gives the caller pause/resume/stop on it. Refused past a limit: {error:'limit', scope, role, running, max, reason}. Returns {id, role, tier}. from:{agent:str, at:int}? branches the new agent off a fork point of `agent` instead (ai_core `fork {at}`, same memory store, so it starts with that conversation up to `at`), then applies the card and limits; it sits BESIDE its source, not under you, and you still get pause/resume/stop on it; you must hold `fork` on `agent` or it is refused `not_allowed`; a refusal from fork (cuts_turn, cuts_fold, no_such_row, parent_mid_turn, ...) passes through unchanged and nothing is left behind. Then the reply also has from:{agent, at} and copied. The card's grants on tools are opened on both ends (the child's own grant and the tool's callable_by, all or nothing): a card may only grant what the spawner itself holds, or the spawn is refused `not_allowed`, and a grant that cannot be written undoes the whole spawn (`grant_failed`).",
    "report": "args: state:waiting|running|blocked|done|failed|paused (req), result:any?. A spawned agent tells its parent: the parent gets a message {type:'report', from, state, result}. Nothing is stored on the record (the task tree is `task_registry`'s). Refused `not_spawned` for any other caller. Nothing is cleaned up on done. Returns {ok:true}.",
    "fleet": "No args. Returns {roles:{<role>:[{id}]}} — the agents this registry spawned, per role, read from the records (survives restart).",
    "set_limit": "args: role:str? (omitted = all agents in total), max:int|null (req; null clears). Returns {ok:true, role, max}.",
    "get_count": "args: role:str? (omitted = in total). Returns {running, max} — running counts every agent this registry spawned that still exists (a slot frees on delete_agent); max is null when uncapped.",
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


def _spawned(kernel, reg: str) -> list[dict]:
    out = [
        {
            "id": a.id,
            "role": a._meta.get("role") if isinstance(a._meta.get("role"), str) else "",
            "lead": a._meta.get("lead") if isinstance(a._meta.get("lead"), str) else None,
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


def _below_lead(kernel, s: dict, lead: str) -> bool:
    """Is `s` in `lead`'s subtree: below it in the tree, or spawned by it `from` a fork?"""
    return s["lead"] == lead or lead in _ancestors(kernel, s["id"])


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
    # the task: a string is the child's first message
    first_message = None
    task = payload.get("task")
    if task is None:
        pass
    elif isinstance(task, str):
        first_message = task
    else:
        return bad("spawn", "task", "a string")
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
    # `from`: branch the new agent off a fork point of an existing one
    src = at = None
    frm = payload.get("from")
    if frm is not None:
        a = frm.get("agent") if isinstance(frm, dict) else None
        n = frm.get("at") if isinstance(frm, dict) else None
        if not isinstance(a, str) or not a or isinstance(n, bool) or not isinstance(n, int):
            return bad("spawn", "from", "{agent:str, at:int}")
        src, at = a, n

    caller = current_origin()
    escalated = _escalation(kernel, caller, card)
    if escalated is not None:
        return escalated
    parent = caller or reg
    allsp = _spawned(kernel, reg)
    limits = meta.get("limits")

    # the registry's caps: per role, then in total
    in_role = sum(1 for s in allsp if s["role"] == role)
    mx = _cap(limits, role)
    if mx is not None and in_role >= mx:
        return _limit_refusal("role", role, in_role, mx)
    total = len(allsp)
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
            if _below_lead(kernel, s, lead)
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
                if _below_lead(kernel, s, parent)
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

    if src is not None:
        return await _spawn_from(
            kernel, reg, card, tier_fields, caller, delegated, name,
            first_message, src, at,
        )

    # the child's grants: the card's, with `$registry` made real, plus `report`
    can_call = _card_can_call(reg, card)
    # a tier that names the store the agent keeps its conversation in: the agent may
    # use it (the other end is opened below, before the first message)
    store = tier_fields.get("memory_store_id")
    store = store if isinstance(store, str) and store else None
    if store is not None:
        _grant_verbs(can_call, store, "*")
    reg_verbs = _reg_verbs_of(can_call, reg)
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
    if delegated:
        create["limits"] = delegated
    made = await kernel._verb_create_agent(create)
    child = made.get("id") if isinstance(made, dict) else None
    if not isinstance(child, str):
        return made

    # the registry may hear the child's `report`; the parent controls its subtree
    store_id = store if store is not None and store in ctx.agents else None
    if store_id is not None:

        def _store_edit(m):
            _grant_verbs(m, child, "*")
            return True

        ctx.edit_callable_by(store_id, _store_edit)
    err = _open_edges(kernel, reg, caller, child, reg_verbs) or _open_tool_edges(
        kernel, child, card
    )
    if err:
        await _undo_spawn(kernel, reg, caller, store_id, card, child)
        return err
    if first_message is not None:
        _send_first(kernel, reg, child, first_message)
    return {"id": child, "role": role, "tier": card["tier"]}


def _card_can_call(reg, card) -> dict:
    """The child's grants from its card: `$registry` made real, plus `report`."""
    can_call: dict = {}
    for k, v in card["grants"].items():
        _grant_verbs(can_call, reg if k == REGISTRY_TOKEN else k, v)
    _grant_verbs(can_call, reg, ["report"])
    return can_call


def _reg_verbs_of(can_call: dict, reg: str):
    """What the registry lets the child call on it."""
    return can_call.get(reg, ["report"])


def _open_edges(kernel, reg, caller, child, reg_verbs):
    """The two edges every spawn opens: the registry accepts the child's calls, and
    the caller (when there is one) controls the child. A refusal, or None."""
    ctx = kernel.ctx

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
    return None


def _send_first(kernel, reg, child, text) -> None:
    """A string task: let the registry `send` and send it, DETACHED, as the registry
    (a spawn answers at once, it never waits for the child's turn to end)."""

    def _send_edit(m):
        _grant_verbs(m, child, ["send"])
        return True

    kernel.ctx.edit_can_call(reg, _send_edit)
    t = asyncio.ensure_future(
        kernel.send([{"target": child, "payload": {"type": "send", "text": text}}])
    )
    _DETACHED.add(t)
    t.add_done_callback(_DETACHED.discard)


def _tool_grants(card) -> list:
    """The card's grants on tools: literal local agent ids only (not `$registry`, not
    `$self`-like tokens, not `<guid>|x` peer keys)."""
    return [
        (k, v)
        for k, v in card["grants"].items()
        if not k.startswith("$") and "|" not in k
    ]


def _escalation(kernel, caller, card):
    """A card may only grant what its spawner holds: None when there is no caller (the
    operator) or every verb the card lists on a tool is in the caller's own can_call;
    otherwise the `not_allowed` refusal. `"*"` needs the caller to hold `"*"`."""
    if caller is None:
        return None
    ctx = kernel.ctx
    holder = ctx.agents.get(caller)
    grant = holder._meta.get("can_call") if holder is not None else None
    parent = holder.parent.id if holder is not None and holder.parent is not None else None
    for tool, verbs in _tool_grants(card):
        listed = [verbs] if isinstance(verbs, str) else list(verbs)
        target = ctx.agents.get(tool)
        ancestors = ancestor_ids(target) if target is not None else set()
        for verb in listed:
            if not authorize_call(
                grant,
                verb,
                tool,
                holder_id=caller,
                holder_parent=parent,
                other_ancestors=ancestors,
            ).allowed:
                return _fail(
                    "not_allowed",
                    f"spawn: {caller} may not grant {verb!r} on {tool!r} (role {card['name']!r})",
                )
    return None


def _open_tool_edges(kernel, child, card):
    """Admit `child` on the tools its card grants (the tool's end of the edge; the
    child's own end is its can_call). A tool that is not an agent is skipped. All or
    nothing: on a refused edit the edits made are taken back. A refusal, or None."""
    ctx = kernel.ctx
    done: list = []
    for tool, verbs in _tool_grants(card):
        if tool not in ctx.agents:
            continue

        def _edit(m, verbs=verbs):
            _grant_verbs(m, child, verbs)
            return True

        _, err = ctx.edit_callable_by(tool, _edit)
        if err:
            _tool_ungrant(ctx, child, done)
            return _fail("grant_failed", f"spawn: {err}")
        done.append(tool)
    return None


def _tool_ungrant(ctx, child, tools) -> None:
    """Take `child` back out of the callable_by of `tools`."""
    for t in tools:
        ctx.edit_callable_by(t, lambda m: m.pop(child, None) is not None)


async def _undo_spawn(kernel, reg, caller, store, card, child) -> None:
    """Take a half-made spawn back: its tool edges, its keys on the registry, the
    memory store and the caller, then the agent itself."""
    ctx = kernel.ctx
    _tool_ungrant(ctx, child, [t for t, _ in _tool_grants(card) if t in ctx.agents])
    if store is not None:
        ctx.edit_callable_by(store, lambda m: m.pop(child, None) is not None)
    ctx.edit_callable_by(reg, lambda m: m.pop(child, None) is not None)
    if caller is not None:
        ctx.edit_can_call(caller, lambda m: m.pop(child, None) is not None)
    agent = ctx.agents.get(child)
    if agent is not None:
        await agent._cascade_delete()


def _holds_fork(kernel, who: str, source) -> bool:
    """Does `who` hold `fork` on `source` in its own `can_call`?"""
    w = kernel.ctx.agents.get(who)
    if w is None:
        return False
    return authorize_call(
        w._meta.get("can_call"),
        "fork",
        source.id,
        holder_id=who,
        holder_parent=w.parent.id if w.parent is not None else None,
        other_ancestors=ancestor_ids(source),
    ).allowed


def _fork_grant(ctx, reg: str, src: str):
    """Let the registry `fork` the source. Returns what to put back afterwards:
    (the registry's can_call entry for the source, the source's callable_by entry for
    the registry; None = there was none), or (None, error)."""
    before: list = [None, None]

    def _cc(m):
        before[0] = m.get(src)
        _grant_verbs(m, src, ["fork"])
        return True

    def _cb(m):
        before[1] = m.get(reg)
        _grant_verbs(m, reg, ["fork"])
        return True

    _, err = ctx.edit_can_call(reg, _cc)
    if err:
        return None, err
    _, err = ctx.edit_callable_by(src, _cb)
    if err:
        _fork_ungrant(ctx, reg, src, (before[0], None))
        return None, err
    return (before[0], before[1]), None


def _fork_ungrant(ctx, reg: str, src: str, before) -> None:
    """Take back what `_fork_grant` added."""

    def _put(key, old):
        def edit(m):
            if old is None:
                m.pop(key, None)
            else:
                m[key] = old
            return True

        return edit

    ctx.edit_can_call(reg, _put(src, before[0]))
    ctx.edit_callable_by(src, _put(reg, before[1]))


async def _spawn_from(
    kernel, reg, card, tier_fields, caller, delegated, name, first_message,
    src, at,
):
    """`spawn {role, from:{agent, at}}`: `fork {at}` of `src` makes the agent (it sits
    beside its source, in the source's store); the role card and the
    limits are then applied over the copy like any spawn."""
    ctx = kernel.ctx
    source = ctx.agents.get(src)
    # the caller (the operator when there is none) must itself hold `fork` on the source
    if caller is not None and source is not None and not _holds_fork(kernel, caller, source):
        return _fail("not_allowed", f"spawn: {caller} may not fork {src!r}")
    before, err = _fork_grant(ctx, reg, src)
    if err:
        if source is not None:
            return _fail("grant_failed", f"spawn: {err}")
        before = (None, None)
    # the registry forks AS ITSELF; whatever the fork refuses passes through unchanged
    out = (await kernel.send([{"target": src, "payload": {"type": "fork", "at": at}}]))[
        0
    ]["result"]
    _fork_ungrant(ctx, reg, src, before)
    child = out.get("id") if isinstance(out, dict) else None
    if not isinstance(child, str):
        return out
    copied = out.get("copied")
    fork = ctx.agents.get(child)
    if fork is None:
        return out

    # the copy is the source's whole record: overwrite what the role decides, drop the rest
    store = fork._meta.get("memory_store_id")
    store = store if isinstance(store, str) and store else None
    can_call: dict = {}
    fcc = fork._meta.get("can_call")
    if store is not None and isinstance(fcc, dict) and store in fcc:
        can_call[store] = fcc[store]
    for k, v in _card_can_call(reg, card).items():
        _grant_verbs(can_call, k, v)
    reg_verbs = _reg_verbs_of(can_call, reg)
    patch: dict = {
        k: v
        for k, v in tier_fields.items()
        # the fork stays in the source's store: its copied prefix must stay readable
        if k not in ("memory_store_id", "handler_module")
    }
    patch.update(
        {
            "system_prompt": card["directive"],
            "spawned_by": reg,
            "role": card["name"],
            "tier": card["tier"],
            "can_call": can_call,
        }
    )
    if name is not None:
        patch["name"] = name
        patch["display_name"] = name
    cleared = []
    # a task line copied from the source record is dropped
    cleared.append("task")
    if delegated:
        patch["limits"] = dict(delegated)
    else:
        cleared.append("limits")
    if caller is not None:
        patch["lead"] = caller
    else:
        cleared.append("lead")
    for k in cleared:
        fork._meta.pop(k, None)
    kernel.update(child, **patch)

    # the caller controls it; the registry hears its `report` and may `send` it
    def _child_edit(m):
        if caller is not None:
            m[caller] = list(PARENT_VERBS)
        if first_message is not None:
            _grant_verbs(m, reg, ["send"])
        return True

    _, err = ctx.edit_callable_by(child, _child_edit)
    fail = _fail("grant_failed", f"spawn: {err}") if err else None
    fail = fail or _open_edges(kernel, reg, caller, child, reg_verbs)
    fail = fail or _open_tool_edges(kernel, child, card)
    if fail:
        # nothing of the registry is left behind
        await _undo_spawn(kernel, reg, caller, store, card, child)
        return fail
    if first_message is not None:
        _send_first(kernel, reg, child, first_message)
    return {
        "id": child,
        "role": card["name"],
        "tier": card["tier"],
        "from": {"agent": src, "at": at},
        "copied": copied,
    }


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
        roles.setdefault(s["role"], []).append({"id": s["id"]})
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
        if (role is None or s["role"] == role)
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
