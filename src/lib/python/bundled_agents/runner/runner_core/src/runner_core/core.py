"""Shared lifecycle verb bodies for the runner bundles.

Each verb is `async def <verb>(id, transport, kernel) -> dict`, driven by a
`Transport` the bundle builds per-call. The transport supplies every
divergent primitive (filesystem vs ssh); core owns the shared skeleton:
the start lock-poll loop, the reflect/status/get_webapp assembly, and the
no-op boot.

`stop` is delegated wholesale to `transport.stop()` — local and ssh diverge
too far on death semantics (SIGTERM→SIGKILL on a local pid + lock sweep vs
kill a tunnel + ssh-kill a remote pid) to share a body. `restart` lives in
each bundle so it calls that bundle's own (test-patchable) `_stop`/`_start`.

The lock-poll constants are read off the transport (`lock_poll_timeout` /
`lock_poll_interval`), which proxies the bundle module's module-level
names, so monkeypatching them in a bundle's tests is honoured here.
"""

from __future__ import annotations

import asyncio

from .health import _ws_health

# Default poll-loop constants. Bundles re-export their own module-level
# copies (which tests monkeypatch); core reads them via the transport.
LOCK_POLL_TIMEOUT = 30.0
LOCK_POLL_INTERVAL = 0.5
STOP_POLL_TIMEOUT = 6.0
STOP_POLL_INTERVAL = 0.1

__all__ = [
    "LOCK_POLL_TIMEOUT",
    "LOCK_POLL_INTERVAL",
    "STOP_POLL_TIMEOUT",
    "STOP_POLL_INTERVAL",
    "reflect",
    "boot",
    "start",
    "stop",
    "status",
    "get_webapp",
]


async def reflect(id, transport, kernel, verbs) -> dict:
    """Identity + every record field + live status. `verbs` is the bundle's
    VERBS dict (for the verb-doc index)."""
    return {
        "id": id,
        "sentence": transport.sentence,
        **transport.reflect_fields(),
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in verbs.items()
        },
    }


async def boot(id, transport, kernel) -> None:
    """No-op. Runners do NOT auto-start — `start` is explicit so a kernel
    restart doesn't unintentionally boot every registered project."""
    return None


async def start(id, transport, kernel) -> dict:
    """Validate, short-circuit if already running, bring the daemon up, then
    poll the lock until {pid} + a web port appear; hand off to the
    transport's finish_start (local: done; ssh: open tunnel)."""
    err = transport.validate_start()
    if err is not None:
        return err

    running = await transport.already_running()
    if running is not None:
        return running

    err = await transport.bring_up()
    if err is not None:
        return err

    deadline = asyncio.get_event_loop().time() + transport.lock_poll_timeout
    while asyncio.get_event_loop().time() < deadline:
        info = await transport.read_lock()
        if info and isinstance(info.get("pid"), int):
            port = await transport.web_port()
            if port is not None:
                return await transport.finish_start(info["pid"])
        await asyncio.sleep(transport.lock_poll_interval)
    return transport.start_timeout_error()


async def stop(id, transport, kernel) -> dict:
    """Tear the project down. Delegated to the transport — death semantics
    diverge too far between local and ssh to share a body."""
    return await transport.stop()


async def status(id, transport, kernel) -> dict:
    """Transport-specific liveness dict; ws_ok is a 2s WS probe."""
    return await transport.status()


async def get_webapp(id, transport, kernel) -> dict:
    """Canvas UI descriptor when live, else {error} so the canvas skips the
    frame instead of rendering a broken iframe."""
    return await transport.get_webapp(id)


# Re-exported so transports can reuse the probe without importing health.
ws_health = _ws_health
