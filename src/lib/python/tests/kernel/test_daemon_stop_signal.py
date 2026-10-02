"""The daemon installs its stop handlers BEFORE it prints `[kernel] up`.

Whoever reads that line may signal at once. When the handlers went in after the
print, a SIGTERM in that window took the default action: the process died with no
teardown and the PID lock held (rust closed the same window, `StopSignals::install`
and its `a_sigterm_before_the_wait_is_held_not_lost`).

The test raises SIGTERM the instant `[kernel] up` is printed and expects the clean
path: the per-signal line, the teardown, `[kernel] down`, the lock released. A
python-level guard catches a SIGTERM that arrives with no loop handler, so a
regression fails this test instead of killing the test worker.
"""

from __future__ import annotations

import asyncio
import builtins
import signal
import sys
import types

import kernel.modes.longrun as longrun

_WEB = types.ModuleType("web.tools")


class _Ctx:
    """The surface `_default` reads on a non-tty daemon with one web agent."""

    def __init__(self) -> None:
        self.agents = {"web": types.SimpleNamespace(handler_module=_WEB)}
        self.shutdowns = 0

    async def start(self, boot_all: bool) -> dict:
        return {"ok": True}

    async def shutdown(self) -> None:
        self.shutdowns += 1


async def test_a_sigterm_the_instant_up_is_printed_shuts_down_cleanly(
    tmp_path, monkeypatch, capsys
):
    monkeypatch.chdir(tmp_path)
    monkeypatch.setattr(longrun, "_find_bundle_module", lambda name, ctx: _WEB)
    monkeypatch.setattr(sys, "stdin", types.SimpleNamespace(isatty=lambda: False))

    def _print(*args, **kw):
        builtins.print(*args, **kw)
        if args == ("[kernel] up",):
            signal.raise_signal(signal.SIGTERM)

    monkeypatch.setattr(longrun, "print", _print, raising=False)

    stray: list[int] = []
    sigs = [signal.SIGINT, signal.SIGTERM, signal.SIGHUP]
    saved = {s: signal.getsignal(s) for s in sigs}
    signal.signal(signal.SIGTERM, lambda signum, frame: stray.append(signum))
    ctx = _Ctx()
    try:
        await asyncio.wait_for(longrun._default(ctx), timeout=10)
    finally:
        loop = asyncio.get_running_loop()
        for s in sigs:
            loop.remove_signal_handler(s)
            signal.signal(s, saved[s])

    out, err = capsys.readouterr()
    assert stray == [], "the SIGTERM landed before the daemon's handler existed"
    assert out == "[kernel] up\n"
    assert err == (
        "\n[kernel] SIGTERM — shutting down...\n"
        "[kernel] tearing down agents...\n"
        "[kernel] down\n"
    )
    assert ctx.shutdowns == 1
    assert ctx.shutdown_event.is_set()
    assert not (tmp_path / ".fantastic" / "lock.yaml").exists()
