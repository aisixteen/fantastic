"""OllamaProvider — pure raw-text streaming chat (NO native tool API).

Tool-calling is owned by ai_core (it parses `<call>` text out of this
stream). This provider just streams the model's text tokens.
"""

from __future__ import annotations

from contextlib import aclosing
from typing import AsyncIterator

import httpx
from ai_core.core import THINK_CLOSE, THINK_OPEN
from ai_core.provider import ProviderError


#: Sampling temperature when the record names none — LOW, because an agentic
#: backend must emit exact calls, not improvise.
DEFAULT_TEMPERATURE = 0.3


class OllamaProvider:
    #: The prompt size the endpoint REPORTED for the last pass (None: not reported).
    #: ai_core calibrates its own token estimate with it.
    last_prompt_tokens: int | None = None
    #: The output tokens the server reported for the last pass (None = not reported).
    last_completion_tokens: int | None = None
    last_stop_reason: str | None = None

    def __init__(
        self,
        endpoint: str,
        model: str,
        num_ctx: int | None = None,
        temperature: float | None = None,
        num_predict: int | None = None,
        think: bool | None = None,
        key=None,
    ):
        self._endpoint = endpoint
        self._model = model
        # `async key(endpoint) -> str | None`, awaited before each request — the
        # record's `credentials` ref (a server behind a door); None = no header.
        self._key = key
        # Context window. ollama defaults to 4096 when unset, which is too small
        # for the agentic menu + tool results + history of a multi-step run; the
        # record's `num_ctx` lifts it (e.g. 32768; gemma4 supports up to 262144).
        self._num_ctx = num_ctx
        # Sampling temperature. ollama defaults to 0.8 (creative) — wrong for an
        # AGENTIC tool-calling backend, where the model must emit EXACT tool-call
        # JSON + (for view-authoring) precise connector code, not improvise. We
        # default LOW (0.3 — precise, with a touch of variety); `temperature`
        # on the record overrides.
        self._temperature = (
            DEFAULT_TEMPERATURE if temperature is None else float(temperature)
        )
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

    async def chat(
        self, messages: list[dict], max_tokens: int | None = None
    ) -> AsyncIterator[str]:
        """Stream raw text tokens from ollama — NO tools array. ai_core parses
        any `<call>` element out of the text stream.

        THE LINK FAILED, not the model (#198): a connection that never opened or a
        stream that broke mid-answer is a `ProviderError` marked transport, so a
        retry may take it. An HTTP status the server answered
        (`ollama.ResponseError`) is raised as it was."""
        try:
            async with aclosing(self._chat(messages, max_tokens)) as stream:
                async for token in stream:
                    yield token
        except (httpx.ConnectError, httpx.ConnectTimeout) as e:
            raise ProviderError(f"ollama: request failed: {e}", transport=True) from e
        except (httpx.ReadError, httpx.ReadTimeout, httpx.RemoteProtocolError) as e:
            raise ProviderError(f"ollama: stream error: {e}", transport=True) from e

    async def _chat(
        self, messages: list[dict], max_tokens: int | None
    ) -> AsyncIterator[str]:
        """The stream itself (see `chat`)."""
        if not self._model:
            raise RuntimeError(
                "ollama_backend: no `model` set — this connector does not guess one. "
                "Name a model on the agent record (model=...)."
            )
        import ollama

        options: dict = {"temperature": self._temperature}
        if self._num_ctx:
            options["num_ctx"] = self._num_ctx
        if max_tokens is not None:  # a side call's own bound (a fold summary's budget)
            options["num_predict"] = int(max_tokens)
        elif self._num_predict:
            options["num_predict"] = self._num_predict
        kwargs: dict = {}
        if self._think is not None:
            kwargs["think"] = self._think
        # THINKING MODELS SPLIT THEIR OUTPUT — read every text field, not just one.
        # ollama streams a reasoning model's tokens as `message.thinking` and only
        # its conclusion as `message.content`, often far shorter. Reading `content`
        # alone silently drops whole turns — a turn spent entirely reasoning reads as
        # an EMPTY completion, and the chain ends with no tool call and no error while
        # ollama's own log shows the request answered normally.
        #
        # Same defect and same fix as vllm's `reasoning_content`. A raw text
        # protocol reads the text the model produced; it does not let the provider
        # decide which of its fields counts as output. (`or ""` also normalizes
        # ollama's occasional explicit `null` — protocol shape, not bug masking.)
        # The reasoning is WRAPPED, once, so ai_core can parse it but not persist
        # it (see `ai_core.core.without_thinking`). Opened on the first thinking
        # chunk and closed when content starts or the stream ends — not per chunk,
        # or the text would be a confetti of tags.
        # ONE client per call: the provider is built per turn, and a client kept
        # on it would hold its connection past the turn.
        secret = await self._key(self._endpoint) if self._key else None
        auth = {"headers": {"Authorization": f"Bearer {secret}"}} if secret else {}
        async with ollama.AsyncClient(host=self._endpoint, **auth) as client:
            stream = await client.chat(
                model=self._model,
                messages=messages,
                stream=True,
                options=options,
                **kwargs,
            )
            thinking = False
            async for chunk in stream:
                if chunk.get("prompt_eval_count"):
                    # PROVIDER-REPORTED USAGE — ai_core calibrates its estimate with it.
                    self.last_prompt_tokens = int(chunk["prompt_eval_count"])
                if chunk.get("eval_count"):
                    self.last_completion_tokens = int(chunk["eval_count"])
                if chunk.get("done_reason"):
                    dr = chunk["done_reason"]
                    self.last_stop_reason = "max_tokens" if dr == "length" else dr
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
