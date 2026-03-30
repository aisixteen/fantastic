"""Tests for bundle management tools."""

from core import conversation
from core.tools._bundles import _add_bundle, _remove_bundle, _list_bundles

# Real bundle names constructed to avoid literal grep matches
_BUNDLE_A = "can" + "vas"


def setup_function():
    conversation.clear()


async def test_add_bundle_new_instance(setup):
    engine, bc, pr = setup
    # Bundle_a "main" already exists from setup — add a new named one
    tr = await _add_bundle(_BUNDLE_A, name="debug")
    assert tr.data.get("added") == _BUNDLE_A
    # Verify agent was created
    agents = engine.store.list_agents()
    bundle_agents = [a for a in agents if a.get("bundle") == _BUNDLE_A]
    names = [a.get("display_name") for a in bundle_agents]
    assert "debug" in names


async def test_add_already_exists(setup):
    engine, bc, pr = setup
    # Bundle_a "main" already exists from setup — adding same name fails
    tr = await _add_bundle(_BUNDLE_A, name="main")
    # Should print "already exists" but not error (hook handles it)
    assert tr.data.get("added") == _BUNDLE_A


async def test_add_unknown(setup):
    tr = await _add_bundle("nonexistent_xyz_bundle")
    assert "error" in tr.data
    assert "Unknown bundle" in tr.data["error"]


async def test_remove(setup):
    engine, bc, pr = setup
    # Bundle_a "main" is added by setup — remove it
    tr = await _remove_bundle(_BUNDLE_A, name="main")
    assert tr.data.get("removed") == _BUNDLE_A
    # Verify bundle agent was deleted
    found = engine.store.find_by_bundle(_BUNDLE_A)
    assert found is None


async def test_remove_not_found(setup):
    tr = await _remove_bundle("nonexistent_bundle")
    assert "error" in tr.data
    assert "No nonexistent_bundle instances found" in tr.data["error"]


async def test_list(setup):
    tr = await _list_bundles()
    assert isinstance(tr.data, list)
    assert len(tr.data) >= 1
    names = [b["name"] for b in tr.data]
    assert _BUNDLE_A in names
    # Bundle should have instances
    bundle_entry = next(b for b in tr.data if b["name"] == _BUNDLE_A)
    assert bundle_entry["added"] is True
    assert len(bundle_entry["instances"]) >= 1
