"""fs — THE single clamped disk surface for the whole kernel.

This module is the ONLY place that calls `open()`/reads/writes the filesystem. The
running-dir law lives here (a `root` is clamped to the kernel's running dir — the dir
holding its root's `.fantastic`, `Kernel.workdir`; a `path` is clamped to that root —
`../`, `~`, absolute paths, outward symlinks all refuse), so the clamp is
**unbypassable**: nothing can read or write outside its allowed root by going around
it, because there is no other disk code to go around to.

Who imports `fs` is DELIBERATELY narrow — the END STATE is the `file_bridge` AGENT (the
gate) + the cold bootstrap ONLY; every other bundle does disk IO by `send`ing verbs to a
gated file_bridge agent (addressed by id, opened on demand), so the
deny-all gate is unbypassable too — not just the clamp. `scheduler` / `ai_core` already
route this way (no `fs` import). `kernel_state` (boot-read), and
`exec_bridge` (a PTY job's pasted-image scratch dir, OUTSIDE the running dir) still import `fs` for
now — the latter pending agent-routing once file_bridge agents accept an explicit
external root.

Two surfaces, ONE per call site (never tried in sequence — not a fallback):
  - **clamped** (default) — `root` is a `Root`: the configured root string + the running
    dir it is clamped to (ONE source: `Kernel.workdir`; nothing here reads the process
    cwd). The kernel's OWN state
    (the `file_bridge` agent's served root; `kernel_state`'s boot-read of `.fantastic`).
    The running-dir law guarantees the kernel can't escape its own dir.
  - **external** (`external=True`) — `root` is an operator/peer-chosen base that may live
    ANYWHERE; `path` is still clamped within it (can't escape the base via `..`). This is
    for surfaces that legitimately operate OUTSIDE the running dir BY DESIGN: an
    `exec_bridge` PTY job writing a pasted clipboard image into a system temp scratch dir. The
    running-dir law does NOT apply (the base isn't the kernel's own state) — but the open()
    still funnels through here, so there is still exactly one disk module.

Each call site picks ONE surface statically: kernel-state code always clamped, the
orchestration tools always `external=True`. There is no "try clamped, else external" —
that would be a fallback, which this kernel refuses.

The ONLY disk code outside this module is:
  - the substrate's fixed-path bootstrap (`kernel/_lock.py` lock.yaml, `_env` cwd/.env,
    `Agent._read_readme` reading an agent's own `readme.md` back for reflect) — the
    substrate is decoupled from bundles BY DESIGN (it sends verbs, never imports one), so
    it cannot import `fs`; these are fixed-relative reads of the kernel's own tree (within
    the running dir), read-mostly.
  - reads of a bundle's OWN baked-in package resources via `importlib.resources` (the
    installed wheel, e.g. a seeded `readme.md` / `help.md` / `index.html` / favicon) — a
    resource lookup in the package, not an arbitrary filesystem path.
Everything that takes an arbitrary path comes through here.

Every function takes `(root, path)`: `root` is the clamp boundary (a `Root`), `path` is
relative to it. Pass `external=True` (with a plain base path) for the unclamped-base
surface described above.
"""

from __future__ import annotations

import os
from pathlib import Path
from typing import NamedTuple


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


def resolve(
    root: Root | str | Path, path: str | Path, *, external: bool = False
) -> Path:
    """Clamp `path` within `root`. `root` itself is clamped to its running dir unless
    `external` (then the base may live anywhere — see module docstring). The target may not escape root via
    `..`/absolute/symlink either way. Returns the resolved absolute Path."""
    root_p = resolve_external_root(root) if external else resolve_root(root)
    rel = str(path or "")
    if rel.startswith("/"):
        # An absolute path is a hard escape — refused, never silently re-rooted
        # (a stripped slash would serve `/etc/passwd` as `root/etc/passwd`, and
        # the docstring above promised the opposite). The agent layer answers
        # this with `bad(verb, "path", …)` BEFORE reaching here.
        raise ValueError(f"fs: path {path!r} escapes root")
    target = (root_p / rel).resolve() if rel else root_p
    try:
        target.relative_to(root_p)
    except ValueError:
        raise ValueError(f"fs: path {path!r} escapes root") from None
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


def is_symlink(root, path, *, external: bool = False) -> bool:
    """True if `path` ITSELF is a symlink (dangling or not) — `exists()` follows
    links, so a dangling one reports absent while still occupying the name."""
    return _lexical_leaf(root, path, external=external).is_symlink()


# ─── existence / stat ───────────────────────────────────────────


def exists(root, path, *, external: bool = False) -> bool:
    return resolve(root, path, external=external).exists()


def is_file(root, path, *, external: bool = False) -> bool:
    return resolve(root, path, external=external).is_file()


def is_dir(root, path, *, external: bool = False) -> bool:
    return resolve(root, path, external=external).is_dir()


def size(root, path, *, external: bool = False) -> int:
    return resolve(root, path, external=external).stat().st_size


# ─── whole-file read / write ────────────────────────────────────


def read_bytes(root, path, *, external: bool = False) -> bytes:
    return resolve(root, path, external=external).read_bytes()


def read_text(root, path, *, external: bool = False) -> str:
    return resolve(root, path, external=external).read_text(encoding="utf-8")


def write_bytes(
    root, path, data: bytes, *, atomic: bool = True, external: bool = False
) -> None:
    """Write whole-file. Creates parent dirs. `atomic` ⇒ temp+rename (the safe default
    for records/state); non-atomic for in-place streaming writes via write_chunk."""
    target = resolve(root, path, external=external)
    target.parent.mkdir(parents=True, exist_ok=True)
    if atomic:
        tmp = target.with_name(target.name + ".tmp")
        tmp.write_bytes(data)
        os.replace(tmp, target)
    else:
        target.write_bytes(data)


def write_text(
    root, path, text: str, *, atomic: bool = True, external: bool = False
) -> None:
    write_bytes(root, path, text.encode("utf-8"), atomic=atomic, external=external)


def open_append(root, path, *, external: bool = False):
    """Open `path` for unbuffered binary APPEND, creating parent dirs. Returns the raw
    file object — for handing a long-lived sink (a subprocess's stdout/stderr log) to
    `subprocess.Popen`. The CALLER owns closing it."""
    target = resolve(root, path, external=external)
    target.parent.mkdir(parents=True, exist_ok=True)
    return target.open("ab", buffering=0)


# ─── chunked / streaming (the stream-verb backing) ──────────────


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
        chunk = f.read(max(0, length))
    return chunk, total


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


def list_dir(root, path, *, external: bool = False):
    """Sorted entries of a dir as resolved Paths (raw — exposure is the AGENT's `allow`
    list, not a disk concern). Raises if not a dir."""
    target = resolve(root, path, external=external)
    return sorted(target.iterdir())


def mkdir(root, path, *, external: bool = False) -> None:
    resolve(root, path, external=external).mkdir(parents=True, exist_ok=True)


def rename(root, old, new, *, external: bool = False) -> None:
    o = resolve(root, old, external=external)
    n = resolve(root, new, external=external)
    n.parent.mkdir(parents=True, exist_ok=True)
    o.rename(n)


def remove(root, path, *, recursive: bool = False, external: bool = False) -> None:
    """Delete a file, an empty dir, or (recursive) a dir tree. Clamped to root.

    A symlink is UNLINKED, never followed: the leaf is probed before `resolve()`
    follows it. Measured 2026-09-02: `delete link` (link -> data/) recursed into
    the real `data/` and left the link dangling — the target died, the name
    survived, and the dangling name could then never be deleted (`exists()`
    follows). A dangling symlink is deletable for the same reason."""
    leaf = _lexical_leaf(root, path, external=external)
    if leaf.is_symlink():
        leaf.unlink()
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
        if sub.is_dir() and not sub.is_symlink():
            _rmtree(sub)
        else:
            sub.unlink()
    target.rmdir()
