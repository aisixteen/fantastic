"""Shared mutable state for server submodules."""

from typing import Callable

# Global instances — wired during lifespan
engine = None
process_runner = None
file_watcher_task = None

# ─── Plugin hooks ───────────────────────────────────────────────────────
# Bundles register these during register_tools() to extend server behavior
# without core/server knowing about specific plugins.

_route_hooks: list[Callable] = []  # (app, state_module) → register REST routes
_broadcast_resolvers: list[Callable] = []  # (message) → scope or ""
_lifespan_hooks: list[
    Callable
] = []  # async (state_module, broadcast_fn) → startup tasks
_lifespan_hooks_ran: int = 0  # tracks how many hooks have been executed


def register_route_hook(fn: Callable) -> None:
    """Register a hook to add custom REST/WS routes. Called with (app, state_module)."""
    _route_hooks.append(fn)


def register_broadcast_resolver(fn: Callable) -> None:
    """Register a broadcast resolver. Returns scope name or '' for global."""
    _broadcast_resolvers.append(fn)


def register_lifespan_hook(fn: Callable) -> None:
    """Register an async startup hook. Called with (state_module, broadcast_fn)."""
    _lifespan_hooks.append(fn)


def clear_hooks() -> None:
    """Clear all hooks — for tests."""
    global _lifespan_hooks_ran
    _route_hooks.clear()
    _broadcast_resolvers.clear()
    _lifespan_hooks.clear()
    _lifespan_hooks_ran = 0
