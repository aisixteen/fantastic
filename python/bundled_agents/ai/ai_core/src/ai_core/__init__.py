"""ai_core — shared machinery for the reflect-driven LLM agent backends.

A LIBRARY (no `fantastic.bundles` entry point), imported by the
ollama / anthropic / nvidia_nim backends. Holds:

  - `provider.Provider`: the streaming-chat interface every backend's
    real provider duck-types.
  - `core`: the queue / FIFO-lock / menu-cache state dicts (keyed by
    agent id, so safe to share across backends loaded in ONE kernel),
    prompt assembly, the agentic `_run` loop, and the verb bodies.
  - `build(...)`: a CLOSURE FACTORY. Each backend calls it with its own
    Provider builder + config and gets back a `(VERBS, handler)` pair
    bound to THAT backend. The module-global STATE is shared by id; the
    CONFIG is per-backend, so an ollama agent and an anthropic agent in
    the same kernel never clobber each other.
"""

from __future__ import annotations

from .core import build
from .provider import Provider

__all__ = ["Provider", "build"]
