"""fs — THE single clamped disk surface. Two surfaces, picked statically per call site:
clamped (the kernel's own state, root bound to the running dir it names) and external (an operator/peer base
that may live anywhere, path still clamped within it). Never tried in sequence."""

from __future__ import annotations

import errno
import os
from pathlib import Path

import pytest
from _testkit import posix_only

from file_bridge import _sys, fs


# ─── clamped surface (the running-dir law) ──────────────────────


def test_clamped_round_trip(tmp_path):
    R = fs.Root(tmp_path, ".fantastic")
    fs.write_text(R, "a/b.txt", "hi")
    assert fs.read_text(R, "a/b.txt") == "hi"
    assert fs.exists(R, "a/b.txt")
    assert (tmp_path / ".fantastic" / "a" / "b.txt").read_text(encoding="utf-8") == "hi"


def test_clamp_base_is_the_named_running_dir_not_the_cwd(tmp_path, monkeypatch):
    """The base is the Root's own running dir (`Kernel.workdir`); the process cwd
    plays no part."""
    (tmp_path / "work").mkdir()
    (tmp_path / "elsewhere").mkdir()
    monkeypatch.chdir(tmp_path / "elsewhere")
    fs.write_text(fs.Root(tmp_path / "work", "d"), "f.txt", "x")
    assert (tmp_path / "work" / "d" / "f.txt").read_text(encoding="utf-8") == "x"
    with pytest.raises(ValueError):
        fs.resolve(fs.Root(tmp_path / "work", str(tmp_path / "elsewhere")), "")


def test_a_bare_path_is_not_a_clamped_root(tmp_path):
    with pytest.raises(TypeError):
        fs.read_text(str(tmp_path), "anything.txt")


def test_clamped_root_escape_refuses(tmp_path):
    # an absolute root OUTSIDE the running dir is refused (the running-dir law)
    with pytest.raises(ValueError):
        fs.read_text(fs.Root(tmp_path, str(tmp_path.parent)), "anything.txt")


def test_clamped_path_escape_refuses(tmp_path):
    with pytest.raises(ValueError):
        fs.resolve(fs.Root(tmp_path, ".fantastic"), "../../etc/passwd")


def test_atomic_write_is_default(tmp_path):
    R = fs.Root(tmp_path, "root")
    fs.write_bytes(R, "f.bin", b"\x00\x01")
    # no stray tmp left behind after the rename
    assert [p.name for p in (tmp_path / "root").iterdir()] == ["f.bin"]
    assert fs.read_bytes(R, "f.bin") == b"\x00\x01"


@pytest.mark.parametrize("mode", [None, 0o600])
def test_atomic_write_never_follows_a_planted_tmp_link(tmp_path, mode):
    """A symlink at `<name>.tmp` pointing outside the root is never written
    through: each write stages in a fresh, exclusively created tmp."""
    outside = tmp_path / "outside.txt"
    outside.write_text("keep")
    root = tmp_path / "root"
    root.mkdir()
    (root / "f.txt.tmp").symlink_to(outside)
    fs.write_bytes(fs.Root(tmp_path, "root"), "f.txt", b"new", mode=mode)
    assert outside.read_text(encoding="utf-8") == "keep"
    assert (root / "f.txt").read_bytes() == b"new"


# ─── external surface (operator/peer base, anywhere) ────────────


def test_external_base_outside_cwd(tmp_path, monkeypatch):
    # cwd is elsewhere; the external base is a sibling dir OUTSIDE it.
    work = tmp_path / "work"
    work.mkdir()
    monkeypatch.chdir(work)
    foreign = tmp_path / "other_project"
    fs.write_text(str(foreign), ".fantastic/lock.yaml", '{"pid": 7}', external=True)
    # clamped surface would refuse this base; external serves it.
    with pytest.raises(ValueError):
        fs.read_text(fs.Root(work, str(foreign)), ".fantastic/lock.yaml")
    assert fs.read_text(str(foreign), ".fantastic/lock.yaml", external=True)
    assert fs.exists(str(foreign), ".fantastic/lock.yaml", external=True)


def test_external_path_still_clamped_within_base(tmp_path):
    # even external, a path may not escape the chosen base via `..`
    with pytest.raises(ValueError):
        fs.resolve(str(tmp_path), "../escape.txt", external=True)


def test_external_empty_path_is_the_base(tmp_path):
    base = tmp_path / "scratch"
    base.mkdir()
    assert fs.is_dir(str(base), "", external=True)
    assert fs.resolve(str(base), "", external=True) == base.resolve()


def test_external_remove_recursive(tmp_path):
    base = tmp_path / "scratch"
    fs.write_text(str(base), "x/y.txt", "z", external=True)
    assert fs.is_dir(str(base), "x", external=True)
    fs.remove(str(base), "", recursive=True, external=True)
    assert not base.exists()


# ─── symlinks are names, not their targets ──────────────────────


def test_remove_unlinks_a_symlink_not_its_target(tmp_path, monkeypatch):
    """`remove` never `resolve()`s first — which FOLLOWS the link — so
    `delete link` (link -> data/) never rmtree's the real `data/` and leaves the link."""
    import os

    R = fs.Root(tmp_path, "root")
    fs.write_text(R, "data/keep.txt", "k")
    os.symlink(tmp_path / "root" / "data", tmp_path / "root" / "link")
    fs.remove(R, "link", recursive=True)
    assert not os.path.lexists(tmp_path / "root" / "link")
    assert (tmp_path / "root" / "data" / "keep.txt").read_text(encoding="utf-8") == "k"
    # a file symlink too — the target survives
    os.symlink(tmp_path / "root" / "data" / "keep.txt", tmp_path / "root" / "flink")
    fs.remove(R, "flink")
    assert not (tmp_path / "root" / "flink").is_symlink()
    assert (tmp_path / "root" / "data" / "keep.txt").exists()


def test_dangling_symlink_is_deletable(tmp_path, monkeypatch):
    import os

    R = fs.Root(tmp_path, "root")
    (tmp_path / "root").mkdir()
    os.symlink(tmp_path / "root" / "gone.txt", tmp_path / "root" / "dangling")
    assert not fs.exists(R, "dangling")  # follows ⇒ absent…
    assert fs.is_symlink(R, "dangling")  # …but the NAME is there
    fs.remove(R, "dangling")
    assert not (tmp_path / "root" / "dangling").is_symlink()


def test_absolute_path_refuses_not_re_roots(tmp_path, monkeypatch):
    R = fs.Root(tmp_path, "root")
    fs.write_text(R, "x.txt", "x")
    with pytest.raises(ValueError):
        fs.resolve(R, "/x.txt")


def test_read_private_judges_the_open_descriptor_it_reads(tmp_path):
    """`read_private` opens ONCE (`O_NOFOLLOW`) and judges
    the open descriptor — regular, this user's, no group/other bits — before
    reading it, so no swap can land between the check and the read. A symlink in
    the leaf's place and a widened mode are refused (`NotPrivate`, a
    PermissionError, nothing read); an absent file is FileNotFoundError. (On
    Windows the mode is the DACL's, set and read through `_sys`.)"""
    secret = tmp_path / "s.yaml"
    secret.write_bytes(b"k: v\n")
    _sys.set_mode(secret, 0o600)
    assert fs.read_private(tmp_path, "s.yaml", external=True) == b"k: v\n"
    _sys.set_mode(secret, 0o644)
    with pytest.raises(fs.NotPrivate) as wide:
        fs.read_private(tmp_path, "s.yaml", external=True)
    assert (wide.value.mode, wide.value.uid) == (0o644, _sys.own_uid())
    assert isinstance(wide.value, PermissionError)
    _sys.set_mode(secret, 0o600)
    (tmp_path / "link.yaml").symlink_to(secret)
    with pytest.raises(fs.NotPrivate):
        fs.read_private(tmp_path, "link.yaml", external=True)
    with pytest.raises(FileNotFoundError):
        fs.read_private(tmp_path, "absent.yaml", external=True)


@posix_only("a FIFO does not exist on Windows")
def test_read_private_refuses_a_fifo_without_hanging(tmp_path):
    import os

    os.mkfifo(tmp_path / "pipe")
    with pytest.raises(fs.NotPrivate):  # judged, never a hang on the open
        fs.read_private(tmp_path, "pipe", external=True)


def test_a_private_write_is_private_from_its_create(tmp_path):
    """`mode=0o600` makes the file private on the create itself — `read_private`
    takes it as written."""
    fs.write_bytes(str(tmp_path), "w.yaml", b"x: 1\n", external=True, mode=0o600)
    assert _sys.mode_of(tmp_path / "w.yaml") == 0o600
    assert fs.read_private(tmp_path, "w.yaml", external=True) == b"x: 1\n"


def test_an_absolute_path_is_refused_on_every_os(tmp_path):
    """`/…` is absolute on every OS; on Windows a drive or a UNC share too
    (`fs.is_absolute`) — refused, never re-rooted."""
    assert fs.is_absolute("/x") and not fs.is_absolute("a/b")
    with pytest.raises(ValueError):
        fs.resolve(fs.Root(tmp_path, "root"), "/x")


# ─── atomic-write rename retries a transient refusal ────────────


def _tmps(d):
    return [p.name for p in d.iterdir() if ".tmp." in p.name]


def test_replace_retries_transient_denial_then_succeeds(tmp_path, monkeypatch):
    R = fs.Root(tmp_path, "root")
    monkeypatch.setattr(fs.time, "sleep", lambda s: None)
    real = os.replace
    calls = []

    def flaky(a, b):
        calls.append(1)
        if len(calls) <= 2:
            raise PermissionError(13, "denied")
        return real(a, b)

    monkeypatch.setattr(os, "replace", flaky)
    fs.write_bytes(R, "f.txt", b"new")
    assert fs.read_bytes(R, "f.txt") == b"new"
    assert len(calls) == 3
    assert _tmps(tmp_path / "root") == []


def test_replace_gives_up_after_bounded_tries(tmp_path, monkeypatch):
    R = fs.Root(tmp_path, "root")
    monkeypatch.setattr(fs.time, "sleep", lambda s: None)
    calls = []

    def deny(a, b):
        calls.append(1)
        raise PermissionError(13, "denied")

    monkeypatch.setattr(os, "replace", deny)
    with pytest.raises(PermissionError):
        fs.write_bytes(R, "f.txt", b"new")
    assert len(calls) == fs._RENAME_TRIES == 8
    assert _tmps(tmp_path / "root") == []
    assert not (tmp_path / "root" / "f.txt").exists()


def test_replace_does_not_retry_other_errors(tmp_path, monkeypatch):
    R = fs.Root(tmp_path, "root")
    monkeypatch.setattr(fs.time, "sleep", lambda s: None)
    calls = []

    def gone(a, b):
        calls.append(1)
        raise OSError(errno.ENOENT, "gone")

    monkeypatch.setattr(os, "replace", gone)
    with pytest.raises(OSError) as ei:
        fs.write_bytes(R, "f.txt", b"new")
    assert not isinstance(ei.value, PermissionError)
    assert len(calls) == 1
    assert _tmps(tmp_path / "root") == []


def test_write_bytes_atomic_leaves_no_tmp_on_replace_failure(tmp_path, monkeypatch):
    R = fs.Root(tmp_path, "root")
    monkeypatch.setattr(fs.time, "sleep", lambda s: None)
    d = tmp_path / "root" / "f.txt"
    d.mkdir(parents=True)
    (d / "child").write_text("x")
    with pytest.raises(OSError):
        fs.write_bytes(R, "f.txt", b"new")
    assert _tmps(tmp_path / "root") == []


def test_transient_rename_error_reads_winerror_codes():
    for code, want in ((5, True), (32, True), (33, True), (2, False)):
        e = OSError(errno.EIO, "x")
        e.winerror = code
        assert fs._transient_rename_error(e) is want
    assert fs._transient_rename_error(PermissionError(13, "x"))
    assert not fs._transient_rename_error(OSError(errno.ENOENT, "x"))


# ─── Rust twins (src/lib/rust/crates/bundles/fantastic-file/src/tests.rs) ─────


def test_resolve_dot_dot_after_descent_is_ok(tmp_path):
    """a/b/.. resolves to root/a — still inside."""
    p = fs.resolve(fs.Root(tmp_path, "."), "a/b/../leaf.txt")
    assert p.is_relative_to(tmp_path.resolve())
    assert p == tmp_path.resolve() / "a" / "leaf.txt"


@posix_only("a root symlink needs symlink privilege on Windows")
def test_root_symlink_pointing_outside_the_running_dir_refuses(tmp_path):
    """The clamp resolves symlinks on both sides: an outward symlinked root
    escapes (a file symlink inside the root is another test)."""
    base = tmp_path / "base"
    outside = tmp_path / "outside"
    base.mkdir()
    outside.mkdir()
    os.symlink(outside, base / "out")
    with pytest.raises(ValueError) as e:
        fs.resolve_root(fs.Root(base, "out"))
    assert str(e.value) == "fs: root 'out' escapes the running dir"


def test_tilde_root_is_literal_not_home(tmp_path):
    """`fs` never expands `~` for a clamped root: the name is a directory."""
    resolved = fs.resolve_root(fs.Root(tmp_path, "~/projects"))
    assert resolved == tmp_path.resolve() / "~" / "projects"
    assert resolved != Path(os.path.expanduser("~")).resolve()
