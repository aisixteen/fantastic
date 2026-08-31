"""NvidiaNimProvider — OpenAI-compatible streaming chat, PURE RAW TEXT.

NVIDIA NIM exposes 100+ models on a free OpenAI-compatible API at
`https://integrate.api.nvidia.com/v1`. Free tier is rate-limited
(~40 RPM/model). Auth via `Authorization: Bearer <nvapi-...>`.

Mirrors `OllamaProvider`: streams `str` content tokens ONLY — NO native tools
array. Tool-calling is owned by ai_core, which parses `<tool_call>` text out of
this stream.
"""

from __future__ import annotations

import json
from typing import AsyncIterator

import httpx

from ai_core.core import THINK_CLOSE, THINK_OPEN

DEFAULT_ENDPOINT = "https://integrate.api.nvidia.com/v1"
# NO guessy default (by default, no defaults): never name a network-reaching model.
# The sentinel exists ONLY so `reflect` can say what is true; `chat()` fails loud
# naming `model` if it is left unset.
DEFAULT_MODEL = "(no default — set the `model` meta)"


class NvidiaNimProvider:
    def __init__(
        self,
        api_key: str,
        endpoint: str = DEFAULT_ENDPOINT,
        model: str = DEFAULT_MODEL,
        transport: httpx.AsyncBaseTransport | None = None,
    ):
        self._api_key = api_key
        self._endpoint = endpoint.rstrip("/")
        self._model = model
        self._transport = transport
        self._client: httpx.AsyncClient | None = None

    def _get_client(self) -> httpx.AsyncClient:
        if self._client is None:
            self._client = httpx.AsyncClient(
                base_url=self._endpoint,
                headers={"Authorization": f"Bearer {self._api_key}"},
                timeout=httpx.Timeout(connect=10, read=300, write=30, pool=10),
                transport=self._transport,
            )
        return self._client

    async def chat(self, messages: list[dict]) -> AsyncIterator[str]:
        """Stream raw text tokens — NO tools array. ai_core parses any
        `<tool_call>` envelope out of the text stream."""
        if not self._model or self._model == DEFAULT_MODEL:
            raise RuntimeError(
                "nvidia_nim_backend: no `model` set — this connector does not guess "
                "one. Name a model on the agent record (model=...)."
            )
        client = self._get_client()
        body: dict = {
            "model": self._model,
            "messages": messages,
            "stream": True,
        }
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
                # REASONING MODELS SPLIT THEIR OUTPUT. nemotron/deepseek-style NIM
                # models stream most tokens as `reasoning_content` and only the
                # conclusion as `content` — measured 740 chars of reasoning to 129 of
                # content on one turn. Reading `content` alone silently drops whole
                # turns: observed live, the model spent an entire pass reasoning, we
                # saw an EMPTY completion, and the chain died with no error anywhere.
                # A raw text protocol reads the text the model produced; it does not
                # let the provider decide which field counts as output.
                # WRAPPED once (see ollama_backend + `ai_core.core.without_thinking`):
                # parsed for tool calls, stripped before the turn is persisted.
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
        return self._model

    async def aclose(self) -> None:
        if self._client is not None:
            await self._client.aclose()
            self._client = None

    def stop(self) -> None:
        self._client = None
