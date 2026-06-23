"""Provider — the streaming-chat interface every LLM backend implements.

The real providers (OllamaProvider, AnthropicProvider, NvidiaNimProvider)
live in their own bundles; this ABC documents the contract the shared
`core._run` loop relies on. A provider is a PURE RAW-TEXT streamer: it takes
plain chat messages (role system/user/assistant, text content) and yields
content tokens (str). It does NOT do tool-calling — Fantastic NEVER uses a
provider's native tool API. Tool-calling is owned by the base class
(`ai_core.tool_parse` parses `<tool_call>` text out of this stream). The
provider holds no agent state — queue / lock / history live in `ai_core.core`.
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from typing import AsyncIterator


class Provider(ABC):
    """Streaming chat adapter for one upstream LLM.

    Implementations duck-type this (they need not subclass it). The
    `core._run` loop consumes `chat()` identically across backends.
    """

    @abstractmethod
    def chat(self, messages: list[dict]) -> AsyncIterator[str]:
        """Stream a plain-text completion for `messages` (role
        system/user/assistant, text content). An async generator yielding
        `str` content tokens in order. NO tools — the base class teaches the
        `send` tool in the prompt and parses the call from this text stream.
        """
        raise NotImplementedError

    @property
    @abstractmethod
    def model(self) -> str:
        """The upstream model id this provider talks to."""
        raise NotImplementedError

    @abstractmethod
    def stop(self) -> None:
        """Release any held client/connection. Idempotent."""
        raise NotImplementedError
