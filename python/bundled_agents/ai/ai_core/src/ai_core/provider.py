"""Provider — the streaming-chat interface every LLM backend implements.

The real providers (OllamaProvider, AnthropicProvider, NvidiaNimProvider)
live in their own bundles; this ABC documents the contract the shared
`core._run` loop relies on. A provider is a thin streaming adapter: it
takes OpenAI-style messages + the universal `send` tool and yields either
content tokens (str) or completed tool-calls (dict). It holds no agent
state — the queue / lock / history all live in `ai_core.core`.
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from typing import AsyncIterator, Union


class Provider(ABC):
    """Streaming chat adapter for one upstream LLM.

    Implementations duck-type this (they need not subclass it). The
    `core._run` loop consumes `chat()` identically across backends.
    """

    @abstractmethod
    def chat(
        self, messages: list[dict], tools: list[dict] | None = None
    ) -> AsyncIterator[Union[str, dict]]:
        """Stream a completion for `messages` (OpenAI-style: role
        system/user/assistant with optional `tool_calls`, plus role:tool
        results) given the available `tools` (the universal SEND tool).

        An async generator yielding, in order:
          - `str` — a content token (streamed to the caller live).
          - `{"tool_call": {"id": str, "name": str, "arguments": dict}}`
            — one completed tool-call (arguments already a dict, even for
            wire formats that stream argument fragments).
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
