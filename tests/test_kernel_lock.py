"""Single-instance lock for `kernel.py serve`."""

from __future__ import annotations

import json
import os

import pytest

from kernel import (
    LOCK_FILE,
    _pid_alive,
    _read_lock,
    _release_serve_lock,
    acquire_serve_lock,
)


@pytest.fixture(autouse=True)
def _cwd(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    yield
    # Defensive cleanup so atexit-registered releases don't trip on
    # state from a previous test (each test runs in its own tmp dir
    # anyway, so the file vanishes — but be explicit).
    if LOCK_FILE.exists():
        try:
            LOCK_FILE.unlink()
        except OSError:
            pass


def test_pid_alive_self():
    assert _pid_alive(os.getpid())


def test_pid_alive_dead():
    # Pick a PID extremely unlikely to be in use.
    assert not _pid_alive(2**31 - 2)


def test_pid_alive_zero_negative():
    assert not _pid_alive(0)
    assert not _pid_alive(-1)


def test_acquire_writes_lock_when_none():
    acquire_serve_lock(8888)
    assert LOCK_FILE.exists()
    data = json.loads(LOCK_FILE.read_text())
    assert data == {"pid": os.getpid(), "port": 8888}


def test_acquire_raises_when_live_lock_present(tmp_path):
    # Pretend another live process owns the lock — use our own pid since
    # `_pid_alive(os.getpid())` is True.
    LOCK_FILE.parent.mkdir(parents=True, exist_ok=True)
    LOCK_FILE.write_text(json.dumps({"pid": os.getpid(), "port": 8888}))
    with pytest.raises(RuntimeError, match="kernel already running"):
        acquire_serve_lock(8889)
    # Existing lock untouched.
    assert json.loads(LOCK_FILE.read_text())["port"] == 8888


def test_acquire_overwrites_stale_lock():
    LOCK_FILE.parent.mkdir(parents=True, exist_ok=True)
    LOCK_FILE.write_text(json.dumps({"pid": 2**31 - 2, "port": 1234}))
    acquire_serve_lock(8888)
    data = json.loads(LOCK_FILE.read_text())
    assert data["pid"] == os.getpid()
    assert data["port"] == 8888


def test_acquire_overwrites_corrupt_lock():
    LOCK_FILE.parent.mkdir(parents=True, exist_ok=True)
    LOCK_FILE.write_text("not json {{{")
    acquire_serve_lock(8888)
    data = json.loads(LOCK_FILE.read_text())
    assert data["pid"] == os.getpid()


def test_release_removes_when_owner():
    acquire_serve_lock(8888)
    assert LOCK_FILE.exists()
    _release_serve_lock()
    assert not LOCK_FILE.exists()


def test_release_keeps_when_not_owner():
    LOCK_FILE.parent.mkdir(parents=True, exist_ok=True)
    LOCK_FILE.write_text(json.dumps({"pid": os.getpid() + 100000, "port": 1111}))
    _release_serve_lock()
    assert LOCK_FILE.exists()


def test_release_silent_when_no_lock():
    # No exception even when the lock has already been removed.
    _release_serve_lock()
    assert not LOCK_FILE.exists()


def test_read_lock_none_when_missing():
    assert _read_lock() is None


def test_read_lock_none_when_corrupt():
    LOCK_FILE.parent.mkdir(parents=True, exist_ok=True)
    LOCK_FILE.write_text("garbage")
    assert _read_lock() is None
