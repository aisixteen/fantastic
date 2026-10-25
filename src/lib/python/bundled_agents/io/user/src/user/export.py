"""EXPORT A SUBTREE AS A BLUEPRINT: an agent and every agent under it, written in
the shape of a blueprint — nesting under `agents:` is the parent (no `parent_id`),
grants are written by id on both ends, `@root` stands for the root.

Two layers:
- `build` is PURE: records in, blueprint text, owned files and ports out. It takes
  records only, so row text (conversation history) is never read.
- `export_subtree` is the verb on `user`: it collects the kernel's records, writes
  the folder (temp dir, then rename) and scans it for secrets.

Port of the Rust `fantastic-user` `export.rs`: same names, same checks, same text.
"""

from __future__ import annotations

import copy
import os
import re
import shutil
import time
from dataclasses import dataclass, field
from pathlib import Path, PurePath

from kernel_state import yaml_io
from kernel._refusal import (
    BAD_ARG,
    EXISTS,
    FAILED,
    NOT_FOUND,
    SECRET_FOUND,
    refusal,
)

BLUEPRINT_FILE = "blueprint.yaml"
ROOT_PORT = "@root"

# Runtime state a record carries that a blueprint never does.
RUNTIME_FIELDS = (
    "parent_id",
    "guid",
    "pid",
    "version",
    "rows",
    "history",
    "cache",
    "usage",
    "created_at",
    "updated_at",
    "ts",
    "last_error",
    "error",
)

# Keys whose value is a secret: dropped wherever they appear.
SECRET_KEYS = ("api_key", "token", "secret", "password")

# Record fields that hold one agent id.
REF_FIELDS = ("memory_store_id", "file_bridge_id", "backend_id")

# Record fields whose KEYS are agent ids.
GRANT_FIELDS = ("can_call", "callable_by")

# The owned directive file.
DIRECTIVE_FIELD = "system_prompt_file"

_VERB = "user.export_subtree"


@dataclass
class Port:
    """One port: an outside thing the subtree reaches."""

    name: str  # `@memory`, `@connector:<id>`, `@path:<field>`...
    kind: str  # memory, credentials, exec, files, model, connector, path
    target: str  # an outside agent id; empty for a path (never kept)
    why: list[str] = field(default_factory=list)
    used_by: list[str] = field(default_factory=list)


@dataclass
class Exported:
    """What `build` makes."""

    yaml: str
    files: list[tuple[str, str]]  # (path relative to the folder, text)
    ports: list[Port]  # sorted by name
    notes: list[str]
    agents: int


def is_runtime(key: str) -> bool:
    """Whether a key is runtime state."""
    return (
        key in RUNTIME_FIELDS
        or key == "agents"
        or key.startswith("last_")
        or key.endswith("_error")
        or key.endswith("_at")
        or key.endswith("_ts")
    )


def drop_secrets(v):
    """Drop every secret key at any depth, in place."""
    if isinstance(v, dict):
        for k in [k for k in v if k in SECRET_KEYS]:
            del v[k]
        for x in v.values():
            drop_secrets(x)
    elif isinstance(v, list):
        for x in v:
            drop_secrets(x)


def strip_runtime(meta: dict) -> dict:
    """A record's meta minus runtime state and secret values."""
    out = {
        k: copy.deepcopy(v)
        for k, v in meta.items()
        if not is_runtime(k) and k not in SECRET_KEYS
    }
    for v in out.values():
        drop_secrets(v)
    return out


def is_absolute(s: str) -> bool:
    """Whether a string is an absolute path (`/...`, `\\\\...` or `C:\\...`)."""
    return (
        s.startswith("/")
        or s.startswith("\\\\")
        or (
            len(s) >= 3
            and s[0].isascii()
            and s[0].isalpha()
            and s[1] == ":"
            and s[2] in "\\/"
        )
    )


def relative_to(s: str, folder: str) -> str | None:
    """`s` relative to `folder` when it lies under it."""
    f = folder.rstrip("/\\")
    if not f:
        return None
    if s == f:
        return "."
    if not s.startswith(f):
        return None
    rest = s[len(f) :]
    if rest[:1] not in ("/", "\\"):
        return None
    rest = rest[1:]
    return rest or "."


def port_kind(handler_module: str | None, field: str) -> str:
    """The port kind for an outside agent, from its handler and the field that
    reaches it."""
    hm = handler_module or ""
    if hm == "credential_store.tools" or field == "credentials.store":
        return "credentials"
    if field == "memory_store_id" or hm.endswith("_store.tools"):
        return "memory"
    if hm == "exec_bridge.tools":
        return "exec"
    if hm == "file_bridge.tools":
        return "files"
    if hm.endswith("_backend.tools") or field == "backend_id":
        return "model"
    return "connector"


# One outside reach: ("agent", target, field) or ("path", field, value).
def _agent_reach(target: str, field: str) -> tuple:
    return ("agent", target, field)


def _path_reach(field: str, value: str) -> tuple:
    return ("path", field, value)


def _canon(x: tuple) -> tuple:
    """A reach as a port key: an agent by target, a path by (field, value). The
    tuples sort agents before paths, as the Rust enum does."""
    if x[0] == "agent":
        return (0, x[1], "")
    return (1, x[1], x[2])


class _Ctx:
    def __init__(self, inside: set, root_id: str, folder: str | None):
        self.inside, self.root_id, self.folder = inside, root_id, folder

    def id(self, id: str) -> str | None:
        """An id as the blueprint writes it; None = an outside agent (a port)."""
        if id in self.inside:
            return id
        if id == self.root_id:
            return ROOT_PORT
        if id == "*" or id.startswith(("$", "@")) or "|" in id:
            # the baseline grants (`$self`...), wildcards, ports and page grants stay
            return id
        return None


def _paths(v, field: str, ctx: _Ctx, port):
    """Rewrite every absolute path string under `v`; returns the new value."""
    if isinstance(v, str) and is_absolute(v):
        rel = relative_to(v, ctx.folder) if ctx.folder else None
        return rel if rel is not None else port(_path_reach(field, v))
    if isinstance(v, list):
        return [_paths(x, field, ctx, port) for x in v]
    if isinstance(v, dict):
        return {k: _paths(x, field, ctx, port) for k, x in v.items()}
    return v


def _rewrite(rec: dict, ctx: _Ctx, owned: str | None, port) -> dict:
    """One record as the blueprint writes it (meta only; id, handler and children
    are placed by the caller). `port(me, reach)` names an outside reach; `owned`
    holds the directive's export path when its text was read."""
    me = rec["id"]
    m = strip_runtime(rec["meta"])

    def name(field: str, id: str) -> str:
        n = ctx.id(id)
        return n if n is not None else port(me, _agent_reach(id, field))

    for g in GRANT_FIELDS:
        if isinstance(m.get(g), dict):
            m[g] = {name(g, k): v for k, v in m[g].items()}
    for f in REF_FIELDS:
        if isinstance(m.get(f), str):
            m[f] = name(f, m[f])
    cred = m.get("credentials")
    if isinstance(cred, dict) and isinstance(cred.get("store"), str):
        cred["store"] = name("credentials.store", cred["store"])
    if owned is not None:
        m[DIRECTIVE_FIELD] = owned
    for k in list(m):
        if k not in GRANT_FIELDS:
            m[k] = _paths(m[k], k, ctx, lambda r: port(me, r))
    return m


def _sorted(v):
    """Maps with their keys in sorted order, at any depth."""
    if isinstance(v, dict):
        return {k: _sorted(v[k]) for k in sorted(v)}
    if isinstance(v, list):
        return [_sorted(x) for x in v]
    return v


def _subtree(records: list[dict], top: str) -> list[dict]:
    """The records under `top`, depth first, children in record order."""
    out: list[dict] = []
    stack = [top]
    while stack:
        id = stack.pop()
        if any(r["id"] == id for r in out):
            continue
        r = next((r for r in records if r["id"] == id), None)
        if r is not None:
            out.append(r)
        kids = [x["id"] for x in records if x.get("parent_id") == id]
        stack.extend(reversed(kids))
    return out


def build(records, top: str, root_id: str, folder: str | None, read_file) -> Exported:
    """BUILD the export of `top` and everything under it. `folder` is the
    subtree's own folder (absolute paths under it become relative);
    `read_file(agent id, path)` reads an owned file, None when it cannot.

    Raises ValueError when `top` is the root, unknown, or sits in no tree."""
    if top == root_id:
        raise ValueError("the root is the kernel's own: export an agent under it")
    t = next((r for r in records if r["id"] == top), None)
    if t is None:
        raise ValueError(f'no agent "{top}"')
    if t.get("parent_id") is None:
        raise ValueError(f'"{top}" sits in no tree (no parent)')
    sub = _subtree(records, top)
    ctx = _Ctx({r["id"] for r in sub}, root_id, folder)

    def hm_of(id):
        r = next((r for r in records if r["id"] == id), None)
        return r.get("handler_module") if r else None

    # OWNED FILES: a relative, readable directive goes beside the blueprint
    files: list[tuple[str, str]] = []
    notes: list[str] = []
    owned: dict[str, str] = {}
    for r in sub:
        p = r["meta"].get(DIRECTIVE_FIELD)
        if isinstance(p, str):
            text = None if is_absolute(p) else read_file(r["id"], p)
            if text is not None:
                rel = f"{r['id']}/directive.md"
                files.append((rel, text))
                owned[r["id"]] = rel
            else:
                notes.append(
                    f"agent {r['id']}: {DIRECTIVE_FIELD} could not be read; "
                    "the field is kept without its text"
                )

    # PASS 1: every outside reach, so ports are numbered by outside id
    uses: list[tuple[str, tuple]] = []

    def collect(me, x):
        uses.append((me, x))
        return ""

    for r in sub:
        _rewrite(r, ctx, owned.get(r["id"]), collect)
    # the kind of each outside target: a memory_store_id use decides it
    kinds: dict[tuple, tuple[str, str]] = {}
    for _, x in uses:
        key = _canon(x)
        if x[0] == "agent":
            _, target, fld = x
            if any(
                y[0] == "agent" and y[1] == target and y[2] == "memory_store_id"
                for _, y in uses
            ):
                fld = "memory_store_id"
            kind, label = port_kind(hm_of(target), fld), target
        else:
            kind, label = "path", x[1]
        kinds.setdefault(key, (kind, label))
    # names: per kind (per field for paths), in outside-id order
    names: dict[tuple, str] = {}
    count: dict[str, int] = {}
    for key in sorted(kinds):
        kind, label = kinds[key]
        if kind == "connector":
            base = f"@connector:{label}"
        elif kind == "path":
            base = f"@path:{label}"
        else:
            base = f"@{kind}"
        count[base] = count.get(base, 0) + 1
        n = count[base]
        names[key] = base if n == 1 else f"{base}_{n}"

    # PASS 2: the records, ports named
    ports: dict[str, Port] = {}
    metas: dict[str, dict] = {}

    def name_port(me, x):
        key = _canon(x)
        n = names[key]
        kind = kinds[key][0]
        if x[0] == "agent":
            target, why = x[1], f"agent {me} field {x[2]} reaches {x[1]}"
        else:
            target = ""
            why = f"agent {me} field {x[1]} names a path outside the subtree"
        p = ports.get(n)
        if p is None:
            p = ports[n] = Port(n, kind, target)
        if why not in p.why:
            p.why.append(why)
        if me not in p.used_by:
            p.used_by.append(me)
        return n

    for r in sub:
        metas[r["id"]] = _rewrite(r, ctx, owned.get(r["id"]), name_port)

    # THE TEXT: `agents:` nested (no parent_id), then `ports:`
    def nest(id: str) -> dict:
        r = next(r for r in sub if r["id"] == id)
        e: dict = {"id": id}
        if r.get("handler_module") is not None:
            e["handler_module"] = r["handler_module"]
        e.update(_sorted(metas[id]))
        kids = [nest(x["id"]) for x in sub if x.get("parent_id") == id]
        if kids:
            e["agents"] = kids
        return e

    pm = {
        p.name: {"kind": p.kind, "why": "; ".join(p.why), "used_by": list(p.used_by)}
        for p in (ports[n] for n in sorted(ports))
    }
    body = yaml_io.dump({"agents": [nest(top)], "ports": pm})
    yaml = (
        f"# EXPORTED SUBTREE: agent {top} and every agent under it ({len(sub)}), as a\n"
        "# blueprint (the shape of src/tui/blueprint.yaml). Nesting under `agents:`\n"
        "# is the parent; `@root` is the root; every other `@…` name is a PORT\n"
        "# (`ports:` below) for something outside the subtree. Ids are the names;\n"
        "# the importer makes fresh ones. No runtime state, key values or rows.\n"
    )
    for n in notes:
        yaml += f"# NOTE: {n}\n"
    yaml += body
    return Exported(yaml, files, [ports[n] for n in sorted(ports)], notes, len(sub))


# ─── the secrets scan ───────────────────────────────────────────

_KEY_TOKEN = re.compile(r"(?<![A-Za-z0-9])sk-[A-Za-z0-9_-]{4,}")


def has_key_token(text: str) -> bool:
    """Whether `text` holds an `sk-...` key token (4+ key characters after it)."""
    return _KEY_TOKEN.search(text) is not None


def _scan_yaml(v, file: str, hits: list[str]) -> None:
    if isinstance(v, str):
        if is_absolute(v):
            hits.append(f"{file}: an absolute path")
    elif isinstance(v, list):
        for x in v:
            _scan_yaml(x, file, hits)
    elif isinstance(v, dict):
        for k, x in v.items():
            if isinstance(k, str):
                if k in SECRET_KEYS:
                    hits.append(f"{file}: a {k} key")
                if k in ("rows", "history"):
                    hits.append(f"{file}: row text ({k})")
            _scan_yaml(x, file, hits)


def scan_secrets(dir) -> list[str]:
    """THE SECRETS SCAN over an export folder: every hit names the file and what it
    found (never the value). The blueprint is checked by structure (secret keys,
    row keys, absolute paths); every file for `sk-...` tokens."""
    root = Path(dir)
    hits: list[str] = []
    for p in root.rglob("*"):
        if not p.is_file():
            continue
        name = p.relative_to(root).as_posix()
        try:
            text = p.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        if has_key_token(text):
            hits.append(f"{name}: an sk- key token")
        if name == BLUEPRINT_FILE:
            try:
                _scan_yaml(yaml_io.load(text), name, hits)
            except Exception as e:  # noqa: BLE001 - any parse failure is a hit
                hits.append(f"{name}: not yaml ({e})")
    return sorted(hits)


# ─── the verb ───────────────────────────────────────────────────


def _records(agent) -> tuple[list[dict], str]:
    """Every record of the tree, depth first from the root, children in creation
    order."""
    root = agent.ctx.root
    if root is None:
        return [], ""
    out: list[dict] = []
    seen: set[str] = set()
    stack = [root.id]
    while stack:
        id = stack.pop()
        a = agent.ctx.agents.get(id)
        if a is None or id in seen:
            continue
        seen.add(id)
        rec = dict(a.record)
        out.append(
            {
                "id": rec.pop("id"),
                "handler_module": rec.pop("handler_module", None),
                "parent_id": rec.pop("parent_id", None),
                "meta": rec,
            }
        )
        stack.extend(reversed(a.child_ids()))
    return out, root.id


def _absolute(base: Path, p) -> Path:
    p = Path(p)
    return p if p.is_absolute() else base / p


def mkdir(p: Path) -> None:
    os.mkdir(p, 0o700)


def write_file(p: Path, text: str) -> None:
    fd = os.open(p, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8", newline="") as f:
        f.write(text)


def _write_folder(tmp: Path, ex: Exported) -> None:
    """Write the export into `tmp` (fresh)."""
    try:
        mkdir(tmp)
    except OSError as e:
        raise OSError(f"could not create the export folder: {e}") from e
    write_file(tmp / BLUEPRINT_FILE, ex.yaml)
    for rel, text in ex.files:
        p = tmp / rel
        if not p.parent.exists():
            mkdir(p.parent)
        write_file(p, text)


def _has_dotdot(p: str) -> bool:
    return ".." in PurePath(p).parts or ".." in p.replace("\\", "/").split("/")


def export_subtree(agent, payload: dict) -> dict:
    """The verb: write the agent and everything under it as a blueprint folder at
    `path` (relative to the running dir), which must be new or empty."""
    agent_id, path = payload.get("agent_id"), payload.get("path")
    if not (isinstance(agent_id, str) and agent_id and isinstance(path, str) and path):
        return refusal(BAD_ARG, f"{_VERB}: agent_id (str) + path (str) required")
    if _has_dotdot(path):
        return refusal(BAD_ARG, f"{_VERB}: path may not hold `..`")
    base = agent.ctx.workdir
    out = _absolute(base, path)
    if out.exists():
        empty = out.is_dir() and not any(out.iterdir())
        if not empty:
            return refusal(EXISTS, f"{_VERB}: {path} exists and is not an empty folder")
    parent = out.parent
    if not parent.is_dir():
        return refusal(
            NOT_FOUND, f"{_VERB}: the folder that holds {path} does not exist"
        )

    recs, root_id = _records(agent)

    def folder_of(id: str) -> Path | None:
        a = agent.ctx.agents.get(id)
        rp = getattr(a, "_root_path", None) if a is not None else None
        if rp is None or str(rp) in ("", "."):
            return None
        return _absolute(base, rp)

    folder = folder_of(agent_id)

    def read(id: str, f: str) -> str | None:
        # an owned file: relative, no `..`, under the agent's folder or the running dir
        if _has_dotdot(f):
            return None
        d = folder_of(id)
        cands = ([d / f] if d is not None else []) + [base / f]
        p = next((c for c in cands if c.is_file()), None)
        if p is None:
            return None
        try:
            return p.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            return None

    try:
        ex = build(recs, agent_id, root_id, str(folder) if folder else None, read)
    except ValueError as e:
        return refusal(BAD_ARG, f"{_VERB}: {e}")

    tmp = parent / f".{out.name}.export-{os.getpid()}-{time.time_ns()}"
    try:
        _write_folder(tmp, ex)
    except OSError as e:
        shutil.rmtree(tmp, ignore_errors=True)
        return refusal(FAILED, f"{_VERB}: {e}")
    hits = scan_secrets(tmp)
    if hits:
        shutil.rmtree(tmp, ignore_errors=True)
        return refusal(
            SECRET_FOUND,
            f"{_VERB}: the output was not written \u2014 {'; '.join(hits)}",
        )
    if out.exists():
        try:
            out.rmdir()
        except OSError:
            pass
    try:
        os.rename(tmp, out)
    except OSError as e:
        shutil.rmtree(tmp, ignore_errors=True)
        return refusal(FAILED, f"{_VERB}: {e}")
    return {
        "exported": ex.agents,
        "path": path,
        "ports": [p.name for p in ex.ports],
        "files": [BLUEPRINT_FILE] + [f for f, _ in ex.files],
    }
