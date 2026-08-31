"""The engine's own tests — hydrate / dehydrate / reach / the ID registry.

The runner in `test_agentic.py` trusts these three primitives with every claim
the suite makes, so they are pinned directly here rather than only through a
scenario. Two properties matter most:

- `reach()` must actually make the driver an AGENT. Outside a dispatch
  `_current_sender` is None and a bare `kernel.send` is an OPERATOR send, which
  passes the gate ungated — a reachability probe that forgot the sender would
  report every edge as usable and never fail.
- `hydrate()` must refuse to hand back a kernel that did not do what its config
  declared, because `create_agent` boots its child, discards the reply and
  swallows the exception: a half-built roster looks exactly like a healthy one
  from the create reply alone.

These test the RUNNER, not the substrate — so they carry no requirement ids.
"""

from __future__ import annotations

from dataclasses import replace
from pathlib import Path

import pytest

from _engine import (
    CORE_REQUIREMENTS,
    SPEC_DIR,
    ConfigError,
    HydrationError,
    Requirement,
    _index,
    config_paths,
    coverage_matrix,
    dehydrate,
    denial_side,
    denied,
    hydrate,
    load_config,
    reach,
    requirement,
)
from _testkit import persist


async def _mk(kernel, **meta) -> str:
    """A plain file_bridge child of the root, OPENED so the bundle's own io seal
    never masks the call gate under test (the idiom `test_call_security._mk`
    uses, and for the same reason)."""
    payload = {
        "type": "create_agent",
        "parent_id": kernel.id,
        "handler_module": "file_bridge.tools",
        "ingress_rule": "allow_all_ingress",
    }
    payload.update(meta)
    rec = (await kernel.send([{"target": kernel.id, "payload": payload}]))[0]["result"]
    assert "id" in rec, rec
    return rec["id"]


# ─── the requirement registry ───────────────────────────────────


def test_a_duplicate_requirement_id_is_refused_when_the_table_is_built():
    """The one failure that makes the registry worthless: two rows under one id.
    Both validators would then read as covered whichever invariant actually ran,
    so the check is at build time, not at report time."""
    with pytest.raises(ValueError, match="duplicate requirement id 'X-1'"):
        _index((("X-1", "MUST", "first"), ("X-1", "MUST", "second")))


def test_a_requirement_level_outside_the_three_is_refused():
    with pytest.raises(ValueError, match="level"):
        _index((("X-1", "SHALL", "not a level"),))


def test_an_unknown_requirement_id_names_itself_and_the_known_set():
    """A typo'd tag must not read as an untagged assertion — it raises here,
    naming the id, and `load_config` walks a config's `requirements:` through
    this so a bad tag fails at load rather than at coverage-report time."""
    with pytest.raises(KeyError, match="PC-9"):
        requirement("PC-9")


def test_the_core_preconfig_invariants_are_all_registered_as_MUST():
    for rid in ("PC-1", "PC-2", "PC-3", "PC-4", "PC-5"):
        req = requirement(rid)
        assert isinstance(req, Requirement)
        assert req.level == "MUST", f"{rid} is {req.level}, the core five are hard"
    assert set(coverage_matrix()) >= {"PC-1", "C14", "C23"}


# ─── config parsing — strict, unlike the manifest it wraps ──────


def _pair(tmp_path, *, spec: str, config: str = "") -> Path:
    """Write a spec + the config it names, and return the SPEC path.

    The two files are separate on disk on purpose (a config is deployable and
    carries no test metadata), so every parser test has to write both halves.
    """
    (tmp_path / "configs").mkdir(exist_ok=True)
    (tmp_path / "specs").mkdir(exist_ok=True)
    (tmp_path / "configs" / "c.yaml").write_text(config)
    sp = tmp_path / "specs" / "c.yaml"
    sp.write_text("config: c.yaml\n" + spec)
    return sp


def test_an_unknown_config_key_is_refused_rather_than_dropped(tmp_path):
    """`static_loader` silently ignores every top-level key but `agents:` and
    never objects to a misspelled meta — measured: `can_calls:` becomes inert
    record meta with no complaint anywhere. A contract file that quietly drops
    half of what you wrote is the exact failure this suite exists to catch, so
    the wrapper refuses what the loader would swallow."""
    p = _pair(tmp_path, spec="name: x\n", config="agents: []\nloaders: []\n")
    with pytest.raises(ConfigError, match="unknown key"):
        load_config(p)


def test_a_loader_must_name_exactly_one_manifest_source(tmp_path):
    p = _pair(
        tmp_path,
        spec="",
        config="loaders:\n  - id: l\n    manifest: {agents: []}\n"
        "    missing_manifest: true\n",
    )
    with pytest.raises(ConfigError, match="exactly one of"):
        load_config(p)


def test_a_config_may_not_claim_a_requirement_the_general_runner_owns(tmp_path):
    """`requirements:` names what a scenario's OWN validator pins. The five core
    ids are pinned by the runner on every config, so listing one says nothing
    about this file — and letting configs list them made the field mean three
    different things across eight of them (some named all five, some a subset,
    some none, all while the runner walked all five regardless)."""
    p = _pair(
        tmp_path,
        spec="requirements: [PC-1, C20]\n",
        config="loaders:\n  - id: l\n    manifest: {agents: []}\n",
    )
    with pytest.raises(ConfigError, match="PC-1"):
        load_config(p)


def test_every_shipped_config_declares_only_scenario_requirements():
    """The rule above, walked over the real `configs/` directory rather than only
    over a synthetic file. `load_config` raising is what enforces it, so a config
    that drifted fails at collection anyway — this is where the failure NAMES the
    file and the ids, instead of surfacing as a parametrized fixture error."""
    for path in config_paths():
        cfg = load_config(path)
        core = sorted(set(cfg.requirements) & CORE_REQUIREMENTS)
        assert not core, f"{path.name}: declares core ids {core}"


def test_a_declared_failure_must_name_the_message_it_expects(tmp_path):
    """A test that asserts "this failed" without pinning WHY passes for the wrong
    reason the first time an unrelated thing breaks."""
    p = _pair(
        tmp_path,
        spec="expect:\n  hydration: refused_at_write\n",
        config="loaders:\n  - id: l\n    missing_manifest: true\n",
    )
    with pytest.raises(ConfigError, match="error_contains"):
        load_config(p)


# ─── reach — the reachability primitive ─────────────────────────


async def test_reach_makes_the_driver_an_agent_and_returns_what_it_gets(seeded_kernel):
    """The gate only runs when `_current_sender` is set. `reach` sets it around
    ONLY the send under test — the setup creates stay operator-authority — and
    returns the reply verbatim, `side` included, so a validator asserts on the
    machine keys rather than on a stringified error."""
    a = await _mk(seeded_kernel)
    b = await _mk(seeded_kernel)
    r = await reach(seeded_kernel, a, b, {"type": "list", "path": ""})
    assert denied(r), r
    assert denial_side(r) == "egress"  # a has no can_call reach to b
    assert r.get("hint")  # the denial teaches how to open it — from above


async def test_a_bare_send_is_an_operator_send_and_proves_nothing(seeded_kernel):
    """Why `reach` exists at all. The same call without a sender is the ORIGIN
    root of trust and passes ungated — a probe that forgot `sender_context`
    would report every edge in every config as usable and never go red."""
    a = await _mk(seeded_kernel)
    b = await _mk(seeded_kernel)
    ungated = (
        await seeded_kernel.send(
            [{"target": b, "payload": {"type": "list", "path": ""}}]
        )
    )[0]["result"]
    assert not denied(ungated), ungated
    assert denied(await reach(seeded_kernel, a, b, {"type": "list", "path": ""}))


async def test_reach_reports_a_two_sided_pass_as_a_non_denial(seeded_kernel):
    """An ALLOW is asserted as a NEGATIVE. A pass returns the handler's own
    reply, which may itself carry an `error` (a sealed leg, a missing store) —
    a different animal from a refusal, and conflating them would make every
    "declared edge is usable" assertion untrustworthy in both directions."""
    b = await _mk(seeded_kernel, callable_by={"caller": ["list"]}, id="callee")
    a = await _mk(seeded_kernel, can_call={b: ["list"]}, id="caller")
    ok = await reach(seeded_kernel, a, b, {"type": "list", "path": ""})
    assert not denied(ok), ok


# ─── hydrate / dehydrate ────────────────────────────────────────


async def test_hydrate_builds_the_declared_kernel_and_hands_back_its_receipts(
    hydrate_scenario,
):
    """The receipts are the point: `creates` is the only loud channel for a
    write-time refusal, `boots` the only one for a manifest that parses and then
    cannot build. Note the boot reply here reports the roster as SKIPPED, not
    created — `create_agent` already hydrated it inside its swallowed boot, and
    the explicit re-boot is idempotent by id."""
    h = await hydrate_scenario("engine_smoke")
    assert h.outcome() == "ok", h.outcome_error()
    assert h.creates["static_loader"]["id"] == "static_loader"
    boot = h.loader_boot("static_loader")
    assert boot["booted"] is True, boot
    assert set(boot["skipped"]) == {"store", "memory", "worker", "quiet", "island"}, (
        boot
    )
    assert boot["created"] == [], boot
    assert "store" in h.agents  # the layered operator declaration ran first


async def test_hydrate_refuses_a_kernel_that_did_not_match_its_declaration(
    hydrate_scenario,
):
    """`engine_smoke_bad_entry` declares `boot_error`, so requiring `ok` from it
    must raise rather than return a silently partial roster. This is the guard a
    scenario validator leans on."""
    h = await hydrate_scenario("engine_smoke_bad_entry", require=False)
    assert h.outcome() == "boot_error"
    assert "good" in h.agents and "never" not in h.agents  # partial, unreported
    as_if_healthy = replace(
        h, config=replace(h.config, expect=replace(h.config.expect, hydration="ok"))
    )
    with pytest.raises(HydrationError, match="declared hydration"):
        as_if_healthy.require_declared_outcome()


async def test_hydrate_outside_its_own_tmp_path_is_refused(agentic_cwd):
    """The manifest read is clamped to `Path.cwd()`, so hydrating against some
    other directory would silently look for the manifest in the wrong place —
    or, with a leading `/`, silently reinterpret it as cwd-relative."""
    spec = SPEC_DIR / "engine_smoke.yaml"
    with pytest.raises(HydrationError, match="clamped to the cwd"):
        await hydrate(spec, agentic_cwd / "somewhere-else")


async def test_dehydrate_reads_the_running_system_back_through_one_reflect(
    hydrate_scenario,
):
    """Medium-agnostic and opinion-free: the tree comes from the same root
    reflect a client would get, flattened. The declared roster is present with
    its parentage, and the layered store shows up as the discovered persistence
    provider — the thing a manifest-declared store can never become."""
    h = await hydrate_scenario("engine_smoke")
    d = await dehydrate(h.kernel)
    assert {"static_loader", "memory", "worker", "quiet", "island", "store"} <= set(
        d["ids"]
    )
    # The store is a ROSTER entry now, not a layered root child — `_find_store`
    # walks the subtree, so declaring it in the manifest is enough.
    assert d["agents"]["store"]["parent_id"] == "static_loader"
    assert d["agents"]["memory"]["parent_id"] == "static_loader"
    assert d["agents"]["memory"]["handler_module"] == "text_file.tools"
    assert set(d["children"]["static_loader"]) == {
        "store",
        "memory",
        "worker",
        "quiet",
        "island",
    }
    assert d["persistence_provider"] == "store"
    assert "disk" not in d  # on-disk state is opt-in, not every scenario reads it


async def test_dehydrate_reports_the_persisted_tree_when_asked(hydrate_scenario):
    """`disk=True` adds the records `read_tree` finds plus the file layout, for
    the scenarios that care WHERE a child's state landed. The flush loop is not
    running (that is deliberate — a background debounce is not determinism), so
    disk state exists only after an explicit `persist`."""
    h = await hydrate_scenario("engine_smoke")
    persist(h.kernel)
    d = await dehydrate(h.kernel, disk=True)
    assert {"static_loader", "memory", "store"} <= set(d["disk_ids"])
    # Namespacing is POSITIONAL: a roster agent's record lives under its loader.
    assert "agents/static_loader/agents/memory/agent.json" in d["disk_files"]
    assert all("docs/" not in f for f in d["disk_files"])
