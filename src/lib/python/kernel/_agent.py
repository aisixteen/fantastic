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
import os
import secrets
import sys
from importlib.metadata import entry_points
from pathlib import Path
from typing import Any, Callable

from kernel._authz import DEFAULT_CAN_CALL, authorize_call
from kernel._kernel import (
    BUNDLE_ENTRY_GROUP,
    Kernel,
    _current_sender,
    _current_task,
    _summarize_payload,
    operator_context,
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
        # The TASK TABLE — work in flight, universal. `ps` reads it, `cancel`
        # kills a task and its subtree. Baked into the substrate, NOT a bundle:
        # cancellation is a property of the one primitive (`send`), so EVERY
        # agent is cancelable and a caller never needs to know which bundle it
        # faces. Plain verbs — an AGENT can call them on another agent.
        "ps",
        "cancel",
        # Root-only control verb — gated inside its handler to the tree
        # root (a non-root agent answers with an error). Stops the whole
        # kernel process gracefully; see `_verb_shutdown_kernel`.
        "shutdown_kernel",
    }
)

# One line per system verb — the honest reflect surface. Bundle verbs are
# derived from their VERBS docstrings; these are the substrate's own, so they
# appear in no bundle's map and are advertised here (root reflect).
# The verbs that CHANGE something. Distilled reflect carries their argument line;
# read-only verbs carry only their name (their reply teaches the rest).
_MUTATING_VERBS = frozenset(
    {"create_agent", "update_agent", "delete_agent", "cancel", "shutdown_kernel"}
)

_SYSTEM_VERB_DOCS = {
    "create_agent": (
        "args: handler_module:str, id:str?, **meta — spawn a child agent + boot it."
    ),
    "update_agent": "args: id:str, **meta — patch a record's meta.",
    "delete_agent": (
        "args: id:str — cascade-delete an agent + its subtree (delete_lock refuses)."
    ),
    "list_agents": "No args. Every agent's record, flat.",
    "ps": (
        "No args. The live TASK TABLE — {tasks:[{id, agent, parent, verb, "
        "started_at}]}. One row per in-flight dispatch; `parent` chains them "
        "into the send tree."
    ),
    "cancel": (
        "args: task_id:str — kill that task AND ITS SUBTREE (children first). "
        "Returns {cancelled:bool, killed:[task_id]}; the blocked caller's `send` "
        "resolves to {cancelled:true}. Unknown/finished id ⇒ {cancelled:false}."
    ),
    "shutdown_kernel": "No args. ROOT ONLY — stop the kernel process gracefully.",
}


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
        # share, so the webapp proxy's ctx.ensure_inbox(client_id) and
        # routing fanout both look up here.
        self._inbox: asyncio.Queue = self.ctx.ensure_inbox(self.id)
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

    def _read_readme(self) -> str | None:
        """The agent's own `readme.md` content, or None if it has none.
        Read-only — the loader seeds the file (copy-if-missing) when it
        persists the record; Agent just reads it back for `reflect`.

        For an agent whose manual is a keyed doc TREE (a `docs/` dir beside
        `readme.md`), this is the SHORT orientation; the topics are fetched by
        key via `_readme_view` (`readme=index` / `readme=<key>`)."""
        p = self._root_path / "readme.md"
        if p.exists():
            try:
                return p.read_text(encoding="utf-8")
            except OSError:
                return None
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
    def _docs_dir(self) -> Path:
        return self._root_path / "docs"

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

    def _readme_index(self, base: Path | None = None, prefix: str = "") -> list[dict]:
        """Token-annotated topic index under `docs/` (recursive; nesting
        preserved). Each entry `{key, desc, tokens, children?}` — NO body."""
        d = base if base is not None else self._docs_dir
        if not d.is_dir():
            return []
        entries: list[dict] = []
        for f in sorted(d.iterdir()):
            if not (f.is_file() and f.suffix == ".md"):
                continue
            try:
                body = f.read_text(encoding="utf-8")
            except OSError:
                continue
            key = prefix + f.stem
            entry: dict[str, Any] = {
                "key": key,
                "desc": self._read_desc(body),
                "tokens": self._est_tokens(body),
            }
            sub = d / f.stem
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
        f = d / (safe + ".md")
        try:
            f.resolve().relative_to(d.resolve())  # clamp inside docs/
        except (ValueError, OSError):
            return {"error": f"bad key {sel!r}", "keys": self._readme_keys()}
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
        sub = d / safe
        if sub.is_dir():
            children = self._readme_index(sub, safe + "/")
            if children:
                view["children"] = children
        return view

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

        The dispatch runs as a REGISTERED asyncio.Task (one row in the
        kernel task table) — that is what makes any send cancelable
        (`cancel`). A cancelled dispatch does NOT raise out of `send`:
        it resolves to `{"cancelled": True}`.
        """
        if target_id == "kernel":
            target = self._root()
            # `kernel` is an alias for the tree root. reflect is a pure
            # read — answer it from the root's identity without a full
            # dispatch (no telemetry fanout); flags applied below.
            if payload.get("type") == "reflect":
                return await self._apply_reflect_flags(
                    target, payload, target._reflect_identity()
                )
        else:
            # Resolve a well-known alias (declared via an `alias` meta, e.g.
            # `web_loader`) to its agent id; a literal id resolves to itself.
            target = self.ctx.agents.get(self.ctx.well_known.get(target_id, target_id))
        if target is None:
            return self.ctx.unknown_agent(target_id)
        # ── CALL GATE — deny-all for agent-originated sends ──────────────
        # The trust boundary is who ORIGINATED the send. An operator send
        # (`_current_sender` is None — CLI / cold bootstrap / loader-at-boot /
        # test driver) is the root of trust and passes ungated. An agent send
        # (a handler/script calling out; `_current_sender` is its id) is
        # DENY-ALL: permitted only by an explicit grant on the sender's record.
        # `reflect` is never gated (discovery moat).
        verb = payload.get("type")
        sender_id = _current_sender.get()
        if sender_id is not None and verb != "reflect":
            # A sender with no record cannot be vouched for → an empty grant,
            # which is deny-all (never a hole).
            sender_rec = self.ctx.get(sender_id) or {"id": sender_id}
            sender_agent = self.ctx.agents.get(sender_id)
            child_ids = (
                frozenset(sender_agent._children) if sender_agent is not None else None
            )
            d = authorize_call(
                sender_rec,
                verb,
                target.id,
                loader_id=self._loader_id(),
                child_ids=child_ids,
            )
            if not d.allowed:
                return {"error": d.reason, "reason": "unauthorized", "hint": d.hint}
        reply = await self._run_dispatch(target, payload)
        return await self._apply_reflect_flags(target, payload, reply)

    def _loader_id(self) -> str | None:
        """The id `$loader` resolves to in a grant — the persistence loader, which
        IS the tree root (`kernel_state`, where `persist_blob`/`load_blob` live).
        A per-namespace sub-loader (e.g. the frontend's `web/<session>`) would
        refine this to the nearest loader ancestor; the host kernel has one."""
        return self._root().id

    async def _run_dispatch(self, target: "Agent", payload: dict) -> dict | None:
        """Run ONE dispatch as a registered, abortable asyncio.Task and await
        it — the unit of the task table.

        Parent propagation: the task stamps its own id on `_current_task`, so
        any `send` the handler issues registers as a CHILD of this task. The
        table is a tree; `cancel` walks it.

        The cancellation contract, and why it is not ambiguous. Both cases
        surface as a `CancelledError` at this one await:
          - THE DISPATCH was cancelled (someone sent `cancel`) → the awaiting
            caller is NOT being killed; it gets the reply `{"cancelled": True}`.
            Cancellation never raises out of `send`.
          - WE were cancelled (our own task is inside the killed subtree) →
            `current_task().cancelling()` is non-zero, so we re-raise and take
            the dispatch down with us. Swallowing here would resurrect a task
            the kernel just killed.
        """
        ctx = self.ctx
        task_id = ctx.mint_task_id(target.id)
        task = asyncio.create_task(
            target._dispatch_task(task_id, payload), name=f"fantastic:{task_id}"
        )
        ctx.register_task(
            task_id,
            target.id,
            payload.get("type"),
            task,
            parent=_current_task.get(),
        )
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

    _MORE_HINT = (
        "Distilled: every agent and what it is FOR. Reflect one agent for its verbs "
        "({type:'reflect'}). Manual: {type:'reflect', readme:'index'} - keys include "
        "system-verbs (ps/cancel/create_agent args) and self-extension (author your own "
        "durable tools). {type:'reflect', detail:'full'} adds wiring and posture - ask "
        "for it only when you need to see how agents are plumbed, not to find one."
    )

    async def _roster(self, a) -> dict:
        """`{id, sentence, mode?, children:[...]}` — the tree, but carrying what
        each agent is FOR instead of how it is wired.

        Same nesting as `_tree` (ownership is real information, and every caller
        already walks it that way); the swap is the FIELDS. `_tree` gives
        `handler_module` + posture — an operator's view, asked once. This gives
        `sentence` + `mode` — the router's view, asked every turn. Two agents of
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
        # Purpose comes from the agent's REFLECT, not its record: `mode` is often
        # a bundle default (yaml_state is "data" unless told otherwise), so a
        # record-only read reports nothing for exactly the agents whose purpose
        # you most need — and would be a second source of truth.
        for k in self._PURPOSE_KEYS:
            if raw.get(k) is not None:
                node[k] = raw[k]
        node["children"] = [await self._roster(c) for c in a._children.values()]
        return node

    async def _apply_reflect_flags(self, target, payload, reply):
        """Compose any reflect reply with the universal flags — applied
        uniformly to bare-agent and bundle reflects alike:

        - `detail=distilled|full` (default distilled). DISTILLED answers the
          question a caller actually has most turns — *who do I send to?* — with
          a roster of `{id, sentence, mode?}` and nothing else. FULL is the
          orientation document: wiring/posture on every node, verb docstrings,
          and the inlined doc frame. The default is distilled because selection
          happens every turn while orientation happens once, and because the
          full view is NOT a superset: it carries `handler_module` and
          `file_bridge_id` but no `sentence`, so two agents of the same bundle
          are indistinguishable in it at any price. Escalation rides in the
          reply's own `more` line, not in any caller's system prompt.
        - `tree=all|ids|none` (default all): `all` nests the distilled
          subtree; `ids` is a flat descendant-id index; `none` omits it.
        - `bundles=all|ids|none` (default none): `all` is the
          {name, handler_module} catalog; `ids` is bare names; `none`
          omits it.
        - `readme=true`: attach the agent's `readme.md` orientation (string
          or null). `readme="index"`: the token-annotated topic tree.
          `readme="<key>"`: one topic's body + its sub-docs' index.
          Atomic — one agent.

        Transport/wire docs are NOT here — they live in the doc tree
        (`reflect readme=index`, then drill by key)."""
        if payload.get("type") != "reflect" or not isinstance(reply, dict):
            return reply
        full = payload.get("detail") == "full"
        # `description` is a substrate meta field — surface it on EVERY
        # reflect (bundle handlers don't know about it), unless the
        # reply already set one.
        if target.description is not None and "description" not in reply:
            reply["description"] = target.description
        # Kernel runtime identity + deployment context — surfaced on the ROOT
        # reflect so a client that hops to this kernel learns, in one
        # round-trip: which runtime (`runtime`), WHERE it runs (`env` —
        # "container" when launched from the image, else "host"), and which
        # build (`version`). env/version are read from the optional
        # FANTASTIC_ENV / FANTASTIC_VERSION envs the container bakes in; they
        # are RUN-scoped (never persisted to the portable .fantastic workdir,
        # which can move host↔container). Same field names + key order
        # (runtime → env → version) across all four runtimes.
        # SYSTEM VERBS — on EVERY reflect, not just the root.
        #
        # They are universal: every agent answers create/update/delete/ps/cancel
        # natively, and they appear in no bundle's `verbs` map. Advertising them
        # only on the ROOT was an optimization that put them where they were least
        # needed — gemma4:12b reflected `assets`/`transcripts`/`secrets` (that is
        # where the descriptions are), never reflected the root, and so never saw
        # `update_agent`'s signature at all. It guessed the args from neighbouring
        # names, guessed wrong, and abandoned the task. Placement has to follow
        # WHERE A CALLER LOOKS, not where the field is cheapest to attach.
        #
        # Distilled carries every NAME plus the ARGUMENTS of the mutating verbs;
        # full carries every docstring. The split is by BLAST RADIUS: a wrong guess
        # about `ps` costs one failed read and the reply teaches you; a wrong guess
        # about `update_agent` blocks the task or mutates the wrong thing — and
        # those are the calls a weak model will not fire blind to find out.
        #
        # Costs ~115 tok, +11-17% on a per-agent reflect. Measured, and accepted.
        reply["system_verbs"] = (
            dict(_SYSTEM_VERB_DOCS)
            if full
            else {
                k: (
                    _SYSTEM_VERB_DOCS[k].split("—")[0].strip()
                    if k in _MUTATING_VERBS
                    else ""
                )
                for k in sorted(_SYSTEM_VERB_DOCS)
            }
        )
        if target.parent is None:
            reply["runtime"] = "python"
            reply["env"] = os.environ.get("FANTASTIC_ENV", "host")
            reply["version"] = os.environ.get("FANTASTIC_VERSION")
            # The substrate's OWN verbs — answered natively by EVERY agent, so
            # they appear in no bundle's `verbs` map. Advertised here (once, on
            # the root) so the reflect surface stays honest: a caller learns
            # from one round-trip that work in flight is inspectable (`ps`) and
            # killable (`cancel`), on any agent.
            # Duck-typed root extension: the root's handler_module (the loader) may
            # contribute root-only fields the substrate can't compute without coupling
            # to bundle logic — e.g. `persistence: {provider}` (which file_bridge the
            # loader auto-persists THROUGH, or null = RAM). No bundle import: if the
            # module exposes `reflect_root_extra(agent) -> dict`, merge it.
            if target.handler_module:
                try:
                    extra = importlib.import_module(
                        target.handler_module
                    ).reflect_root_extra(target, full=full)
                    if isinstance(extra, dict):
                        reply.update(extra)
                except Exception:
                    pass
        # NOTE: the identity fields (`parent_id`, `handler_module`, `guid`,
        # `version`, `display_name`) are NOT trimmed in distilled. Dropping them
        # was tried: it saves ~50 tok of a ~600 tok payload and breaks 10 tests,
        # because they are not trivia — `guid` is the kernel INSTANCE identity a
        # relay peer advertises as `owner_guid`, `version` names the build, and
        # `parent_id` is structure. Cheap fields with real consumers stay.
        tree = payload.get("tree", "all")
        if tree == "all":
            reply["tree"] = (
                target._tree(depth=None, details=False, current_depth=0)
                if full
                else await self._roster(target)
            )
        elif tree == "ids":
            reply["tree"] = target._descendant_ids()
        if not full:
            reply["more"] = self._MORE_HINT
        bundles = payload.get("bundles", "none")
        if bundles == "all":
            reply["bundles"] = target._available_bundles()
        elif bundles == "ids":
            reply["bundles"] = [b["name"] for b in target._available_bundles()]
        rm = payload.get("readme")
        if rm is True:
            reply["readme"] = target._read_readme()
        elif isinstance(rm, str) and rm:
            reply["readme"] = target._readme_view(rm)
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
            return self.ctx.unknown_agent(agent_id, deleted=False)
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

    async def _dispatch_task(self, task_id: str, payload: dict) -> dict | None:
        """The body of a registered task: stamp `_current_task` so nested sends
        become CHILDREN of this task, then dispatch. The contextvar is set (not
        wrapped in try/finally) because an asyncio task owns a COPY of the
        context — the stamp cannot leak to the caller. The table entry drops via
        the task's done-callback, cancelled or not."""
        _current_task.set(task_id)
        return await self._dispatch(payload)

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
        if verb == "ps":
            return self._verb_ps(payload)
        if verb == "cancel":
            return await self._verb_cancel(payload)
        if verb == "shutdown_kernel":
            return await self._verb_shutdown_kernel(payload)
        return {"error": f"unhandled system verb {verb!r}"}

    def _verb_ps(self, payload: dict) -> dict:
        """No args. The live TASK TABLE — {tasks:[{id, agent, parent, verb,
        started_at}]}. Kernel-wide (like `list_agents`), not per-agent: one row
        per in-flight dispatch, `parent` chaining them into the send tree, so a
        caller sees the whole tree and who spawned what. `ps` shows its own row
        too — it is itself a dispatch."""
        return {"tasks": self.ctx.task_rows()}

    async def _verb_cancel(self, payload: dict) -> dict:
        """args: task_id:str — kill that task AND ITS SUBTREE (children first,
        depth-first, like killing a process group). Each bundle's `on_cancel`
        hook runs BEFORE the abort so real resources (OS subprocesses, PTYs)
        die with it. Returns {cancelled:True, killed:[task_id, ...]}; an unknown
        or already-finished id returns {cancelled:False, killed:[]} — it does
        not pretend. The cancelled dispatch's blocked caller gets the reply
        {"cancelled": True}."""
        task_id = payload.get("task_id")
        if not isinstance(task_id, str) or not task_id:
            return {"error": "cancel: task_id (str) required"}
        return await self.ctx.cancel_task(task_id)

    async def _verb_shutdown_kernel(self, payload: dict) -> dict:
        """Gracefully stop the whole kernel PROCESS (root control surface
        only). Privileged: a non-root agent answers with an error so the
        verb is gated to the kernel's own control surface, not an
        arbitrary child.

        Order: (1) ack `{type:"shutdown_kernel", ok:true}` and let it
        flush; (2) AFTER the reply is on the wire, trigger the daemon's
        normal graceful path (release `.fantastic/lock.json`, drain
        in-flight + stop the HTTP/WS listeners), (3) exit code 0. The exit
        is DEFERRED via `call_later` so the in-flight REST body / WS frame
        is fully written before uvicorn is drained — never exit
        synchronously here or the caller sees a dropped socket, not an ack.

        Backend-agnostic stop: the kernel is PID 1 in a container, so its
        exit stops the container (auto-removed under `--rm`); a bare host
        process simply dies. Either way the port goes down and the lock
        releases — the caller need not know how the kernel was launched.

        Idempotent / one-shot: reuses the daemon's single `stop` event, so
        a racing SIGTERM collapses to one shutdown; a second call lands on
        a dead port. In non-daemon contexts (one-shot CLI / REPL-only with
        no serve loop) there is nothing to stop — it still acks."""
        if self.parent is not None:
            return {
                "error": (
                    "shutdown_kernel: root control surface only; address "
                    "the kernel root (alias 'kernel')"
                )
            }
        ev = self.ctx.shutdown_event
        if ev is not None and not ev.is_set():
            # Defer the trigger so the ack reply is fully flushed to the
            # caller before the serve loop tears uvicorn down. 100ms is
            # imperceptible and well within a bounded grace; uvicorn's own
            # graceful drain (server.should_exit) handles the socket close.
            asyncio.get_running_loop().call_later(0.1, ev.set)
        return {"type": "shutdown_kernel", "ok": True}

    _LIST_HINT = (
        "Raw records — how agents are WIRED, not what they are FOR, and no manual. "
        "{type:'reflect'} on the root is the briefing: each agent's purpose, plus "
        "`self_extension` (how to author your own durable tools in THIS kernel — which "
        "executor, and the fields without which the agent is created and never runs) "
        "and `readmes` (the manual index). Reflect the root before building anything."
    )

    def _verb_list_agents(self, payload: dict) -> dict:
        # Flat list of every agent's record (global registry). For
        # own-children only, callers use the `list_children` Python
        # method on the Agent instance, or walk a `reflect` tree.
        #
        # `more` is not decoration. Measured 2026-08-07 (probe 06 A/B, gemma-4-12B):
        # the arm that opened with `reflect` received `self_extension` and attempted
        # `create_agent` 10 times; the arm that opened with `list_agents` got these
        # bare records, never learned that authoring a script agent was the mechanism,
        # spent 8 turns reasoning "it doesn't explicitly say how to call send from
        # inside the script" — which `self_extension` states outright — and made 0
        # authoring attempts. Same model, same task; the whole divergence was the
        # FIRST call. This verb is advertised in every reflect's `system_verbs`, so
        # "list the agents" is a natural opener, and it was a dead end with no
        # signpost back.
        return {
            "agents": [a.record for a in self.ctx.agents.values()],
            "more": self._LIST_HINT,
        }

    @staticmethod
    def _refuse_meta(
        handler_module: str | None, meta: dict, record: dict | None = None
    ) -> str | None:
        """A bundle's own objection to this meta patch, or None. Optional + duck-typed.

        `record` is the agent's CURRENT record (`{}` at create, where none exists yet),
        so a bundle validates the state the write WILL produce rather than the patch in
        isolation. Without it a cross-field rule is unenforceable: `exec_bridge` refuses
        an `interpreter` that is not in `interpreters`, and validating the patch alone
        rejected `update_agent id=x interpreter=<already on x's allowlist>` — a legal
        write refused because the key it must be checked against was not in the same
        payload. A false refusal is worse than no check: it makes the correct operation
        look impossible.

        ONE implementation, used by BOTH `create_agent` and `update_agent`. It was only
        wired into update, and the asymmetry was live: `create_agent
        handler_module=file_bridge.tools ingress_rule=bogus` returned a record with
        `ingress_rule: "bogus"` while the identical `update_agent` was refused by the
        identical function. Measured 2026-08-07. The argument the update path already
        makes applies verbatim at create — accepting a value you cannot honour and
        reporting success is the worst of both, and it is worse at create because there
        is no prior good state to fall back to.
        """
        if not handler_module:
            return None
        try:
            validate = importlib.import_module(handler_module).validate_meta
        except (ImportError, AttributeError):
            return None
        return validate(meta, record or {})

    async def _verb_create_agent(self, payload: dict) -> dict:
        handler_module = payload.get("handler_module")
        if not handler_module:
            return {"error": "create_agent: handler_module required"}
        meta = {
            k: v
            for k, v in payload.items()
            if k not in ("type", "handler_module", "id")
        }
        # Every created agent carries the structural baseline (command your own
        # subtree, answer your creator, persist your own state through the loader).
        # It's a BASELINE, not a lateral grant, so it MERGES: an explicit `can_call`
        # in the payload adds lateral edges on top (and may override a baseline key
        # by naming it), but never has to re-declare $self/$parent/$children/$loader.
        # Deny-all still governs every edge NOT in the merged grant.
        grant = dict(DEFAULT_CAN_CALL)
        grant.update(meta.get("can_call") or {})
        meta["can_call"] = grant
        # Refuse BEFORE the agent exists, so a bad wiring value never becomes a record
        # that boots, half-works, and fails three calls later somewhere else.
        # No record exists yet, so the merged view IS the patch — an `interpreter`
        # with no `interpreters` beside it genuinely cannot run and is refused here.
        if why := self._refuse_meta(handler_module, meta, {}):
            return {"error": why, "created": False}
        rec = self.create(handler_module, id=payload.get("id"), **meta)
        if "error" in rec:
            return rec
        # Boot via kernel routing. This send is a SIDE EFFECT of the already-
        # authorized create_agent — the kernel initializing the child it was
        # told to make, not the parent reaching out — so it runs with operator
        # authority (ungated), never against the parent's own grant.
        try:
            with operator_context():
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
        target = self.ctx.agents.get(target_id)
        if target is None:
            return self.ctx.unknown_agent(target_id, updated=False)
        # WRITE-TIME VALIDATION, duck-typed. A record can carry meta only its bundle
        # understands, so the substrate cannot judge it — but the bundle can. If the
        # handler_module exposes `validate_meta(meta) -> str | None`, a returned
        # string REFUSES the patch and is reported at the point of the mistake.
        #
        # Without this, `update_agent` accepted anything: on 2026-08-02 an agent set
        # `ingress_rule="password:kernel-internal-transcripts"`, got `{"updated":
        # true}` back, and only discovered it was meaningless when the next call to
        # that agent failed. Accepting a value you cannot honour, and reporting
        # success, is the worst of both — the caller believes a security control is
        # in place that is not. Same discovery + optionality as `on_delete`.
        if why := self._refuse_meta(target.handler_module, meta, target.record):
            return {"error": why, "id": target_id, "updated": False}
        rec = self.update(target_id, **meta)
        if rec is None:
            return self.ctx.unknown_agent(target_id, updated=False)
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
        raises is logged and the abort still happens: cancel is not refusable."""
        if not self.handler_module:
            return
        try:
            mod = importlib.import_module(self.handler_module)
            fn = getattr(mod, "on_cancel", None)
            if fn is not None:
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

        Hook resolution per agent: a bundle that wants to distinguish
        "I'm being shut down, will restart" from "I'm being deleted
        forever" can define a module-level `on_shutdown(agent)` — this
        walker calls it. If absent, it falls back to the bundle's
        `on_delete(agent)` (most bundles just kill subprocesses there,
        which is exactly what shutdown wants). Run on SIGTERM /
        SIGINT / SIGHUP (see kernel.modes._default) and as an
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

    # Wiring/posture meta surfaced inline in the DISTILLED tree (not only the full
    # `details` node), so a client reads the IO landscape from ONE root reflect: which
    # legs are open vs sealed-by-default, what a consumer is wired to. The substrate
    # only exposes its own record fields (no rule interpretation, no bundle import) —
    # the LLM reads `ingress_rule: deny_inbound` (sealed) / `allow_all` (open) /
    # `password` (gated) and acts per the keystone readme (read-key == write-key);
    # ABSENT on an io leg ⇒ sealed-by-default. `root`/`file_bridge_id` show the binding.
    _POSTURE_KEYS = ("ingress_rule", "egress_rule", "auth", "root", "file_bridge_id")
    # PURPOSE, not wiring. Posture answers "how is this plumbed" (an operator
    # question, asked once); purpose answers "what is this for" (a routing
    # question, asked every turn). The roster carries purpose.
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
