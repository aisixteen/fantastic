"""AnthropicProvider — streaming chat over the Anthropic Messages API.

Matches the OllamaProvider contract so the shared agent loop in tools.py is
reused verbatim: `chat(messages, tools)` takes OpenAI-style messages (role
system/user/assistant + `tool_calls`, plus role:tool results) and the OpenAI
`SEND_TOOL`, and yields `str` content tokens or
`{"tool_call": {"id", "name", "arguments": {...}}}`.

Internally it translates to the Anthropic wire shape: a top-level `system`
string, `messages` with `tool_use` / `tool_result` content blocks, and `tools`
with `input_schema`. The key is read from `ANTHROPIC_KEY` (or `ANTHROPIC_API_KEY`)
in the environment at call time (`.env` is loaded into os.environ at boot).
"""

from __future__ import annotations

import json
import os
from typing import AsyncIterator, Union

DEFAULT_ENDPOINT = "https://api.anthropic.com/v1/messages"
DEFAULT_MODEL = "claude-opus-4-8"  # latest, smartest
API_VERSION = "2023-06-01"
MAX_TOKENS = 8192


def _to_anthropic(messages: list[dict]) -> tuple[str | None, list[dict]]:
    """OpenAI-style messages → (system string, Anthropic messages).

    - role:system  → the top-level `system` string.
    - role:user    → {role:user, content:<str>}.
    - role:assistant (content + optional tool_calls) → {role:assistant,
      content:[{text} , {tool_use id,name,input} …]}.
    - role:tool    → a {type:tool_result, tool_use_id, content} block, merged
      with any adjacent tool results into ONE user turn (Anthropic groups them).
    """
    system: str | None = None
    out: list[dict] = []
    pending: list[dict] = []

    def flush() -> None:
        nonlocal pending
        if pending:
            out.append({"role": "user", "content": pending})
            pending = []

    for m in messages:
        role = m.get("role")
        if role == "system":
            system = m.get("content", "") or ""
            continue
        if role == "tool":
            pending.append(
                {
                    "type": "tool_result",
                    "tool_use_id": m.get("tool_call_id"),
                    "content": m.get("content", "") or "",
                }
            )
            continue
        flush()  # any pending tool_results close out a user turn first
        if role == "user":
            out.append({"role": "user", "content": m.get("content", "") or ""})
        elif role == "assistant":
            blocks: list[dict] = []
            text = m.get("content") or ""
            if text:
                blocks.append({"type": "text", "text": text})
            for tc in m.get("tool_calls") or []:
                fn = tc.get("function") or {}
                args = fn.get("arguments")
                if isinstance(args, str):
                    try:
                        args = json.loads(args)
                    except json.JSONDecodeError:
                        args = {}
                blocks.append(
                    {
                        "type": "tool_use",
                        "id": tc.get("id"),
                        "name": fn.get("name"),
                        "input": args or {},
                    }
                )
            if not blocks:
                blocks = [{"type": "text", "text": "(no content)"}]
            out.append({"role": "assistant", "content": blocks})
    flush()
    return system, out


def _to_anthropic_tools(tools: list[dict]) -> list[dict]:
    """OpenAI function tools → Anthropic tools (name/description/input_schema)."""
    anth: list[dict] = []
    for t in tools or []:
        fn = t.get("function") or {}
        anth.append(
            {
                "name": fn.get("name"),
                "description": fn.get("description", ""),
                "input_schema": fn.get("parameters")
                or {"type": "object", "properties": {}},
            }
        )
    return anth


class AnthropicProvider:
    def __init__(self, endpoint: str = DEFAULT_ENDPOINT, model: str = DEFAULT_MODEL):
        self._endpoint = endpoint
        self._model = model

    async def chat(
        self, messages: list[dict], tools: list[dict]
    ) -> AsyncIterator[Union[str, dict]]:
        """Stream from Anthropic. Yields str content tokens, or
        {"tool_call": {"id", "name", "arguments": {...}}} for each tool_use."""
        import httpx

        key = os.environ.get("ANTHROPIC_KEY") or os.environ.get("ANTHROPIC_API_KEY")
        if not key:
            raise RuntimeError("ANTHROPIC_KEY not set in environment (.env)")

        system, anth_messages = _to_anthropic(messages)
        body: dict = {
            "model": self._model,
            "max_tokens": MAX_TOKENS,
            "messages": anth_messages,
            "tools": _to_anthropic_tools(tools),
            "stream": True,
        }
        if system:
            body["system"] = system
        headers = {
            "x-api-key": key,
            "anthropic-version": API_VERSION,
            "content-type": "application/json",
        }

        # Per-index content-block state: text streams directly; tool_use
        # accumulates a partial-JSON buffer until content_block_stop.
        blocks: dict[int, dict] = {}
        timeout = httpx.Timeout(120.0, connect=15.0)
        async with httpx.AsyncClient(timeout=timeout) as client:
            async with client.stream(
                "POST", self._endpoint, json=body, headers=headers
            ) as resp:
                if resp.status_code >= 400:
                    detail = (await resp.aread()).decode("utf-8", "replace")
                    raise RuntimeError(f"anthropic {resp.status_code}: {detail[:300]}")
                async for line in resp.aiter_lines():
                    if not line.startswith("data:"):
                        continue
                    data = line[len("data:") :].strip()
                    if not data:
                        continue
                    try:
                        evt = json.loads(data)
                    except json.JSONDecodeError:
                        continue
                    etype = evt.get("type")
                    if etype == "content_block_start":
                        idx = evt.get("index", 0)
                        cb = evt.get("content_block") or {}
                        if cb.get("type") == "tool_use":
                            blocks[idx] = {
                                "kind": "tool_use",
                                "id": cb.get("id"),
                                "name": cb.get("name"),
                                "json": "",
                            }
                        else:
                            blocks[idx] = {"kind": cb.get("type", "text")}
                    elif etype == "content_block_delta":
                        idx = evt.get("index", 0)
                        delta = evt.get("delta") or {}
                        dt = delta.get("type")
                        if dt == "text_delta":
                            txt = delta.get("text") or ""
                            if txt:
                                yield txt
                        elif dt == "input_json_delta":
                            blk = blocks.get(idx)
                            if blk is not None:
                                blk["json"] += delta.get("partial_json") or ""
                    elif etype == "content_block_stop":
                        idx = evt.get("index", 0)
                        blk = blocks.pop(idx, None)
                        if blk and blk.get("kind") == "tool_use":
                            raw = blk.get("json") or "{}"
                            try:
                                args = json.loads(raw) if raw.strip() else {}
                            except json.JSONDecodeError:
                                args = {}
                            yield {
                                "tool_call": {
                                    "id": blk.get("id"),
                                    "name": blk.get("name"),
                                    "arguments": args,
                                }
                            }
                    elif etype == "error":
                        err = evt.get("error") or {}
                        raise RuntimeError(f"anthropic stream error: {err}")
                    # message_start / message_delta / message_stop / ping: ignored

    @property
    def model(self) -> str:
        return self._model

    def stop(self) -> None:
        pass
