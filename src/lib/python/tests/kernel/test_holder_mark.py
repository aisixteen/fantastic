"""The SECRET-HOLDER MARK's two kernel rules, pinned WHOLE (owner 2026-10-02).

`holds_secrets: true` makes an agent unreachable from a model turn (ai_core). The
kernel owns the field itself:
  - SHAPE, at the write, on every agent: exactly `true`, or `null` (the clear) —
    anything else is `invalid_meta`, at create_agent and update_agent alike;
  - WHO LOWERS IT, in the gate: on a holder, only the operator writes anything but
    `true` — an agent granted `update_agent` cannot unmark a holder and then call it.
Order is fixed: the gate (who) → the kernel's shape checks → the bundle's own
`validate_meta`.
"""

from __future__ import annotations

import types

from kernel import sender_context, register_unlisted

_mod = types.ModuleType("holder_mark_probe.tools")


async def _handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {"id": id, "sentence": "holder probe", "verbs": {}}
    return {"ok": id, "verb": verb}


_mod.handler = _handler
_mod.VERBS = {}
register_unlisted("holder_mark_probe.tools", _mod)

MOD = "holder_mark_probe.tools"
# AN AGENT EDITS A RECORD IT NAMES (AV-1): `update_agent` is gated on its operand —
# egress naming it with the verb, and the operand's ingress accepting the editor.
UPDATER = {"vault": ["update_agent"], "plain": ["update_agent"]}
ACCEPTS_A = {"a": ["update_agent"]}


def _unmark(sender: str, target: str) -> dict:
    return {
        "error": (
            f"{sender} may not clear holds_secrets on {target}: it holds secrets, "
            "and only the operator lowers that mark"
        ),
        "reason": "unauthorized",
        "side": "secret-holder",
        "hint": (
            "only the operator clears it (update_agent holds_secrets=null); an agent "
            "may mark a holder, never unmark one"
        ),
    }


def _shape(got: str) -> str:
    return (
        "holds_secrets must be exactly true (this agent holds secrets) or null "
        f"(the operator clears the mark) — got {got}"
    )


# every non-true, non-null value, with the type name the refusal reports
BAD = (
    (False, "bool"),
    ("yes", "str"),
    ("true", "str"),
    (1, "int"),
    (0, "int"),
    (1.5, "float"),
    ([True], "list"),
    ({"v": True}, "dict"),
)


async def _op(k, payload: dict) -> dict:
    return (await k.send([{"target": "kernel", "payload": payload}]))[0]["result"]


async def _as(k, sender: str, payload: dict) -> dict:
    with sender_context(sender):
        return (await k.send([{"target": "kernel", "payload": payload}]))[0]


# ─── K1: only the operator lowers the mark ───────────────────────────────────


async def test_an_agent_cannot_clear_the_mark_on_a_holder(seeded_kernel):
    k = seeded_kernel
    k.create(MOD, id="vault", holds_secrets=True, callable_by=ACCEPTS_A)
    k.create(MOD, id="a", can_call=UPDATER)
    for v in (None, *(b for b, _ in BAD)):
        out = await _as(
            k, "a", {"type": "update_agent", "id": "vault", "holds_secrets": v}
        )
        assert out["pid"] is None and out["status"] == "failed", out
        r = out["result"]
        assert r == _unmark("a", "vault"), (v, r)
        assert list(r) == ["error", "reason", "side", "hint"]
    assert k.get("vault")["holds_secrets"] is True


async def test_the_refusal_wins_over_every_other_field_in_the_patch(seeded_kernel):
    """The whole update is refused — a field beside the clear is not written."""
    k = seeded_kernel
    k.create(MOD, id="vault", holds_secrets=True, callable_by=ACCEPTS_A)
    k.create(MOD, id="a", can_call=UPDATER)
    r = (
        await _as(
            k,
            "a",
            {
                "type": "update_agent",
                "id": "vault",
                "holds_secrets": None,
                "description": "unmarked",
            },
        )
    )["result"]
    assert r == _unmark("a", "vault"), r
    assert k.get("vault").get("description") is None


async def test_a_holder_cannot_clear_its_own_mark(seeded_kernel):
    k = seeded_kernel
    k.create(MOD, id="vault", holds_secrets=True, can_call=UPDATER)
    r = (
        await _as(
            k, "vault", {"type": "update_agent", "id": "vault", "holds_secrets": None}
        )
    )["result"]
    assert r == _unmark("vault", "vault"), r
    assert k.get("vault")["holds_secrets"] is True


async def test_an_agent_may_mark_but_never_unmark(seeded_kernel):
    """`true` passes from anywhere — on a plain agent it marks it, on a holder it is
    the value already there. Other fields of a holder stay writable."""
    k = seeded_kernel
    k.create(MOD, id="plain", callable_by=ACCEPTS_A)
    k.create(MOD, id="vault", holds_secrets=True, callable_by=ACCEPTS_A)
    k.create(MOD, id="a", can_call=UPDATER)
    for target in ("plain", "vault"):
        out = await _as(
            k, "a", {"type": "update_agent", "id": target, "holds_secrets": True}
        )
        assert out["result"].get("updated") is True, out
        assert k.get(target)["holds_secrets"] is True
    out = await _as(k, "a", {"type": "update_agent", "id": "vault", "description": "d"})
    assert out["result"].get("updated") is True, out
    r = (
        await _as(
            k, "a", {"type": "update_agent", "id": "plain", "holds_secrets": None}
        )
    )["result"]
    assert r == _unmark("a", "plain"), "once marked, the agent cannot take it back"


async def test_null_on_an_unmarked_agent_is_not_a_clear(seeded_kernel):
    """The rule guards a HOLDER. `null` on an agent with no mark changes nothing it
    protects, so it is not refused."""
    k = seeded_kernel
    k.create(MOD, id="plain", callable_by=ACCEPTS_A)
    k.create(MOD, id="a", can_call=UPDATER)
    out = await _as(
        k, "a", {"type": "update_agent", "id": "plain", "holds_secrets": None}
    )
    assert out["result"].get("updated") is True, out


async def test_the_gate_judges_before_the_shape(seeded_kernel):
    """A non-operator's malformed value on a HOLDER gets the gate's refusal; on a
    plain agent, the shape's."""
    k = seeded_kernel
    k.create(MOD, id="plain", callable_by=ACCEPTS_A)
    k.create(MOD, id="vault", holds_secrets=True, callable_by=ACCEPTS_A)
    k.create(MOD, id="a", can_call=UPDATER)
    on_holder = (
        await _as(
            k, "a", {"type": "update_agent", "id": "vault", "holds_secrets": "yes"}
        )
    )["result"]
    assert on_holder == _unmark("a", "vault"), on_holder
    on_plain = (
        await _as(
            k, "a", {"type": "update_agent", "id": "plain", "holds_secrets": "yes"}
        )
    )["result"]
    assert on_plain == {
        "error": _shape("str"),
        "reason": "invalid_meta",
        "id": "plain",
    }, on_plain


async def test_the_operator_clears_the_mark_with_null(seeded_kernel):
    """`holds_secrets: null` is the clear: update_agent REPLACES the named field, so
    the record carries `null` — not a holder (only exactly `true` marks one)."""
    k = seeded_kernel
    k.create(MOD, id="vault", holds_secrets=True, callable_by=ACCEPTS_A)
    out = await _op(k, {"type": "update_agent", "id": "vault", "holds_secrets": None})
    assert out.get("updated") is True, out
    assert out["agent"]["holds_secrets"] is None
    assert k.get("vault")["holds_secrets"] is None
    # cleared means cleared: an agent's `null` is no longer refused
    k.create(MOD, id="a", can_call=UPDATER)
    again = await _as(
        k, "a", {"type": "update_agent", "id": "vault", "holds_secrets": None}
    )
    assert again["result"].get("updated") is True, again


# ─── K2: the shape, at the write, on every agent ─────────────────────────────


async def test_every_non_true_value_is_refused_at_update(seeded_kernel):
    k = seeded_kernel
    k.create(MOD, id="plain", callable_by=ACCEPTS_A)
    for v, got in BAD:
        r = await _op(k, {"type": "update_agent", "id": "plain", "holds_secrets": v})
        assert r == {"error": _shape(got), "reason": "invalid_meta", "id": "plain"}, (
            v,
            r,
        )
        assert list(r) == ["error", "reason", "id"]
    assert "holds_secrets" not in k.get("plain")


async def test_every_non_true_value_is_refused_at_create(seeded_kernel):
    k = seeded_kernel
    for i, (v, got) in enumerate(BAD):
        aid = f"made_{i}"
        r = await _op(
            k,
            {
                "type": "create_agent",
                "parent_id": k.id,
                "handler_module": MOD,
                "id": aid,
                "holds_secrets": v,
            },
        )
        assert r == {"error": _shape(got), "reason": "invalid_meta"}, (v, r)
        assert aid not in k.ctx.agents


async def test_an_agent_create_is_shape_checked_too(seeded_kernel):
    """The shape check is the write's, not the operator's: an agent forking its own
    subtree is refused the same way."""
    k = seeded_kernel
    k.create(MOD, id="a")
    r = (
        await _as(
            k,
            "a",
            {
                "type": "create_agent",
                "parent_id": "a",
                "handler_module": MOD,
                "id": "kid",
                "holds_secrets": False,
            },
        )
    )["result"]
    assert r == {"error": _shape("bool"), "reason": "invalid_meta"}, r
    assert "kid" not in k.ctx.agents


async def test_true_and_null_are_legal_at_create(seeded_kernel):
    k = seeded_kernel
    for aid, v in (("marked", True), ("unmarked", None)):
        r = await _op(
            k,
            {
                "type": "create_agent",
                "parent_id": k.id,
                "handler_module": MOD,
                "id": aid,
                "holds_secrets": v,
            },
        )
        assert r.get("id") == aid, r
        assert k.get(aid)["holds_secrets"] is v
