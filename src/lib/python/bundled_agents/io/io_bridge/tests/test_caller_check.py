"""A bridge frame's qualified `caller` (`<guid>|<agent id>`) is checked on INGRESS,
before the kernel sees it: malformed or claiming THIS kernel's guid ⇒ refused."""

from __future__ import annotations

from types import SimpleNamespace

from io_bridge._engine import _check_caller


def _kernel(guid: str):
    return SimpleNamespace(
        ctx=SimpleNamespace(root=SimpleNamespace(record={"guid": guid}))
    )


def test_a_remote_callers_qualified_address_passes():
    assert _check_caller(_kernel("local-g"), "remote-g|ai") is None


def test_no_caller_is_not_a_claim():
    assert _check_caller(_kernel("local-g"), None) is None


def test_a_remote_claiming_this_kernels_guid_is_refused():
    out = _check_caller(_kernel("local-g"), "local-g|ai")
    assert out["reason"] == "unauthorized" and "THIS kernel's guid" in out["error"]


def test_a_malformed_caller_is_refused():
    for bad in ("ai", "g|", "|ai", "a|b|c", 42):
        assert _check_caller(_kernel("local-g"), bad)["reason"] == "unauthorized", bad
