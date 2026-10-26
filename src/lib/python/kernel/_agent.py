"""The Agent class — recursive node in the kernel tree.

Every entity in the system is an Agent. Agents have:
  - An in-memory `record` (id + handler_module + parent_id + meta). A
    loader agent persists it to `<root_path>/agent.yaml` — Agent itself
    never touches disk; `_root_path` is just its address in the tree.
  - A `_children: dict[str, Agent]` (empty for leaves).
  - An asyncio inbox for incoming payloads.
  - A handler_module that answers domain verbs (when the verb isn't
    a system verb that Agent answers natively).

Routing:
  All cross-agent communication goes through the kernel — `agent.send
  (target_id, payload)` resolves `target_id` in the flat global
  `ctx.agents` dict and dispatches its handler. Bundles never call
  each other's handler functions directly.

Lifecycle:
  - `create_agent` (system verb) — substrate persists a new record on
    disk under the parent, registers in `ctx.agents`, sends `boot`.
  - `_boot` (per-bundle hook) — hydrates process-memory state. May
    idempotently spawn children — `_boot` is idempotent, so a reboot
    finds existing children already present and skips re-creation.
  - `_shutdown` (per-bundle hook) — tears down process-memory state
    only. Records are NOT touched here.
  - `delete_agent` (system verb) — the only thing that removes
    records. Cascades depth-first: deepest descendants first run
    `_shutdown`, then are removed from `ctx.agents` + parent's
    `_children` + disk; then the next level up. Any `delete_lock`
    descendant blocks the entire cascade with `{locked, blocked_by}`.

Bundle handlers receive an Agent instance as the `kernel` parameter
and call `agent.send`, `agent.emit`, `agent.get`, `agent.update`,
`agent.create`, `agent.delete`, `agent.list`,
`agent.watch`/`unwatch`, `agent.add_state_subscriber`,
`agent.state_snapshot`.
"""

from __future__ import annotations

import asyncio
import contextlib
import importlib
import importlib.resources
import os
import secrets
import sys
from importlib.metadata import entry_points
from pathlib import Path
from typing import Any, Callable

from dataclasses import dataclass

from kernel._refusal import (
    BAD_ARG,
    DRAINING,
    EXISTS,
    INVALID_META,
    LOCKED,
    NO_CONSUMER,
    NOT_FOUND,
    NOT_HERE,
    TIMEOUT,
    UNAUTHORIZED,
    UNHANDLED,
    bad,
    guard_agent_id,
    missing,
    refusal,
)
from kernel._state import RESERVED_RECORD_KEYS
from kernel._authz import (
    DEFAULT_CALLABLE_BY,
    DEFAULT_CAN_CALL,
    LEGAL_GRANT_KEYS,
    _FROM_ABOVE,
    ancestor_ids,
    authorize_call,
    bad_grant_key,
)
from kernel._kernel import (
    BUNDLE_ENTRY_GROUP,
    _in_shutdown,
    CANCELLED,
    COMPLETED,
    DEAD_STATUSES,
    ERRORED,
    FAILED,
    Kernel,
    TIMED_OUT,
    _current_origin,
    _current_secret,
    _current_sender,
    _current_task,
    _reply_sink,
    _summarize_payload,
    outcome,
    stored_outcome,
    page,
)
from kernel._bundles import (
    _find_bundle_module,
    _in_catalog,
    bundle_enabled,
    bundle_package,
    folder_rows,
    import_handler,
    is_folder,
)
from kernel import _docs, _secret, _sys


_KEEP_MEMORY = "_keep_memory"


def keep_memory(ctx_or_agent, id: str) -> bool:
    """True while `id` is being deleted by a `delete_agent keep_memory:true`. An
    `on_delete` hook (which gets no payload) reads it: a memory-owning bundle then
    leaves its persisted rows. Set for ONE delete only (see `Agent.delete`)."""
    ctx = getattr(ctx_or_agent, "ctx", ctx_or_agent)
    kept = getattr(ctx, _KEEP_MEMORY, None)
    return bool(kept) and id in kept


# System verbs answered natively by every Agent for its own children.
# When a payload with `type` in this set is dispatched, Agent answers
# directly without consulting `handler_module`. Domain verbs (anything
# else) route to handler_module.handler.
_SYSTEM_VERBS = frozenset(
    {
        "create_agent",
        "delete_agent",
        "update_agent",
        "list_agents",
        # The TASK TABLE — work in flight, universal. `ps` reads it, `cancel`
        # kills a task and its subtree. Baked into the substrate, NOT a bundle:
        # cancellation is a property of the one primitive (`send`), so EVERY
        # agent is cancelable and a caller never needs to know which bundle it
        # faces. Plain verbs — an AGENT can call them on another agent.
        "ps",
        "cancel",
        # The EXECUTION LEDGER's read surface — every execution, live and
        # settled, outcomes included. `ps` is work in flight; `ledger` is what
        # work BECAME. Terminal non-completed rows ARE the dead-letter queue:
        # parked, inspectable, never silently retried; `ack_deadletter` retires one.
        "ledger",
        "ack_deadletter",
        # The kernel's LIFECYCLE. `kernel_start` is the one start sequence (the
        # root's memory store first, then everyone); `kernel_stop` the graceful
        # stop of the process. `shutdown_agents` stops the agents, not the process.
        "kernel_start",
        "shutdown_agents",
        "kernel_stop",
        # HOW TO CALL an agent — its bundle's verb docs, in two tiers. Answered
        # by the root and gated like every kernel verb (`_gate`).
        "docs",
    }
)

# One doc per system verb — what `docs of=kernel` serves (the verbs appear in no
# bundle's `VERBS` map). The oneliner tier is the text before the first top-level
# `. ` or ` — ` (`kernel/_docs.signature`); `detailed` is the whole entry.
_SYSTEM_VERB_DOCS = {
    "create_agent": (
        "args: handler_module:str, parent_id:str REQUIRED, id:str?, **meta. "
        "Creates a CHILD OF `parent_id` — any agent you name (your own id grows "
        "your own subtree); 'kernel_state' for top-level. No default parent."
    ),
    "update_agent": (
        "args: id:str, **meta. EVERY FIELD YOU NAME IS REPLACED, NOT MERGED "
        "(a field you do not name is untouched), so a dict field must be sent "
        "WHOLE — `can_call`/`callable_by` included: restate every key you want "
        "to keep, the baseline `$self` among them, or the keys you omit are GONE. "
        "Granting one edge reads like a PATCH and behaves like a PUT. Whoever "
        "holds update_agent on the root updates any agent, itself included."
    ),
    "delete_agent": (
        "args: id:str. CASCADES — deletes that agent AND ITS WHOLE SUBTREE, "
        "deepest first; refuses if any descendant carries `delete_lock`. Whoever "
        "holds delete_agent on the root deletes any agent."
    ),
    "list_agents": "No args. Every agent's record, flat. Paged.",
    "ps": (
        "No args. The live kernel-wide TASK TABLE — {tasks:[{id, agent, parent, "
        "verb, started_at}]}, one row per in-flight dispatch. Paged."
    ),
    # EVERY execution, not just the named ones. This said NAMED-only while the
    # verb iterated the whole table, so a model reading the root reflect expected
    # its own handful of jobs and received every minted-pid row beside them — the
    # doc contradicting the reply it had just been handed.
    "ledger": (
        "No args. EVERY execution and what it became — named under its id, "
        "unnamed under its minted pid — {executions:[{id, target, status, "
        "attempts, retries, named, outcome?, acked?}]}, paged, scoped to what YOU "
        "originated; read from the kernel's memory store plus what is not yet "
        "flushed. `outcome` keeps the RESULT only for a named call; a dead letter "
        "keeps its error; anything else is {status, bytes} — the reply itself went "
        "to its caller. Failed/timed_out/cancelled rows are the dead-letter queue "
        "(announced by the `on_deadletter` event): inspect, retry with a NEW id "
        "if you choose, then `ack_deadletter` it — nothing is retried silently."
    ),
    "ack_deadletter": (
        "args: id:int|str (req — an execution id, or its name). Acknowledges a DEAD "
        "LETTER (failed/timed_out/cancelled) you originated: recorded in the "
        "kernel's store, it may then be pruned. Returns {acked:true, id}."
    ),
    "kernel_start": (
        "No args. THE START SEQUENCE — the root must name `memory_store_id` (fail "
        "loud otherwise): boots the root, then that store, starts flushing the "
        "kernel's execution rows into it, then boots every other agent in "
        "registration order. Idempotent. Returns {started, store, booted:[ids]}."
    ),
    "cancel": (
        "args: task_id:str. Kills that task AND ITS SUBTREE — the id names its "
        "own agent. Returns {cancelled:bool, killed:[…]}; the blocked caller's "
        "batch item settles as status:'cancelled'. Unknown id ⇒ {cancelled:false}."
    ),
    "shutdown_agents": (
        "No args. Stops the AGENTS, not the process: drains (new roots refused), "
        "sends `shutdown` to every agent advertising it, waits for in-flight work, "
        "cancels the rest. Returns {drained, shut:[ids], cancelled:[ids]}."
    ),
    "kernel_stop": (
        "No args. Stops the kernel PROCESS gracefully — drain, every call settles "
        "(done, or cancelled into the dead letters), the last rows flush, agents "
        "shut down, exit. Under env:'container', the container."
    ),
    "docs": (
        "args: of:str (req — an agent id (`<guid>|<agent id>` on a connected kernel), a bundle, or 'kernel'), "
        "tier:'oneliners'|'detailed'?, verb:str?. HOW TO CALL an agent — its "
        "bundle's verbs: `oneliners` (default) is one signature line per verb, "
        "`detailed` the full text (no `verb`: plus emits, cancel and notes). Pull "
        "once per bundle; `detailed` for a verb you are about to use. Gated like "
        "every kernel verb: a `can_call` grant of `docs` on the root. "
        "Returns {of, bundle, tier, verbs, emits?, cancel?, notes?}."
    ),
}


def _json_safe(o):
    """A reply body a JSON wire can carry: bytes become `<bytes:N>` (the telemetry
    rule), everything else passes through, so a ledger holding a `read_stream`
    outcome still serializes over REST."""
    if isinstance(o, (bytes, bytearray)):
        return f"<bytes:{len(o)}>"
    if isinstance(o, dict):
        return {k: _json_safe(v) for k, v in o.items()}
    if isinstance(o, (list, tuple)):
        return [_json_safe(v) for v in o]
    return o


def _as_id(x) -> int | None:
    """A message id as an int: an int, or its decimal string (a model may quote
    what it copied out of a tag). Anything else is not an id."""
    if isinstance(x, bool):
        return None
    if isinstance(x, int):
        return x if x > 0 else None
    if isinstance(x, str) and x.isdecimal():
        return int(x)
    return None


class _Exited(Exception):
    """A dispatch's `SystemExit`, carried out of its task as an ordinary raise
    (`Agent._dispatch_task`); the settle reports the SystemExit itself."""


@dataclass
class _Dispatched:
    """One launched batch item awaiting settle. `pid is None` marks the
    kernel-reflect fast path — a pure identity read that never enters the
    task table."""

    index: int
    pid: int | None
    task: "asyncio.Task | None"
    timeout: float | None
    target: "Agent"
    payload: dict
    # Caller-NAMED execution (the item's `id`): `exec_fut` is the ledger future
    # the owner settles; a JOINER has `task=None` and only awaits it. A joiner's
    # timeout bounds its own wait, never the execution it does not own.
    exec_fut: "asyncio.Future | None" = None
    joined: bool = False
    # The kernel-reflect fast path applies reflect flags INSIDE its task; settle
    # must not apply them twice when such an execution is caller-named.
    flags_done: bool = False
    # The caller's NAME for this execution (its item `id`), echoed on the outcome.
    name: str | None = None
    # The item's `secret` marks (`kernel/_secret`), carried to every attempt.
    marks: tuple = ()
    # The reply marks this dispatch noted (`note_reply_secret`): the list its
    # current attempt owns. None for a joiner or a fast path (nothing ran).
    sink: list | None = None


class RemoteTarget:
    """An agent on a CONNECTED PEER KERNEL, as ONE send sees it: addressed `<guid>|<agent id>`, never registered in this tree.
    The send pipeline treats it like any target — a ledger row, a task-table
    row, a settled outcome — except that its dispatch hands the call to the
    peer's routes instead of a handler here. The transport a route rides only
    carries frames; the peer judges the call as the leg that serves it there.

    `routes` are the guid's live routes when the send resolved it — every tab of
    one identity: the call goes to ALL of them and settles
    with the FIRST reply.
    """

    parent = None

    def __init__(self, qualified: str, guid: str, agent_id: str, routes, via_agent):
        self.id = qualified
        self.guid = guid
        self.agent_id = agent_id
        self.routes = tuple(routes)
        self._via = via_agent  # the local agent whose kernel holds the routes
        self._children: dict = {}
        self._meta: dict = {"callable_by": {}}
        self.handler_module = None

    async def _dispatch_task(
        self,
        task_id,
        payload: dict,
        origin: str | None = None,
        marks: tuple = (),
        sink: list | None = None,
    ):
        _current_task.set(task_id)
        calls = [
            asyncio.ensure_future(r.call(self.agent_id, payload)) for r in self.routes
        ]
        try:
            done, _ = await asyncio.wait(calls, return_when=asyncio.FIRST_COMPLETED)
        finally:
            for c in calls:  # the losers — and every one when this call is stopped
                c.cancel()
        # TODO: first-reply-wins hides tab DESYNC — if the tabs of
        # one identity diverge, the other replies are dropped unseen. Must be
        # solved (later).
        return next(c for c in calls if c in done).result()


class Agent:
    """One node in the kernel tree. Recursive: any agent may have
    children, regardless of bundle.

    Public attributes — read freely; mutate only through the methods.

    Class-level `ephemeral`: when True, a loader skips this agent
    entirely — `save()` omits it and no agent.yaml is ever written.
    Use for per-process composables that have no meaningful state —
    e.g. the stdout renderer (`Cli`). Defaults to False; subclasses
    override.
    """

    # Subclass-level opt-out: ephemeral agents are skipped by `save()`
    # and never persisted by a loader. They live in memory only; reboot
    # loses them; the bootstrap re-composes them per-process.
    ephemeral: bool = False

    # ─── construction ──────────────────────────────────────────

    def __init__(
        self,
        id: str,
        root_path: Path | None = None,
        *,
        ctx: Kernel,
        parent: "Agent | None" = None,
        handler_module: str | None = None,
        **meta: Any,
    ) -> None:
        """Construct one Agent. `ctx` is REQUIRED — no auto-mint.

        `parent` is the parent Agent. When set, this agent auto-
        registers in `parent._children`, and `root_path` defaults to
        `parent._children_dir() / id` (i.e. `<parent>/agents/<id>/`).

        `root_path` is a DERIVED ADDRESS ONLY — the loader agent owns
        all disk I/O; an Agent never reads or writes it. Sidecar bundles
        (file / chat / readme) compute their paths under it, and
        the loader maps `<root_path>/agent.yaml` ←→ this record. Children
        address at `<root_path>/agents/<child_id>/`.
        """
        if not isinstance(ctx, Kernel):
            raise TypeError(
                "Agent: ctx (Kernel) is required — construct "
                "`Kernel()` first, then pass it to every Agent."
            )
        self.id = id
        self.handler_module = handler_module
        self.parent = parent
        is_ephemeral = type(self).ephemeral
        # `_root_path` is the agent's address under the tree — never
        # touched by Agent itself (the loader persists/hydrates it).
        if root_path is None:
            if parent is not None:
                root_path = parent._children_dir() / id
            else:
                root_path = Path(".fantastic")  # root address; loader owns disk
        self._root_path = root_path
        self._children: dict[str, Agent] = {}
        # Watcher ids — set of ids whose inbox mirrors THIS agent's traffic
        # (both agents and synthetic browser-client ids).
        self._watcher_ids: set[str] = set()
        self._in_flight: int = 0
        self._meta: dict[str, Any] = dict(meta)
        # Structural authority baseline, stamped + MERGED on BOTH fields for EVERY
        # agent that passes through here — the seeded root, a created child, a
        # disk-loaded record alike (one funnel, so nothing lacks it). `can_call`
        # (who I may call) and `callable_by` (who may call me) both start as
        # {$self:*, $parent:*, $subtree:*} — reach/accept your own subtree and your
        # creator. It MERGES UNDER any explicit grant already in `meta` (so an
        # explicit key overrides a baseline key; the merge is idempotent on reload).
        # Deny-all governs every edge NOT in the merged grant.
        for _field, _base in (
            ("can_call", DEFAULT_CAN_CALL),
            ("callable_by", DEFAULT_CALLABLE_BY),
        ):
            _merged = dict(_base)
            _merged.update(self._meta.get(_field) or {})
            self._meta[_field] = _merged
        self.ctx = ctx
        self.ctx.register(self)
        # NO INBOX HERE. A queue exists iff a READER registered it —
        # `ctx.ensure_inbox(id)` IS that registration, and the only callers
        # that make one are the callers that then drain it (the web_ws proxy
        # for a browser client, exec_bridge for a relay proxy, a test that
        # asserts on traffic). Minting one per agent at construction gave
        # every agent a 256-slot ring that NOTHING in the tree read: it
        # filled once and then evicted one payload per message, forever, for
        # the life of the process. Fanout now puts only on queues that exist.
        # No disk I/O here — the agent is pure in-memory. A loader agent
        # subscribes to the `added` event below and persists this record;
        # `Kernel.load()` rehydrates from records the loader read back.
        # Wire into parent's children dict + publish lifecycle event.
        # `parent.create(...)` is the indirect path; this one supports
        # direct construction (`Cli(kernel, parent=kernel_state)`).
        if self.parent is not None:
            self.parent._children[self.id] = self
            self.ctx.publish_state(
                {
                    "agent_id": self.id,
                    "kind": "added",
                    "name": self.display_name or self.id,
                    "parent_id": self.parent.id,
                }
            )
        else:
            # First parent-less Agent in this Kernel ctx becomes the root.
            if self.ctx.root is None and not is_ephemeral:
                self.ctx.root = self

    # ─── address (the loader maps this to bytes; Agent never does) ──

    def _children_dir(self) -> Path:
        # The container dir name is declared config on the kernel (default
        # "agents"); not hardcoded — see `Kernel.children_dir`.
        return self._root_path / self.ctx.children_dir

    def _bundle_files(self):
        """The agent's bundle package files (importlib Traversable), or None.
        A shipped bundle's readme and `docs/` live IN the bundle — never copied
        into the agent's dir."""
        pkg = bundle_package(self.handler_module) if self.handler_module else None
        if pkg is None:
            return None
        try:
            return importlib.resources.files(pkg)
        except (ModuleNotFoundError, TypeError, AttributeError):
            return None  # a single-file module or a synthetic stub ships none

    def _read_readme(self) -> str | None:
        """The bundle's `readme.md`, or None if it ships none.

        For an agent whose manual is a keyed doc TREE (a `docs/` dir beside
        `readme.md`), this is the SHORT orientation; the topics are fetched by
        key via `_readme_view` (`readme=index` / `readme=<key>`)."""
        base = self._bundle_files()
        if base is None:
            return None
        p = base / "readme.md"
        try:
            return p.read_text(encoding="utf-8") if p.is_file() else None
        except OSError:
            return None

    # ─── keyed / tiered doc tree ───────────────────────────────
    # A bundle's manual may be a SHORT `readme.md` orientation + a `docs/`
    # tree of topic files (nested dirs = tiers). `reflect readme=index`
    # lists the tree token-annotated; `reflect readme=<key>` drills one
    # topic. A topic = a `docs/<key>.md` file; a sibling dir `docs/<key>/`
    # extends it with sub-docs (`<key>/<sub>`). Simple bundles ship one
    # `readme.md` and no `docs/` — the index is then empty and `readme=true`
    # returns that single doc, unchanged.

    _CHARS_PER_TOKEN = 4  # mirrors ai_core.context.CHARS_PER_TOKEN (ceil-div)

    @property
    def _docs_dir(self):
        base = self._bundle_files()
        return base / "docs" if base is not None else None

    @classmethod
    def _est_tokens(cls, text: str) -> int:
        """Ceil(chars / 4) — the same cheap estimate ai_core uses, so a doc's
        advertised cost matches what the model actually pays to read it."""
        return (len(text) + cls._CHARS_PER_TOKEN - 1) // cls._CHARS_PER_TOKEN

    @staticmethod
    def _read_desc(text: str) -> str:
        """A topic's one-line descriptor — the leading `> ` line if present,
        else the first prose line (skipping headings)."""
        lines = text.splitlines()
        for line in lines:
            if line.startswith("> "):
                return line[2:].strip()
        for line in lines:
            s = line.strip()
            if s and not s.startswith("#") and not s.startswith(">"):
                return s
        return ""

    def _readme_index(self, base=None, prefix: str = "") -> list[dict]:
        """Token-annotated topic index under `docs/` (recursive; nesting
        preserved). Each entry `{key, desc, tokens, children?}` — NO body."""
        d = base if base is not None else self._docs_dir
        if d is None or not d.is_dir():
            return []
        entries: list[dict] = []
        for f in sorted(d.iterdir(), key=lambda x: x.name):
            if not (f.is_file() and f.name.endswith(".md")):
                continue
            try:
                body = f.read_text(encoding="utf-8")
            except OSError:
                continue
            stem = f.name[: -len(".md")]
            key = prefix + stem
            entry: dict[str, Any] = {
                "key": key,
                "desc": self._read_desc(body),
                "tokens": self._est_tokens(body),
            }
            sub = d / stem
            if sub.is_dir():
                children = self._readme_index(sub, key + "/")
                if children:
                    entry["children"] = children
            entries.append(entry)
        return entries

    def _readme_keys(self) -> list[str]:
        """Flat list of every doc key (for an unknown-key error reply)."""
        keys: list[str] = []

        def walk(entries: list[dict]) -> None:
            for e in entries:
                keys.append(e["key"])
                walk(e.get("children", []))

        walk(self._readme_index())
        return keys

    def _readme_view(self, sel: str):
        """Polymorphic `readme=<str>`: `"index"` → the token-annotated tree;
        a `<key>` → that topic's body + its immediate sub-docs' index; an
        unknown/escaping key → `{error, keys}` (the reply stays well-formed)."""
        if sel == "index":
            return {"index": self._readme_index()}
        d = self._docs_dir
        safe = sel.strip("/").replace("\\", "/")
        # LEXICAL clamp, then EXACT key membership — never the filesystem's
        # opinion: `a/../b` is refused by the key, not by whether the OS can
        # traverse a missing `a`, and a case-insensitive disk never serves
        # `TOPIC` for `topic`.
        parts = safe.split("/")
        if not safe or any(p in ("", ".", "..") for p in parts):
            return {"error": f"bad key {sel!r}", "keys": self._readme_keys()}
        if safe not in self._readme_keys():
            return {"error": f"no doc {sel!r}", "keys": self._readme_keys()}
        # `safe` is one of the keys the index listed, so it names a file the
        # bundle ships — no path outside `docs/` can be one.
        f = d.joinpath(*(safe + ".md").split("/"))
        if not f.is_file():
            return {"error": f"no doc {sel!r}", "keys": self._readme_keys()}
        try:
            body = f.read_text(encoding="utf-8")
        except OSError:
            return {"error": f"unreadable {sel!r}", "keys": self._readme_keys()}
        view: dict[str, Any] = {
            "key": safe,
            "desc": self._read_desc(body),
            "tokens": self._est_tokens(body),
            "body": body,
        }
        sub = d.joinpath(*safe.split("/"))
        if sub.is_dir():
            children = self._readme_index(sub, safe + "/")
            if children:
                view["children"] = children
        return view

    # ─── record ────────────────────────────────────────────────

    @property
    def record(self) -> dict:
        """Persistent fields written to agent.yaml — id +
        handler_module + parent_id + meta."""
        rec: dict[str, Any] = {"id": self.id}
        if self.handler_module is not None:
            rec["handler_module"] = self.handler_module
        rec.update(self._meta)
        # STRUCTURE WINS. `parent_id` is not meta — it is where this agent
        # actually sits, and the tree is the only thing that knows. Written
        # BEFORE the meta merge, a `parent_id` that leaked into meta would
        # overwrite it and the record would report a parent the tree does not
        # have (`$parent` resolving differently on the two halves of the gate).
        # Written last, it cannot be shadowed.
        if self.parent is not None:
            rec["parent_id"] = self.parent.id
        else:
            rec.pop("parent_id", None)  # the root has no parent, whatever meta says
        return rec

    @property
    def display_name(self) -> str | None:
        v = self._meta.get("display_name")
        return v if isinstance(v, str) else None

    @property
    def description(self) -> str | None:
        """Optional short meta saying what this agent does. Surfaced in
        every reflect (top-level + distilled tree nodes). Set via
        create_agent / update_agent. Forward-looking: a persistent-memory
        agent uses it to tell an LLM what the memory holds."""
        v = self._meta.get("description")
        return v if isinstance(v, str) else None

    # ─── public API: routing ───────────────────────────────────

    async def send(self, items: list[dict]) -> list[dict]:
        """ARRAY-ONLY batch send: [{target, payload, timeout?}, ...] ->
        [{pid, status, result}]. There is deliberately NO single-call form —
        one shape prevents single/batch drift. Single call =
        `(await send([{...}]))[0]`.

        Dispatch is CONCURRENT; the returned array preserves input order.
        Every dispatched item is a REGISTERED asyncio.Task (one `ps` row,
        `cancel`-able by its pid, which the outcome carries). Pids are minted
        in a synchronous launch phase, so the whole batch is visible in `ps`
        before the first await.

        `id` (optional) NAMES the execution — ensure-semantics, like a supplied
        `create_agent` id: unknown ⇒ dispatch and ledger it; in flight ⇒ JOIN
        (no second run); settled ⇒ the recorded outcome returns and no handler
        runs. Idempotency and once-only execution in one field. The NAME is a
        string of the caller's; the execution's id is the kernel's counter and
        the outcome carries both (`pid`, `name`). An int `id` on an item JOINS
        that execution by id and never creates one. A name is BOUND to its first
        execution's target. Retention: session lifetime.

        `timeout` is per-item seconds. The kernel NEVER defaults it:
        absent/None = wait forever. On expiry the item is killed through the
        front door (`cancel_task`), so every bundle's `on_cancel` hook runs
        before the abort — a bare task.cancel() would orphan the OS resources
        those hooks release.

        Statuses: completed | errored | failed | cancelled | timed_out — see the
        outcome vocabulary in `_kernel.py`. `pid is None` ⟺ the item never
        reached the task table (refusal, malformed item, kernel-reflect fast
        path). Nothing raises out of `send` except CancelledError (the caller
        itself being killed) — a handler raise settles as `failed`.
        """
        if not isinstance(items, list):
            raise TypeError("send takes a LIST of {target, payload, timeout?} items")
        # NAMED EXECUTIONS OUTLIVE RAM. Once flushed, a settled execution is only in
        # the kernel's store; an item naming one is looked up there FIRST, so the
        # synchronous launch below finds it exactly as if it had never left RAM.
        if self.ctx.store_id is not None and any(
            isinstance(it, dict) and it.get("id") is not None for it in items
        ):
            await self.ctx.recall_executions(items)
        results: list[dict | None] = [None] * len(items)
        dispatched: list[_Dispatched] = []
        for i, item in enumerate(items):  # SYNC loop — every pid exists
            out = self._launch(i, item, dispatched)  # before the first await
            if out is not None:
                results[i] = out
        if dispatched:
            settled = await asyncio.gather(*(self._settle(d) for d in dispatched))
            for d, out in zip(dispatched, settled):
                if d.name is not None and isinstance(out, dict):
                    out = {**out, "name": d.name}
                results[d.index] = out
        return results

    def _launch(
        self, index: int, item: Any, dispatched: list["_Dispatched"]
    ) -> dict | None:
        """One item's SYNCHRONOUS half: validate, resolve, gate, dispatch.
        Returns an immediate outcome for anything that never reaches the task
        table; returns None after appending a `_Dispatched` for the rest."""
        if not isinstance(item, dict):
            return outcome(
                None,
                FAILED,
                refusal(BAD_ARG, "send: item must be a dict", arg="item"),
            )
        target_id = item.get("target")
        payload = item.get("payload")
        timeout = item.get("timeout")
        if not isinstance(target_id, str) or not target_id:
            return outcome(None, FAILED, missing("send", "target"))
        if not isinstance(payload, dict):
            return outcome(None, FAILED, missing("send", "payload"))
        if timeout is not None and (
            isinstance(timeout, bool)
            or not isinstance(timeout, (int, float))
            or timeout <= 0
        ):
            return outcome(
                None,
                FAILED,
                refusal(
                    BAD_ARG,
                    f"send: timeout must be a positive number of seconds, "
                    f"got {timeout!r}",
                    arg="timeout",
                ),
            )
        # `type` is the verb; there is no send without one. Validated for EVERY
        # send (operator included) BEFORE the gate, so a typeless payload is a
        # coded refusal, never a TypeError out of `send` (HTTP 500 on web_rest)
        # and never an `unhandled` from the target.
        if "type" not in payload:
            return outcome(None, FAILED, missing("send", "payload.type"))
        verb = payload.get("type")
        if not isinstance(verb, str) or not verb:
            # A non-string is named by its TYPE, never echoed: the octet REST leg
            # and a binary WS frame can put a whole body here, and its repr would
            # make a large body cost several times its size in refusal text.
            got = repr(verb) if isinstance(verb, str) else type(verb).__name__
            return outcome(
                None,
                FAILED,
                refusal(
                    BAD_ARG,
                    f"send: payload.type must be a non-empty string, got {got}",
                    arg="payload.type",
                ),
            )
        # The item's OWN fields are validated before anything is resolved, gated
        # or spawned: a bad `id`/`retries` caught only after an unnamed dispatch
        # had started would orphan a running task.
        exec_id = item.get("id")
        join_id: int | None = None  # an int item id: JOIN by message id
        name: str | None = None  # a string item id: the caller's NAME
        if exec_id is not None:
            # A caller-NAMED execution — "ensure it exists", the same semantics a
            # supplied `create_agent` id has. The name is the caller's; the
            # execution's id is the kernel's counter. An int names an execution
            # by its id and can only join it — the kernel is the only allocator.
            if isinstance(exec_id, bool) or not isinstance(exec_id, (str, int)):
                return outcome(
                    None,
                    FAILED,
                    refusal(
                        BAD_ARG,
                        f"send: id must be a name (non-empty string) or a message "
                        f"id (int), got {exec_id!r}",
                        arg="id",
                    ),
                )
            if isinstance(exec_id, int):
                if exec_id not in self.ctx.executions:
                    return outcome(
                        None,
                        FAILED,
                        refusal(
                            BAD_ARG,
                            f"send: id {exec_id} names no known execution — "
                            "ids are minted by the kernel and can be joined, "
                            "never created",
                            arg="id",
                        ),
                    )
                join_id = exec_id
            else:
                if not exec_id:
                    return outcome(
                        None,
                        FAILED,
                        refusal(BAD_ARG, "send: id must be a non-empty name", arg="id"),
                    )
                name = exec_id
                join_id = self.ctx.resolve_name(name)
        bad_retries = self._check_retries(item)
        if bad_retries is not None:
            return bad_retries
        marks, bad_secret = _secret.check(item)
        if bad_secret is not None:
            return outcome(None, FAILED, bad_secret)
        # The target's OWN secret paths (its bundle's `payload_secret`) join the
        # item's marks before any refusal below, so a refused call row shows them too.
        marks = self._bundle_marks(self._resolve_agent(target_id), payload, marks)
        # A DYING TASK REGISTERS NO NEW WORK. `cancel_task` marks the whole
        # subtree before it aborts anything, because `on_cancel` hooks await —
        # so a doomed handler gets scheduling passes on its way out, and what it
        # sent there would otherwise start fresh dispatches that outlive the cancel.
        cur = _current_task.get()
        if cur is not None and (self.ctx.tasks.get(cur) or {}).get("_killed"):
            return outcome(None, CANCELLED, {"cancelled": True})
        # WHILE DRAINING, NO NEW ROOTS. A send from inside a live DISPATCH (a
        # parent call in the table — including an agent's own `shutdown` dispatch
        # using its tools) passes; the kernel's own shutdown traffic and its
        # keepalive persisters pass; everything else is a new root and is refused
        # before it costs an id. A FORK (a ws call, a scheduler tick, a bridge's
        # read loop, a job's relay) starts new work, so it is refused like any
        # transport — a ws call is treated exactly like the same REST call.
        # The root's own control surface stays open while draining: kernel verbs
        # (`shutdown_agents` again, `shutdown_kernel`, `ps`, `messages`, `ledger`)
        # and `reflect` are how an operator watches and finishes the stop. Over a
        # wire the operator's call arrives as a leg's `inbound_call` carrying it —
        # judged by the call it carries, or a remote operator could never finish
        # the stop it started.
        if (
            self.ctx.draining
            and not _in_shutdown.get()
            and not self._is_control(target_id, payload)
            and not self.ctx.drain_admits(cur, asyncio.current_task())
        ):
            return outcome(
                None,
                FAILED,
                refusal(
                    DRAINING,
                    "send: the kernel is draining for shutdown — no new work is "
                    "accepted; in-flight work may finish",
                ),
            )
        if target_id == "kernel":
            target = self._root()
        else:
            target = self.ctx.agents.get(target_id) or self._qualified_target(target_id)
        if target is None:
            return outcome(None, FAILED, self.ctx.unknown_agent(target_id))
        # ── CALL GATE — fully-gated, two-sided, from-above ──────────────
        # Operator sends (`_current_sender` is None: made outside every dispatch —
        # the CLI, the cold bootstrap, a test driver, a transport handing an
        # inbound frame to its leg) are the ORIGIN root of trust and pass ungated.
        # Every AGENT-originated send — ANY verb, `reflect` included, nothing
        # exempt — needs BOTH the sender's `can_call` (egress) AND the target's
        # `callable_by` (ingress) to grant the edge.
        sender_id = _current_sender.get()
        if sender_id is not None:
            denial = self._gate(sender_id, verb, target)
            if denial is not None:
                return outcome(None, FAILED, denial)
        # `kernel` reflect fast-path: a pure identity read (no dispatch, no
        # telemetry, no table row), applied AFTER the gate so it is authorized
        # like any other reflect. Async (reflect flags nest dispatches), so it
        # rides the settle phase as a pid-less task.
        if exec_id is not None:
            entry = self.ctx.executions.get(join_id) if join_id is not None else None
            if entry is not None:
                # The id is BOUND to its first execution's target — returning an
                # unrelated agent's outcome would be silent corruption.
                if entry["target"] != target.id:
                    return outcome(
                        None,
                        FAILED,
                        refusal(
                            BAD_ARG,
                            f"send: id {exec_id!r} already names an execution on "
                            f"{entry['target']!r}, not {target.id!r} — an id is "
                            "bound to its first execution; a retry aimed "
                            "elsewhere needs a new id",
                            arg="id",
                        ),
                    )
                # JOIN — settled or in flight, the ledger future carries the one
                # true outcome; no second execution ever starts.
                #
                # AUTHORIZED AGAINST THE EXECUTION IT JOINS, not against the
                # payload the joiner happened to send. Joining hands back
                # ANOTHER caller's result verbatim, so it is equivalent to
                # having made that call — and the gate must be asked that
                # question, not a different one.
                #
                # Otherwise an agent granted only `safe_verb` on a target could
                # reuse an id another agent used for a sensitive verb and receive
                # that result verbatim, its own gate check passing on a verb whose
                # reply it never got. Same class as the read side-channel closed
                # by origin-scoping the `ledger` VERB, through `send` itself.
                # Reachable from the wire, where a frame id IS the execution id
                # and two legs can name one id.
                #
                # An OPERATOR joiner (sender None) is ungated here as everywhere.
                joiner = _current_sender.get()
                orig_verb = (entry.get("payload") or {}).get("type")
                if joiner is not None:
                    denial = self._gate(joiner, orig_verb, target)
                    if denial is not None:
                        return outcome(None, FAILED, denial)
                dispatched.append(
                    _Dispatched(
                        index,
                        join_id,
                        None,
                        timeout,
                        target,
                        payload,
                        exec_fut=entry["fut"],
                        joined=True,
                        name=entry.get("name"),
                    )
                )
                return None
            # NAMED and new: the message id is minted like any other; the name
            # is a field on it.
            exec_id = self.ctx.mint_id()
            fut, bad = self._ledger_open(
                exec_id, target, payload, timeout, item, name=name, marks=marks
            )
            if bad is not None:
                return bad
            if target_id == "kernel" and verb == "reflect":
                # The identity-read fast path, caller-named: still no table row
                # (nothing to cancel in a pure read), but the ledger applies —
                # dedup and join work, and the outcome records under this id.
                task = asyncio.create_task(
                    self._apply_reflect_flags(
                        target, payload, target._reflect_identity()
                    )
                )
                dispatched.append(
                    _Dispatched(
                        index,
                        exec_id,
                        task,
                        timeout,
                        target,
                        payload,
                        exec_fut=fut,
                        flags_done=True,
                        name=name,
                    )
                )
                return None
            sink: list = []
            pid, task = self._spawn_dispatch(
                target, payload, task_id=exec_id, marks=marks, sink=sink
            )
            dispatched.append(
                _Dispatched(
                    index,
                    pid,
                    task,
                    timeout,
                    target,
                    payload,
                    exec_fut=fut,
                    name=name,
                    marks=marks,
                    sink=sink,
                )
            )
            return None
        if target_id == "kernel" and verb == "reflect":
            task = asyncio.create_task(
                self._apply_reflect_flags(target, payload, target._reflect_identity())
            )
            dispatched.append(_Dispatched(index, None, task, timeout, target, payload))
            return None
        # UNNAMED work is ledgered too — under its minted pid. Same row, same
        # engine: re-fetchable/joinable by pid, retryable, dead-letterable; the
        # ring (Kernel.ledger_cap) bounds what history costs.
        # The row opens BEFORE the spawn: `retries` was validated up front, so
        # nothing here can refuse after work has started.
        pid = self.ctx.mint_id()
        fut, bad = self._ledger_open(
            pid, target, payload, timeout, item, name=None, marks=marks
        )
        if bad is not None:
            return bad
        sink: list = []
        pid, task = self._spawn_dispatch(
            target, payload, task_id=pid, marks=marks, sink=sink
        )
        dispatched.append(
            _Dispatched(
                index,
                pid,
                task,
                timeout,
                target,
                payload,
                exec_fut=fut,
                marks=marks,
                sink=sink,
            )
        )
        return None

    @staticmethod
    def _check_retries(item: dict) -> dict | None:
        """`retries` must be a non-negative int. Checked in the launch phase
        before anything runs; returns the failed outcome or None."""
        retries = item.get("retries", 0)
        if isinstance(retries, bool) or not isinstance(retries, int) or retries < 0:
            return outcome(
                None,
                FAILED,
                refusal(
                    BAD_ARG,
                    f"send: retries must be a non-negative int, got {retries!r}",
                    arg="retries",
                ),
            )
        return None

    def _bundle_marks(self, target, payload: dict, marks: tuple) -> tuple:
        """The item's marks plus the paths the target's bundle names for this
        payload (its module-level `payload_secret(payload)` hook), each once.
        Malformed or `type` paths are dropped: the bundle hook never widens the
        verb. An unknown target or a bundle without the hook returns `marks`."""
        if target is None:
            return marks
        hook = target.module_hook("payload_secret")
        if hook is None:
            return marks
        extra = []
        for m in hook(payload) or ():
            if (
                isinstance(m, str)
                and m
                and m != "type"
                and "" not in m.split(".")
                and m not in marks
                and m not in extra
            ):
                extra.append(m)
        return tuple(marks) + tuple(extra) if extra else marks

    def _reply_marks(self, target_id, payload, reply) -> tuple:
        """The reply paths the target's bundle names (its module-level
        `reply_secret(payload, reply)` hook). They are recorded as `[secret]` in
        the settle row only; the caller still gets the reply whole."""
        target = self._resolve_agent(target_id) if isinstance(target_id, str) else None
        if (
            target is None
            or not isinstance(payload, dict)
            or not isinstance(reply, dict)
        ):
            return ()
        hook = target.module_hook("reply_secret")
        if hook is None:
            return ()
        return tuple(
            m
            for m in (hook(payload, reply) or ())
            if isinstance(m, str) and m and m != "type" and "" not in m.split(".")
        )

    def _ledger_open(
        self, exec_id: int, target, payload, timeout, item, *, name, marks=()
    ):
        """Open one execution under a pre-minted id: the CALL ROW in the message
        store (what outlives the process) and the process side (`executions`).
        `retries` already validated. Returns (fut, refusal?). The row keeps the
        payload AS SHOWN (`kernel/_secret`); the RAM side keeps the real one, which
        a retry re-sends."""
        retries = item.get("retries", 0)
        if bad := self._check_retries(item):
            return None, bad
        fut: asyncio.Future = asyncio.get_event_loop().create_future()
        caller = _current_sender.get()
        # EVERY SEND IS A MESSAGE. The row names the call — who was asked what,
        # by which message — and takes the outcome's status at settle; the reply
        # body stays with the reply. `parent` is the message that made the call.
        # ONE TAPE. The row carries the WHOLE call — the dispatch payload
        # verbatim, the retry budget, the attempt count as at dispatch, the
        # deadline — because there is no second file: `messages.yaml` is the only
        # thing that outlives the process, and `attempts` is updated on this dict
        # at every retry (the kernel owns the row in RAM).
        row = {
            "id": exec_id,
            "kind": "call",
            "verb": payload.get("type"),
            "parent": _current_task.get(),
            "caller": caller,
            "payload": _secret.shown(payload, marks),
            "named": name is not None,
            "retries": retries,
            "attempts": 1,
            "timeout": timeout,
        }
        if name is not None:
            row["name"] = name
        if marks:
            row["secret"] = list(marks)  # the marks are paths, not values
        self.ctx.rows_put(target.id, row)
        # The side entry is the WHOLE engine: dedup and join key on it, retry
        # re-issues against it, dead-letter is reading it. `attempts` counts real
        # runs; `caller` scopes who may see the row through the `ledger` verb.
        self.ctx.ledger_put(
            exec_id,
            {
                "target": target.id,
                "fut": fut,
                "payload": payload,
                "timeout": timeout,
                "retries": retries,
                "attempts": 1,
                "named": name is not None,
                "name": name,
                "caller": caller,
            },
        )
        return fut, None

    def _spawn_dispatch(
        self,
        target: "Agent",
        payload: dict,
        *,
        task_id: int,
        marks: tuple = (),
        sink: list | None = None,
    ) -> tuple[int, "asyncio.Task"]:
        """Start ONE dispatch as a registered asyncio.Task — the unit of the task
        table — under the message id its call row was opened with. Tasks never
        mint: the id is the call's. Synchronous: the pid exists before any await,
        which is what lets a batch report every pid up front."""
        ctx = self.ctx
        # Captured HERE, before the dispatch rewrites `_current_sender` to the
        # target's own id — this is the same value the gate just authorized.
        origin = _current_sender.get()
        task = asyncio.create_task(
            target._dispatch_task(
                task_id, payload, origin, marks, sink=[] if sink is None else sink
            ),
            name=f"fantastic:{task_id}",
        )
        ctx.register_task(
            task_id,
            target.id,
            payload.get("type"),
            task,
            parent=_current_task.get(),
        )
        return task_id, task

    async def _settle(self, d: "_Dispatched") -> dict:
        """One item's settle. Runs as its own gather task, so the cancellation
        contract from `_run_dispatch` ports verbatim: a CancelledError at
        `await d.task` with our OWN cancelling() == 0 means the CHILD was
        killed (→ outcome cancelled); cancelling() > 0 means WE are being
        killed (→ take the child down, re-raise; gather propagates out of
        `send` — the one thing that does).

        A JOINER (caller-named execution already known to the ledger) never
        touches the task: it awaits the ledger FUTURE the owner settles, so the
        outcome — reflect flags included — is computed exactly once. Its own
        `timeout` bounds only its wait; it does not own the execution, so
        expiry settles the joiner `timed_out` and leaves the execution alone."""
        while True:
            out = await self._settle_once(d)
            if not out.pop("retrying", False):
                return out
            row = self.ctx.executions[d.pid]
            row["attempts"] += 1
            call = self.ctx.rows.get(d.pid)
            if call is not None:
                call["attempts"] = row["attempts"]  # the tape counts real runs
            # forget_task has already dropped the previous attempt's row, so the
            # SAME task id is free to register again — one id, one `cancel`
            # handle, attempt after attempt.
            d.sink = []  # a fresh attempt notes its own reply marks
            _pid, task = self._spawn_dispatch(
                d.target, d.payload, task_id=d.pid, marks=d.marks, sink=d.sink
            )
            d.task = task

    async def _settle_once(self, d: "_Dispatched") -> dict:
        if d.joined:
            try:
                if d.timeout is not None:
                    done, _pending = await asyncio.wait(
                        {asyncio.ensure_future(asyncio.shield(d.exec_fut))},
                        timeout=d.timeout,
                    )
                    if not done:
                        return outcome(
                            d.pid,
                            TIMED_OUT,
                            refusal(
                                TIMEOUT,
                                f"send: joined execution {d.pid!r} did not settle "
                                f"within this caller's {d.timeout}s wait; the "
                                "execution itself keeps running",
                                task_id=d.pid,
                            ),
                        )
                recorded = await asyncio.shield(d.exec_fut)
            except asyncio.CancelledError:
                cur = asyncio.current_task()
                if cur is not None and cur.cancelling() > 0:
                    raise  # we were killed; the execution is not ours to kill
                return outcome(d.pid, CANCELLED, {"cancelled": True})
            return dict(recorded)
        try:
            if d.timeout is not None:
                done, pending = await asyncio.wait({d.task}, timeout=d.timeout)
                if pending:
                    if d.pid is not None:
                        # THE FRONT DOOR. `cancel_task` kills the SUBTREE and
                        # runs every bundle's `on_cancel` first (exec_bridge
                        # SIGKILLs its OS child) — a bare task.cancel() here
                        # would orphan them. Each hook runs as its own agent
                        # (`Agent.on_cancel`), never as the batch caller.
                        await self.ctx.cancel_task(d.pid)
                    else:
                        # pid-less identity read: nothing real to release
                        d.task.cancel()
                    try:
                        await d.task
                    except asyncio.CancelledError:
                        cur = asyncio.current_task()
                        if cur is not None and cur.cancelling() > 0:
                            raise  # we were killed mid-timeout-handling
                        return self._record(
                            d,
                            outcome(
                                d.pid,
                                TIMED_OUT,
                                refusal(
                                    TIMEOUT,
                                    f"send: {d.target.id} "
                                    f"{d.payload.get('type')!r} exceeded its "
                                    f"{d.timeout}s timeout; task cancelled",
                                    task_id=d.pid or "-",
                                ),
                            ),
                        )
                    # else: the handler finished during the SIGTERM grace —
                    # fall through and report honestly what happened.
            reply = await d.task  # instant when already done
            if d.pid is None:  # fast path already ran _apply_reflect_flags
                return outcome(None, COMPLETED, reply)
            # A PEER's reply is that kernel's own answer, decorated THERE — a
            # reflect of `<guid>|kernel` is its tree, not something to re-render
            # here (and there is no local agent to render it from).
            if not d.flags_done and not isinstance(d.target, RemoteTarget):
                reply = await self._apply_reflect_flags(d.target, d.payload, reply)
            # FAILURE IS A STATUS. A handler that RAN and said no — a missing
            # path, a patch that did not match, a refusal it composed itself —
            # settles `errored`, not `completed`. The reply is kept verbatim
            # either way; what changes is that a reader learns the outcome from
            # `status` instead of parsing `result`.
            settled = (
                ERRORED if isinstance(reply, dict) and reply.get("error") else COMPLETED
            )
            return self._record(d, outcome(d.pid, settled, reply))
        except asyncio.CancelledError:
            cur = asyncio.current_task()
            if cur is not None and cur.cancelling() > 0:
                if not d.task.done():
                    d.task.cancel()  # caller cancelled: child dies with us
                # The OWNER dying must still settle the ledger, or every joiner
                # (and every future re-send of this id) hangs on a dead future.
                # Settled DIRECTLY — cancel is absorbing, no retry consideration.
                if d.exec_fut is not None and not d.exec_fut.done():
                    out = outcome(d.pid, CANCELLED, {"cancelled": True})
                    d.exec_fut.set_result(out)
                    self._stamp_call(d.pid, out)
                raise
            if d.task.cancelled():
                return self._record(d, outcome(d.pid, CANCELLED, {"cancelled": True}))
            raise
        except Exception as e:  # noqa: BLE001 — a handler raise is a RESULT
            # An OSError reads as POSIX python prints it on every OS (`_sys`).
            raised = _sys.posix_error(e.__cause__ if isinstance(e, _Exited) else e)
            return self._record(
                d,
                outcome(
                    d.pid,
                    FAILED,
                    {"error": f"{type(raised).__name__}: {raised}", "raised": True},
                ),
            )

    def _record(self, d: "_Dispatched", out: dict) -> dict:
        """Settle the ledger future of a caller-named execution — the ONE place
        an owned execution's outcome is recorded. Unnamed executions have no
        future and pass through untouched.

        THE RETRY ENGINE IS FOUR LINES HERE. A terminal `failed`/`timed_out`
        with budget left marks the row for another attempt instead of settling;
        the settle loop re-dispatches under the SAME id (same `ps`/`cancel`
        handle — cancelling the id kills the current attempt AND the loop).
        `completed` never retries; `cancelled` never retries — a cancel is an
        absorbing human decision, not a transient. Joiners and re-senders only
        ever see the FINAL outcome: the future settles once, at the end.

        THE REPLY MARKS are settled here too (`_settle_marks`): an outcome that
        names secret paths in its reply carries them as `secret`, so the caller
        that forwarded the call can record the same paths. Omitted when none."""
        marks = self._settle_marks(d, out)
        if marks:
            out = {**out, "secret": marks}
        if d.exec_fut is None:
            return out
        row = self.ctx.executions.get(d.pid)
        if (
            row is not None
            and out["status"] in (FAILED, TIMED_OUT)
            and row["attempts"] <= row["retries"]
        ):
            out["retrying"] = True  # the settle loop reads this, callers never do
            return out
        if not d.exec_fut.done():
            if row is not None:
                out = dict(out)
                out["attempts"] = row["attempts"]
            d.exec_fut.set_result(out)
            self._stamp_call(d.pid, out)
        return out

    def _settle_marks(self, d: "_Dispatched", out: dict) -> list:
        """The reply marks an outcome settles with: the target bundle's
        `reply_secret` names plus the marks the dispatch noted
        (`note_reply_secret`), well-formed (a str, not `type`, no empty segment),
        each once, in order."""
        raw = list(self._reply_marks(d.target.id, d.payload, out.get("result")))
        raw.extend(d.sink or ())
        marks: list = []
        for m in raw:
            if (
                isinstance(m, str)
                and m
                and m != "type"
                and "" not in m.split(".")
                and m not in marks
            ):
                marks.append(m)
        return marks

    def _stamp_call(self, pid: int | None, out: dict) -> None:
        """Settle the call: append a `settle` row carrying what the kernel KEEPS of
        the outcome (`stored_outcome`: the whole result for a named call, the error
        text for a dead letter, `{status, bytes}` otherwise).

        THE SETTLE IS ITS OWN ROW — a row is append-only, so the ending is an
        append, never an edit of the call row. With the call row it is the record
        of what a call became, and it is what the kernel flushes into its memory
        store and reads back for `ledger` and for named-call dedup.

        The call row may already be flushed (and gone from RAM) by now; the target
        comes from the live execution. A dead ending (failed / timed_out /
        cancelled) is ANNOUNCED — `on_deadletter` — because nothing retries it on
        its own: someone has to look."""
        if pid is None:
            return
        row = self.ctx.rows.get(pid)
        exe = self.ctx.executions.get(pid)
        res = out.get("result")
        if isinstance(res, dict):
            # The error text is read from the SHOWN result: a value the bundle
            # marked secret (an `error` path that echoes it) never reaches the row.
            res = _secret.shown(res, out.get("secret") or ())
        stamp = {"status": out.get("status")}
        if isinstance(res, dict) and res.get("error"):
            stamp["error"] = str(res.get("error"))[:200]
            if res.get("reason"):
                stamp["reason"] = res.get("reason")
        if row is not None:
            row["outcome"] = stamp  # the live row, while it is still in RAM
        attempts = out.get("attempts")
        if attempts is None and exe is not None:
            attempts = exe["attempts"]
        target = (exe or {}).get("target") or (row or {}).get("agent") or self.id
        # ON THE ROOT: a settle is KERNEL bookkeeping; `settled` names the target.
        self.ctx.rows_put(
            self.ctx.root.id,
            {
                "kind": "settle",
                "parent": pid,
                "settled": target,
                "outcome": stored_outcome(
                    out,
                    bool((exe or {}).get("named")),
                    out.get("secret") or (),
                ),
                "attempts": attempts,
            },
        )
        if out.get("status") in DEAD_STATUSES:
            self.ctx.announce_deadletter(pid, out, attempts)

    def advertises(self, verb: str) -> bool:
        """Whether this agent answers `verb`: its bundle's `VERBS` table, or the
        agent's own table when the bundle names one through `instance_verbs(agent)`
        (the same per-instance rule `docs` follows)."""
        if not self.handler_module:
            return False
        try:
            mod = import_handler(self.handler_module)
        except Exception:  # noqa: BLE001 — an unimportable bundle advertises nothing
            return False
        own = getattr(mod, "instance_verbs", None)
        verbs = own(self) if callable(own) else None
        if verbs is None:
            verbs = getattr(mod, "VERBS", None)
        return isinstance(verbs, dict) and verb in verbs

    def module_hook(self, name: str):
        """A duck-typed module-level hook of this agent's bundle, or None."""
        if not self.handler_module:
            return None
        try:
            mod = import_handler(self.handler_module)
        except Exception:  # noqa: BLE001
            return None
        fn = getattr(mod, name, None)
        return fn if callable(fn) else None

    def _resolve_agent(self, agent_id: object) -> "Agent | None":
        """Resolve an id the way `send` does — the reserved `kernel` address or a literal id.
        None when it names no live agent."""
        if not isinstance(agent_id, str) or not agent_id:
            return None
        if agent_id == "kernel":
            return self._root()
        return self.ctx.agents.get(agent_id)

    def _qualified_target(self, target_id: str) -> "Agent | RemoteTarget | None":
        """`<guid>|<agent id>`: this kernel's own guid names a LOCAL agent; the
        guid of a CONNECTED PEER names an agent there (a `RemoteTarget` the send
        hands to that peer's live routes); anything else names nothing."""
        guid, bar, agent_id = target_id.partition("|")
        if not bar or not guid or not agent_id or "|" in agent_id:
            return None
        if guid == self._root()._meta.get("guid"):
            return (
                self._root() if agent_id == "kernel" else self.ctx.agents.get(agent_id)
            )
        routes = self.ctx.peers.get(guid)
        return RemoteTarget(target_id, guid, agent_id, routes, self) if routes else None

    def _gate(self, sender_id: str, verb: str, target: "Agent"):
        """The two-sided call gate. Returns an error dict if the send is refused,
        else None.

        EGRESS — the sender's `can_call` must grant `(target, verb)` — AND INGRESS
        — the target's `callable_by` must grant `(sender, verb)`, both via the one
        `authorize_call` engine. `"*"` covers every verb. A kernel verb answered by
        the root takes the same check against the root — the tree verbs
        (`create_agent`/`update_agent`/`delete_agent`) included: GRANTS ARE THE ONLY
        RULE, so whoever holds the verb on the root acts on any
        agent it names. The sender is always a local agent — a call a bridge leg
        serves is that leg's."""
        sender = self.ctx.agents.get(sender_id)
        sender_rec = self.ctx.get(sender_id) or {"id": sender_id}
        s_can = sender_rec.get("can_call") or {}
        s_parent = sender_rec.get("parent_id")
        # Egress — may the SENDER call the target? A target on ANOTHER KERNEL is
        # named `<guid>|<agent>`; whether that agent accepts the call is its OWN
        # kernel's decision, made where the agent lives — egress is the whole
        # check here.
        d = authorize_call(
            s_can,
            verb,
            target.id,
            holder_id=sender_id,
            holder_parent=s_parent,
            other_ancestors=ancestor_ids(target),
        )
        if isinstance(target, RemoteTarget):
            if d.allowed:
                return None
            return refusal(
                UNAUTHORIZED,
                f"{sender_id} may not call {verb!r} on {target.id}",
                hint=(
                    f"{target.id} is an agent on the connected kernel {target.guid}: "
                    f"name it in can_call — {{{target.id!r}: [...]}} or "
                    f"{{'{target.guid}|*': [...]}}"
                ),
                side="egress",
            )
        if not d.allowed:
            return refusal(
                UNAUTHORIZED,
                f"{sender_id} may not call {verb!r} on {target.id}",
                hint=_FROM_ABOVE,
                side="egress",
            )
        # Ingress — does the TARGET accept the sender? (tokens relative to target)
        di = authorize_call(
            target._meta.get("callable_by") or {},
            verb,
            sender_id,
            holder_id=target.id,
            holder_parent=target.parent.id if target.parent is not None else None,
            other_ancestors=ancestor_ids(sender),
        )
        if not di.allowed:
            return refusal(
                UNAUTHORIZED,
                f"{target.id} does not accept {verb!r} from {sender_id}",
                hint=_FROM_ABOVE,
                side="ingress",
            )
        return None

    async def _run_dispatch(self, target: "Agent", payload: dict) -> dict | None:
        """Run ONE dispatch as a registered, abortable asyncio.Task and await
        it — internal single-item helper kept for `_roster`, which needs a
        dispatch WITHOUT `_apply_reflect_flags` (no recursion). The public
        surface is the array `send`; both share `_spawn_dispatch`.

        The cancellation contract, and why it is not ambiguous. Both cases
        surface as a `CancelledError` at this one await:
          - THE DISPATCH was cancelled (someone sent `cancel`) → the awaiting
            caller is NOT being killed; it gets the reply `{"cancelled": True}`.
            Cancellation never raises out of the dispatch.
          - WE were cancelled (our own task is inside the killed subtree) →
            `current_task().cancelling()` is non-zero, so we re-raise and take
            the dispatch down with us. Swallowing here would resurrect a task
            the kernel just killed.
        """
        # An internal dispatch is a call like any other: it takes a message id
        # and a call row (`internal: True` tells a reader it was the kernel's own
        # roster read, not a caller's send).
        pid = self.ctx.mint_id()
        self.ctx.rows_put(
            target.id,
            {
                "id": pid,
                "kind": "call",
                "verb": payload.get("type"),
                "parent": _current_task.get(),
                "caller": _current_sender.get(),
                "payload": payload,
                "internal": True,
            },
        )
        _pid, task = self._spawn_dispatch(target, payload, task_id=pid)
        try:
            return await task
        except asyncio.CancelledError:
            cur = asyncio.current_task()
            if cur is not None and cur.cancelling() > 0:
                if not task.done():
                    task.cancel()
                raise
            if task.cancelled():
                return {"cancelled": True}
            raise

    def _reflect_identity(self) -> dict:
        """Uniform reflect identity for a bare agent (the root, or any
        node without a handler_module): id + sentence + record fields +
        flat meta. Bundle agents answer reflect via their own handler;
        the substrate appends the tree/bundles/readme flags uniformly in
        `_apply_reflect_flags`, so root is NOT special-cased."""
        node: dict[str, Any] = {
            "id": self.id,
            "sentence": self._sentence(),
            "parent_id": self.parent.id if self.parent else None,
            "handler_module": self.handler_module,
            "display_name": self.display_name or self.id,
        }
        if self.description is not None:
            node["description"] = self.description
        for k, v in self._meta.items():
            node.setdefault(k, v)
        return node

    def _sentence(self) -> str:
        if self.parent is None:
            return (
                "Fantastic kernel. Everything is reachable by sending "
                "messages to agents."
            )
        return "Bare agent (no handler_module) — answers substrate verbs only."

    async def _roster(self, a, ignore: set = frozenset()) -> dict:
        """`{id, sentence, mode?, children:[...]}` — the tree, but carrying what
        each agent is FOR instead of how it is wired.

        Same nesting as `_tree` (ownership is real information, and every caller
        already walks it that way); the swap is the FIELDS. `_tree` gives
        `handler_module` + posture — an operator's view, asked once. This gives
        `sentence` + `mode` — the router's view, asked on every message. Two agents of
        the same bundle are indistinguishable in the former and obvious here.

        The sentence is ASKED FROM THE AGENT ITSELF (its own reflect), never
        mirrored into a parallel source, so it cannot drift from what the agent
        actually says. `_run_dispatch` (not `send`) is deliberate: it skips
        `_apply_reflect_flags`, so building a roster cannot recurse into
        building rosters.
        """
        if a.handler_module:
            raw = await self._run_dispatch(a, {"type": "reflect"})
        else:
            raw = a._reflect_identity()
        raw = raw if isinstance(raw, dict) else {}
        node: dict[str, Any] = {"id": a.id}
        if raw.get("sentence"):
            node["sentence"] = raw["sentence"]
        # Operator-set purpose. More specific than the bundle sentence when
        # present ("the prod deploy key" vs "a yaml store"), so it belongs
        # in the router's view above all else.
        if a.description is not None:
            node["description"] = a.description
        # Purpose comes from the agent's REFLECT, not its record: much of what an
        # agent is FOR lives in its reflect (its `sentence`, its verbs), not as a
        # record field, so a record-only read reports nothing for exactly the agents
        # whose purpose you most need — and would be a second source of truth.
        for k in self._PURPOSE_KEYS:
            if raw.get(k) is not None:
                node[k] = raw[k]
        # A TASK LINE is state the agent was given (`agentic_resources` `spawn` /
        # `report`), not a purpose its bundle states: the tree reads it off the record,
        # so a lead sees its subtree's plan and progress in one `reflect tree=all`.
        if a._meta.get("task") is not None:
            node["task"] = a._meta["task"]
        node["children"] = [
            _docs.pruned_stub(c) if c.id in ignore else await self._roster(c, ignore)
            for c in a._children.values()
        ]
        return node

    async def _apply_reflect_flags(self, target, payload, reply):
        """Compose any reflect reply with the universal flags — applied
        uniformly to bare-agent and bundle reflects alike:

        - `detail=distilled|full` (default distilled). DISTILLED answers the
          question a caller actually has most messages — *who do I send to?* — with
          a roster of `{id, sentence, mode?}` and nothing else. FULL is the
          wiring view: record fields and posture on every node. The full view is
          NOT a superset: it carries `handler_module` and posture but no
          `sentence`, so two agents of the same bundle are indistinguishable in it.
        - `tree=all|ids|none` (default all): `all` nests the distilled
          subtree; `ids` is a flat descendant-id index; `none` omits it.
          `show_subtrees=[ids]` answers with only those branches (a list);
          `ignore_subtrees=[ids]` prunes each to `{id, pruned: n}`.
        - `bundles=all|ids|none` (default none): `all` is the
          {name, handler_module} catalog; `ids` is bare names; `none`
          omits it.
        - `readme=true`: attach the agent's `readme.md` orientation (string
          or null). `readme="index"`: the token-annotated topic tree.
          `readme="<key>"`: one topic's body + its sub-docs' index.
          Atomic — one agent.

        A reflect says what an agent IS, never how to call it: no verbs, emits or
        cancel — those are the `docs` kernel verb. Transport/
        wire docs live in the doc tree (`reflect readme=index`, then by key)."""
        if payload.get("type") != "reflect" or not isinstance(reply, dict):
            return reply
        # A REFUSAL IS NOT A REFLECT. Decorating one with a tree would produce a
        # reply that reads like a healthy reflect with a stray error field, and a
        # caller could not tell the two apart.
        if "error" in reply:
            return reply
        full = payload.get("detail") == "full"
        # `description` is a substrate meta field — surface it on EVERY
        # reflect (bundle handlers don't know about it), unless the
        # reply already set one.
        if target.description is not None and "description" not in reply:
            reply["description"] = target.description
        # YOUR OWN AUTHORITY, on your own reflect. A bundle handler composes the
        # reply and knows nothing of the substrate's gate fields, so they are added
        # here — the same way `description` is. Absent means "baseline only", which
        # is what the kernel stamps; it never means "unknown".
        for _gate in ("can_call", "callable_by"):
            if _gate in target._meta and _gate not in reply:
                reply[_gate] = target._meta[_gate]
        # Kernel runtime identity + deployment context — surfaced on the ROOT
        # reflect so a client that hops to this kernel learns, in one
        # round-trip: which runtime (`runtime`), WHERE it runs (`env` —
        # "container" when launched from the image, else "host"), and which
        # build (`version`). env/version are read from the optional
        # FANTASTIC_ENV / FANTASTIC_VERSION envs the container bakes in; they
        # are RUN-scoped (never persisted to the portable .fantastic workdir,
        # which can move host↔container). Same field names + key order
        # (runtime → env → version) across all four runtimes.
        if target.parent is None:
            reply["runtime"] = "python"
            # The kernels this one is CONNECTED to — each agent there is reachable
            # as `<guid>|<agent id>` (reflect `<guid>|kernel` for its tree).
            reply["peers"] = self.ctx.peer_rows()
            reply["env"] = os.environ.get("FANTASTIC_ENV", "host")
            reply["version"] = os.environ.get("FANTASTIC_VERSION")
            # Duck-typed root extension: the root's handler_module (the loader) may
            # contribute root-only fields the substrate can't compute without coupling
            # to bundle logic — e.g. `persistence: {provider}` (which file_bridge the
            # loader auto-persists THROUGH, or null = RAM). No bundle import: if the
            # module exposes `reflect_root_extra(agent) -> dict`, merge it.
            # A hook that RAISES fails the reflect: swallowed, it would drop
            # `persistence` and `guid` — what an operator reads to learn whether
            # state persists — with nothing to say so.
            if target.handler_module:
                hook = getattr(
                    import_handler(target.handler_module),
                    "reflect_root_extra",
                    None,
                )
                if hook is not None:
                    extra = hook(target, full=full)
                    if isinstance(extra, dict):
                        reply.update(extra)
        # NOTE: the identity fields (`parent_id`, `handler_module`, `guid`,
        # `version`, `display_name`) are NOT trimmed in distilled. Dropping them
        # was tried: it saves ~50 tok of a ~600 tok payload and breaks 10 tests,
        # because they are not trivia — `guid` is the kernel INSTANCE identity a
        # relay peer advertises as `owner_guid`, `version` names the build, and
        # `parent_id` is structure. Cheap fields with real consumers stay.
        tree = payload.get("tree", "all")
        if tree in ("all", "ids"):
            # SUBTREE FILTERS: `show_subtrees` answers with only
            # those branches, `ignore_subtrees` prunes each named branch to a stub
            # `{id, pruned: n}`; together, show first and ignore inside it.
            filt = _docs.subtree_filter(payload, target, self.ctx.agents)
            if isinstance(filt, dict):
                return filt
            show, ignore = filt
            roots = show or [target]
        if tree == "all":
            nodes = [
                _docs.pruned_stub(r)
                if r.id in ignore
                else r._tree(depth=None, details=False, current_depth=0, ignore=ignore)
                if full
                else await self._roster(r, ignore)
                for r in roots
            ]
            reply["tree"] = nodes if show else nodes[0]
        elif tree == "ids":
            # The FLAT id index is the one reflect shape that is a plain list, so
            # it is the one that pages. `tree:"all"` is a nested subtree whose
            # value is its structure — slicing it would hand back a forest with
            # no root and call it a page. Ask for ids to walk a big tree.
            flat = [i for r in roots for i in r._descendant_ids(ignore)]
            ids, nxt = page(
                flat, payload, key=lambda x: x["id"] if isinstance(x, dict) else x
            )
            reply["tree"] = ids
            if nxt is not None:
                reply["next"] = nxt
        bundles = payload.get("bundles", "none")
        if bundles == "all":
            cat, nxt = page(
                target._available_bundles(schemas=True),
                payload,
                key=lambda b: b["name"],
            )
            reply["bundles"] = cat
            if nxt is not None:
                reply["bundles_next"] = nxt
        elif bundles == "ids":
            names, nxt = page([b["name"] for b in target._available_bundles()], payload)
            reply["bundles"] = names
            if nxt is not None:
                reply["bundles_next"] = nxt
        rm = payload.get("readme")
        if rm is True:
            reply["readme"] = target._read_readme()
        elif isinstance(rm, str) and rm:
            reply["readme"] = target._readme_view(rm)
        return reply

    def _descendant_ids(self, ignore: set = frozenset()) -> list:
        """Flat id index of self + all descendants (DFS, self first). An id in
        `ignore` stands as its stub `{id, pruned: n}`, its branch unwalked."""
        if self.id in ignore:
            return [_docs.pruned_stub(self)]
        out: list = [self.id]
        for c in self._children.values():
            out.extend(c._descendant_ids(ignore))
        return out

    def child_ids(self) -> list[str]:
        """Public: ids of this agent's DIRECT children (insertion order). Lets a
        bundle enumerate children without reaching into the private `_children`."""
        return list(self._children.keys())

    @staticmethod
    def _available_bundles(schemas: bool = False) -> list[dict]:
        """Entry-point-discovered installable bundles, sorted by name.
        Recomputed live, so a freshly installed bundle shows up on the
        next reflect.

        With `schemas`, an entry also carries `schema` when its handler module
        publishes `RECORD_SCHEMA` — what that KIND's record takes (`fields`),
        readable before any agent of it exists. Duck-typed like `validate_meta`, which enforces the same
        schema at the write. Importing the module is the cost of asking, so the
        cheap `bundles=ids` listing never does it."""
        out = []
        for ep in entry_points(group=BUNDLE_ENTRY_GROUP):
            if not bundle_enabled(ep.value):
                continue
            entry = {"name": ep.name, "handler_module": ep.value}
            if schemas:
                sch = getattr(importlib.import_module(ep.value), "RECORD_SCHEMA", None)
                if sch is not None:
                    entry["schema"] = sch
            out.append(entry)
        # FOLDER BUNDLES of this kernel's root, beside the installed ones — a
        # refused folder too, with its `error`.
        out += folder_rows()
        return sorted(out, key=lambda b: b["name"])

    async def emit(self, target_id: str, payload: dict, *, quiet: bool = False) -> None:
        """Drop a payload into target_id's inbox + tell watchers.
        Non-routing — does NOT invoke any handler. Telemetry sees an
        `emit` event.

        QUIET: the payload reaches the target's inbox and its
        watchers and NOTHING is published on the state stream — a streaming
        `part_delta` arrives at token rate, and the state stream carries a part's
        lifecycle, never its every delta.

        GATED LIKE A SEND when an agent emits on ANOTHER agent (verb `emit`), so an
        open ws leg cannot drop events on any inbox, an AI's input stream
        included. Emitting on your OWN inbox — how every
        bundle publishes its events — and an operator emit are ungated. A
        refused emit is dropped: an emit has no reply channel.

        `<guid>|<agent>` naming a CONNECTED PEER reaches EVERY live route of that
        guid (every tab of one identity), gated like a send to
        it (verb `emit`, egress only — the peer gates its own agents)."""
        if "|" in target_id:
            remote = self._qualified_target(target_id)
            if isinstance(remote, RemoteTarget):
                await self._emit_remote(remote, payload)
                return
        target = self.ctx.agents.get(target_id)
        if target is None:
            return
        sender = _current_sender.get()
        if sender is not None and sender != target_id:
            if self._gate(sender, "emit", target) is not None:
                return
        target._fanout(payload, kind="emit", publish=not quiet)

    async def _emit_remote(self, remote: "RemoteTarget", payload: dict) -> None:
        """Hand one emit to every live route of a peer guid. A refused emit is
        dropped."""
        sender = _current_sender.get()
        if sender is not None:
            if self._gate(sender, "emit", remote) is not None:
                return
        # TODO: DESYNC between the tabs of one identity is envisioned
        # and unsolved — each tab gets the event, nothing reconciles what they do
        # with it. Think later.
        for route in remote.routes:
            await route.emit(remote.agent_id, payload)

    def watch(self, src_id: str, tgt_id: str) -> dict | None:
        """Mirror src_id's incoming traffic into tgt_id's inbox. Returns a
        refusal when the gate denies it, else None.

        GATED LIKE A SEND, because it IS a read of the target. A watcher
        receives src's raw inbound payloads verbatim — the model prompt, the
        document body, whatever was sent — so an ungated `watch` is a read
        side-channel around `callable_by`: an agent that may not CALL an agent
        could still read everything sent TO it.

        Ungated, a browser leg granted exactly `assistant:[send]` could mirror
        ANY agent's inbox in its session and read the packed confidential
        context out of the model's prompt.

        The verb checked is `watch`, so an operator grants it exactly like any
        other edge (`callable_by: {viewer: ["watch"]}`). An OPERATOR watch
        (`sender=None` — the bootstrap wiring a renderer, a test) is the origin
        root of trust and stays ungated, as with send."""
        src = self.ctx.agents.get(src_id)
        if src is None:
            return None  # nothing to mirror; not a refusal
        sender_id = _current_sender.get()
        if sender_id is not None:
            denial = self._gate(sender_id, "watch", src)
            if denial is not None:
                return denial
        # A WATCHER MUST ALREADY BE A READER. Lazy-creating the queue here would
        # let a caller watch and never drain: the payloads would pile up to
        # `INBOX_BOUND` and then be evicted one per message, forever, with nothing
        # to say so. `ensure_inbox` is the registration; making one here on the
        # watcher's behalf would invent a consumer that does not exist.
        if tgt_id not in self.ctx.inboxes:
            return refusal(
                NO_CONSUMER,
                f"watch: {tgt_id!r} has no registered inbox, so nothing would "
                "read what is mirrored there",
                hint=(
                    f"register the reader first — ctx.ensure_inbox({tgt_id!r}) "
                    "— then watch"
                ),
                watcher=tgt_id,
                src=src_id,
            )
        src._watcher_ids.add(tgt_id)
        return None

    def authorize_state_subscribe(self, watcher_id: str) -> dict | None:
        """May `watcher_id` tap the kernel-wide STATE STREAM? Refusal, or None.

        The stream carries every send's sender, target, verb and payload summary
        for every agent in the tree, so subscribing is the broadest read the
        kernel offers — broader than any single `watch`. It is therefore gated on
        the ROOT, exactly as a kernel verb is: the operand is the whole tree, so
        the edge must be named explicitly (`can_call: {<root>: ["state_subscribe"]}`).

        An ingress rule answers "may this client reach this leg"; it says nothing
        about what the client may then READ. Conflating the two is what let a
        leg with an open `ingress_rule` and no grants tap the whole stream."""
        # An OPERATOR tap (`sender=None` — the bootstrap wiring a telemetry
        # view, the CLI, a test) is the origin root of trust and stays ungated,
        # exactly as with send and watch. `watcher_id` names WHO would read;
        # whether the read is gated at all depends on the ORIGIN.
        if _current_sender.get() is None:
            return None
        return self._gate(watcher_id, "state_subscribe", self._root())

    def unwatch(self, src_id: str, tgt_id: str) -> None:
        src = self.ctx.agents.get(src_id)
        if src is not None:
            src._watcher_ids.discard(tgt_id)

    def has_watchers(self, src_id: str) -> bool:
        """Whether anything still mirrors src_id's traffic: a watcher that has
        registered and not yet detached. An agent that is gone has none."""
        src = self.ctx.agents.get(src_id)
        return src is not None and bool(src._watcher_ids)

    # ─── public API: record CRUD ───────────────────────────────

    def get(self, agent_id: str) -> dict | None:
        """Flat global record lookup. Returns the record dict (id,
        handler_module, parent_id, **meta) or None."""
        a = self.ctx.agents.get(agent_id)
        return a.record if a else None

    def list(self) -> list[dict]:
        """Flat list of every agent's record across the whole tree (the same as
        `Kernel.list()`). For own-children only, use `list_children()`."""
        return [a.record for a in self.ctx.agents.values()]

    def list_children(self) -> list[dict]:
        """Own children's records only."""
        return [c.record for c in self._children.values()]

    def update(self, agent_id: str, **meta: Any) -> dict | None:
        """Flat global record update. Patches the target's meta dict,
        persists, emits `agent_updated`. Returns the updated record
        or None if no such agent."""
        target = self.ctx.agents.get(agent_id)
        if target is None:
            return None
        target._meta.update(meta)
        # No disk write — the `updated` event drives a loader to re-persist.
        self.ctx.publish_state(
            {"agent_id": agent_id, "kind": "updated", "changed": list(meta.keys())}
        )
        return target.record

    def create(self, handler_module: str, id: str | None = None, **meta: Any) -> dict:
        """Spawn a child of THIS agent. Sync — only persists the
        record + registers in ctx; does NOT fire boot. The system
        verb `create_agent` wraps this with `await send(..., boot)`.

        `agent.create` always creates a child of the calling agent.
        To create a top-level agent from within a handler, walk to
        root explicitly: `agent._root().create(...)`.
        """
        if id is None:
            bundle = (
                handler_module.split(".")[-2]
                if "." in handler_module
                else handler_module
            )
            id = f"{bundle}_{secrets.token_hex(3)}"
        # An id BECOMES A DIRECTORY (kernel_state joins it onto the store root),
        # so it must be one safe segment. Enforced at the mint, not at each
        # consumer.
        if (bad_id := guard_agent_id("create_agent", id)) is not None:
            return bad_id
        if id in self.ctx.agents:
            return refusal(
                EXISTS,
                f"agent {id!r} exists",
                hint="pick another id, or update the existing agent",
                id=id,
            )
        # Agent.__init__ wires up parent._children + publishes the
        # `added` state event when parent is set.
        child = Agent(
            id=id,
            ctx=self.ctx,
            parent=self,
            handler_module=handler_module,
            **meta,
        )
        return child.record

    def ensure(self, id: str, handler_module: str, **meta: Any) -> dict:
        """Idempotent create as direct child of self. If an agent with
        `id` already exists ANYWHERE in the tree, return its record
        without mutating. Otherwise create as child of self."""
        existing = self.ctx.agents.get(id)
        if existing is not None:
            return existing.record
        return self.create(handler_module, id=id, **meta)

    async def delete(self, agent_id: str, *, keep_memory: bool = False) -> dict:
        """Cascade-delete an agent and its entire subtree. Returns:
        - {"deleted": True, "id": agent_id} on success
        - {"locked": True, "blocked_by": <id>, "id": agent_id} if any
          descendant has delete_lock=true (no mutations)
        - {"error": "..."} on bad input

        `keep_memory=True` tells every `on_delete` hook of the subtree to leave
        the agents' persisted memory alone (see `keep_memory`); it lasts for this
        one delete.

        Cascade is depth-first: deepest descendants run their
        `_shutdown` hook (kernel-routed, so process state tears down)
        BEFORE being removed from ctx.agents/parent._children/disk.
        By the time the topmost agent is gone, every PTY/uvicorn task/
        in-flight resource downstream is already torn down.
        """
        target = self.ctx.agents.get(agent_id)
        if target is None:
            return self.ctx.unknown_agent(agent_id, deleted=False)
        # THE ROOT IS NOT DELETABLE. It owns the persistence surface and answers
        # every kernel verb, so cascading it empties the kernel and leaves a
        # process with no control surface at all — `{"deleted": true}` and an
        # empty tree.
        if target.parent is None:
            return refusal(
                BAD_ARG,
                f"delete_agent: {agent_id} is the ROOT — it owns persistence and "
                "answers the kernel verbs; deleting it would leave no kernel",
                arg="id",
                id=agent_id,
                deleted=False,
            )
        blocked_by = target._find_locked_descendant()
        if blocked_by is not None:
            # THE ONE REFUSAL SHAPE: a coded `reason`, never a bare `locked: True`
            # flag a caller would have to know `delete_agent` to read.
            return refusal(
                LOCKED,
                f"delete_agent: {agent_id!r} blocked by delete_lock on "
                f"{blocked_by!r}; clear it via update_agent before deleting",
                hint="clear the lock with update_agent, then delete again",
                blocked_by=blocked_by,
                id=agent_id,
            )
        gone = set(target._subtree_ids())
        kept = getattr(self.ctx, _KEEP_MEMORY, None)
        if kept is None:
            kept = set()
            setattr(self.ctx, _KEEP_MEMORY, kept)
        added = (gone - kept) if keep_memory else set()
        kept.update(added)
        try:
            await target._cascade_delete()
        finally:
            kept.difference_update(added)
        self._prune_grants(gone)
        return {"deleted": True, "id": agent_id}

    def _subtree_ids(self):
        yield self.id
        for c in self._children.values():
            yield from c._subtree_ids()

    def _prune_grants(self, gone: set) -> None:
        """A deleted id leaves every grant that named it. Grant keys are literal
        ids, so a stale one would hand its edge to whoever re-creates that id
        Each record changed is re-persisted by its `updated`."""
        for agent in tuple(self.ctx.agents.values()):
            changed = []
            for field in ("can_call", "callable_by"):
                grants = agent._meta.get(field)
                if isinstance(grants, dict) and gone.intersection(grants):
                    agent._meta[field] = {
                        k: v for k, v in grants.items() if k not in gone
                    }
                    changed.append(field)
            if changed:
                self.ctx.publish_state(
                    {"agent_id": agent.id, "kind": "updated", "changed": changed}
                )

    # ─── public API: state stream ──────────────────────────────

    def add_state_subscriber(self, callback: Callable[[dict], None]):
        """Tap the state stream (agent lifecycle + traffic telemetry). Conversation
        rows are NOT on it: the kernel holds none — an AI agent announces its own
        rows to its watchers (`watch`, gated by that agent)."""
        return self.ctx.add_state_subscriber(callback)

    def state_snapshot(self) -> list[dict]:
        return self.ctx.state_snapshot()

    # ─── dispatch (internal) ───────────────────────────────────

    async def _dispatch_task(
        self,
        task_id: str,
        payload: dict,
        origin: str | None = None,
        marks: tuple = (),
        sink: list | None = None,
    ) -> dict | None:
        """The body of a registered task: stamp `_current_task` so nested sends
        become CHILDREN of this task, then dispatch. The contextvar is set (not
        wrapped in try/finally) because an asyncio task owns a COPY of the
        context — the stamp cannot leak to the caller. The table entry drops via
        the task's done-callback, cancelled or not."""
        _current_task.set(task_id)
        # The gate read the caller before dispatch; carry it so a handler whose
        # PAYLOAD names a resource can tell whose it is. `_current_sender` is
        # about to become this agent's own id, which is why this is separate.
        _current_origin.set(origin)
        _current_secret.set(marks)
        _reply_sink.set(sink)
        try:
            return await self._dispatch(payload)
        except SystemExit as e:
            # A SystemExit out of an asyncio task stops the EVENT LOOP — the whole
            # kernel, for one handler (or one handler module's import-time code,
            # reached by any `create_agent` under yourself) that called sys.exit
            # Here it is a raise like any other: THIS
            # call settles failed, the loop runs on.
            raise _Exited() from e

    @contextlib.contextmanager
    def _as_self(self):
        """THE DISPATCH IDENTITY — the kernel running this agent's code: its
        handler (`_dispatch`) and its hooks (`on_cancel`, `on_delete`). Every send
        made inside, and every task spawned inside, is this agent's. It is the one
        place an identity is set; the kernel sets it only when it runs an agent's
        own code."""
        token = _current_sender.set(self.id)
        try:
            yield
        finally:
            _current_sender.reset(token)

    async def _kernel_run(self, payload: dict) -> dict | None:
        """THE KERNEL RUNS THIS AGENT'S HANDLER for its own bookkeeping — the boot
        at `create_agent` and `kernel_start`, the `shutdown` of a stop, the reads
        and writes of the kernel's ledger in its memory store (`Kernel.bookkeeping`).
        A plain call, not a send: no gate, no task row, no call row. The handler
        runs as this agent (its sends and spawned tasks are its own) with NO
        origin — no caller asked; the kernel did. A raise propagates."""
        token = _current_origin.set(None)
        stoken = _current_secret.set(())
        rtoken = _reply_sink.set(None)
        try:
            return await self._dispatch(payload)
        finally:
            _reply_sink.reset(rtoken)
            _current_secret.reset(stoken)
            _current_origin.reset(token)

    async def _dispatch(self, payload: dict) -> dict | None:
        """Resolve verb → native or handler_module, run as this agent."""
        verb = payload.get("type")
        self._in_flight += 1
        self._fanout(_secret.shown(payload, _current_secret.get()), kind="send")
        token = _current_sender.set(self.id)
        try:
            if verb in _SYSTEM_VERBS:
                # KERNEL-OWNED, answered by the ROOT ONLY. These act on the TREE
                # (or on the kernel-wide task table); the agent they name is the
                # OPERAND, never the actor. Answering them everywhere would make
                # the target ambiguous: with `create_agent` on all N agents and a
                # doc naming only its arguments, "send it to X" could mean X
                # performs it, X is created under something, or X is the parent.
                #
                # Refused HERE, in the handler, not in the call gate — the gate
                # runs long before this and several probes assert a specific
                # denial `side` for a kernel verb sent to a non-root agent.
                if self.parent is not None:
                    return refusal(
                        NOT_HERE,
                        f"{verb} is a kernel verb — {self.id} does not answer it",
                        hint="address the kernel root ('kernel'); name the "
                        "agent you mean in `parent_id` / `id`",
                        verb=verb,
                    )
                return await self._handle_system_verb(verb, payload)
            # The tree root answers `reflect` with the substrate identity
            # even when it carries a handler_module (the root loader): its
            # persistence verbs (load_tree / persist_record / boot) still
            # route to the bundle below, but `reflect <root>` is the
            # kernel's own identity + tree, matching the `kernel` address.
            if verb == "reflect" and self.parent is None:
                return self._reflect_identity()
            if not self.handler_module:
                # Bare agents (root) handle a few universal verbs natively:
                # - boot/shutdown: no-op (no process state to manage)
                # - reflect: uniform identity (the tree/bundles/readme
                #   flags are appended by _apply_reflect_flags in send())
                if verb in ("boot", "shutdown"):
                    return None
                if verb == "reflect":
                    return self._reflect_identity()
                # CODED, not a bare error. A reason-less `{error}` is decorated
                # by `_apply_reflect_flags` into something that reads like a
                # healthy reflect (a tree around it) with a stray error
                # field — the caller cannot tell a refusal from an answer.
                return refusal(
                    UNHANDLED,
                    f"agent {self.id!r} has no handler_module; cannot answer "
                    f"verb {verb!r}",
                    verb=verb,
                )
            try:
                mod = import_handler(self.handler_module)
            except Exception as e:
                # NOT a create-time refusal: this kernel legitimately holds
                # records whose bundle belongs to ANOTHER runtime (the browser's
                # `*.ts` agents persist here), or is installed later. The call
                # simply cannot be answered HERE, and says so with a code.
                return refusal(
                    UNHANDLED,
                    f"agent {self.id!r}: handler_module "
                    f"{self.handler_module!r} cannot be imported in this "
                    f"runtime: {type(e).__name__}: {e}",
                    verb=verb,
                )
            if not hasattr(mod, "handler"):
                return refusal(
                    UNHANDLED,
                    f"{self.handler_module} has no handler()",
                    verb=verb,
                )
            return await mod.handler(self.id, payload, self)
        finally:
            _current_sender.reset(token)
            self._in_flight = max(0, self._in_flight - 1)
            self.ctx.publish_state(
                {"agent_id": self.id, "kind": "drain", "backlog": self._in_flight}
            )

    def _fanout(self, payload: dict, *, kind: str, publish: bool = True) -> None:
        """Publish state event for self's traffic + mirror to watchers.

        Watchers may be agents OR synthetic ids (browser WS clients
        registered by the webapp proxy). Both have entries in
        `ctx.inboxes`. Fanout puts on `ctx.inboxes[watcher_id]` when one is
        there and skips silently when it is not — an id with no registered
        reader has no queue, by construction.

        State events carry the full payload (`payload` field) for
        observers — bundle tests + telemetry pane both subscribe.
        Summary stays as the trimmed one-line view for visualization.
        Telemetry rays drop synthetic-id watchers (would mint phantom
        sprites in the agent vis); agent watchers do publish.
        """
        sender = _current_sender.get()
        summary = _summarize_payload(payload)
        own = self.ctx.inboxes.get(self.id)
        if own is not None:
            self._put_drop_oldest(own, payload)
        if publish:
            self.ctx.publish_state(
                {
                    "agent_id": self.id,
                    "kind": kind,
                    "backlog": self._in_flight,
                    "sender": sender,
                    "summary": summary,
                    "payload": payload,
                }
            )
        for tgt_id in tuple(self._watcher_ids):
            tgt_inbox = self.ctx.inboxes.get(tgt_id)
            if tgt_inbox is None:
                continue
            self._put_drop_oldest(tgt_inbox, payload)
            tgt_agent = self.ctx.agents.get(tgt_id)
            if tgt_agent is not None and publish:
                self.ctx.publish_state(
                    {
                        "agent_id": tgt_id,
                        "kind": kind,
                        "backlog": tgt_agent._in_flight,
                        "sender": sender,
                        "summary": summary,
                        "payload": payload,
                    }
                )

    @staticmethod
    def _put_drop_oldest(q: asyncio.Queue, payload: dict) -> None:
        try:
            q.put_nowait(payload)
        except asyncio.QueueFull:
            try:
                q.get_nowait()
            except asyncio.QueueEmpty:
                pass
            try:
                q.put_nowait(payload)
            except asyncio.QueueFull:
                pass

    # ─── system verbs (native) ─────────────────────────────────

    async def _handle_system_verb(self, verb: str, payload: dict) -> dict | None:
        if verb == "list_agents":
            return self._verb_list_agents(payload)
        if verb == "create_agent":
            return await self._verb_create_agent(payload)
        if verb == "update_agent":
            return await self._verb_update_agent(payload)
        if verb == "delete_agent":
            return await self._verb_delete_agent(payload)
        if verb == "ps":
            return self._verb_ps(payload)
        if verb == "ledger":
            return await self._verb_ledger(payload)
        if verb == "ack_deadletter":
            return await self._verb_ack_deadletter(payload)
        if verb == "cancel":
            return await self._verb_cancel(payload)
        if verb == "kernel_start":
            return await self._verb_kernel_start(payload)
        if verb == "shutdown_agents":
            return await self._verb_shutdown_agents(payload)
        if verb == "kernel_stop":
            return await self._verb_kernel_stop(payload)
        if verb == "docs":
            of = payload.get("of")
            if isinstance(of, str) and "|" in of:
                return await self._remote_docs(of, payload)
            return self._verb_docs(payload)
        return refusal(UNHANDLED, f"unhandled system verb {verb!r}", verb=verb)

    async def _remote_docs(self, of: str, payload: dict) -> dict:
        """`docs` of an agent on a CONNECTED KERNEL, `of: <guid>|<agent id>`. The
        question goes to THAT kernel's own `docs` — which answers any
        caller its gate admits — and the reply names the qualified id. The asker's
        own `docs` grant on THIS root is what the gate checked; this kernel then asks
        as itself over the peer's live routes, and the peer judges its leg. With verbs
        out of reflect, this is how a model learns a peer agent's verbs at all."""
        guid, _, agent_id = of.partition("|")
        target = (
            self._qualified_target(f"{guid}|kernel")
            if guid and agent_id and "|" not in agent_id
            else None
        )
        if target is None:
            return refusal(
                BAD_ARG,
                f"docs: {of!r} names no agent, bundle or 'kernel'",
                hint="a qualified id names an agent on a CONNECTED kernel "
                "(`<guid>|<agent id>`, as a reflect tree lists it)",
                arg="of",
            )
        if not isinstance(target, RemoteTarget):  # this kernel's own guid
            return self._verb_docs({**payload, "of": agent_id})
        ask = {"type": "docs", "of": agent_id}
        for k in ("verb", "tier"):
            if k in payload:
                ask[k] = payload[k]
        calls = [asyncio.ensure_future(r.call("kernel", ask)) for r in target.routes]
        try:
            done, _ = await asyncio.wait(calls, return_when=asyncio.FIRST_COMPLETED)
        finally:
            for c in calls:
                c.cancel()
        out = next(c for c in calls if c in done).result()
        if isinstance(out, dict) and "of" in out:
            out = {**out, "of": of}
        return out

    def _verb_docs(self, payload: dict) -> dict:
        """args: of:str (req), tier:str?, verb:str?. HOW TO CALL an agent — its
        bundle's verb docs (`kernel/_docs.py`). `of` is `kernel` (the kernel
        verbs), an agent id (the bundle it runs), or a bundle — by its listed
        name or its handler module — readable before any agent of it exists."""
        of = payload.get("of")
        if not isinstance(of, str) or not of:
            return missing(
                "docs",
                "of",
                hint="an agent id, a bundle (e.g. 'exec_bridge.tools') or 'kernel'",
            )
        tier = payload.get("tier", "oneliners")
        if tier not in _docs.TIERS:
            return bad("docs", "tier", "'oneliners' or 'detailed'")
        verb = payload.get("verb")
        if verb is not None and (not isinstance(verb, str) or not verb):
            return bad("docs", "verb", "a verb name")
        if of == "kernel":
            return _docs.render(of, "kernel", tier, verb, dict(_SYSTEM_VERB_DOCS), {})
        agent = self.ctx.agents.get(of)
        if agent is not None:
            module = agent.handler_module
            if not module:
                return _docs.render(of, None, tier, verb, {}, {})
        else:
            # A BUNDLE by either name the kernel lists it under: `bundles='ids'`
            # lists the NAME (`file_bridge`), an agent record carries the
            # handler_module (`file_bridge.tools`); models ask for the listed name.
            module = (
                of
                if is_folder(of) or _in_catalog(of)
                else _find_bundle_module(of, self.ctx)
            )
            if module is None:
                return refusal(
                    BAD_ARG,
                    f"docs: {of!r} names no agent, bundle or 'kernel'",
                    hint="an agent id from a reflect tree, a bundle's name or "
                    "handler_module (reflect bundles='all' lists both), or 'kernel'",
                    arg="of",
                )
        mod = import_handler(module)
        table, extras = _docs.module_docs(mod)
        if agent is not None:
            # PER-INSTANCE VERBS: a bundle whose agents serve
            # different verbs (an MCP server's tools) names each instance's own
            # table through the duck-typed `instance_verbs(agent)`; None keeps the
            # bundle's. A bundle named by `of` (no agent) shows its own table.
            own = getattr(mod, "instance_verbs", None)
            table = (own(agent) if callable(own) else None) or table
        return _docs.render(of, module, tier, verb, table, extras)

    def _verb_ps(self, payload: dict) -> dict:
        """No args. The live TASK TABLE — {tasks:[{id, agent, parent, verb,
        started_at}]}. Kernel-wide (like `list_agents`), not per-agent: one row
        per in-flight dispatch, `parent` chaining them into the send tree, so a
        caller sees the whole tree and who spawned what. `ps` shows its own row
        too — it is itself a dispatch."""
        tasks, nxt = page(self.ctx.task_rows(), payload, key=lambda r: r["id"])
        return {"tasks": tasks, "next": nxt}

    async def _verb_ledger(self, payload: dict) -> dict:
        """No args. EVERY execution and what it became — named under its id,
        unnamed under its minted pid — {executions:[{id, target, status,
        attempts, retries, named, outcome?, acked?}]}, paged. `status` is
        'in_flight' until the execution settles, then the outcome's own status.

        A PROJECTION, not a table: the executions still in RAM (in flight, or
        settled and not yet flushed) merged with the `call`/`settle`/`ack` rows the
        kernel flushed into its memory store — each call paired with its settle.
        Rows whose status is failed/timed_out/cancelled are the DEAD-LETTER QUEUE:
        terminal, inspectable, never silently retried; `ack_deadletter` retires one."""
        # CALLER-SCOPED. The ledger carries other agents' RESULT BODIES; an
        # unscoped read would be a side-channel around `callable_by` (read any
        # agent's replies without its ingress ever saying yes). An agent sees
        # the executions IT originated; the operator (sender=None) sees all.
        viewer = _current_origin.get()
        entries = await self.ctx.ledger_entries()
        rows = [
            e for e in entries if viewer is None or e.pop("_caller", None) == viewer
        ]
        for e in rows:
            e.pop("_caller", None)
        page_rows, nxt = page(rows, payload, key=lambda r: r["id"])
        return {"executions": page_rows, "next": nxt}

    async def _verb_ack_deadletter(self, payload: dict) -> dict:
        """args: id:int|str (req — an execution id, or its name). Acknowledge a DEAD
        LETTER you originated (the operator: any). The ack is a row in the
        kernel's store; an acked dead letter may be pruned. Returns {acked, id}."""
        raw = payload.get("id")
        if raw is None or raw == "" or isinstance(raw, bool):
            return missing("ack_deadletter", "id")
        viewer = _current_origin.get()
        entries = await self.ctx.ledger_entries()
        eid = _as_id(raw)
        hit = next(
            (
                e
                for e in entries
                if (eid is not None and e["id"] == eid)
                or (eid is None and e.get("name") == raw)
            ),
            None,
        )
        if hit is None or (viewer is not None and hit.get("_caller") != viewer):
            return refusal(
                NOT_FOUND,
                f"ack_deadletter: no execution {raw!r} of yours in the ledger",
                arg="id",
            )
        if hit.get("status") not in DEAD_STATUSES:
            return refusal(
                BAD_ARG,
                f"ack_deadletter: execution {hit['id']} is {hit.get('status')!r}, not a dead "
                "letter — only failed / timed_out / cancelled work is acknowledged",
                arg="id",
            )
        if hit.get("acked"):
            return {"acked": True, "id": hit["id"], "already": True}
        self.ctx.rows_put(
            self.ctx.root.id,
            {"kind": "ack", "parent": hit["id"], "caller": viewer},
        )
        exe = self.ctx.executions.get(hit["id"])
        if exe is not None:
            exe["acked"] = True
        return {"acked": True, "id": hit["id"]}

    async def _verb_cancel(self, payload: dict) -> dict:
        """args: task_id:int — kill that task AND ITS SUBTREE (children first,
        depth-first, like killing a process group). Each bundle's `on_cancel`
        hook runs BEFORE the abort so real resources (OS subprocesses, PTYs)
        die with it. Returns {cancelled:True, killed:[task_id, ...]}; an unknown
        or already-finished id returns {cancelled:False, killed:[]} — it does
        not pretend. The cancelled dispatch's blocked caller gets the reply
        {"cancelled": True}."""
        raw = payload.get("task_id")
        if (
            raw is None
            or raw == ""
            or isinstance(raw, bool)
            or not isinstance(raw, (int, str))
        ):
            return missing("cancel", "task_id")
        task_id = _as_id(raw)
        if task_id is None and isinstance(raw, str):
            # A caller's NAME for an execution is its handle too.
            task_id = self.ctx.resolve_name(raw)
        if task_id is None:
            # Names no task — honest, not a refusal (the same answer a stale id gets).
            return {"cancelled": False, "killed": []}
        return await self.ctx.cancel_task(task_id)

    async def _verb_kernel_start(self, payload: dict) -> dict:
        """No args. The start sequence — see `Kernel.start`. Idempotent."""
        return await self.ctx.start(boot_all=True)

    async def _verb_shutdown_agents(self, payload: dict) -> dict:
        """No args. The agent half of a graceful stop: drain, the `shutdown` verb
        to every agent that advertises it, quiet, cancel the rest. The kernel keeps
        serving; the flusher catches up. Root control surface only."""
        return await self.ctx.shutdown_agents()

    async def _verb_kernel_stop(self, payload: dict) -> dict:
        """Gracefully stop the whole kernel PROCESS (root control surface only).

        Order: (1) ack `{type:"kernel_stop", ok:true}` and let it flush; (2) AFTER
        the reply is on the wire, trigger the daemon's graceful path — drain, every
        call settles (done, or cancelled into the dead letters), the last execution
        rows flush into the kernel's store, agents shut down, the lock releases —
        (3) exit code 0. The exit is DEFERRED via `call_later` so the in-flight
        REST body / WS frame is fully written before uvicorn is drained.

        The kernel is PID 1 in a container, so its exit stops the container; a
        bare host process simply dies. Idempotent: reuses the daemon's single
        `stop` event, so a racing SIGTERM collapses to one shutdown. With no serve
        loop (one-shot CLI / REPL-only) there is nothing to stop — it still acks."""
        ev = self.ctx.shutdown_event
        if ev is not None and not ev.is_set():
            asyncio.get_running_loop().call_later(0.1, ev.set)
        return {"type": "kernel_stop", "ok": True}

    _LIST_HINT = (
        "Raw records — how agents are WIRED, not what they are FOR, and no manual. "
        "<type>reflect</type> on the root is the briefing: each agent's purpose, plus "
        "`self_extension` (how to author your own durable tools in THIS kernel — which "
        "executor, and the fields without which the agent is created and never runs) "
        'and, with `detail:"full"`, `readmes` (the manual index). Reflect the root '
        "before building anything."
    )

    def _verb_list_agents(self, payload: dict) -> dict:
        # Flat list of every agent's record (global registry). For
        # own-children only, callers use the `list_children` Python
        # method on the Agent instance, or walk a `reflect` tree.
        #
        # `more` is not decoration. A model that opens with `list_agents` gets bare
        # records and never learns what `reflect` would have told it; the FIRST call
        # can decide the whole run. This verb is advertised in every reflect's
        # `system_verbs`, so "list the agents" is a natural opener, and without
        # `more` it is a dead end with no signpost back.
        agents, nxt = page(
            [a.record for a in self.ctx.agents.values()],
            payload,
            key=lambda r: r["id"],
        )
        return {"agents": agents, "next": nxt, "more": self._LIST_HINT}

    @staticmethod
    def _refuse_meta(
        handler_module: str | None, meta: dict, record: dict | None = None
    ) -> str | None:
        """A bundle's own objection to this meta patch, or None. Optional + duck-typed.

        `record` is the agent's CURRENT record (`{}` at create, where none exists yet),
        so a bundle validates the state the write WILL produce rather than the patch in
        isolation. Without it a cross-field rule is unenforceable: a rule that reads two
        record fields together cannot judge a patch that names only one, and validating
        the patch alone rejects a legal write because the field it must be checked
        against lives on the existing record and was not in the same
        payload. A false refusal is worse than no check: it makes the correct operation
        look impossible.

        ONE implementation, used by BOTH `create_agent` and `update_agent`, so a value
        refused on update is refused on create too. The argument the update path
        makes applies verbatim at create — accepting a value you cannot honour and
        reporting success is the worst of both, and it is worse at create because there
        is no prior good state to fall back to.
        """
        if not handler_module:
            return None
        try:
            validate = import_handler(handler_module).validate_meta
        except (ImportError, AttributeError):
            return None
        return validate(meta, record or {})

    @staticmethod
    def _bad_grant_shape(meta: dict) -> str | None:
        """A grant map's SHAPE, judged at WRITE time — the gate only judges edges.

        `{id|token: "*" | [verbs]}` — anything else is refused here, so a record
        can never carry a grant the gate would have to crash or guess on (a bare
        string `can_call:"*"` would make the gate raise out of a web serve)."""
        for field in ("can_call", "callable_by"):
            if field not in meta:
                continue
            g = meta[field]
            if not isinstance(g, dict):
                return (
                    f'{field} must be a grant MAP {{"<id|$token>": "*" | '
                    f'["<verb>", …]}} — got {type(g).__name__}. To open wide, '
                    f'grant per key (e.g. {{"$self": "*", "ai": "*"}}), never a '
                    f"bare value"
                )
            for k, v in g.items():
                if bad_grant_key(field, k):
                    return (
                        f"{field}[{k!r}] is not a grant key — legal keys: "
                        f"{LEGAL_GRANT_KEYS[field]}"
                    )
                if not (
                    v == "*"
                    or (isinstance(v, list) and all(isinstance(x, str) for x in v))
                ):
                    return f'{field}[{k!r}] must be "*" or a list of verb strings'
        return None

    async def _verb_create_agent(self, payload: dict) -> dict:
        """args: handler_module, parent_id (REQUIRED), id?, **meta.

        `parent_id` NAMES THE OWNER. The placement is always stated; the
        addressed agent is never the implicit parent. There is no implicit parent — an
        agent building its own subtree names itself, which is one word and
        cannot be confused with "wherever this happened to be sent"."""
        handler_module = payload.get("handler_module")
        if not isinstance(handler_module, str) or not handler_module:
            return missing("create_agent", "handler_module")
        parent_id = payload.get("parent_id")
        if not isinstance(parent_id, str) or not parent_id:
            return missing(
                "create_agent",
                "parent_id",
                hint="name the agent that will OWN the new one — your own id for a "
                "tool you keep, or the root ('kernel_state') for a top-level agent",
            )
        # Resolve the same way `send` does, so `parent_id` accepts anything a
        # caller could have addressed — including the reserved `kernel` address for the root,
        # which is otherwise special-cased in `send` alone.
        if parent_id == "kernel":
            parent = self._root()
        else:
            parent = self.ctx.agents.get(parent_id)
        if parent is None:
            return self.ctx.unknown_agent(parent_id)
        # Reserved keys name substrate wiring; a record may not carry them (the
        # loader refuses such a record on the next boot).
        for k in sorted(RESERVED_RECORD_KEYS):
            if k in payload:
                return bad(
                    "create_agent",
                    k,
                    "absent — root_path/ctx/parent name substrate wiring the "
                    "record may not carry",
                )
        # `children_dir` BECOMES A DIRECTORY (`<agent>/<children_dir>/<child>`),
        # so it is one safe segment, exactly as an id is.
        if "children_dir" in payload and (
            bad_cd := guard_agent_id(
                "create_agent", payload["children_dir"], arg="children_dir"
            )
        ):
            return bad_cd
        # A handler_module this runtime cannot import is NOT refused: the host
        # legitimately holds records for agents of ANOTHER kernel (the browser's
        # `*.ts` bundles persist back here through `proxy_loader`), and a bundle
        # can also be installed later. What was wrong is the SILENCE downstream —
        # every later call answered a reason-less `{error: import …}` dressed up
        # as a healthy reflect. That is fixed at dispatch, where the truth is
        # known (see `_dispatch`), not by guessing here.
        meta = {
            k: v
            for k, v in payload.items()
            if k not in ("type", "handler_module", "id", "parent_id")
        }
        # The structural authority baseline ({$self,$parent,$subtree} on both
        # `can_call` and `callable_by`) is stamped + merged uniformly in Agent.__init__
        # for EVERY agent — so it is NOT re-stamped here; an explicit grant in the
        # payload rides in `meta` and merges over the baseline at construction.
        # Refuse BEFORE the agent exists, so a bad wiring value never becomes a record
        # that boots, half-works, and fails three calls later somewhere else.
        # No record exists yet, so the merged view IS the patch — a create that
        # omits a field its handler requires genuinely cannot run and is refused here.
        if why := self._bad_grant_shape(meta):
            return refusal(INVALID_META, why)
        if why := self._refuse_meta(handler_module, meta, {}):
            return refusal(INVALID_META, why)
        rec = parent.create(handler_module, id=payload.get("id"), **meta)
        if "error" in rec:
            return rec
        # BOOT — kernel bookkeeping: the kernel initializing the child it was told
        # to make, not the parent reaching out. The child's handler runs directly
        # (`Kernel.bookkeeping`), as the child, with no origin — never through a
        # gated send, never against the parent's own grant.
        boot = await self.ctx.bookkeeping(self.ctx.agents[rec["id"]], {"type": "boot"})
        if boot["status"] == FAILED:
            # Surfaced, not fatal: the agent exists, its boot broke.
            print(f"  [create_agent] {rec['id']} boot failed: {boot['result']}")
        # Lifecycle event on the PARENT's inbox, not the root's: whoever watches an
        # agent for new children still sees them arrive. Kernel bookkeeping too —
        # put on the parent's inbox and mirrored to its watchers directly
        # (`_fanout`), never a gated emit.
        parent._fanout(
            {"type": "agent_created", "id": rec["id"], "agent": rec}, kind="emit"
        )
        return rec

    async def _verb_update_agent(self, payload: dict) -> dict:
        """Set a record's meta. EVERY NAMED FIELD IS REPLACED, NOT MERGED.

        A field you do not name is untouched; a field you DO name overwrites what
        was there. For a dict field this means the whole map is swapped, never
        combined — send it WHOLE.

        The trap: granting one edge reads like a PATCH
        (`callable_by={"web":["read_stream"]}`) and behaves like a PUT — every
        other grant in the field (the `{$self: "*"}` baseline stamped in
        `Agent.__init__` included) is dropped, and the call still answers
        `{"updated": true}`. Send the map WHOLE.

        NOT refused when the baseline is absent: dropping a grant is a legitimate
        thing for an operator to express, and a wrong grant is recoverable with
        another `update_agent`. Documented, not gated.
        """
        target_id = payload.get("id")
        if not isinstance(target_id, str) or not target_id:
            return missing("update_agent", "id")
        # `parent_id` is STRUCTURE, not meta: an agent's place in the tree is
        # changed by creating it there, never by patching a field. Accepting it
        # here let a record describe a parent it did not have.
        #
        # `handler_module` is fixed at create — swapping it would leave a live
        # agent whose in-process state belongs to the OLD bundle. Reserved keys
        # name substrate wiring (`root_path`/`ctx`/`parent`); written into the
        # record they would be REFUSED by the loader on the next boot, so an
        # accepted update would make the workdir unbootable.
        for k in ("handler_module", *sorted(RESERVED_RECORD_KEYS)):
            if k in payload:
                return bad(
                    "update_agent",
                    k,
                    "absent — handler_module is fixed at create_agent, and "
                    "root_path/ctx/parent name substrate wiring the record "
                    "may not carry",
                )
        # `children_dir` BECOMES A DIRECTORY — one safe segment, as at create.
        if "children_dir" in payload and (
            bad_cd := guard_agent_id(
                "update_agent", payload["children_dir"], arg="children_dir"
            )
        ):
            return bad_cd
        meta = {
            k: v for k, v in payload.items() if k not in ("type", "id", "parent_id")
        }
        changed = list(meta.keys())
        target = self.ctx.agents.get(target_id)
        if target is None:
            return self.ctx.unknown_agent(target_id)
        # WRITE-TIME VALIDATION, duck-typed. A record can carry meta only its bundle
        # understands, so the substrate cannot judge it — but the bundle can. If the
        # handler_module exposes `validate_meta(meta) -> str | None`, a returned
        # string REFUSES the patch and is reported at the point of the mistake.
        #
        # Without this, `update_agent` would accept anything — an unknown
        # `ingress_rule` answered `{"updated": true}` and found meaningless only when
        # the next call to that agent fails. Accepting a value you cannot honour, and reporting
        # success, is the worst of both — the caller believes a security control is
        # in place that is not. Same discovery + optionality as `on_delete`.
        if why := self._bad_grant_shape(meta):
            return refusal(INVALID_META, why, id=target_id)
        if why := self._refuse_meta(target.handler_module, meta, target.record):
            return refusal(INVALID_META, why, id=target_id)
        rec = self.update(target_id, **meta)
        if rec is None:
            return self.ctx.unknown_agent(target_id)
        # Emit lifecycle event on self's inbox for watchers (canvas
        # frame chrome, panels) — same convention as create/delete.
        await self.emit(
            self.id,
            {
                "type": "agent_updated",
                "id": target_id,
                "changed": changed,
                "agent": rec,
            },
        )
        return {"updated": True, "id": target_id, "agent": rec}

    async def _verb_delete_agent(self, payload: dict) -> dict:
        target_id = payload.get("id")
        if not isinstance(target_id, str) or not target_id:
            return missing("delete_agent", "id")
        # Only the literal `true` keeps the memory (a string is not a yes).
        result = await self.delete(
            target_id, keep_memory=payload.get("keep_memory") is True
        )
        if result.get("deleted"):
            await self.emit(self.id, {"type": "agent_deleted", "id": target_id})
        return result

    # ─── tree walks ────────────────────────────────────────────

    def _find_locked_descendant(self) -> str | None:
        """DFS for any agent in self's subtree (including self) with
        `delete_lock=true`. Returns first locked id or None."""
        if self._meta.get("delete_lock"):
            return self.id
        for c in self._children.values():
            blocked = c._find_locked_descendant()
            if blocked is not None:
                return blocked
        return None

    async def _cascade_delete(self) -> None:
        """Recursive cascade. Runs deepest-first:
          1. For each child: recurse (their children die first).
          2. Call self.on_delete() — bundle teardown + disk cleanup.
          3. Unregister from ctx.agents + ctx.inboxes.
          4. Remove from parent's _children.
          5. Emit `removed` state event.

        By the time this returns, every node in the subtree is gone
        from process state AND disk. The on_delete hook is the single
        place bundles tear down process-memory state (PTY children,
        uvicorn servers, in-flight tasks) AND remove their disk
        artifact."""
        for cid in list(self._children.keys()):
            await self._children[cid]._cascade_delete()
        try:
            await self.on_delete()
        except Exception as e:
            print(f"  [cascade] {self.id} on_delete raised: {e}")
        self.ctx.unregister(self.id)
        if self.parent is not None:
            self.parent._children.pop(self.id, None)
        self.ctx.publish_state(
            {
                "agent_id": self.id,
                "kind": "removed",
                "name": self.display_name or self.id,
            }
        )

    async def on_delete(self) -> None:
        """Cascade hook — invoked once per agent during cascade-delete,
        depth-first (children first). Tears down bundle-specific
        process-memory state only.

        Default: if `handler_module` exposes `async def on_delete(agent)`,
        call it. Disk cleanup is NOT here — the `removed` state event
        (published right after this in `_cascade_delete`) drives a loader
        agent to rmtree the record's directory.

        Bundles port their teardown logic into a module-level
        `on_delete(agent)` function in their tools.py — substrate looks
        it up and invokes it.

        The hook runs AS THIS AGENT (its own id is the sender), whoever asked
        for the delete — like `on_cancel`."""
        if self.handler_module:
            try:
                mod = import_handler(self.handler_module)
                fn = getattr(mod, "on_delete", None)
                if fn is not None:
                    with self._as_self():
                        await fn(self)
            except Exception as e:
                print(f"  [cascade] {self.id} bundle on_delete raised: {e}")

    async def on_cancel(self, task_id: str) -> None:
        """Cancel hook — invoked once per agent in a cancelled subtree, BEFORE
        the asyncio.Task is aborted. The SIGTERM half of `cancel` (the abort is
        the SIGKILL half).

        Cancelling an asyncio.Task frees NO real resource: it unwinds Python
        coroutines and nothing else. A bundle holding an OS subprocess, a PTY,
        or a socket for that task MUST release it here — this is where the
        orphaned-subprocess class of bug is fixed.

        Default: if `handler_module` exposes `async def on_cancel(id, task_id,
        kernel)`, call it. Optional — a pure-async bundle needs nothing (same
        discovery + optionality as the `on_delete` cascade hook). A hook that
        raises is logged and the abort still happens: cancel is not refusable.

        THE HOOK RUNS AS THIS AGENT — its own id is the sender — on every path
        that kills the task: the `cancel` verb, an item's own deadline,
        `shutdown_agents`. An emit on its own inbox is then ungated and every
        other send is gated as this agent. Run as the ROOT (whose dispatch it
        sits in), a non-top-level agent's own-inbox emit — ai_core's final
        `status done` to a browser client — would be gated root → agent and
        dropped."""
        if not self.handler_module:
            return
        try:
            mod = import_handler(self.handler_module)
            fn = getattr(mod, "on_cancel", None)
            if fn is not None:
                with self._as_self():
                    await fn(self.id, task_id, self)
        except Exception as e:
            print(f"  [cancel] {self.id} bundle on_cancel raised: {e}")

    async def shutdown(self) -> None:
        """Graceful process-shutdown walk. Depth-first like
        cascade-delete, but DOES NOT touch records, disk, or tree
        membership — only invokes each bundle's teardown hook to free
        OS resources (subprocesses, PTYs, open sockets, in-flight
        tasks). The agent tree survives in `ctx.agents` and on disk,
        so the next boot rehydrates cleanly.

        Hook per agent: the bundle's module-level `on_shutdown(agent)` — and
        only that. A bundle whose shutdown is its delete teardown says so
        explicitly (`on_shutdown = on_delete`); an absent hook means nothing
        to free, never a silent fall back to `on_delete`. Run on SIGTERM /
        SIGINT / SIGHUP (see kernel.modes._default) and as an atexit safety
        net from main.py."""
        for cid in list(self._children.keys()):
            child = self._children.get(cid)
            if child is None:
                continue
            try:
                await child.shutdown()
            except Exception as e:
                print(f"  [shutdown] {cid} raised: {e}", file=sys.stderr)
        if not self.handler_module:
            return
        try:
            mod = import_handler(self.handler_module)
        except Exception as e:
            print(
                f"  [shutdown] {self.id} import {self.handler_module!r} raised: {e}",
                file=sys.stderr,
            )
            return
        fn = getattr(mod, "on_shutdown", None)
        if fn is None:
            return
        try:
            await fn(self)
        except Exception as e:
            print(
                f"  [shutdown] {self.id} hook raised: {e}",
                file=sys.stderr,
            )

    # ─── reflection ────────────────────────────────────────────

    def _root(self) -> "Agent":
        node = self
        while node.parent is not None:
            node = node.parent
        return node

    def _is_control(self, target_id: str, payload: dict) -> bool:
        """Is this send the root's CONTROL SURFACE — a kernel verb or `reflect`
        on the root — directly, or carried by a wire leg's `inbound_call`? The
        drain gate keeps it open while the kernel stops."""
        verb = payload.get("type")
        if verb == "inbound_call":
            item = payload.get("item")
            if not isinstance(item, dict) or not isinstance(item.get("payload"), dict):
                return False
            target_id, payload = item.get("target"), item["payload"]
            verb = payload.get("type")
        to_root = target_id == "kernel" or target_id == self._root().id
        return to_root and (verb in _SYSTEM_VERBS or verb == "reflect")

    # Wiring/posture meta surfaced inline in the DISTILLED tree (not only the full
    # `details` node), so a client reads the IO landscape from ONE root reflect: which
    # legs are open vs sealed-by-default, what a consumer is wired to. The substrate
    # only exposes its own record fields (no rule interpretation, no bundle import) —
    # the LLM reads `ingress_rule: deny_ingress` (sealed) / `allow_all` (open) /
    # `password` (gated) and acts per the keystone readme (read-key == write-key);
    # ABSENT on an io leg ⇒ sealed-by-default. `root` shows a file_bridge's binding,
    # `allow` what it exposes under that root.
    # The CALL gate belongs here too: `can_call`/`callable_by` decide whether a send
    # is even attempted, and with them absent an agent cannot read its OWN
    # authority — it can only discover it by trying, probing empirically to learn
    # by experiment what one field states. This is not a secrecy boundary:
    # the gate ENFORCES, it does not hide, and everything surfaced here is already
    # discoverable by attempting the call. Hiding it only makes discovery expensive.
    _POSTURE_KEYS = (
        "ingress_rule",
        "egress_rule",
        "auth",
        "root",
        "allow",
        "can_call",
        "callable_by",
    )
    # PURPOSE, not wiring. Posture answers "how is this plumbed" (an operator
    # question, asked once); purpose answers "what is this for" (a routing
    # question, asked on every message). The roster carries purpose.
    _PURPOSE_KEYS = ("mode",)

    def _node_summary(self, *, details: bool) -> dict:
        node: dict[str, Any] = {
            "id": self.id,
            "parent_id": self.parent.id if self.parent else None,
            "handler_module": self.handler_module,
            "display_name": self.display_name or self.id,
        }
        if self.description is not None:
            node["description"] = self.description
        if not details:
            for k in self._POSTURE_KEYS:
                if k in self._meta:
                    node[k] = self._meta[k]
            return node
        node.update(self._meta)
        node["in_flight"] = self._in_flight
        return node

    def _tree(
        self,
        *,
        depth: int | None,
        details: bool,
        current_depth: int,
        ignore: set = frozenset(),
    ) -> dict:
        # A reflect that passes the gate returns the tree whole.
        node = self._node_summary(details=details)
        if depth is None or current_depth < depth:
            node["children"] = [
                _docs.pruned_stub(c)
                if c.id in ignore
                else c._tree(
                    depth=depth,
                    details=details,
                    current_depth=current_depth + 1,
                    ignore=ignore,
                )
                for c in self._children.values()
            ]
        else:
            node["children"] = []
        return node

    # ─── reflect surface ───────────────────────────────────────
    #
    # The composable `reflect` flags (tree / bundles / readme) live in
    # `_apply_reflect_flags` (next to `send`); transport/wire docs live in
    # the root readme (`reflect readme=true`).
