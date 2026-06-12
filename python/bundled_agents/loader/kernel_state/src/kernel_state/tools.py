"""kernel_state — the durable-state ROOT agent (a weak-bound STREAM CONSUMER).

`kernel_state` IS the tree root (`id="kernel_state"`). It owns the record⇄bytes
serialization, the tree→path mapping, and the auto-persist lifecycle. The auto-flush
persists ONLY through a stream PROVIDER it DISCOVERS (`_find_store`: the first
`file_bridge` child of the root whose root resolves to `.fantastic`). NO PROVIDER ⇒
NO AUTO-PERSIST — state lives in RAM until one is wired. There is no direct-write
fallback (kernel-style: one path, no defensive substitution). An operator/LLM wires
the provider — a normal, visible, gated `file_bridge` (see the keystone readme
example); point it at a `network_bridge` and the kernel's own state persists remotely
(G1 turned inward). Stream persistence is EMERGENT + opt-in; nothing ephemeral.

`read_tree`/`write_record` below are the BOOTSTRAP cold primitives + the explicit
`load_tree`/`persist_record` verb contract (used by session/remote loaders) — NOT
the auto-flush, which is provider-only.

  persist  ->  send(store, write_stream {path:<rel>/agent.json, bytes:<record>})
  forget   ->  send(store, delete {path:<rel>, recursive:true})

Contract (duck-typed; any agent answering these is a loader):
  load_tree                 -> {records, version}   read the subtree (direct)
  persist_record {record}   -> {ok}                 write one agent.json
  forget_record {id}        -> {ok}                 remove one agent's dir

The root SUBSCRIBES to the kernel state stream and flushes on add/update/remove
(dirty-queue + debounce). A loader serving a remote kernel sets `watch=false`.

Disk layout (provider-relative, under `.fantastic`):
  agent.json                 root record
  agents/<id>/agent.json     child, recursively
"""

from __future__ import annotations

import asyncio
import importlib.resources
import json
from pathlib import Path
from typing import Any

from file_bridge import fs
from kernel import CURRENT_VERSION


# ─── disk I/O via `fs` (the ONE disk surface) — bootstrap cold primitives ──
# These run before any agent exists, so they call `fs`'s functions directly (not the
# file_bridge AGENT). Still the same clamped surface: every read/write goes through
# `fs`, so nothing here escapes the running dir.


def read_tree(root_dir: Path) -> list[dict]:
    """Walk a `.fantastic` tree → flat list of records (root + descendants).
    `parent_id` comes from disk position (authoritative). Pure read; the
    bootstrap calls this BEFORE any agent exists. Corrupt records skip."""
    rd = str(root_dir)
    if not fs.exists(rd, "agent.json"):
        return []
    try:
        root_rec = json.loads(fs.read_text(rd, "agent.json"))
    except (json.JSONDecodeError, OSError, ValueError):
        return []
    # The root record declares the children-dir name (declared config; default
    # "agents"). A self-describing layout sets e.g. "host_agents" / "web_agents".
    cd = root_rec.get("children_dir")
    children_dir = cd if isinstance(cd, str) and cd else "agents"
    records: list[dict] = [root_rec]
    _walk(rd, children_dir, root_rec.get("id"), records, children_dir)
    return records


def _walk(
    root_dir: str,
    rel_dir: str,
    parent_id: str | None,
    out: list[dict],
    children_dir: str,
) -> None:
    if not (fs.exists(root_dir, rel_dir) and fs.is_dir(root_dir, rel_dir)):
        return
    try:
        entries = fs.list_dir(root_dir, rel_dir)
    except (OSError, ValueError):
        return
    for entry in entries:
        rel = f"{rel_dir}/{entry.name}".lstrip("/")
        af = f"{rel}/agent.json"
        if not fs.exists(root_dir, af):
            continue
        try:
            rec = json.loads(fs.read_text(root_dir, af))
        except (json.JSONDecodeError, OSError, ValueError):
            continue  # corrupt sibling → skip
        rec["parent_id"] = parent_id  # disk position is authoritative
        out.append(rec)
        _walk(root_dir, f"{rel}/{children_dir}", rec.get("id"), out, children_dir)


def write_record(root_path: Path, record: dict) -> None:
    """Merge-write one record's `agent.json` at `root_path` (the agent's own dir),
    THROUGH `fs` (atomic temp+rename). Reads any existing file and overlays only the
    kernel-managed keys, preserving unknown fields + sidecars. Also seeds the bundle's
    `readme.md` (copy-if-missing) — the loader owns ALL of an agent's disk sidecars."""
    rp = str(root_path)
    on_disk: dict[str, Any] = {}
    if fs.exists(rp, "agent.json"):
        try:
            existing = json.loads(fs.read_text(rp, "agent.json"))
            if isinstance(existing, dict):
                on_disk = existing
        except (json.JSONDecodeError, OSError, ValueError):
            pass
    on_disk.update(record)
    fs.write_text(rp, "agent.json", json.dumps(on_disk, indent=2))  # atomic
    _seed_readme(rp, record.get("handler_module"))


def _seed_readme(root_path: Path | str, handler_module: str | None) -> None:
    """Copy a bundle's shipped `readme.md` into the agent's dir on first persist,
    THROUGH `fs`. Copy-if-missing. The SOURCE is a package resource (installed code
    via importlib, not user disk); only the WRITE goes through `fs`."""
    if not handler_module:
        return
    rp = str(root_path)
    if fs.exists(rp, "readme.md"):
        return
    pkg = handler_module.split(".")[0]
    try:
        src = importlib.resources.files(pkg) / "readme.md"
        if src.is_file():
            fs.write_text(rp, "readme.md", src.read_text(encoding="utf-8"))
    except (
        ModuleNotFoundError,
        FileNotFoundError,
        OSError,
        TypeError,
        AttributeError,
        ValueError,
    ):
        # AttributeError: handler modules that aren't proper packages
        # (single-file modules, synthetic test stubs). Nothing to seed.
        pass


def rmtree(path: Path) -> None:
    """Recursive delete of an agent's dir, THROUGH `fs` (clamped to cwd)."""
    rp = str(path)
    if not fs.exists(rp, ""):
        return
    try:
        fs.remove(rp, "", recursive=True)
    except (OSError, ValueError):
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


# ─── the stream provider binding (DISCOVERED, weak-coupled) ────────


def _find_store(agent) -> str | None:
    """DISCOVER the persistence provider: the first `file_bridge` CHILD of the loader
    whose root resolves to the loader's own `.fantastic` dir. Bound by MATCH, not a
    fixed id and not kernel-composed — an operator/LLM creates it (see the keystone
    readme example), the loader finds it. Returns its id, or None when none is wired
    — in which case the flush does NOTHING (state stays in RAM). No fallback."""
    kernel = agent.ctx
    try:
        want = _root(agent).resolve()
    except (OSError, ValueError):
        return None
    base = Path.cwd().resolve()
    for cid in agent.child_ids():
        rec = kernel.get(cid) or {}
        if rec.get("handler_module") != "file_bridge.tools":
            continue
        r = rec.get("root", "") or ""
        try:
            root = (Path(r) if Path(r).is_absolute() else base / r).resolve()
        except (OSError, ValueError):
            continue
        if root == want:
            return cid
    return None


def _store_reldir(agent, abs_path: Path) -> str:
    """An agent's dir RELATIVE to the provider root (`_root(agent)` == `.fantastic`).
    The root loader itself → "" (its agent.json sits at the provider root)."""
    try:
        rel = Path(abs_path).relative_to(_root(agent))
    except ValueError:
        rel = Path(abs_path)
    s = str(rel)
    return "" if s == "." else s


async def _persist_via_store(
    agent, store_id: str, abs_path: Path, record: dict
) -> None:
    """Write one record's agent.json (+ seed its readme) THROUGH the provider's
    stream verbs. Merge-not-overwrite: read the existing bytes, overlay the
    kernel-managed keys, write back — so sidecar fields survive. If the provider
    refuses (sealed), the write simply doesn't land — NO fallback; gating the store
    is the operator's choice and its consequences are theirs."""
    kernel = agent.ctx
    reldir = _store_reldir(agent, abs_path)
    af = f"{reldir}/agent.json" if reldir else "agent.json"
    existing: dict = {}
    got = await kernel.send(store_id, {"type": "read_stream", "path": af})
    if isinstance(got, dict) and isinstance(got.get("bytes"), (bytes, bytearray)):
        try:
            parsed = json.loads(bytes(got["bytes"]))
            if isinstance(parsed, dict):
                existing = parsed
        except (ValueError, json.JSONDecodeError):
            existing = {}
    existing.update(record)
    body = json.dumps(existing, indent=2).encode("utf-8")
    await kernel.send(
        store_id,
        {
            "type": "write_stream",
            "path": af,
            "bytes": body,
            "truncate": True,
        },
    )
    await _seed_readme_via_store(agent, store_id, reldir, record.get("handler_module"))


async def _seed_readme_via_store(
    agent, store_id: str, reldir: str, handler_module: str | None
) -> None:
    """Copy-if-missing the bundle's shipped readme.md into the agent's dir THROUGH
    the provider. Source is the installed package (in-process resource); sink is
    the stream provider."""
    if not handler_module:
        return
    kernel = agent.ctx
    path = f"{reldir}/readme.md" if reldir else "readme.md"
    got = await kernel.send(store_id, {"type": "read_stream", "path": path})
    if isinstance(got, dict) and isinstance(got.get("bytes"), (bytes, bytearray)):
        return  # already present — never clobber operator edits
    pkg = handler_module.split(".")[0]
    try:
        src = importlib.resources.files(pkg) / "readme.md"
        if src.is_file():
            data = src.read_text(encoding="utf-8").encode("utf-8")
            await kernel.send(
                store_id,
                {
                    "type": "write_stream",
                    "path": path,
                    "bytes": data,
                    "truncate": True,
                },
            )
    except (ModuleNotFoundError, FileNotFoundError, OSError, TypeError, AttributeError):
        pass


async def _forget_via_store(agent, store_id: str, abs_path: Path) -> None:
    """Recursively remove one agent's dir THROUGH the provider (never the root)."""
    reldir = _store_reldir(agent, abs_path)
    if not reldir:
        return  # never remove the root
    await agent.ctx.send(
        store_id, {"type": "delete", "path": reldir, "recursive": True}
    )


# ─── the auto-flush loop (the root subscribes the live tree) ────────


class _FlushLoop:
    """Subscribe to the kernel state stream; debounce-flush records on add/update,
    remove on delete — ONLY through a DISCOVERED provider (`_find_store`: a file_bridge
    child rooted at `.fantastic`). Writing rides the provider's verbs (not the state
    stream, so it never feeds back). No provider ⇒ the flush is a no-op and state
    stays in RAM (no fallback). Wiring a provider makes the kernel a stream
    consumer; until then it simply isn't persistent."""

    def __init__(self, agent) -> None:
        self.agent = agent
        self.kernel = agent.ctx
        self._persist: dict[str, None] = {}  # ordered set of ids to write
        self._forget: dict[str, Path] = {}  # id -> dir to remove
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
                await self.aflush()
        except asyncio.CancelledError:
            pass

    async def aflush(self) -> None:
        store_id = _find_store(self.agent)
        if store_id is None:
            return  # no provider wired → records stay queued until one is. NO fallback.
        persist_ids = list(self._persist.keys())
        self._persist.clear()
        forget = dict(self._forget)
        self._forget.clear()
        for aid in persist_ids:
            a = self.kernel.get_agent(aid)
            if a is None or type(a).ephemeral:
                continue
            await _persist_via_store(self.agent, store_id, a._root_path, a.record)
        for path in forget.values():
            await _forget_via_store(self.agent, store_id, path)

    async def stop(self) -> None:
        if self._unsub is not None:
            self._unsub()
            self._unsub = None
        if self._task is not None:
            self._task.cancel()
            self._task = None
        await self.aflush()  # final flush — a clean shutdown loses nothing


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


def reflect_root_extra(agent) -> dict:
    """Duck-typed root-reflect contribution (the substrate merges it on the ROOT only).
    Surfaces WHICH file_bridge the loader auto-persists THROUGH — the discovered store
    provider, or `null` when none is wired (state stays in RAM). The provider's own
    posture (open / sealed) is visible inline in the tree node, so a client reads, in
    ONE reflect, whether persistence is wired AND whether the wired leg is actually
    open — no silent RAM/sealed surprise, no kernel guessing."""
    return {"persistence": {"provider": _find_store(agent)}}


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
        return {"error": f"kernel_state: unknown type {payload.get('type')!r}"}
    return await fn(id, payload, agent)
