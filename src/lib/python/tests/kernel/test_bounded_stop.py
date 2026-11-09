"""#562 — a signal stop is bounded: the clean stop first, a hang past STOP_GRACE
or a second stop signal releases the lock and exits 1 (twins of the Rust host's
`stop_bounded` tests). Python writes no run file, so the lock is what is checked."""

from __future__ import annotations

import asyncio
import json
import socket
import subprocess
import sys
import time
import types
from pathlib import Path

import _testkit

import kernel.modes.longrun as longrun
from kernel import Kernel, _lock, register_unlisted
from kernel.modes.longrun import Stopped

_ENTERED: set[str] = set()

_mod = types.ModuleType("stop_hang.tools")


async def _handler(id, payload, kernel):
    if payload.get("type") == "hang":
        _ENTERED.add(id)
        await asyncio.Event().wait()
    return {}


_mod.handler = _handler
_mod.VERBS = {}
register_unlisted("stop_hang.tools", _mod)


def test_stop_grace_is_the_page_grace_plus_the_quiet_window():
    assert longrun.STOP_GRACE == 15.0
    assert longrun.STOP_GRACE == 5.0 + Kernel.QUIET_TIMEOUT


async def test_a_stop_with_a_call_in_flight_finishes_clean_inside_the_grace(
    seeded_kernel,
):
    await seeded_kernel.send(
        [
            {
                "target": "kernel_state",
                "payload": {
                    "type": "create_agent",
                    "parent_id": seeded_kernel.id,
                    "id": "hang562",
                    "handler_module": "stop_hang.tools",
                },
            }
        ]
    )
    call = asyncio.create_task(
        seeded_kernel.send([{"target": "hang562", "payload": {"type": "hang"}}])
    )
    for _ in range(50):
        if "hang562" in _ENTERED:
            break
        await asyncio.sleep(0.1)
    assert "hang562" in _ENTERED
    t0 = time.monotonic()
    out = await longrun.stop_bounded(
        seeded_kernel.ctx.shutdown(), asyncio.Event().wait(), longrun.STOP_GRACE
    )
    dt = time.monotonic() - t0
    assert out is Stopped.CLEAN
    assert 9.0 <= dt < 15.0, dt
    entries = await seeded_kernel.ctx.ledger_entries()
    dead = [
        e
        for e in entries
        if e.get("target") == "hang562" and e.get("status") == "cancelled"
    ]
    assert dead, entries
    assert dead[0].get("acked") is not True
    call.cancel()
    await asyncio.gather(call, return_exceptions=True)


def _lock_setup(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    _lock.acquire_lock()
    assert (tmp_path / ".fantastic" / "lock.yaml").exists()


def _lock_gone(tmp_path):
    assert not (tmp_path / ".fantastic" / "lock.yaml").exists()
    assert _lock._held is None
    _lock.acquire_lock()
    _lock.release_lock()


async def test_a_hang_past_the_grace_is_cut_with_the_lock_and_run_file_gone(
    tmp_path, monkeypatch, capsys
):
    """Python writes no run file; the lock is what the cut releases."""
    _lock_setup(tmp_path, monkeypatch)
    t0 = time.monotonic()
    out = await longrun.stop_bounded(
        asyncio.Event().wait(), asyncio.Event().wait(), 0.3
    )
    dt = time.monotonic() - t0
    assert out is Stopped.CUT
    assert 0.3 <= dt < 2, dt
    _lock_gone(tmp_path)
    assert "did not finish within 0.3 s" in capsys.readouterr().err


async def test_a_second_signal_cuts_at_once(tmp_path, monkeypatch, capsys):
    _lock_setup(tmp_path, monkeypatch)
    t0 = time.monotonic()
    out = await longrun.stop_bounded(
        asyncio.Event().wait(), asyncio.sleep(0.1), longrun.STOP_GRACE
    )
    assert out is Stopped.CUT
    assert time.monotonic() - t0 < 2
    _lock_gone(tmp_path)
    assert "a second signal" in capsys.readouterr().err


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


_LAUNCH = """\
import asyncio, sys, types
from pathlib import Path
sys.path.insert(0, {main_dir!r})
from kernel import register_unlisted

mod = types.ModuleType("stop_hang.tools")


async def handler(id, payload, kernel):
    return {{}}


async def on_shutdown(agent):
    await asyncio.Event().wait()


mod.handler = handler
mod.on_shutdown = on_shutdown
mod.VERBS = {{}}
register_unlisted("stop_hang.tools", mod)
import main

main.main_dispatch()
"""


def _spawn_hanging_daemon(tmp_path):
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
    _record(d, "memory", handler_module="ram_store.tools")
    _record(
        d,
        "store",
        handler_module="file_bridge.tools",
        allow=["."],
        root=".fantastic",
        ingress_rule="allow_all_ingress",
    )
    _record(d, "web", handler_module="web.tools", port=_free_port())
    _record(d, "hang", handler_module="stop_hang.tools")
    (tmp_path / "launch.py").write_text(
        _LAUNCH.format(main_dir=str(Path(main.__file__).parent))
    )
    klog = tmp_path / "klog.txt"
    out = klog.open("wb")
    p = subprocess.Popen(
        [sys.executable, "launch.py"],
        cwd=tmp_path,
        stdin=subprocess.DEVNULL,
        stdout=out,
        stderr=subprocess.STDOUT,
        **_testkit.POLITE_SPAWN,
    )
    for _ in range(600):
        if "[kernel] up" in klog.read_text(errors="replace"):
            return p, d, klog
        assert p.poll() is None, klog.read_text(errors="replace")
        time.sleep(0.1)
    p.kill()
    raise AssertionError(f"never booted:\n{klog.read_text(errors='replace')}")


def test_a_daemon_cut_by_a_second_signal_exits_1_at_once_with_the_lock_released(
    tmp_path,
):
    p, d, klog = _spawn_hanging_daemon(tmp_path)
    try:
        _testkit.polite_stop(p)
        time.sleep(1.0)
        _testkit.polite_stop(p)
        t1 = time.monotonic()
        rc = p.wait(timeout=10)
        assert rc == 1
        assert time.monotonic() - t1 < 3
        assert not (d / "lock.yaml").exists()
        assert "a second signal" in klog.read_text(errors="replace")
    finally:
        if p.poll() is None:
            p.kill()
            p.wait(timeout=10)


def test_a_daemon_whose_stop_hangs_is_cut_at_the_grace_and_exits_1(tmp_path):
    p, d, klog = _spawn_hanging_daemon(tmp_path)
    try:
        _testkit.polite_stop(p)
        t0 = time.monotonic()
        rc = p.wait(timeout=40)
        dt = time.monotonic() - t0
        assert rc == 1
        assert 14.5 <= dt < 20, dt
        assert not (d / "lock.yaml").exists()
        assert "did not finish within 15 s" in klog.read_text(errors="replace")
    finally:
        if p.poll() is None:
            p.kill()
            p.wait(timeout=10)
