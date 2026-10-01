"""The kernel's own single-item sends that read the WHOLE outcome, not just its
`result`: `kernel_start` (root + store boot, the store probe, the boot-all loop),
`create_agent`'s child boot, and `flush_executions`.

A single-call helper that hands back only `result` (reduction candidate J) would
lose the `status` these branch on. These pin what each does for each status: a
FAILED boot (a raise), an ERRORED boot (an `error` reply), a refused flush.
"""

from __future__ import annotations

import sys
import types

import ram_store.tools as rst

_mod = types.ModuleType("status_probe.tools")


async def _handler(id, payload, kernel):
    verb = payload.get("type")
    if verb == "boot":
        if id.startswith("raise"):
            raise RuntimeError("boot blew")
        if id.startswith("err"):
            return {"error": "boot refused", "reason": "unhandled"}
        return None
    if verb == "reflect":
        return {"id": id, "sentence": "status probe", "verbs": {}}
    return {"ok": True}


_mod.handler = _handler
_mod.VERBS = {}
sys.modules["status_probe.tools"] = _mod

RAISED = {"error": "RuntimeError: boot blew", "raised": True}


async def _call(k, target, payload) -> dict:
    return (await k.send([{"target": target, "payload": payload}]))[0]


def _with_store(k, sid: str = "memory") -> None:
    k.ctx.update(k.id, guid="test-guid")
    k.create("ram_store.tools", id=sid, operator=True, admins=["kernel"])
    k.ctx.update(k.id, memory_store_id=sid)


# ─── create_agent's child boot ───────────────────────────────────────────────


async def test_a_failed_child_boot_is_printed_and_the_agent_kept(seeded_kernel, capsys):
    k = seeded_kernel
    out = await _call(
        k,
        "kernel",
        {
            "type": "create_agent",
            "parent_id": k.id,
            "handler_module": "status_probe.tools",
            "id": "raise_1",
        },
    )
    assert out["status"] == "completed" and out["result"]["id"] == "raise_1", out
    assert "raise_1" in k.ctx.agents
    assert f"  [create_agent] raise_1 boot failed: {RAISED}" in capsys.readouterr().out


async def test_an_errored_child_boot_is_silent(seeded_kernel, capsys):
    k = seeded_kernel
    out = await _call(
        k,
        "kernel",
        {
            "type": "create_agent",
            "parent_id": k.id,
            "handler_module": "status_probe.tools",
            "id": "err_1",
        },
    )
    assert out["status"] == "completed" and out["result"]["id"] == "err_1", out
    assert "boot failed" not in capsys.readouterr().out
    # the boot's own outcome is `errored`, not `failed`
    again = await _call(k, "err_1", {"type": "boot"})
    assert again["status"] == "errored" and again["result"]["error"] == "boot refused"


# ─── kernel_start ────────────────────────────────────────────────────────────


async def test_kernel_start_without_a_store_texts(seeded_kernel):
    k = seeded_kernel
    r = (await _call(k, "kernel", {"type": "kernel_start"}))["result"]
    assert r == {
        "error": (
            "kernel_start: the root record names no `memory_store_id` — the kernel "
            "keeps its execution rows (ledger, dead letters, named-call dedup) in a "
            "memory store and will not start without one"
        ),
        "reason": "missing_arg",
        "hint": (
            "a memory store is needed — create a ram_store and name it in "
            "memory_store_id if you have none: `create_agent parent_id=kernel_state "
            "handler_module=ram_store.tools id=memory operator=true`, then "
            "`update_agent id=kernel_state memory_store_id=memory`"
        ),
        "arg": "memory_store_id",
    }
    k.ctx.update(k.id, memory_store_id="ghost_store")
    r = (await _call(k, "kernel", {"type": "kernel_start"}))["result"]
    assert r["reason"] == "unknown_agent", r
    assert r["hint"] == "the root's `memory_store_id` names no agent in this tree"
    assert k.ctx.started is False


async def test_kernel_start_refuses_a_store_whose_boot_fails(seeded_kernel):
    k = seeded_kernel
    k.ctx.update(k.id, guid="test-guid")
    k.create("status_probe.tools", id="raise_store")
    k.ctx.update(k.id, memory_store_id="raise_store")
    r = (await _call(k, "kernel", {"type": "kernel_start"}))["result"]
    assert r == {
        "error": f"kernel_start: boot of 'raise_store' did not complete (failed): {RAISED}",
        "reason": "unhandled",
    }
    assert k.ctx.started is False


async def test_kernel_start_names_a_store_that_refuses_the_kernel(seeded_kernel):
    k = seeded_kernel
    k.ctx.update(k.id, guid="test-guid")
    k.create("ram_store.tools", id="shy", operator=False)
    k.ctx.update(k.id, memory_store_id="shy")
    r = (await _call(k, "kernel", {"type": "kernel_start"}))["result"]
    assert r["reason"] == "unauthorized", r
    assert r["error"].startswith(
        "kernel_start: the memory store 'shy' refuses the kernel: "
    )
    assert list(r) == ["error", "reason", "hint"]


async def test_boot_all_skips_failed_and_errored_boots_loudly(seeded_kernel, capsys):
    k = seeded_kernel
    _with_store(k)
    for aid in ("fine_1", "raise_2", "err_2"):
        k.create("status_probe.tools", id=aid)
    r = (await _call(k, "kernel", {"type": "kernel_start"}))["result"]
    assert r["started"] is True and r["store"] == "memory"
    assert r["booted"][:2] == [k.id, "memory"]
    assert "fine_1" in r["booted"]
    assert "raise_2" not in r["booted"] and "err_2" not in r["booted"]
    err = capsys.readouterr().err
    assert f"  [kernel] boot 'raise_2': failed {RAISED}" in err
    assert (
        "  [kernel] boot 'err_2': errored "
        "{'error': 'boot refused', 'reason': 'unhandled'}"
    ) in err


# ─── flush_executions ────────────────────────────────────────────────────────


async def test_a_refused_flush_keeps_the_rows_in_ram(
    seeded_kernel, monkeypatch, capsys
):
    k = seeded_kernel
    _with_store(k)
    assert (await k.ctx.start(boot_all=False)).get("started")
    await k.ctx.flush_executions()
    k.create("status_probe.tools", id="fine_2")
    done = await _call(k, "fine_2", {"type": "work"})
    pid = done["pid"]
    assert pid in k.ctx.rows

    async def refuse(id, payload, agent):
        return {"error": "store full", "reason": "bad_arg"}

    monkeypatch.setitem(rst.VERBS, "append", refuse)
    out = await k.ctx.flush_executions()
    assert out == {"flushed": 0, "error": {"error": "store full", "reason": "bad_arg"}}
    assert pid in k.ctx.rows, "a refused batch stays in RAM for the next flush"
    assert "execution row(s) to 'memory' failed: errored " in capsys.readouterr().err
    monkeypatch.undo()
    out = await k.ctx.flush_executions()
    assert out["flushed"] > 0 and pid not in k.ctx.rows


# ─── the long-running default mode reads `kernel.start`'s answer ─────────────


async def test_the_default_mode_with_nothing_to_serve_exits_silently(kernel, tmp_path):
    from kernel.modes.longrun import _default

    assert await _default(kernel.ctx) is None
    assert not (tmp_path / ".fantastic" / "lock.yaml").exists()


async def test_the_default_mode_fails_loud_when_the_kernel_cannot_start(
    kernel, tmp_path
):
    import pytest

    from kernel.modes.longrun import _default

    kernel.create("web.tools", id="web_pin", port=0)
    with pytest.raises(RuntimeError) as e:
        await _default(kernel.ctx)
    assert str(e.value) == (
        "kernel_start: the root record names no `memory_store_id` — the kernel "
        "keeps its execution rows (ledger, dead letters, named-call dedup) in a "
        "memory store and will not start without one\n"
        "  hint: a memory store is needed — create a ram_store and name it in "
        "memory_store_id if you have none: `create_agent parent_id=kernel_state "
        "handler_module=ram_store.tools id=memory operator=true`, then "
        "`update_agent id=kernel_state memory_store_id=memory`"
    )
    # the lock was taken for the attempt and released with it
    assert not (tmp_path / ".fantastic" / "lock.yaml").exists()
