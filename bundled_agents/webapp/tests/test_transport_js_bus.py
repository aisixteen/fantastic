"""Snapshot tests for the new transport.js features: binary handling + BroadcastChannel bus.

We can't run the JS in pytest, but we verify the source contains the
required symbols + invariants. If somebody removes them by mistake,
these break.
"""

from __future__ import annotations

from webapp._transport_js import TRANSPORT_JS


# ─── binary protocol ───


def test_sets_binary_type_arraybuffer():
    assert "ws.binaryType = 'arraybuffer'" in TRANSPORT_JS


def test_has_encode_and_decode_frame():
    assert "encodeFrame" in TRANSPORT_JS
    assert "decodeFrame" in TRANSPORT_JS


def test_has_binary_path_helpers():
    assert "findBinaryPath" in TRANSPORT_JS
    assert "_binary_path" in TRANSPORT_JS


def test_uses_textencoder_textdecoder():
    assert "TextEncoder" in TRANSPORT_JS
    assert "TextDecoder" in TRANSPORT_JS


def test_handles_arraybuffer_in_findpath():
    assert "ArrayBuffer" in TRANSPORT_JS


# ─── BroadcastChannel bus ───


def test_creates_named_broadcast_channel():
    assert "new BroadcastChannel('fantastic')" in TRANSPORT_JS


def test_bus_send_uses_postMessage():
    assert "bcast.postMessage" in TRANSPORT_JS


def test_bus_filters_by_target_id():
    assert "target_id !== agentId" in TRANSPORT_JS


def test_bus_skips_own_echoes():
    assert "source_id === agentId" in TRANSPORT_JS


def test_bus_exposed_on_returned_object():
    # The returned object must have a `bus` field with send/on/onAny/broadcast.
    assert "bus: bus" in TRANSPORT_JS or "bus: {" in TRANSPORT_JS
    assert "send: function" in TRANSPORT_JS
    assert "broadcast: function" in TRANSPORT_JS


def test_envelope_carries_source_id_and_target_id():
    assert "source_id: agentId" in TRANSPORT_JS
    assert "target_id: target_id" in TRANSPORT_JS
