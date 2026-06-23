"""OllamaProvider — pure raw-text streaming chat (NO native tool API).

Tool-calling is owned by ai_core (it parses `<tool_call>` text out of this
stream). This provider just streams the model's text tokens.
"""

from __future__ import annotations

from typing import AsyncIterator

DEFAULT_ENDPOINT = "http://localhost:11434"
DEFAULT_MODEL = "gemma4:e2b"


class OllamaProvider:
    def __init__(
        self,
        endpoint: str = DEFAULT_ENDPOINT,
        model: str = DEFAULT_MODEL,
        num_ctx: int | None = None,
        temperature: float | None = None,
    ):
        self._endpoint = endpoint
        self._model = model
        # Context window. ollama defaults to 4096 when unset, which is too small
        # for the agentic menu + tool results + history of a multi-step run; the
        # record's `num_ctx` lifts it (e.g. 32768; gemma4 supports up to 262144).
        self._num_ctx = num_ctx
        # Sampling temperature. ollama defaults to 0.8 (creative) — wrong for an
        # AGENTIC tool-calling backend, where the model must emit EXACT tool-call
        # JSON + (for view-authoring) precise connector code, not improvise. We
        # default LOW (0.3 — precise, with a touch of variety); `temperature`
        # on the record overrides.
        self._temperature = 0.3 if temperature is None else float(temperature)
        self._client = None

    def _get_client(self):
        if self._client is None:
            import ollama

            self._client = ollama.AsyncClient(host=self._endpoint)
        return self._client

    async def chat(self, messages: list[dict]) -> AsyncIterator[str]:
        """Stream raw text tokens from ollama — NO tools array. ai_core parses
        any `<tool_call>` envelope out of the text stream."""
        client = self._get_client()
        options: dict = {"temperature": self._temperature}
        if self._num_ctx:
            options["num_ctx"] = self._num_ctx
        stream = await client.chat(
            model=self._model,
            messages=messages,
            stream=True,
            options=options,
        )
        # ollama sometimes returns `content` as `null` (not absent), so normalize
        # null -> "" with `or ""`. This is protocol normalization, not bug masking.
        async for chunk in stream:
            content = (chunk.get("message") or {}).get("content") or ""
            if content:
                yield content

    @property
    def model(self) -> str:
        return self._model

    def stop(self) -> None:
        self._client = None
