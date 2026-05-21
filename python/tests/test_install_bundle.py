"""install-bundle Layout 3 helpers — multi-bundle workspace install.

Layout 1 = single bundle pyproject.toml at the root.
Layout 2 = single bundle nested in a subdir (handled via
`#subdirectory=...` on the git spec — already works in plain
`uv pip install`).
Layout 3 = root pyproject.toml is a meta package whose
`[tool.uv.workspace] members = [...]` enumerates real bundle
packages in subdirs. Each member must be installed separately so
its entry points are picked up; `uv pip install <root>` installs
only the meta.

These tests exercise the three helpers added to kernel/_modes.py:
  _parse_git_spec, _find_members_in_dir, _resolve_workspace_members.
No network: git-URL behaviour is covered indirectly via
`_parse_git_spec` and via a local-path workspace fixture.
"""

from __future__ import annotations

from pathlib import Path

from kernel._modes import (
    _find_members_in_dir,
    _parse_git_spec,
    _resolve_workspace_members,
)


def _make_workspace(root: Path, members: list[str]) -> None:
    members_toml = ", ".join(f'"{m}"' for m in members)
    (root / "pyproject.toml").write_text(
        "[project]\n"
        'name = "meta-pack"\n'
        'version = "0.0.0"\n'
        "\n"
        "[tool.uv.workspace]\n"
        f"members = [{members_toml}]\n"
    )
    for m in members:
        sub = root / m
        sub.mkdir(parents=True, exist_ok=True)
        (sub / "pyproject.toml").write_text(
            f'[project]\nname = "{m}"\nversion = "0.0.0"\n'
        )


# ─── _parse_git_spec ─────────────────────────────────────────────


def test_parse_git_spec_bare_url():
    url, ref, subdir = _parse_git_spec("https://github.com/u/r.git")
    assert url == "https://github.com/u/r.git"
    assert ref is None
    assert subdir == ""


def test_parse_git_spec_with_ref():
    url, ref, subdir = _parse_git_spec("https://github.com/u/r.git@v0.2.1")
    assert url == "https://github.com/u/r.git"
    assert ref == "v0.2.1"
    assert subdir == ""


def test_parse_git_spec_with_subdir():
    url, ref, subdir = _parse_git_spec(
        "https://github.com/u/r.git#subdirectory=pkgs/foo"
    )
    assert url == "https://github.com/u/r.git"
    assert ref is None
    assert subdir == "pkgs/foo"


def test_parse_git_spec_with_ref_and_subdir():
    url, ref, subdir = _parse_git_spec(
        "https://github.com/u/r.git@main#subdirectory=pkgs/foo"
    )
    assert url == "https://github.com/u/r.git"
    assert ref == "main"
    assert subdir == "pkgs/foo"


def test_parse_git_spec_ssh_style():
    # `git@github.com:u/r.git` — the `@` is userinfo, not a ref.
    # We don't claim to parse this form (no `://`), it just passes through.
    url, ref, subdir = _parse_git_spec("git@github.com:u/r.git")
    assert url == "git@github.com:u/r.git"
    assert ref is None


# ─── _find_members_in_dir ────────────────────────────────────────


def test_find_members_in_dir_not_a_workspace(tmp_path: Path):
    # No pyproject at all → None.
    assert _find_members_in_dir(tmp_path) is None
    # pyproject with no workspace table → None.
    (tmp_path / "pyproject.toml").write_text('[project]\nname = "x"\nversion = "0"\n')
    assert _find_members_in_dir(tmp_path) is None


def test_find_members_in_dir_flat_workspace(tmp_path: Path):
    _make_workspace(tmp_path, ["foo", "bar"])
    members = _find_members_in_dir(tmp_path)
    assert members is not None
    names = {m.name for m in members}
    assert names == {"foo", "bar"}
    for m in members:
        assert (m / "pyproject.toml").exists()


def test_find_members_in_dir_glob(tmp_path: Path):
    # `members = ["pkgs/*"]` expands; members without pyproject are skipped.
    (tmp_path / "pyproject.toml").write_text(
        "[project]\n"
        'name = "meta"\n'
        'version = "0.0.0"\n'
        "\n"
        "[tool.uv.workspace]\n"
        'members = ["pkgs/*"]\n'
    )
    pkgs = tmp_path / "pkgs"
    (pkgs / "a").mkdir(parents=True)
    (pkgs / "a" / "pyproject.toml").write_text('[project]\nname = "a"\nversion = "0"\n')
    (pkgs / "b").mkdir()
    (pkgs / "b" / "pyproject.toml").write_text('[project]\nname = "b"\nversion = "0"\n')
    # `c` has no pyproject — must be skipped.
    (pkgs / "c").mkdir()

    members = _find_members_in_dir(tmp_path)
    assert members is not None
    names = {m.name for m in members}
    assert names == {"a", "b"}


def test_find_members_in_dir_malformed_pyproject(tmp_path: Path):
    (tmp_path / "pyproject.toml").write_text("this is not = valid [toml")
    assert _find_members_in_dir(tmp_path) is None


# ─── _resolve_workspace_members ──────────────────────────────────


def test_resolve_workspace_members_local_workspace(tmp_path: Path):
    _make_workspace(tmp_path, ["one", "two"])
    members = _resolve_workspace_members(str(tmp_path))
    assert members is not None
    assert {m.name for m in members} == {"one", "two"}


def test_resolve_workspace_members_local_non_workspace(tmp_path: Path):
    # Plain single-bundle layout — should return None so caller
    # falls back to a normal `uv pip install`.
    (tmp_path / "pyproject.toml").write_text(
        '[project]\nname = "solo"\nversion = "0"\n'
    )
    assert _resolve_workspace_members(str(tmp_path)) is None


def test_resolve_workspace_members_nonexistent_path():
    assert _resolve_workspace_members("/nonexistent/path/xyz") is None


def test_resolve_workspace_members_pypi_name():
    # Plain PyPI name → not a path, not a git URL → None.
    assert _resolve_workspace_members("some-package") is None
