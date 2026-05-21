"""NvidiaNimProvider — OpenAI-compatible streaming chat with tool-call support.

NVIDIA NIM exposes 100+ models on a free OpenAI-compatible API at
`https://integrate.api.nvidia.com/v1`. Free tier is rate-limited
(~40 RPM/model). Auth via `Authorization: Bearer <nvapi-...>`.

This provider mirrors `OllamaProvider`'s yield interface exactly so
`tools._run` consumes either backend identically:
    - yields `str` for assistant content tokens
    - yields `{"tool_call": {id, name, arguments}}` for completed tool_calls

OpenAI streams `function.arguments` as string fragments across many
chunks per tool_call index. We aggregate per index and emit one
tool_call dict per completed call after the stream ends.
"""

from __future__ import annotations

import json
import secrets
from typing import AsyncIterator, Union

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

    async def chat(
        self, messages: list[dict], tools: list[dict] | None = None
    ) -> AsyncIterator[Union[str, dict]]:
        client = self._get_client()
        body: dict = {
            "model": self._model,
            "messages": messages,
            "stream": True,
        }
        if tools:
            body["tools"] = tools
            body["tool_choice"] = "auto"

        # OpenAI streams tool_call.function.arguments as string fragments
        # split across many chunks under the same `index`. Aggregate, then
        # emit one tool_call dict per completed index after the stream ends.
        pending: dict[int, dict] = {}

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
                content = delta.get("content")
                if isinstance(content, str) and content:
                    yield content
                for tc in delta.get("tool_calls") or []:
                    idx = tc.get("index", 0)
                    slot = pending.setdefault(
                        idx, {"id": "", "name": "", "arguments": ""}
                    )
                    if tc.get("id"):
                        slot["id"] = tc["id"]
                    fn = tc.get("function") or {}
                    if fn.get("name"):
                        slot["name"] = fn["name"]
                    args_frag = fn.get("arguments")
                    if isinstance(args_frag, str):
                        slot["arguments"] += args_frag

        for slot in pending.values():
            if not slot["name"]:
                continue
            try:
                args = json.loads(slot["arguments"]) if slot["arguments"] else {}
            except json.JSONDecodeError:
                args = {}
            yield {
                "tool_call": {
                    "id": slot["id"] or f"call_{secrets.token_hex(4)}",
                    "name": slot["name"],
                    "arguments": args,
                }
            }

    @property
    def model(self) -> str:
        return self._model

    async def aclose(self) -> None:
        if self._client is not None:
            await self._client.aclose()
            self._client = None

    def stop(self) -> None:
        self._client = None
