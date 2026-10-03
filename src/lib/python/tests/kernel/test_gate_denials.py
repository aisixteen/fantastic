"""Every call-gate denial, pinned WHOLE: its text, `reason`, `side`, `hint`, the
extra facts it carries, and the key order of the dict.

The gate builds these in several places today (`_gate`, `_gate_placement`,
`_check_conferral`, `_heritable_conferral`) — some through `refusal()`, some by
hand. Folding those into one egress/ingress primitive (reduction candidate D)
must answer every caller byte-for-byte as it does now: the ports diff these
replies, and a model reads the text to repair its call.
"""

from __future__ import annotations

import types

from kernel import remote_caller_context, sender_context, register_unlisted
from kernel._authz import _FROM_ABOVE
from kernel._kernel import PeerRoute


async def _no_emit(agent_id, payload, caller):  # these routes carry no events
    raise AssertionError("unexpected emit")


_mod = types.ModuleType("gate_pin_probe.tools")


async def _handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {"id": id, "sentence": "gate probe", "verbs": {}}
    return {"ok": id, "verb": verb}


_mod.handler = _handler
_mod.VERBS = {}
register_unlisted("gate_pin_probe.tools", _mod)

MOD = "gate_pin_probe.tools"


def _mk(k, aid: str, parent=None, **meta) -> str:
    rec = k.create(MOD, id=aid, **meta) if parent is None else None
    if parent is not None:
        rec = k.ctx.agents[parent].create(MOD, id=aid, **meta)
    assert rec.get("id") == aid, rec
    return aid


async def _as(k, sender: str, target: str, payload: dict) -> dict:
    with sender_context(sender):
        return (await k.send([{"target": target, "payload": payload}]))[0]


# ─── egress / ingress on an ordinary call ────────────────────────────────────


async def test_egress_denial_is_exact(seeded_kernel):
    k = seeded_kernel
    _mk(k, "a")
    _mk(k, "b")
    out = await _as(k, "a", "b", {"type": "ping"})
    assert out["pid"] is None and out["status"] == "failed"
    r = out["result"]
    assert r == {
        "error": "a may not call 'ping' on b",
        "reason": "unauthorized",
        "hint": _FROM_ABOVE,
        "side": "egress",
    }
    assert list(r) == ["error", "reason", "hint", "side"]


async def test_ingress_denial_is_exact(seeded_kernel):
    k = seeded_kernel
    _mk(k, "b")
    _mk(k, "a", can_call={"b": ["ping"]})
    r = (await _as(k, "a", "b", {"type": "ping"}))["result"]
    assert r == {
        "error": "b does not accept 'ping' from a",
        "reason": "unauthorized",
        "hint": _FROM_ABOVE,
        "side": "ingress",
    }
    assert list(r) == ["error", "reason", "hint", "side"]


async def test_both_ends_granted_lands(seeded_kernel):
    k = seeded_kernel
    _mk(k, "b", callable_by={"a": ["ping"]})
    _mk(k, "a", can_call={"b": ["ping"]})
    out = await _as(k, "a", "b", {"type": "ping"})
    assert out["status"] == "completed" and out["result"] == {"ok": "b", "verb": "ping"}


async def test_a_foreign_caller_is_judged_on_ingress_with_its_own_hint(seeded_kernel):
    k = seeded_kernel
    _mk(k, "leg", can_call={"b": "*"})
    _mk(k, "b", callable_by={"leg": "*"})
    with remote_caller_context("leg", "g9|scout"):
        r = (await _as(k, "leg", "b", {"type": "ping"}))["result"]
    assert r == {
        "error": "b does not accept 'ping' from g9|scout",
        "reason": "unauthorized",
        "hint": (
            "a foreign caller is accepted only by name: callable_by "
            "{'g9|scout': [...]} or {'g9|*': [...]} on b"
        ),
        "side": "ingress",
    }
    # accepting the LEG is not accepting its peer; naming the peer is
    k.ctx.update("b", callable_by={"leg": "*", "g9|scout": ["ping"]})
    with remote_caller_context("leg", "g9|scout"):
        ok = await _as(k, "leg", "b", {"type": "ping"})
    assert ok["status"] == "completed", ok


async def test_a_remote_target_is_egress_only_with_its_qualified_hint(seeded_kernel):
    k = seeded_kernel
    k._meta["guid"] = "h0"

    async def call(agent_id, payload, caller, secret):
        return {"remote": agent_id}

    assert (
        k.ctx.register_peer("g1", PeerRoute(via="leg", call=call, emit=_no_emit))
        is None
    )
    _mk(k, "a")
    r = (await _as(k, "a", "g1|canvas", {"type": "around"}))["result"]
    assert r == {
        "error": "a may not call 'around' on g1|canvas",
        "reason": "unauthorized",
        "hint": (
            "g1|canvas is an agent on the connected kernel g1: name it in "
            "can_call — {'g1|canvas': [...]} or {'g1|*': [...]}"
        ),
        "side": "egress",
    }


# ─── placement (create_agent's operand is the parent) ────────────────────────


async def test_placement_egress_denial_is_exact(seeded_kernel):
    k = seeded_kernel
    _mk(k, "a", can_call={"kernel_state": ["create_agent"]})
    _mk(k, "p")
    r = (
        await _as(
            k,
            "a",
            "kernel",
            {"type": "create_agent", "parent_id": "p", "handler_module": MOD},
        )
    )["result"]
    assert r == {
        "error": "a may not create under p",
        "reason": "unauthorized",
        "hint": _FROM_ABOVE,
        "side": "egress",
    }


async def test_placement_ingress_denial_is_exact(seeded_kernel):
    k = seeded_kernel
    _mk(k, "p")
    _mk(k, "a", can_call={"p": ["create_agent"]})
    r = (
        await _as(
            k,
            "a",
            "kernel",
            {"type": "create_agent", "parent_id": "p", "handler_module": MOD},
        )
    )["result"]
    assert r == {
        "error": "p does not accept a child from a",
        "reason": "unauthorized",
        "hint": _FROM_ABOVE,
        "side": "ingress",
    }


async def test_placement_under_your_own_subtree_needs_no_grant(seeded_kernel):
    k = seeded_kernel
    _mk(k, "a")
    _mk(k, "kid", parent="a")
    for parent in ("a", "kid"):
        out = await _as(
            k,
            "a",
            "kernel",
            {"type": "create_agent", "parent_id": parent, "handler_module": MOD},
        )
        assert out["status"] == "completed" and "id" in out["result"], out


async def test_placement_for_a_relayed_caller_is_exact(seeded_kernel):
    """A foreign caller a bridge leg relays owns no subtree here (audit G-1):
    placing under the leg or its child takes the path any other placement takes
    — the leg's egress, then the parent's ingress against `<guid>|<agent>`."""
    k = seeded_kernel
    _mk(k, "leg")
    _mk(k, "kid", parent="leg")

    def create(parent):
        return {"type": "create_agent", "parent_id": parent, "handler_module": MOD}

    with remote_caller_context("leg", "g9|scout"):
        r = (await _as(k, "leg", "kernel", create("kid")))["result"]
    assert r == {
        "error": "leg may not create under kid",
        "reason": "unauthorized",
        "hint": _FROM_ABOVE,
        "side": "egress",
    }
    with remote_caller_context("leg", "g9|scout"):
        r = (await _as(k, "leg", "kernel", create("leg")))["result"]
    assert r == {
        "error": "leg does not accept a child from g9|scout",
        "reason": "unauthorized",
        "hint": _FROM_ABOVE,
        "side": "ingress",
    }
    # the parent naming the peer opens it
    k.ctx.update(
        "leg",
        callable_by={
            "$self": "*",
            "$parent": "*",
            "$children": "*",
            "g9|scout": ["create_agent"],
        },
    )
    with remote_caller_context("leg", "g9|scout"):
        out = await _as(k, "leg", "kernel", create("leg"))
    assert out["status"] == "completed" and "id" in out["result"], out


# ─── conferral: authority flows from above only ──────────────────────────────


async def test_self_grant_denial_is_exact(seeded_kernel):
    k = seeded_kernel
    _mk(k, "a", can_call={"kernel_state": ["update_agent"]})
    r = (
        await _as(
            k,
            "a",
            "kernel",
            {"type": "update_agent", "id": "a", "can_call": {"x": "*"}},
        )
    )["result"]
    assert r == {
        "error": "a may not edit its own can_call/callable_by",
        "reason": "unauthorized",
        "side": "self-grant",
        "hint": _FROM_ABOVE,
    }
    assert list(r) == ["error", "reason", "side", "hint"]


async def test_a_non_structural_token_denial_is_exact(seeded_kernel):
    k = seeded_kernel
    _mk(k, "a")
    r = (
        await _as(
            k,
            "a",
            "kernel",
            {
                "type": "create_agent",
                "parent_id": "a",
                "handler_module": MOD,
                "can_call": {"$root": "*"},
            },
        )
    )["result"]
    assert r == {
        "error": (
            "a cannot confer '$root': not a structural token — $self, $parent, "
            "$children and $loader are the only ones"
        ),
        "reason": "bad_arg",
        "arg": "can_call",
        "side": "delegation",
        "hint": _FROM_ABOVE,
    }
    assert list(r) == ["error", "reason", "arg", "side", "hint"]


async def test_loader_delegation_denial_is_exact(seeded_kernel):
    k = seeded_kernel
    _mk(k, "a")
    _mk(k, "kid", parent="a")
    r = (
        await _as(
            k,
            "a",
            "kernel",
            {
                "type": "create_agent",
                "parent_id": "a",
                "handler_module": MOD,
                "can_call": {"$loader": ["ps"]},
            },
        )
    )["result"]
    assert r == {
        "error": (
            "a cannot delegate 'ps' on $loader: it resolves to kernel_state, which "
            "you do not hold — $loader is real reach on the kernel's control "
            "surface, not a structural token"
        ),
        "reason": "unauthorized",
        "side": "delegation",
        "hint": _FROM_ABOVE,
    }


async def test_structural_tokens_are_free_to_confer(seeded_kernel):
    k = seeded_kernel
    _mk(k, "a")
    out = await _as(
        k,
        "a",
        "kernel",
        {
            "type": "create_agent",
            "parent_id": "a",
            "handler_module": MOD,
            "can_call": {"$self": "*", "$parent": "*", "$children": "*"},
        },
    )
    assert out["status"] == "completed" and "id" in out["result"], out


async def test_no_such_agent_and_not_your_child_denials_are_exact(seeded_kernel):
    k = seeded_kernel
    _mk(k, "a")
    _mk(k, "elsewhere")
    base = {"type": "create_agent", "parent_id": "a", "handler_module": MOD}
    r = (await _as(k, "a", "kernel", {**base, "can_call": {"ghost": ["ping"]}}))[
        "result"
    ]
    assert r == {
        "error": (
            "a cannot delegate 'ping' on ghost: no such agent — calls of one batch "
            "run concurrently, so an agent created in this batch does not exist "
            "yet; create in one batch, grant in the next"
        ),
        "reason": "unauthorized",
        "side": "delegation",
        "missing": "ghost",
        "hint": _FROM_ABOVE,
    }
    assert list(r) == ["error", "reason", "side", "missing", "hint"]
    r = (await _as(k, "a", "kernel", {**base, "can_call": {"elsewhere": "*"}}))[
        "result"
    ]
    assert r == {
        "error": (
            "a cannot delegate '*' on elsewhere: elsewhere is not your child (its "
            "parent is kernel_state) and not in your can_call — you may only confer "
            "can_call you already hold; create the agents you will wire as YOUR "
            "children"
        ),
        "reason": "unauthorized",
        "side": "delegation",
        "parent": "kernel_state",
        "hint": _FROM_ABOVE,
    }
    assert list(r) == ["error", "reason", "side", "parent", "hint"]


async def test_a_concrete_list_probes_each_verb(seeded_kernel):
    k = seeded_kernel
    _mk(k, "t")
    _mk(k, "a", can_call={"t": ["read"]})
    base = {"type": "create_agent", "parent_id": "a", "handler_module": MOD}
    ok = await _as(k, "a", "kernel", {**base, "can_call": {"t": ["read"]}})
    assert ok["status"] == "completed" and "id" in ok["result"], ok
    r = (await _as(k, "a", "kernel", {**base, "can_call": {"t": ["read", "write"]}}))[
        "result"
    ]
    assert r["side"] == "delegation" and r["error"].startswith(
        "a cannot delegate 'write' on t: t is not your child"
    ), r


async def test_heritable_conferral_denial_is_exact(seeded_kernel):
    k = seeded_kernel
    _mk(k, "t")
    _mk(k, "a")
    r = (
        await _as(
            k,
            "a",
            "kernel",
            {
                "type": "create_agent",
                "parent_id": "a",
                "handler_module": MOD,
                "can_call": {"t": {"verbs": ["ping"], "for": "descendants"}},
            },
        )
    )["result"]
    assert r == {
        "error": (
            "a cannot confer a heritable 'ping' on t: you do not hold that "
            "heritable entry — only the operator, or a holder of the same entry, "
            "confers one (a subset of it)"
        ),
        "reason": "unauthorized",
        "side": "delegation",
        "hint": _FROM_ABOVE,
    }
    assert list(r) == ["error", "reason", "side", "hint"]


async def test_a_heritable_entry_never_grants_its_holder(seeded_kernel):
    k = seeded_kernel
    _mk(k, "t", callable_by={"$subtree:hub": "*"})
    _mk(k, "hub", can_call={"t": {"verbs": ["ping"], "for": "descendants"}})
    _mk(k, "kid", parent="hub")
    denied = (await _as(k, "hub", "t", {"type": "ping"}))["result"]
    assert (denied["reason"], denied["side"]) == ("unauthorized", "egress"), denied
    assert denied["error"] == "hub may not call 'ping' on t"
    ok = await _as(k, "kid", "t", {"type": "ping"})
    assert ok["status"] == "completed", ok
    # a verb the entry does not name is not inherited
    no = (await _as(k, "kid", "t", {"type": "pong"}))["result"]
    assert no["side"] == "egress", no


async def test_a_kernel_verb_is_egress_only(seeded_kernel):
    """`ps` to the root: egress decides; the root's `callable_by` is never asked."""
    k = seeded_kernel
    _mk(k, "a", can_call={"kernel_state": ["ps"]})
    k.ctx.update("kernel_state", callable_by={})
    out = await _as(k, "a", "kernel", {"type": "ps"})
    assert out["status"] == "completed" and "tasks" in out["result"], out
    r = (await _as(k, "a", "kernel", {"type": "ledger"}))["result"]
    assert r == {
        "error": "a may not call 'ledger' on kernel_state",
        "reason": "unauthorized",
        "hint": _FROM_ABOVE,
        "side": "egress",
    }
