"""cabinet bundle — a master AI opens cabinets: canvases, each with one worker AI.

THE SHAPE. One MANAGER (a `cabinet.tools` agent the operator makes once, with the
grants in `readme.md`) owns the CABINETS as its children, and names the CANVAS that
spawns them (`canvas_id` on its record). A cabinet is a CANVAS of its own: `open`
asks that canvas (`new_canvas`) to make it, so the cabinet's canvas is a CHILD of the
spawning canvas, nested in the hierarchy. The cabinet's record is the live status
cell (`view.html`) that the main canvas shows and that opens the cabinet's canvas.
The cabinet owns ONE WORKER AI as its child.

The worker's EQUIPMENT (exec runners, file bridges, helper AIs, panels) is placed on
the cabinet's CANVAS with `create` (a child of that canvas) and `place` (a view). It
is reached through the canvas's grants and nothing else.

Roles, on the record (`cabinet_role`):
  manager   open_cabinet · close_cabinet · cabinets · conclude · reflect
  cabinet   status · view · reflect   (`status` is the worker's report up)

NOTHING IS MADE OR CHANGED BEHIND A GRANT. The manager makes and edits every record
by a gated send to the ROOT (`create_agent`, `update_agent`, `delete_agent`), as itself:
the operator must grant the manager those three verbs on the root, and the root must
name the manager in its `callable_by`. Each record's grants are restated whole, because
`update_agent` replaces a dict field (a grant map included), never merges it. The
manager also asks the spawning canvas for `new_canvas`, granted as the wiring of
`open` does (its own `can_call`, the canvas's `callable_by`).

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
own cabinet's status (`status`) and its own cabinet canvas (`create`, `place`, and
nothing else on it), and itself (`$self`). Nothing else: the worker holds no grant on
the root. The close deletes the cabinet with its worker, and the canvas with the
equipment on it; the kernel prunes every grant that names a deleted id.

SERIAL WRITES. open_cabinet, close_cabinet and conclude run ONE AT A TIME per manager
(`_lock_of`). Each one reads whole grant maps and writes them back, so two in flight
would overwrite each other's entries; the lock covers the whole read-wire-refresh.

THE CEILING. An optional `max_cabinets` on the manager refuses an open loudly when
that many cabinets are live (`reason: "ceiling"`). None: the LLM decides the count.

CONCLUDE KEEPS. `conclude {keep: [ids]}` closes every cabinet not named in `keep`.
A `keep` id is a live cabinet of this manager (the whole cabinet stays: its canvas,
worker and equipment), or a RESULT: an agent placed on a cabinet's canvas, at any depth.
Keeping a result keeps the cabinet's canvas and the path from it to the result, and
closes the cabinet's worker, its record and every other piece of equipment. Any other
`keep` id is refused before anything is deleted.
"""

from __future__ import annotations

import asyncio
import re
import secrets
import time
import weakref
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

#: The canvas verb that makes a child canvas: the manager asks the spawning canvas for it.
NEW_CANVAS_VERB = "new_canvas"

#: What a worker may do on its cabinet's canvas: place equipment (`create`) and place
#: views (`place`). Nothing else on the canvas.
CANVAS_VERBS = ["create", "place"]

#: The manager's verbs that wire or unwire: each runs under the manager's lock.
_SERIAL_VERBS = frozenset({"open_cabinet", "close_cabinet", "conclude"})

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
        "canvas",
        "status",
        "cabinet_peers",
        "max_cabinets",
        "canvas_id",
    }
)
#: Keys a `setup=` may NOT carry: wiring the manager writes itself.
_WIRING = _NOT_COPIED - {"system_prompt", "pre_system_prompt", "system_prompt_file"}

_SLUG = re.compile(r"[^a-z0-9]+")
#: One lock per manager, keyed by the manager's agent: a dead manager's lock goes with it.
_LOCKS: weakref.WeakKeyDictionary = weakref.WeakKeyDictionary()
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


def _lock_of(manager) -> asyncio.Lock:
    """The manager's one lock (`_SERIAL_VERBS`): created on first use, per manager."""
    lock = _LOCKS.get(manager)
    if lock is None:
        lock = _LOCKS[manager] = asyncio.Lock()
    return lock


async def _unmake(kernel, cab_id: str, canvas_id: str | None) -> None:
    """Delete a half-made cabinet and its canvas (the canvas's subtree goes with it)."""
    if cab_id and kernel.get(cab_id) is not None:
        await _tree(kernel, {"type": "delete_agent", "id": cab_id})
    if canvas_id is not None and kernel.get(canvas_id) is not None:
        await _tree(kernel, {"type": "delete_agent", "id": canvas_id})


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


# the `kernel_state` handler: a canvas's page loader is one of its children (the host's `<canvas>_loader`)
LOADER_HANDLER = "kernel_state.tools"


def _is_loader(kernel, node: str, child: str) -> bool:
    """Whether `child` is `node`'s page loader: the page's own records, not equipment and
    never a result. Pruning keeps it, so a kept result's canvas stays openable."""
    if child == f"{node}_loader":
        return True
    return (kernel.get(child) or {}).get("handler_module") == LOADER_HANDLER


def _canvas_items(kernel, canvas: str) -> list[str]:
    """Every agent on `canvas` at any depth (equipment and nested canvases), without the
    canvas itself and without any page loader."""
    out: list[str] = []

    def walk(node: str) -> None:
        for child in kernel.ctx.agents[node].child_ids():
            if _is_loader(kernel, node, child):
                continue
            out.append(child)
            walk(child)

    walk(canvas)
    return out


def _equipment(kernel, cab: dict) -> list[str]:
    """The ids placed on the cabinet's canvas: its children (equipment, nested canvases)."""
    canvas = cab.get("canvas", "")
    agent = kernel.ctx.agents.get(canvas)
    if agent is None:
        return []
    return [c for c in agent.child_ids() if not _is_loader(kernel, canvas, c)]


def _is_ai(rec: dict) -> bool:
    return isinstance(rec.get("memory_store_id"), str) and isinstance(
        rec.get("handler_module"), str
    )


def _instructions(cab: dict, peers: list[dict], master: str | None) -> str:
    """The worker's post-system block: its role, its directory and how to work. It
    rides AFTER the prompt in force (`post_system_prompt`), so the backend's own prompt
    is never replaced."""
    worker = cab["worker"]
    canvas = cab["canvas"]
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
        f"- Equipment: your cabinet is the canvas `{canvas}`. Build on it with `create` "
        f'(`send {{target: "{canvas}", payload: {{type: "create", record: {{handler_module: "..."}}}}}}`) '
        "and show views with `place`. Those are the only verbs you hold on it.",
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
        text = _instructions(cab, peers, cab.get("master"))
        err = await _wire(
            kernel, cab["worker"], cabinet_peers=peers, post_system_prompt=text
        )
        if err:
            return err
    return None


# ─── verbs: the manager ──────────────────────────────────────────


async def _open(manager: str, payload: dict, kernel):
    """args: task:str (req), name:str?, backend:str? (an existing AI agent id; default the master), setup:dict? (fields over the backend's copy; a new backend is flagged). Opens one cabinet: a canvas of its own, a child of the canvas that spawns it (the manager's `canvas_id`), with its worker AI, wired to every live worker and to the master. The worker is NOT started — send it its first message (the task). Returns {id, canvas, worker, master, backend, new_backend, peers}."""
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
    canvas_id = kernel.get(kernel.id).get("canvas_id")
    if not isinstance(canvas_id, str) or not canvas_id:
        return refusal(
            MISSING_ARG,
            "open_cabinet: the manager names no canvas — its cabinets are canvases made by the canvas that spawns them",
            arg="canvas_id",
            hint=f"name the spawning canvas on the manager: update_agent {{id: {kernel.id}, canvas_id: <canvas node>}}",
        )
    if kernel.get(canvas_id) is None:
        return refusal(
            UNKNOWN_AGENT,
            f"open_cabinet: canvas `{canvas_id}` does not exist",
            id=canvas_id,
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
    if kernel.get(cab_id) is not None:
        return refusal(
            "exists",
            f"open_cabinet: `{cab_id}` already exists — name the cabinet another name",
            id=cab_id,
        )
    # the manager may ask the spawning canvas for a child canvas, and nothing else
    for err in (
        await _wire(kernel, kernel.id, can_call={canvas_id: [NEW_CANVAS_VERB]}),
        await _wire(kernel, canvas_id, callable_by={kernel.id: [NEW_CANVAS_VERB]}),
    ):
        if err:
            return {"error": f"open_cabinet: could not grant the canvas: {err}"}
    # THE CABINET'S CANVAS: the spawning canvas makes it, a child of its own
    made_canvas = await _call(
        kernel, canvas_id, {"type": NEW_CANVAS_VERB, "name": cab_id}
    )
    if made_canvas.get("error"):
        return {
            **made_canvas,
            "error": f"open_cabinet: the canvas `{canvas_id}` did not make the cabinet's canvas: {made_canvas['error']}",
        }
    cab_canvas = made_canvas.get("id")
    if not isinstance(cab_canvas, str) or not cab_canvas:
        return {
            "error": f"open_cabinet: the canvas `{canvas_id}` answered no id for the cabinet's canvas"
        }
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
            "canvas": cab_canvas,
            "status": dict(_EMPTY_STATUS),
            "display_name": name.strip() if name else task.strip()[:60],
            "description": f"Cabinet: {task.strip()[:200]}",
            # the live status cell on the main canvas: it opens the cabinet's canvas
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
        await _unmake(kernel, "", cab_canvas)
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
            # itself, its cabinet's `status` up, and its cabinet canvas (`create` and
            # `place` on it, nothing else). Nothing on the root.
            "can_call": {
                "$self": "*",
                "$parent": [STATUS_VERB],
                cab_canvas: CANVAS_VERBS,
            },
            "callable_by": {"$self": "*", kernel.id: "*", master: "*"},
            **worker_rec,
        },
    )
    if made_worker.get("error"):
        await _unmake(kernel, cab_id, cab_canvas)
        return made_worker

    # Wire the NEW worker to the master, the stores, its canvas and every live worker,
    # and every live worker to it. Every step runs (no short-circuit); the first failure
    # deletes the cabinet it half-made.
    steps = [
        # the worker places on its cabinet's canvas, and only there
        _wire(kernel, cab_canvas, callable_by={worker_id: CANVAS_VERBS}),
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
        await _unmake(kernel, cab_id, cab_canvas)
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
        "canvas": cab_canvas,
        "worker": worker_id,
        "master": master,
        "backend": backend_id,
        "new_backend": new_backend,
        "peers": peers,
        "next": (
            f'send `{worker_id}` its first message (the task) with {{type: "send", text: ...}}; '
            f'show its status cell on the spawning canvas with that canvas view\'s place {{backend_id: "{cab_id}", x, y}}'
        ),
    }
    if new_backend:
        out["warning"] = _NEW_BACKEND_WARNING
    return (
        out
        if cab_rec is not None
        else {"error": "open_cabinet: the cabinet vanished while wiring"}
    )


async def _prune(kernel, node: str, keep: frozenset) -> tuple[list[str], str | None]:
    """Delete, under `node`, every agent that is not kept and holds no kept agent. A kept
    agent stays with its path up to `node`, and a page loader is never deleted (it is the
    canvas's own records). Returns (deleted ids, the error or None)."""
    deleted: list[str] = []
    for child in list(kernel.ctx.agents[node].child_ids()):
        if child in keep or _is_loader(kernel, node, child):
            continue
        sub = _subtree(kernel, child)
        if any(k in keep for k in sub):
            more, err = await _prune(kernel, child, keep)
            deleted += more
            if err:
                return deleted, err
            continue
        res = await _tree(kernel, {"type": "delete_agent", "id": child})
        if not res.get("deleted"):
            return deleted, res.get("error") or f"`{child}` was not deleted"
        deleted += sub
    return deleted, None


async def _close(manager: str, payload: dict, kernel, keep: frozenset = frozenset()):
    """args: id:str (req) — a cabinet of this manager. Deletes the cabinet with its worker, and its canvas with ALL the equipment on it, then rewrites the live workers' directories. `keep` (from `conclude`) names results on this canvas to keep: the canvas stays with them, and everything else on it goes. Returns {closed, deleted}."""
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
    canvas = rec.get("canvas")
    has_canvas = canvas is not None and kernel.get(canvas) is not None
    kept_here = (
        [k for k in keep if k in _canvas_items(kernel, canvas)] if has_canvas else []
    )
    gone = _subtree(kernel, cab_id)
    res = await _tree(kernel, {"type": "delete_agent", "id": cab_id})
    if not res.get("deleted"):
        return {
            **res,
            "error": res.get("error") or f"close_cabinet: `{cab_id}` was not deleted",
        }
    if has_canvas and kept_here:
        pruned, err = await _prune(kernel, canvas, keep)
        gone += pruned
        if err:
            return {
                "error": f"close_cabinet: `{cab_id}` was deleted, but its canvas `{canvas}` was not pruned: {err}",
                "deleted": gone,
            }
    elif has_canvas:
        gone += _subtree(kernel, canvas)
        res = await _tree(kernel, {"type": "delete_agent", "id": canvas})
        if not res.get("deleted"):
            return {
                **res,
                "error": res.get("error")
                or f"close_cabinet: `{cab_id}` was deleted, but its canvas `{canvas}` was not",
                "deleted": gone,
            }
    if err := await _refresh_directories(kernel):
        return {
            "error": f"close_cabinet: deleted `{cab_id}`, but a worker's directory did not update: {err}",
            "deleted": gone,
        }
    return {"closed": cab_id, "deleted": gone, "kept": sorted(kept_here)}


async def _cabinets_verb(manager: str, payload: dict, kernel):
    """No args. Every live cabinet: its task, worker, canvas, live status and equipment. The master's monitor."""
    rows = [
        {
            "id": c["id"],
            "name": c.get("display_name"),
            "task": c["task"],
            "worker": c["worker"],
            "canvas": c.get("canvas"),
            "status": c.get("status"),
            "equipment": _equipment(kernel, c),
        }
        for c in _cabinets(kernel)
    ]
    return {
        "cabinets": rows,
        "count": len(rows),
        "origin": f"{manager} cabinets — re-send; changes as cabinets open, report and close",
    }


async def _conclude(manager: str, payload: dict, kernel):
    """args: keep:[str]? — the ids to KEEP: live cabinets of this manager (kept whole), or results on a cabinet's canvas (the canvas and the path to them stay). Every other cabinet is closed. A keep id that is neither is refused before anything is deleted. The manager stays. Returns {concluded, kept, deleted}."""
    keep = payload.get("keep", [])
    if not isinstance(keep, list) or not all(isinstance(k, str) and k for k in keep):
        return bad("conclude", "keep", "a list of cabinet or result ids")
    live = _cabinets(kernel)
    cab_ids = {c["id"] for c in live}
    results = {
        e
        for c in live
        if c.get("canvas") in kernel.ctx.agents
        for e in _canvas_items(kernel, c["canvas"])
    }
    unknown = sorted(set(keep) - cab_ids - results)
    if unknown:
        return {
            "error": f"conclude: keep names no live cabinet or result of this manager: {unknown}",
            "reason": "unknown_keep",
            "unknown": unknown,
        }
    kept = frozenset(keep)
    concluded, deleted = [], []
    for cab in live:
        if cab["id"] in kept:
            continue
        res = await _close(manager, {"id": cab["id"]}, kernel, keep=kept)
        if res.get("error"):
            return {
                **res,
                "concluded": concluded,
                "kept": sorted(keep),
                "deleted": deleted,
            }
        concluded.append(cab["id"])
        deleted.extend(res.get("deleted", []))
    return {
        "concluded": concluded,
        "kept": sorted(keep),
        "deleted": deleted,
        "count": len(concluded),
    }


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
    """No args. The cabinet's panel: its task, worker, canvas, live status and equipment."""
    rec = kernel.get(cab_id) or {}
    return {
        "id": cab_id,
        "task": rec.get("task"),
        "worker": rec.get("worker"),
        "canvas": rec.get("canvas"),
        "status": rec.get("status"),
        "equipment": _equipment(kernel, rec),
    }


# ─── reflect and dispatch ────────────────────────────────────────


async def _reflect(id: str, payload: dict, kernel):
    """No args. The agent's role and what it holds."""
    rec = kernel.get(id) or {}
    if rec.get(ROLE) == MANAGER:
        return {
            "sentence": "A cabinet manager: opens cabinets (a canvas of its own, a child of the spawning canvas, with one worker AI each), monitors them, closes them, concludes.",
            "role": MANAGER,
            "canvas_id": rec.get("canvas_id"),
            "max_cabinets": rec.get("max_cabinets"),
            "open": len(_cabinets(kernel)),
        }
    return {
        "sentence": f"A cabinet: the status cell of one worker AI and its canvas — {rec.get('task', '')}".rstrip(),
        "role": CABINET,
        "task": rec.get("task"),
        "worker": rec.get("worker"),
        "canvas": rec.get("canvas"),
        "status": rec.get("status"),
    }


MANAGER_VERBS = {
    "reflect": _reflect,
    "open_cabinet": _open,
    "close_cabinet": _close,
    "cabinets": _cabinets_verb,
    "conclude": _conclude,
}
CABINET_VERBS = {
    "reflect": _reflect,
    "status": _status,
    "view": _view,
}


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
    if role == MANAGER and t in _SERIAL_VERBS:
        async with _lock_of(kernel):
            return await fn(id, payload, kernel)
    return await fn(id, payload, kernel)
