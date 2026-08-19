"""OllamaProvider — pure raw-text streaming chat (NO native tool API).

Tool-calling is owned by ai_core (it parses `<tool_call>` text out of this
stream). This provider just streams the model's text tokens.
"""

from __future__ import annotations

from typing import AsyncIterator

from ai_core.core import THINK_CLOSE, THINK_OPEN

DEFAULT_ENDPOINT = "http://localhost:11434"
DEFAULT_MODEL = "gemma4:e2b"


class OllamaProvider:
    def __init__(
        self,
        endpoint: str = DEFAULT_ENDPOINT,
        model: str = DEFAULT_MODEL,
        num_ctx: int | None = None,
        temperature: float | None = None,
        num_predict: int | None = None,
        think: bool | None = None,
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
        # Generation cap. Unbounded, a weak model can ramble for thousands of
        # tokens instead of emitting one tool-call, blowing the send timeout;
        # the record's `num_predict` bounds it. None = ollama's default.
        self._num_predict = num_predict
        # Reasoning toggle for THINKING models. Such a model spends hundreds of
        # hidden reasoning tokens before its answer (gemma4:12b: ~250 tok and ~11x
        # latency for the SAME one-line tool-call), and a `num_predict` cap is spent
        # on those tokens first — truncating mid-thought into an EMPTY completion.
        # An agentic backend wants the ACTION, not the rumination: set `think=False`.
        # None = don't send it (non-thinking models reject the field).
        self._think = think
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
        if self._num_predict:
            options["num_predict"] = self._num_predict
        kwargs: dict = {}
        if self._think is not None:
            kwargs["think"] = self._think
        stream = await client.chat(
            model=self._model,
            messages=messages,
            stream=True,
            options=options,
            **kwargs,
        )
        # THINKING MODELS SPLIT THEIR OUTPUT — read every text field, not just one.
        # ollama streams a reasoning model's tokens as `message.thinking` and only
        # its conclusion as `message.content`. Measured on gemma4:12b: 336 chars of
        # thinking to 38 of content. Reading `content` alone silently drops whole
        # turns — observed live on 2026-08-02, where gemma spent all 578 generated
        # tokens reasoning, we saw an EMPTY completion, and the chain ended at pass
        # one with no tool call and no error. ollama's own log showed the request
        # answered normally, so it looked like the server had wedged.
        #
        # Same defect and same fix as nvidia_nim's `reasoning_content`. A raw text
        # protocol reads the text the model produced; it does not let the provider
        # decide which of its fields counts as output. (`or ""` also normalizes
        # ollama's occasional explicit `null` — protocol shape, not bug masking.)
        # The reasoning is WRAPPED, once, so ai_core can parse it but not persist
        # it (see `ai_core.core.without_thinking`). Opened on the first thinking
        # chunk and closed when content starts or the stream ends — not per chunk,
        # or the text would be a confetti of tags.
        thinking = False
        async for chunk in stream:
            message = chunk.get("message") or {}
            think = message.get("thinking") or ""
            if think:
                if not thinking:
                    thinking = True
                    yield THINK_OPEN
                yield think
            content = message.get("content") or ""
            if content:
                if thinking:
                    thinking = False
                    yield THINK_CLOSE
                yield content
        if thinking:
            yield THINK_CLOSE

    @property
    def model(self) -> str:
        return self._model

    def stop(self) -> None:
        self._client = None
