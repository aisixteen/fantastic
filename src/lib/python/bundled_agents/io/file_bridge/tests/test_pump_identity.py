"""The PUMP runs both ends AS THE FILE_BRIDGE itself.

Identity comes only from dispatch: `pump` sends its `read_stream` and
`write_stream` from inside its own dispatch, so both far ends judge THE BRIDGE —
its `can_call` must reach them and their `callable_by` must accept it. A caller's
own grants on the ends play no part: granting `pump` on a bridge lends exactly the
reach that bridge has.
"""

from __future__ import annotations

from _testkit import as_agent

DATA = b"G-DATA"


async def _op(k, target, payload, as_=None) -> dict:
    item = [{"target": target, "payload": payload}]
    if as_ is None:
        return (await k.send(item))[0]
    with as_agent(k, as_):
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


async def _wire(k, *, f_reaches_g: bool):
    """G holds a file; C may call F's `pump` and `read`; F reaches G only when
    `f_reaches_g` — both halves of the F→G edge."""
    g_cb = {"f": ["read_stream", "write_stream"]} if f_reaches_g else {}
    f_cc = {"g": ["read_stream", "write_stream"]} if f_reaches_g else {}
    await _bridge(k, "g", callable_by=g_cb)
    await _bridge(k, "f", can_call=f_cc, callable_by={"c": ["pump", "read"]})
    k.create(None, id="c", can_call={"f": "*"})
    w = await _op(
        k,
        "g",
        {"type": "write_stream", "path": "data.txt", "bytes": DATA, "truncate": True},
    )
    assert w["result"].get("written") == len(DATA), w


async def _pump(k, as_, **args):
    return (await _op(k, "f", {"type": "pump", **args}, as_=as_))["result"]


async def test_a_pump_reads_and_writes_as_the_bridge(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    k = seeded_kernel
    await _wire(k, f_reaches_g=True)
    # C reaches G not at all — the pump's ends are F's sends, not C's
    direct = await _op(k, "g", {"type": "read_stream", "path": "data.txt"}, as_="c")
    assert direct["result"].get("reason") == "unauthorized", direct
    out = await _pump(k, "c", source="g", source_path="data.txt", sink_path="copy.txt")
    assert out.get("bytes") == len(DATA), out
    back = await _op(k, "f", {"type": "read_stream", "path": "copy.txt"})
    assert back["result"].get("bytes") == DATA, back


async def test_a_pump_cannot_reach_an_end_the_bridge_cannot(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    k = seeded_kernel
    await _wire(k, f_reaches_g=False)
    out = await _pump(k, "c", source="g", source_path="data.txt", sink_path="copy.txt")
    assert out.get("reason") == "transport_error", out
    assert "unauthorized" in out.get("error", "") or "may not call" in out.get(
        "error", ""
    ), out
