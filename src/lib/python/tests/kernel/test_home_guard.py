"""A kernel never keeps its state in the fantastic home: a bootstrap whose
`.fantastic` IS the home (`FANTASTIC_HOME`, else `~/.fantastic`) is refused."""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

import pytest

from kernel._home import refuse_home_as_state_dir

PY = Path(__file__).resolve().parents[2]


def test_the_rule_refuses_the_home_and_nothing_else(monkeypatch, tmp_path):
    home = tmp_path / ".fantastic"
    monkeypatch.setenv("FANTASTIC_HOME", str(home))
    with pytest.raises(SystemExit) as e:
        refuse_home_as_state_dir(tmp_path, home)
    assert str(e.value) == (
        f"[kernel] {tmp_path}: its .fantastic is the fantastic home ({home})"
        " — run fantastic in a project folder"
    )
    # folded lexically
    with pytest.raises(SystemExit):
        refuse_home_as_state_dir(tmp_path, tmp_path / "x" / ".." / ".fantastic")
    # the home kernel's own state dir, and a project's
    refuse_home_as_state_dir(home / "app", home / "app")
    refuse_home_as_state_dir(tmp_path / "p", tmp_path / "p" / ".fantastic")


def test_the_default_home_is_dot_fantastic_in_the_users_home(monkeypatch, tmp_path):
    monkeypatch.delenv("FANTASTIC_HOME", raising=False)
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.setenv("USERPROFILE", str(tmp_path))
    with pytest.raises(SystemExit, match="is the fantastic home"):
        refuse_home_as_state_dir(tmp_path, tmp_path / ".fantastic")


def test_a_cli_run_in_the_homes_parent_is_refused_and_writes_nothing(tmp_path):
    home = tmp_path / ".fantastic"
    env = {**os.environ, "FANTASTIC_HOME": str(home)}
    p = subprocess.run(
        [sys.executable, str(PY / "main.py"), "kernel", "list_agents"],
        cwd=tmp_path,
        env=env,
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert p.returncode != 0
    assert "its .fantastic is the fantastic home" in p.stderr, p.stderr
    assert not home.exists()
