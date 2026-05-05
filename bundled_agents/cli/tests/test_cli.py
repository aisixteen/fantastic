"""cli bundle — terminal output renderer."""

from __future__ import annotations


async def test_token_writes_to_stdout(seeded_kernel, capsys):
    await seeded_kernel.send("cli", {"type": "token", "text": "hello"})
    captured = capsys.readouterr()
    assert "hello" in captured.out


async def test_done_writes_newline(seeded_kernel, capsys):
    await seeded_kernel.send("cli", {"type": "done"})
    captured = capsys.readouterr()
    assert captured.out.endswith("\n")


async def test_say_with_source_prefix(seeded_kernel, capsys):
    await seeded_kernel.send(
        "cli", {"type": "say", "text": "hello", "source": "agent_x"}
    )
    captured = capsys.readouterr()
    assert "[agent_x]" in captured.out
    assert "hello" in captured.out


async def test_say_without_source(seeded_kernel, capsys):
    await seeded_kernel.send("cli", {"type": "say", "text": "no-source"})
    captured = capsys.readouterr()
    assert "no-source" in captured.out


async def test_error_writes_error_prefix(seeded_kernel, capsys):
    await seeded_kernel.send("cli", {"type": "error", "text": "boom"})
    captured = capsys.readouterr()
    assert "ERROR" in captured.out
    assert "boom" in captured.out


async def test_unknown_type_returns_none(seeded_kernel):
    r = await seeded_kernel.send("cli", {"type": "garbage"})
    assert r is None


async def test_status_handler_renders_phase_markers(seeded_kernel, capsys):
    """queued, thinking (regular + rate-limit), tool_calling entry+exit
    each produce one specific line. streaming and done produce nothing."""
    await seeded_kernel.send(
        "cli",
        {
            "type": "status",
            "source": "ollama_xx",
            "phase": "queued",
            "detail": {"ahead": 2, "send_id": "abc"},
        },
    )
    await seeded_kernel.send(
        "cli",
        {
            "type": "status",
            "source": "ollama_xx",
            "phase": "thinking",
            "detail": {"send_id": "abc"},
        },
    )
    await seeded_kernel.send(
        "cli",
        {
            "type": "status",
            "source": "nvidia_nim_xx",
            "phase": "thinking",
            "detail": {"waiting_on": "rate_limit", "wait_s": 5},
        },
    )
    await seeded_kernel.send(
        "cli",
        {
            "type": "status",
            "source": "ollama_xx",
            "phase": "tool_calling",
            "detail": {
                "tool": {
                    "call_id": "c1",
                    "target": "core",
                    "verb": "list_agents",
                    "args": {"target_id": "core", "payload": {"type": "list_agents"}},
                },
            },
        },
    )
    await seeded_kernel.send(
        "cli",
        {
            "type": "status",
            "source": "ollama_xx",
            "phase": "tool_calling",
            "detail": {
                "tool": {
                    "call_id": "c1",
                    "target": "core",
                    "verb": "list_agents",
                    "args": {},
                    "reply_preview": '{"agents":[{"id":"core"}]}',
                },
            },
        },
    )
    out = capsys.readouterr().out
    lines = [line for line in out.splitlines() if line.strip()]
    assert len(lines) == 5, f"expected 5 lines, got {len(lines)}: {lines}"
    assert "queued (2 ahead)" in lines[0]
    assert "thinking" in lines[1]
    assert "rate-limited" in lines[2] and "5s" in lines[2]
    assert "→ list_agents(core)" in lines[3]
    assert "← list_agents(core)" in lines[4]


async def test_status_streaming_and_done_are_silent(seeded_kernel, capsys):
    await seeded_kernel.send(
        "cli", {"type": "status", "phase": "streaming", "detail": {}}
    )
    await seeded_kernel.send(
        "cli", {"type": "status", "phase": "done", "detail": {"reason": "ok"}}
    )
    assert capsys.readouterr().out == ""
