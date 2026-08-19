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
    task id (`"<agent_id>:<n>"`, a pid). Tasks form a TREE through the
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
import importlib
import json
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import TYPE_CHECKING, Any, Callable

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
_current_task: contextvars.ContextVar[str | None] = contextvars.ContextVar(
    "_current_task", default=None
)


def current_task() -> str | None:
    """The TASK id of the dispatch running right now (`"<agent_id>:<n>"`), or
    None outside any dispatch.

    The PUBLIC, non-underscore read of `_current_task` — the twin of
    `sender_context`/`_current_sender`. A bundle that must know WHICH task it
    IS reads it here: an AI backend stamps it on its in-flight entry so its
    `on_cancel` hook can tell "the kernel is killing MY generation" from "the
    kernel is killing some other dispatch on the same agent" (a `history`, a
    `reflect`). Bundles never touch the contextvar itself."""
    return _current_task.get()


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
    #   task id  = "<agent_id>:<n>"   (a pid; n is a per-agent counter)
    #   entry    = {id, agent, parent, verb, started_at, _task: asyncio.Task}
    #
    # Tasks form a TREE through the send chain: a `send` issued INSIDE a
    # handler is a CHILD of that handler's task (the current task id rides
    # `_current_task`, exactly as the sender rides `_current_sender`). That
    # tree is what makes `cancel` a process-GROUP kill rather than a single
    # abort. `_task` is the abort handle (a real asyncio.Task) and never
    # leaves the kernel — `ps` serves the five public fields only.
    tasks: dict[str, dict] = field(default_factory=dict)

    # Per-agent task counter (the `n` in "<agent_id>:<n>"). Monotonic for
    # the process lifetime: an id is never reused, so a stale id cancels
    # nothing rather than killing an innocent task.
    task_seq: dict[str, int] = field(default_factory=dict)

    def mint_task_id(self, agent_id: str) -> str:
        """Next task id for `agent_id` — `"<agent_id>:<n>"`."""
        n = self.task_seq.get(agent_id, 0) + 1
        self.task_seq[agent_id] = n
        return f"{agent_id}:{n}"

    def register_task(
        self,
        task_id: str,
        agent_id: str,
        verb: str | None,
        task: "asyncio.Task",
        *,
        parent: str | None,
    ) -> dict:
        """Put one in-flight asyncio.Task in the table. Self-cleaning: the
        entry is forgotten when the task ends (done-callback), cancelled or
        not, so the table only ever holds LIVE work.

        A parent that is no longer in the table is dropped to None — the task
        is a root, adopted by init. This is not defensive slack: a bundle's
        long-lived loop (a bridge read loop, a job's connector relay) inherits
        the contextvar of the dispatch that created it, and that dispatch is
        long gone; a `send` it issues now must not claim a dead parent, or the
        `ps` tree would lie."""
        if parent is not None and parent not in self.tasks:
            parent = None
        entry = {
            "id": task_id,
            "agent": agent_id,
            "parent": parent,
            "verb": verb,
            "started_at": time.time(),
            "_task": task,
        }
        self.tasks[task_id] = entry
        task.add_done_callback(lambda _t, tid=task_id: self.forget_task(tid))
        return entry

    def spawn_task(self, agent_id: str, verb: str, task: "asyncio.Task") -> str:
        """Register BACKGROUND work a bundle forked out of a dispatch (an OS
        subprocess pump, a PTY reader) — work that OUTLIVES the dispatch that
        started it. Bundles call this so their real work is visible to `ps`
        and killable by `cancel`; without it the dispatch returns, its entry
        drops, and the resource is unreachable (that is the orphan bug).

        Parent = the dispatch that forked it, so the fork stays inside the
        caller's process group. Returns the new task id."""
        task_id = self.mint_task_id(agent_id)
        self.register_task(task_id, agent_id, verb, task, parent=_current_task.get())
        return task_id

    def forget_task(self, task_id: str) -> None:
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

    def task_subtree(self, task_id: str) -> list[str]:
        """`task_id` + every descendant, POST-ORDER (leaves first) — the kill
        order of a process group: children die before the parent that awaits
        them."""
        kids: dict[str, list[str]] = {}
        for e in self.tasks.values():
            if e["parent"] is not None:
                kids.setdefault(e["parent"], []).append(e["id"])
        out: list[str] = []

        def walk(tid: str) -> None:
            for k in kids.get(tid, []):
                walk(k)
            out.append(tid)

        walk(task_id)
        return out

    async def cancel_task(self, task_id: str) -> dict:
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
        for tid in ids:  # ① SIGTERM
            entry = self.tasks.get(tid)
            if entry is None:
                continue
            agent = self.agents.get(entry["agent"])
            if agent is not None:
                await agent.on_cancel(tid)
        killed: list[str] = []
        for tid in ids:  # ② SIGKILL — no await in this loop
            entry = self.tasks.pop(tid, None)
            if entry is None:
                continue
            if entry["_task"].cancel():
                killed.append(tid)
        return {"cancelled": True, "killed": killed}

    # ─── tree management (front-door API) ──────────────────────

    async def send(self, target_id: str, payload: dict) -> dict | None:
        """Flat global send from outside any handler. Delegates to the
        root Agent's `send` so all of `Agent.send`'s behavior — the
        `kernel` primer alias, the readme reflect post-process
        — applies uniformly whether you call through `Kernel` or an
        `Agent`."""
        if self.root is None:
            return {"error": "kernel: no root agent registered"}
        return await self.root.send(target_id, payload)

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
        reply = {"error": f"no agent {agent_id!r}", "agent_count": len(ids)}
        reply["agents"] = ids[: self.UNKNOWN_AGENT_IDS_MAX]
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

    async def shutdown(self) -> None:
        """Graceful tree-wide process-shutdown. Depth-first walks
        every agent's `on_shutdown`/`on_delete` hook to free OS
        resources (PTYs, subprocesses like `code serve-web`, open
        sockets) without touching records or disk — the tree
        survives so the next boot rehydrates cleanly. Idempotent:
        guarded by `_shutdown_complete`, so the SIGTERM signal
        handler and the atexit safety net don't double-run."""
        if self._shutdown_complete or self.root is None:
            return
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
        records = [a.record for a in self.agents.values() if not type(a).ephemeral]
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
        """Lazy-create an inbox queue for `id`. Used by agents at
        construction AND by the webapp WS proxy for synthetic browser
        client ids."""
        q = self.inboxes.get(id)
        if q is None:
            from kernel._kernel import INBOX_BOUND  # avoid forward ref

            q = asyncio.Queue(maxsize=INBOX_BOUND)
            self.inboxes[id] = q
        return q
