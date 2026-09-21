"""The Kernel context — tree-wide shared state.

`Kernel` is NOT an agent and does NOT own a class hierarchy. It is a
plain context container created once when the first Agent is born
(see `_agent.Agent.__init__`) and passed by reference to every
descendant. Holds:

  - `agents`: flat global id → Agent index. The canonical place to
    look up any agent for routing. Send/emit/get/update all resolve
    through this dict.
  - `state_subscribers`: tree-wide telemetry tap. Callbacks see one
    event per send/emit/drain/lifecycle, regardless of which agent
    in the tree produced it.
  - `bundle_resolver`: cached entry-point lookups (bundle name →
    handler module). Hot path on every fresh-agent send.
  - `pending_forwards`: corr_id → asyncio.Future, for cross-tree
    `forward` reply correlation when bridges are involved.
  - `well_known`: short-name → agent_id index for named singletons
    (`webapp`, `file_root`, etc.).
  - `tasks`: the TASK TABLE — every in-flight dispatch, keyed by a
    task id (`"<agent_id>:<token>"`, a pid). Tasks form a TREE through the
    send chain; `ps` reads it, `cancel` kills a subtree.

`_current_sender` is a process-wide contextvar (not part of Kernel)
so that nested send/emit calls inside a handler attribute back to
the dispatching agent. ContextVars are task-local in asyncio, so
concurrent handlers don't trample each other. `_current_task` mirrors
it exactly, carrying the CURRENT TASK ID — that is how a nested send
learns which task is its parent.
"""

from __future__ import annotations

import asyncio
import contextlib
import contextvars
from collections.abc import Coroutine
import importlib
import json
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import TYPE_CHECKING, Any, Callable

from kernel._refusal import UNKNOWN_AGENT, refusal
from kernel._state import CURRENT_VERSION, validate_records

if TYPE_CHECKING:
    from kernel._agent import Agent


# Contextvar set during a handler's dispatch so that nested send/emit
# calls — which fire FROM INSIDE the handler — know who's calling.
# Surfaces in state events as `sender`. None when send/emit is invoked
# from outside any handler (the WS proxy, `fantastic call`, REPL).
_current_sender: contextvars.ContextVar[str | None] = contextvars.ContextVar(
    "_current_sender", default=None
)

# The TASK-TABLE twin of `_current_sender`: the id of the task whose dispatch
# is running right now. Set inside the dispatch task (contexts are copied per
# asyncio task, so it is task-local), read by the next `send` to record its
# PARENT — which is what makes the table a tree. None outside any dispatch
# (the WS/REST surface, `fantastic <id> <verb>`, the REPL, a test) — such a
# task is a root of the table, like a process spawned by init.
_current_task: contextvars.ContextVar[int | None] = contextvars.ContextVar(
    "_current_task", default=None
)

# THE SHUTDOWN SEQUENCE'S OWN SENDS. While the kernel drains, a send with no live
# parent task is refused — except the kernel's own `shutdown` verbs, which run
# under this marker (see `Kernel.shutdown_agents` and `Agent._launch`).
_in_shutdown: contextvars.ContextVar[bool] = contextvars.ContextVar(
    "_in_shutdown", default=False
)


@contextlib.contextmanager
def shutdown_context():
    """Mark the sends inside as the kernel's own shutdown traffic."""
    tok = _in_shutdown.set(True)
    try:
        yield
    finally:
        _in_shutdown.reset(tok)


# WHO CALLED THIS DISPATCH. `_current_sender` is rewritten to the TARGET's id
# the moment a handler runs (`Agent._dispatch`), so a handler cannot see its
# caller — the gate is the only place that identity has ever existed. That is
# fine while every authorization decision is `(sender, target, verb)`, which the
# gate owns. It is NOT fine when a verb's PAYLOAD names a resource: `persist_blob`
# takes the namespace to write as `agent_id`, so without the caller the loader
# cannot tell "persist my own memory" from "overwrite someone else's".
#
# Measured 2026-08-23: an agent holding exactly
# `can_call: {kernel_state: [persist_blob]}` wrote another agent's `state.yaml`,
# and the victim read the forged content back as its own memory. The believed
# defence was POSITION (the root accepts `$children`, a grandchild is not one) —
# but every agent an operator creates at the root IS a direct child.
#
# `None` means OPERATOR ORIGIN, which an agent cannot forge: the CLI, the cold
# bootstrap, a loader at boot, or a verb completing its own side effect under
# `operator_context`.
_current_origin: contextvars.ContextVar[str | None] = contextvars.ContextVar(
    "_current_origin", default=None
)


def current_origin() -> str | None:
    """The id that ORIGINATED this dispatch, or None for an operator send.

    Read it when a verb's payload names a resource the caller might not own.
    Do NOT use it for `(sender, target, verb)` decisions — that is the gate's
    job, it runs first, and duplicating it here would be a second truth.
    """
    return _current_origin.get()


def current_task() -> int | None:
    """The TASK id of the dispatch running right now — the MESSAGE id of the call
    that started it (an int from the one counter) — or None outside any dispatch.

    The PUBLIC, non-underscore read of `_current_task` — the twin of
    `sender_context`/`_current_sender`. A bundle that must know WHICH task it
    IS reads it here: an AI backend stamps it on its in-flight entry so its
    `on_cancel` hook can tell "the kernel is killing MY generation" from "the
    kernel is killing some other dispatch on the same agent" (a `history`, a
    `reflect`). Bundles never touch the contextvar itself."""
    return _current_task.get()


def page(items: list, payload: dict, *, key=None) -> tuple[list, str | None]:
    """Apply `limit` + `after` to one list reply. Returns `(page, next_cursor)`.

    UNBOUNDED LIST REPLIES ARE A REAL COST, not a conformance box. Measured
    2026-08-20: reflect bulk was ~42% of an AI agent's context, which is what
    forced `dedupe_stale_reads`, the reflect stub and the point-of-use
    distillation nudge in `ai_core` — every one of those shrinks what is
    RETAINED because nothing could shrink what is SENT. Paging attacks the cause.

    The cursor is OPAQUE by construction: it is the last item's identity, not an
    offset, so a caller cannot compute the next one or skip ahead by arithmetic.
    That also makes it stable under mutation in the one way that matters — a page
    resumes AFTER a named item, so items inserted before it are simply not
    revisited, and items appended after it are picked up. It is NOT a snapshot:
    over a live tree a cursor may miss an item deleted mid-walk and may re-show
    one that moved. This is the kernel, not a database; that is stated rather
    than papered over with a snapshot nobody asked to pay for.

    Absent `limit` ⇒ everything, `next: None`. So a caller that never learned
    about paging gets exactly what it always got — the default is unchanged, and
    the field is inert until asked for.
    """
    ident = key or (lambda x: x)
    after = payload.get("after")
    # An int cursor (a message id) and its decimal string are the same cursor.
    if isinstance(after, bool):
        after = None
    if after is not None and after != "":
        after = str(after)
        for i, item in enumerate(items):
            if str(ident(item)) == after:
                items = items[i + 1 :]
                break
        else:
            # A cursor naming something that is gone: the caller is walking a tree
            # that moved under it. Give it the tail rather than silently the WHOLE
            # list, which would look like a successful restart from the top.
            return [], None
    limit = payload.get("limit")
    if not isinstance(limit, int) or isinstance(limit, bool) or limit <= 0:
        return list(items), None
    head = list(items[:limit])
    nxt = str(ident(head[-1])) if head and len(items) > limit else None
    return head, nxt


@contextlib.contextmanager
def operator_context():
    """Run the enclosed sends with OPERATOR authority (`_current_sender` = None
    → the call gate skips them). This is NOT a privilege escalation for agent
    code: it is used only by the substrate to complete the side effects of an
    ALREADY-AUTHORIZED privileged verb — e.g. `create_agent` booting the child
    it was just authorized to make. The kernel initializing the tree it was told
    to build acts as the operator, not as the agent that asked."""
    token = _current_sender.set(None)
    try:
        yield
    finally:
        _current_sender.reset(token)


@contextlib.contextmanager
def sender_context(sender_id: str):
    """Attribute nested send/emit to `sender_id` for the duration of the
    block. A web/REST surface — which dispatches browser-originated traffic
    from OUTSIDE any handler — uses this to tag external calls with its own
    agent id so telemetry rays originate from its sprite. Mirrors the
    substrate's per-dispatch tagging in `Agent._dispatch`. This is the
    PUBLIC, non-underscore entry point bundles bind to; `_current_sender`
    itself stays a kernel internal."""
    token = _current_sender.set(sender_id)
    try:
        yield
    finally:
        _current_sender.reset(token)


# ─── dispatch outcomes ──────────────────────────────────────────
#
# The terminal states of ONE batch item, as `send` reports them. The task table
# itself stays LIVE-ONLY (an entry is forgotten the moment its task ends); an
# outcome is returned to the caller and retained nowhere.
#
#   completed — the target's handler ran and returned an answer that is not an
#               error. `result` is its reply VERBATIM; may be None.
#   errored   — the handler RAN and said NO: it returned a dict carrying a
#               truthy `error`. A semantic failure — a missing path, a patch
#               that did not match, a refusal the handler itself composed.
#               `result` is that reply, verbatim.
#
#               Until 2026-09-11 this settled `completed`, on the rule that the
#               kernel does not introspect domain replies. The rule did not
#               survive contact with its own storage: `_stamp_call` already
#               read `error`/`reason` off the result and wrote them onto the
#               durable row BESIDE `status: completed`, so one row said both
#               "fine" and "failed". Failure is a STATUS; nothing downstream
#               should have to parse a result to learn it.
#   failed    — no handler answer exists: the kernel refused before dispatch
#               (gate denial, unknown agent, malformed item) or the handler
#               raised ({"error": f"{type}: {e}", "raised": True}).
#   cancelled — the dispatch was killed via `cancel` while the batch awaited
#               it; `result` is {"cancelled": True}.
#   timed_out — the item's own deadline expired and the kernel killed it
#               through the front door (`cancel_task`, so `on_cancel` hooks
#               ran); `result` is a refusal(TIMEOUT, ...).
#
# Invariant: `pid is None` ⟺ nothing was ever in the task table (refusals,
# malformed items, the kernel-reflect fast path).
COMPLETED = "completed"
ERRORED = "errored"
FAILED = "failed"
CANCELLED = "cancelled"
TIMED_OUT = "timed_out"


def outcome(pid: int | None, status: str, result: dict | None) -> dict:
    """The one shape a batch item settles to: {pid, status, result}."""
    return {"pid": pid, "status": status, "result": result}


def _summarize_payload(payload: Any) -> str:
    """Compact one-line view of a payload for telemetry overlays.

    Bytes values become `<bytes:N>` so JSON serialization doesn't
    explode on binary protocol payloads (audio/image frames). Never
    raises; falls back to repr.

    NOT TRUNCATED. This used to trim to 160 chars + an ellipsis. A summary
    that silently drops the end of a payload is a telemetry view that lies
    about what was sent — and the reader cannot tell a short payload from a
    cut one. If a payload is long, the overlay's job is to show it long or
    to elide visibly at RENDER time, where the reader can see the choice.
    """

    def _shrink(o: Any) -> Any:
        if isinstance(o, bytes):
            return f"<bytes:{len(o)}>"
        if isinstance(o, dict):
            return {k: _shrink(v) for k, v in o.items()}
        if isinstance(o, list):
            return [_shrink(v) for v in o]
        return o

    try:
        s = json.dumps(_shrink(payload), default=str, ensure_ascii=False)
    except (TypeError, ValueError):
        s = repr(payload)
    return s


# Substrate constants — filesystem + entry-point conventions every
# Agent shares.
# Aligned with Rust's `DEFAULT_INBOX_BOUND = 256` (kernel/kernel.rs).
# Smaller-than-historical (was 500) to bound per-agent memory under
# bursty TUI output; drop-oldest still keeps the freshest events.
INBOX_BOUND = 256
BUNDLE_ENTRY_GROUP = "fantastic.bundles"


@dataclass
class Kernel:
    """Tree-wide shared state. Created implicitly by the first Agent
    born without a `ctx`. All descendants inherit the same instance
    by reference.

    Lifetime: from root Agent construction to root Agent destruction.
    Multiple roots in the same Python process get separate Kernel
    instances (clean test isolation, no cross-tree leakage).
    """

    # Flat global id → Agent index. Routes every send/emit, lookups by
    # id resolve here. Populated when an Agent is constructed; pruned
    # when an Agent is removed via cascade.
    agents: dict[str, "Agent"] = field(default_factory=dict)

    # Inbox per id — agents AND synthetic non-agent clients (browser
    # WS connections, etc.). Agents register their inbox here at
    # construction. The webapp WS proxy mints a synthetic id per
    # connection and registers an inbox to receive watched events.
    # Lives on ctx (not on Agent) so synthetic ids without an Agent
    # record still work.
    inboxes: dict[str, asyncio.Queue] = field(default_factory=dict)

    # Tree-wide telemetry tap. Direct-callback list — never routed
    # through send/emit/inboxes (no recursion path).
    state_subscribers: list[Callable[[dict], None]] = field(default_factory=list)

    # Entry-point cache: bundle name → handler module. Populated lazily
    # on first lookup; cleared (rare) only if entry_points change at
    # runtime (third-party bundle install).
    bundle_resolver: dict[str, str] = field(default_factory=dict)

    # Cross-tree forward correlation: corr_id → Future the sender
    # awaits. Populated on `forward` send, resolved on reply, cleaned
    # on timeout / cascade. Empty in pure-local trees.
    pending_forwards: dict[str, Any] = field(default_factory=dict)

    # Short-name → agent_id index for named singletons.
    well_known: dict[str, str] = field(default_factory=dict)

    # The on-disk children-container dir name — `<agent>/<children_dir>/<child>`.
    # Declared config (not hardcoded): the root record may carry a
    # `children_dir` meta (e.g. "host_agents"); `load` reads it. Default
    # "agents". A loader serving a foreign namespace (web_loader) carries its
    # own `children_dir` on its record. The JS kernel is the same kernel — it
    # has this field too, though it lays out no disk (its loader does).
    children_dir: str = field(default="agents")

    # The tree root — set when the first parent-less, non-ephemeral Agent
    # registers (the root loader in main.py). All `Kernel.create/list/...`
    # delegations route through it.
    root: "Agent | None" = field(default=None)

    # Set once `shutdown()` has walked the tree, so signal handlers
    # and the atexit safety net don't double-run bundle teardown.
    _shutdown_complete: bool = field(default=False)

    # The daemon serve loop's stop Event, published here so an in-kernel
    # verb (the root-only `shutdown_kernel`) can trigger the SAME graceful
    # exit a SIGTERM does — reachable from any handler via `agent.ctx`.
    # None outside daemon mode (one-shot / REPL-only / before the daemon
    # installs it) and must be created INSIDE the running loop (asyncio.
    # Event binds to a loop), so the daemon assigns it; never default it
    # to `asyncio.Event()` at dataclass-construction time.
    shutdown_event: "asyncio.Event | None" = field(default=None)

    # ─── the TASK TABLE ────────────────────────────────────────
    #
    # A Linux-style process table for WORK IN FLIGHT. The unit is a TASK —
    # ONE in-flight dispatch — not an agent: an agent may have many tasks
    # running at once, and each is separately addressable.
    #
    #   task id  = the MESSAGE id (an int from the one counter, `mint_id`)
    #   entry    = {id, agent, parent, verb, started_at, _task: asyncio.Task}
    #
    # Tasks form a TREE through the send chain: a `send` issued INSIDE a
    # handler is a CHILD of that handler's task (the current task id rides
    # `_current_task`, exactly as the sender rides `_current_sender`). That
    # tree is what makes `cancel` a process-GROUP kill rather than a single
    # abort. `_task` is the abort handle (a real asyncio.Task) and never
    # leaves the kernel — `ps` serves the five public fields only.
    tasks: dict[int, dict] = field(default_factory=dict)

    # THE ONE COUNTER. Every id in the kernel — a conversation row, a call (one
    # per batch item), a forked task, a reply body's pid, a checkpoint — is the
    # next value of this int64. Kernel-wide and monotonic for the process
    # lifetime: `a < b` means "a was created before b", across agents, and an
    # id is never reused (a stale `cancel` kills nothing). Not persisted by the
    # kernel: a loader that hydrates messages restores it as `max(id) + 1`
    # (see `kernel_state` — the kernel never guesses a counter).
    next_id: int = 1

    # THE EXECUTION LEDGER — caller-NAMED executions (a batch item's `id`).
    # `send([{.., id:"job-42"}])` means ENSURE that execution exists (the same
    # semantics `create_agent` gives a supplied agent id): unknown id ⇒ dispatch
    # and record; id in flight ⇒ JOIN it (no second run — C10); id settled ⇒ the
    # recorded outcome returns and no handler runs (C9/C11 idempotency). The id
    # is the SAME identifier `ps`/`cancel`/reply tags carry — one id space.
    # THE MESSAGE IS THE ROW; this dict is the PROCESS side of a call: its
    # future, the payload a retry re-issues, the attempt count. The call's
    # message row (`kind:"call"`, in `rows`) carries what outlives the process.
    # Entry: {"target", "fut": Future[outcome], "payload", "timeout",
    #         "retries", "attempts", "named", "name", "origin"}.
    executions: dict[int, dict] = field(default_factory=dict)

    # Caller NAMES → message id. A batch item's `id` is a NAME (ensure
    # semantics: unknown ⇒ dispatch, known ⇒ join); the execution's id is the
    # counter's. Forgotten when the ring evicts the execution.
    names: dict[str, int] = field(default_factory=dict)

    # LEDGER RETENTION — a RING: when the ledger exceeds the cap, the OLDEST
    # SETTLED rows are evicted, unnamed before named (a named row carries a
    # dedup contract; evicting it silently re-arms a side effect, so named go
    # last). In-flight rows are never evicted — the cap may overshoot while
    # work is live. None = unbounded (the operator's explicit choice).
    # Set at kernel start: `--ledger-cap N|inf` / FANTASTIC_LEDGER_CAP.
    ledger_cap: int | None = 1_000_000

    # Installed by the ROOT LOADER at boot (kernel_state) — the substrate never
    # touches disk itself. None = no persistence (no store wired, or a test
    # kernel): settled outcomes live and die with the process, exactly like the
    # tree's own auto-flush law. Called (sync, cheap) at every settle, with the
    # id of the execution that settled: the loader decides whether that row is
    # worth persisting for (its OWN persistence traffic is not).
    ledger_flusher: "Callable[[int | None], None] | None" = None

    # THE MESSAGE STORE — every message the kernel ever made, id-addressed by the
    # one counter, append-only: a row is never edited or removed by this API. An
    # agent's CONVERSATION (a message, a delivered reply, a fold mark, a recall
    # decision) and the kernel's own bookkeeping — a `call` row per batch item, a
    # `task` row per forked job — live in the same table; readers filter by `kind`.
    # A dehydrated reply BODY is a `body` row whose `parent` is the call's id; an
    # agent's VIEW (the ordered ids it sees, each at a resolution) is a `view` row,
    # the newest one being current. ONE TABLE, so one append-only stream carries
    # everything. Rows outlive `delete_agent`: agent id is conversation id, and a
    # re-created id continues where it left off — a clean slate is a new id.
    #
    # THE KERNEL NEVER KNOWS DISK. It starts empty, always. A hydrator (a loader
    # bundle, from above) fills it with `rows_put` and sets `next_id`; a persister
    # tails it with `rows_stream(after)`. Both are optional and interchangeable.
    rows: dict[int, dict] = field(default_factory=dict)
    _rows_by_agent: dict[str, list[int]] = field(default_factory=dict)
    _body_by_parent: dict[int, int] = field(default_factory=dict)  # pid → body row
    _view_by_agent: dict[str, int] = field(default_factory=dict)  # agent → view row
    _tails: list["asyncio.Queue"] = field(default_factory=list)

    # DRAINING: the first phase of a graceful stop (`shutdown_agents`). New roots
    # are refused `{reason:"draining"}`; sends with a live parent task, and the
    # kernel's own `shutdown` verbs, still pass.
    draining: bool = False

    def mint_id(self) -> int:
        """The next message id. The ONLY allocator in the kernel: a call, a forked
        task, a conversation row, a reply body all take their id here, so one
        number names one thing everywhere (`ps`, `cancel`, `messages`, a reply
        tag). Minting reserves the id; the row may be written later (`rows_put`
        accepts a pre-minted id) — an AI message names its own id in the reply tags
        it sends before its row exists."""
        n = self.next_id
        self.next_id = n + 1
        return n

    def rows_put(self, agent_id: str, row: dict) -> int:
        """Append one message for `agent_id`; mint its id when absent, accept a
        PRE-MINTED unused one, refuse anything else — the store is the only
        allocator. Bookkeeping from above (the same standing as `ledger_put`),
        never gated. Returns the id."""
        rid = row.get("id")
        if rid is None:
            rid = self.mint_id()
        elif (
            isinstance(rid, bool)
            or not isinstance(rid, int)
            or rid <= 0
            or rid >= self.next_id
            or rid in self.rows
        ):
            raise ValueError(
                f"rows_put: id must be absent or a pre-minted unused id, got {rid!r}"
            )
        stored = {**row, "id": rid, "agent": agent_id}
        stored.setdefault("ts", time.time())
        self.rows[rid] = stored
        self._rows_by_agent.setdefault(agent_id, []).append(rid)
        kind = stored.get("kind")
        if kind == "body" and isinstance(stored.get("parent"), int):
            self._body_by_parent[stored["parent"]] = rid
        elif kind == "view":
            self._view_by_agent[agent_id] = rid
        for q in self._tails:
            q.put_nowait(stored)
        return rid

    async def rows_stream(self, after: int):
        """THE TAIL. Every row with `id > after` in id order, then every row as it is
        appended — for a persister (the loader's pump) or any other consumer. Replay
        then live, deduped by id, so a subscriber that opens at the id it last saw
        misses nothing and doubles nothing. Yielding is not a message: no send, no
        task, no row. The queue holds references to rows already in RAM."""
        q: asyncio.Queue = asyncio.Queue()
        self._tails.append(q)  # registered BEFORE the snapshot: nothing slips between
        # IDS DO NOT ARRIVE IN ORDER. An AI message's id is minted when the message starts
        # and its row lands when the message ends, after calls minted in between — so
        # "newer than the last yielded" would drop it (measured 2026-09-08: every
        # conversation message missing from the log). Dedupe only against the replay.
        replayed = sorted(i for i in self.rows if i > after)
        try:
            for rid in replayed:
                row = self.rows.get(rid)
                if row is not None:
                    yield row
            seen = set(replayed)
            while True:
                row = await q.get()
                if row["id"] in seen or row["id"] <= after:
                    continue
                seen.add(row["id"])
                yield row
        finally:
            if q in self._tails:
                self._tails.remove(q)

    def rows_for(self, agent_id: str) -> list[dict]:
        """That agent's rows, in append order."""
        return [
            self.rows[i]
            for i in self._rows_by_agent.get(agent_id, ())
            if i in self.rows
        ]

    def rows_get(self, ids: list[int]) -> list[dict]:
        """Rows by id, any agent, in the order asked. The `messages` verb gates."""
        return [self.rows[i] for i in ids if i in self.rows]

    def body_put(self, agent_id: str, pid: int, text: str) -> int:
        """A reply body too large to inline: a `body` row under the conversation that
        received it, `parent` = the call's id. Returns the row id."""
        return self.rows_put(agent_id, {"kind": "body", "parent": pid, "content": text})

    def body_get(self, pid: int, offset: int, length: int):
        """One page of a stored body: (text, next_offset, eof, size), or None."""
        rid = self._body_by_parent.get(pid)
        row = self.rows.get(rid) if rid is not None else None
        if row is None:
            return None
        b = str(row.get("content", ""))
        chunk = b[offset : offset + max(1, length)]
        nxt = offset + len(chunk)
        return chunk, nxt, nxt >= len(b), len(b)

    def view_put(self, agent_id: str, doc: dict) -> int:
        """The agent's VIEW as a `view` row; the newest one is current."""
        return self.rows_put(agent_id, {**doc, "kind": "view"})

    def view_get(self, agent_id: str) -> dict | None:
        rid = self._view_by_agent.get(agent_id)
        row = self.rows.get(rid) if rid is not None else None
        if row is None:
            return None
        return {
            k: v for k, v in row.items() if k not in ("kind", "agent", "id", "parent")
        }

    def ledger_settled(self, exec_id: int | None) -> None:
        """One execution settled. The id goes through: the loader's own
        persistence writes settle too, and a flush kicked by them would write
        the file its own write just dirtied, forever."""
        if self.ledger_flusher is not None:
            self.ledger_flusher(exec_id)

    def resolve_name(self, name: str) -> int | None:
        """The message id a caller's name stands for, while its execution is held."""
        return self.names.get(name)

    def ledger_put(self, exec_id: int, row: dict) -> None:
        """Insert one execution's PROCESS state, then enforce the ring."""
        self.executions[exec_id] = row
        if row.get("name"):
            self.names[row["name"]] = exec_id
        cap = self.ledger_cap
        if cap is None or len(self.executions) <= cap:
            return
        # oldest-first (dict preserves insertion order); two passes: settled
        # unnamed, then settled named. Never the live ones.
        for named_pass in (False, True):
            if len(self.executions) <= cap:
                return
            for eid in list(self.executions):
                r = self.executions[eid]
                if r["named"] is not named_pass:
                    continue
                if not r["fut"].done():
                    continue
                del self.executions[eid]
                if r.get("name") and self.names.get(r["name"]) == eid:
                    del self.names[r["name"]]
                if len(self.executions) <= cap:
                    return

    def register_task(
        self,
        task_id: int,
        agent_id: str,
        verb: str | None,
        task: "asyncio.Task",
        *,
        parent: int | None,
        keepalive: bool = False,
        forked: bool = False,
    ) -> dict:
        """Put one in-flight asyncio.Task in the table. Self-cleaning: the
        entry is forgotten when the task ends (done-callback), cancelled or
        not, so the table only ever holds LIVE work.

        A PARENT IS ALWAYS ALIVE. There used to be a silent `parent = None`
        here for the case where `_current_task` named a task that had already
        ended, defended in a comment as init-adoption. It was the fingerprint
        of a bug, not a rule: an UNREGISTERED fork (a bare `create_task` in a
        bundle) kept using the scope of the dispatch that made it, so work
        registered later claimed a dead parent — and converting that to None
        put the work OUTSIDE every subtree, where `cancel` on its caller could
        never reach it.

        Measured over a full suite run: 97 hits before, 45 after `spawn_task`
        began minting each registered fork its own id, 0 once the bare forks
        (scheduler tick, bridge read loop, ws drain + call, exec drains, the
        persistence writer) were registered too. Nothing absorbs it now — if a
        dead parent ever reappears, `forget_task`'s re-parenting is what should
        have handled it, and the row is wrong."""
        entry = {
            "id": task_id,
            "agent": agent_id,
            "parent": parent,
            "verb": verb,
            "started_at": time.time(),
            "_task": task,
            # Set by `cancel_task` BEFORE the walk, so a dying handler cannot
            # register new work on its way out (see `_launch`).
            "_killed": False,
            # A persister loop that must outlive `shutdown_agents`' quiet phase
            # (the loader's message pump). Everything else is pending work.
            "_keepalive": keepalive,
            # A `spawn_task` FORK — a service loop (scheduler tick, bridge read
            # loop, ws drain, job pump, pty reader), not a dispatch. It is work
            # that outlives its dispatch BY CONSTRUCTION, so it never settles:
            # `shutdown_agents` cancels it instead of waiting for it.
            "_forked": forked,
        }
        self.tasks[task_id] = entry
        task.add_done_callback(lambda _t, tid=task_id: self.forget_task(tid))
        return entry

    def spawn_task(
        self,
        agent_id: str,
        verb: str,
        coro: "Coroutine[Any, Any, Any]",
        *,
        keepalive: bool = False,
    ) -> int:
        """Fork BACKGROUND work out of a dispatch (an OS subprocess pump, a PTY
        reader) — work that OUTLIVES the dispatch that started it. Bundles call
        this so their real work is visible to `ps` and killable by `cancel`;
        without it the dispatch returns, its entry drops, and the resource is
        unreachable (that is the orphan bug).

        THE KERNEL CREATES THE TASK, because only the kernel can mint the id
        the task must run under. It takes a COROUTINE, not an
        `asyncio.Task`: a caller that created the task itself had already
        started it under the DISPATCH's scope, so `_current_task` inside the
        fork named the forking dispatch — which settles moments later. Every
        send the fork then made was rowed with a parent that was already gone
        (measured 2026-09-11: a program's sends through the exec relay carried
        the settled `start` pid), and `register_task` had to paper over the
        same case by dropping a dead parent to None. The fork now runs under
        its OWN id.

        Sender and origin ride the copied context untouched — a fork acts as
        whoever forked it, which is what the gate must see.

        Parent = the dispatch that forked it, so the fork stays inside the
        caller's process group. A FORK IS A MESSAGE: the task's id is the id of
        a `task` row written here, so every id in the kernel names a row.
        Returns that id."""
        parent = _current_task.get()
        task_id = self.rows_put(
            agent_id, {"kind": "task", "verb": verb, "parent": parent}
        )
        ctx = contextvars.copy_context()
        ctx.run(_current_task.set, task_id)
        task = asyncio.create_task(coro, context=ctx)
        self.register_task(
            task_id,
            agent_id,
            verb,
            task,
            parent=parent,
            keepalive=keepalive,
            forked=True,
        )
        return task_id

    def is_registered(self, task) -> bool:
        """Whether this asyncio task IS a row of the table (a spawned loop, e.g. the
        loader's pump) — live work by definition, whatever its contextvar says."""
        return task is not None and any(e["_task"] is task for e in self.tasks.values())

    def forget_task(self, task_id: int) -> None:
        """Drop one entry. Surviving children are RE-PARENTED to the dead
        task's parent — init-adoption: a forked job outlives the dispatch
        that spawned it, and must stay in its caller's process group (else
        cancelling the caller could never reach it). No-op if absent."""
        entry = self.tasks.pop(task_id, None)
        if entry is None:
            return
        for e in self.tasks.values():
            if e["parent"] == task_id:
                e["parent"] = entry["parent"]

    def task_rows(self) -> list[dict]:
        """The public task table — the `ps` reply rows, oldest first. The
        asyncio handle stays behind."""
        return [
            {
                "id": e["id"],
                "agent": e["agent"],
                "parent": e["parent"],
                "verb": e["verb"],
                "started_at": e["started_at"],
            }
            for e in self.tasks.values()
        ]

    def task_subtree(self, task_id: int) -> list[int]:
        """`task_id` + every descendant, POST-ORDER (leaves first) — the kill
        order of a process group: children die before the parent that awaits
        them."""
        kids: dict[int, list[int]] = {}
        for e in self.tasks.values():
            if e["parent"] is not None:
                kids.setdefault(e["parent"], []).append(e["id"])
        out: list[int] = []

        def walk(tid: int) -> None:
            for k in kids.get(tid, []):
                walk(k)
            out.append(tid)

        walk(task_id)
        return out

    async def cancel_task(self, task_id: int) -> dict:
        """Kill a task AND ITS SUBTREE. Two-phase, mirroring SIGTERM→SIGKILL:

        ① `on_cancel` — each agent's bundle hook releases the REAL resources
           it owns for that task (an OS subprocess, a PTY, a socket).
           Cancelling an asyncio.Task frees NONE of those. Nothing is aborted
           in this phase, so no handler can wake mid-walk and observe a
           half-killed group.
        ② abort — the asyncio.Tasks are cancelled leaves-up, synchronously
           (no `await` in that loop): a parent can never resume between its
           child's death and its own.

        Returns {cancelled, killed:[task_id]}. An unknown or already-finished
        id is NOT in the table → {"cancelled": False, "killed": []}: cancel
        fails honestly, it never pretends."""
        if task_id not in self.tasks:
            return {"cancelled": False, "killed": []}
        ids = self.task_subtree(task_id)
        # MARK FIRST, kill second. `on_cancel` runs real teardown and can await,
        # so a handler in the doomed subtree gets scheduling passes between the
        # mark and the abort. Anything it sends in that window belongs to work
        # that is already dead: without the mark it started fresh dispatches —
        # new `ps` rows, new OS children — that outlived the cancel (measured
        # 2026-09-02).
        for tid in ids:
            entry = self.tasks.get(tid)
            if entry is not None:
                entry["_killed"] = True
        for tid in ids:  # ① SIGTERM
            entry = self.tasks.get(tid)
            if entry is None:
                continue
            agent = self.agents.get(entry["agent"])
            if agent is not None:
                await agent.on_cancel(tid)
        killed: list[int] = []
        for tid in ids:  # ② SIGKILL — no await in this loop
            entry = self.tasks.pop(tid, None)
            if entry is None:
                continue
            if entry["_task"].cancel():
                killed.append(tid)
        return {"cancelled": True, "killed": killed}

    # ─── tree management (front-door API) ──────────────────────

    async def send(self, items: list[dict]) -> list[dict]:
        """ARRAY-ONLY batch send from outside any handler:
        [{target, payload, timeout?}, ...] -> [{pid, status, result}].
        Delegates to the root Agent's `send` so all of its behavior — the
        `kernel` primer alias, the call gate, the readme reflect
        post-process — applies uniformly whether you call through `Kernel`
        or an `Agent`. Single call = `(await send([{...}]))[0]`."""
        if self.root is None:
            if not isinstance(items, list):
                raise TypeError(
                    "send takes a LIST of {target, payload, timeout?} items"
                )
            err = {"error": "kernel: no root agent registered"}
            return [outcome(None, FAILED, dict(err)) for _ in items]
        return await self.root.send(items)

    def create(
        self,
        handler_module: str,
        *,
        id: str | None = None,
        parent: "Agent | None" = None,
        **meta: Any,
    ) -> dict:
        """Add a new agent. `parent` defaults to root (top-level child)."""
        if parent is None:
            if self.root is None:
                return {"error": "kernel: no root agent registered"}
            parent = self.root
        return parent.create(handler_module, id=id, **meta)

    # How many ids an unknown-agent reply carries before it just reports the count.
    # Big enough for any hand-composed kernel, small enough not to bury the error.
    UNKNOWN_AGENT_IDS_MAX = 40

    def unknown_agent(self, agent_id: str, **extra) -> dict:
        """The reply for an id that names no agent — carrying the LIVE id index.

        Reading `self.agents` costs nothing: the flat index is already maintained, so
        this is a dict lookup, not a reflect. And a bare `no agent 'x'` teaches the
        caller nothing it can act on.

        Measured 2026-08-04: a model copied the literal placeholder `<id>` out of the
        prompt's own example, received `{"error": "no agent '<id>'"}` THIRTY-FOUR
        times across one run, and never once corrected. The error stated a fact and
        withheld every fact that would have repaired it. Same principle the ingress
        rules already follow — a denial names the legal values (`DenyInbound(unknown=…)`).

        This is NOT a repair and NOT a fallback: the call still fails, exactly once,
        by the one path. It just says what the legal ids are on the way out.
        """
        ids = sorted(self.agents)
        reply = refusal(
            UNKNOWN_AGENT,
            f"no agent {agent_id!r}",
            agent_count=len(ids),
            agents=ids[: self.UNKNOWN_AGENT_IDS_MAX],
        )
        # An id wrapped in angle brackets is structurally a SLOT, not a name — no id
        # this kernel mints can contain them (`{bundle}_{hex6}` or a bundle name), so
        # this costs no guessing.
        if agent_id.startswith("<") and agent_id.endswith(">"):
            reply["hint"] = (
                f"{agent_id!r} is a PLACEHOLDER from an example, not an id — "
                "substitute one of `agents`."
            )
        reply.update(extra)
        return reply

    async def delete(self, agent_id: str) -> dict:
        """Cascade-delete an agent + its subtree. Root can't be deleted."""
        agent = self.agents.get(agent_id)
        if agent is None:
            return self.unknown_agent(agent_id)
        if agent.parent is None:
            return {"error": f"cannot delete root agent {agent_id!r}"}
        return await agent.parent.delete(agent_id)

    SHUTDOWN_VERB_TIMEOUT = 30.0  # one bound for every agent's `shutdown` dispatch
    QUIET_TIMEOUT = 10.0  # how long in-flight work may take to settle before cancel

    async def shutdown_agents(self) -> dict:
        """The AGENT half of a graceful stop — also the `shutdown_agents` kernel verb.

        1. DRAIN: new roots are refused; sends with a live parent task still pass.
        2. `shutdown` VERB: every agent whose bundle advertises it is told, all at
           once, under `shutdown_context()`, bounded. For an AI it is a message ("the
           kernel is stopping; persist what matters, then done"); its tools work.
        3. QUIET: wait for the in-flight DISPATCHES to settle (persister loops
           marked `keepalive` and `spawn_task` service loops excepted), bounded;
           the rest is cancelled through the normal `cancel_task` path (subtree,
           `on_cancel` hooks).
        Idempotent: a second call answers the same shape and shuts nothing new.
        The kernel keeps serving afterwards — reads, `messages`, the persister —
        until `shutdown()` tears it down."""
        self.draining = True
        shut: list[str] = []
        targets = [a for a in list(self.agents.values()) if a.advertises("shutdown")]
        if targets and self.root is not None:
            with shutdown_context(), operator_context():
                outs = await self.root.send(
                    [
                        {
                            "target": a.id,
                            "payload": {"type": "shutdown"},
                            "timeout": self.SHUTDOWN_VERB_TIMEOUT,
                        }
                        for a in targets
                    ]
                )
            shut = [
                a.id for a, o in zip(targets, outs) if o.get("status") == "completed"
            ]
        # The dispatch running THIS verb is in the table too; it is not pending work.
        me = _current_task.get()

        def pending() -> list[int]:
            return [
                tid
                for tid, e in self.tasks.items()
                if not e.get("_keepalive") and tid != me
            ]

        def settling() -> list[int]:
            """The pending DISPATCHES — work that can finish on its own. A
            `spawn_task` FORK (scheduler tick, bridge read loop, ws drain, job
            pump, pty reader) is a service loop that never settles by
            construction, so waiting on one burned the WHOLE `QUIET_TIMEOUT` on
            every daemon that ran one: measured 2026-09-21, SIGTERM→exit was
            10.25 s with a scheduler on the tree and 0.29 s without it, and the
            e2e harness's 10 s stop window then SIGKILLed the kernel with its
            PID lock (`.fantastic/lock.json`) still on disk. Forks are cancelled
            below — that is what a stop does to them — never awaited."""
            return [tid for tid in pending() if not self.tasks[tid].get("_forked")]

        deadline = time.monotonic() + self.QUIET_TIMEOUT
        while time.monotonic() < deadline and settling():
            await asyncio.sleep(0.05)
        cancelled: list[int] = []
        for tid in pending():
            e = self.tasks.get(tid)
            if e is None or e.get("parent") is not None:
                continue  # roots only: the subtree dies with its root
            with operator_context():
                out = await self.cancel_task(tid)
            cancelled += out.get("killed", [])
        return {"drained": True, "shut": shut, "cancelled": cancelled}

    async def shutdown(self) -> None:
        """Graceful tree-wide process-shutdown: `shutdown_agents()` (drain, the
        `shutdown` verb, quiet), then the root bundle's duck-typed
        `flush_pending(agent)` — awaited BEFORE the teardown walk, so a persister
        can write its last window while its provider child is still alive — then
        the depth-first `on_shutdown`/`on_delete` walk that frees OS resources
        (PTYs, subprocesses, sockets) without touching records or disk. The tree
        survives so the next boot rehydrates cleanly. Idempotent: guarded by
        `_shutdown_complete`, so the SIGTERM handler and the atexit safety net
        don't double-run."""
        if self._shutdown_complete or self.root is None:
            return
        await self.shutdown_agents()
        hook = self.root.module_hook("flush_pending")
        if hook is not None:
            try:
                with shutdown_context():  # its writes are the kernel's own traffic
                    await hook(self.root)
            except Exception as e:  # noqa: BLE001 — teardown continues
                print(f"  [shutdown] flush_pending raised: {e}", file=sys.stderr)
        await self.root.shutdown()
        self._shutdown_complete = True

    def update(self, agent_id: str, **meta: Any) -> dict | None:
        """Patch an agent's meta + persist."""
        if self.root is None:
            return None
        return self.root.update(agent_id, **meta)

    def list(self) -> list[dict]:
        """Flat list of every agent's record."""
        return [a.record for a in self.agents.values()]

    def get(self, agent_id: str) -> dict | None:
        """Flat get — record dict or None."""
        agent = self.agents.get(agent_id)
        return agent.record if agent else None

    # ─── snapshot (medium-agnostic; LOADER agents own the medium) ──

    def save(self) -> dict:
        """Snapshot the live tree as a flat record list (mirror Rust
        `Kernel::save`). Ephemeral agents are skipped — per-process
        composition, never round-tripped. Sorted by id for deterministic
        output. Pure read, no I/O — a loader agent turns this into bytes."""
        # The ephemeral SUBTREE, not just the ephemeral agent. Filtering
        # per-agent emitted any child of an ephemeral with a `parent_id` naming
        # an agent that is not in the document — and `Kernel.load` then refuses
        # it: "agent 'x' parent 'cli' not in snapshot". A `save()` whose output
        # its own `load()` rejects is a checkpoint that cannot be restored, and
        # nothing reported it at write time.
        dropped = {a.id for a in self.agents.values() if type(a).ephemeral}
        while True:
            more = {
                a.id
                for a in self.agents.values()
                if a.id not in dropped
                and a.parent is not None
                and a.parent.id in dropped
            }
            if not more:
                break
            dropped |= more
        records = [a.record for a in self.agents.values() if a.id not in dropped]
        records.sort(key=lambda r: r["id"])
        return {"version": CURRENT_VERSION, "records": records}

    def load(
        self, snapshot: "list[dict] | dict", *, root_path: Path | None = None
    ) -> None:
        """Replace the tree with a flat record list (mirror Rust
        `Kernel::load`). Accepts a bare list or a {version, records}
        envelope. Validates (one root / unique ids / resolvable parents),
        drops the current tree, then DFS-rebuilds from the root.

        Weak-load: a record whose `handler_module` isn't importable in
        THIS runtime is logged + skipped along with its whole subtree,
        left untouched in the snapshot/on disk. Builds in-memory only
        (Agent never touches disk); fires no `boot` — the bootstrap
        does that after, which is when a loader subscribes + persists."""
        from kernel._agent import Agent  # local import — Agent imports this module

        if isinstance(snapshot, dict):
            version = snapshot.get("version", CURRENT_VERSION)
            records = snapshot.get("records", [])
        else:
            version, records = CURRENT_VERSION, snapshot
        validate_records(records, version=version)

        children: dict[str, list[dict]] = {}
        root_rec: dict | None = None
        for r in records:
            pid = r.get("parent_id")
            if pid is None:
                root_rec = r
            else:
                children.setdefault(pid, []).append(r)

        # The root record declares this tree's children-dir name (declared
        # config; default "agents"). Set BEFORE build, since each Agent derives
        # its address from `ctx.children_dir`.
        if isinstance(root_rec, dict):
            cd = root_rec.get("children_dir")
            self.children_dir = cd if isinstance(cd, str) and cd else "agents"

        # drop the current tree (inboxes close as their queues are dropped)
        self.agents.clear()
        self.inboxes.clear()
        self.root = None

        def build(rec: dict, parent: "Agent | None") -> None:
            hm = rec.get("handler_module")
            if hm:
                try:
                    importlib.import_module(hm)
                except Exception:
                    # canonical log shape — CI + selftests grep it verbatim.
                    sys.stderr.write(
                        f"[kernel] skipping agent {rec.get('id')}: "
                        f"bundle {hm} not installed in this runtime\n"
                    )
                    return  # weak-load: skip this record AND its subtree
            meta = {
                k: v
                for k, v in rec.items()
                if k not in ("id", "handler_module", "parent_id")
            }
            agent = Agent(
                id=rec["id"],
                root_path=root_path if parent is None else None,
                ctx=self,
                parent=parent,
                handler_module=hm,
                **meta,
            )
            for child in children.get(rec["id"], []):
                build(child, agent)

        if root_rec is not None:
            build(root_rec, None)

    # ─── state stream ──────────────────────────────────────────

    def publish_state(self, event: dict) -> None:
        """Synchronously dispatch one event to every subscriber. The
        tap is direct-callback — never routes through send/emit/
        inboxes. Subscribers may call agent.send/emit/create/delete
        from inside their callback; that produces normal traffic
        events (bounded; never feedback-loops because state events
        themselves don't re-publish)."""
        if not self.state_subscribers:
            return
        event = {**event, "ts": time.time()}
        # Snapshot so a subscriber that unsubscribes mid-iteration
        # doesn't shift indexes.
        for cb in tuple(self.state_subscribers):
            try:
                cb(event)
            except Exception as e:
                print(f"  [kernel] state subscriber raised: {e}", file=sys.stderr)

    def add_state_subscriber(
        self, callback: Callable[[dict], None]
    ) -> Callable[[], None]:
        """Register a synchronous tap. Returns an unsubscribe closure."""
        self.state_subscribers.append(callback)

        def unsubscribe() -> None:
            try:
                self.state_subscribers.remove(callback)
            except ValueError:
                pass

        return unsubscribe

    def state_snapshot(self) -> list[dict]:
        """Synchronous read of every loaded agent's identity + in-flight
        handler count. Used by new subscribers to bootstrap before the
        first event arrives. No queue puts, no fanout."""
        return [
            {
                "agent_id": a.id,
                "name": a.display_name or a.id,
                "backlog": a._in_flight,
            }
            for a in self.agents.values()
        ]

    # ─── routing helpers ───────────────────────────────────────

    def get_agent(self, agent_id: str) -> "Agent | None":
        """Flat global lookup. Returns the live Agent instance or None."""
        return self.agents.get(agent_id)

    def register(self, agent: "Agent") -> None:
        """Add agent to the flat global index. Idempotent on re-register.

        If the agent declares an `alias` meta, wire it into `well_known` so
        `send` (and the WS surface, which passes the raw target through) can
        address it by that stable name — like the built-in `kernel`→root
        alias. This is declared config the substrate makes functional, not
        autonomous behavior."""
        self.agents[agent.id] = agent
        alias = agent._meta.get("alias")
        if isinstance(alias, str) and alias:
            self.well_known[alias] = agent.id

    def unregister(self, agent_id: str) -> None:
        """Remove agent from the flat global index + drop its inbox + any
        alias pointing at it. No-op if absent."""
        self.agents.pop(agent_id, None)
        self.inboxes.pop(agent_id, None)
        for name in [n for n, t in self.well_known.items() if t == agent_id]:
            del self.well_known[name]

    def ensure_inbox(self, id: str) -> asyncio.Queue:
        """Register `id` as a READER and hand back its queue.

        This call IS the registration: a queue exists iff someone called this
        to get one, and every caller that does drains it (the webapp WS proxy
        for a synthetic browser client, exec_bridge for a relay proxy, a test
        asserting on traffic). Nothing else mints one — an agent does not get
        a queue merely by existing, and `watch` refuses a watcher that has not
        registered rather than inventing a consumer on its behalf.

        Idempotent, so a reader may call it defensively before every read."""
        q = self.inboxes.get(id)
        if q is None:
            from kernel._kernel import INBOX_BOUND  # avoid forward ref

            q = asyncio.Queue(maxsize=INBOX_BOUND)
            self.inboxes[id] = q
        return q
