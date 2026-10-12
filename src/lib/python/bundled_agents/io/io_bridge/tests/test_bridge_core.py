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


def test_next_corr_namespaces_by_agent_id_and_boot():
    """`<bridge id>-<boot nonce>-<n>`: the id keeps bridges apart, the nonce keeps
    BOOTS apart — the counter restarts at every boot, and a frame id names the
    execution on the receiver (audit IO-5)."""
    core._bridges.clear()
    sa = core._state("a")
    sb = core._state("b")
    sa.corr_prefix, sb.corr_prefix = (
        core._new_corr_prefix("a"),
        core._new_corr_prefix("b"),
    )
    assert core._next_corr(sa) == f"{sa.corr_prefix}-1"
    assert core._next_corr(sa) == f"{sa.corr_prefix}-2"
    assert core._next_corr(sb) == f"{sb.corr_prefix}-1"  # independent per agent
    assert sa.corr_prefix.startswith("a-") and len(sa.corr_prefix) == len("a-") + 12
    assert ":" not in sa.corr_prefix, "the kernel-minted namespace stays out"
    reboot = core._new_corr_prefix("a")
    assert reboot != sa.corr_prefix, "every boot mints its own nonce"
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
