"""cli bundle — terminal output renderer."""

from __future__ import annotations


async def test_token_writes_to_stdout(seeded_kernel, capsys):
    (
        await seeded_kernel.send(
            [{"target": "cli", "payload": {"type": "token", "text": "hello"}}]
        )
    )[0]["result"]
    captured = capsys.readouterr()
    assert "hello" in captured.out


async def test_done_writes_newline(seeded_kernel, capsys):
    (await seeded_kernel.send([{"target": "cli", "payload": {"type": "done"}}]))[0][
        "result"
    ]
    captured = capsys.readouterr()
    assert captured.out.endswith("\n")


async def test_say_with_source_prefix(seeded_kernel, capsys):
    (
        await seeded_kernel.send(
            [
                {
                    "target": "cli",
                    "payload": {"type": "say", "text": "hello", "source": "agent_x"},
                }
            ]
        )
    )[0]["result"]
    captured = capsys.readouterr()
    assert "[agent_x]" in captured.out
    assert "hello" in captured.out


async def test_say_without_source(seeded_kernel, capsys):
    (
        await seeded_kernel.send(
            [{"target": "cli", "payload": {"type": "say", "text": "no-source"}}]
        )
    )[0]["result"]
    captured = capsys.readouterr()
    assert "no-source" in captured.out


async def test_error_writes_error_prefix(seeded_kernel, capsys):
    (
        await seeded_kernel.send(
            [{"target": "cli", "payload": {"type": "error", "text": "boom"}}]
        )
    )[0]["result"]
    captured = capsys.readouterr()
    assert "ERROR" in captured.out
    assert "boom" in captured.out


async def test_unknown_type_returns_none(seeded_kernel):
    r = (await seeded_kernel.send([{"target": "cli", "payload": {"type": "garbage"}}]))[
        0
    ]["result"]
    assert r is None


async def test_status_handler_renders_phase_markers(seeded_kernel, capsys):
    """queued, thinking, tool_calling entry+exit each produce one specific
    line. streaming and done produce nothing."""
    (
        await seeded_kernel.send(
            [
                {
                    "target": "cli",
                    "payload": {
                        "type": "status",
                        "source": "ollama_xx",
                        "phase": "queued",
                        "detail": {"ahead": 2, "send_id": "abc"},
                    },
                }
            ]
        )
    )[0]["result"]
    (
        await seeded_kernel.send(
            [
                {
                    "target": "cli",
                    "payload": {
                        "type": "status",
                        "source": "ollama_xx",
                        "phase": "thinking",
                        "detail": {"send_id": "abc"},
                    },
                }
            ]
        )
    )[0]["result"]
    (
        await seeded_kernel.send(
            [
                {
                    "target": "cli",
                    "payload": {
                        "type": "status",
                        "source": "ollama_xx",
                        "phase": "tool_calling",
                        "detail": {
                            "tool": {
                                "call_id": "c1",
                                "target": "kernel_state",
                                "verb": "list_agents",
                                "args": {
                                    "target_id": "kernel_state",
                                    "payload": {"type": "list_agents"},
                                },
                            },
                        },
                    },
                }
            ]
        )
    )[0]["result"]
    (
        await seeded_kernel.send(
            [
                {
                    "target": "cli",
                    "payload": {
                        "type": "status",
                        "source": "ollama_xx",
                        "phase": "tool_calling",
                        "detail": {
                            "tool": {
                                "call_id": "c1",
                                "target": "kernel_state",
                                "verb": "list_agents",
                                "args": {},
                                "reply_preview": '{"agents":[{"id":"kernel_state"}]}',
                            },
                        },
                    },
                }
            ]
        )
    )[0]["result"]
    out = capsys.readouterr().out
    lines = [line for line in out.splitlines() if line.strip()]
    assert len(lines) == 4, f"expected 4 lines, got {len(lines)}: {lines}"
    assert "queued (2 ahead)" in lines[0]
    assert "thinking" in lines[1]
    assert "→ list_agents(kernel_state)" in lines[2]
    assert "← list_agents(kernel_state)" in lines[3]


async def test_status_streaming_and_done_are_silent(seeded_kernel, capsys):
    (
        await seeded_kernel.send(
            [
                {
                    "target": "cli",
                    "payload": {"type": "status", "phase": "streaming", "detail": {}},
                }
            ]
        )
    )[0]["result"]
    (
        await seeded_kernel.send(
            [
                {
                    "target": "cli",
                    "payload": {
                        "type": "status",
                        "phase": "done",
                        "detail": {"reason": "ok"},
                    },
                }
            ]
        )
    )[0]["result"]
    assert capsys.readouterr().out == ""


# ─── two-phase PTY intro ────────────────────────────────────────


async def test_intro_booting_prints_control_plane_map(seeded_kernel, capsys):
    """First PTY push (pre-boot): identity + the pull/push plane map, port-free."""
    (
        await seeded_kernel.send(
            [{"target": "cli", "payload": {"type": "intro_booting"}}]
        )
    )[0]["result"]
    out = capsys.readouterr().out
    assert "booting" in out
    assert "send(<id>" in out
    assert "PULL" in out and "PUSH" in out and "REACH" in out
    assert "reflect readme=true" in out
    # identity line carries deployment context + the root id + this pid
    assert "python" in out and "env=" in out and "root=kernel_state" in out


async def test_booted_is_a_dumb_sink_no_tree_inspection(seeded_kernel, capsys):
    """Final PTY push: the kernel's 'all booted' close. cli is a dumb sink — it
    prints a fixed line and never inspects the tree for ports/surfaces (those are
    announced by each agent on its own boot)."""
    (await seeded_kernel.send([{"target": "cli", "payload": {"type": "booted"}}]))[0][
        "result"
    ]
    out = capsys.readouterr().out
    assert "all booted" in out
    assert "reflect readme=true" in out


# ─── control characters never reach the terminal ───────────────

# An OSC window title (BEL-terminated), an erase-line CSI, a C1 CSI, a carriage
# return, DEL and NUL — plus the two kept: newline and tab.
_HOSTILE = "a\x1b]0;TEST-VALUE-TITLE\x07b\x1b[2Kc\x9b31md\re\x7ff\x00g\th\ni"
_CLEAN = "a]0;TEST-VALUE-TITLEb[2Kc31mdefg\th\ni"


async def test_agent_text_reaches_the_terminal_without_control_characters(
    seeded_kernel, capsys
):
    """Audit BV-11: `token`/`say`/`error`/`status`/`context` wrote agent and model
    text raw, so an escape sequence in it was INTERPRETED by the operator's
    terminal (a title, a clipboard write, a rewritten line). C0/C1 controls are
    stripped, newline and tab kept — in every field the sink prints."""
    k = seeded_kernel
    for payload, want in (
        ({"type": "token", "text": _HOSTILE}, _CLEAN),
        ({"type": "say", "text": _HOSTILE, "source": "s\x1bx"}, f"  [sx] {_CLEAN}\n"),
        ({"type": "error", "text": _HOSTILE}, f"  ERROR: {_CLEAN}\n"),
        (
            {
                "type": "status",
                "phase": "tool_calling",
                "detail": {
                    "tool": {"verb": "v\x07", "target": "t", "reply_preview": _HOSTILE}
                },
            },
            f"  ← v(t)  {_CLEAN}\n",
        ),
        (
            {"type": "context", "phase": "too_small", "detail": {"hint": _HOSTILE}},
            f"  ✗ context too small — {_CLEAN}\n",
        ),
    ):
        await k.send([{"target": "cli", "payload": payload}])
        assert capsys.readouterr().out == want, payload
