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

  persist  ->  send(store, write_stream {path:<rel>/agent.yaml, bytes:<record>})
  forget   ->  send(store, delete {path:<rel>, recursive:true})

Contract (duck-typed; any agent answering these is a loader):
  load_tree                 -> {records, version}   read the subtree (direct)
  persist_record {record}   -> {ok}                 write one agent.yaml
  forget_record {id}        -> {ok}                 remove one agent's dir

The root SUBSCRIBES to the kernel state stream and flushes on add/update/remove
(dirty-queue + debounce). A loader serving a remote kernel sets `watch=false`.

Disk layout (provider-relative, under `.fantastic`):
  agent.yaml                 root record
  agents/<id>/agent.yaml     child, recursively
"""

from __future__ import annotations

import asyncio
import os
import posixpath
import re
import secrets
import sys
import uuid
from importlib.metadata import entry_points
from pathlib import Path
from typing import Any

from file_bridge import fs

import yaml

from kernel_state import yaml_io

from kernel import CURRENT_VERSION
from kernel._bundles import (
    _find_bundle_module,
    clashes,
    import_handler,
    refuse_folder,
    register_folder,
)
from kernel import current_origin
from kernel._kernel import BUNDLE_ENTRY_GROUP
from kernel._refusal import (
    BAD_ARG,
    INVALID_META,
    NOT_FOUND,
    NOT_HERE,
    UNAUTHORIZED,
    UNHANDLED,
    guard_agent_id,
    missing,
    refusal,
)

# ─── disk I/O via `fs` (the ONE disk surface) — bootstrap cold primitives ──
# These run before any agent exists, so they call `fs`'s functions directly (not the
# file_bridge AGENT). Still the same clamped surface: every read/write goes through
# `fs`, so nothing here escapes the running dir.


_RECORD_FILE = "agent.yaml"


ROOT_ID = "kernel_state"


def _read_record(root_dir: fs.Root, rel: str) -> dict | None:
    """One `agent.yaml` → its mapping, or None when absent/corrupt/not a mapping."""
    if not fs.exists(root_dir, rel):
        return None
    try:
        parsed = yaml_io.load(fs.read_text(root_dir, rel))
    except (yaml.YAMLError, OSError, ValueError):
        return None
    return parsed if isinstance(parsed, dict) else None


def read_tree(root_dir: fs.Root, root_rec: dict | None = None) -> list[dict]:
    """Walk a `.fantastic` tree → flat list of records (root + descendants).
    `parent_id` comes from disk position (authoritative). Pure read; the
    bootstrap calls this BEFORE any agent exists. Corrupt records skip.

    `root_rec` is the root's LIVE record when the caller has one (a session
    loader serving its namespace): the namespace keeps no copy of it on disk."""
    rd = root_dir
    if root_rec is None:
        root_rec = _read_record(rd, _RECORD_FILE)
    # A MISSING OR CORRUPT ROOT RECORD DOES NOT HIDE THE TREE. It used to return
    # `[]`, so the boot reseeded a fresh root and hydrated none of the children
    # sitting under `agents/` (measured 2026-09-16). The children are walked under
    # the default layout and parented to the id the reseed will mint.
    if root_rec is None:
        records: list[dict] = []
        _walk(rd, "agents", ROOT_ID, records, "agents")
        return records
    # The root record declares the children-dir name (declared config; default
    # "agents"). A self-describing layout sets e.g. "host_agents" / "web_agents".
    cd = root_rec.get("children_dir")
    children_dir = cd if isinstance(cd, str) and cd else "agents"
    records = [root_rec]
    _walk(rd, children_dir, root_rec.get("id"), records, children_dir)
    return records


# ─── folder bundles (owner, 2026-10-08) ──────────────────────────────
# `<root>/bundles/<name>/bundle.yaml` — code that EXTENDS a bundled agent (`base`).
# This loader owns the `.fantastic` layout, so it reads the folders: at boot beside
# `read_tree` (before `kernel.load`, so an instance's handler resolves), and on the
# root's `reload_bundle`. The base builds the module (`folder_bundle`).

_BUNDLES_DIR = "bundles"
_BUNDLE_FILE = "bundle.yaml"
_FOLDER_NAME = re.compile(r"[A-Za-z_][A-Za-z0-9_-]*")


def _known_bases() -> list[str]:
    """The installed bundled agents a folder can extend (they build `folder_bundle`)."""
    out = []
    for ep in entry_points(group=BUNDLE_ENTRY_GROUP):
        try:
            mod = import_handler(ep.value)
        except Exception:  # noqa: BLE001 — a bundle that cannot import is no base
            continue
        if hasattr(mod, "folder_bundle"):
            out.append(ep.name)
    return sorted(out)


def load_bundle(root_dir: fs.Root, name: str) -> str | None:
    """Load ONE folder bundle `<root>/bundles/<name>/` into the kernel's registry.
    → None, or the error — recorded on its catalog row too, never loaded."""
    rel = f"{_BUNDLES_DIR}/{name}"
    shown = Path(str(root_dir.rel)).name
    row = {"name": name, "handler_module": name, "folder": f"{shown}/{rel}"}

    def refuse(why: str) -> str:
        err = f"bundles/{name}: {why}"
        refuse_folder(name, {**row, "error": err})
        return err

    if not _FOLDER_NAME.fullmatch(name):
        return refuse(
            "a bundle folder is named with letters, digits, _ and - (the name is "
            "its instances' handler_module)"
        )
    if clashes(name):
        return refuse(f"{name!r} is a built-in bundle — a folder may not shadow it")
    if not fs.exists(root_dir, f"{rel}/{_BUNDLE_FILE}"):
        return refuse("no bundle.yaml")
    try:
        spec = yaml_io.load(fs.read_text(root_dir, f"{rel}/{_BUNDLE_FILE}"))
    except (yaml.YAMLError, OSError, ValueError) as e:
        return refuse(f"bundle.yaml does not parse: {e}")
    if not isinstance(spec, dict):
        return refuse("bundle.yaml is not a mapping")
    base = spec.get("base")
    if not isinstance(base, str) or not base:
        return refuse(
            "base: required — the bundled agent it extends "
            f"(here: {', '.join(_known_bases()) or 'none'})"
        )
    sentence = spec.get("sentence")
    if not isinstance(sentence, str) or not sentence.strip():
        return refuse("sentence: required — one line saying what it is")
    module = _find_bundle_module(base)
    base_mod = None
    if module is not None:
        try:
            base_mod = import_handler(module)
        except Exception:  # noqa: BLE001
            base_mod = None
    if base_mod is None or not hasattr(base_mod, "folder_bundle"):
        return refuse(
            f"base: {base!r} is no bundled agent a folder can extend in this "
            f"kernel — known: {', '.join(_known_bases()) or 'none'}"
        )
    built = base_mod.folder_bundle(name, str(fs.resolve(root_dir, rel)), spec)
    if isinstance(built, str):
        return refuse(built)
    register_folder(name, built, {**row, "base": base, "sentence": sentence.strip()})
    return None


def load_bundles(root_dir: fs.Root) -> None:
    """Load every folder under `<root>/bundles/` — the cold read at boot."""
    if not (fs.exists(root_dir, _BUNDLES_DIR) and fs.is_dir(root_dir, _BUNDLES_DIR)):
        return
    for entry in fs.list_dir(root_dir, _BUNDLES_DIR):
        if entry.is_dir():
            load_bundle(root_dir, entry.name)


def _walk(
    root_dir: fs.Root,
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
        rec = _read_record(root_dir, f"{rel}/{_RECORD_FILE}")
        if rec is None:
            continue  # absent or corrupt sibling → skip
        rec["parent_id"] = parent_id  # disk position is authoritative
        out.append(rec)
        _walk(root_dir, f"{rel}/{children_dir}", rec.get("id"), out, children_dir)


def write_record(root_path: fs.Root, record: dict) -> None:
    """Merge-write one record's `agent.yaml` at `root_path` (the agent's own dir),
    THROUGH `fs` (atomic temp+rename). Reads any existing file and overlays only the
    kernel-managed keys, preserving unknown fields + sidecars. A bundle's readme
    and `docs/` are never copied here — `reflect` reads them from the bundle."""
    rp = root_path
    on_disk: dict[str, Any] = _read_record(rp, _RECORD_FILE) or {}
    on_disk.update(record)
    fs.write_text(rp, _RECORD_FILE, yaml_io.dump(on_disk))  # atomic


def rmtree(path: fs.Root) -> None:
    """Recursive delete of an agent's dir, THROUGH `fs` (clamped to the running dir)."""
    rp = path
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


def _disk_parents(agent) -> dict[str, str | None]:
    """This loader's PERSISTED records → `{id: parent_id}`, the parent taken from DISK
    POSITION under its root (authoritative, as `read_tree` does): a record directly
    under the root's children dir is parented to the loader itself. A record the
    host never runs — a federated kernel's, sent through a bridge — exists only
    here, so a walk that stops at the live tree loses every record nested under it."""
    out: list[dict] = []
    cd = _children_name(agent)
    _walk(fs.Root(agent.ctx.workdir, _root(agent)), cd, agent.id, out, cd)
    return {r["id"]: r.get("parent_id") for r in out if isinstance(r.get("id"), str)}


def _where(agent, node: str, disk: dict | None) -> tuple[bool, str | None, dict | None]:
    """(known, parent, disk) for one id: the LIVE tree first, then this loader's
    persisted records (read once per walk, cached in `disk`). Unknown in both ⇒
    (False, None) — the caller decides; nothing is guessed."""
    live = agent.ctx.get(node)
    if live is not None:
        return True, live.get("parent_id"), disk
    if disk is None:
        disk = _disk_parents(agent)
    if node in disk:
        return True, disk[node], disk
    return False, None, disk


def _resolve_path(agent, record: dict) -> Path:
    """Compute a record's on-disk dir under this loader's root by walking its parent
    chain: the live tree, then this loader's persisted records (so a record nested
    2+ levels under a federated record lands where it belongs). A record that
    already exists keeps ITS position; the payload's `parent_id` places only a NEW
    one. Used when the agent isn't (or is no longer) live."""
    parts: list[str] = [record["id"]]
    known, pid, disk = _where(agent, record["id"], None)
    if not known:
        pid = record.get("parent_id")
    seen: set[str] = set()
    while pid and pid != agent.id and pid not in seen:
        seen.add(pid)
        parts.append(pid)
        _, pid, disk = _where(agent, pid, disk)
    path = _root(agent)
    children_dir = _children_name(agent)
    for part in reversed(parts):
        path = path / children_dir / part
    return path


# ─── the stream provider binding (DISCOVERED, weak-coupled) ────────


def _find_store(agent) -> str | None:
    """DISCOVER the persistence provider: a `file_bridge` in this loader's SUBTREE
    whose root resolves to the loader's own `.fantastic` dir. Bound by MATCH, not a
    fixed id and not kernel-composed — an operator/LLM creates it (see the keystone
    readme example), the loader finds it. Returns its id, or None when none is wired
    — in which case the flush does NOTHING (state stays in RAM). No fallback.

    SUBTREE, not direct children. It scanned one level for a long time, and the
    consequence was invisible: a provider declared through a `static_loader`
    manifest lands as a GRANDCHILD (child of the roster loader, not of the root),
    was never discovered, and `aflush` then no-ops with no error anywhere — the
    tree just silently stays in RAM. Declaring it was never the problem; finding
    it was. Breadth-first so the shallowest match still wins, which keeps the
    previous behaviour whenever a direct child matches.
    """
    kernel = agent.ctx
    base = kernel.workdir
    try:
        want = fs.resolve_root(fs.Root(base, _root(agent)))
    except (OSError, ValueError):
        return None
    queue: list[str] = list(agent.child_ids())
    seen: set[str] = set()
    while queue:
        cid = queue.pop(0)
        if cid in seen:
            continue
        seen.add(cid)
        child = kernel.agents.get(cid)
        if child is not None:
            queue.extend(child.child_ids())
        rec = kernel.get(cid) or {}
        if rec.get("handler_module") != "file_bridge.tools":
            continue
        try:
            root = fs.resolve_root(fs.Root(base, rec.get("root", "") or ""))
        except (OSError, ValueError):
            continue
        if root == want:
            return cid
    return None


def _store_reldir(agent, abs_path: Path) -> str:
    """An agent's dir RELATIVE to the provider root (`_root(agent)` == `.fantastic`).
    The root loader itself → "" (its agent.yaml sits at the provider root)."""
    try:
        rel = Path(abs_path).relative_to(_root(agent))
    except ValueError:
        rel = Path(abs_path)
    s = str(rel)
    return "" if s == "." else s


def _covers(allow, path: str) -> bool:
    """Does a store's `allow` list expose `path` (a listed path, or under a listed
    directory)? The file_bridge's own rule, read off its record."""
    for s in allow if isinstance(allow, list) else []:
        if not isinstance(s, str) or s.startswith("/"):
            continue
        e = posixpath.normpath(s or ".")
        if e == "." or path == e or path.startswith(e + "/"):
            return True
    return False


def _persist_failed(agent, store_id: str, path: str, why) -> None:
    """A refused/failed persist write is LOUD: stderr, and `persistence.error` on the
    root reflect (which then reports no provider) until a write lands again."""
    msg = why.get("error", why) if isinstance(why, dict) else why
    print(
        f"  [{agent.id}] persist through {store_id!r} failed: {path}: {msg}",
        file=sys.stderr,
    )
    agent._persist_error = {"store": store_id, "path": path, "error": msg}


def _persist_landed(agent) -> None:
    agent._persist_error = None


def _check_store_allow(agent) -> None:
    """At boot: a discovered store whose `allow` does not expose the records
    (`agent.yaml` and the children dir) would refuse every write — say so now."""
    store_id = _find_store(agent)
    if store_id is None:
        return
    allow = (agent.ctx.get(store_id) or {}).get("allow")
    missing_paths = [
        p for p in (_RECORD_FILE, _children_name(agent)) if not _covers(allow, p)
    ]
    if missing_paths:
        _persist_failed(
            agent,
            store_id,
            ", ".join(missing_paths),
            "the store's `allow` does not expose the records "
            f"(add them, or `.`): allow={allow!r}",
        )


async def _store_call(agent, store_id: str, payload: dict):
    """ONE CALL TO THE LOADER'S OWN STORE — persistence bookkeeping, so the kernel
    runs the store's handler DIRECTLY (`Kernel.bookkeeping`): not a gated send,
    never judged against the loader's own grant (the bootstrap root carries none),
    the same from the persister loop, a blob verb or a one-shot. The store's own
    leg gate (its `ingress_rule`, its `allow`) still judges every call, inside its
    handler. A store that is gone answers its unknown-agent refusal."""
    store = agent.ctx.agents.get(store_id)
    if store is None:
        return agent.ctx.unknown_agent(store_id)
    return (await agent.ctx.bookkeeping(store, payload))["result"]


async def _persist_via_store(
    agent, store_id: str, abs_path: Path, record: dict
) -> None:
    """Write one record's agent.yaml THROUGH the provider's
    stream verbs. Merge-not-overwrite: read the existing bytes, overlay the
    kernel-managed keys, write back — so sidecar fields survive. If the provider
    refuses (sealed, unexposed), the write doesn't land — NO fallback — and the
    refusal is LOUD (`_persist_failed`)."""
    reldir = _store_reldir(agent, abs_path)
    af = f"{reldir}/{_RECORD_FILE}" if reldir else _RECORD_FILE
    existing: dict = {}
    # The WHOLE document: `read_stream` answers one chunk (64 KiB by default),
    # so a record larger than that failed to parse and silently lost its
    # unknown keys on the next write (measured 2026-09-16). Page to eof.
    buf = bytearray()
    offset = 0
    while True:
        got = await _store_call(
            agent,
            store_id,
            {
                "type": "read_stream",
                "path": af,
                "offset": offset,
            },
        )
        if not (
            isinstance(got, dict) and isinstance(got.get("bytes"), (bytes, bytearray))
        ):
            buf = bytearray()
            break
        buf += bytes(got["bytes"])
        if got.get("eof", True) or not isinstance(got.get("next_offset"), int):
            break
        offset = got["next_offset"]
    if buf:
        try:
            parsed = yaml_io.load(bytes(buf).decode("utf-8"))
            if isinstance(parsed, dict):
                existing = parsed
        except (ValueError, yaml.YAMLError):
            existing = {}
    existing.update(record)
    body = yaml_io.dump(existing).encode("utf-8")
    # ATOMIC COMMIT: stream into a tmp beside the target, then RENAME. A
    # `write_stream` that dies after truncating leaves a half-written or empty
    # `agent.yaml` — the record is gone and the next boot hydrates a tree with a
    # hole in it. `rename` is the leg's own single atomic op, and it creates
    # parents, so the failure modes are "the old record survives" or "the new one
    # is complete", never a torn file.
    tmp = f"{af}.tmp.{secrets.token_hex(4)}"
    w = await _store_call(
        agent,
        store_id,
        {
            "type": "write_stream",
            "path": tmp,
            "bytes": body,
            "truncate": True,
        },
    )
    if not isinstance(w, dict) or w.get("error"):
        # Leave the previous record untouched; a debris tmp is visible in `list`.
        _persist_failed(agent, store_id, af, w)
        return
    mv = await _store_call(
        agent,
        store_id,
        {
            "type": "rename",
            "old_path": tmp,
            "new_path": af,
        },
    )
    if not isinstance(mv, dict) or mv.get("error"):
        _persist_failed(agent, store_id, af, mv)
        return
    _persist_landed(agent)


async def _forget_via_store(agent, store_id: str, abs_path: Path) -> None:
    """Recursively remove one agent's dir THROUGH the provider (never the root)."""
    reldir = _store_reldir(agent, abs_path)
    if not reldir:
        return  # never remove the root
    out = await _store_call(
        agent, store_id, {"type": "delete", "path": reldir, "recursive": True}
    )
    if isinstance(out, dict) and out.get("error"):
        _persist_failed(agent, store_id, reldir, out)


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
        self._drain = asyncio.Lock()  # one drain at a time; flushes wait FIFO
        self._unsub = None
        self._task: asyncio.Task | None = None

    def start(self) -> None:
        for a in self.kernel.agents.values():
            self._paths[a.id] = a._root_path
        self._unsub = self.kernel.add_state_subscriber(self._on_state)
        # REGISTERED, and KEEPALIVE: the persistence writer must outlive the
        # quiet phase of `shutdown_agents` (it is what flushes what shutdown
        # produced). A bare task left every write it made — read_stream,
        # write_stream, rename on the store — rowed under the dispatch that
        # booted the loader, which ends immediately.
        tid = self.kernel.spawn_task(
            self.agent.id, "persist_writer", self._run(), keepalive=True
        )
        self._task = self.kernel.tasks[tid]["_task"]

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
        """Drain the dirty queues once — the loop's body AND what `flush_pending` /
        a one-shot awaits.

        ONE DRAIN AT A TIME. When it returns, every write that was dirty when it was
        called has LANDED — also the ones the debounced loop took first. It used to
        return at once when the loop held them, with their writes still in flight:
        the flush came back before the record was on disk (measured: a 0.152–0.160 s
        gap between `create` and `aflush`, just past the debounce, left `agent.yaml`
        unwritten)."""
        async with self._drain:
            store_id = _find_store(self.agent)
            if store_id is None:
                return  # no provider wired → records stay queued. NO fallback.
            persist_ids = list(self._persist.keys())
            self._persist.clear()
            forget = dict(self._forget)
            self._forget.clear()
            # The loader streaming the live tree to its OWN store is persistence
            # bookkeeping — each store call runs directly (`_store_call`), never
            # against the root's own grant (the bootstrap root carries none).
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
        # NO FINAL FLUSH HERE. `stop` runs mid-teardown, when the store may already
        # be down; the last write happens in `flush_pending`, before the walk.


# ─── verbs ─────────────────────────────────────────────────────────


async def _reflect(id, payload, agent):
    """Identity + the disk root it owns. No args."""
    return {
        "id": id,
        "sentence": "Disk persistence/hydration root — owns .fantastic; "
        "answers load_tree / persist_record / forget_record.",
        "root": str(_root(agent)),
    }


def reflect_root_extra(agent, *, full: bool = True) -> dict:
    """Duck-typed root-reflect contribution (the substrate merges it on the ROOT only).
    Surfaces WHICH file_bridge the loader auto-persists THROUGH — the discovered store
    provider, or `null` when none is wired (state stays in RAM). The provider's own
    posture (open / sealed) is visible inline in the tree node, so a client reads, in
    ONE reflect, whether persistence is wired AND whether the wired leg is actually
    open — no silent RAM/sealed surprise, no kernel guessing.

    Also surfaces the kernel's stable instance `guid` — the persistence-scoped
    identity minted ONCE at boot and persisted into the root record's meta (unlike
    run-scoped runtime/env/version, which are read inline from the environment each
    reflect). Sourced from the persisted meta so it round-trips a reboot verbatim.

    Also surfaces the always-on doc-tree FRAME: `readmes` (the top-level
    token-annotated topic index) + `docs` (a one-liner teaching keyed fetch).
    So the manual's map is in front of a caller from the first root reflect —
    it drills with `readme=index` / `readme=<key>` instead of eating a monolith."""
    err = getattr(agent, "_persist_error", None)
    persistence = {"provider": None if err else _find_store(agent)}
    if err:
        persistence["error"] = err
    base = {
        "persistence": persistence,
        "guid": agent.record.get("guid"),
    }
    if not full:
        # DISTILLED: the doc frame is 1,267 tok of inlined manual (readmes + docs +
        # the long self_extension prose) that `readme=index` serves on demand, and
        # the reply's `more` line names it.
        #
        # But the SHAPE of a durable program cannot be left to a readme, because a
        # model that lacks it writes something that cannot work — and the weak models
        # are exactly the ones that do not follow the pointer to go read it. A durable
        # program is a FILE plus a command; the two halves a model has to get right:
        #
        # 1. THE FILE IS BYTES, NOT JSON. A program squeezed into a JSON string is
        #    lost to one stray quote. The
        #    program's bytes ride a STREAM — `write_stream`, whose `bytes` is verbatim — so
        #    nothing is escaped. A model told only "write the program" reaches for a
        #    JSON string; the string names the stream path so it does not.
        #
        # 2. RUNNING IT IS A COMMAND. The executor is a general-purpose COMMAND runner
        #    gated by `allowed` regexes (no default — `start` refuses when the record
        #    lists none) plus `ingress_rule` (a leg is SEALED by default, so a leg
        #    created without it is silently inert — `create_agent` discards its `boot`
        #    reply, so the denial is never even seen). The WHICH-interpreter and the
        #    allowlist are the operator's, not ours, so the pointer names WHERE to
        #    read the gate (`reflect` the leg) instead of inventing a path.
        #
        # An affordance whose CONTRACT you do not know is not usable, and that costs a
        # whole run. Prose stays behind `readme:'self-extension'`; this shape does not.
        base["self_extension"] = (
            "A DURABLE PROGRAM is a FILE plus a command (no `code` field): write it "
            "with a file agent's <type>write_stream</type> (`bytes` verbatim), run it on "
            "an `exec_bridge.tools` agent — <type>start</type> <command>python3</command> "
            "<args><item>prog.py</item></args>, matched against its `allowed` — and "
            "re-run to update. Wiring an agent to another takes BOTH halves: its "
            "`can_call` and the other's `callable_by`. More: reflect kernel with "
            "<readme>self-extension</readme>."
        )
        return base
    return {
        **base,
        "readmes": agent._readme_index(),
        "docs": (
            "How to CALL an agent: send 'kernel' <type>docs</type> <of>agent id, "
            "bundle or kernel</of> — one signature line per verb; add "
            "<verb>name</verb> <tier>detailed</tier> for one verb's full text. The "
            "manual is a keyed doc tree: send 'kernel' <type>reflect</type> with "
            "<readme>index</readme> (every topic, ~token cost), <readme>key</readme> "
            "(one topic) or <readme>true</readme> (the short orientation). Read only "
            "what a task needs."
        ),
        "self_extension": (
            "A DURABLE PROGRAM is a FILE you write plus a command that runs it — "
            "not a `code`-carrying agent (there is no `code` field). A program "
            "carried as a JSON string is exactly how a call gets lost to escaping. "
            "Assemble these THREE pieces and you have built something that "
            "persists and re-runs:\n"
            "1. WRITE THE PROGRAM TO A FILE: never carry file contents in a JSON "
            "string. Use the file leg's STREAM verbs — <type>write_stream</type> <path>…</path> "
            "<truncate>true</truncate> <bytes>…</bytes> to write, <type>read_stream</type> <path>…</path> <offset>0</offset> to "
            "read, one raw chunk at a time. `bytes` is VERBATIM: the program between "
            "`<bytes>` and `</bytes>` exactly as written, real newlines, nothing escaped. The "
            "text verbs (`write`, `edit`, `append`) take `bytes` the same way.\n"
            "2. RUN IT: the executor is a general-purpose COMMAND runner. Reflect "
            "it, read its `allowed` regexes, then send "
            "<type>start</type> <command>path</command> <args><item>…</item></args>; poll <type>status</type>. "
            "It runs commands, not inline programs.\n"
            "3. RE-RUN: the file is durable on disk, so the SAME `start` command run "
            "again later re-executes the SAME program against whatever the inputs "
            "hold THEN — that is how a thing 'keeps itself up to date'.\n"
            "A WORKER THAT COMPUTES is an `exec_bridge.tools` agent (the EXISTING "
            "command runner) that owns a program file — you never author a new "
            "handler_module (you cannot add modules to the catalog and do not need "
            "to). 'A specialist with child workers' = a parent agent whose children "
            "are exec_bridge runners, each with its own program file.\n"
            "WIRING TO A SIBLING: a record is edited with `update_agent` on the root — "
            "holding that grant (both ends) you edit ANY record, `totals`/`readings` "
            "included; without it you cannot raise your OWN authority. To let your "
            "worker call `totals`: grant the worker `can_call:{totals}` AND "
            "`update_agent id='totals'` to add the worker to `totals.callable_by` "
            "(send the WHOLE map — update replaces, not merges).\n"
            "A SPAWNED PROGRAM reaches agents over its control fd — a TWO-WAY socket "
            "(FANTASTIC_CTRL_FD): write {op:'send', rid, target, payload}; the reply "
            "comes back on the SAME fd as {op:'reply', rid, data}. It is not inline "
            "— read frames until one carries your `rid`, skipping event/inbox frames.\n"
            "COMPOSE the agents you were given for everything else (`list_agents`, "
            "reflect each, send). CREATE/DELETE plain agents (create_agent with a "
            "handler_module and grants, delete_agent) work as before."
        ),
    }


async def _boot(id, payload, agent):
    """Start the auto-flush loop unless `watch=false` (a loader serving a
    remote kernel only answers verbs). Subscribes AFTER the bootstrap's
    `kernel.load`, so hydration's bulk `added` events don't re-flush what
    was just read. Idempotent.

    A SESSION loader (a `root` meta pointing at a sub-namespace like
    `.fantastic/web/<session>/`) writes no record of its own there: its one
    record lives in its parent's tree, and `load_tree` walks the namespace from
    it (owner 2026-10-14 — a copy at `<root>/agent.yaml` froze and diverged)."""
    rec = agent.record
    if rec.get("watch", True) is False:
        return
    if getattr(agent, "_fs_flush_loop", None) is not None:
        return
    _check_store_allow(agent)
    loop = _FlushLoop(agent)
    agent._fs_flush_loop = loop
    loop.start()
    # Mint the kernel's stable instance guid ONCE (root only), AFTER the flush loop
    # subscribes so `kernel.update`'s "updated" event is caught → write_stream →
    # .fantastic/agent.yaml (no new persistence code). The seal, exactly as
    # relay_connector mints its WS-path guid: auto-gen only FILLS an absent field
    # (an explicit/existing guid always wins), and once written back it is never
    # regenerated — the next hydration reads it verbatim, so the identity is stable
    # across reboots. An in-memory (no store wired) boot still mints it into the
    # record (just not persisted), so root reflect always carries a guid.
    if agent.parent is None and not agent.record.get("guid"):
        agent.ctx.update(agent.id, guid=uuid.uuid4().hex)
    return


async def _load_tree(id, payload, agent):
    """args: none. Read this loader's subtree from disk → {records, version}.
    Flat list; parent_id encodes structure. Reads `root` if set (a session
    loader serving a sub-namespace), else the loader's own dir."""
    root_dir = _root(agent)
    live = agent.record if root_dir != agent._root_path else None
    return {
        "records": read_tree(fs.Root(agent.ctx.workdir, root_dir), live),
        "version": CURRENT_VERSION,
    }


def _guard_parent_chain(verb: str, parent_id: object) -> dict | None:
    """`_resolve_path` walks the parent chain and joins EVERY link into the
    path, so a poisoned `parent_id` escapes exactly as a poisoned `id` does."""
    if parent_id is None:
        return None
    return guard_agent_id(verb, parent_id)


def _guard_own_tree(verb: str, agent) -> dict | None:
    """The ROOT loader's tree is the one this kernel HYDRATES at boot, so only the
    operator (its own flush, the CLI, boot) writes it. An agent's record changes
    through `update_agent`, gated like any verb — a record written here
    by an agent would be hydrated with whatever grants it named (audit
    2026-09-28). A session loader (a remote's sub-namespace, never hydrated here)
    keeps serving its remote."""
    if agent.parent is None and current_origin() is not None:
        return refusal(
            UNAUTHORIZED,
            f"{verb}: the kernel's own tree is written only by the operator — "
            "an agent's record changes through update_agent",
            hint="update_agent id=<id> …  (needs update_agent granted on the root)",
            side="store",
        )
    return None


async def _persist_record(id, payload, agent):
    """args: record:dict (req). Merge-write one agent.yaml. Location is the
    live agent's _root_path if present, else computed under this loader's
    root from the record's parent chain."""
    record = payload.get("record")
    if (denied := _guard_own_tree("persist_record", agent)) is not None:
        return denied
    if not isinstance(record, dict) or not record.get("id"):
        return missing("persist_record", "record (dict with id)")
    # DEFENCE IN DEPTH. `create_agent` guards the id at the mint, but this verb
    # takes one straight from the PAYLOAD — and a foreign record arrives here
    # from a bridge/session loader, a lower-trust caller. An id becomes a
    # directory, so an unvalidated one writes wherever it points.
    if (bad_id := guard_agent_id("persist_record", record["id"])) is not None:
        return bad_id
    if (
        bad_pid := _guard_parent_chain("persist_record", record.get("parent_id"))
    ) is not None:
        return bad_pid
    a = agent.ctx.get_agent(record["id"])
    path = a._root_path if a is not None else _resolve_path(agent, record)
    write_record(fs.Root(agent.ctx.workdir, path), record)
    return {"ok": True}


async def _forget_record(id, payload, agent):
    """args: id:str (req), parent_id:str (opt). rmtree the agent's dir. For a
    live host agent the path is its own `_root_path`; for a foreign (bridge-
    sent) record the `parent_id` lets the loader resolve the NESTED path under
    its root — a removed agent is no longer in the tree to walk."""
    if (denied := _guard_own_tree("forget_record", agent)) is not None:
        return denied
    target = payload.get("id")
    if not target:
        return missing("forget_record", "id")
    # This verb ends in `rmtree`. Measured 2026-08-28: an agent with NO explicit
    # grant sent id="../../important_project_dir" and recursively deleted a
    # directory outside the store entirely.
    if (bad_id := guard_agent_id("forget_record", target)) is not None:
        return bad_id
    if (
        bad_pid := _guard_parent_chain("forget_record", payload.get("parent_id"))
    ) is not None:
        return bad_pid
    a = agent.ctx.get_agent(target)
    if a is not None:
        path = a._root_path
    else:
        path = _resolve_path(
            agent, {"id": target, "parent_id": payload.get("parent_id")}
        )
    rmtree(fs.Root(agent.ctx.workdir, path))
    return {"ok": True}


# ─── arbitrary-blob persistence (the loader as the ONE store authority) ────


_RESERVED_BLOB_NAMES = {_RECORD_FILE}


def _guard_blob_name(name) -> str | None:
    """Validate a blob filename: a single bare segment, not a kernel-managed file,
    no escape/hidden. Returns an error string, or None when OK. Defense-in-depth
    over the provider's clamp so a bad `name` fails AT the loader with a clear,
    loader-scoped message instead of a deep fs error."""
    if not isinstance(name, str) or not name:
        return "name (non-empty str) required"
    if name != Path(name).name:
        return f"name {name!r} must be a single filename (no path segments)"
    if name.startswith(".") or ".." in name:
        return f"name {name!r} must not be hidden or contain '..'"
    if name.lower() in _RESERVED_BLOB_NAMES:
        return f"name {name!r} is reserved (kernel-managed)"
    return None


def _blob_reldir(agent, agent_id: str) -> str:
    """Store-relative dir for an agent's sidecars — `agents/<id>`. Prefers the LIVE
    agent's own `_root_path`, falls back to walking the record's parent chain."""
    a = agent.ctx.get_agent(agent_id)
    if a is not None:
        abs_dir = a._root_path
    else:
        rec = agent.ctx.get(agent_id) or {}
        abs_dir = _resolve_path(
            agent, {"id": agent_id, "parent_id": rec.get("parent_id")}
        )
    return _store_reldir(agent, abs_dir)


def _decoded_blob(name: str, raw: bytes) -> dict:
    """Decode a sidecar STRICTLY. `errors="replace"` turned a non-text file into
    U+FFFD mojibake and handed it back as `content` — the caller could not tell a
    corrupt read from a real one, and writing that back would destroy the file.
    A sidecar is text by contract; anything else is a refusal naming the byte."""
    try:
        return {"name": name, "content": raw.decode("utf-8")}
    except UnicodeDecodeError as e:
        return {
            "error": f"load_blob: {name} is not utf-8 at byte {e.start}",
            "reason": "bad_encoding",
            "name": name,
            "content": None,
        }


async def persist_blob(agent, aid: str, name: str, content: str) -> dict:
    """Persist agent `aid`'s sidecar `name` THROUGH the loader `agent`'s
    discovered store — a whole-file write at `agents/<aid>/<name>`. The DIRECT
    function behind the `persist_blob` verb: a bundle persisting its own sidecar
    (the `scheduler`) calls it as a plain function — persistence through the loader
    is bookkeeping, never a gated send. No store wired ⇒ error (NO RAM fallback)."""
    if err := _guard_blob_name(name):
        return {"error": f"persist_blob: {err}"}
    store_id = _find_store(agent)
    if store_id is None:
        return {"error": "persist_blob: no store wired — persistence not available"}
    reldir = _blob_reldir(agent, aid)
    path = f"{reldir}/{name}" if reldir else name
    reply = await _store_call(
        agent,
        store_id,
        {
            "type": "write_stream",
            "path": path,
            "bytes": content.encode("utf-8"),
            "truncate": True,
        },
    )
    if isinstance(reply, dict) and reply.get("error"):
        out = {"error": f"persist_blob: {reply['error']}"}
        if reply.get("reason"):
            out["reason"] = reply["reason"]
        if reply.get("hint"):
            out["hint"] = reply["hint"]
        return out
    return {
        "ok": True,
        "name": name,
        "size": reply.get("size") if isinstance(reply, dict) else None,
    }


async def load_blob(agent, aid: str, name: str) -> dict:
    """Read agent `aid`'s sidecar `name` THROUGH the loader `agent`'s discovered
    store — the DIRECT function behind the `load_blob` verb (see `persist_blob`).
    Returns {name, content:str|null} — null when the file is absent."""
    if err := _guard_blob_name(name):
        return {"error": f"load_blob: {err}"}
    store_id = _find_store(agent)
    if store_id is None:
        return {"error": "load_blob: no store wired — persistence not available"}
    reldir = _blob_reldir(agent, aid)
    path = f"{reldir}/{name}" if reldir else name
    # PUMP TO EOF. `read_stream` is a CHUNK verb — `file_bridge` caps a chunk at
    # 65536 bytes — so a single call returns a PREFIX of any larger sidecar, and
    # this used to return that prefix as if it were the file. Measured: a 100_000
    # byte `state.yaml` round-tripped as 65_536 bytes, silently, with no error on
    # any channel. A memory agent's whole point is that what it wrote is what it
    # reads back, so a truncating read is data loss dressed as a successful load.
    chunks: list[bytes] = []
    offset = 0
    while True:
        reply = await _store_call(
            agent,
            store_id,
            {"type": "read_stream", "path": path, "offset": offset},
        )
        if not isinstance(reply, dict) or not isinstance(
            reply.get("bytes"), (bytes, bytearray)
        ):
            break
        chunks.append(bytes(reply["bytes"]))
        if reply.get("eof"):
            return _decoded_blob(name, b"".join(chunks))
        nxt = reply.get("next_offset")
        if not isinstance(nxt, int) or nxt <= offset:
            # No forward progress: stop rather than spin. Returning what we have
            # would be the truncation this loop exists to prevent, so say so.
            return {
                "error": f"load_blob: {name} stalled at offset {offset}"
                " — the store returned no forward progress",
                "name": name,
                "content": None,
            }
        offset = nxt
    if chunks:
        return _decoded_blob(name, b"".join(chunks))
    if isinstance(reply, dict) and reply.get("reason") == "unauthorized":
        out = {
            "error": f"load_blob: {reply.get('error', 'unauthorized')}",
            "reason": "unauthorized",
        }
        if reply.get("hint"):
            out["hint"] = reply["hint"]
        return out
    return {"name": name, "content": None}  # absent → empty (NOT an error)


async def _persist_blob(id, payload, agent):
    """args: agent_id:str (req), name:str (req), content:str (req). Persist an
    agent's sidecar (e.g. state.yaml) THROUGH the loader's DISCOVERED store — a
    whole-file write at `agents/<agent_id>/<name>`. A dumb BYTE authority: NO data
    semantics (the caller owns its format). The consumer never computes a path or
    wires a provider. No store wired ⇒ error (NO RAM fallback)."""
    aid = payload.get("agent_id")
    name = payload.get("name")
    content = payload.get("content")
    if not isinstance(aid, str) or not aid:
        return {"error": "persist_blob: agent_id (str) required"}
    if err := _guard_blob_name(name):
        return {"error": f"persist_blob: {err}"}
    if not isinstance(content, str):
        return {"error": "persist_blob: content (str) required"}
    return await persist_blob(agent, aid, name, content)


async def _load_blob(id, payload, agent):
    """args: agent_id:str (req), name:str (req). Read an agent's sidecar THROUGH the
    loader's discovered store. Returns {name, content:str|null} — null when the file
    is absent (a never-written sidecar reads empty). Surfaces a gate denial as an
    error; the consumer decides whether to treat that as empty."""
    aid = payload.get("agent_id")
    name = payload.get("name")
    if not isinstance(aid, str) or not aid:
        return {"error": "load_blob: agent_id (str) required"}
    if err := _guard_blob_name(name):
        return {"error": f"load_blob: {err}"}
    return await load_blob(agent, aid, name)


async def flush_pending(agent):
    """The kernel's shutdown hook (awaited before the teardown walk, PROVIDER STILL
    ALIVE): write the pending RECORDS.

    THE RECORD FLUSH LIVES HERE, not in `on_shutdown`. The record loop debounces
    0.15 s; a `create_agent` + `update_agent` followed straight away by
    `shutdown()` used to reach its final flush mid-teardown-walk, with the store
    already gone, and the update never landed."""
    loop = getattr(agent, "_fs_flush_loop", None)
    if loop is not None:
        await loop.aflush()
    # DEBUG FILE FLUSH — a tree with no record store lives in RAM; with
    # `FANTASTIC_DEBUG_FLUSH=1` (set by the e2e harness, never by default) every
    # non-ephemeral record lands in its `agent.yaml` at stop, so a test restart
    # boots the tree it stopped. The wire twin of `_testkit.persist`.
    if os.environ.get("FANTASTIC_DEBUG_FLUSH") == "1":
        for a in list(agent.ctx.agents.values()):
            if not type(a).ephemeral:
                write_record(fs.Root(agent.ctx.workdir, a._root_path), a.record)


async def on_shutdown(agent):
    """Unsubscribe + stop the record loop. `flush_pending` normally ran already (the
    kernel calls it before the walk, while the provider is alive) and is what
    WRITES."""
    loop = getattr(agent, "_fs_flush_loop", None)
    if loop is not None:
        await loop.stop()


async def _reload_bundle(id, payload, agent):
    """args: name:str (req). Re-read `bundles/<name>/bundle.yaml` (a new folder loads) and restart every live instance of it, abruptly — an in-flight call is answered with an error naming the reload. Returns {reloaded, instances}. The root answers it.

    A folder that no longer loads is refused naming the folder and field; its live
    instances keep running the previous load until a reload succeeds."""
    if agent.parent is not None:
        return refusal(
            NOT_HERE,
            "reload_bundle: the kernel's bundles are the ROOT loader's",
            hint="send it to kernel",
        )
    name = payload.get("name")
    if not isinstance(name, str) or not name:
        return missing(
            "reload_bundle", "name", hint="a folder under .fantastic/bundles/"
        )
    root_dir = fs.Root(agent.ctx.workdir, _root(agent))
    rel = f"{_BUNDLES_DIR}/{name}"
    if not (_FOLDER_NAME.fullmatch(name) and fs.exists(root_dir, rel)):
        return refusal(
            NOT_FOUND,
            f"reload_bundle: no folder {Path(str(root_dir.rel)).name}/{rel}",
            hint="a bundle is a folder under .fantastic/bundles/ with a bundle.yaml",
        )
    err = load_bundle(root_dir, name)
    if err is not None:
        return refusal(INVALID_META, err)
    live = sorted(
        (a for a in agent.ctx.agents.values() if a.handler_module == name),
        key=lambda a: a.id,
    )
    # EACH INSTANCE, AS ITSELF (kernel bookkeeping): `shutdown` (the reason rides
    # it — an in-flight call is answered naming the reload), then `boot` under the
    # freshly loaded folder.
    for a in live:
        await agent.ctx.bookkeeping(a, {"type": "shutdown", "reason": "reloaded"})
        await agent.ctx.bookkeeping(a, {"type": "boot"})
    return {"reloaded": name, "instances": [a.id for a in live]}


async def _read_bundles(id, payload, agent):
    """args: name:str?. The folder bundles under this loader's root AS TEXT — {bundles:[{name, folder, text?, error?}]}, each `bundles/<name>/bundle.yaml` raw (`error` when it has none); `name` reads one. For a kernel that loads its own folders through this loader (a page kernel's proxy_loader); this loader parses nothing."""
    root_dir = fs.Root(agent.ctx.workdir, _root(agent))
    shown = Path(str(root_dir.rel)).name
    name = payload.get("name")
    if name is not None and (not isinstance(name, str) or not name):
        return refusal(BAD_ARG, "read_bundles: name must be a folder name", arg="name")
    if name is not None:
        if not (
            _FOLDER_NAME.fullmatch(name)
            and fs.exists(root_dir, f"{_BUNDLES_DIR}/{name}")
            and fs.is_dir(root_dir, f"{_BUNDLES_DIR}/{name}")
        ):
            return refusal(
                NOT_FOUND,
                f"read_bundles: no folder {shown}/{_BUNDLES_DIR}/{name}",
                hint=f"a bundle is a folder under {shown}/{_BUNDLES_DIR}/ with a bundle.yaml",
            )
        names = [name]
    elif fs.exists(root_dir, _BUNDLES_DIR) and fs.is_dir(root_dir, _BUNDLES_DIR):
        names = sorted(
            e.name for e in fs.list_dir(root_dir, _BUNDLES_DIR) if e.is_dir()
        )
    else:
        names = []
    out = []
    for n in names:
        rel = f"{_BUNDLES_DIR}/{n}"
        row = {"name": n, "folder": f"{shown}/{rel}"}
        if fs.exists(root_dir, f"{rel}/{_BUNDLE_FILE}"):
            row["text"] = fs.read_text(root_dir, f"{rel}/{_BUNDLE_FILE}")
        else:
            row["error"] = f"bundles/{n}: no bundle.yaml"
        out.append(row)
    return {"bundles": out}


VERBS = {
    "reflect": _reflect,
    "boot": _boot,
    "reload_bundle": _reload_bundle,
    "read_bundles": _read_bundles,
    "load_tree": _load_tree,
    "persist_record": _persist_record,
    "forget_record": _forget_record,
    "persist_blob": _persist_blob,
    "load_blob": _load_blob,
}


async def handler(id: str, payload: dict, agent) -> dict | None:
    fn = VERBS.get(payload.get("type"))
    if fn is None:
        # CODED. A reason-less error on the ROOT — the most-addressed agent in
        # the kernel — was decorated by `_apply_reflect_flags` into something
        # that reads like a healthy reflect (2026-09-02).
        t = payload.get("type")
        return refusal(UNHANDLED, f"kernel_state: unknown type {t!r}", verb=t)
    return await fn(id, payload, agent)
