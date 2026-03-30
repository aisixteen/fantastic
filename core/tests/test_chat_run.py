"""Tests for core.chat_run — @chat_run decorator and discovery."""

import types

from core.chat_run import chat_run, find_chat_run


def test_chat_run_sets_attribute():
    @chat_run
    async def run(ask, say):
        pass

    assert run._chat_run is True


def test_find_chat_run_finds_decorated():
    mod = types.ModuleType("test")

    @chat_run
    async def run(ask, say):
        pass

    mod.run = run
    assert find_chat_run(mod) is run


def test_find_chat_run_returns_none():
    mod = types.ModuleType("test")
    mod.run = lambda: None  # no @chat_run
    assert find_chat_run(mod) is None
