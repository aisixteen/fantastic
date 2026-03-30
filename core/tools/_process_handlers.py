"""WS-only process handlers + agent_run."""

import logging
from typing import Any

from ..dispatch import ToolResult, register_dispatch
from . import _state
from ._instance_tracking import _instance_list_sync

logger = logging.getLogger(__name__)


@register_dispatch("get_state")
async def _get_state() -> ToolResult:
    """Get state including instances (for WS clients). VFX added via state hooks."""
    state = _state._engine.get_state()
    instances = _instance_list_sync()
    if instances:
        state["instances"] = instances
    return ToolResult(
        data=state,
        reply=[{"type": "state", "state": state}],
    )


@register_dispatch("agent_run")
async def _agent_run(agent_id: str = "", code: str = "") -> ToolResult:
    """Execute code on an agent (WS agent_run handler)."""
    try:
        result = await _state._engine.execute_code(agent_id, code)
        return ToolResult(
            data=result,
            broadcast=[
                {
                    "type": "agent_output",
                    "agent_id": agent_id,
                    "outputs": result["outputs"],
                    "success": result["success"],
                },
                {
                    "type": "agent_complete",
                    "agent_id": agent_id,
                    "final_code": code,
                    "outputs": result["outputs"],
                },
            ],
        )
    except Exception as e:
        return ToolResult(
            data={"error": str(e)},
            broadcast=[{"type": "error", "message": str(e)}],
        )


_SENTINEL = object()


@register_dispatch("process_attach")
async def _process_attach(
    agent_id: str = "",
    cols: int = 80,
    rows: int = 24,
    command: str | None = None,
    args: list[str] | None = None,
    welcome_command: Any = _SENTINEL,
    process_id: str = "",
) -> ToolResult:
    """Attach to or create a process for an agent (WS process_attach handler)."""
    aid = agent_id or process_id
    reply: list[dict] = []

    if _state._process_runner.exists(aid):
        await _state._process_runner.resize(aid, cols, rows)
        scrollback = _state._process_runner.get_scrollback(aid)
        if scrollback:
            reply.append(
                {
                    "type": "process_output",
                    "agent_id": aid,
                    "data": scrollback,
                }
            )
    else:
        # Load saved params from agent.json (if agent exists on disk from previous session)
        agent = _state._engine.store.get_agent(aid)
        saved_params = (agent or {}).get("process_params", {})

        # Use explicit WS args if provided, otherwise fall back to saved params
        effective_command = command or saved_params.get("command")
        effective_args = args or saved_params.get("args")

        if welcome_command is _SENTINEL:
            # Try saved params first, then global config
            saved_wc = saved_params.get("welcome_command")
            if saved_wc is not None:
                welcome_command = saved_wc
            else:
                config = _state._engine.store.get_config()
                welcome_command = config.get("welcome_command")

        # Check for disk scrollback to replay
        disk_scrollback = _state._process_runner.load_scrollback_from_disk(aid)
        if disk_scrollback:
            reply.append(
                {
                    "type": "process_output",
                    "agent_id": aid,
                    "data": disk_scrollback,
                }
            )
            # Seed in-memory buffer so subsequent browser refreshes get it
            _state._process_runner.seed_scrollback(aid, disk_scrollback)
            # Don't re-run welcome_command — user already saw it in previous session
            effective_welcome = None
        else:
            effective_welcome = welcome_command

        await _state._process_runner.create(
            aid,
            cols=cols,
            rows=rows,
            cwd=str(_state._engine.resolve_working_dir(aid)),
            command=effective_command,
            args=effective_args,
            welcome_command=effective_welcome,
        )

        # Persist process params to agent.json for future restarts
        resolved_wc = welcome_command  # save the full welcome_command regardless
        _state._engine.update_agent_meta(
            aid,
            process_params={
                "command": effective_command,
                "args": effective_args,
                "welcome_command": resolved_wc,
            },
        )

    reply.append({"type": "process_created", "agent_id": aid})
    return ToolResult(data={"agent_id": aid}, reply=reply)


@register_dispatch("process_create")
async def _process_create(**kwargs) -> ToolResult:
    # Map process_id to agent_id for backwards compat
    if "process_id" in kwargs and "agent_id" not in kwargs:
        kwargs["agent_id"] = kwargs.pop("process_id")
    return await _process_attach(**kwargs)


@register_dispatch("process_input")
async def _process_input(
    process_id: str = "", agent_id: str = "", data: str = ""
) -> ToolResult:
    aid = agent_id or process_id
    await _state._process_runner.write(aid, data)
    return ToolResult()


@register_dispatch("process_resize")
async def _process_resize(
    process_id: str = "", agent_id: str = "", cols: int = 80, rows: int = 24
) -> ToolResult:
    aid = agent_id or process_id
    await _state._process_runner.resize(aid, cols, rows)
    return ToolResult()


@register_dispatch("process_enter")
async def _process_enter(process_id: str = "", agent_id: str = "") -> ToolResult:
    aid = agent_id or process_id
    logger.info(f"process_enter: sending \\r to {aid}")
    await _state._process_runner.write(aid, "\r")
    return ToolResult()


@register_dispatch("process_close")
async def _process_close(process_id: str = "", agent_id: str = "") -> ToolResult:
    aid = agent_id or process_id
    _state._engine.unregister_server(aid)
    await _state._process_runner.close(aid)
    return ToolResult(
        data={"agent_id": aid},
        broadcast=[{"type": "process_closed", "agent_id": aid}],
    )
