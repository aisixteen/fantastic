"""JOINING a caller-scoped kernel verb's execution (owner-approved 2026-10-03,
audit KV-1).

`ledger` and `ack_deadletter` answer from WHO sends them — the executions that
caller originated. A join never re-runs the handler, so before the fix a joiner
was handed the original caller's view: `B`, granted `ledger`, joined `A`'s named
ledger read (or the operator's unscoped one, by its int pid) and received A's (or
every caller's) rows. Now a join of such an execution by any caller other than the
one that sent it is refused; the original caller still dedups, and cross-caller
joins of every other verb are unchanged."""

from __future__ import annotations

import types

from kernel import register_unlisted, sender_context

_mod = types.ModuleType("scoped_join_probe.tools")


async def _probe(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown", "reflect"):
        return {"id": id, "sentence": "scoped join probe", "verbs": {}}
    if verb == "work":
        return {"body": f"result-of-{payload.get('tag')}"}
    if verb == "boom":
        raise RuntimeError("boom")
    return {"error": f"unknown verb {verb!r}"}


_mod.handler = _probe
_mod.VERBS = {}
register_unlisted("scoped_join_probe.tools", _mod)


def _refused(exec_id: str, verb: str) -> dict:
    return {
        "error": f"send: id {exec_id} names another caller's {verb} execution — "
        f"{verb} answers only the caller that sent it, so it cannot be joined; "
        f"make your own {verb} call under a new id",
        "reason": "bad_arg",
        "arg": "id",
    }


async def _seed(k):
    """`A` and `B`, children of the root granted its caller-scoped verbs (and `ps`)
    by name — a wildcard never covers a kernel verb — and a `desk` both may call."""
    for aid in ("A", "B"):
        k.create(
            None,
            id=aid,
            can_call={"desk": "*", k.id: ["ledger", "ack_deadletter", "ps"]},
        )
    k.create("scoped_join_probe.tools", id="desk", callable_by={"A": "*", "B": "*"})


async def _send(k, item, sender=None):
    if sender is None:
        return (await k.send([item]))[0]
    with sender_context(sender):
        return (await k.send([item]))[0]


def _work() -> dict:
    """A NAMED call: the ledger keeps a named execution's result body."""
    return {"target": "desk", "payload": {"type": "work", "tag": "a"}, "id": "work-a"}


def _ledger(**extra) -> dict:
    return {"target": "kernel", "payload": {"type": "ledger"}, **extra}


async def test_a_foreign_join_of_a_named_ledger_read_is_refused(kernel):
    k = kernel
    await _seed(k)
    await _send(k, _work(), "A")
    own = await _send(k, _ledger(id="led-A"), "A")
    assert own["status"] == "completed", own
    assert "result-of-a" in repr(own["result"])

    by_name = await _send(k, _ledger(id="led-A"), "B")
    assert by_name["status"] == "failed", by_name
    assert by_name["result"] == _refused("'led-A'", "ledger")
    by_pid = await _send(k, _ledger(id=own["pid"]), "B")
    assert by_pid["status"] == "failed", by_pid
    assert by_pid["result"] == _refused(str(own["pid"]), "ledger")
    assert "result-of-a" not in repr([by_name, by_pid])

    # the original caller still dedups: the recorded view, the handler not re-run
    again = await _send(k, _ledger(id="led-A"), "A")
    assert again == own


async def test_an_agent_joining_the_operators_ledger_read_is_refused(kernel):
    k = kernel
    await _seed(k)
    await _send(k, _work(), "A")
    op = await _send(k, _ledger())
    assert "result-of-a" in repr(op["result"]), "the operator's read is unscoped"
    out = await _send(k, _ledger(id=op["pid"]), "B")
    assert out["result"] == _refused(str(op["pid"]), "ledger"), out
    assert "result-of-a" not in repr(out)


async def test_the_operator_joining_an_agents_ledger_read_is_refused(kernel):
    """The operator is a caller like any other here: its own read is unscoped, so a
    join of an agent's view is never what it would have been answered."""
    k = kernel
    await _seed(k)
    await _send(k, _ledger(id="led-A"), "A")
    out = await _send(k, _ledger(id="led-A"))
    assert out["result"] == _refused("'led-A'", "ledger"), out


async def test_a_foreign_join_of_an_ack_deadletter_is_refused(kernel):
    k = kernel
    await _seed(k)
    dead = await _send(k, {"target": "desk", "payload": {"type": "boom"}}, "A")
    assert dead["status"] == "failed", dead
    ack = {
        "target": "kernel",
        "payload": {"type": "ack_deadletter", "id": dead["pid"]},
        "id": "ack-A",
    }
    own = await _send(k, ack, "A")
    assert own["status"] == "completed" and own["result"]["acked"] is True, own
    out = await _send(k, ack, "B")
    assert out["result"] == _refused("'ack-A'", "ack_deadletter"), out


async def test_cross_caller_joins_of_other_verbs_stay_open(kernel):
    """`ps` answers the same table to everyone: a join of A's named `ps` by B is
    the once-only contract, unchanged."""
    k = kernel
    await _seed(k)
    own = await _send(
        k, {"target": "kernel", "payload": {"type": "ps"}, "id": "ps-A"}, "A"
    )
    joined = await _send(
        k, {"target": "kernel", "payload": {"type": "ps"}, "id": "ps-A"}, "B"
    )
    assert own["status"] == "completed" and joined == own, (own, joined)


async def test_a_recalled_ledger_read_refuses_a_foreign_join(tmp_path, monkeypatch):
    """After the flush the execution lives on as its store rows; recalled into RAM,
    it still knows its caller."""
    from _testkit import boot_root

    monkeypatch.chdir(tmp_path)
    k = boot_root()
    await k.send(
        [
            {
                "target": "kernel",
                "payload": {
                    "type": "create_agent",
                    "parent_id": k.id,
                    "handler_module": "ram_store.tools",
                    "id": "memory",
                    "operator": True,
                    "admins": ["kernel"],
                },
            }
        ]
    )
    k.ctx.update(k.id, memory_store_id="memory")
    assert (await k.ctx.start(boot_all=False)).get("started")
    await _seed(k)
    await _send(k, _work(), "A")
    own = await _send(k, _ledger(id="led-A"), "A")
    await k.ctx.flush_executions()
    assert own["pid"] not in k.ctx.executions, "the read must have left RAM"

    out = await _send(k, _ledger(id="led-A"), "B")
    assert out["result"] == _refused("'led-A'", "ledger"), out
    again = await _send(k, _ledger(id="led-A"), "A")
    assert again["status"] == "completed", again
    assert again["result"] == own["result"]
