"""The interactive REPL (`_repl_loop`), driven line by line.

It had no test at all, and it is the one caller of three things the reduction
analysis listed as dead: `Kernel.list` (the `list` command), `Kernel.delete` (the
`delete` command) and the `bundle_resolver` cache (`add <bundle>` resolves
through it). These pin each command's output so removing any of them fails here.
The line reader is the one seam: stdin is not a tty under pytest.
"""

from __future__ import annotations

import kernel.modes.longrun as longrun

UNKNOWN = "  unknown command: {!r}  (try: list, add <bundle>, delete <id>, @<id> ...)"


def _script(monkeypatch, lines: list[str]) -> None:
    queue = list(lines)

    async def read_line(prompt: str) -> str:
        assert prompt == "fantastic> "
        if not queue:
            raise EOFError
        return queue.pop(0)

    monkeypatch.setattr(longrun, "_read_line", read_line)


async def test_list_add_at_delete_and_unknown(kernel, monkeypatch, capsys):
    ctx = kernel.ctx
    _script(
        monkeypatch,
        [
            "",
            "list",
            "add file_bridge id=fb_repl",
            "add",
            "add no_such_bundle",
            "@",
            "delete fb_repl",
            "delete fb_repl",
            "frobnicate",
            "exit",
            "list",  # never reached
        ],
    )
    assert ctx.bundle_resolver == {}
    await longrun._repl_loop(ctx)
    out = capsys.readouterr().out.splitlines()
    assert out == [
        f"  {kernel.id}  →  kernel_state.tools",
        "  created fb_repl",
        # a bare `add` is stripped to "add" — not an `add ` command at all
        UNKNOWN.format("add"),
        "  unknown bundle 'no_such_bundle'",
        "  usage: @<id> <text> | @<id> <verb> k=v ...",
        "  {",
        '  "deleted": true,',
        '  "id": "fb_repl"',
        "}",
        "  error: no agent 'fb_repl'",
        UNKNOWN.format("frobnicate"),
    ]
    # `add` resolved the bundle through the kernel's resolver cache
    assert ctx.bundle_resolver == {"file_bridge": "file_bridge.tools"}
    assert "fb_repl" not in ctx.agents


async def test_an_at_line_sends_and_prints_the_reply(kernel, monkeypatch, capsys):
    kernel.create("file_bridge.tools", id="fb_at", allow=["."])
    _script(monkeypatch, ["@fb_at reflect"])
    await longrun._repl_loop(kernel.ctx)
    out = capsys.readouterr().out.splitlines()
    assert out[:2] == ["  {", '  "id": "fb_at",']
    assert out[-2:] == ["}", ""]  # the JSON, then EOF's own newline


async def test_quit_and_eof_end_the_loop(kernel, monkeypatch, capsys):
    _script(monkeypatch, ["quit", "list"])
    await longrun._repl_loop(kernel.ctx)
    assert capsys.readouterr().out == ""
    _script(monkeypatch, [])
    await longrun._repl_loop(kernel.ctx)
    assert capsys.readouterr().out == "\n"


async def test_kernel_delete_and_list(kernel):
    ctx = kernel.ctx
    assert await ctx.delete(kernel.id) == {
        "error": f"cannot delete root agent {kernel.id!r}"
    }
    r = await ctx.delete("nobody")
    assert r["reason"] == "unknown_agent", r
    kernel.create("file_bridge.tools", id="del_me", allow=["."])
    assert "del_me" in [a["id"] for a in ctx.list()]
    assert await ctx.delete("del_me") == {"deleted": True, "id": "del_me"}
    assert "del_me" not in ctx.agents
    assert [a["id"] for a in ctx.list()] == list(ctx.agents)
