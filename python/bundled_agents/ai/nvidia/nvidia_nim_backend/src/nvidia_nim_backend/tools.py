"""nvidia_nim_backend — reflect-driven LLM agent against NVIDIA NIM (OpenAI-compatible).

A THIN binding over `ai_core` (see ollama_backend.tools for the base
pattern). Same surface as ollama_backend (send/history/interrupt/
refresh_menu/reflect) PLUS the NIM-specific bits, all wired through
`ai_core.build()`:

- API key required, stored OUT-OF-BAND at the store-relative `agents/<id>/api_key`
  via `file_bridge_id` (never in agent.json). Verbs `set_api_key` /
  `clear_api_key` (passed as `extra_verbs`); `reflect` reports
  `has_api_key:bool` only (via `reflect_extra`). The key/sidecar logic
  stays HERE — ai_core never sees it.
- `make_provider` is ASYNC (reads the sidecar) and returns None when no
  key is set; `require_provider=True` makes `send` failfast cleanly.
- Free tier rate-limits at ~40 RPM/model. `_stream_with_rate_limit_retry`
  (passed as `stream_wrapper`) retries once on a pre-stream HTTP 429.
- OpenAI wants tool_call arguments as a JSON string → `tool_args_as_json=True`.
- `error_mapper` turns a provider httpx.HTTPStatusError into a clean
  caller-facing error inside `send`.

The shared state dicts + `SEND_TIMEOUT` / `MAX_CALL_DEPTH` are re-exported
from `ai_core.core` so the existing monkeypatch seams keep working on the
shared path; `_parse_retry_after` / `RATE_LIMIT_*` / `_key_path` are
nvidia-local (the tests patch/read them here).
"""

from __future__ import annotations

import asyncio

import httpx

from ai_core import build
from ai_core.core import (  # noqa: F401 — re-exported test seams
    DEFAULT_CLIENT_ID,
    MAX_CALL_DEPTH,
    SEND_TIMEOUT,
    SEND_TOOL,
    _assemble,
    _build_menu,
    _current,
    _emit_status,
    _file_bridge_id,
    _invalidate_menu,
    _locks,
    _menu_cache,
    _providers,
    _queue,
    _tasks,
)

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


# ─── api_key sidecar (nvidia-local; routed through file_bridge_id) ──


def _key_path(self_id: str) -> str:
    """Sidecar file holding the API key, STORE-RELATIVE (`agents/<id>/…`): wired to the
    `.fantastic` store it lands next to the agent's own agent.json (no double-nest).
    Kept out of agent.json so it never leaks through `kernel.list()` or any reflect."""
    return f"agents/{self_id}/api_key"


async def _read_api_key(self_id: str, kernel) -> str | None:
    fid = _file_bridge_id(self_id, kernel)
    if not fid:
        return None
    r = await kernel.send(fid, {"type": "read", "path": _key_path(self_id)})
    if not r or "content" not in r:
        return None
    key = (r.get("content") or "").strip()
    return key or None


async def _has_api_key(self_id: str, kernel) -> bool:
    return bool(await _read_api_key(self_id, kernel))


async def make_provider(id, kernel):
    """Async provider builder — reads the api_key sidecar; returns None
    (→ clean failfast) when no key is set yet."""
    key = await _read_api_key(id, kernel)
    if not key:
        return None
    from nvidia_nim_backend.provider import (
        DEFAULT_ENDPOINT,
        DEFAULT_MODEL,
        NvidiaNimProvider,
    )

    rec = kernel.get(id) or {}
    return NvidiaNimProvider(
        api_key=key,
        endpoint=rec.get("endpoint", DEFAULT_ENDPOINT),
        model=rec.get("model", DEFAULT_MODEL),
    )


# ─── rate-limit retry stream wrapper (nvidia-local) ────────────


async def _stream_with_rate_limit_retry(
    provider, messages, kernel, self_id: str, client_id: str
):
    """Wrap provider.chat() with retry-once-on-429.

    On HTTP 429 BEFORE any chunk is yielded, sleep `Retry-After` (clamped)
    and retry once. Mid-stream 429 (rare — quota usually checked up front)
    or any non-429 error propagates unchanged. A
    `status(thinking, waiting_on='rate_limit')` event surfaces the
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
            await _emit_status(
                kernel,
                self_id,
                client_id,
                "thinking",
                waiting_on="rate_limit",
                wait_s=wait,
            )
            await asyncio.sleep(wait)


def _map_error(e: Exception) -> str | None:
    """Map a provider httpx.HTTPStatusError to a clean caller-facing
    error (used by ai_core's send). Returns None for other exceptions
    (which propagate / surface as generic errors)."""
    if isinstance(e, httpx.HTTPStatusError):
        code = e.response.status_code
        if code == 429:
            wait = _parse_retry_after(e.response)
            return f"send: rate limited (429); retry in {wait}s"
        return f"send: provider HTTP {code}"
    return None


# ─── api_key verbs (nvidia-local; merged via extra_verbs) ──────


async def _set_api_key(id, payload, kernel):
    """args: api_key:str (req). Persists the key to `.fantastic/agents/<id>/api_key` via file_bridge_id. Drops the cached provider so the next send reads fresh. Failfast if file_bridge_id unset or key empty."""
    if not _file_bridge_id(id, kernel):
        return {"error": "nvidia_nim_backend: file_bridge_id required"}
    key = payload.get("api_key", "")
    if not isinstance(key, str) or not key.strip():
        return {"error": "set_api_key: api_key must be a non-empty string"}
    fid = _file_bridge_id(id, kernel)
    r = await kernel.send(
        fid,
        {"type": "write", "path": _key_path(id), "content": key.strip()},
    )
    if r and r.get("error"):
        return {"error": f"set_api_key: file write failed: {r['error']}"}
    _providers.pop(id, None)
    return {"ok": True}


async def _clear_api_key(id, payload, kernel):
    """No args. Deletes the api_key sidecar via file_bridge_id and drops the cached provider. Returns {ok:true, deleted:bool}."""
    if not _file_bridge_id(id, kernel):
        return {"error": "nvidia_nim_backend: file_bridge_id required"}
    fid = _file_bridge_id(id, kernel)
    r = await kernel.send(fid, {"type": "delete", "path": _key_path(id)})
    _providers.pop(id, None)
    return {"ok": True, "deleted": bool((r or {}).get("deleted", False))}


async def _reflect_extra(id, kernel):
    """Merged into reflect: has_api_key boolean (never the key value)."""
    return {"has_api_key": await _has_api_key(id, kernel)}


def _build():
    from nvidia_nim_backend.provider import DEFAULT_ENDPOINT, DEFAULT_MODEL

    return build(
        sentence="NVIDIA NIM-backed LLM agent (OpenAI-compatible, native tool-calling).",
        default_model=DEFAULT_MODEL,
        default_endpoint=DEFAULT_ENDPOINT,
        make_provider=make_provider,
        name="nvidia_nim_backend",
        module_name=__name__,
        extra_verbs={"set_api_key": _set_api_key, "clear_api_key": _clear_api_key},
        reflect_extra=_reflect_extra,
        stream_wrapper=_stream_with_rate_limit_retry,
        tool_args_as_json=True,
        require_provider=True,
        provider_missing_error="nvidia_nim_backend: api_key not set; call set_api_key first",
        error_mapper=_map_error,
    )


VERBS, handler = _build()
