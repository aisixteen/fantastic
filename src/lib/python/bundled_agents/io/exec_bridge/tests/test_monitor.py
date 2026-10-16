"""exec_bridge MONITORS — a filter on a running job's output
that SENDS what matches to an agent as a turn; polls nothing; throttled by `every`;
capped by `tail`; a kernel task that `cancel`/`unmonitor` ends and that ends with
its job."""

from __future__ import annotations

import asyncio
import sys
import time
import types

import pytest

from _testkit import LF_STDIO, child_env

from kernel import register_unlisted

# ─── a listener that records every message it is sent ───────────────────────

_HEARD: dict[str, list[tuple[float, str]]] = {}


async def _listener_handler(id, payload, kernel):
    if payload.get("type") == "send":
        _HEARD.setdefault(id, []).append((time.monotonic(), payload.get("text", "")))
        return {"ok": True}
    if payload.get("type") == "reflect":
        return {"id": id, "sentence": "records what it is sent"}
    return {"error": "unknown verb"}


_listener = types.ModuleType("monitor_listener_probe")
_listener.handler = _listener_handler
_listener.VERBS = {}
register_unlisted("monitor_listener_probe.tools", _listener)

# Prints `n` lines, one every `gap` seconds; FATAL on the ones listed; then sleeps.
PRINTER = LF_STDIO + (
    "import sys, time\n"
    "n, gap, fatal, hold = int(sys.argv[1]), float(sys.argv[2]), set(sys.argv[3].split(',')), float(sys.argv[4])\n"
    "for k in range(n):\n"
    "    print(('FATAL step %d' if str(k) in fatal else 'INFO step %d') % k, flush=True)\n"
    "    time.sleep(gap)\n"
    "time.sleep(hold)\n"
)


@pytest.fixture(autouse=True)
async def _stop_jobs(seeded_kernel):
    """Every job a test left running is stopped, and its pump allowed to end."""
    yield
    if seeded_kernel.ctx.agents.get("runner") is not None:
        await _call(seeded_kernel, "runner", {"type": "stop"})
        await asyncio.sleep(0.2)


async def _call(k, target, payload):
    return (await k.send([{"target": target, "payload": payload}]))[0]["result"]


async def _setup(k, listener="ear"):
    _HEARD.pop(listener, None)
    r = await _call(
        k,
        k.id,
        {
            "type": "create_agent",
            "parent_id": k.id,
            "id": listener,
            "handler_module": "monitor_listener_probe.tools",
            "callable_by": {"runner": "*"},
        },
    )
    assert r.get("id") == listener, r
    r = await _call(
        k,
        k.id,
        {
            "type": "create_agent",
            "parent_id": k.id,
            "id": "runner",
            "handler_module": "exec_bridge.tools",
            "ingress_rule": "allow_all_ingress",
            "cwd": ".",
            "inherit_env": child_env("PATH"),
            "allowed": [".*"],
            "can_call": {listener: ["send"]},
        },
    )
    assert r.get("id") == "runner", r


async def _printer(k, n, gap, fatal="", hold=5.0):
    r = await _call(
        k,
        "runner",
        {
            "type": "start",
            "command": sys.executable,
            "args": ["-c", PRINTER, str(n), str(gap), fatal or "-", str(hold)],
        },
    )
    assert "job_id" in r, r
    return r["job_id"]


async def _until(pred, secs=6.0, what="condition"):
    for _ in range(int(secs / 0.02)):
        if pred():
            return
        await asyncio.sleep(0.02)
    raise AssertionError(f"{what} never held within {secs}s")


def _heard(listener="ear") -> list[str]:
    return [t for _, t in _HEARD.get(listener, [])]


async def test_a_match_is_sent_with_its_header_and_nothing_else(seeded_kernel):
    k = seeded_kernel
    await _setup(k)
    jid = await _printer(k, 6, 0.05, fatal="2,4")
    r = await _call(
        k,
        "runner",
        {
            "type": "monitor",
            "job_id": jid,
            "notify": "ear",
            "match": "FATAL",
            "label": "w fatal",
        },
    )
    mid = r["monitor_id"]
    assert r == {
        "monitor_id": mid,
        "job_id": jid,
        "notify": "ear",
        "match": "FATAL",
        "every": None,
        "tail": 100,
        "label": "w fatal",
    }, r
    await _until(
        lambda: sum(t.count("FATAL") for t in _heard()) >= 2, what="two FATALs"
    )
    texts = _heard()
    joined = "\n".join(texts)
    assert "FATAL step 2" in joined and "FATAL step 4" in joined, texts
    assert "INFO" not in joined, "only matching lines are sent"
    assert texts[0].startswith(
        f"[monitor {mid} · job {jid} · from the operator's call "
    ), texts[0]
    assert '"w fatal"' in texts[0].split("\n")[0]


async def test_every_throttles_and_tail_caps(seeded_kernel):
    k = seeded_kernel
    await _setup(k)
    jid = await _printer(k, 40, 0.025)  # ~1 s of output, 40 lines
    r = await _call(
        k,
        "runner",
        {
            "type": "monitor",
            "job_id": jid,
            "notify": "ear",
            "every": 0.4,
            "tail": 5,
        },
    )
    assert r.get("monitor_id"), r
    # waited ON the deliveries, not on a clock: a loaded runner starts the
    # printer late, and a fixed sleep then sees too little of its output
    await _until(
        lambda: (
            len(_HEARD.get("ear", [])) >= 2
            and any("earlier lines dropped" in t for t in _heard())
        ),
        8.0,
        "two deliveries, one cut by tail",
    )
    stamps = [t for t, _ in _HEARD.get("ear", [])]
    assert 2 <= len(stamps) <= 5, f"{len(stamps)} deliveries for ~1 s at every=0.4"
    gaps = [b - a for a, b in zip(stamps, stamps[1:])]
    assert all(g >= 0.35 for g in gaps), gaps
    for text in _heard():
        body = text.split("\n")[1:]
        lines = [x for x in body if x.startswith(("INFO", "FATAL"))]
        assert len(lines) <= 5, text
    assert any("earlier lines dropped" in t for t in _heard()), "tail cut something"


async def test_without_match_every_is_required_and_args_are_checked(seeded_kernel):
    k = seeded_kernel
    await _setup(k)
    jid = await _printer(k, 1, 0.01, hold=3.0)
    cases = [
        ({"notify": "ear"}, "missing_arg", "every"),
        ({"notify": "ear", "match": "(", "every": 1}, "bad_arg", "match"),
        ({"notify": "ear", "match": "x", "every": 0}, "bad_arg", "every"),
        ({"notify": "ear", "match": "x", "tail": 0}, "bad_arg", "tail"),
        ({"match": "x"}, "missing_arg", "notify"),
        ({"notify": "nobody", "match": "x"}, "not_found", "notify"),
    ]
    for extra, reason, arg in cases:
        r = await _call(k, "runner", {"type": "monitor", "job_id": jid, **extra})
        assert r.get("reason") == reason and r.get("arg") == arg, (extra, r)
    r = await _call(
        k,
        "runner",
        {"type": "monitor", "job_id": 999999, "notify": "ear", "match": "x"},
    )
    assert r.get("reason") == "not_found", r


async def test_cancel_and_unmonitor_end_a_monitor_and_the_job_keeps_running(
    seeded_kernel,
):
    k = seeded_kernel
    await _setup(k)
    jid = await _printer(k, 400, 0.02, hold=2.0)  # ~8 s of lines
    a = (
        await _call(
            k,
            "runner",
            {
                "type": "monitor",
                "job_id": jid,
                "notify": "ear",
                "match": "INFO",
                "label": "a",
            },
        )
    )["monitor_id"]
    b = (
        await _call(
            k,
            "runner",
            {
                "type": "monitor",
                "job_id": jid,
                "notify": "ear",
                "match": "INFO",
                "label": "b",
            },
        )
    )["monitor_id"]
    listed = (await _call(k, "runner", {"type": "monitors"}))["monitors"]
    assert {m["monitor_id"] for m in listed} == {a, b}, listed
    ps = (await _call(k, "kernel", {"type": "ps"}))["tasks"]
    assert {a, b} <= {t["id"] for t in ps if t["verb"] == "monitor"}, ps
    await _until(
        lambda: any('"a"' in t for t in _heard()) and any('"b"' in t for t in _heard())
    )
    # kernel cancel ends `a`; `unmonitor` ends `b`
    out = await _call(k, "kernel", {"type": "cancel", "task_id": a})
    assert out.get("cancelled") is True, out
    out = await _call(k, "runner", {"type": "unmonitor", "monitor_id": b})
    assert out == {"unmonitored": True, "monitor_id": b}, out
    await _until(
        lambda: (
            not (
                _call
                and any(
                    m
                    for m in __import__(
                        "exec_bridge._monitor", fromlist=["x"]
                    )._MONITORS.values()
                    if m["owner"] == "runner"
                )
            )
        ),
        what="both monitors gone",
    )
    assert (await _call(k, "runner", {"type": "monitors"}))["monitors"] == []
    await asyncio.sleep(0.3)  # an in-flight delivery may still land
    before = len(_heard())
    await asyncio.sleep(0.8)
    assert len(_heard()) == before, "nothing more is sent after the cancel"
    st = (await _call(k, "runner", {"type": "status", "job_id": jid}))["jobs"][0]
    assert st["status"] == "running", "cancelling a monitor never touches its job"
    await _call(k, "runner", {"type": "stop", "job_id": jid})
    r = await _call(k, "runner", {"type": "unmonitor", "monitor_id": a})
    assert r.get("reason") == "not_found", r


async def test_a_monitor_ends_with_its_job_delivering_what_waited(seeded_kernel):
    k = seeded_kernel
    await _setup(k)
    jid = await _printer(k, 3, 0.01, fatal="1", hold=0.3)
    mid = (
        await _call(
            k,
            "runner",
            {"type": "monitor", "job_id": jid, "notify": "ear", "every": 30},
        )
    )["monitor_id"]
    # every=30: the first line goes at once, the rest wait — and go when the job ends
    await _until(
        lambda: any("job ended" in t for t in _heard()),
        secs=5,
        what="the final delivery at job end",
    )
    assert "FATAL step 1" in "\n".join(_heard())
    await _until(
        lambda: mid not in __import__("exec_bridge._monitor", fromlist=["x"])._MONITORS,
        what="monitor gone",
    )


async def test_a_refused_delivery_ends_the_monitor_and_monitors_says_why(
    seeded_kernel, caplog
):
    """The listener does not accept the bridge's `send` (its `callable_by` lacks it):
    the first delivery is refused, the monitor ENDS — never a silent drop: a warning
    line names it, a `monitor_dead` event lands on the bridge's inbox, and
    `monitors` keeps it as `dead` with the refusal until `unmonitor` clears it."""
    k = seeded_kernel
    await _setup(k, "deaf")
    inbox = k.ctx.ensure_inbox("runner")
    r = await _call(
        k, k.id, {"type": "update_agent", "id": "deaf", "callable_by": {"$self": "*"}}
    )
    assert r.get("updated") is True, r
    jid = await _printer(k, 30, 0.05)
    r = await _call(
        k,
        "runner",
        {"type": "monitor", "job_id": jid, "notify": "deaf", "match": "INFO"},
    )
    mid = r["monitor_id"]

    rows: list[dict] = []

    async def listed():
        rows[:] = (await _call(k, "runner", {"type": "monitors"}))["monitors"]

    for _ in range(300):
        await listed()
        if rows and rows[0].get("status") == "dead":
            break
        await asyncio.sleep(0.02)
    assert len(rows) == 1, rows
    m = rows[0]
    assert m["monitor_id"] == mid and m["status"] == "dead", m
    assert "deaf does not accept 'send' from runner" in m["error"], m
    assert m["delivered"] == 0, m
    assert _heard("deaf") == []
    # loud: the event on the bridge's inbox ...
    dead = []

    async def drain():
        while not inbox.empty():
            p = inbox.get_nowait()
            if p.get("type") == "monitor_dead":
                dead.append(p)

    # the event lands right after the status flips: wait for it, then drain the rest
    for _ in range(150):
        await drain()
        if dead:
            break
        await asyncio.sleep(0.02)
    await asyncio.sleep(0.1)
    await drain()
    assert len(dead) == 1, dead
    assert dead[0] == {
        "type": "monitor_dead",
        "monitor_id": mid,
        "job_id": jid,
        "notify": "deaf",
        "reason": "unauthorized",
        "error": m["error"],
    }, dead
    # ... and the warning line
    warned = [r.getMessage() for r in caplog.records if r.levelname == "WARNING"]
    assert any(
        f"exec_bridge runner: monitor {mid} (job {jid}) dead — deaf refused the "
        "delivery: "
        in w
        and "deaf does not accept 'send' from runner" in w
        for w in warned
    ), warned
    # dead stays dead: later output is not sent
    await asyncio.sleep(0.3)
    await listed()
    assert rows[0]["status"] == "dead" and rows[0]["delivered"] == 0, rows
    r = await _call(k, "runner", {"type": "unmonitor", "monitor_id": mid})
    assert r == {"unmonitored": True, "monitor_id": mid}, r
    await listed()
    assert rows == [], rows
