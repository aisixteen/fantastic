"""A bad snapshot is refused BEFORE the live tree is torn down.

`Kernel.load` clears `agents` and rebuilds. Every check that can fail therefore
has to run first, or a hand-authored document leaves a kernel with an empty tree
and no root — bricked by its own recovery path.

Measured 2026-09-02: a parent CYCLE resolved on both ends (each `parent_id` named
a record in the set) and passed validation, then hydrated nothing, silently
dropping every record in the cycle. And `_pid_alive` mapped EPERM to "dead", so a
lock held by another user's live process looked stale and was overwritten.
"""

from __future__ import annotations

import os

import pytest

from kernel._lock import _pid_alive
from kernel._state import SnapshotError, validate_records


def test_a_parent_cycle_is_refused():
    records = [
        {"id": "root", "parent_id": None},
        {"id": "a", "parent_id": "b"},
        {"id": "b", "parent_id": "a"},
    ]
    with pytest.raises(SnapshotError) as e:
        validate_records(records)
    assert "unreachable" in str(e.value)
    assert "'a'" in str(e.value) and "'b'" in str(e.value)


def test_a_reachable_tree_validates():
    records = [
        {"id": "root", "parent_id": None},
        {"id": "a", "parent_id": "root"},
        {"id": "b", "parent_id": "a"},
    ]
    assert validate_records(records) == "root"


async def test_load_refuses_a_cycle_before_tearing_the_tree_down(seeded_kernel):
    live = set(seeded_kernel.ctx.agents)
    assert live, "fixture must have a live tree"
    with pytest.raises(SnapshotError):
        seeded_kernel.ctx.load(
            [
                {"id": "root", "parent_id": None},
                {"id": "x", "parent_id": "y"},
                {"id": "y", "parent_id": "x"},
            ]
        )
    assert set(seeded_kernel.ctx.agents) == live, "the live tree must be intact"
    assert seeded_kernel.ctx.root is not None


def test_lock_treats_eperm_as_alive(monkeypatch):
    def _kill(pid, sig):
        raise PermissionError(1, "Operation not permitted")

    monkeypatch.setattr(os, "kill", _kill)
    assert _pid_alive(4242) is True

    def _gone(pid, sig):
        raise ProcessLookupError(3, "No such process")

    monkeypatch.setattr(os, "kill", _gone)
    assert _pid_alive(4242) is False
