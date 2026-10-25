"""The fantastic home, and the one rule about it: a kernel never keeps its own
state there.

The home is `FANTASTIC_HOME`, else `~/.fantastic`. It holds what the user keeps
across every fantastic (`user/`, `fantastics/`, `logs/`, the app's own `app/`),
so a kernel booted in `~` — whose `.fantastic` IS the home — is refused. Paths
are compared absolutized and folded (`.`/`..`), with no filesystem access, the
same rule as rust's `refuse_home_as_state_dir`.
"""

from __future__ import annotations

import os
from pathlib import Path

HOME_VAR = "FANTASTIC_HOME"


def home_spelled() -> str:
    """The home as a path is written: `FANTASTIC_HOME` made absolute, else
    `~/.fantastic` (the `~` kept, for a reader)."""
    h = os.environ.get(HOME_VAR)
    if h:
        return os.path.abspath(h)
    return "~/.fantastic"


def fantastic_home() -> Path | None:
    """The home dir, `~` expanded; `None` when neither `FANTASTIC_HOME` nor the
    user's home is set."""
    p = Path(home_spelled()).expanduser()
    return None if str(p).startswith("~") else p


def refuse_home_as_state_dir(workdir: Path, state_dir: Path) -> None:
    """Raise `SystemExit` naming both when `state_dir` is the fantastic home."""
    home = fantastic_home()
    if home is None:
        return
    if os.path.abspath(state_dir) == os.path.abspath(home):
        raise SystemExit(
            f"[kernel] {os.path.abspath(workdir)}: its .fantastic is the fantastic "
            f"home ({home}) — run fantastic in a project folder"
        )
