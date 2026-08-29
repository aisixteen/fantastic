"""Fixtures for the manifest-driven suite.

Isolation is per test: each one gets its own `tmp_path`, chdirs into it, and
boots a fresh kernel from a config. Nothing is shared, so the suite stays safe
under `-n auto` (xdist workers are separate processes; `monkeypatch.chdir` never
races because tests are serial within a worker).
"""

from __future__ import annotations

from pathlib import Path

import pytest

from _engine import EXERCISED, Hydration, config_paths, hydrate


@pytest.fixture
def agentic_cwd(tmp_path, monkeypatch) -> Path:
    """Run this test inside an empty `tmp_path`. Mandatory before `hydrate`:
    `static_loader` reads its manifest through `fs.read_text(Path.cwd(), path)`,
    clamped to the cwd, so a manifest has to be materialized HERE and named
    relatively — a config file in `configs/` can never be passed by path."""
    monkeypatch.chdir(tmp_path)
    return tmp_path


def _resolve(name: str | Path) -> Path:
    """A config by bare name (`"sealed_by_default"`), stem, or path."""
    p = Path(name)
    if p.suffix == ".yaml" and p.exists():
        return p
    for candidate in config_paths():
        if candidate.stem == str(name) or candidate.name == str(name):
            return candidate
    raise FileNotFoundError(
        f"no config {name!r} in configs/ — have {[c.stem for c in config_paths()]}"
    )


@pytest.fixture
def hydrate_scenario(agentic_cwd):
    """Factory: `await hydrate_scenario("sealed_by_default")` → a `Hydration`.

    `require=True` (the default) refuses to hand back a kernel whose hydration
    did not do what the config declared. That is the whole point: `create_agent`
    boots its child, discards the reply and swallows the exception, so a scenario
    validator handed a raw kernel could be asserting against a roster that is
    silently half-built. The general runner passes `require=False` — asserting
    that verdict IS its job."""

    async def _run(config, *, require: bool = True) -> Hydration:
        path = config if isinstance(config, Path) else _resolve(config)
        h = await hydrate(path, agentic_cwd)
        return h.require_declared_outcome() if require else h

    return _run


def pytest_terminal_summary(terminalreporter) -> None:
    """The coverage matrix: which requirement ids this run actually pinned. A
    registered id with 0 checks is a hole, not a pass — and these counts are the
    PARITY PROOF, since rust and swift pin the identical set from their own
    runners over the same corpus."""
    if not EXERCISED:
        return
    lines = ", ".join(f"{rid}×{n}" for rid, n in sorted(EXERCISED.items()))
    terminalreporter.write_line(f"agentic requirements pinned: {lines}")
