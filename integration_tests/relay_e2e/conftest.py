"""relay_e2e boots our relay (`relayd`, named by the `RELAYD` env var). It is NOT
gated behind an opt-in env flag — it self-skips per-test when `relayd` (or, under
`FANTASTIC_TARGET=container`, the relay image) or a kernel binary isn't built (see
`relay_harness.py`). So a default `pytest` run picks it up automatically wherever
the relay is present, and skips cleanly everywhere else.
"""

from __future__ import annotations
