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
import secrets
import uuid
from pathlib import Path
from typing import Any

from file_bridge import fs

from kernel_state import messages_xml

from kernel import CURRENT_VERSION, operator_context
from kernel import current_origin
from kernel._refusal import UNAUTHORIZED, UNHANDLED, guard_agent_id, missing, refusal

# ─── disk I/O via `fs` (the ONE disk surface) — bootstrap cold primitives ──
# These run before any agent exists, so they call `fs`'s functions directly (not the
# file_bridge AGENT). Still the same clamped surface: every read/write goes through
# `fs`, so nothing here escapes the running dir.


_MESSAGES_FILE = "messages.xml"


def read_messages(root_dir: Path) -> tuple[list[dict], int]:
    """COLD HYDRATION of the kernel's message store — `(rows, next_id)`.

    The message log is `.fantastic/messages.xml`: ONE XML document whose root is
    never closed, one `<row>` element per row the kernel ever made (a turn, a
    delivered reply, a fold, a recall, a `body` — the text merged in — a `view`, a
    `call`, a `task`), in id order, exactly as the pump appended it (format:
    `messages_xml`). Read through `fs` (clamped), before any agent exists, like
    `read_tree`. The bootstrap hands the rows to `Kernel.rows_put` FROM ABOVE and
    sets `next_id` — the kernel starts empty every time and never guesses a counter;
    this is the hydrator's logic. A file of another version, or a cut row, is a LOUD
    error — nothing is skipped or repaired. A second ingestion of the same file is
    the user's fault: `rows_put` raises on the first duplicate id."""
    rd = str(root_dir)
    if not fs.exists(rd, _MESSAGES_FILE):
        return [], 1
    text = fs.read_text(rd, _MESSAGES_FILE)
    head = text.split("\n", 1)[0].strip()
    if head != messages_xml.HEADER.strip():
        raise RuntimeError(
            f"{_MESSAGES_FILE}: unknown log version — first line is {head!r}, "
            f"this loader writes {messages_xml.HEADER.strip()!r}"
        )
    rows = [
        r for r in messages_xml.parse_messages(text) if isinstance(r.get("id"), int)
    ]
    top = max((r["id"] for r in rows), default=0)
    return rows, top + 1


def hydrate_messages(kernel, root_dir: Path) -> int:
    """Apply `read_messages` to a kernel FROM ABOVE, before its first mint. Sets the
    counter first so every hydrated id is a pre-minted one. Returns the row count."""
    rows, next_id = read_messages(root_dir)
    kernel.next_id = max(kernel.next_id, next_id)
    for r in rows:
        kernel.rows_put(r["agent"], r)
    # REPLAY THE ENDINGS. A settle is its own row, so the call it settles comes
    # back off disk as it was WRITTEN — at launch, with no outcome. Stamping it
    # here rebuilds the state a live kernel had, and it is the only reason the
    # log can answer "how did this end" for a call that settled after the
    # pump's debounce (which is most of them: every AI turn, every job).
    for r in rows:
        if r.get("kind") != "settle":
            continue
        call = kernel.rows.get(r.get("parent"))
        if call is not None and r.get("outcome") is not None:
            call["outcome"] = r["outcome"]
    return len(rows)


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
    """Copy a bundle's shipped `readme.md` AND its `docs/` topic tree into the
    agent's dir on first persist, THROUGH `fs`. Copy-if-missing per file (never
    clobbers operator edits). The SOURCE is a package resource (installed code via
    importlib, not user disk); only the WRITE goes through `fs`. The `docs/` tree
    is what `reflect readme=index`/`readme=<key>` serves back."""
    if not handler_module:
        return
    rp = str(root_path)
    pkg = handler_module.split(".")[0]
    try:
        base = importlib.resources.files(pkg)
        readme = base / "readme.md"
        if readme.is_file() and not fs.exists(rp, "readme.md"):
            fs.write_text(rp, "readme.md", readme.read_text(encoding="utf-8"))
        _seed_docs_local(base / "docs", rp, "docs")
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


def _seed_docs_local(src_dir, rp: str, rel: str) -> None:
    """Recursively copy a `docs/` topic tree (`.md` files; nested dirs = tiers)
    into the agent's dir THROUGH `fs`. Copy-if-missing; `src_dir` is an importlib
    Traversable (absent ⇒ `is_dir()` False ⇒ no-op)."""
    try:
        if not src_dir.is_dir():
            return
    except (FileNotFoundError, OSError, AttributeError):
        return
    for child in src_dir.iterdir():
        childrel = f"{rel}/{child.name}"
        if child.is_dir():
            _seed_docs_local(child, rp, childrel)
        elif (
            child.is_file()
            and child.name.endswith(".md")
            and not fs.exists(rp, childrel)
        ):
            fs.write_text(rp, childrel, child.read_text(encoding="utf-8"))


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
    try:
        want = _root(agent).resolve()
    except (OSError, ValueError):
        return None
    base = Path.cwd().resolve()
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
    got = (
        await kernel.send(
            [{"target": store_id, "payload": {"type": "read_stream", "path": af}}]
        )
    )[0]["result"]
    if isinstance(got, dict) and isinstance(got.get("bytes"), (bytes, bytearray)):
        try:
            parsed = json.loads(bytes(got["bytes"]))
            if isinstance(parsed, dict):
                existing = parsed
        except (ValueError, json.JSONDecodeError):
            existing = {}
    existing.update(record)
    body = json.dumps(existing, indent=2).encode("utf-8")
    # ATOMIC COMMIT: stream into a tmp beside the target, then RENAME. A
    # `write_stream` that dies after truncating leaves a half-written or empty
    # `agent.json` — the record is gone and the next boot hydrates a tree with a
    # hole in it. `rename` is the leg's own single atomic op, and it creates
    # parents, so the failure modes are "the old record survives" or "the new one
    # is complete", never a torn file. Same protocol `text_file._write_atomic`
    # uses for a document.
    tmp = f"{af}.tmp.{secrets.token_hex(4)}"
    w = (
        await kernel.send(
            [
                {
                    "target": store_id,
                    "payload": {
                        "type": "write_stream",
                        "path": tmp,
                        "bytes": body,
                        "truncate": True,
                    },
                }
            ]
        )
    )[0]["result"]
    if isinstance(w, dict) and w.get("error"):
        # Leave the previous record untouched; a debris tmp is visible in `list`.
        return
    (
        await kernel.send(
            [
                {
                    "target": store_id,
                    "payload": {
                        "type": "rename",
                        "old_path": tmp,
                        "new_path": af,
                    },
                }
            ]
        )
    )[0]["result"]
    await _seed_readme_via_store(agent, store_id, reldir, record.get("handler_module"))


async def _seed_readme_via_store(
    agent, store_id: str, reldir: str, handler_module: str | None
) -> None:
    """Copy-if-missing the bundle's shipped readme.md AND its `docs/` topic tree
    into the agent's dir THROUGH the provider. Source is the installed package
    (in-process resource); sink is the stream provider."""
    if not handler_module:
        return
    kernel = agent.ctx
    pkg = handler_module.split(".")[0]
    prefix = f"{reldir}/" if reldir else ""
    try:
        base = importlib.resources.files(pkg)
        await _seed_doc_via_store(
            kernel, store_id, base / "readme.md", f"{prefix}readme.md"
        )
        await _seed_docs_via_store(kernel, store_id, base / "docs", f"{prefix}docs")
    except (ModuleNotFoundError, FileNotFoundError, OSError, TypeError, AttributeError):
        pass


async def _seed_doc_via_store(kernel, store_id: str, src, path: str) -> None:
    """Copy one `.md` file THROUGH the provider, copy-if-missing (never clobber)."""
    if not src.is_file():
        return
    got = (
        await kernel.send(
            [{"target": store_id, "payload": {"type": "read_stream", "path": path}}]
        )
    )[0]["result"]
    if isinstance(got, dict) and isinstance(got.get("bytes"), (bytes, bytearray)):
        return
    (
        await kernel.send(
            [
                {
                    "target": store_id,
                    "payload": {
                        "type": "write_stream",
                        "path": path,
                        "bytes": src.read_text(encoding="utf-8").encode("utf-8"),
                        "truncate": True,
                    },
                }
            ]
        )
    )[0]["result"]


async def _seed_docs_via_store(kernel, store_id: str, src_dir, rel: str) -> None:
    """Recursively copy a `docs/` topic tree THROUGH the provider (absent ⇒ no-op)."""
    try:
        if not src_dir.is_dir():
            return
    except (FileNotFoundError, OSError, AttributeError):
        return
    for child in src_dir.iterdir():
        childrel = f"{rel}/{child.name}"
        if child.is_dir():
            await _seed_docs_via_store(kernel, store_id, child, childrel)
        elif child.is_file() and child.name.endswith(".md"):
            await _seed_doc_via_store(kernel, store_id, child, childrel)


async def _forget_via_store(agent, store_id: str, abs_path: Path) -> None:
    """Recursively remove one agent's dir THROUGH the provider (never the root)."""
    reldir = _store_reldir(agent, abs_path)
    if not reldir:
        return  # never remove the root
    (
        await agent.ctx.send(
            [
                {
                    "target": store_id,
                    "payload": {"type": "delete", "path": reldir, "recursive": True},
                }
            ]
        )
    )[0]["result"]


# ─── the auto-flush loop (the root subscribes the live tree) ────────


class _MessagePump:
    """PERSIST THE MESSAGE STORE — a tail of the kernel, appended to the provider.

    The kernel exposes `rows_stream(after)`; this pump is one consumer of it. Rows are
    batched (0.15 s debounce, the `_FlushLoop` rhythm) and each batch is ONE
    `write_stream` append of JSON lines through the discovered provider (`_find_store`)
    — no held-open channel exists on the stream verbs, and one send per row would be
    a call row per row. THE PUMP SKIPS ITS OWN CALLS: a flush is a send, so it mints
    a `call` row on the provider; persisting that row would mint another, forever.
    The rule is general: operator-context calls ON THE STORE (the loader's own
    persistence traffic — this pump, the record flusher, the blob verbs) and the
    pump's own `task` rows are bookkeeping of persisting and never enter the log.

    ORDER, by contract: hydrate (bootstrap, cold) → subscribe (here, at `after = the
    highest id present`) → only then anything that creates children. Subscribing
    before hydrating would re-append what was just read; creating children before
    subscribing would lose their first messages.

    No provider ⇒ batches are dropped and said so once (RAM only, by design — the
    same law as records). Swap the sink by wiring a different provider that answers
    `write_stream`; the pump does not know what is behind it."""

    VERBS = ("message_pump", "message_pump_flush")

    def __init__(self, agent) -> None:
        self.agent = agent
        self.kernel = agent.ctx
        self.pending: list[dict] = []
        self.seen = 0  # the highest id the tail has handed us
        self.written = 0  # the highest id landed on the provider
        self._wake = asyncio.Event()
        self._tasks: list[asyncio.Task] = []
        self._said_no_store = False
        # Unknown until the first flush asks the store whether the log exists: a
        # NEW log opens with the root element, an existing one is appended to.
        self._header_needed: bool | None = None

    def start(self, after: int) -> None:
        self.seen = after
        self.written = after
        ids = [
            self.kernel.spawn_task(self.agent.id, verb, coro, keepalive=True)
            for verb, coro in zip(self.VERBS, (self._read(after), self._flush_loop()))
        ]
        self._tasks = [self.kernel.tasks[i]["_task"] for i in ids]

    def _skip(self, row: dict) -> bool:
        store = _find_store(self.agent)
        if row.get("kind") == "task" and row.get("agent") == self.agent.id:
            return row.get("verb") in self.VERBS
        if row.get("kind") == "call" and row.get("agent") == store:
            return row.get("origin") is None  # the loader's own persistence traffic
        # A SETTLE FOLLOWS ITS CALL. The pump's own writes are skipped so that
        # persisting a row does not itself produce a row to persist; their
        # endings must be skipped for the same reason, or the recursion comes
        # back through the other half.
        if row.get("kind") == "settle":
            parent = self.kernel.rows.get(row.get("parent"))
            return parent is not None and self._skip(parent)
        return False

    async def _read(self, after: int) -> None:
        try:
            async for row in self.kernel.rows_stream(after):
                self.seen = max(self.seen, row["id"])
                if self._skip(row):
                    continue
                self.pending.append(row)
                self._wake.set()
        except asyncio.CancelledError:
            pass

    async def _flush_loop(self) -> None:
        try:
            while True:
                await self._wake.wait()
                await asyncio.sleep(0.15)
                self._wake.clear()
                await self.flush()
        except asyncio.CancelledError:
            pass

    async def flush(self) -> None:
        if not self.pending:
            return
        store_id = _find_store(self.agent)
        if store_id is None:
            if not self._said_no_store:
                print("  [messages] no store wired — messages stay in RAM", flush=True)
                self._said_no_store = True
            self.pending.clear()
            return
        batch, self.pending = self.pending, []
        if self._header_needed is None:
            with operator_context():
                probe = await self.kernel.send(
                    [
                        {
                            "target": store_id,
                            "payload": {
                                "type": "read_stream",
                                "path": _MESSAGES_FILE,
                                "offset": 0,
                                "length": 1,
                            },
                        }
                    ]
                )
            pres = (probe[0] or {}).get("result") or {}
            self._header_needed = bool(isinstance(pres, dict) and pres.get("error"))
        lines = "".join(messages_xml.row_to_xml(r) for r in batch)
        if self._header_needed:
            # The root, once, at the head of a NEW log; never closed (a stream).
            lines = messages_xml.HEADER + lines
            self._header_needed = False
        with operator_context():
            out = await self.kernel.send(
                [
                    {
                        "target": store_id,
                        "payload": {
                            "type": "write_stream",
                            "path": _MESSAGES_FILE,
                            "bytes": lines.encode("utf-8"),
                        },
                    }
                ]
            )
        res = (out[0] or {}).get("result") or {}
        if isinstance(res, dict) and res.get("error"):
            print(f"  [messages] append refused: {res.get('error')}", flush=True)
            self.pending = batch + self.pending  # nothing lost; retried next flush
            return
        self.written = max(self.written, max(r["id"] for r in batch))

    async def catch_up(self) -> None:
        """Land every row minted so far. Each flush mints one call row (skipped), so
        the loop converges when the tail has seen everything and nothing pends."""
        for _ in range(1000):
            await asyncio.sleep(0)
            if self.seen >= self.kernel.next_id - 1 and not self.pending:
                return
            await self.flush()
            await asyncio.sleep(0.01)

    async def stop(self) -> None:
        await self.catch_up()
        for t in self._tasks:
            t.cancel()
        self._tasks = []


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
        store_id = _find_store(self.agent)
        if store_id is None:
            return  # no provider wired → records stay queued until one is. NO fallback.
        persist_ids = list(self._persist.keys())
        self._persist.clear()
        forget = dict(self._forget)
        self._forget.clear()
        # The loader streaming the live tree to its OWN store is persistence
        # bookkeeping — it runs with operator authority (like persist_blob), not
        # against the root's own grant (the bootstrap root carries none).
        with operator_context():
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
    base = {
        "persistence": {"provider": _find_store(agent)},
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
        #    lost to one stray quote (the old `code` field's whole failure). The
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
            "A DURABLE PROGRAM is a FILE plus a command — no `code` field. (1) WRITE "
            "it to a file: <type>write_stream</type> <path>…</path> <truncate>true</truncate> <bytes>…</bytes> — `bytes` is "
            "VERBATIM (the program between `<bytes>` and `</bytes>` exactly as written, real newlines, nothing escaped). "
            "(2) RUN it on the COMMAND runner: <type>start</type> <command>…</command> <args><item>…</item></args>, gated by its "
            "`allowed` regexes (reflect it). (3) Re-run the same command to keep it "
            "current. A COMPUTE worker is an `exec_bridge.tools` agent + a program "
            "file — NOT a new handler_module. Create it as YOUR child; to wire it "
            "to a SIBLING (`totals`) grant `can_call:{totals}` AND `update_agent "
            "id='totals'` to add it to `totals.callable_by`. Read <type>reflect</type> "
            "<readme>self-extension</readme> — send it to 'kernel' — for the fd wire."
        )
        return base
    return {
        **base,
        "readmes": agent._readme_index(),
        "docs": (
            "This kernel's manual is a keyed doc tree. To read it, send 'kernel' "
            "a reflect with a string `readme` field: <type>reflect</type> "
            "<readme>index</readme> lists every topic with its ~token cost; "
            "<readme>key</readme> opens one (e.g. self-extension, streams); "
            "<readme>true</readme> is the short orientation. The field is named exactly "
            "`readme` (a string) - not 'readme_key'. Read only what a task needs."
        ),
        "self_extension": (
            "A DURABLE PROGRAM is a FILE you write plus a command that runs it — "
            "not a `code`-carrying agent. There is no `code` field: a program used "
            "to be embedded as a JSON string, which is exactly how a call gets lost "
            "to escaping. Assemble these THREE pieces and you have built something "
            "that persists and re-runs, with no `code` field at all:\n"
            "1. WRITE THE PROGRAM TO A FILE: never carry file contents in a JSON "
            "string. Use the file leg's STREAM verbs — <type>write_stream</type> <path>…</path> "
            "<truncate>true</truncate> <bytes>…</bytes> to write, <type>read_stream</type> <path>…</path> <offset>0</offset> to "
            "read, one raw chunk at a time. `bytes` is VERBATIM: the program between "
            "`<bytes>` and `</bytes>` exactly as written, real newlines, nothing escaped. That is "
            "the only byte path; the string read/write verbs are gone.\n"
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
            "WIRING TO A SIBLING: you cannot raise your OWN authority, but if you "
            "hold `update_agent` (e.g. on the root) you may open ANY agent's ingress "
            "— including a sibling like `totals`/`readings` that is not your child. "
            "To let your worker call `totals`: grant the worker `can_call:{totals}` "
            "AND `update_agent id='totals'` to add the worker to `totals.callable_by` "
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


# ─── the DURABLE execution ledger (C9 across restart) ───────────
#
# Settled rows persist as ONE sidecar (`agents/<root>/ledger.json`) through the
# SAME discovered store the tree flushes through — no store wired ⇒ RAM only,
# the auto-flush law, not a fallback. Only SETTLED rows persist: work in flight
# at a crash never completed, so a re-send after reboot re-executing it is the
# truth, not a loss. Rehydration rebuilds each row with a DONE future, so dedup
# ("a completed step is never re-executed") holds ACROSS REBOOTS — the dbos
# `test_recovery_workflow` property, re-keyed to guarded calls.

_LEDGER_BLOB = "ledger.json"


# v2 (2026-09-08): ids are the kernel's int64 message ids; rows are an ARRAY
# (JSON object keys are strings and would stringify them); a named row carries
# `name`.
_LEDGER_VERSION = 2


def _ledger_dump(kernel) -> str:
    """STRICT serialization — fidelity over completeness (C25). A row whose
    outcome does not survive an exact JSON round-trip (bytes in a read_stream
    result, non-string keys) stays RAM-ONLY rather than persisting corrupted:
    `default=str` used to turn bytes into their repr and rehydrate it as a
    confidently WRONG recorded outcome — worse than no dedup. A caller that
    wants durable bytes stores them through `file_bridge`, the one disk
    surface."""
    rows = []
    for eid, row in kernel.executions.items():
        if not row["fut"].done():
            continue
        outcome = row["fut"].result()
        try:
            if json.loads(json.dumps(outcome)) != outcome:
                continue  # round-trips but not EXACTLY (tuples, int keys)
        except (TypeError, ValueError):
            continue  # not JSON-serializable at all (bytes): RAM-only
        rows.append(
            {
                "id": eid,
                "target": row["target"],
                "retries": row["retries"],
                "attempts": row["attempts"],
                "named": row["named"],
                "name": row.get("name"),
                "origin": row.get("origin"),
                "outcome": outcome,
            }
        )
    return json.dumps({"v": _LEDGER_VERSION, "rows": rows})


async def flush_ledger_now(agent) -> dict:
    """One full write of the settled ledger. Test seam AND the flusher's body."""
    if _find_store(agent) is None:
        return {"skipped": "no store wired"}
    return await _persist_blob(
        agent.id,
        {
            "agent_id": agent.id,
            "name": _LEDGER_BLOB,
            "content": _ledger_dump(agent.ctx),
        },
        agent,
    )


async def _rehydrate_ledger(agent) -> None:
    if _find_store(agent) is None:
        return
    got = await _load_blob(
        agent.id, {"agent_id": agent.id, "name": _LEDGER_BLOB}, agent
    )
    content = (got or {}).get("content")
    if not content:
        return
    try:
        doc = json.loads(content)
    except json.JSONDecodeError as e:
        print(f"  [ledger] {_LEDGER_BLOB} unreadable, starting empty: {e}")
        return
    # THE UPGRADE CONTRACT (C22), from day one: an unknown version starts empty
    # LOUDLY — never guessed at, never silently reinterpreted. A future format
    # change adds a migration branch here, keyed on `v`.
    if doc.get("v") != _LEDGER_VERSION:
        print(
            f"  [ledger] {_LEDGER_BLOB} is version {doc.get('v')!r}, this kernel "
            f"reads {_LEDGER_VERSION} — starting empty (no migration written)"
        )
        return
    rows = doc["rows"]
    loop = asyncio.get_event_loop()
    for r in rows:
        eid = int(r["id"])
        fut = loop.create_future()
        fut.set_result(r["outcome"])
        agent.ctx.ledger_put(
            eid,
            {
                "target": r["target"],
                "fut": fut,
                "payload": None,  # settled: nothing left to re-run
                "timeout": None,
                "retries": r["retries"],
                "attempts": r["attempts"],
                "named": r["named"],
                "name": r.get("name"),
                "origin": r.get("origin"),
            },
        )
    # THE COUNTER is restored by `hydrate_messages` (the bootstrap, cold, before the
    # first mint). The ledger only RAISES it: a settled call whose id the message log
    # never saw (a log that stopped short) must still stay below the next mint.
    if rows:
        ids = [int(r["id"]) for r in rows]
        agent.ctx.next_id = max(agent.ctx.next_id, max(ids) + 1)


def _install_ledger_flusher(agent) -> None:
    """Debounced write-through: a settle kicks one flush task; settles landing
    mid-write coalesce into exactly one follow-up pass."""
    state = {"pending": False, "running": False}

    def kick() -> None:
        if state["running"]:
            state["pending"] = True
            return
        state["running"] = True

        async def run() -> None:
            try:
                while True:
                    state["pending"] = False
                    await flush_ledger_now(agent)
                    if not state["pending"]:
                        return
            finally:
                state["running"] = False

        agent.ctx.spawn_task(agent.id, "ledger_flush", run())

    agent.ctx.ledger_flusher = kick


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
    if agent.parent is None:
        # The ROOT loader owns the kernel-wide execution ledger. Rehydrate
        # BEFORE installing the flusher so hydration never rewrites the file
        # it just read.
        await _rehydrate_ledger(agent)
        _install_ledger_flusher(agent)
        # THE PUMP SUBSCRIBES HERE: after hydration (the bootstrap did it, cold),
        # before anything below creates or wakes children.
        if getattr(agent, "_message_pump", None) is None:
            pump = _MessagePump(agent)
            agent._message_pump = pump
            pump.start(after=agent.ctx.next_id - 1)
    if rec.get("watch", True) is False:
        return
    if getattr(agent, "_fs_flush_loop", None) is not None:
        return
    loop = _FlushLoop(agent)
    agent._fs_flush_loop = loop
    loop.start()
    # Mint the kernel's stable instance guid ONCE (root only), AFTER the flush loop
    # subscribes so `kernel.update`'s "updated" event is caught → write_stream →
    # .fantastic/agent.json (no new persistence code). The seal, exactly as
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
    return {"records": read_tree(_root(agent)), "version": CURRENT_VERSION}


def _guard_parent_chain(verb: str, parent_id: object) -> dict | None:
    """`_resolve_path` walks the parent chain and joins EVERY link into the
    path, so a poisoned `parent_id` escapes exactly as a poisoned `id` does."""
    if parent_id is None:
        return None
    return guard_agent_id(verb, parent_id)


async def _persist_record(id, payload, agent):
    """args: record:dict (req). Merge-write one agent.json. Location is the
    live agent's _root_path if present, else computed under this loader's
    root from the record's parent chain."""
    record = payload.get("record")
    if not isinstance(record, dict) or not record.get("id"):
        return missing("persist_record", "record (dict with id)")
    # DEFENCE IN DEPTH. `create_agent` guards the id at the mint, but this verb
    # takes one straight from the PAYLOAD — and a foreign record arrives here
    # from a bridge/session loader, a lower-trust caller. An id becomes a
    # directory, so an unvalidated one writes wherever it points.
    if (bad_id := guard_agent_id("persist_record", record["id"])) is not None:
        return bad_id
    if (
        denied := _guard_record_owner(
            "persist_record", record["id"], record.get("parent_id"), agent
        )
    ) is not None:
        return denied
    if (
        bad_pid := _guard_parent_chain("persist_record", record.get("parent_id"))
    ) is not None:
        return bad_pid
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
        return missing("forget_record", "id")
    # This verb ends in `rmtree`. Measured 2026-08-28: an agent with NO explicit
    # grant sent id="../../important_project_dir" and recursively deleted a
    # directory outside the store entirely.
    if (bad_id := guard_agent_id("forget_record", target)) is not None:
        return bad_id
    if (
        denied := _guard_record_owner(
            "forget_record", target, payload.get("parent_id"), agent
        )
    ) is not None:
        return denied
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
    rmtree(path)
    return {"ok": True}


# ─── arbitrary-blob persistence (the loader as the ONE store authority) ────


_RESERVED_BLOB_NAMES = {"agent.json", "readme.md"}


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


def _guard_record_owner(
    verb: str, record_id: str, parent_id: object, agent
) -> dict | None:
    """Refuse a RECORD verb that names an agent outside the caller's own subtree.

    Same hole `_guard_blob_owner` closed for sidecars, one field over: the record
    id arrives in the PAYLOAD, so any agent granted `persist_record` on the
    loader could rewrite — or `forget_record` could DELETE — a sibling's record.
    An OPERATOR (`current_origin()` is None: the loader's own flush, the CLI, a
    bridge hydrating at boot) is the bookkeeping this verb exists for and stays
    ungated. An AGENT may name ITSELF or something it owns.
    """
    caller = current_origin()
    if caller is None or caller == record_id:
        return None
    # Walk up from the named record: the payload's `parent_id` first (a foreign
    # record is not in the tree), then the live chain.
    seen: set[str] = set()
    node = parent_id if isinstance(parent_id, str) and parent_id else None
    if node is None:
        node = (agent.ctx.get(record_id) or {}).get("parent_id")
    while node and node not in seen:
        if node == caller:
            return None
        seen.add(node)
        node = (agent.ctx.get(node) or {}).get("parent_id")
    return refusal(
        UNAUTHORIZED,
        f"{caller!r} may not {verb} {record_id!r} — a record verb is scoped to "
        "the caller's own subtree; only an operator may name one outside it",
    )


def _guard_blob_owner(aid: str) -> str | None:
    """Refuse a blob verb that names SOMEONE ELSE'S namespace.

    `agent_id` is the namespace, and it arrives in the PAYLOAD — so without this
    any agent granted `persist_blob` on the loader could write any other agent's
    sidecar. Measured 2026-08-23: an agent holding exactly
    `can_call: {kernel_state: [persist_blob]}` overwrote another agent's
    `state.yaml`, and the victim read the forged content back as its own memory.

    The defence was believed to be POSITION — the root accepts `$children`, and a
    grandchild is not one — but a DIRECT child passes that check, and every agent
    an operator creates at the root is a direct child. Position bounds who can
    reach the verb; it says nothing about which namespace they may name.

    An OPERATOR send (`sender` is None: the loader's own flush, `text_file`
    persisting itself under `operator_context`, the CLI) is the bookkeeping this
    verb exists for and stays ungated — that is the origin root of trust, and an
    agent cannot forge it. An AGENT may only name ITSELF.
    """
    caller = current_origin()
    if caller is None or caller == aid:
        return None
    return (
        f"{caller!r} may not use another agent's namespace {aid!r} —"
        " an agent's blob verbs are scoped to itself; only an operator may name"
        " a namespace it does not own"
    )


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
    if err := _guard_blob_owner(aid):
        return {"error": f"persist_blob: {err}", "reason": "unauthorized"}
    if not isinstance(content, str):
        return {"error": "persist_blob: content (str) required"}
    store_id = _find_store(agent)
    if store_id is None:
        return {"error": "persist_blob: no store wired — persistence not available"}
    reldir = _blob_reldir(agent, aid)
    path = f"{reldir}/{name}" if reldir else name
    # The loader writing to its OWN discovered store is persistence bookkeeping —
    # a side effect of this already-authorized verb, not the loader reaching out
    # as an agent — so it runs with operator authority (like create_agent's boot).
    with operator_context():
        reply = (
            await agent.ctx.send(
                [
                    {
                        "target": store_id,
                        "payload": {
                            "type": "write_stream",
                            "path": path,
                            "bytes": content.encode("utf-8"),
                            "truncate": True,
                        },
                    }
                ]
            )
        )[0]["result"]
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
    if err := _guard_blob_owner(aid):
        return {"error": f"load_blob: {err}", "reason": "unauthorized"}
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
    with operator_context():  # loader reading its own store — see _persist_blob
        while True:
            reply = (
                await agent.ctx.send(
                    [
                        {
                            "target": store_id,
                            "payload": {
                                "type": "read_stream",
                                "path": path,
                                "offset": offset,
                            },
                        }
                    ]
                )
            )[0]["result"]
            if not isinstance(reply, dict) or not isinstance(
                reply.get("bytes"), (bytes, bytearray)
            ):
                break
            chunks.append(bytes(reply["bytes"]))
            if reply.get("eof"):
                return _decoded_blob(name, b"".join(chunks))
            nxt = reply.get("next_offset")
            if not isinstance(nxt, int) or nxt <= offset:
                # No forward progress: stop rather than spin. Returning what we
                # have would be the truncation this loop exists to prevent, so
                # say so instead.
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


async def flush_pending(agent):
    """The kernel's shutdown hook (awaited before the teardown walk, provider still
    alive): land every message minted so far, then stop the pump."""
    pump = getattr(agent, "_message_pump", None)
    if pump is not None:
        await pump.stop()
        agent._message_pump = None


async def on_shutdown(agent):
    """Final flush + unsubscribe — a graceful shutdown loses nothing. `flush_pending`
    normally ran already (the kernel calls it before the walk); a path that skipped
    it (a bare `Agent.shutdown`) still lands the last window here."""
    loop = getattr(agent, "_fs_flush_loop", None)
    if loop is not None:
        await loop.stop()
    await flush_pending(agent)


VERBS = {
    "reflect": _reflect,
    "boot": _boot,
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
