"""Shared pytest fixtures.

Each test runs in its own tmp_path with a fresh Kernel — safe to run
in parallel via pytest-xdist (-n auto).
"""

from __future__ import annotations

import sys
from pathlib import Path

# Add new_codebase/ (this dir) to sys.path so `from kernel import Kernel`
# works. Do NOT add the parent directory — that's the OLD codebase root
# and contains a stale `core/` package that would shadow the installed
# `core` workspace member.
_HERE = Path(__file__).resolve().parent
if str(_HERE) not in sys.path:
    sys.path.insert(0, str(_HERE))

import pytest

from kernel import Kernel


@pytest.fixture
def kernel(tmp_path, monkeypatch):
    """Fresh Kernel rooted in tmp_path. No singletons seeded."""
    monkeypatch.chdir(tmp_path)
    return Kernel()


@pytest.fixture
async def seeded_kernel(kernel):
    """Kernel with core + cli singletons (no boot fanout)."""
    kernel.ensure("core", "core.tools", singleton=True, display_name="core")
    kernel.ensure("cli", "cli.tools", singleton=True, display_name="cli")
    return kernel


@pytest.fixture
async def file_agent(seeded_kernel):
    """A real file agent rooted at cwd, ready to use as file_agent_id."""
    rec = await seeded_kernel.send(
        "core",
        {"type": "create_agent", "handler_module": "file.tools"},
    )
    assert "id" in rec, f"file agent creation failed: {rec!r}"
    return rec["id"]
