"""THE AGENTIC SUITE — one runner over the shared corpus at `contract/agentic/`.

Every scenario is DATA: a deployable kernel `config` and a `spec` that declares
what that config must prove. This file is the python RUNNER; rust and swift ship
their own, reading the same files. A wire-visible property is decidable from
outside, so asserting it once as data beats asserting it four times in four
languages — measured on the C21 corpus, which found four real rust drifts on its
first run.

A UNIT suite: free, no model, in process. It lives here rather than in `e2e/`
because that is where the rust and swift copies live, and because it should run
on every commit rather than only when someone invokes the paid harness.

Each scenario pays two phases:

  CORE (`_core.py`, PC-1..PC-5) — the price of being a config at all: the
  hydration verdict on the loud channel, the running tree against the declared
  shape both directions, a reach to walk, the roster closed after boot, and a
  reboot that rehydrates identically. A spec never lists these; they are not a
  property of the scenario.

  STEPS (`_engine.run_steps`) — the scenario's own ordered sequence. This is
  where CAUSE lives: the two-sided gate opens one grant half at a time and
  watches the denial move from `egress` to `ingress`, which a flat edge list
  could never say.
"""

from __future__ import annotations

import pytest

from _core import CORE_AFTER, CORE_BEFORE, run_core
from _engine import config_paths, load_config, run_steps

# Discovery is a GLOB: adding a scenario is adding a file. Specs, not configs —
# a config is a deployable kernel document and says nothing about being tested,
# so the suite is enumerated by what ASSERTS, and each spec names its config.
_SPECS = config_paths() or [None]


@pytest.mark.parametrize("spec_path", _SPECS, ids=lambda p: p.stem if p else "none")
async def test_scenario(spec_path, hydrate_scenario):
    if spec_path is None:
        pytest.skip("no scenarios in contract/agentic/specs")
    cfg = load_config(spec_path)
    # `require=False`: asserting the hydration verdict is the CORE phase's job,
    # so the runner must be handed the kernel even when hydration was declared
    # to fail.
    h = await hydrate_scenario(spec_path, require=False)

    # CORE FIRST, REBOOT INCLUDED, then the scenario's own steps.
    #
    # PC-4 asserts that rebooting the DECLARED system rehydrates it identically,
    # so it has to run before any step has mutated the tree — a scenario that
    # deletes an agent (lifecycle's C15) would otherwise fail it for the right
    # reason at the wrong target, since the manifest still declares that agent
    # and a reboot correctly builds it back.
    #
    # Running it first also makes the steps STRONGER: they execute against a
    # kernel that has been persisted and rehydrated, not just built.
    h = await run_core(h, CORE_BEFORE)
    h = await run_core(h, CORE_AFTER)
    if cfg.expect.hydration == "ok":
        await run_steps(h, cfg.expect.steps)


def test_every_requirement_a_spec_claims_is_actually_pinned():
    """A spec may not claim a requirement its steps never tag.

    Without this the coverage matrix reads as covered on a scenario that stopped
    asserting the thing — the one failure mode a requirement registry exists to
    prevent."""
    for spec_path in _SPECS:
        if spec_path is None:
            continue
        cfg = load_config(spec_path)
        tagged = {s.because for s in cfg.expect.steps if s.because}
        missing = set(cfg.requirements) - tagged
        assert not missing, (
            f"{spec_path.name}: `requirements:` claims {sorted(missing)},"
            f" but no step tags them (`because:`); tagged={sorted(tagged)}"
        )
