"""relay_e2e boots binaries from the sibling `../fantastic_relay` repo. It is NOT
gated behind an opt-in env flag — it self-skips per-test when the relay binaries
(router / issuer) or the canvas venv aren't built (see `relay_harness.py`). So a
default `pytest` run picks it up automatically wherever the relay is present, and
skips cleanly everywhere else.
"""

from __future__ import annotations
