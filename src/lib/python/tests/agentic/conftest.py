"""Fixtures for the manifest-driven suite.

Isolation is per test: each one gets its own `tmp_path`, chdirs into it, and
boots a fresh kernel from a config. Nothing is shared, so the suite stays safe
under `-n auto` (xdist workers are separate processes; `monkeypatch.chdir` never
races because tests are serial within a worker).

The four root fixtures (`kernel`, `seeded_kernel`, `file_bridge`, `store_agent`
in `<py>/conftest.py`) are still inherited and stay available for engine-level
unit tests — but a manifest scenario never uses them: its kernel must come from
its config, or the suite would be asserting against a shape nobody declared.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from _engine import EXERCISED, Hydration, config_paths, hydrate

# Globbed at collection. An empty `configs/` yields a single skipped item per
# runner test rather than a collection error — the suite is meant to be able to
# stand with no scenarios in it.
_CONFIGS = config_paths() or [None]


@pytest.fixture
def agentic_cwd(tmp_path, monkeypatch) -> Path:
    """Run this test inside an empty `tmp_path`. Mandatory before `hydrate`:
    `static_loader` reads its manifest through `fs.read_text(Path.cwd(), path)`,
    clamped to the cwd, so a manifest has to be materialized HERE and named
    relatively — a config file in `configs/` can never be passed by path."""
    monkeypatch.chdir(tmp_path)
    return tmp_path


@pytest.fixture
def hydrate_scenario(agentic_cwd):
    """Factory: `await hydrate_scenario("sealed_by_default")` → a `Hydration`.

    `require=True` (the default) refuses to hand back a kernel whose hydration
    did not do what the config declared. That is the whole point: `create_agent`
    boots its child, discards the reply and swallows the exception
    (`kernel/_agent.py:1231-1236`), so a scenario validator handed a raw kernel
    could be asserting against a roster that is silently half-built. The general
    runner passes `require=False` — asserting that verdict IS its job."""

    async def _run(config, *, require: bool = True) -> Hydration:
        path = config if isinstance(config, Path) else _resolve(config)
        h = await hydrate(path, agentic_cwd)
        return h.require_declared_outcome() if require else h

    return _run


def _resolve(name: str | Path) -> Path:
    """A config by bare name (`"roster_shape"`), stem, or path."""
    p = Path(name)
    if p.suffix == ".yaml" and p.exists():
        return p
    for candidate in config_paths():
        if candidate.stem == str(name) or candidate.name == str(name):
            return candidate
    raise FileNotFoundError(
        f"no config {name!r} in configs/ — have {[c.stem for c in config_paths()]}"
    )


@pytest.fixture(
    params=_CONFIGS, ids=lambda p: p.stem if p is not None else "no-configs"
)
def scenario_config(request) -> Path:
    """Every `configs/*.yaml`, one at a time. Discovery is a glob: adding a
    scenario is adding a file, never editing the runner."""
    if request.param is None:
        pytest.skip("configs/ is empty — nothing to reconcile yet")
    return request.param


@pytest.fixture
async def hydrated_config(scenario_config, hydrate_scenario) -> Hydration:
    """The current config, hydrated, receipts and all — NOT vetted against its
    declaration, because vetting it is what the runner asserts."""
    return await hydrate_scenario(scenario_config, require=False)


def pytest_terminal_summary(terminalreporter) -> None:
    """The coverage matrix a2a-tck keeps: which requirement ids this run
    actually pinned. A registered id with 0 checks is a hole, not a pass."""
    if not EXERCISED:
        return
    lines = ", ".join(f"{rid}×{n}" for rid, n in sorted(EXERCISED.items()))
    terminalreporter.write_line(f"agentic requirements pinned: {lines}")
