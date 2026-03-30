"""Tests for ProcessRunner — PTY management and scrollback."""

import pytest

from core.process_runner import ProcessRunner


@pytest.fixture
async def runner(tmp_path):
    agents_dir = tmp_path / ".fantastic" / "agents"
    agents_dir.mkdir(parents=True)
    output_chunks = []

    async def on_output(agent_id: str, data: str):
        output_chunks.append((agent_id, data))

    pr = ProcessRunner(on_output=on_output, agents_dir=agents_dir)
    pr._output_chunks = output_chunks  # attach for test access
    yield pr
    await pr.close_all()


async def test_create_and_exists(runner, tmp_path):
    agents_dir = tmp_path / ".fantastic" / "agents"
    (agents_dir / "a1").mkdir()
    await runner.create(
        "a1", cwd=str(tmp_path), command="/bin/echo", args=["/bin/echo", "hi"]
    )
    assert runner.exists("a1")


async def test_create_idempotent(runner, tmp_path):
    agents_dir = tmp_path / ".fantastic" / "agents"
    (agents_dir / "a1").mkdir()
    shell = runner._detect_shell()
    await runner.create("a1", cwd=str(tmp_path), command=shell)
    await runner.create("a1", cwd=str(tmp_path), command=shell)  # no-op
    assert runner.exists("a1")


async def test_not_exists(runner):
    assert runner.exists("nonexistent") is False


async def test_scrollback(runner):
    runner._append_scrollback("a1", "hello ")
    runner._append_scrollback("a1", "world")
    assert runner.get_scrollback("a1") == "hello world"


async def test_scrollback_eviction(runner):
    # Ensure large data gets evicted
    runner._append_scrollback("a1", "x" * 300_000)
    assert runner._scrollback_bytes["a1"] <= 256 * 1024 + 300_000


async def test_clear_scrollback(runner):
    runner._append_scrollback("a1", "data")
    runner.clear_scrollback("a1")
    assert runner.get_scrollback("a1") == ""


async def test_seed_scrollback(runner):
    runner.seed_scrollback("a1", "seeded data")
    assert runner.get_scrollback("a1") == "seeded data"


async def test_seed_empty(runner):
    runner.seed_scrollback("a1", "")
    assert runner.get_scrollback("a1") == ""


async def test_flush_scrollback(runner, tmp_path):
    agents_dir = tmp_path / ".fantastic" / "agents"
    (agents_dir / "t1").mkdir()
    runner._append_scrollback("t1", "flushed data")
    runner._flush_scrollback("t1")
    log_path = agents_dir / "t1" / "process.log"
    assert log_path.exists()
    assert log_path.read_text() == "flushed data"


async def test_load_scrollback_from_disk(runner, tmp_path):
    agents_dir = tmp_path / ".fantastic" / "agents"
    (agents_dir / "t1").mkdir()
    (agents_dir / "t1" / "process.log").write_text("disk data")
    loaded = runner.load_scrollback_from_disk("t1")
    assert loaded == "disk data"


async def test_load_scrollback_missing(runner):
    loaded = runner.load_scrollback_from_disk("nonexistent")
    assert loaded == ""


async def test_close_nonexistent(runner):
    # Should not raise
    await runner.close("nonexistent")


async def test_get_dimensions_nonexistent(runner):
    assert runner.get_dimensions("nonexistent") is None


async def test_shell_detect(runner):
    shell = runner._detect_shell()
    assert shell.startswith("/")
