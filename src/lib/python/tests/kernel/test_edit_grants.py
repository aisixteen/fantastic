"""Atomic grant edits: `Kernel.edit_grants` (and its `edit_callable_by` /
`edit_can_call` wrappers) read both grant maps as they are at the write, run the
write-time checks once over what changed, and write nothing when refused.

Mirrors the Rust kernel's `edit_grants` tests (`fantastic-kernel`, PR #324).
"""

from __future__ import annotations

import types

from kernel import register_unlisted
from kernel._agent import Agent

_mod = types.ModuleType("edit_grants_probe.tools")


async def _handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    return {"ok": id, "verb": verb}


_mod.handler = _handler
_mod.VERBS = {}
register_unlisted("edit_grants_probe.tools", _mod)
MOD = "edit_grants_probe.tools"
AID = "grantee"


def _updated_events(events: list[dict]) -> list[dict]:
    return [e for e in events if e["kind"] == "updated" and e["agent_id"] == AID]


def test_edit_grants_merges_both_maps_in_one_step(seeded_kernel):
    seeded_kernel.create(MOD, id=AID)
    k = seeded_kernel.ctx
    events: list[dict] = []
    k.add_state_subscriber(events.append)

    def edit(cc, cb):
        cc["peer-a"] = ["ping"]
        cb["peer-b"] = ["ping"]
        return True, True

    changed, error = k.edit_grants(AID, edit)

    assert error is None, error
    assert changed is True
    rec = k.get(AID)
    assert rec["can_call"]["peer-a"] == ["ping"]
    assert rec["callable_by"]["peer-b"] == ["ping"]
    updated = _updated_events(events)
    assert len(updated) == 1, updated
    assert updated[0]["changed"] == ["can_call", "callable_by"]


def test_edit_grants_refusal_writes_neither(seeded_kernel):
    seeded_kernel.create(MOD, id=AID)
    k = seeded_kernel.ctx
    before_cc = dict(k.get(AID)["can_call"])
    before_cb = dict(k.get(AID)["callable_by"])
    events: list[dict] = []
    k.add_state_subscriber(events.append)

    def edit(cc, cb):
        cc["peer-ok"] = "ping"  # a bare string, not a verb list: refused
        cb["peer-g|ai"] = ["ping"]  # a bad token: refused
        return True, True

    expected = Agent._bad_grant_shape(
        {"can_call": {"peer-ok": "ping"}, "callable_by": {"peer-g|ai": ["ping"]}}
    )
    assert expected is not None
    changed, error = k.edit_grants(AID, edit)

    assert changed is False
    assert error == expected
    rec = k.get(AID)
    assert rec["can_call"] == before_cc
    assert rec["callable_by"] == before_cb
    assert _updated_events(events) == []


def test_edit_grants_no_change_publishes_nothing(seeded_kernel):
    seeded_kernel.create(MOD, id=AID)
    k = seeded_kernel.ctx
    events: list[dict] = []
    k.add_state_subscriber(events.append)

    changed, error = k.edit_grants(AID, lambda cc, cb: (False, False))
    assert (changed, error) == (False, None)
    assert _updated_events(events) == []

    changed, error = k.edit_callable_by(AID, lambda cb: False)
    assert (changed, error) == (False, None)
    assert _updated_events(events) == []

    changed, error = k.edit_grants("no_such_agent", lambda cc, cb: (True, True))
    assert (changed, error) == (False, "no such agent")
