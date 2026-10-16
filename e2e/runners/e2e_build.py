"""ONE BUILD AT A TIME into the shared e2e target dir (`src/target/e2e`).

Tests build `fantastic` with DIFFERENT features into one target dir: a run that
builds, then copies the binary, can copy another run's build if that one landed
in between (E23 got a binary without `vllm`, built by E16 a moment later). The
build AND the copy happen under this lock — an exclusive `flock` on a file in
the target dir, released when the process lets go of it.
"""

from __future__ import annotations

import sys
from contextlib import contextmanager
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from helpers import oslayer  # noqa: E402


@contextmanager
def build_lock(target: Path):
    target.mkdir(parents=True, exist_ok=True)
    with oslayer.exclusive_lock(target / ".e2e-build.lock"):
        yield


def variant_name(features: list[str]) -> str:
    """The prebuilt shell a scenario with these cargo `features` runs
    (`e2e/build.py` writes one per feature set): `fantastic-tui-<f1+f2|none>`."""
    return "fantastic-tui-" + ("+".join(sorted(features)) or "none")
