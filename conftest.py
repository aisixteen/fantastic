"""Shared pytest fixtures.

Each test runs in its own tmp_path with a fresh root Agent — safe to
run in parallel via pytest-xdist (-n auto). Multiple roots in the
same Python process get separate Kernel ctx objects (no cross-test
state leakage).
"""

from __future__ import annotations

import sys
from pathlib import Path

# Add repo root to sys.path so `from kernel import …` works.
_HERE = Path(__file__).resolve().parent
if str(_HERE) not in sys.path:
    sys.path.insert(0, str(_HERE))

import pytest

from cli import Cli
from core import Core

from kernel import Kernel


@pytest.fixture
def kernel(tmp_path, monkeypatch):
    """Fresh root Agent rooted in tmp_path.

    Returns the root Agent (a `Core` instance with argv=[]). It
    answers the standard Agent surface (send / emit / get / update /
    create / delete / list / watch / etc.); tests interact via
    `kernel.send`, `kernel.create`, ... directly.
    """
    monkeypatch.chdir(tmp_path)
    return Core(Kernel(), argv=[], root_path=Path(".fantastic"))


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
