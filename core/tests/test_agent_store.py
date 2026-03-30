"""Tests for AgentStore — persistent .fantastic/ directory."""

import json

import pytest

from core.agent_store import AgentStore


@pytest.fixture
def store(tmp_path):
    s = AgentStore(tmp_path)
    s.init()
    return s


# ─── Initialization ──────────────────────────────────────────────────────


def test_init_creates_fantastic_dir(store, tmp_path):
    assert (tmp_path / ".fantastic").is_dir()
    assert (tmp_path / ".fantastic" / "agents").is_dir()


def test_init_creates_config(store, tmp_path):
    config_path = tmp_path / ".fantastic" / "config.json"
    assert config_path.exists()
    config = json.loads(config_path.read_text())
    assert config["port"] == 8888


def test_init_creates_registry(store, tmp_path):
    registry_path = tmp_path / ".fantastic" / "registry.json"
    assert registry_path.exists()
    assert json.loads(registry_path.read_text()) == {}


def test_init_idempotent(tmp_path):
    """Calling init() twice doesn't break anything."""
    s = AgentStore(tmp_path)
    s.init()
    s.init()
    agents = s.list_agents()
    assert len(agents) == 0


# ─── Hooks ────────────────────────────────────────────────────────────


def test_on_agent_deleted_hook(store):
    deleted_ids = []
    store.on_agent_deleted(lambda aid: deleted_ids.append(aid))
    store.create_agent(agent_id="a1")
    store.delete_agent("a1")
    assert deleted_ids == ["a1"]


def test_on_enrich_agent_hook(store):
    """Enrich hooks are called on agent dict construction."""
    store.on_enrich_agent(lambda aid, d: d.update({"enriched": True}))
    store.create_agent(agent_id="a1")
    agent = store.get_agent("a1")
    assert agent["enriched"] is True


def test_multiple_enrich_hooks(store):
    store.on_enrich_agent(lambda aid, d: d.update({"hook1": True}))
    store.on_enrich_agent(lambda aid, d: d.update({"hook2": True}))
    store.create_agent(agent_id="a1")
    agent = store.get_agent("a1")
    assert agent["hook1"] is True
    assert agent["hook2"] is True


# ─── Agent CRUD ──────────────────────────────────────────────────────


def test_create_agent(store):
    agent = store.create_agent(agent_id="a1")
    assert agent["id"] == "a1"
    assert agent["delete_lock"] is False


def test_create_agent_auto_id(store):
    agent = store.create_agent()
    assert agent["id"].startswith("agent_")
    assert len(agent["id"]) == 12  # agent_ + 6 hex chars


def test_create_agent_with_bundle(store):
    agent = store.create_agent(agent_id="t1", bundle="bundle_b")
    assert agent["bundle"] == "bundle_b"


def test_create_agent_duplicate_raises(store):
    store.create_agent(agent_id="a1")
    with pytest.raises(ValueError, match="already exists"):
        store.create_agent(agent_id="a1")


def test_get_agent(store):
    store.create_agent(agent_id="a1")
    agent = store.get_agent("a1")
    assert agent is not None
    assert agent["id"] == "a1"


def test_get_agent_not_found(store):
    assert store.get_agent("nonexistent") is None


def test_list_agents(store):
    store.create_agent(agent_id="a1")
    store.create_agent(agent_id="a2")
    agents = store.list_agents()
    assert len(agents) == 2
    ids = {a["id"] for a in agents}
    assert ids == {"a1", "a2"}


def test_delete_agent(store):
    store.create_agent(agent_id="a1")
    assert store.delete_agent("a1") is True
    assert store.get_agent("a1") is None
    assert store.delete_agent("a1") is False


def test_update_agent_meta(store):
    store.create_agent(agent_id="a1")
    store.update_agent_meta("a1", display_name="My Agent")
    agent = store.get_agent("a1")
    assert agent["display_name"] == "My Agent"


def test_update_agent_meta_not_found(store):
    with pytest.raises(ValueError, match="not found"):
        store.update_agent_meta("nonexistent", display_name="x")


# ─── Source / Output ─────────────────────────────────────────────────


def test_source_read_write(store):
    store.create_agent(agent_id="a1")
    assert store.get_source("a1") == ""
    store.set_source("a1", "print('hello')")
    assert store.get_source("a1") == "print('hello')"


def test_output_read_write(store):
    store.create_agent(agent_id="a1")
    assert store.get_output("a1") == ""
    store.set_output("a1", "<p>hi</p>")
    assert store.get_output("a1") == "<p>hi</p>"


# ─── Registry / Config ──────────────────────────────────────────────


def test_registry(store):
    assert store.get_registry() == {}
    store.set_registry({"a1": {"url": "http://x"}})
    assert store.get_registry() == {"a1": {"url": "http://x"}}


def test_config(store):
    config = store.get_config()
    assert config["port"] == 8888
    config["port"] = 9999
    store.set_config(config)
    assert store.get_config()["port"] == 9999


# ─── Bundle lookup ──────────────────────────────────────────────────


def test_find_by_bundle(store):
    store.create_agent(agent_id="a1", bundle="bundle_a")
    store.create_agent(agent_id="a2", bundle="bundle_b")
    agent = store.find_by_bundle("bundle_a")
    assert agent is not None
    assert agent["id"] == "a1"
    assert agent["bundle"] == "bundle_a"


def test_find_by_bundle_not_found(store):
    store.create_agent(agent_id="a1", bundle="bundle_b")
    assert store.find_by_bundle("bundle_a") is None


def test_find_by_bundle_empty(store):
    assert store.find_by_bundle("bundle_a") is None


# ─── working_dir ──────────────────────────────────────────────────


def test_build_agent_dict_includes_working_dir(store):
    agent = store.create_agent(agent_id="a1")
    store.update_agent_meta("a1", working_dir="./notebooks")
    agent = store.get_agent("a1")
    assert agent["working_dir"] == "./notebooks"


def test_build_agent_dict_omits_working_dir_when_empty(store):
    agent = store.create_agent(agent_id="a1")
    assert "working_dir" not in agent


# ─── get_root_parent ──────────────────────────────────────────────


def test_get_root_parent_single_level(store):
    store.create_agent(agent_id="root1")
    root = store.get_root_parent("root1")
    assert root is not None
    assert root["id"] == "root1"


def test_get_root_parent_two_levels(store):
    store.create_agent(agent_id="root_a1", bundle="bundle_a")
    store.create_agent(agent_id="child1", parent="root_a1")
    root = store.get_root_parent("child1")
    assert root is not None
    assert root["id"] == "root_a1"


def test_get_root_parent_nonexistent(store):
    assert store.get_root_parent("nonexistent") is None
