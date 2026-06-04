"""runner_core — shared lifecycle for the local/ssh `fantastic` runners.

A LIBRARY (no `fantastic.bundles` entry point), imported by both
local_runner and ssh_runner. Holds the seven shared lifecycle verbs
(`core`), the transport seam (`transport.Transport`), and the WS health
probe (`health._ws_health`).
"""

from __future__ import annotations

from .core import (
    LOCK_POLL_INTERVAL,
    LOCK_POLL_TIMEOUT,
    STOP_POLL_INTERVAL,
    STOP_POLL_TIMEOUT,
)
from .health import _ws_health
from .transport import Transport

__all__ = [
    "LOCK_POLL_TIMEOUT",
    "LOCK_POLL_INTERVAL",
    "STOP_POLL_TIMEOUT",
    "STOP_POLL_INTERVAL",
    "Transport",
    "_ws_health",
]
