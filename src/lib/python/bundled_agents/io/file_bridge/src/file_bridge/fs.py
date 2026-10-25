"""fs — THE single clamped disk surface for the whole kernel.

This module is the ONLY place that opens an arbitrary path to read or write it. The
running-dir law lives here (a `root` is clamped to the kernel's running dir — the dir
holding its root's `.fantastic`, `Kernel.workdir`; a `path` is clamped to that root —
`../`, `~`, absolute paths, outward symlinks all refuse), so the clamp is
**unbypassable** for every path a caller names: nothing reads or writes one outside its
allowed root by going around this module. The disk code that does not come through here
is enumerated below — none of it opens a caller-named path.

Who imports `fs` is DELIBERATELY narrow — the END STATE is the `file_bridge` AGENT (the
gate) + the cold bootstrap ONLY; every other bundle does disk IO by `send`ing verbs to a
gated file_bridge agent (addressed by id, opened on demand), so the
deny-all gate is unbypassable too — not just the clamp. `scheduler` / `ai_core` already
route this way (no `fs` import). `kernel_state` (boot-read), `static_loader` (its
manifest), `exec_bridge` (a PTY job's pasted-image scratch dir, OUTSIDE the running dir)
and `credential_store` (its `file` backend — secrets at an absolute path OUTSIDE the
running dir by design, written 0600, read through `read_private`) import `fs` directly.

Two surfaces, ONE per call site (never tried in sequence — not a fallback):
  - **clamped** (default) — `root` is a `Root`: the configured root string + the running
    dir it is clamped to (ONE source: `Kernel.workdir`; nothing here reads the process
    cwd). The kernel's OWN state
    (the `file_bridge` agent's served root; `kernel_state`'s boot-read of `.fantastic`).
    The running-dir law guarantees the kernel can't escape its own dir.
  - **external** (`external=True`) — `root` is an operator/peer-chosen base that may live
    ANYWHERE; `path` is still clamped within it (can't escape the base via `..`). This is
    for surfaces that legitimately operate OUTSIDE the running dir BY DESIGN: an
    `exec_bridge` PTY job writing a pasted clipboard image into a system temp scratch dir;
    a `credential_store` keeping its secrets file at an absolute path. The
    running-dir law does NOT apply (the base isn't the kernel's own state) — but the open()
    still funnels through here, so there is still exactly one disk module.

Each call site picks ONE surface statically: kernel-state code always clamped, the
orchestration tools always `external=True`. There is no "try clamped, else external" —
that would be a fallback, which this kernel refuses.

The ONLY disk code outside this module is:
  - the substrate's fixed-path bootstrap (`kernel/_lock.py` lock.yaml, `_env` cwd/.env,
    `Agent._read_readme` reading an agent's own `readme.md` back for reflect, and
    `Agent._readme_index`/`_readme_view` reading its `docs/` topic tree) — the
    substrate is decoupled from bundles BY DESIGN (it sends verbs, never imports one), so
    it cannot import `fs`; these are fixed-relative reads of the kernel's own tree (within
    the running dir), read-mostly.
  - reads of a bundle's OWN baked-in package resources via `importlib.resources` (the
    installed wheel, e.g. a seeded `readme.md` / `help.md` / `index.html` / favicon) — a
    resource lookup in the package, not an arbitrary filesystem path.
  - `exec_bridge`'s process plumbing: its `_sys` lists `/proc` (Linux) to find a
    job's session members; `proc.resolve` probes the child's `PATH` dirs for a bare
    command (`os.path.isfile` + `os.access` — metadata only; the OS opens what it
    execs); `tools._paste_image` creates a PTY job's paste scratch dir in the system
    temp dir (`tempfile.mkdtemp`) — the image itself is written into it through here
    (`external=True`).
  - the `file_bridge` agent's `list`, which stats the entries `list_dir` returned
    (`is_dir`, `stat().st_size`) — paths this module already clamped.
Everything that takes an arbitrary path comes through here.

Every function takes `(root, path)`: `root` is the clamp boundary (a `Root`), `path` is
relative to it. Pass `external=True` (with a plain base path) for the unclamped-base
surface described above.

What differs by OS — the no-follow create, the private-file judgment, which wire paths
are absolute — lives in `file_bridge._sys`; every `OSError` this module raises reads as
POSIX python prints it, on every OS (`kernel._sys.posix_errors`).
"""

from __future__ import annotations

import os
import secrets
import time
from pathlib import Path
from typing import NamedTuple

from file_bridge import _sys
from kernel._sys import posix_errors


class Root(NamedTuple):
    """A clamped root: `rel` (the configured root string; relative ⇒ under `base`) and
    `base`, the running dir it may not leave — the dir holding the kernel root's
    `.fantastic` (`Kernel.workdir`)."""

    base: Path
    rel: str | Path = ""


def resolve_root(root: Root) -> Path:
    """Clamp `root.rel` to `root.base`. A relative root resolves under the base; an
    absolute root is allowed only if it lies inside it. `~`/`..` escapes refuse."""
    if not isinstance(root, Root):
        raise TypeError(f"fs: a clamped root is a Root, not {type(root).__name__}")
    base = Path(root.base).resolve()
    r = str(root.rel or "")
    p = (Path(r) if Path(r).is_absolute() else base / r).resolve()
    try:
        p.relative_to(base)
    except ValueError:
        raise ValueError(f"fs: root {r!r} escapes the running dir") from None
    return p


def resolve_external_root(root: str | Path) -> Path:
    """Resolve an EXTERNAL base dir — an operator/peer-chosen root that may live anywhere
    (a sibling project, a system temp dir, a deployment asset). `~` expands; the base is
    NOT clamped to the running dir. The running-dir law does not apply to these by design (see module
    docstring); the per-path clamp in `resolve(..., external=True)` still applies."""
    return Path(str(root or "")).expanduser().resolve()


def is_absolute(path: str) -> bool:
    """Is a WIRE path absolute — refused, never re-rooted? `/…` on every OS; on
    Windows also a drive (`C:`) or a UNC share (`_sys`)."""
    return _sys.is_absolute(path)


def not_found_hint(root_p: Path, path: str) -> str:
    """Hint appended to a `not found` or `escapes root` refusal: names the root and
    the relative form of the path (issue #305). An absolute path inside the root
    shows its relative form; a path that repeats the root's own name shows it
    stripped. `root_p` is the resolved root."""
    root_s = str(root_p)
    if is_absolute(path):
        try:
            rel = Path(path).relative_to(root_p)
        except ValueError:
            return f" — paths are relative to the root {root_s}"
        return f" — paths are relative to the root {root_s}, e.g. {rel}"
    base = root_p.name
    if base and path.startswith(f"{base}/"):
        stripped = path[len(base) + 1 :]
        return f" — paths are relative to the root {root_s}, e.g. {stripped}"
    return f" — paths are relative to the root {root_s}"


def resolve(
    root: Root | str | Path, path: str | Path, *, external: bool = False
) -> Path:
    """Clamp `path` within `root`. `root` itself is clamped to its running dir unless
    `external` (then the base may live anywhere — see module docstring). The target may not escape root via
    `..`/absolute/symlink either way. Returns the resolved absolute Path."""
    root_p = resolve_external_root(root) if external else resolve_root(root)
    rel = str(path or "")
    if is_absolute(rel):
        # An absolute path is a hard escape — refused, never silently re-rooted
        # (a stripped slash would serve `/etc/passwd` as `root/etc/passwd`, and
        # the docstring above promised the opposite). The agent layer answers
        # this with `bad(verb, "path", …)` BEFORE reaching here.
        raise ValueError(f"fs: path {path!r} escapes root{not_found_hint(root_p, rel)}")
    target = (root_p / rel).resolve() if rel else root_p
    try:
        target.relative_to(root_p)
    except ValueError:
        raise ValueError(
            f"fs: path {path!r} escapes root{not_found_hint(root_p, rel)}"
        ) from None
    return target


def _lexical_leaf(root, path, *, external: bool = False) -> Path:
    """The UNRESOLVED leaf under a resolved parent — the path a symlink itself
    lives at. `resolve()` follows the link all the way to its target, so testing
    `is_symlink()` on its result is always False; the leaf must be probed before
    the follow. The parent is still clamped (resolve), so the leaf cannot escape."""
    rel = str(path or "")
    parent_rel, _, name = rel.rpartition("/")
    parent = resolve(root, parent_rel, external=external)
    return parent / name if name else parent


@posix_errors
def is_symlink(root, path, *, external: bool = False) -> bool:
    """True if `path` ITSELF is a symlink (dangling or not) — `exists()` follows
    links, so a dangling one reports absent while still occupying the name. (On
    Windows a junction is a link too — `_sys.is_link`.)"""
    return _sys.is_link(_lexical_leaf(root, path, external=external))


# ─── existence / stat ───────────────────────────────────────────


@posix_errors
def exists(root, path, *, external: bool = False) -> bool:
    return resolve(root, path, external=external).exists()


@posix_errors
def is_file(root, path, *, external: bool = False) -> bool:
    return resolve(root, path, external=external).is_file()


@posix_errors
def is_dir(root, path, *, external: bool = False) -> bool:
    return resolve(root, path, external=external).is_dir()


@posix_errors
def size(root, path, *, external: bool = False) -> int:
    return resolve(root, path, external=external).stat().st_size


# ─── whole-file read / write ────────────────────────────────────


@posix_errors
def read_bytes(root, path, *, external: bool = False) -> bytes:
    return resolve(root, path, external=external).read_bytes()


@posix_errors
def read_text(root, path, *, external: bool = False) -> str:
    return resolve(root, path, external=external).read_text(encoding="utf-8")


# A watcher or reader holding the target can make the rename refuse for a moment:
# retry a bounded number of times, pausing between tries (mirrors Rust #352).
_RENAME_TRIES = 8
_RENAME_PAUSE = 0.05  # seconds


def _transient_rename_error(e: OSError) -> bool:
    """A refusal worth retrying: any PermissionError, or the Windows sharing/access
    codes 5/32/33 (`winerror` exists only on Windows). Anything else is final."""
    return isinstance(e, PermissionError) or getattr(e, "winerror", None) in (5, 32, 33)


def _rename_replace_with(op, tries: int, pause: float) -> None:
    attempts = max(tries, 1)
    for n in range(1, attempts + 1):
        try:
            op()
            return
        except OSError as e:
            if n >= attempts or not _transient_rename_error(e):
                raise
            time.sleep(pause)


def _rename_replace(tmp, target) -> None:
    _rename_replace_with(lambda: os.replace(tmp, target), _RENAME_TRIES, _RENAME_PAUSE)


@posix_errors
def write_bytes(
    root,
    path,
    data: bytes,
    *,
    atomic: bool = True,
    external: bool = False,
    mode: int | None = None,
) -> None:
    """Write whole-file. Creates parent dirs. `atomic` ⇒ temp+rename (the safe default
    for records/state); non-atomic for in-place streaming writes via write_chunk.
    `mode` (atomic only) creates the file with exactly those permission bits — a
    0600 secret file is never readable by others, not even between create and chmod."""
    target = resolve(root, path, external=external)
    target.parent.mkdir(parents=True, exist_ok=True)
    if atomic:
        # A FRESH tmp per write, created exclusively and never through a link: a
        # fixed `<name>.tmp` would follow whatever symlink sat at that name (a write
        # escaping the root) and be shared by concurrent writers.
        tmp = target.with_name(f"{target.name}.tmp.{secrets.token_hex(4)}")
        fd = _sys.create_new(tmp, mode)
        try:
            view = memoryview(data)
            while view:
                view = view[os.write(fd, view) :]
        except BaseException:
            os.close(fd)
            tmp.unlink(missing_ok=True)
            raise
        os.close(fd)
        try:
            _rename_replace(tmp, target)
        except BaseException:
            tmp.unlink(missing_ok=True)
            raise
    else:
        if mode is not None:
            raise ValueError("fs: `mode` needs an atomic write")
        target.write_bytes(data)


@posix_errors
def write_text(
    root,
    path,
    text: str,
    *,
    atomic: bool = True,
    external: bool = False,
    mode: int | None = None,
) -> None:
    write_bytes(
        root, path, text.encode("utf-8"), atomic=atomic, external=external, mode=mode
    )


class NotPrivate(PermissionError):
    """What `read_private` refused, nothing read: the entry it found is not a
    private file. `mode` (its permission bits) and `uid` (its owner) say what it
    IS, for the caller's own refusal text."""

    def __init__(self, path: Path, mode: int, uid: int):
        self.mode, self.uid = mode, uid
        super().__init__(
            f"fs: {path} is not a private file (mode {oct(mode)}, uid {uid}) — "
            "refused, nothing read"
        )


@posix_errors
def read_private(root, path, *, external: bool = False) -> bytes:
    """Read a PRIVATE file — a regular file owned by this user with no group/other
    permission bits — through ONE open. `O_NOFOLLOW` refuses a symlink in the
    leaf's place, and the OPEN DESCRIPTOR is what is judged (`fstat`) and read,
    so nothing can swap the file between the check and the read (an `lstat` of
    the path and then a read of the path again would be a window). `O_NONBLOCK` so a FIFO planted there cannot hang the open — it is
    judged and refused like any other non-regular entry. Raises `NotPrivate`
    (nothing read) when the entry is not private; `FileNotFoundError` when absent.
    (On Windows the same one open judges the handle's owner and DACL — `_sys`.)"""
    leaf = _lexical_leaf(root, path, external=external)
    judged = _sys.open_private(leaf)
    if judged.fd is None:
        raise NotPrivate(leaf, judged.mode, judged.uid)
    fd = judged.fd
    try:
        chunks = []
        while chunk := os.read(fd, 1 << 16):
            chunks.append(chunk)
        return b"".join(chunks)
    finally:
        os.close(fd)


# ─── chunked / streaming (the stream-verb backing) ──────────────


@posix_errors
def read_chunk(
    root, path, offset: int, length: int, *, external: bool = False
) -> tuple[bytes, int]:
    """Read one chunk at `offset` (the stream cursor). Returns `(chunk, total_size)`;
    the caller derives next_offset/eof. The OS handle opens + closes within this call —
    no held stream (see the SOURCE/SINK protocol)."""
    target = resolve(root, path, external=external)
    total = target.stat().st_size
    with target.open("rb") as f:
        f.seek(offset)
        # clamp to what exists: a huge `length` must not size an allocation
        chunk = f.read(min(max(0, length), max(0, total - offset)))
    return chunk, total


@posix_errors
def write_chunk(
    root,
    path,
    data: bytes,
    offset: int | None,
    *,
    truncate: bool = False,
    external: bool = False,
) -> tuple[int, int]:
    """Write one chunk at `offset` (None ⇒ append at end). `truncate` (or first write
    to a new file) starts fresh. Returns `(offset_written, new_size)`. In-place seek
    write — handle opens + closes within this call."""
    target = resolve(root, path, external=external)
    target.parent.mkdir(parents=True, exist_ok=True)
    if truncate or not target.exists():
        target.write_bytes(b"")
    off = target.stat().st_size if offset is None else int(offset)
    with target.open("r+b") as f:
        f.seek(off)
        f.write(data)
    return off, target.stat().st_size


# ─── directory walk / mutate ────────────────────────────────────


@posix_errors
def list_dir(root, path, *, external: bool = False):
    """Sorted entries of a dir as resolved Paths (raw — exposure is the AGENT's `allow`
    list, not a disk concern). Raises if not a dir."""
    target = resolve(root, path, external=external)
    return sorted(target.iterdir())


@posix_errors
def mkdir(root, path, *, external: bool = False) -> None:
    resolve(root, path, external=external).mkdir(parents=True, exist_ok=True)


@posix_errors
def rename(root, old, new, *, external: bool = False) -> None:
    o = resolve(root, old, external=external)
    n = resolve(root, new, external=external)
    n.parent.mkdir(parents=True, exist_ok=True)
    o.replace(n)  # `rename(2)` on every OS: an existing `new` is replaced


@posix_errors
def remove(root, path, *, recursive: bool = False, external: bool = False) -> None:
    """Delete a file, an empty dir, or (recursive) a dir tree. Clamped to root.

    A symlink is UNLINKED, never followed: the leaf is probed before `resolve()`
    follows it. Followed, `delete link` (link -> data/) would recurse into the
    real `data/` and leave the link dangling — the target dead, the name
    surviving, and the dangling name never deletable (`exists()` follows). A dangling symlink is deletable for the same reason."""
    leaf = _lexical_leaf(root, path, external=external)
    if _sys.is_link(leaf):
        leaf.unlink()  # a directory link (Windows) too: CPython removes the entry
        return
    target = resolve(root, path, external=external)
    if target.is_dir():
        if recursive:
            _rmtree(target)
        else:
            target.rmdir()
    else:
        target.unlink()


def _rmtree(target: Path) -> None:
    for sub in target.iterdir():
        if sub.is_dir() and not _sys.is_link(sub):
            _rmtree(sub)
        else:
            sub.unlink()
    target.rmdir()
