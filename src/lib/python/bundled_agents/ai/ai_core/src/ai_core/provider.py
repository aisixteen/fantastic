"""Provider — the streaming-chat interface every LLM backend implements.

The real providers (OllamaProvider, ResponsesProvider, VllmProvider)
live in their own bundles; this ABC documents the contract the shared
`core._run` loop relies on. A provider is a PURE RAW-TEXT streamer: it takes
plain chat messages (role system/user/assistant, text content) and yields
content tokens (str). It does NOT do tool-calling — Fantastic NEVER uses a
provider's native tool API. Tool-calling is owned by the base class
(`ai_core.tool_parse` parses `<call>` text out of this stream). The
provider holds no agent state — queue / lock / history live in `ai_core.core`.
"""

from __future__ import annotations

import math
from abc import ABC, abstractmethod
from collections.abc import AsyncIterator

from ai_core.constants import STREAM_IDLE_SECONDS


class ProviderError(RuntimeError):
    """A provider failure. `transport` is True when the LINK failed (connect,
    read, dropped stream, a 502/503/504 gateway), so a retry may be safe;
    False for everything else."""

    def __init__(self, message: str, *, transport: bool = False):
        super().__init__(message)
        self.transport = transport


def stream_idle_of(meta: dict, default: float = STREAM_IDLE_SECONDS) -> float:
    """The read clock a provider's stream runs on: the record's `stream_idle_seconds`
    (a positive number of seconds), else `default`. Anything else is refused by name
    — a window the backend cannot run on is never guessed at."""
    v = meta.get("stream_idle_seconds")
    if v is None:
        return default
    if (
        isinstance(v, bool)
        or not isinstance(v, (int, float))
        or not math.isfinite(v)
        or v <= 0
    ):
        raise ProviderError(
            f"stream_idle_seconds must be a positive number of seconds, not {v!r}"
        )
    return float(v)


def stalled_error(name: str, endpoint: str, secs: float) -> ProviderError:
    """The turn's failure for a stalled stream: `stream_stalled`, the seconds and the
    endpoint. A raise, so the turn ends with an error row."""
    return ProviderError(
        f"{name}: stream_stalled — {endpoint} sent no bytes for {secs:g}s; "
        "this turn is NOT complete",
        transport=True,
    )


class Provider(ABC):
    """Streaming chat adapter for one upstream LLM.

    Implementations duck-type this (they need not subclass it). The
    `core._run` loop consumes `chat()` identically across backends.
    """

    #: The endpoint's own count of the last prompt (calibrates the estimate), or None.
    last_prompt_tokens: int | None = None
    #: Why the last stream ended — `"max_tokens"` when it stopped AT the output bound.
    last_stop_reason: str | None = None

    @abstractmethod
    def chat(
        self, messages: list[dict], max_tokens: int | None = None
    ) -> AsyncIterator[str]:
        """Stream a plain-text completion for `messages` (role
        system/user/assistant, text content). An async generator yielding
        `str` content tokens in order. NO tools — the base class teaches the
        `send` tool in the prompt and parses the call from this text stream.
        `max_tokens` bounds THIS call's output (a side call's budget); None keeps
        the provider's own bound. A stream that stopped at the bound sets
        `last_stop_reason = "max_tokens"`.
        """
        raise NotImplementedError

    @property
    @abstractmethod
    def model(self) -> str:
        """The upstream model id this provider talks to."""
        raise NotImplementedError
