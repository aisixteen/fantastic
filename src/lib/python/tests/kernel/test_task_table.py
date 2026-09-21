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
import sys
import types

# ─── a test bundle: real handler_module, resolved through sys.modules ───
#
# `Agent._dispatch` resolves the handler with `importlib.import_module`, which
# returns a module already in `sys.modules` — so a module object built here IS
# a bundle as far as the substrate is concerned. It gives us the two things the
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
        # `cancel`. Plain verbs — no privileged surface, no bundle coupling.
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
sys.modules["slow_agent.tools"] = _mod

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
    """`cancel` is a PLAIN VERB, so an agent wields it: the killer agent's own
    handler discovers the victim's task through `ps` and sends `cancel`. No UI,
    no privileged caller — the capability is in the substrate."""
    _hook_calls.clear()
    victim = await _mk(seeded_kernel, "slow_victim")
    killer = await _mk(seeded_kernel, "slow_killer")

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
    in-process callers."""
    victim = await _mk(seeded_kernel, "slow_victim")
    rec = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        **__import__("_testkit").py_runner_meta(),
                    },
                }
            ]
        )
    )[0]["result"]
    runner = rec["id"]

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


# ─── reflect advertises the substrate's own verbs ──────────────


async def test_every_reflect_advertises_the_kernel_verbs(seeded_kernel):
    """`kernel_verbs` is ADVERTISED everywhere and ANSWERED only by the root.

    Advertised everywhere: root-only advertisement was tried and reverted. It put
    the surface where it was least needed — gemma4:12b reflected the three agents
    it cared about (that is where the descriptions are), never reflected the root,
    never saw `update_agent`'s signature, guessed the args from neighbouring names,
    guessed wrong, and abandoned the task. Placement follows WHERE A CALLER LOOKS,
    not where the field is cheapest to attach (~115 tok, +11-17%).

    Answered by the root only (2026-08-24): these verbs act on the TREE and on the
    kernel-wide task table, so the agent they name is the OPERAND, not the actor.
    While every agent answered them, "send create_agent to X" could mean X performs
    it, X is created under something, or X is the parent — and only the last was
    true. A model asked "But how do I invoke create_agent? Which target?", never
    resolved it, and finished 16 messages with `agents_created: []`.

    So the two properties are deliberately different, and the `_` line is what
    reconciles them for a reader who only ever reflects one leaf."""
    r = (
        await seeded_kernel.send([{"target": "kernel", "payload": {"type": "reflect"}}])
    )[0]["result"]
    kv = r["kernel_verbs"]
    for v in ("ps", "cancel", "create_agent", "update_agent", "delete_agent"):
        assert v in kv
    assert "OPERAND" in kv["_"] and "kernel" in kv["_"]

    # Every verb carries its signature — the line gemma needed…
    assert "id:str" in kv["update_agent"], f"{kv['update_agent']!r}"
    assert "handler_module:str" in kv["create_agent"]
    assert "task_id:str" in kv["cancel"]
    # …including the read-only ones, which used to render as a bare name.
    assert kv["ps"] and kv["list_agents"], "a name with no signature is unusable"

    # BLAST RADIUS SURVIVES THE TRUNCATION. Distilled keeps only the text before
    # the first em-dash, and these are the facts a caller needs BEFORE firing.
    assert "REQUIRED" in kv["create_agent"], "the parent must be stated"
    assert "CASCADES" in kv["delete_agent"], "a subtree delete must say so"
    assert "SUBTREE" in kv["cancel"].upper()
    assert "REPLACED, NOT MERGED" in kv["update_agent"]

    full = (
        await seeded_kernel.send(
            [{"target": "kernel", "payload": {"type": "reflect", "detail": "full"}}]
        )
    )[0]["result"]
    fkv = full["kernel_verbs"]
    assert "task_id" in fkv["cancel"] and "subtree" in fkv["cancel"].lower()
    assert "tasks" in fkv["ps"]

    # A NON-ROOT agent ADVERTISES them — the whole point of the revert…
    leaf = (
        await seeded_kernel.send([{"target": "cli", "payload": {"type": "reflect"}}])
    )[0]["result"]
    assert "id:str" in leaf["kernel_verbs"]["update_agent"]
    assert leaf["kernel_verbs"]["ps"]
    # …and REFUSES them, naming where to go instead.
    denied = (await seeded_kernel.send([{"target": "cli", "payload": {"type": "ps"}}]))[
        0
    ]["result"]
    assert denied["reason"] == "not_here" and denied["verb"] == "ps"
    assert "kernel" in denied["hint"], f"a refusal must be actionable: {denied!r}"
    # `runtime` stays root-only — it describes the KERNEL, not what an agent answers.
    assert "runtime" not in leaf


async def test_system_verb_signatures_are_reachable_from_the_doc_tree(seeded_kernel):
    """THE TRIM'S PRECONDITION. Distilled drops the system-verb docstrings, so
    their arguments must be readable somewhere a caller can actually get to —
    otherwise `cancel`'s signature is unreachable at any price."""
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
    # And the distilled reply must POINT at it, or nobody finds the page.
    more = (
        (
            await seeded_kernel.send(
                [{"target": "kernel", "payload": {"type": "reflect"}}]
            )
        )[0]["result"]
    )["more"]
    assert "system-verbs" in more
