"""cabinet bundle — the manager, its cabinets and their workers, on a real kernel.

Every caller acts through `_as_self` (the test standing in for the agent's own
dispatch), so each send is gated as that agent: the master opens cabinets, a worker
messages a peer or creates equipment, and the gate refuses what the grants do not
allow. The operator wires the manager once, as the readme says.
"""

from __future__ import annotations

import pytest

from _testkit import memory_meta

MANAGER = "cabinet_manager"
MASTER = "master"
VERBS = ["create_agent", "update_agent", "delete_agent"]


@pytest.fixture
async def world(seeded_kernel):
    """A kernel with the operator's wiring: a MASTER AI, a MANAGER that may create,
    update and delete on the root, and the root that names the manager."""
    k = seeded_kernel
    root = k.id
    # the operator's writes are ungated: no sender
    await k.send(
        [
            {
                "target": root,
                "payload": {
                    "type": "create_agent",
                    "parent_id": root,
                    "id": MANAGER,
                    "handler_module": "cabinet.tools",
                    "cabinet_role": "manager",
                    "can_call": {"$self": "*", root: VERBS},
                    "callable_by": {"$self": "*", MASTER: "*"},
                },
            }
        ]
    )
    memory = memory_meta(k, MASTER, {MANAGER: "*"})
    await k.send(
        [
            {
                "target": root,
                "payload": {
                    "type": "create_agent",
                    "parent_id": root,
                    "id": MASTER,
                    "handler_module": "ollama_backend.tools",
                    "endpoint": "http://ollama.test",
                    "model": "model-a",
                    "context_window": 8000,
                    **memory,
                },
            },
            {
                "target": root,
                "payload": {
                    "type": "update_agent",
                    "id": root,
                    "callable_by": {"$self": "*", MANAGER: VERBS},
                },
            },
        ]
    )
    assert k.get(MASTER) is not None and k.get(MANAGER) is not None
    return k


async def as_(k, sender: str, target: str, payload: dict) -> dict:
    """One send, made by `sender` as its own dispatch (gated as that agent)."""
    agent = k.ctx.agents[sender]
    with agent._as_self():
        (out,) = await agent.send([{"target": target, "payload": payload}])
    return out.get("result") or {"error": out.get("status")}


async def open_cabinet(k, task: str, **kw) -> dict:
    return await as_(k, MASTER, MANAGER, {"type": "open_cabinet", "task": task, **kw})


def worker_of(k, cab: str) -> str:
    return k.get(cab)["worker"]


# ─── wiring ──────────────────────────────────────────────────────


async def test_open_wires_every_worker_to_every_peer_and_the_master(world):
    k = world
    names = ["alpha", "beta", "gamma"]
    cabs = []
    for name in names:
        res = await open_cabinet(k, f"task {name}", name=name)
        assert not res.get("error"), res
        cabs.append(res["id"])
    workers = [worker_of(k, c) for c in cabs]
    assert len(set(workers)) == 3
    for i, w in enumerate(workers):
        rec = k.get(w)
        peers = [x for x in workers if x != w]
        for p in peers:
            # egress and ingress both name every peer
            assert rec["can_call"][p] == "*", (w, rec["can_call"])
            assert rec["callable_by"][p] == "*", (w, rec["callable_by"])
        assert rec["can_call"][MASTER] == "*" and rec["callable_by"][MASTER] == "*"
        # the directory lists exactly its peers, with their tasks
        directory = {d["worker"]: d["task"] for d in rec["cabinet_peers"]}
        assert set(directory) == set(peers)
        for j, p in enumerate(workers):
            if p != w:
                assert directory[p] == f"task {names[j]}"
                assert p in rec["post_system_prompt"]
        assert f"cabinet `{cabs[i]}`" in rec["post_system_prompt"]
        assert "one of 3 cooperating workers" in rec["post_system_prompt"]
    # the master reaches each worker, and the manager monitors them
    assert k.get(MASTER)["can_call"][workers[0]] == "*"
    assert k.get(MASTER)["callable_by"][workers[2]] == "*"
    assert k.get(MANAGER)["can_call"][workers[1]] == "*"


async def test_close_unwires_the_peers_and_leaves_nothing_behind(world):
    k = world
    cabs = [(await open_cabinet(k, f"t{i}", name=f"c{i}"))["id"] for i in range(3)]
    workers = [worker_of(k, c) for c in cabs]
    gone_cab, gone_worker = cabs[1], workers[1]
    res = await as_(k, MASTER, MANAGER, {"type": "close_cabinet", "id": gone_cab})
    assert not res.get("error"), res
    assert {gone_cab, gone_worker} <= set(res["deleted"])
    assert k.get(gone_cab) is None and k.get(gone_worker) is None
    for w in (workers[0], workers[2]):
        rec = k.get(w)
        assert (
            gone_worker not in rec["can_call"] and gone_worker not in rec["callable_by"]
        )
        assert gone_worker not in [d["worker"] for d in rec["cabinet_peers"]]
        assert len(rec["cabinet_peers"]) == 1
    assert gone_worker not in k.get(MASTER)["can_call"]
    assert gone_worker not in k.get(MASTER)["callable_by"]


async def test_close_refuses_an_agent_that_is_not_a_cabinet_of_this_manager(world):
    res = await as_(world, MASTER, MANAGER, {"type": "close_cabinet", "id": MASTER})
    assert res.get("reason") == "unknown_agent", res


# ─── the default backend ─────────────────────────────────────────


async def test_the_default_backend_is_the_existing_master(world):
    k = world
    res = await open_cabinet(k, "solve part one")
    assert not res.get("error"), res
    assert res["backend"] == MASTER and res["new_backend"] is False
    assert "warning" not in res
    worker, master = k.get(res["worker"]), k.get(MASTER)
    assert worker["handler_module"] == master["handler_module"]
    assert (
        worker["endpoint"] == master["endpoint"] and worker["model"] == master["model"]
    )
    assert worker["memory_store_id"] == master["memory_store_id"]
    assert worker["parent_id"] == res["id"]


async def test_setup_over_the_backend_is_flagged_as_a_new_backend(world):
    k = world
    res = await open_cabinet(
        k, "a different endpoint", setup={"endpoint": "http://other.test"}
    )
    assert not res.get("error"), res
    assert res["new_backend"] is True
    assert "discouraged" in res["warning"]
    assert k.get(res["worker"])["endpoint"] == "http://other.test"


async def test_open_refuses_loudly_on_bad_inputs(world):
    k = world
    assert (await open_cabinet(k, "  "))["reason"] == "missing_arg"
    assert (await open_cabinet(k, "x", backend="nobody"))["reason"] == "unknown_agent"
    res = await open_cabinet(k, "x", backend=MANAGER)
    assert res["reason"] == "bad_arg" and "not an AI backend" in res["error"]
    res = await open_cabinet(k, "x", setup={"can_call": {"*": "*"}})
    assert res["reason"] == "bad_arg" and "wiring" in res["error"]
    # an operator send has no master to wire to, and makes nothing
    before = set(k.ctx.agents)
    (op,) = await k.send(
        [{"target": MANAGER, "payload": {"type": "open_cabinet", "task": "x"}}]
    )
    assert "master" in op["result"]["error"]
    assert set(k.ctx.agents) == before


# ─── the ceiling ─────────────────────────────────────────────────


async def test_the_ceiling_refuses_loudly_and_makes_nothing(world):
    k = world
    k.update(MANAGER, max_cabinets=2)
    assert not (await open_cabinet(k, "one", name="one")).get("error")
    assert not (await open_cabinet(k, "two", name="two")).get("error")
    before = set(k.ctx.agents)
    res = await open_cabinet(k, "three", name="three")
    assert res["reason"] == "ceiling" and res["ceiling"] == 2 and res["open"] == 2
    assert "ceiling is 2" in res["error"]
    assert set(k.ctx.agents) == before


# ─── peer messages and equipment ─────────────────────────────────


async def test_a_worker_messages_a_peer_and_the_peer_answers(world):
    k = world
    a = worker_of(k, (await open_cabinet(k, "a", name="a"))["id"])
    b = worker_of(k, (await open_cabinet(k, "b", name="b"))["id"])
    res = await as_(k, a, b, {"type": "reflect"})
    assert not res.get("error"), res
    assert res.get("sentence")


async def test_equipment_is_reached_by_its_own_worker_and_refused_to_a_peer(world):
    k = world
    a = worker_of(k, (await open_cabinet(k, "a", name="a"))["id"])
    b = worker_of(k, (await open_cabinet(k, "b", name="b"))["id"])
    made = await as_(
        k,
        a,
        "kernel_state",
        {
            "type": "create_agent",
            "parent_id": a,
            "id": "a_exec",
            "handler_module": "file_bridge.tools",
            "allow": ["."],
            "callable_by": {a: "*"},
        },
    )
    assert not made.get("error"), made
    assert k.ctx.agents["a_exec"].parent.id == a
    # the worker reaches its equipment through its `$subtree` grant
    assert not (await as_(k, a, "a_exec", {"type": "reflect"})).get("error")
    # the peer does not: the equipment grants its own worker and nothing else
    refused = await as_(k, b, "a_exec", {"type": "reflect"})
    assert refused.get("reason") == "unauthorized", refused


async def test_a_worker_cannot_reach_a_peers_status_or_the_manager(world):
    k = world
    a_cab = (await open_cabinet(k, "a", name="a"))["id"]
    b_cab = (await open_cabinet(k, "b", name="b"))["id"]
    a = worker_of(k, a_cab)
    assert (await as_(k, a, b_cab, {"type": "status", "line": "hijack"})).get(
        "reason"
    ) == "unauthorized"
    assert (await as_(k, a, MANAGER, {"type": "cabinets"})).get(
        "reason"
    ) == "unauthorized"
    # its own cabinet's status is its one channel up
    assert (await as_(k, a, a_cab, {"type": "status", "line": "mine"})).get(
        "ok"
    ) is True


# ─── status up to the master's view ──────────────────────────────


async def test_status_reaches_the_masters_monitor(world):
    k = world
    cab = (await open_cabinet(k, "report me", name="rep"))["id"]
    w = worker_of(k, cab)
    res = await as_(
        k,
        w,
        cab,
        {"type": "status", "line": "halfway", "progress": 50, "result": {"n": 3}},
    )
    assert res == {"ok": True, "cabinet": cab}, res
    listed = await as_(k, MASTER, MANAGER, {"type": "cabinets"})
    (row,) = [r for r in listed["cabinets"] if r["id"] == cab]
    assert row["status"]["line"] == "halfway"
    assert row["status"]["progress"] == 50 and row["status"]["result"] == {"n": 3}
    view = await as_(k, MASTER, cab, {"type": "view"})
    assert view["status"]["line"] == "halfway" and view["worker"] == w


async def test_status_refuses_bad_fields_loudly(world):
    k = world
    cab = (await open_cabinet(k, "bad", name="bad"))["id"]
    w = worker_of(k, cab)
    assert (await as_(k, w, cab, {"type": "status"}))["reason"] == "bad_arg"
    assert (await as_(k, w, cab, {"type": "status", "line": "x", "progress": 101}))[
        "reason"
    ] == "bad_arg"
    assert (await as_(k, w, cab, {"type": "status", "line": "x", "progress": True}))[
        "reason"
    ] == "bad_arg"
    assert k.get(cab)["status"]["line"] == ""


# ─── conclusion ──────────────────────────────────────────────────


async def test_conclude_removes_every_cabinet_worker_and_equipment(world):
    k = world
    cabs = [(await open_cabinet(k, f"t{i}", name=f"n{i}"))["id"] for i in range(2)]
    workers = [worker_of(k, c) for c in cabs]
    made = await as_(
        k,
        workers[0],
        "kernel_state",
        {
            "type": "create_agent",
            "parent_id": workers[0],
            "id": "n0_exec",
            "handler_module": "file_bridge.tools",
            "allow": ["."],
            "callable_by": {workers[0]: "*"},
        },
    )
    assert not made.get("error"), made
    res = await as_(k, MASTER, MANAGER, {"type": "conclude"})
    assert not res.get("error"), res
    assert sorted(res["concluded"]) == sorted(cabs)
    assert {*cabs, *workers, "n0_exec"} <= set(res["deleted"])
    for gone in [*cabs, *workers, "n0_exec"]:
        assert gone not in k.ctx.agents, gone
    assert k.get(MANAGER) is not None and k.get(MASTER) is not None
    assert not any(
        w in k.get(MASTER)["can_call"] or w in k.get(MASTER)["callable_by"]
        for w in workers
    )
    assert (await as_(k, MASTER, MANAGER, {"type": "cabinets"}))["count"] == 0


async def test_reflect_tells_the_role_and_the_ceiling(world):
    k = world
    k.update(MANAGER, max_cabinets=4)
    m = await as_(k, MASTER, MANAGER, {"type": "reflect"})
    assert m["role"] == "manager" and m["max_cabinets"] == 4 and m["open"] == 0
    cab = (await open_cabinet(k, "x", name="x"))["id"]
    c = await as_(k, MASTER, cab, {"type": "reflect"})
    assert c["role"] == "cabinet" and c["worker"] == worker_of(k, cab)


async def test_an_agent_without_a_cabinet_role_refuses_every_verb(world):
    k = world
    (made,) = await k.send(
        [
            {
                "target": k.id,
                "payload": {
                    "type": "create_agent",
                    "parent_id": k.id,
                    "id": "loose",
                    "handler_module": "cabinet.tools",
                },
            }
        ]
    )
    assert "error" not in made["result"], made
    (out,) = await k.send([{"target": "loose", "payload": {"type": "reflect"}}])
    assert (
        out["result"]["reason"] == "unhandled"
        and "no cabinet_role" in out["result"]["error"]
    )


async def test_each_cabinet_is_a_live_html_panel_of_its_own_id(world):
    k = world
    cab = (await open_cabinet(k, "panel", name="panel"))["id"]
    view = k.get(cab)["view"]
    assert view["kind"] == "html"
    assert f'const ID = "{cab}";' in view["bytes"]
    assert "__CABINET_ID__" not in view["bytes"]
    assert "fantastic.watch(ID" in view["bytes"]
    assert set(view["calls"]) == {"view", "watch"}
