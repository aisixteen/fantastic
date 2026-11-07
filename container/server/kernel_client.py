"""The kernel's sockets and the check harness's small helpers, for CI checks that talk
to a fantastic server from outside (the server container's `container_check.py`).

  Fail             the first failure ends the run, with one line naming the step
  log              timestamped lines, also appended to the file `set_log_file` names
  the /user/ws client   `http_get`, `ws_connect`, `forward_call`: requests name the
                   server's authority in `Host` and are dialled at a port map; no
                   `Origin` is sent unless asked (a browser page's is checked by the guard)
  until, retrying  poll a condition; log a retried error once

Stdlib plus `websockets` (imported in the socket helpers only, so the pure helpers
import anywhere).
"""

from __future__ import annotations

import asyncio
import http.client
import itertools
import json
import time
from pathlib import Path

# the port a check dials by default
HOME_FORWARD = 18765

# the step now running; the entry point puts it in the one failure line (read as `module.STEP`)
STEP = "0"


def at(step: str) -> None:
    global STEP
    STEP = step




class Fail(Exception):
    """The first failure ends the run, with one line naming the step."""



T0 = time.monotonic()
_log_file: Path | None = None


def set_log_file(path: Path | None) -> None:
    """Every `log` line is also appended to `path`."""
    global _log_file
    _log_file = path


def log(text: str) -> None:
    line = f"[{time.monotonic() - T0:7.1f}s] {text}"
    print(line, flush=True)
    if _log_file is not None:
        with _log_file.open("a", encoding="utf-8") as f:
            f.write(line + "\n")


# ── QEMU: the QMP and guest-agent channels, and the guest ────────────────────



# ── the home kernel, through the forward ─────────────────────────────────────

_call_ids = itertools.count(1)


def http_get(port: int, path: str, *, connect: int = HOME_FORWARD, authority: str | None = None) -> tuple[int, str]:
    """GET through the forward, naming the guest's authority in `Host` (the host guard).

    `connect` is the TCP port dialled on loopback (the forward, or a container's port
    map); `authority` is the `Host` value (default `127.0.0.1:<port>`, a loopback bind's
    own authority)."""
    c = http.client.HTTPConnection("127.0.0.1", connect, timeout=5)
    try:
        c.putrequest("GET", path, skip_host=True, skip_accept_encoding=True)
        c.putheader("Host", authority or f"127.0.0.1:{port}")
        c.endheaders()
        r = c.getresponse()
        return r.status, r.read().decode("utf-8", "replace")
    finally:
        c.close()


def ws_connect(
    port: int, path: str, *, connect: int = HOME_FORWARD, authority: str | None = None, origin: str | None = None
):
    """A websocket to the guest's kernel: the TCP connection goes to the forward, the
    URI (and so `Host`) names the guest's port. No `Origin` is sent unless `origin` is
    given: a browser page's websocket sends its own (the guard checks it)."""
    from websockets.asyncio.client import connect as open_ws

    return open_ws(
        f"ws://{authority or f'127.0.0.1:{port}'}{path}", host="127.0.0.1", port=connect, open_timeout=10, origin=origin
    )


def ws_errors() -> tuple[type[BaseException], ...]:
    """What a websocket raises while the forward or the kernel is not (yet) there."""
    from websockets.exceptions import WebSocketException

    return OSError, TimeoutError, EOFError, WebSocketException


_seen: set[str] = set()


def retrying(what: str, e: BaseException) -> None:
    """Log a retried error once per distinct text (the wait's failure names the step)."""
    text = f"{what}: {type(e).__name__}: {e}"
    if text not in _seen:
        _seen.add(text)
        log(f"(retrying) {text}")


def reply_data(frame: object, call_id: str) -> object | None:
    """The `data` of the reply to call `call_id`; None for any other frame.

    An `error` frame for the call is a failure, never a value."""
    if not isinstance(frame, dict) or frame.get("id") != call_id:
        return None
    if frame.get("type") == "error":
        raise Fail(f"the call {call_id} failed: {frame}")
    if frame.get("type") == "reply":
        return frame.get("data")
    return None


async def forward_call(
    port: int,
    target: str,
    payload: dict,
    timeout: float = 10,
    *,
    connect: int = HOME_FORWARD,
    authority: str | None = None,
    origin: str | None = None,
) -> object:
    """A call to agent `target` the way a page makes one: through `user`'s `forward`."""
    call_id = f"boot-check-{next(_call_ids)}"
    frame = {"type": "call", "target": "user", "payload": {"type": "forward", "target": target, "payload": payload}, "id": call_id}
    async with ws_connect(port, "/user/ws", connect=connect, authority=authority, origin=origin) as ws:
        await ws.send(json.dumps(frame))
        end = time.monotonic() + timeout
        while True:
            data = reply_data(json.loads(await asyncio.wait_for(ws.recv(), max(0.1, end - time.monotonic()))), call_id)
            if data is not None:
                return data


def until(vm, what: str, check, timeout: float, every: float = 1.0):
    """`check()` until it is truthy (its value); fails naming `what` after `timeout`.

    `vm` is a run to watch while waiting (anything with `alive()`, a fail-fast on its exit), or None."""
    end = time.monotonic() + timeout
    while True:
        if vm is not None:
            vm.alive()
        got = check()
        if got:
            return got
        if time.monotonic() > end:
            raise Fail(f"{what}: not within {timeout:g} s")
        time.sleep(every)




def page_served(get, port: int, path: str, marker: str) -> bool:
    """`get(port, path)` answers 200 with `marker` in the body. A refused or
    half-closed connection is not yet served (the server is still coming up)."""
    try:
        status, body = get(port, path)
    except (OSError, http.client.HTTPException):
        return False
    return status == 200 and marker in body
