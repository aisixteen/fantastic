"""`task_registry` — which agent or subtree holds which task, and where it came from.

A generic extension between agents and task sources (gh, jira, ...): it makes NO
outbound call. `source` and `update_to` are opaque strings; the LLM reads them and
pushes updates itself. OFF BY DEFAULT: a host loads the bundle only when
`FANTASTIC_TASK_REGISTRY` is truthy, and an instance whose record has no
`task_store_id` refuses every verb `disabled`.

STORAGE IS ABSTRACT. The registry keeps its tables in the store agent named by its
record's `task_store_id`, through the store interface the agent memory uses
(`append {rows}`, `read {kinds, match, order, limit, after}`): the creator wires
`can_call` to the store and the store's `callable_by` to the registry. Each write
appends one snapshot row `{kind:"task", task_id, content:<entry>}`; every read takes
the latest row per `task_id`. Nothing is held in memory (a restart rebuilds from the
store) and nothing falls back to RAM: a store that refuses or is unreachable is
`store_refused`. A file, a path or a database is the store's business, never this
bundle's.

WRITERS are branch leads. The authority is the kernel call gate: only a lead holds
the write verbs in its `can_call` on the registry. A worker only `report`s to its
lead (agentic_resources) and the lead writes here. The one rule inside the bundle is
that a `report`'s `from` is the caller or one of its descendants.

RECORDED FACTS. An entry may also carry `versions` (`[{repo, branch, commit}]`),
`source_version` (an opaque string) and `fork_from` (`{agent, mark}`), set by
`add`/`assign`/`reassign`/`status` and kept in the reassign history, and `usage`
(the last `report`'s token counts). All are opaque facts: nothing is checked
against a repo or an agent. An entry stored before these existed loads with the
defaults (`[]`, null, null, null).

Verbs (mirror Rust verb-for-verb): `reflect`, `add`, `assign`, `reassign`,
`status`, `report`, `list`, `get`.
"""

from __future__ import annotations

import asyncio
import time

from kernel import current_origin
from kernel._refusal import (
    ALREADY_HELD,
    DISABLED,
    EXISTS,
    NOT_FOUND,
    NOT_HELD,
    NOT_IN_SUBTREE,
    STORE_REFUSED,
    UNHANDLED,
    bad,
    missing,
    refusal,
)

STATES = ("waiting", "running", "blocked", "done", "failed", "paused")
#: How many rows one store read asks for.
PAGE = 500

VERB_DOCS = {
    "reflect": "Identity and whether the registry is enabled (has a task_store_id). No args.",
    "add": "args: id:str (req), source:str (req, opaque: gh:o/r#12, jira:KEY-5, ...), parent:str? (a task id), update_to:str? (default source), holder:str|{subtree:str}? (an existing agent), status:str? (default waiting), versions:[{repo,branch,commit}]? (opaque strings, one per repo the task touches), source_version:str? (opaque), fork_from:{agent,mark}? (opaque). Refused `exists` (id taken), `not_found` (parent or holder). Returns the entry.",
    "assign": "args: id:str (req), holder:str|{subtree:str} (req; an existing agent id, or a subtree named by its root agent). versions:[{repo,branch,commit}]?, source_version:str?, fork_from:{agent,mark}? (each, when given, replaces the entry's value; `versions: []` clears it). Only a task nobody holds; else `already_held`. Returns the entry.",
    "reassign": "args: id:str (req), holder:str|{subtree:str} (req), versions?, source_version?, fork_from? (as `assign`). Only a held task; else `not_held`. Agent to agent, to a subtree and back; every step is kept in history {from, to, by, at}, plus the entry's non-empty versions, source_version and fork_from as set by that step. Returns the entry.",
    "status": "args: id:str (req), status:waiting|running|blocked|done|failed|paused?, versions?, source_version?, fork_from? (as `assign`). At least one of status and the three facts is required, else `missing_arg` for status. The lead's way to update the facts. Returns the entry.",
    "report": "args: id:str (req), from:str? (an agent id; default you), state:str?, result:any?, usage:{input_tokens,cache_read_tokens,cache_write_tokens,output_tokens}? (non-negative integers, missing keys count 0). Records the task's last report {from, state, result, at, usage}; a usage also becomes the entry's `usage` (the last one per task; a report without usage keeps it). `from` must be you or one of your descendants, else `not_in_subtree`. Returns the entry.",
    "list": "args: parent:str?, holder:str|{subtree:str}?, status:str?, ids:[str]? (only these tasks; an id that is not a task is `not_found`). Returns {tasks:[entry], usage:{input_tokens,cache_read_tokens,cache_write_tokens,output_tokens}} sorted by id; usage is the sum of the returned tasks' last usage (none counts 0).",
    "get": "args: id:str (req). Returns the entry plus children (the ids of the tasks whose parent it is); an unknown id is `not_found`.",
}


class _Refused(Exception):
    """A refusal to hand back as the reply."""

    def __init__(self, reply: dict):
        super().__init__(reply.get("error"))
        self.reply = reply


# ─── helpers ─────────────────────────────────────────────────────


def _fail(code: str, error: str, hint: str = "") -> dict:
    """A refusal of the registry's own, in the kernel's shape:
    {error:<sentence>, reason:<code>, hint?}."""
    return refusal(code, error, hint=hint)


def _ancestors(kernel, aid: str) -> list[str]:
    """Every id above `aid`, nearest first."""
    out = []
    a = kernel.ctx.agents.get(aid)
    p = a.parent if a is not None else None
    while p is not None:
        out.append(p.id)
        p = p.parent
    return out


def _str_arg(verb: str, payload: dict, arg: str) -> str:
    v = payload.get(arg)
    if v is None:
        raise _Refused(missing(verb, arg))
    if not isinstance(v, str) or not v:
        raise _Refused(bad(verb, arg, "a non-empty string"))
    return v


def _opt_str(verb: str, payload: dict, arg: str):
    v = payload.get(arg)
    if v is None:
        return None
    if not isinstance(v, str) or not v:
        raise _Refused(bad(verb, arg, "a non-empty string"))
    return v


USAGE_KEYS = ("input_tokens", "cache_read_tokens", "cache_write_tokens", "output_tokens")


def _nonempty_str(v) -> bool:
    return isinstance(v, str) and bool(v)


def _versions_arg(verb: str, payload: dict):
    """An optional `versions`: a list of `{repo, branch, commit}`, all non-empty
    strings."""
    v = payload.get("versions")
    if v is None:
        return None
    want = "a list of {repo, branch, commit} strings"
    if not isinstance(v, list):
        raise _Refused(bad(verb, "versions", want))
    for item in v:
        if not (
            isinstance(item, dict)
            and len(item) == 3
            and all(_nonempty_str(item.get(k)) for k in ("repo", "branch", "commit"))
        ):
            raise _Refused(bad(verb, "versions", want))
    return v


def _fork_from_arg(verb: str, payload: dict):
    """An optional `fork_from`: `{agent, mark}`, both non-empty strings (opaque)."""
    v = payload.get("fork_from")
    if v is None:
        return None
    if not (
        isinstance(v, dict)
        and len(v) == 2
        and all(_nonempty_str(v.get(k)) for k in ("agent", "mark"))
    ):
        raise _Refused(bad(verb, "fork_from", "an {agent, mark} object of strings"))
    return v


def _facts_arg(verb: str, payload: dict) -> tuple:
    return (
        _versions_arg(verb, payload),
        _opt_str(verb, payload, "source_version"),
        _fork_from_arg(verb, payload),
    )


def _usage_arg(payload: dict):
    """An optional `usage`: token counts, a subset of USAGE_KEYS, each a non-negative
    integer. Stored with all four keys (missing ones 0)."""
    v = payload.get("usage")
    if v is None:
        return None
    want = (
        "an object of non-negative integer token counts "
        "(input_tokens, cache_read_tokens, cache_write_tokens, output_tokens)"
    )
    if not isinstance(v, dict) or any(k not in USAGE_KEYS for k in v):
        raise _Refused(bad("report", "usage", want))
    out = {}
    for k in USAGE_KEYS:
        n = v.get(k, 0)
        if not isinstance(n, int) or isinstance(n, bool) or n < 0:
            raise _Refused(bad("report", "usage", want))
        out[k] = n
    return out


def _with_defaults(content: dict) -> dict:
    """An entry stored before the recorded facts existed gets their defaults."""
    e = dict(content)
    for k, d in (
        ("versions", []),
        ("source_version", None),
        ("fork_from", None),
        ("usage", None),
    ):
        e.setdefault(k, d)
    return e


def _set_facts(entry: dict, facts: tuple) -> None:
    versions, source_version, fork_from = facts
    if versions is not None:
        entry["versions"] = versions
    if source_version is not None:
        entry["source_version"] = source_version
    if fork_from is not None:
        entry["fork_from"] = fork_from


def _state_arg(verb: str, arg: str, v: str) -> None:
    if v not in STATES:
        raise _Refused(bad(verb, arg, f"one of {'|'.join(STATES)}"))


def _holder_arg(kernel, verb: str, v):
    """A holder: an agent id, or `{subtree: <root agent id>}`; it must name an
    existing agent. Returns the holder as stored."""
    want = "an agent id or {subtree: agent id}"
    if isinstance(v, str) and v:
        aid = v
    elif (
        isinstance(v, dict)
        and len(v) == 1
        and isinstance(v.get("subtree"), str)
        and v["subtree"]
    ):
        aid = v["subtree"]
    else:
        raise _Refused(bad(verb, "holder", want))
    if aid not in kernel.ctx.agents:
        raise _Refused(refusal(NOT_FOUND, f"{verb}: holder {aid!r} is not an agent"))
    return v


def _not_found_task(verb: str, tid: str) -> _Refused:
    return _Refused(refusal(NOT_FOUND, f"{verb}: task {tid!r} not found"))


def _history_item(frm, to, by, entry: dict) -> dict:
    """One history step; it carries the entry's recorded facts only when it has any."""
    item = {"from": frm, "to": to, "by": by, "at": time.time()}
    for k in ("versions", "source_version", "fork_from"):
        v = entry.get(k)
        if v is not None and v != []:
            item[k] = v
    return item


# ─── the store ───────────────────────────────────────────────────


async def _store_call(kernel, store: str, payload: dict) -> dict:
    """ONE gated send to the store, as the registry. A refusal or a failed dispatch is
    `store_refused`, with the store's own error and reason."""
    out = (await kernel.send([{"target": store, "payload": payload}]))[0]
    res = out.get("result")
    err = res.get("error") if isinstance(res, dict) else None
    if err or out.get("status") != "completed":
        text = str(err) if err else str(out.get("status"))
        r = _fail(STORE_REFUSED, f"store {store!r} refused: {text}")
        r["store_error"] = text
        r["store_reason"] = res.get("reason") if isinstance(res, dict) else None
        raise _Refused(r)
    return res if isinstance(res, dict) else {}


async def _load(kernel, store: str, only: str | None = None) -> dict:
    """Every task, latest snapshot per `task_id`, read page by page."""
    tasks: dict = {}
    after = None
    while True:
        q = {"type": "read", "kinds": ["task"], "order": "asc", "limit": PAGE}
        if only is not None:
            q["match"] = {"task_id": only}
        if after is not None:
            q["after"] = after
        res = await _store_call(kernel, store, q)
        for row in res.get("rows") or []:
            content = row.get("content")
            if isinstance(row.get("task_id"), str) and isinstance(content, dict):
                tasks[row["task_id"]] = _with_defaults(content)
        after = res.get("next")
        if after is None:
            return dict(sorted(tasks.items()))


async def _save(kernel, store: str, entry: dict) -> None:
    row = {"kind": "task", "task_id": entry["id"], "content": entry}
    await _store_call(kernel, store, {"type": "append", "rows": [row]})


def _lock_of(kernel) -> asyncio.Lock:
    """One lock per registry agent, so the read-modify-append of two leads cannot
    race."""
    lock = getattr(kernel, "_task_lock", None)
    loop = asyncio.get_running_loop()
    if lock is None or getattr(kernel, "_task_lock_loop", None) is not loop:
        lock = asyncio.Lock()
        kernel._task_lock = lock
        kernel._task_lock_loop = loop
    return lock


# ─── verbs ───────────────────────────────────────────────────────


async def _add(kernel, store, caller, payload):
    tid = _str_arg("add", payload, "id")
    source = _str_arg("add", payload, "source")
    parent = _opt_str("add", payload, "parent")
    update_to = _opt_str("add", payload, "update_to") or source
    status = _opt_str("add", payload, "status") or "waiting"
    _state_arg("add", "status", status)
    facts = _facts_arg("add", payload)
    holder = None
    if payload.get("holder") is not None:
        holder = _holder_arg(kernel, "add", payload["holder"])
    tasks = await _load(kernel, store)
    if tid in tasks:
        raise _Refused(refusal(EXISTS, f"add: task {tid!r} already exists"))
    if parent is not None and parent not in tasks:
        raise _not_found_task("add", parent)
    entry = {
        "id": tid,
        "parent": parent,
        "source": source,
        "update_to": update_to,
        "holder": holder,
        "history": [],
        "status": status,
        "report": None,
        "versions": [],
        "source_version": None,
        "fork_from": None,
        "usage": None,
    }
    _set_facts(entry, facts)
    if holder is not None:
        entry["history"] = [_history_item(None, holder, caller, entry)]
    await _save(kernel, store, entry)
    return entry


async def _assign(kernel, store, caller, payload, re: bool = False):
    verb = "reassign" if re else "assign"
    tid = _str_arg(verb, payload, "id")
    facts = _facts_arg(verb, payload)
    if payload.get("holder") is None:
        raise _Refused(missing(verb, "holder"))
    to = _holder_arg(kernel, verb, payload["holder"])
    entry = (await _load(kernel, store, tid)).get(tid)
    if entry is None:
        raise _not_found_task(verb, tid)
    frm = entry["holder"]
    if not re and frm is not None:
        raise _Refused(
            _fail(ALREADY_HELD, f"assign: task {tid!r} is held by {frm}; use reassign")
        )
    if re and frm is None:
        raise _Refused(
            _fail(NOT_HELD, f"reassign: task {tid!r} has no holder; use assign")
        )
    _set_facts(entry, facts)
    entry["history"].append(_history_item(frm, to, caller, entry))
    entry["holder"] = to
    await _save(kernel, store, entry)
    return entry


async def _reassign(kernel, store, caller, payload):
    return await _assign(kernel, store, caller, payload, re=True)


async def _status(kernel, store, caller, payload):
    tid = _str_arg("status", payload, "id")
    facts = _facts_arg("status", payload)
    if any(f is not None for f in facts):
        status = _opt_str("status", payload, "status")
    else:
        status = _str_arg("status", payload, "status")
    if status is not None:
        _state_arg("status", "status", status)
    entry = (await _load(kernel, store, tid)).get(tid)
    if entry is None:
        raise _not_found_task("status", tid)
    if status is not None:
        entry["status"] = status
    _set_facts(entry, facts)
    await _save(kernel, store, entry)
    return entry


async def _report(kernel, store, caller, payload):
    tid = _str_arg("report", payload, "id")
    state = _opt_str("report", payload, "state")
    if state is not None:
        _state_arg("report", "state", state)
    result = payload.get("result")
    usage = _usage_arg(payload)
    who = _opt_str("report", payload, "from") or caller
    # the reporter is the caller or below it; the operator may report for anyone
    if who is not None:
        if who not in kernel.ctx.agents:
            raise _Refused(refusal(NOT_FOUND, f"report: {who!r} is not an agent"))
        if caller is not None and who != caller and caller not in _ancestors(kernel, who):
            raise _Refused(
                _fail(
                    NOT_IN_SUBTREE,
                    f"report: {who!r} is not {caller} or one of its descendants",
                )
            )
    entry = (await _load(kernel, store, tid)).get(tid)
    if entry is None:
        raise _not_found_task("report", tid)
    entry["report"] = {
        "from": who,
        "state": state,
        "result": result,
        "at": time.time(),
        "usage": usage,
    }
    if usage is not None:
        entry["usage"] = usage
    await _save(kernel, store, entry)
    return entry


async def _list(kernel, store, caller, payload):
    parent = _opt_str("list", payload, "parent")
    status = _opt_str("list", payload, "status")
    holder = payload.get("holder")
    ids = payload.get("ids")
    if ids is not None and not (
        isinstance(ids, list) and all(_nonempty_str(i) for i in ids)
    ):
        raise _Refused(bad("list", "ids", "a list of non-empty task ids"))
    tasks = await _load(kernel, store)
    for i in ids or []:
        if i not in tasks:
            raise _not_found_task("list", i)
    out = [
        t
        for t in tasks.values()
        if (ids is None or t["id"] in ids)
        and (parent is None or t["parent"] == parent)
        and (status is None or t["status"] == status)
        and (holder is None or t["holder"] == holder)
    ]
    total = {
        k: sum((t.get("usage") or {}).get(k, 0) for t in out) for k in USAGE_KEYS
    }
    return {"tasks": out, "usage": total}


async def _get(kernel, store, caller, payload):
    tid = _str_arg("get", payload, "id")
    tasks = await _load(kernel, store)
    entry = tasks.get(tid)
    if entry is None:
        raise _not_found_task("get", tid)
    entry = dict(entry)
    entry["children"] = [t["id"] for t in tasks.values() if t["parent"] == tid]
    return entry


#: The verbs that change the registry (serialized per registry agent).
WRITES = {
    "add": _add,
    "assign": _assign,
    "reassign": _reassign,
    "status": _status,
    "report": _report,
}
READS = {"list": _list, "get": _get}



async def _reflect(kernel, store, caller, payload):
    return {
        "id": kernel.id,
        "sentence": "A registry of tasks: who holds each one and where it came from.",
        "enabled": store is not None,
    }


VERBS = {"reflect": _reflect, **WRITES, **READS}

for _n, _f in VERBS.items():
    _f.__doc__ = VERB_DOCS[_n]


def _store_id(meta: dict):
    s = meta.get("task_store_id")
    return s if isinstance(s, str) and s else None


async def handler(id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    if t in ("boot", "shutdown"):
        return None
    store = _store_id(kernel._meta)
    if t == "reflect":
        return await _reflect(kernel, store, None, payload)
    if store is None:
        return _fail(
            DISABLED,
            "task_registry is disabled: the record has no task_store_id",
            hint="set task_store_id on the record",
        )
    caller = current_origin()
    try:
        if t in READS:
            return await READS[t](kernel, store, caller, payload)
        if t in WRITES:
            async with _lock_of(kernel):
                return await WRITES[t](kernel, store, caller, payload)
    except _Refused as e:
        return e.reply
    return refusal(UNHANDLED, f"task_registry: unknown type {t!r}", verb=t)
