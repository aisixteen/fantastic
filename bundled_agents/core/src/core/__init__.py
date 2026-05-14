"""core — userland orchestrator agent.

Lives at id="core" as the substrate's tree root. No handler_module —
root is a hollow tree container; the substrate handles all dispatch.

Composes a stdout-renderer child when stdin is a tty; otherwise
runs silent. `.run()` hands argv to `kernel.dispatch_argv` for mode
execution.
"""

from __future__ import annotations

import sys
from importlib import resources
from pathlib import Path

from kernel import Agent, Kernel, dispatch_argv


class Core(Agent):
    """The userland root agent. Composes Cli when interactive."""

    def __init__(
        self,
        kernel: Kernel,
        *,
        argv: list[str],
        root_path: Path | str = Path(".fantastic"),
    ) -> None:
        super().__init__(
            id="core",
            root_path=Path(root_path),
            ctx=kernel,
        )
        self.argv = argv
        # The root has no handler_module, so the substrate's _seed_readme
        # skips it — seed `.fantastic/readme.md` (the bootstrap primer)
        # explicitly. Copy-if-missing: operator edits + the GitHub-
        # canonical version are never clobbered.
        self._seed_root_readme()
        if sys.stdin.isatty():
            # Cli is ephemeral — composed per-process, never persisted.
            from cli import Cli

            Cli(self.ctx, parent=self)

    def _seed_root_readme(self) -> None:
        dest = self._root_path / "readme.md"
        if dest.exists():
            return
        try:
            src = resources.files("core") / "readme.md"
            if src.is_file():
                dest.write_text(src.read_text(encoding="utf-8"), encoding="utf-8")
        except (FileNotFoundError, OSError, TypeError):
            pass

    async def run(self) -> None:
        await dispatch_argv(self.ctx, self.argv)
