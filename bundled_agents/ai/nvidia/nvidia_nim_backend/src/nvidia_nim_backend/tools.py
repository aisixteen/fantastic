"""nvidia_nim_backend — reflect-driven LLM agent against NVIDIA NIM (OpenAI-compatible).

Same surface as ollama_backend (send/history/interrupt/refresh_menu/reflect)
so the chat UI (`ai_chat_webapp`) and any other caller can swap providers
by changing `upstream_id` only. Differences from ollama_backend:

- API key required. Stored OUT-OF-BAND in `.fantastic/agents/<id>/api_key`
  via `file_agent_id`. Never lives in agent.json (which any reflect
  caller can see). Verbs: `set_api_key`, `clear_api_key`.
  `reflect` reports `has_api_key:bool` only — never the key value.
- Provider speaks OpenAI HTTP+SSE; tool_call argument fragments are
  aggregated per index inside the provider. The yield contract is
  identical to OllamaProvider, so `_run` is mechanically the same.
- Free tier rate-limits at ~40 RPM/model. On HTTP 429 BEFORE any chunk
  has been yielded, we retry once after sleeping `Retry-After` (capped
  at RATE_LIMIT_MAX_WAIT). `say` event surfaces the wait so the chat UI
  shows progress. Mid-stream 429 is rare and propagates unchanged.

The agentic loop runs UNTIL the model stops emitting tool_calls —
bounded only by SEND_TIMEOUT (hard) and the `interrupt` verb.
"""

from __future__ import annotations

import asyncio
import json
import secrets
import time

import httpx

_providers: dict = {}
_tasks: dict[str, asyncio.Task] = {}
_locks: dict[str, asyncio.Lock] = {}

_menu_cache: dict[str, list[dict]] = {}

# Structural state for the `status` verb + status events. Mirrors
# ollama_backend/tools.py — see that file for the rationale + phase
# state machine. Phases:
#   queued → thinking → streaming → tool_calling → thinking → … → done.
_queue: dict[str, list[dict]] = {}
_current: dict[str, dict] = {}


# ─── status state helpers ───────────────────────────────────────


def _new_send_id() -> str:
    """Opaque id for one user submission. Travels through every status
    event of that submission so the UI can correlate phases."""
    return secrets.token_urlsafe(8)


def _enqueue(self_id: str, entry: dict) -> int:
    q = _queue.setdefault(self_id, [])
    q.append(entry)
    return len(q)


def _dequeue_send(self_id: str, send_id: str) -> dict | None:
    q = _queue.get(self_id, [])
    for i, e in enumerate(q):
        if e.get("send_id") == send_id:
            return q.pop(i)
    return None


def _set_current(self_id: str, entry: dict) -> None:
    _current[self_id] = {
        **entry,
        "started_at": time.time(),
        "phase": "thinking",
        "text_so_far": "",
    }


def _clear_current(self_id: str) -> None:
    _current.pop(self_id, None)


def _redact_text(d: dict | None) -> dict | None:
    if d is None:
        return None
    return {k: v for k, v in d.items() if k not in ("text", "text_so_far", "last_tool")}


def _status_snapshot(self_id: str, requesting_client_id: str | None) -> dict:
    """Privacy-filtered snapshot. With requesting_client_id: full text
    only for that caller's entries; other clients collapse to
    `others_pending` count. Without it: text is redacted everywhere."""
    cur = _current.get(self_id)
    queue = list(_queue.get(self_id, []))
    if requesting_client_id:
        is_mine = bool(cur and cur.get("client_id") == requesting_client_id)
        if cur is None:
            cur_out: dict | None = None
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


SEND_TIMEOUT = 180.0
DEFAULT_CLIENT_ID = "cli"

# Free tier of NIM rate-limits at ~40 RPM/model. On 429 we retry ONCE
# (per provider.chat call) after honoring `Retry-After`, capped so a
# misbehaving server can't pin us forever inside SEND_TIMEOUT.
RATE_LIMIT_MAX_WAIT = 60
RATE_LIMIT_DEFAULT_WAIT = 5
RATE_LIMIT_MAX_RETRIES = 1


def _parse_retry_after(resp: "httpx.Response") -> int:
    """Read Retry-After (seconds), clamp to [1, RATE_LIMIT_MAX_WAIT]."""
    val = resp.headers.get("retry-after", "")
    try:
        n = int(val)
    except (TypeError, ValueError):
        return RATE_LIMIT_DEFAULT_WAIT
    if n < 1:
        return 1
    return min(n, RATE_LIMIT_MAX_WAIT)


def _lock_for(self_id: str) -> asyncio.Lock:
    """Per-backend FIFO serializer: only one `_send` runs at a time per agent."""
    if self_id not in _locks:
        _locks[self_id] = asyncio.Lock()
    return _locks[self_id]


SEND_TOOL = {
    "type": "function",
    "function": {
        "name": "send",
        "description": (
            "Send a message to any agent in the Fantastic substrate. "
            "Universal verb on every agent: reflect (returns identity + state). "
            "Discover agents by sending list_agents to the core agent."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "target_id": {
                    "type": "string",
                    "description": "Agent id to send the payload to (e.g. 'core', 'cli', 'terminal_xxx').",
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


# ─── persistence (routed through file_agent_id) ────────────────


def _safe_client(client_id: str) -> str:
    s = (client_id or DEFAULT_CLIENT_ID).strip() or DEFAULT_CLIENT_ID
    return "".join(c if (c.isalnum() or c in "._-") else "_" for c in s)[:64]


def _chat_path(self_id: str, client_id: str) -> str:
    return f".fantastic/agents/{self_id}/chat_{_safe_client(client_id)}.json"


def _key_path(self_id: str) -> str:
    """Sidecar file holding the API key. Kept out of agent.json so it
    never leaks through `kernel.list()` or any reflect output."""
    return f".fantastic/agents/{self_id}/api_key"


def _file_agent_id(self_id: str, kernel) -> str | None:
    rec = kernel.get(self_id) or {}
    return rec.get("file_agent_id")


async def _load_history(self_id: str, kernel, client_id: str) -> list[dict]:
    fid = _file_agent_id(self_id, kernel)
    if not fid:
        return []
    r = await kernel.send(fid, {"type": "read", "path": _chat_path(self_id, client_id)})
    if not r or "content" not in r:
        return []
    try:
        return json.loads(r["content"])
    except json.JSONDecodeError:
        return []


async def _save_history(
    self_id: str, kernel, client_id: str, messages: list[dict]
) -> None:
    fid = _file_agent_id(self_id, kernel)
    if not fid:
        return
    await kernel.send(
        fid,
        {
            "type": "write",
            "path": _chat_path(self_id, client_id),
            "content": json.dumps(messages, indent=2),
        },
    )


async def _read_api_key(self_id: str, kernel) -> str | None:
    fid = _file_agent_id(self_id, kernel)
    if not fid:
        return None
    r = await kernel.send(fid, {"type": "read", "path": _key_path(self_id)})
    if not r or "content" not in r:
        return None
    key = (r.get("content") or "").strip()
    return key or None


async def _has_api_key(self_id: str, kernel) -> bool:
    return bool(await _read_api_key(self_id, kernel))


async def _get_provider(self_id: str, kernel):
    """Returns a cached NvidiaNimProvider or None when no api_key is set yet."""
    if self_id in _providers:
        return _providers[self_id]
    key = await _read_api_key(self_id, kernel)
    if not key:
        return None
    from nvidia_nim_backend.provider import (
        DEFAULT_ENDPOINT,
        DEFAULT_MODEL,
        NvidiaNimProvider,
    )

    rec = kernel.get(self_id) or {}
    endpoint = rec.get("endpoint", DEFAULT_ENDPOINT)
    model = rec.get("model", DEFAULT_MODEL)
    _providers[self_id] = NvidiaNimProvider(api_key=key, endpoint=endpoint, model=model)
    return _providers[self_id]


# ─── prompt assembly (Phase 1) ──────────────────────────────────


def _render_reflect(d: dict) -> str:
    d = dict(d)
    sentence = d.pop("sentence", "")
    fields = "  ".join(
        f"{k}={json.dumps(v) if not isinstance(v, str) else v}" for k, v in d.items()
    )
    return f"{sentence}  {fields}".strip()


async def _build_menu(self_id: str, kernel) -> list[dict]:
    online = await kernel.send("core", {"type": "list_agents"})
    items: list[dict] = []
    for a in online.get("agents", []):
        if a["id"] == self_id:
            continue
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
    if not menu:
        return "## Available agents\n(none — only `core` and `self`)"
    lines = [
        "## Available agents (reflect on any for full verb signatures + arg shapes)"
    ]
    for m in menu:
        verbs = m.get("verbs") or []
        head = ", ".join(verbs[:10]) + (" …" if len(verbs) > 10 else "")
        lines.append(f"- `{m['id']}` — {m['sentence']} — verbs: {head or '(none)'}")
    return "\n".join(lines)


_SEND_HOWTO = """## How to use the `send` tool
You have ONE tool: `send(target_id, payload)`. EVERY action goes through it.
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


async def _assemble(self_id: str, user_text: str, kernel, client_id: str) -> list[dict]:
    primer = await kernel.send("kernel", {"type": "reflect"})
    me = await kernel.send(self_id, {"type": "reflect"})

    if self_id not in _menu_cache:
        _menu_cache[self_id] = await _build_menu(self_id, kernel)
    menu = _menu_cache[self_id]

    sys_blocks = [
        _render_reflect(primer),
        f"You are `{self_id}`. " + _render_reflect(me),
        _render_menu(menu),
        _SEND_HOWTO,
    ]
    messages: list[dict] = [{"role": "system", "content": "\n\n".join(sys_blocks)}]
    messages.extend(await _load_history(self_id, kernel, client_id))
    messages.append({"role": "user", "content": user_text})
    return messages


# ─── streaming + native tool-calls (Phase 2) ────────────────────


async def _to_caller(kernel, self_id: str, client_id: str, ev: dict) -> None:
    ev = {**ev, "client_id": client_id}
    if client_id == DEFAULT_CLIENT_ID:
        await kernel.send("cli", ev)
    else:
        await kernel.emit(self_id, ev)


async def _emit_status(
    kernel, self_id: str, client_id: str, phase: str, **detail
) -> None:
    """Mirror of ollama_backend's _emit_status. See that file for the
    rationale + phase state machine."""
    cur = _current.get(self_id)
    if cur is not None:
        cur["phase"] = phase
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


async def _stream_with_rate_limit_retry(
    provider, messages, kernel, self_id: str, client_id: str
):
    """Wrap provider.chat() with retry-once-on-429.

    On HTTP 429 BEFORE any chunk is yielded, sleep `Retry-After` (clamped)
    and retry once. Mid-stream 429 (rare — quota usually checked up front)
    or any non-429 error propagates unchanged. A back-compat `say` event
    AND a `status(thinking, waiting_on='rate_limit')` event surface the
    wait so the chat UI can pulse "waiting on provider".
    """
    attempt = 0
    while True:
        yielded_anything = False
        try:
            async for chunk in provider.chat(messages, tools=[SEND_TOOL]):
                yielded_anything = True
                yield chunk
            return
        except httpx.HTTPStatusError as e:
            if (
                yielded_anything
                or e.response.status_code != 429
                or attempt >= RATE_LIMIT_MAX_RETRIES
            ):
                raise
            attempt += 1
            wait = _parse_retry_after(e.response)
            await _to_caller(
                kernel,
                self_id,
                client_id,
                {
                    "type": "say",
                    "text": f"[provider rate limited (429); waiting {wait}s]",
                    "source": self_id,
                },
            )
            await _emit_status(
                kernel,
                self_id,
                client_id,
                "thinking",
                waiting_on="rate_limit",
                wait_s=wait,
            )
            await asyncio.sleep(wait)


async def _run(self_id: str, user_text: str, kernel, client_id: str) -> dict:
    provider = await _get_provider(self_id, kernel)
    # Caller (`_send`) pre-checks api_key; this branch is defensive.
    if provider is None:
        return {"error": "nvidia_nim_backend: api_key not set"}
    messages = await _assemble(self_id, user_text, kernel, client_id)
    last_text = ""

    iteration = 0
    while True:
        iteration += 1
        if iteration > 1:
            # Re-entering the loop after tool_calls — phase transitions
            # back to thinking while the model decides its next move.
            await _emit_status(kernel, self_id, client_id, "thinking")
        content_parts: list[str] = []
        tool_calls: list[dict] = []
        first_chunk = True
        async for chunk in _stream_with_rate_limit_retry(
            provider, messages, kernel, self_id, client_id
        ):
            if isinstance(chunk, str):
                if first_chunk:
                    await _emit_status(kernel, self_id, client_id, "streaming")
                    first_chunk = False
                content_parts.append(chunk)
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

        # OpenAI wants tool_call.function.arguments as a JSON string.
        messages.append(
            {
                "role": "assistant",
                "content": last_text,
                "tool_calls": [
                    {
                        "id": c["id"],
                        "type": "function",
                        "function": {
                            "name": c["name"],
                            "arguments": json.dumps(c["arguments"]),
                        },
                    }
                    for c in tool_calls
                ],
            }
        )

        # Execute tool_calls IN PARALLEL via asyncio.gather. Order
        # preserved in results; role:tool messages append in the order
        # the model emitted them.
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
            if self_id in _current:
                _current[self_id]["last_tool"] = tool_entry
            await _emit_status(
                kernel, self_id, client_id, "tool_calling", tool=tool_entry
            )
            try:
                reply = await kernel.send(target, payload)
            except Exception as e:
                reply = {"error": str(e)}
            reply_str = json.dumps(reply, default=str)
            tool_entry_done = {**tool_entry, "reply_preview": reply_str[:120]}
            if self_id in _current:
                _current[self_id]["last_tool"] = tool_entry_done
            await _emit_status(
                kernel, self_id, client_id, "tool_calling", tool=tool_entry_done
            )
            await _to_caller(
                kernel,
                self_id,
                client_id,
                {
                    "type": "say",
                    "text": f"[tool {target} -> {reply_str[:120]}]",
                    "source": self_id,
                },
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

    # Status before the back-compat `done` event.
    await _emit_status(kernel, self_id, client_id, "done", reason="ok")
    await _to_caller(kernel, self_id, client_id, {"type": "done", "source": self_id})

    history = await _load_history(self_id, kernel, client_id)
    history.append({"role": "user", "content": user_text})
    history.append({"role": "assistant", "content": last_text})
    await _save_history(self_id, kernel, client_id, history)
    return {"response": last_text, "final": last_text, "client_id": client_id}


# ─── verbs ──────────────────────────────────────────────────────


async def _reflect(id, payload, kernel):
    """Identity + model + endpoint + has_api_key + generating + file_agent_id binding. No args. The api_key value itself is NEVER returned — only the boolean."""
    rec = kernel.get(id) or {}
    from nvidia_nim_backend.provider import DEFAULT_ENDPOINT, DEFAULT_MODEL

    return {
        "id": id,
        "sentence": "NVIDIA NIM-backed LLM agent (OpenAI-compatible, native tool-calling).",
        "model": rec.get("model", DEFAULT_MODEL),
        "endpoint": rec.get("endpoint", DEFAULT_ENDPOINT),
        "file_agent_id": rec.get("file_agent_id"),
        "has_api_key": await _has_api_key(id, kernel),
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
        "generating": id in _tasks and not _tasks[id].done(),
        "emits": {
            "queued": "{type:'queued', source, client_id, send_id} — back-compat: emitted when send arrives but a previous generation holds the lock. The new `status` event with phase='queued' carries the same signal plus structured fields.",
            "token": "{type:'token', text:str, source, client_id} — streaming chunk. Routed ONLY to the caller.",
            "say": "{type:'say', text:'[tool target -> reply…]', source, client_id} — per tool_call summary, plus rate-limit notices on 429 retry. Back-compat; `status` with phase='tool_calling' or detail.waiting_on='rate_limit' carries the same signal structured.",
            "status": "{type:'status', source, client_id, ts, phase:'queued'|'thinking'|'streaming'|'tool_calling'|'done', detail:{send_id, started_at, queue_depth, ...phase_specific}} — single channel for phase transitions. detail.tool={call_id,target,verb,args,reply_preview?} for tool_calling (entry has no reply_preview, exit re-emits same call_id with it). detail.reason='ok'|'interrupted'|'timeout'|'error' for done. detail.ahead for queued. detail.waiting_on='rate_limit' + wait_s during 429 backoff.",
            "done": "{type:'done', source, client_id} — back-compat: end of generation, interrupted, or timed out. Always preceded by status(phase='done', detail.reason).",
        },
        "concurrency": "Per-backend FIFO lock around `send`; reflect/history/interrupt/set_api_key/clear_api_key skip the lock.",
    }


async def _send(id, payload, kernel):
    """args: text:str (req), client_id:str? (default 'cli'). Same surface as ollama_backend.send. Failfast if file_agent_id unset OR api_key not set (call set_api_key first). Streams tokens to ONLY the caller. Persists per-client chat.json. Per-backend FIFO lock. Emits both a back-compat `queued` event AND a structured `status` event (phase='queued', detail.ahead) when contended; first `token`/`status(thinking)` for the same client_id implicitly unqueues."""
    if not _file_agent_id(id, kernel):
        return {"error": "nvidia_nim_backend: file_agent_id required"}
    provider = await _get_provider(id, kernel)
    if provider is None:
        return {"error": "nvidia_nim_backend: api_key not set; call set_api_key first"}
    text = payload.get("text", "")
    client_id = _safe_client(payload.get("client_id") or DEFAULT_CLIENT_ID)
    send_id = _new_send_id()
    entry = {
        "client_id": client_id,
        "text": text,
        "send_id": send_id,
        "queued_at": time.time(),
    }
    _enqueue(id, entry)

    lock = _lock_for(id)
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
        _dequeue_send(id, send_id)
        _set_current(id, entry)
        await _emit_status(kernel, id, client_id, "thinking")
        task = asyncio.create_task(_run(id, text, kernel, client_id))
        _tasks[id] = task
        try:
            return await asyncio.wait_for(asyncio.shield(task), timeout=SEND_TIMEOUT)
        except asyncio.TimeoutError:
            task.cancel()
            await _emit_status(kernel, id, client_id, "done", reason="timeout")
            await _to_caller(kernel, id, client_id, {"type": "done", "source": id})
            return {
                "error": f"send: timeout after {SEND_TIMEOUT}s",
                "client_id": client_id,
            }
        except asyncio.CancelledError:
            await _emit_status(kernel, id, client_id, "done", reason="interrupted")
            await _to_caller(kernel, id, client_id, {"type": "done", "source": id})
            return {"response": "", "interrupted": True, "client_id": client_id}
        except httpx.HTTPStatusError as e:
            # Rate-limit retries exhausted, or non-429 HTTP error from
            # the provider. Surface cleanly to the caller.
            code = e.response.status_code
            if code == 429:
                wait = _parse_retry_after(e.response)
                msg = f"send: rate limited (429); retry in {wait}s"
            else:
                msg = f"send: provider HTTP {code}"
            await _emit_status(kernel, id, client_id, "done", reason="error", error=msg)
            await _to_caller(kernel, id, client_id, {"type": "done", "source": id})
            return {"error": msg, "client_id": client_id}
        except Exception as e:
            await _emit_status(
                kernel, id, client_id, "done", reason="error", error=str(e)
            )
            await _to_caller(kernel, id, client_id, {"type": "done", "source": id})
            raise
        finally:
            _tasks.pop(id, None)
            _clear_current(id)


async def _history(id, payload, kernel):
    """args: client_id:str? (default 'cli'). Returns {messages:[...], client_id}. Failfast if file_agent_id unset."""
    if not _file_agent_id(id, kernel):
        return {"error": "nvidia_nim_backend: file_agent_id required"}
    client_id = _safe_client(payload.get("client_id") or DEFAULT_CLIENT_ID)
    return {
        "messages": await _load_history(id, kernel, client_id),
        "client_id": client_id,
    }


async def _interrupt(id, payload, kernel):
    """No args. Cancels any in-flight `send`. Returns {interrupted:bool}."""
    task = _tasks.get(id)
    if task and not task.done():
        task.cancel()
        return {"interrupted": True}
    return {"interrupted": False}


async def _refresh_menu(id, payload, kernel):
    """No args. Drops the cached agent menu so the next user turn rebuilds it from live reflect. Returns {refreshed:true}."""
    _invalidate_menu(id)
    return {"refreshed": True}


async def _set_api_key(id, payload, kernel):
    """args: api_key:str (req). Persists the key to `.fantastic/agents/<id>/api_key` via file_agent_id. Drops the cached provider so the next send reads fresh. Failfast if file_agent_id unset or key empty."""
    if not _file_agent_id(id, kernel):
        return {"error": "nvidia_nim_backend: file_agent_id required"}
    key = payload.get("api_key", "")
    if not isinstance(key, str) or not key.strip():
        return {"error": "set_api_key: api_key must be a non-empty string"}
    fid = _file_agent_id(id, kernel)
    r = await kernel.send(
        fid,
        {"type": "write", "path": _key_path(id), "content": key.strip()},
    )
    if r and r.get("error"):
        return {"error": f"set_api_key: file write failed: {r['error']}"}
    _providers.pop(id, None)
    return {"ok": True}


async def _clear_api_key(id, payload, kernel):
    """No args. Deletes the api_key sidecar via file_agent_id and drops the cached provider. Returns {ok:true, deleted:bool}."""
    if not _file_agent_id(id, kernel):
        return {"error": "nvidia_nim_backend: file_agent_id required"}
    fid = _file_agent_id(id, kernel)
    r = await kernel.send(fid, {"type": "delete", "path": _key_path(id)})
    _providers.pop(id, None)
    return {"ok": True, "deleted": bool((r or {}).get("deleted", False))}


async def _status(id, payload, kernel):
    """args: client_id:str? — when provided, returns a privacy-filtered snapshot: full text only for this client's queue entries / current generation; other clients collapse to `others_pending` (an integer count). Without client_id the snapshot redacts text everywhere. Used by chat UIs to rebuild queued/in-flight state on page reload (and for headless polling). Returns {source, client_id, generating, current:{phase, send_id, started_at, elapsed, is_mine, text?, text_so_far?, last_tool?}|null, mine_pending:[{send_id,text,queued_at}], others_pending:int}."""
    cid = payload.get("client_id")
    if cid:
        cid = _safe_client(cid)
    return _status_snapshot(id, cid)


async def _boot(id, payload, kernel):
    """No-op. Returns None."""
    return None


# ─── dispatch ───────────────────────────────────────────────────


VERBS = {
    "reflect": _reflect,
    "send": _send,
    "history": _history,
    "interrupt": _interrupt,
    "refresh_menu": _refresh_menu,
    "set_api_key": _set_api_key,
    "clear_api_key": _clear_api_key,
    "status": _status,
    "boot": _boot,
}


async def handler(id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    fn = VERBS.get(t)
    if fn is None:
        return {"error": f"nvidia_nim_backend: unknown type {t!r}"}
    return await fn(id, payload, kernel)
