"""Per-project-dir lock — `.fantastic/lock.yaml`.

One fantastic process per project dir at a time. The decision is an `flock`
on `lock.yaml`, held on an open descriptor for the life of the process: the OS
grants it to exactly one opener, and drops it the moment its holder exits —
however it exits — so a dead holder's lock is simply taken over. Python, rust
and swift take the SAME flock on the SAME file, so kernels of different
runtimes exclude each other too. (On Windows the lock is a `LockFileEx` lock on
one byte of the file, the one rust locks — the OS calls live in `_sys`.)

Lock file contents: `{pid: int}` as a YAML mapping (`pid: 123`) — the holder's
pid, for the human-readable refusal, never for the decision. The one-shot CLI
paths (`call`, `init`) take this same flock — they never judge a pid alive. It is in the file
BEFORE the file is `lock.yaml`: the pid goes into a private
`lock.yaml.tmp.<hex>`, which is flocked and then hard-linked to `lock.yaml`
(atomic, refused when the name exists), so no opener ever reads a lock without
its pid. A `lock.yaml` nobody holds (its holder died) is replaced by renaming
the staged file over it while holding the dead file's flock — a second taker
of the same dead file finds it held. Endpoint discovery (port for HTTP/WS) is
a separate concern belonging to whichever bundle publishes a transport — not
the substrate.

Use the `FantasticLock` context manager rather than calling `acquire_lock` /
`release_lock` directly — it pairs them safely. The one exception is the
one-shot CLI (`modes/dispatch.py`): it calls `acquire_lock` itself, to word its
own refusal from `LockHeld`, and releases in a `finally`.
"""

from __future__ import annotations

import atexit
import os
import secrets
from pathlib import Path

import yaml

from kernel import _sys

FANTASTIC_DIR = Path(".fantastic")
LOCK_FILE = FANTASTIC_DIR / "lock.yaml"

# The descriptor whose flock IS this process's lock; open while it is held.
_held: int | None = None


class LockHeld(RuntimeError):
    """A live process holds the flock. `pid` is the one its file names — for the
    refusal text (the daemon's own, or a one-shot's `[call]`/`[init]` line)."""

    def __init__(self, pid: int) -> None:
        super().__init__(
            f"another fantastic owns this dir: pid={pid}\n"
            f"  -> kill it:  kill {pid}\n"
            f"  -> or, if stale, remove the lock:  rm {LOCK_FILE}"
        )
        self.pid = pid


def _parse_lock(raw: str | bytes) -> dict | None:
    try:
        doc = yaml.load(raw, Loader=yaml.CSafeLoader)
    except yaml.YAMLError:
        return None
    return doc if isinstance(doc, dict) else None


def _read_lock() -> dict | None:
    if not LOCK_FILE.exists():
        return None
    try:
        return _parse_lock(LOCK_FILE.read_text())
    except OSError:
        return None


def acquire_lock() -> None:
    """Take the flock on `.fantastic/lock.yaml` for the life of this process.
    Raises `LockHeld` if a live process holds it; a lock whose holder died is
    taken over.
    atexit-registers release so abnormal exits still clean up."""
    global _held
    FANTASTIC_DIR.mkdir(parents=True, exist_ok=True)
    staged = LOCK_FILE.with_name(f"{LOCK_FILE.name}.tmp.{secrets.token_hex(4)}")
    fd = _sys.open_lock(staged, os.O_RDWR | os.O_CREAT | os.O_EXCL, 0o644)
    try:
        os.write(
            fd,
            yaml.dump(
                {"pid": os.getpid()}, Dumper=yaml.CSafeDumper, sort_keys=False
            ).encode(),
        )
        _sys.lock(fd)  # uncontended: the staged file is ours alone
        while not _publish(staged):
            pass
    except BaseException:
        _sys.unlock_close(fd)
        staged.unlink(missing_ok=True)
        raise
    staged.unlink(missing_ok=True)
    _held = fd
    atexit.register(release_lock)


def _publish(staged: Path) -> bool:
    """One attempt to make the staged (flocked, pid-filled) file `lock.yaml`.
    True once it is; False when `lock.yaml` changed under us (try again); raises
    when a live process holds it."""
    try:
        os.link(staged, LOCK_FILE)
        return True
    except FileExistsError:
        pass
    try:
        cur = _sys.open_lock(LOCK_FILE, os.O_RDONLY)
    except FileNotFoundError:
        return False  # its holder released it between the two calls
    try:
        try:
            _sys.try_lock(cur)
        except BlockingIOError:
            # Held: a live owner — or a taker still replacing a dead lock, whose
            # file (and dead pid) this racer then reads. Either way, refused.
            doc = _parse_lock(os.read(cur, 4096))
            pid = doc.get("pid") if doc else None
            if not isinstance(pid, int):
                raise RuntimeError(
                    f"lock file at {LOCK_FILE}: lock.yaml malformed"
                ) from None
            raise LockHeld(pid) from None
        # Nobody holds it: its holder died. Replace it — but only while it is
        # still the file `lock.yaml` names (another taker may have replaced it).
        try:
            same = os.path.samestat(os.fstat(cur), os.stat(LOCK_FILE))
        except FileNotFoundError:
            return False
        if not same:
            return False
        _sys.replace(staged, LOCK_FILE)
        return True
    finally:
        _sys.unlock_close(cur)


def release_lock() -> None:
    """Drop the lock if this process holds it: remove the file while it still
    names this pid, then close the descriptor (which releases the flock).
    No-op otherwise."""
    global _held
    if _held is None:
        return
    cur = _read_lock()
    if cur and cur.get("pid") == os.getpid():
        try:
            LOCK_FILE.unlink()
        except OSError:
            pass
    _sys.unlock_close(_held)
    _held = None


class FantasticLock:
    """Context manager for the lock. No port, no discovery. Raises
    `RuntimeError` on `__enter__` if another live process holds the dir."""

    def __enter__(self) -> "FantasticLock":
        acquire_lock()
        return self

    def __exit__(self, *exc: object) -> None:
        release_lock()
