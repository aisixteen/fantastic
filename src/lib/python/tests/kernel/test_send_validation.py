"""`send` validates the ITEM before anything is resolved, gated or spawned.

Measured 2026-09-02: a bad `retries` on an unnamed item was caught only after the
dispatch had started (an orphaned running task behind a `failed` outcome); a
payload without `type` reached the gate and the target instead of being a coded
refusal; a non-string `type` raised out of `send` (HTTP 500 on web_rest)."""

from __future__ import annotations

from _testkit import as_agent


async def _mk(kernel, agent_id, **meta):
    payload = {
        "type": "create_agent",
        "parent_id": kernel.id,
        "id": agent_id,
        "handler_module": "file_bridge.tools",
        "allow": ["."],
        "ingress_rule": "allow_all_ingress",
        **meta,
    }
    rec = (await kernel.send([{"target": kernel.id, "payload": payload}]))[0]["result"]
    assert rec.get("id") == agent_id, rec
    return agent_id


async def test_bad_retries_on_unnamed_item_runs_nothing(seeded_kernel):
    a = await _mk(seeded_kernel, "sv_retries")
    out = (
        await seeded_kernel.send(
            [{"target": a, "payload": {"type": "reflect"}, "retries": -1}]
        )
    )[0]
    assert out["status"] == "failed" and out["pid"] is None, out
    assert out["result"]["reason"] == "bad_arg" and out["result"]["arg"] == "retries"
    rows = (
        await seeded_kernel.send([{"target": "kernel", "payload": {"type": "ps"}}])
    )[0]["result"]["tasks"]
    assert not [r for r in rows if r.get("agent") == a], rows


async def test_missing_type_is_missing_arg_before_the_gate(seeded_kernel):
    a = await _mk(seeded_kernel, "sv_a")
    b = await _mk(seeded_kernel, "sv_b")  # no edge a→b: the gate WOULD refuse
    with as_agent(seeded_kernel, a):
        out = (await seeded_kernel.send([{"target": b, "payload": {}}]))[0]
    r = out["result"]
    assert out["status"] == "failed" and out["pid"] is None
    assert r["reason"] == "missing_arg" and r["arg"] == "payload.type", r
    assert "side" not in r, "validated BEFORE the gate — no gate side"


async def test_non_string_type_never_raises(seeded_kernel):
    a = await _mk(seeded_kernel, "sv_c")
    for bad in (["x"], 7, "", None):
        out = (await seeded_kernel.send([{"target": a, "payload": {"type": bad}}]))[0]
        assert out["result"]["reason"] == "bad_arg", (bad, out)
        assert out["result"]["arg"] == "payload.type"


async def test_non_string_ids_on_system_verbs_are_missing_arg(seeded_kernel):
    k = seeded_kernel.id
    for payload in (
        {"type": "delete_agent", "id": 5},
        {"type": "update_agent", "id": ["x"]},
        {"type": "create_agent", "handler_module": 7, "parent_id": k},
        {
            "type": "create_agent",
            "handler_module": "file_bridge.tools",
            "allow": ["."],
            "parent_id": 3,
        },
        {"type": "cancel", "task_id": []},
    ):
        r = (await seeded_kernel.send([{"target": k, "payload": payload}]))[0]["result"]
        assert r.get("reason") == "missing_arg", (payload, r)
        assert "raised" not in r, (payload, r)


async def test_a_supplied_empty_id_on_create_is_refused_not_minted(seeded_kernel):
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": seeded_kernel.id,
                    "payload": {
                        "type": "create_agent",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "parent_id": seeded_kernel.id,
                        "id": "",
                    },
                }
            ]
        )
    )[0]["result"]
    assert r.get("reason") == "bad_arg" and r.get("arg") == "id", r
