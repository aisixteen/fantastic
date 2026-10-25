"""Python twins of the Rust kernel lifecycle / rows unit tests (#170).

Each test is named `test_<rust test name>` and asserts what the Rust test asserts;
Python is the spec, so a failure here is a drift on the Python side too.
"""

from __future__ import annotations

import re
import types

from kernel import Kernel, register_unlisted

MOD = "lifecycle_twins_probe.tools"
_mod = types.ModuleType(MOD)


async def _handler(id, payload, kernel):
    if payload.get("type") in ("boot", "shutdown"):
        return None
    return {"ok": True}


_mod.handler = _handler
_mod.VERBS = {}
register_unlisted(MOD, _mod)


async def _call(kernel, payload: dict) -> dict:
    return (await kernel.send([{"target": "kernel", "payload": payload}]))[0]["result"]


def _create(**kw) -> dict:
    return {"type": "create_agent", "handler_module": MOD, "parent_id": "kernel", **kw}


async def test_mint_id_takes_the_second_last_dotted_segment(seeded_kernel):
    """The bundle part of a minted id is the second-last dotted segment (the whole
    string when there is no dot), then `_` and 6 hex."""
    root = seeded_kernel
    for hm, bundle in [
        ("a.b.tools", "b"),
        ("x.y", "x"),
        ("pkg.ts", "pkg"),
        ("web.tools", "web"),
        ("plain", "plain"),
        (".tools", ""),
    ]:
        rec = root.create(hm)
        m = re.fullmatch(r"(.*)_([0-9a-f]{6})", rec["id"])
        assert m, f"{hm} -> {rec}"
        assert m.group(1) == bundle, f"{hm} -> {rec['id']}"


async def test_create_agent_demands_a_parent_and_resolves_it_like_a_target(seeded_kernel):
    k = seeded_kernel
    # Absent => missing_arg, and the refusal says what to name.
    payload = _create(id="x")
    del payload["parent_id"]
    missing = await _call(k, payload)
    assert missing["reason"] == "missing_arg"
    assert "OWN the new one" in missing["hint"]
    # Unknown => unknown_agent, carrying the live id index.
    unknown = await _call(k, _create(parent_id="nope", id="x"))
    assert unknown["reason"] == "unknown_agent"
    assert isinstance(unknown["agents"], list)
    # Named => the new agent lands THERE, not under whoever was addressed.
    await _call(k, _create(parent_id="kernel", id="mid"))
    rec = await _call(k, _create(parent_id="mid", id="leaf"))
    assert rec["parent_id"] == "mid"
    assert k.ctx.agents["leaf"].parent is k.ctx.agents["mid"]


async def test_duplicate_id_refusal_is_python_text(seeded_kernel):
    k = seeded_kernel
    await _call(k, _create(id="dup"))
    r = await _call(k, _create(id="dup"))
    assert r["error"] == "agent 'dup' exists"
    assert r["reason"] == "exists"
    assert r["hint"] == "pick another id, or update the existing agent"
    assert r["id"] == "dup"
    assert list(r) == ["error", "reason", "hint", "id"]


async def test_empty_required_args_are_missing(seeded_kernel):
    k = seeded_kernel
    r = await _call(k, _create(handler_module="", id="x"))
    assert r["reason"] == "missing_arg"
    assert r["arg"] == "handler_module"
    r = await _call(k, _create(parent_id="", id="x"))
    assert r["reason"] == "missing_arg"
    assert r["arg"] == "parent_id"


async def test_create_refusal_precedence_matches_python(seeded_kernel):
    """handler_module -> parent_id -> resolve parent -> reserved keys (sorted) ->
    grant shape -> id guard; each probe trips exactly one gate earlier than the
    next down the list would."""
    k = seeded_kernel
    # Both handler_module AND parent_id absent -> handler_module wins.
    r = await _call(k, {"type": "create_agent"})
    assert r["arg"] == "handler_module"
    # Unresolvable parent AND a reserved key -> parent resolution wins.
    r = await _call(k, _create(parent_id="ghost", root_path="/evil"))
    assert r["reason"] == "unknown_agent"
    # Resolvable parent, two reserved keys -> the alphabetically first is named.
    r = await _call(k, _create(root_path="/evil", parent="x"))
    assert r["arg"] == "parent"
    # Reserved keys clear, grant shape bad -> invalid_meta before the id guard.
    r = await _call(k, _create(can_call="*", id=123))
    assert r["reason"] == "invalid_meta"
    assert r["error"] == (
        'can_call must be a grant MAP {"<id|$token>": "*" | ["<verb>", …]} '
        "— got str. To open wide, grant per key "
        '(e.g. {"$self": "*", "ai": "*"}), never a bare value'
    )
    # Grant shape fine, id invalid (digit-only) -> the id guard.
    r = await _call(k, _create(id="123"))
    assert r["reason"] == "bad_arg"


async def test_update_agent_checks_reserved_keys_before_resolving_id(seeded_kernel):
    """Reserved keys are checked BEFORE the id resolves: an unknown id with a
    reserved key answers bad_arg, not unknown_agent."""
    k = seeded_kernel
    r = await _call(k, {"type": "update_agent", "id": "ghost", "handler_module": "x.tools"})
    assert r["reason"] == "bad_arg"
    assert r["arg"] == "handler_module"
    r = await _call(
        k, {"type": "update_agent", "id": "ghost", "root_path": "/evil", "parent": "x"}
    )
    assert r["arg"] == "parent"
    r = await _call(k, {"type": "update_agent", "id": ""})
    assert r["reason"] == "missing_arg"
    assert r["arg"] == "id"


async def test_delete_agent_empty_id_is_missing_arg(seeded_kernel):
    r = await _call(seeded_kernel, {"type": "delete_agent", "id": ""})
    assert r["reason"] == "missing_arg"
    assert r["arg"] == "id"


async def test_delete_lock_int_refuses_like_python(seeded_kernel):
    """`delete_lock: 1` (the CLI's k=v coercion) locks a GRANDCHILD by truthiness
    and the cascade is refused, though the parent's own lock is 0."""
    k = seeded_kernel
    for id, parent, lock in [("P", "kernel", 0), ("C", "P", 1)]:
        r = await _call(k, _create(id=id, parent_id=parent, delete_lock=lock))
        assert "error" not in r, r
    r = await _call(k, {"type": "delete_agent", "id": "P"})
    assert r["reason"] == "locked", r
    assert r["blocked_by"] == "C"
    assert "C" in k.ctx.agents and "P" in k.ctx.agents


async def test_lifecycle_events_name_an_empty_display_name_by_id(seeded_kernel):
    """`display_name or id`: '' is no name in added/removed and the snapshot."""
    k = seeded_kernel
    seen: list[dict] = []
    k.add_state_subscriber(seen.append)
    await _call(k, _create(id="Q", display_name=""))
    row = next(r for r in k.state_snapshot() if r["agent_id"] == "Q")
    assert row["name"] == "Q"
    await _call(k, {"type": "delete_agent", "id": "Q"})
    kinds = []
    for ev in seen:
        if ev["agent_id"] == "Q" and ev["kind"] in ("added", "removed"):
            assert ev["name"] == "Q", ev
            kinds.append(ev["kind"])
    assert kinds == ["added", "removed"]


# ─── the row table (Rust `rows`) ────────────────────────────────────────────


def test_ids_come_from_one_counter_starting_at_one():
    k = Kernel()
    assert k.next_id == 1
    assert k.mint_id() == 1
    assert k.mint_id() == 2
    # A put with no id takes the next one: the SAME counter, not a second.
    assert k.rows_put("ai", {"kind": "call"}) == 3
    assert k.next_id == 4


def test_a_pre_minted_id_is_accepted_and_anything_else_refused():
    import pytest

    k = Kernel()
    reserved = k.mint_id()  # 1
    k.mint_id()  # 2
    assert k.rows_put("ai", {"kind": "call", "id": reserved}) == reserved
    with pytest.raises(ValueError, match="pre-minted unused id"):
        k.rows_put("ai", {"kind": "call", "id": reserved})
    for bad in (0, -1, 99, True, "7"):
        with pytest.raises(ValueError, match="pre-minted unused id"):
            k.rows_put("ai", {"kind": "call", "id": bad})


def test_every_row_carries_its_agent_and_a_timestamp():
    k = Kernel()
    rid = k.rows_put("ai", {"kind": "call"})
    got = k.rows[rid]
    assert got["agent"] == "ai"
    assert got["id"] == rid
    assert isinstance(got["ts"], (int, float)) and not isinstance(got["ts"], bool)
