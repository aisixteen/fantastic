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

from kernel._refusal import MISSING_ARG, UNAUTHORIZED, UNHANDLED, UNKNOWN_AGENT, refusal
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


# THE QUALIFIED REMOTE CALLER. A call that crossed a bridge arrives carrying its
# sender's address `<remote kernel guid>|<remote agent id>` on the frame. The inbound
# leg dispatches it under `remote_caller_context(leg_id, address)`; the dispatch that
# `send` launches (origin == that leg) reads the address through
# `current_remote_caller()`. `Agent._dispatch_task` consumes the pair, so a send the
# handler itself makes does NOT inherit a caller it never had — its own writes are
# keyed under its own id.
_remote_caller: contextvars.ContextVar[tuple[str, str] | None] = contextvars.ContextVar(
    "_remote_caller", default=None
)
_current_remote: contextvars.ContextVar[str | None] = contextvars.ContextVar(
    "_current_remote", default=None
)


@contextlib.contextmanager
def remote_caller_context(leg_id: str, address: str):
    """Mark the enclosed sends as a bridge leg relaying `address`'s call."""
    token = _remote_caller.set((leg_id, address))
    try:
        yield
    finally:
        _remote_caller.reset(token)


def current_remote_caller() -> str | None:
    """The qualified `<guid>|<agent id>` of a caller on ANOTHER kernel, when this
    dispatch was relayed by a bridge leg that carried it; None otherwise."""
    return _current_remote.get()


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
# THE DEAD ENDINGS. An execution that settled one of these is a DEAD LETTER: it is
# announced (`on_deadletter`), listed by `ledger`, never retried silently, and
# retired by `ack_deadletter`.
DEAD_STATUSES = frozenset({"failed", "timed_out", "cancelled"})


def _store_safe(o: Any) -> Any:
    """A row a memory store can hold and a JSON wire can carry: raw bytes are reduced
    to `{len: N}` — the one reduction; everything else passes through."""
    if isinstance(o, (bytes, bytearray)):
        return {"len": len(o)}
    if isinstance(o, dict):
        return {k: _store_safe(v) for k, v in o.items()}
    if isinstance(o, (list, tuple)):
        return [_store_safe(v) for v in o]
    return o


def stored_outcome(out: dict, named: bool) -> dict:
    """WHAT A SETTLE ROW KEEPS of an outcome — the one definition (decided 2026-09-26).

    A NAMED call keeps its whole result: its name is a dedup contract, and a re-send
    returns the recorded outcome (C9). A DEAD LETTER keeps its error text: someone has
    to read why it died. Every other settle keeps a digest — `{status, bytes}`.

    Why not the body everywhere: the kernel's own store reads are executions too (the
    `ledger` projection, every AI agent's head read), so a settle holding its body
    re-recorded the store inside the store — measured ~6x growth per `ledger` call
    (2 KB → 14.8 MB in five). And a reply body is not persisted anyway (§3): its
    receiver holds it in RAM with the one-line `origin` that gets it again."""
    status = out.get("status")
    res = out.get("result")
    if named:
        return {"status": status, "result": res}
    if status in DEAD_STATUSES:
        kept: dict = {"status": status}
        if isinstance(res, dict):
            for k in ("error", "reason", "side", "cancelled", "timed_out"):
                if k in res:
                    v = res[k]
                    kept[k] = str(v)[:500] if isinstance(v, str) else v
        return kept
    try:
        size = len(json.dumps(_store_safe(res), default=str))
    except (TypeError, ValueError):
        size = None
    return {"status": status, "bytes": size}


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
# Aligned with Rust's `DEFAULT_INBOX_BOUND = 256` (kernel/kernel.rs); bounds
# per-agent memory under bursty output; drop-oldest keeps the freshest events.
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

    # THE ONE COUNTER. Every id in the kernel — a call (one per batch item), a
    # forked task, a settle, a conversation row an AI agent writes to its store —
    # is the next value of this int64. Kernel-wide and monotonic: `a < b` means "a
    # was created before b", across agents, and an id is never reused (a stale
    # `cancel` kills nothing).
    #
    # PERSISTED BY BLOCK RESERVATION, beside `guid` in the ROOT RECORD: `load`
    # reserves a block past the persisted bound, `start` writes that bound into the
    # root record (`next_id`), and a started kernel minting past it reserves and
    # writes the next block — the loader persists it like any record edit. A reboot starts at the persisted bound, so ids never
    # repeat; a crash skips at most one block (gaps are harmless — ids are opaque).
    # The guid and the counter persist TOGETHER: a root that is not durable mints
    # a new guid on its next boot, so every store key is new and ids restarting
    # from 1 cannot collide. A counter going BACKWARDS under the same guid (an old
    # agent.yaml restored) is the admin's to prevent.
    next_id: int = 1
    id_reserved: int = 1
    ID_BLOCK = 1000

    # THE EXECUTION LEDGER, process side — one entry per execution still in RAM:
    # in flight, or settled and not yet flushed. `send([{.., id:"job-42"}])` means
    # ENSURE that execution exists (the semantics `create_agent` gives a supplied
    # agent id): unknown name ⇒ dispatch and record; in flight ⇒ JOIN it (no second
    # run — C10); settled ⇒ the recorded outcome returns and no handler runs
    # (C9/C11). A settled execution that was flushed lives on as its `call` +
    # `settle` rows in the kernel's memory store; `send` looks a name up there
    # (`recall_executions`) before launching, so dedup survives both the flush and
    # a restart.
    # Entry: {"target", "fut": Future[outcome], "payload", "timeout",
    #         "retries", "attempts", "named", "name", "caller", "acked"?}.
    executions: dict[int, dict] = field(default_factory=dict)

    # Caller NAMES → message id, while the execution is in RAM.
    names: dict[str, int] = field(default_factory=dict)

    # RETENTION. In RAM a RING bounds `executions` (oldest SETTLED first, unnamed
    # before named, never in flight) — what matters for a kernel that was never
    # started and so never flushes. In the store, the same number is the prune
    # window. None = unbounded (the operator's explicit choice).
    # Set at kernel start: `--ledger-cap N|inf` / FANTASTIC_LEDGER_CAP.
    ledger_cap: int | None = 1_000_000

    # THE EXECUTION ROWS — `call`, `settle`, `task`, `ack_deadletter` rows the kernel minted
    # and has NOT YET FLUSHED to its memory store, id-addressed. The kernel keeps
    # NO conversation: an AI agent's rows live in that agent's memory store. Once a
    # row is flushed it leaves RAM (`flush_executions`); a kernel that was never
    # started keeps them here until the process ends.
    rows: dict[int, dict] = field(default_factory=dict)
    _tails: list["asyncio.Queue"] = field(default_factory=list)

    # THE MEMORY STORE the kernel writes its execution rows to — the root record's
    # `memory_store_id`, resolved by `start`. None ⇒ not started: rows stay in RAM.
    store_id: str | None = None
    started: bool = False
    _flush_wake: "asyncio.Event | None" = None
    _flush_lock: "asyncio.Lock | None" = None
    _flusher_task_id: int | None = None
    # The flush rows this kernel wrote (by id), and the calls that carried them:
    # those calls' own rows are ALREADY RECORDED — the flush row is their record.
    _own_flushes: set[int] = field(default_factory=set)
    _own_flush_calls: dict[int, int] = field(default_factory=dict)  # call id → flush id
    # How many `call` rows the store holds for the kernel (counted as flushed, re-read
    # at every prune) — what the retention window is measured against.
    _stored_calls: int = 0

    # DRAINING: the first phase of a graceful stop (`shutdown_agents`). New roots
    # are refused `{reason:"draining"}`; sends with a live parent task, and the
    # kernel's own `shutdown` verbs, still pass.
    draining: bool = False

    def mint_id(self) -> int:
        """The next message id. The ONLY allocator in the kernel: a call, a forked
        task, a conversation row, a checkpoint all take their id here, so one
        number names one thing everywhere (`ps`, `cancel`, a reply tag, a store
        row). Reserves a new block in the root record when the old one is spent."""
        n = self.next_id
        self.next_id = n + 1
        if n >= self.id_reserved:
            self.id_reserved = n + self.ID_BLOCK
            if self.started and self.root is not None:
                self.root.update(self.root.id, next_id=self.id_reserved)
        return n

    def rows_put(self, agent_id: str, row: dict) -> int:
        """Append one EXECUTION row (`agent` = the agent it is about); mint its id
        when absent, accept a PRE-MINTED unused one, refuse anything else — the
        kernel is the only allocator. Bookkeeping from above, never gated. Wakes the
        flusher. Returns the id."""
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
        for q in self._tails:
            q.put_nowait(stored)
        if self._flush_wake is not None:
            self._flush_wake.set()
        return rid

    async def rows_stream(self, after: int):
        """THE TAIL. Every row still in RAM with `id > after` in id order, then every
        row as it is appended. Replay then live, deduped by id. Yielding is not a
        message: no send, no task, no row."""
        q: asyncio.Queue = asyncio.Queue()
        self._tails.append(q)  # registered BEFORE the snapshot: nothing slips between
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
                self._forget_execution(eid)
                if len(self.executions) <= cap:
                    return

    def _forget_execution(self, eid: int) -> None:
        r = self.executions.pop(eid, None)
        if r is not None and r.get("name") and self.names.get(r["name"]) == eid:
            del self.names[r["name"]]

    # ─── the kernel's memory store: start, flush, read back ─────────

    FLUSH_DEBOUNCE = 0.15  # seconds of quiet before a batch is written

    async def start(self, *, boot_all: bool = True) -> dict:
        """THE START SEQUENCE — the `kernel_start` verb, and what the daemon runs.

        1. The root record must name `memory_store_id`. None ⇒ FAIL LOUD: the kernel
           keeps its execution rows (the ledger, dead letters, named-call dedup) in
           a memory store and does not start without one. No built-in default.
        2. Boot the root (it mints the guid every store key carries), then THAT
           store — before anyone who might call it.
        3. The store must serve the operator (the kernel writes as `kernel`) —
           probed now, refused now, never discovered on the first flush.
        4. Start flushing: every execution row minted so far and from now on is
           written into the store and leaves RAM.
        5. `boot_all`: boot every other agent in registration order (AI agents boot
           lazily — they read their own store on their first turn).
        Idempotent: a second call reports the store it is already writing to."""
        if self.started:
            return {"started": True, "already": True, "store": self.store_id}
        root = self.root
        if root is None:
            return {"error": "kernel: no root agent registered"}
        sid = root.record.get("memory_store_id")
        if not isinstance(sid, str) or not sid:
            return refusal(
                MISSING_ARG,
                "kernel_start: the root record names no `memory_store_id` — the kernel "
                "keeps its execution rows (ledger, dead letters, named-call dedup) in a "
                "memory store and will not start without one",
                hint=(
                    "a memory store is needed — create a ram_store and name it in "
                    f"memory_store_id if you have none: `create_agent parent_id={root.id} "
                    "handler_module=ram_store.tools id=memory operator=true`, then "
                    f"`update_agent id={root.id} memory_store_id=memory`"
                ),
                arg="memory_store_id",
            )
        store = self.agents.get(sid)
        if store is None:
            return self.unknown_agent(
                sid, hint="the root's `memory_store_id` names no agent in this tree"
            )
        booted: list[str] = []
        with operator_context():
            for a in (root, store):
                out = (
                    await self.send([{"target": a.id, "payload": {"type": "boot"}}])
                )[0]
                if out["status"] != COMPLETED:
                    return refusal(
                        UNHANDLED,
                        f"kernel_start: boot of {a.id!r} did not complete "
                        f"({out['status']}): {out['result']}",
                    )
                booted.append(a.id)
            probe = (
                await self.send(
                    [{"target": store.id, "payload": {"type": "read", "limit": 1}}]
                )
            )[0]["result"]
        if isinstance(probe, dict) and probe.get("error"):
            return refusal(
                UNAUTHORIZED,
                f"kernel_start: the memory store {store.id!r} refuses the kernel: "
                f"{probe.get('error')}",
                hint=probe.get("hint")
                or f"the store must serve the operator: update_agent id={store.id} operator=true",
            )
        # The reserved id bound goes to the loader now (the root is booted, so its
        # record writer is listening): a crash from here on skips ids, never reuses.
        root.update(root.id, next_id=self.id_reserved)
        self.store_id = store.id
        self._flush_wake = asyncio.Event()
        self._flush_lock = asyncio.Lock()
        self.started = True
        self._flusher_task_id = self.spawn_task(
            root.id, "flush_executions", self._flusher(), keepalive=True
        )
        self._flush_wake.set()
        if boot_all:
            for a in list(self.agents.values()):
                if a.id in (root.id, store.id):
                    continue
                with operator_context():
                    out = (
                        await self.send([{"target": a.id, "payload": {"type": "boot"}}])
                    )[0]
                if out["status"] == COMPLETED and not (
                    isinstance(out["result"], dict) and out["result"].get("error")
                ):
                    booted.append(a.id)
                else:
                    print(
                        f"  [kernel] boot {a.id!r}: {out['status']} {out['result']}",
                        file=sys.stderr,
                    )
        return {"started": True, "store": store.id, "booted": booted}

    async def _flusher(self) -> None:
        """The flush loop: wake on a new row, let a burst settle, write a batch."""
        assert self._flush_wake is not None
        while True:
            await self._flush_wake.wait()
            await asyncio.sleep(self.FLUSH_DEBOUNCE)
            self._flush_wake.clear()
            await self.flush_executions()

    def _is_own_flush_row(self, row: dict) -> bool:
        """A row the kernel's own flush produced — ALREADY RECORDED by its flush row.

        A flush is a `send` to the store, so it mints a `call` row and a `settle`
        row like every send; writing those would mint two more on the next flush,
        forever. The flush therefore describes ITSELF: it mints one id and appends a
        `flush` row inside the very batch it writes. The call that carried that batch
        names the flush row in its payload — the store already holds the record of
        this execution — so its call and settle rows are not written again. This is
        dedup by execution (the store proves it), not a blind spot: every OTHER call
        to the store, the kernel's own reads included, is recorded like any call."""
        if row.get("kind") == "call" and row.get("agent") == self.store_id:
            payload = row.get("payload") or {}
            if payload.get("type") == "append":
                for r in payload.get("rows") or ():
                    if (
                        isinstance(r, dict)
                        and r.get("kind") == "flush"
                        and r.get("id") in self._own_flushes
                    ):
                        self._own_flush_calls[row["id"]] = r["id"]
                        return True
        if row.get("kind") == "settle" and row.get("parent") in self._own_flush_calls:
            return True
        return False

    async def flush_executions(self) -> dict:
        """Write every execution row still in RAM to the memory store, as ONE batch
        closed by a self-describing `flush` row; on success the rows leave RAM and a
        settled execution whose settle row landed leaves `executions` (the store is
        its record now). All or nothing: a refused batch stays in RAM for the next
        flush, and the refused flush is itself a dead letter.

        EDGE (recorded, not resolved): the write can LAND while its reply is lost
        (a timeout). The store then holds the flush row as `done` while RAM saw the
        flush call fail — that call is a dead letter in RAM (the ledger shows it,
        `on_deadletter` announced it), and its own rows are not re-flushed because
        the store already recorded them. RAM and store disagree about THAT flush
        only; the admin reconciles."""
        if self.store_id is None or self._flush_lock is None:
            return {"flushed": 0}
        async with self._flush_lock:
            own = [rid for rid, r in self.rows.items() if self._is_own_flush_row(r)]
            for rid in own:
                r = self.rows.pop(rid)
                if r.get("kind") == "settle":
                    # The flush call ended: its bookkeeping goes. A flush that did
                    # not complete stays in `executions` — a dead letter in RAM.
                    fid = self._own_flush_calls.pop(r.get("parent"), None)
                    self._own_flushes.discard(fid)
                    status = (r.get("outcome") or {}).get("status")
                    if status == COMPLETED:
                        self._forget_execution(r["parent"])
            ids = sorted(self.rows)
            if not ids:
                return {"flushed": 0}
            fid = self.mint_id()
            self._own_flushes.add(fid)
            batch_kinds = {i: self.rows[i].get("kind") for i in ids}
            batch = [_store_safe(self.rows[i]) for i in ids]
            batch.append(
                {
                    "id": fid,
                    "kind": "flush",
                    "covers": ids,
                    "status": "done",
                    "ts": time.time(),
                }
            )
            with operator_context():
                out = (
                    await self.send(
                        [
                            {
                                "target": self.store_id,
                                "payload": {"type": "append", "rows": batch},
                            }
                        ]
                    )
                )[0]
            res = out["result"]
            if out["status"] != COMPLETED or (
                isinstance(res, dict) and res.get("error")
            ):
                print(
                    f"  [kernel] flush of {len(ids)} execution row(s) to "
                    f"{self.store_id!r} failed: {out['status']} {res}",
                    file=sys.stderr,
                )
                return {"flushed": 0, "error": res}
            for i in ids:
                r = self.rows.pop(i, None)
                if r is not None and r.get("kind") == "settle":
                    exe = self.executions.get(r.get("parent"))
                    if exe is not None and exe["fut"].done():
                        self._forget_execution(r["parent"])
            self._stored_calls += sum(1 for i in ids if (batch_kinds.get(i) == "call"))
        pruned = 0
        if self.ledger_cap is not None and self._stored_calls > self.ledger_cap:
            pruned = await self.prune_executions()
        return {"flushed": len(ids), "flush": fid, "pruned": pruned}

    async def prune_executions(self) -> int:
        """THE STORE-SIDE RETENTION WINDOW (`ledger_cap`). Over the cap, the OLDEST
        settled executions leave the kernel's store — unnamed first, then named (a
        named one carries a dedup contract, so it goes last) — each with its settle and
        ack rows, and the task/flush rows that only covered what was pruned. A DEAD
        LETTER that nobody acked stays: it is waiting for a person. In-flight work is
        never pruned. Returns how many executions went."""
        rows = await self._store_read({})
        calls = [r for r in rows if r.get("kind") == "call" and not r.get("internal")]
        settles = {r.get("parent"): r for r in rows if r.get("kind") == "settle"}
        acked = {r.get("parent") for r in rows if r.get("kind") == "ack"}
        self._stored_calls = len(calls)
        cap = self.ledger_cap or 0
        if len(calls) <= cap:
            return 0
        # HYSTERESIS: down to 90% of the window, so a full store is not re-read and
        # re-pruned on every flush once it sits at the cap.
        over = len(calls) - (cap * 9 // 10 if cap >= 10 else cap)

        def prunable(c: dict) -> bool:
            st = settles.get(c["id"])
            if st is None or c["id"] in self.executions:
                return False  # in flight (or still live in RAM)
            status = (st.get("outcome") or {}).get("status")
            return status not in DEAD_STATUSES or c["id"] in acked

        doomed_calls: list[int] = []
        for named_pass in (False, True):
            for c in sorted(calls, key=lambda r: r["id"]):
                if len(doomed_calls) >= over:
                    break
                if bool(c.get("named")) is named_pass and prunable(c):
                    doomed_calls.append(c["id"])
        if not doomed_calls:
            return 0
        gone = set(doomed_calls)
        doomed = set(gone)
        for r in rows:
            if r.get("kind") in ("settle", "ack") and r.get("parent") in gone:
                doomed.add(r["id"])
        kept_calls = [c["id"] for c in calls if c["id"] not in gone]
        floor = min(kept_calls) if kept_calls else None
        for r in rows:
            if r.get("kind") == "flush" and set(r.get("covers") or ()) <= doomed | {
                x for x in (r.get("covers") or ()) if floor is not None and x < floor
            }:
                doomed.add(r["id"])
            elif r.get("kind") == "task" and floor is not None and r["id"] < floor:
                doomed.add(r["id"])
        with operator_context():
            out = (
                await self.send(
                    [
                        {
                            "target": self.store_id,
                            "payload": {"type": "prune", "ids": sorted(doomed)},
                        }
                    ]
                )
            )[0]["result"]
        if isinstance(out, dict) and out.get("error"):
            print(
                f"  [kernel] prune of the kernel's store failed: {out}", file=sys.stderr
            )
            return 0
        self._stored_calls -= len(gone)
        return len(gone)

    async def _store_read(self, payload: dict) -> list[dict]:
        """Every row of the kernel's own key matching `payload`, all pages."""
        out: list[dict] = []
        after = None
        while True:
            q = {"type": "read", **payload, "limit": 1000}
            if after is not None:
                q["after"] = after
            with operator_context():
                res = (await self.send([{"target": self.store_id, "payload": q}]))[0][
                    "result"
                ]
            if not isinstance(res, dict) or res.get("error"):
                raise RuntimeError(f"kernel: memory store read failed: {res}")
            out.extend(res.get("rows") or [])
            after = res.get("next")
            if after is None:
                return out

    async def ledger_entries(self) -> list[dict]:
        """The LEDGER PROJECTION: flushed `call`/`settle`/`ack_deadletter` rows from the store,
        overlaid by what is still in RAM (live executions, unflushed rows). Each
        entry carries its originator as `_caller` for the verb to scope by."""
        entries: dict[int, dict] = {}
        acked: set[int] = set()
        rows: list[dict] = []
        if self.store_id is not None:
            rows = await self._store_read({"kinds": ["call", "settle", "ack"]})
        rows = rows + [
            r for r in self.rows.values() if r.get("kind") in ("call", "settle", "ack")
        ]
        settles: dict[int, dict] = {}
        for r in rows:
            k = r.get("kind")
            if k == "ack":
                acked.add(r.get("parent"))
            elif k == "settle":
                settles[r.get("parent")] = r
        for r in rows:
            if r.get("kind") != "call" or r.get("internal"):
                continue
            eid = r["id"]
            e = {
                "id": eid,
                "target": r.get("agent"),
                "attempts": r.get("attempts", 1),
                "retries": r.get("retries", 0),
                "named": bool(r.get("named")),
                "_caller": r.get("caller"),
            }
            if r.get("name"):
                e["name"] = r["name"]
            st = settles.get(eid)
            if st is not None:
                oc = st.get("outcome") or {}
                e["status"] = oc.get("status")
                e["attempts"] = st.get("attempts") or e["attempts"]
                e["outcome"] = {"pid": eid, **oc, "attempts": e["attempts"]}
            else:
                e["status"] = "in_flight"
            entries[eid] = e
        from kernel._agent import _json_safe

        for eid, x in self.executions.items():
            e = {
                "id": eid,
                "target": x["target"],
                "attempts": x["attempts"],
                "retries": x["retries"],
                "named": x["named"],
                "_caller": x.get("caller"),
            }
            if x.get("name"):
                e["name"] = x["name"]
            if x["fut"].done():
                res = x["fut"].result()
                e["status"] = res["status"]
                # THE SAME SHAPE flushed or not: what the settle row keeps.
                e["outcome"] = {
                    "pid": eid,
                    **_json_safe(stored_outcome(res, bool(x["named"]))),
                    "attempts": res.get("attempts", x["attempts"]),
                }
            else:
                e["status"] = "in_flight"
            if x.get("acked"):
                acked.add(eid)
            entries[eid] = e
        for eid in acked:
            if eid in entries:
                entries[eid]["acked"] = True
        return [entries[i] for i in sorted(entries)]

    async def recall_executions(self, items: list) -> None:
        """Bring a flushed execution back into RAM before `send` launches an item that
        names it — a caller NAME not held in RAM, or an int id not in `executions`.
        A settled one comes back as a DONE future holding its recorded outcome (dedup
        survives the flush and a restart); one with no settle row was in flight when
        the process stopped and rebuilds nothing — re-sending runs it."""
        wanted: list[tuple[str, object]] = []
        for it in items:
            if not isinstance(it, dict):
                continue
            x = it.get("id")
            if isinstance(x, str) and x and x not in self.names:
                wanted.append(("name", x))
            elif (
                isinstance(x, int)
                and not isinstance(x, bool)
                and x not in self.executions
            ):
                wanted.append(("id", x))
        if not wanted:
            return
        loop = asyncio.get_event_loop()
        for kind, key in wanted:
            match = {"name": key} if kind == "name" else None
            calls = await self._store_read(
                {"kinds": ["call"], **({"match": match} if match else {"ids": [key]})}
            )
            if not calls:
                continue
            call = calls[-1]
            settles = await self._store_read(
                {"kinds": ["settle"], "match": {"parent": call["id"]}}
            )
            if not settles:
                continue
            st = settles[-1]
            oc = st.get("outcome") or {}
            attempts = st.get("attempts") or call.get("attempts") or 1
            fut = loop.create_future()
            # A NAMED call's settle kept its result; an unnamed one (joined by its
            # int id) kept only what `stored_outcome` keeps — and says so.
            result = (
                oc["result"]
                if "result" in oc
                else {k: v for k, v in oc.items() if k != "status"}
            )
            fut.set_result(
                {
                    "pid": call["id"],
                    "status": oc.get("status"),
                    "result": result,
                    "attempts": attempts,
                }
            )
            self.ledger_put(
                call["id"],
                {
                    "target": call.get("agent"),
                    "fut": fut,
                    "payload": call.get("payload"),
                    "timeout": None,
                    "retries": call.get("retries", 0),
                    "attempts": attempts,
                    "named": bool(call.get("named")),
                    "name": call.get("name"),
                    "caller": call.get("caller"),
                },
            )

    def announce_deadletter(self, pid: int, out: dict, attempts) -> None:
        """`on_deadletter` — an execution settled failed / timed_out / cancelled.
        Emitted on the ORIGINATING caller's own inbox (and so to whoever watches it,
        a UI included), or on the root for an operator send. Nothing retries it; the
        event is how a person learns there is something to decide."""
        exe = self.executions.get(pid) or {}
        caller = exe.get("caller")
        entry = {
            "type": "on_deadletter",
            "id": pid,
            "target": exe.get("target"),
            "caller": caller,
            "status": out.get("status"),
            "attempts": attempts,
        }
        if exe.get("name"):
            entry["name"] = exe["name"]
        src = self.agents.get(caller) if caller else self.root
        if src is None:
            src = self.root
        if src is not None:
            src._fanout(entry, kind="emit")

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
        reserved `kernel` address, the call gate, the readme reflect
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
    SETTLE_TIMEOUT = 2.0  # how long a cancelled batch may take to record its ending

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
            PID lock (`.fantastic/lock.yaml`) still on disk. Forks are cancelled
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
        # EVERY CALL SETTLES before this returns. A killed dispatch's batch records
        # its `cancelled` outcome (and its settle row) a few loop turns after the
        # abort; the stop that follows flushes the last rows, so it must not start
        # before those endings exist. Bounded: a batch that never settles is not
        # waited on forever.
        deadline = time.monotonic() + self.SETTLE_TIMEOUT
        while time.monotonic() < deadline and any(
            not x["fut"].done() and eid not in self.tasks
            for eid, x in self.executions.items()
        ):
            await asyncio.sleep(0.01)
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
        if self.started:
            # THE LAST EXECUTION ROWS, while the store is still alive: everything
            # settled by the drain (done, or cancelled into the dead letters) lands.
            try:
                with shutdown_context():
                    await self.flush_executions()
            except Exception as e:  # noqa: BLE001 — teardown continues
                print(f"  [shutdown] final flush raised: {e}", file=sys.stderr)
            entry = self.tasks.get(self._flusher_task_id or -1)
            if entry is not None:
                entry["_task"].cancel()
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
            # THE COUNTER COMES BACK WITH THE GUID: the root record's reserved bound
            # is where this run starts, and this run's first block is reserved in
            # memory. `load` never edits a record (save → load → save is identical);
            # `start` writes the bound, and a started kernel persists every later block.
            nid = root_rec.get("next_id")
            if isinstance(nid, int) and not isinstance(nid, bool) and nid > 0:
                self.next_id = max(self.next_id, nid)
            self.id_reserved = self.next_id + self.ID_BLOCK

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

    @property
    def workdir(self) -> Path:
        """The kernel's RUNNING DIR — the dir holding its root's `.fantastic`. The
        ONE source of the disk clamp base (`file_bridge.fs.Root`); no root ⇒ none."""
        if self.root is None:
            raise RuntimeError("kernel has no root — no running dir to clamp disk to")
        return self.root._root_path.resolve().parent

    # ─── routing helpers ───────────────────────────────────────

    def get_agent(self, agent_id: str) -> "Agent | None":
        """Flat global lookup. Returns the live Agent instance or None."""
        return self.agents.get(agent_id)

    def register(self, agent: "Agent") -> None:
        """Add agent to the flat global index. Idempotent on re-register."""
        self.agents[agent.id] = agent

    def unregister(self, agent_id: str) -> None:
        """Remove agent from the flat global index + drop its inbox. No-op if
        absent."""
        self.agents.pop(agent_id, None)
        self.inboxes.pop(agent_id, None)

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
