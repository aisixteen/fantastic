"""Shared reflect-driven LLM-agent machinery for the ollama / anthropic /
nvidia_nim backends.

Two layers:

  1. MODULE-GLOBAL STATE, keyed by agent id. The provider cache, in-flight
     task table, per-backend FIFO locks, menu cache, and the queue /
     current-entry status state. These are keyed by the agent's id, so
     three backends loaded into ONE kernel share these dicts SAFELY — an
     ollama agent's id never collides with an anthropic agent's id. A
     bundle's tests patch them as `<bundle>.tools._providers[id] = fake`,
     so each bundle re-exports the SAME dict objects (see `build()`'s note).

  2. PER-BACKEND CONFIG, captured by a `Backend` dataclass. `build()` is a
     CLOSURE FACTORY: each backend calls it with its own Provider builder +
     constants and gets back a `(VERBS, handler)` pair whose verb closures
     capture THAT backend's `Backend`. So `sentence`, the provider builder,
     the OpenAI-vs-anthropic tool-args shape, the optional rate-limit
     stream wrapper, and the error-message prefix are per-backend; the
     state above is shared. An ollama agent and an anthropic agent in the
     same kernel each use their own provider and never clobber each other.

`SEND_TIMEOUT` is read LIVE off the calling bundle module (each bundle
re-exports it) so a test's `monkeypatch.setattr("<bundle>.tools.SEND_TIMEOUT",
0.05)` is honoured on this shared path — mirroring runner_core's transport
proxy of the lock-poll constants.

AI rehaul backlog (TODO — not in scope for the current decoupling).
These items will need a coordinated redesign across all LLM
backends + the TS ai_view before the next major bump:
  1. Cross-backend conversation portability — history now lives in a
     mounted chat yaml_state (per-client key, persisted THROUGH the
     loader; the AI writes no disk itself), but it is a child of THIS
     agent, so switching upstream_id still starts a fresh conversation.
     Future: history travels with the chat tile, backends become
     stateless-modulo-streaming.
  2. Tool-call streaming protocol — current contract is one
     tool_call per chunk (ollama) vs argument fragments aggregated
     across chunks (OpenAI/NIM). Pick one and version it.
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
import secrets
import sys
import time
from dataclasses import dataclass, field
from typing import Any, Awaitable, Callable

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

# ─── shared module-global state (keyed by agent id) ─────────────

_providers: dict = {}
_tasks: dict[str, asyncio.Task] = {}
_locks: dict[str, asyncio.Lock] = {}

# Per-backend cache of "what other agents exist + what they answer".
# Built lazily at assemble time; invalidated automatically after every
# tool_call (which may have mutated the population) and on the
# `refresh_menu` verb (so the LLM can self-invalidate). Never persisted
# to chat.json — it's prepended to the system block on each turn.
_menu_cache: dict[str, list[dict]] = {}

# Structural state for the `status` verb + status events. One channel,
# one source of truth — UI subscribes to `status` events for live phase
# transitions and calls the `status` verb to rebuild after page reload.
#
# _queue[backend_id]: pending entries waiting on the per-backend lock,
#   in arrival order. Each: {client_id, text, queued_at, send_id}.
# _current[backend_id]: the entry that holds the lock right now, or
#   absent. Carries the same fields plus `started_at` and `phase`.
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

# The AI's mounted `chat` yaml_state id (its durable history store), keyed by agent id.
# The conversation lives there (persisted THROUGH the loader) — the AI writes NO disk.
_chat_agent: dict[str, str] = {}


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
    tool_args_as_json: bool = False
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
    backend lock. Adds started_at, phase=thinking, empty text_so_far."""
    _current[self_id] = {
        **entry,
        "started_at": time.time(),
        "phase": "thinking",
        "text_so_far": "",
    }


def _clear_current(self_id: str) -> None:
    _current.pop(self_id, None)


def _redact_text(d: dict | None) -> dict | None:
    """Return a copy of a current/queue entry with text-bearing fields
    stripped. Used to surface the existence of someone else's in-flight
    work to a caller without leaking content."""
    if d is None:
        return None
    return {k: v for k, v in d.items() if k not in ("text", "text_so_far", "last_tool")}


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


def _invalidate_menu(self_id: str) -> None:
    """Drop the agent menu so the next assemble rebuilds it from live reflect."""
    _menu_cache.pop(self_id, None)


def _lock_for(self_id: str) -> asyncio.Lock:
    """Per-backend FIFO serializer: only one `_send` runs at a time per
    agent. Concurrent callers (cli, browser tabs) wait their turn."""
    if self_id not in _locks:
        _locks[self_id] = asyncio.Lock()
    return _locks[self_id]


SEND_TOOL = {
    "type": "function",
    "function": {
        "name": "send",
        "description": (
            "Send a message to any agent in the Fantastic substrate. "
            "Universal verb on every agent: reflect (returns identity + state; "
            "add readme:true for the agent's full guide, and reflect the ROOT "
            "agent with readme:true for the whole-system guide). "
            "Discover agents by sending list_agents to the kernel_state agent."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "target_id": {
                    "type": "string",
                    "description": "Agent id to send the payload to (e.g. 'kernel_state', 'cli', 'terminal_xxx').",
                },
                "payload": {
                    "type": "object",
                    "description": '{"type": "<verb>", ...fields}. Universal verb: reflect.',
                },
            },
            "required": ["target_id", "payload"],
        },
    },
}


# ─── persistence (routed through file_bridge_id) ────────────────


def _safe_client(client_id: str) -> str:
    """Trim and sanitize a client id so it's safe as a filename suffix.
    Spaces / slashes / weirdness collapse to underscores."""
    s = (client_id or DEFAULT_CLIENT_ID).strip() or DEFAULT_CLIENT_ID
    return "".join(c if (c.isalnum() or c in "._-") else "_" for c in s)[:64]


def _file_bridge_id(self_id: str, kernel) -> str | None:
    """Still used by the nvidia backend for its api_key sidecar. Chat history NO LONGER
    uses it — that lives in a mounted `chat` yaml_state (below), persisted via the loader."""
    rec = kernel.get(self_id) or {}
    return rec.get("file_bridge_id")


async def _ensure_chat_agent(self_id: str, kernel) -> str | None:
    """The AI's mounted `chat` yaml_state — its durable conversation store, which
    persists THROUGH the loader (the AI writes NO disk itself). Discovered among this
    agent's children, or created (idempotent + cached). None only if creation fails."""
    cached = _chat_agent.get(self_id)
    if cached and kernel.get(cached):
        return cached
    _chat_agent.pop(self_id, None)
    try:
        online = await kernel.send("kernel_state", {"type": "list_agents"})
    except Exception:
        online = {}
    for a in online.get("agents", []) if isinstance(online, dict) else []:
        rec = kernel.get(a.get("id")) or {}
        if (
            rec.get("parent_id") == self_id
            and rec.get("handler_module") == "yaml_state.tools"
            and rec.get("mode") == "chat"
        ):
            _chat_agent[self_id] = a["id"]
            return a["id"]
    r = await kernel.send(
        self_id,
        {"type": "create_agent", "handler_module": "yaml_state.tools", "mode": "chat"},
    )
    cid = r.get("id") if isinstance(r, dict) else None
    if cid:
        _chat_agent[self_id] = cid
    return cid


async def _load_history(self_id: str, kernel, client_id: str) -> list[dict]:
    """The client's conversation, read from the mounted `chat` yaml_state (key-per-client)."""
    cid = await _ensure_chat_agent(self_id, kernel)
    if not cid:
        return []
    r = await kernel.send(cid, {"type": "read", "key": _safe_client(client_id)})
    val = r.get("value") if isinstance(r, dict) else None
    return val if isinstance(val, list) else []


async def _save_history(
    self_id: str, kernel, client_id: str, new_turns: list[dict]
) -> None:
    """APPEND this turn's new messages to the `chat` yaml_state (key-per-client). The
    durable record is never trimmed; the projection only shapes what the model sees."""
    if not new_turns:
        return
    cid = await _ensure_chat_agent(self_id, kernel)
    if not cid:
        return
    await kernel.send(
        cid, {"type": "append", "key": _safe_client(client_id), "value": new_turns}
    )


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


async def _build_menu(self_id: str, kernel) -> list[dict]:
    """Reflect on every running agent (skip self) and collect their
    one-line sentence + verb names. Used to grow the system prompt
    into a real "menu of capabilities" the model can see at a glance.
    """
    online = await kernel.send("kernel_state", {"type": "list_agents"})
    items: list[dict] = []
    for a in online.get("agents", []):
        if a["id"] == self_id:
            continue  # self is described separately in the prompt
        try:
            r = await kernel.send(a["id"], {"type": "reflect"})
        except Exception:
            r = {}
        verbs = r.get("verbs", {}) if isinstance(r, dict) else {}
        verb_names = list(verbs.keys()) if isinstance(verbs, dict) else list(verbs)
        items.append(
            {
                "id": a["id"],
                "sentence": (r or {}).get("sentence", "")
                if isinstance(r, dict)
                else "",
                "verbs": verb_names,
            }
        )
    return items


def _render_menu(menu: list[dict]) -> str:
    """Format the menu as bullet lines for the system prompt."""
    if not menu:
        return "## Available agents\n(none — only `kernel_state` and `self`)"
    lines = [
        "## Available agents (reflect any for verb signatures; reflect the root"
        " agent with readme:true for the full system guide)"
    ]
    for m in menu:
        verbs = m.get("verbs") or []
        head = ", ".join(verbs[:10]) + (" …" if len(verbs) > 10 else "")
        lines.append(f"- `{m['id']}` — {m['sentence']} — verbs: {head or '(none)'}")
    return "\n".join(lines)


_SEND_HOWTO = """## How to use the `send` tool
You have ONE tool: `send(target_id, payload)`. EVERY action goes through it.
- ORIENT FIRST. For anything beyond the menu's verb names — especially the
  browser frontend (panels/views), persistence, or how agents are wired — read
  the full system guide in ONE call BEFORE acting:
  `send('kernel_state', {type:'reflect', readme:true})`. It explains the transports,
  how compute/memory/views are addressed, and how the browser frontend and
  persistence work. Don't guess the wiring — read it first.
- To do something concrete (read a file, run python, list agents, etc.), pick
  an agent from the menu above whose verbs cover what you need, then build
  `{type:'<verb>', ...args}` and pass it as `payload`.
- To learn an agent's full verb signatures (arg names, types):
  `send('<id>', {type:'reflect'})` returns `{verbs: {name: 'doc'}, ...}`.
- To rebuild your menu of agents (useful right after you create one):
  `send('<your_own_id>', {type:'refresh_menu'})` — next turn shows the fresh menu.
- NEVER claim "I don't have access" without trying the menu first. The
  send tool reaches every agent in the system.
"""

_CONTEXT_HOWTO = """## Context window & compaction
Your live context is finite. When the conversation outgrows it, the system COMPACTS your
view: older turns are summarized or elided and a `[context-notice]` turn is inserted. The
DURABLE transcript is ALWAYS whole — compaction only shapes what you see right now, and
nothing is ever truly lost.
- To page dropped turns back on demand: `send('<your_own_id>', {type:'recall', query?, limit?})`.
  `query` substring-filters your full history; `limit` caps the page. Use it whenever you
  need a detail from earlier that isn't in your current view.
- When a compaction notice lands and the dropped span held durable facts (names, decisions,
  preferences), persist them to your memory agent (in the menu above) via the send tool —
  that is how they survive beyond this window.
- To inspect your budget + the last compaction + your own last reaction:
  `send('<your_own_id>', {type:'context_status'})`.
"""


async def _assemble(
    self_id: str,
    user_text: str,
    kernel,
    client_id: str,
    system_prompt: str | None = None,
    messages_override: list[dict] | None = None,
) -> list[dict]:
    """Build the message list. A caller-supplied `system_prompt` REPLACES the
    deterministic substrate prompt — the generic 'caller controls context' door:
    a state agent or a python routine reads a stored prompt and passes it here, so
    the AI needs NO yaml-specific code. A caller-supplied `messages` list REPLACES
    the persisted history (fully stateless — the AI holds no per-call state).
    Both fall back to the default when absent."""
    if isinstance(system_prompt, str) and system_prompt.strip():
        sys_content = system_prompt
    else:
        # Lean substrate context for the system prompt: an id-index of the
        # tree + the bundle catalog by name (not the full nested tree).
        primer = await kernel.send(
            "kernel", {"type": "reflect", "tree": "ids", "bundles": "ids"}
        )
        me = await kernel.send(self_id, {"type": "reflect", "tree": "none"})
        # Lazy menu: rebuild only when invalidated (None / missing).
        if self_id not in _menu_cache:
            _menu_cache[self_id] = await _build_menu(self_id, kernel)
        menu = _menu_cache[self_id]
        sys_content = "\n\n".join(
            [
                _render_reflect(primer),
                f"You are `{self_id}`. " + _render_reflect(me),
                _render_menu(menu),
                _SEND_HOWTO,
                _CONTEXT_HOWTO,
            ]
        )
    # System block is rebuilt on EVERY user turn; chat.json holds only
    # the user/assistant turns. Menu + howto are not persisted — they
    # always reflect the latest state at send-time.
    messages: list[dict] = [{"role": "system", "content": sys_content}]
    if isinstance(messages_override, list) and messages_override:
        messages.extend(messages_override)  # stateless: caller supplies the convo
    else:
        messages.extend(await _load_history(self_id, kernel, client_id))
    messages.append({"role": "user", "content": user_text})
    return messages


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
        await kernel.send("cli", ev)
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
    tools=[] (the summarizer can't tool-call); input is capped so summarizing can't
    itself blow the window."""

    async def _summarize(msgs: list[dict]) -> str:
        rendered = "\n".join(
            f"{m.get('role', '?')}: {str(m.get('content', ''))}" for m in msgs
        )[:20000]
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
        async for chunk in provider.chat(prompt, tools=[]):
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
    rec, cfg: Backend, provider, self_id, kernel, client_id, messages
):
    """Shape the assembled `messages` to fit the agent's token budget via its configured
    `context_strategy` (default `compact`), then prepend the ONE canonical context-notice
    and push a `context:compacted` event to the caller. Returns the projected message
    list, OR an `{error}` dict when even the system block + the live user turn won't fit
    (the `too_small` failsafe — a failfast that ALSO pushes a `context:too_small` event,
    NOT a fallback). Durable store untouched. Called ONCE per send, at turn entry."""
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
    out_body = proj.body
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
    _compaction_mark[self_id] = {
        "fired_at_index": max(0, len(body) - 1),
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
    call_stack: list | None = None,
) -> dict:
    call_stack = call_stack or []
    stateless = isinstance(messages_override, list) and bool(messages_override)
    provider = await _get_provider(self_id, kernel, cfg)
    # Caller (`_send`) pre-checks when require_provider; this is defensive.
    if provider is None:
        return {
            "error": cfg.provider_missing_error or f"{cfg.name}: provider unavailable"
        }
    messages = await _assemble(
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
        )
        if isinstance(projected, dict):  # too_small failsafe / config error
            return projected
        messages = projected
    last_text = ""
    # The NEW turns this send produces — appended to the durable chat store at the end
    # (NOT `messages[1:]`, which is the projected/trimmed view, not the full record).
    new_turns: list[dict] = [{"role": "user", "content": user_text}]

    # Loop until the model stops emitting tool_calls. Bounded only by
    # SEND_TIMEOUT (asyncio.wait_for around the whole _run task) and
    # the user-callable `interrupt` verb. No fixed step cap — Claude
    # Code-style runs as long as the model keeps proposing tools.
    iteration = 0
    while True:
        iteration += 1
        if iteration > 1:
            # Re-entering the loop after tool_calls — phase transitions
            # streaming/tool_calling back to `thinking` while the model
            # decides its next move. UI uses this to keep the pulse
            # animation alive between tool blocks.
            await _emit_status(kernel, self_id, client_id, "thinking")
        content_parts: list[str] = []
        tool_calls: list[dict] = []
        first_chunk = True
        # An optional per-backend stream wrapper (nvidia's 429 retry)
        # wraps provider.chat(); default is the raw provider stream.
        if cfg.stream_wrapper is not None:
            stream = cfg.stream_wrapper(provider, messages, kernel, self_id, client_id)
        else:
            stream = provider.chat(messages, tools=[SEND_TOOL])
        async for chunk in stream:
            if isinstance(chunk, str):
                if first_chunk:
                    # First text chunk for this iteration — phase
                    # transitions thinking → streaming.
                    await _emit_status(kernel, self_id, client_id, "streaming")
                    first_chunk = False
                content_parts.append(chunk)
                # Accumulate so the `status` verb's snapshot includes
                # everything streamed so far, letting the UI rebuild a
                # mid-stream view on page reload.
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
            else:
                tool_calls.append(chunk["tool_call"])
        last_text = "".join(content_parts)

        if not tool_calls:
            break

        # Record assistant turn carrying its tool_calls. ollama wants
        # `arguments` as a dict; OpenAI-flavored backends (nvidia) want a
        # JSON string — `cfg.tool_args_as_json` selects.
        assistant_turn = {
            "role": "assistant",
            "content": last_text,
            "tool_calls": [
                {
                    "id": c["id"],
                    "type": "function",
                    "function": {
                        "name": c["name"],
                        "arguments": json.dumps(c["arguments"])
                        if cfg.tool_args_as_json
                        else c["arguments"],
                    },
                }
                for c in tool_calls
            ],
        }
        messages.append(assistant_turn)
        new_turns.append(assistant_turn)

        # Execute tool_calls IN PARALLEL via asyncio.gather. Each runs
        # its own status entry → kernel.send → status exit → say emit;
        # gather preserves result order so role:tool messages append in
        # the order the model emitted them. Menu invalidation runs once
        # per batch (the population may have changed via any of the
        # parallel create/delete/update calls).
        async def _exec_one(c):
            args = c["arguments"]
            target = args.get("target_id", "")
            payload = args.get("payload", {})
            verb = (payload or {}).get("type", "") if isinstance(payload, dict) else ""
            tool_entry = {
                "call_id": c["id"],
                "target": target,
                "verb": verb,
                "args": args,
            }
            # ENTRY: emit before invocation so the UI can render an
            # in-progress tool block (header pulses, body empty).
            if self_id in _current:
                _current[self_id]["last_tool"] = tool_entry
            await _emit_status(
                kernel, self_id, client_id, "tool_calling", tool=tool_entry
            )
            try:
                send_payload = payload
                if isinstance(payload, dict):
                    # Propagate the call chain so a downstream AI agent can refuse
                    # cycles / over-depth (recursion guard). Non-AI targets ignore
                    # the extra key; it is NOT recorded in the persisted args.
                    send_payload = {**payload, "_call_stack": call_stack + [self_id]}
                reply = await kernel.send(target, send_payload)
            except Exception as e:
                reply = {"error": str(e)}
            reply_str = json.dumps(reply, default=str)
            tool_entry_done = {**tool_entry, "reply_preview": reply_str[:120]}
            # EXIT: same call_id, with reply_preview now filled.
            if self_id in _current:
                _current[self_id]["last_tool"] = tool_entry_done
            await _emit_status(
                kernel, self_id, client_id, "tool_calling", tool=tool_entry_done
            )
            return {
                "role": "tool",
                "tool_call_id": c["id"],
                "name": c["name"],
                "content": reply_str,
            }

        results = await asyncio.gather(*[_exec_one(c) for c in tool_calls])
        _invalidate_menu(self_id)
        messages.extend(results)
        new_turns.extend(results)

    # Status before the back-compat `done` event so subscribers can
    # observe the final phase transition first.
    await _emit_status(kernel, self_id, client_id, "done", reason="ok")
    await _to_caller(kernel, self_id, client_id, {"type": "done", "source": self_id})

    # Final assistant turn (the no-tool_calls completion that broke the loop).
    final_turn = {"role": "assistant", "content": last_text}
    messages.append(final_turn)
    new_turns.append(final_turn)
    # APPEND this send's new turns (user + assistant(+tool_calls) + role:tool replies +
    # final assistant) to the durable `chat` yaml_state. NOT `messages[1:]` — that's the
    # projected/trimmed view; the store keeps the FULL conversation. Keeping tool_calls +
    # role:tool replies means faulty calls are auditable and the next turn has full memory.
    if not stateless:
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
            "generating": id in _tasks and not _tasks[id].done(),
            "emits": {
                "queued": "{type:'queued', source, client_id, send_id} — emitted when send arrives but a previous generation holds the lock. The `status` event with phase='queued' carries the same signal plus structured fields.",
                "token": "{type:'token', text:str, source, client_id} — streaming chunk. Routed ONLY to the caller: cli (stdout) when client_id='cli', else this agent's own inbox (browser filters by client_id).",
                "status": "{type:'status', source, client_id, ts, phase:'queued'|'thinking'|'streaming'|'tool_calling'|'done', detail:{send_id, started_at, queue_depth, ...phase_specific}} — single channel for phase transitions. detail.tool={call_id,target,verb,args,reply_preview?} for tool_calling (entry has no reply_preview, exit re-emits same call_id with it). detail.reason='ok'|'interrupted'|'timeout'|'error' for done. detail.ahead for queued.",
                "done": "{type:'done', source, client_id} — end of generation, interrupted, or timed out. Always preceded by status(phase='done', detail.reason).",
                "context": "{type:'context', source, client_id, ts, phase:'compacted'|'too_small', detail:{...}} — the context protocol's push half. compacted: detail={strategy, dropped_turns, kept_turns, summarized} when the live view was compacted to fit the window. too_small: detail={context_window, system_tokens, hint} when even the system block + one turn won't fit (the model is NOT called — a failfast). Pull counterpart: the `context_status` verb.",
            },
            "concurrency": "Per-backend FIFO lock around `send`: one generation at a time. Other callers wait (and receive a `queued` event so their UI can show it). `reflect`/`history`/`interrupt` skip the lock and stay snappy.",
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
        """args: text:str (req), client_id:str? (default 'cli'). Streams tokens to ONLY the caller — cli (stdout) for client_id='cli', or the browser tab whose WS is subscribed and filters by client_id otherwise. Persists per-client chat.json. Per-backend FIFO lock: concurrent callers serialize. If the lock is held when this call arrives, emits both a back-compat `queued` event AND a structured `status` event (phase='queued', detail.ahead) for the caller; first `token` (or `status` of phase != queued) for the same client_id implicitly unqueues. Optional system_prompt:str REPLACES the auto-built prompt (caller-supplied role/context — read it from a state agent yourself, no yaml coupling here); optional messages:list REPLACES persisted history (fully stateless, no file_bridge_id needed). _call_stack is reserved for the recursion guard (cycle/over-depth refusal before the lock). Returns {response, final, client_id}."""
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
        # History is auto-mounted (a `chat` yaml_state, persisted through the loader) —
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
            await _to_caller(
                kernel,
                id,
                client_id,
                {"type": "queued", "source": id, "send_id": send_id},
            )
            await _emit_status(
                kernel, id, client_id, "queued", send_id=send_id, ahead=ahead
            )

        async with lock:
            # We've got the lock — pop ourselves from the queue and become
            # the in-flight entry. _set_current establishes phase='thinking'
            # and the started_at timestamp.
            _dequeue_send(id, send_id)
            _set_current(id, entry)
            await _emit_status(kernel, id, client_id, "thinking")
            task = asyncio.create_task(
                _run(
                    id,
                    text,
                    kernel,
                    client_id,
                    cfg,
                    system_prompt,
                    messages_override,
                    call_stack,
                )
            )
            _tasks[id] = task
            try:
                return await asyncio.wait_for(
                    asyncio.shield(task), timeout=cfg.send_timeout
                )
            except asyncio.TimeoutError:
                task.cancel()
                await _emit_status(kernel, id, client_id, "done", reason="timeout")
                await _to_caller(kernel, id, client_id, {"type": "done", "source": id})
                return {
                    "error": f"send: timeout after {cfg.send_timeout}s",
                    "client_id": client_id,
                }
            except asyncio.CancelledError:
                await _emit_status(kernel, id, client_id, "done", reason="interrupted")
                await _to_caller(kernel, id, client_id, {"type": "done", "source": id})
                return {"response": "", "interrupted": True, "client_id": client_id}
            except Exception as e:
                # A backend may install a custom error mapper (nvidia: clean
                # 429 / HTTP messages instead of re-raising).
                if cfg._error_mapper is not None:
                    mapped = cfg._error_mapper(e)
                    if mapped is not None:
                        await _emit_status(
                            kernel, id, client_id, "done", reason="error", error=mapped
                        )
                        await _to_caller(
                            kernel, id, client_id, {"type": "done", "source": id}
                        )
                        return {"error": mapped, "client_id": client_id}
                await _emit_status(
                    kernel, id, client_id, "done", reason="error", error=str(e)
                )
                await _to_caller(kernel, id, client_id, {"type": "done", "source": id})
                raise
            finally:
                _tasks.pop(id, None)
                _clear_current(id)

    return _send


def _make_history(cfg: Backend):
    async def _history(id, payload, kernel):
        """args: client_id:str? (default 'cli'). Returns {messages:[...], client_id} — that client's full conversation from the mounted chat yaml_state (the durable record)."""
        client_id = _safe_client(payload.get("client_id") or DEFAULT_CLIENT_ID)
        return {
            "messages": await _load_history(id, kernel, client_id),
            "client_id": client_id,
        }

    return _history


async def _interrupt(id, payload, kernel):
    """No args. Cancels any in-flight `send` (releases the per-backend lock immediately). Returns {interrupted:bool}."""
    task = _tasks.get(id)
    if task and not task.done():
        task.cancel()
        return {"interrupted": True}
    return {"interrupted": False}


async def _refresh_menu(id, payload, kernel):
    """No args. Drops the cached agent menu so the next user turn rebuilds it from live reflect. Useful right after the LLM creates/deletes/updates an agent. Returns {refreshed:true}."""
    _invalidate_menu(id)
    return {"refreshed": True}


async def _status(id, payload, kernel):
    """args: client_id:str? — when provided, returns a privacy-filtered snapshot: full text only for this client's queue entries / current generation; other clients collapse to `others_pending` (an integer count). Without client_id the snapshot redacts text everywhere. Used by chat UIs to rebuild queued/in-flight state on page reload (and for headless polling). Returns {source, client_id, generating, current:{phase, send_id, started_at, elapsed, is_mine, text?, text_so_far?, last_tool?}|null, mine_pending:[{send_id,text,queued_at}], others_pending:int}."""
    cid = payload.get("client_id")
    if cid:
        cid = _safe_client(cid)
    return _status_snapshot(id, cid)


def _recall_render(m: dict) -> str:
    """Compact ONE stored turn for a recall reply: bulky tool-call JSON → a short marker,
    content capped — so paging back can't itself blow the window. Bounds the REPLY only."""
    content = m.get("content")
    if not content and m.get("tool_calls"):
        names = [(tc.get("function") or {}).get("name") for tc in m["tool_calls"]]
        content = "[tool_calls: " + ", ".join(n for n in names if n) + "]"
    s = content if isinstance(content, str) else json.dumps(content, default=str)
    return s[:2000]


async def _recall(id, payload, kernel):
    """args: client_id?:str (default 'cli'), query?:str (case-insensitive substring over the
    serialized turn — matches tool args/replies too), limit?:int (default 20, max 100),
    before?:int (store index — page backward). Pages turns back from the DURABLE chat store
    (the FULL conversation, never trimmed — so anything compaction dropped is one call away).
    Read-only. Returns {messages:[{index,role,content}], total, truncated, client_id}."""
    client_id = _safe_client(payload.get("client_id") or DEFAULT_CLIENT_ID)
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
    return {
        "messages": [
            {"index": i, "role": m.get("role"), "content": _recall_render(m)}
            for i, m in page
        ],
        "total": len(indexed),
        "truncated": truncated,
        "client_id": client_id,
    }


async def _derive_reaction(id, kernel) -> dict | None:
    """Read-model over the durable transcript: AFTER the last compaction notice (its
    `fired_at_index`), did the model react? Scans the same client's thread for `send`
    tool-calls — a `recall` to its OWN id, or a memory write (`set`/`append`/`replace`)
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
        for tc in m.get("tool_calls") or []:
            fn = tc.get("function") or {}
            args = fn.get("arguments")
            if isinstance(args, str):
                try:
                    args = json.loads(args)
                except (json.JSONDecodeError, ValueError):
                    args = {}
            if not isinstance(args, dict):
                args = {}
            payload = args.get("payload")
            ptype = payload.get("type") if isinstance(payload, dict) else None
            if args.get("target_id") == id and ptype == "recall":
                recalled = True
                recall_count += 1
            elif ptype in ("set", "append", "replace"):
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
    return None


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
    tool_args_as_json: bool = False,
    require_provider: bool = False,
    provider_missing_error: str | None = None,
    error_mapper: Callable | None = None,
) -> tuple[dict, Callable]:
    """Bind one backend's config into a fresh `(VERBS, handler)` pair.

    The verb closures capture the returned `Backend`, so the per-backend
    config (sentence, provider builder, tool-args shape, stream wrapper,
    error prefix) is isolated; the module-global STATE (`_providers`,
    `_queue`, `_current`, `_menu_cache`, `_tasks`, `_locks`) is shared and
    keyed by agent id, so multiple backends coexist in one kernel.

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
        into VERBS (nvidia: set_api_key / clear_api_key).
      reflect_extra: fn(id, kernel) -> dict (sync OR async), merged into
        the reflect reply (nvidia: {"has_api_key": ...}).
      stream_wrapper: fn(provider, messages, kernel, id, client_id) ->
        async-iter, wrapping provider.chat() (nvidia: 429 retry).
      tool_args_as_json: True serializes tool_call arguments to a JSON
        string in the persisted assistant turn (OpenAI shape, nvidia).
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
        tool_args_as_json=tool_args_as_json,
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
        "interrupt": _interrupt,
        "refresh_menu": _refresh_menu,
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
