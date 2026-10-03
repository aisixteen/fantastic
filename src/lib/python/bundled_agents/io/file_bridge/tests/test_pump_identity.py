"""The PUMP runs both ends AS ITS CALLER — the bridge is no deputy.

Audit AV-7 (2026-10-03): `pump` sent its `read_stream` and `write_stream` from
inside its own dispatch, so the far ends judged THE BRIDGE, never the caller. A
client allowed only `pump` on F copied a private G's file into F and read it
there, and the reverse pump overwrote G. Now both ends run as the pump's caller:
the caller needs its own grants on the source and on the sink (the operator, as
ever, is ungated).
"""

from __future__ import annotations

from kernel import sender_context

PRIVATE = b"PRIVATE-G-DATA"


async def _op(k, target, payload, as_=None) -> dict:
    item = [{"target": target, "payload": payload}]
    if as_ is None:
        return (await k.send(item))[0]
    with sender_context(as_):
        return (await k.send(item))[0]


async def _bridge(k, id, **meta):
    out = await _op(
        k,
        k.id,
        {
            "type": "create_agent",
            "parent_id": k.id,
            "handler_module": "file_bridge.tools",
            "id": id,
            "root": id,
            "allow": ["."],
            "ingress_rule": "allow_all_ingress",
            **meta,
        },
    )
    assert out["result"].get("id") == id, out


async def _wire(k):
    """G is private to F; F answers C's `pump` and `read`; C reaches nothing else."""
    await _bridge(k, "g", callable_by={"f": ["read_stream", "write_stream"]})
    await _bridge(
        k,
        "f",
        can_call={"g": ["read_stream", "write_stream"]},
        callable_by={"c": ["pump", "read", "read_stream", "write_stream"]},
    )
    k.create(None, id="c", can_call={"f": "*"})
    w = await _op(
        k,
        "g",
        {
            "type": "write_stream",
            "path": "secret.txt",
            "bytes": PRIVATE,
            "truncate": True,
        },
    )
    assert w["result"].get("written") == len(PRIVATE), w


async def _pump(k, as_, **args):
    return (await _op(k, "f", {"type": "pump", **args}, as_=as_))["result"]


async def test_a_pump_caller_cannot_read_an_end_it_cannot_reach(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    k = seeded_kernel
    await _wire(k)
    direct = await _op(k, "g", {"type": "read_stream", "path": "secret.txt"}, as_="c")
    assert direct["result"].get("reason") == "unauthorized", direct
    out = await _pump(
        k, "c", source="g", source_path="secret.txt", sink_path="loot.txt"
    )
    assert out["reason"] == "transport_error", out
    assert out["error"].startswith("pump: read from 'g' failed: {"), out
    assert "c may not call 'read_stream' on g" in out["error"], out
    assert not (tmp_path / "f" / "loot.txt").exists(), "nothing was copied"


async def test_a_pump_caller_cannot_write_an_end_it_cannot_reach(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    k = seeded_kernel
    await _wire(k)
    w = await _op(
        k,
        "f",
        {
            "type": "write_stream",
            "path": "mine.txt",
            "bytes": b"OVERWRITTEN",
            "truncate": True,
        },
    )
    assert w["status"] == "completed", w
    out = await _pump(k, "c", source_path="mine.txt", sink="g", sink_path="secret.txt")
    assert out["reason"] == "transport_error", out
    assert out["error"].startswith("pump: write to 'g' failed: {"), out
    assert (tmp_path / "g" / "secret.txt").read_bytes() == PRIVATE


async def test_a_pump_runs_with_the_callers_own_grants(
    seeded_kernel, tmp_path, monkeypatch
):
    """Granted on both ends ITSELF, the caller pumps — the bridge's own reach is
    not what lets it."""
    monkeypatch.chdir(tmp_path)
    k = seeded_kernel
    await _wire(k)
    k.update("c", can_call={"f": "*", "g": ["read_stream"]})
    cb = dict(k.ctx.agents["g"]._meta.get("callable_by") or {})
    k.update("g", callable_by={**cb, "c": ["read_stream"]})
    out = await _pump(
        k, "c", source="g", source_path="secret.txt", sink_path="copy.txt"
    )
    assert out == {
        "source": "secret.txt",
        "sink": "copy.txt",
        "bytes": len(PRIVATE),
        "chunks": 1,
    }, out
    assert (tmp_path / "f" / "copy.txt").read_bytes() == PRIVATE
