"""Tests for core.conversation — ring buffer + color formatting."""

from core.conversation import (
    say,
    read,
    clear,
    format_entry,
    actor_color,
    CONVERSATION_BUFFER_SIZE,
    NAME_PAD,
    CORE_COLOR,
    USER_COLOR,
    AGENT_COLOR,
    RESET,
    CORE_ACTORS,
    _buffer,
)


def setup_function():
    """Clear buffer before each test."""
    clear()


def test_say():
    entry = say("user", "hello")
    assert entry["who"] == "user"
    assert entry["message"] == "hello"
    assert "ts" in entry


def test_read():
    say("user", "one")
    say("system", "two")
    entries = read()
    assert len(entries) == 2
    assert entries[0]["message"] == "one"
    assert entries[1]["message"] == "two"


def test_read_max_lines():
    for i in range(10):
        say("user", f"msg{i}")
    entries = read(max_lines=3)
    assert len(entries) == 3
    assert entries[0]["message"] == "msg7"
    assert entries[2]["message"] == "msg9"


def test_eviction():
    # Fill beyond buffer size
    for i in range(CONVERSATION_BUFFER_SIZE + 50):
        say("user", f"msg{i}")
    assert len(_buffer) == CONVERSATION_BUFFER_SIZE
    entries = read(max_lines=CONVERSATION_BUFFER_SIZE)
    assert entries[0]["message"] == "msg50"


def test_clear():
    say("user", "hello")
    assert len(read()) == 1
    clear()
    assert len(read()) == 0


def test_format_entry_core_color():
    entry = say("fantastic", "started")
    formatted = format_entry(entry)
    padded = "fantastic".ljust(NAME_PAD)
    assert f"{CORE_COLOR}{padded}{RESET} : started" == formatted


def test_format_entry_user_color():
    entry = say("user", "hello")
    formatted = format_entry(entry)
    padded = "user".ljust(NAME_PAD)
    assert f"{USER_COLOR}{padded}{RESET} : hello" == formatted


def test_format_entry_agent_color():
    entry = say("actor_a", "registered")
    formatted = format_entry(entry)
    padded = "actor_a".ljust(NAME_PAD)
    assert f"{AGENT_COLOR}{padded}{RESET} : registered" == formatted


def test_actor_color_categories():
    # Core actors → magenta
    for actor in CORE_ACTORS:
        assert actor_color(actor) == CORE_COLOR
    # User → green
    assert actor_color("user") == USER_COLOR
    assert actor_color("User") == USER_COLOR  # case insensitive
    # Everything else → cyan
    assert actor_color("actor_a") == AGENT_COLOR
    assert actor_color("actor_b") == AGENT_COLOR
    assert actor_color("mybundle") == AGENT_COLOR
