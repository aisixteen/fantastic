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
    head = json.dumps({"type": "reply", "_binary_path": "no.such.place"}).encode()
    with pytest.raises(ValueError):
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
    them. Off the wire they are refused like any other malformed JSON (parity
    2026-09-30: a `NaN` REST body python took and rust/swift refused)."""
    from io_bridge import loads

    for bad in ('{"x": NaN}', '{"x": Infinity}', '{"x": -Infinity}'):
        with pytest.raises(json.JSONDecodeError):
            loads(bad)
        with pytest.raises(ValueError, match="bad text frame"):
            decode_frame(bad)
    assert loads('{"x": 1.5, "big": 123456789012345678901234567890}') == {
        "x": 1.5,
        "big": 123456789012345678901234567890,
    }
