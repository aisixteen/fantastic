"""IMPORT A BLUEPRINT SUBTREE: the other half of `export` — a blueprint folder
(`blueprint.yaml` + owned files) becomes a live team under any parent.

Two layers:
- `plan` is PURE: the blueprint text, the folder's files and what the kernel holds
  in; the agents to create (fresh ids, grants re-wired, ports bound), the grant
  edges to add on the bound agents, and what the team will reach out; or the
  refusal. Nothing is created before it is whole.
- `import_subtree` is the verb on `user`: it reads the folder, plans, and — only
  with `approve: true` — creates the agents depth first and writes the other end of
  every bound edge; a failure deletes what it made.

The rules, in the order they are checked:
- one top entry under `agents:`; nesting is the parent (`parent_id` is refused);
- NO SECRETS: a key value (`SECRET_KEYS`, at any depth), row text (`rows`,
  `history`), an `sk-` token in any file, or an absolute path;
- PORTS: every `@...` name used must be declared under `ports:`; every declared
  port must be bound (`bind: {port: agent id}`) to an existing agent of the port's
  kind; `@path:*` ports are not bindable yet;
- FRESH IDS: every id gets the same suffix `_<k>`, the smallest `k` for which no new
  id is taken (a second import under the same parent gets `_2`); `@root` becomes
  the root's id; `$self`, `$parent`, `$subtree`, `*` and page grants stay;
- CODE: a handler that is not a registered bundle, or a file other than
  `blueprint.yaml` and `<agent id>/directive.md`, is code — refused unless
  `allow_code: true`.

Port of the Rust `fantastic-user` `import.rs`: same names, same checks, same text.
"""

from __future__ import annotations

import copy
import json
import os
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable

from kernel_state import yaml_io
from kernel._refusal import (
    BAD_ARG,
    BAD_BINDING,
    CODE_NOT_ALLOWED,
    FAILED,
    NOT_FOUND,
    SECRET_FOUND,
    UNBOUND_PORTS,
    guard_agent_id,
    refusal,
)

from kernel_state.export import (
    BLUEPRINT_FILE,
    GRANT_FIELDS,
    REF_FIELDS,
    ROOT_PORT,
    SECRET_KEYS,
    _has_dotdot,
    has_key_token,
    is_absolute,
    port_kind,
)

_VERB = "user.import_subtree"

#: The largest file the import reads from a blueprint folder.
MAX_FILE = 1 << 20

#: The most files a blueprint folder may hold.
MAX_FILES = 512


class ImportError(Exception):  # noqa: A001 - the Rust name; shadows the builtin here only
    """Why a plan was refused: a `reason` to branch on and a message naming the fix."""

    def __init__(self, reason: str, message: str):
        super().__init__(message)
        self.reason = reason
        self.message = f"{_VERB}: {message}"


@dataclass
class Planned:
    """One agent to create."""

    old_id: str  # its id in the blueprint
    new_id: str  # its fresh id
    parent: str  # the import's parent for the top, a new id below
    handler_module: str
    meta: dict  # its record fields, ids re-wired


@dataclass
class Edge:
    """The other end of a bound edge: `agent`'s `field` gains `key` (a new id)."""

    agent: str  # the existing agent (a bound port, or the root)
    field: str  # `callable_by` (the new agent calls it) or `can_call`
    key: str  # the new agent's id
    grant: Any  # the grant, as the blueprint wrote it


@dataclass
class Plan:
    """What `plan` makes."""

    agents: list[Planned]  # depth first (a parent before its children)
    ids: dict[str, str]  # blueprint id -> fresh id
    edges: list[Edge]  # the grants to add on existing agents
    reaches: list[dict]  # per new agent: what it can call, what can call it, its ports
    code: list[str]  # the code it carries (handlers and files), one line each
    ports: dict[str, str]  # port -> the agent it is bound to


@dataclass
class World:
    """What the kernel holds, as `plan` asks it."""

    parent_id: str  # where the top agent goes
    root_id: str  # what `@root` becomes
    bind: dict  # {port: agent id}
    allow_code: bool
    agent: Callable[[str], "str | None"]  # an agent's handler ("" if none); None = absent
    known_bundle: Callable[[str], bool]  # whether a handler is a registered bundle


@dataclass
class _Entry:
    id: str
    parent: str | None
    hm: str
    meta: dict = field(default_factory=dict)


def _walk(v, parent: str | None, out: list[_Entry]) -> None:
    if not isinstance(v, dict):
        raise ImportError(BAD_ARG, "every entry under `agents:` must be a map")
    id = v.get("id")
    if not (isinstance(id, str) and id):
        raise ImportError(BAD_ARG, "every entry under `agents:` needs an id")
    if "parent_id" in v:
        raise ImportError(
            BAD_ARG,
            f"agent {id}: parent_id is not written in a blueprint — nesting under `agents:` is the parent",
        )
    hm = v.get("handler_module")
    if not (isinstance(hm, str) and hm):
        raise ImportError(BAD_ARG, f"agent {id}: handler_module is required")
    if guard_agent_id("import_subtree", id) is not None:
        raise ImportError(BAD_ARG, f"agent {id!r}: not a safe agent id")
    if any(e.id == id for e in out):
        raise ImportError(BAD_ARG, f"agent {id} appears twice")
    meta = {
        k: copy.deepcopy(x)
        for k, x in v.items()
        if k not in ("id", "handler_module", "agents")
    }
    out.append(_Entry(id, parent, hm, meta))
    kids = v.get("agents")
    if kids is None:
        return
    if not isinstance(kids, list):
        raise ImportError(BAD_ARG, f"agent {id}: `agents:` must be a list")
    for k in kids:
        _walk(k, id, out)


def _secrets(v, id: str, fld: str, hits: list[str]) -> None:
    """Every secret under `v`, by key or path (never the value)."""
    if isinstance(v, str):
        if is_absolute(v):
            hits.append(f"agent {id} field {fld}: an absolute path")
    elif isinstance(v, list):
        for x in v:
            _secrets(x, id, fld, hits)
    elif isinstance(v, dict):
        for k, x in v.items():
            if k in SECRET_KEYS:
                hits.append(f"agent {id}: a {k} key")
            if k in ("rows", "history"):
                hits.append(f"agent {id}: row text ({k})")
            _secrets(x, id, fld, hits)


def _path_ports(v, out: set[str]) -> None:
    """Every `@path:` string under `v`."""
    if isinstance(v, str):
        if v.startswith("@path:"):
            out.add(v)
    elif isinstance(v, list):
        for x in v:
            _path_ports(x, out)
    elif isinstance(v, dict):
        for x in v.values():
            _path_ports(x, out)


def _handler_kind(hm: str) -> str:
    return port_kind(hm, "")


def _is_kept(name: str) -> bool:
    """A name that stays as it is: a baseline token, a wildcard, a page grant."""
    return name == "*" or name.startswith("$") or "|" in name


def _names_in(meta: dict) -> list[tuple[str, str]]:
    """Every id-holding place of a record: `(field, the name there)`."""
    out: list[tuple[str, str]] = []
    for g in GRANT_FIELDS:
        o = meta.get(g)
        if isinstance(o, dict):
            out.extend((g, str(k)) for k in o)
    for f in REF_FIELDS:
        t = meta.get(f)
        if isinstance(t, str):
            out.append((f, t))
    c = meta.get("credentials")
    if isinstance(c, dict) and isinstance(c.get("store"), str):
        out.append(("credentials.store", c["store"]))
    return out


def plan(yaml_text: str, files: dict[str, str], w: World) -> Plan:
    """PLAN the import of `yaml_text` (+ the folder's other `files`: path -> text)
    under `w.parent_id`. Raises `ImportError` with the first rule the blueprint
    breaks, in the order of the module doc."""
    # a. THE SHAPE
    try:
        doc = yaml_io.load(yaml_text)
    except Exception as e:  # noqa: BLE001 - any parse failure is a bad blueprint
        raise ImportError(BAD_ARG, f"{BLUEPRINT_FILE} is not yaml ({e})") from e
    tops = doc.get("agents") if isinstance(doc, dict) else None
    if not isinstance(tops, list):
        raise ImportError(
            BAD_ARG, f"{BLUEPRINT_FILE} needs `agents:` with one top entry"
        )
    if len(tops) != 1:
        raise ImportError(
            BAD_ARG,
            f"{BLUEPRINT_FILE} holds {len(tops)} top entries under `agents:`; a subtree has exactly one",
        )
    entries: list[_Entry] = []
    _walk(tops[0], None, entries)
    declared: dict[str, str] = {}
    ports_doc = doc.get("ports")
    if ports_doc is None:
        pass
    elif isinstance(ports_doc, dict):
        for name, p in ports_doc.items():
            kind = p.get("kind") if isinstance(p, dict) else None
            if not isinstance(kind, str):
                raise ImportError(BAD_ARG, f"port {name}: kind is required")
            declared[str(name)] = kind
    else:
        raise ImportError(BAD_ARG, "`ports:` must be a map")

    # b. NO SECRETS
    hits: list[str] = []
    for e in entries:
        for k, v in e.meta.items():
            if k in SECRET_KEYS:
                hits.append(f"agent {e.id}: a {k} key")
            if k in ("rows", "history"):
                hits.append(f"agent {e.id}: row text ({k})")
            _secrets(v, e.id, k, hits)
    if has_key_token(yaml_text):
        hits.append(f"{BLUEPRINT_FILE}: an sk- key token")
    for name, text in files.items():
        if has_key_token(text):
            hits.append(f"{name}: an sk- key token")
    if hits:
        raise ImportError(
            SECRET_FOUND,
            f"the blueprint carries secrets — {'; '.join(sorted(set(hits)))}; "
            "remove it and export again, or bind a credentials port",
        )

    # c. PORTS
    inside = {e.id for e in entries}
    paths: set[str] = set()
    for e in entries:
        for v in e.meta.values():
            _path_ports(v, paths)
        for fld, name in _names_in(e.meta):
            if name.startswith("@") and name != ROOT_PORT and name not in declared:
                raise ImportError(
                    BAD_ARG,
                    f"agent {e.id} field {fld} names port {name}, which `ports:` does not declare",
                )
    paths.update(n for n, k in declared.items() if k == "path" or n.startswith("@path:"))
    if paths:
        raise ImportError(
            UNBOUND_PORTS,
            f"path ports are not bindable yet: {', '.join(sorted(paths))} — "
            "set the field to a relative path and export again",
        )
    names = sorted(declared)
    missing = [f"{n} ({declared[n]})" for n in names if n not in w.bind]
    if missing:
        example = [
            f'{json.dumps(n)}: "<agent id>"' for n in names if n not in w.bind
        ]
        raise ImportError(
            UNBOUND_PORTS,
            f"unbound required ports: {', '.join(missing)}: pass bind {{{', '.join(example)}}}",
        )
    ports: dict[str, str] = {}
    for name, v in w.bind.items():
        kind = declared.get(name)
        if kind is None:
            raise ImportError(
                BAD_BINDING,
                f"bind names {name}, which is not a port of this blueprint (ports: {', '.join(names)})",
            )
        if not (isinstance(v, str) and v):
            raise ImportError(BAD_BINDING, f"bind {name}: an agent id (str)")
        hm = w.agent(v)
        if hm is None:
            raise ImportError(BAD_BINDING, f"bind {name}: no agent {v!r}")
        found = _handler_kind(hm)
        if kind != "connector" and found != kind:
            raise ImportError(
                BAD_BINDING,
                f"bind {name}: the port wants a {kind} agent; {v} is {found} ({hm})",
            )
        ports[name] = v

    # d. FRESH IDS: one suffix for all
    k = 1
    while True:
        ids = {e.id: f"{e.id}_{k}" for e in entries}
        if all(
            w.agent(n) is None
            and n != w.root_id
            and guard_agent_id("import_subtree", n) is None
            for n in ids.values()
        ):
            break
        k += 1
        if k > 100_000:
            raise ImportError(BAD_ARG, "no free id suffix")
    ids = dict(sorted(ids.items()))

    # e. REWRITE: inside id -> new id, @root -> the root, port -> bound agent
    def resolve(id: str, fld: str, name: str) -> str:
        if name in ids:
            return ids[name]
        if name == ROOT_PORT:
            return w.root_id
        if name in ports:
            return ports[name]
        if _is_kept(name):
            return name
        raise ImportError(
            BAD_ARG,
            f"agent {id} field {fld} names {name}, which is neither in the blueprint nor a port",
        )

    def outside(name: str) -> bool:
        return name not in inside and not _is_kept(name)

    agents: list[Planned] = []
    edges: list[Edge] = []
    reaches: list[dict] = []
    for e in entries:
        new_id = ids[e.id]
        meta = copy.deepcopy(e.meta)
        reach: dict = {}
        used: dict[str, str] = {}
        for g in GRANT_FIELDS:
            o = meta.get(g)
            if not isinstance(o, dict):
                continue
            out: dict = {}
            for key, grant in o.items():
                key = str(key)
                to = resolve(e.id, g, key)
                # f. BOTH ENDS: the bound agent (or the root) grants back
                if outside(key):
                    edges.append(
                        Edge(
                            to,
                            "callable_by" if g == "can_call" else "can_call",
                            new_id,
                            copy.deepcopy(grant),
                        )
                    )
                    used[key] = to
                out[to] = grant
            reach[g] = sorted(out)
            meta[g] = out
        for f in REF_FIELDS:
            t = meta.get(f)
            if isinstance(t, str):
                to = resolve(e.id, f, t)
                if outside(t):
                    used[t] = to
                meta[f] = to
        c = meta.get("credentials")
        if isinstance(c, dict) and isinstance(c.get("store"), str):
            t = c["store"]
            to = resolve(e.id, "credentials.store", t)
            if outside(t):
                used[t] = to
            c["store"] = to
        reach["id"] = new_id
        reach["from"] = e.id
        reach["ports"] = dict(sorted(used.items()))
        reaches.append(reach)
        agents.append(
            Planned(
                e.id,
                new_id,
                w.parent_id if e.parent is None else ids[e.parent],
                e.hm,
                meta,
            )
        )

    # g. CODE
    code = [
        f"agent {e.id}: handler {e.hm} is not a built-in bundle"
        for e in entries
        if not w.known_bundle(e.hm)
    ]
    for name in files:
        owned = name.endswith("/directive.md") and name[: -len("/directive.md")] in inside
        if name != BLUEPRINT_FILE and not owned:
            code.append(f"file {name}")
    if code and not w.allow_code:
        raise ImportError(
            CODE_NOT_ALLOWED,
            f"the blueprint carries code — {'; '.join(code)}; pass allow_code: true to import code",
        )
    return Plan(agents, ids, edges, reaches, code, ports)


def read_folder(d: Path) -> dict[str, str]:
    """The folder's files, `/`-separated relative path -> text (lossy). No symlinks,
    at most `MAX_FILES` files of `MAX_FILE` bytes each. Raises `ValueError`."""
    out: dict[str, str] = {}
    stack = [d]
    while stack:
        cur = stack.pop()
        try:
            entries = list(os.scandir(cur))
        except OSError as e:
            raise ValueError(str(e)) from e
        for e in entries:
            p = Path(e.path)
            rel = p.relative_to(d).as_posix()
            st = os.lstat(p)
            if e.is_symlink():
                raise ValueError(
                    f"{rel} is a symlink; a blueprint folder holds plain files"
                )
            if e.is_dir(follow_symlinks=False):
                stack.append(p)
                continue
            if st.st_size > MAX_FILE:
                raise ValueError(f"{rel} is larger than {MAX_FILE} bytes")
            if len(out) >= MAX_FILES:
                raise ValueError(f"the folder holds more than {MAX_FILES} files")
            try:
                out[rel] = p.read_bytes().decode("utf-8", errors="replace")
            except OSError as e2:
                raise ValueError(str(e2)) from e2
    return out


def _literal_true(p: dict, k: str) -> bool:
    """Only the literal `true` counts."""
    return p.get(k) is True


async def _roll_back(ctx, top: str | None, edges: list[Edge]) -> None:
    """Delete what an import made (the top agent, cascading) and take back the
    edges it wrote."""
    for e in edges:

        def undo(cc, cb, e=e):
            m = cc if e.field == "can_call" else cb
            changed = m.pop(e.key, None) is not None
            return (e.field == "can_call" and changed, e.field == "callable_by" and changed)

        ctx.edit_grants(e.agent, undo)
    if top is not None and top in ctx.agents:
        await ctx.delete(top)


async def import_subtree(agent, p: dict) -> dict:
    """The verb: plan the import of the blueprint folder at `path` (relative to the
    running dir) under `parent_id`; without `approve: true` the reply is the preview
    and nothing is created."""
    path, parent_id = p.get("path"), p.get("parent_id")
    if not (isinstance(path, str) and path and isinstance(parent_id, str) and parent_id):
        return refusal(BAD_ARG, f"{_VERB}: path (str) + parent_id (str) required")
    bind = p.get("bind")
    if bind is None:
        bind = {}
    elif not isinstance(bind, dict):
        return refusal(
            BAD_ARG, f'{_VERB}: bind is a map {{"@port": "<agent id>"}}'
        )
    if _has_dotdot(path):
        return refusal(BAD_ARG, f"{_VERB}: path may not hold `..`")
    ctx = agent.ctx
    root = ctx.root
    if root is None:
        return refusal(FAILED, f"{_VERB}: the kernel has no root")
    if parent_id not in ctx.agents:
        return refusal(
            NOT_FOUND, f"{_VERB}: no agent {parent_id!r} to import under"
        )
    d = Path(path)
    if not d.is_absolute():
        d = ctx.workdir / d
    if not (d / BLUEPRINT_FILE).is_file():
        return refusal(NOT_FOUND, f"{_VERB}: {path} holds no {BLUEPRINT_FILE}")
    try:
        files = read_folder(d)
    except ValueError as e:
        return refusal(BAD_ARG, f"{_VERB}: {e}")
    yaml_text = files.pop(BLUEPRINT_FILE, "")

    def lookup(id: str) -> "str | None":
        a = ctx.agents.get(id)
        return None if a is None else (a.handler_module or "")

    def known(hm: str) -> bool:
        from kernel._bundles import import_handler

        try:
            import_handler(hm)
        except Exception:  # noqa: BLE001 - any failure: not a registered bundle
            return False
        return True

    world = World(
        parent_id, root.id, bind, _literal_true(p, "allow_code"), lookup, known
    )
    try:
        pl = plan(yaml_text, files, world)
    except ImportError as e:
        return refusal(e.reason, e.message)

    # THE PREVIEW: what it will reach and what code it carries; nothing made
    if not _literal_true(p, "approve"):
        return {
            "approval_required": True,
            "agents": len(pl.agents),
            "new_ids": pl.ids,
            "reaches": pl.reaches,
            "ports": pl.ports,
            "code": pl.code,
            "hint": "nothing was created: call again with approve: true to import",
        }

    # APPROVED: depth first, then the other end of every bound edge
    top = pl.agents[0].new_id if pl.agents else None
    for a in pl.agents:
        payload = dict(a.meta)
        payload.update(
            {
                "type": "create_agent",
                "id": a.new_id,
                "handler_module": a.handler_module,
                "parent_id": a.parent,
            }
        )
        r = await root._verb_create_agent(payload)
        if isinstance(r, dict) and "error" in r:
            await _roll_back(ctx, top, [])
            err = r["error"]
            return refusal(
                FAILED,
                f"{_VERB}: agent {a.old_id} ({a.new_id}) was not created — "
                f"{err if isinstance(err, str) else json.dumps(err)}; nothing was kept",
            )
    done: list[Edge] = []
    for e in pl.edges:

        def add(cc, cb, e=e):
            m = cc if e.field == "can_call" else cb
            m[e.key] = e.grant
            return (e.field == "can_call", e.field == "callable_by")

        _, why = ctx.edit_grants(e.agent, add)
        if why is not None:
            await _roll_back(ctx, top, done)
            return refusal(
                FAILED,
                f"{_VERB}: the grant {e.field}[{e.key}] on {e.agent} was refused — "
                f"{why}; nothing was kept",
            )
        done.append(e)
    return {
        "imported": len(pl.agents),
        "ids": pl.ids,
        "parent_id": parent_id,
        "ports": pl.ports,
    }
