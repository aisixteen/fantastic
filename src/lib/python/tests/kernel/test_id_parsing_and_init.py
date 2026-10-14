"""Two things reduction candidate I would merge, pinned before it does:

1. THE THREE `_as_id` COPIES DISAGREE. The kernel's (`cancel task_id`,
   `ack_deadletter id`) accepts any Unicode decimal string and no padding; the
   ai_core one (`history fold`) and the ram_store one (`read ids/after/limit`)
   accept ASCII digits only, after stripping whitespace. These tests hold each
   side's CURRENT answer, so merging them is a visible, deliberate change.
2. THE CLI `init` PATH (`fantastic init <loader> <manifest>`) had no test at all.
"""

from __future__ import annotations

import asyncio
import fcntl
import json
import os
import types

import pytest
import yaml
from _testkit import memory_meta

from kernel import dispatch_argv, register_unlisted

ARABIC = str.maketrans("0123456789", "٠١٢٣٤٥٦٧٨٩")


def _arabic(n: int) -> str:
    return str(n).translate(ARABIC)


_flags: dict = {}


async def _handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {"id": id, "sentence": "id probe", "verbs": {}}
    if verb == "slow":
        _flags.setdefault(id, asyncio.Event()).set()
        await asyncio.sleep(30)
    if verb == "fail":
        raise RuntimeError("down")
    return {"ok": True}


_mod = types.ModuleType("id_probe.tools")
_mod.handler = _handler
_mod.VERBS = {}
register_unlisted("id_probe.tools", _mod)


async def _call(k, target, payload) -> dict:
    return (await k.send([{"target": target, "payload": payload}]))[0]


async def _slow_pid(k, aid: str):
    _flags.pop(aid, None)
    pending = asyncio.create_task(
        k.send([{"target": aid, "payload": {"type": "slow"}}])
    )
    await asyncio.wait_for(_flags.setdefault(aid, asyncio.Event()).wait(), 2)
    tasks = (await _call(k, "kernel", {"type": "ps"}))["result"]["tasks"]
    return pending, next(t["id"] for t in tasks if t["agent"] == aid)


# ─── the kernel's `_as_id`: cancel task_id ───────────────────────────────────


async def test_cancel_task_id_forms(seeded_kernel):
    k = seeded_kernel
    k.create("id_probe.tools", id="sl")
    nothing = {"cancelled": False, "killed": []}
    pending, pid = await _slow_pid(k, "sl")
    # padded and signed forms name no task (not decimal); zero/negative too
    for raw in (f" {pid}", f"{pid} ", f"+{pid}", 0, -pid, f"{pid}.0"):
        r = (await _call(k, "kernel", {"type": "cancel", "task_id": raw}))["result"]
        assert r == nothing, raw
    for raw in (True, None, "", 1.5, [pid]):
        r = (await _call(k, "kernel", {"type": "cancel", "task_id": raw}))["result"]
        assert r == {
            "error": "cancel: task_id required",
            "reason": "missing_arg",
            "arg": "task_id",
        }, raw
    # NON-ASCII decimal digits ARE accepted by the kernel's copy today
    r = (await _call(k, "kernel", {"type": "cancel", "task_id": _arabic(pid)}))[
        "result"
    ]
    assert r == {"cancelled": True, "killed": [pid]}
    await asyncio.wait_for(pending, 2)


async def test_cancel_accepts_a_leading_zero_and_a_plain_string(seeded_kernel):
    k = seeded_kernel
    k.create("id_probe.tools", id="sl2")
    pending, pid = await _slow_pid(k, "sl2")
    r = (await _call(k, "kernel", {"type": "cancel", "task_id": f"0{pid}"}))["result"]
    assert r == {"cancelled": True, "killed": [pid]}
    await asyncio.wait_for(pending, 2)
    pending, pid = await _slow_pid(k, "sl2")
    r = (await _call(k, "kernel", {"type": "cancel", "task_id": str(pid)}))["result"]
    assert r == {"cancelled": True, "killed": [pid]}
    await asyncio.wait_for(pending, 2)


# ─── the kernel's `_as_id`: ack_deadletter id ────────────────────────────────


async def test_ack_deadletter_id_forms_and_texts(seeded_kernel):
    k = seeded_kernel
    k.create("id_probe.tools", id="dl")
    dead = await _call(k, "dl", {"type": "fail"})
    assert dead["status"] == "failed", dead
    pid = dead["pid"]
    for raw in (True, None, ""):
        r = (await _call(k, "kernel", {"type": "ack_deadletter", "id": raw}))["result"]
        assert r == {
            "error": "ack_deadletter: id required",
            "reason": "missing_arg",
            "arg": "id",
        }, raw
    # a padded id is not an id — it is looked up as a NAME, and none matches
    r = (await _call(k, "kernel", {"type": "ack_deadletter", "id": f" {pid}"}))[
        "result"
    ]
    assert r == {
        "error": f"ack_deadletter: no execution ' {pid}' of yours in the ledger",
        "reason": "not_found",
        "arg": "id",
    }
    # non-ASCII decimal digits ARE an id to the kernel's copy
    r = (await _call(k, "kernel", {"type": "ack_deadletter", "id": _arabic(pid)}))[
        "result"
    ]
    assert r == {"acked": True, "id": pid}
    r = (await _call(k, "kernel", {"type": "ack_deadletter", "id": f"0{pid}"}))[
        "result"
    ]
    assert r == {"acked": True, "id": pid, "already": True}
    ok = await _call(k, "dl", {"type": "work"})
    r = (await _call(k, "kernel", {"type": "ack_deadletter", "id": ok["pid"]}))[
        "result"
    ]
    assert r == {
        "error": (
            f"ack_deadletter: execution {ok['pid']} is 'completed', not a dead "
            "letter — only failed / timed_out / cancelled work is acknowledged"
        ),
        "reason": "bad_arg",
        "arg": "id",
    }


async def test_ack_deadletter_by_name(seeded_kernel):
    k = seeded_kernel
    k.create("id_probe.tools", id="dn")
    item = {"target": "dn", "payload": {"type": "fail"}, "id": "job-1"}
    dead = (await k.send([item]))[0]
    assert dead["status"] == "failed" and dead["name"] == "job-1"
    r = (await _call(k, "kernel", {"type": "ack_deadletter", "id": "job-1"}))["result"]
    assert r == {"acked": True, "id": dead["pid"]}


# ─── ai_core's `_as_id`: history fold ────────────────────────────────────────


async def test_ai_history_fold_forms(seeded_kernel):
    k = seeded_kernel
    meta = memory_meta(k, "hai")
    rec = k.create(
        "ollama_backend.tools",
        id="hai",
        endpoint="http://127.0.0.1:9/stub",
        model="stub",
        **meta,
    )
    assert rec.get("id") == "hai", rec

    async def hist(fold):
        return (await _call(k, "hai", {"type": "history", "fold": fold}))["result"]

    bad = {
        "error": "history: `fold` must be a fold id (int)",
        "reason": "bad_arg",
        "arg": "fold",
    }
    # ai_core's copy: ASCII digits only — a non-ASCII decimal is NOT an id here
    for raw in (_arabic(7), True, 0, -3, "x", ""):
        assert await hist(raw) == bad, raw
    # …and whitespace is stripped, a leading zero is fine
    for raw in (" 7", "7 ", "07", 7):
        assert await hist(raw) == {
            "error": "history: 7 is not a fold in your conversation",
            "reason": "not_found",
            "arg": "fold",
        }, raw
    r = (await _call(k, "hai", {"type": "history"}))["result"]
    assert r == {"rows": [], "next": None, "total": 0}


# ─── ram_store's `_as_id`: read ids / after / limit ──────────────────────────


@pytest.fixture
async def mem(kernel):
    kernel.ctx.update(kernel.id, guid="g1")
    kernel.create(None, id="owner", can_call={"mem": "*"})
    kernel.create(
        "ram_store.tools",
        id="mem",
        callable_by={"owner": "*"},
    )
    from _testkit import as_agent

    with as_agent(kernel, "owner"):
        out = (
            await kernel.send(
                [
                    {
                        "target": "mem",
                        "payload": {
                            "type": "append",
                            "rows": [{"id": i, "content": f"r{i}"} for i in (1, 2, 3)],
                        },
                    }
                ]
            )
        )[0]["result"]
    assert out == {"ids": [1, 2, 3], "appended": 3}, out
    return kernel


async def _read(k, **q) -> list:
    from _testkit import as_agent

    with as_agent(k, "owner"):
        r = (await k.send([{"target": "mem", "payload": {"type": "read", **q}}]))[0][
            "result"
        ]
    return [row["id"] for row in r["rows"]]


async def test_ram_store_id_forms(mem):
    # padded and leading-zero ASCII → ids; non-ASCII decimals, bools, 0 → not ids
    assert await _read(mem, ids=[" 1", "03", _arabic(2), True, 0]) == [1, 3]
    assert await _read(mem, after=" 1") == [2, 3]
    assert await _read(mem, after=_arabic(1)) == [1, 2, 3]  # not an id → no filter
    assert await _read(mem, limit="2 ") == [1, 2]
    assert await _read(mem, limit=True) == [1, 2, 3]
    assert await _read(mem, before="03") == [1, 2]


async def test_ram_store_append_refuses_a_string_row_id(mem):
    from _testkit import as_agent

    for rid in ("4", " 4", True, 0):
        with as_agent(mem, "owner"):
            r = (
                await mem.send(
                    [
                        {
                            "target": "mem",
                            "payload": {"type": "append", "rows": [{"id": rid}]},
                        }
                    ]
                )
            )[0]["result"]
        assert r == {
            "error": f"append: a row id is a kernel int, got {rid!r}",
            "reason": "bad_arg",
            "arg": "rows",
        }, rid


# ─── the CLI `init` path ─────────────────────────────────────────────────────


def _manifest(tmp_path) -> str:
    (tmp_path / "fantastic.yaml").write_text(
        yaml.safe_dump(
            {"agents": [{"id": "roster_fb", "handler_module": "file_bridge.tools"}]}
        )
    )
    return "./fantastic.yaml"


async def test_init_seeds_and_boots_a_loader_and_prints_its_record(
    kernel, tmp_path, capsys
):
    path = _manifest(tmp_path)
    assert await dispatch_argv(kernel.ctx, ["init", "static_loader", path]) is None
    printed = json.loads(capsys.readouterr().out)
    assert printed["id"] == "static_loader"
    assert printed["handler_module"] == "static_loader.tools"
    assert printed["parent_id"] == kernel.id
    assert printed["manifest"] == path
    # the loader booted and hydrated its roster, as its own child
    assert "roster_fb" in kernel.ctx.agents
    assert kernel.ctx.agents["roster_fb"].parent.id == "static_loader"
    # the lock was taken and released
    assert not (tmp_path / ".fantastic" / "lock.yaml").exists()
    # a second init is idempotent by id: create refuses the duplicate
    assert await dispatch_argv(kernel.ctx, ["init", "static_loader", path]) is None
    again = json.loads(capsys.readouterr().out)
    assert again["reason"] == "exists", again


def _held_lock(tmp_path, pid: int) -> int:
    """`lock.yaml` naming `pid`, its flock HELD on a descriptor of its own — what
    a live daemon's lock is. The caller closes the descriptor."""
    d = tmp_path / ".fantastic"
    d.mkdir(exist_ok=True)
    (d / "lock.yaml").write_text(yaml.safe_dump({"pid": pid}))
    fd = os.open(d / "lock.yaml", os.O_RDONLY)
    fcntl.flock(fd, fcntl.LOCK_EX)
    return fd


def _stale_lock(tmp_path) -> None:
    """`lock.yaml` naming a LIVE pid (this test's parent) that holds no flock — a
    dead holder's file whose pid was reused."""
    d = tmp_path / ".fantastic"
    d.mkdir(exist_ok=True)
    (d / "lock.yaml").write_text(yaml.safe_dump({"pid": os.getppid()}))


async def test_init_refuses_while_another_fantastic_owns_the_dir(
    kernel, tmp_path, capsys
):
    fd = _held_lock(tmp_path, os.getppid())
    try:
        with pytest.raises(SystemExit) as e:
            await dispatch_argv(kernel.ctx, ["init", "static_loader", "./x.yaml"])
    finally:
        os.close(fd)
    assert e.value.code == 1
    assert capsys.readouterr().err == (
        f"[init] another fantastic owns this dir (pid={os.getppid()}). Stop it first.\n"
    )
    assert "static_loader" not in kernel.ctx.agents


async def test_one_shots_probe_the_flock_not_the_pid(kernel, tmp_path, capsys):
    """A one-shot takes the daemon's own flock. A lock file
    naming a live pid that holds no flock is stale — taken over, not refused."""
    path = _manifest(tmp_path)
    _stale_lock(tmp_path)
    assert await dispatch_argv(kernel.ctx, ["init", "static_loader", path]) is None
    assert json.loads(capsys.readouterr().out)["id"] == "static_loader"
    assert not (tmp_path / ".fantastic" / "lock.yaml").exists()
    _stale_lock(tmp_path)
    assert await dispatch_argv(kernel.ctx, ["kernel", "list_agents"]) is None
    assert json.loads(capsys.readouterr().out)["agents"]
    assert not (tmp_path / ".fantastic" / "lock.yaml").exists()


async def test_init_usage(kernel, capsys):
    with pytest.raises(SystemExit) as e:
        await dispatch_argv(kernel.ctx, ["init", "static_loader"])
    assert e.value.code == 2
    assert capsys.readouterr().err == (
        "usage: fantastic init <loader_bundle> <manifest>\n"
        "   e.g. fantastic init static_loader ./fantastic.yaml\n"
    )


async def test_call_refuses_while_another_fantastic_owns_the_dir(
    kernel, tmp_path, capsys
):
    fd = _held_lock(tmp_path, os.getppid())
    try:
        with pytest.raises(SystemExit) as e:
            await dispatch_argv(kernel.ctx, ["kernel", "ps"])
    finally:
        os.close(fd)
    assert e.value.code == 1
    assert capsys.readouterr().err == (
        f"[call] another fantastic owns this dir (pid={os.getppid()}). Stop it, or "
        "forward your call over the WS bridge (see ws_bridge bundle).\n"
    )


async def test_call_prints_the_reply_and_releases_the_lock(kernel, tmp_path, capsys):
    assert await dispatch_argv(kernel.ctx, ["kernel", "list_agents"]) is None
    out = json.loads(capsys.readouterr().out)
    assert [a["id"] for a in out["agents"]][:1] == [kernel.id]
    assert not (tmp_path / ".fantastic" / "lock.yaml").exists()


async def test_reflect_sugar_targets_and_flags(kernel, capsys):
    await dispatch_argv(kernel.ctx, ["reflect", "tree=ids"])
    r = json.loads(capsys.readouterr().out)
    assert r["id"] == kernel.id and r["tree"][0] == kernel.id
    await dispatch_argv(kernel.ctx, ["reflect", kernel.id, "tree=none"])
    r = json.loads(capsys.readouterr().out)
    assert r["id"] == kernel.id and "tree" not in r


async def test_one_positional_and_help(kernel, capsys):
    with pytest.raises(SystemExit) as e:
        await dispatch_argv(kernel.ctx, ["lonely"])
    assert e.value.code == 2
    assert capsys.readouterr().err.startswith(
        "usage: fantastic <id> <verb> [k=v ...]\n"
    )
    assert await dispatch_argv(kernel.ctx, ["--help"]) is None
    assert capsys.readouterr().out.strip()
