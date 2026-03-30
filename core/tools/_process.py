"""Process control tools + agent_call."""

import asyncio
import logging
import time

from ..dispatch import ToolResult, register_dispatch, register_tool
from . import _fire_broadcasts
from . import _state

logger = logging.getLogger(__name__)


@register_dispatch("agent_call")
async def _agent_call(
    target_agent_id: str = "",
    message: str = "",
    from_agent_id: str = "",
) -> ToolResult:
    target = target_agent_id

    agent = _state._engine.get_agent(target)
    if agent is None:
        return ToolResult(data={"error": f"Agent {target} not found"})

    timestamp = time.time()

    delivered_to_process = False
    has_process = _state._process_runner and _state._process_runner.exists(target)
    logger.info(f"agent_call: target={target}, has_process={has_process}")
    if has_process:
        for ch in message:
            await _state._process_runner.write(target, ch)
            await asyncio.sleep(0.02)
        await asyncio.sleep(0.5)
        await _state._process_runner.write(target, "\r")
        logger.info(f"agent_call: typed {len(message)} chars + Enter to pty")
        delivered_to_process = True

    return ToolResult(
        data={
            "delivered": True,
            "delivered_to_process": delivered_to_process,
            "target_agent_id": target,
            "timestamp": timestamp,
        },
    )


@register_tool("agent_call")
async def agent_call(
    target_agent_id: str,
    message: str,
    from_agent_id: str = "",
) -> dict:
    """Send a message to another agent.

    Delivered to its process (if one exists). Use this for inter-agent communication.

    Args:
        target_agent_id: The agent to send the message to.
        message: The message content.
        from_agent_id: Optional — the sending agent's id.
    """
    tr = await _agent_call(
        target_agent_id=target_agent_id, message=message, from_agent_id=from_agent_id
    )
    await _fire_broadcasts(tr)
    return tr.data


@register_dispatch("process_output")
async def _process_output(agent_id: str = "", max_lines: int = 200) -> ToolResult:
    raw = _state._process_runner.get_scrollback(agent_id)
    if not raw:
        raw = _state._process_runner.load_scrollback_from_disk(agent_id)
    if not raw:
        return ToolResult(
            data={"error": f"Process {agent_id} not found or has no output"}
        )
    lines = raw.split("\n")
    if len(lines) > max_lines:
        lines = lines[-max_lines:]
    return ToolResult(data={"output": "\n".join(lines), "lines": len(lines)})


@register_dispatch("process_restart")
async def _process_restart(agent_id: str = "") -> ToolResult:
    if not agent_id:
        return ToolResult(data={"error": "agent_id is required"})
    try:
        await _state._process_runner.restart(agent_id)
    except ValueError as e:
        return ToolResult(data={"error": str(e)})
    return ToolResult(
        data={"agent_id": agent_id, "restarted": True},
        broadcast=[
            {"type": "process_closed", "agent_id": agent_id},
            {"type": "process_started", "agent_id": agent_id},
        ],
    )


@register_dispatch("process_signal")
async def _process_signal(agent_id: str = "", signal: int = 2) -> ToolResult:
    try:
        _state._process_runner.send_signal(agent_id, signal)
    except ValueError as e:
        return ToolResult(data={"error": str(e)})
    return ToolResult(data={"agent_id": agent_id, "signal": signal})
