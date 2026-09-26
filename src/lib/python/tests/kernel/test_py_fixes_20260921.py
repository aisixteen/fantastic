"""Twins for the 2026-09-21 python fixes: the flusher loop and the one-shot spin.

A. THE KERNEL'S OWN FLUSH NEVER RE-RECORDS ITSELF. A flush is a call to the store,
   and recording THAT call would mint another flush, forever. The flush row the
   batch carries IS the record of that call (see `Kernel._is_own_flush_row`): an
   idle started kernel writes nothing.
B. A ONE-SHOT SHUTS DOWN ON THE LOOP THAT RAN THE CALL, fast.
C. A DAEMON RELEASES ITS PID LOCK ON SIGTERM. The quiet phase waited for the task
   table to EMPTY, so a `spawn_task` service loop (the scheduler's `tick_loop`) —
   which never settles by construction — held it for the whole `QUIET_TIMEOUT`:
   SIGTERM→exit 10.25 s with a scheduler on the tree, 0.29 s without. The e2e
   harness SIGKILLs 10 s after the SIGTERM, so the kill landed first and
   `.fantastic/lock.yaml` survived the stop (`e2e/runs/parity/r3-20260921`).
"""

from __future__ import annotations

import asyncio
import json
import signal
import socket
import subprocess
import sys
import time
from pathlib import Path


async def _start_with_store(k):
    await k.send(
        [
            {
                "target": k.id,
                "payload": {
                    "type": "create_agent",
                    "parent_id": k.id,
                    "id": "memory",
                    "handler_module": "ram_store.tools",
                    "operator": True,
                    "admins": ["kernel"],
                },
            }
        ]
    )
    k.ctx.update(k.id, memory_store_id="memory")
    out = await k.ctx.start(boot_all=False)
    assert out.get("started"), out


def _store_rows(k) -> int:
    t = k.ctx.agents["memory"]._ram_tables
    return sum(len(v) for v in t.rows.values())


async def test_an_idle_started_kernel_writes_nothing(seeded_kernel):
    """The anti-spin property: with the flusher running and NOTHING happening, the
    store stops growing. A flush that recorded its own call would never settle."""
    k = seeded_kernel
    await _start_with_store(k)
    await asyncio.sleep(0.6)  # let the start's own rows drain
    base = _store_rows(k)
    await asyncio.sleep(1.2)  # ≥ 8 flusher debounce windows of pure idle
    assert _store_rows(k) == base, "the flush recorded itself"


async def test_a_real_settle_reaches_the_store(seeded_kernel):
    """Skipping the flush's OWN rows must not skip anybody else's: a real call lands
    as a `call` row and a `settle` row."""
    k = seeded_kernel
    await _start_with_store(k)
    out = await k.send(
        [{"target": "kernel", "payload": {"type": "ps"}, "id": "a-real-call"}]
    )
    pid = out[0]["pid"]
    await k.ctx.flush_executions()
    t = k.ctx.agents["memory"]._ram_tables
    rows = [r for owner in t.rows.values() for r in owner.values()]
    call = [r for r in rows if r.get("id") == pid]
    assert call and call[0]["name"] == "a-real-call", rows
    settle = [r for r in rows if r.get("parent") == pid and r.get("kind") == "settle"]
    assert settle and settle[0]["outcome"]["status"] == "completed", settle


def test_a_one_shot_is_fast_and_clean(tmp_path):
    """The product path: `main.py kernel ps` in a tree with a store named. A
    one-shot is an UNSTARTED kernel: it answers and exits, fast."""
    import main

    d = tmp_path / ".fantastic"
    (d / "agents" / "memory").mkdir(parents=True)
    (d / "agent.yaml").write_text(
        json.dumps(
            {
                "id": "kernel_state",
                "handler_module": "kernel_state.tools",
                "memory_store_id": "memory",
            }
        )
    )
    (d / "agents" / "memory" / "agent.yaml").write_text(
        json.dumps(
            {
                "id": "memory",
                "handler_module": "ram_store.tools",
                "parent_id": "kernel_state",
                "operator": True,
            }
        )
    )
    t0 = time.monotonic()
    p = subprocess.run(
        [sys.executable, str(Path(main.__file__)), "kernel", "ps"],
        cwd=tmp_path,
        capture_output=True,
        text=True,
        timeout=60,
    )
    wall = time.monotonic() - t0
    assert p.returncode == 0, p.stderr
    assert wall < 5.0, f"one-shot took {wall:.1f}s (the atexit spin was 11.5s)"
    assert '"tasks"' in p.stdout, p.stdout


async def test_a_service_loop_does_not_hold_the_quiet_phase(seeded_kernel):
    """A `spawn_task` fork is cancelled by the stop, never waited for."""
    k = seeded_kernel
    ctx = k.ctx

    async def _forever() -> None:
        while True:
            await asyncio.sleep(3600)

    tid = ctx.spawn_task(k.id, "tick_loop", _forever())
    assert ctx.tasks[tid]["_forked"] is True
    t0 = time.monotonic()
    out = await ctx.shutdown_agents()
    wall = time.monotonic() - t0
    assert wall < 1.0, f"the quiet phase waited {wall:.1f}s on a service loop"
    assert tid in out["cancelled"], out
    assert tid not in ctx.tasks


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def _record(d: Path, rid: str, **meta) -> None:
    p = d / "agents" / rid
    p.mkdir(parents=True)
    (p / "agent.yaml").write_text(
        json.dumps({"id": rid, "parent_id": "kernel_state", **meta})
    )


def test_a_daemon_releases_its_lock_well_inside_the_stop_window(tmp_path):
    """The product path: the daemon the e2e harness spawns, stopped the way it
    stops it. 10 s after the SIGTERM the harness SIGKILLs — which runs no
    `atexit` and no `__exit__` — so a stop that outruns the window strands
    `.fantastic/lock.yaml` and the next boot in that dir refuses."""
    import main

    d = tmp_path / ".fantastic"
    d.mkdir()
    (d / "agent.yaml").write_text(
        json.dumps(
            {
                "id": "kernel_state",
                "handler_module": "kernel_state.tools",
                "memory_store_id": "memory",
            }
        )
    )
    _record(d, "memory", handler_module="ram_store.tools", operator=True)
    _record(
        d,
        "store",
        handler_module="file_bridge.tools",
        root=".fantastic",
        ingress_rule="allow_all_ingress",
    )
    # `web` is what keeps a non-tty kernel alive as a daemon at all; the
    # scheduler is the service loop that used to hold the quiet phase open.
    _record(d, "web", handler_module="web.tools", port=_free_port())
    _record(d, "sched", handler_module="scheduler.tools", file_bridge_id="store")

    klog = tmp_path / "klog.txt"
    with klog.open("wb") as out:
        p = subprocess.Popen(
            [sys.executable, str(Path(main.__file__))],
            cwd=tmp_path,
            stdin=subprocess.DEVNULL,
            stdout=out,
            stderr=subprocess.STDOUT,
        )
        try:
            for _ in range(600):
                if "[kernel] up" in klog.read_text(errors="replace"):
                    break
                assert p.poll() is None, klog.read_text(errors="replace")
                time.sleep(0.1)
            else:
                raise AssertionError(f"never booted:\n{klog.read_text()}")
            assert (d / "lock.yaml").exists(), "the daemon took no lock"
            t0 = time.monotonic()
            p.send_signal(signal.SIGTERM)
            rc = p.wait(timeout=30)
            wall = time.monotonic() - t0
        finally:
            if p.poll() is None:
                p.kill()
                p.wait(timeout=10)
    assert rc == 0, f"exit {rc}:\n{klog.read_text(errors='replace')}"
    assert wall < 5.0, f"stop took {wall:.1f}s (the harness kills at 10 s)"
    assert not (d / "lock.yaml").exists(), "the PID lock outlived the daemon"


# ─── D. THE REST LEG ANSWERS — IT DOES NOT RAISE INSIDE THE ASGI APP ───
# Measured in `e2e/runs/parity/python-baseline/53801fd8-20260921/a/wire`: the
# python kernel's OWN log carried two `Exception in ASGI application`
# tracebacks (196 diff lines, identical against both ports) and uvicorn
# answered a bare text/plain 500 for each — cases `invalid_utf8_body` (a body
# that is not UTF-8, so `json.loads` raises UnicodeDecodeError, not
# JSONDecodeError) and `rest_self_get_routes` (a reply carrying LIVE endpoint
# callables, which `JSONResponse` cannot render). The second is fixed at the
# source: `get_routes` is DECLARATIVE — the endpoints reach `web` through the
# `route_endpoints(agent)` module hook — so the verb answers 200 like rust/swift
# and its settled row survives `_ledger_dump`'s strict round-trip.


async def _rest_client(kernel, rid: str = "pyfix_rest"):
    """A `web_rest` leg on `kernel`, called in-process over httpx's ASGI
    transport — which RE-RAISES an app exception, so a crash fails the test
    rather than arriving as a status code."""
    import httpx
    from fastapi import FastAPI

    from web_rest.tools import _make_post_endpoint

    rec = (
        await kernel.send(
            [
                {
                    "target": kernel.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": kernel.id,
                        "handler_module": "web_rest.tools",
                        "id": rid,
                        "ingress_rule": "allow_all_ingress",
                    },
                }
            ]
        )
    )[0]["result"]
    assert rec.get("id") == rid, rec
    app = FastAPI()
    app.add_api_route(
        f"/{rid}/{{target_id}}", _make_post_endpoint(rid, kernel), methods=["POST"]
    )
    return httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://pyfix"
    )


async def test_a_body_that_is_not_utf8_is_a_malformed_body_not_a_crash(kernel):
    """RFC 8259 §8.1 — a JSON body IS UTF-8; bytes that don't decode are the
    same client error as `{not json`."""
    async with await _rest_client(kernel) as c:
        r = await c.post(
            "/pyfix_rest/pyfix_rest",
            content=b'{"type": "reflect", "x": "\xff\xfe"}',
            headers={"content-type": "application/json"},
        )
    assert r.status_code == 400
    assert r.json() == {"error": "web_rest: request body is not valid JSON"}


async def test_get_routes_is_declarative_and_answers_200_over_rest(kernel):
    """`get_routes` is DATA now — the live endpoints reach `web` through the
    `route_endpoints(agent)` module hook, not through a reply. So the verb
    renders over its own leg, 200, exactly as rust and swift answer it."""
    async with await _rest_client(kernel) as c:
        r = await c.post("/pyfix_rest/pyfix_rest", json={"type": "get_routes"})
        ok = await c.post("/pyfix_rest/pyfix_rest", json={"type": "reflect"})
    assert r.status_code == 200, r.text
    assert r.json() == {
        "routes": [
            {"kind": "http", "method": "POST", "path": "/pyfix_rest/{target_id}"},
            {"kind": "http", "method": "GET", "path": "/pyfix_rest/_reflect"},
            {
                "kind": "http",
                "method": "GET",
                "path": "/pyfix_rest/_reflect/{target_id}",
            },
        ]
    }
    # the same leg still carries an ordinary reply, byte-for-byte as before
    assert ok.status_code == 200 and ok.json()["id"] == "pyfix_rest"


def test_a_reply_with_no_json_form_is_still_named_not_raised():
    """`_json_reply` stays: a reply that genuinely has no JSON form (an
    in-process object, an out-of-range float) is a stated transport refusal,
    never an exception raised inside the ASGI app."""
    from web_rest.tools import _json_reply

    for reply in ({"endpoint": lambda: None}, {"x": float("inf")}):
        resp = _json_reply(reply)
        assert resp.status_code == 500
        body = json.loads(resp.body)
        assert body["reason"] == "transport_error", body
        assert "no JSON representation" in body["error"], body
        assert body["hint"]


async def test_a_settled_get_routes_is_a_storable_row(kernel):
    """A settle row carries the FULL result body, and the kernel's store has to be
    able to hold it. With the callables gone from `get_routes`, python records the
    row rust and swift record."""
    from kernel._kernel import _store_safe

    await kernel.send(
        [
            {
                "target": kernel.id,
                "payload": {
                    "type": "create_agent",
                    "parent_id": kernel.id,
                    "handler_module": "web_ws.tools",
                    "id": "pyfix_ws",
                },
            }
        ]
    )
    out = (
        await kernel.send(
            [
                {
                    "target": "pyfix_ws",
                    "payload": {"type": "get_routes"},
                    "id": "gr-1",
                }
            ]
        )
    )[0]
    assert out["status"] == "completed", out
    rows = kernel.ctx.rows
    call = next(r for r in rows.values() if r.get("name") == "gr-1")
    assert call["agent"] == "pyfix_ws"
    settle = next(r for r in rows.values() if r.get("parent") == call["id"])
    assert settle["outcome"] == {
        "status": "completed",
        "result": {"routes": [{"kind": "websocket", "path": "/{host_id}/ws"}]},
    }
    assert json.loads(json.dumps(_store_safe(settle)))["outcome"] == settle["outcome"]
