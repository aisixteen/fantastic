"""fs_loader — the disk persistence + hydration ROOT agent.

The loader IS the tree root (`id="fs_loader"`): it owns the `.fantastic/`
medium. Persistence is decoupled from the kernel — the kernel only
converts between the live tree and a flat record list (`Kernel.save`/
`Kernel.load`); the loader turns that list into bytes and back.

Contract (duck-typed; any agent answering these is a loader):
  load_tree                 -> {records, version}   read the subtree from disk
  persist_record {record}   -> {ok}                 merge-write one agent.json
  forget_record {id}        -> {ok}                 rmtree one agent's dir

The root loader also SUBSCRIBES to the kernel state stream and flushes
on add/update/remove (dirty-queue + debounce), so the live tree
auto-persists. A loader serving a remote kernel over the bridge sets
`watch=false` and only answers the verbs.

Disk layout (unchanged from the legacy substrate — drop-in compatible):
  <root>/agent.json                 root record
  <root>/agents/<id>/agent.json     child, recursively
Merge-not-overwrite: persist updates only the kernel-managed record
keys, leaving sidecar files + unknown fields untouched.
"""

from __future__ import annotations

import asyncio
import importlib.resources
import json
import os
from pathlib import Path
from typing import Any

from kernel import CURRENT_VERSION


# ─── pure disk I/O (no kernel needed — the bootstrap calls these) ──


def read_tree(root_dir: Path) -> list[dict]:
    """Walk a `.fantastic` tree → flat list of records (root + descendants).
    `parent_id` comes from disk position (authoritative). Pure read; the
    bootstrap calls this BEFORE any agent exists. Corrupt records skip."""
    root_dir = Path(root_dir)
    af = root_dir / "agent.json"
    if not af.exists():
        return []
    try:
        root_rec = json.loads(af.read_text(encoding="utf-8"))
    except (json.JSONDecodeError, OSError):
        return []
    # The root record declares the children-dir name (declared config; default
    # "agents"). A self-describing layout sets e.g. "host_agents" / "web_agents".
    cd = root_rec.get("children_dir")
    children_dir = cd if isinstance(cd, str) and cd else "agents"
    records: list[dict] = [root_rec]
    _walk(root_dir / children_dir, root_rec.get("id"), records, children_dir)
    return records


def _walk(
    agents_dir: Path, parent_id: str | None, out: list[dict], children_dir: str
) -> None:
    if not agents_dir.exists():
        return
    for entry in sorted(agents_dir.iterdir()):
        af = entry / "agent.json"
        if not af.exists():
            continue
        try:
            rec = json.loads(af.read_text(encoding="utf-8"))
        except (json.JSONDecodeError, OSError):
            continue  # corrupt sibling → skip (matches legacy behavior)
        rec["parent_id"] = parent_id  # disk position is authoritative
        out.append(rec)
        _walk(entry / children_dir, rec.get("id"), out, children_dir)


def write_record(root_path: Path, record: dict) -> None:
    """Merge-write one record's `agent.json` at `root_path` (the agent's
    own dir). Reads any existing file and overlays only the kernel-managed
    keys, preserving unknown fields + sidecars (Rust `persistence::persist`
    semantics). Atomic temp+rename. Also seeds the bundle's `readme.md`
    (copy-if-missing) — the loader owns ALL of an agent's disk sidecars."""
    root_path = Path(root_path)
    root_path.mkdir(parents=True, exist_ok=True)
    af = root_path / "agent.json"
    on_disk: dict[str, Any] = {}
    if af.exists():
        try:
            existing = json.loads(af.read_text(encoding="utf-8"))
            if isinstance(existing, dict):
                on_disk = existing
        except (json.JSONDecodeError, OSError):
            pass
    on_disk.update(record)
    tmp = af.with_name("agent.json.tmp")
    tmp.write_text(json.dumps(on_disk, indent=2), encoding="utf-8")
    os.replace(tmp, af)
    _seed_readme(root_path, record.get("handler_module"))


def _seed_readme(root_path: Path, handler_module: str | None) -> None:
    """Copy a bundle's shipped `readme.md` into the agent's dir on first
    persist. Copy-if-missing — operator edits + the GitHub-canonical
    version are never clobbered. Bundle-agnostic: derives the package
    from `handler_module` and asks `importlib.resources` whether it ships
    a `readme.md`. No module / no readme → nothing seeded (not an error)."""
    if not handler_module:
        return
    dest = root_path / "readme.md"
    if dest.exists():
        return
    pkg = handler_module.split(".")[0]
    try:
        src = importlib.resources.files(pkg) / "readme.md"
        if src.is_file():
            dest.write_text(src.read_text(encoding="utf-8"), encoding="utf-8")
    except (
        ModuleNotFoundError,
        FileNotFoundError,
        OSError,
        TypeError,
        AttributeError,
    ):
        # AttributeError: handler modules that aren't proper packages
        # (single-file modules, synthetic test stubs). Nothing to seed.
        pass


def rmtree(path: Path) -> None:
    """Recursive delete of an agent's dir (relocated from Agent._rmtree)."""
    path = Path(path)
    if not path.exists():
        return
    for sub in path.iterdir():
        if sub.is_dir():
            rmtree(sub)
        else:
            try:
                sub.unlink()
            except OSError:
                pass
    try:
        path.rmdir()
    except OSError:
        pass


def _root(agent) -> Path:
    """The disk dir this loader owns. Defaults to the loader's own address
    (`_root_path`) — the host ROOT loader owns `.fantastic`. A `root` meta
    overrides it, so a SESSION loader (a child of `web`, `watch=false`) can
    serve a sub-namespace like `.fantastic/web/<session>/` for a federated
    JS kernel's records, separate from the host tree."""
    r = agent.record.get("root")
    return Path(r) if isinstance(r, str) and r else agent._root_path


def _children_name(agent) -> str:
    """The children-container dir name for this loader's namespace (declared
    config; default "agents"). A `children_dir` meta on the loader's record sets
    e.g. "web_agents" so a served namespace is self-describing on disk."""
    cd = agent.record.get("children_dir")
    return cd if isinstance(cd, str) and cd else "agents"


def _resolve_path(agent, record: dict) -> Path:
    """Compute a record's on-disk dir under this loader's root by walking
    the record's parent chain in the kernel. Used when the agent isn't (or
    is no longer) live (e.g. a removed agent, or a bridge-sent record)."""
    parts: list[str] = [record["id"]]
    pid = record.get("parent_id")
    k = agent.ctx
    seen: set[str] = set()
    while pid and pid != agent.id and pid not in seen:
        seen.add(pid)
        parts.append(pid)
        prec = k.get(pid)
        pid = prec.get("parent_id") if prec else None
    path = _root(agent)
    children_dir = _children_name(agent)
    for part in reversed(parts):
        path = path / children_dir / part
    return path


# ─── the auto-flush loop (the root loader subscribes the live tree) ─


class _FlushLoop:
    """Subscribe to the kernel state stream; debounce-flush records to disk
    on add/update, rmtree on remove. DIRECT I/O (not via send), so it never
    feeds back into the state stream."""

    def __init__(self, agent) -> None:
        self.agent = agent
        self.kernel = agent.ctx
        self._persist: dict[str, None] = {}  # ordered set of ids to write
        self._forget: dict[str, Path] = {}  # id -> dir to rmtree
        self._paths: dict[str, Path] = {}  # id -> dir cache (for removed ids)
        self._wake = asyncio.Event()
        self._unsub = None
        self._task: asyncio.Task | None = None

    def start(self) -> None:
        for a in self.kernel.agents.values():
            self._paths[a.id] = a._root_path
        self._unsub = self.kernel.add_state_subscriber(self._on_state)
        self._task = asyncio.create_task(self._run())

    def _on_state(self, evt: dict) -> None:
        kind = evt.get("kind")
        aid = evt.get("agent_id")
        if not aid:
            return
        if kind in ("added", "updated"):
            a = self.kernel.get_agent(aid)
            if a is None or type(a).ephemeral:
                return
            self._paths[aid] = a._root_path
            self._persist[aid] = None
            self._forget.pop(aid, None)
            self._wake.set()
        elif kind == "removed":
            path = self._paths.pop(aid, None)
            if path is not None:
                self._forget[aid] = path
                self._persist.pop(aid, None)
                self._wake.set()

    async def _run(self) -> None:
        try:
            while True:
                await self._wake.wait()
                await asyncio.sleep(0.15)  # debounce / coalesce rapid mutations
                self._wake.clear()
                self.flush()
        except asyncio.CancelledError:
            pass

    def flush(self) -> None:
        persist_ids = list(self._persist.keys())
        self._persist.clear()
        forget = dict(self._forget)
        self._forget.clear()
        for aid in persist_ids:
            a = self.kernel.get_agent(aid)
            if a is not None and not type(a).ephemeral:
                write_record(a._root_path, a.record)
        for path in forget.values():
            rmtree(path)

    async def stop(self) -> None:
        if self._unsub is not None:
            self._unsub()
            self._unsub = None
        if self._task is not None:
            self._task.cancel()
            self._task = None
        self.flush()  # final flush — a clean shutdown loses nothing


# ─── verbs ─────────────────────────────────────────────────────────


async def _reflect(id, payload, agent):
    """Identity + the disk root it owns. No args."""
    return {
        "id": id,
        "sentence": "Disk persistence/hydration root — owns .fantastic; "
        "answers load_tree / persist_record / forget_record.",
        "root": str(_root(agent)),
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
    }


async def _boot(id, payload, agent):
    """Start the auto-flush loop unless `watch=false` (a loader serving a
    remote kernel only answers verbs). Subscribes AFTER the bootstrap's
    `kernel.load`, so hydration's bulk `added` events don't re-flush what
    was just read. Idempotent.

    A SESSION loader (a `root` meta pointing at a sub-namespace like
    `.fantastic/web/<session>/`) also seeds its namespace anchor
    (`<root>/agent.json`) so `read_tree` has a root to walk from — the
    federated JS kernel's records nest under `<root>/agents/`."""
    root_dir = _root(agent)
    if root_dir != agent._root_path and not (root_dir / "agent.json").exists():
        write_record(root_dir, agent.record)
    rec = agent.record
    if rec.get("watch", True) is False:
        return None
    if getattr(agent, "_fs_flush_loop", None) is not None:
        return None
    loop = _FlushLoop(agent)
    agent._fs_flush_loop = loop
    loop.start()
    return None


async def _load_tree(id, payload, agent):
    """args: none. Read this loader's subtree from disk → {records, version}.
    Flat list; parent_id encodes structure. Reads `root` if set (a session
    loader serving a sub-namespace), else the loader's own dir."""
    return {"records": read_tree(_root(agent)), "version": CURRENT_VERSION}


async def _persist_record(id, payload, agent):
    """args: record:dict (req). Merge-write one agent.json. Location is the
    live agent's _root_path if present, else computed under this loader's
    root from the record's parent chain."""
    record = payload.get("record")
    if not isinstance(record, dict) or not record.get("id"):
        return {"error": "persist_record: record (dict with id) required"}
    a = agent.ctx.get_agent(record["id"])
    write_record(
        a._root_path if a is not None else _resolve_path(agent, record), record
    )
    return {"ok": True}


async def _forget_record(id, payload, agent):
    """args: id:str (req), parent_id:str (opt). rmtree the agent's dir. For a
    live host agent the path is its own `_root_path`; for a foreign (bridge-
    sent) record the `parent_id` lets the loader resolve the NESTED path under
    its root — a removed agent is no longer in the tree to walk."""
    target = payload.get("id")
    if not target:
        return {"error": "forget_record: id required"}
    a = agent.ctx.get_agent(target)
    if a is not None:
        path = a._root_path
    else:
        path = _resolve_path(
            agent, {"id": target, "parent_id": payload.get("parent_id")}
        )
    rmtree(path)
    return {"ok": True}


async def on_shutdown(agent):
    """Final flush + unsubscribe — a graceful shutdown loses nothing."""
    loop = getattr(agent, "_fs_flush_loop", None)
    if loop is not None:
        await loop.stop()


VERBS = {
    "reflect": _reflect,
    "boot": _boot,
    "load_tree": _load_tree,
    "persist_record": _persist_record,
    "forget_record": _forget_record,
}


async def handler(id: str, payload: dict, agent) -> dict | None:
    fn = VERBS.get(payload.get("type"))
    if fn is None:
        return {"error": f"fs_loader: unknown type {payload.get('type')!r}"}
    return await fn(id, payload, agent)
