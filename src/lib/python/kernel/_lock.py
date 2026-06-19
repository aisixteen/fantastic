"""Per-project-dir PID lock — `.fantastic/lock.json`.

The lock is **PID-only**: one fantastic process per project dir at a
time, enforced by checking whether the recorded pid is still alive.
Nothing else lives in the lock file.

Lock file contents: `{pid: int}`. That's it. Endpoint discovery (port
for HTTP/WS) is a separate concern belonging to whichever bundle
publishes a transport — not the substrate.

Use the `FantasticLock` context manager rather than calling
`acquire_lock` / `release_lock` directly — it pairs them safely.
"""

from __future__ import annotations

import atexit
import json
import os
from pathlib import Path

FANTASTIC_DIR = Path(".fantastic")
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


def acquire_lock() -> None:
    """Acquire the PID lock for `.fantastic/`. Refuses if another
    live pid owns it. Stale locks (dead pid) are silently overwritten.
    atexit-registers release so abnormal exits still clean up."""
    cur = _read_lock()
    if cur:
        cur_pid = cur.get("pid")
        if isinstance(cur_pid, int) and _pid_alive(cur_pid):
            raise RuntimeError(
                f"another fantastic owns this dir: pid={cur_pid}\n"
                f"  -> kill it:  kill {cur_pid}\n"
                f"  -> or, if stale, remove the lock:  rm {LOCK_FILE}"
            )
    FANTASTIC_DIR.mkdir(parents=True, exist_ok=True)
    LOCK_FILE.write_text(json.dumps({"pid": os.getpid()}))
    atexit.register(release_lock)


def release_lock() -> None:
    """Remove the lock file if this pid owns it. No-op otherwise."""
    cur = _read_lock()
    if cur and cur.get("pid") == os.getpid():
        try:
            LOCK_FILE.unlink()
        except OSError:
            pass


class FantasticLock:
    """Context manager for the PID lock. PID-only — no port, no
    discovery. Raises `RuntimeError` on `__enter__` if another live
    pid owns the dir."""

    def __enter__(self) -> "FantasticLock":
        acquire_lock()
        return self

    def __exit__(self, *exc: object) -> None:
        release_lock()
