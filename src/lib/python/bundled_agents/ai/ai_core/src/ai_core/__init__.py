"""ai_core — shared machinery for the reflect-driven LLM agent backends.

A LIBRARY (no `fantastic.bundles` entry point), imported by the
ollama / responses / vllm backends. Holds:

  - `provider.Provider`: the streaming-chat interface every backend's
    real provider duck-types.
  - `core`: the queue / FIFO-lock / menu-cache state dicts (keyed by
    agent id, so safe to share across backends loaded in ONE kernel),
    prompt assembly, the agentic `_run` loop, and the verb bodies.
  - `build(...)`: a CLOSURE FACTORY. Each backend calls it with its own
    Provider builder + config and gets back a `(VERBS, handler)` pair
    bound to THAT backend. The module-global STATE is shared by id; the
    CONFIG is per-backend, so an ollama agent and a vllm agent in
    the same kernel never clobber each other.
  - `on_cancel(id, task_id, kernel)`: the shared bundle CANCEL hook. A
    backend re-exports it from its `tools` module and the substrate
    discovers it there (same mechanism as `on_delete`). There is no
    `interrupt` verb — an in-flight `send` is a kernel TASK, killed with
    the universal `ps` → `cancel` system verbs; this hook closes the turn
    out for the client.
  - `on_delete(agent)`: the shared bundle DELETE hook, re-exported the same
    way — it drops every row `core` keeps for the deleted id.
"""

from __future__ import annotations

from .core import CANCEL, EMITS, NOTES, build, on_cancel, on_delete, required_meta
from .provider import Provider, ProviderError, stalled_error, stream_idle_of

__all__ = [
    "CANCEL",
    "EMITS",
    "NOTES",
    "Provider",
    "ProviderError",
    "build",
    "on_cancel",
    "on_delete",
    "required_meta",
    "stalled_error",
    "stream_idle_of",
]
