"""Tests for core._install — plugin install helpers."""

import json
import subprocess
from unittest.mock import patch

import pytest

from core._install import (
    _is_git_url,
    install_plugin,
    read_plugins_json,
    write_plugins_json,
    get_plugin_dir,
    list_plugin_dirs,
)


# ── _is_git_url ──────────────────────────────────────────────────────────


def test_is_git_url_https():
    assert _is_git_url("https://github.com/user/repo.git")


def test_is_git_url_ssh():
    assert _is_git_url("git@github.com:user/repo.git")


def test_is_git_url_dot_git_suffix():
    assert _is_git_url("some-host.com/repo.git")


def test_is_git_url_local_path():
    assert not _is_git_url("/some/local/path")


def test_is_git_url_relative_path():
    assert not _is_git_url("./relative/path")


# ── install_plugin (local) ───────────────────────────────────────────────


def test_install_local(tmp_path):
    project = tmp_path / "project"
    project.mkdir()
    source = tmp_path / "myplugin"
    source.mkdir()
    (source / "template.json").write_text(
        json.dumps({"name": "test", "bundle": "test"})
    )
    (source / "tools.py").write_text("def on_add(project_dir): pass")

    result = install_plugin(project, str(source), "test")

    assert result == project / ".fantastic" / "plugins" / "test"
    assert (result / "template.json").exists()
    assert (result / "tools.py").exists()


def test_install_local_records_plugins_json(tmp_path):
    project = tmp_path / "project"
    project.mkdir()
    source = tmp_path / "myplugin"
    source.mkdir()
    (source / "template.json").write_text("{}")

    install_plugin(project, str(source), "test")

    pj = read_plugins_json(project)
    assert "test" in pj
    assert pj["test"]["from"] == str(source)
    assert "installed" in pj["test"]


def test_install_local_no_template_json(tmp_path):
    project = tmp_path / "project"
    project.mkdir()
    source = tmp_path / "bad_plugin"
    source.mkdir()
    (source / "tools.py").write_text("")

    with pytest.raises(RuntimeError, match="no template.json"):
        install_plugin(project, str(source), "bad")


def test_install_local_not_a_dir(tmp_path):
    project = tmp_path / "project"
    project.mkdir()

    with pytest.raises(RuntimeError, match="not a directory"):
        install_plugin(project, str(tmp_path / "nonexistent"), "bad")


def test_install_local_overwrites_existing(tmp_path):
    project = tmp_path / "project"
    project.mkdir()
    source = tmp_path / "myplugin"
    source.mkdir()
    (source / "template.json").write_text('{"v": 1}')

    install_plugin(project, str(source), "test")

    # Update source and reinstall
    (source / "template.json").write_text('{"v": 2}')
    install_plugin(project, str(source), "test")

    dest = project / ".fantastic" / "plugins" / "test" / "template.json"
    assert json.loads(dest.read_text())["v"] == 2


# ── install_plugin (git) ─────────────────────────────────────────────────


def test_install_git_calls_clone(tmp_path):
    project = tmp_path / "project"
    project.mkdir()

    dest = project / ".fantastic" / "plugins" / "repo"

    def fake_clone(cmd, **kwargs):
        # Simulate git clone by creating the dir with template.json
        dest.mkdir(parents=True, exist_ok=True)
        (dest / "template.json").write_text("{}")
        return subprocess.CompletedProcess(cmd, 0, "", "")

    with patch("core._install.subprocess.run", side_effect=fake_clone):
        result = install_plugin(project, "https://github.com/user/repo.git", "repo")

    assert result == dest
    assert (dest / "template.json").exists()


def test_install_git_clone_failure(tmp_path):
    project = tmp_path / "project"
    project.mkdir()

    def fake_fail(cmd, **kwargs):
        return subprocess.CompletedProcess(cmd, 1, "", "fatal: repo not found")

    with patch("core._install.subprocess.run", side_effect=fake_fail):
        with pytest.raises(RuntimeError, match="git clone failed"):
            install_plugin(project, "git@github.com:user/repo.git", "repo")


# ── plugins.json helpers ─────────────────────────────────────────────────


def test_read_plugins_json_missing(tmp_path):
    assert read_plugins_json(tmp_path) == {}


def test_write_and_read_plugins_json(tmp_path):
    data = {"foo": {"from": "/path", "installed": 123}}
    write_plugins_json(tmp_path, data)
    assert read_plugins_json(tmp_path) == data


def test_read_plugins_json_corrupt(tmp_path):
    (tmp_path / ".fantastic").mkdir()
    (tmp_path / ".fantastic" / "plugins.json").write_text("not json")
    assert read_plugins_json(tmp_path) == {}


# ── get_plugin_dir / list_plugin_dirs ────────────────────────────────────


def test_get_plugin_dir(tmp_path):
    assert (
        get_plugin_dir(tmp_path, "foo") == tmp_path / ".fantastic" / "plugins" / "foo"
    )


def test_list_plugin_dirs_empty(tmp_path):
    assert list_plugin_dirs(tmp_path) == []


def test_list_plugin_dirs_finds_plugins(tmp_path):
    pdir = tmp_path / ".fantastic" / "plugins"
    (pdir / "alpha").mkdir(parents=True)
    (pdir / "alpha" / "template.json").write_text("{}")
    (pdir / "beta").mkdir()
    (pdir / "beta" / "template.json").write_text("{}")
    (pdir / "no_template").mkdir()  # no template.json — should be skipped

    dirs = list_plugin_dirs(tmp_path)
    assert len(dirs) == 2
    assert dirs[0].name == "alpha"
    assert dirs[1].name == "beta"
