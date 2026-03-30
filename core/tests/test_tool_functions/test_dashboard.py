"""Tests for dashboard bundle on_add hook."""

from core.agent_store import AgentStore


def test_dashboard_on_add(tmp_path):
    """on_add creates a dashboard agent with bundle='dashboard' and correct display_name."""
    from bundled_agents.dashboard.tools import on_add

    on_add(str(tmp_path), name="ops")

    store = AgentStore(tmp_path)
    store.init()
    agents = store.list_agents()
    dashboards = [a for a in agents if a.get("bundle") == "dashboard"]
    assert len(dashboards) == 1
    assert dashboards[0]["display_name"] == "ops"


def test_dashboard_on_add_duplicate(tmp_path):
    """Calling on_add twice with the same name creates only one agent."""
    from bundled_agents.dashboard.tools import on_add

    on_add(str(tmp_path), name="ops")
    on_add(str(tmp_path), name="ops")

    store = AgentStore(tmp_path)
    store.init()
    dashboards = [a for a in store.list_agents() if a.get("bundle") == "dashboard"]
    assert len(dashboards) == 1


def test_dashboard_on_add_multiple(tmp_path):
    """Different names can coexist."""
    from bundled_agents.dashboard.tools import on_add

    on_add(str(tmp_path), name="ops")
    on_add(str(tmp_path), name="debug")

    store = AgentStore(tmp_path)
    store.init()
    dashboards = [a for a in store.list_agents() if a.get("bundle") == "dashboard"]
    assert len(dashboards) == 2
    names = {d["display_name"] for d in dashboards}
    assert names == {"ops", "debug"}
