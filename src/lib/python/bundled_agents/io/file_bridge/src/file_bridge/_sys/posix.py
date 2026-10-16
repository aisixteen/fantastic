"""POSIX (macOS, Linux): `O_NOFOLLOW` opens, permission bits and the owner uid."""

from __future__ import annotations

import contextlib
import errno
import os
import stat
from pathlib import Path

# defined before `_sys` picks this module, so it is there to import
from file_bridge._sys import Private

__all__ = [
    "create_new",
    "is_absolute",
    "is_link",
    "mode_of",
    "open_private",
    "own_uid",
    "set_mode",
]


def is_absolute(path: str) -> bool:
    """A wire path is absolute when it starts `/`."""
    return path.startswith("/")


def is_link(path) -> bool:
    """Is the entry AT `path` a symlink (dangling or not) — never followed."""
    return Path(path).is_symlink()


def create_new(path, mode: int | None) -> int:
    """Create `path` exclusively for writing — never through a symlink in its
    place (`O_EXCL | O_NOFOLLOW`) — and return its descriptor. `mode` (or
    `0o666`, less the umask) is its permission bits from the create on: a `0600`
    file is never readable by others, not even between the create and a chmod."""
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW
    fd = os.open(path, flags, 0o666 if mode is None else mode)
    if mode is not None:
        try:
            os.fchmod(fd, mode)  # the umask may have narrowed it; never widened
        except BaseException:
            os.close(fd)
            with contextlib.suppress(FileNotFoundError):
                os.unlink(path)
            raise
    return fd


def open_private(path) -> Private:
    """ONE open that judges what it opened: `O_NOFOLLOW` refuses a symlink in the
    leaf's place (judged by the link's own `lstat`, never read), `O_NONBLOCK` so a
    FIFO planted there cannot hang the open, and the OPEN DESCRIPTOR is what is
    judged (`fstat`) — regular, this user's, no group/other bits — so nothing can
    swap the file between the check and the read. `FileNotFoundError` when
    absent."""
    flags = os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC
    try:
        fd = os.open(path, flags)
    except OSError as e:
        if e.errno != errno.ELOOP:
            raise
        st = os.lstat(path)  # the symlink itself, named in the refusal — never read
        return Private(None, stat.S_IMODE(st.st_mode), st.st_uid)
    try:
        st = os.fstat(fd)
    except BaseException:
        os.close(fd)
        raise
    mode = stat.S_IMODE(st.st_mode)
    if not stat.S_ISREG(st.st_mode) or st.st_uid != os.getuid() or mode & 0o077:
        os.close(fd)
        return Private(None, mode, st.st_uid)
    return Private(fd, mode, st.st_uid)


# ─── the private-file fixtures (tests) ──────────────────────────


def set_mode(path, mode: int) -> None:
    """`chmod`."""
    os.chmod(path, mode)


def mode_of(path) -> int:
    """The permission bits of `path`, followed."""
    return stat.S_IMODE(os.stat(path).st_mode)


def own_uid() -> int:
    """The uid a file this process creates is owned by."""
    return os.getuid()
