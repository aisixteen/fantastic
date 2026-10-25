"""The `.fantastic/` lock: an flock on `lock.yaml` held for the process
lifetime. The lock file is `{pid:int}` and nothing else — endpoint discovery
(e.g. the web bundle's port) is a separate concern that lives in whichever
bundle publishes a transport, not in the substrate."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import time
from pathlib import Path

import pytest
import yaml

from kernel import (
    LOCK_FILE,
    LockHeld,
    _read_lock,
    acquire_lock,
    release_lock,
)


@pytest.fixture(autouse=True)
def _cwd(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    yield
    # A test that acquired without releasing leaves the descriptor open;
    # close it here, in the test's own dir, so the next test starts unheld.
    release_lock()


def test_acquire_writes_lock_when_none():
    acquire_lock()
    assert LOCK_FILE.exists()
    data = yaml.safe_load(LOCK_FILE.read_text(encoding="utf-8"))
    assert data == {"pid": os.getpid()}


def test_acquire_raises_when_live_lock_present(tmp_path):
    # The flock is held on its own descriptor — a second acquisition (a fresh
    # open of the same file) is refused, naming the holder's pid.
    acquire_lock()
    with pytest.raises(LockHeld) as e:
        acquire_lock()
    pid = os.getpid()
    assert e.value.pid == pid
    assert str(e.value) == (
        f"another fantastic owns this dir: pid={pid}\n"
        f"  -> kill it:  kill {pid}\n"
        "  -> or, if stale, remove the lock:  rm .fantastic/lock.yaml"
    )
    # Existing lock untouched.
    assert yaml.safe_load(LOCK_FILE.read_text(encoding="utf-8"))["pid"] == pid


def test_a_live_pid_without_the_flock_is_taken_over():
    # The pid in the file is for the message, never the decision: a file naming
    # a live pid that holds no flock (its holder is gone, the pid reused) is
    # taken over.
    LOCK_FILE.parent.mkdir(parents=True, exist_ok=True)
    LOCK_FILE.write_text(json.dumps({"pid": os.getppid()}))
    acquire_lock()
    assert yaml.safe_load(LOCK_FILE.read_text(encoding="utf-8")) == {"pid": os.getpid()}


def test_acquire_leaves_no_staged_file():
    acquire_lock()
    assert sorted(p.name for p in LOCK_FILE.parent.iterdir()) == ["lock.yaml"]
    with pytest.raises(RuntimeError):
        acquire_lock()
    assert sorted(p.name for p in LOCK_FILE.parent.iterdir()) == ["lock.yaml"]


def test_acquire_overwrites_stale_lock():
    LOCK_FILE.parent.mkdir(parents=True, exist_ok=True)
    LOCK_FILE.write_text(json.dumps({"pid": 2**31 - 2}))
    acquire_lock()
    data = yaml.safe_load(LOCK_FILE.read_text(encoding="utf-8"))
    assert data == {"pid": os.getpid()}


def test_acquire_overwrites_corrupt_lock():
    LOCK_FILE.parent.mkdir(parents=True, exist_ok=True)
    LOCK_FILE.write_text("not json {{{")
    acquire_lock()
    data = yaml.safe_load(LOCK_FILE.read_text(encoding="utf-8"))
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


def test_release_lets_the_next_acquire_in():
    acquire_lock()
    release_lock()
    acquire_lock()
    assert yaml.safe_load(LOCK_FILE.read_text(encoding="utf-8")) == {"pid": os.getpid()}


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
        assert yaml.safe_load(LOCK_FILE.read_text(encoding="utf-8")) == {
            "pid": os.getpid()
        }
    assert not LOCK_FILE.exists()


def test_context_manager_raises_on_conflict():
    from kernel import FantasticLock

    with FantasticLock():
        with pytest.raises(RuntimeError, match="another fantastic owns this dir"):
            with FantasticLock():
                pass
        # the refused inner lock did not release the outer one
        assert yaml.safe_load(LOCK_FILE.read_text(encoding="utf-8")) == {
            "pid": os.getpid()
        }


def test_lock_shape():
    """The lock file is `{pid:int}`."""
    acquire_lock()
    data = yaml.safe_load(LOCK_FILE.read_text(encoding="utf-8"))
    assert data == {"pid": os.getpid()}


_RACER = r"""
import os, sys, time
t0 = float(sys.argv[1])
from kernel._lock import acquire_lock
while time.time() < t0:
    pass
try:
    acquire_lock()
    ok = 1
except RuntimeError:
    ok = 0
print(ok, flush=True)
time.sleep(0.5)
"""


@pytest.mark.parametrize("planted", [None, {"pid": 2**31 - 2}])
def test_two_racing_acquisitions_exactly_one_wins(tmp_path, planted):
    """A read-then-write lock would let two processes starting together in one dir
    BOTH acquire it. Exactly one wins every round — on a fresh dir and over a dead holder's
    lock alike."""
    python_dir = str(Path(__file__).resolve().parents[2])
    env = {**os.environ, "PYTHONPATH": python_dir}
    for round_ in range(8):
        d = tmp_path / f"r{round_}"
        d.mkdir()
        if planted is not None:
            (d / ".fantastic").mkdir()
            (d / ".fantastic" / "lock.yaml").write_text(yaml.safe_dump(planted))
        t0 = time.time() + 0.4
        procs = [
            subprocess.Popen(
                [sys.executable, "-c", _RACER, str(t0)],
                cwd=d,
                env=env,
                stdout=subprocess.PIPE,
                text=True,
            )
            for _ in range(2)
        ]
        outs = sorted(p.communicate(timeout=30)[0].strip() for p in procs)
        assert outs == ["0", "1"], f"round {round_}: {outs}"


def test_release_keeps_a_malformed_lock():
    """`release_lock` reads the file back; a lock it cannot parse is one it cannot
    verify it owns, so the file is left ALONE (never blindly removed)."""
    acquire_lock()
    LOCK_FILE.write_text("{NOT_JSON", encoding="utf-8")
    assert _read_lock() is None
    release_lock()
    assert LOCK_FILE.exists(), "a malformed lock survives release_lock()"
    assert LOCK_FILE.read_text(encoding="utf-8") == "{NOT_JSON"


def test_current_holder_returns_pid():
    """`_read_lock()` is the holder: None before any acquire, `{pid}` after."""
    assert _read_lock() is None
    acquire_lock()
    assert _read_lock() == {"pid": os.getpid()}
