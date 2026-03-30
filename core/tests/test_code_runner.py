"""Tests for CodeRunner — subprocess-based Python execution."""

import pytest

from core.code_runner import CodeRunner


@pytest.fixture
def runner(tmp_path):
    return CodeRunner(project_dir=str(tmp_path))


async def test_execute_stdout(runner):
    result = await runner.execute("a1", "print('hello world')")
    assert result["success"] is True
    assert any("hello world" in o.get("text", "") for o in result["outputs"])


async def test_execute_stderr_on_error(runner):
    result = await runner.execute("a1", "raise ValueError('boom')")
    assert result["success"] is False
    assert any(o["output_type"] == "error" for o in result["outputs"])


async def test_execute_stderr_as_stream(runner):
    result = await runner.execute("a1", "import sys; sys.stderr.write('warn\\n')")
    assert result["success"] is True
    assert any(o.get("name") == "stderr" for o in result["outputs"])


async def test_execute_timeout(runner):
    result = await runner.execute("a1", "import time; time.sleep(10)", timeout=0.5)
    assert result["success"] is False
    assert any("TimeoutError" in o.get("ename", "") for o in result["outputs"])


async def test_execute_cwd(runner, tmp_path):
    (tmp_path / "data.txt").write_text("ok")
    result = await runner.execute("a1", "print(open('data.txt').read())")
    assert result["success"] is True
    assert any("ok" in o.get("text", "") for o in result["outputs"])


async def test_execute_no_output(runner):
    result = await runner.execute("a1", "x = 1 + 1")
    assert result["success"] is True
    assert len(result["outputs"]) == 0


async def test_execute_with_custom_cwd(tmp_path):
    custom_dir = tmp_path / "custom"
    custom_dir.mkdir()
    (custom_dir / "marker.txt").write_text("found")
    runner = CodeRunner(project_dir=str(tmp_path))
    result = await runner.execute(
        "a1", "print(open('marker.txt').read())", cwd=str(custom_dir)
    )
    assert result["success"] is True
    assert any("found" in o.get("text", "") for o in result["outputs"])


async def test_interrupt(runner):
    result = await runner.interrupt("nonexistent")
    assert result is False


async def test_stop_all(runner):
    await runner.stop_all()  # no-op when empty
