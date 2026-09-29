"""A bridge frame's qualified `caller` (`<guid>|<agent id>`) is checked on INGRESS,
before the kernel sees it. Every inbound call names its caller, and the guid must
be the ONE peer the leg is pinned to (`peer_guid`): a peer cannot pose as a third
kernel's agent (audit 2026-09-28). No pin, no inbound call."""

from __future__ import annotations

from io_bridge._engine import _check_caller, validate_meta

_PINNED = {"peer_guid": "peer-g"}


def test_a_caller_from_the_pinned_peer_passes():
    assert _check_caller(_PINNED, "peer-g|ai") is None


def test_a_caller_from_any_other_kernel_is_refused():
    out = _check_caller(_PINNED, "third-g|ai")
    assert out["reason"] == "unauthorized" and "pinned peer" in out["error"], out


def test_no_pin_serves_no_inbound_call():
    out = _check_caller({}, "peer-g|ai")
    assert out["reason"] == "unauthorized" and "peer_guid" in out["hint"], out


def test_a_missing_or_malformed_caller_is_refused():
    for bad in (None, "ai", "g|", "|ai", "a|b|c", 42):
        assert _check_caller(_PINNED, bad)["reason"] == "unauthorized", bad


def test_the_pin_is_validated_at_write():
    assert validate_meta({"peer_guid": "peer-g"}, {}) is None
    for bad in ("", "a|b", 7):
        assert validate_meta({"peer_guid": bad}, {}) is not None, bad
