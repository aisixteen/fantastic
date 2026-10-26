"""Every call-gate denial, pinned WHOLE: its text, `reason`, `side`, `hint`, the
extra facts it carries, and the key order of the dict.

The ports diff these replies, and a model reads the text to repair its call.
The tree verbs' reach is pinned in `test_minimal_gate.py`.
"""

from __future__ import annotations

import types

from kernel import register_unlisted

from _testkit import as_agent
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
    with as_agent(k, sender):
        return (await k.send([{"target": target, "payload": payload}]))[0]


# ─── egress / ingress on an ordinary call ────────────────────────────────────
# twin: absent_grant_denies_everything
# twin: the_denial_hint_is_the_from_above_text
# twin: gate_denial_keys_are_error_reason_hint_side


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


async def test_a_remote_target_is_egress_only_with_its_qualified_hint(seeded_kernel):
    k = seeded_kernel
    k._meta["guid"] = "h0"

    async def call(agent_id, payload):
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
