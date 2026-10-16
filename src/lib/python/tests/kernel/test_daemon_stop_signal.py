"""The daemon installs its stop handlers BEFORE it prints `[kernel] up`.

Whoever reads that line may signal at once. When the handlers went in after the
print, a SIGTERM in that window took the default action: the process died with no
teardown and the PID lock held (rust closed the same window, `StopSignals::install`
and its `a_sigterm_before_the_wait_is_held_not_lost`).

The test raises each of the OS's stop signals (`_sys.STOP_SIGNALS`: SIGTERM, and
SIGINT / SIGHUP; on Windows SIGBREAK — the CTRL_BREAK a polite stop sends — in
SIGHUP's place) the instant `[kernel] up` is printed and expects the clean path:
the per-signal line, the teardown, `[kernel] down`, the lock released. A
python-level guard catches a signal that arrives with no loop handler, so a
regression fails this test instead of killing the test worker.
"""

from __future__ import annotations

import asyncio
import builtins
import signal
import sys
import types

import pytest

import kernel.modes.longrun as longrun
from kernel import _sys

_WEB = types.ModuleType("web.tools")


class _Ctx:
    """The surface `_default` reads on a non-tty daemon with one web agent."""

    def __init__(self) -> None:
        self.agents = {"web": types.SimpleNamespace(handler_module=_WEB)}
        self.shutdowns = 0
        self.pending_seed = None  # an existing tree: nothing to seed

    async def start(self, boot_all: bool) -> dict:
        return {"ok": True}

    async def shutdown(self) -> None:
        self.shutdowns += 1


@pytest.mark.parametrize("name", _sys.STOP_SIGNALS)
async def test_a_stop_signal_the_instant_up_is_printed_shuts_down_cleanly(
    tmp_path, monkeypatch, capsys, name
):
    monkeypatch.chdir(tmp_path)
    monkeypatch.setattr(longrun, "_find_bundle_module", lambda name, ctx: _WEB)
    monkeypatch.setattr(sys, "stdin", types.SimpleNamespace(isatty=lambda: False))
    sig = getattr(signal, name)

    def _print(*args, **kw):
        builtins.print(*args, **kw)
        if args == ("[kernel] up",):
            signal.raise_signal(sig)

    monkeypatch.setattr(longrun, "print", _print, raising=False)

    stray: list[int] = []
    sigs = [getattr(signal, n) for n in _sys.STOP_SIGNALS]
    saved = {s: signal.getsignal(s) for s in sigs}
    signal.signal(sig, lambda signum, frame: stray.append(signum))
    ctx = _Ctx()
    try:
        await asyncio.wait_for(longrun._default(ctx), timeout=10)
    finally:
        _sys.remove_stop_signals(asyncio.get_running_loop())
        for s in sigs:
            signal.signal(s, saved[s])

    out, err = capsys.readouterr()
    assert stray == [], f"the {name} landed before the daemon's handler existed"
    assert out == "[kernel] up\n"
    assert err == (
        f"\n[kernel] {name} — shutting down...\n"
        "[kernel] tearing down agents...\n"
        "[kernel] down\n"
    )
    assert ctx.shutdowns == 1
    assert ctx.shutdown_event.is_set()
    assert not (tmp_path / ".fantastic" / "lock.yaml").exists()
