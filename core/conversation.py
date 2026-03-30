"""Conversation ring buffer + color formatting.

The conversation buffer ({who}:{message}) is the universal backbone.
Color-coded: core=magenta, user=green, agent/bundle=cyan.
"""

from collections import deque
from datetime import datetime, timezone

CONVERSATION_BUFFER_SIZE = 1000

# ANSI color codes
CORE_COLOR = "\033[35m"  # magenta
USER_COLOR = "\033[32m"  # green
AGENT_COLOR = "\033[36m"  # cyan
RESET = "\033[0m"

CORE_ACTORS = {"fantastic", "system"}

# Padding width for name column alignment
NAME_PAD = 10

_buffer: deque[dict] = deque(maxlen=CONVERSATION_BUFFER_SIZE)


def say(who: str, message: str) -> dict:
    """Append {ts, who, message} to the conversation buffer."""
    entry = {
        "ts": datetime.now(timezone.utc).isoformat(),
        "who": who,
        "message": message,
    }
    _buffer.append(entry)
    return entry


def read(max_lines: int = 100) -> list[dict]:
    """Return the last N entries from the buffer."""
    n = min(max_lines, len(_buffer))
    return list(_buffer)[-n:]


def clear():
    """Clear the conversation buffer."""
    _buffer.clear()


def actor_color(who: str) -> str:
    """Return ANSI color for a given actor."""
    if who.lower() in CORE_ACTORS:
        return CORE_COLOR
    if who.lower() == "user":
        return USER_COLOR
    return AGENT_COLOR


def format_entry(entry: dict) -> str:
    """Format a conversation entry with padded name: {color}{who:<pad}{reset} : {message}"""
    color = actor_color(entry["who"])
    name = entry["who"].ljust(NAME_PAD)
    return f"{color}{name}{RESET} : {entry['message']}"
