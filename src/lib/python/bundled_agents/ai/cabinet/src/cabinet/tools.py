"""cabinet bundle — a master AI opens cabinets: subcanvases, each with one worker AI.

THE SHAPE. One MANAGER (a `cabinet.tools` agent the operator makes once, with the
grants in `readme.md`) owns the CABINETS as its children. A cabinet is a subcanvas:
its record carries a live html view (`view.html`) that watches it, and it owns ONE
WORKER AI as its child. The worker's EQUIPMENT (exec runners, file bridges, helper AIs,
panels — whatever it creates) is the worker's own child, so it lives inside the cabinet
and is reached through the worker's `$subtree` grant and nothing else.

Roles, on the record (`cabinet_role`):
  manager   open_cabinet · close_cabinet · cabinets · conclude · reflect
  cabinet   status · view · reflect   (`status` is the worker's report up)

NOTHING IS MADE OR CHANGED BEHIND A GRANT. The manager makes and edits every record
by a gated send to the ROOT (`create_agent`, `update_agent`, `delete_agent`), as itself:
the operator must grant the manager those three verbs on the root, and the root must
name the manager in its `callable_by`. Each record's grants are restated whole, because
`update_agent` replaces a dict field (a grant map included), never merges it.

THE MASTER is the AI that sent `open_cabinet` (the dispatch origin). Its WORKER is an
existing AI backend's record, copied: default the master itself (`backend=` names
another). The copy keeps the connection and the credential the backend names
(`credentials`), and the memory store, so an existing backend's endpoint and key stay
where their owner set them. `setup=` overrides fields of that copy; a worker on a
different handler, endpoint or key is allowed and flagged `new_backend` with a warning,
because a new backend is the thing the docs advise against.

WIRING. Every open and close rewrites the directory (`cabinet_peers`: each live
cabinet's id, worker and task) and the instructions (`post_system_prompt`) of every
live worker, and grants. A worker and each peer worker, and a worker and the master,
may exchange MESSAGES only (`MESSAGE_VERBS`: `send`, `reflect`). The master and the
manager may call each worker in full (monitor, re-task, stop). A worker may call its
own cabinet's `status`, its own equipment (`$subtree`) and itself (`$self`), and the
root's `create_agent` alone, which is how it makes its equipment. The close deletes the
cabinet with its whole subtree; the kernel prunes every grant that names a deleted id.

THE CEILING. An optional `max_cabinets` on the manager refuses an open loudly when
that many cabinets are live (`reason: "ceiling"`). None: the LLM decides the count.
"""

from __future__ import annotations

import re
import secrets
import time
from importlib import resources

from kernel._kernel import current_origin
from kernel._refusal import (
    BAD_ARG,
    MISSING_ARG,
    UNHANDLED,
    UNKNOWN_AGENT,
    bad,
    missing,
    refusal,
)
from kernel._state import RESERVED_RECORD_KEYS

HANDLER = "cabinet.tools"
ROLE = "cabinet_role"
MANAGER = "manager"
CABINET = "cabinet"

#: The worker's own report up: the only verb a worker sends its cabinet.
STATUS_VERB = "status"

#: What one worker may send another worker, or the master: messages, and nothing
#: else. A peer that may only message cannot stop, clear or re-task the other.
MESSAGE_VERBS = ["send", "reflect"]

#: Record keys a worker does NOT copy from its backend: identity, wiring and the
#: backend's own prompt and view. Everything else (handler, endpoint, model, window,
#: sampling, the credential reference, the memory store) IS copied.
_NOT_COPIED = frozenset(
    {
        "id",
        "parent_id",
        "can_call",
        "callable_by",
        "description",
        "display_name",
        "system_prompt",
        "system_prompt_file",
        "pre_system_prompt",
        "post_system_prompt",
        "view",
        "delete_lock",
        "root",
        "children_dir",
        "watch",
        "ingress_rule",
        "main",
        ROLE,
        "task",
        "worker",
        "master",
        "status",
        "cabinet_peers",
        "max_cabinets",
    }
)
#: Keys a `setup=` may NOT carry: wiring the manager writes itself.
_WIRING = _NOT_COPIED - {"system_prompt", "pre_system_prompt", "system_prompt_file"}

_SLUG = re.compile(r"[^a-z0-9]+")
_EMPTY_STATUS = {"line": "", "progress": None, "result": None, "at": None}
_NEW_BACKEND_WARNING = (
    "a NEW backend: its endpoint, key or handler is not the existing one's. Creating a "
    "backend is discouraged — its credentials, cost and limits stay where their owner "
    "set them when a worker connects to an existing backend."
)


def _view_bytes(cabinet_id: str) -> str:
    html = resources.files("cabinet").joinpath("view.html").read_text(encoding="utf-8")
    return html.replace("__CABINET_ID__", cabinet_id)


# ─── the one gated write path ────────────────────────────────────


def _root(kernel) -> str:
    return kernel.ctx.root.id


async def _call(kernel, target: str, payload: dict) -> dict:
    """One send as this agent. A failed or refused send is a dict with `error`."""
    (out,) = await kernel.send([{"target": target, "payload": payload}])
    res = out.get("result")
    if not isinstance(res, dict):
        return {"error": f"{target} {payload.get('type')}: {out.get('status')}"}
    return res


async def _tree(kernel, payload: dict) -> dict:
    """A tree verb (create / update / delete), sent to the root, gated as this agent."""
    return await _call(kernel, _root(kernel), payload)


async def _wire(
    kernel, holder: str, *, can_call=None, callable_by=None, **fields
) -> str | None:
    """Edit `holder`'s grants and fields in ONE update. `can_call` / `callable_by` map
    `key -> verbs` (None removes the key); the whole maps are restated, because
    `update_agent` replaces them. Returns the refusal's error, or None."""
    rec = kernel.get(holder)
    if rec is None:
        return f"`{holder}` does not exist"
    upd: dict = {"type": "update_agent", "id": holder, **fields}
    for field, changes in (("can_call", can_call), ("callable_by", callable_by)):
        if not changes:
            continue
        grant = dict(rec.get(field) or {})
        for key, verbs in changes.items():
            if verbs is None:
                grant.pop(key, None)
            else:
                grant[key] = verbs
        upd[field] = grant
    if set(upd) == {"type", "id"}:
        return None
    return (await _tree(kernel, upd)).get("error")


# ─── reading the cabinets ────────────────────────────────────────


def _cabinets(kernel) -> list[dict]:
    """The manager's live cabinets' records, in creation order."""
    out = []
    for cid in kernel.child_ids():
        rec = kernel.get(cid)
        if rec is not None and rec.get(ROLE) == CABINET:
            out.append(rec)
    return out


def _subtree(kernel, agent_id: str) -> list[str]:
    node = kernel.ctx.agents[agent_id]
    return [agent_id] + [
        c for child in node.child_ids() for c in _subtree(kernel, child)
    ]


def _equipment(kernel, worker: str) -> list[str]:
    agent = kernel.ctx.agents.get(worker)
    return agent.child_ids() if agent is not None else []


def _is_ai(rec: dict) -> bool:
    return isinstance(rec.get("memory_store_id"), str) and isinstance(
        rec.get("handler_module"), str
    )


def _instructions(cab: dict, peers: list[dict], master: str | None, root: str) -> str:
    """The worker's post-system block: its role, its directory and how to work. It
    rides AFTER the prompt in force (`post_system_prompt`), so the backend's own prompt
    is never replaced."""
    worker = cab["worker"]
    lines = [
        f"You are the worker AI of cabinet `{cab['id']}`. Your task: {cab['task']}",
        f"Your worker id is `{worker}`. You are one of {len(peers) + 1} cooperating workers, "
        "one per cabinet. Your peers:",
    ]
    if peers:
        lines += [
            f"- worker `{p['worker']}` (cabinet `{p['cabinet']}`): {p['task']}"
            for p in peers
        ]
    else:
        lines.append("- none yet; more cabinets may open later.")
    lines += [
        "",
        "How to work with each other:",
        f"- Message a peer, or your master `{master}`, by sending `send` with a `text`: "
        '`send {target: <worker id>, payload: {type: "send", text: "..."}}`. '
        "They can message you the same way; messages are all you may send them.",
        f"- Report your state up to your cabinet, every time it changes: "
        f'`send {{target: "{cab["id"]}", payload: {{type: "status", line: "...", '
        "progress: 0-100, result: ...}}`. Put your final answer in `result`.",
        f"- Equipment: send `create_agent` to the root `{root}` with "
        f'`parent_id: "{worker}"`, so the new agent is your child. Grant each one to '
        f"your cabinet only: its `callable_by` names `{worker}` or your own equipment, "
        f"nothing else. The root takes only your `create_agent`; nothing else of it is yours.",
    ]
    return "\n".join(lines)


async def _refresh_directories(kernel) -> str | None:
    """Rewrite the directory and instructions of every live worker, from the live cabinets."""
    live = _cabinets(kernel)
    for cab in live:
        peers = [
            {"cabinet": c["id"], "worker": c["worker"], "task": c["task"]}
            for c in live
            if c["id"] != cab["id"]
        ]
        text = _instructions(cab, peers, cab.get("master"), _root(kernel))
        err = await _wire(
            kernel, cab["worker"], cabinet_peers=peers, post_system_prompt=text
        )
        if err:
            return err
    return None


# ─── verbs: the manager ──────────────────────────────────────────


async def _open(manager: str, payload: dict, kernel):
    """args: task:str (req), name:str?, backend:str? (an existing AI agent id; default the master), setup:dict? (fields over the backend's copy; a new backend is flagged). Opens one cabinet: a subcanvas with its worker AI, wired to every live worker and to the master. The worker is NOT started — send it its first message (the task). Returns {id, worker, master, backend, new_backend, peers}."""
    task = payload.get("task")
    if not isinstance(task, str) or not task.strip():
        return missing(
            "open_cabinet",
            "task",
            hint='say what the cabinet must do: open_cabinet {task: "..."}',
        )
    name = payload.get("name")
    if name is not None and (not isinstance(name, str) or not name.strip()):
        return bad("open_cabinet", "name", "a non-empty string")
    master = current_origin()
    if master is None:
        return refusal(
            MISSING_ARG,
            "open_cabinet: the master is the AI that sends it — an operator send has no master to wire to",
            arg="master",
            hint="open the cabinet from the master AI's own turn",
        )
    backend_id = payload.get("backend") or master
    if not isinstance(backend_id, str) or not backend_id:
        return bad("open_cabinet", "backend", "an existing AI agent id")
    backend = kernel.get(backend_id)
    if backend is None:
        return refusal(
            UNKNOWN_AGENT,
            f"open_cabinet: backend `{backend_id}` does not exist",
            id=backend_id,
        )
    if not _is_ai(backend):
        return refusal(
            BAD_ARG,
            f"open_cabinet: `{backend_id}` is not an AI backend (no memory_store_id)",
            arg="backend",
        )
    setup = payload.get("setup") or {}
    if not isinstance(setup, dict) or not all(isinstance(k, str) for k in setup):
        return bad("open_cabinet", "setup", "a map of record fields")
    if wiring := sorted(k for k in setup if k in _WIRING or k in RESERVED_RECORD_KEYS):
        return bad("open_cabinet", "setup", f"a map without wiring keys (got {wiring})")
    ceiling = kernel.get(kernel.id).get("max_cabinets")
    if ceiling is not None:
        if isinstance(ceiling, bool) or not isinstance(ceiling, int) or ceiling < 1:
            return bad(
                "open_cabinet", "max_cabinets", "a positive int on the manager record"
            )
        open_now = len(_cabinets(kernel))
        if open_now >= ceiling:
            return {
                "error": f"open_cabinet: the ceiling is {ceiling} cabinets and {open_now} are open — close or conclude one first",
                "reason": "ceiling",
                "ceiling": ceiling,
                "open": open_now,
            }

    worker_rec = {k: v for k, v in backend.items() if k not in _NOT_COPIED}
    worker_rec.update(setup)
    store = worker_rec.get("memory_store_id")
    if kernel.get(store) is None:
        return refusal(
            UNKNOWN_AGENT,
            f"open_cabinet: the worker's memory store `{store}` does not exist",
            id=store,
        )
    cred = worker_rec.get("credentials")
    if isinstance(cred, dict) and kernel.get(cred.get("store")) is None:
        return refusal(
            UNKNOWN_AGENT,
            f"open_cabinet: the worker's credential store `{cred.get('store')}` does not exist",
            id=cred.get("store"),
        )
    new_backend = (
        worker_rec.get("handler_module") != backend.get("handler_module")
        or worker_rec.get("endpoint") != backend.get("endpoint")
        or worker_rec.get("credentials") != backend.get("credentials")
    )

    slug = _SLUG.sub("_", name.strip().lower()).strip("_") if name else ""
    cab_id = f"cabinet_{slug}" if slug else f"cabinet_{secrets.token_hex(3)}"
    worker_id = f"{cab_id}_worker"
    root = _root(kernel)
    made = await _tree(
        kernel,
        {
            "type": "create_agent",
            "parent_id": kernel.id,
            "id": cab_id,
            "handler_module": HANDLER,
            ROLE: CABINET,
            "task": task.strip(),
            "master": master,
            "worker": worker_id,
            "status": dict(_EMPTY_STATUS),
            "display_name": name.strip() if name else task.strip()[:60],
            "description": f"Cabinet: {task.strip()[:200]}",
            "view": {
                "kind": "html",
                "bytes": _view_bytes(cab_id),
                "calls": ["view", "watch"],
            },
            "can_call": {"$self": "*"},
            "callable_by": {
                "$self": "*",
                "$parent": "*",
                worker_id: [STATUS_VERB],
                # the master reads its cabinet directly; the canvas (`user`) draws its panel
                master: ["view", "reflect"],
                "user": ["view", "watch", "reflect"],
            },
        },
    )
    if made.get("error"):
        return made

    handler = worker_rec.pop("handler_module")
    made_worker = await _tree(
        kernel,
        {
            "type": "create_agent",
            "parent_id": cab_id,
            "id": worker_id,
            "handler_module": handler,
            "description": f"Worker of cabinet {cab_id}: {task.strip()[:200]}",
            # creating on the root (its equipment: `create_agent` only), its equipment
            # (`$subtree`), itself, and the cabinet's status channel
            "can_call": {
                "$self": "*",
                "$parent": [STATUS_VERB],
                "$subtree": "*",
                root: ["create_agent"],
            },
            "callable_by": {"$self": "*", kernel.id: "*", master: "*"},
            **worker_rec,
        },
    )
    if made_worker.get("error"):
        await _tree(kernel, {"type": "delete_agent", "id": cab_id})
        return made_worker

    # Wire the NEW worker to the master, the stores and every live worker, and every
    # live worker to it. Every step runs (no short-circuit); the first failure deletes
    # the cabinet it half-made.
    steps = [
        _wire(kernel, root, callable_by={worker_id: ["create_agent"]}),
        # the master calls the worker in full, and the worker only messages the master
        _wire(
            kernel,
            master,
            can_call={worker_id: "*", cab_id: ["view", "reflect"]},
            callable_by={worker_id: MESSAGE_VERBS},
        ),
        _wire(kernel, kernel.id, can_call={worker_id: "*"}),
        _wire(kernel, store, callable_by={worker_id: "*"}),
        _wire(
            kernel,
            worker_id,
            can_call={master: MESSAGE_VERBS, store: "*"},
        ),
    ]
    if isinstance(cred, dict):
        steps.append(_wire(kernel, cred["store"], callable_by={worker_id: "*"}))
        steps.append(_wire(kernel, worker_id, can_call={cred["store"]: "*"}))
    for other in _cabinets(kernel):
        if other["worker"] != worker_id:
            # messages both ways between this worker and each peer, nothing more
            steps.append(
                _wire(
                    kernel,
                    other["worker"],
                    can_call={worker_id: MESSAGE_VERBS},
                    callable_by={worker_id: MESSAGE_VERBS},
                )
            )
            steps.append(
                _wire(
                    kernel,
                    worker_id,
                    can_call={other["worker"]: MESSAGE_VERBS},
                    callable_by={other["worker"]: MESSAGE_VERBS},
                )
            )
    failure = None
    for step in steps:
        err = await step
        failure = failure or err
    failure = failure or await _refresh_directories(kernel)
    if failure:
        await _tree(kernel, {"type": "delete_agent", "id": cab_id})
        return {
            "error": f"open_cabinet: wiring failed and the cabinet was removed: {failure}"
        }

    cab_rec = kernel.get(cab_id)
    peers = [
        {"cabinet": c["id"], "worker": c["worker"], "task": c["task"]}
        for c in _cabinets(kernel)
        if c["id"] != cab_id
    ]
    out = {
        "id": cab_id,
        "worker": worker_id,
        "master": master,
        "backend": backend_id,
        "new_backend": new_backend,
        "peers": peers,
        "next": f'send `{worker_id}` its first message (the task) with {{type: "send", text: ...}}',
    }
    if new_backend:
        out["warning"] = _NEW_BACKEND_WARNING
    return (
        out
        if cab_rec is not None
        else {"error": "open_cabinet: the cabinet vanished while wiring"}
    )


async def _close(manager: str, payload: dict, kernel):
    """args: id:str (req) — a cabinet of this manager. Deletes the cabinet with its worker and ALL its equipment, then rewrites the live workers' directories. Returns {closed, deleted}."""
    cab_id = payload.get("id")
    if not isinstance(cab_id, str) or not cab_id:
        return missing(
            "close_cabinet", "id", hint='close_cabinet {id: "cabinet_<name>"}'
        )
    rec = kernel.get(cab_id)
    if rec is None or rec.get(ROLE) != CABINET or rec.get("parent_id") != manager:
        return refusal(
            UNKNOWN_AGENT,
            f"close_cabinet: `{cab_id}` is not a cabinet of this manager",
            id=cab_id,
        )
    gone = _subtree(kernel, cab_id)
    res = await _tree(kernel, {"type": "delete_agent", "id": cab_id})
    if not res.get("deleted"):
        return {
            **res,
            "error": res.get("error") or f"close_cabinet: `{cab_id}` was not deleted",
        }
    if err := await _refresh_directories(kernel):
        return {
            "error": f"close_cabinet: deleted `{cab_id}`, but a worker's directory did not update: {err}",
            "deleted": gone,
        }
    return {"closed": cab_id, "deleted": gone}


async def _cabinets_verb(manager: str, payload: dict, kernel):
    """No args. Every live cabinet: its task, worker, live status and equipment. The master's monitor."""
    rows = [
        {
            "id": c["id"],
            "name": c.get("display_name"),
            "task": c["task"],
            "worker": c["worker"],
            "status": c.get("status"),
            "equipment": _equipment(kernel, c["worker"]),
        }
        for c in _cabinets(kernel)
    ]
    return {
        "cabinets": rows,
        "count": len(rows),
        "origin": f"{manager} cabinets — re-send; changes as cabinets open, report and close",
    }


async def _conclude(manager: str, payload: dict, kernel):
    """No args. Closes EVERY cabinet this manager opened, deleting each subcanvas with its worker and equipment. The manager stays. Returns {concluded, deleted}."""
    concluded, deleted = [], []
    for cab in _cabinets(kernel):
        res = await _close(manager, {"id": cab["id"]}, kernel)
        if res.get("error"):
            return {**res, "concluded": concluded, "deleted": deleted}
        concluded.append(cab["id"])
        deleted.extend(res.get("deleted", []))
    return {"concluded": concluded, "deleted": deleted, "count": len(concluded)}


# ─── verbs: a cabinet ────────────────────────────────────────────


async def _status(cab_id: str, payload: dict, kernel):
    """args: line:str (req), progress:number 0-100?, result:any?. The worker's report up: stored on the cabinet and emitted on its own id, so its view and every watcher see it live."""
    line = payload.get("line")
    if not isinstance(line, str):
        return bad(STATUS_VERB, "line", "a string: what the worker is doing now")
    progress = payload.get("progress")
    if progress is not None and (
        isinstance(progress, bool)
        or not isinstance(progress, (int, float))
        or not 0 <= progress <= 100
    ):
        return bad(STATUS_VERB, "progress", "a number from 0 to 100")
    st = {
        "line": line,
        "progress": progress,
        "result": payload.get("result"),
        "at": time.time(),
    }
    kernel.update(cab_id, status=st)
    await kernel.emit(cab_id, {"type": "update", "cabinet": cab_id, "status": st})
    return {"ok": True, "cabinet": cab_id}


async def _view(cab_id: str, payload: dict, kernel):
    """No args. The cabinet's panel: its task, worker, live status and equipment."""
    rec = kernel.get(cab_id) or {}
    return {
        "id": cab_id,
        "task": rec.get("task"),
        "worker": rec.get("worker"),
        "status": rec.get("status"),
        "equipment": _equipment(kernel, rec.get("worker", "")),
    }


# ─── reflect and dispatch ────────────────────────────────────────


async def _reflect(id: str, payload: dict, kernel):
    """No args. The agent's role and what it holds."""
    rec = kernel.get(id) or {}
    if rec.get(ROLE) == MANAGER:
        return {
            "sentence": "A cabinet manager: opens cabinets (a subcanvas with one worker AI each), monitors them, closes them, concludes.",
            "role": MANAGER,
            "max_cabinets": rec.get("max_cabinets"),
            "open": len(_cabinets(kernel)),
        }
    return {
        "sentence": f"A cabinet: the subcanvas of one worker AI — {rec.get('task', '')}".rstrip(),
        "role": CABINET,
        "task": rec.get("task"),
        "worker": rec.get("worker"),
        "status": rec.get("status"),
    }


MANAGER_VERBS = {
    "reflect": _reflect,
    "open_cabinet": _open,
    "close_cabinet": _close,
    "cabinets": _cabinets_verb,
    "conclude": _conclude,
}
CABINET_VERBS = {"reflect": _reflect, "status": _status, "view": _view}


async def handler(id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    role = (kernel.get(id) or {}).get(ROLE)
    table = (
        MANAGER_VERBS if role == MANAGER else CABINET_VERBS if role == CABINET else None
    )
    if table is None:
        return refusal(
            UNHANDLED, f"cabinet: {id} has no cabinet_role (manager or cabinet)", verb=t
        )
    fn = table.get(t)
    if fn is None:
        return refusal(UNHANDLED, f"cabinet: unknown type {t!r} for a {role}", verb=t)
    return await fn(id, payload, kernel)
