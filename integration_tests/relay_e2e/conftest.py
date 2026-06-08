"""relay_e2e is OPT-IN — it boots binaries from the sibling `../fantastic_relay`
repo, so it must not silently join the canvas-only default suite. Enable with
`FANTASTIC_RELAY_E2E=1`. (It also skips per-test if the relay binaries aren't
built — this gate just keeps the *default* `pytest` run self-contained.)"""

from __future__ import annotations

import os

import pytest

_ON = os.environ.get("FANTASTIC_RELAY_E2E", "").strip().lower() in ("1", "true", "yes", "on")


def pytest_collection_modifyitems(config, items):
    if _ON:
        return
    skip = pytest.mark.skip(
        reason="relay e2e is opt-in — set FANTASTIC_RELAY_E2E=1 (heavy; needs ../fantastic_relay built)"
    )
    for item in items:
        if "relay_e2e" in str(item.fspath):
            item.add_marker(skip)
