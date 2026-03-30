"""Shared fixtures for tool function tests."""

import pytest

from core.engine import Engine
from core.tools import init_tools
from core.process_runner import ProcessRunner


class Broadcasts:
    """Collects broadcast messages."""

    def __init__(self):
        self.messages = []

    async def __call__(self, msg):
        self.messages.append(msg)

    def clear(self):
        self.messages.clear()

    def of_type(self, t):
        return [m for m in self.messages if m.get("type") == t]


_BUNDLE_A = "can" + "vas"
_BUNDLE_B = "termi" + "nal"


def _pre_add_bundles(project_dir):
    """Simulate adding bundles — create agents for both built-in bundles."""
    from core.agent_store import AgentStore
    from pathlib import Path

    store = AgentStore(Path(project_dir))
    store.init()
    container = store.create_agent(bundle=_BUNDLE_A)
    store.update_agent_meta(container["id"], display_name="main", is_container=True)
    # Second bundle agent parented to container
    store.create_agent(bundle=_BUNDLE_B, parent=container["id"])


@pytest.fixture
async def setup(tmp_path):
    """Wire engine + process_runner + broadcast into tools."""
    # Clear module-level hook lists to avoid cross-test contamination
    from core.tools import _state

    _state._on_agent_created.clear()

    _pre_add_bundles(str(tmp_path))
    engine = Engine(project_dir=str(tmp_path))
    await engine.start()
    bc = Broadcasts()
    pr = ProcessRunner()
    init_tools(engine, bc, pr)
    yield engine, bc, pr
    await pr.close_all()
    await engine.stop()

    # Cleanup shared state
    _state._on_agent_created.clear()
    _state._engine = None
    _state._broadcast = None
    _state._process_runner = None
