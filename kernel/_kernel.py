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

`_current_sender` is a process-wide contextvar (not part of Kernel)
so that nested send/emit calls inside a handler attribute back to
the dispatching agent. ContextVars are task-local in asyncio, so
concurrent handlers don't trample each other.
"""

from __future__ import annotations

import asyncio
import contextvars
import json
import sys
import time
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any, Callable

if TYPE_CHECKING:
    from kernel._agent import Agent


# Contextvar set during a handler's dispatch so that nested send/emit
# calls — which fire FROM INSIDE the handler — know who's calling.
# Surfaces in state events as `sender`. None when send/emit is invoked
# from outside any handler (the WS proxy, `fantastic call`, REPL).
_current_sender: contextvars.ContextVar[str | None] = contextvars.ContextVar(
    "_current_sender", default=None
)


_SUMMARY_MAX_LEN = 160


def _summarize_payload(payload: Any, max_len: int = _SUMMARY_MAX_LEN) -> str:
    """Compact one-line view of a payload for telemetry overlays.

    Bytes values become `<bytes:N>` so JSON serialization doesn't
    explode on binary protocol payloads (audio/image frames). Result
    is JSON-stringified and trimmed to `max_len` chars with an
    ellipsis. Never raises; falls back to repr.
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
    if len(s) > max_len:
        s = s[: max_len - 1] + "…"
    return s


# Substrate constants — filesystem + entry-point conventions every
# Agent shares.
INBOX_BOUND = 500
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

    # The tree root — set when the first parent-less, non-ephemeral
    # Agent registers (typically `Core(self)` in main.py). All
    # `Kernel.create/list/...` delegations route through it.
    root: "Agent | None" = field(default=None)

    # ─── tree management (front-door API) ──────────────────────

    async def send(self, target_id: str, payload: dict) -> dict | None:
        """Flat global send from outside any handler. Delegates to the
        root Agent's `send` so all of `Agent.send`'s behavior — the
        `kernel` primer alias, the `return_readme` reflect post-process
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

    async def delete(self, agent_id: str) -> dict:
        """Cascade-delete an agent + its subtree. Root can't be deleted."""
        agent = self.agents.get(agent_id)
        if agent is None:
            return {"error": f"no agent {agent_id!r}"}
        if agent.parent is None:
            return {"error": f"cannot delete root agent {agent_id!r}"}
        return await agent.parent.delete(agent_id)

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
        """Add agent to the flat global index. Idempotent on re-register."""
        self.agents[agent.id] = agent

    def unregister(self, agent_id: str) -> None:
        """Remove agent from the flat global index + drop its inbox.
        No-op if absent."""
        self.agents.pop(agent_id, None)
        self.inboxes.pop(agent_id, None)

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
