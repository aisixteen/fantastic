"""REPL utilities — _parse_at, _coerce, _find_bundle_module.

These helpers live in `kernel/_modes.py` as module-private helpers
used by the REPL and argv parser. Tests reach in directly."""

from __future__ import annotations


from kernel._modes import _coerce, _parse_at

from kernel import _find_bundle_module


def test_coerce_bool():
    assert _coerce("true") is True
    assert _coerce("false") is False


def test_coerce_int():
    assert _coerce("42") == 42


def test_coerce_float():
    assert _coerce("1.5") == 1.5


def test_coerce_json_object():
    assert _coerce('{"a":1}') == {"a": 1}


def test_coerce_json_array():
    assert _coerce("[1,2,3]") == [1, 2, 3]


def test_coerce_string_passthrough():
    assert _coerce("hello") == "hello"


def test_parse_at_bare_id():
    assert _parse_at("@id") == ("id", {"type": "send", "text": ""})


def test_parse_at_single_word_verb():
    assert _parse_at("@id reflect") == ("id", {"type": "reflect"})


def test_parse_at_verb_with_kv():
    target, payload = _parse_at("@id verb a=1 b=hello")
    assert target == "id"
    assert payload == {"type": "verb", "a": 1, "b": "hello"}


def test_parse_at_free_text_send():
    target, payload = _parse_at("@id this is free text")
    assert target == "id"
    assert payload["type"] == "send"
    assert payload["text"] == "this is free text"


def test_find_bundle_module_resolves_via_entry_points():
    assert _find_bundle_module("cli") == "cli.tools"
    assert _find_bundle_module("terminal_backend") == "terminal_backend.tools"
    assert _find_bundle_module("ai_chat_webapp") == "ai_chat_webapp.tools"


def test_find_bundle_module_returns_none_for_missing():
    assert _find_bundle_module("definitely_not_a_bundle_xyz") is None
