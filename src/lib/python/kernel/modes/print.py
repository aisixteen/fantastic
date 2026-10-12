"""REPL output + input helpers."""

from __future__ import annotations

import asyncio
import threading
import json
from typing import Any


async def _print_result(result: Any) -> None:
    if result is None:
        return
    if isinstance(result, dict):
        if "error" in result:
            print(f"  error: {result['error']}")
            return
    try:
        print(f"  {json.dumps(result, indent=2, default=str)}")
    except (TypeError, ValueError):
        print(f"  {result}")


async def _read_line(prompt: str) -> str:
    """`input()` on a DAEMON thread. The default executor's threads are joined
    at interpreter exit, so a daemon blocked in `input()` never exited on
    SIGTERM — it printed `[kernel] down` and hung until SIGKILL (measured
    2026-09-16). A daemon thread is abandoned with the process instead."""
    loop = asyncio.get_running_loop()
    fut: asyncio.Future[str] = loop.create_future()

    def _run() -> None:
        try:
            line = input(prompt)
        except BaseException as e:  # EOFError / KeyboardInterrupt cross the thread
            loop.call_soon_threadsafe(_settle, fut, None, e)
            return
        loop.call_soon_threadsafe(_settle, fut, line, None)

    threading.Thread(target=_run, name="fantastic-stdin", daemon=True).start()
    return await fut


def _settle(fut: "asyncio.Future[str]", line, exc) -> None:
    if fut.done():
        return
    if exc is not None:
        fut.set_exception(exc)
    else:
        fut.set_result(line)
