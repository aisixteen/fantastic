"""cli — stdout renderer agent.

Cli(kernel, *, parent) -> Agent
    Renders status/token/done/say/error events to stdout.
    Ephemeral (never persisted). Watches whatever parent (or
    external watcher wiring) directs to its inbox.
"""

from __future__ import annotations

from kernel import Agent, Kernel


class Cli(Agent):
    """Stdout renderer agent. Ephemeral — never persists to disk."""

    ephemeral = True

    def __init__(self, kernel: Kernel, *, parent: Agent) -> None:
        super().__init__(
            id="cli",
            ctx=kernel,
            parent=parent,
            handler_module="cli.tools",
            display_name="cli",
            singleton=True,
        )
