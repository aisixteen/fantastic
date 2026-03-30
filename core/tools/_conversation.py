"""Conversation read/write + core_chat_message tool."""

from .. import conversation
from ..dispatch import ToolResult, register_dispatch, register_tool
from . import _fire_broadcasts


@register_dispatch("conversation_log")
async def _conversation_log(max_lines: int = 100) -> ToolResult:
    """Read recent conversation entries."""
    entries = conversation.read(max_lines=max_lines)
    return ToolResult(data={"lines": len(entries), "entries": entries})


@register_dispatch("conversation_say")
async def _conversation_say(who: str = "", message: str = "") -> ToolResult:
    """Post a message to the conversation buffer."""
    if not who or not message:
        return ToolResult(data={"error": "who and message are required"})
    entry = conversation.say(who, message)
    return ToolResult(
        data=entry,
        broadcast=[{"type": "conversation_message", "entry": entry}],
    )


@register_dispatch("core_chat_message")
async def _core_chat_message(who: str = "", message: str = "") -> ToolResult:
    """Post a message to the conversation (buffer + CLI + WS broadcast)."""
    if not who or not message:
        return ToolResult(data={"error": "who and message are required"})
    entry = conversation.say(who, message)
    print(conversation.format_entry(entry))
    return ToolResult(
        data=entry,
        broadcast=[{"type": "conversation_message", "entry": entry}],
    )


@register_tool("core_chat_message")
async def core_chat_message(who: str, message: str) -> str:
    """Post a message to the shared conversation visible in CLI and all connected clients.

    Args:
        who: Actor name (e.g. "agent", "system").
        message: The message text.
    """
    tr = await _core_chat_message(who, message)
    await _fire_broadcasts(tr)
    return tr.data
