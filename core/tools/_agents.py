"""Agent tools — agent CRUD, execute_python, post_output, refresh, state."""

import logging

from ..dispatch import ToolResult, register_dispatch, register_tool

from . import _fire_broadcasts, _format_outputs
from . import _state

logger = logging.getLogger(__name__)


@register_dispatch("execute_python")
async def _execute_python(code: str, agent_id: str = "") -> ToolResult:
    if not agent_id:
        return ToolResult(data={"error": "agent_id is required"})
    wd = _state._engine.resolve_working_dir(agent_id)
    result = await _state._engine.runner.execute(agent_id, code, cwd=str(wd))
    text = _format_outputs(result["outputs"])
    if not result["success"]:
        return ToolResult(data={"error": text, "raw": result})
    return ToolResult(data={"text": text or "(no output)", "raw": result})


@register_tool("execute_python")
async def execute_python(code: str, agent_id: str = "") -> str:
    """Execute Python code via subprocess. Stateless — each call is independent.

    Args:
        code: Python code to execute.
        agent_id: Agent to associate the execution with. Required.
    """
    tr = await _execute_python(code, agent_id)
    if "error" in tr.data:
        raw = tr.data.get("raw")
        if raw:
            return f"[ERROR]\n{tr.data['error']}"
        return f"[ERROR] {tr.data['error']}"
    return tr.data["text"]


@register_dispatch("create_agent")
async def _create_agent(
    agent_id: str | None = None,
    template: str = "",
    url: str = "",
    html_content: str = "",
    options: dict | None = None,
    parent: str = "",
    author_type: int = 0,
    created_by: str | None = None,
) -> ToolResult:
    bundle = template if template else None

    agent = _state._engine.create_agent(
        agent_id=agent_id,
        bundle=bundle,
        parent=parent or None,
        url=url or None,
        html_content=html_content or None,
        author_type=author_type,
        created_by=created_by,
    )

    try:
        # Auto-set has_iframe for agents with HTML content
        if html_content or url:
            _state._engine.update_agent_meta(agent["id"], has_iframe=True)
            agent["has_iframe"] = True

        # Apply options BEFORE hooks so hooks can see layout set by caller
        if options:
            _state._engine.update_agent_meta(agent["id"], **options)
            agent.update(options)

        # Notify creation hooks (e.g. auto-parenting, default layout)
        for hook in _state._on_agent_created:
            hook(agent["id"], agent)
    except Exception:
        # Rollback: remove partially created agent
        try:
            _state._engine.store.delete_agent(agent["id"])
        except Exception:
            pass
        raise

    return ToolResult(
        data=agent,
        broadcast=[
            {"type": "agent_created", "agent": agent},
        ],
    )


@register_tool("create_agent")
async def create_agent(
    template: str = "",
    url: str = "",
    html_content: str = "",
    options: dict | None = None,
    parent: str = "",
) -> dict:
    """Create a new agent.

    Args:
        template: Agent bundle name (e.g. "html"). Empty = no bundle.
        url: URL to display in iframe.
        html_content: HTML content to render. Supports full HTML.
        options: Optional properties to set on creation (e.g. {"x": 100, "y": 200, "width": 800, "height": 600}).
        parent: Parent agent ID (container). Empty = auto-assign.

    Returns the created agent's id, type, and position.
    """
    tr = await _create_agent(
        template=template,
        url=url,
        html_content=html_content,
        options=options,
        parent=parent,
    )
    await _fire_broadcasts(tr)
    agent = tr.data
    result = {
        "agent_id": agent["id"],
        "bundle": agent.get("bundle", ""),
        "parent": agent.get("parent", ""),
    }
    for key in ("x", "y", "z", "width", "height", "rotation"):
        if key in agent:
            result[key] = agent[key]
    if agent.get("url"):
        result["url"] = agent["url"]
    return result


@register_dispatch("list_agents")
async def _list_agents(parent: str = "") -> ToolResult:
    state = _state._engine.get_state()
    result = []
    for a in state.get("agents", []):
        if parent and a.get("parent") != parent:
            continue
        entry = {
            "agent_id": a["id"],
            "display_name": a.get("display_name", ""),
            "bundle": a.get("bundle", ""),
            "parent": a.get("parent", ""),
            "source": a.get("source", ""),
            "delete_lock": a.get("delete_lock", False),
        }
        for key in ("x", "y", "z", "width", "height", "rotation"):
            if key in a:
                entry[key] = a[key]
        if a.get("url"):
            entry["url"] = a["url"]
        result.append(entry)
    return ToolResult(data=result)


@register_tool("list_agents")
async def list_agents(parent: str = "") -> list[dict]:
    """List agents. Filter by parent to see only one container's agents.

    Args:
        parent: Parent agent ID to filter by. Empty = all agents.
    """
    tr = await _list_agents(parent=parent)
    return tr.data


@register_dispatch("read_agent")
async def _read_agent(agent_id: str = "") -> ToolResult:
    agent = _state._engine.get_agent(agent_id)
    if agent is None:
        return ToolResult(data={"error": f"Agent {agent_id} not found"})
    result = {
        "agent_id": agent["id"],
        "display_name": agent.get("display_name", ""),
        "bundle": agent.get("bundle", ""),
        "parent": agent.get("parent", ""),
        "source": agent.get("source", ""),
        "output_html": agent.get("output_html", ""),
        "delete_lock": agent.get("delete_lock", False),
    }
    for key in ("x", "y", "z", "width", "height", "rotation"):
        if key in agent:
            result[key] = agent[key]
    if agent.get("url"):
        result["url"] = agent["url"]
    return ToolResult(data=result)


@register_tool("read_agent")
async def read_agent(agent_id: str) -> dict:
    """Read an agent's source code, outputs, and metadata.

    Args:
        agent_id: The agent identifier.
    """
    tr = await _read_agent(agent_id)
    return tr.data


def _collect_descendants(parent_id: str) -> list[str]:
    """Collect all descendant agent IDs depth-first."""
    children = _state._engine.store.list_children(parent_id)
    result = []
    for child in children:
        result.extend(_collect_descendants(child["id"]))
        result.append(child["id"])
    return result


@register_dispatch("delete_agent")
async def _delete_agent(agent_id: str = "") -> ToolResult:
    agent = _state._engine.get_agent(agent_id)
    if not agent:
        return ToolResult(data={"error": f"Cannot delete agent {agent_id} (not found)"})
    if agent.get("delete_lock"):
        return ToolResult(
            data={"error": f"Agent {agent_id} is delete-locked. Cannot delete."}
        )

    # Cascade: collect all descendants depth-first, then delete root
    all_ids = _collect_descendants(agent_id) + [agent_id]

    broadcasts = []
    for aid in all_ids:
        _state._engine.unregister_server(aid)
        if _state._process_runner:
            await _state._process_runner.close(aid)
        _state._engine.delete_agent(aid)
        broadcasts.append({"type": "agent_deleted", "agent_id": aid})

    return ToolResult(
        data={"agent_id": agent_id, "deleted": True, "count": len(all_ids)},
        broadcast=broadcasts,
    )


@register_tool("delete_agent")
async def delete_agent(agent_id: str) -> str:
    """Delete an agent.

    Removes the agent, closes its process, and unregisters any server it owns.

    Args:
        agent_id: The agent to delete.
    """
    tr = await _delete_agent(agent_id)
    if "error" in tr.data:
        return f"[ERROR] {tr.data['error']}"
    await _fire_broadcasts(tr)
    return f"Agent {agent_id} deleted"


@register_dispatch("rename_agent")
async def _rename_agent(agent_id: str = "", display_name: str = "") -> ToolResult:
    if not _state._engine.update_agent_meta(agent_id, display_name=display_name):
        return ToolResult(data={"error": f"Agent {agent_id} not found"})
    return ToolResult(
        data={"agent_id": agent_id, "display_name": display_name},
        broadcast=[
            {
                "type": "agent_updated",
                "agent_id": agent_id,
                "display_name": display_name,
            },
        ],
    )


@register_dispatch("update_agent")
async def _update_agent(agent_id: str = "", options: dict | None = None) -> ToolResult:
    if not options:
        return ToolResult(data={"error": "No options provided"})
    if not _state._engine.update_agent_meta(agent_id, **options):
        return ToolResult(data={"error": f"Agent {agent_id} not found"})
    return ToolResult(
        data={"agent_id": agent_id, **options},
        broadcast=[
            {"type": "agent_updated", "agent_id": agent_id, **options},
        ],
    )


@register_dispatch("post_output")
async def _post_output(agent_id: str = "", html: str = "") -> ToolResult:
    if len(html) > 512 * 1024:
        logger.warning(
            f"post_output: payload is {len(html) // 1024}KB for {agent_id}. "
            f"Use content_alias_file() for large assets instead of inlining."
        )
    try:
        await _state._engine.post_output(agent_id, html)
        return ToolResult(data={"posted": True, "agent_id": agent_id})
    except ValueError as e:
        return ToolResult(data={"error": str(e)})


@register_dispatch("refresh_agent")
async def _refresh_agent(agent_id: str = "") -> ToolResult:
    agent = _state._engine.get_agent(agent_id)
    if agent is None:
        return ToolResult(data={"error": f"Agent {agent_id} not found"})

    # If agent has a running process, restart it
    if _state._process_runner and _state._process_runner.exists(agent_id):
        await _state._process_runner.restart(agent_id)
        return ToolResult(
            data={
                "agent_id": agent_id,
                "refreshed": True,
                "action": "process_restarted",
            },
            broadcast=[
                {"type": "process_closed", "agent_id": agent_id},
                {"type": "process_started", "agent_id": agent_id},
            ],
        )

    return ToolResult(
        data={"agent_id": agent_id, "refreshed": True, "action": "agent_refresh"},
        broadcast=[
            {"type": "agent_refresh", "agent_id": agent_id},
        ],
    )


@register_dispatch("get_full_state")
async def _get_full_state(scope: str = "") -> ToolResult:
    state = _state._engine.get_state()
    if scope:
        # Find container agent by display_name, filter to it + children
        container = None
        for a in state.get("agents", []):
            if a.get("is_container") and a.get("display_name") == scope:
                container = a
                break
        if container:
            cid = container["id"]
            state["agents"] = [
                a for a in state["agents"] if a["id"] == cid or a.get("parent") == cid
            ]
    return ToolResult(data=state)


@register_tool("get_state")
async def get_state(scope: str = "") -> dict:
    """Get full state. Filter by scope (container name) to see only that container and its children.

    Args:
        scope: Container display name to filter by. Empty = full state.
    """
    tr = await _get_full_state(scope=scope)
    return tr.data
