"""Tests for conversation tools."""

from core import conversation
from core.tools._conversation import (
    _conversation_say,
    _conversation_log,
    _core_chat_message,
)


def setup_function():
    conversation.clear()


async def test_say_broadcast():
    tr = await _conversation_say(who="actor_a", message="hello world")
    assert tr.data["who"] == "actor_a"
    assert tr.data["message"] == "hello world"
    assert len(tr.broadcast) == 1
    assert tr.broadcast[0]["type"] == "conversation_message"
    assert tr.broadcast[0]["entry"]["who"] == "actor_a"


async def test_say_missing_fields():
    tr = await _conversation_say(who="", message="hello")
    assert "error" in tr.data

    tr = await _conversation_say(who="user", message="")
    assert "error" in tr.data


async def test_log():
    conversation.say("user", "one")
    conversation.say("system", "two")
    tr = await _conversation_log(max_lines=100)
    assert tr.data["lines"] == 2
    assert len(tr.data["entries"]) == 2
    assert tr.data["entries"][0]["message"] == "one"


async def test_log_empty():
    tr = await _conversation_log()
    assert tr.data["lines"] == 0
    assert tr.data["entries"] == []


# ─── core_chat_message ──────────────────────────────────────────


async def test_core_chat_message_success():
    tr = await _core_chat_message(
        who="actor_a", message="http://localhost:8888/view/main"
    )
    assert tr.data["who"] == "actor_a"
    assert tr.data["message"] == "http://localhost:8888/view/main"
    assert len(tr.broadcast) == 1
    assert tr.broadcast[0]["type"] == "conversation_message"
    assert tr.broadcast[0]["entry"]["who"] == "actor_a"


async def test_core_chat_message_in_buffer():
    await _core_chat_message(who="actor_b", message="ready")
    entries = conversation.read()
    assert len(entries) == 1
    assert entries[0]["who"] == "actor_b"
    assert entries[0]["message"] == "ready"


async def test_core_chat_message_missing_who():
    tr = await _core_chat_message(who="", message="hello")
    assert "error" in tr.data


async def test_core_chat_message_missing_message():
    tr = await _core_chat_message(who="actor_a", message="")
    assert "error" in tr.data


async def test_core_chat_message_prints(capsys):
    await _core_chat_message(who="actor_a", message="hello world")
    captured = capsys.readouterr()
    assert "actor_a" in captured.out
    assert "hello world" in captured.out
