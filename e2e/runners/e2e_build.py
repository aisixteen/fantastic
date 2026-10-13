"""ONE BUILD AT A TIME into the shared e2e target dir (`src/target/e2e`).

Tests build `fantastic` with DIFFERENT features into one target dir: a run that
builds, then copies the binary, can copy another run's build if that one landed
in between (E23 got a binary without `vllm`, built by E16 a moment later). The
build AND the copy happen under this lock — an exclusive `flock` on a file in
the target dir, released when the process lets go of it.
"""

from __future__ import annotations

import fcntl
from contextlib import contextmanager
from pathlib import Path


@contextmanager
def build_lock(target: Path):
    target.mkdir(parents=True, exist_ok=True)
    with open(target / ".e2e-build.lock", "w") as f:
        fcntl.flock(f, fcntl.LOCK_EX)
        try:
            yield
        finally:
            fcntl.flock(f, fcntl.LOCK_UN)
