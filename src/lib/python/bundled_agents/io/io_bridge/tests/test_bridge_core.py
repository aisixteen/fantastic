"""bridge_core — direct unit tests of the shared engine primitives (the
transport-agnostic surface; full forward/round-trip coverage lives in the
ws_bridge + relay_connector suites that drive it through MemoryTransport).

The authorization rules the engine consults are tested in the `io_bridge` suite."""

from __future__ import annotations

from io_bridge import _engine as core


async def _noop_build(kind, rec, kernel, st):  # pragma: no cover - not invoked here
    raise AssertionError("build_transport should not be called in these tests")


def _verbs():
    return core.make_verbs(
        build_transport=_noop_build,
        sentence="test bridge",
        reflect_fields=lambda rec, st: {"x": 1},
        default_kind="ws",
    )


def test_make_verbs_builds_the_six_verbs():
    verbs = _verbs()
    assert set(verbs) == {
        "reflect",
        "boot",
        "reconnect",
        "forward",
        "watch_remote",
        "unwatch_remote",
    }


def test_next_corr_namespaces_by_agent_id():
    core._bridges.clear()
    sa = core._state("a")
    sb = core._state("b")
    assert core._next_corr("a", sa) == "a:1"
    assert core._next_corr("a", sa) == "a:2"
    assert core._next_corr("b", sb) == "b:1"  # independent counter per agent
    core._bridges.clear()


async def test_dispatch_unknown_type_errors():
    verbs = _verbs()
    out = await core.dispatch(verbs, "id", {"type": "nope"}, kernel=None)
    assert "unknown type" in out["error"]


async def test_forward_before_boot_reports_not_connected():
    core._bridges.clear()
    verbs = _verbs()
    out = await verbs["forward"](
        "id", {"type": "forward", "target": "t", "payload": {}}, kernel=None
    )
    assert "not connected" in out["error"]
    core._bridges.clear()
