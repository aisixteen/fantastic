"""Tests for core.agent — @autorun decorator and AST discovery."""

from pathlib import Path

from core.agent import autorun, discover_autorun


# ── autorun decorator ────────────────────────────────────────────


def test_autorun_bare():
    @autorun
    def fn():
        pass

    assert fn._autorun_config == {"pty": False, "env": {}}


def test_autorun_with_pty():
    @autorun(pty=True)
    def fn():
        pass

    assert fn._autorun_config == {"pty": True, "env": {}}


def test_autorun_with_env():
    @autorun(env={"K": "V"})
    def fn():
        pass

    assert fn._autorun_config == {"pty": False, "env": {"K": "V"}}


def test_autorun_with_all_args():
    @autorun(pty=True, env={"K": "V"})
    def fn():
        pass

    assert fn._autorun_config == {"pty": True, "env": {"K": "V"}}


def test_autorun_defaults():
    @autorun()
    def fn():
        pass

    assert fn._autorun_config["pty"] is False
    assert fn._autorun_config["env"] == {}


# ── discover_autorun ─────────────────────────────────────────────


def test_discover_bare_autorun(tmp_path: Path):
    src = tmp_path / "source.py"
    src.write_text("from core.agent import autorun\n\n@autorun\ndef main(): pass\n")
    result = discover_autorun(src)
    assert result == {"pty": False, "env": {}}


def test_discover_autorun_with_pty(tmp_path: Path):
    src = tmp_path / "source.py"
    src.write_text(
        "from core.agent import autorun\n\n@autorun(pty=True)\ndef main(): pass\n"
    )
    result = discover_autorun(src)
    assert result == {"pty": True, "env": {}}


def test_discover_autorun_with_env(tmp_path: Path):
    src = tmp_path / "source.py"
    src.write_text('@autorun(env={"A": "1"})\ndef x(): pass\n')
    result = discover_autorun(src)
    assert result == {"pty": False, "env": {"A": "1"}}


def test_discover_agent_dot_autorun(tmp_path: Path):
    src = tmp_path / "source.py"
    src.write_text("import core.agent as agent\n\n@agent.autorun\ndef main(): pass\n")
    result = discover_autorun(src)
    assert result == {"pty": False, "env": {}}


def test_discover_agent_dot_autorun_with_args(tmp_path: Path):
    src = tmp_path / "source.py"
    src.write_text(
        "import core.agent as agent\n\n@agent.autorun(pty=True)\ndef main(): pass\n"
    )
    result = discover_autorun(src)
    assert result == {"pty": True, "env": {}}


def test_discover_bare_function_named_autorun(tmp_path: Path):
    src = tmp_path / "source.py"
    src.write_text("def autorun(): pass\n")
    result = discover_autorun(src)
    assert result == {"pty": False, "env": {}}


def test_discover_missing_file(tmp_path: Path):
    result = discover_autorun(tmp_path / "nonexistent.py")
    assert result is None


def test_discover_empty_file(tmp_path: Path):
    src = tmp_path / "source.py"
    src.write_text("")
    result = discover_autorun(src)
    assert result is None


def test_discover_syntax_error(tmp_path: Path):
    src = tmp_path / "source.py"
    src.write_text("def !!!")
    result = discover_autorun(src)
    assert result is None


def test_discover_no_autorun(tmp_path: Path):
    src = tmp_path / "source.py"
    src.write_text("def main(): pass\n")
    result = discover_autorun(src)
    assert result is None


def test_discover_other_decorator_on_autorun_func(tmp_path: Path):
    """A function named 'autorun' with a non-autorun decorator — the decorator_list
    is non-empty so the bare-name fallback doesn't trigger, but @other is not
    an autorun decorator either → None."""
    src = tmp_path / "source.py"
    src.write_text("@other\ndef autorun(): pass\n")
    result = discover_autorun(src)
    assert result is None
