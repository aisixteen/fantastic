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
import json
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
    _FROM_ABOVE,
    authorize_call,
)
from kernel._kernel import (
    BUNDLE_ENTRY_GROUP,
    _in_shutdown,
    CANCELLED,
    COMPLETED,
    FAILED,
    Kernel,
    TIMED_OUT,
    _current_origin,
    _current_sender,
    _current_task,
    _summarize_payload,
    operator_context,
    outcome,
    page,
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
        # The EXECUTION LEDGER's read surface — named executions, live and
        # settled, outcomes included. `ps` is work in flight; `ledger` is what
        # named work BECAME. Terminal non-completed rows ARE the dead-letter
        # queue: parked, inspectable, never silently retried.
        "ledger",
        # THE ROW STORE's read surface — conversation rows by id, at DIGEST
        # resolution. Gated INSIDE the handler (root verbs skip ingress): an
        # agent reads its own rows; anyone else's only when that owner's
        # `callable_by` names the caller for `messages`; the operator sees all.
        "messages",
        # Root-only control verb — gated inside its handler to the tree
        # root (a non-root agent answers with an error). Stops the whole
        # kernel process gracefully; see `_verb_shutdown_kernel`.
        "shutdown_agents",
        "shutdown_kernel",
    }
)

# One line per system verb — the honest reflect surface. Bundle verbs are
# derived from their VERBS docstrings; these are the substrate's own, so they
# appear in no bundle's map and are advertised here (root reflect).
# The verbs that CHANGE something. Distilled reflect carries their argument line;
# read-only verbs carry only their name (their reply teaches the rest).
_MUTATING_VERBS = frozenset(
    {
        "create_agent",
        "update_agent",
        "delete_agent",
        "cancel",
        "shutdown_agents",
        "shutdown_kernel",
    }
)

_SYSTEM_VERB_DOCS = {
    # THE CONSEQUENCE MUST PRECEDE THE FIRST EM-DASH. A default reflect keeps only
    # the text BEFORE it (see `_apply_reflect_flags`), so anything after the dash
    # reaches `detail:"full"` callers only — and these are the facts a caller needs
    # BEFORE firing the verb, not after. Measured 2026-08-24: `create_agent` read
    # as `args: handler_module:str, id:str?, **meta` with "spawn a child agent"
    # cut, and a model spent 16 turns unable to work out what the target meant,
    # finishing with `agents_created: []`.
    "create_agent": (
        "args: handler_module:str, parent_id:str REQUIRED, id:str?, **meta. "
        "Creates a CHILD OF `parent_id` — your own id grows your own subtree "
        "(no grant needed); 'kernel_state' for top-level. No default parent."
    ),
    "update_agent": (
        "args: id:str, **meta. EVERY FIELD YOU NAME IS REPLACED, NOT MERGED "
        "(a field you do not name is untouched), so a dict field must be sent "
        "WHOLE — `can_call`/`callable_by` included: restate every key you want "
        "to keep, the baseline `$self`/`$parent`/`$children` among them, or the "
        "keys you omit are GONE. Granting one edge reads like a PATCH and "
        "behaves like a PUT."
    ),
    "delete_agent": (
        "args: id:str. CASCADES — deletes that agent AND ITS WHOLE SUBTREE, "
        "deepest first; refuses if any descendant carries `delete_lock`."
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
        "attempts, retries, named, outcome?}]}, paged, scoped to what YOU "
        "originated. Failed/timed_out/cancelled rows are the dead-letter queue: "
        "inspect, then retry with a NEW id if you choose — nothing is retried "
        "silently."
    ),
    "messages": (
        "args: ids:list[str] (req), limit:int?, after:str?. Conversation rows by id, "
        "at DIGEST resolution — {rows:[{id, agent, parent, kind, role, head, bytes, "
        "title?, level?, refs?}], refused:[ids], next?}. Your own rows always; another "
        "agent's only if its callable_by names you for `messages`. Bodies are behind "
        "that agent's `pull`."
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
    "shutdown_kernel": (
        "No args. Stops the kernel PROCESS — under env:'container', the container."
    ),
}


def _as_id(x) -> int | None:
    """A message id as an int: an int, or its decimal string (a model may quote
    what it copied out of a tag). Anything else is not an id."""
    if isinstance(x, bool):
        return None
    if isinstance(x, int):
        return x if x > 0 else None
    if isinstance(x, str) and x.isdigit():
        return int(x)
    return None


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
        (file / chat / readme) compute their paths under it, and
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
        # Structural authority baseline, stamped + MERGED on BOTH fields for EVERY
        # agent that passes through here — the seeded root, a created child, a
        # disk-loaded record alike (one funnel, so nothing lacks it). `can_call`
        # (who I may call) and `callable_by` (who may call me) both start as
        # {$self:*, $parent:*, $children:*} — reach/accept your own subtree and your
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
        rec.update(self._meta)
        # STRUCTURE WINS. `parent_id` is not meta — it is where this agent
        # actually sits, and the tree is the only thing that knows. It used to be
        # written BEFORE the meta merge, so a `parent_id` that leaked into meta
        # overwrote it and the record reported a parent the tree did not have:
        # `$parent` then resolved differently on the two halves of the gate,
        # `web/tools.py` read the lie, and a reboot silently corrected it from
        # disk position. Written last, it cannot be shadowed.
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

        Statuses: completed | failed | cancelled | timed_out — see the
        outcome vocabulary in `_kernel.py`. `pid is None` ⟺ the item never
        reached the task table (refusal, malformed item, kernel-reflect fast
        path). Nothing raises out of `send` except CancelledError (the caller
        itself being killed) — a handler raise settles as `failed`.
        """
        if not isinstance(items, list):
            raise TypeError("send takes a LIST of {target, payload, timeout?} items")
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
        # and never an `unhandled` from the target (measured 2026-09-02).
        if "type" not in payload:
            return outcome(None, FAILED, missing("send", "payload.type"))
        verb = payload.get("type")
        if not isinstance(verb, str) or not verb:
            return outcome(
                None,
                FAILED,
                refusal(
                    BAD_ARG,
                    f"send: payload.type must be a non-empty string, got {verb!r}",
                    arg="payload.type",
                ),
            )
        # The item's OWN fields are validated before anything is resolved, gated
        # or spawned: a bad `id`/`retries` used to be caught only after an unnamed
        # dispatch had already started (orphaning a running task).
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
        # A DYING TASK REGISTERS NO NEW WORK. `cancel_task` marks the whole
        # subtree before it aborts anything, because `on_cancel` hooks await —
        # so a doomed handler gets scheduling turns on its way out, and what it
        # sent there used to start fresh dispatches that outlived the cancel.
        cur = _current_task.get()
        if cur is not None and (self.ctx.tasks.get(cur) or {}).get("_killed"):
            return outcome(None, CANCELLED, {"cancelled": True})
        # WHILE DRAINING, NO NEW ROOTS. A send from inside live work (a parent task
        # in the table — including an agent's own `shutdown` turn using its tools)
        # passes; the kernel's own shutdown traffic passes; everything else is a
        # new root and is refused before it costs an id.
        # The root's own control surface stays open while draining: kernel verbs
        # (`shutdown_agents` again, `shutdown_kernel`, `ps`, `messages`, `ledger`)
        # and `reflect` are how an operator watches and finishes the stop.
        to_root = target_id == "kernel" or target_id == self._root().id
        if (
            self.ctx.draining
            and not _in_shutdown.get()
            and not (to_root and (verb in _SYSTEM_VERBS or verb == "reflect"))
            and (cur is None or cur not in self.ctx.tasks)
            and not self.ctx.is_registered(asyncio.current_task())
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
            # Resolve a well-known alias (declared via an `alias` meta, e.g.
            # `web_loader`) to its agent id; a literal id resolves to itself.
            target = self.ctx.agents.get(self.ctx.well_known.get(target_id, target_id))
        if target is None:
            return outcome(None, FAILED, self.ctx.unknown_agent(target_id))
        # ── CALL GATE — fully-gated, two-sided, from-above ──────────────
        # Operator sends (`_current_sender` is None: CLI / cold bootstrap /
        # loader-at-boot / test driver, or a verb running under
        # `operator_context`) are the ORIGIN root of trust and pass ungated. Every
        # AGENT-originated send — ANY verb, `reflect` included, nothing exempt —
        # needs BOTH the sender's `can_call` (egress) AND the target's
        # `callable_by` (ingress) to grant the edge.
        sender_id = _current_sender.get()
        if sender_id is not None:
            denial = self._gate(sender_id, verb, target, payload)
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
                # true outcome; no second execution ever starts (C9/C10/C11).
                #
                # AUTHORIZED AGAINST THE EXECUTION IT JOINS, not against the
                # payload the joiner happened to send. Joining hands back
                # ANOTHER caller's result verbatim, so it is equivalent to
                # having made that call — and the gate must be asked that
                # question, not a different one.
                #
                # Measured 2026-08-28: `agent_b`, granted only `safe_verb` on a
                # target, reused an id `agent_a` had used for
                # `create_privileged` and received agent_a's secret verbatim.
                # Its own gate check passed — on `safe_verb`, a verb whose reply
                # it never got. Same class as the read side-channel #36 closed
                # by origin-scoping the `ledger` VERB; this is the same leak
                # through `send` itself. Reachable from the wire, where a frame
                # id IS the execution id (#38) and two legs can name one id.
                #
                # An OPERATOR joiner (sender None) is ungated here as everywhere.
                joiner = _current_sender.get()
                if joiner is not None:
                    orig_verb = (entry.get("payload") or {}).get("type")
                    denial = self._gate(
                        joiner, orig_verb, target, entry.get("payload") or {}
                    )
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
                exec_id, target, payload, timeout, item, name=name
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
            pid, task = self._spawn_dispatch(target, payload, task_id=exec_id)
            dispatched.append(
                _Dispatched(
                    index, pid, task, timeout, target, payload, exec_fut=fut, name=name
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
        fut, bad = self._ledger_open(pid, target, payload, timeout, item, name=None)
        if bad is not None:
            return bad
        pid, task = self._spawn_dispatch(target, payload, task_id=pid)
        dispatched.append(
            _Dispatched(index, pid, task, timeout, target, payload, exec_fut=fut)
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

    def _ledger_open(self, exec_id: int, target, payload, timeout, item, *, name):
        """Open one execution under a pre-minted id: the CALL ROW in the message
        store (what outlives the process) and the process side (`executions`).
        `retries` already validated. Returns (fut, refusal?)."""
        retries = item.get("retries", 0)
        if bad := self._check_retries(item):
            return None, bad
        fut: asyncio.Future = asyncio.get_event_loop().create_future()
        origin = _current_sender.get()
        # EVERY SEND IS A MESSAGE. The row names the call — who was asked what,
        # by which message — and takes the outcome's status at settle; the reply
        # body stays with the reply. `parent` is the message that made the call.
        row = {
            "id": exec_id,
            "kind": "call",
            "verb": payload.get("type"),
            "parent": _current_task.get(),
            "origin": origin,
        }
        if name is not None:
            row["name"] = name
        self.ctx.rows_put(target.id, row)
        # The side entry is the WHOLE engine: dedup and join key on it, retry
        # re-issues against it, dead-letter is reading it. `attempts` counts real
        # runs; `origin` scopes who may see the row through the `ledger` verb.
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
                "origin": origin,
            },
        )
        return fut, None

    def _spawn_dispatch(
        self, target: "Agent", payload: dict, *, task_id: int
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
            target._dispatch_task(task_id, payload, origin),
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
            # forget_task has already dropped the previous attempt's row, so the
            # SAME task id is free to register again — one id, one `cancel`
            # handle, attempt after attempt.
            _pid, task = self._spawn_dispatch(d.target, d.payload, task_id=d.pid)
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
                        # would orphan them. `operator_context` because the
                        # kernel enforcing the caller's own deadline is
                        # bookkeeping, and it keeps an on_cancel hook's own
                        # sends (ai_core emits status via send("cli", ...))
                        # from being gated as the batch caller.
                        with operator_context():
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
            if not d.flags_done:
                reply = await self._apply_reflect_flags(d.target, d.payload, reply)
            return self._record(d, outcome(d.pid, COMPLETED, reply))
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
                    self.ctx.ledger_settled()
                raise
            if d.task.cancelled():
                return self._record(d, outcome(d.pid, CANCELLED, {"cancelled": True}))
            raise
        except Exception as e:  # noqa: BLE001 — a handler raise is a RESULT
            return self._record(
                d,
                outcome(
                    d.pid,
                    FAILED,
                    {"error": f"{type(e).__name__}: {e}", "raised": True},
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
        ever see the FINAL outcome: the future settles once, at the end."""
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
            self.ctx.ledger_settled()
        return out

    def _stamp_call(self, pid: int | None, out: dict) -> None:
        """The call row takes the outcome's STATUS (and a refusal's error/reason),
        never the result body — bodies are the reply's business."""
        row = self.ctx.rows.get(pid) if pid is not None else None
        if row is None:
            return
        res = out.get("result")
        stamp = {"status": out.get("status")}
        if isinstance(res, dict) and res.get("error"):
            stamp["error"] = str(res.get("error"))[:200]
            if res.get("reason"):
                stamp["reason"] = res.get("reason")
        row["outcome"] = stamp

    def advertises(self, verb: str) -> bool:
        """Whether this agent's bundle answers `verb` (its `VERBS` table)."""
        if not self.handler_module:
            return False
        try:
            mod = importlib.import_module(self.handler_module)
        except Exception:  # noqa: BLE001 — an unimportable bundle advertises nothing
            return False
        verbs = getattr(mod, "VERBS", None)
        return isinstance(verbs, dict) and verb in verbs

    def module_hook(self, name: str):
        """A duck-typed module-level hook of this agent's bundle, or None."""
        if not self.handler_module:
            return None
        try:
            mod = importlib.import_module(self.handler_module)
        except Exception:  # noqa: BLE001
            return None
        fn = getattr(mod, name, None)
        return fn if callable(fn) else None

    def _loader_id(self) -> str | None:
        """The id `$loader` resolves to in a grant — the persistence root."""
        return self._root().id

    def _resolve_agent(self, agent_id: object) -> "Agent | None":
        """Resolve an id the way `send` does — the `kernel` alias, a declared
        `alias`, or a literal id. None when it names no live agent."""
        if not isinstance(agent_id, str) or not agent_id:
            return None
        if agent_id == "kernel":
            return self._root()
        return self.ctx.agents.get(self.ctx.well_known.get(agent_id, agent_id))

    def _gate_placement(self, sender_id, s_can, s_parent, s_kids, root_id, parent):
        """Two-sided gate on the PLACEMENT TARGET of `create_agent`. Returns a
        refusal, or None when the sender may create under `parent`.

        Placing inside your OWN subtree is baseline (no grant). Anywhere else
        needs egress on the parent (explicitly named — `create_agent` is
        privileged, so a wildcard never covers it) AND the parent's own
        `callable_by` accepting the sender."""
        node = parent
        while node is not None:
            if node.id == sender_id:
                return None  # your own subtree — baseline
            node = node.parent
        d = authorize_call(
            s_can,
            "create_agent",
            parent.id,
            enforce_privileged=True,
            holder_id=sender_id,
            holder_parent=s_parent,
            holder_children=s_kids,
            loader_id=root_id,
        )
        if not d.allowed:
            return refusal(
                UNAUTHORIZED,
                f"{sender_id} may not create under {parent.id}",
                hint=_FROM_ABOVE,
                side="egress",
            )
        di = authorize_call(
            parent._meta.get("callable_by") or {},
            "create_agent",
            sender_id,
            holder_id=parent.id,
            holder_parent=parent.parent.id if parent.parent is not None else None,
            holder_children=frozenset(parent._children),
            loader_id=root_id,
            enforce_privileged=False,
        )
        if not di.allowed:
            return refusal(
                UNAUTHORIZED,
                f"{parent.id} does not accept a child from {sender_id}",
                hint=_FROM_ABOVE,
                side="ingress",
            )
        return None

    def _gate(self, sender_id: str, verb: str, target: "Agent", payload: dict):
        """The two-sided call gate. Returns an error dict if the send is refused,
        else None. Checks egress (sender's `can_call`) then ingress (target's
        `callable_by`), both via the one `authorize_call` engine, then the
        authority-conferral rules (no self-raise; delegate only a subset)."""
        root_id = self._root().id
        sender = self.ctx.agents.get(sender_id)
        sender_rec = self.ctx.get(sender_id) or {"id": sender_id}
        s_can = sender_rec.get("can_call") or {}
        s_parent = sender_rec.get("parent_id")
        s_kids = frozenset(sender._children) if sender is not None else frozenset()
        # PLACEMENT IS THE OPERAND. `create_agent` is answered by the root, but
        # the agent it ACTS UPON is the parent named in `parent_id` — so that is
        # what the gate asks about, on both sides. Gating the root instead made
        # one grant (`<root>:[create_agent]`, the ordinary "may author tools"
        # wiring) a key to the whole tree: the holder could plant a child under
        # ANY agent and inherit `$parent:*` reach into it (measured 2026-09-02).
        #
        # FORKING YOUR OWN SUBTREE IS BASELINE — placing under yourself or a
        # descendant needs no grant at all, which is what makes self-extension
        # work with zero wiring at any depth.
        if verb == "create_agent":
            parent = self._resolve_agent(payload.get("parent_id"))
            if parent is not None:
                denial = self._gate_placement(
                    sender_id, s_can, s_parent, s_kids, root_id, parent
                )
                if denial is not None:
                    return denial
                return self._check_conferral(
                    sender_id, s_can, s_parent, s_kids, root_id, payload
                )
            # An unknown parent falls through to the ordinary path; the verb
            # itself answers `unknown_agent`.
        # Egress — may the SENDER call the target?
        d = authorize_call(
            s_can,
            verb,
            target.id,
            enforce_privileged=True,
            holder_id=sender_id,
            holder_parent=s_parent,
            holder_children=s_kids,
            loader_id=root_id,
        )
        if not d.allowed:
            return refusal(
                UNAUTHORIZED,
                f"{sender_id} may not call {verb!r} on {target.id}",
                hint=_FROM_ABOVE,
                side="egress",
            )
        # INGRESS DOES NOT APPLY TO A KERNEL VERB. `callable_by` answers "do I
        # accept being acted upon by you" — and for `create_agent`/`ps`/`cancel`
        # and friends the root is not what is acted upon. It is the control
        # surface; the operand rides in the payload (`parent_id`, `id`,
        # `task_id`). Asking the root to accept each caller would make the
        # operator edit the ROOT's record for every agent that needs to read the
        # task table, and would answer a question about the wrong agent.
        #
        # So egress is the whole check here, which is the decision on record:
        # placement is ungated, `can_call` decides. An agent granted
        # `kernel_state: [ps, cancel]` can watch and kill ANY task — that
        # granularity was the price of moving these verbs to the root.
        if verb in _SYSTEM_VERBS and target.parent is None:
            if verb in ("create_agent", "update_agent"):
                return self._check_conferral(
                    sender_id, s_can, s_parent, s_kids, root_id, payload
                )
            return None
        # Ingress — does the TARGET accept the sender? (tokens relative to target)
        t_call = target._meta.get("callable_by") or {}
        t_parent = target.parent.id if target.parent is not None else None
        t_kids = frozenset(target._children)
        di = authorize_call(
            t_call,
            verb,
            sender_id,
            holder_id=target.id,
            holder_parent=t_parent,
            holder_children=t_kids,
            loader_id=root_id,
            enforce_privileged=False,
        )
        if not di.allowed:
            return refusal(
                UNAUTHORIZED,
                f"{target.id} does not accept {verb!r} from {sender_id}",
                hint=_FROM_ABOVE,
                side="ingress",
            )
        # Authority conferral — no self-raise, delegate only a subset you hold.
        if verb in ("create_agent", "update_agent"):
            return self._check_conferral(
                sender_id, s_can, s_parent, s_kids, root_id, payload
            )
        return None

    def _check_conferral(self, sender_id, s_can, s_parent, s_kids, root_id, payload):
        """Authority flows from ABOVE only: an agent may not raise its OWN
        authority, and may confer on another only a SUBSET of its own `can_call`.
        Operator conferral (`sender is None`) never reaches here."""
        if not isinstance(payload, dict):
            return None
        if "can_call" not in payload and "callable_by" not in payload:
            return None
        edited = payload.get("id")
        # Self-raise: never edit your own authority fields.
        if payload.get("type") == "update_agent" and edited == sender_id:
            return {
                "error": f"{sender_id} may not edit its own can_call/callable_by",
                "reason": "unauthorized",
                "side": "self-grant",
                "hint": _FROM_ABOVE,
            }
        # Subset: every LATERAL edge in the conferred `can_call` must be one the
        # granter itself holds (structural tokens are free; `callable_by` grants no
        # reach, so it is unbounded).
        conferred = payload.get("can_call")
        if isinstance(conferred, dict):
            for key, verbs in conferred.items():
                if key in ("$self", "$parent", "$children"):
                    continue  # relative to the HOLDER — no lateral reach
                if key == "$loader":
                    # NOT relative: `$loader` resolves to the ROOT for whoever
                    # holds it, so conferring it hands over real reach on the
                    # kernel's control surface (ps, cancel, ledger,
                    # shutdown_kernel). Subset-probe it like any lateral edge —
                    # skipping it let a grandchild grant its own child root
                    # reach it did not hold (measured 2026-09-02).
                    probe_key = root_id
                elif key.startswith("$"):
                    return {
                        "error": (
                            f"{sender_id} cannot confer {key!r}: not a structural "
                            "token — $self, $parent, $children and $loader are the "
                            "only ones"
                        ),
                        "reason": "bad_arg",
                        "arg": "can_call",
                        "side": "delegation",
                        "hint": _FROM_ABOVE,
                    }
                else:
                    probe_key = key
                need = list(verbs) if isinstance(verbs, (list, tuple)) else None
                # Conferring `"*"` (or a list containing it) requires the granter
                # to hold `"*"` on that target; a concrete list requires each verb.
                probe = need if (need and "*" not in need) else ["*"]
                for v in probe:
                    ok = authorize_call(
                        s_can,
                        v,
                        probe_key,
                        holder_id=sender_id,
                        holder_parent=s_parent,
                        holder_children=s_kids,
                        loader_id=root_id,
                    ).allowed
                    if not ok:
                        if key == "$loader":
                            return {
                                "error": (
                                    f"{sender_id} cannot delegate {v!r} on $loader: "
                                    f"it resolves to {root_id}, which you do not "
                                    "hold — $loader is real reach on the kernel's "
                                    "control surface, not a structural token"
                                ),
                                "reason": "unauthorized",
                                "side": "delegation",
                                "hint": _FROM_ABOVE,
                            }
                        # Name the TRUE reason. A target that is not an agent at
                        # all is almost always one created in the SAME batch —
                        # items launch concurrently, so it is not there yet when
                        # this gate runs. "you don't hold it" sent a builder
                        # into a 382-second deliberation (measured 2026-09-02).
                        if self.ctx.agents.get(key) is None:
                            return {
                                "error": (
                                    f"{sender_id} cannot delegate {v!r} on {key}: "
                                    "no such agent — calls of one turn run "
                                    "concurrently, so an agent created in this "
                                    "batch does not exist yet; create in one turn, "
                                    "grant in the next"
                                ),
                                "reason": "unauthorized",
                                "side": "delegation",
                                "missing": key,
                                "hint": _FROM_ABOVE,
                            }
                        # The target exists but the granter does not hold it. Say
                        # WHY in one line — the usual cause is a specialist created
                        # under the root instead of under the granter, and "you
                        # don't hold it" hid that for twenty turns (2026-09-02).
                        held_parent = self.ctx.agents[key].parent
                        pid = held_parent.id if held_parent is not None else None
                        return {
                            "error": (
                                f"{sender_id} cannot delegate {v!r} on {key}: {key} is "
                                f"not your child (its parent is {pid}) and not in your "
                                "can_call — you may only confer can_call you already "
                                "hold; create the agents you will wire as YOUR children"
                            ),
                            "reason": "unauthorized",
                            "side": "delegation",
                            "parent": pid,
                            "hint": _FROM_ABOVE,
                        }
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
                "origin": _current_sender.get(),
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
        # THE ROSTER IS GATED, one node at a time. `reflect` is not exempt from
        # the call gate anywhere else, but this walk dispatched it directly, so
        # any caller allowed to reflect ONE agent read every descendant's
        # `sentence` and `description` through the tree — lateral discovery the
        # gate exists to close (measured 2026-09-02). A denied node keeps its
        # `id` and `children` (the SHAPE is already public through `tree=ids`)
        # and nothing the agent says about itself. An operator (`sender` None)
        # sees everything, as everywhere else.
        sender = _current_sender.get()
        may_reflect = sender is None or self._gate(sender, "reflect", a, {}) is None
        if a.handler_module and may_reflect:
            raw = await self._run_dispatch(a, {"type": "reflect"})
        elif may_reflect:
            raw = a._reflect_identity()
        else:
            raw = {}
        raw = raw if isinstance(raw, dict) else {}
        node: dict[str, Any] = {"id": a.id}
        if raw.get("sentence"):
            node["sentence"] = raw["sentence"]
        # Operator-set purpose. More specific than the bundle sentence when
        # present ("the prod deploy key" vs "a yaml store"), so it belongs
        # in the router's view above all else.
        if a.description is not None and may_reflect:
            node["description"] = a.description
        # Purpose comes from the agent's REFLECT, not its record: much of what an
        # agent is FOR lives in its reflect (its `sentence`, its verbs), not as a
        # record field, so a record-only read reports nothing for exactly the agents
        # whose purpose you most need — and would be a second source of truth.
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
        # A REFUSAL IS NOT A REFLECT. Decorating one with kernel_verbs/tree/more
        # produced a reply that reads like a healthy reflect with a stray error
        # field, and a caller could not tell the two apart (measured 2026-09-02).
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
        # `kernel_verbs`, not `system_verbs`, and it sits beside `verbs` on purpose:
        # `verbs` is what THIS agent does, `kernel_verbs` is what the KERNEL does
        # when you name this agent. Presenting both under one shape said they were
        # the same kind of thing, and a model read `delete` (drop one key from this
        # store) and `delete_agent` (destroy this subtree) as peers.
        #
        # Still advertised on EVERY agent even though only the root ANSWERS them.
        # Root-only advertisement was tried and reverted: gemma4:12b never reflected
        # the root, never saw the signatures, guessed the args and abandoned the
        # task. Discoverable everywhere, answered in one place, with the `_` line
        # saying which is which.
        reply["kernel_verbs"] = {
            "_": (
                "Kernel-owned: send to 'kernel'. The id you name is the OPERAND, "
                "not the actor. (`verbs` is what this agent itself does.)"
            ),
            **(
                dict(_SYSTEM_VERB_DOCS)
                if full
                else {
                    # Every verb carries its text, truncated at the em-dash.
                    # Read-only ones used to render as "" (name only), on the
                    # ground that their reply teaches the rest — but a name with
                    # no signature is exactly what left `ps` and `list_agents`
                    # unusable without a `full` reflect, and each is one line.
                    k: _SYSTEM_VERB_DOCS[k].split("—")[0].strip()
                    for k in sorted(_SYSTEM_VERB_DOCS)
                }
            ),
        }
        if target.parent is None:
            reply["runtime"] = "python"
            reply["env"] = os.environ.get("FANTASTIC_ENV", "host")
            reply["version"] = os.environ.get("FANTASTIC_VERSION")
            # The KERNEL's own verbs — answered by the root alone, so they appear
            # in no bundle's `verbs` map. Advertised on every agent (see
            # `kernel_verbs` above) so a caller learns in one round-trip that work
            # in flight is inspectable (`ps`) and killable (`cancel`).
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
            # The FLAT id index is the one reflect shape that is a plain list, so
            # it is the one that pages. `tree:"all"` is a nested subtree whose
            # value is its structure — slicing it would hand back a forest with
            # no root and call it a page. Ask for ids to walk a big tree.
            ids, nxt = page(target._descendant_ids(), payload)
            reply["tree"] = ids
            if nxt is not None:
                reply["next"] = nxt
        if not full:
            reply["more"] = self._MORE_HINT
        bundles = payload.get("bundles", "none")
        if bundles == "all":
            cat, nxt = page(
                target._available_bundles(), payload, key=lambda b: b["name"]
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

    def watch(self, src_id: str, tgt_id: str) -> dict | None:
        """Mirror src_id's incoming traffic into tgt_id's inbox. Returns a
        refusal when the gate denies it, else None.

        GATED LIKE A SEND, because it IS a read of the target. A watcher
        receives src's raw inbound payloads verbatim — the model prompt, the
        document body, whatever was sent — so an ungated `watch` is a read
        side-channel around `callable_by`: an agent that may not CALL an agent
        could still read everything sent TO it.

        Found 2026-08-27 in a production audit: a browser leg granted exactly
        `assistant:[send]` could mirror ANY agent's inbox in its session and
        read the packed confidential context out of the model's prompt. The
        `call` path went through the two-sided gate; this one never did.

        The verb checked is `watch`, so an operator grants it exactly like any
        other edge (`callable_by: {viewer: ["watch"]}`). An OPERATOR watch
        (`sender=None` — the bootstrap wiring a renderer, a test) is the origin
        root of trust and stays ungated, as with send."""
        src = self.ctx.agents.get(src_id)
        if src is None:
            return None  # nothing to mirror; not a refusal
        sender_id = _current_sender.get()
        if sender_id is not None:
            denial = self._gate(sender_id, "watch", src, {})
            if denial is not None:
                return denial
        src._watcher_ids.add(tgt_id)
        # Make sure tgt has an inbox (synthetic clients need to
        # be addressable; we lazy-create here so callers don't
        # have to ensure_inbox first).
        self.ctx.ensure_inbox(tgt_id)
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
        return self._gate(watcher_id, "state_subscribe", self._root(), {})

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
        # THE ROOT IS NOT DELETABLE. It owns the persistence surface and answers
        # every kernel verb, so cascading it empties the kernel and leaves a
        # process with no control surface at all — `{"deleted": true}` and an
        # empty tree (measured 2026-09-02). The guard lived only on the unused
        # `Kernel.delete`, which nothing calls.
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
            # THE ONE REFUSAL SHAPE (#27). This carried a bare `locked: True`
            # flag and no `reason` until 2026-08-27, when the shared agentic
            # corpus caught it — in all three runtimes at once, because the flag
            # #27 declared dropped was never actually removed here. A caller had
            # to know it had called `delete_agent` to know which field to read.
            return refusal(
                LOCKED,
                f"delete_agent: {agent_id!r} blocked by delete_lock on "
                f"{blocked_by!r}; clear it via update_agent before deleting",
                hint="clear the lock with update_agent, then delete again",
                blocked_by=blocked_by,
                id=agent_id,
            )
        await target._cascade_delete()
        return {"deleted": True, "id": agent_id}

    # ─── public API: state stream ──────────────────────────────

    def add_state_subscriber(self, callback: Callable[[dict], None]):
        return self.ctx.add_state_subscriber(callback)

    def state_snapshot(self) -> list[dict]:
        return self.ctx.state_snapshot()

    # ─── dispatch (internal) ───────────────────────────────────

    async def _dispatch_task(
        self, task_id: str, payload: dict, origin: str | None = None
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
        return await self._dispatch(payload)

    async def _dispatch(self, payload: dict) -> dict | None:
        """Resolve verb → native or handler_module."""
        verb = payload.get("type")
        self._in_flight += 1
        self._fanout(payload, kind="send")
        token = _current_sender.set(self.id)
        try:
            if verb in _SYSTEM_VERBS:
                # KERNEL-OWNED, answered by the ROOT ONLY. These act on the TREE
                # (or on the kernel-wide task table); the agent they name is the
                # OPERAND, never the actor. Answering them everywhere made the
                # target ambiguous: `create_agent` appeared on all N agents with a
                # doc naming only its arguments, so "send it to X" could mean X
                # performs it, X is created under something, or X is the parent —
                # and only the last was true. Measured 2026-08-24 (`live_subai`,
                # the model): the model asked "But how do I invoke create_agent?
                # Which target?", never resolved it, and finished 16 turns with
                # `agents_created: []`.
                #
                # Refused HERE, in the handler, not in the call gate — the gate
                # runs long before this and several probes assert a specific
                # denial `side` for a non-root privileged send. `shutdown_kernel`
                # already refused this way; this generalises it to all seven.
                if self.parent is not None:
                    return refusal(
                        NOT_HERE,
                        f"{verb} is a kernel verb — {self.id} does not answer it",
                        hint="address the kernel root (alias 'kernel'); name the "
                        "agent you mean in `parent_id` / `id`",
                        verb=verb,
                    )
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
                # CODED, not a bare error. A reason-less `{error}` is decorated
                # by `_apply_reflect_flags` into something that reads like a
                # healthy reflect (kernel_verbs, tree, more) with a stray error
                # field — the caller cannot tell a refusal from an answer
                # (measured 2026-09-02).
                return refusal(
                    UNHANDLED,
                    f"agent {self.id!r} has no handler_module; cannot answer "
                    f"verb {verb!r}",
                    verb=verb,
                )
            try:
                mod = importlib.import_module(self.handler_module)
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
        if verb == "ledger":
            return self._verb_ledger(payload)
        if verb == "messages":
            return self._verb_messages(payload)
        if verb == "cancel":
            return await self._verb_cancel(payload)
        if verb == "shutdown_agents":
            return await self._verb_shutdown_agents(payload)
        if verb == "shutdown_kernel":
            return await self._verb_shutdown_kernel(payload)
        return refusal(UNHANDLED, f"unhandled system verb {verb!r}", verb=verb)

    def _verb_ps(self, payload: dict) -> dict:
        """No args. The live TASK TABLE — {tasks:[{id, agent, parent, verb,
        started_at}]}. Kernel-wide (like `list_agents`), not per-agent: one row
        per in-flight dispatch, `parent` chaining them into the send tree, so a
        caller sees the whole tree and who spawned what. `ps` shows its own row
        too — it is itself a dispatch."""
        tasks, nxt = page(self.ctx.task_rows(), payload, key=lambda r: r["id"])
        return {"tasks": tasks, "next": nxt}

    def _verb_messages(self, payload: dict) -> dict:
        """args: ids:list[int] (req), limit:int?, after:int?. Rows by id at DIGEST
        resolution, gated INSIDE: the caller's own rows always; another agent's only
        when that owner's `callable_by` names the caller for `messages` (the same
        ingress check `_gate` runs, re-run here against the OWNER because a root verb
        skips ingress); the operator sees all. Refused ids are LISTED, never silently
        dropped. Bodies stay behind the owner's `pull`."""
        raw = payload.get("ids")
        ids = [_as_id(x) for x in raw] if isinstance(raw, list) else []
        if not ids or any(x is None for x in ids):
            return refusal(
                BAD_ARG,
                "messages: `ids` must be a non-empty list of message ids (ints)",
                arg="ids",
            )
        caller = _current_origin.get()
        root_id = self._root().id
        allowed_owner: dict[str, bool] = {}

        def may_read(owner_id: str) -> bool:
            if caller is None or caller == owner_id:
                return True
            if owner_id in allowed_owner:
                return allowed_owner[owner_id]
            owner = self.ctx.agents.get(owner_id)
            if owner is None:
                allowed_owner[owner_id] = False
                return False
            d = authorize_call(
                owner._meta.get("callable_by") or {},
                "messages",
                caller,
                holder_id=owner.id,
                holder_parent=owner.parent.id if owner.parent is not None else None,
                holder_children=frozenset(owner._children),
                loader_id=root_id,
                enforce_privileged=False,
            )
            allowed_owner[owner_id] = bool(d.allowed)
            return allowed_owner[owner_id]

        after = _as_id(payload.get("after"))
        if after is not None and after in ids:
            ids = ids[ids.index(after) + 1 :]
        out, refused = [], []
        for rid in ids:
            row = self.ctx.rows.get(rid)
            if row is None:
                refused.append(rid)
                continue
            if not may_read(row.get("agent", "")):
                refused.append(rid)
                continue
            content = str(row.get("content", ""))
            d = {
                "id": rid,
                "agent": row.get("agent"),
                "parent": row.get("parent"),
                "kind": row.get("kind") or row.get("role"),
                "role": row.get("role"),
                "head": content[:120].replace("\n", " "),
                "bytes": len(content),
            }
            if row.get("title"):
                d["title"] = row["title"]
            if row.get("level"):
                d["level"] = row["level"]
            if row.get("refs"):
                d["refs"] = len(row["refs"])
            if row.get("kind") in ("call", "task"):
                d["verb"] = row.get("verb")
                if row.get("name"):
                    d["name"] = row["name"]
                if row.get("outcome"):
                    d["status"] = row["outcome"].get("status")
            elif row.get("kind") == "view":
                # a view digest says how much it holds, never the array itself
                d["ids"] = len(row.get("ids") or [])
                d["budget"] = row.get("budget")
                d["title"] = row.get("why")
            out.append(d)
        # DIGESTS FIT WHERE THEY ARE GOING: page by `limit`, and never past ~32KB.
        try:
            limit = int(payload.get("limit") or 0)
        except (TypeError, ValueError):
            limit = 0
        cap_bytes = 32 * 1024
        kept: list[dict] = []
        used = 0
        for d in out:
            if limit and len(kept) >= limit:
                break
            n = len(json.dumps(d))
            if used + n > cap_bytes:
                break
            kept.append(d)
            used += n
        nxt = kept[-1]["id"] if len(kept) < len(out) and kept else None
        return {"rows": kept, "refused": refused, "next": nxt}

    def _verb_ledger(self, payload: dict) -> dict:
        """No args. EVERY execution and what it became — named under its id,
        unnamed under its minted pid — {executions:[{id, target, status,
        attempts, retries, named, outcome?}]}, paged. `status` is 'in_flight'
        until the row settles, then the outcome's own status; `outcome` appears
        only once settled. `named` says which kind of row it is: True for a
        caller-named execution (its `id` is the caller's own, and is also its
        `cancel`/`ps` handle), False for an unnamed send. Either way `id` is the
        kernel's message id (an int); a named row also carries `name`.

        This docstring — and the `ledger` line every reflect carries — used to
        say NAMED executions only, and that "unnamed sends never appear here".
        Both were true before unnamed executions were ledgered under their pid
        and neither was true afterwards, so a model reading the reflect expected
        its own handful of jobs and received every minted-pid row beside them.

        Rows whose status is failed/timed_out/cancelled are the DEAD-LETTER
        QUEUE: terminal, inspectable, never silently retried — a retry is a
        caller's explicit new id."""
        # ORIGIN-SCOPED. The ledger carries other agents' RESULT BODIES; an
        # unscoped read would be a side-channel around `callable_by` (read any
        # agent's replies without its ingress ever saying yes). An agent sees
        # the executions IT originated; the operator (sender=None) sees all.
        viewer = _current_origin.get()
        rows = []
        for eid, row in self.ctx.executions.items():
            if viewer is not None and row.get("origin") != viewer:
                continue
            fut = row["fut"]
            entry = {
                "id": eid,
                "target": row["target"],
                "attempts": row["attempts"],
                "retries": row["retries"],
                "named": row["named"],
            }
            if row.get("name"):
                entry["name"] = row["name"]
            if fut.done():
                out = fut.result()
                entry["status"] = out["status"]
                # TODO(ledger digest): this is the FULL result body of every execution,
                # with no default `limit` (page(): absent ⇒ everything) and a docstring
                # that says "No args." — so one call returns more than the caller's
                # whole window and the turn dies on `too_small`. Measured 2026-09-05:
                # 34k–127k tokens against a 12k window, 5 of 6 e2e arms killed. Fix:
                # digest rows {id, target, verb, status, attempts, bytes, head, title}
                # here, full body only via {type:'ledger', id}. Same in rust and swift.
                # Kernel change — waiting on explicit approval, python first.
                entry["outcome"] = out
            else:
                entry["status"] = "in_flight"
            rows.append(entry)
        page_rows, nxt = page(rows, payload, key=lambda r: r["id"])
        return {"executions": page_rows, "next": nxt}

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

    async def _verb_shutdown_agents(self, payload: dict) -> dict:
        """No args. The agent half of a graceful stop: drain, the `shutdown` verb
        to every agent that advertises it, quiet, cancel the rest. The kernel keeps
        serving; the persister catches up. Root control surface only."""
        if self.parent is not None:
            return {
                "error": (
                    "shutdown_agents: root control surface only; address "
                    "the kernel root (alias 'kernel')"
                )
            }
        return await self.ctx.shutdown_agents()

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

    @staticmethod
    def _bad_grant_shape(meta: dict) -> str | None:
        """A grant map's SHAPE, judged at WRITE time — the gate only judges edges.

        `{id|token: "*" | [verbs]}` — anything else is refused here, so a record
        can never carry a grant the gate would have to crash or guess on.
        Measured 2026-09-01 (E21_plan): `update_agent` accepted `can_call:"*"`
        (a bare string), answered `{"updated": true}`, and the gate raised
        `.items()` out of a web serve — a correctly-opened leg answered 500."""
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
                if not isinstance(k, str) or not (
                    v == "*"
                    or (isinstance(v, list) and all(isinstance(x, str) for x in v))
                ):
                    return f'{field}[{k!r}] must be "*" or a list of verb strings'
        return None

    async def _verb_create_agent(self, payload: dict) -> dict:
        """args: handler_module, parent_id (REQUIRED), id?, **meta.

        `parent_id` NAMES THE OWNER. Until 2026-08-24 this verb was answered by
        every agent and the ADDRESSED agent became the parent — the only way to
        nest — while a `parent_id` in the payload was stripped and silently
        ignored. Both halves were bad: the working mechanism was undiscoverable
        (a default reflect truncates the doc before "spawn a child agent"), and
        the field that looked like it expressed placement did nothing. Verified
        before the change: asking for `parent_id:"specialist"` produced
        `parent_id='kernel_state'` with no error.

        Now the placement is always stated. There is no implicit parent — an
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
        # caller could have addressed — including the `kernel` alias for the root,
        # which is otherwise special-cased in `send` alone.
        if parent_id == "kernel":
            parent = self._root()
        else:
            parent = self.ctx.agents.get(self.ctx.well_known.get(parent_id, parent_id))
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
        # The structural authority baseline ({$self,$parent,$children} on both
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
        # Boot via kernel routing. This send is a SIDE EFFECT of the already-
        # authorized create_agent — the kernel initializing the child it was
        # told to make, not the parent reaching out — so it runs with operator
        # authority (ungated), never against the parent's own grant.
        with operator_context():
            boot = (
                await self.send([{"target": rec["id"], "payload": {"type": "boot"}}])
            )[0]
        if boot["status"] == FAILED:
            # A handler raise settles as a failed outcome now — send never
            # leaks it. Surfaced, not fatal: the agent exists, its boot broke.
            print(f"  [create_agent] {rec['id']} boot failed: {boot['result']}")
        # Lifecycle event on self's inbox.
        # On the PARENT's inbox, not the root's: whoever watches an agent for new
        # children still sees them arrive. Addressing used to make these the same.
        await parent.emit(
            parent.id, {"type": "agent_created", "id": rec["id"], "agent": rec}
        )
        return rec

    async def _verb_update_agent(self, payload: dict) -> dict:
        """Set a record's meta. EVERY NAMED FIELD IS REPLACED, NOT MERGED.

        A field you do not name is untouched; a field you DO name overwrites what
        was there. For a dict field this means the whole map is swapped, never
        combined — send it WHOLE.

        The trap: granting one edge reads like a PATCH
        (`callable_by={"web":["read_stream"]}`) and behaves like a PUT — the
        structural baseline `{$self,$parent,$children}` is stamped once in
        `Agent.__init__` and nothing re-merges it here, so that call silently
        severs the agent from its own parent and children and still answers
        `{"updated": true}`. Send the map WHOLE.

        Measured 2026-08-24 (`live_subai`, the model): the model reasoned the
        danger out correctly — "if update_agent does a shallow merge ... I'd lose
        the baseline grants" — could not settle it from `reflect`, chose to
        establish it empirically, and lost the run to a repetition loop while
        trying. The semantics were right; the fact that they were undiscoverable
        is what cost the run.

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
        # name substrate wiring (`root_path`/`ctx`/`parent`); they used to be
        # written into the record and then REFUSED by the loader on the next
        # boot, so an accepted update made the workdir unbootable (2026-09-02).
        for k in ("handler_module", *sorted(RESERVED_RECORD_KEYS)):
            if k in payload:
                return bad(
                    "update_agent",
                    k,
                    "absent — handler_module is fixed at create_agent, and "
                    "root_path/ctx/parent name substrate wiring the record "
                    "may not carry",
                )
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
        # Without this, `update_agent` accepted anything: on 2026-08-02 an agent set
        # `ingress_rule="password_ingress:kernel-internal-transcripts"`, got `{"updated":
        # true}` back, and only discovered it was meaningless when the next call to
        # that agent failed. Accepting a value you cannot honour, and reporting
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
    # the LLM reads `ingress_rule: deny_ingress` (sealed) / `allow_all` (open) /
    # `password` (gated) and acts per the keystone readme (read-key == write-key);
    # ABSENT on an io leg ⇒ sealed-by-default. `root`/`file_bridge_id` show the binding.
    # The CALL gate belongs here too. It was left out, and the omission is not
    # cosmetic: `can_call`/`callable_by` decide whether a send is even attempted,
    # and with them absent an agent cannot read its OWN authority — it can only
    # discover it by trying. Measured 2026-08-24 (`live_subai`, the model): the
    # model wrote "the key question is: what are MY (ai's) can_call grants? The
    # full reflect didn't include can_call/callable_by", and across the day it
    # spent whole turns probing empirically — a throwaway `_probe_script` agent, a
    # `selfchild` "test create_agent on self", a `_probe_ai` scratch write — to
    # learn by experiment what one field states. This is not a secrecy boundary:
    # the gate ENFORCES, it does not hide, and everything surfaced here is already
    # discoverable by attempting the call. Hiding it only made discovery expensive.
    _POSTURE_KEYS = (
        "ingress_rule",
        "egress_rule",
        "auth",
        "root",
        "file_bridge_id",
        "can_call",
        "callable_by",
    )
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
