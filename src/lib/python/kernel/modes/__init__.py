"""kernel.modes — substrate CLI dispatch, split by concern.

`dispatch_argv` is the public entrypoint.
"""

from __future__ import annotations

from kernel.modes.dispatch import dispatch_argv

__all__ = ["dispatch_argv"]
