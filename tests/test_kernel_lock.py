"""PID-only lock for `.fantastic/`. The lock file is `{pid:int}` and
nothing else — endpoint discovery (e.g. the web bundle's port) is a
separate concern that lives in whichever bundle publishes a transport,
not in the substrate."""

from __future__ import annotations

import json
import os

import pytest

from kernel import (
    LOCK_FILE,
    _pid_alive,
    _read_lock,
    acquire_lock,
    release_lock,
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
    acquire_lock()
    assert LOCK_FILE.exists()
    data = json.loads(LOCK_FILE.read_text())
    assert data == {"pid": os.getpid()}


def test_acquire_raises_when_live_lock_present(tmp_path):
    # Pretend another live process owns the lock — use our own pid since
    # `_pid_alive(os.getpid())` is True.
    LOCK_FILE.parent.mkdir(parents=True, exist_ok=True)
    LOCK_FILE.write_text(json.dumps({"pid": os.getpid()}))
    with pytest.raises(RuntimeError, match="another fantastic owns this dir"):
        acquire_lock()
    # Existing lock untouched.
    assert json.loads(LOCK_FILE.read_text())["pid"] == os.getpid()


def test_acquire_overwrites_stale_lock():
    LOCK_FILE.parent.mkdir(parents=True, exist_ok=True)
    LOCK_FILE.write_text(json.dumps({"pid": 2**31 - 2}))
    acquire_lock()
    data = json.loads(LOCK_FILE.read_text())
    assert data == {"pid": os.getpid()}


def test_acquire_overwrites_corrupt_lock():
    LOCK_FILE.parent.mkdir(parents=True, exist_ok=True)
    LOCK_FILE.write_text("not json {{{")
    acquire_lock()
    data = json.loads(LOCK_FILE.read_text())
    assert data == {"pid": os.getpid()}


def test_release_removes_when_owner():
    acquire_lock()
    assert LOCK_FILE.exists()
    release_lock()
    assert not LOCK_FILE.exists()


def test_release_keeps_when_not_owner():
    LOCK_FILE.parent.mkdir(parents=True, exist_ok=True)
    LOCK_FILE.write_text(json.dumps({"pid": os.getpid() + 100000}))
    release_lock()
    assert LOCK_FILE.exists()


def test_release_silent_when_no_lock():
    # No exception even when the lock has already been removed.
    release_lock()
    assert not LOCK_FILE.exists()


def test_read_lock_none_when_missing():
    assert _read_lock() is None


def test_read_lock_none_when_corrupt():
    LOCK_FILE.parent.mkdir(parents=True, exist_ok=True)
    LOCK_FILE.write_text("garbage")
    assert _read_lock() is None


def test_context_manager_acquires_and_releases():
    from kernel import FantasticLock

    assert not LOCK_FILE.exists()
    with FantasticLock():
        assert LOCK_FILE.exists()
        assert json.loads(LOCK_FILE.read_text()) == {"pid": os.getpid()}
    assert not LOCK_FILE.exists()


def test_context_manager_raises_on_conflict():
    from kernel import FantasticLock

    LOCK_FILE.parent.mkdir(parents=True, exist_ok=True)
    LOCK_FILE.write_text(json.dumps({"pid": os.getpid()}))
    with pytest.raises(RuntimeError, match="another fantastic owns this dir"):
        with FantasticLock():
            pass


def test_lock_shape():
    """The lock file is `{pid:int}`."""
    acquire_lock()
    data = json.loads(LOCK_FILE.read_text())
    assert data == {"pid": os.getpid()}
