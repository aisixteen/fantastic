"""LlamaCppProvider — pure raw-text streaming chat against a local `llama-server`.

Tool-calling is owned by ai_core (it parses `<tool_call>` text out of this
stream). This provider just streams the model's text tokens.

`llama-server` speaks the OpenAI-compatible surface at `<endpoint>/chat/completions`
and NOTHING else — the ollama routes (`/api/chat`, `/api/tags`, `/api/generate`,
`/api/show`) all 404, which is why the ollama backend cannot be pointed at it.
"""

from __future__ import annotations

import json
from collections.abc import AsyncIterator

import httpx
from ai_core.core import THINK_CLOSE, THINK_OPEN

# llama-server's own documented bind + the OpenAI-compatible path it mounts.
# Nothing is downloaded or started from here: the operator runs `llama-server`
# consciously and this only names where to reach it.
DEFAULT_ENDPOINT = "http://127.0.0.1:8080/v1"
# A SENTINEL, never sent on the wire. `llama-server` serves exactly ONE model —
# whichever `-hf`/`-m` loaded it — and ignores the request's `model` field. So
# there is no model to guess and none to get wrong; this string exists only so
# `reflect` can say what is true. `model` on the record is passed through when
# set (useful when a proxy in front of llama-server does route on it).
DEFAULT_MODEL = "(server-loaded)"


class LlamaCppProvider:
    def __init__(
        self,
        endpoint: str = DEFAULT_ENDPOINT,
        model: str | None = None,
        api_key: str | None = None,
        temperature: float | None = None,
        num_predict: int | None = None,
        think: bool | None = None,
        transport: httpx.AsyncBaseTransport | None = None,
    ):
        self._endpoint = endpoint.rstrip("/")
        # None ⇒ the `model` key is omitted from the body entirely. See DEFAULT_MODEL.
        self._model = model
        # llama-server is KEYLESS unless started with `--api-key`. Absent ⇒ no
        # Authorization header at all; we do not invent one.
        self._api_key = api_key
        # Sampling temperature. An agentic backend must emit EXACT tool-call JSON,
        # not improvise, so we default LOW — same reasoning and same value as the
        # ollama backend; `temperature` on the record overrides.
        self._temperature = 0.3 if temperature is None else float(temperature)
        # Generation cap (`max_tokens`). Unbounded, a weak model can ramble for
        # thousands of tokens instead of emitting one tool-call and blow the send
        # timeout. None = the server's own default.
        self._num_predict = num_predict
        # Reasoning toggle. gemma-4 gates its thought channel behind the template
        # kwarg `enable_thinking`, DEFAULT FALSE (chat_template.jinja:186), and emits
        # `<|channel>thought … <channel|>` rather than `<think>` tags. A server that
        # sets neither produces the measured ollama failure: thinking text present,
        # `content` EMPTY. Sent PER REQUEST here, so toggling thinking is a record
        # edit and not a server restart. None = don't send it (models without a
        # thinking gate reject an unknown kwarg).
        self._think = think
        # NOT a knob here: the context window is fixed at server start (`-c N`).
        # There is no per-request equivalent, so this backend does not pretend to
        # own one — set it on the `llama-server` command line.
        self._transport = transport
        self._client = None

    def _get_client(self):
        if self._client is None:
            headers = {}
            if self._api_key:
                headers["Authorization"] = f"Bearer {self._api_key}"
            self._client = httpx.AsyncClient(
                base_url=self._endpoint,
                headers=headers,
                timeout=httpx.Timeout(connect=10, read=300, write=30, pool=10),
                transport=self._transport,
            )
        return self._client

    async def chat(self, messages: list[dict]) -> AsyncIterator[str]:
        """Stream raw text tokens — NO tools array. ai_core parses any
        `<tool_call>` envelope out of the text stream."""
        client = self._get_client()
        body: dict = {"messages": messages, "stream": True}
        if self._model:
            body["model"] = self._model
        if self._temperature is not None:
            body["temperature"] = self._temperature
        if self._num_predict:
            body["max_tokens"] = int(self._num_predict)
        if self._think is not None:
            body["chat_template_kwargs"] = {"enable_thinking": bool(self._think)}

        thinking = False
        async with client.stream("POST", "/chat/completions", json=body) as resp:
            resp.raise_for_status()
            async for line in resp.aiter_lines():
                if not line or not line.startswith("data:"):
                    continue
                data = line[5:].strip()
                if not data or data == "[DONE]":
                    if data == "[DONE]":
                        break
                    continue
                try:
                    obj = json.loads(data)
                except json.JSONDecodeError:
                    continue
                choices = obj.get("choices") or []
                if not choices:
                    continue
                delta = choices[0].get("delta") or {}
                # A REASONING MODEL SPLITS ITS OUTPUT. Started with
                # `--reasoning-format deepseek`, llama-server streams the thought
                # channel as `reasoning_content` and only the conclusion as
                # `content` — measured 602 chars of reasoning to 176 of content on
                # gemma-4-12B-it. Reading `content` alone silently drops whole turns.
                # A raw text protocol reads the text the model produced; it does not
                # let the provider decide which field counts as output. WRAPPED once
                # (see `ai_core.core.without_thinking`): parsed for tool calls,
                # stripped before the turn is persisted.
                think = delta.get("reasoning_content")
                if isinstance(think, str) and think:
                    if not thinking:
                        thinking = True
                        yield THINK_OPEN
                    yield think
                content = delta.get("content")
                if isinstance(content, str) and content:
                    if thinking:
                        thinking = False
                        yield THINK_CLOSE
                    yield content

        if thinking:
            yield THINK_CLOSE

    @property
    def model(self) -> str:
        return self._model or DEFAULT_MODEL

    async def aclose(self) -> None:
        if self._client is not None:
            await self._client.aclose()
            self._client = None

    def stop(self) -> None:
        self._client = None
