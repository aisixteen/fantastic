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

DEFAULT_ENDPOINT = "https://integrate.api.nvidia.com/v1"
DEFAULT_MODEL = "nvidia/llama-3_1-nemotron-ultra-253b-v1"


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
        client = self._get_client()
        body: dict = {
            "model": self._model,
            "messages": messages,
            "stream": True,
        }
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
                content = (choices[0].get("delta") or {}).get("content")
                if isinstance(content, str) and content:
                    yield content

    @property
    def model(self) -> str:
        return self._model

    async def aclose(self) -> None:
        if self._client is not None:
            await self._client.aclose()
            self._client = None

    def stop(self) -> None:
        self._client = None
