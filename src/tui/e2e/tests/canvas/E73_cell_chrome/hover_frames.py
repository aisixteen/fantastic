"""E73 `hover_costs_no_frames`: the frame rate at rest against the frame rate
while the pointer sweeps over the cells (#87).

The probe samples the two in pairs (rest, then sweep, three times), so each
pair meets the same runner load. On a software-rendered runner the rest samples
alone spread about 30% (17, 24, 22 fps), more than three times the tolerance,
so a best-of-each or a median-against-median test reads that spread as a hover
cost. A hover costs frames only when the pointer drags EVERY sweep sample below
the slowest rest sample, by more than the tolerance.
"""

from __future__ import annotations

TOLERANCE = 0.10


def hover_keeps_frames(
    rest: list[int], sweep: list[int], tolerance: float = TOLERANCE
) -> bool:
    """True unless every sweep sample is below the slowest rest sample by more
    than `tolerance` (a fraction: 0.10 is 10%). One sweep sample at the rest
    rate is enough to show the hover costs nothing."""
    if not rest or not sweep:
        raise ValueError("needs at least one rest sample and one sweep sample")
    return max(sweep) >= (1 - tolerance) * min(rest)
