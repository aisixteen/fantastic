"""OllamaProvider — native streaming chat with tool-call support."""

from __future__ import annotations

import secrets
from typing import AsyncIterator, Union

DEFAULT_ENDPOINT = "http://localhost:11434"
DEFAULT_MODEL = "gemma4:e2b"


class OllamaProvider:
    def __init__(self, endpoint: str = DEFAULT_ENDPOINT, model: str = DEFAULT_MODEL):
        self._endpoint = endpoint
        self._model = model
        self._client = None

    def _get_client(self):
        if self._client is None:
            import ollama

            self._client = ollama.AsyncClient(host=self._endpoint)
        return self._client

    async def chat(
        self, messages: list[dict], tools: list[dict]
    ) -> AsyncIterator[Union[str, dict]]:
        """Stream from ollama with tool-call support.

        Yields str for content tokens, dict for tool_calls:
            {"tool_call": {"id":..., "name":..., "arguments":{...}}}
        """
        client = self._get_client()
        stream = await client.chat(
            model=self._model,
            messages=messages,
            tools=tools,
            stream=True,
        )
        # ollama sometimes returns these fields as `null` (not absent), so
        # `.get(key, default)` is not enough — use `or default` to normalize
        # null -> default. This is protocol normalization, not bug masking.
        async for chunk in stream:
            msg = chunk.get("message") or {}
            content = msg.get("content") or ""
            if content:
                yield content
            for call in msg.get("tool_calls") or []:
                fn = call.get("function") or {}
                yield {
                    "tool_call": {
                        "id": call.get("id") or f"call_{secrets.token_hex(4)}",
                        "name": fn.get("name") or "",
                        "arguments": fn.get("arguments") or {},
                    }
                }

    @property
    def model(self) -> str:
        return self._model

    def stop(self) -> None:
        self._client = None
