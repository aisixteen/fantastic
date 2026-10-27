"""`await_set` — one wake for a set of agents. The twin of rust
`fantastic-kernel/src/await_set/tests.rs` (and, for the restart,
`fantastic-ram-store/tests/await_restart.rs`); every test is named
`test_<rust test>`.

The signal is a message of `type: report` from a named agent to the waiting
caller; the combined message reaches the caller as `{type: send, text, wait_id,
label, done, missing, timed_out}`."""

from __future__ import annotations

import asyncio
import json
import types

from _testkit import as_agent, boot_root, persist, wire
from kernel import register_unlisted

# ── a probe bundle that RECORDS every payload it is dispatched ───────────────
_seen: dict[str, list[dict]] = {}


async def _handler(id, payload, kernel):
    if payload.get("type") == "boot":
        return None
    _seen.setdefault(id, []).append(payload)
    return {"ok": True}


_mod = types.ModuleType("await_probe.tools")
_mod.handler = _handler
_mod.VERBS = {}
register_unlisted("await_probe.tools", _mod)


async def _call(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]


async def _fleet(root, workers):
    _seen.clear()
    for aid in ("lead", *workers):
        r = (
            await _call(
                root,
                root.id,
                {
                    "type": "create_agent",
                    "parent_id": root.id,
                    "id": aid,
                    "handler_module": "await_probe.tools",
                },
            )
        )["result"]
        assert r.get("id") == aid, r
    wire(root, "lead", root.id, ["await_set", "await_cancel"])
    for w in workers:
        wire(root, w, "lead", ["report"])


async def _as(root, who, target, payload):
    with as_agent(root, who):
        return (await _call(root, target, payload))["result"]


async def _report(root, frm, to, **fields):
    return await _as(root, frm, to, {"type": "report", **fields})


async def _wait_for(agent, n):
    for _ in range(300):
        if len(_seen.get(agent, [])) >= n:
            return list(_seen[agent])
        await asyncio.sleep(0.01)
    raise AssertionError(f"{agent} got {_seen.get(agent)}, wanted {n}")


def _set(ids, **extra):
    return {"type": "await_set", "ids": ids, **extra}


async def test_await_set_wakes_the_caller_once_for_n_reports(seeded_kernel):
    root = seeded_kernel
    await _fleet(root, ["w1", "w2", "w3"])
    r = await _as(root, "lead", root.id, _set(["w1", "w2", "w3"], label="phase-1"))
    wid = r["wait_id"]
    assert isinstance(wid, int) and r["label"] == "phase-1"
    assert r["ids"] == ["w1", "w2", "w3"]
    for w, res in (("w3", "r3"), ("w1", "r1"), ("w2", "r2")):
        held = await _report(root, w, "lead", state="done", result=res)
        assert held == {"held": True, "wait_id": wid}, w
    got = await _wait_for("lead", 1)
    await asyncio.sleep(0.1)
    assert len(_seen["lead"]) == 1, f"ONE message, no per-report turns: {_seen['lead']}"
    m = got[0]
    assert m["type"] == "send" and m["wait_id"] == wid and m["label"] == "phase-1"
    assert m["timed_out"] is False and m["missing"] == []
    assert m["done"] == [
        {"id": "w1", "state": "done", "result": "r1"},
        {"id": "w2", "state": "done", "result": "r2"},
        {"id": "w3", "state": "done", "result": "r3"},
    ]
    assert json.loads(m["text"])["done"] == m["done"], "`text` carries the same body"
    assert all(p["type"] != "report" for p in _seen["lead"])


async def test_await_set_timeout_names_the_missing(seeded_kernel):
    root = seeded_kernel
    await _fleet(root, ["w1", "w2"])
    r = await _as(root, "lead", root.id, _set(["w1", "w2"], timeout_s=0.2))
    assert r["timeout_s"] == 0.2
    await _report(root, "w1", "lead", state="done", result="a")
    assert not _seen.get("lead"), "nothing before the deadline"
    got = await _wait_for("lead", 1)
    assert got[0]["timed_out"] is True
    assert got[0]["done"] == [{"id": "w1", "state": "done", "result": "a"}]
    assert got[0]["missing"] == ["w2"]
    late = await _report(root, "w2", "lead", state="done", result="b")
    assert late == {"ok": True}, "a late report is an ordinary message now"
    assert len(_seen["lead"]) == 2 and _seen["lead"][1]["type"] == "report"


async def test_await_set_holds_a_report_instead_of_delivering_it(seeded_kernel):
    root = seeded_kernel
    await _fleet(root, ["w1", "w2", "stranger"])
    wire(root, "stranger", "lead", ["report", "note"])
    wire(root, "w1", "lead", ["report", "note"])
    r = await _as(root, "lead", root.id, _set(["w1", "w2"]))
    wid = r["wait_id"]
    held = await _report(root, "w1", "lead", state="done", result="x")
    assert held == {"held": True, "wait_id": wid}
    assert not _seen.get("lead"), "held, not delivered"
    # a report from outside the set, and a non-report from inside it: normal
    assert await _report(root, "stranger", "lead", state="done") == {"ok": True}
    assert await _as(root, "w1", "lead", {"type": "note"}) == {"ok": True}
    assert len(_seen["lead"]) == 2


async def test_await_cancel_releases_held_reports_in_order(seeded_kernel):
    root = seeded_kernel
    await _fleet(root, ["w1", "w2", "w3"])
    wid = (await _as(root, "lead", root.id, _set(["w1", "w2", "w3"])))["wait_id"]
    await _report(root, "w2", "lead", state="done", result="two")
    await _report(root, "w1", "lead", state="failed", result="one")
    assert not _seen.get("lead")
    c = await _as(root, "lead", root.id, {"type": "await_cancel", "wait_id": wid})
    assert c == {"cancelled": True, "wait_id": wid, "released": 2}
    got = await _wait_for("lead", 2)
    assert got[0]["type"] == "report" and got[0]["result"] == "two", "in the order held"
    assert got[1]["result"] == "one"
    late = await _report(root, "w3", "lead", state="done", result="three")
    assert late == {"ok": True}
    await asyncio.sleep(0.1)
    assert len(_seen["lead"]) == 3
    assert all(p["type"] == "report" for p in _seen["lead"])
    # unknown and foreign waits are an honest false; no wait_id is refused
    none = await _as(root, "lead", root.id, {"type": "await_cancel", "wait_id": wid})
    assert none["cancelled"] is False
    w2 = (await _as(root, "lead", root.id, _set(["w1"])))["wait_id"]
    wire(root, "w1", root.id, ["await_cancel"])
    foreign = await _as(root, "w1", root.id, {"type": "await_cancel", "wait_id": w2})
    assert foreign["cancelled"] is False, foreign
    miss = await _as(root, "lead", root.id, {"type": "await_cancel"})
    assert miss["reason"] == "missing_arg", miss


async def test_await_set_refuses_bad_args(seeded_kernel):
    root = seeded_kernel
    await _fleet(root, ["w1"])
    cases = [
        ({"type": "await_set"}, "missing_arg", "ids"),
        ({"type": "await_set", "ids": []}, "missing_arg", "ids"),
        ({"type": "await_set", "ids": "w1"}, "bad_arg", "ids"),
        ({"type": "await_set", "ids": [3]}, "bad_arg", "ids"),
        ({"type": "await_set", "ids": ["nobody"]}, "unknown_agent", ""),
        ({"type": "await_set", "ids": ["lead"]}, "bad_arg", "ids"),
        (_set(["w1"], timeout_s=0), "bad_arg", "timeout_s"),
        (_set(["w1"], timeout_s=-2), "bad_arg", "timeout_s"),
        (_set(["w1"], timeout_s="5"), "bad_arg", "timeout_s"),
        (_set(["w1"], label=7), "bad_arg", "label"),
    ]
    for p, reason, arg in cases:
        r = await _as(root, "lead", root.id, p)
        assert r["reason"] == reason, (p, r)
        if arg:
            assert r["arg"] == arg, (p, r)
    # an operator send has nobody to wake
    op = await _call(root, root.id, _set(["w1"]))
    assert op["result"]["reason"] == "bad_arg", op
    # nothing was registered: a report is delivered, not held
    assert await _report(root, "w1", "lead", state="done") == {"ok": True}
    assert len(_seen["lead"]) == 1


async def test_await_set_is_root_only_and_gated(seeded_kernel):
    root = seeded_kernel
    await _fleet(root, ["w1"])
    wire(root, "lead", "w1", ["await_set"])
    r = await _as(root, "lead", "w1", _set(["w1"]))
    assert r["reason"] == "not_here", r
    r = await _as(root, "w1", root.id, _set(["lead"]))
    assert r["reason"] == "unauthorized", r


async def test_a_duplicate_report_from_one_agent_replaces_the_first(seeded_kernel):
    root = seeded_kernel
    await _fleet(root, ["w1", "w2"])
    await _as(root, "lead", root.id, _set(["w1", "w2"]))
    await _report(root, "w1", "lead", state="working", result="first")
    await _report(root, "w1", "lead", state="done", result="second")
    await asyncio.sleep(0.1)
    assert not _seen.get("lead"), "a duplicate does not complete the set"
    await _report(root, "w2", "lead", state="done", result="w")
    got = await _wait_for("lead", 1)
    assert got[0]["done"] == [
        {"id": "w1", "state": "done", "result": "second"},
        {"id": "w2", "state": "done", "result": "w"},
    ]
    assert got[0]["missing"] == []


async def test_await_set_completion_race_delivers_once(seeded_kernel):
    root = seeded_kernel
    await _fleet(root, ["w1"])
    await _as(root, "lead", root.id, _set(["w1"], timeout_s=0.05))
    await asyncio.sleep(0.048)
    await _report(root, "w1", "lead", state="done", result="x")
    await asyncio.sleep(0.3)
    combined = [p for p in _seen.get("lead", []) if "wait_id" in p]
    assert len(combined) == 1, f"exactly one combined message: {_seen.get('lead')}"


# ── the restart: the wait is rows in the kernel's store, rebuilt on boot ─────


async def _store(root):
    r = (
        await _call(
            root,
            root.id,
            {
                "type": "create_agent",
                "parent_id": root.id,
                "id": "memory",
                "handler_module": "ram_store.tools",
            },
        )
    )["result"]
    assert r.get("id") == "memory", r
    root.ctx.update(root.id, memory_store_id="memory")
    assert (await root.ctx.start(boot_all=False)).get("started")


async def _first(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    root = boot_root()
    await _call(root, root.id, {"type": "boot"})  # mints the guid
    await _fleet(root, ["w1", "w2"])
    await _store(root)
    return root


async def _restart(root, tmp_path, monkeypatch, down=0.0):
    """Flush, persist the tree, and boot a SECOND kernel on the same dir whose store
    holds what the first one flushed. The first is DEAD from the dump on."""
    await root.ctx.flush_executions()
    tables = (await _call(root, "memory", {"type": "dump"}))["result"]["tables"]
    persist(root)
    await asyncio.sleep(down)
    _seen.clear()
    root2 = boot_root()
    await _call(root2, "memory", {"type": "inject", "tables": tables})
    assert (await root2.ctx.start(boot_all=False)).get("started")
    return root2


async def test_await_set_survives_a_restart(tmp_path, monkeypatch):
    # 1. w1 reports (held); restart; w2 reports -> ONE message, BOTH reports
    root = await _first(tmp_path, monkeypatch)
    r = await _as(root, "lead", root.id, _set(["w1", "w2"], label="L"))
    wid = r["wait_id"]
    h = await _report(root, "w1", "lead", state="done", result="one")
    assert h == {"held": True, "wait_id": wid}
    root2 = await _restart(root, tmp_path, monkeypatch)
    h2 = await _report(root2, "w2", "lead", state="done", result="two")
    assert h2 == {"held": True, "wait_id": wid}, "the restored wait holds the report"
    got = await _wait_for("lead", 1)
    await asyncio.sleep(0.1)
    assert len(_seen["lead"]) == 1, "ONE message"
    assert got[0]["wait_id"] == wid and got[0]["label"] == "L"
    assert got[0]["timed_out"] is False
    assert got[0]["done"] == [
        {"id": "w1", "state": "done", "result": "one"},
        {"id": "w2", "state": "done", "result": "two"},
    ]


async def test_await_set_survives_a_restart_past_deadline(tmp_path, monkeypatch):
    # 2. a deadline that passed while the kernel was down: completes at once
    root = await _first(tmp_path, monkeypatch)
    await _as(root, "lead", root.id, _set(["w1", "w2"], timeout_s=0.3))
    await _report(root, "w1", "lead", state="done", result="one")
    await _restart(root, tmp_path, monkeypatch, down=0.45)
    got = await _wait_for("lead", 1)
    assert got[0]["timed_out"] is True and got[0]["missing"] == ["w2"]
    assert got[0]["done"] == [{"id": "w1", "state": "done", "result": "one"}]


async def test_await_set_survives_a_restart_closed_wait_stays_closed(
    tmp_path, monkeypatch
):
    # 3. a wait closed before the restart is not rebuilt
    root = await _first(tmp_path, monkeypatch)
    await _as(root, "lead", root.id, _set(["w1"]))
    await _report(root, "w1", "lead", state="done", result="x")
    await asyncio.sleep(0.1)  # the one message, then the close row
    root2 = await _restart(root, tmp_path, monkeypatch)
    await asyncio.sleep(0.1)
    assert not _seen.get("lead"), "nothing re-delivered on boot"
    r = await _report(root2, "w1", "lead", state="done", result="y")
    assert r == {"ok": True}, "not held: the wait is closed"


async def test_await_set_refusals_are_kernel_shape(seeded_kernel):
    root = seeded_kernel
    await _fleet(root, ["w1"])
    for p, code in (
        ({"type": "await_set"}, "missing_arg"),
        ({"type": "await_set", "ids": ["nobody"]}, "unknown_agent"),
    ):
        r = await _as(root, "lead", root.id, p)
        assert r["reason"] == code, (p, r)
        assert " " in r["error"] and r["error"] != code, (p, r)
