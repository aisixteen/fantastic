"""The Agent class — recursive node in the kernel tree.

Every entity in the system is an Agent. Agents have:
  - A persistent record on disk (`<root_path>/agent.json`).
  - A `_children: dict[str, Agent]` (empty for leaves).
  - An asyncio inbox for incoming payloads.
  - A handler_module that answers domain verbs (when the verb isn't
    a system verb that Agent answers natively).

Routing:
  All cross-agent communication goes through the kernel — `agent.send
  (target_id, payload)` resolves `target_id` in the flat global
  `ctx.agents` dict and dispatches its handler. Bundles never call
  each other's handler functions directly. The browser bus
  (BroadcastChannel) is a separate, parallel channel for browser-side
  iframe-to-iframe traffic that bypasses the server.

Lifecycle:
  - `create_agent` (system verb) — substrate persists a new record on
    disk under the parent, registers in `ctx.agents`, sends `boot`.
  - `_boot` (per-bundle hook) — hydrates process-memory state. May
    idempotently spawn children (terminal_webapp creates terminal_-
    backend on first boot; subsequent boots find the child already
    present and skip).
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
import importlib.resources
import json
import secrets
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

    Class-level `ephemeral`: when True, the agent never persists to
    disk (no agent.json, no agents/ dir). Use for per-process
    composables that have no meaningful state — e.g. the stdout
    renderer (`Cli`). Defaults to False; subclasses override.
    """

    # Subclass-level opt-out: ephemeral agents skip _persist() + dir
    # creation. They live in memory only; reboot loses them; mode
    # composition (in main.py / core) decides when to recreate them.
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

        `root_path` is THIS agent's directory. The agent's record file
        is `<root_path>/agent.json`. Children live at
        `<root_path>/agents/<child_id>/`. Required when `parent` is
        None and not `ephemeral`; optional otherwise.
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
        if root_path is None:
            if parent is None and not is_ephemeral:
                raise ValueError(
                    "Agent: root_path required when parent is None (non-ephemeral)"
                )
            # Ephemeral root-less agent: no disk path needed.
            # Ephemeral child: still pick a path under parent for
            # consistency (even though we won't write to it).
            if parent is not None:
                root_path = parent._children_dir() / id
            else:
                root_path = Path("/tmp/_ephemeral_root")  # never used
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
        if not is_ephemeral:
            self._root_path.mkdir(parents=True, exist_ok=True)
            self._persist()
            self._seed_readme()
        # Wire into parent's children dict + publish lifecycle event.
        # `parent.create(...)` is the indirect path; this one supports
        # direct construction (`Cli(kernel, parent=core)`).
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
        if not is_ephemeral:
            self._load_children()

    # ─── persistence ───────────────────────────────────────────

    def _agent_file(self) -> Path:
        return self._root_path / "agent.json"

    def _children_dir(self) -> Path:
        return self._root_path / "agents"

    def _persist(self) -> None:
        if type(self).ephemeral:
            return
        self._agent_file().write_text(json.dumps(self.record, indent=2))

    def _seed_readme(self) -> None:
        """Copy the bundle's shipped `readme.md` into this agent's dir
        on first creation. Copy-if-missing — operator edits and the
        GitHub-canonical version are never clobbered. The substrate
        stays bundle-agnostic: it derives the package from
        `handler_module` and asks `importlib.resources` whether that
        package ships a `readme.md`. Bundles without one → no readme;
        `reflect(return_readme=true)` then returns `readme: null`."""
        if not self.handler_module:
            return
        dest = self._root_path / "readme.md"
        if dest.exists():
            return
        pkg = self.handler_module.split(".")[0]
        try:
            src = importlib.resources.files(pkg) / "readme.md"
            if src.is_file():
                dest.write_text(src.read_text(encoding="utf-8"), encoding="utf-8")
        except (ModuleNotFoundError, FileNotFoundError, OSError, TypeError):
            pass

    def _read_readme(self) -> str | None:
        """The agent's own `readme.md` content, or None if it has none."""
        p = self._root_path / "readme.md"
        if p.exists():
            try:
                return p.read_text(encoding="utf-8")
            except OSError:
                return None
        return None

    def _load_children(self) -> None:
        """Recursively hydrate children from `<self>/agents/`."""
        cdir = self._children_dir()
        if not cdir.exists():
            return
        for entry in sorted(cdir.iterdir()):
            af = entry / "agent.json"
            if not af.exists():
                continue
            try:
                rec = json.loads(af.read_text())
            except (json.JSONDecodeError, OSError):
                continue
            child_meta = {
                k: v
                for k, v in rec.items()
                if k not in ("id", "handler_module", "parent_id")
            }
            child = Agent(
                id=rec["id"],
                root_path=entry,
                ctx=self.ctx,
                parent=self,
                handler_module=rec.get("handler_module"),
                **child_meta,
            )
            self._children[child.id] = child

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
            # `kernel.reflect` returns the substrate primer (root primer).
            if payload.get("type") == "reflect":
                reply = target.primer()
                return self._maybe_attach_readme(target, payload, reply)
        else:
            target = self.ctx.agents.get(target_id)
        if target is None:
            return {"error": f"no agent {target_id!r}"}
        reply = await target._dispatch(payload)
        return self._maybe_attach_readme(target, payload, reply)

    @staticmethod
    def _maybe_attach_readme(target, payload, reply):
        """Post-process a `reflect` reply: when the caller passed
        `return_readme: true`, attach the target agent's `readme.md`
        content as `reply["readme"]` (or None if it has none). Default
        is off — reflect stays lean unless asked."""
        if (
            payload.get("type") == "reflect"
            and payload.get("return_readme")
            and isinstance(reply, dict)
        ):
            reply["readme"] = target._read_readme()
        return reply

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
        target._persist()
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
            if not self.handler_module:
                # Bare agents (root) handle a few universal verbs natively:
                # - boot/shutdown: no-op (no process state to manage)
                # - reflect: substrate primer if root, else distilled summary
                if verb in ("boot", "shutdown"):
                    return None
                if verb == "reflect":
                    return (
                        self.primer()
                        if self.parent is None
                        else self._node_summary(details=True)
                    )
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
        if verb == "reflect":
            return self._verb_reflect(payload)
        if verb == "list_agents":
            return self._verb_list_agents(payload)
        if verb == "create_agent":
            return await self._verb_create_agent(payload)
        if verb == "update_agent":
            return await self._verb_update_agent(payload)
        if verb == "delete_agent":
            return await self._verb_delete_agent(payload)
        return {"error": f"unhandled system verb {verb!r}"}

    def _verb_reflect(self, payload: dict) -> dict:
        depth = payload.get("depth", None)
        flat = bool(payload.get("flat", False))
        details = bool(payload.get("details", False))
        return self.reflect(depth=depth, flat=flat, details=details, root_view=False)

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
        depth-first (children first). Two responsibilities:
          1. Tear down bundle-specific process-memory state.
          2. Clean up own disk artifact (rmtree this agent's root_path).

        Default: if `handler_module` exposes `async def on_delete(agent)`,
        call it for (1); then rmtree `self._root_path` for (2) unless
        this is an ephemeral agent.

        Bundles port their teardown logic into a module-level
        `on_delete(agent)` function in their tools.py — substrate looks
        it up and invokes it before disk removal."""
        if self.handler_module:
            try:
                mod = importlib.import_module(self.handler_module)
                fn = getattr(mod, "on_delete", None)
                if fn is not None:
                    await fn(self)
            except Exception as e:
                print(f"  [cascade] {self.id} bundle on_delete raised: {e}")
        if not type(self).ephemeral and self._root_path.exists():
            self._rmtree(self._root_path)

    @staticmethod
    def _rmtree(path: Path) -> None:
        if not path.exists():
            return
        for sub in path.iterdir():
            if sub.is_dir():
                Agent._rmtree(sub)
            else:
                try:
                    sub.unlink()
                except OSError:
                    pass
        try:
            path.rmdir()
        except OSError:
            pass

    # ─── reflection ────────────────────────────────────────────

    def _root(self) -> "Agent":
        node = self
        while node.parent is not None:
            node = node.parent
        return node

    def reflect(
        self,
        *,
        depth: int | None = None,
        flat: bool = False,
        details: bool = False,
        root_view: bool = True,
    ) -> dict:
        """Return a tree (default) or flat list describing this agent
        and its descendants.

        - `depth=None` → unbounded recursion
        - `depth=0` → just self, no children
        - `flat=False` → nested tree with `children:[...]`
        - `flat=True` → flat list, each carrying `parent_id`
        - `details=False` → distilled (id, parent_id, handler_module, display_name)
        - `details=True` → full per-agent (verbs from VERBS dict, meta)
        - `root_view=True` (default) → applies the discovery contract:
          distilled is the default; details opt-in
        - `root_view=False` (per-agent direct reflect) → defaults to
          full detail (since the caller already chose this agent)
        """
        effective_details = details or (not root_view)
        if flat:
            out: list[dict] = []
            self._flatten(out, depth=depth, details=effective_details, current_depth=0)
            return {"agents": out, "agent_count": len(out)}
        return self._tree(depth=depth, details=effective_details, current_depth=0)

    def _node_summary(self, *, details: bool) -> dict:
        node: dict[str, Any] = {
            "id": self.id,
            "parent_id": self.parent.id if self.parent else None,
            "handler_module": self.handler_module,
            "display_name": self.display_name or self.id,
        }
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

    def _flatten(
        self,
        out: list[dict],
        *,
        depth: int | None,
        details: bool,
        current_depth: int,
    ) -> None:
        out.append(self._node_summary(details=details))
        if depth is None or current_depth < depth:
            for c in self._children.values():
                c._flatten(
                    out, depth=depth, details=details, current_depth=current_depth + 1
                )

    # ─── primer (root-only substrate description) ──────────────

    def primer(self) -> dict:
        """Substrate primer — what `kernel.reflect` returns over WS
        and what CLI `reflect` prints. Tree-style structural reflect
        plus wire/transport metadata external tools need to bootstrap.
        Only meaningful on root."""
        bundles = sorted(
            (
                {"name": ep.name, "handler_module": ep.value}
                for ep in entry_points(group=BUNDLE_ENTRY_GROUP)
            ),
            key=lambda b: b["name"],
        )
        return {
            "sentence": "Fantastic kernel. Everything is reachable by sending messages to agents.",
            "primitive": "send(target_id, payload) -> reply | None",
            "envelope": '{"type": "<verb>", ...fields}',
            "universal_verb": "reflect — every agent answers it; returns identity + flat state dict.",
            "transports": {
                "in_process": {
                    "shape": "await agent.send(target_id, payload)",
                    "use_when": "Python code running inside the kernel process.",
                },
                "in_prompt": {
                    "shape": '<send id="<agent_id>" payload=\'{"type":"<verb>", ...}\'/>',
                    "use_when": "agentic LLM loops emitting XML-tagged tool calls.",
                    "example": '<send id="<agent_id>" payload=\'{"type":"list_agents"}\'/>',
                },
                "cli": {
                    "shape": "fantastic <agent_id> <verb> [k=v ...]",
                    "shorthand": "fantastic reflect [<agent_id>]",
                    "note": "one-shot — refused while a daemon owns the dir; use the web surface then.",
                },
            },
            "well_known": dict(self.ctx.well_known),
            "tree": self.reflect(),
            "available_bundles": bundles,
            "agent_count": len(self.ctx.agents),
            "binary_protocol": {
                "trigger": "any bytes value anywhere in the payload",
                "wire_format": "WS binary frame: [4-byte BE uint32 H | H-byte JSON header | M-byte raw bytes]",
                "header_field": "_binary_path names the dotted-path field whose value is the body",
                "purpose": "skip base64+JSON encoding for high-throughput byte payloads (audio, image, video)",
            },
            "browser_bus": {
                "channel": "fantastic",
                "envelope": "{type, target_id, source_id, ...fields}",
                "transport": "BroadcastChannel (browser-only; structured-clone)",
                "scope": "intra-browser messaging between iframes; bypasses kernel.send entirely",
                "available_in_js": "fantastic_transport().bus",
                "use_when": "UI-internal traffic (audio frames, drag events, cursor) where round-tripping the server adds no value",
            },
        }
