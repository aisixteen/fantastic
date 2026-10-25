"""The REPLY SHAPES a kernel verb answers with.

Python is the spec; the Rust kernel's `reply_shapes.rs` is graded against it. The
twins below pin the same shapes here, so a drift on this side shows too: the
`{updated, id, agent}` an update answers, the coded refusal for an unknown id, the
ledger row's key set, the registration order `list_agents` walks, and the key order
of a record.
"""

from __future__ import annotations

import types

from kernel import register_unlisted

_mod = types.ModuleType("reply_shapes_probe.tools")


async def _handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    return {"ok": True}


_mod.handler = _handler
_mod.VERBS = {}
register_unlisted("reply_shapes_probe.tools", _mod)
MOD = "reply_shapes_probe.tools"


async def _call(kernel, target: str, payload: dict) -> dict:
    return (await kernel.send([{"target": target, "payload": payload}]))[0]["result"]


async def _mk_child(kernel, id: str, **meta) -> dict:
    r = await _call(
        kernel,
        "kernel",
        {
            "type": "create_agent",
            "parent_id": "kernel_state",
            "handler_module": MOD,
            "id": id,
            **meta,
        },
    )
    assert r["id"] == id, f"fixture create failed: {r}"
    return r


async def test_update_agent_answers_updated_id_and_the_record(seeded_kernel):
    await _mk_child(seeded_kernel, "kid")
    r = await _call(
        seeded_kernel, "kernel", {"type": "update_agent", "id": "kid", "note": "hello"}
    )
    assert r["updated"] is True, r
    assert r["id"] == "kid", r
    # The record rides UNDER `agent`, whole.
    assert r["agent"]["id"] == "kid", r
    assert r["agent"]["note"] == "hello", r
    # And the three keys are the whole reply.
    assert list(r) == ["updated", "id", "agent"], r


async def test_delete_agent_on_an_unknown_id_is_the_unknown_agent_refusal(
    seeded_kernel,
):
    await _mk_child(seeded_kernel, "kid")
    r = await _call(seeded_kernel, "kernel", {"type": "delete_agent", "id": "typo"})
    assert r["reason"] == "unknown_agent", r
    assert r["deleted"] is False, r
    # The legal values, so the caller can self-correct without a second verb.
    assert "kid" in r["agents"], r
    assert "kernel_state" in r["agents"], r


async def test_ledger_rows_carry_no_verb_key(seeded_kernel):
    await _mk_child(seeded_kernel, "kid")
    # One NAMED execution, so the row is settled and carries an outcome.
    await seeded_kernel.send(
        [{"target": "kid", "payload": {"type": "poke"}, "id": "job-1"}]
    )
    r = await _call(seeded_kernel, "kernel", {"type": "ledger"})
    rows = r["executions"]
    assert rows, r
    for row in rows:
        assert "verb" not in row, f"row carries `verb`: {row}"
        for want in ("id", "target", "status", "attempts", "retries", "named"):
            assert want in row, f"row is missing {want}: {row}"


async def test_list_agents_is_registry_order(kernel):
    # Deliberately NOT alphabetical: sorting would put `alpha` first.
    for id in ("zulu", "mike", "alpha"):
        await _mk_child(kernel, id)

    def ids_of(r):
        return [a["id"] for a in r["agents"]]

    r = await _call(kernel, "kernel", {"type": "list_agents"})
    assert ids_of(r) == ["kernel_state", "zulu", "mike", "alpha"], r

    # A delete removes it from the order and leaves the rest as they were.
    d = await _call(kernel, "kernel", {"type": "delete_agent", "id": "mike"})
    assert d["deleted"] is True, d
    r = await _call(kernel, "kernel", {"type": "list_agents"})
    assert ids_of(r) == ["kernel_state", "zulu", "alpha"], r

    # ...and the cursor pages through THAT order, not a sorted one.
    p = await _call(kernel, "kernel", {"type": "list_agents", "limit": 2})
    assert ids_of(p) == ["kernel_state", "zulu"], p


async def test_record_parent_id_is_last(seeded_kernel):
    created = await _mk_child(seeded_kernel, "kid", display_name="Kid", note="second")
    keys = list(created)
    assert keys[0] == "id", created
    assert keys[1] == "handler_module", created
    assert keys[-1] == "parent_id", created
    # Meta keeps its own order between the two.
    assert keys.index("display_name") < keys.index("note"), keys

    # Same order off `list_agents` — one record shape, one spelling.
    r = await _call(seeded_kernel, "kernel", {"type": "list_agents"})
    kid = next(a for a in r["agents"] if a["id"] == "kid")
    assert list(kid)[:2] == ["id", "handler_module"], kid
    assert list(kid)[-1] == "parent_id", kid
    assert list(kid).index("display_name") < list(kid).index("note"), kid
