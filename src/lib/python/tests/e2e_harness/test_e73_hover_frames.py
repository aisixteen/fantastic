"""The E73 `hover_costs_no_frames` check (#87): rest and sweep frame rates are
sampled in pairs, and the hover costs frames only when every sweep sample is
below the slowest rest sample by more than 10%.

`hover_frames.py` sits beside the probe and has no imports, so it is loaded by
path here: the probe itself needs the live canvas rig (yaml, websockets) to run.
The numbers are the failing run's (summary.json, xcode-27, run 20261019-012825):
rest 17/24/22 and sweep 21/17/18 fps, with no hover cost in them.
"""

from __future__ import annotations

import importlib.util
from pathlib import Path

import pytest

HOVER = (
    next(
        p
        for p in Path(__file__).resolve().parents
        if (p / "e2e" / ".e2e-root").exists()
    )
    / "src"
    / "tui"
    / "e2e"
    / "tests"
    / "canvas"
    / "E73_cell_chrome"
    / "hover_frames.py"
)

_spec = importlib.util.spec_from_file_location("hover_frames", HOVER)
hover_frames = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(hover_frames)
hover_keeps_frames = hover_frames.hover_keeps_frames


def test_tolerance_is_ten_percent():
    assert hover_frames.TOLERANCE == 0.10


def test_failing_run_passes():
    # the run that failed on the old best-of-3 check (24 against 21, a miss of
    # 0.6 fps): no hover cost is in these samples, so the check must pass
    assert hover_keeps_frames([17, 24, 22], [21, 17, 18])


def test_failing_run_order_does_not_matter():
    assert hover_keeps_frames([24, 17, 22], [18, 21, 17])


def test_half_rate_sweep_fails():
    # a real hover cost: every sweep sample at half the rest rate
    assert not hover_keeps_frames([17, 24, 22], [8, 12, 11])
    assert not hover_keeps_frames([20, 22, 24], [10, 11, 12])


def test_one_sweep_sample_at_rest_rate_passes():
    # a sweep that dipped twice but once ran at the rest rate costs nothing
    assert hover_keeps_frames([20, 22, 24], [5, 21, 6])


def test_boundary_is_inclusive_at_ninety_percent():
    # 0.9 * 20 = 18: a sweep at exactly 18 is within tolerance, 17 is not
    assert hover_keeps_frames([20, 24, 22], [18, 10, 10])
    assert not hover_keeps_frames([20, 24, 22], [17, 10, 10])


def test_slowest_rest_sample_is_the_reference():
    # the fastest rest sample (24) would fail a sweep of 20; the slowest (17)
    # passes it, so the rest spread is noise, not the baseline
    assert hover_keeps_frames([17, 24, 22], [20, 19, 18])


def test_single_pair_is_compared_like_for_like():
    assert hover_keeps_frames([20], [18])
    assert not hover_keeps_frames([20], [17])


@pytest.mark.parametrize("rest, sweep", [([], [20]), ([20], []), ([], [])])
def test_no_samples_raises(rest, sweep):
    with pytest.raises(ValueError):
        hover_keeps_frames(rest, sweep)
