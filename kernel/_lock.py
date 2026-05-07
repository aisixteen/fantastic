"""Single-instance `serve` lock. `.fantastic/lock.json` carries {pid, port}."""

from __future__ import annotations

import atexit
import json
import os

from kernel._kernel import FANTASTIC_DIR

LOCK_FILE = FANTASTIC_DIR / "lock.json"


def _pid_alive(pid: int) -> bool:
    if pid <= 0:
        return False
    try:
        os.kill(pid, 0)
        return True
    except (OSError, ProcessLookupError):
        return False


def _read_lock() -> dict | None:
    if not LOCK_FILE.exists():
        return None
    try:
        return json.loads(LOCK_FILE.read_text())
    except (json.JSONDecodeError, OSError):
        return None


def acquire_serve_lock(port: int) -> None:
    """Refuse to start `serve` if a live serve is already recorded.

    Writes `.fantastic/lock.json` with `{pid, port}`. Removes it via
    `atexit` on graceful shutdown. Stale locks (whose pid is dead) are
    silently overwritten.
    """
    cur = _read_lock()
    if cur:
        cur_pid = cur.get("pid")
        cur_port = cur.get("port")
        if isinstance(cur_pid, int) and _pid_alive(cur_pid):
            raise RuntimeError(
                f"kernel already running: pid={cur_pid} port={cur_port}\n"
                f"  -> kill it:  kill {cur_pid}\n"
                f"  -> or, if stale, remove the lock:  rm {LOCK_FILE}"
            )
    FANTASTIC_DIR.mkdir(parents=True, exist_ok=True)
    LOCK_FILE.write_text(json.dumps({"pid": os.getpid(), "port": port}))
    atexit.register(_release_serve_lock)


def _release_serve_lock() -> None:
    cur = _read_lock()
    if cur and cur.get("pid") == os.getpid():
        try:
            LOCK_FILE.unlink()
        except OSError:
            pass
