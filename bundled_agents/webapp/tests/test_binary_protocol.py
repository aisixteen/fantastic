"""Binary protocol on the WS proxy — encode/decode round-trip."""

from __future__ import annotations

import json


from webapp._proxy import (
    _find_bytes_path,
    _set_path,
    decode_inbound,
    encode_outbound,
)


# ─── path helpers ───


def test_find_bytes_path_top_level():
    assert _find_bytes_path({"bytes": b"\x00\x01"}) == ("bytes", b"\x00\x01")


def test_find_bytes_path_nested():
    p, v = _find_bytes_path({"payload": {"pcm": b"raw"}})
    assert p == "payload.pcm"
    assert v == b"raw"


def test_find_bytes_path_in_list():
    p, v = _find_bytes_path({"items": [{"data": b"hi"}]})
    assert p == "items.0.data"
    assert v == b"hi"


def test_find_bytes_path_none_when_no_bytes():
    assert _find_bytes_path({"a": 1, "b": "string", "c": [1, 2, 3]}) is None


def test_set_path_nested():
    obj = {"a": {"b": {"c": "old"}}}
    _set_path(obj, "a.b.c", "new")
    assert obj["a"]["b"]["c"] == "new"


def test_set_path_in_list():
    obj = {"items": [None, None]}
    _set_path(obj, "items.1", "value")
    assert obj["items"][1] == "value"


# ─── encode/decode round-trips ───


def test_encode_text_when_no_bytes():
    env = {"type": "event", "payload": {"type": "x", "n": 42}}
    frame, is_bin = encode_outbound(env)
    assert is_bin is False
    assert json.loads(frame.decode("utf-8")) == env


def test_encode_binary_when_bytes_in_payload():
    body = b"\x00\x01\x02\x03"
    env = {"type": "event", "payload": {"type": "audio", "pcm": body}}
    frame, is_bin = encode_outbound(env)
    assert is_bin is True
    # Frame: [4-byte len][header JSON][body]
    head_len = int.from_bytes(frame[:4], "big")
    head = json.loads(frame[4 : 4 + head_len].decode("utf-8"))
    assert head["_binary_path"] == "payload.pcm"
    assert head["payload"]["pcm"] is None
    assert frame[4 + head_len :] == body


def test_decode_inbound_text():
    payload = {
        "type": "call",
        "target": "core",
        "payload": {"type": "reflect"},
        "id": "1",
    }
    out = decode_inbound(json.dumps(payload))
    assert out == payload


def test_decode_inbound_binary_round_trip():
    body = b"\xde\xad\xbe\xef"
    env = {"type": "event", "payload": {"type": "audio", "pcm": body}}
    frame, is_bin = encode_outbound(env)
    assert is_bin
    out = decode_inbound(frame)
    assert out["type"] == "event"
    assert out["payload"]["type"] == "audio"
    assert out["payload"]["pcm"] == body
    assert "_binary_path" not in out


def test_round_trip_large_bytes():
    body = bytes(range(256)) * 1000  # 256 KB
    env = {"type": "event", "payload": {"frame": body}}
    frame, is_bin = encode_outbound(env)
    assert is_bin
    out = decode_inbound(frame)
    assert out["payload"]["frame"] == body


def test_encoded_envelope_strips_bytes_value():
    """The JSON header MUST NOT contain the raw bytes (they're in the body)."""
    body = b"SECRET-NEVER-IN-JSON"
    env = {"type": "event", "payload": {"data": body}}
    frame, is_bin = encode_outbound(env)
    head_len = int.from_bytes(frame[:4], "big")
    head_str = frame[4 : 4 + head_len].decode("utf-8")
    assert b"SECRET-NEVER-IN-JSON".decode() not in head_str
