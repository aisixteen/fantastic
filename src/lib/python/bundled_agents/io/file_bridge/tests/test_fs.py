"""fs — THE single clamped disk surface. Two surfaces, picked statically per call site:
clamped (the kernel's own state, root bound to the running dir it names) and external (an operator/peer base
that may live anywhere, path still clamped within it). Never tried in sequence."""

from __future__ import annotations

import pytest

from file_bridge import fs


# ─── clamped surface (the running-dir law) ──────────────────────


def test_clamped_round_trip(tmp_path):
    R = fs.Root(tmp_path, ".fantastic")
    fs.write_text(R, "a/b.txt", "hi")
    assert fs.read_text(R, "a/b.txt") == "hi"
    assert fs.exists(R, "a/b.txt")
    assert (tmp_path / ".fantastic" / "a" / "b.txt").read_text() == "hi"


def test_clamp_base_is_the_named_running_dir_not_the_cwd(tmp_path, monkeypatch):
    """The base is the Root's own running dir (`Kernel.workdir`); the process cwd
    plays no part (audit 2026-09-28, c6)."""
    (tmp_path / "work").mkdir()
    (tmp_path / "elsewhere").mkdir()
    monkeypatch.chdir(tmp_path / "elsewhere")
    fs.write_text(fs.Root(tmp_path / "work", "d"), "f.txt", "x")
    assert (tmp_path / "work" / "d" / "f.txt").read_text() == "x"
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
    # no stray .tmp left behind after the rename
    assert not (tmp_path / "root" / "f.bin.tmp").exists()
    assert fs.read_bytes(R, "f.bin") == b"\x00\x01"


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


def test_open_append_hands_back_a_sink(tmp_path):
    base = tmp_path / "logs"
    h = fs.open_append(str(base), "serve.log", external=True)
    try:
        h.write(b"line\n")
    finally:
        h.close()
    assert fs.read_bytes(str(base), "serve.log", external=True) == b"line\n"


# ─── symlinks are names, not their targets ──────────────────────


def test_remove_unlinks_a_symlink_not_its_target(tmp_path, monkeypatch):
    """`remove` used to `resolve()` first — which FOLLOWS the link — and so
    `delete link` (link -> data/) rmtree'd the real `data/` and left the link."""
    import os

    R = fs.Root(tmp_path, "root")
    fs.write_text(R, "data/keep.txt", "k")
    os.symlink(tmp_path / "root" / "data", tmp_path / "root" / "link")
    fs.remove(R, "link", recursive=True)
    assert not os.path.lexists(tmp_path / "root" / "link")
    assert (tmp_path / "root" / "data" / "keep.txt").read_text() == "k"
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
