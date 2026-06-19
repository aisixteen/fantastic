"""Shared pytest fixtures.

Each test runs in its own tmp_path with a fresh root Agent — safe to
run in parallel via pytest-xdist (-n auto). Multiple roots in the
same Python process get separate Kernel ctx objects (no cross-test
state leakage).

The root is an `kernel_state` agent (`id="kernel_state"`) built the way the real
bootstrap builds it (see `_testkit.boot_root`). The loader's flush loop
is NOT started — logic tests stay pure in-memory; disk-lifecycle tests
call `_testkit.persist(root)` for deterministic on-disk state.
"""

from __future__ import annotations

import sys
from pathlib import Path

# Add repo root to sys.path so `from kernel import …` and `import
# _testkit` work.
_HERE = Path(__file__).resolve().parent
if str(_HERE) not in sys.path:
    sys.path.insert(0, str(_HERE))

import pytest  # noqa: E402

from cli import Cli  # noqa: E402

from _testkit import boot_root  # noqa: E402


@pytest.fixture
def kernel(tmp_path, monkeypatch):
    """Fresh root Agent rooted in tmp_path.

    Returns the root Agent — an `kernel_state` at `id="kernel_state"`. It answers
    the standard Agent surface (send / emit / get / update / create /
    delete / list / watch / ...); tests interact via `kernel.send`,
    `kernel.create`, ... directly.
    """
    monkeypatch.chdir(tmp_path)
    return boot_root()


@pytest.fixture
async def seeded_kernel(kernel):
    """Root Agent with a `cli` renderer attached (ephemeral, not
    persisted)."""
    Cli(kernel.ctx, parent=kernel)
    return kernel


@pytest.fixture
async def file_bridge(seeded_kernel):
    """A real file_bridge agent rooted at cwd, OPENED (the fs edge is sealed by
    default) — a GENERIC file surface (file_bridge's own tests use it directly)."""
    rec = await seeded_kernel.send(
        seeded_kernel.id,
        {
            "type": "create_agent",
            "handler_module": "file_bridge.tools",
            "ingress_rule": "allow_all",
        },
    )
    assert "id" in rec, f"file_bridge agent creation failed: {rec!r}"
    return rec["id"]


@pytest.fixture
async def store_agent(seeded_kernel):
    """The canonical persistence STORE: a file_bridge rooted at `.fantastic`, OPENED.
    The loader (`kernel_state`) DISCOVERS it as its provider (root resolves to `.fantastic`)
    and persists records — AND internal-state agents (yaml_state, scheduler) — THROUGH it
    via `persist_blob`, landing sidecars at `.fantastic/agents/<id>/` next to their
    agent.json (one shared store, no nest). Tests just need this fixture present so the
    loader has a store. (The ai backends still wire `file_bridge_id` to it directly — their
    migration is separate.)"""
    rec = await seeded_kernel.send(
        seeded_kernel.id,
        {
            "type": "create_agent",
            "handler_module": "file_bridge.tools",
            "root": ".fantastic",
            "ingress_rule": "allow_all",
        },
    )
    assert "id" in rec, f"store agent creation failed: {rec!r}"
    return rec["id"]
