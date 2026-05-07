"""Unit tests for local_runner.tools.

Strategy: drive the verbs directly against an in-process Kernel with a
record + a mocked subprocess.Popen. No real `fantastic` invoked, no
actual ports bound — we simulate the spawned kernel by writing a fake
lock.json into the project directory mid-test.
"""

from __future__ import annotations

import json
import os
import signal

import pytest

from kernel import Kernel
from local_runner import tools as lr


@pytest.fixture
def k(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    return Kernel()


@pytest.fixture
def proj(tmp_path):
    p = tmp_path / "myproj"
    p.mkdir()
    (p / ".fantastic").mkdir()
    return p


def _make_rec(k, proj_path, **extra):
    rec = k.create(
        "local_runner.tools",
        remote_path=str(proj_path),
        **extra,
    )
    return rec["id"]


# ─── reflect ────────────────────────────────────────────────────


async def test_reflect_basic_fields(k, proj):
    aid = _make_rec(k, proj, display_name="myapp")
    r = await lr._reflect(aid, {}, k)
    assert r["id"] == aid
    assert r["remote_path"] == str(proj)
    assert r["remote_cmd"] == "fantastic"
    assert r["entry_path"] == ""
    assert r["running"] is False
    assert r["pid"] is None
    assert "verbs" in r
    assert set(r["verbs"]).issuperset(
        {
            "reflect",
            "boot",
            "shutdown",
            "start",
            "stop",
            "restart",
            "status",
            "get_webapp",
        }
    )


async def test_reflect_running(k, proj):
    aid = _make_rec(k, proj)
    # Simulate live serve: write lock.json with our own pid (always alive).
    lock = proj / ".fantastic" / "lock.json"
    lock.write_text(json.dumps({"pid": os.getpid(), "port": 49001}))
    r = await lr._reflect(aid, {}, k)
    assert r["running"] is True
    assert r["pid"] == os.getpid()
    assert r["port"] == 49001


# ─── boot ───────────────────────────────────────────────────────


async def test_boot_is_noop(k, proj):
    aid = _make_rec(k, proj)
    assert await lr._boot(aid, {}, k) is None


# ─── start ──────────────────────────────────────────────────────


async def test_start_missing_remote_path(k, tmp_path):
    rec = k.create("local_runner.tools")  # no remote_path
    r = await lr._start(rec["id"], {}, k)
    assert "error" in r
    assert "remote_path" in r["error"]


async def test_start_not_a_directory(k, tmp_path):
    aid = _make_rec(k, tmp_path / "nonexistent")
    r = await lr._start(aid, {}, k)
    assert "error" in r
    assert "not a directory" in r["error"]


async def test_start_already_running(k, proj):
    aid = _make_rec(k, proj)
    lock = proj / ".fantastic" / "lock.json"
    lock.write_text(json.dumps({"pid": os.getpid(), "port": 49002}))
    r = await lr._start(aid, {}, k)
    assert r["started"] is True
    assert r["already_running"] is True
    assert r["pid"] == os.getpid()


async def test_start_spawns_and_polls_lock(k, proj, monkeypatch):
    aid = _make_rec(k, proj)
    monkeypatch.setattr(lr, "LOCK_POLL_TIMEOUT", 2.0)
    monkeypatch.setattr(lr, "LOCK_POLL_INTERVAL", 0.05)
    spawned = {}

    def fake_popen(args, **kwargs):
        # Capture spawn args, then immediately write the lock.json the
        # real spawned kernel would write, so the polling loop succeeds.
        spawned["args"] = args
        spawned["cwd"] = kwargs.get("cwd")
        spawned["start_new_session"] = kwargs.get("start_new_session")
        port = int(args[args.index("--port") + 1])
        (proj / ".fantastic" / "lock.json").write_text(
            json.dumps({"pid": os.getpid(), "port": port})
        )

        class _P:  # minimal Popen stand-in
            pass

        return _P()

    monkeypatch.setattr(lr.subprocess, "Popen", fake_popen)
    r = await lr._start(aid, {}, k)
    assert r["started"] is True
    assert r["pid"] == os.getpid()
    assert isinstance(r["port"], int)
    assert spawned["args"][:2] == ["fantastic", "serve"]
    assert "--port" in spawned["args"]
    assert spawned["cwd"] == str(proj)
    assert spawned["start_new_session"] is True


async def test_start_lock_never_appears(k, proj, monkeypatch):
    aid = _make_rec(k, proj)
    monkeypatch.setattr(lr, "LOCK_POLL_TIMEOUT", 0.3)
    monkeypatch.setattr(lr, "LOCK_POLL_INTERVAL", 0.05)
    monkeypatch.setattr(lr.subprocess, "Popen", lambda *a, **kw: type("_P", (), {})())
    r = await lr._start(aid, {}, k)
    assert "error" in r
    assert "lock.json never appeared" in r["error"]
    assert "requested_port" in r


async def test_start_uses_custom_remote_cmd(k, proj, monkeypatch):
    aid = _make_rec(k, proj, remote_cmd="/opt/custom/fantastic")
    monkeypatch.setattr(lr, "LOCK_POLL_TIMEOUT", 1.0)
    monkeypatch.setattr(lr, "LOCK_POLL_INTERVAL", 0.05)
    captured = {}

    def fake_popen(args, **kwargs):
        captured["args"] = args
        port = int(args[args.index("--port") + 1])
        (proj / ".fantastic" / "lock.json").write_text(
            json.dumps({"pid": os.getpid(), "port": port})
        )
        return type("_P", (), {})()

    monkeypatch.setattr(lr.subprocess, "Popen", fake_popen)
    await lr._start(aid, {}, k)
    assert captured["args"][0] == "/opt/custom/fantastic"


# ─── stop ───────────────────────────────────────────────────────


async def test_stop_no_lock_file(k, proj):
    aid = _make_rec(k, proj)
    # Lock dir exists but no file.
    r = await lr._stop(aid, {}, k)
    assert r["stopped"] is True
    assert r["pid"] is None


async def test_stop_already_dead_pid(k, proj):
    aid = _make_rec(k, proj)
    # Use a ridiculous pid that can't exist; lock file will get cleaned up.
    lock = proj / ".fantastic" / "lock.json"
    lock.write_text(json.dumps({"pid": 999999999, "port": 49003}))
    r = await lr._stop(aid, {}, k)
    assert r["stopped"] is True
    # Function returns "already_gone" because os.kill raises immediately.
    assert r.get("already_gone") is True or r["pid"] == 999999999
    assert not lock.exists()


async def test_stop_signals_pid_and_waits(k, proj, monkeypatch):
    aid = _make_rec(k, proj)
    monkeypatch.setattr(lr, "STOP_POLL_TIMEOUT", 0.5)
    monkeypatch.setattr(lr, "STOP_POLL_INTERVAL", 0.05)

    state = {"alive": True}
    sent = []

    def fake_kill(pid, sig):
        sent.append((pid, sig))
        if sig == signal.SIGTERM:
            state["alive"] = False  # cooperative shutdown
        if sig == 0 and not state["alive"]:
            raise ProcessLookupError

    lock = proj / ".fantastic" / "lock.json"
    lock.write_text(json.dumps({"pid": 12345, "port": 49004}))
    monkeypatch.setattr(lr.os, "kill", fake_kill)

    r = await lr._stop(aid, {}, k)
    assert r["stopped"] is True
    assert r["pid"] == 12345
    assert r["died_cleanly"] is True
    # SIGTERM should have been sent; no SIGKILL needed.
    assert any(s[1] == signal.SIGTERM for s in sent)
    assert not any(s[1] == signal.SIGKILL for s in sent)
    assert not lock.exists()


async def test_stop_escalates_to_sigkill_when_pid_wont_die(k, proj, monkeypatch):
    aid = _make_rec(k, proj)
    monkeypatch.setattr(lr, "STOP_POLL_TIMEOUT", 0.2)
    monkeypatch.setattr(lr, "STOP_POLL_INTERVAL", 0.05)

    sent = []

    def fake_kill(pid, sig):
        sent.append((pid, sig))
        # Process refuses to die on SIGTERM, only on SIGKILL.
        if sig == 0 and signal.SIGKILL not in (s[1] for s in sent):
            return  # pretend alive
        if sig == 0:
            raise ProcessLookupError

    (proj / ".fantastic" / "lock.json").write_text(
        json.dumps({"pid": 12346, "port": 49005})
    )
    monkeypatch.setattr(lr.os, "kill", fake_kill)
    r = await lr._stop(aid, {}, k)
    assert r["stopped"] is True
    assert r["died_cleanly"] is False
    assert any(s[1] == signal.SIGKILL for s in sent)


# ─── restart ────────────────────────────────────────────────────


async def test_restart_calls_stop_then_start(k, proj, monkeypatch):
    aid = _make_rec(k, proj)
    calls = []

    async def fake_stop(*a, **kw):
        calls.append("stop")
        return {"stopped": True}

    async def fake_start(*a, **kw):
        calls.append("start")
        return {"started": True, "pid": 1, "port": 49006}

    monkeypatch.setattr(lr, "_stop", fake_stop)
    monkeypatch.setattr(lr, "_start", fake_start)
    r = await lr._restart(aid, {}, k)
    assert calls == ["stop", "start"]
    assert r["started"] is True


# ─── status ─────────────────────────────────────────────────────


async def test_status_not_running(k, proj):
    aid = _make_rec(k, proj)
    r = await lr._status(aid, {}, k)
    assert r["running"] is False
    assert r["pid"] is None
    assert r["http_ok"] is False


async def test_status_running_no_http(k, proj):
    aid = _make_rec(k, proj)
    (proj / ".fantastic" / "lock.json").write_text(
        json.dumps({"pid": os.getpid(), "port": 1})  # port 1 unlikely bound
    )
    r = await lr._status(aid, {}, k)
    assert r["running"] is True
    assert r["pid"] == os.getpid()
    assert r["http_ok"] is False  # nothing listening on :1


# ─── get_webapp ─────────────────────────────────────────────────


async def test_get_webapp_not_running(k, proj):
    aid = _make_rec(k, proj)
    r = await lr._get_webapp(aid, {}, k)
    assert "error" in r
    assert "not running" in r["error"]


async def test_get_webapp_running_returns_url(k, proj):
    aid = _make_rec(k, proj, display_name="myproj", entry_path="canvas/")
    (proj / ".fantastic" / "lock.json").write_text(
        json.dumps({"pid": os.getpid(), "port": 49007})
    )
    r = await lr._get_webapp(aid, {}, k)
    assert r["url"] == "http://localhost:49007/canvas/"
    assert r["title"] == "myproj"
    assert r["default_width"] > 0
    assert r["default_height"] > 0


async def test_get_webapp_default_entry_path(k, proj):
    aid = _make_rec(k, proj, display_name="bare")
    (proj / ".fantastic" / "lock.json").write_text(
        json.dumps({"pid": os.getpid(), "port": 49008})
    )
    r = await lr._get_webapp(aid, {}, k)
    assert r["url"] == "http://localhost:49008/"


# ─── shutdown alias ─────────────────────────────────────────────


async def test_shutdown_calls_stop(k, proj, monkeypatch):
    aid = _make_rec(k, proj)
    called = {}

    async def fake_stop(*a, **kw):
        called["stop"] = True
        return {"stopped": True}

    monkeypatch.setattr(lr, "_stop", fake_stop)
    r = await lr._shutdown(aid, {}, k)
    assert called.get("stop") is True
    assert r["stopped"] is True


# ─── handler dispatch ───────────────────────────────────────────


async def test_handler_unknown_verb(k, proj):
    aid = _make_rec(k, proj)
    r = await lr.handler(aid, {"type": "nope"}, k)
    assert "error" in r
    assert "unknown type" in r["error"]


async def test_handler_routes_known_verb(k, proj):
    aid = _make_rec(k, proj)
    r = await lr.handler(aid, {"type": "reflect"}, k)
    assert r["id"] == aid


# ─── helpers (smoke) ────────────────────────────────────────────


def test_free_port_returns_int():
    p = lr._free_port()
    assert isinstance(p, int)
    assert 1024 <= p <= 65535


def test_pid_alive_self():
    assert lr._pid_alive(os.getpid()) is True


def test_pid_alive_dead():
    assert lr._pid_alive(999999999) is False


def test_pid_alive_zero():
    assert lr._pid_alive(0) is False


def test_read_lock_missing(tmp_path):
    assert lr._read_lock(str(tmp_path)) is None


def test_read_lock_valid(tmp_path):
    (tmp_path / ".fantastic").mkdir()
    (tmp_path / ".fantastic" / "lock.json").write_text('{"pid": 1, "port": 8000}')
    assert lr._read_lock(str(tmp_path)) == {"pid": 1, "port": 8000}


def test_read_lock_corrupt(tmp_path):
    (tmp_path / ".fantastic").mkdir()
    (tmp_path / ".fantastic" / "lock.json").write_text("not json")
    assert lr._read_lock(str(tmp_path)) is None
