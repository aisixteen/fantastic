"""io_bridge codec — the ONE wire encoding, and what it does with a HOSTILE frame.

Every length in a binary frame is written by the peer. Before the bounds checks a
4-byte prefix claiming 4 GiB of header on a 3-byte frame sliced garbage and raised
`struct.error`/`JSONDecodeError` straight out of whichever read loop called
`decode_frame` — killing a connection over one bad frame. Now every malformed
frame leaves as ONE exception type (`ValueError`), so a caller catches one thing
and keeps its socket.
"""

from __future__ import annotations

import json
import struct

import pytest

from io_bridge import decode_frame, encode_frame


def test_round_trip_text_and_binary():
    wire, is_bin = encode_frame({"type": "call", "id": "1", "payload": {"x": 1}})
    assert is_bin is False
    assert decode_frame(wire.decode("utf-8")) == {
        "type": "call",
        "id": "1",
        "payload": {"x": 1},
    }
    wire, is_bin = encode_frame({"type": "reply", "data": {"bytes": b"\x00\xffhi"}})
    assert is_bin is True
    assert decode_frame(wire)["data"]["bytes"] == b"\x00\xffhi"


def test_short_binary_frame_raises_value_error():
    # The exact frame from the sweep: 3 bytes, shorter than the length prefix.
    with pytest.raises(ValueError):
        decode_frame(b"\x00\x00\x00")
    with pytest.raises(ValueError):
        decode_frame(b"")


def test_header_length_past_the_end_raises_value_error():
    with pytest.raises(ValueError):
        decode_frame(struct.pack(">I", 4096) + b"{}")
    with pytest.raises(ValueError):
        decode_frame(struct.pack(">I", 0xFFFFFFFF) + b"{}")


def test_bad_header_json_raises_value_error():
    head = b"not json"
    with pytest.raises(ValueError):
        decode_frame(struct.pack(">I", len(head)) + head + b"body")


def test_bogus_binary_path_raises_value_error():
    head = json.dumps({"type": "reply", "_binary_path": ["no", "such", "place"]})
    head = head.encode()
    with pytest.raises(ValueError, match="bad _binary_path"):
        decode_frame(struct.pack(">I", len(head)) + head + b"body")


def test_bad_text_frame_raises_value_error():
    with pytest.raises(ValueError):
        decode_frame("{not json")


def test_non_object_text_frame_decodes_without_raising():
    # A list/int is well-formed JSON — the codec hands it back as-is and the
    # CALLER decides it is not an envelope (web_ws answers `bad_arg`).
    assert decode_frame("[1]") == [1]


def test_non_finite_numbers_are_not_json():
    """`NaN`/`Infinity`/`-Infinity` are not JSON; python's `json.loads` alone takes
    them. Off the wire they are refused like any other malformed JSON."""
    from io_bridge import loads

    for bad in ('{"x": NaN}', '{"x": Infinity}', '{"x": -Infinity}'):
        with pytest.raises(json.JSONDecodeError):
            loads(bad)
        with pytest.raises(ValueError, match="bad text frame"):
            decode_frame(bad)
    assert loads('{"x": 1.5, "n": -9223372036854775808}') == {
        "x": 1.5,
        "n": -(2**63),
    }


def test_only_values_every_runtime_can_hold():
    """An integer outside int64, a number that overflows a
    double and a lone surrogate are refused at the wire — rust would
    hold a different value, or none."""
    from io_bridge import loads

    for bad in (
        '{"x": 9223372036854775808}',
        '{"x": -9223372036854775809}',
        '{"x": 1e400}',
        '{"s": "\\udcff"}',
        '{"\\ud800": 1}',
        '[["deep", "\\udfff"]]',
    ):
        with pytest.raises(json.JSONDecodeError):
            loads(bad)
    # the int64 edges and a PAIRED surrogate (one real character) are fine
    assert loads("[9223372036854775807, -9223372036854775808]") == [2**63 - 1, -(2**63)]
    assert loads('{"s": "\\ud83d\\ude00"}') == {"s": "\U0001f600"}


#: Deeper than python's json scanner can enter (it charges each `[`/`{` against
#: `sys.getrecursionlimit()`, 1000) — the depth no caller's stack can reach.
_DEEP = 100_000


def test_a_too_deep_document_is_a_json_decode_error():
    """A document nested past what the json scanner can enter is malformed input
    like any other: `loads` refuses it as a `JSONDecodeError` (it raised a bare
    `RecursionError`, which every caller's "not valid JSON" path missed — web_rest
    answered 500), and `decode_frame` as its one `ValueError`, text or binary."""
    from io_bridge import loads

    for deep in ("[" * _DEEP + "]" * _DEEP, '{"a":' * _DEEP + "1" + "}" * _DEEP):
        with pytest.raises(json.JSONDecodeError):
            loads(deep)
        with pytest.raises(json.JSONDecodeError):
            loads(deep.encode())
        with pytest.raises(ValueError, match="bad text frame"):
            decode_frame(deep)
        head = deep.encode()
        with pytest.raises(ValueError, match="bad binary frame header"):
            decode_frame(struct.pack(">I", len(head)) + head + b"body")


# ─── ONE binary body per frame ─────────────────────────────────────────────


def test_an_envelope_with_two_bodies_is_refused_naming_both_paths():
    from io_bridge import FrameError

    env = {"type": "reply", "id": "1", "data": {"a": b"x", "more": [{"b": b"y"}]}}
    with pytest.raises(FrameError) as e:
        encode_frame(env)
    assert '["data", "a"]' in str(e.value) and '["data", "more", 0, "b"]' in str(
        e.value
    )
    # never the old silent `default=str` rendering of the second body
    assert "b'y'" not in str(e.value)


def test_sendable_answers_a_reply_or_an_event_it_cannot_carry_with_the_refusal():
    from io_bridge import sendable

    one = {"type": "reply", "id": "1", "data": {"chunk": b"x"}}
    assert sendable(one) is one, "one body: sent as it is"
    two = {"type": "reply", "id": "7", "data": {"a": b"x", "b": b"y"}}
    out = sendable(two)
    assert out["type"] == "reply" and out["id"] == "7"
    assert out["data"]["reason"] == "transport_error"
    assert "ONE binary body" in out["data"]["error"]
    wire, binary = encode_frame(out)
    assert binary is False and json.loads(wire)["data"]["reason"] == "transport_error"
    ev = sendable({"type": "event", "src": "s", "payload": {"a": b"x", "b": b"y"}})
    assert ev["src"] == "s" and ev["payload"]["reason"] == "transport_error"
    conn = sendable({"op": "reply", "rid": "3", "data": [b"x", b"y"]})
    assert conn["rid"] == "3" and conn["data"]["reason"] == "transport_error"


def test_a_call_with_two_bodies_is_refused_at_its_sender():
    from io_bridge import FrameError, sendable

    with pytest.raises(FrameError):
        sendable(
            {
                "type": "call",
                "id": "1",
                "target": "t",
                "payload": {"a": b"x", "b": b"y"},
            }
        )
