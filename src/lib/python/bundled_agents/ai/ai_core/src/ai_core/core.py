"""Shared reflect-driven LLM-agent machinery for the ollama / anthropic /
nvidia_nim backends.

Two layers:

  1. MODULE-GLOBAL STATE, keyed by agent id. The provider cache, per-backend
     FIFO locks, and the queue / current-entry status state. These are keyed
     by the agent's id, so three backends loaded into ONE kernel share these
     dicts SAFELY — an ollama agent's id never collides with an anthropic
     agent's id. A bundle's tests patch them as
     `<bundle>.tools._providers[id] = fake`, so each bundle re-exports the
     SAME dict objects (see `build()`'s note).

     There is NO backend-private task table: a `send` IS a kernel task (one
     `ps` row, `"<agent>:<n>"`), the generation runs INLINE in that dispatch,
     and the kernel's `cancel` aborts it. See `on_cancel` below.

  2. PER-BACKEND CONFIG, captured by a `Backend` dataclass. `build()` is a
     CLOSURE FACTORY: each backend calls it with its own Provider builder +
     constants and gets back a `(VERBS, handler)` pair whose verb closures
     capture THAT backend's `Backend`. So `sentence`, the provider builder,
     the optional rate-limit stream wrapper, and the error-message prefix
     are per-backend; the state above is shared. An ollama agent and an
     anthropic agent in the same kernel each use their own provider and
     never clobber each other. Tool-calling is uniform + RAW for all of them
     (owned by this base class via `tool_parse`, not any native provider API).

`SEND_TIMEOUT` is read LIVE off the calling bundle module (each bundle
re-exports it) so a test's `monkeypatch.setattr("<bundle>.tools.SEND_TIMEOUT",
0.05)` is honoured on this shared path.

AI rehaul backlog (TODO — not in scope for the current decoupling).
These items will need a coordinated redesign across all LLM
backends + the TS ai_view before the next major bump:
  1. Cross-backend conversation portability — history now lives in a
     chat JSONL sidecar (one line per turn, persisted through its
     loader; the AI writes no disk itself), but it is a child of THIS
     agent, so switching upstream_id still starts a fresh conversation.
     Future: history travels with the chat tile, backends become
     stateless-modulo-streaming.
  2. (resolved) Tool-call protocol is now RAW + uniform: every provider
     streams plain text; the shared `tool_parse` parses one `<tool_call>`
     envelope contract for all backends. No native provider tool API.
  3. Multi-modal binary frames — image/audio payloads currently
     have no defined wire shape. Needs the WS binary frame channel
     (also blocks terminal_backend's image-paste).
  4. Cost / token tracking — no per-turn cost report today.
  5. Auth — api_key sidecar is plaintext; no per-tenant scoping.
"""

from __future__ import annotations

import asyncio
import inspect
import json
import os
import re
import secrets
import sys
import time
from collections.abc import Awaitable, Callable
from dataclasses import dataclass, field
from typing import Any

from ai_core.context import (
    ProjectionCtx,
    budget,
    estimate_one,
    estimate_tokens,
    output_reserve,
    resolve_context_window,
)
from ai_core.strategies import DEFAULT_STRATEGY, get_strategy, strategy_names
from ai_core.strategies.base import NOTICE_ENVELOPE_RESERVE, drop_orphan_tools

# TWO live consumers. The agentic LOOP is still emergent — the backend does ONE
# provider pass per send and never iterates — but `_route_reply` DOES parse this
# pass's output and dispatch what it names, so the parser is load-bearing on the
# routing path, not just a read-model. (This comment claimed otherwise until
# 2026-08-06; `tool_parse`'s own docstring had already been corrected, and one of
# two stale copies of a fact is worse than none.) `_derive_reaction` is the other
# consumer, scanning the durable transcript for a recall/persist reaction.
from ai_core.tool_parse import (
    CHANNEL_CLOSE,
    CHANNEL_OPEN,
    CLOSE,
    INVOKE_CLOSE,
    INVOKE_OPEN,
    OPEN,
    extract_tool_calls,
    malformed_calls,
)
from kernel import current_task, operator_context

# ─── shared module-global state (keyed by agent id) ─────────────

_providers: dict = {}
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

# Last context-overflow projection per agent — the PUBLIC summary surfaced by the
# `context_status` verb. {fired:bool, strategy?, kept_turns?, dropped_turns?, summarized?,
# too_small?}. Kept free of internal bookkeeping (see `_compaction_mark`).
_projection: dict[str, dict] = {}

# PRIVATE reaction cursor per agent — where the last compaction notice landed in the
# durable store + which client's thread it was, so `_derive_reaction` can scan from there
# for the model's recall/persist reaction. Internal; never surfaced in a verb reply.
_compaction_mark: dict[str, dict] = {}

# ─── shared constants ───────────────────────────────────────────

SEND_TIMEOUT = float(
    os.environ.get("FANTASTIC_AI_SEND_TIMEOUT", "180")
)  # hard ceiling per-generation; releases the lock (env-overridable — slow local models need more)
DEFAULT_CLIENT_ID = "cli"  # headless / REPL caller defaults here
MAX_CALL_DEPTH = 8  # recursion guard: AI→AI→… chains refuse past this depth


# ─── per-backend config (captured by the verb closures) ─────────


@dataclass
class Backend:
    """Per-backend config bound into the verb closures by `build()`.

    `module_name` is the importable name of the calling bundle's tools
    module (e.g. `"ollama_backend.tools"`). It is used to read the LIVE
    `SEND_TIMEOUT` off that module so a test monkeypatch on the bundle
    is honoured here.
    """

    sentence: str
    default_model: str
    default_endpoint: str
    make_provider: Callable[
        [str, Any], Any
    ]  # (id, kernel) -> Provider | None; sync OR async
    name: str  # error-message prefix, e.g. "ollama_backend"
    module_name: str
    extra_verbs: dict = field(default_factory=dict)
    reflect_extra: Callable[[str, Any], Awaitable[dict] | dict] | None = None
    stream_wrapper: Callable | None = None
    require_provider: bool = False  # nvidia: send failfasts when make_provider -> None
    provider_missing_error: str | None = None

    @property
    def send_timeout(self) -> float:
        mod = sys.modules.get(self.module_name)
        return getattr(mod, "SEND_TIMEOUT", SEND_TIMEOUT) if mod else SEND_TIMEOUT


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


def _clear_current(self_id: str) -> None:
    _current.pop(self_id, None)


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
    work to a caller without leaking content. `task_id` is bookkeeping, not
    status — it stays out of the snapshot (the live table is `ps`'s job)."""
    if d is None:
        return None
    return {
        k: v
        for k, v in d.items()
        if k not in ("text", "text_so_far", "last_tool", "task_id")
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
    """Project the message list to what the model actually reads — pure text, every
    role a chat template renders. Maps any `role:tool` (e.g. legacy turns in an old
    chat.json, or a caller-supplied convo) to `role:user` because many templates
    (incl. tiny local models) don't render a `tool` role. Strips non-text keys."""
    out: list[dict] = []
    for m in messages:
        role = m.get("role")
        content = m.get("content", "")
        out.append({"role": "user" if role == "tool" else role, "content": content})
    return out


# ─── persistence (routed through file_bridge_id) ────────────────


def _safe_client(client_id: str) -> str:
    """Trim and sanitize a client id so it's safe as a filename suffix.
    Spaces / slashes / weirdness collapse to underscores."""
    s = (client_id or DEFAULT_CLIENT_ID).strip() or DEFAULT_CLIENT_ID
    return "".join(c if (c.isalnum() or c in "._-") else "_" for c in s)


def _file_bridge_id(self_id: str, kernel) -> str | None:
    """The agent's file leg: chat history persists through it (the JSONL sidecar
    below), and the nvidia backend reads its api_key sidecar through it."""
    rec = kernel.get(self_id) or {}
    return rec.get("file_bridge_id")


def _chat_path(self_id: str, client_id: str) -> str:
    """The chat sidecar's path on the agent's OWN file leg — one JSONL file per
    client, one JSON line per turn. The encoding is ai_core's PRIVATE choice as
    a consumer (this code is its only reader; models get turns through the
    `history`/`recall` verbs) — the substrate imposes no format on anyone."""
    return f"agents/{self_id}/chat_{_safe_client(client_id)}.jsonl"


async def _load_history(self_id: str, kernel, client_id: str) -> list[dict]:
    """The client's conversation, read from the JSONL sidecar on the agent's
    file leg. LINE INDEX == STORE INDEX — `recall.before` and the compaction
    cursor page by it, so the identity is load-bearing. No leg wired, or no
    sidecar yet, reads as empty (writes are the loud half)."""
    fb = _file_bridge_id(self_id, kernel)
    if not fb:
        return []
    path = _chat_path(self_id, client_id)
    chunks: list[bytes] = []
    offset = 0
    while True:
        # BOOKKEEPING, PROCESSED FROM ABOVE — the same standing as the loader's
        # persist_blob and the old chat-store create: the agent does not choose
        # to have a conversation log. Confused-deputy-safe by construction: the
        # target is the record's OWN `file_bridge_id` (operator-set), the verb
        # is literal, and the path is self-scoped (`agents/<self>/…`, client
        # sanitized) — no caller can aim it at anyone else's namespace.
        with operator_context():
            r = (
                await kernel.send(
                    [
                        {
                            "target": fb,
                            "payload": {
                                "type": "read_stream",
                                "path": path,
                                "offset": offset,
                                "length": 65536,
                            },
                        }
                    ]
                )
            )[0]["result"]
        if not isinstance(r, dict) or r.get("error"):
            return []  # no sidecar yet = no history
        b = r.get("bytes") or b""
        chunks.append(bytes(b))
        if r.get("eof") or not b:
            break
        offset = r.get("next_offset", offset + len(b))
    out: list[dict] = []
    for line in b"".join(chunks).decode("utf-8", "replace").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            turn = json.loads(line)
        except ValueError:
            # A torn tail line (a crash mid-append) is the ONE legal defect in
            # an append-only log; skipping it loses at most that turn and never
            # shifts the index of the lines before it.
            continue
        if isinstance(turn, dict):
            out.append(turn)
    return out


async def _save_history(
    self_id: str, kernel, client_id: str, new_turns: list[dict]
) -> None:
    """APPEND this turn's new messages to the JSONL sidecar — one line per turn,
    one `write_stream` append per turn (absent offset = append-at-end; creates
    the file). The durable record is never trimmed; the projection only shapes
    what the model sees. Ordering: the per-agent FIFO lock serializes an
    agent's sends, which is what makes the leg's stat-then-seek append safe —
    an EXTERNAL concurrent writer to the same sidecar would not be.
    No `file_bridge_id` wired ⇒ nothing persists — and stays loudly absent
    from `history`, never silently in RAM."""
    if not new_turns:
        return
    fb = _file_bridge_id(self_id, kernel)
    if not fb:
        return
    path = _chat_path(self_id, client_id)
    for turn in new_turns:
        line = (json.dumps(turn, ensure_ascii=False) + "\n").encode("utf-8")
        # PROCESSED FROM ABOVE — see `_load_history`: own leg, literal verb,
        # self-scoped path. Without this every fixture would need a
        # can_call/callable_by pair just so an AI can keep its own history.
        with operator_context():
            (
                await kernel.send(
                    [
                        {
                            "target": fb,
                            "payload": {
                                "type": "write_stream",
                                "path": path,
                                "bytes": line,
                            },
                        }
                    ]
                )
            )[0]["result"]


async def _get_provider(self_id: str, kernel, cfg: Backend):
    """Return the cached provider for `self_id`, building it via the
    backend's `make_provider` (sync OR async) on cache miss. May return
    None when the backend's builder declines (e.g. nvidia with no key)."""
    if self_id in _providers:
        return _providers[self_id]
    prov = cfg.make_provider(self_id, kernel)
    if inspect.isawaitable(prov):
        prov = await prov
    if prov is None:
        return None
    _providers[self_id] = prov
    return prov


# ─── prompt assembly (Phase 1) ──────────────────────────────────


def _render_reflect(d: dict) -> str:
    d = dict(d)
    sentence = d.pop("sentence", "")
    fields = "  ".join(
        f"{k}={json.dumps(v) if not isinstance(v, str) else v}" for k, v in d.items()
    )
    return f"{sentence}  {fields}".strip()


_CONTEXT_HOWTO = """## Context window & compaction
Your live context is finite. When the conversation outgrows it, the system COMPACTS your
view: older turns are summarized or elided and a `[context-notice]` turn is inserted. The
DURABLE transcript is ALWAYS whole — compaction only shapes what you see right now, and
nothing is ever truly lost.
- To page dropped turns back on demand: `send('<your_own_id>', {type:'recall', query?, limit?})`.
  `query` substring-filters your full history; `limit` caps the page. Use it whenever you
  need a detail from earlier that isn't in your current view.
- When a compaction notice lands and the dropped span held durable facts (names, decisions,
  preferences), persist them to a memory agent (reflect the tree to find one) via the send
  tool — that is how they survive beyond this window.
- To inspect your budget + the last compaction + your own last reaction:
  `send('<your_own_id>', {type:'context_status'})`.
"""


_SEND_TO_HOWTO = """## Reaching other agents
Everything is an agent with a string id. `send(target_id, payload)` reaches any of them —
the payload's `type` names the verb; `{type:'reflect'}` returns that agent's verbs and
state. To send, emit exactly this, alone on its line:
<tool_call>{"name":"send","arguments":{"target_id":"<id>","payload":{"type":"<verb>"},"reply_to":"self"}}</tool_call>
`reply_to` is where that send's reply goes. It defaults to you — leave it out and the
answer comes back as your next message. Name another agent's id to hand the answer to
them instead, or `null` to discard it. Text outside the tag is your message to whoever
asked. Interpret instructions literally — act on what is asked, not on inferred
intent. Keep prose brief: the work happens in calls, not in explanation.

## Sending verbatim content — `send_binary`
When a payload field's VALUE is a body of text you are authoring (source, a document,
a patch), a JSON string breaks on escaping — use `send_binary` with the FENCED form:
keep the JSON small and complete, put `"@body"` where that value goes, and write the
body OUTSIDE the JSON between two marker lines, still inside the same tool_call:
<tool_call>{"name":"send_binary","arguments":{"target_id":"<id>","payload":{…,"bytes":"@body"}}}
*** Begin Bytes
<the body, verbatim — real newlines, real quotes, nothing escaped>
*** End Bytes
</tool_call>
Both markers start at column 0. Everything between them rides VERBATIM — no escaping,
any content, any size; nothing to quote, balance or count. The JSON line above them
is ordinary and small (`target_id`, `reply_to`, `id`, `timeout`, `retries` live
there). Which verbs take a body, and in which field, is each agent's own story —
reflect the agent to learn its composition; `into` names a field other than `bytes`.

A `<tool_call>` IS AN INVOCATION, WHEREVER YOU WRITE IT — including inside your own
thinking. The envelope is parsed out of everything you emit and dispatched the
moment it is complete, so there is no hypothetical or illustrative call: writing one
makes it happen.

Your thinking is NOT carried over between turns — it is stripped from history, so a
conclusion you only reasoned about is lost. Put what you worked out into the call you
make, or into a message. Reasoning longer never carries further than acting now does.
Before ending a turn, check one thing: every action you decided on exists as a
`<tool_call>` envelope in the turn. A decision without its envelope did not happen.

Stopping is EXPLICIT and costs one line: when the work is done, say so and emit
<tool_call>{"name":"done"}</tool_call>
— that ends your chain. A turn with NO call ends nothing: the runtime reminds you
once (a call decided in thinking does not execute unless the envelope is written
out), so never leave a finished task unsignalled — call `done`.

NEEDING A HUMAN IS ALSO NORMAL, and the human is AN AGENT LIKE ANY OTHER. When you
cannot proceed without a decision that is theirs to make, CALL them —
`send` to the agent whose reflect says it is the human (conventionally `user`)
with `{"type":"question", "question":"...", "kind":"text|single|multi|confirm",
"options":[...]}`. The ANSWER COMES BACK AS THE REPLY, in your next turn, tagged
with its pid like every other reply. Nothing suspends and nothing is scraped out
of your prose: asking is an ordinary call, so it is gated, it appears in the
ledger, and a human who is not wired REFUSES rather than inventing an answer.
Reflect the tree to see whether a human is reachable before assuming one is.

HOW REPLIES ARRIVE. Every send is dispatched under a PID (`<agent>:<token>`) and
every reply comes back tagged `[reply to <pid> · <agent> <verb>]`. Read that
literally:

- ALL the calls you make in ONE turn are answered TOGETHER in your next turn —
  one block per call, each tagged `· N of M in batch <token>` when you made
  several. M is complete on arrival: there is nothing still coming from that
  batch, so never wait, poll, or re-ask for a "missing" reply — if a call
  failed or timed out, its block says so.
- The calls of one turn run CONCURRENTLY. Their side effects have NO defined
  order — if B must happen after A, put B in the next turn, after A's reply.
- Each call may carry `timeout: <seconds>` in its arguments to bound that call;
  otherwise a generous default applies. A call that exceeds it is CANCELLED and
  its block reports the timeout.
- A call may carry `id: "<name>"` in its arguments — BESIDE `payload`, never
  inside it: `{"name":"send","arguments":{"target_id":"x","payload":{...},
  "id":"job-1"}}`. That NAMES the execution.
  Re-sending the SAME id NEVER runs the work twice: you get the first run's
  recorded outcome back, byte for byte (even if it failed). Use it for any call
  you must not double-fire (a write, a payment, a create) and to safely re-fetch
  a reply; a genuinely NEW attempt needs a NEW id. Ids may not contain ':'.
  Add `retries: N` and the KERNEL re-runs a failing or timing-out call up to N
  more times before answering — you send once and receive the final outcome
  (`attempts` says how many runs it took). Work that stays failed is parked:
  `send 'kernel' {type:'ledger'}` lists your executions and what became of
  them, outcomes included — inspect there instead of re-firing blind.
- `reply_to:null` means the call still runs but its answer is discarded — it is
  not in M and nothing comes back. `reply_to:<someone else>` hands that block to
  THEM instead of you.
- A reply whose PID you do not recognise is NORMAL — another agent can route its
  result to you. Read it, use it if useful, and move on.

A `reflect` reply is LARGE and TRANSIENT — you see its full text once, in the turn it
arrives, and the reply itself carries the instruction for what to record before it
goes. Follow it there. If you later need a detail you did not keep, reflect that agent
again — it is cheap and always current, and re-reading beats guessing.
"""

# ─── reply routing ──────────────────────────────────────────────
#
# After ONE provider pass, its output goes where it was addressed.
#
#   the model's own <tool_call>s — each names a target, a payload and `reply_to`. All are
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
# so awaiting a self-addressed send from inside the holder deadlocks until `send_timeout`.
# Spawned, it queues behind the current turn.


def _deliver_soon(kernel, target, text, client_id, system_prompt) -> None:
    """Hand `text` to `target` as a fresh turn, WITHOUT waiting for that turn.

    THIS IS WHY IT MUST NOT BE AWAITED INLINE. A send to an AI agent takes its
    FIFO lock and does not return until that whole turn is done — model call,
    streaming, and the turn's own routing. `_route_reply` used to `await` each
    delivery inside its per-call loop, so reply 2 of a 4-call turn waited for the
    entire conversation reply 1 started, and if the chain ended first it was never
    delivered at all.

    Measured 2026-08-24 (`live_build`, the model): 26 of 36 calls never answered
    — `runner` 6 sent / 0 back (it was never first in its batch), `kernel` 10 / 1.
    The model re-sent what never came back and was read as stubborn three separate
    times (opus 08-07 "acks lag a turn behind"; the model 08-20 "the reply queue is
    running ahead of me"; the model 08-24). Each was diagnosed as a labelling problem
    and answered with a better reply tag. The cause was here.

    Invisible to every unit test because a stub kernel's turn costs no time, and at
    zero latency serial and concurrent delivery are indistinguishable.

    Registered with `spawn_task` so the work is a `ps` row and `cancel` reaches it —
    the same contract `exec_bridge` uses for a forked job. Fire-and-forget without
    that is the orphan bug.
    """
    task = asyncio.create_task(_deliver(kernel, target, text, client_id, system_prompt))
    try:
        kernel.ctx.spawn_task(target, "deliver_reply", task)
    except Exception:  # noqa: BLE001 — a stub kernel in tests has no ctx
        _ORPHAN_DELIVERIES.add(task)
        task.add_done_callback(_ORPHAN_DELIVERIES.discard)


# Strong refs for deliveries the kernel could not register, so the event loop does
# not garbage-collect a pending task mid-flight.
_ORPHAN_DELIVERIES: set = set()


async def _deliver(kernel, target, text, client_id, system_prompt) -> None:
    """Hand `text` to `target` as a fresh turn. AWAITS the whole turn — call
    `_deliver_soon` unless you specifically need to block on it.

    A FAILED HANDOFF IS ANNOUNCED, not dropped. `_send` answers a turn it could
    not run with `{"error": ...}` rather than raising — a timeout, a provider
    transport failure, a refused gate. This function used to discard that reply,
    so a chain that died mid-flight looked exactly like a model that chose to stop:
    no error anywhere, the last turn simply never happened.

    Measured 2026-08-24 (`live_subai`): the chain ended at pass 5 with the model
    mid-task, and the run scored FAIL with no indication that anything had gone
    wrong at all. The first send's error was surfaced by its caller; every
    SUBSEQUENT turn's was invisible, which is every turn that matters.

    So the error is emitted on the target's own inbox, the same `status(phase=done,
    reason=error)` shape a failed direct send produces. Whoever watches that agent —
    a cli, a test harness, a panel — sees it. We do NOT raise: the dispatch that
    routed this reply is a detached task, and raising there kills a chain that may
    have other live branches."""
    payload = {"type": "send", "text": text, "client_id": client_id}
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
        await _emit_status(kernel, target, client_id, "done", reason="error", error=err)


# ─── thinking: PARSED, not PERSISTED ────────────────────────────
#
# A reasoning model streams two things. ollama labels them `message.thinking` /
# `message.content`; NIM labels them `reasoning_content` / `content`. We must read
# BOTH — a turn spent entirely reasoning is otherwise an empty completion that
# silently ends the chain (gemma4:12b, 2026-08-02). But we must not KEEP both:
# feeding a reasoning model its own prior deliberation back is against every
# vendor's guidance, and it compounds — gemma's 3,800-char turn-1 deliberation is
# re-sent in every later prompt, crowding out the task.
#
# So the two uses split. Providers wrap reasoning in these markers; the FULL text
# is parsed (a tool call the model wrote mid-thought still dispatches) and the
# STRIPPED text is persisted and shown as the answer. One convention, no provider
# learns anything about the tool protocol.
THINK_OPEN = "<think>"
THINK_CLOSE = "</think>"


# ─── stale whole-state reads: DEDUPED, not replayed ─────────────
#
# Measured (2026-08-20, the `live_plan` probe): reflect replies were 42% of the entire
# conversation — 73k of 174k chars — and 9 of 16 were a REPEAT of a target already
# reflected (`kernel_state` six times). That bulk is what fills the window, and a full
# window is what triggers compaction, which is what put the goal at risk in the first
# place. Trimming it attacks the cause, not the symptom.
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
_REPLY_TAG = re.compile(r"^\[reply to \S+ · (\S+) (\S+)[^\]]*\]")
# A delivered turn carries ONE OR MORE reply blocks — a batch arrives whole, one
# block per call, joined by a blank line. The trimmers below work on the BLOCK,
# the unit they were written for; treating the whole turn as one reply either
# deleted siblings (drop_answered_reflects) or trimmed nothing (a count guard
# that skipped every multi-reply turn — tried 2026-08-25, superseded same day).
_BLOCK_SPLIT = re.compile(r"\n\n(?=\[reply to \S+ · )")


def _reply_blocks(content: str) -> list[str]:
    return _BLOCK_SPLIT.split(content)


# A reflect that the model has ALREADY ANSWERED is replaced by this stub. The model
# was told to carry the facts it needs into that answer (by `_REFLECT_NUDGE`, which
# rode on the reflect reply itself — see the measurement there), so the
# distillation it wrote is what survives — written by the reader, at read time, with
# the task in mind, which no post-hoc summarizer can match. The stub names the agent so
# "I have already looked at this" stays true, and re-reflecting is always available if
# the distillation turns out to be thin.
_REFLECT_DROPPED = "[reflect of {agent} — full text dropped; your distillation follows]"
# What counts as "the model recorded it": the `SEEN <agent> — …` line the system block
# asks for. Matched leniently (any case, anywhere in the answer) — the point is whether
# facts were carried forward, not whether a format was obeyed.
_DISTILLED = re.compile(r"\bSEEN\b", re.I)

# The distillation instruction rides ON the reflect reply, not in the system block.
# Measured 2026-08-20 (`live_plan`, the passing run): with the instruction stated ONCE
# as system-block policy, the model wrote a `SEEN` line for 0 of 10 reflects — and
# re-reflected the SAME agent (`kernel_state`) 9 times in a 17-turn run. Policy sits
# thousands of tokens from the data and applies to a turn that has not happened yet;
# this arrives attached to the body it talks about, every time, at the moment it
# applies. It is deliberately the LAST thing in the message: it is what the model
# reads immediately before writing its answer.
_REFLECT_NUDGE = (
    "\n[this body is TRANSIENT — after your next message it is replaced by a stub. "
    "Open that message with the facts you will need, one compact line per agent:\n"
    "SEEN <id> — <what it is for>; verbs: <the ones you intend to use>. "
    "One reflect per agent is complete — reflecting it again yields nothing new; "
    "the next useful call is an action.]"
)


def drop_answered_reflects(history: list[dict]) -> list[dict]:
    """Replace the body of every reflect reply the model has already responded to.

    The IN-FLIGHT reflect (the last turn, not yet answered) is left intact — the model
    is reading it right now. Everything older is a reflect it has already had its
    chance to distil."""
    out: list[dict] = []
    for i, turn in enumerate(history):
        m = (
            _REPLY_TAG.match(turn.get("content") or "")
            if turn.get("role") == "user"
            else None
        )
        # FAIL CLOSED. Drop the body only if the model actually WROTE the distillation
        # in the turn that answered it. Measured 2026-08-20 (`live_plan`): the model
        # replied to several reflects with a bare tool_call and no prose, the body was
        # dropped anyway, and it had to re-fetch what it had lost — "The index was
        # dropped from my view. Let me send it again and, THIS TIME, actually record
        # the facts." Burning turns re-reading is the exact loop this trim exists to
        # prevent, so a reflect the model did not distil is KEPT. Non-compliance then
        # costs context, never comprehension.
        answer = next(
            (t for t in history[i + 1 :] if t.get("role") == "assistant"), None
        )
        distilled = answer is not None and _DISTILLED.search(
            answer.get("content") or ""
        )
        if not (m and distilled):
            out.append(turn)
            continue
        # Per BLOCK: only the reflect bodies drop; sibling replies that share the
        # turn (a batch arrives whole) stay verbatim.
        blocks = _reply_blocks(turn.get("content") or "")
        trimmed = [
            _REFLECT_DROPPED.format(agent=mb.group(1))
            if (mb := _REPLY_TAG.match(b)) and mb.group(2) == "reflect"
            else b
            for b in blocks
        ]
        if trimmed == blocks:
            out.append(turn)
        else:
            out.append({"role": "user", "content": "\n\n".join(trimmed)})
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
            keep_rev.append(
                {"role": "user", "content": "\n\n".join(reversed(kept_rev))}
            )
    return list(reversed(keep_rev))


# A `<tool_call>` written INSIDE a thought is not deliberation — it is an action
# that was taken. Dispatch reads the FULL text, so such a call really does run and
# really does return a reply; stripping the whole span deleted it from history, and
# the next turn showed the model an effect with no cause.
#
# Measured 2026-08-24 (`live_compose`, the model): turn 1 called `exec_bridge`
# from inside a thought and `file_bridge` after it. Both dispatched. History kept
# only `file_bridge`. The exec_bridge reply arrived and the model wrote "I sent a
# reflect to file_bridge_38de25, but the reply came back from exec_bridge_e05a8f" —
# a correct reading of a record we had silently edited. It re-sent the call and gave
# up. 3 of 169 dispatched calls across the day's runs were lost this way, and 2 of
# the 3 were in the run that failed at turn 2 because of it.
#
# ALL THREE DIALECTS, because dispatch parses all three. Keeping only the JSON
# `<tool_call>` one left the identical gap open for a model that speaks another
# grammar: gemma-4 emits the CHANNEL form (`<|tool_call>call:…<tool_call|>`), so its
# calls ran, returned a `[reply to …]` next turn, and left no cause in the record —
# the exact effect-without-cause failure this stripper exists to prevent. Rust's
# `kept_envelopes` (think.rs) has scanned all three since it was written.
#
# The pairing MIRRORS `tool_parse._ENVELOPES` — that table is the source of truth
# for which delimiters bound a call; a fourth envelope there is one line here.
_ENVELOPE_PAIRS = (
    (OPEN, CLOSE),
    (INVOKE_OPEN, INVOKE_CLOSE),
    (CHANNEL_OPEN, CHANNEL_CLOSE),
)


def _kept_envelopes(span: str) -> str:
    """Every COMPLETE tool-call envelope inside `span`, in SOURCE order, one per line.

    Source order, not dialect order: a turn that opened a channel call and then a
    JSON one must read back the way it was written, or the record misstates the
    sequence of actions. An opener with no closer is not kept — dispatch could not
    have parsed it either, so it is reasoning, not an action."""
    found: list[tuple[int, str]] = []
    for open_tag, close_tag in _ENVELOPE_PAIRS:
        at = 0
        while True:
            a = span.find(open_tag, at)
            if a == -1:
                break
            b = span.find(close_tag, a)
            if b == -1:
                break
            at = b + len(close_tag)
            found.append((a, span[a:at]))
    if not found:
        return ""
    found.sort(key=lambda hit: hit[0])
    return "\n" + "\n".join(e for _, e in found) + "\n"


def without_thinking(text: str) -> str:
    """Drop every `<think>…</think>` span — what goes into durable history.

    KEEPS any tool-call envelope found inside a dropped span — all three dialects
    (`<tool_call>`, `<invoke>`, `<|tool_call>`), since dispatch parses all three:
    the reasoning goes, the action stays. What was invoked must appear in the record
    of what happened, or the model cannot account for the replies it receives."""
    out, i = [], 0
    while True:
        a = text.find(THINK_OPEN, i)
        if a == -1:
            out.append(text[i:])
            break
        out.append(text[i:a])
        b = text.find(THINK_CLOSE, a)
        if b == -1:
            # Unterminated (stream cut mid-thought) — the reasoning tail is dropped,
            # but a call already dispatched from it is still an action taken.
            out.append(_kept_envelopes(text[a:]))
            break
        out.append(_kept_envelopes(text[a:b]))
        i = b + len(THINK_CLOSE)
    return "".join(out).strip()


# NO TRUNCATOR. `_real_output` used to cut every completion at the first
# `[reply to ` — our own marker, so its presence proves the model started writing
# replies it never received. The cut was right about that text and wrong about
# everything after it: on 2026-08-02 opus-5 emitted a fabricated reply BETWEEN two
# real calls, and the second (`update_agent assets readonly=true`) was silently
# dropped — never dispatched, no error to the model, no record in the kernel. It
# only surfaced because the model distrusted the ack and re-verified state itself.
#
# Measured against the transcript that motivated the truncator in the first place,
# it also bought nothing: gpt-oss:120b marked its fabrications `[awaiting response]`
# and `<awaiting reply>`, never `[reply to `, so all six of its calls — including
# ones composed from an invented `kv_store` — dispatched with the truncator ON.
# It caught the case it was built for zero times and cost a real call once.
#
# Fabrication is a real failure; silently deleting a model's work is a worse one,
# because the model can debug the first and cannot even see the second.


# THE NO-CALL NUDGE — one runtime reminder when a CHAIN turn dispatched nothing.
#
# Measured 2026-08-25 (`live_subai`, twice) and re-confirmed 2026-09-01
# (`repo_map:swift`, twice): mid-task the model decides its next action INSIDE
# its thinking, closes the thought, and ends the completion without any
# envelope — or stops early believing itself finished. Every agent harness
# treats "no action emitted" as a control decision, not an ending;
# interactively a human says "continue" — an unattended chain has nobody, so
# the runtime says it, ONCE. DONE is EXPLICIT now: the `done` tool call ends a
# chain silently and is never nudged.
#
# Why once: the nudge is itself a chain turn, so a second consecutive no-call
# turn arrives with the nudge as its incoming text — and is NOT nudged again.
# No counter, no per-chain state.
#: Consecutive silent chain turns the reminder tolerates before the chain is
#: let go (record meta `nudge_limit` overrides). A safety bound on an unattended
#: kernel, not a budget — the harness's max_turns is the outer backstop.
_NUDGE_LIMIT = 24
#: (kernel instance, agent id, client id) → consecutive silent chain turns so far.
_silent: dict[tuple[int, str, str], int] = {}

_NO_CALL_NUDGE = (
    "[no call was dispatched] Your turn emitted no <tool_call> envelope, so "
    "NOTHING ran — a call decided in thinking does not execute unless the "
    "envelope is written out. If the task is unfinished, emit that call now. "
    'If it is finished, end the chain explicitly: <tool_call>{"name":"done"}'
    "</tool_call>"
)


async def _route_reply(
    self_id, kernel, text, client_id, send_to, system_prompt, send_timeout, incoming
):
    """Honour where this pass's output goes. Runs DETACHED, after the lock is
    released.

    ONE TURN, ONE BATCH, ONE MESSAGE PER DESTINATION. Every call the model
    emitted is dispatched in a single `kernel.send([...])` (concurrent, order
    of side effects across calls undefined), and everything owed to one
    `reply_to` destination — results, parse errors, shape errors — arrives as
    ONE delivered turn, each piece its own `[reply to …]` block, tagged
    `N of M in batch <token>` when M > 1.

    Why whole, never split: replies used to land one per turn, so a turn that
    fanned out 7 calls spent the next 7 turns collecting its own answers, and
    every question asked in between was answered turns later by something
    older. Measured 2026-08-25 (`live_subai_langgraph`): the model re-asked
    `list_agents` five times and the run died with nothing built. Anthropic
    documents the split shape as the anti-pattern ("separate user messages
    reduce parallel tool use"), OpenAI 400s on it, LangGraph's ToolNode
    collects all ToolMessages before the agent runs again. The model never
    sees a partial batch, so it never re-asks for the missing half.

    `send_timeout` is the AI layer's per-call deadline — the KERNEL defaults
    nothing; this layer always supplies one. The model may override per call
    with a `timeout` arg in the envelope (validated by the kernel; a bad value
    settles that call as a refusal it can read).
    """
    calls = extract_tool_calls(text or "")
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
    for bad in malformed_calls(text or ""):
        units.append((self_id, "-", "-", None, json.dumps(bad)))
    done_called = any(c.get("name") == "done" for c in calls)
    for call in calls:
        if call.get("name") == "done":
            # The EXPLICIT end — not a dispatch, not an error, never nudged.
            continue
        args = call.get("arguments") or {}
        target = args.get("target_id") or args.get("target")
        # `reply_to` DEFAULTS TO SELF. An omitted key means the model gets its
        # own answer back, not that the answer is thrown away. Discarding a
        # reply is something you must SAY (`reply_to: null` / "none") — a
        # dropped key costs a wasted turn, not the whole task (measured:
        # gemma4:12b left the key out of a perfect call and the chain died at
        # pass 1 with nothing to see).
        if "reply_to" in args:
            reply_to = args.get("reply_to")
            reply_to = reply_to.strip() if isinstance(reply_to, str) else None
            if reply_to and reply_to.lower() in ("none", "null", "nobody"):
                reply_to = None
        else:
            reply_to = self_id
        if reply_to in ("self", self_id):
            reply_to = self_id
        if not isinstance(target, str) or not target.strip():
            # NAME THE SHAPE, not just the fault — a send must name the AGENT
            # it goes to, and only the model can close that gap (measured
            # 2026-08-06: a bare fault sent gemma-4 re-deriving the envelope
            # from the system prompt instead of retrying).
            if reply_to:
                units.append(
                    (
                        reply_to,
                        "-",
                        "-",
                        None,
                        json.dumps(
                            {
                                "error": (
                                    "no target_id, so nothing was dispatched — a "
                                    "send must name the AGENT it goes to. Shape: "
                                    '{"name":"send","arguments":{"target_id":'
                                    '"<agent>","payload":{"type":"<verb>"}}}. '
                                    "Send `{type:'reflect'}` to the kernel for "
                                    "the list of agent ids."
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
        if not isinstance(payload, dict):
            # NO PAYLOAD, NO SEND — what arrives here has no verb in it, and
            # saying so beats a Python error about our internals.
            if reply_to:
                units.append(
                    (
                        reply_to,
                        target,
                        "-",
                        None,
                        json.dumps(
                            {
                                "error": (
                                    f"send to {target!r} carried no payload, so "
                                    "nothing was dispatched. A payload is an "
                                    "object whose `type` names the verb, e.g. "
                                    '{"type":"reflect"}.'
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
        # send_binary: encode ONE named payload field from a JSON string to real
        # BYTES. A model tool call is JSON and JSON has no bytes type, so a stream
        # sink (`write_stream`) that demands raw bytes is otherwise unreachable — it
        # refuses a str. The model writes the contents as an ordinary JSON string in
        # `bytes` (its own serializer does the escaping, which it does reliably); we
        # utf-8-encode that one field here and the send is ordinary from the kernel's
        # side. The whole call is a single JSON object — no second region.
        if call.get("name") == "send_binary" and isinstance(payload, dict):
            field = str(args.get("into") or "bytes")
            val = payload.get(field)
            # A LITERAL "@body" is the fence placeholder with NO fenced block —
            # valid JSON, so it parses and would be WRITTEN AS THE FILE (measured
            # 2026-09-01, gym go/bowling + go/connect: `illegal character '@'`).
            # Refuse loud instead of dispatching the placeholder as content.
            if val == "@body":
                if reply_to:
                    units.append(
                        (
                            reply_to,
                            target,
                            payload.get("type", "?"),
                            None,
                            json.dumps(
                                {
                                    "error": (
                                        "send_binary NOT dispatched: `bytes` is the "
                                        'literal placeholder "@body" but no fenced '
                                        "block followed. Re-send the SAME call with "
                                        "the program between column-0 marker lines "
                                        "inside the same <tool_call>: the JSON line, "
                                        "then `*** Begin Bytes`, the program verbatim, "
                                        "then `*** End Bytes`."
                                    ),
                                    "target": target,
                                }
                            ),
                        )
                    )
                continue
            if isinstance(val, str):
                payload[field] = val.encode("utf-8")
        # The model's own `timeout` overrides the layer default; the kernel
        # validates it, so a bad value comes back as a readable refusal.
        item = {"target": target, "payload": payload, "timeout": send_timeout}
        if "timeout" in args:
            item["timeout"] = args.get("timeout")
        # The model's execution id rides through untouched — the kernel owns the
        # ledger semantics AND the validation (a bad id settles as a refusal the
        # model can read).
        if "id" in args:
            item["id"] = args.get("id")
        if "retries" in args:
            item["retries"] = args.get("retries")
        items.append(item)
        if reply_to:
            units.append(
                (reply_to, target, payload.get("type", "?"), len(items) - 1, None)
            )
        # fire-and-forget: dispatched, counted nowhere, nobody awaits the answer
    outcomes = await kernel.send(items) if items else []
    if units:
        # A STABLE TOKEN PER TURN — two batches are distinguishable at a
        # glance (measured 2026-08-24: bare "1 of 3" labels collided across
        # turns and the model re-read one file forever). The PID still
        # identifies the individual call and is the `ps`/`cancel` handle.
        batch = secrets.token_hex(2)
        groups: dict[str, list[str]] = {}
        order: list[str] = []
        for dest, target, verb, idx, body in units:
            if dest not in groups:
                groups[dest] = []
                order.append(dest)
            groups[dest].append("")  # reserve the slot; fill below with n/M known
        counts = {d: 0 for d in groups}
        for dest, target, verb, idx, body in units:
            if idx is not None:
                out = outcomes[idx]
                pid = out["pid"] or "-"
                body = json.dumps(out["result"], default=str)
                # Only reflect bodies are dropped later, so only they carry
                # the distillation notice.
                if verb == "reflect":
                    body += _REFLECT_NUDGE
            else:
                pid = "-"
            n = counts[dest] = counts[dest] + 1
            m_total = sum(1 for u in units if u[0] == dest)
            tag = f"[reply to {pid} · {target} {verb}"
            tag += f" · {n} of {m_total} in batch {batch}]" if m_total > 1 else "]"
            groups[dest][n - 1] = tag + "\n" + body
        for dest in order:
            _deliver_soon(
                kernel, dest, "\n\n".join(groups[dest]), client_id, system_prompt
            )
    if done_called:
        _silent.pop((id(kernel), self_id, client_id), None)
        return  # explicitly finished — nothing more runs, nothing reminds
    if not calls and not units:
        # A CHAIN turn (its incoming text was a delivered reply, or a nudge) that
        # dispatched nothing is REMINDED AGAIN — the model is queried until it
        # emits a call, `done`, or a question. Until 2026-09-01 the reminder fired
        # ONCE and a second silent turn ended the chain silently; a planning-heavy
        # model (E30 subai) wrote prose for two turns and the run read as
        # finished with the task untouched. A chat turn is not a chain (prose to
        # a person is the product). Bounded by `nudge_limit` on the record
        # (default 24 consecutive silent turns) so an unattended kernel cannot
        # spin forever on a model that never calls; the harness's max_turns is
        # the outer backstop.
        chained = isinstance(incoming, str) and (
            incoming.startswith("[reply to ")
            or incoming.startswith("[no call was dispatched]")
        )
        # ASKING IS A CALL NOW, so there is no question-turn exemption here.
        # A turn that asks the human DISPATCHES (`send` to the human agent) and
        # is therefore never silent; the answer arrives as that call's reply.
        # This used to scan the model's own prose for `{"type":"question"}` and
        # suppress the nudge on a hit — a text convention the kernel had to
        # recognise, with the human living outside it entirely.
        if chained:
            # Scoped to the KERNEL instance, not the process: two kernels in one
            # process (the test suite, an app embedding several) may carry the
            # same agent/client ids.
            key = (id(kernel), self_id, client_id)
            streak = _silent.get(key, 0) + 1
            _silent[key] = streak
            rec = kernel.get(self_id) or {}
            limit = int(rec.get("nudge_limit") or _NUDGE_LIMIT)
            if streak <= limit:
                _deliver_soon(kernel, self_id, _NO_CALL_NUDGE, client_id, system_prompt)
            else:
                _silent.pop(key, None)
            return
    _silent.pop((id(kernel), self_id, client_id), None)
    if calls:
        return
    if send_to is not None:
        _deliver_soon(kernel, send_to, text, client_id, system_prompt)


async def _assemble(
    self_id: str,
    user_text: str,
    kernel,
    client_id: str,
    system_prompt: str | None = None,
    messages_override: list[dict] | None = None,
) -> tuple[list[dict], int]:
    """Build the message list. A caller-supplied `system_prompt` REPLACES the
    deterministic substrate prompt — the generic 'caller controls context' door:
    a state agent or a python routine reads a stored prompt and passes it here, so
    the AI needs NO yaml-specific code. A caller-supplied `messages` list REPLACES
    the persisted history (fully stateless — the AI holds no per-call state).
    Both fall back to the default when absent.

    Returns `(messages, store_len)` — `store_len` is how many turns the DURABLE
    store held when this view was built (0 when the caller supplied its own
    messages: there is no store behind them). The view is deduped and trimmed, so
    its length is NOT a store index; `_project_context` needs the store one to mark
    where this turn's reaction will land. Returning it here is the only place both
    numbers are in hand at once."""
    if isinstance(system_prompt, str) and system_prompt.strip():
        sys_content = system_prompt
    else:
        # The DEFAULT system block is deliberately lean: a tree id-index + bundle
        # catalog primer, self-identity, and the context-window howto (context IS
        # the backend's — see `_CONTEXT_HOWTO`). NO agent menu, NO tool howto — this
        # backend does not run a tool loop; a caller that wants tool-calling supplies
        # its own `system_prompt` teaching its convention + whatever menu it built.
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
        me = (
            await kernel.send(
                [{"target": self_id, "payload": {"type": "reflect", "tree": "none"}}]
            )
        )[0]["result"]
        sys_content = "\n\n".join(
            [
                _render_reflect(primer),
                f"You are `{self_id}`. " + _render_reflect(me),
                _SEND_TO_HOWTO,
                _CONTEXT_HOWTO,
            ]
        )
    # System block is rebuilt on EVERY user turn; chat.json holds only
    # the user/assistant turns. The howto is not persisted — it always
    # reflects the latest state at send-time.
    messages: list[dict] = [{"role": "system", "content": sys_content}]
    store_len = 0
    if isinstance(messages_override, list) and messages_override:
        messages.extend(messages_override)  # stateless: caller supplies the convo
    else:
        # Deduped VIEW, not a rewrite of the store: a stale whole-state read that a
        # later reply supersedes never reaches the model. See `dedupe_stale_reads`.
        # Trimmed VIEW, not a rewrite of the store (durable history keeps every turn,
        # so the trace stays complete): stale whole-state reads deduped, and answered
        # reflects reduced to a stub — the model's own distillation carries the facts.
        hist = await _load_history(self_id, kernel, client_id)
        store_len = len(hist)
        messages.extend(drop_answered_reflects(dedupe_stale_reads(hist)))
    messages.append({"role": "user", "content": user_text})
    return messages, store_len


# ─── streaming + native tool-calls (Phase 2) ────────────────────


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


async def _emit_status(
    kernel, self_id: str, client_id: str, phase: str, **detail
) -> None:
    """Broadcast a structured `status` event AND update _current's phase
    so the on-demand `status` verb stays in sync. Phases:
        queued, thinking, streaming, tool_calling, done.
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
        n = 6
    return max(1, min(n, 50))


def _make_summarizer(provider):
    """Closure over the backend provider: summarize a span of turns to a string.
    A plain text completion (no tools — providers stream raw text); input is capped
    so summarizing can't itself blow the window."""

    async def _summarize(msgs: list[dict]) -> str:
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
        async for chunk in provider.chat(prompt):
            if isinstance(chunk, str):
                parts.append(chunk)
        return "".join(parts)

    return _summarize


def _context_notice(
    strategy: str, summary: str | None, omitted_marker: bool, dropped_n: int
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
    lines.append(
        "The full transcript is intact in durable storage. To page dropped turns back, "
        "send {type:'recall', query?, limit?} to your OWN id. If the dropped span holds "
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
    b = budget(rec)
    if estimate_tokens(messages) <= b:
        _projection[self_id] = {"fired": False}
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
                    # NAME THE KILLER. Measured 2026-08-26 (`live_subai_graph`,
                    # 131k window): this fired needing a single >126k-token turn
                    # and recorded nothing about it — the run's tempdir was gone
                    # before anyone could ask WHICH message was that big.
                    "last_turn_tokens": estimate_one(body[-1]),
                    "last_turn_head": str(body[-1].get("content"))[:200],
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
    ctx = ProjectionCtx(
        budget=body_budget,
        recent_n=_recent_n(rec),
        summarize=_make_summarizer(provider),
        self_id=self_id,
        kernel=kernel,
    )
    proj = await strat(body, system_block, rec, ctx)
    notice = _context_notice(
        strat_name, proj.summary, proj.omitted_marker, len(body) - len(proj.body)
    )
    # Single budget authority: the strategy reserved the notice envelope, but guard the
    # final fit anyway — trim oldest body turns (tool-pairing-safe). Never drop the last
    # (live) turn: the failsafe above guarantees room for [notice + live turn], so the
    # `len > 1` guard is a defensive backstop, not a fallback.
    # Trim from the MIDDLE, not the front. `out_body[0]` is the goal turn every
    # strategy deliberately pins (`compact`/`truncate` both put it first); the
    # last entry is the live turn. Taking from the front silently undid the pin
    # — the exact regression the pin exists to prevent, re-introduced one layer
    # up. Measured 2026-08-28: with a snug window the goal sentence was absent
    # from the projection entirely, in both strategies.
    out_body = proj.body
    while len(out_body) > 2 and estimate_tokens(system_block + [notice] + out_body) > b:
        out_body = drop_orphan_tools([out_body[0]] + out_body[2:])
    # Last resort, and only when nothing else is left to give: [goal, live] still
    # over budget. The live turn is what the model must answer, so the goal is
    # what goes — loudly, in the notice's own count, not silently.
    while len(out_body) > 1 and estimate_tokens(system_block + [notice] + out_body) > b:
        out_body = drop_orphan_tools(out_body[1:])
    dropped_n = max(0, len(body) - len(out_body))
    _projection[self_id] = {
        "fired": True,
        "strategy": strat_name,
        "kept_turns": len(out_body),
        "dropped_turns": dropped_n,
        "summarized": proj.summary is not None,
    }
    # PRIVATE reaction cursor: the store index where THIS turn's new turns (incl. any
    # recall/memory reactions) will land — `_derive_reaction` scans from here.
    #
    # `store_len`, NOT `len(body) - 1`. `body` is the DEDUPED/trimmed model view and
    # `_derive_reaction` slices the FULL durable store; whenever `dedupe_stale_reads`
    # dropped whole turns the view index is short by that many, so the scan started
    # BEFORE the notice and scored an assistant turn the model wrote earlier as its
    # reaction to a compaction it had not yet seen.
    _compaction_mark[self_id] = {
        "fired_at_index": store_len,
        "client_id": client_id,
    }
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
                "dropped_turns": dropped_n,
                "kept_turns": len(out_body),
                "summarized": proj.summary is not None,
            },
        },
    )
    return system_block + [notice] + out_body


async def _run(
    self_id: str,
    user_text: str,
    kernel,
    client_id: str,
    cfg: Backend,
    system_prompt: str | None = None,
    messages_override: list[dict] | None = None,
) -> dict:
    stateless = isinstance(messages_override, list) and bool(messages_override)
    provider = await _get_provider(self_id, kernel, cfg)
    # Caller (`_send`) pre-checks when require_provider; this is defensive.
    if provider is None:
        return {
            "error": cfg.provider_missing_error or f"{cfg.name}: provider unavailable"
        }
    messages, store_len = await _assemble(
        self_id, user_text, kernel, client_id, system_prompt, messages_override
    )
    # Context-overflow projection: shape what the MODEL sees this turn to fit the
    # window via the agent's `context_strategy` (default compact). Skipped when the
    # caller supplies its own `messages` (stateless — it owns the context). Done ONCE
    # at turn entry, never mid-tool-loop (would orphan role:tool from its assistant).
    if not stateless:
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
    # ONE provider pass — no tool loop. The provider streams pure text; every chunk
    # is a content token routed to the caller. Any tool convention is the caller's
    # (an authored agent reads this completion + drives its own loop). The optional
    # per-backend stream wrapper (nvidia's 429 retry) wraps the raw provider stream.
    content_parts: list[str] = []
    first_chunk = True
    model_messages = _render_for_model(messages)
    if cfg.stream_wrapper is not None:
        raw = cfg.stream_wrapper(provider, model_messages, kernel, self_id, client_id)
    else:
        raw = provider.chat(model_messages)
    async for chunk in raw:
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
    last_text = "".join(content_parts)

    # `status(phase='done')` IS the completion event. There is no second `done`
    # event: two events for one transition is two truths, and a subscriber that
    # watches the wrong one drifts silently.
    await _emit_status(kernel, self_id, client_id, "done", reason="ok")

    # APPEND this send's user + assistant turns to the durable chat store.
    # NOT `messages[1:]` — that's the projected/trimmed view; the store keeps the
    # FULL conversation so context management (compaction/recall) works next turn.
    if not stateless:
        new_turns = [
            {"role": "user", "content": user_text},
            # STRIPPED — the deliberation is not part of the conversation. The raw
            # text (with thinking) already went to `_route_reply` for parsing.
            {"role": "assistant", "content": without_thinking(last_text)},
        ]
        await _save_history(self_id, kernel, client_id, new_turns)
    return {"response": last_text, "final": last_text, "client_id": client_id}


# ─── verb factories (close over the per-backend cfg) ────────────


def _make_reflect(cfg: Backend):
    async def _reflect(id, payload, kernel):
        """Identity + model + endpoint + generating flag + file_bridge_id binding. No args."""
        rec = kernel.get(id) or {}
        out = {
            "id": id,
            "sentence": cfg.sentence,
            "model": rec.get("model", cfg.default_model),
            "endpoint": rec.get("endpoint", cfg.default_endpoint),
            "file_bridge_id": rec.get("file_bridge_id"),
            "context_window": resolve_context_window(rec),
            "context_strategy": rec.get("context_strategy", DEFAULT_STRATEGY),
            "verbs": {
                n: (f.__doc__ or "").strip().splitlines()[0]
                for n, f in cfg._verbs.items()
            },
            # A generation IS its kernel dispatch task — the backend holds no
            # task handle of its own. `_current[id]` present ⇔ in flight.
            "generating": id in _current,
            "cancel": CANCEL_DOC,
            "emits": {
                "token": "{type:'token', text:str, source, client_id} — streaming chunk. Routed ONLY to the caller: cli (stdout) when client_id='cli', else this agent's own inbox (browser filters by client_id).",
                "status": "{type:'status', source, client_id, ts, phase:'queued'|'thinking'|'streaming'|'tool_calling'|'done', detail:{send_id, started_at, queue_depth, ...phase_specific}} — single channel for phase transitions. detail.tool={call_id,target,verb,args,reply_preview?} for tool_calling (entry has no reply_preview, exit re-emits same call_id with it). detail.reason='ok'|'cancelled'|'timeout'|'error' for done. detail.ahead for queued.",
                "context": "{type:'context', source, client_id, ts, phase:'compacted'|'too_small', detail:{...}} — the context protocol's push half. compacted: detail={strategy, dropped_turns, kept_turns, summarized} when the live view was compacted to fit the window. too_small: detail={context_window, system_tokens, hint} when even the system block + one turn won't fit (the model is NOT called — a failfast). Pull counterpart: the `context_status` verb.",
            },
            "concurrency": "Per-backend FIFO lock around `send`: one generation at a time. Other callers wait (and receive a `queued` event so their UI can show it). `reflect`/`history`/`status` skip the lock and stay snappy.",
        }
        if cfg.reflect_extra is not None:
            extra = cfg.reflect_extra(id, kernel)
            if inspect.isawaitable(extra):
                extra = await extra
            if extra:
                out.update(extra)
        return out

    return _reflect


def _make_send(cfg: Backend):
    async def _send(id, payload, kernel):
        """args: text:str (req), client_id:str? (default 'cli'). Streams tokens to ONLY the caller — cli (stdout) for client_id='cli', or the browser tab whose WS is subscribed and filters by client_id otherwise. Persists per-client chat.json. Per-backend FIFO lock: concurrent callers serialize. If the lock is held when this call arrives, emits a `status` event (phase='queued', detail.ahead) for the caller; first `token` (or `status` of phase != queued) for the same client_id implicitly unqueues. Optional system_prompt:str REPLACES the auto-built prompt (caller-supplied role/context — read it from a state agent yourself, no yaml coupling here); optional messages:list REPLACES persisted history (fully stateless, no file_bridge_id needed). _call_stack is reserved for the recursion guard (cycle/over-depth refusal before the lock). Optional send_to:str hands this pass's TEXT to that agent id as a fresh turn, once. The model routes its own work with `reply_to` inside its <tool_call>s. Returns {response, final, client_id}."""
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
        messages_override = payload.get("messages")
        # Where this pass's TEXT goes when the model issues no call of its own.
        send_to = payload.get("send_to")
        if not isinstance(send_to, str) or not send_to.strip():
            send_to = None
        # History is auto-mounted (the JSONL sidecar, persisted via its file leg) —
        # no operator wiring needed; nothing to failfast on here.
        # Backends that require a provider up front (nvidia: api_key) failfast here.
        if cfg.require_provider:
            provider = await _get_provider(id, kernel, cfg)
            if provider is None:
                return {
                    "error": cfg.provider_missing_error
                    or f"{cfg.name}: provider unavailable",
                    "client_id": client_id,
                }
        text = payload.get("text", "")
        send_id = _new_send_id()
        entry = {
            "client_id": client_id,
            "text": text,
            "send_id": send_id,
            "queued_at": time.time(),
        }
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
        # The dequeue is the acquire's own failure path; swift already does this.
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
            _set_current(id, entry)
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
                async with asyncio.timeout(cfg.send_timeout):
                    reply = await _run(
                        id,
                        text,
                        kernel,
                        client_id,
                        cfg,
                        system_prompt,
                        messages_override,
                    )
            except TimeoutError:
                await _emit_status(kernel, id, client_id, "done", reason="timeout")
                return {
                    "error": f"send: timeout after {cfg.send_timeout}s",
                    "client_id": client_id,
                }
            except Exception as e:
                # A backend may install a custom error mapper (nvidia: clean
                # 429 / HTTP messages instead of re-raising).
                if cfg._error_mapper is not None:
                    mapped = cfg._error_mapper(e)
                    if mapped is not None:
                        await _emit_status(
                            kernel, id, client_id, "done", reason="error", error=mapped
                        )
                        return {"error": mapped, "client_id": client_id}
                await _emit_status(
                    kernel, id, client_id, "done", reason="error", error=str(e)
                )
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
        # just held and deadlock until `send_timeout`. The task row makes the chain
        # visible in `ps` and reachable by `cancel`.
        if isinstance(reply, dict) and not reply.get("error"):
            task = asyncio.create_task(
                _route_reply(
                    id,
                    kernel,
                    reply.get("response", ""),
                    client_id,
                    send_to,
                    system_prompt,
                    # The AI layer's per-tool-call deadline. The kernel defaults
                    # nothing; this is where a default belongs — the same knob
                    # that bounds a generation bounds a call to one.
                    cfg.send_timeout,
                    # The text this turn ANSWERED — what tells routing whether
                    # this was a chain turn (a delivered reply) or a chat turn.
                    text,
                )
            )
            kernel.ctx.spawn_task(id, "route_reply", task)
        return reply

    return _send


def _make_history(cfg: Backend):
    async def _history(id, payload, kernel):
        """args: client_id:str? (default 'cli'). Returns {messages:[...], client_id} — that client's full conversation from the chat store (the durable record)."""
        client_id = _safe_client(payload.get("client_id") or DEFAULT_CLIENT_ID)
        return {
            "messages": await _load_history(id, kernel, client_id),
            "client_id": client_id,
        }

    return _history


# ─── the on_cancel bundle hook (shared by every backend) ────────

# What every backend's `reflect` says INSTEAD of an `interrupt` verb — the
# substrate's universal cancel, taught in one line so a caller (or an LLM
# reading the reflect) needs no bundle-private knowledge to stop a turn.
CANCEL_DOC = (
    "No `interrupt` verb — an in-flight `send` IS a kernel task. Kill it with the "
    "substrate's universal system verbs: send 'kernel' {type:'ps'} for the live task "
    "table, find the row {agent:'<this agent>', verb:'send'}, then send 'kernel' "
    "{type:'cancel', task_id:'<agent>:<n>'}. The turn closes with status(phase='done', "
    "reason='cancelled') + done, its HTTP stream and FIFO lock are released, and the "
    "blocked caller's `send` resolves to {cancelled:true}. Cancel kills the task's whole "
    "SUBTREE, so any work the turn spawned dies with it."
)


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
    await _emit_status(
        kernel,
        id,
        client_id,
        "done",
        reason="cancelled",
        send_id=entry.get("send_id"),
        started_at=entry.get("started_at"),
    )


async def _status(id, payload, kernel):
    """args: client_id:str? — when provided, returns a privacy-filtered snapshot: full text only for this client's queue entries / current generation; other clients collapse to `others_pending` (an integer count). Without client_id the snapshot redacts text everywhere. Used by chat UIs to rebuild queued/in-flight state on page reload (and for headless polling). Returns {source, client_id, generating, current:{phase, send_id, started_at, elapsed, is_mine, text?, text_so_far?, last_tool?}|null, mine_pending:[{send_id,text,queued_at}], others_pending:int}."""
    cid = payload.get("client_id")
    if cid:
        cid = _safe_client(cid)
    return _status_snapshot(id, cid)


def _recall_render(m: dict) -> str:
    """Render ONE stored turn for a recall reply — WHOLE, never truncated.

    Turns are pure text (tool calls/replies are inline `<tool_call>`/`<tool_response>`
    text), so this only serializes a non-str content. The page `limit` is the bound
    that belongs here: recall exists to hand back a detail the model no longer has,
    and a per-turn cap silently drops the very part it went looking for — the caller
    says how many turns it wants, and each one arrives intact."""
    content = m.get("content")
    s = content if isinstance(content, str) else json.dumps(content, default=str)
    return s


async def _recall(id, payload, kernel):
    """args: client_id?:str (default: the conversation this call was issued FROM, else
    'cli'), query?:str (case-insensitive substring over the serialized turn — matches tool
    args/replies too), limit?:int (default 20, max 100), before?:int (store index — page
    backward). Pages turns back from the DURABLE chat store (the FULL conversation, never
    trimmed — so anything compaction dropped is one call away). Read-only. Returns
    {messages:[{index,role,content}], total, truncated, client_id, note?} — `note` states
    WHY an empty page is empty (nothing compacted / query matched nothing)."""
    explicit = payload.get("client_id")
    # No explicit client: the model calls recall MID-GENERATION, and that generation
    # knows which conversation it is (`_current[id].client_id`). Resolving from it is
    # reading a fact about the in-flight turn, not a default. Measured 2026-08-07
    # (combined probe, opus): the builder ran under client 'probe', called bare
    # `recall`, the old code defaulted to 'cli', got the empty thread of a client that
    # never spoke, and concluded its own history was gone — then stopped working and
    # asked a question into a walked-away conversation. DEFAULT_CLIENT_ID remains only
    # for external callers with no generation in flight (the headless/REPL contract,
    # same as `history`).
    client_id = _safe_client(
        explicit or _current.get(id, {}).get("client_id") or DEFAULT_CLIENT_ID
    )
    full = await _load_history(id, kernel, client_id)
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
    reply = {
        "messages": [
            {"index": i, "role": m.get("role"), "content": _recall_render(m)}
            for i, m in page
        ],
        "total": len(indexed),
        "truncated": truncated,
        "client_id": client_id,
    }
    if not page:
        # An empty page with no reason reads as "your history is lost" — the opposite of
        # what it usually means. Say why, so the caller can tell the cases apart.
        proj = _projection.get(id)
        if q and full:
            reply["note"] = (
                f"query {q!r} matched none of the {len(full)} stored turns "
                f"for client {client_id!r}."
            )
        elif not (proj or {}).get("fired"):
            reply["note"] = (
                "nothing has been compacted away — recall pages back turns that "
                "compaction DROPPED, and none have been. If you can see this reply, "
                "your live context already holds the whole conversation."
            )
        else:
            reply["note"] = (
                f"no stored turns for client {client_id!r} in this page; the durable "
                "thread is keyed per client_id — name one to read another thread."
            )
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
    client_id = mark.get("client_id", DEFAULT_CLIENT_ID)
    store = await _load_history(id, kernel, client_id)
    recalled = persisted = False
    recall_count = 0
    for m in store[idx:]:
        if m.get("role") != "assistant":
            continue
        # Tool calls now live as `<tool_call>` text inside the assistant content —
        # extract them with the same shared parser the loop uses.
        for call in extract_tool_calls(m.get("content") or ""):
            args = call.get("arguments") or {}
            payload = args.get("payload")
            ptype = payload.get("type") if isinstance(payload, dict) else None
            if args.get("target_id") == id and ptype == "recall":
                recalled = True
                recall_count += 1
            # `text_file`'s MUTATING verbs. The old tuple ("set"/"append"/"replace")
            # was durable_memory's KV surface, which is deleted — memory is plain
            # files now, so a model that persisted with `write` or `patch` scored as
            # not having reacted at all.
            elif ptype in ("write", "patch", "append"):
                persisted = True
    return {"recalled": recalled, "persisted": persisted, "recall_count": recall_count}


async def _context_status(id, payload, kernel):
    """No args. The context-budget posture + the last overflow projection + the model's
    derived reaction to it. Returns {context_window, output_reserve, budget, strategy,
    last_projection:{fired, strategy?, kept_turns?, dropped_turns?, summarized?,
    too_small?}|null, last_reaction:{recalled, persisted, recall_count}|null}."""
    rec = kernel.get(id) or {}
    return {
        "context_window": resolve_context_window(rec),
        "output_reserve": output_reserve(rec),
        "budget": budget(rec),
        "strategy": rec.get("context_strategy", DEFAULT_STRATEGY),
        "last_projection": _projection.get(id),
        "last_reaction": await _derive_reaction(id, kernel),
    }


async def _boot(id, payload, kernel):
    """No-op. Returns None."""
    return


# ─── the closure factory ────────────────────────────────────────


def build(
    *,
    sentence: str,
    default_model: str,
    default_endpoint: str,
    make_provider: Callable[[str, Any], Any],
    name: str,
    module_name: str,
    extra_verbs: dict | None = None,
    reflect_extra: Callable | None = None,
    stream_wrapper: Callable | None = None,
    require_provider: bool = False,
    provider_missing_error: str | None = None,
    error_mapper: Callable | None = None,
) -> tuple[dict, Callable]:
    """Bind one backend's config into a fresh `(VERBS, handler)` pair.

    The verb closures capture the returned `Backend`, so the per-backend
    config (sentence, provider builder, stream wrapper, error prefix) is
    isolated; the module-global STATE (`_providers`, `_queue`, `_current`,
    `_locks`) is shared and keyed by agent id, so multiple backends
    coexist in one kernel.

    There is no `interrupt` verb: an in-flight `send` is a KERNEL task, killed
    with the universal `ps` → `cancel` system verbs. The turn-closing half is
    the module-level `on_cancel` hook above — each backend re-exports it from
    its `tools` module so the substrate discovers it, exactly as `reflect_extra`
    / `make_provider` compose the rest of the shared machinery.

    Args:
      sentence: the agent's one-line self-description (reflect `sentence`).
      default_model / default_endpoint: reflect fallbacks when the record
        omits them.
      make_provider: fn(id, kernel) -> Provider | None. May be sync OR
        async (awaited if a coroutine). Called only on a `_providers`
        cache miss — tests pre-seed `_providers[id]` to bypass it.
      name: error-message prefix (e.g. "ollama_backend").
      module_name: the calling bundle's tools module name (e.g.
        "ollama_backend.tools"), used to read the LIVE SEND_TIMEOUT off it.
      extra_verbs: extra {verb_name: async fn(id, payload, kernel)} merged
        into VERBS. No bundle uses it today — the key-storage verbs it
        carried are gone (keys come from the environment).
      reflect_extra: fn(id, kernel) -> dict (sync OR async), merged into
        the reflect reply (nvidia: {"has_api_key": ...}).
      stream_wrapper: fn(provider, messages, kernel, id, client_id) ->
        async-iter, wrapping provider.chat() (nvidia: 429 retry).
      require_provider: True makes `send` failfast when make_provider
        returns None (nvidia: api_key not set yet).
      provider_missing_error: the error string for that failfast.
      error_mapper: fn(exc) -> str | None; maps a provider exception to a
        clean caller-facing error inside `send` (nvidia: 429 / HTTP).
    """
    cfg = Backend(
        sentence=sentence,
        default_model=default_model,
        default_endpoint=default_endpoint,
        make_provider=make_provider,
        name=name,
        module_name=module_name,
        extra_verbs=extra_verbs or {},
        reflect_extra=reflect_extra,
        stream_wrapper=stream_wrapper,
        require_provider=require_provider,
        provider_missing_error=provider_missing_error,
    )
    # Stash hooks the verb closures read but that aren't part of the
    # public Backend surface.
    cfg._error_mapper = error_mapper  # type: ignore[attr-defined]

    verbs: dict[str, Callable] = {
        "reflect": _make_reflect(cfg),
        "send": _make_send(cfg),
        "history": _make_history(cfg),
        **(extra_verbs or {}),
        "status": _status,
        "context_status": _context_status,
        "recall": _recall,
        "boot": _boot,
    }
    cfg._verbs = verbs  # type: ignore[attr-defined]  # reflect lists these

    async def handler(id: str, payload: dict, kernel) -> dict | None:
        t = payload.get("type")
        fn = verbs.get(t)
        if fn is None:
            return {"error": f"{name}: unknown type {t!r}"}
        return await fn(id, payload, kernel)

    return verbs, handler
