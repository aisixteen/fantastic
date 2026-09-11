"""The message store persists through a LOADER PUMP and hydrates COLD — the kernel knows
neither. One table (bodies and views are rows), a tail (`rows_stream`), a drain, the
`shutdown_agents` half, and the loader's `messages.xml`."""

from __future__ import annotations

import asyncio
import sys
import types
from pathlib import Path

import pytest

from kernel import Kernel
from kernel._kernel import shutdown_context
from kernel_state import messages_xml
from kernel_state.tools import (
    _MESSAGES_FILE,
    flush_pending,
    hydrate_messages,
    read_messages,
)

# ── a test bundle that ADVERTISES `shutdown` and can hold a task in flight ──
_shut_calls: list[str] = []
_run_flag: dict[str, asyncio.Event] = {}


async def _handler(id, payload, kernel):
    verb = payload.get("type")
    if verb == "boot":
        return None
    if verb == "reflect":
        return {
            "id": id,
            "sentence": "shutdown probe",
            "verbs": {"shutdown": "wraps up"},
        }
    if verb == "shutdown":
        _shut_calls.append(id)
        # the shutdown turn may still use its tools: a nested send passes the drain
        r = (await kernel.send([{"target": "kernel", "payload": {"type": "ps"}}]))[0]
        return {"wrapped": True, "tools_ok": r["status"] == "completed"}
    if verb == "slow":
        _run_flag.setdefault(id, asyncio.Event()).set()
        await asyncio.sleep(30)
        return {"finished": True}
    if verb == "work":
        return {"ok": True}
    return {"error": f"unknown {verb!r}", "reason": "unhandled"}


_mod = types.ModuleType("shut_probe.tools")
_mod.handler = _handler
_mod.VERBS = {"reflect": None, "shutdown": None, "slow": None, "work": None}
sys.modules["shut_probe.tools"] = _mod


async def _mk(kernel, aid="sp"):
    r = (
        await kernel.send(
            [
                {
                    "target": kernel.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": kernel.id,
                        "id": aid,
                        "handler_module": "shut_probe.tools",
                        "callable_by": {"$parent": "*", "rest": "*"},
                    },
                }
            ]
        )
    )[0]["result"]
    assert r.get("id") == aid, r
    return aid


# ── the tail ────────────────────────────────────────────────────────────


async def test_rows_stream_replays_after_then_tails_without_duplicates(seeded_kernel):
    k = seeded_kernel.ctx
    a = k.rows_put("ai", {"role": "user", "content": "one"})
    b = k.rows_put("ai", {"role": "user", "content": "two"})
    got: list[int] = []

    async def consume():
        async for row in k.rows_stream(a):
            got.append(row["id"])
            if len(got) == 3:
                return

    t = asyncio.create_task(consume())
    await asyncio.sleep(0.01)
    c = k.rows_put("ai", {"role": "user", "content": "three"})
    d = k.rows_put("other", {"role": "user", "content": "four"})
    await asyncio.wait_for(t, timeout=2)
    assert got == [b, c, d], "replay (> after) then live, once each"
    assert k._tails == [], "a consumer that stopped is dropped"


async def test_rows_stream_keeps_a_pre_minted_row_that_lands_late(seeded_kernel):
    """An AI turn's id is minted at turn start; its row lands after later calls."""
    k = seeded_kernel.ctx
    base = k.rows_put("ai", {"role": "user", "content": "root"})
    got: list[int] = []

    async def consume():
        async for row in k.rows_stream(base):
            got.append(row["id"])
            if len(got) == 3:
                return

    t = asyncio.create_task(consume())
    await asyncio.sleep(0.01)
    turn = k.mint_id()  # reserved now, written later
    later = k.rows_put("notes", {"kind": "call", "verb": "read"})
    later2 = k.rows_put("notes", {"kind": "call", "verb": "read"})
    k.rows_put("ai", {"id": turn, "role": "assistant", "content": "the turn"})
    await asyncio.wait_for(t, timeout=2)
    assert got == [later, later2, turn], "arrival order, the late pre-minted row kept"


# ── the drain and the agents half of a stop ─────────────────────────────


async def test_draining_refuses_new_roots_but_not_live_work_or_shutdown_traffic(
    seeded_kernel,
):
    a = await _mk(seeded_kernel)
    k = seeded_kernel.ctx
    k.draining = True
    try:
        out = (await seeded_kernel.send([{"target": a, "payload": {"type": "work"}}]))[
            0
        ]
        assert out["status"] == "failed" and out["result"]["reason"] == "draining"
        assert out["pid"] is None, "a refused root costs no id"
        with shutdown_context():
            out = (
                await seeded_kernel.send([{"target": a, "payload": {"type": "work"}}])
            )[0]
        assert out["status"] == "completed"
    finally:
        k.draining = False


async def test_shutdown_agents_tells_advertisers_waits_then_cancels_the_rest(
    seeded_kernel,
):
    _shut_calls.clear()
    a = await _mk(seeded_kernel, "sp_a")
    b = await _mk(seeded_kernel, "sp_b")
    k = seeded_kernel.ctx
    k.QUIET_TIMEOUT = 0.3
    slow = asyncio.create_task(
        seeded_kernel.send([{"target": b, "payload": {"type": "slow"}}])
    )
    await asyncio.wait_for(_run_flag.setdefault(b, asyncio.Event()).wait(), timeout=2)
    # a keepalive loop (a persister) survives the quiet phase
    # THE KERNEL CREATES THE TASK — it hands over a coroutine, not an
    # already-running Task, because only the kernel can mint the id the fork
    # must run under.
    kid = k.spawn_task(
        seeded_kernel.id, "message_pump", asyncio.sleep(30), keepalive=True
    )
    keep = k.tasks[kid]["_task"]
    out = (
        await seeded_kernel.send(
            [{"target": "kernel", "payload": {"type": "shutdown_agents"}}]
        )
    )[0]["result"]
    assert out.get("drained") is True and sorted(out["shut"]) == [a, b], out
    assert _shut_calls and set(_shut_calls) == {a, b}, "every advertiser was told"
    assert out["cancelled"], "the slow task was cancelled after the quiet bound"
    assert kid in k.tasks, "the keepalive loop is not pending work"
    settled = await asyncio.wait_for(slow, timeout=2)
    assert settled[0]["status"] == "cancelled"
    # the kernel keeps serving reads; a new root is refused
    r = (await seeded_kernel.send([{"target": a, "payload": {"type": "work"}}]))[0]
    assert r["result"]["reason"] == "draining"
    again = (
        await seeded_kernel.send(
            [{"target": "kernel", "payload": {"type": "shutdown_agents"}}]
        )
    )[0]["result"]
    assert again.get("drained") is True and again.get("cancelled") == [], again
    keep.cancel()
    k.draining = False


async def test_shutdown_agents_is_root_only(seeded_kernel):
    a = await _mk(seeded_kernel, "sp_c")
    r = (
        await seeded_kernel.send(
            [{"target": a, "payload": {"type": "shutdown_agents"}}]
        )
    )[0]
    assert "error" in r["result"] or r["result"].get("reason") == "unhandled"


# ── the loader: cold hydrate, the pump, the last window ────────────────


def _lines(root_dir: Path) -> list[dict]:
    p = root_dir / _MESSAGES_FILE
    if not p.exists():
        return []
    return messages_xml.parse_messages(p.read_text())


async def test_read_messages_round_trips_and_sets_the_counter(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    root = Path(".fantastic")
    root.mkdir()
    rows = [
        {
            "id": 3,
            "agent": "ai",
            "role": "user",
            "content": "hi",
            "parent": None,
            "ts": 1.0,
        },
        {
            "id": 5,
            "agent": "ai",
            "kind": "body",
            "parent": 4,
            "content": "BODY",
            "ts": 1.0,
        },
        {
            "id": 9,
            "agent": "ai",
            "kind": "view",
            "ids": [{"id": 3, "res": "full"}],
            "ts": 1.0,
        },
    ]
    (root / _MESSAGES_FILE).write_text(
        messages_xml.HEADER + "".join(messages_xml.row_to_xml(r) for r in rows)
    )
    got, nxt = read_messages(root)
    assert [r["id"] for r in got] == [3, 5, 9] and nxt == 10
    k = Kernel()
    assert hydrate_messages(k, root) == 3
    assert k.next_id == 10 and k.mint_id() == 10, (
        "the first mint is above every hydrated id"
    )
    assert k.body_get(4, 0, 10) == ("BODY", 4, True, 4)
    assert k.view_get("ai")["ids"][0]["id"] == 3
    assert [r["id"] for r in k.rows_for("ai")] == [3, 5, 9]
    with pytest.raises(ValueError):
        hydrate_messages(k, root)  # a second ingestion is the user's fault, surfaced


async def test_the_pump_appends_every_row_but_its_own_calls(seeded_kernel, store_agent):
    root = seeded_kernel
    k = root.ctx
    # boot the root: ledger rehydrate + the pump subscribes at the highest id present
    await root.send([{"target": root.id, "payload": {"type": "boot"}}])
    pump = getattr(root, "_message_pump", None)
    assert pump is not None
    # CATCHES UP, rather than starting at the top. `boot` itself now settles —
    # and a settle is a row — so ids keep arriving after the pump computed its
    # start point. `rows_stream(after)` replays everything above that point, so
    # what matters is that no id is missed, not that `seen` equals the high
    # water mark at the instant boot returned.
    for _ in range(100):
        if pump.seen >= k.next_id - 1:
            break
        await asyncio.sleep(0.02)
    assert pump.seen >= k.next_id - 1, (pump.seen, k.next_id)
    before = _lines(Path(".fantastic"))
    a = await _mk(root, "sp_p")
    for i in range(3):
        await root.send([{"target": a, "payload": {"type": "work"}}])
    rid = k.rows_put("ai", {"role": "user", "content": "planted"})
    k.body_put("ai", rid, "a body " * 100)
    await asyncio.sleep(0.5)
    lines = _lines(Path(".fantastic"))
    ids = [r["id"] for r in lines]
    assert ids == sorted(ids) and len(set(ids)) == len(ids), "in id order, once each"
    assert rid in ids and any(
        r.get("kind") == "body" and r["parent"] == rid for r in lines
    )
    # A dispatch writes TWO rows now: the `call` at launch and its `settle`.
    assert all(
        r["kind"] in ("call", "settle")
        for r in lines
        if r["agent"] == a and r.get("kind")
    )
    own = [r for r in lines if r.get("kind") == "call" and r["agent"] == store_agent]
    assert own == [], "the pump's own writes to the store never enter the log"
    assert not any(
        r.get("kind") == "task" and r.get("verb") in pump.VERBS for r in lines
    )
    assert len(lines) > len(before)
    # the last window lands on the shutdown hook, provider still alive
    late = k.rows_put("ai", {"role": "user", "content": "last"})
    await flush_pending(root)
    assert late in [r["id"] for r in _lines(Path(".fantastic"))]
    assert getattr(root, "_message_pump", None) is None
    # a fresh kernel on the same dir hydrates it all and continues the counter
    from _testkit import boot_root

    k2 = boot_root()
    hydrated = [r["id"] for r in k2.ctx.rows_for("ai")]
    assert late in hydrated and rid in hydrated
    assert k2.ctx.next_id > late and k2.ctx.body_get(rid, 0, 6) == (
        "a body",
        6,
        False,
        700,
    )


async def test_no_store_means_ram_only_and_says_so_once(seeded_kernel, capsys):
    root = seeded_kernel
    await root.send([{"target": root.id, "payload": {"type": "boot"}}])
    pump = getattr(root, "_message_pump", None)
    assert pump is not None
    for i in range(2):
        root.ctx.rows_put("ai", {"role": "user", "content": str(i)})
        await asyncio.sleep(0.3)
    assert not (Path(".fantastic") / _MESSAGES_FILE).exists()
    assert capsys.readouterr().out.count("no store wired") == 1
    await flush_pending(root)


async def test_kernel_shutdown_runs_the_flush_hook_before_the_walk(
    seeded_kernel, store_agent, monkeypatch
):
    root = seeded_kernel
    await root.send([{"target": root.id, "payload": {"type": "boot"}}])
    order: list[str] = []
    import kernel_state.tools as kst

    real = kst.flush_pending

    async def spy(agent):
        order.append("flush")
        await real(agent)

    monkeypatch.setattr(kst, "flush_pending", spy)
    real_shutdown = root.shutdown

    async def walk():
        order.append("walk")
        await real_shutdown()

    monkeypatch.setattr(root, "shutdown", walk)
    rid = root.ctx.rows_put("ai", {"role": "user", "content": "bye"})
    await root.ctx.shutdown()
    assert order[:2] == ["flush", "walk"], order  # on_shutdown re-runs the hook, idle
    assert rid in [r["id"] for r in _lines(Path(".fantastic"))]
    assert root.ctx.draining is True and root.ctx._shutdown_complete is True


def test_xml_rows_round_trip_every_shape_and_escape():
    """The library escapes; a body may hold the log's own tags, `&`, newlines, and a
    megabyte, and every scalar comes back typed."""
    rows = [
        {
            "id": 1,
            "agent": "ai",
            "role": "assistant",
            "parent": None,
            "ts": 1.5,
            "content": "a </content> & </row> <messages> inside\nline 2\n",
            "title": 'quoted "title" with <tag>',
        },
        {"id": 2, "agent": "ai", "kind": "call", "verb": "read", "internal": True},
        {"id": 3, "agent": "ai", "kind": "fold", "level": 2, "refs": [1, 2]},
        {
            "id": 4,
            "agent": "ai",
            "kind": "view",
            "ids": [{"id": 3, "res": "full"}],
            "budget": 12,
            "why": "t",
        },
        {
            "id": 5,
            "agent": "ai",
            "kind": "body",
            "parent": 2,
            "content": "x" * 1_000_000,
        },
        {"id": 6, "agent": "ai", "kind": "late", "content": ""},
    ]
    text = messages_xml.HEADER + "".join(messages_xml.row_to_xml(r) for r in rows)
    back = messages_xml.parse_messages(text)
    want = [{k: v for k, v in r.items() if v is not None} for r in rows]
    assert back == want


def test_a_cut_last_row_is_a_loud_error_not_a_repair():
    import xml.etree.ElementTree as ET

    good = messages_xml.row_to_xml({"id": 1, "agent": "ai", "content": "ok"})
    cut = messages_xml.row_to_xml({"id": 2, "agent": "ai", "content": "lost"})[:-12]
    with pytest.raises(ET.ParseError, match="row 1"):
        messages_xml.parse_messages(messages_xml.HEADER + good + cut)


def test_an_unknown_log_version_refuses(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    root = Path(".fantastic")
    root.mkdir()
    (root / _MESSAGES_FILE).write_text('<messages v="9">\n')
    with pytest.raises(RuntimeError, match="unknown log version"):
        read_messages(root)


async def test_a_slow_settle_still_reaches_the_log(seeded_kernel, store_agent):
    """THE BUG THE SETTLE ROW EXISTS FOR. The call row is appended at LAUNCH and
    the outcome used to be stamped onto that same dict in place; the pump renders
    rows on a 0.15 s debounce, so a call that settled after its launch row had
    already been written left NO ending on disk. Everything slow — every AI turn,
    every job, every stream — was recorded as a beginning with no end, and a cold
    read could not tell "never settled" from "settled late".

    Here the handler takes 0.4 s, comfortably past the debounce."""
    root = seeded_kernel
    await root.send([{"target": root.id, "payload": {"type": "boot"}}])

    async def _slow(id, payload, kernel):
        if payload.get("type") in ("boot", "shutdown"):
            return None
        if payload.get("type") == "reflect":
            return {"id": id, "sentence": "slow", "verbs": {}}
        await asyncio.sleep(0.4)
        return {"ok": True}

    import sys
    import types

    m = types.ModuleType("slow_settle.tools")
    m.handler = _slow
    sys.modules["slow_settle.tools"] = m
    root.create("slow_settle.tools", id="slowpoke")

    out = (await root.send([{"target": "slowpoke", "payload": {"type": "work"}}]))[0]
    pid = out["pid"]
    assert out["status"] == "completed"
    await asyncio.sleep(0.5)

    lines = _lines(Path(".fantastic"))
    call = next((r for r in lines if r["id"] == pid), None)
    assert call is not None, f"the launch row never reached the log: {lines}"
    settle = next(
        (r for r in lines if r.get("kind") == "settle" and r.get("parent") == pid),
        None,
    )
    assert settle is not None, (
        "the call settled 0.4 s after launch and the log records no ending — "
        f"rows on disk: {[(r['id'], r.get('kind')) for r in lines]}"
    )
    assert settle["outcome"]["status"] == "completed", settle
