"""Shared pytest fixtures.

Each test runs in its own tmp_path with a fresh root Agent — safe to
run in parallel via pytest-xdist (-n auto). Multiple roots in the
same Python process get separate Kernel ctx objects (no cross-test
state leakage).

The root is an `fs_loader` agent (`id="fs_loader"`) built the way the real
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

import pytest

from cli import Cli

from _testkit import boot_root


@pytest.fixture
def kernel(tmp_path, monkeypatch):
    """Fresh root Agent rooted in tmp_path.

    Returns the root Agent — an `fs_loader` at `id="fs_loader"`. It answers
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
async def file_agent(seeded_kernel):
    """A real file agent rooted at cwd, ready to use as file_agent_id."""
    rec = await seeded_kernel.send(
        seeded_kernel.id,
        {"type": "create_agent", "handler_module": "file.tools"},
    )
    assert "id" in rec, f"file agent creation failed: {rec!r}"
    return rec["id"]
