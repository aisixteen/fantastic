"""The Agent class — recursive node in the kernel tree.

Every entity in the system is an Agent. Agents have:
  - An in-memory `record` (id + handler_module + parent_id + meta). A
    loader agent persists it to `<root_path>/agent.json` — Agent itself
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
import importlib
import secrets
import sys
from importlib.metadata import entry_points
from pathlib import Path
from typing import Any, Callable

from kernel._kernel import (
    BUNDLE_ENTRY_GROUP,
    Kernel,
    _current_sender,
    _summarize_payload,
)


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
    }
)


class Agent:
    """One node in the kernel tree. Recursive: any agent may have
    children, regardless of bundle.

    Public attributes — read freely; mutate only through the methods.

    Class-level `ephemeral`: when True, a loader skips this agent
    entirely — `save()` omits it and no agent.json is ever written.
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
        (file / yaml_state / readme) compute their paths under it, and
        the loader maps `<root_path>/agent.json` ←→ this record. Children
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
        self.ctx = ctx
        self.ctx.register(self)
        # Inbox lives on ctx — same dict that synthetic browser clients
        # share, so the webapp proxy's _ensure_inbox(client_id) and
        # routing fanout both look up here.
        self._inbox: asyncio.Queue = self.ctx.ensure_inbox(self.id)
        # No disk I/O here — the agent is pure in-memory. A loader agent
        # subscribes to the `added` event below and persists this record;
        # `Kernel.load()` rehydrates from records the loader read back.
        # Wire into parent's children dict + publish lifecycle event.
        # `parent.create(...)` is the indirect path; this one supports
        # direct construction (`Cli(kernel, parent=fs_loader)`).
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

    def _read_readme(self) -> str | None:
        """The agent's own `readme.md` content, or None if it has none.
        Read-only — the loader seeds the file (copy-if-missing) when it
        persists the record; Agent just reads it back for `reflect`."""
        p = self._root_path / "readme.md"
        if p.exists():
            try:
                return p.read_text(encoding="utf-8")
            except OSError:
                return None
        return None

    # ─── record ────────────────────────────────────────────────

    @property
    def record(self) -> dict:
        """Persistent fields written to agent.json — id +
        handler_module + parent_id + meta."""
        rec: dict[str, Any] = {"id": self.id}
        if self.handler_module is not None:
            rec["handler_module"] = self.handler_module
        if self.parent is not None:
            rec["parent_id"] = self.parent.id
        rec.update(self._meta)
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

    async def send(self, target_id: str, payload: dict) -> dict | None:
        """Flat global send. Resolves `target_id` in `ctx.agents` and
        dispatches its handler. Sets `_current_sender` contextvar
        around the dispatch so nested send/emit attribute correctly.
        Verbs in `_SYSTEM_VERBS` are answered natively by the target
        Agent without consulting handler_module.
        """
        if target_id == "kernel":
            target = self._root()
            # `kernel` is an alias for the tree root. reflect is a pure
            # read — answer it from the root's identity without a full
            # dispatch (no telemetry fanout); flags applied below.
            if payload.get("type") == "reflect":
                return self._apply_reflect_flags(
                    target, payload, target._reflect_identity()
                )
        else:
            # Resolve a well-known alias (declared via an `alias` meta, e.g.
            # `web_loader`) to its agent id; a literal id resolves to itself.
            target = self.ctx.agents.get(self.ctx.well_known.get(target_id, target_id))
        if target is None:
            return {"error": f"no agent {target_id!r}"}
        reply = await target._dispatch(payload)
        return self._apply_reflect_flags(target, payload, reply)

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

    @staticmethod
    def _apply_reflect_flags(target, payload, reply):
        """Compose any reflect reply with the universal flags — applied
        uniformly to bare-agent and bundle reflects alike:

        - `tree=all|ids|none` (default all): `all` nests the distilled
          subtree; `ids` is a flat descendant-id index; `none` omits it.
        - `bundles=all|ids|none` (default none): `all` is the
          {name, handler_module} catalog; `ids` is bare names; `none`
          omits it.
        - `readme=true` (legacy `return_readme` also honored): attach the
          agent's readme.md (string or null). Atomic — one agent.

        Transport/wire docs are NOT here — they live in the root readme
        (`reflect readme=true`)."""
        if payload.get("type") != "reflect" or not isinstance(reply, dict):
            return reply
        # `description` is a substrate meta field — surface it on EVERY
        # reflect (bundle handlers don't know about it), unless the
        # reply already set one.
        if target.description is not None and "description" not in reply:
            reply["description"] = target.description
        tree = payload.get("tree", "all")
        if tree == "all":
            reply["tree"] = target._tree(depth=None, details=False, current_depth=0)
        elif tree == "ids":
            reply["tree"] = target._descendant_ids()
        bundles = payload.get("bundles", "none")
        if bundles == "all":
            reply["bundles"] = target._available_bundles()
        elif bundles == "ids":
            reply["bundles"] = [b["name"] for b in target._available_bundles()]
        if payload.get("readme") or payload.get("return_readme"):
            reply["readme"] = target._read_readme()
        return reply

    def _descendant_ids(self) -> list[str]:
        """Flat id index of self + all descendants (DFS, self first)."""
        out = [self.id]
        for c in self._children.values():
            out.extend(c._descendant_ids())
        return out

    def child_ids(self) -> list[str]:
        """Public: ids of this agent's DIRECT children (insertion order). Lets a
        bundle enumerate children without reaching into the private `_children`."""
        return list(self._children.keys())

    @staticmethod
    def _available_bundles() -> list[dict]:
        """Entry-point-discovered installable bundles, sorted by name.
        Recomputed live, so a freshly installed bundle shows up on the
        next reflect."""
        return sorted(
            (
                {"name": ep.name, "handler_module": ep.value}
                for ep in entry_points(group=BUNDLE_ENTRY_GROUP)
            ),
            key=lambda b: b["name"],
        )

    async def emit(self, target_id: str, payload: dict) -> None:
        """Drop a payload into target_id's inbox + tell watchers.
        Non-routing — does NOT invoke any handler. Telemetry sees an
        `emit` event."""
        target = self.ctx.agents.get(target_id)
        if target is None:
            return
        target._fanout(payload, kind="emit")

    def watch(self, src_id: str, tgt_id: str) -> None:
        """Mirror src_id's incoming traffic into tgt_id's inbox.
        tgt_id may be an agent or a synthetic browser-client id."""
        src = self.ctx.agents.get(src_id)
        if src is not None:
            src._watcher_ids.add(tgt_id)
            # Make sure tgt has an inbox (synthetic clients need to
            # be addressable; we lazy-create here so callers don't
            # have to ensure_inbox first).
            self.ctx.ensure_inbox(tgt_id)

    def unwatch(self, src_id: str, tgt_id: str) -> None:
        src = self.ctx.agents.get(src_id)
        if src is not None:
            src._watcher_ids.discard(tgt_id)

    # ─── webapp-proxy compat shims ─────────────────────────────

    def _ensure_inbox(self, client_id: str) -> asyncio.Queue:
        """Lazy-create a queue for a synthetic non-agent id (used by
        the webapp WS proxy for browser client connections)."""
        return self.ctx.ensure_inbox(client_id)

    @property
    def _inboxes(self) -> dict[str, asyncio.Queue]:
        """Direct ctx.inboxes for compat with code reaching into the
        old Kernel's `_inboxes` dict (mostly the webapp proxy
        cleanup)."""
        return self.ctx.inboxes

    async def recv(self, agent_id: str | None = None):
        """Async iterator over an agent's inbox. Default: self's
        inbox. Pass an id to read another agent's inbox (compat with
        Kernel.recv(id))."""
        if agent_id is None or agent_id == self.id:
            target = self
        else:
            target = self.ctx.agents.get(agent_id)
            if target is None:
                return
        while True:
            yield await target._inbox.get()

    # ─── public API: record CRUD ───────────────────────────────

    def get(self, agent_id: str) -> dict | None:
        """Flat global record lookup. Returns the record dict (id,
        handler_module, parent_id, **meta) or None."""
        a = self.ctx.agents.get(agent_id)
        return a.record if a else None

    def list(self) -> list[dict]:
        """Flat list of every agent's record across the whole tree.
        Compat with today's Kernel.list(). For own-children only,
        use `list_children()`."""
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
        if id in self.ctx.agents:
            return {"error": f"agent {id!r} exists"}
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

    async def delete(self, agent_id: str) -> dict:
        """Cascade-delete an agent and its entire subtree. Returns:
        - {"deleted": True, "id": agent_id} on success
        - {"locked": True, "blocked_by": <id>, "id": agent_id} if any
          descendant has delete_lock=true (no mutations)
        - {"error": "..."} on bad input

        Cascade is depth-first: deepest descendants run their
        `_shutdown` hook (kernel-routed, so process state tears down)
        BEFORE being removed from ctx.agents/parent._children/disk.
        By the time the topmost agent is gone, every PTY/uvicorn task/
        in-flight resource downstream is already torn down.
        """
        target = self.ctx.agents.get(agent_id)
        if target is None:
            return {"error": f"no agent {agent_id!r}", "deleted": False}
        blocked_by = target._find_locked_descendant()
        if blocked_by is not None:
            return {
                "locked": True,
                "blocked_by": blocked_by,
                "error": (
                    f"delete_agent: {agent_id!r} blocked by delete_lock on "
                    f"{blocked_by!r}; clear it via update_agent before deleting"
                ),
                "id": agent_id,
            }
        await target._cascade_delete()
        return {"deleted": True, "id": agent_id}

    # ─── public API: state stream ──────────────────────────────

    def add_state_subscriber(self, callback: Callable[[dict], None]):
        return self.ctx.add_state_subscriber(callback)

    def state_snapshot(self) -> list[dict]:
        return self.ctx.state_snapshot()

    # ─── dispatch (internal) ───────────────────────────────────

    async def _dispatch(self, payload: dict) -> dict | None:
        """Resolve verb → native or handler_module."""
        verb = payload.get("type")
        self._in_flight += 1
        self._fanout(payload, kind="send")
        token = _current_sender.set(self.id)
        try:
            if verb in _SYSTEM_VERBS:
                return await self._handle_system_verb(verb, payload)
            # The tree root answers `reflect` with the substrate identity
            # even when it carries a handler_module (the root loader): its
            # persistence verbs (load_tree / persist_record / boot) still
            # route to the bundle below, but `reflect <root>` is the
            # kernel's own identity + tree, matching the `kernel` alias.
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
                return {
                    "error": f"agent {self.id!r} has no handler_module; cannot answer verb {verb!r}"
                }
            try:
                mod = importlib.import_module(self.handler_module)
            except Exception as e:
                return {"error": f"import {self.handler_module!r}: {e}"}
            if not hasattr(mod, "handler"):
                return {"error": f"{self.handler_module} has no handler()"}
            return await mod.handler(self.id, payload, self)
        finally:
            _current_sender.reset(token)
            self._in_flight = max(0, self._in_flight - 1)
            self.ctx.publish_state(
                {"agent_id": self.id, "kind": "drain", "backlog": self._in_flight}
            )

    def _fanout(self, payload: dict, *, kind: str) -> None:
        """Publish state event for self's traffic + mirror to watchers.

        Watchers may be agents OR synthetic ids (browser WS clients
        registered by the webapp proxy). Both have entries in
        `ctx.inboxes`. Fanout puts on `ctx.inboxes[watcher_id]`
        regardless.

        State events carry the full payload (`payload` field) for
        observers — bundle tests + telemetry pane both subscribe.
        Summary stays as the trimmed one-line view for visualization.
        Telemetry rays drop synthetic-id watchers (would mint phantom
        sprites in the agent vis); agent watchers do publish.
        """
        sender = _current_sender.get()
        summary = _summarize_payload(payload)
        self._put_drop_oldest(self._inbox, payload)
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
            if tgt_agent is not None:
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
        return {"error": f"unhandled system verb {verb!r}"}

    def _verb_list_agents(self, payload: dict) -> dict:
        # Flat list of every agent's record (global registry). For
        # own-children only, callers use the `list_children` Python
        # method on the Agent instance, or walk a `reflect` tree.
        return {"agents": [a.record for a in self.ctx.agents.values()]}

    async def _verb_create_agent(self, payload: dict) -> dict:
        handler_module = payload.get("handler_module")
        if not handler_module:
            return {"error": "create_agent: handler_module required"}
        meta = {
            k: v
            for k, v in payload.items()
            if k not in ("type", "handler_module", "id")
        }
        rec = self.create(handler_module, id=payload.get("id"), **meta)
        if "error" in rec:
            return rec
        # Boot via kernel routing.
        try:
            await self.send(rec["id"], {"type": "boot"})
        except Exception as e:
            print(f"  [create_agent] {rec['id']} boot raised: {e}")
        # Lifecycle event on self's inbox.
        await self.emit(
            self.id, {"type": "agent_created", "id": rec["id"], "agent": rec}
        )
        return rec

    async def _verb_update_agent(self, payload: dict) -> dict:
        target_id = payload.get("id")
        if not target_id:
            return {"error": "update_agent: id required"}
        meta = {k: v for k, v in payload.items() if k not in ("type", "id")}
        changed = list(meta.keys())
        rec = self.update(target_id, **meta)
        if rec is None:
            return {"error": f"no agent {target_id!r}"}
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
        if not target_id:
            return {"error": "delete_agent: id required"}
        result = await self.delete(target_id)
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
        it up and invokes it."""
        if self.handler_module:
            try:
                mod = importlib.import_module(self.handler_module)
                fn = getattr(mod, "on_delete", None)
                if fn is not None:
                    await fn(self)
            except Exception as e:
                print(f"  [cascade] {self.id} bundle on_delete raised: {e}")

    async def shutdown(self) -> None:
        """Graceful process-shutdown walk. Depth-first like
        cascade-delete, but DOES NOT touch records, disk, or tree
        membership — only invokes each bundle's teardown hook to free
        OS resources (subprocesses, PTYs, open sockets, in-flight
        tasks). The agent tree survives in `ctx.agents` and on disk,
        so the next boot rehydrates cleanly.

        Hook resolution per agent: a bundle that wants to distinguish
        "I'm being shut down, will restart" from "I'm being deleted
        forever" can define a module-level `on_shutdown(agent)` — this
        walker calls it. If absent, it falls back to the bundle's
        `on_delete(agent)` (most bundles just kill subprocesses there,
        which is exactly what shutdown wants). Run on SIGTERM /
        SIGINT / SIGHUP (see kernel._modes._default) and as an
        atexit safety net from main.py."""
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
            mod = importlib.import_module(self.handler_module)
        except Exception as e:
            print(
                f"  [shutdown] {self.id} import {self.handler_module!r} raised: {e}",
                file=sys.stderr,
            )
            return
        fn = getattr(mod, "on_shutdown", None) or getattr(mod, "on_delete", None)
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
            return node
        node.update(self._meta)
        if self.handler_module:
            try:
                mod = importlib.import_module(self.handler_module)
                verbs_dict = getattr(mod, "VERBS", None)
                if isinstance(verbs_dict, dict):
                    node["verbs"] = {
                        n: (f.__doc__ or "").strip().splitlines()[0]
                        for n, f in verbs_dict.items()
                    }
            except Exception:
                pass
        node["in_flight"] = self._in_flight
        return node

    def _tree(self, *, depth: int | None, details: bool, current_depth: int) -> dict:
        node = self._node_summary(details=details)
        if depth is None or current_depth < depth:
            node["children"] = [
                c._tree(depth=depth, details=details, current_depth=current_depth + 1)
                for c in self._children.values()
            ]
        else:
            node["children"] = []
        return node

    # ─── reflect surface ───────────────────────────────────────
    #
    # The composable `reflect` flags (tree / bundles / readme) live in
    # `_apply_reflect_flags` (next to `send`). There is no `primer()` —
    # transport/wire docs moved into the root readme (`reflect
    # readme=true`); `available_bundles` is now the `bundles` flag.
