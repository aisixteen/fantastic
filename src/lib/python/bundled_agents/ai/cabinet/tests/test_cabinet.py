"""cabinet bundle — the manager, its cabinets and their workers, on a real kernel.

Every caller acts through `_as_self` (the test standing in for the agent's own
dispatch), so each send is gated as that agent: the master opens cabinets, a worker
messages a peer or creates equipment, and the gate refuses what the grants do not
allow. The operator wires the manager once, as the readme says.
"""

from __future__ import annotations

import asyncio
import re
import types

import pytest

from _testkit import memory_meta
from kernel import register_unlisted

MANAGER = "cabinet_manager"
MASTER = "master"
CANVAS = "canvas_main"
VERBS = ["create_agent", "update_agent", "delete_agent"]
MESSAGES = ["send", "reflect"]
STATUS = "status"
CANVAS_VERBS = ["create", "place"]
ROLE = "cabinet_role"

# ─── the canvas that spawns the cabinets (a stub of the Rust host's canvas node) ──
#
# A canvas node answers `new_canvas` (a CHILD canvas of itself), `create` (a child
# agent of itself: equipment) and `place` (a view, here an acknowledgement). The
# children are wired to the root the way the host wires its canvases: root `create_agent`
# for the canvas, granted on the root's `callable_by`.

_canvas = types.ModuleType("canvas_stub.tools")


async def _grant_canvas(kernel, node: str) -> None:
    """The root grants a new canvas `create_agent` (its `callable_by`, its `can_call`)."""
    root = kernel.ctx.root.id
    cb = dict(kernel.get(root).get("callable_by") or {})
    cb[node] = ["create_agent"]
    (out,) = await kernel.send(
        [
            {
                "target": root,
                "payload": {"type": "update_agent", "id": root, "callable_by": cb},
            }
        ]
    )
    assert not out["result"].get("error"), out
    (out,) = await kernel.send(
        [
            {
                "target": root,
                "payload": {
                    "type": "update_agent",
                    "id": node,
                    "can_call": {"$self": "*", root: ["create_agent"]},
                },
            }
        ]
    )
    assert not out["result"].get("error"), out


async def _canvas_handler(id, payload, kernel):
    verb = payload.get("type")
    root = kernel.ctx.root.id
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {"id": id, "sentence": "stub canvas"}
    if verb == "new_canvas":
        slug = re.sub(r"[^a-z0-9]+", "_", payload["name"].lower()).strip("_")
        node = f"canvas_{slug}"
        (out,) = await kernel.send(
            [
                {
                    "target": root,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": id,
                        "id": node,
                        "handler_module": "canvas_stub.tools",
                        "can_call": {"$self": "*"},
                        "callable_by": {"$self": "*"},
                    },
                }
            ]
        )
        if out["result"].get("error"):
            return {"error": out["result"]["error"]}
        await _grant_canvas(kernel, node)
        return {"id": node}
    if verb == "create":
        record = dict(payload["record"])
        store = record.get("memory_store_id")
        if store is not None:
            # an AI made on the canvas reaches its store, and the store admits it: the
            # host's `make_ai` path (backend.rs), which every AI create takes
            record["can_call"] = {**(record.get("can_call") or {}), store: "*"}
        (out,) = await kernel.send(
            [
                {
                    "target": root,
                    "payload": {"type": "create_agent", "parent_id": id, **record},
                }
            ]
        )
        if store is not None and not out["result"].get("error"):
            await _admit_to_store(kernel, store, record["id"])
        return out["result"]
    if verb == "place":
        return {"placed": True}
    return {"error": f"canvas stub: unknown type {verb!r}"}


async def _admit_to_store(kernel, store: str, ai: str) -> None:
    """The store's side of the gate: its `callable_by` names the new AI (`admit_to_memory`).
    Made as the root, the store's parent (the host makes it as `user`, its parent too):
    grants come from above, never from the canvas that made the AI."""
    root = kernel.ctx.root
    cb = dict(kernel.get(store).get("callable_by") or {})
    cb[ai] = "*"
    with root._as_self():
        (out,) = await root.send(
            [
                {
                    "target": root.id,
                    "payload": {"type": "update_agent", "id": store, "callable_by": cb},
                }
            ]
        )
    assert not out["result"].get("error"), out


_canvas.handler = _canvas_handler
_canvas.VERBS = {}
register_unlisted("canvas_stub.tools", _canvas)


@pytest.fixture
async def world(seeded_kernel):
    """A kernel with the operator's wiring: a MASTER AI, a MANAGER that may create,
    update and delete on the root, the root that names the manager, and the CANVAS that
    spawns the cabinets (the manager's `canvas_id`)."""
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
                    "id": CANVAS,
                    "handler_module": "canvas_stub.tools",
                    "can_call": {"$self": "*", root: ["create_agent", "update_agent"]},
                    "callable_by": {"$self": "*"},
                },
            },
            {
                "target": root,
                "payload": {
                    "type": "create_agent",
                    "parent_id": root,
                    "id": MANAGER,
                    "handler_module": "cabinet.tools",
                    "cabinet_role": "manager",
                    "canvas_id": CANVAS,
                    "can_call": {"$self": "*", root: VERBS},
                    "callable_by": {"$self": "*", MASTER: "*"},
                },
            },
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
                    "callable_by": {
                        "$self": "*",
                        MANAGER: VERBS,
                        CANVAS: ["create_agent", "update_agent"],
                    },
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


async def test_each_cabinets_canvas_is_a_child_of_the_canvas_that_spawns_it(world):
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
            # egress and ingress both name every peer, for messages and nothing else
            assert rec["can_call"][p] == MESSAGES, (w, rec["can_call"])
            assert rec["callable_by"][p] == MESSAGES, (w, rec["callable_by"])
        # the master calls the worker in full; the worker only messages the master
        assert rec["can_call"][MASTER] == MESSAGES
        assert rec["callable_by"][MASTER] == "*"
        # the directory lists exactly its peers, with their tasks
        directory = {d["worker"]: d["task"] for d in rec["cabinet_peers"]}
        assert set(directory) == set(peers)
        for j, p in enumerate(workers):
            if p != w:
                assert directory[p] == f"task {names[j]}"
                assert p in rec["post_system_prompt"]
        assert f"cabinet `{cabs[i]}`" in rec["post_system_prompt"]
        assert "one of 3 cooperating workers" in rec["post_system_prompt"]
    # the master reaches each worker in full, and the manager monitors them
    assert k.get(MASTER)["can_call"][workers[0]] == "*"
    assert k.get(MASTER)["callable_by"][workers[2]] == MESSAGES
    assert k.get(MANAGER)["can_call"][workers[1]] == "*"
    # the worker holds NOTHING on the root; it places on its own cabinet's canvas only
    root = k.ctx.root.id
    for cab, w in zip(cabs, workers):
        assert root not in k.get(w)["can_call"]
        assert w not in k.get(root)["callable_by"]
        canvas = k.get(cab)["canvas"]
        assert k.get(w)["can_call"][canvas] == CANVAS_VERBS
        assert k.get(canvas)["callable_by"][w] == CANVAS_VERBS
        assert k.ctx.agents[canvas].parent.id == CANVAS


async def test_close_unwires_the_peers_and_leaves_nothing_behind(world):
    k = world
    cabs = [(await open_cabinet(k, f"t{i}", name=f"c{i}"))["id"] for i in range(3)]
    workers = [worker_of(k, c) for c in cabs]
    gone_cab, gone_worker = cabs[1], workers[1]
    gone_canvas = k.get(gone_cab)["canvas"]
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
    # the closed cabinet's canvas goes with it, and the kernel prunes its grants
    assert gone_canvas not in k.ctx.agents
    assert gone_canvas not in k.get(CANVAS)["callable_by"] if k.get(CANVAS) else True


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


async def test_a_worker_messages_a_peer_and_the_peer_receives_it(
    world, fake_providers
):
    k = world
    a = worker_of(k, (await open_cabinet(k, "a", name="a"))["id"])
    b = worker_of(k, (await open_cabinet(k, "b", name="b"))["id"])
    peer = _Script(["heard you"])
    fake_providers[b] = peer
    res = await as_(k, a, b, {"type": "send", "text": "peer hello from a"})
    assert not res.get("error"), res
    # the peer's turn saw the text, under the kernel's own `from <sender>` header
    assert peer.calls == 1
    assert "peer hello from a" in peer.incoming[0], peer.incoming
    assert f"[from {a}" in peer.incoming[0], peer.incoming
    # and the peer's conversation keeps the turn it heard
    (found,) = await k.send(
        [{"target": b, "payload": {"type": "recall", "query": "peer hello"}}]
    )
    assert any("peer hello from a" in m["content"] for m in found["result"]["messages"])


class _Script:
    """A scripted mind: answers each turn with the next queued reply."""

    def __init__(self, turns):
        self.turns = list(turns)
        self.calls = 0
        self.incoming: list[str] = []

    async def chat(self, messages, max_tokens=None):
        self.calls += 1
        self.incoming.append(messages[-1]["content"])
        yield self.turns.pop(0) if self.turns else "…"


async def test_a_worker_reaches_a_peer_only_with_messages(world):
    k = world
    a = worker_of(k, (await open_cabinet(k, "a", name="a"))["id"])
    b = worker_of(k, (await open_cabinet(k, "b", name="b"))["id"])
    # the peer answers `reflect`, and nothing that changes it
    assert not (await as_(k, a, b, {"type": "reflect"})).get("error")
    refused = await as_(k, a, b, {"type": "stop"})
    assert refused.get("reason") == "unauthorized", refused
    assert refused.get("side") == "egress", refused
    # and the peer may not stop, clear or re-task its neighbour either
    assert (await as_(k, b, a, {"type": "clear"})).get("reason") == "unauthorized"


async def test_a_worker_reaches_its_master_only_with_messages(world):
    k = world
    cab = (await open_cabinet(k, "a", name="a"))["id"]
    w = worker_of(k, cab)
    assert (await as_(k, w, MASTER, {"type": "reflect"})).get("error") is None
    assert (await as_(k, w, MASTER, {"type": "stop"})).get("reason") == "unauthorized"
    # the master calls its worker in full (monitor, re-task, stop)
    assert not (await as_(k, MASTER, w, {"type": "reflect"})).get("error")


async def create_on(
    k,
    canvas: str,
    sender: str,
    *,
    handler_module: str,
    id=None,
    setup=None,
    callable_by=None,
) -> dict:
    """The worker's `create` on its cabinet's canvas (as the worker): equipment, a child of the canvas."""
    record = {"handler_module": handler_module, **(setup or {})}
    if id is not None:
        record["id"] = id
    if callable_by is not None:
        record["callable_by"] = callable_by
    return await as_(k, sender, canvas, {"type": "create", "record": record})


async def test_a_worker_no_longer_creates_on_the_root(world):
    k = world
    a = worker_of(k, (await open_cabinet(k, "a", name="a"))["id"])
    # the direct route is closed: the worker's own create_agent on the root is refused
    refused = await as_(
        k,
        a,
        k.id,
        {
            "type": "create_agent",
            "parent_id": a,
            "id": "sneak",
            "handler_module": "file_bridge.tools",
        },
    )
    assert (
        refused.get("reason") == "unauthorized" and refused.get("side") == "egress"
    ), refused
    assert "sneak" not in k.ctx.agents


async def test_a_worker_holds_no_verb_on_the_root(world):
    k = world
    a = worker_of(k, (await open_cabinet(k, "a", name="a"))["id"])
    for verb in ("create_agent", "update_agent", "delete_agent"):
        refused = await as_(
            k,
            a,
            k.id,
            {
                "type": verb,
                "id": MASTER,
                "description": "x",
                "parent_id": a,
                "handler_module": "file_bridge.tools",
            },
        )
        assert refused.get("reason") == "unauthorized", (verb, refused)
    assert k.get(MASTER) is not None


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


async def test_status_is_emitted_live_on_the_cabinet_id(world):
    k = world
    cab = (await open_cabinet(k, "live", name="live"))["id"]
    w = worker_of(k, cab)
    # a watcher of the cabinet's id: the view draws from exactly these events
    k.ctx.ensure_inbox("watch_live")
    k.watch(cab, "watch_live")
    q = k.ctx.inboxes["watch_live"]
    st = {"line": "compiling", "progress": 40, "result": None}
    await as_(k, w, cab, {"type": "status", **st})
    await asyncio.sleep(0)
    events = []
    while not q.empty():
        events.append(q.get_nowait())
    (update,) = [e for e in events if e.get("type") == "update"]
    assert update["cabinet"] == cab
    assert update["status"]["line"] == "compiling"
    assert update["status"]["progress"] == 40
    assert isinstance(update["status"]["at"], float)


# ─── the manager's branches ──────────────────────────────────────


@pytest.mark.parametrize("bad_value", [0, -1, True, "2", 2.5])
async def test_a_bad_ceiling_is_refused_loudly(world, bad_value):
    k = world
    k.update(MANAGER, max_cabinets=bad_value)
    before = set(k.ctx.agents)
    res = await open_cabinet(k, "x", name="x")
    assert res["reason"] == "bad_arg" and res["arg"] == "max_cabinets", res
    assert set(k.ctx.agents) == before


async def test_a_worker_on_a_missing_memory_store_is_refused(world):
    k = world
    before = set(k.ctx.agents)
    res = await open_cabinet(
        k, "x", name="x", setup={"memory_store_id": "no_such_store"}
    )
    assert res["reason"] == "unknown_agent" and res["id"] == "no_such_store", res
    assert set(k.ctx.agents) == before


async def test_a_worker_on_a_missing_credential_store_is_refused(world):
    k = world
    k.update(MASTER, credentials={"store": "no_keys", "ref": "api"})
    before = set(k.ctx.agents)
    res = await open_cabinet(k, "x", name="x")
    assert res["reason"] == "unknown_agent" and res["id"] == "no_keys", res
    assert set(k.ctx.agents) == before


async def test_a_worker_with_a_credential_store_is_wired_to_read_it(world):
    k = world
    # the store the backend's key lives in: the worker reads it, under its own grant
    k.create("ram_store.tools", id="keys")
    k.update(MASTER, credentials={"store": "keys", "ref": "api"})
    res = await open_cabinet(k, "x", name="x")
    assert not res.get("error"), res
    w = res["worker"]
    assert k.get("keys")["callable_by"][w] == "*"
    assert k.get(w)["can_call"]["keys"] == "*"
    assert k.get(w)["credentials"] == {"store": "keys", "ref": "api"}


async def test_a_wiring_failure_removes_the_half_made_cabinet_and_its_canvas(world):
    k = world
    # a cabinet whose worker does not exist: wiring the new worker to it must fail
    await k.send(
        [
            {
                "target": k.id,
                "payload": {
                    "type": "create_agent",
                    "parent_id": MANAGER,
                    "id": "cabinet_ghost",
                    "handler_module": "cabinet.tools",
                    "cabinet_role": "cabinet",
                    "worker": "ghost_worker",
                    "task": "ghost",
                },
            }
        ]
    )
    before = set(k.ctx.agents)
    res = await open_cabinet(k, "x", name="x")
    assert "wiring failed and the cabinet was removed" in res["error"], res
    assert "`ghost_worker` does not exist" in res["error"], res
    assert "cabinet_x" not in k.ctx.agents and "cabinet_x_worker" not in k.ctx.agents
    assert set(k.ctx.agents) == before


async def test_a_worker_that_cannot_be_created_removes_its_cabinet_and_its_canvas(world):
    k = world
    # a squatter already holds the id the new worker needs
    await k.send(
        [
            {
                "target": k.id,
                "payload": {
                    "type": "create_agent",
                    "parent_id": k.id,
                    "id": "cabinet_sq_worker",
                    "handler_module": "cabinet.tools",
                },
            }
        ]
    )
    before = set(k.ctx.agents)
    res = await open_cabinet(k, "x", name="sq")
    assert res.get("reason") == "exists" and res.get("error"), res
    assert "cabinet_sq" not in k.ctx.agents
    assert set(k.ctx.agents) == before


async def test_a_duplicate_name_is_refused_and_the_first_cabinet_stays(world):
    k = world
    first = await open_cabinet(k, "first", name="dup")
    assert not first.get("error"), first
    worker = first["worker"]
    res = await open_cabinet(k, "second", name="dup")
    assert res.get("error"), res
    assert res.get("reason") == "exists", res
    rec = k.get(first["id"])
    assert rec is not None and rec["task"] == "first"
    assert k.get(worker)["parent_id"] == first["id"]


# ─── conclusion ──────────────────────────────────────────────────


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


async def test_the_status_cell_names_its_canvas_and_opens_it(world):
    """The cell's button opens the cabinet's canvas: the view names it, and the cell's
    script hands that id to the page (`fantastic.openCanvas`), never a guessed URL."""
    k = world
    cab = (await open_cabinet(k, "open", name="open"))["id"]
    canvas = k.get(cab)["canvas"]
    view = await as_(k, MASTER, cab, {"type": "view"})
    assert view["canvas"] == canvas, view
    html = k.get(cab)["view"]["bytes"]
    assert 'id="open"' in html
    assert (
        '$("open").onclick = () => { if (canvas) fantastic.openCanvas(canvas); };'
        in html
    )
    assert "canvas = v.canvas ?? null" in html
    # with no canvas named, the button is disabled rather than pointing nowhere
    assert '$("open").disabled = !canvas;' in html


async def test_a_helper_ai_made_as_equipment_reaches_its_memory_store(world):
    """A helper AI the worker makes on its canvas (`create` with a memory store) reaches
    that store, and the store admits it, as every AI create does."""
    k = world
    store = k.get(MASTER)["memory_store_id"]
    cab = (await open_cabinet(k, "helpers", name="helpers"))["id"]
    w, canvas = worker_of(k, cab), k.get(cab)["canvas"]
    made = await create_on(
        k,
        canvas,
        w,
        handler_module="ollama_backend.tools",
        id="helper",
        setup={
            "memory_store_id": store,
            "endpoint": "http://ollama.test",
            "model": "model-a",
            "context_window": 8000,
        },
    )
    assert not made.get("error"), made
    assert k.get("helper")["can_call"][store] == "*"
    assert k.get(store)["callable_by"]["helper"] == "*"
    # a helper made with no store names none, and reaches none
    plain = await create_on(
        k, canvas, w, handler_module="file_bridge.tools", id="plain"
    )
    assert not plain.get("error"), plain
    assert store not in k.get("plain").get("can_call", {})


# ─── concurrency: one manager, several opens in one batch ────────


async def test_three_opens_in_one_batch_keep_every_grant(world):
    k = world
    names = ["alpha", "beta", "gamma"]
    master = k.ctx.agents[MASTER]
    # the AI's tool calls of one turn go out as one concurrent batch
    with master._as_self():
        outs = await master.send(
            [
                {
                    "target": MANAGER,
                    "payload": {"type": "open_cabinet", "task": f"task {n}", "name": n},
                }
                for n in names
            ]
        )
    results = [out.get("result") or {} for out in outs]
    for res in results:
        assert not res.get("error"), res
    cabs = [r["id"] for r in results]
    workers = [r["worker"] for r in results]
    for cab, w in zip(cabs, workers):
        peers = [x for x in workers if x != w]
        # every worker: its own grants, its peers, the master
        assert k.get(w)["can_call"][MASTER] == MESSAGES
        assert k.get(w)["callable_by"][MASTER] == "*"
        for p in peers:
            assert k.get(w)["can_call"][p] == MESSAGES
            assert k.get(w)["callable_by"][p] == MESSAGES
        assert {d["worker"] for d in k.get(w)["cabinet_peers"]} == set(peers)
        # the master, the manager, the memory store and the root: each named the worker
        assert k.get(MASTER)["can_call"][w] == "*"
        assert k.get(MASTER)["callable_by"][w] == MESSAGES
        assert k.get(MANAGER)["can_call"][w] == "*"
        assert k.get("memory")["callable_by"][w] == "*"
        assert k.get(k.get(cab)["canvas"])["callable_by"][w] == CANVAS_VERBS
        assert k.get(cab)["callable_by"][w] == [STATUS]


# ─── equipment: placed on the cabinet's canvas, granted inside it ───


async def test_a_worker_creates_equipment_on_its_own_canvas_only(world):
    k = world
    cab_a = (await open_cabinet(k, "a", name="a"))["id"]
    cab_b = (await open_cabinet(k, "b", name="b"))["id"]
    a = worker_of(k, cab_a)
    canvas_a, canvas_b = k.get(cab_a)["canvas"], k.get(cab_b)["canvas"]
    made = await create_on(
        k,
        canvas_a,
        a,
        handler_module="file_bridge.tools",
        id="a_exec",
        setup={"allow": ["."]},
    )
    assert not made.get("error"), made
    assert k.ctx.agents["a_exec"].parent.id == canvas_a
    # the peer's canvas is not its to build on
    refused = await create_on(
        k, canvas_b, a, handler_module="file_bridge.tools", id="sneak"
    )
    assert (
        refused.get("reason") == "unauthorized" and refused.get("side") == "egress"
    ), refused
    assert "sneak" not in k.ctx.agents
    # and a worker's `new_canvas` on its own canvas is refused: its grant is create and place
    nested = await as_(k, a, canvas_a, {"type": "new_canvas", "name": "deeper"})
    assert nested.get("reason") == "unauthorized", nested
    assert "canvas_deeper" not in k.ctx.agents


async def test_a_worker_reaches_its_equipment_only_through_the_grants_it_is_given(
    world,
):
    k = world
    cab_a = (await open_cabinet(k, "a", name="a"))["id"]
    cab_b = (await open_cabinet(k, "b", name="b"))["id"]
    a, b = worker_of(k, cab_a), worker_of(k, cab_b)
    canvas_a = k.get(cab_a)["canvas"]
    made = await create_on(
        k,
        canvas_a,
        a,
        handler_module="file_bridge.tools",
        id="a_exec",
        setup={"allow": ["."]},
        callable_by={a: "*"},
    )
    assert not made.get("error"), made
    # the worker's own can_call names no equipment: the egress gate refuses it until a grant does
    refused = await as_(k, a, "a_exec", {"type": "reflect"})
    assert (
        refused.get("reason") == "unauthorized" and refused.get("side") == "egress"
    ), refused
    k.update(a, can_call={**k.get(a)["can_call"], "a_exec": "*"})
    assert not (await as_(k, a, "a_exec", {"type": "reflect"})).get("error")
    # the peer does not, whatever the equipment grants
    refused = await as_(k, b, "a_exec", {"type": "reflect"})
    assert refused.get("reason") == "unauthorized", refused


async def test_a_place_on_the_cabinet_canvas_is_allowed_to_the_worker(world):
    k = world
    cab = (await open_cabinet(k, "a", name="a"))["id"]
    w = worker_of(k, cab)
    placed = await as_(k, w, k.get(cab)["canvas"], {"type": "place", "kind": "html"})
    assert placed == {"placed": True}, placed


async def test_equipment_of_a_closed_cabinet_is_removed_with_it(world):
    k = world
    cab = (await open_cabinet(k, "a", name="a"))["id"]
    canvas = k.get(cab)["canvas"]
    made = await create_on(
        k, canvas, worker_of(k, cab), handler_module="file_bridge.tools", id="a_exec"
    )
    assert not made.get("error"), made
    res = await as_(k, MASTER, MANAGER, {"type": "close_cabinet", "id": cab})
    assert not res.get("error"), res
    assert {cab, canvas, "a_exec"} <= set(res["deleted"])
    assert canvas not in k.ctx.agents and "a_exec" not in k.ctx.agents
    assert canvas not in k.get(CANVAS)["callable_by"] if k.get(CANVAS) else True


async def test_open_refuses_loudly_when_the_manager_names_no_canvas(world):
    k = world
    k.update(MANAGER, canvas_id=None)
    before = set(k.ctx.agents)
    res = await open_cabinet(k, "x", name="x")
    assert res.get("reason") == "missing_arg" and res.get("arg") == "canvas_id", res
    assert set(k.ctx.agents) == before


async def test_open_refuses_loudly_when_the_canvas_does_not_exist(world):
    k = world
    k.update(MANAGER, canvas_id="canvas_nowhere")
    before = set(k.ctx.agents)
    res = await open_cabinet(k, "x", name="x")
    assert res.get("reason") == "unknown_agent" and res.get("id") == "canvas_nowhere", (
        res
    )
    assert set(k.ctx.agents) == before


async def test_conclude_keeps_the_named_cabinets_and_refuses_unknown_ids(world):
    k = world
    cabs = [(await open_cabinet(k, f"t{i}", name=f"k{i}"))["id"] for i in range(3)]
    before = set(k.ctx.agents)
    # an unknown id refuses the whole conclude, and deletes nothing
    res = await as_(
        k, MASTER, MANAGER, {"type": "conclude", "keep": [cabs[0], "cabinet_nope"]}
    )
    assert res.get("reason") == "unknown_keep" and res["unknown"] == ["cabinet_nope"], (
        res
    )
    assert set(k.ctx.agents) == before
    # a kept cabinet stays with its worker and its canvas; the rest are concluded
    res = await as_(k, MASTER, MANAGER, {"type": "conclude", "keep": [cabs[1]]})
    assert not res.get("error"), res
    assert res["kept"] == [cabs[1]]
    assert sorted(res["concluded"]) == sorted([cabs[0], cabs[2]])
    kept_canvas = k.get(cabs[1])["canvas"]
    assert cabs[1] in k.ctx.agents and kept_canvas in k.ctx.agents
    assert worker_of(k, cabs[1]) in k.ctx.agents
    for gone in (cabs[0], cabs[2]):
        assert gone not in k.ctx.agents
    assert (await as_(k, MASTER, MANAGER, {"type": "cabinets"}))["count"] == 1


async def test_conclude_removes_every_cabinet_worker_canvas_and_equipment(world):
    k = world
    cabs = [(await open_cabinet(k, f"t{i}", name=f"n{i}"))["id"] for i in range(2)]
    workers = [worker_of(k, c) for c in cabs]
    canvases = [k.get(c)["canvas"] for c in cabs]
    made = await create_on(
        k,
        canvases[0],
        workers[0],
        handler_module="file_bridge.tools",
        id="n0_exec",
        setup={"allow": ["."]},
    )
    assert not made.get("error"), made
    res = await as_(k, MASTER, MANAGER, {"type": "conclude"})
    assert not res.get("error"), res
    assert sorted(res["concluded"]) == sorted(cabs)
    assert {*cabs, *workers, *canvases, "n0_exec"} <= set(res["deleted"])
    for gone in [*cabs, *workers, *canvases, "n0_exec"]:
        assert gone not in k.ctx.agents, gone
    assert k.get(MANAGER) is not None and k.get(MASTER) is not None
    assert not any(
        w in k.get(MASTER)["can_call"] or w in k.get(MASTER)["callable_by"]
        for w in workers
    )
    assert (await as_(k, MASTER, MANAGER, {"type": "cabinets"}))["count"] == 0


async def test_conclude_keeps_a_result_with_its_canvas_path_and_closes_the_rest(world):
    """A kept RESULT (an agent under a cabinet's canvas, at any depth) keeps that canvas
    and the path from it to the result. The cabinet's record and worker go, and every
    other piece of equipment with them. The canvas, the worker and a bogus id are not
    results: refused before anything is deleted."""
    k = world
    cab = (await open_cabinet(k, "res", name="res"))["id"]
    other = (await open_cabinet(k, "oth", name="oth"))["id"]
    w, canvas = worker_of(k, cab), k.get(cab)["canvas"]
    for id_ in ("keep_me", "drop_me"):
        made = await create_on(k, canvas, w, handler_module="file_bridge.tools", id=id_)
        assert not made.get("error"), made
    # a canvas under the cabinet's canvas holds a result one level deeper (made by the host)
    await k.send(
        [
            {
                "target": k.id,
                "payload": {
                    "type": "create_agent",
                    "parent_id": canvas,
                    "id": "deep",
                    "handler_module": "canvas_stub.tools",
                },
            },
            {
                "target": k.id,
                "payload": {
                    "type": "create_agent",
                    "parent_id": "deep",
                    "id": "deep_keep",
                    "handler_module": "file_bridge.tools",
                },
            },
        ]
    )
    assert "deep_keep" in k.ctx.agents
    # every canvas has its page loader (the host's `<canvas>_loader`, a kernel_state child)
    loaders = [f"{canvas}_loader", "deep_loader"]
    for loader, node in zip(loaders, [canvas, "deep"]):
        made = await k.send(
            [
                {
                    "target": k.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": node,
                        "id": loader,
                        "handler_module": "kernel_state.tools",
                    },
                }
            ]
        )
        assert not made[0]["result"].get("error"), made
    before = set(k.ctx.agents)
    # a page loader is not a result: naming it is refused like any unknown id
    for bad_id in (canvas, w, "nope", *loaders):
        res = await as_(k, MASTER, MANAGER, {"type": "conclude", "keep": [bad_id]})
        assert res.get("reason") == "unknown_keep" and res["unknown"] == [bad_id], res
    assert set(k.ctx.agents) == before
    res = await as_(k, MASTER, MANAGER, {"type": "conclude", "keep": ["deep_keep"]})
    assert not res.get("error"), res
    assert res["kept"] == ["deep_keep"] and res["concluded"] == [cab, other]
    # the result, the canvas and the path to it (`deep`) stay, with their page loaders;
    # the rest goes
    kept = {"deep_keep", "deep", canvas, *loaders}
    assert kept <= set(k.ctx.agents)
    assert {cab, w, "keep_me", "drop_me", other}.isdisjoint(k.ctx.agents)
    assert {cab, w, "keep_me", "drop_me", other} <= set(res["deleted"])
    assert not kept & set(res["deleted"])
    assert (await as_(k, MASTER, MANAGER, {"type": "cabinets"}))["count"] == 0


# ─── _wire: grants merge atomically, then the other fields ──────────


async def test_wire_does_not_drop_a_concurrent_callable_by_key(world, monkeypatch):
    """`_wire` merges its keys into `callable_by` at the write, so another writer's key
    landed while `_wire` is awaiting its field write (`view_i`) survives. The old shape
    read the map, awaited the `update_agent`, then restated the whole stale map, which
    dropped `view_i`. The send's own await is made a real yield point here (a wrapper
    around the tree verb), since the send itself never suspends in-process."""
    import cabinet.tools as tools

    real_tree = tools._tree

    async def yielding_tree(kernel, payload):
        await asyncio.sleep(0)  # the await between `_wire`'s read and its write
        return await real_tree(kernel, payload)

    monkeypatch.setattr(tools, "_tree", yielding_tree)
    k = world
    mgr = k.ctx.agents[MANAGER]

    async def concurrent_writer(i: int) -> None:
        await asyncio.sleep(0)
        k.ctx.edit_callable_by(
            MASTER, lambda cb: (cb.__setitem__(f"view_{i}", ["*"]), True)[1]
        )

    for i in range(100):
        with mgr._as_self():
            res, _ = await asyncio.gather(
                tools._wire(
                    mgr,
                    MASTER,
                    callable_by={f"peer_{i}": ["*"]},
                    description=f"round {i}",
                ),
                concurrent_writer(i),
            )
        assert res is None, res
        cb = k.get(MASTER)["callable_by"]
        assert f"peer_{i}" in cb, (i, cb)
        assert f"view_{i}" in cb, (i, cb)


async def test_wire_grants_then_fields(world):
    from cabinet.tools import _wire

    k = world
    mgr = k.ctx.agents[MANAGER]
    with mgr._as_self():
        res = await _wire(
            mgr,
            MASTER,
            can_call={"peer_x": ["ping"]},
            callable_by={"peer_y": ["ping"]},
            description="hello",
        )
    assert res is None, res
    rec = k.get(MASTER)
    assert rec["can_call"]["peer_x"] == ["ping"]
    assert rec["callable_by"]["peer_y"] == ["ping"]
    assert rec["description"] == "hello"

    # a bad grant shape is refused: its error comes back, and the field is NOT written
    with mgr._as_self():
        res = await _wire(mgr, MASTER, can_call={"peer_x": "ping"}, description="nope")
    assert res, res
    assert k.get(MASTER)["description"] == "hello"


# ─── Rust twins (#170) ───────────────────────────────────────────────────────


async def test_a_canvas_name_clash_is_refused_and_the_first_canvas_stays(world):
    k = world
    # a canvas already holds the node a cabinet named `clash` would take
    (made,) = await k.send(
        [
            {
                "target": k.id,
                "payload": {
                    "type": "create_agent",
                    "parent_id": CANVAS,
                    "id": "canvas_cabinet_clash",
                    "handler_module": "canvas_stub.tools",
                },
            }
        ]
    )
    assert not made["result"].get("error"), made
    before = set(k.ctx.agents)
    res = await open_cabinet(k, "x", name="clash")
    assert "canvas_cabinet_clash" in (res.get("error") or ""), res
    assert k.get("cabinet_clash") is None
    assert set(k.ctx.agents) == before


async def test_the_equip_verb_is_gone_and_no_longer_documented(world):
    from cabinet.tools import VERBS

    k = world
    cab = (await open_cabinet(k, "old", name="old"))["id"]
    # the operator's own send reaches the cabinet's handler: no `equip` there
    (out,) = await k.send(
        [
            {
                "target": cab,
                "payload": {"type": "equip", "handler_module": "file_bridge.tools"},
            }
        ]
    )
    assert out["result"].get("reason") == "unhandled", out
    assert f"{worker_of(k, cab)}_eq" not in k.ctx.agents
    assert "equip" not in VERBS
    view_html = k.get(cab)["view"]["bytes"]
    assert '"equip"' not in view_html and "`equip`" not in view_html
    from importlib import resources

    readme = resources.files("cabinet").joinpath("readme.md").read_text("utf-8")
    assert "`equip`" not in readme


async def test_cabinets_lists_each_canvas_and_its_equipment(world):
    k = world
    cab = (await open_cabinet(k, "list", name="list"))["id"]
    canvas = k.get(cab)["canvas"]
    made = await create_on(
        k,
        canvas,
        worker_of(k, cab),
        handler_module="file_bridge.tools",
        id="list_exec",
    )
    assert not made.get("error"), made
    res = await as_(k, MASTER, MANAGER, {"type": "cabinets"})
    row = res["cabinets"][0]
    assert row["canvas"] == canvas
    assert row["equipment"] == ["list_exec"]
