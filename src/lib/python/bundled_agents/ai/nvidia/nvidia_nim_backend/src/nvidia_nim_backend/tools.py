"""nvidia_nim_backend — reflect-driven LLM agent against NVIDIA NIM (OpenAI-compatible).

A THIN binding over `ai_core` (see ollama_backend.tools for the base
pattern). Same surface as ollama_backend (send/history/status/recall/
context_status/reflect) PLUS the NIM-specific bits, all wired through
`ai_core.build()`:

- API key required, read from the ENVIRONMENT (`NVIDIA_API_KEY`) at call time.
  There is no key-storage verb and no sidecar: nothing in this bundle writes the
  key to disk, so it cannot leak through `kernel.list()` or any reflect.
  `reflect` reports `has_api_key:bool` only (via `reflect_extra`).
- `make_provider` returns None when the variable is unset; `require_provider=True`
  makes `send` failfast cleanly, NAMING the variable.
- Free tier rate-limits at ~40 RPM/model. `_stream_with_rate_limit_retry`
  (passed as `stream_wrapper`) retries once on a pre-stream HTTP 429.
- Tool-calling is RAW (owned by ai_core): the provider streams plain text and
  ai_core parses the `<call>` element — no native tools array, no JSON.
- `error_mapper` turns a provider httpx.HTTPStatusError into a clean
  caller-facing error inside `send`.

The shared state dicts + `SEND_TIMEOUT` / `MAX_CALL_DEPTH` are re-exported
from `ai_core.core` so the existing monkeypatch seams keep working on the
shared path; `_parse_retry_after` / `RATE_LIMIT_*` / `KEY_VAR` are
nvidia-local (the tests patch/read them here). `on_cancel` — the shared
bundle CANCEL hook — is re-exported too, so the substrate discovers it on
this module (there is no `interrupt` verb: an in-flight `send` is a kernel
TASK, killed with the universal `ps` → `cancel` system verbs).
"""

from __future__ import annotations

import asyncio
import os

import httpx

from ai_core import build, required_meta
from ai_core.schema import field, schema, validate
from ai_core.core import (  # noqa: F401 — re-exported test seams + the cancel hook
    DEFAULT_CLIENT_ID,
    MAX_CALL_DEPTH,
    SEND_TIMEOUT,
    _assemble,
    _current,
    _emit_status,
    _locks,
    _providers,
    _queue,
    on_cancel,
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


# ─── the api key: ENVIRONMENT ONLY ─────────────────────────────


#: The variable carrying the key. Named in the failure text so an unwired
#: connector says exactly what to set.
KEY_VAR = "NVIDIA_API_KEY"
NAME = "nvidia_nim_backend"

#: THE RECORD SCHEMA (lib request #3) — what this kind's record takes, read with
#: no agent existing (`reflect bundles=all`), from the constants the provider runs
#: on. `validate_meta` enforces it at the write, so the page and the kernel cannot
#: disagree about what a record needs.
RECORD_SCHEMA = schema(
    [
        field("endpoint", "str", "the NIM OpenAI-compatible URL", required=True),
        field("model", "str", "the model name", required=True),
    ],
    key_var=KEY_VAR,
)


def validate_meta(meta: dict, record: dict) -> str | None:
    """Refuse, AT THE WRITE, a record missing a required field or naming an enum
    value outside its set — every missing field named at once, from the schema."""
    return validate(RECORD_SCHEMA, NAME, meta, record)


def _read_api_key() -> str | None:
    """The key, from the environment. Nothing else is consulted — not a
    sidecar, not the agent record."""
    return (os.environ.get(KEY_VAR) or "").strip() or None


def _no_key_error() -> str:
    return f"nvidia_nim_backend: no api key — set {KEY_VAR}"


async def make_provider(id, kernel):
    """Provider builder — reads the key from the environment; returns None
    (→ clean failfast) when the variable is unset."""
    key = _read_api_key()
    if not key:
        return None
    from nvidia_nim_backend.provider import (
        NvidiaNimProvider,
    )

    rec = kernel.get(id) or {}
    return NvidiaNimProvider(
        api_key=key,
        endpoint=required_meta(rec, "endpoint", NAME),
        model=required_meta(rec, "model", NAME),
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
            async for chunk in provider.chat(messages):
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


async def _reflect_extra(id, kernel):
    """Merged into reflect: has_api_key boolean (never the key value)."""
    return {"has_api_key": _read_api_key() is not None}


def _build():

    return build(
        sentence="NVIDIA NIM-backed LLM agent (OpenAI-compatible; raw prompt-and-parse tool-calling).",
        make_provider=make_provider,
        name=NAME,
        module_name=__name__,
        reflect_extra=_reflect_extra,
        stream_wrapper=_stream_with_rate_limit_retry,
        require_provider=True,
        provider_missing_error=_no_key_error(),
        error_mapper=_map_error,
    )


VERBS, handler = _build()
