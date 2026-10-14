"""The TASK TABLE — `ps` + `cancel`, the kernel's process table.

The unit is a TASK (ONE in-flight dispatch), not an agent: `"<agent>:<n>"` is
a pid, and tasks form a TREE through the send chain (a `send` issued inside a
handler is a CHILD of that handler's task). `ps` reads the live table; `cancel`
kills a task AND ITS SUBTREE, like killing a process group.

Both are SYSTEM verbs — baked into the substrate, answered natively by every
agent, in no bundle's `verbs` map. So an AGENT can cancel another agent's work
(`test_agent_cancels_another_agents_task*`) — that is the point: cancellation is
a property of the one primitive, not a UI feature bolted onto one bundle.
"""

from __future__ import annotations

import asyncio
import types
from kernel import register_unlisted
from _testkit import wire

# ─── a test bundle: real handler_module, registered unlisted ───
#
# `Agent._dispatch` resolves the handler with `import_handler`, which answers a
# module `register_unlisted` named — so a module object built here IS a bundle
# as far as the substrate is concerned. It gives us the two things the
# real bundles have and the task table needs: a handler that STAYS in flight,
# and the optional `on_cancel` teardown hook.

_started: dict[str, asyncio.Event] = {}  # agent id → "my slow handler is running"
_hook_calls: list[tuple[str, str, bool]] = []  # (agent, task_id, task_still_alive)


def _run_flag(agent_id: str) -> asyncio.Event:
    return _started.setdefault(agent_id, asyncio.Event())


async def _slow_handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {"id": id, "sentence": "slow test agent", "verbs": {}}
    if verb == "slow":
        # In flight until someone kills us (or the test ends).
        _run_flag(id).set()
        await asyncio.sleep(30)
        return {"finished": True}  # never reached in these tests
    if verb == "chain":
        # A nested send — the callee's dispatch becomes a CHILD task of this one.
        return (
            await kernel.send(
                [{"target": payload["next"], "payload": {"type": "slow"}}]
            )
        )[0]["result"]
    if verb == "kill":
        # An AGENT cancelling another agent's work: discover via `ps`, then
        # `cancel`. Plain verbs — no special surface, no bundle coupling.
        table = (await kernel.send([{"target": "kernel", "payload": {"type": "ps"}}]))[
            0
        ]["result"]
        victim = [
            t
            for t in table["tasks"]
            if t["agent"] == payload["victim"] and t["verb"] == "slow"
        ]
        if not victim:
            return {"error": "no victim task"}
        return (
            await kernel.send(
                [
                    {
                        "target": "kernel",
                        "payload": {"type": "cancel", "task_id": victim[0]["id"]},
                    }
                ]
            )
        )[0]["result"]
    return {"error": f"unknown verb {verb!r}"}


async def _slow_on_cancel(id, task_id, kernel):
    """The optional bundle hook — SIGTERM half. Records that it ran BEFORE the
    abort (the asyncio task is still alive here), which is the whole contract:
    a bundle gets to release real resources while its handler still exists."""
    entry = kernel.ctx.tasks.get(task_id)
    alive = entry is not None and not entry["_task"].done()
    _hook_calls.append((id, task_id, alive))


_mod = types.ModuleType("slow_agent.tools")
_mod.handler = _slow_handler
_mod.on_cancel = _slow_on_cancel
_mod.VERBS = {}
register_unlisted("slow_agent.tools", _mod)

SLOW = "slow_agent.tools"


async def _mk(
    kernel,
    agent_id: str,
    can_call: dict | None = None,
    callable_by: dict | None = None,
) -> str:
    payload = {
        "type": "create_agent",
        "parent_id": kernel.id,
        "handler_module": SLOW,
        "id": agent_id,
    }
    # Two-sided deny-all: a `chain` send to a SIBLING needs BOTH the caller's
    # `can_call` (egress) and the callee's `callable_by` (ingress).
    if can_call is not None:
        payload["can_call"] = can_call
    if callable_by is not None:
        payload["callable_by"] = callable_by
    rec = (await kernel.send([{"target": "kernel_state", "payload": payload}]))[0][
        "result"
    ]
    return rec["id"]


async def _rows(kernel) -> list[dict]:
    return (
        (await kernel.send([{"target": "kernel", "payload": {"type": "ps"}}]))[0][
            "result"
        ]
    )["tasks"]


async def _row_for(kernel, agent: str, verb: str) -> dict | None:
    for t in await _rows(kernel):
        if t["agent"] == agent and t["verb"] == verb:
            return t
    return None


# ─── ps: the live table + the send TREE ────────────────────────


async def test_ps_shows_the_tree_of_a_nested_send(seeded_kernel):
    """A `send` issued INSIDE a handler is a CHILD of that handler's task. `ps`
    shows both rows with the parent link, so a caller sees who spawned what."""
    _hook_calls.clear()
    a = await _mk(seeded_kernel, "slow_a", can_call={"slow_b": ["slow"]})
    b = await _mk(seeded_kernel, "slow_b", callable_by={"slow_a": ["slow"]})

    caller = asyncio.create_task(
        seeded_kernel.send([{"target": a, "payload": {"type": "chain", "next": b}}])
    )
    await asyncio.wait_for(_run_flag(b).wait(), timeout=2)

    parent = await _row_for(seeded_kernel, a, "chain")
    child = await _row_for(seeded_kernel, b, "slow")
    assert parent is not None and child is not None
    # A task id IS the message id of the call that started it: an int from the
    # kernel's one counter, so `a < b` means "a was created first", kernel-wide.
    # The agent is a FIELD on the row, never parsed out of the id.
    for row, agent in ((parent, a), (child, b)):
        assert isinstance(row["id"], int) and row["agent"] == agent
    assert child["id"] > parent["id"], "the child call was minted after its parent"
    # The tree: b's dispatch is a child of a's dispatch. a's caller is the test
    # coroutine — not a dispatch — so a's task is a root (init-owned).
    assert child["parent"] == parent["id"]
    assert parent["parent"] is None
    assert set(parent) == {"id", "agent", "parent", "verb", "started_at"}
    assert isinstance(parent["started_at"], float)
    # `ps` shows its own row too (it IS a dispatch) — the table is honest.
    assert any(t["verb"] == "ps" for t in await _rows(seeded_kernel))

    caller.cancel()


# ─── cancel: the subtree dies, the caller learns ───────────────


async def test_cancel_kills_the_whole_subtree(seeded_kernel):
    """Cancelling A's task kills A AND the B dispatch A is blocked on — children
    first, like killing a process group. The blocked caller's `send` RESOLVES to
    a status:'cancelled' outcome; it never raises."""
    _hook_calls.clear()
    a = await _mk(seeded_kernel, "slow_a", can_call={"slow_b": ["slow"]})
    b = await _mk(seeded_kernel, "slow_b", callable_by={"slow_a": ["slow"]})

    caller = asyncio.create_task(
        seeded_kernel.send([{"target": a, "payload": {"type": "chain", "next": b}}])
    )
    await asyncio.wait_for(_run_flag(b).wait(), timeout=2)
    a_task = (await _row_for(seeded_kernel, a, "chain"))["id"]
    b_task = (await _row_for(seeded_kernel, b, "slow"))["id"]

    r = (
        await seeded_kernel.send(
            [{"target": "kernel", "payload": {"type": "cancel", "task_id": a_task}}]
        )
    )[0]["result"]
    assert r["cancelled"] is True
    # Leaves-up: the child is killed before the parent that awaits it.
    assert r["killed"] == [b_task, a_task]

    # The subtree is GONE from the table.
    live = {t["id"] for t in await _rows(seeded_kernel)}
    assert a_task not in live and b_task not in live

    # The blocked caller gets the cancellation as a REPLY.
    out = (await asyncio.wait_for(caller, timeout=2))[0]
    assert out["status"] == "cancelled" and out["result"] == {"cancelled": True}

    # The on_cancel hook ran for BOTH agents, each with its own task id, and in
    # both cases BEFORE the abort (the asyncio task was still alive).
    assert [(agent, tid) for agent, tid, _ in _hook_calls] == [
        (b, b_task),
        (a, a_task),
    ]
    assert all(alive for _, _, alive in _hook_calls)


async def test_cancel_unknown_or_finished_task_is_honest(seeded_kernel):
    """An unknown id — and an id that already finished — cancel NOTHING, and say
    so: {"cancelled": False, "killed": []}. No pretending, no raise."""
    a = await _mk(seeded_kernel, "slow_a")

    r = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {"type": "cancel", "task_id": "no_such_agent:7"},
                }
            ]
        )
    )[0]["result"]
    assert r == {"cancelled": False, "killed": []}

    # A task id that WAS live and is now finished (we just cancelled it).
    caller = asyncio.create_task(
        seeded_kernel.send([{"target": a, "payload": {"type": "slow"}}])
    )
    await asyncio.wait_for(_run_flag(a).wait(), timeout=2)
    task_id = (await _row_for(seeded_kernel, a, "slow"))["id"]
    first = (
        await seeded_kernel.send(
            [{"target": "kernel", "payload": {"type": "cancel", "task_id": task_id}}]
        )
    )[0]["result"]
    assert first["cancelled"] is True
    out = (await asyncio.wait_for(caller, timeout=2))[0]
    assert out["status"] == "cancelled" and out["result"] == {"cancelled": True}

    again = (
        await seeded_kernel.send(
            [{"target": "kernel", "payload": {"type": "cancel", "task_id": task_id}}]
        )
    )[0]["result"]
    assert again == {"cancelled": False, "killed": []}

    # A malformed call fails LOUD, it does not report a phantom success.
    bad = (
        await seeded_kernel.send([{"target": "kernel", "payload": {"type": "cancel"}}])
    )[0]["result"]
    assert "error" in bad


# ─── the emergent one: an AGENT cancels another agent's work ───


async def test_agent_cancels_another_agents_task(seeded_kernel):
    """An agent wields `cancel`: the killer agent's own handler discovers the
    victim's task through `ps` and sends `cancel`. Both are kernel verbs answered by
    the root, so the killer holds a grant naming them on the root, and the root's
    `callable_by` names the killer."""
    _hook_calls.clear()
    victim = await _mk(seeded_kernel, "slow_victim")
    killer = await _mk(
        seeded_kernel, "slow_killer", can_call={"kernel_state": ["ps", "cancel"]}
    )
    wire(seeded_kernel, killer, "kernel_state", ["ps", "cancel"])

    caller = asyncio.create_task(
        seeded_kernel.send([{"target": victim, "payload": {"type": "slow"}}])
    )
    await asyncio.wait_for(_run_flag(victim).wait(), timeout=2)
    victim_task = (await _row_for(seeded_kernel, victim, "slow"))["id"]

    r = (
        await seeded_kernel.send(
            [{"target": killer, "payload": {"type": "kill", "victim": victim}}]
        )
    )[0]["result"]
    assert r["cancelled"] is True
    assert r["killed"] == [victim_task]

    assert victim_task not in {t["id"] for t in await _rows(seeded_kernel)}
    out = (await asyncio.wait_for(caller, timeout=2))[0]
    assert out["status"] == "cancelled" and out["result"] == {"cancelled": True}


async def test_script_agent_cancels_another_agents_task(seeded_kernel):
    """The same capability from OUT OF PROCESS: an `exec_bridge` job — a real
    script, holding only the `kernel` connector — reads `ps`, picks the victim's
    task id, and sends `cancel`. Nothing about cancellation is special-cased for
    in-process callers; the runner holds the same explicit `ps`/`cancel` grant."""
    victim = await _mk(seeded_kernel, "slow_victim")
    rec = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        **__import__("_testkit").py_runner_meta(
                            can_call={"kernel_state": ["ps", "cancel"]}
                        ),
                    },
                }
            ]
        )
    )[0]["result"]
    runner = rec["id"]
    wire(seeded_kernel, runner, "kernel_state", ["ps", "cancel"])

    caller = asyncio.create_task(
        seeded_kernel.send([{"target": victim, "payload": {"type": "slow"}}])
    )
    await asyncio.wait_for(_run_flag(victim).wait(), timeout=2)
    victim_task = (await _row_for(seeded_kernel, victim, "slow"))["id"]

    code = (
        "table = kernel.send('kernel', {'type': 'ps'})\n"
        "mine = [t for t in table['tasks']"
        f" if t['agent'] == {victim!r} and t['verb'] == 'slow']\n"
        "r = kernel.send('kernel', {'type': 'cancel', 'task_id': mine[0]['id']})\n"
        "print('CANCELLED', mine[0]['id'], r['cancelled'], flush=True)\n"
    )
    started = (
        await seeded_kernel.send(
            [
                {
                    "target": runner,
                    "payload": __import__("_testkit").py_start_payload(code),
                }
            ]
        )
    )[0]["result"]
    assert "job_id" in started, started

    # The victim's caller unblocks with the cancellation reply — the script did it.
    out = (await asyncio.wait_for(caller, timeout=15))[0]
    assert out["status"] == "cancelled" and out["result"] == {"cancelled": True}
    assert victim_task not in {t["id"] for t in await _rows(seeded_kernel)}


# ─── the substrate's own verbs: `docs of=kernel`, never a reflect ──────


async def test_kernel_verbs_are_docs_not_reflect(seeded_kernel):
    """The kernel verbs are READ with `docs of=kernel` and ANSWERED only by the root
    (a reflect shows no verbs). They act on the TREE and on the
    kernel-wide task table, so the agent they name is the OPERAND, not the actor —
    a non-root agent refuses them, naming where to go."""
    for target in ("kernel", "cli"):
        r = (
            await seeded_kernel.send(
                [{"target": target, "payload": {"type": "reflect"}}]
            )
        )[0]["result"]
        for key in ("kernel_verbs", "verbs", "more", "emits", "cancel"):
            assert key not in r, (target, key)
    kv = (
        await seeded_kernel.send(
            [{"target": "kernel", "payload": {"type": "docs", "of": "kernel"}}]
        )
    )[0]["result"]["verbs"]
    for v in ("ps", "cancel", "create_agent", "update_agent", "delete_agent", "docs"):
        assert v in kv
    # Every oneliner is the signature line.
    assert "id:str" in kv["update_agent"], f"{kv['update_agent']!r}"
    assert "handler_module:str" in kv["create_agent"]
    assert "task_id:str" in kv["cancel"]
    assert kv["ps"] == "No args" and kv["list_agents"] == "No args"
    full = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {"type": "docs", "of": "kernel", "tier": "detailed"},
                }
            ]
        )
    )[0]["result"]["verbs"]
    assert "CASCADES" in full["delete_agent"]
    assert "REPLACED, NOT MERGED" in full["update_agent"]
    assert "subtree" in full["cancel"].lower()
    denied = (await seeded_kernel.send([{"target": "cli", "payload": {"type": "ps"}}]))[
        0
    ]["result"]
    assert denied["reason"] == "not_here" and denied["verb"] == "ps"
    assert "kernel" in denied["hint"], f"a refusal must be actionable: {denied!r}"


async def test_system_verb_signatures_are_reachable_from_the_doc_tree(seeded_kernel):
    """The system verbs' arguments are readable in the doc tree too, not only
    through `docs of=kernel`."""
    idx = (
        await seeded_kernel.send(
            [{"target": "kernel", "payload": {"type": "reflect", "readme": "index"}}]
        )
    )[0]["result"]
    assert "system-verbs" in {e["key"] for e in idx["readme"]["index"]}
    doc = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {"type": "reflect", "readme": "system-verbs"},
                }
            ]
        )
    )[0]["result"]
    body = doc["readme"]["body"]
    assert "task_id" in body and "handler_module" in body
