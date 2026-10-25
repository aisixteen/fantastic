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
# twin: a_peer_address_in_callable_by_is_refused_at_the_write
# twin: an_edit_the_write_time_checks_refuse_writes_nothing
# twin: a_refused_can_call_shape_writes_nothing
# twin: edit_grants_refused_in_one_map_writes_neither


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


def test_edit_grants_publishes_one_updated_event_only_when_something_changed(seeded_kernel):
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


async def test_edit_grants_persists_both_keys_across_a_restart(tmp_path, monkeypatch):
    """Two grant edits on a persisted agent reach `agent.yaml` through the record
    writer on their own (before any shutdown flush could write them regardless),
    and a rebooted kernel still has both grants."""
    import yaml

    from _testkit import boot_root
    from kernel_state.tools import on_shutdown

    monkeypatch.chdir(tmp_path)
    root = boot_root()
    root.create(
        "file_bridge.tools",
        allow=["."],
        id="store",
        root=".fantastic",
        ingress_rule="allow_all_ingress",
    )
    (await root.send([{"target": root.id, "payload": {"type": "boot"}}]))[0]
    try:
        root.create(MOD, id="child_m")
        # settle the create's own write, so only the edits can make it dirty again
        await root._fs_flush_loop.aflush()
        k = root.ctx
        edited = k.edit_callable_by(
            "child_m", lambda cb: cb.update(view_a=["*"]) or True
        )
        assert edited == (True, None), edited
        edited = k.edit_callable_by(
            "child_m", lambda cb: cb.update(view_b=["*"]) or True
        )
        assert edited == (True, None), edited
        await root._fs_flush_loop.aflush()
        # on disk NOW, before any shutdown flush could persist it regardless
        path = tmp_path / ".fantastic" / "agents" / "child_m" / "agent.yaml"
        on_disk = yaml.safe_load(path.read_text(encoding="utf-8"))
        assert on_disk["callable_by"]["view_a"] == ["*"], on_disk
        assert on_disk["callable_by"]["view_b"] == ["*"], on_disk
    finally:
        await on_shutdown(root)

    del root
    rebooted = boot_root()
    cb = rebooted.ctx.get("child_m")["callable_by"]
    assert cb["view_a"] == ["*"] and cb["view_b"] == ["*"], cb
