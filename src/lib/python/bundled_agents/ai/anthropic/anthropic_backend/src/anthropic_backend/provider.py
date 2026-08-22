"""AnthropicProvider — streaming chat over the Anthropic Messages API, RAW TEXT.

Matches the OllamaProvider contract: `chat(messages)` takes plain chat messages
(role system/user/assistant, text content) and yields `str` content tokens ONLY.
Fantastic NEVER uses Anthropic's native tool_use — tool-calling is owned by
ai_core, which teaches the `<tool_call>` envelope in the prompt and parses it out
of this text stream. (ai_core's `_render_for_model` already maps tool replies to
plain user-role text before we see them.)

Internally translates to the Anthropic wire shape: a top-level `system` string +
alternating user/assistant text turns. The key is read from `ANTHROPIC_KEY` (or
`ANTHROPIC_API_KEY`) at call time (`.env` is loaded into os.environ at boot).
"""

from __future__ import annotations

import os
from typing import AsyncIterator

from ai_core.core import THINK_CLOSE, THINK_OPEN

DEFAULT_ENDPOINT = "https://api.anthropic.com/v1/messages"
DEFAULT_MODEL = "claude-opus-4-8"  # latest, smartest
API_VERSION = "2023-06-01"
MAX_TOKENS = 8192


def _to_anthropic(messages: list[dict]) -> tuple[str | None, list[dict]]:
    """Plain messages → (system string, Anthropic messages).

    - role:system → the top-level `system` string.
    - role:user / role:assistant → {role, content:<str>}.
    Tool calls/replies are already inline text (`<tool_call>`/`<tool_response>`),
    so there is no tool_use/tool_result translation — pure text turns.
    """
    system: str | None = None
    out: list[dict] = []
    for m in messages:
        role = m.get("role")
        content = m.get("content", "") or ""
        if role == "system":
            system = content
        elif role in ("user", "assistant"):
            out.append({"role": role, "content": content or "(no content)"})
    return system, out


class AnthropicProvider:
    def __init__(self, endpoint: str = DEFAULT_ENDPOINT, model: str = DEFAULT_MODEL):
        self._endpoint = endpoint
        self._model = model

    async def chat(self, messages: list[dict]) -> AsyncIterator[str]:
        """Stream raw text tokens from Anthropic — NO tools. ai_core parses any
        `<tool_call>` envelope out of the text stream.

        BOTH channels are read: a reasoning model's `thinking_delta` blocks are
        wrapped in `<think>…</think>` and streamed alongside the `text_delta`
        conclusion, exactly as the ollama/nvidia/llamacpp providers do. ai_core
        parses the RAW text (so a mid-thought tool call still dispatches) and
        persists it stripped."""
        import json

        import httpx

        key = os.environ.get("ANTHROPIC_KEY") or os.environ.get("ANTHROPIC_API_KEY")
        if not key:
            raise RuntimeError("ANTHROPIC_KEY not set in environment (.env)")

        system, anth_messages = _to_anthropic(messages)
        body: dict = {
            "model": self._model,
            "max_tokens": MAX_TOKENS,
            "messages": anth_messages,
            "stream": True,
        }
        if system:
            body["system"] = system
        headers = {
            "x-api-key": key,
            "anthropic-version": API_VERSION,
            "content-type": "application/json",
        }

        timeout = httpx.Timeout(120.0, connect=15.0)
        async with httpx.AsyncClient(timeout=timeout) as client:
            async with client.stream(
                "POST", self._endpoint, json=body, headers=headers
            ) as resp:
                if resp.status_code >= 400:
                    detail = (await resp.aread()).decode("utf-8", "replace")
                    raise RuntimeError(f"anthropic {resp.status_code}: {detail[:300]}")
                stop_reason = None
                thinking = False
                # What the stream actually CONTAINED. A bare `stop_reason` names the
                # symptom and nothing else: measured 2026-08-21 (`live_plan`), a
                # no-tools request came back `stop_reason='tool_use'`, which should be
                # impossible, and the error could not say what the model had emitted.
                # The block types are the one fact that distinguishes "the model wrote
                # a native tool block" from "the connection died before any content".
                blocks: list[str] = []
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
                    if etype == "content_block_delta":
                        delta = evt.get("delta") or {}
                        dtype = delta.get("type")
                        # REASONING MODELS SPLIT THEIR OUTPUT — the same trap the
                        # nvidia/ollama/llamacpp providers already guard. Anthropic's
                        # wire puts the thought in `thinking_delta` blocks and only the
                        # conclusion in `text_delta`; a model served through an
                        # anthropic-compatible proxy (qwen3.8-27b via `/raw`) does this
                        # constantly. Reading `text_delta` alone silently drops whole
                        # turns: a pass spent entirely reasoning arrives as an EMPTY
                        # completion and the chain dies with no error anywhere — and a
                        # `<tool_call>` the model wrote MID-THOUGHT is lost with it.
                        # A raw text protocol reads the text the model produced; it does
                        # not let the provider decide which field counts as output.
                        # WRAPPED once (see `ai_core.core.without_thinking`): parsed for
                        # tool calls, stripped before the turn is persisted.
                        if dtype == "thinking_delta":
                            think = delta.get("thinking") or ""
                            if think:
                                if not thinking:
                                    thinking = True
                                    yield THINK_OPEN
                                yield think
                        elif dtype == "text_delta":
                            txt = delta.get("text") or ""
                            if txt:
                                if thinking:
                                    thinking = False
                                    yield THINK_CLOSE
                                yield txt
                    elif etype == "content_block_start":
                        blocks.append((evt.get("content_block") or {}).get("type"))
                    elif etype == "message_delta":
                        stop_reason = (evt.get("delta") or {}).get("stop_reason")
                    elif etype == "error":
                        err = evt.get("error") or {}
                        raise RuntimeError(f"anthropic stream error: {err}")
                    # message_start / content_block_start/stop / ping: ignored
                # A turn spent ENTIRELY reasoning ends inside the think block —
                # close it so the marker pair is always balanced for the parser.
                if thinking:
                    thinking = False
                    yield THINK_CLOSE
                # A stream that did not END TURN did not finish — say so, loudly.
                # This loop used to ignore `message_delta`, so a completion cut by
                # `max_tokens`, a refusal, or a dropped connection was returned as
                # if the model had finished. Measured 2026-08-07 (combined probe,
                # run 6): a `create_agent` turn stopped at 1,085 chars mid-`code`,
                # the partial parsed as an unterminated envelope, the next
                # completion arrived EMPTY, and the chain died — three symptoms,
                # one silent cause. A cut turn is an error the caller can see and
                # retry; a fake-complete one is invisible.
                if stop_reason not in ("end_turn", "stop_sequence"):
                    raise RuntimeError(
                        "anthropic stream ended without end_turn: "
                        f"stop_reason={stop_reason!r} blocks={blocks!r}"
                        + (
                            " (none seen — connection cut mid-stream)"
                            if stop_reason is None
                            else ""
                        )
                    )

    @property
    def model(self) -> str:
        return self._model

    def stop(self) -> None:
        pass
