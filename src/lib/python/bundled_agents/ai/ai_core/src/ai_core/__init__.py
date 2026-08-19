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
  - `on_cancel(id, task_id, kernel)`: the shared bundle CANCEL hook. A
    backend re-exports it from its `tools` module and the substrate
    discovers it there (same mechanism as `on_delete`). There is no
    `interrupt` verb — an in-flight `send` is a kernel TASK, killed with
    the universal `ps` → `cancel` system verbs; this hook closes the turn
    out for the client.
"""

from __future__ import annotations

from .core import CANCEL_DOC, build, on_cancel
from .provider import Provider

__all__ = ["CANCEL_DOC", "Provider", "build", "on_cancel"]
