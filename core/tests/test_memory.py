"""Tests for per-agent long-term memory (memory_long.jsonl)."""

import json

import pytest
from starlette.testclient import TestClient

from core.agent_store import AgentStore
from core.engine import Engine


# ─── AgentStore memory ───────────────────────────────────────────────────


@pytest.fixture
def store(tmp_path):
    s = AgentStore(tmp_path)
    s.init()
    return s


@pytest.mark.asyncio
async def test_append_memory(store):
    store.create_agent(agent_id="a1")
    entry = await store.append_memory("a1", 0, {"kind": "test", "data": "hello"})
    assert "ts" in entry
    assert entry["type"] == 0
    assert entry["message"]["kind"] == "test"


@pytest.mark.asyncio
async def test_read_memory_empty(store):
    store.create_agent(agent_id="a1")
    entries = store.read_memory("a1")
    assert entries == []


@pytest.mark.asyncio
async def test_read_memory_after_append(store):
    store.create_agent(agent_id="a1")
    await store.append_memory("a1", 0, {"kind": "exec", "code": "x=1"})
    await store.append_memory("a1", 1, {"kind": "exec", "code": "x=2"})
    entries = store.read_memory("a1")
    assert len(entries) == 2
    assert entries[0]["message"]["code"] == "x=1"
    assert entries[1]["message"]["code"] == "x=2"


@pytest.mark.asyncio
async def test_read_memory_filter_from_ts(store):
    store.create_agent(agent_id="a1")
    await store.append_memory("a1", 0, {"n": 1})
    await store.append_memory("a1", 0, {"n": 2})
    entries = store.read_memory("a1")
    assert len(entries) == 2
    # Filter: only entries from the second one onward
    ts_cutoff = entries[1]["ts"]
    filtered = store.read_memory("a1", from_ts=ts_cutoff)
    assert len(filtered) == 1
    assert filtered[0]["message"]["n"] == 2


@pytest.mark.asyncio
async def test_read_memory_filter_to_ts(store):
    store.create_agent(agent_id="a1")
    await store.append_memory("a1", 0, {"n": 1})
    await store.append_memory("a1", 0, {"n": 2})
    entries = store.read_memory("a1")
    # Filter: only entries up to (but not including) the second one
    ts_cutoff = entries[0]["ts"]
    filtered = store.read_memory("a1", to_ts=ts_cutoff)
    assert len(filtered) == 1
    assert filtered[0]["message"]["n"] == 1


@pytest.mark.asyncio
async def test_append_memory_nonexistent_agent(store):
    with pytest.raises(ValueError, match="not found"):
        await store.append_memory("nonexistent", 0, {"x": 1})


@pytest.mark.asyncio
async def test_read_memory_nonexistent_file(store):
    store.create_agent(agent_id="a1")
    # No memory written yet, file doesn't exist
    assert store.read_memory("a1") == []


@pytest.mark.asyncio
async def test_memory_persists_as_jsonl(store, tmp_path):
    store.create_agent(agent_id="a1")
    await store.append_memory("a1", 0, {"k": "v"})
    path = tmp_path / ".fantastic" / "agents" / "a1" / "memory_long.jsonl"
    assert path.exists()
    lines = path.read_text().strip().split("\n")
    assert len(lines) == 1
    parsed = json.loads(lines[0])
    assert parsed["message"]["k"] == "v"


# ─── Agent creation with author_type / created_by ────────────────────────


def test_create_agent_author_type(store):
    agent = store.create_agent(agent_id="a1", author_type=1)
    assert agent["author_type"] == 1


def test_create_agent_created_by(store):
    agent = store.create_agent(agent_id="a1", created_by="user@test.com")
    assert agent["created_by"] == "user@test.com"


def test_create_agent_default_author_type(store):
    agent = store.create_agent(agent_id="a1")
    assert agent["author_type"] == 0


def test_create_agent_no_created_by_by_default(store):
    agent = store.create_agent(agent_id="a1")
    assert "created_by" not in agent


# ─── Engine memory integration ───────────────────────────────────────────


@pytest.fixture
async def engine(tmp_path):
    e = Engine(project_dir=str(tmp_path))
    await e.start()
    yield e
    await e.stop()


@pytest.mark.asyncio
async def test_execute_code_appends_memory(engine):
    engine.create_agent(agent_id="a1")
    await engine.execute_code("a1", "print('hello')")
    entries = engine.store.read_memory("a1")
    assert len(entries) == 1
    msg = entries[0]["message"]
    assert msg["kind"] == "execution"
    assert msg["exit_code"] == 0
    assert msg["duration_ms"] >= 0
    assert "source_hash" in msg
    assert msg["source_snippet"] == "print('hello')"


@pytest.mark.asyncio
async def test_execute_code_memory_records_author_type(engine):
    engine.create_agent(agent_id="a1")
    await engine.execute_code("a1", "x=1", author_type=2, triggered_by="test-runner")
    entries = engine.store.read_memory("a1")
    assert len(entries) == 1
    assert entries[0]["type"] == 2
    assert entries[0]["message"]["triggered_by"] == "test-runner"


@pytest.mark.asyncio
async def test_engine_create_agent_with_author(engine):
    agent = engine.create_agent(agent_id="a1", author_type=1, created_by="bot")
    assert agent["author_type"] == 1
    assert agent["created_by"] == "bot"


# ─── REST memory endpoints ──────────────────────────────────────────────


@pytest.fixture
def app_client(tmp_path, monkeypatch):
    monkeypatch.setenv("PROJECT_DIR", str(tmp_path))
    import importlib
    import core.server as server_mod

    importlib.reload(server_mod)
    client = TestClient(server_mod.app)
    with client:
        yield client


def _create_agent_rest(app_client):
    """Helper: create an agent via REST and return its ID."""
    r = app_client.post(
        "/api/call",
        json={
            "tool": "create_agent",
            "args": {},
        },
    )
    assert r.status_code == 200
    return r.json()["result"]["agent_id"]


def test_get_memory_empty(app_client):
    aid = _create_agent_rest(app_client)
    r = app_client.get(f"/api/agents/{aid}/memory")
    assert r.status_code == 200
    assert r.json() == []


def test_post_and_get_memory(app_client):
    aid = _create_agent_rest(app_client)
    r = app_client.post(
        f"/api/agents/{aid}/memory",
        json={
            "type": 0,
            "message": {"note": "test entry"},
        },
    )
    assert r.status_code == 200
    entry = r.json()
    assert entry["type"] == 0
    assert entry["message"]["note"] == "test entry"

    entries = app_client.get(f"/api/agents/{aid}/memory").json()
    assert len(entries) == 1
    assert entries[0]["message"]["note"] == "test entry"


def test_get_memory_404_for_missing_agent(app_client):
    r = app_client.get("/api/agents/nonexistent/memory")
    assert r.status_code == 404


def test_post_memory_404_for_missing_agent(app_client):
    r = app_client.post(
        "/api/agents/nonexistent/memory",
        json={
            "type": 0,
            "message": {"x": 1},
        },
    )
    assert r.status_code == 404


def test_get_memory_with_time_filter(app_client):
    aid = _create_agent_rest(app_client)
    app_client.post(
        f"/api/agents/{aid}/memory",
        json={
            "type": 0,
            "message": {"n": 1},
        },
    )
    app_client.post(
        f"/api/agents/{aid}/memory",
        json={
            "type": 0,
            "message": {"n": 2},
        },
    )
    all_entries = app_client.get(f"/api/agents/{aid}/memory").json()
    assert len(all_entries) == 2

    # Filter from second entry's timestamp
    ts = all_entries[1]["ts"]
    filtered = app_client.get(f"/api/agents/{aid}/memory?from={ts}").json()
    assert len(filtered) == 1
    assert filtered[0]["message"]["n"] == 2
