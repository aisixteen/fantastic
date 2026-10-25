"""Shared reflect-driven LLM-agent machinery for the ollama / responses /
vllm backends.

Two layers:

  1. MODULE-GLOBAL STATE, keyed by agent id. The per-backend FIFO locks and
     the queue / current-entry status state. These are keyed by the agent's
     id, so three backends loaded into ONE kernel share these dicts SAFELY —
     an ollama agent's id never collides with a vllm agent's id. `on_delete`
     drops an agent's rows. The PROVIDER is not state: it is built per turn
     from the live record (`_build_provider`), as rust builds it.

     There is NO backend-private task table: a `send` IS a kernel task (one
     `ps` row, `"<agent>:<n>"`), the generation runs INLINE in that dispatch,
     and the kernel's `cancel` aborts it. See `on_cancel` below.

  2. PER-BACKEND CONFIG, captured by a `Backend` dataclass. `build()` is a
     CLOSURE FACTORY: each backend calls it with its own Provider builder +
     constants and gets back a `(VERBS, handler)` pair whose verb closures
     capture THAT backend's `Backend`. So `sentence`, the provider builder
     and the error-message prefix are per-backend; the state above is
     shared. An ollama agent and a vllm agent in the same kernel each use
     their own provider and never clobber each other. Tool-calling is uniform + RAW for all of them
     (owned by this base class via `tool_parse`, not any native provider API).

`SEND_TIMEOUT` is read LIVE off this module, so a test's
`monkeypatch.setattr(core, "SEND_TIMEOUT", 0.05)` is honoured.

AI backlog (TODO):
  1. Multi-modal binary frames — image/audio payloads have no defined
     wire shape yet.
  2. Cost / token tracking — no per-turn cost report today.
"""

from __future__ import annotations

import asyncio
import inspect
import json
import re
import secrets
import time
import traceback
from collections.abc import Awaitable, Callable
from dataclasses import dataclass, field
from typing import Any

from ai_core.context import (
    BUDGET_FLOOR,
    ProjectionCtx,
    budget,
    estimate_one,
    estimate_tokens,
    output_reserve,
    resolve_context_window,
)
from ai_core import caching
from ai_core import prompt_file
from ai_core import embed as _embed_mod
from ai_core.provider import ProviderError
from ai_core.folds import anchors_of, is_fold, member_anchor
from ai_core.folds import expand_ids as expand_fold_ids
from ai_core.strategies import DEFAULT_STRATEGY, get_strategy, strategy_names
from ai_core.strategies.base import NOTICE_ENVELOPE_RESERVE
from ai_core.strategies.fold import (
    FoldSummaryError,
    append_fold,
    summarize_span,
    summary_title,
)

# TWO live consumers. The agentic LOOP is still emergent — the backend does ONE
# provider pass per send and never iterates — but `_route_reply` DOES parse this
# pass's output and dispatch what it names, so the parser is load-bearing on the
# routing path, not just a read-model. `_derive_reaction` is the other
# consumer, scanning the durable transcript for a recall/persist reaction.
from ai_core.call_check import check as check_calls
from ai_core.call_check import repair as repair_calls
from ai_core.stream_parts import PartSplitter, cut_marker, stop_rows
from ai_core.tool_parse import (
    _ROUTING,
    extract_tool_calls,
    malformed_calls,
    terminated_calls,
    render_reply,
)
from kernel import current_origin, current_task
from kernel._refusal import BAD_ARG, LOCKED, UNHANDLED, refusal
from kernel._refusal import bad as bad_arg
from ai_core.constants import (  # noqa: F401 — re-exported seams
    DEFAULT_CLIENT_ID,
    MAX_CALL_DEPTH,
    MAX_SILENT_TURNS,
    RECALL_EVERY,
    RECENT_N,
    RECENT_N_RANGE,
    SEND_TIMEOUT,
    TRIGGER,
)

# The first transport retry waits this many seconds, the next twice that: 2 s, 4 s, 8 s
# (#198 part 2). Read live, so a test can shorten it.
RETRY_BACKOFF_BASE = 2.0

# ─── shared module-global state (keyed by agent id) ─────────────

_locks: dict[str, asyncio.Lock] = {}

# Structural state for the `status` verb + status events. One channel,
# one source of truth — UI subscribes to `status` events for live phase
# transitions and calls the `status` verb to rebuild after page reload.
#
# _queue[backend_id]: pending entries waiting on the per-backend lock,
#   in arrival order. Each: {client_id, text, queued_at, send_id}.
# _current[backend_id]: the entry that holds the lock right now, or
#   absent. Carries the same fields plus `started_at`, `phase`, and
#   `task_id` — the KERNEL TASK this generation IS (`"<agent>:<n>"`, the
#   pid `cancel` addresses; `on_cancel` matches on it to recognise its own
#   turn). It is the ONLY handle on the in-flight generation, and it is the
#   kernel's, not ours. `_current[id]` present ⇔ a generation is in flight.
#   Phases: queued → thinking → streaming → tool_calling → thinking
#   → ... → done. Surfaces "still working" between tool calls so the
#   UI can show a pulse indicator (claude-code style).
_queue: dict[str, list[dict]] = {}
_current: dict[str, dict] = {}

# ONE CHAIN PER AGENT (#227), per agent id: `alive` while a pass or its routing
# is running, the `token` this chain's own turns carry, and the messages `held`
# for its next pass — each `(client_id, future)`, the future handed the answer of
# the pass that reads the message.
_chains: dict[str, dict] = {}

# The task each in-flight generation runs in (`asyncio.current_task()`), per agent
# id. Kept apart from `_current`, whose entry status snapshots serialise. `_stop`
# aborts it when the kernel's cancel does not find the turn's task.
_generations: dict[str, asyncio.Task] = {}

# Last context-overflow projection per agent — the PUBLIC summary surfaced by the
# `context_status` verb. {fired:bool, strategy?, kept_messages?, dropped_messages?, summarized?,
# too_small?}. Kept free of internal bookkeeping (see `_compaction_mark`).
_projection: dict[str, dict] = {}

# PRIVATE reaction cursor per agent — where the last compaction notice landed in the
# durable store + which client's thread it was, so `_derive_reaction` can scan from there
# for the model's recall/persist reaction. Internal; never surfaced in a verb reply.
_compaction_mark: dict[str, dict] = {}

# Where the last `_assemble` took its view from — "array" or "store…". Diagnostics only.
_view_source: dict[str, str] = {}
# Agents whose next turn must open with `context_recall` (enough folds landed).
_recall_due: set[str] = set()
_folds_since: dict[str, int] = {}
# INCARNATION per agent id, bumped on every boot. A reply dispatched under one
# incarnation must not become a live turn of the next: after a reboot the rebuilt agent
# would be woken by answers to calls it no longer remembers making, and go back to
# work already done instead of the new instruction.
_incarnation: dict[str, int] = {}
# TOKEN CALIBRATION per agent: the prompt size the endpoint REPORTED for the last pass
# divided by our own estimate of what we sent. The estimate is chars/4 over the
# rendered messages; a real tokenizer disagrees with it, per model and per language.
# The budget is measured in ESTIMATE units scaled by this ratio, so what triggers a
# compaction is what the window really holds. 1.0 until an endpoint reports usage.
_calib: dict[str, float] = {}
_reported: dict[str, int] = {}

# ─── shared constants ───────────────────────────────────────────


# ─── per-backend config (captured by the verb closures) ─────────


@dataclass
class Backend:
    """Per-backend config bound into the verb closures by `build()`."""

    sentence: str
    make_provider: Callable[[str, Any], Any]  # (id, kernel) -> Provider; sync OR async
    name: str  # error-message prefix, e.g. "ollama_backend"
    extra_verbs: dict = field(default_factory=dict)
    reflect_extra: Callable[[str, Any], Awaitable[dict] | dict] | None = None
    # THE CAPABILITY: this backend can run the provider's own cache markers
    # (caching=provider_cache). A backend that cannot refuses a turn in that mode.
    # Only anthropic declares it (task B).
    provider_cache: bool = False


# ─── status state helpers ───────────────────────────────────────


def _new_send_id() -> str:
    """Opaque id for a single user submission. Travels through every
    status event of that submission so the UI can correlate phases."""
    return secrets.token_urlsafe(8)


def _enqueue(self_id: str, entry: dict) -> int:
    """Append entry to the per-backend FIFO. Returns the position
    (1-based: 1 means front of line / next to acquire lock)."""
    q = _queue.setdefault(self_id, [])
    q.append(entry)
    return len(q)


def _dequeue_send(self_id: str, send_id: str) -> dict | None:
    """Remove the entry with this send_id from the queue, return it.
    Returns None if not found (already dequeued / never enqueued)."""
    q = _queue.get(self_id, [])
    for i, e in enumerate(q):
        if e.get("send_id") == send_id:
            return q.pop(i)
    return None


def _set_current(self_id: str, entry: dict) -> None:
    """Promote a queued entry to `_current` — called once we hold the
    backend lock. Adds started_at, phase=thinking, empty text_so_far, and
    `task_id`: the kernel task this generation runs as. That id is how
    `cancel` reaches the turn and how `on_cancel` knows the killed task is
    OURS (not a `history`/`reflect` dispatch on the same agent)."""
    _current[self_id] = {
        **entry,
        "started_at": time.time(),
        "phase": "thinking",
        "text_so_far": "",
        "task_id": current_task(),
    }
    task = asyncio.current_task()
    if task is not None:
        _generations[self_id] = task


def _clear_current(self_id: str) -> None:
    _current.pop(self_id, None)
    _generations.pop(self_id, None)


def _chain_of(self_id: str) -> dict:
    """The agent's chain (#227) — created on first use."""
    return _chains.setdefault(self_id, {"alive": False, "token": "", "held": []})


def _answer_held(held: list, answer: dict) -> None:
    """Hand each held sender `answer` under its own client_id (#227)."""
    for cid, fut in held:
        if not fut.done():
            fut.set_result({**answer, "client_id": cid})


def _end_chain(self_id: str) -> None:
    """STOP MEANS STOP (#227): the chain ends and its held messages leave the wake
    list (their rows stay stored); nothing is woken. Each held send returns the
    cancelled shape."""
    chain = _chain_of(self_id)
    chain["alive"] = False
    chain["token"] = ""
    held, chain["held"] = chain["held"], []
    _answer_held(held, {"held": True, "cancelled": True})


def _reset_chain(self_id: str, why: str) -> None:
    """The chain is over for a reason that is not a stop (delete, shutdown, boot,
    context clear, a wake that could not run): every held sender is answered with
    `why` instead of waiting out the send ceiling; the chain is dead and its token
    is dropped. Rust: `ChainState::reset`."""
    chain = _chain_of(self_id)
    held, chain["held"] = chain["held"], []
    chain["alive"] = False
    chain["token"] = ""
    _answer_held(held, {"error": why, "held": True})


#: The wake turn a chain that ended with held messages gets (#227).
_HELD_WAKE = (
    "[{n} message(s) arrived while you were working — they are above. Answer them.]"
)


def _chain_ended(kernel, self_id: str, system_prompt: str | None) -> None:
    """ONE CHAIN PER AGENT (#227): the chain has ended. Messages held while it ran
    are answered by ONE wake turn (their senders stay held: the wake pass takes and
    answers them); with none held, the chain is no longer alive."""
    chain = _chain_of(self_id)
    held = [cid for cid, _ in chain["held"]]
    if not held:
        chain["alive"] = False
        return
    _deliver_soon(
        kernel,
        self_id,
        _HELD_WAKE.format(n=len(held)),
        held[0],
        system_prompt,
        notice=True,
    )
    # WAKE WATCHDOG: a wake that never gets the lock must not leave its senders
    # waiting forever. After the send ceiling the senders this wake was for, if no
    # pass has taken them, are answered with an error.
    watched = list(chain["held"])

    async def _watch() -> None:
        await asyncio.sleep(SEND_TIMEOUT)
        c = _chain_of(self_id)
        expired = [h for h in c["held"] if any(h is w for w in watched)]
        if not expired:
            return
        c["held"] = [h for h in c["held"] if not any(h is e for e in expired)]
        _answer_held(
            expired,
            {
                "error": "send: no turn read this message within the send ceiling",
                "held": True,
            },
        )
        if not c["held"]:
            c["alive"] = False
            c["token"] = ""

    _ctx(kernel).spawn_task(self_id, "chain_wake_watch", _watch())


def _push_held_or_wake(
    kernel, self_id: str, client_id: str, fut, system_prompt: str | None
) -> None:
    """ONE CHAIN PER AGENT (#227, Rust #330): hold a message on the chain. The
    chain may have ended while the message's row was being stored (the held send
    read `alive` before that await): the push and the `alive` read are one step
    with no await between them, and a message pushed onto a dead chain revives it
    and wakes it, or no pass would ever read the message."""
    chain = _chain_of(self_id)
    chain["held"].append((client_id, fut))
    if not chain["alive"]:
        chain["alive"] = True
        if not chain["token"]:
            chain["token"] = _new_send_id()
        _chain_ended(kernel, self_id, system_prompt)


def _take_current_for_cancel(self_id: str, task_id: str) -> dict | None:
    """Hand the in-flight entry over to `on_cancel` — but ONLY if `task_id`
    names the generation itself. Clears `_current` (so `status`/`reflect` stop
    reporting a phantom generation the instant the kernel aborts the task) and
    returns the entry so the turn can be closed out for its client. None when
    the cancelled task is some OTHER dispatch on this agent."""
    cur = _current.get(self_id)
    if cur is None or cur.get("task_id") != task_id:
        return None
    return _current.pop(self_id)


def _redact_text(d: dict | None) -> dict | None:
    """Return a copy of a current/queue entry with text-bearing fields
    stripped. Used to surface the existence of someone else's in-flight
    work to a caller without leaking content. `task_id`, the streaming `parts`
    (the live splitter + publisher) and `stopped_by` are bookkeeping, not status —
    they stay out of the snapshot (the live table is `ps`'s job)."""
    if d is None:
        return None
    return {
        k: v
        for k, v in d.items()
        if k
        not in ("text", "text_so_far", "last_tool", "task_id", "parts", "stopped_by")
    }


def _status_snapshot(self_id: str, requesting_client_id: str | None) -> dict:
    """Build the response for the `status` verb.

    With requesting_client_id: full text only for that caller's entries;
    other clients collapse to `others_pending` count. Without it: text
    is redacted everywhere — useful for headless inspection without
    leaking browser content.
    """
    cur = _current.get(self_id)
    queue = list(_queue.get(self_id, []))
    if requesting_client_id:
        is_mine = bool(cur and cur.get("client_id") == requesting_client_id)
        cur_out: dict | None
        if cur is None:
            cur_out = None
        else:
            cur_out = {
                "phase": cur.get("phase"),
                "send_id": cur.get("send_id"),
                "started_at": cur.get("started_at"),
                "elapsed": time.time() - cur.get("started_at", time.time()),
                "is_mine": is_mine,
            }
            if is_mine:
                cur_out["text"] = cur.get("text", "")
                cur_out["text_so_far"] = cur.get("text_so_far", "")
                if cur.get("last_tool") is not None:
                    cur_out["last_tool"] = cur["last_tool"]
        mine_pending = [
            {
                "send_id": e["send_id"],
                "text": e.get("text", ""),
                "queued_at": e.get("queued_at"),
            }
            for e in queue
            if e.get("client_id") == requesting_client_id
        ]
        others_pending = sum(
            1 for e in queue if e.get("client_id") != requesting_client_id
        )
    else:
        cur_out = _redact_text(cur)
        if cur_out is not None:
            cur_out["elapsed"] = time.time() - cur.get("started_at", time.time())
            cur_out["is_mine"] = False
        mine_pending = []
        others_pending = len(queue)
    return {
        "source": self_id,
        "client_id": requesting_client_id,
        "generating": cur is not None,
        "current": cur_out,
        "mine_pending": mine_pending,
        "others_pending": others_pending,
    }


def _lock_for(self_id: str) -> asyncio.Lock:
    """Per-backend FIFO serializer: only one `_send` runs at a time per
    agent. Concurrent callers (cli, browser tabs) wait their turn."""
    if self_id not in _locks:
        _locks[self_id] = asyncio.Lock()
    return _locks[self_id]


# NOTE: providers NEVER receive a tools array. This backend is a bare, ONE-PASS
# completion: it does not parse tool calls and does not loop. Any tool convention
# is the CALLER's — an authored agent teaches it in the prompt (system_prompt),
# reads the completion, dispatches, and feeds the result back as the next turn.


def _render_for_model(messages: list[dict]) -> list[dict]:
    """Project the message list to what the model actually reads — its role and
    its text; every other key is stripped. The cache flags travel with their message."""
    out = []
    for m in messages:
        r = {"role": m.get("role"), "content": m.get("content", "")}
        for flag in ("cache_primer", "cache_fold"):
            if m.get(flag):
                r[flag] = True
        out.append(r)
    return out


# ─── persistence (the KERNEL ROW STORE — this bundle owns no files) ─────────


def _as_list(v) -> list:
    """A model-written field as a list: a list as is, a scalar wrapped, falsy empty."""
    if not v:
        return []
    return list(v) if isinstance(v, (list, tuple)) else [v]


def _safe_client(client_id: str) -> str:
    """Trim and sanitize a client id so it's safe as a filename suffix.
    Spaces / slashes / weirdness collapse to underscores."""
    raw = (
        client_id
        if isinstance(client_id, str)
        else (str(client_id) if client_id else "")
    )
    s = (raw or DEFAULT_CLIENT_ID).strip() or DEFAULT_CLIENT_ID
    return "".join(c if (c.isalnum() or c in "._-") else "_" for c in s)


def _ctx(kernel):
    """The kernel behind this agent. A handler's `kernel` is the Agent; `.ctx` is the
    Kernel that holds the row store. Test fakes may hand the store object directly."""
    return getattr(kernel, "ctx", kernel)


def _mint_row_id(self_id: str, kernel) -> int:
    """ONE ID SPACE. The kernel's counter is the only allocator: a row id, a pid, a
    fold, a body all take the next int, so one number names one thing everywhere."""
    return _ctx(kernel).mint_id()


def _as_id(x) -> int | None:
    """A message id as an int — an int, or its decimal string (a model quotes what it
    copied out of a tag). Anything else is not an id."""
    if isinstance(x, bool):
        return None
    if isinstance(x, int):
        return x if x > 0 else None
    # ASCII digits only — `'²'.isdigit()` is True and `int('²')` raises.
    if isinstance(x, str) and x.strip().isascii() and x.strip().isdigit():
        return int(x.strip())
    return None


# EVERY ROW HAS AN ADDRESS. `id` is `<agent>:<hex>` — the same allocator the kernel uses
# for pids, so calls and rows share one id space. `parent` is CAUSALITY (the row this one
# responds to); a thread is the parent chain, derived when needed. `reply_to` on a send
# is DELIVERY and is left alone.
# `↑<id>` stops at whitespace OR the tag's closing `]`: a tag that ENDS on its
# `↑` segment (`[reply to - · 5 reflect · ↑360]`, no batch suffix) would otherwise
# capture `360]`, which is no id, and the reply row would lose its parent.
_TAG_LINKS = re.compile(r"^\[reply to \S+ · [^\]]*?↑([^\s\]]+)[^\]]*\]")


def _notice_kind(self_id: str) -> dict:
    """`{"kind": "notice"}` when the turn in flight answers this layer's own text
    (see `_deliver`), else nothing — the row keeps its role as its kind."""
    return {"kind": "notice"} if (_current.get(self_id) or {}).get("notice") else {}


def _row_ids_for_incoming(self_id: str, text: str, kernel) -> tuple[int, int | None]:
    """(id, parent) for an incoming turn. A delivered reply turn names the assistant
    turn that made the calls (`↑`) on its first tag; anything else is a ROOT — a fresh
    message from a person or a peer."""
    rid = _mint_row_id(self_id, kernel)
    m = _TAG_LINKS.match(str(text or ""))
    return rid, (_as_id(m.group(1)) if m else None)


def _reply_tag(pid, target, verb, title, message_id, n, m_total, batch) -> str:
    """The one line a reply is known by, forever — in the durable store, in `recall`
    pages, and as the stub a stripped body leaves behind:
        [reply to <pid> · <target> <verb> · "<title>" · ↑<message> · N of M in batch <b>]
    Optional segments are appended in that order; `_REPLY_TAG` absorbs them all."""
    tag = f"[reply to {pid} · {target} {verb}"
    if title:
        tag += f' · "{title}"'
    if message_id:
        tag += f" · ↑{message_id}"
    if m_total > 1:
        tag += f" · {n} of {m_total} in batch {batch}"
    return tag + "]"


def _res_of(row: dict) -> str:
    """The resolution a rendered row is at: a fold, a stripped stub, or the row itself."""
    if row.get("kind") == "fold":
        return "fold"
    c = row.get("content")
    if isinstance(c, str) and "(stripped:" in c and c.startswith("[reply to "):
        return "stub"
    return "full"


# ─── the MEMORY STORE (this bundle owns no rows and no files) ──────────
#
# An AI agent's conversation lives in the MEMORY STORE its record names
# (`memory_store_id`) — a separate agent (`ram_store`, or a durable one) that keys
# every row by WHO WROTE IT. So this agent reads and writes ITS OWN rows only, by
# plain gated sends: the creator wires `can_call` to the store and the store's
# `callable_by` to this agent, explicitly. No store named ⇒ the agent fails loud at
# boot and at its first turn; a store that refuses or is unreachable mid-turn ⇒ the
# turn fails fast. Nothing falls back to RAM.
#
# THE HEAD CACHE. The rows from the last `clear` on (the head) are read ONCE, on the
# first turn after boot, and then kept in RAM WRITE-THROUGH: every row is appended
# to the store FIRST and enters the cache only when the store took it, so the cache
# never holds what the store does not. Reads that reach behind the head (recall,
# grep, `history {fold}`) go to the store.
#
# BODIES ARE NEVER PERSISTED. A reply too large to inline is kept in RAM under its
# pid for this incarnation only; what the store keeps is the reply's `origin` line
# (how to get it again). After a restart there are no bodies — the model re-reads.


def _mem(kernel) -> dict:
    """This bundle's RAM for ONE kernel — the head caches, the bodies held this run,
    the row watchers — kept ON the kernel's context, so two
    kernels in one process (tests, a multi-kernel host) never share them."""
    ctx = _ctx(kernel)
    m = getattr(ctx, "_ai_memory", None)
    if m is None:
        m = {"heads": {}, "bodies": {}, "watchers": {}}
        ctx._ai_memory = m
    return m


class MemoryStoreError(RuntimeError):
    """The agent's memory store is missing, refused, or unreachable — fail fast."""


def _store_of(self_id: str, kernel) -> str:
    rec = (kernel.get(self_id) if hasattr(kernel, "get") else None) or {}
    sid = rec.get("memory_store_id")
    if not isinstance(sid, str) or not sid:
        raise MemoryStoreError(
            f"{self_id}: no `memory_store_id` — an AI agent keeps its conversation "
            "in a memory store and runs without none. Create one (e.g. a ram_store), "
            f"set memory_store_id on {self_id}, and grant {self_id} can_call on it "
            "AND the store callable_by for it"
        )
    return sid


async def _store_call(self_id: str, kernel, payload: dict) -> dict:
    """ONE gated send to this agent's memory store, as this agent — this code runs
    in its own dispatch or a task it spawned, so the send is its own (the store
    keys a row by its writer). A refusal or a failed dispatch is a
    `MemoryStoreError` — the turn fails fast."""
    sid = _store_of(self_id, kernel)
    out = (await kernel.send([{"target": sid, "payload": payload}]))[0]
    res = out.get("result")
    if out.get("status") != "completed" or (isinstance(res, dict) and res.get("error")):
        err = res.get("error") if isinstance(res, dict) else out.get("status")
        hint = res.get("hint") if isinstance(res, dict) else None
        raise MemoryStoreError(
            f"{self_id}: memory store {sid!r} {payload.get('type')} failed: {err}"
            + (f" (hint: {hint})" if hint else "")
        )
    return res if isinstance(res, dict) else {}


async def _purge(self_id: str, kernel) -> None:
    """Purge a deleted agent's rows from its memory store (`on_delete`), unless the
    delete asked `keep_memory:true`. ONE gated `prune` of every row the agent owns
    (no `keep`: the fold, view and bookkeeping rows go too; fold refs and the text
    index follow in the store). Delete is not refusable: a store that is missing,
    already gone or refuses is logged and the cascade goes on."""
    from kernel._agent import keep_memory

    if keep_memory(kernel, self_id):
        return
    try:
        _store_of(self_id, kernel)
    except MemoryStoreError:
        return
    try:
        await _store_call(self_id, kernel, {"type": "prune", "before_id": 2**63 - 1})
    except Exception as e:
        print(f"  [cascade] {self_id} memory purge failed: {e}")


# The store holds this agent's conversation AND its view rows. The CONVERSATION a
# reader sees is everything but the view and the recorded fold anchors (an allow-list
# of kinds to HIDE: a kind forgotten here lands in the model's prompt).
_BOOKKEEPING = frozenset({"view", "anchors"})


async def _head(self_id: str, kernel) -> list[dict]:
    """The head cache — loaded from the store on first use (lazy boot)."""
    heads = _mem(kernel)["heads"]
    h = heads.get(self_id)
    if h is None:
        res = await _store_call(
            self_id, kernel, {"type": "read", "from_last_kind": "clear"}
        )
        h = heads[self_id] = [{**r, "agent": self_id} for r in res.get("rows") or []]
    return h


_TURN_ROLES = frozenset({"user", "assistant", "system"})


async def _refresh_head(self_id: str, kernel) -> list[dict]:
    """THE STORE IS THE TRUTH; the head cache must never diverge from it. The cache
    holds this agent's own writes (write-through), but a row may land under this key
    outside the cache too — a program running as this agent, a store fed from outside. So each read of
    the head first reads every row AFTER the newest cached id and merges it by id (a
    row the agent wrote meanwhile is already there). A `clear` among them restarts the
    head. One small read per turn; the store stays passive (no push)."""
    heads = _mem(kernel)["heads"]
    h = heads.get(self_id)
    ids = [r["id"] for r in h or () if isinstance(r.get("id"), int)]
    if h is None or not ids:
        heads.pop(self_id, None)
        return await _head(self_id, kernel)
    res = await _store_call(self_id, kernel, {"type": "read", "after": max(ids)})
    seen = set(ids)
    for r in res.get("rows") or []:
        if r.get("id") in seen:
            continue
        if r.get("kind") == "clear":
            h.clear()
        h.append({**r, "agent": self_id})
    return h


async def _load_history(self_id: str, kernel, *, behind: bool = False) -> list[dict]:
    """The agent's conversation, in order. AGENT ID IS CONVERSATION ID: a second
    conversation is a second agent.

    A CLEAR IS A BOUNDARY, NOT A DELETION (`context_clear`): the head starts at the
    LAST `clear` row. `behind=True` (recall, grep, `history {fold}` — the verbs that
    reach what is not in view) reads the WHOLE conversation from the store."""
    if behind:
        res = await _store_call(self_id, kernel, {"type": "read"})
        rows = [{**r, "agent": self_id} for r in res.get("rows") or []]
    else:
        rows = list(await _refresh_head(self_id, kernel))
    # THE CONVERSATION IS ITS TURNS — rows with a chat `role`. The agent may also
    # `append` a row of its own straight to its store (a note to itself); that row is
    # memory, not a turn: it stays in the store, `grep` finds it, the view never
    # renders it as a message it cannot be.
    rows = [
        r
        for r in rows
        if r.get("kind") not in _BOOKKEEPING and r.get("role") in _TURN_ROLES
    ]
    return rows


async def _save_history(self_id: str, kernel, new_turns: list[dict]) -> None:
    """APPEND rows to the memory store, then to the head cache (write-through). A
    row without an id takes the kernel's next one. A `clear` restarts the head."""
    rows = []
    for t in new_turns:
        r = {k: v for k, v in t.items() if k != "agent"}
        if r.get("id") is None:
            r["id"] = _mint_row_id(self_id, kernel)
        rows.append(r)
    if not rows:
        return
    await _store_call(self_id, kernel, {"type": "append", "rows": rows})
    head = await _head(self_id, kernel) if self_id in _mem(kernel)["heads"] else None
    for r in rows:
        stored = {**r, "agent": self_id}
        if head is not None:
            if r.get("kind") == "clear":
                head.clear()
            head.append(stored)
        if r.get("kind") not in _BOOKKEEPING:
            _announce_row(self_id, kernel, stored)


async def _store_failed_turn(
    self_id: str, kernel, text: str, notice: bool, reason: str
) -> str | None:
    """A turn whose pass FAILED stores its own rows (#177): the incoming message and
    an error row naming why, so the model reads the failure on its next turn and the
    operator sees it in the history. Nothing the turn meant to send was dispatched.
    Returns None when stored, else why the store refused; never raises."""
    try:
        in_id, in_parent = _row_ids_for_incoming(self_id, text, kernel)
        await _save_history(
            self_id,
            kernel,
            [
                {
                    "role": "user",
                    "content": text,
                    "id": in_id,
                    "parent": in_parent,
                    **({"kind": "notice"} if notice else {}),
                },
                {
                    "role": "user",
                    "kind": "error",
                    "content": f"[turn failed] {reason}. No answer was stored, and "
                    "nothing this turn meant to send was dispatched.",
                    "id": _mint_row_id(self_id, kernel),
                    "parent": in_id,
                },
            ],
        )
    except Exception as e:
        return str(e)
    return None


def _with_store_outcome(message: str, stored_error: str | None) -> str:
    """The failure a turn reports; when its error row could not be stored, it says so."""
    if stored_error is None:
        return message
    return f"{message}; the turn's error row could not be stored: {stored_error}"


async def _store_reply(self_id: str, kernel, pid, body: str) -> bool:
    """A reply too big to hand the model as a turn: kept IN RAM under its pid (never
    persisted); the model is handed its digest + `origin` line."""
    p = _as_id(pid)
    if p is None:
        return False
    _mem(kernel)["bodies"].setdefault(self_id, {})[p] = body
    return True


async def _load_reply(self_id: str, kernel, pid, offset: int, length: int):
    """One page of a body held in RAM: (text, next_offset, eof, total) or None —
    None after a restart: bodies are never persisted."""
    b = _mem(kernel)["bodies"].get(self_id, {}).get(_as_id(pid) or -1)
    if b is None:
        return None
    chunk = b[offset : offset + max(1, length)]
    nxt = offset + len(chunk)
    return chunk, nxt, nxt >= len(b), len(b)


# ─── the row event ───────────────────────────────────────────────

# ROW EVENT WATCHERS — the kernel holds no rows, so this agent announces its own:
# `_mem(kernel)["watchers"][agent] = {watcher id: wants bodies}`, registered by the
# `watch` verb (the agent's own gate decides who may call it); a watcher whose inbox
# is gone is dropped.


def _row_digest(row: dict) -> dict:
    c = row.get("content")
    text = c if isinstance(c, str) else json.dumps(c, default=str)
    return {
        "id": row.get("id"),
        "kind": row.get("kind") or row.get("role"),
        "role": row.get("role"),
        "bytes": len(text or ""),
    }


def _announce_row(self_id: str, kernel, row: dict) -> None:
    """Put the row event in every watcher's inbox: a content-free digest, plus the
    content for a `body:true` watcher."""
    mem = _mem(kernel)
    ws = mem["watchers"].get(self_id)
    if not ws:
        return
    inboxes = getattr(_ctx(kernel), "inboxes", None) or {}
    for wid, body in list(ws.items()):
        q = inboxes.get(wid)
        if q is None:
            ws.pop(wid, None)
            continue
        ev = {"type": "row", "source": self_id, "row": _row_digest(row)}
        if body:
            ev["row"]["content"] = row.get("content")
        try:
            q.put_nowait(ev)
        except asyncio.QueueFull:
            try:
                q.get_nowait()
                q.put_nowait(ev)
            except (asyncio.QueueEmpty, asyncio.QueueFull):
                pass


def _inline_cap(rec: dict) -> int:
    """Tokens a reply may occupy INLINE in the model's next turn. Above this it is
    stored and digested. An eighth of the input budget: a 12k window inlines ~1.4k
    tokens (a reflect, an error, a short read) and digests a 6.8KB file body."""
    return max(BUDGET_FLOOR, budget(rec) // 8)


def _fit_reply(
    items: list, cap_tokens: int, limit: int | None = None
) -> tuple[list, bool]:
    """THE CAP INVARIANT. Every verb an agent calls on itself must answer UNDER the
    inline cap, or its answer is stored and digested and the model pulls its own reply.
    Keep items from the front while the rendered reply fits.
    Returns (kept, truncated)."""
    # MEASURED ON THE SERIALIZED FORM, with headroom for the envelope the verb wraps
    # around these items (`total`, `next`, …). JSON escaping inflates raw text by up to
    # ~30% on newline-heavy bodies; the margin is what keeps "fits" true at the seam.
    cap = int(cap_tokens * 0.8)
    kept: list = []
    for it in items:
        if limit is not None and len(kept) >= limit:
            return kept, True
        trial = kept + [it]
        if (
            estimate_one({"role": "user", "content": json.dumps(trial, default=str)})
            > cap
        ):
            return kept, True
        kept = trial
    return kept, False


async def _save_view(
    self_id: str, kernel, ids: list[dict], *, budget_: int, why: str
) -> bool:
    """The agent's STATE: the ordered ids it sees, each at a resolution — a `view`
    row in its memory store; the newest one in the head is current."""
    await _save_history(
        self_id,
        kernel,
        [
            {
                "kind": "view",
                "v": 1,
                "ts": time.time(),
                "budget": budget_,
                "why": why,
                "ids": ids,
            }
        ],
    )
    return True


async def _load_view(self_id: str, kernel) -> dict | None:
    for r in reversed(await _head(self_id, kernel)):
        if r.get("kind") == "view":
            return r if isinstance(r.get("ids"), list) else None
    return None


def _apply_res(row: dict, res: str) -> dict:
    """Render a stored row at the resolution the view array asks for."""
    if res == "stub":
        from ai_core.strategies.strip import (
            _blocks,
            _stub,
        )  # lazy: package import order

        c = row.get("content")
        if isinstance(c, str) and c.startswith("[reply to "):
            return {**row, "content": "\n\n".join(_stub(b) for b in _blocks(c))}
    return row


def required_meta(rec: dict, key: str, backend: str) -> str:
    """A connector field the record MUST name — an endpoint or a model is never
    guessed. Raises, naming the field, so the turn fails loud."""
    v = rec.get(key)
    if not isinstance(v, str) or not v.strip():
        raise ValueError(
            f"{backend}: `{key}` is required on the agent record — nothing is "
            f"guessed (update_agent <id> {key}=…)"
        )
    return v


async def _build_provider(self_id: str, kernel, cfg: Backend):
    """The provider for ONE turn, built from the LIVE record by the backend's
    `make_provider` (sync OR async). Never cached: a provider captures endpoint,
    model and sampling when it is built, so a cached one would keep posting to an
    endpoint `update_agent` had moved the agent away from, and a deleted id
    re-created later would inherit it. A record missing what the provider needs raises, naming the field."""
    prov = cfg.make_provider(self_id, kernel)
    if inspect.isawaitable(prov):
        prov = await prov
    return prov


# ─── prompt assembly (Phase 1) ──────────────────────────────────


def _render_reflect(d: dict) -> str:
    d = dict(d)
    sentence = d.pop("sentence", "")

    # No JSON on the model path: a list of scalars is comma-joined, anything
    # nested is the reply notation collapsed onto the line.
    def _one(v) -> str:
        if isinstance(v, str):
            return v
        if isinstance(v, list) and all(not isinstance(x, (dict, list)) for x in v):
            return ",".join(str(x) for x in v)
        return " ".join(render_reply(v).split())

    fields = "  ".join(f"{k}={_one(v)}" for k, v in d.items())
    return f"{sentence}  {fields}".strip()


_CONTEXT_HOWTO = """## Context window
Your view is finite: when it overflows, older turns are compacted and a
`[context-notice]` says so; the durable transcript stays whole. Send your OWN id
`<type>recall</type>` (`<query>`, `<limit>`) to page dropped turns back, and
`<type>context_status</type>` for your budget. Facts that must outlive this window
(names, decisions, preferences) belong in a memory agent.
"""


def _clean_title(t) -> str | None:
    """A title lives INSIDE the reply tag, so it may not contain the one character that
    closes it, nor a newline that would end the tag line. Capped: it is a sentence."""
    if not isinstance(t, str) or not t.strip():
        return None
    return re.sub(r"[\]\r\n]+", " ", t).strip()[:140] or None


_TITLES_HOWTO = """## Titling your calls
Every call carries a one-sentence `<title>` beside its `<target_id>`:
<call name="send">
<target_id>notes</target_id>
<title>read filler_2 — looking for the audit marker</title>
<type>read</type>
<path>filler_2.txt</path>
</call>
The title rides on the reply's tag and is what remains once the reply BODY is stripped
from your view (bodies stay fetchable by pid). Title the INTENT ("read filler_2 — looking
for the audit marker"), and after a reply SAY WHAT IT TOLD YOU in one sentence — the body
will go; your sentence will not.
"""


_ASYNC_HOWTO = """## Replies are rows, not turns
A reply is DATA, not a message to answer. A LARGE reply arrives as a one-line digest —
tag, your title, size, and its ORIGIN (how to get it again):
[reply to <pid> · notes read · "survey filler_2"]
(settled, 6812 bytes — NOT inlined, held in memory only until this agent restarts. origin: notes notes.md lines 1-200 v3f2a — read {path, range}. To find a line in it: send yourself `<type>grep</type> <query>substring</query> <ids><item>pid</item></ids>`)
GREP FIRST: `<type>grep</type> <query>substring</query>` on yourself finds the line —
in a body held this run, a folded stretch, an old turn, a row shared with you — with
its id and line. BODIES ARE NOT KEPT: after a restart only the origin line remains, so
re-read or re-run what you still need; never answer from a body you cannot see.
FOLDS: when your window fills, finished stretches collapse into `[fold <id> · N refs]
"<title>"` plus a short summary. Nothing is deleted — `<type>history</type>
<fold>id</fold>` lists a fold's members, grep with the fold's id searches inside it.
After a fold, and on your first turn after a boot, `context_recall` fits the head of
your conversation to your budget and asks you what to keep in full; a `[recall …]` line
is your own earlier decision and names the LIVE task. You may call
`<type>context_recall</type>` on yourself any time. `<type>context_clear</type>` on yourself
starts a NEW THREAD: your view begins at the `[cleared …]` line; every earlier row is
that clear's member, reachable through grep, recall and `history fold`.
"""


_MEMORY_HOWTO = """## Your memory
Your conversation lives in your MEMORY STORE `{store}` — every row under YOUR key.
"""


_SEND_TO_HOWTO = """## Reaching other agents
Everything is an agent with a string id; `send` reaches any of them. A call is a
`<call>` element, one child tag per argument — no JSON, no quotes, no escaping:
<call name="send">
<target_id>notes</target_id>
<type>reflect</type>
</call>
`<target_id>` names the agent, `<type>` the verb, every other child is an argument. A
value is text as written (`true`, `12`, a path); a list is `<item>` children
(`<ids><item>12</item><item>15</item></ids>`); an object is child tags
(`<edits><item><path>a.py</path><hunks>2</hunks></item></edits>`); a map's keys are its
tags, `$` included (`<callable_by><$self>*</$self><web><item>read</item></web></callable_by>`).
`<reply_to>` picks
where the reply goes: you by default (your next message), another id hands it to them,
`null` discards it. `reflect` goes to the agent itself (`<target_id>runner</target_id>`)
and says what it IS (sentence, state, tree); how to CALL it is `docs` (below). Text outside calls is your message to whoever asked — keep it
brief, and take instructions literally. EMIT CALLS BEFORE YOU DELIBERATE: the moment you
know the next call, send it; never end a turn on "let me do X" without X in a `<call>`.

## Grants — every call is DENY-ALL
You may call only what your `can_call` (in your self block) grants: `{key: "*" | [verbs]}`,
a key being an agent id, `$self`, `$parent` or `$subtree` (every agent below you), `*`
every verb; the target must also admit you (its `callable_by`). Read `can_call` BEFORE
your first call and plan inside it — any other call is refused and costs a turn; never
retry a refusal. `docs` too is a grant: `docs` on the root (`kernel`).

## `<bytes>` is VERBATIM
A verb that takes a body (a file, a patch, a program) names it `bytes`: write the text
between `<bytes>` and `</bytes>` exactly as it should land — real newlines and quotes,
nothing escaped, no fence. The body ends at the first `</bytes>`.
<call name="send">
<target_id>notes</target_id>
<type>write</type>
<path>notes.md</path>
<bytes>line one
line two, with "quotes"
</bytes>
</call>

## How to call an agent — `docs`
<call name="send">
<target_id>kernel</target_id>
<type>docs</type>
<of>notes</of>
</call>
`docs` is a KERNEL verb: the target is always `kernel` and the agent you ask about goes
in `<of>` (an agent id — `<guid>|<agent>` for one on a connected kernel — a bundle by its
listed name, or `kernel` for create_agent, update_agent, ps, …); it answers one signature
per verb. It is the only verb that takes
`<of>` — every other verb, `reflect` included, goes to the agent itself. Add `<verb>write</verb> <tier>detailed</tier>` for that verb's
full text. Pull docs once per kind of agent, `detailed` only for a verb you are about to
use. A docs reply is TRANSIENT: after your next message it becomes a
one-line stub — act on it then, or ask again. Reflect only agents you are granted; to see just part of a tree, reflect with
`<show_subtrees><item>id</item></show_subtrees>` (only those branches) or
`<ignore_subtrees><item>id</item></ignore_subtrees>` (all but those, each shown as its
id and `pruned: <n>`).

## Several recipients
`<to><item>a</item><item>b</item></to>` in place of `<target_id>` sends each a copy
headed `[from <you> · to a, b · msg N]` (N = the sending turn, the same in every copy).
Answer a group with the SAME `<to>` (minus yourself) and `<re>N</re>`; fewer recipients
fork a private thread. Tell peers facts in words — your ids mean nothing to them.

## Turns
Only your ANSWER acts: a `<call>` in your thinking does not run; write it in your
answer, after thinking; every call in it runs. Thinking is stripped from history: put
conclusions in a call or your message. End every turn with a call, with `<call name="await"></call>`
(stop until the next message wakes you; what you wait for is in flight, not lost, and a
job from `exec start` wakes you with `job_done`; refused when nothing is pending) or with
`<call name="done"></call>` (work done). A turn with no call is reminded and rerolled;
turn after turn of none and the chain gives up, telling your caller you could not answer.

## Inquiries
When a decision is not yours to make, `send` whoever owns it an `inquiry` (`<text>`,
`<kind>text|single|multi|confirm</kind>`, `<options>`). It is accepted at once; the
answer arrives LATER as a message under that call's pid — a person reads it first, an
agent does its work first: keep working, or `<call name="await"></call>` for it. A human is an agent too —
find it among the agents your `can_call` grants. Asked one yourself (`[inquiry from …]`),
do the work it needs, then answer with
<call name="inquiry_answer"><answer>what you concluded</answer></call>
— it dispatches nothing, and nothing else counts as your answer.

## Replies
Every call runs under a pid; its reply comes back tagged `[reply to <pid> · <agent>
<verb>]`. The calls of one turn run CONCURRENTLY (no order — if B needs A, send B after
A's reply) and are answered TOGETHER in one later message, one block each (`N of M in
batch …`; M is complete). Messages queued earlier arrive first: an unseen reply is in
flight — never re-issue it; send `kernel` `<type>ps</type>` to check. A reply whose pid
you do not know was routed to you — use it. Per call: `<timeout>seconds</timeout>`
bounds it; `<exec_id>name</exec_id>` makes it idempotent (the same exec_id never runs
twice — you get the first outcome back; a new attempt needs a new name);
`<retries>N</retries>` lets the kernel re-run a failing call; `kernel`
`<type>ledger</type>` lists your executions and what became of them.
"""

# ─── reply routing ──────────────────────────────────────────────
#
# After ONE provider pass, its output goes where it was addressed.
#
#   the model's own <call>s — each names a target, a verb and `reply_to`. All are
#     dispatched, and each reply is delivered to its own `reply_to`. `reply_to:'self'` is
#     what makes a chain, and the chain continues ONLY because the model asked again.
#     No call ends the turn: stop is the default.
#
#   the caller's `send_to` — hands this pass's TEXT to another agent, once.
#
# Nothing here bounds how far the model goes. Every dispatch is a `ps` row and `cancel`
# kills a subtree, so a runaway is visible and killable exactly like a script agent's
# `while(true)`. Deciding how much thinking is allowed is not this module's call.
#
# Detached is the one hard requirement: `_lock_for` is a plain non-reentrant asyncio.Lock,
# so awaiting a self-addressed send from inside the holder deadlocks until `SEND_TIMEOUT`.
# Spawned, it queues behind the current turn.


def _deliver_soon(
    kernel, target, text, client_id, system_prompt, *, notice: bool = False
) -> None:
    born = _incarnation.get(target, 0)
    """Hand `text` to `target` as a fresh turn, WITHOUT waiting for that turn.

    THIS IS WHY IT MUST NOT BE AWAITED INLINE. A send to an AI agent takes its
    FIFO lock and does not return until that whole turn is done — model call,
    streaming, and the turn's own routing. Awaiting each delivery inside the
    per-call loop would make reply 2 of a 4-call turn wait for the entire
    conversation reply 1 started, and if the chain ended first it would never be
    delivered at all.

    Invisible to a unit test with a stub kernel: its turn costs no time, and at
    zero latency serial and concurrent delivery are indistinguishable.

    Registered with `spawn_task` so the work is a `ps` row and `cancel` reaches it —
    the same contract `exec_bridge` uses for a forked job. Fire-and-forget without
    that is the orphan bug.
    """
    # ONE PATH: no `create_task` fallback — it would turn a REAL spawn failure into
    # an unregistered orphan. A test fake that delivers answers `spawn_task` like
    # the kernel does.
    coro = _deliver(
        kernel, target, text, client_id, system_prompt, born=born, notice=notice
    )
    _ctx(kernel).spawn_task(target, "deliver_reply", coro)


def _watch_job(kernel, owner, exec_id, job_id, client_id, system_prompt) -> None:
    """Record an exec job `owner`'s own batch started (`exec start` answered running),
    and spawn ONE watcher on the owner for it (#306). The watcher waits on the exec
    agent's `status` until the job is no longer `running`/`stopping`, forgets the job,
    then wakes the owner once with `job_done {job_id, exit_code, tail}`. Rust:
    `watch_job` in route.rs. Only this batch's own replies are watched, so another
    agent's job never counts against this one."""
    _running_jobs.setdefault(owner, []).append((job_id, exec_id))
    _ctx(kernel).spawn_task(
        owner,
        "job_watch",
        _job_watch(kernel, owner, exec_id, job_id, client_id, system_prompt),
    )


def _job_field(result, key):
    """A field of the job a `status` or `tail` reply is about: the first of `jobs`, or
    the reply itself when the exec agent answers flat."""
    if not isinstance(result, dict):
        return None
    jobs = result.get("jobs")
    if isinstance(jobs, list) and jobs and isinstance(jobs[0], dict):
        if key in jobs[0]:
            return jobs[0][key]
    return result.get(key)


def _drop_job(owner, job_id, exec_id) -> None:
    """Forget one job; the owner's key goes with its last one."""
    jobs = _running_jobs.get(owner)
    if jobs is None:
        return
    if (job_id, exec_id) in jobs:
        jobs.remove((job_id, exec_id))
    if not jobs:
        _running_jobs.pop(owner, None)


async def _job_watch(kernel, owner, exec_id, job_id, client_id, system_prompt) -> None:
    last = None
    try:
        while True:
            out = await kernel.send(
                [
                    {
                        "target": exec_id,
                        "payload": {"type": "status", "job_id": job_id, "wait_s": 100},
                    }
                ]
            )
            last = out[0].get("result") if out else None
            # An error or unknown-job reply has no status, so it ends the wait too.
            if _job_field(last, "status") not in ("running", "stopping"):
                break
        t = await kernel.send(
            [
                {
                    "target": exec_id,
                    "payload": {"type": "tail", "job_id": job_id, "lines": 20},
                }
            ]
        )
        result = (t[0].get("result") if t else None) or {}
        lines = result.get("lines") if isinstance(result, dict) else None
        tail = (
            "\n".join(
                str(line["line"])
                for line in lines[-20:]
                if isinstance(line, dict) and "line" in line
            )
            if isinstance(lines, list)
            else ""
        )
        exit_code = _job_field(last, "exit_code")
    finally:
        # Forget the job BEFORE the wake (and on cancellation), so the woken turn's
        # `pending` no longer counts it.
        _drop_job(owner, job_id, exec_id)
    text = "job_done " + json.dumps(
        {"job_id": job_id, "exit_code": exit_code, "tail": tail}
    )
    _deliver_soon(kernel, owner, text, client_id, system_prompt, notice=False)


async def _deliver(
    kernel,
    target,
    text,
    client_id,
    system_prompt,
    *,
    born: int | None = None,
    notice: bool = False,
) -> None:
    """Hand `text` to `target` as a fresh turn. AWAITS the whole turn — call
    `_deliver_soon` unless you specifically need to block on it.

    A FAILED HANDOFF IS ANNOUNCED, not dropped. `_send` answers a turn it could
    not run with `{"error": ...}` rather than raising — a timeout, a provider
    transport failure, a refused gate. Discarding that reply would make a chain that
    died mid-flight look exactly like a model that chose to stop: no error anywhere,
    the last turn simply never happened. The first send's error is surfaced by its
    caller; every SUBSEQUENT turn's would be invisible.

    So the error is emitted on the target's own inbox, the same `status(phase=done,
    reason=error)` shape a failed direct send produces. Whoever watches that agent —
    a cli, a test harness, a panel — sees it. We do NOT raise: the dispatch that
    routed this reply is a detached task, and raising there kills a chain that may
    have other live branches."""
    payload = {"type": "send", "text": text, "client_id": client_id}
    # HARNESS TEXT, NOT A MESSAGE: a turn this layer writes to
    # its own agent — a reminder, a malformed-call report, an await refusal — is
    # stored as `kind: "notice"`, so a view can tell it from a person's message.
    # The model still reads it as a user turn: Qwen's template takes no system
    # message past the first. Honoured only on a send to self (see `_send`).
    if notice:
        payload["notice"] = True
    # ONE CHAIN PER AGENT (#227): a reply routed back to the agent whose chain is
    # running is that chain's own turn — it carries the chain token, so it is never
    # held. Harness self-turns (nudges, the held-message wake) are chain turns too.
    if notice or current_origin() == target:
        payload["chain"] = _chain_of(target)["token"]
    if born is not None and _incarnation.get(target, 0) != born:
        # A LATE REPLY TO A PREVIOUS INCARNATION. Kept as a ROW — it is real data —
        # but it does not wake a turn: the agent that made the call is gone, and the
        # one that replaced it is on other work. The TARGET writes it (a store keys
        # every row by its writer), so it rides a `send` marked `late`.
        payload["late"] = True
    if system_prompt is not None:
        payload["system_prompt"] = system_prompt
    out = (await kernel.send([{"target": target, "payload": payload}]))[0]
    reply = out["result"]
    # A failed or timed-out handoff is announced too — the outcome status is the
    # machine truth even when the result body carries no `error` of its own.
    failed = out["status"] in ("failed", "timed_out")
    if failed or (isinstance(reply, dict) and reply.get("error")):
        err = (
            reply.get("error")
            if isinstance(reply, dict) and reply.get("error")
            else out["status"]
        )
        if notice:
            _reset_chain(target, "send: the turn that was to read this message did not run")
        await _emit_status(kernel, target, client_id, "done", reason="error", error=err)


# ─── thinking: neither DISPATCHED nor PERSISTED ──────────────────
#
# A reasoning model streams two things. ollama labels them `message.thinking` /
# `message.content`; vLLM labels them `reasoning_content` / `content`. We must read
# BOTH — a turn spent entirely reasoning is otherwise an empty completion that
# silently ends the chain. But we must not KEEP both:
# feeding a reasoning model its own prior deliberation back is against every
# vendor's guidance, and it compounds — an early turn's long deliberation is
# re-sent in every later prompt, crowding out the task.
#
# Providers wrap reasoning in these markers; everything downstream reads the ANSWER
# (`without_thinking`): dispatch parses it and history persists it — a call written
# inside thinking is thinking. One convention, no provider learns
# anything about the tool protocol.
THINK_OPEN = "<think>"
THINK_CLOSE = "</think>"


# ─── stale whole-state reads: DEDUPED, not replayed ─────────────
#
# Reflect replies can be a large share of a conversation, many of them a REPEAT of a
# target already reflected. That bulk is what fills the window, and a full window is
# what triggers compaction, which puts the goal at risk. Trimming it attacks the
# cause, not the symptom.
#
# The rule is narrow on purpose. A reply may be dropped only when a LATER reply makes
# it strictly redundant, which requires the verb to return WHOLE state rather than an
# answer to a question:
#
#   reflect · list_agents · ps · keys              → whole-state. The newest reply IS
#       the truth; older copies describe a state that no longer exists and can only
#       mislead. Safe to drop.
#   read · list · status · get                       → PARAMETERISED. Two `read`s on the
#       same agent are different files; the tag carries no args, so deduping them would
#       silently delete content. NEVER dropped.
#
# This is dedup, NOT stripping: the newest reply stays verbatim, so the model keeps the
# facts (verbs, ids, tree) and has no reason to re-reflect — re-fetching what it lost is
# exactly the loop we are trying to avoid. Applied to the PROMPT view only; durable
# history keeps every turn, so the trace stays complete.
_WHOLE_STATE_READS = frozenset({"reflect", "list_agents", "ps", "keys"})
# `[^\]]*` absorbs the optional ` · N of M in batch <token>` suffix.
#
# The verb group is `[^\]\s]+`, NOT `\S+`. With `\S+` the group is greedy and `]` is
# not excluded from it, so on `[reply to x · notes reflect]\n{"verbs":["read"]}` it took
# `reflect]` and let `[^\]]*\]` close on a bracket INSIDE THE BODY. The verb then read
# `reflect]`, which is in no vocabulary — so `dedupe_stale_reads` would match nothing
# for ANY reply whose body contains a `]`. Reflect bodies are rendered with
# `json.dumps`, so every list-valued field puts one there.
_REPLY_TAG = re.compile(r"^\[reply to \S+ · (\S+) ([^\]\s]+)[^\]]*\]")
# A delivered turn carries ONE OR MORE reply blocks — a batch arrives whole, one
# block per call, joined by a blank line. The trimmers below work on the BLOCK,
# the unit they were written for; treating the whole turn as one reply either
# deletes siblings (the reflect cut) or trims nothing (a count guard
# that skips every multi-reply turn).
_BLOCK_SPLIT = re.compile(r"\n\n(?=\[reply to \S+ · )")
# The pid off a block's first line — what `grep` and `strip` address a body by.
_PID_TAG = re.compile(r"^\[reply to (\S+) · ")


def _reply_blocks(content: str) -> list[str]:
    return _BLOCK_SPLIT.split(content)


# A `docs` reply the model has ANSWERED is cut to this stub. A cut
# one turn after the read sits next to the last assistant message, so a prefix cache
# recomputes only what the thinking strip already recomputes. The body itself stays
# HELD this run under its pid: the model keeps the lines it needs
# with its own `grep`, and those small replies stay in its view.
_DOCS_DROPPED = (
    "[docs of={of} tier={tier} — dropped from your view; held this run: send yourself "
    "<type>grep</type> <ids><item>{pid}</item></ids> <query>{verb}</query>]"
)
# The FIRST line of a docs body: it is erased after the next message, and how to keep
# what the model will need.
_DOCS_HEAD = (
    "[erased from your view after your next message — decide now which verbs you need "
    "and keep them in this same turn: send yourself <type>grep</type> "
    "<ids><item>{pid}</item></ids> <query>{verb}</query> <width>1000</width>]\n"
)
_DOCS_VERBS = re.compile(r"^<verbs>\n(.*?)^</verbs>", re.S | re.M)
_DOCS_ENTRY = re.compile(r"^<([A-Za-z_]\w*)>.*?</\1>", re.S | re.M)


def example_verb(body: str) -> str | None:
    """A REAL verb of this docs body for the grep example: the first that is not
    `reflect`/`boot`, else the first. A placeholder example is copied verbatim:
    `<query>verb</query>` sent as written matches only the `<verbs>` lines, and the
    model concludes the held body is gone.
    None when the body lists no verbs (nothing to keep, so nothing is held)."""
    m = _DOCS_VERBS.search(body)
    names = _DOCS_ENTRY.findall(m.group(1)) if m else []
    return next(
        (n for n in names if n not in ("reflect", "boot")), names[0] if names else None
    )


_DOCS_FIELD = re.compile(r"^<(of|tier)>([^<\n]*)</\1>$", re.M)


def point_to_docs(target: str, verb: object, result: object) -> object:
    """A refusal of the model's own call — a verb its target does not answer
    (`unhandled`, naming that verb), or an argument it did not send (`missing_arg`
    naming the `arg`) — names where the fix is, `docs`, when it carries no hint
    of its own. Decided on the machine `reason`, never on prose; a `bad_arg` is
    left alone (the refusal says what was wanted)."""
    if not isinstance(result, dict) or result.get("hint"):
        return result
    reason = result.get("reason")
    if reason == "unhandled" and result.get("verb") == verb:
        return {
            **result,
            "hint": f"the verbs {target} answers: send kernel "
            f"{{type: docs, of: '{target}'}}",
        }
    if reason == "missing_arg" and result.get("arg") and isinstance(verb, str):
        return {
            **result,
            "hint": f"how to call it: send kernel {{type: docs, of: "
            f"'{target}', verb: '{verb}', tier: detailed}}",
        }
    return result


def drop_read_docs(history: list[dict]) -> list[dict]:
    """Replace every `docs` reply body the model has answered with `_DOCS_DROPPED`.
    The reply in the last turn (not answered yet) stays whole; a refusal (no `of`
    in the body) stays whole."""
    out: list[dict] = []
    for i, turn in enumerate(history):
        content = turn.get("content") or ""
        answered = any(t.get("role") == "assistant" for t in history[i + 1 :])
        if turn.get("role") != "user" or not answered or not _REPLY_TAG.match(content):
            out.append(turn)
            continue
        blocks = _reply_blocks(content)
        trimmed = []
        for b in blocks:
            mb = _REPLY_TAG.match(b)
            fields = (
                dict(_DOCS_FIELD.findall(b)) if mb and mb.group(2) == "docs" else {}
            )
            if "of" in fields and (verb := example_verb(b)):
                head = b.split("\n", 1)[0]
                b = (
                    head
                    + "\n"
                    + _DOCS_DROPPED.format(
                        of=fields["of"],
                        tier=fields.get("tier", ""),
                        pid=_PID_TAG.match(b).group(1),
                        verb=verb,
                    )
                )
            trimmed.append(b)
        out.append(
            turn if trimmed == blocks else {**turn, "content": "\n\n".join(trimmed)}
        )
    return out


def dedupe_stale_reads(history: list[dict]) -> list[dict]:
    """Drop whole-state read replies that a later reply to the same (agent, verb)
    supersedes. Newest wins; order is otherwise preserved."""
    seen: set[tuple[str, str]] = set()
    keep_rev: list[dict] = []
    for turn in reversed(history):
        content = turn.get("content") or ""
        if turn.get("role") != "user" or not _REPLY_TAG.match(content):
            keep_rev.append(turn)
            continue
        # Per BLOCK — a turn may carry a whole batch. A superseded whole-state
        # read drops alone; its siblings stay. A turn whose every block is
        # superseded drops entirely.
        kept_rev: list[str] = []
        changed = False
        for b in reversed(_reply_blocks(content)):
            mb = _REPLY_TAG.match(b)
            if mb and mb.group(2) in _WHOLE_STATE_READS:
                key = (mb.group(1), mb.group(2))
                if key in seen:
                    changed = True
                    continue  # a later reply already carries this state
                seen.add(key)
            kept_rev.append(b)
        if not kept_rev:
            continue
        if not changed:
            keep_rev.append(turn)
        else:
            keep_rev.append({**turn, "content": "\n\n".join(reversed(kept_rev))})
    return list(reversed(keep_rev))


def dedupe_repeat_bodies(history: list[dict]) -> list[dict]:
    """A reply whose BODY is byte-identical to an EARLIER one (the same file read again,
    the same listing) is shown as a one-line stub citing the first — the first copy
    stays whole. Only bodies worth the saving are stubbed, and a digested reply (its
    origin line) is already one line. The view restarts at a clear, so a read after a
    clear is new again."""
    import hashlib

    seen: dict[str, object] = {}
    out: list[dict] = []
    for turn in history:
        content = turn.get("content") or ""
        if turn.get("role") != "user" or not _REPLY_TAG.match(content):
            out.append(turn)
            continue
        blocks, changed, kept = _reply_blocks(content), False, []
        for b in blocks:
            head, _, body = b.partition("\n")
            if len(body) < 400 or body.startswith("(settled"):
                kept.append(b)
                continue
            h = hashlib.sha1(body.encode("utf-8", "replace")).hexdigest()
            if h in seen:
                kept.append(
                    f"{head}\n[identical to the reply in row {seen[h]} — the same "
                    f"{len(body)} bytes, not repeated]"
                )
                changed = True
            else:
                seen[h] = turn.get("id")
                kept.append(b)
        out.append({**turn, "content": "\n\n".join(kept)} if changed else turn)
    return out


# ONLY THE ANSWER ACTS. A `<call>` written inside thinking is thinking — a draft, an
# illustration, a plan — never an action: it is not streamed as a call part, not
# dispatched, not stored. A call in thinking is often an exact copy of a call in the
# answer (dispatching both runs the action TWICE) or a draft the model never wrote in
# its answer.


def without_thinking(text: str) -> str:
    """The ANSWER: `text` with every `<think>…</think>` span dropped whole — what
    dispatch reads and what goes into durable history. An unclosed `<think>` runs to
    the end: the rest is thinking."""
    out, i = [], 0
    while True:
        a = text.find(THINK_OPEN, i)
        if a == -1:
            out.append(text[i:])
            break
        out.append(text[i:a])
        b = text.find(THINK_CLOSE, a)
        if b == -1:
            break
        i = b + len(THINK_CLOSE)
    return "".join(out).strip()


#: Characters of a generation's thinking kept on its assistant row (`thinking`);
#: what is past them is counted in `thinking_cut`.
THINKING_CAP = 32000


def thinking_fields(text: str) -> dict:
    """A generation's THINKING, for its assistant row (a chat shows it collapsed
    under the answer, after a reload too): every
    `<think>…</think>` span of `text` (an unclosed one runs to the end), joined by a
    blank line — `{"thinking": …}`, the first `THINKING_CAP` characters, plus
    `{"thinking_cut": n}` when n more were dropped; `{}` when there was none. NEVER
    the model's view (`_render_for_model` reads role + content only) and never
    dispatched."""
    spans, i = [], 0
    while True:
        a = text.find(THINK_OPEN, i)
        if a == -1:
            break
        b = text.find(THINK_CLOSE, a)
        spans.append(text[a + len(THINK_OPEN) : len(text) if b == -1 else b].strip())
        if b == -1:
            break
        i = b + len(THINK_CLOSE)
    thought = "\n\n".join(x for x in spans if x)
    if not thought:
        return {}
    out: dict = {"thinking": thought[:THINKING_CAP]}
    if len(thought) > THINKING_CAP:
        out["thinking_cut"] = len(thought) - THINKING_CAP
    return out


# NO TRUNCATOR. A completion is not cut at the first `[reply to ` — our own marker,
# whose presence proves the model started writing replies it never received. Such a
# cut is right about that text and wrong about everything after it: a fabricated
# reply BETWEEN two real calls would silently drop the second — never dispatched, no
# error to the model, no record in the kernel. Models also mark fabrications in other
# ways (`[awaiting response]`, `<awaiting reply>`), so the cut catches little.
#
# Fabrication is a real failure; silently deleting a model's work is a worse one,
# because the model can debug the first and cannot even see the second.


# THE NO-CALL NUDGE — a turn that dispatched nothing is TRY AGAIN.
#
# Mid-task a model can decide its next action INSIDE
# its thinking, closes the thought, and ends the completion without any
# envelope — or stops early believing itself finished. Every agent harness
# treats "no action emitted" as a control decision, not an ending;
# interactively a human says "continue" — an unattended chain has nobody, so
# the runtime says it.
#
# A call-less turn means "you emitted no call, try again", and it means that every
# time. The chain ends where the model says it ends — `done` (finished) or
# `await` (waiting on what is in flight) — with ONE bound on top: MAX_SILENT_TURNS
# silent turns IN A ROW and the chain gives up, LOUDLY — `status done
# reason=gave_up` to the caller, `_GAVE_UP` to a `send_to` — never silently. Any
# call resets the count. A dead turn (only thinking, discarded) is a silent turn.
# WHAT AN AGENT HAS IN FLIGHT: the number of its own routing turns whose batch is
# still settling (the calls it made, not yet answered). Per agent id. Read by
# `await` — the one control call that asks "is anything of mine still coming?".
_inflight: dict[str, int] = {}

_AWAIT_REFUSED = (
    "[await refused] nothing of yours is in flight and nothing is queued for you, "
    "so there is nothing to wait for. Act — make the call — or end the chain with "
    '<call name="done"></call>.'
)

# The silent streak per chain `(agent id, client id)` — see `_route_reply`.
_silent: dict[tuple[str, str], int] = {}

# OPEN INQUIRIES (fully async). An `inquiry` an AI received is
# accepted at once and runs as its own chain `(receiver, client_id)`; the answer the
# model gives in ANY turn of that chain — or the news that it ended without one —
# is delivered to the asker as a later message under the inquiry's pid. Nothing
# waits on it. Keyed by the receiver's chain; the value names the asker, the pid
# its reply tag carries, and the asker's own thread.
_open_inquiries: dict[tuple[str, str], dict] = {}
# Answers an agent is still OWED, by the inquiry pid: added when its inquiry comes
# back `accepted`, cleared when a message under that pid arrives (`[reply to <pid>
# · <who> inquiry]`, from a person or an agent alike). They count as work in
# flight, so `await` is honest while one is out.
_awaiting: dict[str, set[str]] = {}
_ANSWER_TAG = re.compile(r"\[reply to (\S+) · \S+ inquiry\]")
# EXEC JOBS an agent started and has not yet heard end (#306), by owner: `(raw job_id,
# exec agent id)`. An owner with any is awaiting its own work; each one ends in ONE
# `job_done` wake, after which it is dropped here. Rust: `running_jobs` in route.rs.
_running_jobs: dict[str, list[tuple[Any, str]]] = {}


def _gave_up(limit: int) -> str:
    """What a caller is told when a chain gives up (`limit` silent turns in a row —
    the record's `misfire_limit`, default MAX_SILENT_TURNS)."""
    return (
        f"The LLM was not able to answer — {limit} turns in a row produced "
        "no call. Reiterate the inquiry."
    )


def _misfire_limit(kernel, self_id: str) -> int:
    """The record's `misfire_limit` (#203): how many answers in a row may run
    nothing before the chain gives up; unset, MAX_SILENT_TURNS."""
    _get = getattr(kernel, "get", None)  # a bare test kernel has no record
    v = ((_get(self_id) if _get else None) or {}).get("misfire_limit")
    if isinstance(v, int) and not isinstance(v, bool):
        return v
    return MAX_SILENT_TURNS


def _transport_retries(kernel, self_id: str) -> int:
    """The record's `transport_retries` (#198 part 2): how many times a turn whose
    link dropped is re-run, 0..5; unset, missing or invalid, 0 (off)."""
    _get = getattr(kernel, "get", None)  # a bare test kernel has no record
    v = ((_get(self_id) if _get else None) or {}).get("transport_retries")
    if isinstance(v, int) and not isinstance(v, bool):
        return min(max(v, 0), 5)
    return 0


# Prefixed to the reminder when the turn it answers was DEAD (only thinking).
_DEAD_TURN_LINE = (
    "[no answer] Your last turn produced only thinking and no answer — it was "
    "discarded. "
)

_NO_CALL_NUDGE = (
    "[no call was dispatched] Your answer emitted no <call> element, so "
    "NOTHING ran — a call written in your thinking does not run: write it in "
    "your answer, after the thinking. If the task is unfinished, emit that call now. "
    'If it is finished, end the chain explicitly: <call name="done"></call>'
)


def _wrapper_tag(report: str) -> str:
    """The tag a wrapped answer used, named in its report (default `tool_call`)."""
    head = report.split("drop the wrapper", 1)[0]
    quoted = re.findall(r"`([^`]*)`", head)
    return quoted[-1].strip("<>") if quoted else "tool_call"


def _escalation(n: int, kind: str, tag: str | None = None) -> str:
    """The block a chain gets from its second answer in a row that ran nothing."""
    if kind == "wrapped":
        t = tag or "tool_call"
        phrase = f"put the call inside <{t}>"
        rule = (
            'Your next answer must START with <call name="…"> — the very first characters '
            f"of the answer — and contain <{t}> nowhere."
        )
    elif kind == "malformed":
        phrase = "had calls that did not parse"
        rule = (
            'Your next answer must START with <call name="…"> — the very first characters '
            "of the answer — with no other markup around or inside the call."
        )
    elif kind == "thinking_only":
        phrase = "were only thinking, with no answer"
        rule = (
            "Keep your thinking to a few lines, then write the call in your answer, "
            "after the thinking — a call inside the thinking does not run."
        )
    else:
        phrase = "had no call"
        rule = (
            'Your next answer must START with <call name="…"> — the very first characters '
            'of the answer. If the task is finished, that call is <call name="done"></call>.'
        )
    return (
        f"[no progress — {n} answers in a row ran nothing] Your last {n} answers "
        f"{phrase}, so nothing ran.\n"
        f"{rule}\n"
        "Then continue with your current step."
    )


#: The calls that are not dispatches: they end, park or answer the chain.
_CONTROL_CALLS = ("done", "await", "inquiry_answer")


async def _silent_turn(kernel, self_id, client_id, send_to, system_prompt) -> bool:
    """Count one silent turn of the chain (a turn that ran nothing). At
    MAX_SILENT_TURNS in a row the chain GIVES UP — the status says so, an open
    inquiry is closed, a `send_to` is told — and True is returned."""
    chain = (self_id, client_id)
    streak = _silent[chain] = _silent.get(chain, 0) + 1
    limit = _misfire_limit(kernel, self_id)
    if streak < limit:
        return False
    _silent.pop(chain, None)
    gave_up = _gave_up(limit)
    await _emit_status(
        kernel,
        self_id,
        client_id,
        "done",
        reason="gave_up",
        error=gave_up,
        silent_turns=streak,
    )
    await _close_inquiry(kernel, self_id, client_id, {"error": gave_up})
    if send_to is not None:
        _deliver_soon(kernel, send_to, gave_up, client_id, system_prompt)
    return True


def _origin_line(result, target: str, verb: str, payload) -> str:
    """How to get a reply's body again, in ONE line: the replying agent's own
    `origin` when it wrote one (a file_bridge read names path, range and version);
    otherwise the call that produced it — re-sending it may not give the same
    bytes, and the line says so."""
    if isinstance(result, dict):
        o = result.get("origin")
        if isinstance(o, str) and o.strip():
            return " ".join(o.split())[:300]
    args = {
        k: v
        for k, v in (payload or {}).items()
        if k not in ("type", "client_id", "_call_stack")
    }
    shown = json.dumps(args, default=str, ensure_ascii=False)[:200]
    return f"{target} {verb} {shown} — re-send it; the output may differ"


async def _close_inquiry(kernel, self_id: str, client_id: str, body: dict) -> None:
    """Close the inquiry this chain `(self_id, client_id)` is answering: hand `body`
    (`{answer}` or `{error}`) to its asker as a message under the inquiry's pid,
    `[reply to <pid> · <self_id> inquiry]`. The answer is THIS agent's own send to
    the asker, gated like any: it needs `send` on the asker in
    this agent's `can_call` and the asker's `callable_by` admitting it — a refused
    delivery is told to this agent (`_deliver_answer`). An operator's inquiry has
    no conversation to land in: the answer is an event on this agent's inbox
    instead. No-op when nothing is open."""
    open_ = _open_inquiries.pop((self_id, client_id), None)
    if open_ is None:
        return
    body = {**body, "request_id": open_["request_id"]}
    asker = open_["asker"]
    if not asker:
        await kernel.emit(
            self_id, {"type": "inquiry_answer", "re": open_["pid"], **body}
        )
        return
    text = f"[reply to {open_['pid']} · {self_id} inquiry]\n" + render_reply(
        {k: v for k, v in body.items() if v is not None}
    )
    _ctx(kernel).spawn_task(
        self_id,
        "inquiry_answer",
        _deliver_answer(kernel, self_id, client_id, asker, open_["reply_client"], text),
    )


async def _deliver_answer(
    kernel, self_id, client_id, asker, reply_client, text
) -> None:
    """The answer, sent AS THIS AGENT into the asker's conversation. A send to an
    AI runs the asker's whole turn, so this runs as its own task, never inline. A
    REFUSED answer is not dropped: this agent is told, in its own chain, that its
    answer did not reach the asker and why."""
    out = (
        await kernel.send(
            [
                {
                    "target": asker,
                    "payload": {
                        "type": "send",
                        "text": text,
                        "client_id": reply_client,
                    },
                }
            ]
        )
    )[0]
    res = out.get("result")
    err = res.get("error") if isinstance(res, dict) else None
    if err or out.get("status") in ("failed", "timed_out"):
        _deliver_soon(
            kernel,
            self_id,
            f"[your inquiry answer did not reach {asker}: {err or out.get('status')}]",
            client_id,
            None,
            notice=True,
        )


async def _answer_inquiry(
    kernel, self_id: str, client_id: str, call: dict
) -> str | None:
    """The model's `inquiry_answer`: delivered to the asker of the inquiry this chain
    is answering. Returns the error block for the model when there is nothing to
    answer or the answer is the placeholder."""
    if (self_id, client_id) not in _open_inquiries:
        return (
            "inquiry_answer: nothing asked you in this run — an answer goes to an "
            "inquiry you received (`[inquiry from …]`)"
        )
    answer = (call.get("arguments") or {}).get("answer")
    # THE PLACEHOLDER IS NOT AN ANSWER. The shape shown to a model that did not
    # answer is `<answer>what you concluded</answer>`, and a model copies an example
    # back verbatim.
    if not isinstance(answer, str) or answer.strip().strip(".") == "":
        return (
            "that is the PLACEHOLDER, not an answer — `answer` came back as %r. "
            "Say what you actually concluded." % answer
        )
    await _close_inquiry(kernel, self_id, client_id, {"answer": answer})
    return None


#: an argument name in a verb's `docs` signature (`args: name:type …, name:type`)
_DOC_ARG = re.compile(r"(?:\bargs:|,)\s*([A-Za-z_][A-Za-z0-9_]*)\s*:")


def _routing_clash(
    kernel, target: str, verb: str, used: list[str], cache: dict
) -> list[str]:
    """The routing fields of this call that `target`'s `verb` also documents as
    arguments — read from the kernel's own `docs` (the signature the model reads),
    in process: no dispatch, no ledger row, once per (target, verb) per batch. A
    target this kernel cannot document (a peer's agent, an unknown verb) documents
    nothing here: the call goes on, and its own reply says what happened."""
    key = (target, verb)
    if key not in cache:
        res = kernel._verb_docs({"of": target, "verb": verb})
        sig = ""
        if isinstance(res, dict) and isinstance(res.get("verbs"), dict):
            sig = str(res["verbs"].get(verb) or "")
        cache[key] = set(_DOC_ARG.findall(sig))
    return [k for k in used if k in cache[key]]


async def _route_then_settle(kernel, self_id: str, system_prompt, route) -> None:
    """Run a pass's routing; where it sends nothing back to this agent the chain
    has ended (ONE CHAIN PER AGENT, #227) and messages held meanwhile get ONE wake."""
    goes_on = False
    try:
        goes_on = bool(await route)
    finally:
        if not goes_on:
            _chain_ended(kernel, self_id, system_prompt)


async def _route_reply(
    self_id,
    kernel,
    text,
    client_id,
    send_to,
    system_prompt,
    send_timeout,
    incoming,
    *,
    message_id: str | None = None,
    dead_turn: bool = False,
    call_streams: list[tuple[str, str]] | None = None,
    cut: bool = False,
):
    """Honour where this pass's output goes. Runs DETACHED, after the lock is
    released.

    ONE TURN, ONE BATCH, ONE MESSAGE PER DESTINATION. Every call the model
    emitted is dispatched in a single `kernel.send([...])` (concurrent, order
    of side effects across calls undefined), and everything owed to one
    `reply_to` destination — results, parse errors, shape errors — arrives as
    ONE delivered turn, each piece its own `[reply to …]` block, tagged
    `N of M in batch <token>` when M > 1.

    Why whole, never split: replies landing one per turn make a turn that fanned
    out 7 calls spend the next 7 turns collecting its own answers, and every
    question asked in between is answered turns later by something older. Anthropic
    documents the split shape as the anti-pattern ("separate user messages
    reduce parallel tool use"), OpenAI 400s on it, LangGraph's ToolNode
    collects all ToolMessages before the agent runs again. The model never
    sees a partial batch, so it never re-asks for the missing half.

    `send_timeout` is the AI layer's per-call deadline — the KERNEL defaults
    nothing; this layer always supplies one. The model may override per call
    with a `timeout` arg in the envelope (validated by the kernel; a bad value
    settles that call as a refusal it can read).
    """
    # THE MATCH: a dispatched message carries the `stream_id` of
    # the part it was streamed as, so a consumer settles its draft on it. Matched by
    # the call's exact body, in order — one reading of the text, not two.
    streams = list(call_streams or [])
    calls = []
    # ONLY A SUCCESSFUL GENERATION DISPATCHES: a pass the model stopped with a block
    # open sends NOTHING — its earlier closed calls included; the turn's marker names
    # every one as not sent. ONLY THE ANSWER ACTS: calls written inside thinking are read as thinking.
    answer = repair_calls(without_thinking(text or ""))
    for call, body in [] if cut else terminated_calls(answer):
        i = next((n for n, (_sid, b) in enumerate(streams) if b == body), None)
        if i is not None:
            call = {**call, "stream_id": streams.pop(i)[0]}
        calls.append(call)
    # (dest | None, target, verb, item_index | None, body | None) per EMITTED
    # thing, in emission order. `item_index` links a dispatched call to its
    # outcome; `body` is pre-decided for the paths that never dispatch.
    units: list[tuple[str | None, str, str, int | None, str | None]] = []
    items: list[dict] = []
    # An envelope that OPENED and did not parse gets an error block, exactly
    # like a call missing its `target_id` does — counted in the batch, so M
    # matches what the model actually emitted. Goes to SELF: `reply_to` lives
    # inside the body we could not read, and the documented default for an
    # absent `reply_to` is self anyway.
    for bad in [] if cut else malformed_calls(answer):
        # Plain text, not JSON: the model↔harness path carries no JSON either way.
        units.append(
            (
                self_id,
                "-",
                "-",
                None,
                f"{bad['error']}\n\nyour block:\n{bad['your_block']}",
            )
        )
    # CALLS WRITTEN WRONG: markup carrying OUR call vocabulary
    # outside every `<call` envelope — `<tool_call><function=call name="send">`,
    # `<send>`, a JSON call — is reported like a compiler error, with the form to
    # write instead (`call_check`). The answer's well-formed calls run as written,
    # and each report says how many did.
    ran = sum(1 for c in calls if c.get("name") not in _CONTROL_CALLS)
    reports = [] if cut else check_calls(answer, ran=ran)
    if reports and ran == 0:
        n = _silent.get((self_id, client_id), 0) + 1
        if n >= 2:
            kind = "wrapped" if "drop the wrapper" in reports[-1] else "malformed"
            reports[-1] += "\n" + _escalation(n, kind, _wrapper_tag(reports[-1]))
    for report in reports:
        units.append((self_id, "-", "-", None, report))
    done_called = any(c.get("name") == "done" for c in calls)
    await_called = any(c.get("name") == "await" for c in calls)
    # THE SILENT STREAK of this chain (one per agent + client thread): turns in a
    # row that dispatched nothing. Any call — `done` and `await` included — ends
    # the streak; a turn whose only output was BROKEN calls ran nothing and does
    # not (see MAX_SILENT_TURNS).
    chain = (self_id, client_id)
    if calls:
        _silent.pop(chain, None)
    # ONE MESSAGE, SEVERAL RECIPIENTS. A send with `to` fans out here into one item
    # per recipient — the kernel's batch is already one-to-many — and every copy
    # carries the whole list (`to`), the sending turn's id (`msg`, the same number in
    # every copy, so the group can refer to one message) and the message it answers
    # (`re`). A single-recipient message to a peer carries `msg` too, so it can be
    # answered by number. Nothing is shared: each copy is a row in its own tree.
    expanded: list[dict] = []
    for call in calls:
        a = call.get("arguments") or {}
        to = a.get("to")
        if call.get("name") == "send" and isinstance(to, list) and to:
            recipients = [str(t).strip() for t in to if str(t).strip()]
            base = {k: v for k, v in a.items() if k not in ("to", "re", "target_id")}
            for r in recipients:
                payload = dict(a.get("payload") or {})
                payload["to"] = recipients
                payload["msg"] = message_id
                if call.get("stream_id") and payload.get("type") == "send":
                    payload["stream_id"] = call["stream_id"]
                if "re" in a:
                    payload["re"] = a["re"]
                expanded.append(
                    {**call, "arguments": {**base, "target_id": r, "payload": payload}}
                )
        else:
            if (
                call.get("name") == "send"
                and isinstance(a.get("payload"), dict)
                and a["payload"].get("type") == "send"
            ):
                payload = dict(a["payload"])
                payload.setdefault("msg", message_id)
                # A MESSAGE carries the stream it was previewed as (only a message:
                # a verb call's payload stays exactly what the model wrote).
                if call.get("stream_id"):
                    payload["stream_id"] = call["stream_id"]
                if "re" in a:
                    payload["re"] = a["re"]
                call = {
                    **call,
                    "arguments": {
                        **{k: v for k, v in a.items() if k != "re"},
                        "payload": payload,
                    },
                }
            if (
                call.get("name") == "send"
                and isinstance(a.get("payload"), dict)
                and a["payload"].get("type") == "inquiry"
            ):
                # THE ASKER'S THREAD rides the inquiry, so the answer that comes back
                # later (fully async) lands where this chain is working.
                call = {
                    **call,
                    "arguments": {
                        **a,
                        "payload": {**a["payload"], "reply_client_id": client_id},
                    },
                }
            expanded.append(call)
    calls = expanded
    # THE ANSWER to an inquiry this chain received goes to the asker now, under the
    # inquiry's pid; an answer with nothing to answer, or a placeholder, is told so.
    for call in calls:
        if call.get("name") == "inquiry_answer":
            err = await _answer_inquiry(kernel, self_id, client_id, call)
            if err:
                units.append((self_id, "-", "-", None, err))
    doc_args: dict[tuple[str, str], set[str]] = {}
    for call in calls:
        if call.get("name") in _CONTROL_CALLS:
            # CONTROL CALLS, not dispatches. `done` ends the chain;
            # `inquiry_answer` was delivered to its asker above. Neither is a
            # send, neither is an error, and neither is ever nudged.
            continue
        args = call.get("arguments") or {}
        target = args.get("target_id") or args.get("target")
        # `reply_to` DEFAULTS TO SELF. An omitted key means the model gets its
        # own answer back, not that the answer is thrown away. Discarding a
        # reply is something you must SAY (`reply_to: null` / "none") — a
        # dropped key costs a wasted turn, not the whole task.
        if "reply_to" in args:
            reply_to = args.get("reply_to")
            # The tag grammar types a bare number (`<reply_to>1001</reply_to>` →
            # 1001): a number is an id written by the model, never "discard" —
            # it is checked like any id below.
            if isinstance(reply_to, (int, float)) and not isinstance(reply_to, bool):
                reply_to = str(reply_to)
            reply_to = reply_to.strip() if isinstance(reply_to, str) else None
            if reply_to and reply_to.lower() in ("none", "null", "nobody"):
                reply_to = None
        else:
            reply_to = self_id
        if reply_to in ("self", self_id):
            reply_to = self_id
        if not isinstance(target, str) or not target.strip():
            # NAME THE SHAPE, not just the fault — a send must name the AGENT
            # it goes to, and only the model can close that gap; a bare fault sends
            # a model re-deriving the envelope from the system prompt instead of
            # retrying.
            if reply_to:
                units.append(
                    (
                        reply_to,
                        "-",
                        "-",
                        None,
                        render_reply(
                            {
                                "error": (
                                    "no target_id, so nothing was dispatched — a "
                                    "send must name the AGENT it goes to. Shape: "
                                    '<call name="send"><target_id>agent</target_id>'
                                    "<type>verb</type></call>. Send `kernel` "
                                    "<type>reflect</type> for the list of agent ids."
                                ),
                                "you_sent": {
                                    k: v for k, v in args.items() if k != "payload"
                                },
                            }
                        ),
                    )
                )
            continue
        target = target.strip()
        payload = args.get("payload")
        # A REPLY ADDRESSED TO NOBODY IS LOST. `reply_to` is where this call's
        # answer goes, never an argument the target sees; one naming no agent would
        # make the delivery fail on that missing agent's inbox, so nobody sees it and
        # the model never learns what its call did. Nothing is dispatched; the model
        # is told. A peer address
        # (`<guid>|<agent>`) is the peer's to resolve.
        if (
            reply_to
            and reply_to not in (self_id, "kernel")
            and "|" not in reply_to
            and kernel.get(reply_to) is None
        ):
            verb = payload.get("type", "?") if isinstance(payload, dict) else "-"
            units.append(
                (
                    self_id,
                    target,
                    str(verb),
                    None,
                    render_reply(
                        {
                            "error": (
                                f"reply_to={reply_to!r} names no agent, so nothing "
                                f"was dispatched to {target!r}. `reply_to` is where "
                                "this call's reply goes (left out: back to you) and "
                                "never reaches the target — an argument of the "
                                "target's verb needs the name its docs give."
                            ),
                            "target": target,
                            "reply_to": reply_to,
                        }
                    ),
                )
            )
            continue
        if not isinstance(payload, dict) or not payload:
            # NO PAYLOAD, NO SEND — what arrives here has no verb in it, and
            # saying so beats a Python error about our internals.
            if reply_to:
                units.append(
                    (
                        reply_to,
                        target,
                        "-",
                        None,
                        render_reply(
                            {
                                "error": (
                                    f"send to {target!r} carried no payload, so "
                                    "nothing was dispatched. A call names its verb "
                                    "in `<type>`, e.g. <type>reflect</type>."
                                ),
                                "target": target,
                                "you_sent": {
                                    k: v for k, v in args.items() if k != "payload"
                                },
                            }
                        ),
                    )
                )
            continue
        # A ROUTING NAME THE VERB ALSO TAKES IS LOST. `title`,
        # `timeout`, `reply_to`, … are this call's own fields — lifted out, never sent
        # to the target. A verb that documents an argument of the same name can never
        # receive it from a model call, and the model retries and blames the target.
        # The target's `docs` names the verb's arguments; a clash refuses the call, loud.
        used = [k for k in _ROUTING if k != "target_id" and k in args]
        if used and isinstance(payload.get("type"), str):
            clash = _routing_clash(kernel, target, payload["type"], used, doc_args)
            if clash:
                units.append(
                    (
                        self_id,
                        target,
                        payload["type"],
                        None,
                        render_reply(
                            {
                                "error": (
                                    f"{', '.join(clash)} on this call "
                                    + ("is" if len(clash) == 1 else "are")
                                    + " call routing, never sent to the target — but "
                                    f"{target}'s `{payload['type']}` documents an "
                                    f"argument named {', '.join(clash)}, which no call "
                                    "can deliver; nothing was dispatched. Call it "
                                    "without that argument, or have the verb's owner "
                                    "rename it."
                                ),
                                "target": target,
                                "clash": clash,
                            }
                        ),
                    )
                )
                continue
        # The model's own `timeout` overrides the layer default; the kernel
        # validates it, so a bad value comes back as a readable refusal.
        # A SELF-CALL INHERITS ITS CONVERSATION. The model's calls are dispatched here,
        # AFTER the turn closed and `_current` was cleared — so a verb that resolves
        # its conversation from the in-flight entry (`recall`, `grep`, `history`,
        # `pull` of a fold) would see the DEFAULT client and search an empty store.
        if (
            target == self_id
            and isinstance(payload, dict)
            and "client_id" not in payload
        ):
            payload = {**payload, "client_id": client_id}
        item = {"target": target, "payload": payload, "timeout": send_timeout}
        if "timeout" in args:
            item["timeout"] = args.get("timeout")
        # The model's execution NAME rides through untouched as the item's `id` —
        # the kernel owns the ledger semantics AND the validation (a bad name
        # settles as a refusal the model can read).
        if "exec_id" in args:
            item["id"] = args.get("exec_id")
        if "retries" in args:
            item["retries"] = args.get("retries")
        items.append(item)
        if reply_to:
            units.append(
                (
                    reply_to,
                    target,
                    payload.get("type", "?"),
                    len(items) - 1,
                    None,
                    _clean_title(args.get("title")),
                )
            )
        # fire-and-forget: dispatched, counted nowhere, nobody awaits the answer
    # IN FLIGHT while the batch settles: `await` from a LATER turn (a root that
    # arrived meanwhile) must see that these calls are still coming.
    _inflight[self_id] = _inflight.get(self_id, 0) + (1 if items else 0)
    try:
        outcomes = await kernel.send(items) if items else []
    finally:
        if items:
            _inflight[self_id] = max(0, _inflight.get(self_id, 0) - 1)
    # A JOB this agent started (#306): an `exec start` answered `running` is watched, and
    # its end wakes this agent once. Only this batch's own replies are read.
    for item, out in zip(items, outcomes):
        res = out.get("result")
        if (
            (item.get("payload") or {}).get("type") == "start"
            and isinstance(res, dict)
            and res.get("status") == "running"
            and res.get("job_id") is not None
        ):
            _watch_job(
                kernel,
                self_id,
                item["target"],
                res["job_id"],
                client_id,
                system_prompt,
            )
    # AN ACCEPTED INQUIRY IS OWED AN ANSWER — counted until a message under its pid
    # arrives (`_send` clears it).
    for item, out in zip(items, outcomes):
        res = out.get("result")
        if (
            (item.get("payload") or {}).get("type") == "inquiry"
            and isinstance(res, dict)
            and res.get("accepted")
        ):
            _awaiting.setdefault(self_id, set()).add(
                str(out.get("pid") or res.get("re"))
            )
    if not calls and units:
        # Only broken calls: their reports are the next turn — unless this chain
        # has now been silent MAX_SILENT_TURNS in a row, and gives up.
        if await _silent_turn(kernel, self_id, client_id, send_to, system_prompt):
            return False
    # ONE CHAIN PER AGENT (#227): the chain goes on only when a reply batch lands
    # on this agent itself.
    goes_on = False
    if units:
        # A STABLE TOKEN PER TURN — two batches are distinguishable at a
        # glance (bare "1 of 3" labels collide across turns). The PID still
        # identifies the individual call and is the `ps`/`cancel` handle.
        # The batch token IS the assistant row's hex, so `in batch <b>` and `↑<message>`
        # name the same thing and the model can see the row every reply points at.
        # The batch token IS the assistant row's id, so `in batch <b>` and `↑<message>`
        # name the same thing.
        batch = message_id if message_id else secrets.token_hex(2)
        groups: dict[str, list[str]] = {}
        order: list[str] = []
        for dest, target, verb, idx, body, *_ in units:
            if dest not in groups:
                groups[dest] = []
                order.append(dest)
            groups[dest].append("")  # reserve the slot; fill below with n/M known
        counts = {d: 0 for d in groups}
        _get = getattr(kernel, "get", None)  # a bare test kernel has no record
        rec_ = (_get(self_id) if _get else None) or {}
        cap = _inline_cap(rec_)
        # THE CAP IS PER MESSAGE, not per reply: every reply of a batch lands in ONE
        # message to its addressee, so the replies share the cap. A reply that would
        # push its batch past it is held like an oversized one.
        used: dict[str, int] = {}
        for dest, target, verb, idx, body, *rest in units:
            title = rest[0] if rest else None
            if idx is not None:
                out = outcomes[idx]
                pid = out["pid"] or "-"
                # The SAME notation the model writes calls in — never JSON.
                body = render_reply(point_to_docs(target, verb, out["result"]))
                # Only docs bodies are cut later, so only they carry the notice, and
                # are HELD under their pid for the model's own `grep`. A reflect is
                # MUTABLE state and stays whole; a newer reflect
                # of the same agent supersedes it (dedupe).
                if (
                    verb == "docs"
                    and out["pid"] is not None
                    and isinstance(out["result"], dict)
                    and "of" in out["result"]
                    and (example := example_verb(body))
                ):
                    await _store_reply(self_id, kernel, out["pid"], body)
                    body = _DOCS_HEAD.format(pid=out["pid"], verb=example) + body
                # FULL ASYNC. A reply is DATA, not a question — the model needs to
                # be able to see it, not forced to swallow it whole as its next
                # turn. Anything over the inline cap is stored under its pid and
                # replaced by its digest; the model pulls pages when it wants them.
                # This is what makes an unbounded reply (a 240KB ledger dump)
                # non-fatal: it becomes a row to page, never a turn to fit.
                # SYNC OR ASYNC ALIKE: a body is never persisted
                # (§3). Over the inline cap it waits in RAM and the stored turn carries
                # its origin line; under it, it is inline.
                if (
                    pid is not None
                    and used.get(dest, 0)
                    + estimate_one({"role": "user", "content": body})
                    > cap
                ):
                    size = len(body)
                    if await _store_reply(self_id, kernel, pid, body):
                        # THE STUB IS THE ORIGIN. The body is held in RAM for this
                        # incarnation only and never persisted; what survives is the
                        # one line saying how to get it again — the reply's own
                        # `origin` when its agent wrote one, else the call itself.
                        body = (
                            f"(settled, {size} bytes — NOT inlined, held in memory only "
                            f"until this agent restarts. origin: "
                            f"{_origin_line(out['result'], target, verb, items[idx]['payload'])}. "
                            f"To find a line in it: send yourself <type>grep</type> "
                            f"<query>substring</query> <ids><item>{pid}</item></ids>)"
                        )
            else:
                pid = "-"
            used[dest] = used.get(dest, 0) + estimate_one(
                {"role": "user", "content": body}
            )
            n = counts[dest] = counts[dest] + 1
            m_total = sum(1 for u in units if u[0] == dest)
            tag = _reply_tag(pid, target, verb, title, message_id, n, m_total, batch)
            groups[dest][n - 1] = tag + "\n" + body
        for dest in order:
            # A batch to SELF made only of this layer's own text (malformed-call
            # reports, refusals of calls it could not read) is a notice; one that
            # carries any call's outcome is a reply.
            notice = dest == self_id and all(
                u[3] is None for u in units if u[0] == dest
            )
            _deliver_soon(
                kernel,
                dest,
                "\n\n".join(groups[dest]),
                client_id,
                system_prompt,
                notice=notice,
            )
        # ONE CHAIN PER AGENT (#227): the batch landed on this agent itself, so the
        # chain goes on.
        goes_on = self_id in groups
    if await_called and not done_called:
        # AWAIT: "I am not finished, I am waiting for what is in flight; wake me
        # when it lands." No nudge, no reply expected, the chain stays open — the
        # next delivery is the next turn. Recorded as the model's own decision
        # (a status event the user sees), not a silence the harness must read.
        # HONEST OR REFUSED: with nothing of this agent's in flight and nothing
        # queued for it, awaiting is stalling, and it is told so.
        #
        # Routing runs AFTER the turn released its lock, so "queued" is the queue
        # plus a turn another message may already be running under that lock.
        pending = (
            _inflight.get(self_id, 0)
            + len(_queue.get(self_id, []))
            + (1 if _lock_for(self_id).locked() else 0)
            + len(_awaiting.get(self_id, ()))  # answers to its inquiries, still to come
            # ...and the exec jobs it started that are still running (#306): their end
            # wakes it.
            + len(_running_jobs.get(self_id, ()))
        )
        # ONE CHAIN PER AGENT (#227): messages held while this chain ran are answered
        # by the wake turn, so this chain ends now.
        if _chain_of(self_id)["held"]:
            return False
        # TODO — STALL DETECTION. A chain can stop with no error: calls dispatched, then
        # nothing. Three shapes:
        # (1) a call never settles (no timeout by default, its task stays in `ps`); (2) every
        # call settled but the reply batch never reached this agent's conversation; (3) the
        # reply landed but no turn started. This agent KNOWS it is awaiting `pending` work:
        # if nothing changes for T seconds, emit status `stalled` naming the calls not settled
        # / not delivered, so a CLI / canvas / harness shows it like any status.
        if pending > 0 or items:
            await _emit_status(
                kernel, self_id, client_id, "done", reason="awaiting", pending=pending
            )
            # ONE CHAIN PER AGENT (#227): an await is not a running chain. Messages
            # that land while it waits wake it as any message does.
            return False
        _deliver_soon(
            kernel, self_id, _AWAIT_REFUSED, client_id, system_prompt, notice=True
        )
        # That refusal is a turn delivered to the agent itself: the chain goes on.
        return True
    if done_called:
        # AN INQUIRY THIS CHAIN NEVER ANSWERED is closed for its asker, saying so —
        # the asker must not wait on an answer that will not come.
        await _close_inquiry(
            kernel,
            self_id,
            client_id,
            {"error": f"{self_id} ended its run without answering"},
        )
        # Explicitly finished — nothing more runs, nothing reminds. A caller's
        # `send_to` hand-off is the one thing that still applies: the finished
        # text is what it asked to have delivered.
        if send_to is not None and not units:
            _deliver_soon(kernel, send_to, text, client_id, system_prompt)
        return False
    if not calls and not units:
        # TRY AGAIN. Every turn, chained or not: a first turn from a person that answers in prose and emits
        # nothing is the same silence as a mid-chain one — the model ends it
        # with `done`, waits with `await`, or acts. A DEAD turn (only thinking,
        # discarded unstored) is the same silence and is rerolled the same way.
        #
        # ASKING IS A CALL, so there is no question-turn exemption. A turn that
        # asks the human DISPATCHES (`send` to the human agent) and is therefore
        # never silent; the answer arrives as that call's reply.
        #
        # BOUNDED: MAX_SILENT_TURNS in a row and the chain
        # GIVES UP instead of reminding again — the caller is told, loudly, that
        # the model could not answer. A person sees it in their view; a parent
        # that asked for the result by `send_to` gets it as a turn; a clock's
        # tick has nobody reading, so the status event is the record, and the
        # next tick starts a fresh chain with a fresh count.
        if await _silent_turn(kernel, self_id, client_id, send_to, system_prompt):
            return False
        nudge = (_DEAD_TURN_LINE + _NO_CALL_NUDGE) if dead_turn else _NO_CALL_NUDGE
        n = _silent.get((self_id, client_id), 0)
        if n >= 2:
            nudge += "\n" + _escalation(n, "thinking_only" if dead_turn else "no_call")
        _deliver_soon(kernel, self_id, nudge, client_id, system_prompt, notice=True)
        # The nudge is a turn delivered to the agent itself: the chain goes on.
        return True
    if calls:
        # the model routed its own work; a caller's hand-off does not apply
        return True
    if send_to is not None:
        _deliver_soon(kernel, send_to, text, client_id, system_prompt)
    return goes_on


#: The fields of an agent's own reflect its system block shows.
_SELF_KEYS = ("id", "description", "memory_store_id", "can_call", "callable_by")


#: THE PLACEHOLDERS a system prompt may hold, filled per agent and per turn — so a
#: prompt that REPLACES the default block keeps its live parts:
#: `{tree}` the kernel primer (tree ids + bundle catalog), `{self}` this agent's
#: identity and grants, `{store}` its memory store id, `{howto}` the fixed how-to
#: blocks. `{tree}\n\n{self}\n\n{howto}` IS the default block, byte for byte. Only
#: the parts a prompt names are built; any other `{…}` stays as written.
_PLACEHOLDER = re.compile(r"\{(tree|self|store|howto)\}")


async def _parts(self_id: str, kernel, need: set[str]) -> dict[str, str]:
    """The live parts of the system block, built only where `need` names them."""
    out: dict[str, str] = {}
    if "tree" in need:
        primer = (
            await kernel.send(
                [
                    {
                        "target": "kernel",
                        "payload": {"type": "reflect", "tree": "ids", "bundles": "ids"},
                    }
                ]
            )
        )[0]["result"]
        out["tree"] = _render_reflect(primer)
    if "self" in need:
        me = (
            await kernel.send(
                [{"target": self_id, "payload": {"type": "reflect", "tree": "none"}}]
            )
        )[0]["result"]
        # WHO YOU ARE, not how you are tuned: identity, memory and grants. The rest
        # of the agent's own reflect (sampling, kv table, timeouts, endpoint) is
        # operator posture the model never acts on — it cost ~1k chars on every turn.
        if isinstance(me, dict):
            me = {k: me[k] for k in _SELF_KEYS if k in me}
        out["self"] = f"You are `{self_id}`. " + _render_reflect(me)
    rec = kernel.get(self_id) or {}
    store = rec.get("memory_store_id") or "(none)"
    if "store" in need:
        out["store"] = store
    if "howto" in need:
        blocks = [_SEND_TO_HOWTO, _CONTEXT_HOWTO, _MEMORY_HOWTO.format(store=store)]
        # OPT-IN, on the record. The `facts` verb exists on every AI, but teaching it
        # costs every agent prompt tokens for a discipline most runs never need — and
        # a block like this changes behavior just by being read. `context_facts`
        # turns it on INDEPENDENTLY of `context_strategy`, so a comparison can hold
        # the model's behavior fixed and vary only what the strategy does with the
        # dumps.
        if rec.get("context_titles"):
            blocks.append(_TITLES_HOWTO)
        if rec.get("context_async"):
            blocks.append(_ASYNC_HOWTO)
        out["howto"] = "\n\n".join(blocks)
    return out


async def _default_system_content(self_id: str, kernel) -> str:
    """The deterministic substrate prompt — rebuilt every turn from the live tree. The
    default `_system_content` falls to when no prompt replaces it:
    `{tree}\n\n{self}\n\n{howto}`."""
    p = await _parts(self_id, kernel, {"tree", "self", "howto"})
    return "\n\n".join([p["tree"], p["self"], p["howto"]])


async def _render_prompt(text: str, self_id: str, kernel) -> str:
    """A prompt with its placeholders filled for this agent and this turn — one pass,
    so a filled part is never scanned again."""
    need = set(_PLACEHOLDER.findall(text))
    if not need:
        return text
    parts = await _parts(self_id, kernel, need)
    return _PLACEHOLDER.sub(lambda m: parts[m.group(1)], text)


async def _system_content(
    self_id: str, kernel, system_prompt: str | None = None
) -> str:
    """THE system block a turn uses, WRAPPED: the record's `pre_system_prompt` goes
    BEFORE whichever prompt is in force and `post_system_prompt` AFTER it, each read
    live and taken verbatim — no placeholders, and neither
    replaces anything: the default block, a `system_prompt` or a prompt file stays
    whole between them. Absent or null adds nothing."""
    rec = kernel.get(self_id) or {}
    inner = await _inner_system_content(self_id, kernel, system_prompt)
    parts = [
        v
        for v in (rec.get("pre_system_prompt"), inner, rec.get("post_system_prompt"))
        if isinstance(v, str) and v.strip()
    ]
    return "\n\n".join(parts)


async def _inner_system_content(
    self_id: str, kernel, system_prompt: str | None = None
) -> str:
    """The prompt in force, before the pre/post wrap — the one choice `_assemble` builds the turn on
    and `_context_recall` sizes the body budget against, so the two never drift.

    PRECEDENCE: the per-call argument (that
    delivery) > the record's `system_prompt` field, read LIVE so an `update_agent`
    lands on the next turn — a main turn, a nudge and a reply-woken turn alike > the
    file the record's `system_prompt_file` names (read through its file_bridge,
    re-read when the bridge reports it changed — `prompt_file`) > the default
    block. Whichever prompt is in force has its placeholders filled
    (`_render_prompt`); none is ever appended to another."""
    rec_prompt = (kernel.get(self_id) or {}).get("system_prompt")
    if isinstance(system_prompt, str) and system_prompt.strip():
        return await _render_prompt(system_prompt, self_id, kernel)
    if isinstance(rec_prompt, str) and rec_prompt.strip():
        return await _render_prompt(rec_prompt, self_id, kernel)
    # Set and unusable raises `PromptFileError` — the turn fails, never this
    # block's default below.
    file_text = await prompt_file.text(self_id, kernel)
    if file_text is not None:
        return await _render_prompt(file_text, self_id, kernel)
    # The DEFAULT system block is deliberately lean: a tree id-index + bundle
    # catalog primer, self-identity, and the context-window howto (context IS
    # the backend's — see `_CONTEXT_HOWTO`). NO agent menu, NO tool howto — this
    # backend does not run a tool loop; a caller that wants tool-calling supplies
    # its own `system_prompt` teaching its convention + whatever menu it built.
    return await _default_system_content(self_id, kernel)


def _listed(hist: list[dict], view: dict | None) -> list[dict] | None:
    """The view array's entries when there is a usable one: non-empty, and every id it
    names still a row of the store. Otherwise None (no array, or a stale one)."""
    if not view or not view["ids"]:
        return None
    known = {r.get("id") for r in hist}
    if all(e.get("id") in known for e in view["ids"]):
        return view["ids"]
    return None


def _render_tail(
    hist: list[dict], frm: int, clean: bool
) -> list[tuple[int, str, dict]]:
    """The rows of `hist` from `frm` on, each as (store index, res, message). Under
    `clean` they run through the trimmers: a trimmed row keeps its id, a dropped one is
    absent. Otherwise every row is kept as stored."""
    if not clean:
        return [(i, _res_of(hist[i]), hist[i]) for i in range(frm, len(hist))]
    at = {r.get("id"): i for i, r in enumerate(hist) if i >= frm}
    kept = drop_read_docs(dedupe_stale_reads(dedupe_repeat_bodies(hist[frm:])))
    return [(at[r.get("id")], _res_of(r), r) for r in kept]


def _render_history(
    hist: list[dict], view: dict | None, clean: bool
) -> list[tuple[int, str, dict]]:
    """The history part of the model view (no system block, no user turn), as
    (store index, res, message): the view array's entries each at its recorded
    resolution, then the rows after the newest of them (trimmed under `clean`); with no
    usable array, the whole store, trimmed under `clean`."""
    listed = _listed(hist, view)
    if listed is None:
        return _render_tail(hist, 0, clean)
    pos = {r.get("id"): i for i, r in enumerate(hist)}
    out = []
    for e in listed:
        i = pos[e["id"]]
        res = e.get("res", "full")
        # A body a trimmer left on the entry (a `context_fold` keeps the rows before
        # its range as the model saw them) renders as saved while cleanup is on.
        if clean and isinstance(e.get("content"), str):
            out.append((i, res, {**hist[i], "content": e["content"]}))
        else:
            out.append((i, res, _apply_res(hist[i], res)))
    return out + _render_tail(hist, max(pos[e["id"]] for e in listed) + 1, clean)


async def _assemble(
    self_id: str,
    user_text: str,
    kernel,
    client_id: str,
    system_prompt: str | None = None,
    *,
    mode: str = caching.DEFAULT_MODE,
) -> tuple[list[dict], int]:
    """Build the message list. A caller-supplied `system_prompt` REPLACES the
    deterministic substrate prompt — absent, the record's own `system_prompt`
    field replaces it — the generic 'caller controls context' door:
    a state agent or a python routine reads a stored prompt and passes it here, so
    the AI needs NO yaml-specific code. The conversation is ALWAYS this agent's own
    (its memory store): there is no caller-supplied history — a one-off completion
    is a disposable agent with its own store; context for another agent travels in
    the text.

    Returns `(messages, store_len)` — `store_len` is how many turns the head held
    when this view was built. The view is deduped and trimmed, so its length is NOT
    a store index; `_project_context` needs the store one to mark where this turn's
    reaction will land. Returning it here is the only place both numbers are in
    hand at once."""
    sys_content = await _system_content(self_id, kernel, system_prompt)
    # System block is rebuilt on EVERY user turn; chat.json holds only
    # the user/assistant turns. The howto is not persisted — it always
    # reflects the latest state at send-time.
    messages: list[dict] = [{"role": "system", "content": sys_content}]
    store_len = 0
    # Deduped VIEW, not a rewrite of the store: a stale whole-state read that a
    # later reply supersedes never reaches the model. See `dedupe_stale_reads`.
    # Trimmed VIEW, not a rewrite of the store (durable history keeps every turn,
    # so the trace stays complete): stale whole-state reads deduped, and answered
    # reflects reduced to a stub — the model's own distillation carries the facts.
    hist = await _load_history(self_id, kernel)
    store_len = len(hist)
    # THE VIEW ARRAY IS THE STATE. When one exists and every id it names still
    # resolves, the model sees exactly those rows at exactly those resolutions — the
    # same bytes before and after a restart. The rows after its newest entry ride
    # behind it in full (trimmed under cleanup). Otherwise the view is the trimmed
    # store, as before. `_render_history` is the one implementation, shared with
    # `context_fold`, which persists exactly what this renders.
    view = await _load_view(self_id, kernel)
    messages.extend(
        msg for _, _, msg in _render_history(hist, view, caching.cleans(mode))
    )
    if _listed(hist, view) is not None:
        _view_source[self_id] = "array"
    else:
        _view_source[self_id] = (
            "store" if view is None or not view["ids"] else "store (stale array)"
        )
    messages.append({"role": "user", "content": user_text})
    return messages, store_len


# ─── streaming + native tool-calls (Phase 2) ────────────────────


class _PartPublisher:
    """Routes part events (`stream_parts`) to their addressees, deltas BATCHED so a
    token-rate stream does not become a token-rate event stream.

    A call part with an addressee goes to that inbox (each of `to`), as an emit — gated
    like any emit on another agent; a refused one is dropped. Prose, thought and calls
    without an addressee (done, await, inquiry_answer) go where the tokens go: the
    caller (`_to_caller`)."""

    BATCH_SECONDS = 0.05

    def __init__(self, kernel, self_id: str, client_id: str, re: int | None = None):
        self.kernel, self.self_id, self.client_id = kernel, self_id, client_id
        # THE ANCHOR: the message this turn answers — where a consumer places the
        # draft in its own (possibly folded) history.
        self.re = re
        self._pending: dict[str, tuple[dict, str]] = {}
        self._last: dict[str, float] = {}

    async def _send(self, part: dict, ev: dict) -> None:
        ev = {**ev, "stream_id": part["stream_id"], "source": self.self_id}
        # DELTAS ARE QUIET: inbox and watchers only, never the
        # state stream — it carries a part's lifecycle (start / end / abort).
        quiet = ev["type"] == "part_delta"
        addrs = part.get("to") or ([part["target"]] if part.get("target") else [])
        if part["kind"] != "call" or not addrs:
            # the terminal renders tokens, not parts — `cli` gets none
            if self.client_id == DEFAULT_CLIENT_ID:
                return
            await self.kernel.emit(
                self.self_id, {**ev, "client_id": self.client_id}, quiet=quiet
            )
            return
        for addr in addrs:
            await self.kernel.emit(
                self.self_id if addr == "self" else addr, ev, quiet=quiet
            )

    async def _flush(self, sid: str) -> None:
        held = self._pending.pop(sid, None)
        if held is not None:
            part, text = held
            self._last[sid] = time.monotonic()
            await self._send(part, {"type": "part_delta", "text": text})

    async def push(self, events: list[tuple], final: bool = False) -> None:
        for ev in events:
            kind, part = ev[0], ev[1]
            sid = part["stream_id"]
            if kind == "start":
                head = {k: v for k, v in part.items() if k != "stream_id"}
                if self.re is not None:
                    head["re"] = self.re
                await self._send(part, {"type": "part_start", **head})
                self._last[sid] = time.monotonic()
            elif kind == "delta":
                prev = self._pending.get(sid)
                self._pending[sid] = (part, (prev[1] if prev else "") + ev[2])
                if time.monotonic() - self._last.get(sid, 0) >= self.BATCH_SECONDS:
                    await self._flush(sid)
            elif kind == "end":
                await self._flush(sid)
                await self._send(part, {"type": "part_end", **ev[2]})
            elif kind == "abort":
                self._pending.pop(sid, None)
                await self._send(part, {"type": "part_abort", "reason": ev[2]})
        if final:
            for sid in list(self._pending):
                await self._flush(sid)


async def _to_caller(kernel, self_id: str, client_id: str, ev: dict) -> None:
    """Route a stream event to the originating caller ONLY.

    - client_id == 'cli': dispatch via `kernel.send` so cli's handler
      runs and prints to stdout (REPL/headless flow).
    - client_id == anything else (browser uuid, etc.): emit on the
      backend's inbox tagged with client_id; the browser's WS
      subscription mirrors it and filters to its own id.

    No fan-out: cli does NOT see browser tokens, and vice versa.
    """
    ev = {**ev, "client_id": client_id}
    if client_id == DEFAULT_CLIENT_ID:
        (await kernel.send([{"target": "cli", "payload": ev}]))[0]["result"]
    else:
        await kernel.emit(self_id, ev)


def usage_detail(
    provider, model_messages: list, text: str, gen_seconds: float, window: int
) -> dict:
    """The token figures a finished generation's `status` `done` carries (a chat
    shows `42 tok/s · 3.1k / 64k ctx`): `prompt_tokens` and
    `completion_tokens` as the provider REPORTED them — estimated (chars/4, this
    layer's own count) when it reported none, and then `prompt_tokens_estimated` /
    `completion_tokens_estimated` says so — `gen_seconds` (the wall time from the
    request to the stream's end, thinking included) and `context_window` (the window
    in force)."""
    out: dict = {}
    reported = getattr(provider, "last_prompt_tokens", None)
    if isinstance(reported, int) and reported > 0:
        out["prompt_tokens"] = reported
    else:
        out["prompt_tokens"] = estimate_tokens(model_messages)
        out["prompt_tokens_estimated"] = True
    done = getattr(provider, "last_completion_tokens", None)
    if isinstance(done, int) and done > 0:
        out["completion_tokens"] = done
    else:
        out["completion_tokens"] = max(1, len(text) // 4) if text else 0
        out["completion_tokens_estimated"] = True
    out["gen_seconds"] = round(gen_seconds, 3)
    out["context_window"] = window
    return out


async def _emit_status(
    kernel, self_id: str, client_id: str, phase: str, **detail
) -> None:
    """Broadcast a structured `status` event AND update _current's phase
    so the on-demand `status` verb stays in sync. Phases:
        queued, thinking, streaming, tool_calling, retrying, done.
    UI subscribes for live phase transitions (animate a pulse indicator
    while phase is thinking/tool_calling), and calls the `status` verb
    on page boot to rebuild lost state.

    `send_id` and `started_at` for the in-flight entry are pulled from
    `_current[self_id]` so callers don't have to thread them through
    every emit site. `phase='queued'` callers (where `_current` may not
    hold this entry yet) pass `send_id` explicitly via `**detail`.
    """
    cur = _current.get(self_id)
    if cur is not None:
        cur["phase"] = phase
        # Auto-fill correlation fields when the entry is the current one.
        detail.setdefault("send_id", cur.get("send_id"))
        detail.setdefault("started_at", cur.get("started_at"))
    detail.setdefault("queue_depth", len(_queue.get(self_id, [])))
    await _to_caller(
        kernel,
        self_id,
        client_id,
        {
            "type": "status",
            "source": self_id,
            "phase": phase,
            "detail": detail,
            "ts": time.time(),
        },
    )


# ─── context-overflow projection (the per-agent strategy) ───────


def _recent_n(rec: dict) -> int:
    try:
        n = int(rec.get("recent_n"))
    except (TypeError, ValueError):
        n = RECENT_N
    lo, hi = RECENT_N_RANGE
    return max(lo, min(n, hi))


class _Summarizer:
    """The side call that writes a fold's summary: a plain text completion over the
    backend provider (no tools — providers stream raw text). `max_tokens` bounds the
    output (§4: the summary budget); `last_cut` says whether the provider stopped AT
    that bound, so a cut-off summary is never kept as if it were whole."""

    last_cut = False

    def __init__(self, fn):
        self._fn = fn

    async def __call__(self, msgs: list[dict], max_tokens: int | None = None) -> str:
        text, self.last_cut = await self._fn(msgs, max_tokens)
        return text


def _make_summarizer(provider):
    """Closure over the backend provider: summarize a span of turns to a string."""

    async def _summarize(msgs: list[dict], max_tokens: int | None) -> tuple[str, bool]:
        rendered = "\n".join(
            f"{m.get('role', '?')}: {m.get('content', '')!s}" for m in msgs
        )
        prompt = [
            {
                "role": "system",
                "content": (
                    "Summarize the conversation excerpt below concisely, PRESERVING "
                    "names, decisions, facts, preferences, and unresolved tasks. "
                    "Output ONLY the summary."
                ),
            },
            {"role": "user", "content": rendered},
        ]
        parts: list[str] = []
        try:
            provider.last_stop_reason = None
        except AttributeError:
            pass
        async for chunk in provider.chat(prompt, max_tokens=max_tokens):
            if isinstance(chunk, str):
                parts.append(chunk)
        cut = getattr(provider, "last_stop_reason", None) == "max_tokens"
        # A REASONING MODEL THINKS OUT LOUD HERE TOO: unstripped, a fold's summary is
        # buried under the reasoning and reads as noise. Side calls strip like the
        # main turn.
        return without_thinking("".join(parts)), cut

    return _Summarizer(_summarize)


def _context_notice(
    strategy: str,
    summary: str | None,
    omitted_marker: bool,
    dropped_n: int,
    pinned: list[str] | None = None,
    stripped: int = 0,
    stripped_bytes: int = 0,
) -> dict:
    """The ONE canonical inbound context-notice — composed at the SEAM from a strategy's
    `Projection` artifact (no strategy fabricates its own turn anymore). A `role:user`
    turn (the role every backend reliably attends to). Carries the protocol affordances:
    `recall` to page dropped turns back, and persist-to-memory. Injected into the MODEL
    view ONLY — never the durable store."""
    lines = [
        f"[context-notice] Your conversation exceeded the window and was compacted "
        f"(strategy={strategy}, {dropped_n} earlier turn(s) dropped from THIS view)."
    ]
    if summary is not None:
        lines.append("Summary of the dropped span:\n" + summary)
    elif omitted_marker:
        lines.append("An earlier span was omitted in place.")
    if stripped:
        lines.append(
            f"{stripped} reply bod(ies) were STRIPPED to their tag+title line "
            f"({stripped_bytes} bytes left your view); grep finds a line in a body held "
            "this run — after a restart, re-read it from its origin."
        )
    if pinned:
        # POINT AT THE BLOCK, DO NOT REPEAT IT. The facts live in their own stable message
        # ABOVE this notice, in the cached prefix. Embedding them here would put the one
        # append-only thing in the system inside the most volatile one — the notice is
        # rebuilt with a fresh `dropped_n` at every cut, so a fact block carried in it
        # would be rewritten every time and take the whole cached prefix with it.
        lines.append(
            f"Your {len(pinned)} checkpointed fact(s) are in the FACTS block above, "
            "unchanged — they still bind."
        )
    lines.append(
        "The full transcript is intact in your memory store. To page dropped turns back, "
        "send your OWN id `<type>recall</type>` (optional `<query>`, `<limit>`). If the dropped span holds "
        "durable facts (names, decisions, preferences), persist them to your memory agent "
        "now via the send tool — the earlier turns are leaving your live view."
    )
    return {"role": "user", "content": "\n".join(lines)}


async def _project_context(
    rec,
    cfg: Backend,
    provider,
    self_id,
    kernel,
    client_id,
    messages,
    store_len: int,
):
    """Shape the assembled `messages` to fit the agent's token budget via its configured
    `context_strategy` (default `compact`), then prepend the ONE canonical context-notice
    and push a `context:compacted` event to the caller. Returns the projected message
    list, OR an `{error}` dict when even the system block + the live user turn won't fit
    (the `too_small` failsafe — a failfast that ALSO pushes a `context:too_small` event,
    NOT a fallback). Durable store untouched. Called ONCE per send, at turn entry.

    `store_len` is the DURABLE store's length behind `messages` (from `_assemble`) —
    the index the reaction cursor is recorded at. It is NOT derivable from `messages`,
    which is a deduped/trimmed view."""
    # THE BUDGET IN ESTIMATE UNITS: the record's budget, divided by how much the
    # endpoint's own count exceeded our estimate last turn (`_calib`).
    calib = _calib.get(self_id, 1.0)
    b = max(BUDGET_FLOOR, int(budget(rec) / calib)) if calib > 0 else budget(rec)
    if estimate_tokens(messages) <= int(b * TRIGGER):
        _projection[self_id] = {
            "fired": False,
            "trigger": TRIGGER,
            "calibration": round(calib, 3),
        }
        return messages
    system_block = messages[:1]
    body = messages[1:]
    if not body:
        _projection[self_id] = {"fired": False}
        return messages
    sys_tokens = estimate_tokens(system_block)
    body_budget = b - sys_tokens
    # The live user turn AND the prepended notice envelope are both non-negotiable — if
    # there's no room for BOTH, fail loud (so the trim below can never drop the live turn).
    if body_budget < estimate_one(body[-1]) + NOTICE_ENVELOPE_RESERVE:
        hint = (
            f"the system prompt ({sys_tokens} tok) leaves no room in the "
            f"{resolve_context_window(rec)}-token window for even one turn; "
            "reduce agents/menu or raise context_window"
        )
        _projection[self_id] = {"fired": False, "too_small": True}
        _compaction_mark.pop(self_id, None)
        await _to_caller(
            kernel,
            self_id,
            client_id,
            {
                "type": "context",
                "source": self_id,
                "ts": time.time(),
                "phase": "too_small",
                "detail": {
                    "context_window": resolve_context_window(rec),
                    "system_tokens": sys_tokens,
                    # NAME THE KILLER: which message was too big, recorded while
                    # it can still be asked.
                    "last_message_tokens": estimate_one(body[-1]),
                    "last_message_head": str(body[-1].get("content"))[:200],
                    "hint": hint,
                },
            },
        )
        return {"error": f"{cfg.name}: context_insufficient — " + hint}
    strat_name = rec.get("context_strategy", DEFAULT_STRATEGY)
    strat = get_strategy(strat_name)
    if strat is None:
        return {
            "error": f"{cfg.name}: unknown context_strategy {strat_name!r} "
            f"(valid: {strategy_names()})"
        }
    # `none`: never shrink. Refused HERE, before ProjectionCtx, any summarizer or model
    # call, and before any row or event is written, so nothing is dropped. The caller
    # returns this dict straight out of the turn.
    if strat_name == "none":
        return {
            "error": f"{cfg.name}: context full: {estimate_tokens(messages) // 1000}k of "
            f"{b // 1000}k tokens — clear, fold by hand, or raise context_window"
        }
    ctx = ProjectionCtx(
        budget=body_budget,
        recent_n=_recent_n(rec),
        summarize=_make_summarizer(provider),
        self_id=self_id,
        kernel=kernel,
        client_id=client_id,
        # Over the HEAD, not the body: a body rebuilt from the view array holds the
        # folds without the rows they hide.
        anchors=anchors_of(await _head(self_id, kernel)),
    )
    proj = await strat(body, system_block, rec, ctx)
    notice = _context_notice(
        strat_name,
        proj.summary,
        proj.omitted_marker,
        len(body) - len(proj.body),
        proj.pinned,
        proj.stripped,
        proj.stripped_bytes,
    )
    # THE STABLE PREFIX. System block, then the fact block, then everything volatile.
    # MEASURED on this stack (vLLM prefix cache, /metrics deltas): an APPEND after the
    # prefix reuses it in full (4752 of 6559 tokens, 72.3%), while any edit inside it —
    # or a cut taken from the front of it — reuses nothing and recomputes everything.
    # `halve` cuts from the front of the BODY every time it fires, so the only way any
    # prefix survives a run is if everything above the cut is append-only. That is what
    # this ordering buys, and it is why the block is its own message rather than part of
    # the notice.
    fact_block = (
        [
            {
                "role": "user",
                "content": "FACTS (checkpointed by you; append-only, later "
                "lines supersede earlier):\n" + "\n".join(proj.pinned),
            }
        ]
        if proj.pinned
        else []
    )
    # Single budget authority: the strategy reserved the notice envelope, but guard the
    # final fit anyway — trim oldest body turns (tool-pairing-safe). Never drop the last
    # (live) turn: the failsafe above guarantees room for [notice + live turn], so the
    # `len > 1` guard is a defensive backstop, not a fallback.
    # Trim from the MIDDLE, not the front. `out_body[0]` is the goal turn every
    # strategy deliberately pins (`compact`/`truncate` both put it first); the
    # last entry is the live turn. Taking from the front would silently undo the
    # pin — the exact regression the pin exists to prevent, one layer up.
    out_body = proj.body
    stable = system_block + fact_block
    while len(out_body) > 2 and estimate_tokens(stable + [notice] + out_body) > b:
        out_body = [out_body[0]] + out_body[2:]
    # Last resort, and only when nothing else is left to give: [goal, live] still
    # over budget. The live turn is what the model must answer, so the goal is
    # what goes — loudly, in the notice's own count, not silently.
    while len(out_body) > 1 and estimate_tokens(stable + [notice] + out_body) > b:
        out_body = out_body[1:]
    dropped_n = max(0, len(body) - len(out_body))
    _projection[self_id] = {
        "fired": True,
        "trigger": TRIGGER,
        "calibration": round(calib, 3),
        "reported_prompt_tokens": _reported.get(self_id),
        "strategy": strat_name,
        "kept_messages": len(out_body),
        "dropped_messages": dropped_n,
        "summarized": proj.summary is not None,
        "pinned_facts": len(proj.pinned),
        "stripped": proj.stripped,
        "stripped_bytes": proj.stripped_bytes,
        "folded": proj.folded,
        "unanchored": proj.unanchored,
        # What the model actually sees this turn — so "after reload it sees only the
        # fold" is measured from the view, never self-reported.
        "view_ids": [m.get("id") for m in out_body if m.get("id")][:200],
        "view_ids_truncated": sum(1 for m in out_body if m.get("id")) > 200,
    }
    # THE VIEW ARRAY, written by every projection: what the model sees, at what
    # resolution — so the next turn (and the next process) rebuilds the same bytes.
    await _save_view(
        self_id,
        kernel,
        [{"id": m["id"], "res": _res_of(m)} for m in out_body if m.get("id")],
        budget_=b,
        why=f"projection: {strat_name}",
    )
    if proj.folded:
        # "When it feels it folded, it runs context_recall" — at the start of the
        # next turn, over the new head. DEBOUNCED: a recall is a model call, and a
        # recall after EVERY fold batch would double the turn price. Every
        # RECALL_EVERY-th batch, or when a
        # batch folded a lot at once.
        _folds_since[self_id] = _folds_since.get(self_id, 0) + proj.folded
        if _folds_since[self_id] >= RECALL_EVERY:
            _folds_since[self_id] = 0
            _recall_due.add(self_id)
    # PRIVATE reaction cursor: the store index where THIS turn's new turns (incl. any
    # recall/memory reactions) will land — `_derive_reaction` scans from here.
    #
    # `store_len`, NOT `len(body) - 1`. `body` is the DEDUPED/trimmed model view and
    # `_derive_reaction` slices the FULL durable store; whenever `dedupe_stale_reads`
    # dropped whole turns the view index is short by that many, so the scan started
    # BEFORE the notice and scored an assistant turn the model wrote earlier as its
    # reaction to a compaction it had not yet seen.
    _compaction_mark[self_id] = {"fired_at_index": store_len}
    await _to_caller(
        kernel,
        self_id,
        client_id,
        {
            "type": "context",
            "source": self_id,
            "ts": time.time(),
            "phase": "compacted",
            "detail": {
                "strategy": strat_name,
                "dropped_messages": dropped_n,
                "kept_messages": len(out_body),
                "summarized": proj.summary is not None,
                "pinned_facts": len(proj.pinned),
                "stripped": proj.stripped,
                "stripped_bytes": proj.stripped_bytes,
                "folded": proj.folded,
                "unanchored": proj.unanchored,
            },
        },
    )
    return stable + [notice] + out_body


async def _run(
    self_id: str,
    user_text: str,
    kernel,
    client_id: str,
    cfg: Backend,
    system_prompt: str | None = None,
) -> dict:
    provider = await _build_provider(self_id, kernel, cfg)
    # THE CACHING MODE IS CHECKED FIRST: a record that asks for provider caching on a
    # backend that cannot cache is refused before any row is written — before the
    # owed recall, which writes a view and a recall row.
    mode, primer = caching.settings(kernel.get(self_id) or {})
    if caching.marks(mode) and not cfg.provider_cache:
        raise caching.unsupported(cfg.name)
    if self_id in _recall_due:
        _recall_due.discard(self_id)
        await _context_recall(self_id, {}, kernel, cfg, system_prompt, client_id)
    messages, store_len = await _assemble(
        self_id, user_text, kernel, client_id, system_prompt, mode=mode
    )
    # Context-overflow projection: shape what the MODEL sees this turn to fit the
    # window via the agent's `context_strategy` (default compact). Done ONCE at turn
    # entry. The window is read off the record EVERY turn, so a model switched to a smaller one
    # (`update_agent`) is fitted — folded — on its very next turn.
    projected = await _project_context(
        kernel.get(self_id) or {},
        cfg,
        provider,
        self_id,
        kernel,
        client_id,
        messages,
        store_len,
    )
    if isinstance(projected, dict):  # too_small failsafe / config error
        return projected
    messages = projected
    # THE PROVIDER'S MARKERS, flagged on the list it is handed: the primer's end and
    # the last fold's. Only provider_cache asks for them.
    if caching.marks(mode):
        messages = caching.mark(messages, primer)
    # ONE provider pass — no tool loop. The provider streams pure text; every chunk
    # is a content token routed to the caller. Any tool convention is the caller's
    # (an authored agent reads this completion + drives its own loop).
    content_parts: list[str] = []
    first_chunk = True
    model_messages = _render_for_model(messages)
    # A FRESH reading per pass: a provider that reports nothing this time must not
    # leave last pass's number standing.
    for field_ in ("last_prompt_tokens", "last_completion_tokens"):
        try:
            setattr(provider, field_, None)
        except AttributeError:
            pass
    # STREAMING PARTS: the same chunks, split into parts as they
    # are written — animation for the addressee, never action (`stream_parts`).
    splitter = PartSplitter(self_id)
    parts_out = _PartPublisher(kernel, self_id, client_id, re=current_task())
    if self_id in _current:
        _current[self_id]["parts"] = (splitter, parts_out)
    # GENERATION TIME — from the request to the stream's end (the `done` detail).
    gen_started = time.monotonic()
    try:
        async for chunk in provider.chat(model_messages):
            if first_chunk:
                # First text chunk — phase transitions thinking → streaming.
                await _emit_status(kernel, self_id, client_id, "streaming")
                first_chunk = False
            content_parts.append(chunk)
            # Accumulate so the `status` verb's snapshot includes everything streamed
            # so far, letting the UI rebuild a mid-stream view on page reload.
            if self_id in _current:
                _current[self_id]["text_so_far"] = (
                    _current[self_id].get("text_so_far", "") + chunk
                )
            await _to_caller(
                kernel,
                self_id,
                client_id,
                {"type": "token", "text": chunk, "source": self_id},
            )
            await parts_out.push(splitter.feed(chunk))
    except Exception:
        await parts_out.push(splitter.stop("error"), final=True)
        raise
    # A block still open when the model stopped generating is CUT: an open call is
    # never dispatched (`extract_tool_calls` skips it) and the turn's row says so.
    cut_call = splitter.open_call
    await parts_out.push(splitter.finish(), final=True)
    last_text = "".join(content_parts)
    reported = getattr(provider, "last_prompt_tokens", None)
    if isinstance(reported, int) and reported > 0:
        _calib[self_id] = reported / max(1, estimate_tokens(model_messages))
        _reported[self_id] = reported

    # A DEAD TURN — nothing left once the thinking is stripped — did not answer,
    # whatever the provider's finish reason says (a tiny thinking budget can end the
    # thought and stop cleanly with an empty answer). The ANSWER is discarded here —
    # never stored, never counted as an answer — and the turn is routed like any that
    # dispatched nothing: the no-call reminder rerolls it. Failing the turn outright
    # would end a long chain on its first dead turn.
    if not without_thinking(last_text).strip():
        # The INCOMING row is kept — only the empty answer is dropped. The turn
        # this one answered may be a reply batch carrying the results of the
        # model's own calls; unstored, the reroll would arrive with those results
        # gone and the model would re-ask for them.
        in_id, in_parent = _row_ids_for_incoming(self_id, user_text, kernel)
        await _save_history(
            self_id,
            kernel,
            [
                {
                    "role": "user",
                    "content": user_text,
                    "id": in_id,
                    "parent": in_parent,
                    **_notice_kind(self_id),
                }
            ],
        )
        await _emit_status(kernel, self_id, client_id, "done", reason="dead_turn")
        return {
            "response": last_text,
            "final": last_text,
            "client_id": client_id,
            "dead_turn": True,
        }

    # `status(phase='done')` IS the completion event. There is no second `done`
    # event: two events for one transition is two truths, and a subscriber that
    # watches the wrong one drifts silently.
    await _emit_status(
        kernel,
        self_id,
        client_id,
        "done",
        reason="ok",
        **usage_detail(
            provider,
            model_messages,
            last_text,
            time.monotonic() - gen_started,
            resolve_context_window(kernel.get(self_id) or {}),
        ),
    )

    # ROW IDS for this turn: the incoming row (a root, or a delivered reply turn that
    # names its parent on its tag) and the assistant row that answers it. The batch
    # token on this turn's reply tags is the assistant row's own hex, so `↑` on every
    # reply points at something the model can see.
    #
    # MINTED HERE, AT THE APPEND — not at turn start. The store returns an owner's rows
    # in ID order and the head cache keeps APPEND order; the two agree only while ids
    # are minted as rows are appended. Minted at turn start, a fold or view row saved
    # mid-turn (the projection) took a higher id yet landed first, and after a restart
    # the conversation came back in a different order than the agent had lived it.
    in_id, in_parent = _row_ids_for_incoming(self_id, user_text, kernel)
    message_id = _mint_row_id(self_id, kernel)
    # APPEND this send's user + assistant turns to the memory store.
    # NOT `messages[1:]` — that's the projected/trimmed view; the store keeps the
    # FULL conversation so context management (compaction/recall) works next turn.
    new_turns = [
        {
            "role": "user",
            "content": user_text,
            "id": in_id,
            "parent": in_parent,
            **_notice_kind(self_id),
        },
        # STRIPPED — the deliberation is not part of the conversation. The raw
        # text (with thinking) already went to `_route_reply` for parsing. A pass
        # the model stopped with a block open leaves the CUT MARKER instead, on top
        # of the context.
        {
            "role": "assistant",
            "content": (
                cut_marker(
                    last_text,
                    "model stopped generating",
                    [p for p in splitter.parts if p["kind"] == "call"],
                )
                if cut_call is not None
                else repair_calls(without_thinking(last_text))
            ),
            "id": message_id,
            "parent": in_id,
            **thinking_fields(last_text),
        },
    ]
    await _save_history(self_id, kernel, new_turns)
    return {
        "response": last_text,
        "final": last_text,
        "client_id": client_id,
        "message_id": message_id,
        # every closed call part (stream_id, body) — `_route_reply` stamps each
        # dispatched message with the stream it was previewed as
        "call_streams": list(splitter.closed_calls),
        # the model stopped generating with a block open — NOTHING of this pass
        # is sent (only a successful generation dispatches)
        "cut": cut_call is not None,
    }


# ─── verb factories (close over the per-backend cfg) ────────────


def _make_reflect(cfg: Backend):
    async def _reflect(id, payload, kernel):
        """Identity + model + endpoint + generating flag + context posture. No args."""
        rec = kernel.get(id) or {}
        out = {
            "id": id,
            "sentence": cfg.sentence,
            "model": rec.get("model"),
            "endpoint": rec.get("endpoint"),
            "context_window": resolve_context_window(rec),
            "context_strategy": rec.get("context_strategy", DEFAULT_STRATEGY),
            "memory_store_id": rec.get("memory_store_id"),
            # WHERE its key is read — `{store, ref}`, a pointer, never the secret. An
            # agent copying this one's record needs it (a keyed endpoint).
            "credentials": rec.get("credentials"),
            # A generation IS its kernel dispatch task — the backend holds no
            # task handle of its own. `_current[id]` present ⇔ in flight.
            "generating": id in _current,
        }
        if why := prompt_file.error_of(id):
            out["system_prompt_file_error"] = why
        if cfg.reflect_extra is not None:
            extra = cfg.reflect_extra(id, kernel)
            if inspect.isawaitable(extra):
                extra = await extra
            if extra:
                out.update(extra)
        return out

    return _reflect


# What an inquiry carries ON ITSELF: that it IS one and how it is answered. Delivered
# bare as `[from ai]` + the question, a model does the work, writes its verdict as
# prose and ends with `done`; one line in the system block is too far away.
_INQUIRY_HEAD = (
    "[inquiry from {asker} · re {pid} — do any work it needs, then answer with "
    '<call name="inquiry_answer"><answer>…</answer></call> before done: nothing else '
    "counts as your answer]"
)


def _make_inquiry(cfg: Backend):
    """`inquiry` on an AI: accepted at once, answered LATER.

    The human's bundle answers this verb from a record, at once; an AI answers it
    by working and THINKING, in its own time. Either way the asker reads the
    answer as `[reply to <pid> · <who> inquiry]` — the human's as the reply
    itself, the AI's as a later message under the same pid — so it never
    branches on who is at the other end.
    """

    async def _inquiry(id, payload, kernel):
        """args: text:str (req), kind:str? (text|single|multi|confirm), options:list?, action:dict?, request_id:str?. Sends THIS agent an inquiry: accepted at once ({accepted, re, request_id}); the answer it later gives with `inquiry_answer` — or the news that it ended without one — reaches the asker as a message under the inquiry's pid. The same verb a human answers; a sender writes one call for either."""
        q = payload.get("text")
        if not isinstance(q, str) or not q.strip():
            return {
                "error": "inquiry: `text` must be a non-empty string",
                "reason": "missing_arg",
                "arg": "text",
            }
        # FULLY ASYNC. The inquiry is ACCEPTED at once and runs
        # as its own chain on this agent; the answer — given in ANY turn of that
        # chain, after whatever work it needs — reaches the asker later as a
        # message under this call's pid (`_route_reply`) — never read off this
        # agent's FIRST turn only, so work before answering is allowed.
        asker = current_origin()
        pid = current_task()
        client = _safe_client(f"inquiry-{pid}")
        # `options`/`kind` ride in the text because a model reads prose, not a
        # schema; `action` is carried verbatim so an answering AI can see what it
        # is being asked to endorse.
        text = _INQUIRY_HEAD.format(asker=asker or "the operator", pid=pid) + "\n" + q
        opts = payload.get("options")
        if isinstance(opts, list) and opts:
            text += "\n\nOptions: " + ", ".join(str(o) for o in opts)
        act = payload.get("action")
        if isinstance(act, dict):
            text += "\n\nProposed action: " + json.dumps(act)
        _open_inquiries[(id, client)] = {
            "asker": asker,
            "pid": pid,
            "reply_client": _safe_client(
                payload.get("reply_client_id") or DEFAULT_CLIENT_ID
            ),
            "request_id": payload.get("request_id"),
        }
        _deliver_soon(kernel, id, text, client, payload.get("system_prompt"))
        return {"accepted": True, "re": pid, "request_id": payload.get("request_id")}

    return _inquiry


def _make_send(cfg: Backend):
    async def _send(id, payload, kernel):
        """args: text:str (req), client_id:str? (default 'cli'), system_prompt:str?, send_to:str?. Runs one turn on `text` and streams tokens to ONLY the caller — cli (stdout) for client_id='cli', else the browser tab whose WS filters by client_id. The conversation is this agent's own (its memory store). A FIFO lock serializes callers: one that waits gets a `status` event (phase='queued', detail.ahead), and its first `token` (or non-queued `status`) unqueues it. `system_prompt` REPLACES the built prompt for this delivery. `send_to` hands this pass's TEXT to that agent id as a fresh turn, once. `_call_stack` is reserved for the recursion guard. The model routes its own work with `<reply_to>` inside its calls. Returns {response, final, client_id}."""
        client_id = _safe_client(payload.get("client_id") or DEFAULT_CLIENT_ID)
        # Recursion guard — checked BEFORE the lock, so a cycle can never deadlock on
        # the per-backend FIFO lock (A→B→A would otherwise block on A's held lock).
        call_stack = payload.get("_call_stack")
        if not isinstance(call_stack, list):
            call_stack = []
        if id in call_stack:
            return {
                "error": f"{cfg.name}: cycle detected",
                "response": "",
                "cycle": call_stack + [id],
                "client_id": client_id,
            }
        if len(call_stack) >= MAX_CALL_DEPTH:
            return {
                "error": f"{cfg.name}: max call depth {MAX_CALL_DEPTH} reached",
                "response": "",
                "client_id": client_id,
            }
        system_prompt = payload.get("system_prompt")
        # NO STORE, NO TURN — said up front, not discovered mid-generation.
        try:
            _store_of(id, kernel)
        except MemoryStoreError as e:
            return {"error": str(e), "reason": "missing_arg", "client_id": client_id}
        # PAUSED: the message is held in the store, in order, and not run (#219).
        try:
            held = await _hold_if_paused(id, payload, kernel)
        except MemoryStoreError as e:
            return {"error": str(e), "reason": "memory_store", "client_id": client_id}
        if held is not None:
            return held
        if payload.get("late"):
            # A reply to a call a PREVIOUS incarnation made (see `_deliver`): recorded
            # in this agent's own store, never a turn.
            await _save_history(
                id,
                kernel,
                [
                    {
                        "role": "user",
                        "parent": None,
                        "kind": "late",
                        "content": "[late reply — answered a call made before this "
                        "agent last booted; recorded, not acted on]\n"
                        + str(payload.get("text", "")),
                    }
                ],
            )
            return {"late": True, "client_id": client_id}
        # Where this pass's TEXT goes when the model issues no call of its own.
        send_to = payload.get("send_to")
        if not isinstance(send_to, str) or not send_to.strip():
            send_to = None
        text = payload.get("text", "")
        # AN ANSWER UNDER AN INQUIRY'S PID settles what this agent was owed.
        # ONE CHAIN PER AGENT (#227): such an answer is a turn of the chain.
        # The settle happens before the row save and is safe in Python: an owed
        # answer is a chain_turn, never held, so no held message carries it.
        answered_owed = False
        if isinstance(text, str) and _awaiting.get(id):
            for owed in _ANSWER_TAG.findall(text):
                _awaiting[id].discard(owed)
                answered_owed = True
        # A message is TEXT whatever the caller typed: the tag grammar types a bare
        # number (`<text>586</text>` → 586), and "586" is a perfectly good message.
        if isinstance(text, (int, float)) and not isinstance(text, bool):
            text = str(text)
        # WHO SAID IT, TO WHOM, UNDER WHICH NUMBER. A message from another AGENT
        # opens with a header the KERNEL vouches for (the sender is the dispatch's
        # own context, never the sender's prose): `[from solver_1 · to solver_2,
        # solver_3 · msg 412 · re 398]`; `current_origin` is the caller the gate saw.
        # Without it a peer's message arrives anonymous, and a group message is
        # indistinguishable from a private one.
        sender = current_origin()
        if sender and sender != id:
            parts = [f"from {sender}"]
            to = payload.get("to")
            if isinstance(to, list) and to:
                parts.append("to " + ", ".join(str(t) for t in to))
            if payload.get("msg") is not None:
                parts.append(f"msg {payload.get('msg')}")
            if payload.get("re") is not None:
                parts.append(f"re {payload.get('re')}")
            # the stream this message was previewed as — what an AI consumer that
            # watched it stream matches its draft to
            if payload.get("stream_id") is not None:
                parts.append(f"stream {payload.get('stream_id')}")
            text = f"[{' · '.join(parts)}]\n{text}"
        send_id = _new_send_id()
        # ONE CHAIN PER AGENT (#227): a message that lands while this agent's chain
        # is alive is HELD — stored as a row, read by the chain's next pass — unless
        # it is one of the chain's own turns (it carries the chain token, the agent
        # sent it to itself, or it answers an inquiry the agent is owed).
        chain = _chain_of(id)
        token = payload.get("chain")
        chain_turn = (
            (isinstance(token, str) and bool(token) and token == chain["token"])
            or sender == id
            or answered_owed
        )
        if chain["alive"] and not chain_turn:
            # The SAME row the normal incoming path stores, so a held message folds
            # and recalls exactly like any other turn.
            in_id, in_parent = _row_ids_for_incoming(id, text, kernel)
            await _save_history(
                id,
                kernel,
                [{"role": "user", "content": text, "id": in_id, "parent": in_parent}],
            )
            # A held send RETURNS THE ANSWER of the pass that reads it.
            fut = asyncio.get_running_loop().create_future()
            _push_held_or_wake(kernel, id, client_id, fut, system_prompt)
            await _emit_status(
                kernel, id, client_id, "queued", send_id=send_id, ahead=0, held=True
            )
            try:
                async with asyncio.timeout(SEND_TIMEOUT):
                    return await fut
            except TimeoutError:
                return {
                    "error": f"send: timeout after {SEND_TIMEOUT}s",
                    "client_id": client_id,
                }
        entry = {
            "client_id": client_id,
            "text": text,
            "send_id": send_id,
            "queued_at": time.time(),
        }
        # A notice is this agent's own harness text: honoured only when the agent
        # sent it to itself, so no peer can hide a message from a person's view.
        if payload.get("notice") and sender in (id, None):
            entry["notice"] = True
        _enqueue(id, entry)

        lock = _lock_for(id)
        # Best-effort contention detection — at this point our entry is in
        # the queue. If lock.locked(), there's at least one other entry
        # ahead of us (the holder + any others queued). Compute `ahead` as
        # the queue position minus 1.
        if lock.locked():
            ahead = max(0, len(_queue.get(id, [])) - 1)
            await _emit_status(
                kernel, id, client_id, "queued", send_id=send_id, ahead=ahead
            )

        # NOT `async with lock`. Our entry is already in the queue, and the WAIT is
        # cancellable: an item `timeout` or an operator `cancel` landing here raises
        # straight out of `acquire()`, and `async with`'s only cleanup is releasing a
        # lock it never took — the queue row stayed forever, so `status` reported a
        # phantom `others_pending` and every later caller's `ahead` was off by one.
        # The dequeue is the acquire's own failure path.
        try:
            await lock.acquire()
        except BaseException:
            _dequeue_send(id, send_id)
            raise
        try:
            # We've got the lock — pop ourselves from the queue and become
            # the in-flight entry. _set_current establishes phase='thinking',
            # the started_at timestamp, and the kernel task id we run as.
            _dequeue_send(id, send_id)
            # A REPLY IN FLIGHT ACROSS A STOP: it carries the token of a chain that
            # has ended, so it is recorded as a late row and runs nothing.
            if (
                isinstance(token, str)
                and token
                and token != _chain_of(id)["token"]
            ):
                await _save_history(
                    id,
                    kernel,
                    [
                        {
                            "role": "user",
                            "parent": None,
                            "kind": "late",
                            "content": "[late reply — answered a call of a chain "
                            "that has ended; recorded, not acted on]\n" + str(text),
                        }
                    ],
                )
                return {"late": True, "client_id": client_id}
            _set_current(id, entry)
            # ONE CHAIN PER AGENT (#227): this pass owns the chain. A new chain mints
            # its token; this pass TAKES the held messages (it reads their rows) and
            # answers each of them with its own reply below.
            chain = _chain_of(id)
            if not chain["alive"]:
                chain["token"] = _new_send_id()
            chain["alive"] = True
            answering, chain["held"] = chain["held"], []
            await _emit_status(kernel, id, client_id, "thinking")
            try:
                # The generation runs INLINE in this dispatch — the backend spawns
                # no task of its own. It doesn't need to: a `send` IS a kernel task
                # (one `ps` row), so the kernel's `cancel` aborts it, running our
                # `on_cancel` hook first. That abort raises a CancelledError right
                # here, which we do NOT catch: it unwinds the provider stream (the
                # HTTP request dies with it) and releases the FIFO lock, and the
                # substrate resolves the blocked caller's `send` to
                # `{"cancelled": True}`. Closing the turn out for the CLIENT is the
                # one thing the abort cannot do — that is `on_cancel`'s job.
                # TRANSPORT RETRIES (#198 part 2): a pass that failed on the LINK is
                # re-run, visibly, up to the record's `transport_retries`; every other
                # failure is raised to the handlers below untouched. The timeout is per
                # pass, as in Rust.
                retries = _transport_retries(kernel, id)
                attempt = 0
                while True:
                    try:
                        async with asyncio.timeout(SEND_TIMEOUT):
                            reply = await _run(
                                id,
                                text,
                                kernel,
                                client_id,
                                cfg,
                                system_prompt,
                            )
                        break
                    except ProviderError as e:
                        if not (e.transport and attempt < retries):
                            raise
                        attempt += 1
                        # The failed pass's partial stream is not the next pass's text.
                        if id in _current:
                            _current[id]["text_so_far"] = ""
                        # A visible row: the model and the operator both read the retry.
                        await _save_history(
                            id,
                            kernel,
                            [
                                {
                                    "role": "user",
                                    "kind": "notice",
                                    "parent": None,
                                    "content": (
                                        f"[retry {attempt}/{retries}] the link dropped "
                                        f"({e}) — retrying the turn"
                                    ),
                                }
                            ],
                        )
                        await _emit_status(
                            kernel,
                            id,
                            client_id,
                            "retrying",
                            attempt=attempt,
                            of=retries,
                            error=str(e),
                        )
                        # Backoff 2 s, 4 s, 8 s …; a cancel lands in this sleep and
                        # unwinds through the handlers below.
                        await asyncio.sleep(RETRY_BACKOFF_BASE * 2 ** (attempt - 1))
            except TimeoutError:
                await _emit_status(kernel, id, client_id, "done", reason="timeout")
                # The send ceiling ends the turn with an error row too (#177).
                message = f"send: timeout after {SEND_TIMEOUT}s"
                stored = await _store_failed_turn(
                    id, kernel, text, bool(entry.get("notice")), message
                )
                reply = {
                    "error": _with_store_outcome(message, stored),
                    "client_id": client_id,
                }
            except MemoryStoreError as e:
                # FAIL FAST: the store refused or is gone — no turn, said so.
                await _emit_status(
                    kernel, id, client_id, "done", reason="error", error=str(e)
                )
                reply = {
                    "error": str(e),
                    "reason": "memory_store",
                    "client_id": client_id,
                }
            except FoldSummaryError as e:
                # FAIL LOUD: a span's summary would not fit its
                # budget after every re-summarise — no turn on an unfolded view.
                await _emit_status(
                    kernel, id, client_id, "done", reason="error", error=str(e)
                )
                reply = {
                    "error": str(e),
                    "reason": "fold_summary",
                    "client_id": client_id,
                }
            except prompt_file.PromptFileError as e:
                # FAIL LOUD: a system_prompt_file is set and unusable — no turn
                # on another block, said so.
                await _emit_status(
                    kernel, id, client_id, "done", reason="error", error=str(e)
                )
                reply = {
                    "error": str(e),
                    "reason": "system_prompt_file",
                    "client_id": client_id,
                }
            except asyncio.CancelledError:
                # ONE CHAIN PER AGENT (#227): a stopped chain ends and wakes nothing;
                # the held sends this pass took get the cancelled shape.
                _end_chain(id)
                _answer_held(answering, {"held": True, "cancelled": True})
                raise
            except Exception as e:
                # THE TRACEBACK, on stderr, every time: a turn that ends in
                # `reason='error'` with only `str(e)` is undiagnosable from a
                # transcript with no frame to name where.
                traceback.print_exc()
                # A FAILED PASS stores its own error row (#177): nothing ends
                # silently. A refused record (the branches above) opened no pass.
                message = str(e)
                stored = await _store_failed_turn(
                    id, kernel, text, bool(entry.get("notice")), message
                )
                if stored is not None:
                    message = _with_store_outcome(message, stored)
                    e.args = (message,)
                await _emit_status(
                    kernel, id, client_id, "done", reason="error", error=message
                )
                # ONE CHAIN PER AGENT (#227): a pass that raised is not routed — its
                # chain ends here, and the held sends it took get its error.
                _answer_held(answering, {"error": message})
                _chain_ended(kernel, id, system_prompt)
                raise
            finally:
                # Idempotent: on the cancel path `on_cancel` already took the
                # entry (BEFORE the abort), which is what let it close the turn
                # out for the client while this coroutine was still alive.
                _clear_current(id)
        finally:
            lock.release()

        # OUTSIDE the lock — released by now, so a self-addressed follow-up can
        # acquire it. Detached (`spawn_task`) so THIS dispatch returns to its caller
        # immediately: awaiting a self-addressed send here would block on a lock we
        # just held and deadlock until `SEND_TIMEOUT`. The task row makes the chain
        # visible in `ps` and reachable by `cancel`.
        # The closed call parts feed routing only — never the caller's reply.
        call_streams = reply.pop("call_streams", []) if isinstance(reply, dict) else []
        cut = bool(reply.pop("cut", False)) if isinstance(reply, dict) else False
        # ONE CHAIN PER AGENT (#227): the pass that read the held messages answers
        # each of them with its own reply, under that sender's own client_id.
        if isinstance(reply, dict):
            _answer_held(answering, reply)
        if isinstance(reply, dict) and not reply.get("error"):
            coro = _route_reply(
                id,
                kernel,
                reply.get("response", ""),
                client_id,
                send_to,
                system_prompt,
                # The AI layer's per-tool-call deadline. The kernel defaults
                # nothing; this is where a default belongs — the same knob
                # that bounds a generation bounds a call to one.
                SEND_TIMEOUT,
                # The text this turn ANSWERED — what tells routing whether
                # this was a chain turn (a delivered reply) or a chat turn.
                text,
                message_id=reply.get("message_id"),
                dead_turn=bool(reply.get("dead_turn")),
                call_streams=call_streams,
                cut=cut,
            )
            # ONE CHAIN PER AGENT (#227): the routing reports whether the chain goes
            # on; where it does not, the chain ends (and held messages get a wake).
            kernel.ctx.spawn_task(
                id, "route_reply", _route_then_settle(kernel, id, system_prompt, coro)
            )
        else:
            # ONE CHAIN PER AGENT (#227): a pass that is NOT routed ends its chain
            # here, so messages held meanwhile are woken once.
            _chain_ended(kernel, id, system_prompt)
        return reply

    return _send


def _digest(r: dict) -> dict:
    return {
        "id": r.get("id"),
        "parent": r.get("parent"),
        "kind": r.get("kind") or r.get("role"),
        "role": r.get("role"),
        "head": (str(r.get("content", ""))[:120]).replace("\n", " "),
        "bytes": len(str(r.get("content", ""))),
        **({"has_thinking": True} if r.get("thinking") else {}),
    }


def _make_history(cfg: Backend):
    async def _history(id, payload, kernel):
        """args: fold:int? (a fold or clear id — list ITS members instead), limit:int? (default: as many as fit inline), after:int? (a row id — page forward from it). The conversation at DIGEST resolution — {id, parent, kind, role, head, bytes, has_thinking?} per row, oldest first, paged. With `fold`, that fold's title + summary and its members (one level down; a member that is itself a fold opens the same way). A row's text is behind `recall` / `grep`. Returns {rows, next, total, fold?}."""
        fold_id = _as_id(payload.get("fold"))
        head: dict | None = None
        if payload.get("fold") is not None:
            if fold_id is None:
                return {
                    "error": "history: `fold` must be a fold id (int)",
                    "reason": "bad_arg",
                    "arg": "fold",
                }
            everything = await _load_history(id, kernel, behind=True)
            by_id = {r.get("id"): r for r in everything if r.get("id")}
            f = by_id.get(fold_id)
            if f is None or f.get("kind") not in ("fold", "clear"):
                return {
                    "error": f"history: {fold_id} is not a fold in your conversation",
                    "reason": "not_found",
                    "arg": "fold",
                }
            rows = [by_id[m] for m in (f.get("refs") or []) if m in by_id]
            head = {
                "id": fold_id,
                "kind": f.get("kind"),
                "title": f.get("title"),
                "summary": f.get("body") or "",
                "members": len(f.get("refs") or []),
            }
        else:
            rows = await _load_history(id, kernel)
        digest = [_digest(r) for r in rows if r.get("id")]
        # CHAT CARDS (#339): embed / unembed rows have no role, so `_load_history` drops
        # them; the page redraws them from here, in their place among the turns.
        card_count = 0
        if payload.get("fold") is None:
            cards = [
                _digest(r)
                for r in await _embed_mod.card_rows(id, kernel)
                if r.get("id")
            ]
            card_count = len(cards)
            if cards:
                digest = sorted(digest + cards, key=lambda d: d["id"])
        try:
            limit = int(payload.get("limit") or 0)
        except (TypeError, ValueError):
            limit = 0
        after = _as_id(payload.get("after"))
        if after is not None:
            ids = [d["id"] for d in digest]
            digest = digest[ids.index(after) + 1 :] if after in ids else []
        cap = _inline_cap(kernel.get(id) or {})
        kept, truncated = _fit_reply(digest, cap, limit or None)
        out = {
            "rows": kept,
            "next": kept[-1]["id"] if truncated and kept else None,
            "total": len(rows) + card_count,
        }
        if head is not None:
            out["fold"] = head
        return out

    return _history


async def _embed(id, payload, kernel):
    """args: id:str (req) — a HOST agent that has a `view` record. Shows that agent's panel as a CARD in this chat, at this place in the conversation; the card is stored as a row, so it survives a page reload and a kernel restart, and `history` lists it as kind `embed` (the model's prompt never holds it). This is a chat card, not a canvas cell. When you create an agent to embed, design its panel FOR THE CHAT WINDOW: it fills 100% of the column width (don't set a width), define its height well, at most 1200 px (CHAT_CARD_MAX_HEIGHT). Readable at a glance: one purpose per card, no tiny text, no wide tables. Keep it live with events, not polling. HOW: create_agent {view: {kind: html, bytes_file: panel.html, calls: [get_html, watch]}, callable_by: {user: [get_html, watch]}}, then embed {id}. The same checks as the canvas's `place {backend_id}`: the agent must exist (`not_found`), have a `view` record (`no_view`: a default chat or terminal view is not a panel), and let the user call every verb its panel calls plus `watch` (`grants_missing: {<id>: [verbs]}` with the callable_by fix in `hint`). It creates nothing and moves nothing: the same agent can be a card here and a cell on a canvas at once. Embedding an agent already shown answers {already:true}. The reply carries the card's box, {width: '100%', max_height: 1200}; the chat page measures the panel it draws and, when it overflows or is taller than the cap, tells you to redo the panel. Returns {embedded:id, row, card, view, hint}."""
    return await _embed_mod.embed(id, payload, kernel)


async def _unembed(id, payload, kernel):
    """args: id:str (req). Takes the card of agent `id` out of this chat: a tombstone row, so it stays gone after a reload (a later `embed` shows it again). Works the same for a live agent and a deleted one, so it also clears a '[deleted from canvas]' placeholder. It NEVER touches the agent — deleting it stays the kernel's `delete_agent`. Returns {unembedded:id} ({was_embedded:false} when no such card was shown)."""
    return await _embed_mod.unembed(id, payload, kernel)


# ─── the on_cancel bundle hook (shared by every backend) ────────

# What every backend's docs say INSTEAD of an `interrupt` verb — the substrate's
# universal cancel, so a caller needs no bundle-private knowledge to stop a turn.
# Served by `docs` (tier detailed), with EMITS and NOTES, via each backend module.
CANCEL = (
    "No `interrupt` verb — an in-flight `send` IS a kernel task. Kill it with the "
    "substrate's universal system verbs: send 'kernel' <type>ps</type> for the live task "
    "table, find the row {agent:'<this agent>', verb:'send'}, then send 'kernel' "
    "<type>cancel</type> <task_id>n</task_id>. The turn closes with status(phase='done', "
    "reason='cancelled') + done, its HTTP stream and FIFO lock are released, and the "
    "blocked caller's `send` resolves to {cancelled:true}. Cancel kills the task's whole "
    "SUBTREE, so any work the turn spawned dies with it."
)

EMITS = {
    "token": "{type:'token', text:str, source, client_id} — streaming chunk, routed ONLY to the caller: cli (stdout) when client_id='cli', else this agent's own inbox (a browser filters by client_id).",
    "status": "{type:'status', source, client_id, ts, phase:'queued'|'thinking'|'streaming'|'retrying'|'done', detail:{send_id, started_at, queue_depth, ...}} — the one channel for phase transitions. On done, detail.reason='ok'|'cancelled'|'timeout'|'error'|'awaiting'|'dead_turn' (only thinking — discarded, rerolled)|'gave_up' (MAX_SILENT_TURNS call-less turns in a row; detail.error says so). On queued, detail.ahead. On retrying, detail.attempt, detail.of and detail.error: a transport failure is re-run (transport_retries), each retry a visible row.",
    "context": "{type:'context', source, client_id, ts, phase:'compacted'|'too_small', detail} — compacted: {strategy, dropped_messages, kept_messages, summarized}, the view was cut to fit the window; too_small: {context_window, system_tokens, hint}, even the system block + one turn won't fit and the model is NOT called. Pull counterpart: `context_status`.",
}

NOTES = {
    "concurrency": "A FIFO lock around `send`: one generation at a time; other callers wait and get a `queued` status. `reflect`/`history`/`status` skip the lock.",
}


async def _rows(id, payload, kernel):
    """args: after:int? (a row id — the page starts after it), limit:int?. A VIEW of this agent's conversation exactly as the agent HOLDS it in memory — its loaded head, in the order it lived it; no store read, no transform, so what a UI draws is what the agent has. Before the agent has loaded its head (a fresh boot, before its first turn) it holds nothing: {rows:[], loaded:false}. Returns {rows, next, total, loaded}."""
    head = _mem(kernel)["heads"].get(id)
    if head is None:
        return {"rows": [], "next": None, "total": 0, "loaded": False}
    rows = [dict(r) for r in head]
    after = payload.get("after")
    if after is not None:
        at = next((n for n, r in enumerate(rows) if r.get("id") == after), None)
        rows = rows[at + 1 :] if at is not None else []
    limit = payload.get("limit")
    page = rows[:limit] if isinstance(limit, int) and limit > 0 else rows
    more = len(page) < len(rows)
    return {
        "rows": page,
        "next": page[-1].get("id") if more and page else None,
        "total": len(head),
        "loaded": True,
    }


async def _stop(id, payload, kernel):
    """args: stream_id:str? (a part of the generation in flight). STOP this agent's in-flight generation — how a receiver of its streaming parts stops it: the provider stream closes, open parts abort, the turn keeps a `[stopped by <you>]` marker. Returns {stopped:bool}."""
    cur = _current.get(id)
    if cur is None or cur.get("task_id") is None:
        return {"stopped": False}
    sid = payload.get("stream_id")
    if sid is not None:
        parts = cur.get("parts")
        if parts is None or sid not in {p["stream_id"] for p in parts[0].parts}:
            return {"stopped": False}
    cur["stopped_by"] = current_origin() or "operator"
    # ONE CHAIN PER AGENT (#227): stop means stop. The chain ends and the held
    # messages are dropped from the wake list (their rows stay stored); no wake.
    _end_chain(id)
    res = await kernel.ctx.cancel_task(cur["task_id"])
    if not (isinstance(res, dict) and res.get("cancelled")):
        # The kernel did not find the turn's task: close the turn ourselves, then
        # abort the generation that is still running.
        gen = _generations.get(id)
        await on_cancel(id, cur["task_id"], kernel)
        if gen is not None and gen is not asyncio.current_task():
            gen.cancel()
    return {"stopped": True}


# ── PAUSE / RESUME (#219) ───────────────────────────────────────────────────
# A paused agent stops its generation and HOLDS every incoming message, in order,
# in its memory store. The state is rows: `{kind:"pause", paused_since, reason}`,
# one `{kind:"held", payload}` per held message, and `{kind:"resume", role:"user"}`
# once they are released. The rows live in the store, so a restart reads them back.


async def _pause_view(self_id: str, kernel) -> tuple[bool, float | None, list]:
    """(paused, paused_since, held payloads) read off the store head. The last
    `pause`/`resume` row decides whether the agent is paused; the `held` rows after
    the last `resume` are the queue, in arrival order."""
    paused, since, held = False, None, []
    for r in await _refresh_head(self_id, kernel):
        kind = r.get("kind")
        if kind == "pause":
            paused, since = True, r.get("paused_since")
        elif kind == "resume":
            paused, since, held = False, None, []
        elif kind == "held" and "payload" in r:
            held.append(r["payload"])
    return paused, since, held


async def _hold_if_paused(self_id: str, payload: dict, kernel) -> dict | None:
    """THE HOLD CHECK at the top of message intake. Paused: the message is appended to
    the held queue (a row) and not run — its reply is returned. Not paused: None."""
    paused, _since, held = await _pause_view(self_id, kernel)
    if not paused:
        return None
    await _save_history(self_id, kernel, [{"kind": "held", "payload": payload}])
    return {"held": len(held) + 1, "paused": True}


async def _pause(id, payload, kernel):
    """args: reason:str? (why — kept on the pause row). PAUSE this agent: stop its in-flight generation and HOLD every message sent to it, in order, until `resume`. Already paused: a no-op. Returns {paused:bool, held:int}."""
    paused, _since, held = await _pause_view(id, kernel)
    if paused:
        return {"paused": True, "held": len(held)}
    # The stop writes the `[stopped by …]` marker and closes the open parts.
    await _stop(id, {}, kernel)
    await _save_history(
        id,
        kernel,
        [
            {
                "kind": "pause",
                "paused_since": time.time(),
                "reason": payload.get("reason"),
            }
        ],
    )
    return {"paused": True, "held": 0}


async def _resume(id, payload, kernel):
    """args: note:str? (text added to the resumed row). RESUME this agent: write one resumed row, then release the held messages in order, each as a fresh turn after it. Not paused: a no-op. Returns {resumed:bool, released:int}."""
    paused, since, held = await _pause_view(id, kernel)
    if not paused:
        return {"resumed": False, "released": 0}
    started = since if isinstance(since, (int, float)) else time.time()
    secs = max(0.0, time.time() - started)
    content = f"[resumed after a pause of {secs:.0f}s]"
    note = payload.get("note")
    if isinstance(note, str):
        content += " " + note
    await _save_history(
        id, kernel, [{"kind": "resume", "role": "user", "content": content}]
    )

    async def _release() -> None:
        # Each held message is a turn of its own, in arrival order, after the
        # resumed row. Released as a kernel task so this dispatch returns at once;
        # a send takes the per-agent FIFO lock and must not be awaited here.
        for p in held:
            try:
                await kernel.send([{"target": id, "payload": p}])
            except Exception:  # one held turn failing must not drop the rest
                continue

    _ctx(kernel).spawn_task(id, "resume_release", _release())
    return {"resumed": True, "released": len(held)}


async def on_cancel(id, task_id, kernel):
    """The `on_cancel` bundle hook, SHARED by every backend (each re-exports this
    module-level name, so the substrate discovers it exactly like `on_delete`).

    The kernel is killing `task_id` and will abort this generation's coroutine the
    moment we return. That abort frees the provider's HTTP stream and the FIFO lock
    on its own — what it CANNOT do is tell the client the turn ended. So that is
    exactly what happens here, while the task is still alive: take the in-flight
    entry (`status`/`reflect` would otherwise report a phantom generation forever)
    and emit the final `status(done, reason='cancelled')` + `done` the client is
    waiting on.

    No-op when the cancelled task is some OTHER dispatch on this agent (a `history`,
    a `reflect`) — matched on the task id the generation stamped on `_current`."""
    entry = _take_current_for_cancel(id, task_id)
    if entry is None:
        return
    client_id = entry.get("client_id", DEFAULT_CLIENT_ID)
    # THE CUT: every open part ABORTS for its watchers, and the
    # turn leaves the incoming message + a marker row — never a half answer — so the
    # model's next turn knows what it was asked, what it had visibly said, and who
    # stopped it. Nothing of a stopped pass was dispatched.
    parts = entry.get("parts")
    unsent: list[dict] = []
    if parts is not None:
        splitter, publisher = parts
        unsent = [p for p in splitter.parts if p["kind"] == "call"]
        await publisher.push(splitter.stop("stopped"), final=True)
    # A HOST STOP: the kernel is draining — this turn was cut by
    # the stop, not by anyone's cancel. Its cut row is marked `interrupted` (with
    # the client it streamed to), and the next boot resumes it once.
    stopping = bool(getattr(_ctx(kernel), "draining", False))
    if entry.get("text") is not None:
        who = "the host stopping" if stopping else entry.get("stopped_by") or "cancel"
        in_id, in_parent = _row_ids_for_incoming(id, entry["text"], kernel)
        so_far = entry.get("text_so_far", "")
        # A STOPPED TURN SAYS SO IN A USER ROW, never as the model's answer (#232):
        # the visible text streamed so far (if any) is the assistant row; the notice
        # naming who stopped it is a `stopped` user row after it.
        shown, notice = stop_rows(so_far, who, unsent)
        rows = [
            {
                "role": "user",
                "content": entry["text"],
                "id": in_id,
                "parent": in_parent,
                **({"kind": "notice"} if entry.get("notice") else {}),
            }
        ]
        prev = in_id
        if shown is not None:
            prev = _mint_row_id(id, kernel)
            rows.append(
                {
                    "role": "assistant",
                    "content": shown,
                    "id": prev,
                    "parent": in_id,
                    **thinking_fields(so_far),
                }
            )
        rows.append(
            {
                "role": "user",
                "kind": "stopped",
                "content": notice,
                "id": _mint_row_id(id, kernel),
                "parent": prev,
                **({"interrupted": {"client_id": client_id}} if stopping else {}),
            }
        )
        await _save_history(id, kernel, rows)
    await _emit_status(
        kernel,
        id,
        client_id,
        "done",
        reason="cancelled",
        send_id=entry.get("send_id"),
        started_at=entry.get("started_at"),
    )


async def on_delete(agent) -> None:
    """The `on_delete` bundle hook, SHARED by every backend (each re-exports it,
    like `on_cancel`; vllm wraps it to drop its own table too). Drops every row
    this module keeps for the deleted id, so an id re-created later starts clean
    and nothing leaks: its rows in the memory store (its conversation, folds and
    search index; `delete_agent keep_memory:true` keeps those), the FIFO lock and
    queue, the in-flight entry, the projection and its marks, the fold and recall
    counters, the calibration, the silent streaks, the strip set, and this
    kernel's head, bodies and watchers.

    NOT `_incarnation`: it stays monotonic across a re-create, so a reply to a
    call the deleted agent made never becomes a live turn of the new one."""
    from ai_core.strategies.strip import _stripped

    id = agent.id
    _reset_chain(id, "send: the agent was deleted or shut down")
    await _purge(id, agent)
    for table in (
        _locks,
        _queue,
        _current,
        _projection,
        _compaction_mark,
        _view_source,
        _folds_since,
        _calib,
        _reported,
        _inflight,
        _stripped,
        _chains,
        _generations,
        _running_jobs,
    ):
        table.pop(id, None)
    _recall_due.discard(id)
    for chain in [c for c in _silent if c[0] == id]:
        del _silent[chain]
    for chain in [c for c in _open_inquiries if c[0] == id]:
        del _open_inquiries[chain]
    _awaiting.pop(id, None)
    for table in _mem(agent).values():
        table.pop(id, None)
    await prompt_file.drop(id, agent)


async def _status(id, payload, kernel):
    """args: client_id:str? — when provided, returns a privacy-filtered snapshot: full text only for this client's queue entries / current generation; other clients collapse to `others_pending` (an integer count). Without client_id the snapshot redacts text everywhere. Used by chat UIs to rebuild queued/in-flight state on page reload (and for headless polling). Returns {source, client_id, generating, current:{phase, send_id, started_at, elapsed, is_mine, text?, text_so_far?, last_tool?}|null, mine_pending:[{send_id,text,queued_at}], others_pending:int}."""
    cid = payload.get("client_id")
    if cid:
        cid = _safe_client(cid)
    return _status_snapshot(id, cid)


def _recall_render(m: dict) -> str:
    """Render ONE stored turn for a recall reply — WHOLE, never truncated.

    Turns are pure text (tool calls are inline `<call>` elements, replies `[reply to …]` blocks
    text), so this only serializes a non-str content. The page `limit` is the bound
    that belongs here: recall exists to hand back a detail the model no longer has,
    and a per-turn cap silently drops the very part it went looking for — the caller
    says how many turns it wants, and each one arrives intact."""
    content = m.get("content")
    s = content if isinstance(content, str) else json.dumps(content, default=str)
    return s


async def _recall(id, payload, kernel):
    """args: query:str? (substring of a serialized turn, tool args and replies included), limit:int? (default 20, max 100), before:int? (store index — page backward). Pages turns back from the DURABLE store — the FULL conversation, so anything compaction dropped is one call away. Read-only. A page always fits INLINE; trimmed from the oldest end when it would not, with the index to continue from. Returns {messages:[{index,role,content}], total, truncated, note?}."""
    full = await _load_history(id, kernel, behind=True)
    q = str(payload.get("query") or "").lower().strip()
    try:
        limit = int(payload.get("limit") or 20)
    except (TypeError, ValueError):
        limit = 20
    limit = max(1, min(limit, 100))
    before = payload.get("before")
    indexed = list(enumerate(full))
    if isinstance(before, int) and not isinstance(before, bool):
        indexed = [(i, m) for i, m in indexed if i < before]
    if q:
        indexed = [
            (i, m) for i, m in indexed if q in json.dumps(m, default=str).lower()
        ]
    truncated = len(indexed) > limit
    page = indexed[-limit:]
    # A PAGE MUST FIT THE WINDOW IT IS GOING INTO, or `too_small` fires as a failfast
    # and the chain dies. The page is trimmed from the
    # OLDEST end (the newest of what was asked for is kept) and says so, with the index
    # to page further back from — the caller loses a page, never the run.
    get = getattr(kernel, "get", None)
    cap = _inline_cap((get(id) if get else None) or {})
    trimmed = 0

    def _serialized(pg) -> int:
        # THE UNIT THE DELIVERY MEASURES: the whole reply, serialized, as one user turn
        # (`_route_reply` dehydrates on exactly that). Sizing the inner messages alone
        # would let a recall slip past the cap and come back as a stub. Margin as in
        # `_fit_reply`.
        body = render_reply(
            {
                "messages": [
                    {"index": i, "role": m.get("role"), "content": _recall_render(m)}
                    for i, m in pg
                ],
                "total": len(indexed),
                "truncated": True,
                "note": "page trimmed by 999 older turn(s) to fit your window (cap "
                f"{cap} tok); pass before=999999 to page further back.",
            }
        )
        return estimate_one({"role": "user", "content": body})

    while len(page) > 1 and _serialized(page) > int(cap * 0.8):
        page = page[1:]
        trimmed += 1
    reply = {
        "messages": [
            {"index": i, "role": m.get("role"), "content": _recall_render(m)}
            for i, m in page
        ],
        "total": len(indexed),
        "truncated": truncated or trimmed > 0,
    }
    if trimmed:
        reply["note"] = (
            f"page trimmed by {trimmed} older turn(s) to fit your window (cap "
            f"{cap} tok); pass before={page[0][0]} to page further back."
        )
    if not page:
        # An empty page with no reason reads as "your history is lost" — the opposite of
        # what it usually means. Say why, so the caller can tell the cases apart.
        proj = _projection.get(id)
        if q and full:
            reply["note"] = f"query {q!r} matched none of the {len(full)} stored turns."
        elif not (proj or {}).get("fired"):
            reply["note"] = (
                "nothing has been compacted away — recall pages back turns that "
                "compaction DROPPED, and none have been. If you can see this reply, "
                "your live context already holds the whole conversation."
            )
        else:
            reply["note"] = "no stored turns in this page."
    return reply


async def _derive_reaction(id, kernel) -> dict | None:
    """Read-model over the durable transcript: AFTER the last compaction notice (its
    `fired_at_index`), did the model react? Scans the same client's thread for `send`
    tool-calls — a `recall` to its OWN id, or a memory write (`write`/`patch`/`append`)
    to any agent. The reaction IS the 'ack'; this derives it, no extra channel. Returns
    {recalled, persisted, recall_count} or None if no compaction has fired."""
    proj = _projection.get(id)
    mark = _compaction_mark.get(id)
    if not proj or not proj.get("fired") or not mark:
        return None
    idx = mark.get("fired_at_index", 0)
    store = await _load_history(id, kernel)
    recalled = persisted = False
    recall_count = 0
    for m in store[idx:]:
        if m.get("role") != "assistant":
            continue
        # Tool calls now live as `<call>` text inside the assistant content —
        # extract them with the same shared parser the loop uses.
        for call in extract_tool_calls(m.get("content") or ""):
            args = call.get("arguments") or {}
            payload = args.get("payload")
            ptype = payload.get("type") if isinstance(payload, dict) else None
            if args.get("target_id") == id and ptype == "recall":
                recalled = True
                recall_count += 1
            # A file_bridge's MUTATING text verbs — a model persists what it must
            # keep as plain files.
            elif ptype in ("write", "edit", "append"):
                persisted = True
    return {"recalled": recalled, "persisted": persisted, "recall_count": recall_count}


async def _grep(id, payload, kernel):
    """args: query:str (req — case-insensitive substring), ids:list[int]? (row, pid or fold ids to search), limit:int? (default: as many as fit inline), width:int? (chars per matched line, default 200). Finds the LINE that mentions something ANYWHERE in your memory — every row you ever wrote or received (behind every clear, inside every fold; a fold id searches its members), and the reply bodies still held this run. Raise `width` when the value sits deep in a long line. Returns {matches:[{id, line, text}], total, truncated}."""
    q = str(payload.get("query") or "").strip().lower()
    if not q:
        return {
            "error": "grep: `query` is required",
            "reason": "bad_arg",
            "arg": "query",
        }
    try:
        limit = max(1, min(int(payload.get("limit") or 100), 100))
    except (TypeError, ValueError):
        limit = 100
    try:
        # The per-line cut. A bare default (200) that the model can raise: a rule
        # written as one long line loses its tail at 200 and the model concludes grep
        # cannot reach it.
        width = max(20, min(int(payload.get("width") or 200), 20_000))
    except (TypeError, ValueError):
        width = 200
    raw_ids = payload.get("ids")
    ids = [_as_id(x) for x in raw_ids] if isinstance(raw_ids, list) else None
    if raw_ids is not None and (ids is None or any(x is None for x in ids)):
        return {
            "error": "grep: `ids` must be a list of message ids / pids (ints)",
            "reason": "bad_arg",
            "arg": "ids",
        }
    matches: list[dict] = []
    total = 0
    seen_lines: set[str] = set()

    def take(rid, n: int, line: str) -> None:
        # IDENTICAL LINES ONCE. A task the human repeats per delivery can match many
        # times and push the one body hit past the inline cap. The first occurrence
        # keeps its address.
        nonlocal total
        key = " ".join(line.split())
        if key in seen_lines:
            return
        seen_lines.add(key)
        total += 1
        if len(matches) < limit:
            matches.append({"id": rid, "line": n, "text": line.strip()[:width]})

    # BODIES FIRST: what a grep is for is the line inside a body never read whole —
    # rows that merely mention the query come after, so the content hit is never the
    # one the inline cap cuts. Bodies live in RAM for this run only.
    bodies = _mem(kernel)["bodies"].get(id, {})
    in_reach = None
    if ids is not None:
        # A fold id is a DESCENT: its members are reply rows, and their bodies hang
        # off the pids on their tags. An id that is itself a pid counts.
        rows = await _load_history(id, kernel, behind=True)
        scope = set(expand_fold_ids(rows, ids))
        in_reach = set(ids) | scope
        for m in rows:
            if m.get("id") in scope and m.get("role") == "user":
                c = m.get("content")
                for block in _reply_blocks(c if isinstance(c, str) else ""):
                    pm = _PID_TAG.match(block)
                    if pm and (p := _as_id(pm.group(1))) is not None:
                        in_reach.add(p)
    for pid, text in list(bodies.items()):
        if in_reach is not None and pid not in in_reach:
            continue
        for n, line in enumerate(text.splitlines(), 1):
            if q in line.lower():
                take(pid, n, line)
    # YOUR ROWS — the WHOLE conversation, searched by the store itself.
    req = {"type": "search", "query": q, "limit": 1000, "width": width}
    if ids is not None:
        req["ids"] = ids
    res = await _store_call(id, kernel, req)
    for m in res.get("matches") or []:
        take(m.get("id"), m.get("line", 0), m.get("text", ""))
    kept, cut = _fit_reply(matches, _inline_cap(kernel.get(id) or {}), limit)
    more = cut or total > len(kept)
    return {
        "matches": kept,
        "total": total,
        "truncated": more,
        **(
            {"next": {"id": kept[-1]["id"], "line": kept[-1]["line"]}}
            if more and kept
            else {}
        ),
    }


async def _compose(provider, prompt: str) -> str:
    """One plain completion, reasoning stripped (see `_make_summarizer`)."""
    parts: list[str] = []
    async for chunk in provider.chat([{"role": "user", "content": prompt}]):
        if isinstance(chunk, str):
            parts.append(chunk)
    return without_thinking("".join(parts))


def _first_json(text: str) -> dict | None:
    i = text.find("{")
    if i < 0:
        return None
    depth = 0
    for j in range(i, len(text)):
        depth += (text[j] == "{") - (text[j] == "}")
        if depth == 0:
            try:
                out = json.loads(text[i : j + 1])
            except ValueError:
                return None
            return out if isinstance(out, dict) else None
    return None


_RECALL_PROMPT = """You are rebuilding your own working context. Below is the HEAD of your
conversation as one line per row: id · kind · title · size · ↑parent. Fold rows stand for
finished stretches (their members are one descent away). Everything is currently at
DIGEST resolution; your budget for the body is {budget} tokens and the digests use
{used}. Replies ALREADY IN THIS HISTORY (their calls need not be repeated; the task itself may be unfinished):
{done}
Choose which rows you want IN FULL next turn (expansions cost their size) and say in one
sentence why. Reply JSON only:
{{"expand": [<id>, ...], "why": "<one sentence>"}}

{lines}"""


# The share of the recall target the standing rows may take before the newest-first
# fill (#218): the pins can never starve the fill.
PIN_SHARE = 0.5


def _is_task_root(r: dict) -> bool:
    """A row that SET a task: a user turn with no parent, not a reply, not a system kind."""
    return (
        r.get("role") == "user"
        and not r.get("parent")
        and r.get("kind") in (None, "", "user")
        and not _REPLY_TAG.match(str(r.get("content", "")))
    )


def _standing(view: list[dict]) -> list[int]:
    """Indexes into the rebuilt view that recall pins past the goal (index 0), in
    priority order, unique: the first root, the last root, the newest fold, then the
    other roots newest first."""
    from ai_core.folds import is_fold

    roots = [i for i in range(1, len(view)) if _is_task_root(view[i])]
    out: list[int] = []

    def push(i: int) -> None:
        if i != 0 and i not in out:
            out.append(i)

    if roots:
        push(roots[0])
        push(roots[-1])
    for i in range(len(view) - 1, 0, -1):
        if is_fold(view[i]):
            push(i)
            break
    for i in reversed(roots):
        push(i)
    return out


async def _context_recall(
    id,
    payload,
    kernel,
    cfg: Backend | None = None,
    system_prompt: str | None = None,
    client_id: str | None = None,
):
    """args: dry:bool? (mechanical fit only — no model call). Rebuild this agent's VIEW from its store: with `context_strategy` fold, a head longer than the budget is FOLDED first (oldest spans into fold rows, as a turn's projection folds); then fit the head to the budget at digest resolution, ask the model which rows it wants in full, persist the id list, log the decision as a row, and drop RAM scratch. Runs on boot, after a fold batch, or on request. Returns {kept, expanded, dropped, folded, budget, why, degraded}."""
    from ai_core.folds import render as fold_render
    from ai_core.strategies.strip import LOW as _LOW, _stripped as _strip_state

    rec = kernel.get(id) or {}
    rows = await _load_history(id, kernel)
    if not rows:
        return {
            "kept": 0,
            "expanded": 0,
            "dropped": 0,
            "folded": 0,
            "why": "empty store",
            "degraded": False,
        }
    # The block the turn will actually use (`_system_content`), not the default one:
    # a record or per-call `system_prompt` changes how much room the body gets.
    sys_tokens = estimate_one(
        {"role": "system", "content": await _system_content(id, kernel, system_prompt)}
    )
    body_budget = max(BUDGET_FLOOR, budget(rec) - sys_tokens - NOTICE_ENVELOPE_RESERVE)
    target = int(body_budget * _LOW)
    provider = None
    if cfg is not None and not payload.get("dry"):
        try:
            provider = await _build_provider(id, kernel, cfg)
        except Exception:
            provider = None
    # A HEAD LONGER THAN THE BUDGET FOLDS FIRST. A store written
    # under a larger window — another model, a planted past — is folded the way the
    # turn's own `fold` projection folds it: oldest spans into fold rows, folds of
    # folds as they pile up. Before this the fit below only stubbed every reply and
    # dropped the oldest rows that still did not fit — no fold, no summary. Folding
    # takes the model (a summary per span), so a `dry` recall does not fold.
    folded = 0
    anchors = anchors_of(await _head(id, kernel))
    if provider is not None and rec.get("context_strategy", DEFAULT_STRATEGY) == "fold":
        from ai_core.strategies.fold import fold_down

        fctx = ProjectionCtx(
            budget=body_budget,
            recent_n=_recent_n(rec),
            summarize=_make_summarizer(provider),
            self_id=id,
            kernel=kernel,
            client_id=client_id,
            anchors=anchors,
        )
        rows, _, folded, _ = await fold_down(rows, fctx, body_budget)
    view = [r for r in fold_render(rows, anchors) if r.get("id")]
    if not view:
        return {
            "kept": 0,
            "expanded": 0,
            "dropped": 0,
            "folded": 0,
            "why": "no addressable rows",
            "degraded": False,
        }

    def digest(r: dict) -> dict:
        return (
            _apply_res(r, "stub")
            if _res_of(r) == "full"
            and str(r.get("content", "")).startswith("[reply to ")
            else r
        )

    def cost(r: dict) -> int:
        return estimate_one(
            {"role": r.get("role", "user"), "content": r.get("content", "")}
        )

    # 1. MECHANICAL FIT — never fails. The goal turn always in; then the standing rows
    # (task roots, newest fold) within PIN_SHARE of the target; then newest first.
    picked = {0}
    used = cost(digest(view[0]))
    pin_cap = int(target * PIN_SHARE)
    for i in _standing(view):
        c = cost(digest(view[i]))
        if used + c <= pin_cap:  # an oversized pin is skipped, never a stop
            picked.add(i)
            used += c
    for i in range(len(view) - 1, 0, -1):
        if i in picked:
            continue
        c = cost(digest(view[i]))
        if used + c > target:
            break
        picked.add(i)
        used += c
    head = sorted(picked)
    # 2. ONE MODEL CALL — the model chooses expansions; a bad answer changes nothing.
    why, degraded, expand = "mechanical fit", True, set()
    if provider is not None:
        lines = []
        for i in head:
            r = view[i]
            m = re.search(r'· "([^"]{1,140})"', str(r.get("content", "")))
            title = r.get("title") or (
                m.group(1) if m else str(r.get("content", ""))[:80].replace("\n", " ")
            )
            lines.append(
                f'{r["id"]} · {r.get("kind") or r.get("role")} · "{title}" · '
                f"{len(str(r.get('content', '')))}B · ↑{r.get('parent') or '-'}"
            )
        # what the store already answers: every reply row names the call it closed,
        # so a rebooted model does not re-run reads whose results it is about to see
        done: list[str] = []
        for r in view:
            m = _REPLY_TAG.match(str(r.get("content", "")))
            if not m:
                continue
            t = re.search(r'· "([^"]{1,140})"', str(r.get("content", "")))
            line = f"{m.group(1)} {m.group(2)}" + (f' · "{t.group(1)}"' if t else "")
            if line not in done:
                done.append(line)
        done_text = "\n".join(done[-40:]) or "(nothing yet)"
        try:
            ans = _first_json(
                await _compose(
                    provider,
                    _RECALL_PROMPT.format(
                        budget=body_budget,
                        used=used,
                        done=done_text,
                        lines="\n".join(lines),
                    ),
                )
            )
        except Exception:
            ans = None
        if ans:
            head_ids = {view[i]["id"] for i in head}
            want = [
                x
                for x in (_as_id(v) for v in _as_list(ans.get("expand")))
                if x is not None and x in head_ids
            ]
            why = str(ans.get("why") or "")[:300] or "chosen"
            degraded = False
            # charge each expansion at full size, newest first; drop what does not fit
            total = used
            for x in reversed(want):
                r = next(v for v in view if v["id"] == x)
                extra = cost(r) - cost(digest(r))
                if total + extra <= body_budget:
                    expand.add(x)
                    total += extra
    ids = []
    for i in head:
        r = view[i]
        res = (
            "fold"
            if r.get("kind") == "fold"
            else (
                "full"
                if (
                    r["id"] in expand
                    or not str(r.get("content", "")).startswith("[reply to ")
                )
                else "stub"
            )
        )
        ids.append({"id": r["id"], "res": res})
    await _save_view(id, kernel, ids, budget_=body_budget, why=why)
    # the decision is a ROW — the next reload reads why the last recall chose as it did
    rid = _mint_row_id(id, kernel)
    # THE LIVE TASK IS NAMED. The view keeps the first root (the goal) at the top, and
    # after a recall a model with several roots can re-answer the FIRST one and call
    # done. The newest root is what is live; say so.
    # Over the STORE, not the folded render: a root that a fold collapsed is still the
    # live one if nothing newer came.
    live = next((r for r in reversed(rows) if _is_task_root(r)), None)
    live_txt = (
        f' · live task: {live["id"]} "'
        + " ".join(str(live.get("content", "")).split())[:100]
        + '"'
        if live
        else ""
    )
    await _save_history(
        id,
        kernel,
        [
            {
                "role": "user",
                "id": rid,
                "parent": None,
                "kind": "recall",
                "title": why,
                "content": f"[recall {rid} · kept {len(ids)} rows, {len(expand)} in full] {why}{live_txt}",
            }
        ],
    )
    # RAM is scratch, reconstructible by definition; the store is never cleared.
    _projection.pop(id, None)
    _compaction_mark.pop(id, None)
    _strip_state.pop(id, None)
    if folded and client_id:
        # The caller is told its context was compacted, as by a turn's projection.
        await _to_caller(
            kernel,
            id,
            client_id,
            {
                "type": "context",
                "source": id,
                "ts": time.time(),
                "phase": "compacted",
                "detail": {
                    "strategy": "fold",
                    "recall": True,
                    "dropped_messages": len(view) - len(ids),
                    "kept_messages": len(ids),
                    "summarized": False,
                    "pinned_facts": 0,
                    "stripped": 0,
                    "stripped_bytes": 0,
                    "folded": folded,
                },
            },
        )
    return {
        "kept": len(ids),
        "expanded": len(expand),
        "dropped": len(view) - len(ids),
        "folded": folded,
        "budget": body_budget,
        "why": why,
        "degraded": degraded,
    }


async def _context_clear(id, payload, kernel):
    """args: why:str? — start a NEW THREAD on the same store: a CLEAR-FOLD — one row whose members are every row since the previous clear (no summary) — and the model's view begins at its `[cleared <id> · N rows]` line. Nothing is deleted: grep, recall and `history {fold: <id>}` reach every member (`context_forget` is the verb that deletes). Empties the persisted view and drops RAM scratch (projection, fold counters, pending recall). Returns {cleared:id, rows:N}."""
    rec = kernel.get(id) or {}
    head = [r for r in await _head(id, kernel) if r.get("kind") not in _BOOKKEEPING]
    members = [r["id"] for r in head if r.get("id") is not None]
    why = " ".join(str(payload.get("why") or "").split())[:200]
    rid = _mint_row_id(id, kernel)
    await _save_history(
        id,
        kernel,
        [
            {
                "role": "user",
                "id": rid,
                "parent": None,
                "kind": "clear",
                "refs": members,
                "title": why or "cleared",
                "content": f"[cleared {rid} · {len(members)} rows]"
                + (f" {why}" if why else "")
                + " — a new thread; the rows before it are this clear's members, "
                f"reachable by id (grep, recall, history fold={rid}).",
            }
        ],
    )
    # The persisted view names rows behind the clear — empty it, so the view is the
    # store from the clear on. RAM is scratch; the store is never cleared.
    await _save_view(id, kernel, [], budget_=budget(rec), why="cleared")
    _projection.pop(id, None)
    _compaction_mark.pop(id, None)
    _strip_state_pop(id)
    _folds_since.pop(id, None)
    _recall_due.discard(id)
    _view_source.pop(id, None)
    _reset_chain(id, "send: the context was cleared before a turn read this message")
    return {"cleared": rid, "rows": len(members)}


def _is_boundary(row: dict) -> bool:
    """A fold or a clear: the rows a `context_fold` span may not cross."""
    return is_fold(row) or row.get("kind") == "clear"


async def _context_fold(id, payload, kernel, cfg: Backend):
    """args: from:int?, to:int? (row ids, inclusive, consecutive in the view; default `from` = the first row after the last fold or clear in the view, default `to` = the last row), summary:str? (the fold's text, used verbatim; absent, the fold summarizer writes it). Operator only — fold a chosen stretch of this agent's VIEW into ONE fold row, the same kind and shape an automatic fold writes (members in `refs`, reachable by `history {fold}`, `recall`, `grep`). Nothing is deleted; the view becomes the rows before the range (unchanged), the fold, then the rows after. Returns {folded, rows, summarized}; `summarized` is false only when the summarizer gave nothing and the fold carries its titles. Refused, with the reason named: an empty range, an id not in the view, a range crossing a fold or clear row, a range that starts at or before the cache primer, a turn running for this agent."""
    # A turn holds the per-agent lock for its whole run: the view must not move under it.
    if _lock_for(id).locked():
        return refusal(
            LOCKED,
            f"context_fold: a turn is running for {id}",
            hint="fold once the running turn has answered",
        )
    async with _lock_for(id):
        return await _fold_range(id, payload, kernel, cfg)


async def _fold_range(id, payload, kernel, cfg: Backend):
    """`context_fold`'s body, run under the agent's lock (see `_context_fold`)."""
    verb = "context_fold"
    rec = kernel.get(id) or {}
    # The view is rendered as the record's caching mode renders it, so the fold
    # persists exactly the view the next turn will send. A primer pins its prefix.
    try:
        mode, primer = caching.settings(rec)
    except ValueError as why:
        return refusal(
            BAD_ARG,
            f"{verb}: {why}",
            hint="set caching to none, cleanup or provider_cache",
            arg="caching",
        )

    # `from` / `to`: absent or null is the default; anything else must be an id.
    def id_arg(key: str):
        raw = payload.get(key)
        if raw is None:
            return None, None
        v = _as_id(raw)
        return (
            (v, None) if v is not None else (None, bad_arg(verb, key, "a row id (int)"))
        )

    from_id, err = id_arg("from")
    if err is not None:
        return err
    to_id, err = id_arg("to")
    if err is not None:
        return err
    # `summary`: absent or null writes the summarizer's; a given one is the text.
    raw_summary = payload.get("summary")
    if raw_summary is None:
        summary = None
    elif isinstance(raw_summary, str) and raw_summary.strip():
        summary = raw_summary
    else:
        return bad_arg(verb, "summary", "a non-empty string")

    rows = await _load_history(id, kernel)
    anchors = anchors_of(await _head(id, kernel))
    view = _render_history(rows, await _load_view(id, kernel), caching.cleans(mode))
    pos: dict = {}
    for p, (idx, _, _) in enumerate(view):
        pos.setdefault(rows[idx].get("id"), p)

    def empty(arg: str, why: str) -> dict:
        return refusal(
            BAD_ARG,
            f"{verb}: the range is empty — {why}",
            hint="name a range of rows that are in the view, from first to last",
            arg=arg,
        )

    if not view:
        return empty("from", "the view holds no rows")
    # The default `from` is the row after the last fold or clear in the view.
    default_from = max(
        (p + 1 for p, (idx, _, _) in enumerate(view) if _is_boundary(rows[idx])),
        default=0,
    )
    if from_id is None:
        from_p = default_from
    elif from_id in pos:
        from_p = pos[from_id]
    else:
        return bad_arg(verb, "from", "a row id in this agent's view")
    if to_id is None:
        to_p = len(view) - 1
    elif to_id in pos:
        to_p = pos[to_id]
    else:
        return bad_arg(verb, "to", "a row id in this agent's view")
    if from_p > to_p:
        return empty("to", "to comes before from")
    # THE PRIMER IS PINNED: the cached prefix ends at its row, so a range that starts at
    # or before that row would move bytes the provider has cached. A primer not in this
    # view pins nothing.
    if primer is not None and primer in pos and from_p <= pos[primer]:
        return refusal(
            BAD_ARG,
            f"{verb}: the primer is pinned — row {primer} ends the cached prefix, so a "
            "range may not start at or before it",
            hint="fold rows after the primer; the primer and the rows before it stay as they are",
            arg="from",
        )
    span = [idx for idx, _, _ in view[from_p : to_p + 1]]
    if any(_is_boundary(rows[i]) for i in span):
        return refusal(
            BAD_ARG,
            f"{verb}: the range crosses a fold or clear row",
            hint="fold the rows on each side of it separately; a fold row is never folded again by a range",
            arg="from",
        )
    anchor = member_anchor(rows, span, anchors)
    if summary is not None:
        # Verbatim: the body IS the given text, its first line the title. No provider.
        title, body, degraded = summary_title(summary), summary, False
    else:
        try:
            provider = await _build_provider(id, kernel, cfg)
        except Exception:
            provider = None
        ctx = ProjectionCtx(
            budget=budget(rec),
            recent_n=_recent_n(rec),
            summarize=_make_summarizer(provider) if provider is not None else None,
            self_id=id,
            kernel=kernel,
            client_id=None,
            anchors=anchors,
        )
        title, body, degraded = await summarize_span(ctx, rows, span)
    summarized = not degraded
    fold = await append_fold(id, kernel, rows, span, anchor, title, body, degraded)
    fid = fold["id"]
    # THE NEW VIEW: the rows before the range as they were rendered, the fold where the
    # range began, then the rows after. The fold is the newest row, so the array carries
    # every kept row.
    ids: list[dict] = []
    for p, (idx, res, msg) in enumerate(view):
        if p == from_p:
            ids.append({"id": fid, "res": "fold"})
        if from_p <= p <= to_p:
            continue
        entry = {"id": rows[idx]["id"], "res": res}
        # a body a trimmer changed is kept as trimmed, so the next turn renders the
        # same message the model saw before the fold
        if msg != _apply_res(rows[idx], res):
            entry["content"] = msg["content"]
        ids.append(entry)
    await _save_view(id, kernel, ids, budget_=budget(rec), why="context_fold")
    return {"folded": fid, "rows": len(span), "summarized": summarized}


def _make_context_fold(cfg: Backend):
    async def _verb(id, payload, kernel):
        return await _context_fold(id, payload, kernel, cfg)

    _verb.__doc__ = _context_fold.__doc__
    return _verb


async def _context_forget(id, payload, kernel):
    """args: include_cleared:bool? (default false). Compacts this agent's OWN store: drops the rows the model can no longer see and keeps every fold. In ONE `prune` it removes every non-fold row covered by a fold whose place is known, every `view` row but the newest one after the last clear, and with `include_cleared` every row before the last `clear` (the clear row stays). Kept: every fold at every level, every row of an open chain, every row no fold covers, the members of a fold whose place cannot be known, and the newest `anchors` row a kept fold needs — a fold written without an `anchor` has its place recorded first, in one appended `anchors` row. Waits for a running turn. The next prompt is the same before and after. Afterwards `recall`, `grep` and `history {fold}` do not find a pruned id; `history {fold}` still returns the fold's title and summary. Returns {pruned, kept_folds, unanchored}."""
    from ai_core.folds import chains, is_fold, is_open, recorded_anchors

    include_cleared = payload.get("include_cleared", False)
    if not isinstance(include_cleared, bool):
        return bad_arg("context_forget", "include_cleared", "a bool")
    async with _lock_for(id):
        head = list(await _refresh_head(id, kernel))
        everything = (await _store_call(id, kernel, {"type": "read"})).get("rows") or []
        anchors = anchors_of(head)
        by_id = {r.get("id"): r for r in head if r.get("id") is not None}
        head_folds = [r for r in head if is_fold(r)]
        turns = [
            r
            for r in head
            if r.get("kind") not in _BOOKKEEPING and r.get("role") in _TURN_ROLES
        ]
        open_ids = {
            turns[i].get("id")
            for chain in chains(turns)
            if is_open(turns, chain)
            for i in chain
        }
        doomed: set[int] = set()
        # 1. the rows folds already summarise — under a fold whose place is known
        for f in head_folds:
            if f.get("id") not in anchors:
                continue
            stack, seen = list(f.get("refs") or ()), set()
            while stack:
                x = stack.pop()
                if x in seen:
                    continue
                seen.add(x)
                r = by_id.get(x)
                if r is None:
                    continue
                if is_fold(r):
                    stack.extend(r.get("refs") or ())
                elif x not in open_ids:
                    doomed.add(x)
        # 2. every view row but the one in force
        views = [r for r in head if r.get("kind") == "view"]
        current = views[-1].get("id") if views else None
        doomed |= {
            r["id"]
            for r in everything
            if r.get("kind") == "view" and r.get("id") != current
        }
        # 3. behind the last clear, on request; the clear row itself stays
        if include_cleared:
            last = max(
                (i for i, r in enumerate(everything) if r.get("kind") == "clear"),
                default=None,
            )
            if last is not None:
                doomed |= {r["id"] for r in everything[:last]}
        # A fold written before folds carried `anchor` is placed by deriving from its
        # members — about to go. Its place is recorded first, in ONE `anchors` row;
        # an older `anchors` row is superseded by the newest one that holds it all.
        want = {
            f["id"]: anchors[f["id"]]
            for f in head_folds
            if not isinstance(f.get("anchor"), int) and f["id"] in anchors
        }
        rows_of_anchors = [r for r in head if r.get("kind") == "anchors"]
        newest = rows_of_anchors[-1] if rows_of_anchors else None
        holds = newest is not None and all(
            recorded_anchors([newest]).get(k) == v for k, v in want.items()
        )
        if want and not holds:
            await _save_history(
                id,
                kernel,
                [
                    {
                        "kind": "anchors",
                        "v": 1,
                        "anchors": {str(k): v for k, v in sorted(want.items())},
                    }
                ],
            )
            doomed |= {r["id"] for r in rows_of_anchors}
        else:
            keep = newest.get("id") if want and newest is not None else None
            doomed |= {r["id"] for r in rows_of_anchors if r["id"] != keep}
        pruned = 0
        if doomed:
            res = await _store_call(
                id, kernel, {"type": "prune", "ids": sorted(doomed)}
            )
            pruned = int(res.get("pruned") or 0)
        # THE STORE IS THE TRUTH: the prune cascaded each pruned member out of every
        # fold's refs, so the head cache is read again, not patched.
        _mem(kernel)["heads"].pop(id, None)
        head = await _head(id, kernel)
        left = [r for r in head if is_fold(r)]
        after = anchors_of(head)
        return {
            "pruned": pruned,
            "kept_folds": len(left),
            "unanchored": sum(1 for f in left if f.get("id") not in after),
        }


def _strip_state_pop(id: str) -> None:
    from ai_core.strategies.strip import _stripped as _strip_state

    _strip_state.pop(id, None)


async def _context_status(id, payload, kernel):
    """No args. The context-budget posture + the last overflow projection + the model's
    derived reaction to it. Returns {context_window, output_reserve, budget, strategy,
    last_projection:{fired, strategy?, kept_messages?, dropped_messages?, summarized?,
    too_small?}|null, last_reaction:{recalled, persisted, recall_count}|null}."""
    rec = kernel.get(id) or {}
    return {
        "context_window": resolve_context_window(rec),
        "output_reserve": output_reserve(rec),
        "budget": budget(rec),
        "trigger": TRIGGER,
        "calibration": round(_calib.get(id, 1.0), 3),
        "reported_prompt_tokens": _reported.get(id),
        "strategy": rec.get("context_strategy", DEFAULT_STRATEGY),
        "last_projection": _projection.get(id),
        "last_reaction": await _derive_reaction(id, kernel),
        "view_source": _view_source.get(id),
    }


def _make_context_recall(cfg: Backend):
    async def _verb(id, payload, kernel):
        return await _context_recall(id, payload, kernel, cfg)

    _verb.__doc__ = _context_recall.__doc__
    return _verb


def _make_boot(cfg: Backend):
    async def _boot(id, payload, kernel):
        """LAZY: never touches the memory store. Checks the record names one (fail loud otherwise), starts a new incarnation, and marks the view for rebuilding — the first turn reads the head from the store and runs `context_recall`."""
        _incarnation[id] = _incarnation.get(id, 0) + 1
        _reset_chain(id, "send: the agent rebooted before a turn read this message")
        try:
            _store_of(id, kernel)
        except MemoryStoreError as e:
            return {"error": str(e), "reason": "missing_arg", "arg": "memory_store_id"}
        # A new incarnation reads its store afresh; bodies are this run's only.
        _mem(kernel)["heads"].pop(id, None)
        _recall_due.add(id)
        # RESUME AFTER RESTART is PARKED: unwired —
        # `_resume_interrupted` is not spawned here until the kernel lets a drain's
        # `on_cancel` writes through (kernel gate; see that function).
        return None

    return _boot


#: The turn that resumes a turn a host stop cut.
RESUMED_NOTICE = (
    "[resumed after restart] The host stopped while you were answering the message "
    "above; your answer was cut there and nothing of it ran. Answer it now."
)
#: Seconds a resume check waits for the kernel to finish starting.
RESUME_WAIT = 60.0


async def _resume_interrupted(id: str, kernel) -> None:
    """DEFERRED — NOT WIRED. Blocked on the kernel:
    during a drain it refuses `on_cancel`'s store writes, so the cut turn this reads
    is never stored. To enable: let those writes through (kernel gate), then spawn
    this from `_boot`.

    RESUME AFTER RESTART. A turn in flight when the host
    stopped left its incoming row and a `stopped` row marked `interrupted` (`on_cancel`
    during the drain). If that cut row is still the newest of this agent's
    conversation, the turn is run again ONCE — a `[resumed after restart]` notice
    to the client it streamed to; the model has the message above it. A turn ended
    by anyone's cancel, or followed by anything newer, is left as it is."""
    ctx = _ctx(kernel)
    waited = 0.0
    while not getattr(ctx, "started", True) and waited < RESUME_WAIT:
        await asyncio.sleep(0.05)
        waited += 0.05
    rows = [
        r
        for r in await _load_history(id, kernel)
        if r.get("role") in ("user", "assistant")
    ]
    last = rows[-1] if rows else None
    if not (
        last
        and last.get("kind") == "stopped"
        and isinstance(last.get("interrupted"), dict)
    ):
        return
    client_id = last["interrupted"].get("client_id") or DEFAULT_CLIENT_ID
    _deliver_soon(kernel, id, RESUMED_NOTICE, client_id, None, notice=True)


async def _watch(id, payload, kernel):
    """args: body:bool? (default false), watcher:str? (operator only — an inbox id to deliver to). Announce every row this agent appends to YOUR inbox as {type:'row', source, row:{id, kind, role, bytes}} — content-free; with body:true the row's `content` too. Needs a registered inbox. Returns {watching:true, watcher, body}."""
    caller = current_origin()
    watcher = caller
    if caller is None:
        watcher = payload.get("watcher")
        if not isinstance(watcher, str) or not watcher:
            return {
                "error": "watch: the operator must name a `watcher` inbox",
                "reason": "missing_arg",
                "arg": "watcher",
            }
    elif payload.get("watcher") not in (None, caller):
        return {
            "error": "watch: an agent watches for ITSELF — `watcher` is the operator's",
            "reason": "unauthorized",
        }
    inboxes = getattr(_ctx(kernel), "inboxes", None) or {}
    if watcher not in inboxes:
        return {
            "error": f"watch: {watcher!r} has no registered inbox, so nothing would read the row events",
            "reason": "no_consumer",
            "hint": f"register the reader first — ctx.ensure_inbox({watcher!r}) — then watch",
        }
    body = bool(payload.get("body"))
    _mem(kernel)["watchers"].setdefault(id, {})[watcher] = body
    return {"watching": True, "watcher": watcher, "body": body}


async def _unwatch(id, payload, kernel):
    """args: watcher:str? (operator only). Stop announcing rows to you. Returns {watching:false}."""
    watcher = current_origin() or payload.get("watcher")
    _mem(kernel)["watchers"].get(id, {}).pop(watcher, None)
    return {"watching": False, "watcher": watcher}


# ─── the closure factory ────────────────────────────────────────


def build(
    *,
    sentence: str,
    make_provider: Callable[[str, Any], Any],
    name: str,
    extra_verbs: dict | None = None,
    reflect_extra: Callable | None = None,
    provider_cache: bool = False,
) -> tuple[dict, Callable]:
    """Bind one backend's config into a fresh `(VERBS, handler)` pair.

    The verb closures capture the returned `Backend`, so the per-backend
    config (sentence, provider builder, error prefix) is
    isolated; the module-global STATE (`_queue`, `_current`, `_locks`, …) is
    shared and keyed by agent id, so multiple backends coexist in one kernel.

    There is no `interrupt` verb: an in-flight `send` is a KERNEL task, killed
    with the universal `ps` → `cancel` system verbs. The turn-closing half is
    the module-level `on_cancel` hook above, and the teardown of a deleted id
    is `on_delete` — each backend re-exports both from its `tools` module so
    the substrate discovers them, exactly as `reflect_extra` / `make_provider`
    compose the rest of the shared machinery.

    Args:
      sentence: the agent's one-line self-description (reflect `sentence`).
      make_provider: fn(id, kernel) -> Provider. May be sync OR
        async (awaited if a coroutine). Called for EVERY turn
        (`_build_provider`) — the record is read live, never cached.
      name: error-message prefix (e.g. "ollama_backend").
      extra_verbs: extra {verb_name: async fn(id, payload, kernel)} merged
        into VERBS. No bundle uses it today — the key-storage verbs it
        carried are gone (keys come from the environment).
      reflect_extra: fn(id, kernel) -> dict (sync OR async), merged into
        the reflect reply (vllm: the effective sampling + the KV table).
      provider_cache: the backend runs the provider's cache markers
        (caching=provider_cache).
    """
    cfg = Backend(
        sentence=sentence,
        make_provider=make_provider,
        name=name,
        extra_verbs=extra_verbs or {},
        reflect_extra=reflect_extra,
        provider_cache=provider_cache,
    )

    verbs: dict[str, Callable] = {
        "reflect": _make_reflect(cfg),
        "send": _make_send(cfg),
        "stop": _stop,
        # Hold every incoming message in the store; release them, in order, after a
        # resumed row. Both survive a restart: the state is rows, not RAM.
        "pause": _pause,
        "resume": _resume,
        "rows": _rows,
        # ONE INQUIRY VERB, WHOEVER RECEIVES IT. A person answers `question` and so does
        # a peer AI: the asker writes the same call either way and reads the
        # answer off the same reply. Without this an AI asked a question got
        # `unknown type 'question'`, so "ask whoever can answer" was true only
        # for humans — and AI-to-AI inquiry had no shape at all.
        "inquiry": _make_inquiry(cfg),
        "history": _make_history(cfg),
        # Inline agent views in this chat (#339): a card row each way, no role, never a turn.
        "embed": _embed,
        "unembed": _unembed,
        **(extra_verbs or {}),
        "status": _status,
        "context_status": _context_status,
        "recall": _recall,
        # FIND the line anywhere in memory — rows, bodies held this run.
        "grep": _grep,
        # Rebuild the view from the store: on boot, after folds, on request.
        "context_recall": _make_context_recall(cfg),
        # A new thread on the same store: the view starts here, nothing is deleted.
        "context_clear": _context_clear,
        # Operator only: fold a chosen stretch of the view into one fold row.
        "context_fold": _make_context_fold(cfg),
        # Compact the store: drop the rows the folds already summarise.
        "context_forget": _context_forget,
        # THE ROW EVENT: this agent announces its own rows to its watchers.
        "watch": _watch,
        "unwatch": _unwatch,
        "boot": _make_boot(cfg),
    }
    cfg._verbs = verbs  # type: ignore[attr-defined]  # reflect lists these

    async def handler(id: str, payload: dict, kernel) -> dict | None:
        t = payload.get("type")
        fn = verbs.get(t)
        if fn is None:
            return refusal(UNHANDLED, f"{name}: unknown type {t!r}", verb=t)
        return await fn(id, payload, kernel)

    return verbs, handler
